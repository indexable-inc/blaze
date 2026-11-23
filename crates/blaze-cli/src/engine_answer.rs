//! `engine-check`'s stored answer (engine.md E6, the incremental half): the engine's answer and
//! causes as pages (blaze-engine pages.rs), the leaf table of what the request read (sweep.rs
//! `Leaves`), and a sealed pointer naming both (seal.rs), under one root of the store per
//! checkout ([`root_name`]).
//!
//! THE TOOLS are not in the root's name: the answer is one per checkout whichever blaze wrote it.
//! Every row's key holds its own kind's producer (blaze-engine `Tools::key`), and each file's
//! plan digest holds every kind's (pages.rs `plan_digest`), so a request under other tools reads
//! the same answer: a file whose plan digest moved is in its edit, a task whose key moved is
//! entered, and the rest stand by their keys and reads (a moved names solver enters the names,
//! and a body whose scopes came out the same is not entered). Only the certificate, which reads
//! no row, needs the tools: the pointer keeps their digest ([`tools_digest`]), and it stands on
//! equal tools alone. Before (BEL6), the root hashed the tools, so every new blaze build (every
//! producer that moved) started from no answer and entered every task: a whole check of tempo
//! per new binary.
//!
//! Each changed page gets a CAS reach index containing its key, row payloads, and the
//! expansion records its names rows own. The checkout pins the sealed pointer alone,
//! and `<name>.pages` pins the root, printed lines and all live page indexes. The store's
//! generation flip publishes both roots together (pages_sim.rs).
//!
//! The read, before any work: the root name's one key is the sealed pointer; the sweep
//! follows its leaf table; nothing moved, nothing owed and no findings kept is the certificate,
//! from the pointer and the printed lines' record (engine_lines.rs, one get), no page read.
//! Else [`open_root`] reads the directory. [`Stored`] authenticates a page on its first
//! lookup. All consumers share that page's proof for the rest of the request. A refused
//! page stops the edit. A pointer of an older form is reported and rebuilt (rule 26).
//!
//! ```text
//! the root name   engine.<16 hex of H(checkout)>
//! it names        the sealed pointer's key; `<it>.pages` reaches the root, lines and page
//!                 indexes, with their payloads
//! sealed pointer  seal.rs form: last = "BEL8" | answer root key [32] | lines key [32] |
//!                 files u32 | printed u8 (bit 0 names checked, 1 bodies whole, 2 every
//!                 printed line is in the lines record) | findings u32 | owed u32 (the
//!                 certificate's facts) | tools digest [32] | headers u8 (0 current, 1
//!                 absent) | since u64 | 128 x producer header shard key [32] (all zero when
//!                 absent) | the journal's places; leaves = the BCL table
//! ```
//!
//! BEL6 had no tools digest: its root's name held the tools. A BEL6 pointer is an old home's
//! answer, reported and rebuilt ([`Unreadable::Older`]).
//!
//! BEL4 named pages without stored reverse-reader runs or page-local payload reach indexes.
//! BEL5 named 128 shard keys and no state: it could not say that the `Ready` driver's commit
//! (D187: pages alone, no shard read or written) left the shards behind the pages, so a BEL5
//! pointer is an old home's answer, reported and rebuilt ([`Unreadable::Older`]), never read
//! as BEL6 (the form is the pointer's place, D160: the magic is checked before any byte).

use core::fmt;
use core::fmt::Write as _;
use std::sync::OnceLock;

use blaze_cas::plain::Snapshot;
use blaze_core::Driver;
use blaze_drive::{Key, Name};
use blaze_engine::{StoredPages, Tools};
use blaze_engine::pages::{self, Header, HeaderPages, Headers, HeadersPlant, IndexedHeaders, Kept, PagesError, Root, Sealed, VerifiedPage, Writer};

/// The store's proof that a held record's bytes are its key's (rule 74): what the pages take
/// in place of hashing each page at its open (blaze-engine pages.rs, "Checked once").
impl pages::StoreProof for crate::store::plain_place::Checked<'_> {}
use blaze_ids::ContentId;
use blaze_own::SharedAlloc;
use bstd::Cx;
use bstd::mem::Frame;

use crate::store::{CommandStore, Sharing, Store as _};
use crate::sweep::{Leaves, Rows};
use crate::{CliError, seal};

/// The pointer's form inside its seal.
/// BEL8: a file's lint row is ONE (engine_lints.rs, sparse rows: its units' keys in its record,
/// no row a unit); a BEL7 answer's unit rows are an old home's, reported and rebuilt.
pub(crate) const POINTER_MAGIC: &[u8; 4] = b"BEL8";
/// The magic, the answer's root, the lines record's key, then the [`Facts`]: files u32, printed
/// u8, findings u32, owed u32; then the tools' digest ([`tools_digest`]). The header index's
/// state ([`Headers`]) follows: its tag, its `since`, then every shard's key; then the
/// journal's places, of their own length.
const TOOLS_AT: usize = 4 + 32 + 32 + 4 + 1 + 4 + 4;
const HEADERS_AT: usize = TOOLS_AT + 32;
const SHARDS_AT: usize = HEADERS_AT + 1 + 8;
const POINTER_BYTES: usize = SHARDS_AT + 32 * HeaderPages::COUNT;
const HEADERS_CURRENT: u8 = 0;
const HEADERS_ABSENT: u8 = 1;
const POINTER_DOMAIN: &str = "blaze-cli engine-check pointer";

/// The root the checkout's stored answer hangs from, whichever tools wrote it (the module's doc,
/// "THE TOOLS"; a machine's path is hashed here alone: a root is local, never shared).
pub(crate) fn root_name<'f>(checkout: &str, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    let id = ContentId::of("blaze-cli engine-check root 2", checkout.as_bytes());
    let mut name = blaze_own::String::new_in(frame);
    name.push_str("engine.");
    // A home made before the root stopped hashing the tools holds its answers under names no
    // request asks for again: dead roots, whose records the store's cleanup takes when their
    // pins expire, `ROOT_LIFE_NS` after their last renewal (store/plain_place.rs `pin`).
    // Nothing reads or migrates them (AGENTS.md: an old home's
    // answer is rebuilt, never migrated).
    for byte in &id.0[..8] {
        // A frame string's write fails only when the frame is full: the name is then short and
        // names no root, so the request answers in full.
        let _ = write!(name, "{byte:02x}");
    }
    name
}

/// The control of main.rs tests `a_moved_body_checker_enters_its_bodies_alone` (rule 23):
/// [`RootPlant::PerTools`] reads no stored answer written under other tools, as the root did
/// when its name hashed the tools (BEL6).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RootPlant {
    None,
    PerTools,
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static ROOT_PLANT: core::cell::Cell<RootPlant> = const { core::cell::Cell::new(RootPlant::None) };
}

/// The root that named the home's latest sealed pointer, whichever checkout saved it: since
/// 2026-11-21 only the tests' control ([`BasePlant::OnePointer`]); a checkout with no answer of
/// its own starts from its workspace's base ([`BASES_ROOT`], [`load_base`]). What held of the
/// one pointer holds of a base: a checkout with no answer of its own starts from it: every row
/// is valid by its key and reads against today's facts, and the pointer's leaves are
/// checkout-relative, so the sweep follows them here, every row looked at (its journal places
/// are the other checkout's and are not read), and the certificate never stands on it
/// (engine_check.rs, `StoredBase::OtherCheckout`). Its pages stay reachable while the checkout
/// that wrote it keeps them under its own `<root>.pages`; a page that is gone makes the answer
/// unreadable, reported, and the request answers in full (rule 3: a stated path).
#[cfg_attr(not(test), allow(dead_code, reason = "the bases' test control alone (`BasePlant::OnePointer`)"))]
pub(crate) const LATEST_ROOT: &str = "engine.latest";

// ---- The bases: one list of answers to start from, keyed by the workspace (thoughts2 1.5) ----
//
// Until 2026-11-21 a checkout with no answer of its own started from [`LATEST_ROOT`], the
// home's ONE latest pointer: working in repository B replaced the base a new worktree of
// repository A started from, which then entered every body B's answer did not hold. Now the
// home keeps [`BASES_ROOT`]: one record of up to [`BASES_MOST`] entries (workspace, lock digest,
// layout digest, pointer key), at most [`BASES_PER_WORKSPACE`] a workspace, newest first, and a
// checkout with none of its own starts from the newest entry of its lock digest, else of its
// workspace ([`pick_base`]). The single pointer is kept only as the tests' control
// ([`BasePlant`]).
//
// THE WORKSPACE'S IDENTITY ([`workspace_of_lock`]): the digest of the names of the workspace's
// path packages: a lock's packages with no `source` (cargo writes `source` for every registry
// and git package, none for a path package: the members and the path dependencies in the
// repository), or, for a workspace whose layout reads no lock (nothing outside it, units.rs),
// the members' own lock (graph.rs `members_lock`, the same form). It is equal for two
// worktrees of one repository (one lock, or one with the same members on another branch),
// different for two repositories with different members, holds no path, and moves with no
// checkout's place. Two repositories whose members share every name (two forks, or two `cli`
// + `core` pairs) share one list: harmless, since the pick is by an equal lock digest first,
// and every row of any base is valid by its key and reads against today's facts; such a base
// costs entered tasks, never a wrong answer. Versions are left out: a version bump on a branch
// keeps its workspace.
//
// KNOWN WITH NO READ: a save learns its workspace from the request's own read of the lock
// ([`Rows::workspace`]), and on a request whose layout stood (no manifest moved, nothing read)
// from the entry of its layout digest ([`layout_of`]: the manifest rows' paths and ids, which
// the rows hold), else of its lock digest. A request that read nothing and matches no entry
// keeps none ([`BaseKept::Unknown`]: its last base stays, older, still valid). The start
// ([`load_base`], only for a checkout with no answer of its own, after the bases record is
// read) reads the checkout's `Cargo.lock` once: its workspace's entry of that lock, else its
// workspace's newest. With no lock on disk, or no entry of its workspace, it reads the root
// `Cargo.toml` once (the first file the layout reads anyway) and starts from the newest entry
// whose root manifest has those bytes ([`Base::root`], [`pick_by_root`]): a members-only
// workspace (blaze itself; a fixture with no lock) whose save learned its workspace from the
// members' own lock finds its base too, as it did from the one pointer before 2026-11-21
// (main.rs `a_second_worktree_answers_from_the_first_ones_records`).
//
// THE BOUND (rule 18): [`BASES_PER_WORKSPACE`] (4) layouts a workspace and [`BASES_MOST`] (32)
// entries in all; past either the oldest entry (by the record's own counter, never a clock:
// rule 25) is dropped. Each save rewrites the record (about 4.4 KiB at the cap) and pins it
// under [`BASES_ROOT`] and the pointers it lists under [`BASES_KEPT_ROOT`], both through the
// store's root commit (`store.pin`: the pointer/root commit path of blaze-cas plain
// pointer_tests.rs and root_commit_tests.rs), so the list moves with the answer it names, in
// one commit. Two roots, because a root's record holds its keys SORTED (blaze-cas
// `Plain::root_for`, `sorted_keys`): under one root the bases record was wherever its key
// sorted among the pointers, and a read of the root's first key found a pointer, "not of this
// form", on 2026-11-21 (engine_bases_worktrees_tests.rs, every save `Replaced`).

/// The root that names the bases record alone: one key.
pub(crate) const BASES_ROOT: &str = "engine.bases";
/// The root that names every pointer the bases record lists, so cleanup keeps each answer a
/// base names (in key order; the record says which is whose).
pub(crate) const BASES_KEPT_ROOT: &str = "engine.bases.kept";
/// The bases record's form: magic, entries u32, then each entry (workspace 32, lock 32 (zero
/// for a layout that read none), layout 32, root manifest 32 (zero for none), pointer 32, its
/// counter u64), newest first. `BEB1` (no root manifest) is another form: rewritten.
const BASES_MAGIC: &[u8; 4] = b"BEB2";
const BASES_DOMAIN: &str = "blaze-cli engine-check bases 2";
const BASE_ENTRY: usize = 32 * 5 + 8;
/// The checkout's root manifest, the first file every layout reads (its row's path).
const ROOT_MANIFEST: &str = "Cargo.toml";
/// No root manifest row: the entry's [`Base::root`].
const NO_ROOT: ContentId = ContentId([0; 32]);
/// Layouts a workspace keeps a base for.
pub(crate) const BASES_PER_WORKSPACE: usize = 4;
/// Entries the record keeps in all.
pub(crate) const BASES_MOST: usize = 32;
/// The digest's domain of a workspace's identity.
const WORKSPACE_DOMAIN: &str = "blaze-cli workspace of a lock 1";
/// The digest's domain of a layout's manifests ([`layout_of`]).
const LAYOUT_DOMAIN: &str = "blaze-cli engine-check layout of the manifests 1";
/// The checkout's lock, from its root (units.rs `LOCK`).
const LOCK_FILE: &str = "Cargo.lock";
/// No lock read: the entry's lock digest.
const NO_LOCK: ContentId = ContentId([0; 32]);

/// A checkout's workspace ([`workspace_of_lock`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Workspace {
    pub(crate) id: ContentId,
}

impl Workspace {
    /// The workspace of the lock whose bytes are `text` (a `Cargo.lock`, or graph.rs
    /// `members_lock`); `None` for a lock of no path package.
    pub(crate) fn of_lock(text: &[u8], frame: &Frame) -> Option<Workspace> {
        Some(Workspace { id: workspace_of_lock(text, frame)? })
    }
}

/// The digest of the rows' manifests (each `Manifest` row's path, flags and id, by path): equal
/// for two checkouts whose layouts read equal files; `None` with no manifest row.
pub(crate) fn layout_of(rows: &Rows<'_>, frame: &Frame) -> Option<ContentId> {
    let mut manifests: blaze_own::Vec<(&str, u8, ContentId), &Frame> = blaze_own::Vec::new_in(frame);
    manifests.extend(rows.rows.iter().filter(|leaf| leaf.kind == crate::sweep::LeafKind::Manifest).map(|leaf| (rows.path(leaf), leaf.flags, leaf.id)));
    if manifests.is_empty() {
        return None;
    }
    manifests.sort_unstable();
    let mut h = blaze_ids::Hasher::new(LAYOUT_DOMAIN);
    for (path, flags, id) in manifests.iter() {
        h.part(path.as_bytes()).part(&[*flags]).part(&id.0);
    }
    Some(h.finish())
}

/// The digest of the lock's path packages' names (the module's "THE WORKSPACE'S IDENTITY"),
/// sorted and each once; `None` when it has none. Read line by line in cargo's own form
/// (`[[package]]`, then `name = "x"` and, for a fetched package, `source = "..."`): only the
/// names and whether a source is written are read, so a lock this reader cannot follow gives
/// fewer names (a weaker identity, never a wrong answer: the module's doc).
pub(crate) fn workspace_of_lock(text: &[u8], frame: &Frame) -> Option<ContentId> {
    let mut names: blaze_own::Vec<&[u8], &Frame> = blaze_own::Vec::new_in(frame);
    let mut in_package = false;
    let mut name: Option<&[u8]> = None;
    let mut sourced = false;
    fn flush<'t>(in_package: bool, name: Option<&'t [u8]>, sourced: bool, names: &mut blaze_own::Vec<&'t [u8], &Frame>) {
        if let (true, Some(name), false) = (in_package, name, sourced) {
            names.push(name);
        }
    }
    for line in text.split(|b| *b == b'\n') {
        let line = line.trim_ascii();
        if line.starts_with(b"[") {
            flush(in_package, name.take(), sourced, &mut names);
            in_package = line == b"[[package]]";
            sourced = false;
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"name") {
            let rest = rest.trim_ascii_start();
            if let Some(value) = rest.strip_prefix(b"=") {
                let value = value.trim_ascii();
                name = value.strip_prefix(b"\"").and_then(|it| it.strip_suffix(b"\""));
            }
        } else if line.strip_prefix(b"source").is_some_and(|rest| rest.trim_ascii_start().starts_with(b"=")) {
            sourced = true;
        }
    }
    flush(in_package, name, sourced, &mut names);
    if names.is_empty() {
        return None;
    }
    names.sort_unstable();
    names.dedup();
    let mut h = blaze_ids::Hasher::new(WORKSPACE_DOMAIN);
    for name in names.iter() {
        h.part(name);
    }
    Some(h.finish())
}

/// One base: the answer `pointer` a checkout of `workspace` saved with lock `lock` ([`NO_LOCK`]
/// for a layout that read none) and layout `layout` ([`layout_of`]), and its place in the
/// record's order (`seq`: larger is newer).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Base {
    pub(crate) workspace: ContentId,
    pub(crate) lock: ContentId,
    pub(crate) layout: ContentId,
    /// The id of the root manifest's bytes (its row's, `sweep::FILE_DOMAIN`), [`NO_ROOT`] for
    /// rows with none: how a checkout with no lock on disk finds its base ([`pick_by_root`]).
    pub(crate) root: ContentId,
    pub(crate) pointer: Key,
    pub(crate) seq: u64,
}

/// The id of the rows' root manifest ([`Base::root`]), with no read: its row's.
pub(crate) fn root_of(rows: &Rows<'_>) -> ContentId {
    rows.rows
        .iter()
        .find(|leaf| leaf.kind == crate::sweep::LeafKind::Manifest && !leaf.absent() && rows.path(leaf) == ROOT_MANIFEST)
        .map_or(NO_ROOT, |leaf| leaf.id)
}

/// The base a checkout whose root manifest has the id `root` starts from when its lock names
/// none (sans-IO): the newest entry of an equal root manifest; never a zero-root match.
pub(crate) fn pick_by_root(bases: &[Base], root: ContentId) -> Option<Base> {
    bases.iter().find(|it| it.root == root && root != NO_ROOT).copied()
}

/// The bases record's entries, newest first; `None` for bytes not of this form (the record is
/// then written anew, [`BaseKept::Replaced`]).
pub(crate) fn decode_bases<'f>(bytes: &[u8], frame: &'f Frame) -> Option<blaze_own::Vec<Base, &'f Frame>> {
    let rest = bytes.strip_prefix(BASES_MAGIC.as_slice())?;
    let count = usize::try_from(u32::from_le_bytes(rest.get(..4)?.try_into().ok()?)).ok()?;
    let rest = &rest[4..];
    if count > BASES_MOST || rest.len() != count * BASE_ENTRY {
        return None;
    }
    let mut out = blaze_own::Vec::with_capacity_in(count, frame);
    for entry in rest.chunks_exact(BASE_ENTRY) {
        let id = |at: usize| ContentId(entry[at..at + 32].try_into().expect("32 bytes"));
        let seq = u64::from_le_bytes(entry[160..168].try_into().expect("eight bytes"));
        out.push(Base { workspace: id(0), lock: id(32), layout: id(64), root: id(96), pointer: Key(id(128)), seq });
    }
    // Newest first, each seq once: else not a record this writer made.
    out.windows(2).all(|pair| pair[0].seq > pair[1].seq).then_some(out)
}

/// The record's bytes of `bases` (newest first).
pub(crate) fn encode_bases<'f>(bases: &[Base], frame: &'f Frame) -> blaze_own::Vec<u8, &'f Frame> {
    let mut out = blaze_own::Vec::with_capacity_in(8 + bases.len() * BASE_ENTRY, frame);
    out.extend_from_slice(BASES_MAGIC);
    out.extend_from_slice(&u32::try_from(bases.len()).unwrap_or(u32::MAX).to_le_bytes());
    for base in bases {
        out.extend_from_slice(&base.workspace.0);
        out.extend_from_slice(&base.lock.0);
        out.extend_from_slice(&base.layout.0);
        out.extend_from_slice(&base.root.0);
        out.extend_from_slice(&base.pointer.0.0);
        out.extend_from_slice(&base.seq.to_le_bytes());
    }
    out
}

/// `bases` with `now` as its newest entry (sans-IO; `now.seq` is ignored and set past every
/// other): the entry of the same workspace and layout replaced, then the oldest dropped past
/// [`BASES_PER_WORKSPACE`] of one workspace and past [`BASES_MOST`] in all.
pub(crate) fn with_base<'f>(bases: &[Base], now: Base, frame: &'f Frame) -> blaze_own::Vec<Base, &'f Frame> {
    let seq = bases.iter().map(|it| it.seq).max().map_or(0, |most| most.saturating_add(1));
    let mut out = blaze_own::Vec::with_capacity_in(bases.len() + 1, frame);
    out.push(Base { seq, ..now });
    for base in bases {
        if out.len() == BASES_MOST {
            break;
        }
        let same = base.workspace == now.workspace && base.layout == now.layout;
        let of_workspace = out.iter().filter(|it| it.workspace == base.workspace).count();
        if !same && of_workspace < BASES_PER_WORKSPACE {
            out.push(*base);
        }
    }
    out
}

/// The base a checkout of `workspace` with lock `lock` starts from (sans-IO): the newest of its
/// workspace and lock digest, else the newest of its workspace, else none.
pub(crate) fn pick_base(bases: &[Base], workspace: Workspace, lock: ContentId) -> Option<Base> {
    let mine = |it: &&Base| it.workspace == workspace.id;
    bases.iter().filter(mine).find(|it| it.lock == lock && lock != NO_LOCK).or_else(|| bases.iter().find(mine)).copied()
}

/// The workspace a save's rows are of, with no read (the module's "KNOWN WITH NO READ"): the
/// request's own read, else the entry of an equal layout, else of an equal lock digest.
pub(crate) fn workspace_of_rows(bases: &[Base], read: Option<Workspace>, layout: ContentId, lock: ContentId) -> Option<Workspace> {
    read.or_else(|| bases.iter().find(|it| it.layout == layout).map(|it| Workspace { id: it.workspace }))
        .or_else(|| bases.iter().find(|it| it.lock == lock && lock != NO_LOCK).map(|it| Workspace { id: it.workspace }))
}

/// What a save did with the bases ([`Saved::base`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum BaseKept {
    /// Not a save that keeps one (a test's plain encode).
    #[default]
    NotAsked,
    /// This answer is its workspace's newest base for its layout.
    Kept { entries: u32 },
    /// The record read was not of this form, and was written anew with this answer alone.
    Replaced,
    /// The rows hold no manifest: no layout, no workspace.
    NoWorkspace,
    /// No workspace was had: this request read no lock (nor the members' own) and no entry has
    /// its layout or lock digest, or what it read names no path package. Known again when the
    /// layout is next read; the last base stays.
    Unknown,
}

impl fmt::Display for BaseKept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BaseKept::NotAsked => f.write_str("base not kept (not asked)"),
            BaseKept::Kept { entries } => write!(f, "kept as its workspace's newest base ({entries} bases in the home)"),
            BaseKept::Replaced => f.write_str("kept as the one base: the home's bases record was not of this form"),
            BaseKept::NoWorkspace => f.write_str("base not kept: the rows hold no manifest"),
            BaseKept::Unknown => f.write_str("base not kept: this request read no lock and no base has its layout or lock"),
        }
    }
}

/// The tests' control of the bases (rule 23): [`BasePlant::OnePointer`] keeps and reads the one
/// [`LATEST_ROOT`] as before 2026-11-21, so a second repository's answer replaces the first's.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BasePlant {
    None,
    OnePointer,
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static BASE_PLANT: core::cell::Cell<BasePlant> = const { core::cell::Cell::new(BasePlant::None) };
}

#[cfg(test)]
fn base_plant() -> BasePlant {
    BASE_PLANT.get()
}

/// The digest of every kind's tool, in kind order: what the pointer keeps so the certificate
/// stands only under the tools that wrote the answer (the module's doc, "THE TOOLS").
pub(crate) fn tools_digest(tools: &Tools) -> ContentId {
    let mut h = blaze_ids::Hasher::new("blaze-cli engine-check pointer tools 1");
    for tool in tools.0.iter() {
        h.update(&tool.0.0);
    }
    h.finish()
}

/// Why the stored answer is not read: the request answers in full and writes it anew.
#[derive(Debug)]
pub(crate) enum Unreadable {
    /// The root names other than one key (its pointer's).
    Names { bytes: usize },
    /// A record the root names is not in the store.
    Missing { key: Key },
    /// The pointer is of an older form (`BEL5`, `BEL4`, or older still): an old home's answer.
    Older,
    /// The printed lines' record (engine_lines.rs).
    Lines(crate::engine_lines::LinesError),
    /// The pointer's write was cut short.
    Torn(seal::Torn),
    /// A pointer of this form whose header index field no writer makes: a tag of no state, or an
    /// absent index naming a shard key.
    Headers { tag: u8 },
    /// A page or the answer's root (pages.rs).
    Pages(PagesError),
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Unreadable::Older = self {
            return write!(f, "the stored engine answer is an old home's answer (its pointer is of an older form than `BEL8`), rebuilt (rule 26)");
        }
        write!(f, "the stored engine answer is not read (engine_answer.rs `load`): ")?;
        match self {
            Unreadable::Names { bytes } => write!(f, "its root names {bytes} bytes, not its pointer's one key")?,
            Unreadable::Missing { key } => write!(f, "{key:?}, which its root names, is not in the store")?,
            Unreadable::Older => {}
            Unreadable::Lines(why) => write!(f, "{why}")?,
            Unreadable::Torn(torn) => write!(f, "{torn}")?,
            Unreadable::Headers { tag } => write!(f, "its producer header index field (tag {tag}) is no state a writer makes (engine_answer.rs `load_pointer`)")?,
            Unreadable::Pages(why) => write!(f, "{why}")?,
        }
        write!(f, "; answering in full and writing it anew")
    }
}

/// What the certificate prints, kept in the pointer so a request with nothing changed reads no
/// page: the files outlined, whether the crates' names were checked, the rows with findings,
/// and the cells and tasks owed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Facts {
    pub(crate) files: u32,
    pub(crate) printed: Printed,
    pub(crate) findings: u32,
    pub(crate) owed: u32,
}

/// What the last request printed that the answer's rows do not say, from the driver: which
/// stages it checked whole, and whether every line it printed beyond the closing one is in the
/// lines record (`replayable`: no note of a count the rows do not keep). The certificate stands
/// only after a replayable request: it prints the lines record and its coverage.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Printed {
    pub(crate) names: bool,
    /// Every body's TYPES were checked to the end (engine_lines.rs `not_checked_whole`). Whether
    /// their borrows were too is not kept here: it is a count of the lines record itself (its
    /// `Class::Borrows` notes, `Tally::borrows`), which the certificate prints whole, so the
    /// two can never disagree.
    pub(crate) bodies_whole: bool,
    pub(crate) replayable: bool,
}

impl Printed {
    fn byte(self) -> u8 {
        u8::from(self.names) | u8::from(self.bodies_whole) << 1 | u8::from(self.replayable) << 2
    }

    fn of_byte(byte: u8) -> Printed {
        Printed { names: byte & 1 != 0, bodies_whole: byte & 2 != 0, replayable: byte & 4 != 0 }
    }
}

/// The pointer as read: the answer's root, the certificate's facts, and the leaf table of what
/// the last request read.
pub(crate) struct Pointer<'m> {
    /// The sealed pointer's own key, as the root named it when read: what a journal
    /// checkpoint extends (journal_checkpoint.rs).
    pub(crate) key: Key,
    pub(crate) root: Key,
    /// The producer header index: current with the pages, or absent since a ready commit.
    pub(crate) headers: Headers,
    /// The printed lines' record (engine_lines.rs).
    pub(crate) lines: Key,
    pub(crate) facts: Facts,
    /// The digest of the tools that wrote the answer ([`tools_digest`]): the certificate stands
    /// only when today's are equal ([`Pointer::certifies_under`]).
    pub(crate) tools: ContentId,
    pub(crate) leaves: Leaves<'m>,
    /// Where the journal of changes stood when the request that wrote it began its reads.
    pub(crate) journal: crate::journal_sweep::Stored<'m>,
}

impl Pointer<'_> {
    /// THE ONE PLACE the tools decide anything of a stored answer (the module's doc, "THE
    /// TOOLS"): whether the certificate, which reads no row, may stand under `tools`. Equal
    /// digests: yes. Else the request takes the phase path over this same answer, where each
    /// row stands or is entered by its own key (its kind's producer) and reads.
    pub(crate) fn certifies_under(&self, tools: &Tools) -> bool {
        self.tools == tools_digest(tools)
    }
}

fn key_at(bytes: &[u8], at: usize) -> Key {
    let mut id = [0; 32];
    id.copy_from_slice(&bytes[at..at + 32]);
    Key(ContentId(id))
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Only the directory the pointer names. The certificate does not need even this read.
pub(crate) fn open_root<'a, D: Driver, S: Sharing>(
    cx: &Cx, store: &mut CommandStore<'_, '_, D, S>, key: Key, alloc: SharedAlloc<'a>,
) -> Result<Result<Root<'a>, Unreadable>, CliError> {
    let Some(held) = store.get(cx, key)? else { return Ok(Err(Unreadable::Missing { key })) };
    Ok(pages::decode_root(Sealed::by_store(key, held.bytes, held.proof), alloc).map_err(Unreadable::Pages))
}

pub(crate) fn page_error(why: PagesError) -> CliError {
    blaze_engine::EngineError::from(blaze_engine::last::LastError::Pages(why)).into()
}

// A small directory of slots, with one block allocated only if a lookup reaches it. The
// snapshot owns the mappings even while workers publish later generations of the store.
const BLOCK: usize = 64;
type PageBox<'a> = blaze_own::Box<VerifiedPage<'a>, SharedAlloc<'a>>;
type PageSlot<'a> = OnceLock<Option<PageBox<'a>>>;
type PageBlock<'a> = blaze_own::Box<[PageSlot<'a>; BLOCK], SharedAlloc<'a>>;
/// The producer header shards' slots, one each, in the request's memory: 128 slots of about a
/// hundred bytes, which held inline made `Stored` some 12 KiB, copied at each move of it through
/// the command's top-level check frame in a debug build (the stack overflow of
/// `a_changed_archive_checksum_enters_the_body_through_the_graph`, wave-1119 t26).
type HeaderSlots<'a> = blaze_own::Box<[OnceLock<Result<IndexedHeaders<'a>, PagesError>>; HeaderPages::COUNT], SharedAlloc<'a>>;

pub(crate) struct Stored<'a> {
    pub(crate) root: &'a Root<'a>,
    snapshot: &'a Snapshot<SharedAlloc<'a>>,
    /// The producer header index the pointer names: its shards read when current, else every
    /// header and planner row read from its row's page (blaze-engine headers.rs `Headers`).
    headers: &'a Headers,
    header_pages: HeaderSlots<'a>,
    alloc: SharedAlloc<'a>,
    blocks: blaze_own::Vec<OnceLock<PageBlock<'a>>, SharedAlloc<'a>>,
    refused: OnceLock<PagesError>,
    /// How a page's bytes are known to be its key's: the store's proof handed through
    /// ([`pages::Check::Store`], the command's path: no byte hashed, a page's head alone checked)
    /// or every byte hashed and every entry walked ([`pages::Check::Hashed`], `--verify`).
    check: pages::Check,
    task_reads: core::sync::atomic::AtomicUsize,
    header_reads: core::sync::atomic::AtomicUsize,
    /// The bytes of the task and header pages read, and of those hashed (zero on the store's
    /// proof): the `pages:` line ([`PagesRead`]).
    task_bytes: core::sync::atomic::AtomicU64,
    header_bytes: core::sync::atomic::AtomicU64,
    hashed_bytes: core::sync::atomic::AtomicU64,
    /// Rows asked by name ([`Stored::asks`]).
    asks: core::sync::atomic::AtomicUsize,
    /// Each name asked, in ask order, the first [`MOST_ASKS_KEPT`]: what [`Stored::asked`]
    /// tells by kind, repeats and distinct names apart (rule 17: which rows a request's reads
    /// of the answer are, before why they scale).
    asked: blaze_core::lock::Lock<blaze_own::Vec<(Name, blaze_engine::Ask), SharedAlloc<'a>>>,
}

/// The names [`Stored::asked`] keeps (rule 18: a table has a cap): a ready edit asks hundreds;
/// asks past it are counted ([`Stored::asks`]) and not told by kind.
const MOST_ASKS_KEPT: usize = 1 << 16;

/// A request's rows asked of the stored answer, by kind ([`Stored::asked`]): each kind's asks
/// (repeats included) and distinct names, and the asks past the cap not told; and by purpose
/// ([`blaze_engine::Ask`]): each purpose's asks and its distinct reads (a name and purpose, a
/// cell ask's cell too), so a repeat is the same read asked again, never another cell's read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct AskedByKind {
    pub(crate) asks: [u32; blaze_drive::key::KINDS],
    pub(crate) distinct: [u32; blaze_drive::key::KINDS],
    pub(crate) by_purpose: [u32; blaze_engine::Ask::PURPOSES],
    pub(crate) purpose_distinct: [u32; blaze_engine::Ask::PURPOSES],
    /// Each kind's asks by purpose (repeats included): which of a kind's asks repeat (rule 17:
    /// a row asked 9 times is told as its purposes before anyone guesses which reader asks).
    pub(crate) kind_purpose: [[u32; blaze_engine::Ask::PURPOSES]; blaze_drive::key::KINDS],
    pub(crate) untold: u32,
}

impl Default for AskedByKind {
    fn default() -> AskedByKind {
        AskedByKind {
            asks: [0; blaze_drive::key::KINDS],
            distinct: [0; blaze_drive::key::KINDS],
            by_purpose: [0; blaze_engine::Ask::PURPOSES],
            purpose_distinct: [0; blaze_engine::Ask::PURPOSES],
            kind_purpose: [[0; blaze_engine::Ask::PURPOSES]; blaze_drive::key::KINDS],
            untold: 0,
        }
    }
}

impl AskedByKind {
    /// The purposes whose distinct reads differ between `self` and `other`, by word: what a
    /// world test names when a request's reads grow.
    #[cfg(test)]
    pub(crate) fn purposes_moved(&self, other: &AskedByKind) -> std::vec::Vec<&'static str> {
        (0..blaze_engine::Ask::PURPOSES)
            .filter(|&at| self.purpose_distinct[at] != other.purpose_distinct[at])
            .map(|at| blaze_engine::Ask::WORDS[at])
            .collect()
    }
}

/// `Kind asks/distinct` for each kind asked, then the untold asks: the profile's `rows asked:`
/// words and the world test's message.
impl core::fmt::Display for AskedByKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (asks, distinct): (u32, u32) = (self.asks.iter().sum(), self.distinct.iter().sum());
        write!(f, "rows asked {asks} ({distinct} distinct):")?;
        for kind in blaze_drive::Kind::ALL {
            let at = kind.index();
            if self.asks[at] > 0 {
                write!(f, " {kind:?} {}/{}", self.asks[at], self.distinct[at])?;
                // Its asks by purpose, when it asked any row more often than it has rows.
                if self.asks[at] > self.distinct[at] {
                    let mut sep = " (";
                    for (purpose, &count) in self.kind_purpose[at].iter().enumerate() {
                        if count > 0 {
                            write!(f, "{sep}{} {count}", blaze_engine::Ask::WORDS[purpose])?;
                            sep = ", ";
                        }
                    }
                    write!(f, ")")?;
                }
            }
        }
        write!(f, "; by purpose (asks/distinct reads):")?;
        for at in 0..blaze_engine::Ask::PURPOSES {
            if self.by_purpose[at] > 0 {
                write!(f, " {} {}/{}", blaze_engine::Ask::WORDS[at], self.by_purpose[at], self.purpose_distinct[at])?;
            }
        }
        if self.untold > 0 {
            write!(f, "; {} past the cap not told", self.untold)?;
        }
        Ok(())
    }
}

/// What a request read of its stored answer's pages ([`Stored::pages_read_line`]): the profile's
/// `pages: N read, M bytes, K hashed` line, the one the engine's simulation prints for its cases.
/// Its floor on an edit is the pages the edited rows, their files' plans and the producer header
/// pages they name sit on, and `K` is 0 on the command's path.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct PagesRead {
    pub(crate) task: usize,
    pub(crate) header: usize,
    pub(crate) bytes: u64,
    pub(crate) hashed: u64,
}

impl fmt::Display for PagesRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pages: {} read ({} task, {} producer header), {} bytes, {} hashed", self.task + self.header, self.task, self.header, self.bytes, self.hashed)
    }
}

impl<'a> Stored<'a> {
    /// `check`: [`pages::Check::Store`] on the command's path (each page's bytes with the
    /// store's proof, plain_place.rs `snapshot_get`), [`pages::Check::Hashed`] for an explicit
    /// verify (every byte hashed, every entry walked: `VerifiedPage::open_whole`).
    pub(crate) fn new(root: &'a Root<'a>, snapshot: &'a Snapshot<SharedAlloc<'a>>, headers: &'a Headers, check: pages::Check, alloc: SharedAlloc<'a>) -> Self {
        use core::sync::atomic::{AtomicU64, AtomicUsize};
        let blocks = blaze_own::Vec::from_iter_in((0..root.pages.len().div_ceil(BLOCK)).map(|_| OnceLock::new()), alloc);
        Self {
            root, snapshot, headers, header_pages: blaze_own::Box::new_in(core::array::from_fn(|_| OnceLock::new()), alloc), alloc, blocks, refused: OnceLock::new(), check,
            task_reads: AtomicUsize::new(0),
            header_reads: AtomicUsize::new(0),
            task_bytes: AtomicU64::new(0),
            header_bytes: AtomicU64::new(0),
            hashed_bytes: AtomicU64::new(0),
            asks: AtomicUsize::new(0),
            asked: blaze_core::lock::Lock::new("engine.answer: the rows asked by name", blaze_own::Vec::with_capacity_in(0, alloc)),
        }
    }

    /// One ask of `name`'s page for `ask`: counted, and kept under the cap for [`Stored::asked`].
    fn note_ask(&self, name: Name, ask: blaze_engine::Ask) {
        self.asks.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let mut asked = self.asked.lock();
        if asked.len() < MOST_ASKS_KEPT {
            // A refused growth leaves the name untold, counted by `asks` (rule 3: the count
            // says so, `AskedByKind::untold`).
            let _ = asked.try_push((name, ask));
        }
    }

    /// The rows asked so far by kind (repeats and distinct names apart) and by purpose (asks
    /// and distinct reads apart) ([`AskedByKind`]).
    pub(crate) fn asked(&self) -> AskedByKind {
        let mut by = AskedByKind::default();
        let asked = self.asked.lock();
        let mut sorted: blaze_own::Vec<(Name, blaze_engine::Ask), SharedAlloc<'a>> = blaze_own::Vec::with_capacity_in(asked.len(), self.alloc);
        for &(name, ask) in asked.iter() {
            by.asks[name.kind.index()] += 1;
            by.by_purpose[ask.purpose()] += 1;
            by.kind_purpose[name.kind.index()][ask.purpose()] += 1;
            let _ = sorted.try_push((name, ask));
        }
        by.untold = u32::try_from(self.asks().saturating_sub(asked.len())).unwrap_or(u32::MAX);
        drop(asked);
        sorted.sort_unstable();
        let mut last_name = None;
        let mut last_read = None;
        for &(name, ask) in sorted.iter() {
            if last_name != Some(name) {
                by.distinct[name.kind.index()] += 1;
            }
            if last_read != Some((name, ask)) {
                by.purpose_distinct[ask.purpose()] += 1;
            }
            last_name = Some(name);
            last_read = Some((name, ask));
        }
        by
    }

    /// How many times a reader asked this answer for a row by name ([`StoredPages::header`],
    /// [`StoredPages::page`]), repeats included: what a request's reads of the answer scale
    /// with, apart from the page packing (a page holds the rows its names' leading bits pick, so
    /// one set of names asked spans more pages in a larger answer: `pages_read` is not the measure).
    pub(crate) fn asks(&self) -> usize {
        self.asks.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// The page holding `name`'s row, asked by name for `ask` (counted in [`Stored::asks`] and
    /// told by purpose in [`Stored::asked`]).
    pub(crate) fn named_page(&self, name: Name, ask: blaze_engine::Ask) -> Result<Option<&VerifiedPage<'a>>, PagesError> {
        self.note_ask(name, ask);
        self.page_at(blaze_drive::answers::page_of(name, self.root.bits) as usize)
    }

    /// Page `page`'s record under `key` from the snapshot, sealed by this request's `check`:
    /// the store's proof handed through, or every byte hashed (and counted as hashed).
    fn sealed(&self, key: Key, page: u32, bytes_read: &core::sync::atomic::AtomicU64) -> Result<pages::Sealed<'a>, PagesError> {
        use core::sync::atomic::Ordering::Relaxed;
        let held = crate::store::plain_place::snapshot_get(self.snapshot, key)
            .map_err(|_| PagesError::PageRecord { page, key })?
            .ok_or(PagesError::Missing { page, key })?;
        bytes_read.fetch_add(held.bytes.len() as u64, Relaxed);
        match self.check {
            pages::Check::Store => Ok(Sealed::by_store(key, held.bytes, held.proof)),
            pages::Check::Hashed => {
                self.hashed_bytes.fetch_add(held.bytes.len() as u64, Relaxed);
                Sealed::hashed(key, held.bytes).map_err(|torn| torn.at(page))
            }
        }
    }

    /// The record under `key` in this answer's snapshot, in place, under the store's proof (the
    /// one check of its segment, rule 74): what a row of the answer names, read for the request.
    pub(crate) fn record_bytes(&self, key: Key) -> Result<Option<&'a [u8]>, blaze_cas::plain::SnapshotError> {
        Ok(crate::store::plain_place::snapshot_get(self.snapshot, key)?.map(|held| held.bytes))
    }

    /// The `pages:` line's counts so far.
    pub(crate) fn pages_read_line(&self) -> PagesRead {
        use core::sync::atomic::Ordering::Relaxed;
        let (task, header) = self.read_counts();
        PagesRead { task, header, bytes: self.task_bytes.load(Relaxed) + self.header_bytes.load(Relaxed), hashed: self.hashed_bytes.load(Relaxed) }
    }

    pub(crate) fn answer(&self) -> blaze_engine::Answer<'_> {
        let mut answer = blaze_engine::Answer::from_pages(self, self.root.serial, self.alloc);
        answer.owed_tasks.extend(self.root.owed_tasks.iter().map(|(&name, &plan)| (name, plan)));
        answer.owed_cells.extend(self.root.owed_cells.iter().map(|(&id, &at)| (id, at)));
        answer.with_findings.extend(self.root.findings.keys().copied());
        answer
    }

    /// Shard `index`, opened once. Asked only of a current index (headers.rs `Headers::header`,
    /// `Headers::planner_row`, `Headers::after` ask a shard of nothing else); asked of an
    /// absent one it is refused by name, never answered from a stale key.
    fn headers_at(&self, index: usize) -> Result<Option<&IndexedHeaders<'a>>, PagesError> {
        let key = match self.headers {
            Headers::Current(keys) => keys.keys[index],
            Headers::Absent { since } => {
                return Err(*self.refused.get_or_init(|| PagesError::HeadersAbsent { page: u32::try_from(index).unwrap_or(u32::MAX), since: *since }));
            }
        };
        if key == Key::default() {
            return Ok(None);
        }
        let proof = self.header_pages[index].get_or_init(|| {
            self.header_reads.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let page = index as u32;
            IndexedHeaders::open(self.sealed(key, page, &self.header_bytes)?, index)
        });
        match proof {
            Ok(page) => Ok(Some(page)),
            Err(error) => Err(*self.refused.get_or_init(|| *error)),
        }
    }

    /// The units planner and script ordering consume these full prior results: from a current
    /// shard, or from the row's page when the index is absent.
    pub(crate) fn planner_row<'r>(&self, name: Name, alloc: SharedAlloc<'r>) -> Result<Option<blaze_engine::Row<'r>>, PagesError> {
        self.headers.planner_row(name, |index| self.headers_at(index), || self.page(name), alloc)
    }

    /// The header index this answer names (the profile's `headers:` word).
    pub(crate) fn headers(&self) -> &'a Headers {
        self.headers
    }

    pub(crate) fn read_counts(&self) -> (usize, usize) {
        use core::sync::atomic::Ordering::Relaxed;
        (self.task_reads.load(Relaxed), self.header_reads.load(Relaxed))
    }

    #[cfg(test)]
    pub(crate) fn pages_read(&self) -> usize {
        let (tasks, headers) = self.read_counts();
        tasks + headers
    }

    pub(crate) fn page_at(&self, index: usize) -> Result<Option<&VerifiedPage<'a>>, PagesError> {
        let page = u32::try_from(index).unwrap_or(u32::MAX);
        let Some(&key) = self.root.pages.get(index) else {
            return Err(*self.refused.get_or_init(|| PagesError::Count { page: pages::ROOT_PAGE, count: page }));
        };
        if key == Key::default() {
            return Ok(None);
        }
        let block = self.blocks[index / BLOCK].get_or_init(|| blaze_own::Box::new_in(core::array::from_fn(|_| OnceLock::new()), self.alloc));
        let proof = block[index % BLOCK].get_or_init(|| {
            self.task_reads.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // On the store's proof the head alone is checked, no byte hashed and no entry walked
            // (pages/view.rs `open`); an explicit verify hashes and walks every entry.
            let opened = self.sealed(key, page, &self.task_bytes).and_then(|sealed| match self.check {
                pages::Check::Store => VerifiedPage::open(sealed, page, self.root.bits),
                pages::Check::Hashed => VerifiedPage::open_whole(sealed, page, self.root.bits),
            });
            match opened {
                Ok(page) => Some(blaze_own::Box::new_in(page, self.alloc)),
                Err(why) => {
                    self.refused.get_or_init(|| why);
                    None
                }
            }
        });
        match proof {
            Some(page) => Ok(Some(page)),
            None => Err(*self.refused.get().expect("a refused page records its error before publishing its slot")),
        }
    }
}

impl StoredPages for Stored<'_> {
    fn header(&self, name: Name) -> Result<Option<Header>, PagesError> {
        self.note_ask(name, blaze_engine::Ask::Header);
        self.headers.header(name, |index| self.headers_at(index), || self.page_at(blaze_drive::answers::page_of(name, self.root.bits) as usize))
    }
    fn page(&self, name: Name) -> Result<Option<&VerifiedPage<'_>>, PagesError> {
        self.named_page(name, blaze_engine::Ask::Unsaid)
    }
    fn page_for(&self, name: Name, ask: blaze_engine::Ask) -> Result<Option<&VerifiedPage<'_>>, PagesError> {
        self.named_page(name, ask)
    }

    fn refusal(&self) -> Option<PagesError> {
        self.refused.get().copied()
    }

    fn finding(&self, name: Name) -> Option<u32> {
        self.root.findings.get(&name).copied()
    }

    fn visit_rows(&self, visit: &mut dyn FnMut(Name) -> Result<(), blaze_engine::LastError>) -> Result<(), blaze_engine::LastError> {
        for index in 0..self.root.pages.len() {
            if let Some(page) = self.page_at(index).map_err(blaze_engine::LastError::Pages)? {
                for name in page.row_names() {
                    visit(name)?;
                }
            }
        }
        Ok(())
    }
}

/// Exhaustive inspection for commands and tests which explicitly ask for all rows.
pub(crate) fn load_answer<'m, 'a, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, root_key: Key, alloc: SharedAlloc<'a>) -> Result<Result<(Kept<'a>, Root<'a>), Unreadable>, CliError> {
    // Each record through the store; a store's own error stops the command after the decode.
    let mut failed: Option<CliError> = None;
    let root = match open_root(cx, store, root_key, alloc)? {
        Ok(root) => root,
        Err(why) => return Ok(Err(why)),
    };
    let decoded = pages::decode_at(
        &root,
        |key| match store.get(cx, key) {
            Ok(found) => found.map(|held| Ok(Sealed::by_store(key, held.bytes, held.proof))),
            Err(error) => {
                failed.get_or_insert(error);
                None
            }
        },
        alloc,
    );
    if let Some(error) = failed {
        return Err(error);
    }
    let kept = match decoded {
        Ok(kept) => kept,
        Err(why) => return Ok(Err(Unreadable::Pages(why))),
    };
    Ok(Ok((kept, root)))
}

/// The pointer the root `name` names, read in place: `None` when there is none.
pub(crate) fn load_pointer<'m, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, name: &str) -> Result<Option<Result<Pointer<'m>, Unreadable>>, CliError> {
    let Some(keys) = store.named(name)? else { return Ok(None) };
    if keys.len() != 32 {
        return Ok(Some(Err(Unreadable::Names { bytes: keys.len() })));
    }
    pointer_at(cx, store, key_at(keys, 0)).map(Some)
}

/// The pointer record `first`, read in place (a root's one key, or a base's, [`load_base`]).
fn pointer_at<'m, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, first: Key) -> Result<Result<Pointer<'m>, Unreadable>, CliError> {
    let Some(held) = store.get(cx, first)? else { return Ok(Err(Unreadable::Missing { key: first })) };
    let sealed: &'m [u8] = held.bytes;
    if !sealed.starts_with(seal::SEAL_MAGIC) {
        return Ok(Err(Unreadable::Older));
    }
    let (pointer, leaves) = match seal::open(sealed, seal::Plant::None) {
        Ok(opened) => opened,
        Err(torn) => return Ok(Err(Unreadable::Torn(torn))),
    };
    if pointer.len() < POINTER_BYTES || !pointer.starts_with(POINTER_MAGIC) {
        return Ok(Err(Unreadable::Older));
    }
    let Some(journal) = crate::journal_sweep::Stored::of(&pointer[POINTER_BYTES..]) else {
        return Ok(Err(Unreadable::Older));
    };
    let (leaves, older) = Leaves::of(leaves)?;
    if older.is_some() {
        return Ok(Err(Unreadable::Older));
    }
    let facts = Facts { files: word(pointer, 68), printed: Printed::of_byte(pointer[72]), findings: word(pointer, 73), owed: word(pointer, 77) };
    if pointer[72] > 7 {
        return Ok(Err(Unreadable::Older));
    }
    let keys = HeaderPages { keys: core::array::from_fn(|i| key_at(pointer, SHARDS_AT + i * 32)) };
    let since = u64::from_le_bytes(pointer[HEADERS_AT + 1..SHARDS_AT].try_into().expect("eight bytes"));
    let headers = match pointer[HEADERS_AT] {
        HEADERS_CURRENT if since == 0 => Headers::Current(keys),
        HEADERS_ABSENT if keys.keys.iter().all(|key| *key == Key::default()) => Headers::Absent { since: blaze_engine::Gen(since) },
        tag => return Ok(Err(Unreadable::Headers { tag })),
    };
    let tools = key_at(pointer, TOOLS_AT).0;
    Ok(Ok(Pointer { key: first, root: key_at(pointer, 4), headers, lines: key_at(pointer, 36), facts, tools, leaves, journal }))
}

/// The printed lines' record the pointer names, every entry checked (engine_lines.rs `decode`).
pub(crate) fn load_lines<'m, 'a, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    key: Key,
    alloc: SharedAlloc<'a>,
) -> Result<Result<crate::engine_lines::Last<'m, 'a>, Unreadable>, CliError> {
    let Some(held) = store.get(cx, key)? else { return Ok(Err(Unreadable::Missing { key })) };
    let bytes: &'m [u8] = held.bytes;
    Ok(crate::engine_lines::decode(bytes, alloc).map_err(Unreadable::Lines))
}

/// What one write put.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Saved {
    /// The new pages and their reach indexes, the root, the lines and the sealed pointer.
    pub(crate) puts: u32,
    /// Task pages in the new root and their total bytes.
    pub(crate) pages: u32,
    pub(crate) bytes: u64,
    /// Task pages this write put.
    pub(crate) new_pages: u32,
    /// The bytes of the task pages this write put, and of the largest: a commit costs these
    /// (each page is merged whole, keyed by its bytes, then put), so the profile names a page
    /// that is large for what moved on it (a hot cell's reader list, pages.rs `READERS`).
    pub(crate) new_page_bytes: u64,
    pub(crate) largest_new_page: u64,
    pub(crate) new_headers: u32,
    /// The new root's key (pages.rs `Root`): a function of every page's key, the owed sets and
    /// the findings, so two drivers' answers of one checkout are equal when their roots are.
    pub(crate) root: Key,
    /// The digest of the root's page keys in order, the zero key of an empty slot too
    /// ([`PAGES_DOMAIN`]): the world test names which side moved when two roots differ. Made in
    /// a test build alone: 2 MB hashed a commit on tempo's 65,536 pages (edit12 profile,
    /// c0340wave89), and the root's key already covers every page's.
    pub(crate) pages_digest: Option<ContentId>,
    /// The pages this commit keyed into the reach root, and why it was made whole if it was
    /// ([`page_indexes`]).
    pub(crate) rekeyed: Rekeyed,
    /// Of a commit folded into the last answer: the entries its delta wrote and the pages they
    /// fell on (blaze-engine pages.rs `Encoded::changed_entries`), each page read once as its
    /// witness; (0, 0) for a whole encode.
    pub(crate) changed: (u32, u32),
    /// What the save did with the home's bases ([`BASES_ROOT`]).
    pub(crate) base: BaseKept,
}

/// Which pages a commit keys into the answer's reach root ([`page_indexes`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rekey {
    /// Only the pages whose key moved since the last answer: every other page's index is
    /// carried from the last commit's reach root, read in place.
    Changed,
    /// Every page, as a cold commit does: the planted control of the page edit test (a commit
    /// that re-keys what did not move).
    #[cfg(test)]
    All,
}

/// What [`page_indexes`] did: the pages it keyed (their index computed now: one digest for the
/// new key, one for the old a page replaced), and why the reach root was made whole if it was.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Rekeyed {
    pub(crate) pages: u32,
    pub(crate) whole: Option<WhyWhole>,
}

/// Why a commit's reach root was made from every page rather than carried: each a first commit
/// of its kind, said on the profile's line, never a quiet second route.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WhyWhole {
    /// No last answer: the first commit of this checkout's answer.
    NoLast,
    /// The last answer's paging differs (its `bits`): every page's index moved.
    Paging,
    /// No unexpired reach root or answer root under the checkout's names (`{name}.reach`,
    /// `{name}.answer`): the first commit of this form, or one past the roots' life.
    NoRoot,
    /// The answer root names another answer than the one this commit is made over: another
    /// command committed between this one's read and its write.
    NotLast,
    /// The planted control ([`Rekey::All`]).
    Planted,
}

impl fmt::Display for Rekeyed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.whole {
            None => write!(f, "{} page(s) re-keyed, the rest carried from the last reach root", self.pages),
            Some(why) => write!(f, "{} page(s) keyed, the reach root made whole ({why:?})", self.pages),
        }
    }
}

/// The checkout's reach root's version: the pages' indexes alone (the root, lines, header
/// shards and member rows under [`ANSWER_ROOT`]). The form before it (`{name}.pages`, the direct
/// index among the pages') is never read: it expires by its life.
pub(crate) const REACH_ROOT: &str = "reach";
pub(crate) const ANSWER_ROOT: &str = "answer";

const PAGES_DOMAIN: &str = "blaze-cli engine-check page keys";

/// What a request wrote of the stored answer and read of the last (engine_check.rs
/// `Checked::stored`): the oracle of the drivers' world test (engine_ready_edit_tests.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StoredKeys {
    /// The new root's key ([`Saved::root`]).
    pub(crate) root: Key,
    /// The digest of its page keys in order ([`Saved::pages_digest`], a test build's alone) and
    /// how many there are.
    pub(crate) pages: (Option<ContentId>, u32),
    /// What the commit keyed into its reach root ([`Saved::rekeyed`]).
    pub(crate) rekeyed: Rekeyed,
    /// Task pages of the last answer this request read (`Stored::read_counts`'s first).
    pub(crate) pages_read: usize,
    /// Producer header shards of the last answer this request read (its second): 0 under the
    /// ready driver, which answers every header from the row's page.
    pub(crate) header_shards_read: usize,
    /// Rows of the last answer this request asked by name, by kind, repeats and distinct names
    /// apart ([`Stored::asked`]).
    pub(crate) asked: AskedByKind,
    /// The commit's delta: entries written and the pages they fell on ([`Saved::changed`]).
    pub(crate) changed: (u32, u32),
}

/// The profile's `answer keys:` line: what two drivers' runs of one edit are compared by.
impl core::fmt::Display for StoredKeys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (digest, count) = self.pages;
        write!(f, "answer keys: root {}, {count} page(s)", self.root)?;
        if let Some(digest) = digest {
            write!(f, " digest {}", Key(digest))?;
        }
        write!(f, "; read of the last: {} task page(s), {} producer header shard(s); reach: {}", self.pages_read, self.header_shards_read, self.rekeyed)
    }
}

impl StoredKeys {
    pub(crate) fn of(saved: &Saved, last: Option<&Stored<'_>>) -> StoredKeys {
        let (pages_read, header_shards_read) = last.map_or((0, 0), Stored::read_counts);
        StoredKeys { root: saved.root, pages: (saved.pages_digest, saved.pages), rekeyed: saved.rekeyed, pages_read, header_shards_read, asked: last.map_or_else(AskedByKind::default, Stored::asked), changed: saved.changed }
    }
}

/// `kept` and the leaf table of `rows` stored under the root `name` over `last` (its pages not
/// put again), by the phase path: the producer header shards its rows sit on rewritten when
/// current (blaze-engine headers.rs `Headers::after`); visible from the store's commit after
/// the answer.
#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
pub(crate) fn save_edit<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: Option<&Stored<'_>>,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
    row_keys: &[Key],
    last_root: Option<Key>,
) -> Result<Saved, CliError> {
    save(cx, store, lent, name, toolchain, tools, kept, printed, lines, last, Writer::Phases, rows, journal, alloc, frame, row_keys, (last_root, Rekey::Changed))
}

/// [`save_edit`] keying every page into the reach root: the page edit test's planted control.
#[cfg(test)]
#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
pub(crate) fn save_edit_rekeying_all<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: Option<&Stored<'_>>,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
    last_root: Option<Key>,
) -> Result<Saved, CliError> {
    save(cx, store, lent, name, toolchain, tools, kept, printed, lines, last, Writer::Phases, rows, journal, alloc, frame, &[], (last_root, Rekey::All))
}

/// The `Ready` driver's commit of an edit (D187, engine_ready_check.rs): `kept`'s changed rows
/// merged into only the pages they sit on over `last` (pages.rs `encode_changes`), the lines,
/// then the sealed pointer last, in one store commit. No producer header shard is read or
/// written: the pointer names the index [`Headers::Absent`] (since this request, or since the
/// commit that first left it so), and every later header lookup reads the row's page.
#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
pub(crate) fn save_changes<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: &Stored<'_>,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
    row_keys: &[Key],
    last_root: Option<Key>,
) -> Result<Saved, CliError> {
    save(cx, store, lent, name, toolchain, tools, kept, printed, lines, Some(last), Writer::Ready, rows, journal, alloc, frame, row_keys, (last_root, Rekey::Changed))
}

#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
fn save<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: Option<&Stored<'_>>,
    writer: Writer,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
    row_keys: &[Key],
    (last_root, rekey): (Option<Key>, Rekey),
) -> Result<Saved, CliError> {
    let encoded = match last {
        Some(stored) => pages::encode_changes(kept, stored.root, alloc).map_err(page_error)?,
        None => pages::encode(kept, None, alloc),
    };
    // The encoder's puts are the new pages and the root (pages.rs `encode`).
    let new_pages = u32::try_from(encoded.puts.len().saturating_sub(1)).unwrap_or(u32::MAX);
    #[cfg(test)]
    let pages_digest = {
        let mut page_keys: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::with_capacity_in(encoded.pages.len() * 32, frame);
        for page in encoded.pages.iter() {
            page_keys.extend_from_slice(&page.0.0);
        }
        Some(ContentId::of(PAGES_DOMAIN, &page_keys))
    };
    #[cfg(not(test))]
    let pages_digest = None;
    let mut saved = Saved { bytes: encoded.bytes, new_pages, root: encoded.root, pages_digest, changed: (encoded.changed_entries, encoded.changed_pages), ..Saved::default() };
    for (key, bytes) in encoded.puts.iter() {
        store.put(cx, *key, bytes)?;
        saved.puts += 1;
        // The root is among the puts (`new_pages` above leaves it out); a page is keyed by
        // its bytes and the root is not a page's key.
        if *key != encoded.root {
            let len = bytes.len() as u64;
            saved.new_page_bytes += len;
            saved.largest_new_page = saved.largest_new_page.max(len);
        }
    }
    let (headers, writes) = Headers::after(last.map(|base| base.headers), kept, writer, kept.answer.serial, HeadersPlant::None, |index| match last {
        Some(base) => base.headers_at(index),
        None => Ok(None),
    }, alloc).map_err(page_error)?;
    saved.new_headers = writes.len() as u32;
    for (key, bytes) in &writes {
        store.put(cx, *key, bytes)?;
        saved.puts += 1;
    }
    // The printed lines, under the key of their bytes: equal lines are one record.
    store.put(cx, lines.0, lines.1)?;
    saved.puts += 1;
    let leaves = Leaves::bytes_in(rows, frame)?;
    let mut pointer: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::with_capacity_in(POINTER_BYTES, frame);
    pointer.extend_from_slice(POINTER_MAGIC);
    pointer.extend_from_slice(&encoded.root.0.0);
    pointer.extend_from_slice(&lines.0.0.0);
    let facts = Facts { files: encoded.files, printed, findings: encoded.findings, owed: encoded.owed };
    pointer.extend_from_slice(&facts.files.to_le_bytes());
    pointer.push(facts.printed.byte());
    pointer.extend_from_slice(&facts.findings.to_le_bytes());
    pointer.extend_from_slice(&facts.owed.to_le_bytes());
    pointer.extend_from_slice(&tools_digest(tools).0);
    match &headers {
        Headers::Current(keys) => {
            pointer.push(HEADERS_CURRENT);
            pointer.extend_from_slice(&0u64.to_le_bytes());
            for key in &keys.keys {
                pointer.extend_from_slice(&key.0.0);
            }
        }
        Headers::Absent { since } => {
            pointer.push(HEADERS_ABSENT);
            pointer.extend_from_slice(&since.0.to_le_bytes());
            // No shard key: the zero key in each of the index's slots.
            pointer.extend_from_slice(&[0u8; 32 * HeaderPages::COUNT]);
        }
    }
    journal.encode(&mut pointer);
    let sealed = seal::seal(&pointer, &leaves, frame)?;
    let key = Key(ContentId::of(POINTER_DOMAIN, &sealed));
    store.put(cx, key, &sealed)?;
    saved.puts += 1;
    // A page and the payloads of *every* row on it share one immutable reach index.
    // StoreSink also includes each names payload's expansion index and its children.
    // The store follows exactly one index level from an indexed root.
    use crate::engine_answer_rows::{AnswerSink as _, StoreSink};
    let mut sink = StoreSink { cx, store, lent, toolchain };
    let mut records = blaze_own::Vec::new_in(alloc);
    let mut scopes = blaze_own::Vec::new_in(alloc);
    let mut toolchain_records = blaze_own::Vec::new_in(alloc);
    for (page, rows) in encoded.page_records() {
        records.clear();
        scopes.clear();
        records.push(page);
        for &(name, record) in rows {
            records.push(record);
            if name.kind == blaze_drive::Kind::CrateScope {
                scopes.push((name, record));
            }
            if sink.store.is_toolchain(name) {
                toolchain_records.push(record);
            }
        }
        sink.index_rows(page, &records, &scopes, alloc.into()).map_err(reach_error)?;
        saved.puts += 1;
    }
    let reach_name = bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}");
    let answer_name = bstd::mem::format_in!(frame, "{name}.{ANSWER_ROOT}");
    let last_pages = match (last, last_root) {
        (Some(base), Some(key)) => Some((&base.root.pages[..], key)),
        _ => None,
    };
    let (indexes, rekeyed) = page_indexes(lent, (reach_name.as_str(), answer_name.as_str()), last_pages, &encoded.pages, rekey, alloc)?;
    saved.pages = u32::try_from(indexes.len()).unwrap_or(u32::MAX);
    saved.rekeyed = rekeyed;
    let mut direct = blaze_own::Vec::new_in(alloc);
    direct.push(blaze_cas::Key(encoded.root.0));
    direct.push(blaze_cas::Key(lines.0.0));
    if let Some(keys) = headers.current() {
        direct.extend(keys.keys.iter().filter(|key| **key != Key::default()).map(|key| blaze_cas::Key(key.0)));
    }
    // The member listing rows this request read or put (engine_bodies.rs `member_row_keys`,
    // engine_member_rows.rs): named here so they live while the answer does, and the next
    // command reads them by key instead of opening and listing each member again.
    direct.extend(row_keys.iter().map(|key| blaze_cas::Key(key.0)));
    // Two roots, one commit: the pages' indexes (at most one a page: `ROOT_INDEXES_MOST` is
    // the most pages a root's paging makes), and the answer's own index, which names this
    // answer's root key: the next commit's proof that the reach root is this answer's
    // ([`page_indexes`]).
    let answer_index = lent.index(&direct).map_err(crate::store::PlaceError::from)?;
    lent.root_through(reach_name.as_str(), &indexes, blaze_cas::plain::ROOT_LIFE_NS).map_err(crate::store::PlaceError::from)?;
    lent.root_through(answer_name.as_str(), &[answer_index], blaze_cas::plain::ROOT_LIFE_NS).map_err(crate::store::PlaceError::from)?;
    // This root accumulates indexes across commands. Unchanged pages' toolchain
    // payloads were admitted with the page and this renewal extends their lifetime.
    store.keep_toolchain(toolchain, &toolchain_records)?;
    // Refresh the toolchain-only fallback base (`tool-base.<PINNED>.<TRIPLE>.<tools>`) whenever
    // this build holds non-failed closed toolchain rows, so any fresh project or git worktree
    // automatically reuses pre-warmed `core`/`alloc`/`std`/`bstd`.
    keep_toolchain_base(cx, store, lent, toolchain, tools, kept, alloc, frame)?;
    store.pin(name, &[key])?;
    // This answer as its workspace's newest base: a checkout of it with none of its own starts
    // from it ([`BASES_ROOT`]). The control keeps the one home-wide pointer instead.
    #[cfg(test)]
    if base_plant() == BasePlant::OnePointer {
        store.pin(LATEST_ROOT, &[key])?;
        return Ok(saved);
    }
    saved.base = keep_base(cx, store, rows, key, frame)?;
    Ok(saved)
}

/// The bases with `pointer` as its workspace's newest ([`with_base`]), rewritten and pinned
/// with every pointer they list under [`BASES_ROOT`]. The workspace is the one this request
/// read ([`Rows::workspace`]), else the one an entry of its layout or lock names
/// ([`workspace_of_rows`]).
fn keep_base<'m, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, rows: &Rows<'_>, pointer: Key, frame: &Frame) -> Result<BaseKept, CliError> {
    let Some(layout) = layout_of(rows, frame) else { return Ok(BaseKept::NoWorkspace) };
    let lock = rows.lock_id().unwrap_or(NO_LOCK);
    let (bases, replaced) = match read_bases(cx, store, frame)? {
        Some(Some(bases)) => (bases, false),
        Some(None) => (blaze_own::Vec::new_in(frame), true),
        None => (blaze_own::Vec::new_in(frame), false),
    };
    let Some(workspace) = workspace_of_rows(&bases, rows.workspace, layout, lock) else { return Ok(BaseKept::Unknown) };
    let now = Base { workspace: workspace.id, lock, layout, root: root_of(rows), pointer, seq: 0 };
    let bases = with_base(&bases, now, frame);
    let bytes = encode_bases(&bases, frame);
    let record = Key(ContentId::of(BASES_DOMAIN, &bytes));
    store.put(cx, record, &bytes)?;
    store.pin(BASES_ROOT, &[record])?;
    let mut pointers = blaze_own::Vec::with_capacity_in(bases.len(), frame);
    pointers.extend(bases.iter().map(|it| it.pointer));
    store.pin(BASES_KEPT_ROOT, &pointers)?;
    Ok(match replaced {
        true => BaseKept::Replaced,
        false => BaseKept::Kept { entries: u32::try_from(bases.len()).unwrap_or(u32::MAX) },
    })
}

/// The bases the home keeps: `None` with no [`BASES_ROOT`]; `Some(None)` for a root or record
/// not of this form (written anew by the next save).
fn read_bases<'m, 'f, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, frame: &'f Frame) -> Result<Option<Option<blaze_own::Vec<Base, &'f Frame>>>, CliError> {
    let Some(keys) = store.named(BASES_ROOT)? else { return Ok(None) };
    // The record alone ([`BASES_ROOT`]); a root of more keys is the one-root form before
    // 2026-11-21, not this one.
    if keys.len() != 32 {
        return Ok(Some(None));
    }
    let Some(held) = store.get(cx, key_at(keys, 0))? else { return Ok(Some(None)) };
    Ok(Some(decode_bases(held.bytes, frame)))
}

/// The base a checkout with no answer of its own starts from (the module's bases), the bases
/// record read first (no file is read when the home keeps none): its lock read from `root`
/// once, its workspace's newest entry of an equal lock digest, else its workspace's newest;
/// with no lock, a lock of no path package or no entry of its workspace, its root manifest
/// read once and the newest entry of equal bytes ([`pick_by_root`]); else `None`. The control
/// ([`BasePlant::OnePointer`]) reads the home's one [`LATEST_ROOT`].
pub(crate) fn load_base<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    frame: &Frame,
) -> Result<Option<Result<Pointer<'m>, Unreadable>>, CliError> {
    #[cfg(test)]
    if base_plant() == BasePlant::OnePointer {
        return load_pointer(cx, store, LATEST_ROOT);
    }
    let Some(Some(bases)) = read_bases(cx, store, frame)? else { return Ok(None) };
    // One file of the checkout's root, whole: `false` when none is there.
    let read = |name: &str, text: &mut bstd::mem::Bytes<'_>| -> Result<bool, CliError> {
        match root.read_all(cx, &bstd::path::Path::new(name)?, text) {
            Ok(_) => Ok(true),
            Err(error) if matches!(error.kind, bstd::fs::FsKind::NotFound | bstd::fs::FsKind::IsADirectory) => Ok(false),
            Err(error) => Err(error.into()),
        }
    };
    let mut lock_text = bstd::mem::Bytes::new_in(frame, "the checkout's lock, for its workspace's base");
    let by_lock = match read(LOCK_FILE, &mut lock_text)? {
        true => Workspace::of_lock(lock_text.as_slice(), frame).and_then(|now| pick_base(&bases, now, ContentId::of(crate::sweep::FILE_DOMAIN, lock_text.as_slice()))),
        false => None,
    };
    let picked = match by_lock {
        Some(base) => Some(base),
        None => {
            let mut manifest = bstd::mem::Bytes::new_in(frame, "the checkout's root manifest, for its workspace's base");
            match read(ROOT_MANIFEST, &mut manifest)? {
                true => pick_by_root(&bases, ContentId::of(crate::sweep::FILE_DOMAIN, manifest.as_slice())),
                false => None,
            }
        }
    };
    match picked {
        Some(base) => pointer_at(cx, store, base.pointer).map(Some),
        None => Ok(None),
    }
}

/// The home-wide toolchain-only answer root: `tool-base.<PINNED>.<TRIPLE>.<tools>`.
/// Holds only closed toolchain crate rows (`CrateScope` and `ImplBucket`) with empty checkout
/// leaves so any fresh project or git worktree on the same `(toolchain, target, tools)` starts
/// with `core`/`alloc`/`std`/`bstd` already standing via [`StoredBase::OtherCheckout`].
pub(crate) fn toolchain_base_root<'f>(
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    frame: &'f Frame,
) -> blaze_own::String<&'f Frame> {
    let tc_hex = toolchain.hex();
    let tools_id = tools_digest(tools);
    let mut name = blaze_own::String::new_in(frame);
    name.push_str("tool-base.");
    name.push_str(&tc_hex.as_str()[..16]);
    name.push('.');
    name.push_str(crate::graph::TRIPLE);
    name.push('.');
    for byte in &tools_id.0[..8] {
        let _ = write!(name, "{byte:02x}");
    }
    name
}

/// Fallback lookup for a fresh checkout that has neither its own answer nor a matching workspace
/// base: loads the pre-warmed toolchain-only `Answer` pointer under
/// `tool-base.<PINNED>.<TRIPLE>.<tools>`.
pub(crate) fn load_toolchain_base<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    frame: &Frame,
) -> Result<Option<Result<Pointer<'m>, Unreadable>>, CliError> {
    let name = toolchain_base_root(toolchain, tools, frame);
    load_pointer(cx, store, name.as_str())
}

/// Copies a single engine [`blaze_engine::Row`] into `alloc` without cloning via `Clone`.
fn copy_row<'a>(row: &blaze_engine::Row<'_>, alloc: SharedAlloc<'a>) -> blaze_engine::Row<'a> {
    blaze_engine::Row {
        key: row.key,
        phase: row.phase,
        file: row.file,
        source: row.source,
        out: row.out,
        serial: row.serial,
        defines: blaze_engine::SVec::from_slice_in(&row.defines, alloc),
        reads: blaze_engine::SVec::from_slice_in(&row.reads, alloc),
        by_cell: blaze_engine::SVec::from_slice_in(&row.by_cell, alloc),
        findings: row.findings,
        inline: row.inline,
        failed: row.failed,
    }
}

/// Writes or merges the toolchain-only fallback base (`tool-base.<PINNED>.<TRIPLE>.<tools>`)
/// whenever `kept` contains non-failed toolchain rows (`store.is_toolchain`). Unchanged bases
/// perform no writes; new toolchain sets (e.g. `bstd` added after `std`) are merged cleanly.
#[expect(clippy::too_many_arguments, reason = "toolchain base save edge: each argument one handle")]
fn keep_toolchain_base<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
) -> Result<(), CliError> {
    let mut fresh_names: blaze_own::Vec<Name, &Frame> = blaze_own::Vec::new_in(frame);
    for (&name, row) in kept.answer.rows.iter() {
        if !row.failed && store.is_toolchain(name) && kept.records.contains_key(&name) {
            fresh_names.push(name);
        }
    }
    if fresh_names.is_empty() {
        return Ok(());
    }
    let tc_root = toolchain_base_root(toolchain, tools, frame);
    let prev = match load_pointer(cx, store, tc_root.as_str())? {
        Some(Ok(ptr)) => {
            let mut store_err: Option<CliError> = None;
            let decoded = pages::decode(
                ptr.root,
                |k| {
                    if store_err.is_some() {
                        return None;
                    }
                    match store.get(cx, k) {
                        Ok(Some(held)) => Some(Ok(Sealed::by_store(k, held.bytes, held.proof))),
                        Ok(None) => None,
                        Err(error) => {
                            store_err = Some(error);
                            None
                        }
                    }
                },
                alloc,
            );
            if let Some(error) = store_err {
                return Err(error);
            }
            decoded.ok()
        }
        _ => None,
    };
    // Skip rewriting when every fresh toolchain row already stands identically in `tool-base`.
    if let Some(existing) = &prev {
        let unchanged = fresh_names.iter().all(|name| {
            let Some(new_row) = kept.answer.rows.get(name) else { return false };
            let Some(old_row) = existing.answer.rows.get(name) else { return false };
            !old_row.failed
                && old_row.key == new_row.key
                && old_row.out == new_row.out
                && existing.records.get(name) == kept.records.get(name)
        });
        if unchanged {
            return Ok(());
        }
    }
    let mut tc_kept = match prev {
        Some(mut existing) => {
            existing.places.clear();
            existing
        }
        None => Kept::new_in(alloc),
    };
    if tc_kept.answer.serial.0 < kept.answer.serial.0 {
        tc_kept.answer.serial = kept.answer.serial;
    }
    for &name in fresh_names.iter() {
        if let Some(row) = kept.answer.rows.get(&name) {
            tc_kept.answer.rows.insert(name, copy_row(row, alloc));
        }
        if let Some(&record) = kept.records.get(&name) {
            tc_kept.records.insert(name, record);
        }
        if let Some(&cause) = kept.causes.get(&name) {
            tc_kept.causes.insert(name, cause);
        }
    }
    let encoded = pages::encode(&tc_kept, None, alloc);
    for (key, bytes) in encoded.puts.iter() {
        store.put(cx, *key, bytes)?;
    }
    let (headers, writes) = Headers::after(None, &tc_kept, Writer::Phases, tc_kept.answer.serial, HeadersPlant::None, |_| Ok(None), alloc).map_err(page_error)?;
    for (key, bytes) in &writes {
        store.put(cx, *key, bytes)?;
    }
    let (empty_lines_key, empty_lines_bytes) = crate::engine_lines::Lines::new_in(alloc).record();
    store.put(cx, empty_lines_key, &empty_lines_bytes)?;
    let empty_rows = Rows::new_in(frame);
    let leaves = Leaves::bytes_in(&empty_rows, frame)?;
    let mut pointer: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::with_capacity_in(POINTER_BYTES, frame);
    pointer.extend_from_slice(POINTER_MAGIC);
    pointer.extend_from_slice(&encoded.root.0.0);
    pointer.extend_from_slice(&empty_lines_key.0.0);
    let printed = Printed { names: true, bodies_whole: false, replayable: false };
    let facts = Facts { files: encoded.files, printed, findings: encoded.findings, owed: encoded.owed };
    pointer.extend_from_slice(&facts.files.to_le_bytes());
    pointer.push(facts.printed.byte());
    pointer.extend_from_slice(&facts.findings.to_le_bytes());
    pointer.extend_from_slice(&facts.owed.to_le_bytes());
    pointer.extend_from_slice(&tools_digest(tools).0);
    match &headers {
        Headers::Current(keys) => {
            pointer.push(HEADERS_CURRENT);
            pointer.extend_from_slice(&0u64.to_le_bytes());
            for key in &keys.keys {
                pointer.extend_from_slice(&key.0.0);
            }
        }
        Headers::Absent { since } => {
            pointer.push(HEADERS_ABSENT);
            pointer.extend_from_slice(&since.0.to_le_bytes());
            pointer.extend_from_slice(&[0u8; 32 * HeaderPages::COUNT]);
        }
    }
    crate::journal_sweep::Stored::encode_units(core::iter::empty(), None, &mut pointer);
    let sealed = seal::seal(&pointer, &leaves, frame)?;
    let ptr_key = Key(ContentId::of(POINTER_DOMAIN, &sealed));
    store.put(cx, ptr_key, &sealed)?;
    use crate::engine_answer_rows::{AnswerSink as _, StoreSink};
    let mut sink = StoreSink { cx, store, lent, toolchain };
    let mut records = blaze_own::Vec::new_in(alloc);
    let mut scopes = blaze_own::Vec::new_in(alloc);
    for (page, page_rows) in encoded.page_records() {
        records.clear();
        scopes.clear();
        records.push(page);
        for &(name, record) in page_rows {
            records.push(record);
            if name.kind == blaze_drive::Kind::CrateScope {
                scopes.push((name, record));
            }
        }
        sink.index_rows(page, &records, &scopes, alloc.into()).map_err(reach_error)?;
    }
    let mut page_idx = blaze_own::Vec::new_in(alloc);
    page_idx.extend(encoded.pages.iter().filter(|p| **p != Key::default()).map(|p| blaze_cas::plain::beside_key(blaze_cas::Key(p.0))));
    page_idx.sort_unstable();
    let mut direct = blaze_own::Vec::new_in(alloc);
    direct.push(blaze_cas::Key(encoded.root.0));
    direct.push(blaze_cas::Key(empty_lines_key.0));
    if let Some(keys) = headers.current() {
        direct.extend(keys.keys.iter().filter(|k| **k != Key::default()).map(|k| blaze_cas::Key(k.0)));
    }
    let answer_index = lent.index(&direct).map_err(crate::store::PlaceError::from)?;
    let reach_name = bstd::mem::format_in!(frame, "{}.{REACH_ROOT}", tc_root.as_str());
    let answer_name = bstd::mem::format_in!(frame, "{}.{ANSWER_ROOT}", tc_root.as_str());
    let life = crate::store::roots::TOOLCHAIN_LIFE_NS;
    lent.root_through(reach_name.as_str(), &page_idx, life).map_err(crate::store::PlaceError::from)?;
    lent.root_through(answer_name.as_str(), &[answer_index], life).map_err(crate::store::PlaceError::from)?;
    lent.root_for(tc_root.as_str(), &[blaze_cas::Key(ptr_key.0)], life).map_err(crate::store::PlaceError::from)?;
    Ok(())
}

/// The reach root's page indexes for the pages `now` (one a page with anything on it, its
/// blaze-cas `beside_key`), sorted, and what was keyed for them. Under [`Rekey::Changed`] over a
/// last answer (its pages and its root's key), only the pages whose key moved are keyed (the old
/// key's index out, the new one's in); every other index is carried from the last commit's reach
/// root, read in place, once its answer root (`names.1`) proves that commit was the last answer's
/// (its one index names `last`'s root key: both roots are set in one commit). Before, every
/// commit keyed every page: 65,536 digests on tempo for a one-body edit (edit12 profile,
/// c0340wave89, `answer stored` 10.5 ms). The floor is the changed pages' digests plus one pass
/// over the carried keys (a merge of sorted lists); the comparison of `last`'s and `now`'s keys
/// is one more such pass. A reach root that is not the last answer's, or none, is made whole and
/// says why ([`WhyWhole`]); a carried root that lacks a changed page's old index is refused.
fn page_indexes<'h, D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    names: (&str, &str),
    last: Option<(&[Key], Key)>,
    now: &[Key],
    rekey: Rekey,
    alloc: SharedAlloc<'h>,
) -> Result<(blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>>, Rekeyed), CliError> {
    let index_of = |page: Key| blaze_cas::plain::beside_key(blaze_cas::Key(page.0));
    let whole = |why: WhyWhole| {
        let mut indexes = blaze_own::Vec::new_in(alloc);
        indexes.extend(now.iter().filter(|page| **page != Key::default()).map(|page| index_of(*page)));
        indexes.sort_unstable();
        let pages = u32::try_from(indexes.len()).unwrap_or(u32::MAX);
        Ok((indexes, Rekeyed { pages, whole: Some(why) }))
    };
    #[cfg(test)]
    if rekey == Rekey::All {
        return whole(WhyWhole::Planted);
    }
    #[cfg(not(test))]
    let Rekey::Changed = rekey;
    let Some((old, last_root)) = last else { return whole(WhyWhole::NoLast) };
    if old.len() != now.len() {
        return whole(WhyWhole::Paging);
    }
    fn held<'l, D: Driver>(lent: &'l blaze_cas::plain::Lent<D, crate::store::StoreAlloc>, name: &str) -> Result<Option<&'l [u8]>, CliError> {
        let Some(key) = lent.root_named(name) else { return Ok(None) };
        let held = lent.get(&key).map_err(crate::store::PlaceError::from)?;
        let held = held.ok_or_else(|| page_error(PagesError::Reach { owner: Key(key.0), index: Key(key.0), what: "a named root's record is not held" }))?;
        if held.bytes.len() % 32 != 0 {
            return Err(page_error(PagesError::Reach { owner: Key(key.0), index: Key(key.0), what: "a root's record is not whole keys" }));
        }
        Ok(Some(held.bytes))
    }
    let key_at = |bytes: &[u8], at: usize| blaze_cas::Key(ContentId(bytes[at * 32..at * 32 + 32].try_into().expect("a whole key")));
    // The proof: the answer root's one index names the last answer's root key.
    let Some(answer) = held(lent, names.1)? else { return whole(WhyWhole::NoRoot) };
    if answer.len() != 32 {
        return whole(WhyWhole::NotLast);
    }
    let direct = lent.get(&key_at(answer, 0)).map_err(crate::store::PlaceError::from)?;
    let Some(direct) = direct.filter(|got| blaze_keyed::kind::ReservedTag::Index.is(got.tag) &&got.bytes.len() % 32 == 0) else { return whole(WhyWhole::NotLast) };
    // An index's keys are sorted (blaze-cas `index`): found by halving.
    let wanted = blaze_cas::Key(last_root.0);
    let (mut low, mut high) = (0usize, direct.bytes.len() / 32);
    let mut names_last = false;
    while low < high {
        let mid = low + (high - low) / 2;
        match key_at(direct.bytes, mid).cmp(&wanted) {
            core::cmp::Ordering::Less => low = mid + 1,
            core::cmp::Ordering::Greater => high = mid,
            core::cmp::Ordering::Equal => {
                names_last = true;
                break;
            }
        }
    }
    if !names_last {
        return whole(WhyWhole::NotLast);
    }
    let Some(carried) = held(lent, names.0)? else { return whole(WhyWhole::NoRoot) };
    // The changed pages: the old key's index out, the new one's in.
    let mut out: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>> = blaze_own::Vec::new_in(alloc);
    let mut into: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>> = blaze_own::Vec::new_in(alloc);
    let mut pages = 0u32;
    for (was, is) in old.iter().zip(now.iter()).filter(|(was, is)| was != is) {
        pages = pages.saturating_add(1);
        if *was != Key::default() {
            out.push(index_of(*was));
        }
        if *is != Key::default() {
            into.push(index_of(*is));
        }
    }
    out.sort_unstable();
    into.sort_unstable();
    // One merge of three sorted lists: the carried keys but `out`, and `into`.
    let count = carried.len() / 32;
    let mut indexes: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>> = blaze_own::Vec::with_capacity_in((count + into.len()).saturating_sub(out.len()), alloc);
    let (mut gone, mut added) = (0usize, 0usize);
    for at in 0..count {
        let key = key_at(carried, at);
        while added < into.len() && into[added] < key {
            indexes.push(into[added]);
            added += 1;
        }
        if gone < out.len() && out[gone] == key {
            gone += 1;
            continue;
        }
        if gone < out.len() && out[gone] < key {
            return Err(page_error(PagesError::Reach { owner: Key(last_root.0), index: Key(out[gone].0), what: "the last answer's reach root lacks a changed page's old index" }));
        }
        indexes.push(key);
    }
    if gone < out.len() {
        return Err(page_error(PagesError::Reach { owner: Key(last_root.0), index: Key(out[gone].0), what: "the last answer's reach root lacks a changed page's old index" }));
    }
    indexes.extend_from_slice(&into[added..]);
    Ok((indexes, Rekeyed { pages, whole: None }))
}

fn reach_error(error: crate::engine_answer_rows::PublishError) -> CliError {
    use crate::engine_answer_rows::PublishError;
    match error {
        PublishError::Command(error) => error,
        PublishError::Cas(error) => crate::store::PlaceError::from(error).into(),
        PublishError::ExpansionIndex { record, index, .. } => page_error(PagesError::Reach { owner: record, index, what: "invalid expansion index" }),
        PublishError::RowsIndex { segment, index } => page_error(PagesError::Reach { owner: segment, index, what: "missing payload index" }),
        PublishError::RowsReach { segment, .. } => page_error(PagesError::Reach {
            owner: segment, index: Key(blaze_cas::plain::beside_key(blaze_cas::Key(segment.0)).0), what: "page payload reach exceeds the CAS index limit",
        }),
    }
}

#[cfg(test)]
#[path = "tests/engine_answer_page_edit.rs"]
mod page_edit;

#[cfg(test)]
#[path = "tests/engine_bases_tests.rs"]
mod bases_tests;
