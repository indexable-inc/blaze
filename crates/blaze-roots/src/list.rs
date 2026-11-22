//! [`ReadList`]: what a run saw, and [`ReadList::under`], the one check that it lies under the
//! run's roots, whose proof [`Checked`] the code making a record takes (rule 74).

#![blaze::sans_io]

use core::cmp::Ordering;

use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{AVec, Alloc, Vec};

use crate::Limits;
use crate::path::RelPath;
use crate::plant::RootsPlant;
use crate::roots::{Digest, RootId, RootKind, Roots, part, wide};

/// What one lookup saw.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seen {
    /// A file, whole: its content's digest and length.
    File { digest: Digest, len: u64 },
    /// Nothing there (lib.rs, "A failed lookup is an input"). A path that is a directory where
    /// a file was asked is a `Listing` of it, never `Absent`.
    Absent,
    /// A directory, listed: the digest of its names ([`crate::listing_digest`]), only those its
    /// root admits.
    Listing { digest: Digest },
    /// A directory seen to exist and not listed (a `stat`, clang's include search, a macro's
    /// `Path::is_dir`): only its being a directory is an input, so a file added under it moves
    /// nothing. A listing of the same path covers it ([`ReadList::under`] keeps the listing).
    Dir,
}

impl Seen {
    fn rank(self) -> u8 {
        match self {
            Seen::File { .. } => 0,
            Seen::Absent => 1,
            Seen::Listing { .. } => 2,
            Seen::Dir => 3,
        }
    }

    /// Whether two sights of one path agree: equal, or a listing and a sight of the directory.
    fn agrees(self, other: Seen) -> bool {
        self == other || matches!((self, other), (Seen::Listing { .. }, Seen::Dir) | (Seen::Dir, Seen::Listing { .. }))
    }
}

/// One entry, as read back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry<'l> {
    pub root: RootId,
    pub path: RelPath<'l>,
    pub seen: Seen,
}

/// The list is at a cap of its [`Limits`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Full {
    Entries { most: u32 },
    Text { most: u32 },
}

/// Why a read list is not under its roots: the entry it is about is [`Refused::path`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outside {
    /// The entry names a root the [`Roots`] lack.
    UnknownRoot { root: RootId },
    /// The root's globs do not admit the path (a file, a failed lookup or a listing).
    NotDeclared { root: RootId },
    /// A read under the output root: an output is not an input (roots.rs).
    OutputRead { root: RootId },
    /// One path seen two ways in one run (two digests, a file and an absence, a file and a
    /// directory): it moved while the run read it, and no record can be made of the run.
    Moved { root: RootId },
}

/// Options of a read list.
pub struct ReadListOpts<'a> {
    pub limits: Limits,
    pub alloc: Alloc<'a>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Row {
    root: RootId,
    at: u32,
    len: u32,
    seen: Seen,
}

/// What a run saw, in the order it saw it, until [`ReadList::under`] puts it in canonical order.
pub struct ReadList<'a> {
    text: AVec<'a, u8>,
    rows: AVec<'a, Row>,
    limits: Limits,
}

impl<'a> ReadList<'a> {
    pub fn new(opts: ReadListOpts<'a>) -> ReadList<'a> {
        ReadList { text: Vec::new_in(opts.alloc), rows: Vec::new_in(opts.alloc), limits: opts.limits }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Records one lookup. Nothing is checked here but the caps: the check is [`ReadList::under`].
    pub fn push(&mut self, root: RootId, path: RelPath<'_>, seen: Seen) -> Result<(), Full> {
        let most = self.limits.most_entries;
        if u32::try_from(self.rows.len()).map_or(true, |n| n >= most) {
            return Err(Full::Entries { most });
        }
        let bytes = path.as_str().as_bytes();
        let most_text = self.limits.most_text;
        let at = u32::try_from(self.text.len()).map_err(|_| Full::Text { most: most_text })?;
        let len = u32::try_from(bytes.len()).map_err(|_| Full::Text { most: most_text })?;
        if at.checked_add(len).is_none_or(|end| end > most_text) {
            return Err(Full::Text { most: most_text });
        }
        self.text.extend_from_slice(bytes);
        self.rows.push(Row { root, at, len, seen });
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The entries, in the order pushed.
    pub fn entries(&self) -> impl Iterator<Item = Entry<'_>> + '_ {
        self.rows.iter().map(move |row| self.entry(row))
    }

    fn entry(&self, row: &Row) -> Entry<'_> {
        Entry { root: row.root, path: path_of(&self.text, row), seen: row.seen }
    }

    /// THE CHECK: every entry names a root of `roots` that admits it, and no path was seen two
    /// ways. On success the list is in canonical order (by root, then path bytewise, then the
    /// kind of sight), each entry once, and comes back as the proof [`Checked`].
    pub fn under(self, roots: &Roots<'_>) -> Result<Checked<'a>, Refused<'a>> {
        self.under_planted(roots, NoPlant)
    }

    /// [`ReadList::under`] with a plant for the simulation's controls (plant.rs).
    pub fn under_planted(mut self, roots: &Roots<'_>, plant: impl Planted<RootsPlant>) -> Result<Checked<'a>, Refused<'a>> {
        for at in 0..self.rows.len() {
            let row = self.rows[at];
            let path = path_of(&self.text, &row);
            let Some(root) = roots.get(row.root) else {
                return Err(Refused { list: self, at, why: Outside::UnknownRoot { root: row.root } });
            };
            let admitted = match (root.kind, row.seen) {
                (RootKind::Output, _) if plant.has(RootsPlant::OutputReadable) => true,
                (RootKind::Output, _) => {
                    return Err(Refused { list: self, at, why: Outside::OutputRead { root: row.root } });
                }
                (RootKind::Package { .. }, _) if plant.has(RootsPlant::GlobsIgnored) => true,
                (_, Seen::File { .. } | Seen::Absent) => root.admits_file(path) || (row.seen == Seen::Absent && root.admits_dir(path)),
                (_, Seen::Listing { .. } | Seen::Dir) => root.admits_dir(path),
            };
            if !admitted {
                return Err(Refused { list: self, at, why: Outside::NotDeclared { root: row.root } });
            }
        }
        let text = &self.text;
        // Unstable is enough: the order compares every field, so rows it calls equal are equal.
        self.rows.sort_unstable_by(|a, b| order(text, a, b));
        let mut kept = 0;
        for at in 0..self.rows.len() {
            let row = self.rows[at];
            if kept > 0 {
                let last = self.rows[kept - 1];
                if same_path(&self.text, &last, &row) {
                    // A `Dir` ranks last, so the listing it agrees with is the one kept.
                    if last.seen.agrees(row.seen) || plant.has(RootsPlant::MovedKept) {
                        continue;
                    }
                    return Err(Refused { list: self, at, why: Outside::Moved { root: row.root } });
                }
            }
            self.rows[kept] = row;
            kept += 1;
        }
        self.rows.truncate(kept);
        Ok(Checked { list: self })
    }
}

fn path_of<'t>(text: &'t [u8], row: &Row) -> RelPath<'t> {
    let start = usize::try_from(row.at).unwrap_or(usize::MAX);
    let end = start.saturating_add(usize::try_from(row.len).unwrap_or(usize::MAX));
    let bytes = text.get(start..end).unwrap_or(&[]);
    // Pushed from a `RelPath`, so the text is a canonical path: this cannot fail.
    RelPath::from_bytes(bytes, u32::MAX).unwrap_or(RelPath::ROOT)
}

fn order(text: &[u8], a: &Row, b: &Row) -> Ordering {
    a.root
        .cmp(&b.root)
        .then_with(|| path_of(text, a).as_str().as_bytes().cmp(path_of(text, b).as_str().as_bytes()))
        .then_with(|| a.seen.rank().cmp(&b.seen.rank()))
        .then_with(|| seen_bytes(a.seen).cmp(&seen_bytes(b.seen)))
}

fn same_path(text: &[u8], a: &Row, b: &Row) -> bool {
    a.root == b.root && path_of(text, a) == path_of(text, b)
}

/// The digest and length a sight carries, for ordering and the list's digest.
fn seen_bytes(seen: Seen) -> ([u8; 32], u64) {
    match seen {
        Seen::File { digest, len } => (digest.0, len),
        Seen::Absent | Seen::Dir => ([0; 32], 0),
        Seen::Listing { digest } => (digest.0, 0),
    }
}

/// A read list proven under its roots, in canonical order, each path once: the only form the
/// code that makes a record from a run takes. Its only constructor is [`ReadList::under`].
pub struct Checked<'a> {
    list: ReadList<'a>,
}

impl<'a> Checked<'a> {
    pub fn entries(&self) -> impl Iterator<Item = Entry<'_>> + '_ {
        self.list.entries()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The list's identity: each entry's root, path and sight, in canonical order. Two runs
    /// that saw the same things have the same digest whatever order they saw them in.
    pub fn digest(&self) -> Digest {
        let mut h = blaze_blake3::Hasher::new_derive_key("blaze-roots 2026-11-20 read list");
        h.update(&wide(self.list.len()).to_le_bytes());
        for entry in self.entries() {
            h.update(&[entry.root.0, entry.seen.rank()]);
            part(&mut h, entry.path.as_str().as_bytes());
            let (digest, len) = seen_bytes(entry.seen);
            h.update(&digest);
            h.update(&len.to_le_bytes());
        }
        Digest(h.finalize())
    }

    /// Gives the list back (to add to it and check again).
    pub fn into_list(self) -> ReadList<'a> {
        self.list
    }
}

/// [`ReadList::under`]'s refusal: why, and the entry it is about, with the list handed back.
pub struct Refused<'a> {
    list: ReadList<'a>,
    at: usize,
    why: Outside,
}

impl<'a> Refused<'a> {
    pub fn why(&self) -> Outside {
        self.why
    }

    /// The entry refused: its root and path name the file (rule 19).
    pub fn entry(&self) -> Entry<'_> {
        self.list.entry(&self.list.rows[self.at])
    }

    pub fn into_list(self) -> ReadList<'a> {
        self.list
    }
}

impl core::fmt::Debug for Refused<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Refused").field("why", &self.why).field("entry", &self.entry()).finish()
    }
}
