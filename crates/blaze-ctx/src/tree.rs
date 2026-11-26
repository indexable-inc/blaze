//! 3-level Hierarchical Summary Tree, incremental BLAKE3 ingestion, and deterministic CBOR cache.
//!
//! - **Level 0**: [`DomainSummary`] — architectural domain map across crates, docs, and agent notes.
//! - **Level 1**: [`CrateNode`], [`DocNode`], [`AgentNoteNode`] — unified concept nodes with status badges.
//! - **Level 2**: [`SlicePtr`] (`file#Lstart-Lend`) — exact line ranges on headings, exports, and notes.

use core::fmt;

use blaze_cbor::Limits as CborLimits;
use blaze_data::{Int, Map, Value};
use blaze_own::{ABTreeMap, ABTreeSet, AString, AVec, Alloc, format_in};

use crate::embed::{EmbedError, EmbeddingStore};
use crate::graph::{EdgeKind, GraphEdge, build_edges};

/// On-disk cache schema version. Incompatible versions are refused and rebuilt cleanly (Rule 26).
pub const SCHEMA_VERSION: u32 = 1;

/// Maximum exported items stored per crate entry file.
const MAX_EXPORTS_PER_CRATE: usize = 64;
/// Maximum excerpt bytes kept per heading slice.
const MAX_HEADING_EXCERPT_BYTES: usize = 240;
/// Maximum cleaned prose bytes stored in `DocNode::body_terms` for BM25 indexing.
const MAX_DOC_BODY_BYTES: usize = 2400;
/// Maximum detail bytes stored in `AgentNoteNode::detail`.
const MAX_NOTE_DETAIL_BYTES: usize = 1000;

/// Lifecycle status of a workspace crate.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum CrateStatus {
    Live,
    Parked,
    Experimental,
}

impl CrateStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            CrateStatus::Live => "live",
            CrateStatus::Parked => "parked",
            CrateStatus::Experimental => "experimental",
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "live" => Some(CrateStatus::Live),
            "parked" => Some(CrateStatus::Parked),
            "experimental" => Some(CrateStatus::Experimental),
            _ => None,
        }
    }
}

/// Front-matter status of a document under `docs/`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum DocStatus {
    Current,
    Plan,
    Record,
    Superseded,
    Unmarked,
}

impl DocStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            DocStatus::Current => "current",
            DocStatus::Plan => "plan",
            DocStatus::Record => "record",
            DocStatus::Superseded => "superseded",
            DocStatus::Unmarked => "unmarked",
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "current" => Some(DocStatus::Current),
            "plan" => Some(DocStatus::Plan),
            "record" => Some(DocStatus::Record),
            "superseded" => Some(DocStatus::Superseded),
            "unmarked" => Some(DocStatus::Unmarked),
            _ => None,
        }
    }
}

/// Category of an entry under `agents/`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum NoteKind {
    Howto,
    Annoyance,
}

impl NoteKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            NoteKind::Howto => "howto",
            NoteKind::Annoyance => "annoyance",
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "howto" => Some(NoteKind::Howto),
            "annoyance" => Some(NoteKind::Annoyance),
            _ => None,
        }
    }
}

/// Level-2 exact source/document slice pointer (`path#Lstart-Lend`).
pub struct SlicePtr<'a> {
    pub path: AString<'a>,
    pub start_line: u32,
    pub end_line: u32,
}

impl<'a> SlicePtr<'a> {
    pub fn format_ptr(&self, alloc: Alloc<'a>) -> AString<'a> {
        format_in!(alloc, "{}#L{}-L{}", self.path.as_str(), self.start_line, self.end_line)
    }
}

/// Formats `path#Lstart-Lend` directly from a path and 1-based inclusive line numbers.
pub fn format_slice_ptr<'a>(path: &str, start_line: u32, end_line: u32, alloc: Alloc<'a>) -> AString<'a> {
    let s = start_line.max(1);
    let e = end_line.max(s);
    format_in!(alloc, "{path}#L{s}-L{e}")
}

/// A public item exported from a crate's entry file (`src/lib.rs` or `src/main.rs`).
pub struct ExportItem<'a> {
    pub kind: AString<'a>,
    pub name: AString<'a>,
    pub line: u32,
}

impl<'a> ExportItem<'a> {
    fn clone_in<'b>(&self, alloc: Alloc<'b>) -> ExportItem<'b> {
        ExportItem {
            kind: AString::from_str_in(self.kind.as_str(), alloc),
            name: AString::from_str_in(self.name.as_str(), alloc),
            line: self.line,
        }
    }
}

/// Level-1 concept node for a Rust crate in the workspace.
pub struct CrateNode<'a> {
    pub id: AString<'a>,
    pub name: AString<'a>,
    pub path: AString<'a>,
    pub entry_file: AString<'a>,
    pub entry_lines: u32,
    pub status: CrateStatus,
    pub domain: AString<'a>,
    pub summary: AString<'a>,
    pub design_docs: AVec<'a, AString<'a>>,
    pub deps: AVec<'a, AString<'a>>,
    pub exports: AVec<'a, ExportItem<'a>>,
    pub hash: [u8; 32],
}

impl<'a> CrateNode<'a> {
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> CrateNode<'b> {
        CrateNode {
            id: AString::from_str_in(self.id.as_str(), alloc),
            name: AString::from_str_in(self.name.as_str(), alloc),
            path: AString::from_str_in(self.path.as_str(), alloc),
            entry_file: AString::from_str_in(self.entry_file.as_str(), alloc),
            entry_lines: self.entry_lines,
            status: self.status,
            domain: AString::from_str_in(self.domain.as_str(), alloc),
            summary: AString::from_str_in(self.summary.as_str(), alloc),
            design_docs: clone_str_vec(&self.design_docs, alloc),
            deps: clone_str_vec(&self.deps, alloc),
            exports: {
                let mut out = AVec::with_capacity_in(self.exports.len(), alloc);
                for item in self.exports.as_slice() {
                    out.push(item.clone_in(alloc));
                }
                out
            },
            hash: self.hash,
        }
    }
}

/// A Markdown heading with its exact 1-based inclusive line span and concise excerpt.
pub struct HeadingSlice<'a> {
    pub level: u8,
    pub title: AString<'a>,
    pub start_line: u32,
    pub end_line: u32,
    pub excerpt: AString<'a>,
}

impl<'a> HeadingSlice<'a> {
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> HeadingSlice<'b> {
        HeadingSlice {
            level: self.level,
            title: AString::from_str_in(self.title.as_str(), alloc),
            start_line: self.start_line,
            end_line: self.end_line,
            excerpt: AString::from_str_in(self.excerpt.as_str(), alloc),
        }
    }
}

/// Level-1 concept node for a Markdown document under `docs/`.
pub struct DocNode<'a> {
    pub id: AString<'a>,
    pub path: AString<'a>,
    pub status: DocStatus,
    pub date: Option<AString<'a>>,
    pub superseded_by: Option<AString<'a>>,
    pub domain: AString<'a>,
    pub title: AString<'a>,
    pub summary: AString<'a>,
    pub line_count: u32,
    pub headings: AVec<'a, HeadingSlice<'a>>,
    pub mentioned_crates: AVec<'a, AString<'a>>,
    pub linked_docs: AVec<'a, AString<'a>>,
    pub body_terms: AString<'a>,
    pub hash: [u8; 32],
}

impl<'a> DocNode<'a> {
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> DocNode<'b> {
        DocNode {
            id: AString::from_str_in(self.id.as_str(), alloc),
            path: AString::from_str_in(self.path.as_str(), alloc),
            status: self.status,
            date: self.date.as_ref().map(|s| AString::from_str_in(s.as_str(), alloc)),
            superseded_by: self
                .superseded_by
                .as_ref()
                .map(|s| AString::from_str_in(s.as_str(), alloc)),
            domain: AString::from_str_in(self.domain.as_str(), alloc),
            title: AString::from_str_in(self.title.as_str(), alloc),
            summary: AString::from_str_in(self.summary.as_str(), alloc),
            line_count: self.line_count,
            headings: {
                let mut out = AVec::with_capacity_in(self.headings.len(), alloc);
                for h in self.headings.as_slice() {
                    out.push(h.clone_in(alloc));
                }
                out
            },
            mentioned_crates: clone_str_vec(&self.mentioned_crates, alloc),
            linked_docs: clone_str_vec(&self.linked_docs, alloc),
            body_terms: AString::from_str_in(self.body_terms.as_str(), alloc),
            hash: self.hash,
        }
    }
}

/// Level-1 concept node for an `agents/howto` or `agents/annoyances` JSON entry.
pub struct AgentNoteNode<'a> {
    pub id: AString<'a>,
    pub path: AString<'a>,
    pub kind: NoteKind,
    pub note_id: AString<'a>,
    pub date: Option<AString<'a>>,
    pub state: Option<AString<'a>>,
    pub area: AString<'a>,
    pub domain: AString<'a>,
    pub summary: AString<'a>,
    pub detail: AString<'a>,
    pub tags: AVec<'a, AString<'a>>,
    pub line_count: u32,
    pub hash: [u8; 32],
}

impl<'a> AgentNoteNode<'a> {
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> AgentNoteNode<'b> {
        AgentNoteNode {
            id: AString::from_str_in(self.id.as_str(), alloc),
            path: AString::from_str_in(self.path.as_str(), alloc),
            kind: self.kind,
            note_id: AString::from_str_in(self.note_id.as_str(), alloc),
            date: self.date.as_ref().map(|s| AString::from_str_in(s.as_str(), alloc)),
            state: self.state.as_ref().map(|s| AString::from_str_in(s.as_str(), alloc)),
            area: AString::from_str_in(self.area.as_str(), alloc),
            domain: AString::from_str_in(self.domain.as_str(), alloc),
            summary: AString::from_str_in(self.summary.as_str(), alloc),
            detail: AString::from_str_in(self.detail.as_str(), alloc),
            tags: clone_str_vec(&self.tags, alloc),
            line_count: self.line_count,
            hash: self.hash,
        }
    }
}

/// Level-0 architectural domain roll-up.
pub struct DomainSummary<'a> {
    pub domain: AString<'a>,
    pub crate_count: u32,
    pub live_crates: u32,
    pub parked_crates: u32,
    pub experimental_crates: u32,
    pub doc_count: u32,
    pub note_count: u32,
    pub key_crates: AVec<'a, AString<'a>>,
    pub key_docs: AVec<'a, AString<'a>>,
}

/// Sans-IO input for one crate directory.
pub struct CrateSnapshot<'s> {
    /// Repo-relative crate directory, e.g. `"crates/blaze-ty"`.
    pub dir: &'s str,
    /// Raw bytes of `<dir>/Cargo.toml`.
    pub cargo_toml: &'s [u8],
    /// Repo-relative path to primary Rust entry file (`src/lib.rs` or `src/main.rs`), if any.
    pub entry_path: Option<&'s str>,
    /// Raw bytes of the entry file, if any.
    pub entry_bytes: Option<&'s [u8]>,
}

/// Sans-IO input for one raw file (doc or agent note).
pub struct FileSnapshot<'s> {
    /// Repo-relative POSIX path, e.g. `"docs/v2/plan/design/ctx.md"`.
    pub path: &'s str,
    pub bytes: &'s [u8],
}

/// Full repository snapshot passed into the pure sans-IO indexer.
pub struct RepoSnapshot<'s> {
    /// Workspace `[workspace].exclude` entries from root `Cargo.toml`.
    pub excluded_paths: &'s [&'s str],
    pub crates: &'s [CrateSnapshot<'s>],
    pub docs: &'s [FileSnapshot<'s>],
    pub howtos: &'s [FileSnapshot<'s>],
    pub annoyances: &'s [FileSnapshot<'s>],
}

/// Counters reported by an incremental index build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IndexStats {
    pub total_domains: u32,
    pub total_crates: u32,
    pub total_docs: u32,
    pub total_notes: u32,
    pub total_edges: u32,
    pub reused: u32,
    pub reindexed: u32,
    pub removed: u32,
}

/// Errors decoding a persisted `.blaze/ctx-index.cbor` cache.
pub enum CacheError<'a> {
    Cbor(blaze_cbor::Error),
    SchemaVersionMismatch { found: u64, expected: u32 },
    Malformed(AString<'a>),
    Embed(EmbedError),
}

impl fmt::Display for CacheError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheError::Cbor(e) => write!(f, "corrupt ctx-index CBOR: {e}"),
            CacheError::SchemaVersionMismatch { found, expected } => {
                write!(f, "ctx-index schema version mismatch: found v{found}, expected v{expected}")
            }
            CacheError::Malformed(msg) => write!(f, "malformed ctx-index cache: {}", msg.as_str()),
            CacheError::Embed(e) => write!(f, "invalid cached embeddings: {e}"),
        }
    }
}

impl fmt::Debug for CacheError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CacheError({self})")
    }
}

impl core::error::Error for CacheError<'_> {}

/// The complete 3-level repository context index.
pub struct CtxIndex<'a> {
    pub schema_version: u32,
    pub domains: AVec<'a, DomainSummary<'a>>,
    pub crates: AVec<'a, CrateNode<'a>>,
    pub docs: AVec<'a, DocNode<'a>>,
    pub notes: AVec<'a, AgentNoteNode<'a>>,
    pub edges: AVec<'a, GraphEdge<'a>>,
    pub embeddings: EmbeddingStore<'a>,
    alloc: Alloc<'a>,
}

impl<'a> CtxIndex<'a> {
    /// Creates an empty index in `alloc`.
    pub fn empty_in(alloc: Alloc<'a>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            domains: AVec::new_in(alloc),
            crates: AVec::new_in(alloc),
            docs: AVec::new_in(alloc),
            notes: AVec::new_in(alloc),
            edges: AVec::new_in(alloc),
            embeddings: EmbeddingStore::new_in(alloc),
            alloc,
        }
    }

    /// Returns the allocator backing this index.
    pub fn allocator(&self) -> Alloc<'a> {
        self.alloc
    }

    /// Incrementally builds a fresh [`CtxIndex`] from `snapshot`, reusing unchanged nodes from `existing`
    /// whenever their BLAKE3 content hash matches.
    pub fn build_incremental(
        existing: Option<&CtxIndex<'_>>,
        snapshot: &RepoSnapshot<'_>,
        alloc: Alloc<'a>,
    ) -> (Self, IndexStats) {
        let mut prev_crates: ABTreeMap<'a, AString<'a>, usize> = ABTreeMap::new_in(alloc);
        let mut prev_docs: ABTreeMap<'a, AString<'a>, usize> = ABTreeMap::new_in(alloc);
        let mut prev_notes: ABTreeMap<'a, AString<'a>, usize> = ABTreeMap::new_in(alloc);

        if let Some(old) = existing {
            for (i, c) in old.crates.as_slice().iter().enumerate() {
                prev_crates.insert(AString::from_str_in(c.path.as_str(), alloc), i);
            }
            for (i, d) in old.docs.as_slice().iter().enumerate() {
                prev_docs.insert(AString::from_str_in(d.path.as_str(), alloc), i);
            }
            for (i, n) in old.notes.as_slice().iter().enumerate() {
                prev_notes.insert(AString::from_str_in(n.path.as_str(), alloc), i);
            }
        }

        let mut reused = 0u32;
        let mut reindexed = 0u32;
        let mut alive_paths: ABTreeSet<'a, AString<'a>> = ABTreeSet::new_in(alloc);

        // 1. Crates.
        let mut crates = AVec::with_capacity_in(snapshot.crates.len(), alloc);
        for snap in snapshot.crates {
            alive_paths.insert(AString::from_str_in(snap.dir, alloc));
            let hash = hash_crate_snapshot(snap, snapshot.excluded_paths);
            if let Some(old) = existing
                && let Some(&idx) = prev_crates.get(snap.dir)
                && old.crates[idx].hash == hash
            {
                crates.push(old.crates[idx].clone_in(alloc));
                reused += 1;
            } else {
                crates.push(parse_crate(snap, snapshot.excluded_paths, hash, alloc));
                reindexed += 1;
            }
        }
        crates.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));

        // 2. Docs.
        let mut docs = AVec::with_capacity_in(snapshot.docs.len(), alloc);
        for snap in snapshot.docs {
            alive_paths.insert(AString::from_str_in(snap.path, alloc));
            let hash = blaze_blake3::hash(snap.bytes);
            if let Some(old) = existing
                && let Some(&idx) = prev_docs.get(snap.path)
                && old.docs[idx].hash == hash
            {
                docs.push(old.docs[idx].clone_in(alloc));
                reused += 1;
            } else {
                docs.push(parse_doc(snap, hash, alloc));
                reindexed += 1;
            }
        }
        docs.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));

        // 3. Agent notes (howto + annoyances).
        let mut notes = AVec::with_capacity_in(snapshot.howtos.len() + snapshot.annoyances.len(), alloc);
        for &(kind, list) in &[
            (NoteKind::Howto, snapshot.howtos),
            (NoteKind::Annoyance, snapshot.annoyances),
        ] {
            for snap in list {
                alive_paths.insert(AString::from_str_in(snap.path, alloc));
                let hash = blaze_blake3::hash(snap.bytes);
                if let Some(old) = existing
                    && let Some(&idx) = prev_notes.get(snap.path)
                    && old.notes[idx].hash == hash
                {
                    notes.push(old.notes[idx].clone_in(alloc));
                    reused += 1;
                } else {
                    notes.push(parse_agent_note(snap, kind, hash, alloc));
                    reindexed += 1;
                }
            }
        }
        notes.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));

        // Count removed paths from previous index.
        let mut removed = 0u32;
        if let Some(old) = existing {
            for c in old.crates.as_slice() {
                if !alive_paths.contains(c.path.as_str()) {
                    removed += 1;
                }
            }
            for d in old.docs.as_slice() {
                if !alive_paths.contains(d.path.as_str()) {
                    removed += 1;
                }
            }
            for n in old.notes.as_slice() {
                if !alive_paths.contains(n.path.as_str()) {
                    removed += 1;
                }
            }
        }

        // Fast path: when an existing index matches 100% of hashes with zero additions/changes/removals,
        // reuse its prebuilt graph edges directly without re-running edge discovery.
        let edges = match existing {
            Some(old) if reindexed == 0 && removed == 0 => {
                let mut cloned = AVec::with_capacity_in(old.edges.len(), alloc);
                for e in old.edges.as_slice() {
                    cloned.push(e.clone_in(alloc));
                }
                cloned
            }
            _ => build_edges(crates.as_slice(), docs.as_slice(), notes.as_slice(), alloc),
        };

        // 5. Build Level-0 domain summaries.
        let domains = build_domains(
            crates.as_slice(),
            docs.as_slice(),
            notes.as_slice(),
            edges.as_slice(),
            alloc,
        );

        // 6. Preserve surviving embeddings if an existing index had any.
        let embeddings = match existing {
            Some(old) if !old.embeddings.is_empty() => old.embeddings.clone_in(alloc),
            _ => EmbeddingStore::new_in(alloc),
        };

        let stats = IndexStats {
            total_domains: domains.len() as u32,
            total_crates: crates.len() as u32,
            total_docs: docs.len() as u32,
            total_notes: notes.len() as u32,
            total_edges: edges.len() as u32,
            reused,
            reindexed,
            removed,
        };

        let index = CtxIndex {
            schema_version: SCHEMA_VERSION,
            domains,
            crates,
            docs,
            notes,
            edges,
            embeddings,
            alloc,
        };
        (index, stats)
    }

    /// Encodes this index into deterministic RFC 8949 CBOR bytes.
    pub fn to_cbor(&self) -> AVec<'a, u8> {
        let val = self.to_value(self.alloc);
        blaze_cbor::to_vec_in(&val, self.alloc)
    }

    /// Decodes a [`CtxIndex`] from deterministic CBOR bytes, validating `schema_version`.
    pub fn from_cbor(bytes: &[u8], alloc: Alloc<'a>) -> Result<Self, CacheError<'a>> {
        let val = blaze_cbor::read_in(bytes, CborLimits::SHIPPED, alloc).map_err(CacheError::Cbor)?;
        Self::from_value(&val, alloc)
    }

    /// Serializes this index into a [`blaze_data::Value`].
    pub fn to_value(&self, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
        let mut root = Map::with_capacity_in(7, alloc);
        root.insert(
            AString::from_str_in("schema_version", alloc),
            Value::Int(Int::from_u64(u64::from(self.schema_version))),
        );

        // Crates
        let mut crates_list = AVec::with_capacity_in(self.crates.len(), alloc);
        for c in self.crates.as_slice() {
            crates_list.push(crate_to_value(c, alloc));
        }
        root.insert(AString::from_str_in("crates", alloc), Value::List(crates_list));

        // Docs
        let mut docs_list = AVec::with_capacity_in(self.docs.len(), alloc);
        for d in self.docs.as_slice() {
            docs_list.push(doc_to_value(d, alloc));
        }
        root.insert(AString::from_str_in("docs", alloc), Value::List(docs_list));

        // Notes
        let mut notes_list = AVec::with_capacity_in(self.notes.len(), alloc);
        for n in self.notes.as_slice() {
            notes_list.push(note_to_value(n, alloc));
        }
        root.insert(AString::from_str_in("notes", alloc), Value::List(notes_list));

        // Edges
        let mut edges_list = AVec::with_capacity_in(self.edges.len(), alloc);
        for e in self.edges.as_slice() {
            let mut em = Map::with_capacity_in(3, alloc);
            em.insert(AString::from_str_in("from", alloc), Value::Text(AString::from_str_in(e.from.as_str(), alloc)));
            em.insert(AString::from_str_in("to", alloc), Value::Text(AString::from_str_in(e.to.as_str(), alloc)));
            em.insert(AString::from_str_in("kind", alloc), Value::Text(AString::from_str_in(e.kind.as_str(), alloc)));
            edges_list.push(Value::Map(em));
        }
        root.insert(AString::from_str_in("edges", alloc), Value::List(edges_list));

        // Embeddings
        root.insert(AString::from_str_in("embeddings", alloc), self.embeddings.to_value(alloc));

        Value::Map(root)
    }

    /// Deserializes a [`CtxIndex`] from a [`blaze_data::Value`].
    pub fn from_value(value: &Value<Alloc<'a>>, alloc: Alloc<'a>) -> Result<Self, CacheError<'a>> {
        let Value::Map(root) = value else {
            return Err(CacheError::Malformed(AString::from_str_in("root is not a map", alloc)));
        };
        let found_ver = req_u64(root, "schema_version", alloc)?;
        if found_ver != u64::from(SCHEMA_VERSION) {
            return Err(CacheError::SchemaVersionMismatch { found: found_ver, expected: SCHEMA_VERSION });
        }

        let crates_raw = req_list(root, "crates", alloc)?;
        let mut crates = AVec::with_capacity_in(crates_raw.len(), alloc);
        for item in crates_raw {
            crates.push(crate_from_value(item, alloc)?);
        }

        let docs_raw = req_list(root, "docs", alloc)?;
        let mut docs = AVec::with_capacity_in(docs_raw.len(), alloc);
        for item in docs_raw {
            docs.push(doc_from_value(item, alloc)?);
        }

        let notes_raw = req_list(root, "notes", alloc)?;
        let mut notes = AVec::with_capacity_in(notes_raw.len(), alloc);
        for item in notes_raw {
            notes.push(note_from_value(item, alloc)?);
        }

        let edges_raw = req_list(root, "edges", alloc)?;
        let mut edges = AVec::with_capacity_in(edges_raw.len(), alloc);
        for item in edges_raw {
            let Value::Map(em) = item else {
                return Err(CacheError::Malformed(AString::from_str_in("edge is not a map", alloc)));
            };
            let from = req_text(em, "from", alloc)?;
            let to = req_text(em, "to", alloc)?;
            let kind_str = req_text(em, "kind", alloc)?;
            let kind = EdgeKind::from_label(kind_str.as_str()).ok_or_else(|| {
                CacheError::Malformed(format_in!(alloc, "unknown edge kind: {}", kind_str.as_str()))
            })?;
            edges.push(GraphEdge { from, to, kind });
        }

        let embeddings = match root.get("embeddings") {
            Some(v) => EmbeddingStore::from_value(v, alloc).map_err(CacheError::Embed)?,
            None => EmbeddingStore::new_in(alloc),
        };

        let domains = build_domains(
            crates.as_slice(),
            docs.as_slice(),
            notes.as_slice(),
            edges.as_slice(),
            alloc,
        );

        Ok(CtxIndex {
            schema_version: SCHEMA_VERSION,
            domains,
            crates,
            docs,
            notes,
            edges,
            embeddings,
            alloc,
        })
    }
}

/// Maps a crate name, doc path, or topic hint to its Level-0 architectural domain.
pub fn classify_domain(hint: &str) -> &'static str {
    let norm = hint.trim().trim_start_matches("crates/").trim_end_matches('/');
    // Exact crate matches first to avoid prefix collisions (e.g. `blaze-c` vs `blaze-core`/`blaze-cbor`).
    match norm {
        "blaze-c" | "blaze-cc" | "blaze-cunit" | "blaze-build" | "blaze-lir" | "blaze-emit" | "blaze-tier"
        | "blaze-rungs" | "blaze-rung-tiers" | "blaze-ladder" | "blaze-code" | "blaze-native" | "blaze-export"
        | "target-features" => return "backend",
        "blaze-core" | "blaze-run" | "blaze-drive" | "blaze-cli" | "blaze-engine" | "blaze-graph" | "blaze-launch"
        | "blaze-launch-rt" | "blaze-launcher" | "blaze-inproc" | "blaze-inproc-rt" | "blaze-spawn"
        | "blaze-proc-macro" | "blaze-macro-run" | "blaze-wasi" | "blaze-comptime" | "blaze-comptime-sys"
        | "blaze-hvbox" => return "runtime-and-exec",
        "blaze-ctx" | "blaze-data" | "blaze-cbor" | "blaze-json" | "blaze-tab" | "blaze-toml" | "blaze-base64"
        | "blaze-parquet" | "blaze-arrow-ipc" | "blaze-squeeze" | "blaze-deflate" | "blaze-zstd" | "blaze-tar"
        | "blaze-semver" | "blaze-text" | "blaze-text-size" | "blaze-tokens" => return "data-and-formats",
        "blaze-cas" | "blaze-keyed" | "blaze-format" | "blaze-format-core" | "blaze-format-tiers" | "blaze-record"
        | "blaze-remote" | "blaze-fetch" | "blaze-cert" | "blaze-store-check" | "blaze-reclaim" | "blaze-pack"
        | "blaze-roots" | "blaze-facts" | "blaze-memo" | "blaze-memo-macros" | "blaze-id" | "blaze-ids"
        | "blaze-world" | "blaze-world-api" | "blaze-unit-graph" | "blaze-units" | "blaze-manifest" | "blaze-pins"
        | "blaze-toolchain" | "blaze-audit" | "blaze-resolve" | "blaze-source" | "blaze-stage" | "blaze-new"
        | "blaze-producers" | "blaze-build-script" => return "storage-and-cache",
        "blaze-std" | "blaze-own" | "blaze-mem" | "blaze-malloc" | "blaze-columns" | "blaze-kit" | "blaze-swiss"
        | "blaze-hash" | "blaze-hashcons" | "blaze-radix" | "blaze-sort" | "blaze-memchr" | "blaze-int"
        | "blaze-bitflags" | "blaze-bytes" | "blaze-blake3" | "blaze-sha256" | "blaze-ed25519" | "blaze-rng"
        | "blaze-unicode" | "blaze-spin" | "blaze-queue" | "blaze-sys" | "blaze-apple-sdk" | "blaze-bstd-convert"
        | "blaze-cast" | "blaze-cond" | "blaze-algebra" | "blaze-math" | "blaze-dbsp" | "blaze-glob" | "blaze-phf"
        | "paths" | "stdx" => return "std-and-memory",
        "blaze-http" | "blaze-h1" | "blaze-h2" | "blaze-url" | "blaze-git" | "blaze-git-fetch" | "blaze-sql"
        | "blaze-rel" | "blaze-sqlite" | "blaze-pg" | "blaze-protocol" => return "network-and-db",
        "blaze-ecs" | "blaze-ecs-wasm" | "blaze-ui" | "blaze-ui-wasm" | "blaze-site" | "blaze-site-wasm"
        | "blaze-vt" | "blaze-screen" | "blaze-cad" | "blaze-cad-cloth" | "blaze-cad-field" | "blaze-cad-mesh"
        | "blaze-cad-out" | "blaze-cad-picture" | "blaze-pbr" | "gpu" | "gpu-macros" | "harness" => {
            return "ui-cad-and-site";
        }
        "blaze-lint" | "blaze-lint-defs" | "blaze-fmt" | "blaze-sim" | "blaze-check" | "blaze-fuzz" | "blaze-bench"
        | "blaze-profile" | "blaze-trace" | "blaze-meter" | "blaze-watch" | "blaze-alloc-count" | "blaze-report"
        | "blaze-ask" | "blaze-answer" | "blaze-script" | "blaze-real" | "blaze-test-reach" | "blaze-v2-proof"
        | "blaze-base-tools" | "ix-prof" | "test-utils" => return "lint-test-and-obs",
        _ => {}
    }

    let lower_bytes = norm.as_bytes();
    let has = |needle: &str| contains_ascii_ci(lower_bytes, needle.as_bytes());

    if has("blaze-ty")
        || has("typeck")
        || has("blaze-solve")
        || has("solve")
        || has("blaze-impls")
        || has("blaze-scope")
        || has("blaze-expand")
        || has("file-items")
        || has("blaze-surface")
        || has("blaze-lex")
        || has("blaze-body")
        || has("blaze-flow")
        || has("borrow")
        || has("consteval")
        || has("blaze-lower")
        || has("parser")
        || has("syntax")
        || has("rustc_")
        || has("const-ast")
        || has("edition")
        || has("intern")
        || has("refined")
        || has("hir")
        || has("mir")
        || has("type-of")
        || has("error-sets")
        || has("records")
    {
        return "frontend";
    }

    if has("blaze-data")
        || has("blaze-cbor")
        || has("blaze-json")
        || has("blaze-tab")
        || has("blaze-toml")
        || has("blaze-ctx")
        || has("parquet")
        || has("arrow-ipc")
        || has("deflate")
        || has("zstd")
        || has("semver")
        || has("serial")
        || has("/ctx.md")
    {
        return "data-and-formats";
    }

    if has("blaze-lir")
        || has("blaze-emit")
        || has("blaze-tier")
        || has("blaze-rung")
        || has("blaze-ladder")
        || has("blaze-export")
        || has("backend")
        || has("codegen")
        || has("aarch64")
        || has("llvm")
        || has("cranelift")
        || has("pgo")
        || has("jit")
    {
        return "backend";
    }

    if has("blaze-core")
        || has("blaze-run")
        || has("blaze-drive")
        || has("blaze-cli")
        || has("blaze-engine")
        || has("blaze-graph")
        || has("blaze-launch")
        || has("blaze-inproc")
        || has("blaze-spawn")
        || has("proc-macro")
        || has("macro-run")
        || has("blaze-wasi")
        || has("comptime")
        || has("hvbox")
        || has("sandbox")
        || has("fiber")
        || has("hermetic")
        || has("parallel")
    {
        return "runtime-and-exec";
    }

    if has("blaze-cas")
        || has("blaze-keyed")
        || has("blaze-format")
        || has("blaze-record")
        || has("blaze-remote")
        || has("blaze-fetch")
        || has("blaze-cert")
        || has("store")
        || has("reclaim")
        || has("manifest")
        || has("toolchain")
        || has("audit")
        || has("cache")
        || has("persist")
        || has("identity")
        || has("bottom")
        || has("build-script")
    {
        return "storage-and-cache";
    }

    if has("blaze-std")
        || has("bstd")
        || has("blaze-own")
        || has("blaze-mem")
        || has("malloc")
        || has("blaze-columns")
        || has("blake3")
        || has("sha256")
        || has("ed25519")
        || has("memory")
        || has("alloc")
    {
        return "std-and-memory";
    }

    if has("http")
        || has("blaze-url")
        || has("blaze-git")
        || has("git-status")
        || has("sql")
        || has("sqlite")
        || has("network")
        || has("protocol")
    {
        return "network-and-db";
    }

    if has("blaze-ecs")
        || has("blaze-ui")
        || has("blaze-site")
        || has("blaze-vt")
        || has("vt.md")
        || has("vt-inventory")
        || has("screen")
        || has("blaze-cad")
        || has("harness")
        || has("terminal")
    {
        return "ui-cad-and-site";
    }

    if has("lint")
        || has("blaze-fmt")
        || has("blaze-sim")
        || has("fuzz")
        || has("bench")
        || has("profile")
        || has("trace")
        || has("meter")
        || has("watch")
        || has("report")
        || has("astlog")
        || has("smoke")
        || has("test")
        || has("invariant")
        || has("proof")
    {
        return "lint-test-and-obs";
    }

    "other"
}

// ---------------------------------------------------------------------------
// Incremental Hashing & Parsers
// ---------------------------------------------------------------------------

fn hash_crate_snapshot(snap: &CrateSnapshot<'_>, excluded_paths: &[&str]) -> [u8; 32] {
    let is_excluded = excluded_paths.iter().any(|&p| p == snap.dir);
    let manifest_hash = blaze_blake3::hash(snap.cargo_toml);
    let entry_hash = match snap.entry_bytes {
        Some(b) => blaze_blake3::hash(b),
        None => [0u8; 32],
    };
    let mut combined = [0u8; 65];
    combined[..32].copy_from_slice(&manifest_hash);
    combined[32..64].copy_from_slice(&entry_hash);
    combined[64] = u8::from(is_excluded);
    blaze_blake3::hash(&combined)
}

fn parse_crate<'a>(
    snap: &CrateSnapshot<'_>,
    excluded_paths: &[&str],
    hash: [u8; 32],
    alloc: Alloc<'a>,
) -> CrateNode<'a> {
    let fallback_name = snap.dir.rsplit('/').next().unwrap_or(snap.dir);
    let mut name = AString::from_str_in(fallback_name, alloc);
    let mut description = AString::new_in(alloc);
    let mut deps: ABTreeSet<'a, AString<'a>> = ABTreeSet::new_in(alloc);
    let mut design_docs: ABTreeSet<'a, AString<'a>> = ABTreeSet::new_in(alloc);

    if let Ok(toml_text) = core::str::from_utf8(snap.cargo_toml) {
        extract_doc_paths_from_text(toml_text, &mut design_docs, alloc);
        if let Ok(doc) = blaze_toml::Document::parse_in(toml_text, blaze_toml::Limits::CARGO, alloc) {
            let root = doc.root();
            if let Some(pkg) = root.get("package") {
                if let Some(n) = pkg.get("name").and_then(|v| v.as_str()) {
                    name = AString::from_str_in(n, alloc);
                }
                if let Some(d) = pkg.get("description").and_then(|v| v.as_str()) {
                    description = clean_single_line(d, 220, alloc);
                }
            }
            if let Some(dep_tbl) = root.get("dependencies") {
                for child in dep_tbl.children() {
                    let k = child.key();
                    if !k.is_empty() {
                        deps.insert(AString::from_str_in(k, alloc));
                    }
                }
            }
        }
    }

    let is_excluded = excluded_paths.iter().any(|&p| p == snap.dir);
    let status = if contains_ascii_ci(description.as_bytes(), b"experimental")
        || contains_ascii_ci(name.as_bytes(), b"hvbox")
    {
        CrateStatus::Experimental
    } else if is_excluded {
        CrateStatus::Parked
    } else {
        CrateStatus::Live
    };

    let entry_file = match snap.entry_path {
        Some(p) => AString::from_str_in(p, alloc),
        None => format_in!(alloc, "{}/Cargo.toml", snap.dir),
    };

    let mut entry_lines = 1u32;
    let mut exports = AVec::new_in(alloc);
    let mut entry_doc_summary = AString::new_in(alloc);

    if let Some(bytes) = snap.entry_bytes
        && let Ok(src) = core::str::from_utf8(bytes)
    {
        let mut line_num = 0u32;
        for line in src.lines() {
            line_num = line_num.saturating_add(1);
            let trimmed = line.trim();
            if let Some(doc_line) = trimmed.strip_prefix("//!") {
                let cleaned = doc_line.trim();
                extract_doc_paths_from_text(cleaned, &mut design_docs, alloc);
                if entry_doc_summary.is_empty() && !cleaned.is_empty() && !cleaned.starts_with('#') {
                    entry_doc_summary = clean_single_line(cleaned, 220, alloc);
                }
            }
            if exports.len() < MAX_EXPORTS_PER_CRATE
                && let Some((kind, item_name)) = extract_rust_pub_item(trimmed)
            {
                exports.push(ExportItem {
                    kind: AString::from_str_in(kind, alloc),
                    name: AString::from_str_in(item_name, alloc),
                    line: line_num,
                });
            }
        }
        entry_lines = line_num.max(1);
    }

    let summary = if !description.is_empty() {
        description
    } else if !entry_doc_summary.is_empty() {
        entry_doc_summary
    } else {
        format_in!(alloc, "workspace crate {}", name.as_str())
    };

    let domain = AString::from_str_in(classify_domain(name.as_str()), alloc);
    let id = format_in!(alloc, "crate:{}", name.as_str());

    let mut deps_vec = AVec::with_capacity_in(deps.len(), alloc);
    for d in deps {
        deps_vec.push(d);
    }
    let mut docs_vec = AVec::with_capacity_in(design_docs.len(), alloc);
    for d in design_docs {
        docs_vec.push(d);
    }

    CrateNode {
        id,
        name,
        path: AString::from_str_in(snap.dir, alloc),
        entry_file,
        entry_lines,
        status,
        domain,
        summary,
        design_docs: docs_vec,
        deps: deps_vec,
        exports,
        hash,
    }
}

/// Extracts top-level `pub ...` item declarations from a trimmed Rust source line.
fn extract_rust_pub_item(trimmed: &str) -> Option<(&'static str, &str)> {
    let rest = trimmed.strip_prefix("pub ")?;
    // Skip `pub(crate)`, `pub(super)`, etc. — only public exports.
    if rest.starts_with('(') {
        return None;
    }
    let rest = rest.trim_start();
    let (kind, after) = if let Some(r) = rest.strip_prefix("struct ") {
        ("struct", r)
    } else if let Some(r) = rest.strip_prefix("enum ") {
        ("enum", r)
    } else if let Some(r) = rest.strip_prefix("trait ") {
        ("trait", r)
    } else if let Some(r) = rest.strip_prefix("unsafe trait ") {
        ("trait", r)
    } else if let Some(r) = rest.strip_prefix("fn ") {
        ("fn", r)
    } else if let Some(r) = rest.strip_prefix("const fn ") {
        ("fn", r)
    } else if let Some(r) = rest.strip_prefix("unsafe fn ") {
        ("fn", r)
    } else if let Some(r) = rest.strip_prefix("async fn ") {
        ("fn", r)
    } else if let Some(r) = rest.strip_prefix("type ") {
        ("type", r)
    } else if let Some(r) = rest.strip_prefix("mod ") {
        ("mod", r)
    } else if let Some(r) = rest.strip_prefix("const ") {
        ("const", r)
    } else if let Some(r) = rest.strip_prefix("static ") {
        ("static", r)
    } else if let Some(r) = rest.strip_prefix("use ") {
        let ident = extract_pub_use_target(r)?;
        return Some(("use", ident));
    } else {
        return None;
    };

    let name = take_rust_ident(after.trim_start())?;
    Some((kind, name))
}

fn take_rust_ident(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let first = *bytes.first()?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    let mut end = 1;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    Some(&s[..end])
}

fn extract_pub_use_target(after_use: &str) -> Option<&str> {
    let clean = after_use.trim().trim_end_matches(';').trim();
    if clean.is_empty() || clean.starts_with('{') || clean.contains('*') {
        return None;
    }
    // For `pub use foo::Bar;` or `pub use foo::{A, B};`, capture the module or leaf name.
    if let Some(brace) = clean.find("::{") {
        let prefix = clean[..brace].rsplit("::").next().unwrap_or(&clean[..brace]);
        return take_rust_ident(prefix);
    }
    if let Some(pos) = clean.find(" as ") {
        return take_rust_ident(clean[pos + 4..].trim());
    }
    let leaf = clean.rsplit("::").next().unwrap_or(clean);
    take_rust_ident(leaf)
}

fn parse_doc<'a>(snap: &FileSnapshot<'_>, hash: [u8; 32], alloc: Alloc<'a>) -> DocNode<'a> {
    let text = core::str::from_utf8(snap.bytes).unwrap_or("");
    let mut lines: AVec<'a, &str> = AVec::new_in(alloc);
    for l in text.lines() {
        lines.push(l);
    }
    let line_count = (lines.len() as u32).max(1);

    let mut status = DocStatus::Unmarked;
    let mut date: Option<AString<'a>> = None;
    let mut superseded_by: Option<AString<'a>> = None;
    let mut fm_summary = AString::new_in(alloc);

    // 1. Parse YAML front matter if line 0 is `---`.
    let mut body_start = 0usize;
    if lines.first().is_some_and(|l| l.trim() == "---") {
        let mut idx = 1usize;
        while idx < lines.len() {
            let trimmed = lines[idx].trim();
            if trimmed == "---" {
                body_start = idx + 1;
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = k.trim();
                let val = unquote_yaml(v.trim());
                match key {
                    "status" => {
                        if let Some(st) = DocStatus::from_label(val) {
                            status = st;
                        }
                    }
                    "summary" => {
                        fm_summary = clean_single_line(val, 260, alloc);
                    }
                    "date" if !val.is_empty() => {
                        date = Some(AString::from_str_in(val, alloc));
                    }
                    "superseded_by" if !val.is_empty() => {
                        superseded_by = Some(AString::from_str_in(val, alloc));
                    }
                    _ => {}
                }
            }
            idx += 1;
        }
    }

    // 2. Walk lines tracking fenced code blocks to extract headings, excerpts, crate mentions, and links.
    let mut in_fence = false;
    let mut fence_char = b'`';
    let mut headings: AVec<'a, HeadingSlice<'a>> = AVec::new_in(alloc);
    let mut title = AString::new_in(alloc);
    let mut first_para = AString::new_in(alloc);
    let mut mentioned_crates: ABTreeSet<'a, AString<'a>> = ABTreeSet::new_in(alloc);
    let mut linked_docs: ABTreeSet<'a, AString<'a>> = ABTreeSet::new_in(alloc);
    let mut body_terms = AString::new_in(alloc);

    for (idx, &raw_line) in lines.as_slice().iter().enumerate().skip(body_start) {
        let line_no = (idx + 1) as u32;
        let trimmed = raw_line.trim();

        // Check code fence toggle (``` or ~~~).
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let ch = trimmed.as_bytes()[0];
            if !in_fence {
                in_fence = true;
                fence_char = ch;
            } else if ch == fence_char {
                in_fence = false;
            }
            continue;
        }
        if in_fence {
            continue;
        }

        extract_crate_mentions(raw_line, &mut mentioned_crates, alloc);
        extract_doc_links_from_markdown(snap.path, raw_line, &mut linked_docs, alloc);

        if let Some((level, heading_text)) = parse_md_heading(trimmed) {
            if title.is_empty() && level == 1 {
                title = clean_single_line(heading_text, 160, alloc);
            }
            headings.push(HeadingSlice {
                level,
                title: clean_single_line(heading_text, 140, alloc),
                start_line: line_no,
                end_line: line_count,
                excerpt: AString::new_in(alloc),
            });
            continue;
        }

        if !trimmed.is_empty() && !trimmed.starts_with('|') && !trimmed.starts_with("<!--") {
            if first_para.is_empty() && !trimmed.starts_with('>') {
                first_para = clean_single_line(trimmed, 240, alloc);
            }
            if let Some(last_h) = headings.last_mut()
                && last_h.excerpt.len() < MAX_HEADING_EXCERPT_BYTES
            {
                append_cleaned_fragment(&mut last_h.excerpt, trimmed, MAX_HEADING_EXCERPT_BYTES);
            }
            if body_terms.len() < MAX_DOC_BODY_BYTES {
                append_cleaned_fragment(&mut body_terms, trimmed, MAX_DOC_BODY_BYTES);
            }
        }
    }

    // 3. Compute exact end_line for every heading (up to the line before the next heading of <= level).
    let h_len = headings.len();
    for i in 0..h_len {
        let my_level = headings[i].level;
        let mut end = line_count;
        for j in (i + 1)..h_len {
            if headings[j].level <= my_level {
                end = headings[j].start_line.saturating_sub(1).max(headings[i].start_line);
                break;
            }
        }
        headings[i].end_line = end;
    }

    if title.is_empty() {
        let stem = snap.path.rsplit('/').next().unwrap_or(snap.path);
        title = AString::from_str_in(stem.strip_suffix(".md").unwrap_or(stem), alloc);
    }

    let summary = if !fm_summary.is_empty() {
        fm_summary
    } else if !first_para.is_empty() {
        first_para
    } else {
        AString::from_str_in(title.as_str(), alloc)
    };

    // Infer domain from path, then title, then first mentioned crate.
    let mut dom = classify_domain(snap.path);
    if dom == "other" {
        dom = classify_domain(title.as_str());
    }
    if dom == "other"
        && let Some(first_c) = mentioned_crates.iter().next()
    {
        dom = classify_domain(first_c.as_str());
    }

    let mut crates_vec = AVec::with_capacity_in(mentioned_crates.len(), alloc);
    for c in mentioned_crates {
        crates_vec.push(c);
    }
    let mut linked_vec = AVec::with_capacity_in(linked_docs.len(), alloc);
    for l in linked_docs {
        if l.as_str() != snap.path {
            linked_vec.push(l);
        }
    }

    DocNode {
        id: format_in!(alloc, "doc:{}", snap.path),
        path: AString::from_str_in(snap.path, alloc),
        status,
        date,
        superseded_by,
        domain: AString::from_str_in(dom, alloc),
        title,
        summary,
        line_count,
        headings,
        mentioned_crates: crates_vec,
        linked_docs: linked_vec,
        body_terms,
        hash,
    }
}

fn parse_md_heading(trimmed: &str) -> Option<(u8, &str)> {
    let bytes = trimmed.as_bytes();
    let mut hashes = 0usize;
    while hashes < bytes.len() && hashes < 6 && bytes[hashes] == b'#' {
        hashes += 1;
    }
    if hashes == 0 || hashes >= bytes.len() || bytes[hashes] != b' ' {
        return None;
    }
    let text = trimmed[hashes..].trim().trim_end_matches('#').trim();
    if text.is_empty() {
        return None;
    }
    Some((hashes as u8, text))
}

fn parse_agent_note<'a>(snap: &FileSnapshot<'_>, kind: NoteKind, hash: [u8; 32], alloc: Alloc<'a>) -> AgentNoteNode<'a> {
    let text = core::str::from_utf8(snap.bytes).unwrap_or("");
    let line_count = (text.lines().count() as u32).max(1);
    let stem = snap
        .path
        .rsplit('/')
        .next()
        .unwrap_or(snap.path)
        .strip_suffix(".json")
        .unwrap_or(snap.path);

    let mut note_id = AString::from_str_in(stem, alloc);
    let mut date: Option<AString<'a>> = None;
    let mut state: Option<AString<'a>> = None;
    let mut area = AString::new_in(alloc);
    let mut summary = AString::new_in(alloc);
    let mut detail = AString::new_in(alloc);
    let mut tags: ABTreeSet<'a, AString<'a>> = ABTreeSet::new_in(alloc);

    if let Ok(Value::Map(ref map)) = blaze_json::read_in(snap.bytes, blaze_json::Limits::SHIPPED, alloc) {
        if let Some(v) = json_str(&map, "id") {
            note_id = AString::from_str_in(v, alloc);
        }
        if let Some(v) = json_str(&map, "date") {
            date = Some(AString::from_str_in(v, alloc));
        }
        if let Some(v) = json_str(&map, "state") {
            state = Some(normalize_state_badge(v, alloc));
        }
        if let Some(v) = json_str(&map, "area").or_else(|| json_str(&map, "where")).or_else(|| json_str(&map, "lane")) {
            area = clean_single_line(v, 160, alloc);
        }

        // Primary summary & detail depend on note kind / schema variant.
        let primary = match kind {
            NoteKind::Howto => json_str(&map, "task").or_else(|| json_str(&map, "do")),
            NoteKind::Annoyance => json_str(&map, "what").or_else(|| json_str(&map, "annoyance")),
        };
        if let Some(p) = primary {
            summary = clean_single_line(p, 220, alloc);
        }

        for field in ["task", "do", "why", "what", "annoyance", "cause", "workaround", "area", "where", "cost"] {
            if let Some(val) = json_str(&map, field) {
                extract_crate_mentions(val, &mut tags, alloc);
                extract_doc_paths_from_text(val, &mut tags, alloc);
                if detail.len() < MAX_NOTE_DETAIL_BYTES {
                    append_cleaned_fragment(&mut detail, val, MAX_NOTE_DETAIL_BYTES);
                }
            }
        }
    }

    if summary.is_empty() {
        summary = AString::from_str_in(stem, alloc);
    }

    // Also inspect filename stem for crate names (e.g. `blaze-lock-...`).
    extract_crate_mentions(stem, &mut tags, alloc);

    let mut dom = classify_domain(area.as_str());
    if dom == "other" {
        dom = classify_domain(summary.as_str());
    }
    if dom == "other"
        && let Some(first_tag) = tags.iter().next()
    {
        dom = classify_domain(first_tag.as_str());
    }

    let mut tags_vec = AVec::with_capacity_in(tags.len(), alloc);
    for t in tags {
        tags_vec.push(t);
    }

    AgentNoteNode {
        id: format_in!(alloc, "note:{}:{}", kind.as_str(), stem),
        path: AString::from_str_in(snap.path, alloc),
        kind,
        note_id,
        date,
        state,
        area,
        domain: AString::from_str_in(dom, alloc),
        summary,
        detail,
        tags: tags_vec,
        line_count,
        hash,
    }
}

fn json_str<'m, A: core::alloc::Allocator>(map: &'m Map<A>, key: &str) -> Option<&'m str> {
    match map.get(key) {
        Some(Value::Text(s)) if !s.is_empty() => Some(s.as_str()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Domain Roll-up Builder (Level 0)
// ---------------------------------------------------------------------------

fn build_domains<'a>(
    crates: &[CrateNode<'_>],
    docs: &[DocNode<'_>],
    notes: &[AgentNoteNode<'_>],
    edges: &[GraphEdge<'_>],
    alloc: Alloc<'a>,
) -> AVec<'a, DomainSummary<'a>> {
    // Compute in-degree for crate centrality ranking within each domain.
    let mut in_degree: ABTreeMap<'a, AString<'a>, u32> = ABTreeMap::new_in(alloc);
    for e in edges {
        if e.kind == EdgeKind::DependsOn {
            if let Some(cnt) = in_degree.get_mut(e.to.as_str()) {
                *cnt = cnt.saturating_add(1);
            } else {
                in_degree.insert(AString::from_str_in(e.to.as_str(), alloc), 1);
            }
        }
    }

    struct Acc<'b> {
        crates: AVec<'b, (u32, AString<'b>)>,
        live: u32,
        parked: u32,
        experimental: u32,
        docs: AVec<'b, (u8, AString<'b>)>,
        note_count: u32,
    }

    let mut by_domain: ABTreeMap<'a, AString<'a>, Acc<'a>> = ABTreeMap::new_in(alloc);
    let ensure = |map: &mut ABTreeMap<'a, AString<'a>, Acc<'a>>, dom: &str| {
        if !map.contains_key(dom) {
            map.insert(
                AString::from_str_in(dom, alloc),
                Acc {
                    crates: AVec::new_in(alloc),
                    live: 0,
                    parked: 0,
                    experimental: 0,
                    docs: AVec::new_in(alloc),
                    note_count: 0,
                },
            );
        }
    };

    for c in crates {
        ensure(&mut by_domain, c.domain.as_str());
        if let Some(acc) = by_domain.get_mut(c.domain.as_str()) {
            match c.status {
                CrateStatus::Live => acc.live += 1,
                CrateStatus::Parked => acc.parked += 1,
                CrateStatus::Experimental => acc.experimental += 1,
            }
            let deg = in_degree.get(c.id.as_str()).copied().unwrap_or(0);
            acc.crates.push((deg, AString::from_str_in(c.name.as_str(), alloc)));
        }
    }
    for d in docs {
        ensure(&mut by_domain, d.domain.as_str());
        if let Some(acc) = by_domain.get_mut(d.domain.as_str()) {
            let prio = match d.status {
                DocStatus::Current => 0,
                DocStatus::Plan => 1,
                DocStatus::Unmarked => 2,
                DocStatus::Record => 3,
                DocStatus::Superseded => 4,
            };
            acc.docs.push((prio, AString::from_str_in(d.path.as_str(), alloc)));
        }
    }
    for n in notes {
        ensure(&mut by_domain, n.domain.as_str());
        if let Some(acc) = by_domain.get_mut(n.domain.as_str()) {
            acc.note_count += 1;
        }
    }

    let mut out = AVec::with_capacity_in(by_domain.len(), alloc);
    for (domain, mut acc) in by_domain {
        acc.crates
            .sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.as_str().cmp(b.1.as_str())));
        let mut key_crates = AVec::new_in(alloc);
        for (_, name) in acc.crates.as_slice().iter().take(6) {
            key_crates.push(AString::from_str_in(name.as_str(), alloc));
        }

        acc.docs
            .sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.as_str().cmp(b.1.as_str())));
        let mut key_docs = AVec::new_in(alloc);
        for (_, path) in acc.docs.as_slice().iter().take(4) {
            key_docs.push(AString::from_str_in(path.as_str(), alloc));
        }

        out.push(DomainSummary {
            domain,
            crate_count: acc.crates.len() as u32,
            live_crates: acc.live,
            parked_crates: acc.parked,
            experimental_crates: acc.experimental,
            doc_count: acc.docs.len() as u32,
            note_count: acc.note_count,
            key_crates,
            key_docs,
        });
    }

    out.sort_by(|a, b| {
        b.crate_count
            .cmp(&a.crate_count)
            .then_with(|| b.doc_count.cmp(&a.doc_count))
            .then_with(|| a.domain.as_str().cmp(b.domain.as_str()))
    });
    out
}

// ---------------------------------------------------------------------------
// Text / Link Extraction Helpers
// ---------------------------------------------------------------------------

fn unquote_yaml(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn normalize_state_badge<'a>(raw: &str, alloc: Alloc<'a>) -> AString<'a> {
    let trimmed = raw.trim();
    // Extract concise status badge (first lowercase identifier word before ':', ';', whitespace, or '(').
    let end = trimmed
        .bytes()
        .take_while(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        .count()
        .min(18);
    if end == 0 {
        return AString::from_str_in("note", alloc);
    }
    let mut out = AString::with_capacity_in(end, alloc);
    for b in &trimmed.as_bytes()[..end] {
        out.push(b.to_ascii_lowercase() as char);
    }
    out
}

fn clean_single_line<'a>(s: &str, max_bytes: usize, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::new_in(alloc);
    let mut prev_space = true;
    let mut truncated = false;
    for ch in s.chars() {
        let normalized = if ch.is_whitespace() { ' ' } else { ch };
        if normalized == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        if out.len() + normalized.len_utf8() > max_bytes {
            truncated = true;
            break;
        }
        out.push(normalized);
    }
    if truncated && max_bytes >= 12 {
        // Back up to last word boundary to avoid clipped half-words, then add "...".
        let target_len = max_bytes.saturating_sub(3);
        while out.len() > target_len {
            out.pop();
        }
        if let Some(last_sp) = out.as_str().rfind(' ')
            && last_sp >= target_len / 2
        {
            while out.len() > last_sp {
                out.pop();
            }
        }
        while out.as_str().ends_with(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':')) {
            out.pop();
        }
        out.push_str("...");
    } else {
        while out.as_str().ends_with(' ') {
            out.pop();
        }
    }
    out
}

fn append_cleaned_fragment(dst: &mut AString<'_>, src: &str, max_bytes: usize) {
    if !dst.is_empty() && dst.len() + 1 < max_bytes {
        dst.push(' ');
    }
    let mut prev_space = true;
    for ch in src.chars() {
        let c = if ch.is_whitespace() { ' ' } else { ch };
        if c == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        if dst.len() + c.len_utf8() > max_bytes {
            break;
        }
        dst.push(c);
    }
}

fn contains_ascii_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    for win in haystack.windows(needle.len()) {
        if win
            .iter()
            .zip(needle.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
        {
            return true;
        }
    }
    false
}

/// Scans arbitrary text for `docs/.../*.md` repo-relative paths.
fn extract_doc_paths_from_text<'a>(text: &str, out: &mut ABTreeSet<'a, AString<'a>>, alloc: Alloc<'a>) {
    let mut rest = text;
    while let Some(pos) = rest.find("docs/") {
        let candidate = &rest[pos..];
        let mut end = 0usize;
        for (i, ch) in candidate.char_indices() {
            if ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_' | '.') {
                end = i + ch.len_utf8();
            } else {
                break;
            }
        }
        let slice = candidate[..end].trim_end_matches('.');
        if slice.ends_with(".md") {
            out.insert(AString::from_str_in(slice, alloc));
        }
        rest = &candidate[end.max(5)..];
    }
}

/// Scans markdown text for both inline `[label](target.md#anchor)` links and bare `docs/.../*.md` paths.
fn extract_doc_links_from_markdown<'a>(
    doc_path: &str,
    line: &str,
    out: &mut ABTreeSet<'a, AString<'a>>,
    alloc: Alloc<'a>,
) {
    extract_doc_paths_from_text(line, out, alloc);

    let mut rest = line;
    while let Some(open) = rest.find("](") {
        let after = &rest[open + 2..];
        if let Some(close) = after.find(')') {
            let raw_target = after[..close].trim();
            let target = raw_target.split('#').next().unwrap_or(raw_target).trim();
            if target.ends_with(".md") && !target.starts_with("http://") && !target.starts_with("https://") {
                let resolved = resolve_relative_path(doc_path, target, alloc);
                if resolved.as_str().starts_with("docs/") {
                    out.insert(resolved);
                }
            }
            rest = &after[close + 1..];
        } else {
            break;
        }
    }
}

/// Resolves a relative markdown link against `base_file`'s parent directory.
fn resolve_relative_path<'a>(base_file: &str, rel: &str, alloc: Alloc<'a>) -> AString<'a> {
    if rel.starts_with("docs/") {
        return AString::from_str_in(rel, alloc);
    }
    let mut parts: AVec<'a, &str> = AVec::new_in(alloc);
    if let Some((dir, _)) = base_file.rsplit_once('/') {
        for seg in dir.split('/') {
            if !seg.is_empty() && seg != "." {
                parts.push(seg);
            }
        }
    }
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            parts.pop();
        } else {
            parts.push(seg);
        }
    }
    let mut out = AString::new_in(alloc);
    for (i, seg) in parts.as_slice().iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(seg);
    }
    out
}

/// Scans text for `crates/<name>` and `blaze-<name>` crate mentions.
fn extract_crate_mentions<'a>(text: &str, out: &mut ABTreeSet<'a, AString<'a>>, alloc: Alloc<'a>) {
    // 1. Explicit `crates/<dir>` references.
    let mut rest = text;
    while let Some(pos) = rest.find("crates/") {
        let after = &rest[pos + 7..];
        let end = after
            .bytes()
            .take_while(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            .count();
        if end >= 2 {
            out.insert(AString::from_str_in(&after[..end], alloc));
        }
        rest = &after[end.max(1)..];
    }

    // 2. Explicit `blaze-<name>` tokens (hyphenated crate names).
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i + 6 < bytes.len() {
        if bytes[i..].starts_with(b"blaze-")
            && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'-' || bytes[i - 1] == b'_'))
        {
            let mut end = i + 6;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'-') {
                end += 1;
            }
            let cand = text[i..end].trim_end_matches('-');
            if cand.len() > 6 {
                out.insert(AString::from_str_in(cand, alloc));
            }
            i = end;
        } else {
            i += 1;
        }
    }
}

fn clone_str_vec<'a>(src: &[AString<'_>], alloc: Alloc<'a>) -> AVec<'a, AString<'a>> {
    let mut out = AVec::with_capacity_in(src.len(), alloc);
    for s in src {
        out.push(AString::from_str_in(s.as_str(), alloc));
    }
    out
}

// ---------------------------------------------------------------------------
// Value <-> Node Serialization (CBOR cache)
// ---------------------------------------------------------------------------

fn str_list_to_value<'a>(list: &[AString<'_>], alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut out = AVec::with_capacity_in(list.len(), alloc);
    for s in list {
        out.push(Value::Text(AString::from_str_in(s.as_str(), alloc)));
    }
    Value::List(out)
}

fn hash_to_value<'a>(hash: &[u8; 32], alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut v = AVec::with_capacity_in(32, alloc);
    v.extend_from_slice(hash);
    Value::Bytes(v)
}

fn crate_to_value<'a>(c: &CrateNode<'_>, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut m = Map::with_capacity_in(12, alloc);
    m.insert(AString::from_str_in("id", alloc), Value::Text(AString::from_str_in(c.id.as_str(), alloc)));
    m.insert(AString::from_str_in("name", alloc), Value::Text(AString::from_str_in(c.name.as_str(), alloc)));
    m.insert(AString::from_str_in("path", alloc), Value::Text(AString::from_str_in(c.path.as_str(), alloc)));
    m.insert(AString::from_str_in("entry_file", alloc), Value::Text(AString::from_str_in(c.entry_file.as_str(), alloc)));
    m.insert(AString::from_str_in("entry_lines", alloc), Value::Int(Int::from_u64(u64::from(c.entry_lines))));
    m.insert(AString::from_str_in("status", alloc), Value::Text(AString::from_str_in(c.status.as_str(), alloc)));
    m.insert(AString::from_str_in("domain", alloc), Value::Text(AString::from_str_in(c.domain.as_str(), alloc)));
    m.insert(AString::from_str_in("summary", alloc), Value::Text(AString::from_str_in(c.summary.as_str(), alloc)));
    m.insert(AString::from_str_in("design_docs", alloc), str_list_to_value(c.design_docs.as_slice(), alloc));
    m.insert(AString::from_str_in("deps", alloc), str_list_to_value(c.deps.as_slice(), alloc));
    let mut exps = AVec::with_capacity_in(c.exports.len(), alloc);
    for ex in c.exports.as_slice() {
        let mut em = Map::with_capacity_in(3, alloc);
        em.insert(AString::from_str_in("kind", alloc), Value::Text(AString::from_str_in(ex.kind.as_str(), alloc)));
        em.insert(AString::from_str_in("name", alloc), Value::Text(AString::from_str_in(ex.name.as_str(), alloc)));
        em.insert(AString::from_str_in("line", alloc), Value::Int(Int::from_u64(u64::from(ex.line))));
        exps.push(Value::Map(em));
    }
    m.insert(AString::from_str_in("exports", alloc), Value::List(exps));
    m.insert(AString::from_str_in("hash", alloc), hash_to_value(&c.hash, alloc));
    Value::Map(m)
}

fn doc_to_value<'a>(d: &DocNode<'_>, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut m = Map::with_capacity_in(14, alloc);
    m.insert(AString::from_str_in("id", alloc), Value::Text(AString::from_str_in(d.id.as_str(), alloc)));
    m.insert(AString::from_str_in("path", alloc), Value::Text(AString::from_str_in(d.path.as_str(), alloc)));
    m.insert(AString::from_str_in("status", alloc), Value::Text(AString::from_str_in(d.status.as_str(), alloc)));
    if let Some(date) = &d.date {
        m.insert(AString::from_str_in("date", alloc), Value::Text(AString::from_str_in(date.as_str(), alloc)));
    }
    if let Some(sup) = &d.superseded_by {
        m.insert(AString::from_str_in("superseded_by", alloc), Value::Text(AString::from_str_in(sup.as_str(), alloc)));
    }
    m.insert(AString::from_str_in("domain", alloc), Value::Text(AString::from_str_in(d.domain.as_str(), alloc)));
    m.insert(AString::from_str_in("title", alloc), Value::Text(AString::from_str_in(d.title.as_str(), alloc)));
    m.insert(AString::from_str_in("summary", alloc), Value::Text(AString::from_str_in(d.summary.as_str(), alloc)));
    m.insert(AString::from_str_in("line_count", alloc), Value::Int(Int::from_u64(u64::from(d.line_count))));
    let mut hs = AVec::with_capacity_in(d.headings.len(), alloc);
    for h in d.headings.as_slice() {
        let mut hm = Map::with_capacity_in(5, alloc);
        hm.insert(AString::from_str_in("level", alloc), Value::Int(Int::from_u64(u64::from(h.level))));
        hm.insert(AString::from_str_in("title", alloc), Value::Text(AString::from_str_in(h.title.as_str(), alloc)));
        hm.insert(AString::from_str_in("start_line", alloc), Value::Int(Int::from_u64(u64::from(h.start_line))));
        hm.insert(AString::from_str_in("end_line", alloc), Value::Int(Int::from_u64(u64::from(h.end_line))));
        hm.insert(AString::from_str_in("excerpt", alloc), Value::Text(AString::from_str_in(h.excerpt.as_str(), alloc)));
        hs.push(Value::Map(hm));
    }
    m.insert(AString::from_str_in("headings", alloc), Value::List(hs));
    m.insert(AString::from_str_in("mentioned_crates", alloc), str_list_to_value(d.mentioned_crates.as_slice(), alloc));
    m.insert(AString::from_str_in("linked_docs", alloc), str_list_to_value(d.linked_docs.as_slice(), alloc));
    m.insert(AString::from_str_in("body_terms", alloc), Value::Text(AString::from_str_in(d.body_terms.as_str(), alloc)));
    m.insert(AString::from_str_in("hash", alloc), hash_to_value(&d.hash, alloc));
    Value::Map(m)
}

fn note_to_value<'a>(n: &AgentNoteNode<'_>, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut m = Map::with_capacity_in(13, alloc);
    m.insert(AString::from_str_in("id", alloc), Value::Text(AString::from_str_in(n.id.as_str(), alloc)));
    m.insert(AString::from_str_in("path", alloc), Value::Text(AString::from_str_in(n.path.as_str(), alloc)));
    m.insert(AString::from_str_in("kind", alloc), Value::Text(AString::from_str_in(n.kind.as_str(), alloc)));
    m.insert(AString::from_str_in("note_id", alloc), Value::Text(AString::from_str_in(n.note_id.as_str(), alloc)));
    if let Some(date) = &n.date {
        m.insert(AString::from_str_in("date", alloc), Value::Text(AString::from_str_in(date.as_str(), alloc)));
    }
    if let Some(st) = &n.state {
        m.insert(AString::from_str_in("state", alloc), Value::Text(AString::from_str_in(st.as_str(), alloc)));
    }
    m.insert(AString::from_str_in("area", alloc), Value::Text(AString::from_str_in(n.area.as_str(), alloc)));
    m.insert(AString::from_str_in("domain", alloc), Value::Text(AString::from_str_in(n.domain.as_str(), alloc)));
    m.insert(AString::from_str_in("summary", alloc), Value::Text(AString::from_str_in(n.summary.as_str(), alloc)));
    m.insert(AString::from_str_in("detail", alloc), Value::Text(AString::from_str_in(n.detail.as_str(), alloc)));
    m.insert(AString::from_str_in("tags", alloc), str_list_to_value(n.tags.as_slice(), alloc));
    m.insert(AString::from_str_in("line_count", alloc), Value::Int(Int::from_u64(u64::from(n.line_count))));
    m.insert(AString::from_str_in("hash", alloc), hash_to_value(&n.hash, alloc));
    Value::Map(m)
}

fn req_text<'a>(map: &Map<Alloc<'a>>, key: &str, alloc: Alloc<'a>) -> Result<AString<'a>, CacheError<'a>> {
    match map.get(key) {
        Some(Value::Text(s)) => Ok(AString::from_str_in(s.as_str(), alloc)),
        _ => Err(CacheError::Malformed(format_in!(alloc, "missing text field '{key}'"))),
    }
}

fn opt_text<'a>(map: &Map<Alloc<'a>>, key: &str, alloc: Alloc<'a>) -> Option<AString<'a>> {
    match map.get(key) {
        Some(Value::Text(s)) => Some(AString::from_str_in(s.as_str(), alloc)),
        _ => None,
    }
}

fn req_u64<'a>(map: &Map<Alloc<'a>>, key: &str, alloc: Alloc<'a>) -> Result<u64, CacheError<'a>> {
    match map.get(key) {
        Some(Value::Int(n)) => n
            .as_u64()
            .ok_or_else(|| CacheError::Malformed(format_in!(alloc, "negative integer in '{key}'"))),
        _ => Err(CacheError::Malformed(format_in!(alloc, "missing u64 field '{key}'"))),
    }
}

fn req_u32<'a>(map: &Map<Alloc<'a>>, key: &str, alloc: Alloc<'a>) -> Result<u32, CacheError<'a>> {
    let n = req_u64(map, key, alloc)?;
    u32::try_from(n).map_err(|_| CacheError::Malformed(format_in!(alloc, "u32 overflow in '{key}'")))
}

fn req_list<'m, 'a>(map: &'m Map<Alloc<'a>>, key: &str, alloc: Alloc<'a>) -> Result<&'m [Value<Alloc<'a>>], CacheError<'a>> {
    match map.get(key) {
        Some(Value::List(v)) => Ok(v.as_slice()),
        _ => Err(CacheError::Malformed(format_in!(alloc, "missing list field '{key}'"))),
    }
}

fn req_str_list<'a>(map: &Map<Alloc<'a>>, key: &str, alloc: Alloc<'a>) -> Result<AVec<'a, AString<'a>>, CacheError<'a>> {
    let slice = req_list(map, key, alloc)?;
    let mut out = AVec::with_capacity_in(slice.len(), alloc);
    for v in slice {
        let Value::Text(s) = v else {
            return Err(CacheError::Malformed(format_in!(alloc, "non-text entry in '{key}'")));
        };
        out.push(AString::from_str_in(s.as_str(), alloc));
    }
    Ok(out)
}

fn req_hash<'a>(map: &Map<Alloc<'a>>, key: &str, alloc: Alloc<'a>) -> Result<[u8; 32], CacheError<'a>> {
    match map.get(key) {
        Some(Value::Bytes(b)) if b.len() == 32 => {
            let mut out = [0u8; 32];
            out.copy_from_slice(b.as_slice());
            Ok(out)
        }
        _ => Err(CacheError::Malformed(format_in!(alloc, "missing 32-byte hash '{key}'"))),
    }
}

fn crate_from_value<'a>(v: &Value<Alloc<'a>>, alloc: Alloc<'a>) -> Result<CrateNode<'a>, CacheError<'a>> {
    let Value::Map(m) = v else {
        return Err(CacheError::Malformed(AString::from_str_in("crate entry is not a map", alloc)));
    };
    let status_s = req_text(m, "status", alloc)?;
    let status = CrateStatus::from_label(status_s.as_str())
        .ok_or_else(|| CacheError::Malformed(format_in!(alloc, "bad crate status '{}'", status_s.as_str())))?;
    let exps_raw = req_list(m, "exports", alloc)?;
    let mut exports = AVec::with_capacity_in(exps_raw.len(), alloc);
    for item in exps_raw {
        let Value::Map(em) = item else {
            return Err(CacheError::Malformed(AString::from_str_in("export entry is not a map", alloc)));
        };
        exports.push(ExportItem {
            kind: req_text(em, "kind", alloc)?,
            name: req_text(em, "name", alloc)?,
            line: req_u32(em, "line", alloc)?,
        });
    }
    Ok(CrateNode {
        id: req_text(m, "id", alloc)?,
        name: req_text(m, "name", alloc)?,
        path: req_text(m, "path", alloc)?,
        entry_file: req_text(m, "entry_file", alloc)?,
        entry_lines: req_u32(m, "entry_lines", alloc)?,
        status,
        domain: req_text(m, "domain", alloc)?,
        summary: req_text(m, "summary", alloc)?,
        design_docs: req_str_list(m, "design_docs", alloc)?,
        deps: req_str_list(m, "deps", alloc)?,
        exports,
        hash: req_hash(m, "hash", alloc)?,
    })
}

fn doc_from_value<'a>(v: &Value<Alloc<'a>>, alloc: Alloc<'a>) -> Result<DocNode<'a>, CacheError<'a>> {
    let Value::Map(m) = v else {
        return Err(CacheError::Malformed(AString::from_str_in("doc entry is not a map", alloc)));
    };
    let status_s = req_text(m, "status", alloc)?;
    let status = DocStatus::from_label(status_s.as_str())
        .ok_or_else(|| CacheError::Malformed(format_in!(alloc, "bad doc status '{}'", status_s.as_str())))?;
    let hs_raw = req_list(m, "headings", alloc)?;
    let mut headings = AVec::with_capacity_in(hs_raw.len(), alloc);
    for item in hs_raw {
        let Value::Map(hm) = item else {
            return Err(CacheError::Malformed(AString::from_str_in("heading entry is not a map", alloc)));
        };
        let level_u32 = req_u32(hm, "level", alloc)?;
        let level = u8::try_from(level_u32).unwrap_or(6);
        headings.push(HeadingSlice {
            level,
            title: req_text(hm, "title", alloc)?,
            start_line: req_u32(hm, "start_line", alloc)?,
            end_line: req_u32(hm, "end_line", alloc)?,
            excerpt: req_text(hm, "excerpt", alloc)?,
        });
    }
    Ok(DocNode {
        id: req_text(m, "id", alloc)?,
        path: req_text(m, "path", alloc)?,
        status,
        date: opt_text(m, "date", alloc),
        superseded_by: opt_text(m, "superseded_by", alloc),
        domain: req_text(m, "domain", alloc)?,
        title: req_text(m, "title", alloc)?,
        summary: req_text(m, "summary", alloc)?,
        line_count: req_u32(m, "line_count", alloc)?,
        headings,
        mentioned_crates: req_str_list(m, "mentioned_crates", alloc)?,
        linked_docs: req_str_list(m, "linked_docs", alloc)?,
        body_terms: req_text(m, "body_terms", alloc)?,
        hash: req_hash(m, "hash", alloc)?,
    })
}

fn note_from_value<'a>(v: &Value<Alloc<'a>>, alloc: Alloc<'a>) -> Result<AgentNoteNode<'a>, CacheError<'a>> {
    let Value::Map(m) = v else {
        return Err(CacheError::Malformed(AString::from_str_in("note entry is not a map", alloc)));
    };
    let kind_s = req_text(m, "kind", alloc)?;
    let kind = NoteKind::from_label(kind_s.as_str())
        .ok_or_else(|| CacheError::Malformed(format_in!(alloc, "bad note kind '{}'", kind_s.as_str())))?;
    Ok(AgentNoteNode {
        id: req_text(m, "id", alloc)?,
        path: req_text(m, "path", alloc)?,
        kind,
        note_id: req_text(m, "note_id", alloc)?,
        date: opt_text(m, "date", alloc),
        state: opt_text(m, "state", alloc),
        area: req_text(m, "area", alloc)?,
        domain: req_text(m, "domain", alloc)?,
        summary: req_text(m, "summary", alloc)?,
        detail: req_text(m, "detail", alloc)?,
        tags: req_str_list(m, "tags", alloc)?,
        line_count: req_u32(m, "line_count", alloc)?,
        hash: req_hash(m, "hash", alloc)?,
    })
}
