//! Library files' parses kept across a daemon restart (docs/v2/plan/lanes/tree-store/design.md):
//! the trees of every library file's outline, with the bodies parsed so far, stored in a
//! section of the daemon's image by the text's content ID, its edition and the parse's kind,
//! and read back in place: on restart a dependency file's parse is a lookup and a few checks,
//! not a lex and a parse.
//!
//! The section, little-endian, every offset from its start (a multiple of 8 in the image):
//! - a head: magic, [`syntax::image::TREE_VERSION`], [`syntax::IMAGE_LAYOUT`], how many
//!   entries, the index's offset (`u64`), 0 (`u64`);
//! - the parses, each a record of `syntax::image` from a multiple of 8;
//! - the index: an [`ENTRY`]-byte row a parse, sorted by key: the text's content ID (32
//!   bytes), the edition, 1 for an outline, two bytes of 0, the record's length (`u32`) and
//!   offset (`u64`).
//!
//! What a stored parse depends on is its key and the versions in the head and the schema
//! ([`SCHEMA`]): the text (its content ID), the edition, the kind of parse, the lexer's,
//! parser's and grammar's version and the tree's layout. A section of another version is
//! refused whole at [`Trees::open`]; an entry that is not its parse is an error naming the
//! file, never a silent parse ([`Trees::map`]). A text with no entry is parsed as any other
//! and stored with the next image ([`write_section`]).

use std::sync::{Arc, OnceLock};

use blaze_bytes::Bytes;
use blaze_format::{Part, SchemaHash, schema_of};
use blaze_ids::ContentId;
use span::Edition;
use syntax::{IMAGE_LAYOUT, Parse, SourceFile, Text, image::TREE_VERSION};

use crate::Db;

/// The section's schema: its layout here, the parse record's and the tree record's versions.
pub const SCHEMA: SchemaHash = schema_of(&[
    Part::Str(
        "blaze v2 base-db library trees 1: head [u32; 4] u64 u64; parse records; index rows \
         [u8; 32] u8 u8 [u8; 2] u32 u64",
    ),
    Part::U64(TREE_VERSION as u64),
    Part::U64(IMAGE_LAYOUT as u64),
]);

const MAGIC: u32 = u32::from_le_bytes(*b"bzts");
const HEAD: usize = 32;
/// An index row's bytes.
const ENTRY: usize = 48;

/// The stored parses of a section: see the module.
pub struct Trees {
    bytes: Bytes,
    entries: usize,
    index: usize,
    /// The first entry that failed to read, sticky: [`write_section`] carries none of this
    /// section's entries into the next image after one.
    failed: OnceLock<String>,
}

/// Why a section or one of its entries cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)] // clone: an error, cold
pub struct TreeError(pub String);

impl std::fmt::Display for TreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TreeError {}

/// A stored parse's key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
struct Key {
    text: [u8; 32],
    edition: u8,
    outline: u8,
}

impl Key {
    fn new(text: ContentId, edition: Edition, outline: bool) -> Key {
        Key { text: text.0, edition: edition as u8, outline: u8::from(outline) }
    }
}

impl Db {
    /// The stored trees this database reads library parses from: [`install`]'s when it was
    /// made, or [`Db::set_trees`]'.
    pub fn trees(&self) -> Option<&Arc<Trees>> {
        self.parses.trees.as_ref()
    }

    /// Has this database read library parses from `trees` from now on. The parses are the
    /// same with or without them, so nothing is invalidated: a stored tree is a parse's tree.
    pub fn set_trees(&mut self, trees: Option<Arc<Trees>>) {
        self.parses.trees = trees;
    }
}

/// The trees every database made from now on reads ([`install`]).
static INSTALLED: OnceLock<Arc<Trees>> = OnceLock::new();

/// Has every [`Db`] made from now on read library parses from `trees` (the daemon's image's,
/// at start); once per process: `false`, and nothing changed, when trees were installed before.
pub fn install(trees: Arc<Trees>) -> bool {
    INSTALLED.set(trees).is_ok()
}

/// The trees [`install`] installed.
pub fn installed() -> Option<&'static Arc<Trees>> {
    INSTALLED.get()
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at.checked_add(8)?)?.try_into().ok()?))
}

impl Trees {
    /// The section in `bytes` (its whole length, checked against its CRCs by the image): its
    /// head and its index's order checked, no parse read yet.
    pub fn open(bytes: Bytes) -> Result<Trees, TreeError> {
        // alloc: the error's message, once per refused section
        let wrong = |what: &str| TreeError(format!("the image's library trees: {what}"));
        let head = |at| u32_at(&bytes, at).ok_or_else(|| wrong("a head past the section's end"));
        if head(0)? != MAGIC {
            return Err(wrong("not a section of library trees"));
        }
        if head(4)? != TREE_VERSION || head(8)? != IMAGE_LAYOUT {
            // alloc: as above
            return Err(wrong(&format!(
                "trees of parser version {} and layout {}, where this blaze's are {TREE_VERSION} \
                 and {IMAGE_LAYOUT}",
                head(4)?,
                head(8)?,
            )));
        }
        let entries = head(12)? as usize;
        let index = u64_at(&bytes, 16).and_then(|it| usize::try_from(it).ok());
        let end = index.and_then(|at| entries.checked_mul(ENTRY)?.checked_add(at));
        let (Some(index), Some(end)) = (index, end) else {
            return Err(wrong("an index past the section's end"));
        };
        if end > bytes.len() || index < HEAD || index % 8 != 0 {
            return Err(wrong("an index past the section's end"));
        }
        let trees = Trees { bytes, entries, index, failed: OnceLock::new() };
        let sorted = (1..entries).all(|n| trees.key(n - 1) < trees.key(n));
        if !sorted {
            return Err(wrong("an index out of order"));
        }
        Ok(trees)
    }

    /// No stored parses: what a database reads without an image.
    pub fn empty() -> Trees {
        // alloc: an empty section's head, once
        let mut out = vec![0; HEAD];
        finish(&mut out, 0, &[]);
        Trees::open(Bytes::from(Arc::<[u8]>::from(out))).expect("an empty section")
    }

    pub fn len(&self) -> usize {
        self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }

    /// The first entry that failed to read, if one did.
    pub fn failed(&self) -> Option<&str> {
        self.failed.get().map(String::as_str)
    }

    fn row(&self, n: usize) -> &[u8] {
        let at = self.index + ENTRY * n;
        &self.bytes[at..at + ENTRY]
    }

    fn key(&self, n: usize) -> Key {
        let row = self.row(n);
        let text = row[..32].try_into().expect("32 bytes");
        Key { text, edition: row[32], outline: row[33] }
    }

    /// Where entry `n`'s record is: its offset and length.
    fn record(&self, n: usize) -> (usize, usize) {
        let row = self.row(n);
        let len = u32_at(row, 36).expect("in the row") as usize;
        let at = u64_at(row, 40).expect("in the row");
        (usize::try_from(at).unwrap_or(usize::MAX), len)
    }

    fn find(&self, key: Key) -> Option<usize> {
        let (mut lo, mut hi) = (0, self.entries);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.key(mid).cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// Whether a parse of the text of content ID `text` in `edition` and of the kind is
    /// stored.
    pub fn has(&self, text: ContentId, edition: Edition, outline: bool) -> bool {
        self.find(Key::new(text, edition, outline)).is_some()
    }

    /// The stored parse of `text` (of content ID `id`) in `edition` and of the kind, its tree
    /// read in place; `None` when none is stored. An entry that is not that parse is an error,
    /// recorded ([`Trees::failed`]).
    pub(crate) fn map(
        &self,
        id: ContentId,
        text: &Text,
        edition: Edition,
        outline: bool,
    ) -> Result<Option<Parse<SourceFile>>, TreeError> {
        let Some(n) = self.find(Key::new(id, edition, outline)) else { return Ok(None) };
        let (at, len) = self.record(n);
        let inside = at.checked_add(len).is_some_and(|end| end <= self.index) && at >= HEAD;
        let read = match inside {
            true => syntax::image::read(&self.bytes, at, text, edition, outline)
                // alloc: the error's message, once per stored tree refused
                .map_err(|it| it.to_string()),
            // alloc: as above
            false => Err(format!("a record at {at} of {len} bytes outside the section's parses")),
        };
        read.map(Some).map_err(|error| {
            // alloc: as above
            let error = TreeError(format!(
                "the stored tree of the text {id:x} ({edition:?}, {}): {error}",
                if outline { "outline" } else { "full parse" },
            ));
            let _ = self.failed.set(error.0.clone()); // clone: the error path, once
            error
        })
    }
}

/// Appends a section of the library files' parses `db` holds to `out` (from its start, a
/// multiple of 8), for the next image: every library file's outline alive now, with the
/// bodies walked so far, and every entry of `db`'s stored trees whose text a file still has
/// that was not parsed this time, copied as it is (none after an entry failed to read).
pub fn write_section(db: &Db, out: &mut Vec<u8>) {
    assert!(out.len().is_multiple_of(8), "a section starts at a multiple of 8");
    let base = out.len();
    out.resize(base + HEAD, 0);
    // alloc: the index, once per image written
    let mut rows: Vec<(Key, u64, u32)> = Vec::new();
    // alloc: as above
    let mut written = blaze_hash::HashSet::default();
    // alloc: as above
    let mut texts: Vec<[u8; 32]> = Vec::new();
    for input in db.files().texts() {
        let reparse = input.reparse_field(db);
        texts.push(reparse.id().0);
        for (edition, parse) in reparse.kept_outlines() {
            let key = Key::new(reparse.id(), edition, true);
            if written.insert(key) {
                let record = syntax::image::write(&parse, out);
                rows.push((key, (record.start - base) as u64, to_u32(record.len())));
            }
        }
    }
    if let Some(stored) = db.trees().filter(|it| it.failed().is_none()) {
        texts.sort_unstable();
        for n in 0..stored.entries {
            let key = stored.key(n);
            if texts.binary_search(&key.text).is_err() || written.contains(&key) {
                continue;
            }
            let (at, len) = stored.record(n);
            let Some(bytes) = at.checked_add(len).and_then(|end| stored.bytes.get(at..end)) else {
                continue;
            };
            pad(out);
            let start = out.len() - base;
            out.extend_from_slice(bytes);
            rows.push((key, start as u64, to_u32(len)));
        }
    }
    rows.sort_unstable_by_key(|it| it.0);
    finish(out, base, &rows);
}

/// Writes the index of `rows` (offsets from the section's start, `base` in `out`, sorted) at
/// the section's end and its head over the room left for it.
fn finish(out: &mut Vec<u8>, base: usize, rows: &[(Key, u64, u32)]) {
    pad(out);
    let index = out.len() - base;
    for (key, at, len) in rows {
        out.extend_from_slice(&key.text);
        out.extend_from_slice(&[key.edition, key.outline, 0, 0]);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
    }
    let words = [MAGIC, TREE_VERSION, IMAGE_LAYOUT, to_u32(rows.len())];
    for (n, word) in words.into_iter().enumerate() {
        out[base + 4 * n..base + 4 * n + 4].copy_from_slice(&word.to_le_bytes());
    }
    out[base + 16..base + 24].copy_from_slice(&(index as u64).to_le_bytes());
}

fn pad(out: &mut Vec<u8>) {
    out.resize(out.len().next_multiple_of(8), 0);
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).expect("a stored parse of under 4 GiB")
}

#[cfg(test)]
mod tests;
