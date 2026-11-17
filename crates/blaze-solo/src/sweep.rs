//! The sweep: the leaf table against the disk, sans-IO. Its driver stamps every row in
//! batches ([`Sweep::names`], [`Sweep::stamped`]), lists each directory the sweep asks for
//! ([`Sweep::listed`]: one whose stamp moved or is racy, or new) and reads the first bytes of
//! each `CACHEDIR.TAG` it asks to probe ([`Sweep::probed`]); [`Sweep::finish`] gives the rows
//! now and which hashed rows must be hashed again. [`changed`] is the difference of two row
//! sets: paths added, removed, and changed (content id differs).
//!
//! Git's racy rule, through [`blaze_vfs::racy`] (the window's one home): a saved stamp vouches
//! for a row only when it equals the stamp now and is older than the table's instant by more
//! than the file system's grain. A hashed row whose stamp moved or is racy is hashed again; a
//! directory whose stamp moved or is racy is listed again. A row never hashed (no step read
//! it) is compared by stamp alone: nothing depends on its bytes.
//!
//! The view: everything under the checkout but what [`blaze_vfs::watch::fixed_out`] leaves
//! out (the root's `target`, every `.git`) and every directory below the root holding a
//! `CACHEDIR.TAG` that starts with the cache directory tag's signature. Such a directory is a
//! [`Kind::Hidden`] row and its tag a [`Kind::Tag`] row; nothing else under it is a row.

#![blaze::sans_io]

use std::collections::{BTreeMap, BTreeSet};

use blaze_core::Stamp;
use blaze_vfs::watch::fixed_out;

use crate::{
    Plant, planted,
    table::{Kind, Row},
};

/// A cache directory tag's file name. Its home is `blaze_vfs::watch::view::TAG_NAME`, which
/// is `pub(crate)` there (crates/blaze-vfs/src/watch/view.rs:28): to move to its public
/// surface.
pub const TAG_NAME: &str = "CACHEDIR.TAG";

/// What a tag starts with, the 43 bytes the spec names. Its home is
/// `blaze_vfs::watch::view::SIGNATURE`, `pub(crate)` there (view.rs:31): to move likewise.
pub const SIGNATURE: &[u8; 43] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// What two row sets differ by, for a step's reads: paths of files (and links) only.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Changes {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// In both, with a content id that differs, or of another kind (a file become a link).
    pub changed: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    /// Names stamped.
    pub stated: u64,
    /// Directories listed.
    pub listed: u64,
    /// Tags probed.
    pub probed: u64,
    /// Rows whose stamp was equal but racy: not trusted, and the table is written again so a
    /// later run's instant can be past their window.
    pub racy: u64,
}

/// What the sweep asks its driver to look at next.
#[derive(Default, Debug)]
pub struct Walk {
    /// Directories to list, by path.
    pub lists: Vec<String>,
    /// Tags to probe, by path: their first [`SIGNATURE`]'s length of bytes.
    pub probes: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SweepError {
    /// The checkout's own directory is not one.
    NotADirectory { stamp: Stamp },
    /// The driver handed back another number of stamps than names.
    Stamps { asked: usize, got: usize },
}

/// What a sweep ends with.
pub struct Swept {
    /// The table's rows as they were.
    pub old: Vec<Row>,
    /// The rows now, sorted. A row in `dirty` still holds its old id until it is hashed again.
    pub rows: Vec<Row>,
    /// Hashed rows to hash again: their stamp moved or is racy.
    pub dirty: Vec<String>,
    pub counts: Counts,
}

pub struct Sweep {
    old: Vec<Row>,
    instant_ns: u64,
    rows: BTreeMap<String, Row>,
    dirty: BTreeSet<String>,
    /// Directories whose tag is being probed: the new subdirectories found beside it, listed
    /// only once the probe says the directory is in the view.
    parked: BTreeMap<String, Vec<String>>,
    counts: Counts,
}

impl Sweep {
    /// A sweep of `old`, a table looked at from `instant_ns` on.
    pub fn new(old: Vec<Row>, instant_ns: u64) -> Sweep {
        Sweep {
            old,
            instant_ns,
            rows: BTreeMap::new(),
            dirty: BTreeSet::new(),
            parked: BTreeMap::new(),
            counts: Counts::default(),
        }
    }

    fn has_root(&self) -> bool {
        self.old.first().is_some_and(|row| row.path.is_empty())
    }

    /// The paths to stamp, in order: the checkout's own (the empty path) first, then every
    /// row's. [`Sweep::stamped`] takes their stamps in this order.
    pub fn names(&self) -> impl Iterator<Item = &str> + '_ {
        let root = (!self.has_root()).then_some("");
        root.into_iter().chain(self.old.iter().map(|row| row.path.as_str()))
    }

    /// How many names [`Sweep::names`] gives.
    pub fn name_count(&self) -> usize {
        self.old.len() + usize::from(!self.has_root())
    }

    /// The stamps of [`Sweep::names`], in order.
    pub fn stamped(&mut self, stamps: &[Stamp]) -> Result<Walk, SweepError> {
        let asked = self.name_count();
        if stamps.len() != asked {
            return Err(SweepError::Stamps { asked, got: stamps.len() });
        }
        self.counts.stated += stamps.len() as u64;
        let mut walk = Walk::default();
        let mut stamps = stamps.iter();
        if !self.has_root() {
            let stamp = *stamps.next().expect("counted");
            if !stamp.is_dir() {
                return Err(SweepError::NotADirectory { stamp });
            }
            // alloc: the root's row
            self.rows.insert(String::new(), Row { path: String::new(), stamp, id: None, kind: Kind::Dir });
            // alloc: the root's path
            walk.lists.push(String::new());
        }
        let old = std::mem::take(&mut self.old);
        for (row, stamp) in old.iter().zip(stamps) {
            self.look(row, *stamp, &mut walk)?;
        }
        self.old = old;
        // A hidden directory whose tag is no longer one comes back into the view.
        let orphans: Vec<String> = self
            .rows
            .iter()
            .filter(|(path, row)| {
                row.kind == Kind::Hidden && !self.rows.get(&tag_path(path)).is_some_and(|tag| tag.kind == Kind::Tag)
            })
            // clone: one path a directory that comes back
            .map(|(path, _)| path.clone())
            .collect();
        for dir in orphans {
            self.rows.get_mut(&dir).expect("listed above").kind = Kind::Dir;
            walk.lists.push(dir);
        }
        Ok(walk)
    }

    /// Whether `old` still vouches for a row stamped `now`: equal, and out of its racy window
    /// as of the table's instant.
    fn trusted(&mut self, old: &Stamp, now: &Stamp) -> bool {
        if old != now {
            return false;
        }
        if planted(Plant::TrustsRacy) {
            return true;
        }
        if blaze_vfs::racy(old, self.instant_ns) {
            self.counts.racy += 1;
            return false;
        }
        true
    }

    fn look(&mut self, row: &Row, stamp: Stamp, walk: &mut Walk) -> Result<(), SweepError> {
        if row.path.is_empty() {
            if !stamp.is_dir() {
                return Err(SweepError::NotADirectory { stamp });
            }
            let trusted = self.trusted(&row.stamp, &stamp);
            // alloc: the root's row
            self.rows.insert(String::new(), Row { path: String::new(), stamp, id: None, kind: Kind::Dir });
            if !trusted {
                walk.lists.push(String::new());
            }
            return Ok(());
        }
        if stamp.is_gone() {
            if row.kind == Kind::File && planted(Plant::KeepsRemoved) {
                // clone: the planted bug keeps the row
                self.rows.insert(row.path.clone(), row.clone());
            }
            return Ok(());
        }
        let now = Kind::of(&stamp);
        // clone: the row's path, kept by the rows now
        let path = row.path.clone();
        match row.kind {
            Kind::Hidden if now == Kind::Dir => {
                self.rows.insert(path.clone(), Row { path, stamp, id: None, kind: Kind::Hidden });
            }
            Kind::Tag if now == Kind::File => {
                let trusted = self.trusted(&row.stamp, &stamp);
                self.rows.insert(path.clone(), Row { path: path.clone(), stamp, id: None, kind: Kind::Tag });
                if !trusted {
                    walk.probes.push(path);
                }
            }
            Kind::Hidden | Kind::Tag => self.enter(path, stamp, walk),
            kind if kind != now => self.enter(path, stamp, walk),
            Kind::File => {
                let compare = row.id.is_some() || is_tag_name(&path);
                let trusted = !compare || self.trusted(&row.stamp, &stamp);
                if row.id.is_some() && !trusted {
                    self.dirty.insert(path.clone());
                }
                if !trusted && tag_candidate(&path, &stamp) {
                    walk.probes.push(path.clone());
                }
                self.rows.insert(path.clone(), Row { path, stamp, id: row.id, kind: Kind::File });
            }
            Kind::Link => {
                self.rows.insert(path.clone(), Row { path, stamp, id: None, kind: Kind::Link });
            }
            Kind::Dir => {
                let trusted = self.trusted(&row.stamp, &stamp);
                self.rows.insert(path.clone(), Row { path: path.clone(), stamp, id: None, kind: Kind::Dir });
                if !trusted {
                    walk.lists.push(path);
                }
            }
        }
        Ok(())
    }

    /// A row new to the table: a directory is listed, a tag probed.
    fn enter(&mut self, path: String, stamp: Stamp, walk: &mut Walk) {
        let kind = Kind::of(&stamp);
        let candidate = kind == Kind::File && tag_candidate(&path, &stamp);
        // clone: the row's path, its key and its own
        self.rows.insert(path.clone(), Row { path: path.clone(), stamp, id: None, kind });
        if kind == Kind::Dir {
            walk.lists.push(path);
        } else if candidate {
            walk.probes.push(path);
        }
    }

    /// The listing of `dir`: each entry's last name and stamp, as the driver gave them.
    pub fn listed(&mut self, dir: &str, entries: &[(String, Stamp)]) -> Walk {
        let mut walk = Walk::default();
        // Out of the view since it was asked for (a tag probed hid it, or it went).
        if !self.rows.get(dir).is_some_and(|row| row.kind == Kind::Dir) {
            return walk;
        }
        self.counts.listed += 1;
        // alloc: the prefix of the directory's children, once a listing
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let seen: BTreeMap<String, Stamp> = entries
            .iter()
            // alloc: one path an entry of the listing
            .map(|(name, stamp)| (format!("{prefix}{name}"), *stamp))
            .filter(|(path, _)| !fixed_out(path))
            .collect();
        let held: Vec<String> = self
            .rows
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.starts_with(&prefix))
            .filter(|(path, _)| !path.is_empty() && !path[prefix.len()..].contains('/'))
            .filter(|(path, _)| !seen.contains_key(*path))
            // clone: one path a row the listing does not have
            .map(|(path, _)| path.clone())
            .collect();
        for path in held {
            self.remove(&path);
        }
        let mut dirs = Vec::new();
        let mut tag = None;
        for (path, stamp) in seen {
            if self.rows.contains_key(&path) {
                continue;
            }
            let kind = Kind::of(&stamp);
            if kind == Kind::Dir {
                // clone: the new directory's path, its row's and the list's
                dirs.push(path.clone());
            } else if kind == Kind::File && tag_candidate(&path, &stamp) {
                tag = Some(path.clone());
            }
            self.rows.insert(path.clone(), Row { path, stamp, id: None, kind });
        }
        match tag {
            Some(tag) => {
                walk.probes.push(tag);
                self.parked.insert(dir.to_owned(), dirs);
            }
            None => walk.lists = dirs,
        }
        walk
    }

    /// The first bytes of the tag at `path`.
    pub fn probed(&mut self, path: &str, bytes: &[u8]) -> Walk {
        self.counts.probed += 1;
        let mut walk = Walk::default();
        let Some(dir) = tag_of(path) else { return walk };
        let is_tag = bytes.len() >= SIGNATURE.len() && bytes[..SIGNATURE.len()] == SIGNATURE[..];
        let parked = self.parked.remove(dir).unwrap_or_default();
        match (self.rows.get(dir).map(|row| row.kind), is_tag) {
            (Some(Kind::Dir), true) => self.hide(dir, path),
            (Some(Kind::Dir), false) => walk.lists = parked,
            (Some(Kind::Hidden), false) => {
                self.rows.get_mut(dir).expect("matched").kind = Kind::Dir;
                if let Some(tag) = self.rows.get_mut(path) {
                    tag.kind = Kind::File;
                }
                // alloc: the directory's path, listed again
                walk.lists.push(dir.to_owned());
            }
            _ => {}
        }
        walk
    }

    /// `dir` out of the view by its tag at `tag`: everything under it but the tag goes.
    fn hide(&mut self, dir: &str, tag: &str) {
        // alloc: the prefix, once a hide
        let prefix = format!("{dir}/");
        let under: Vec<String> = self
            .rows
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.starts_with(&prefix))
            .filter(|(path, _)| path.as_str() != tag)
            // clone: one path a row that leaves the view
            .map(|(path, _)| path.clone())
            .collect();
        for path in under {
            self.rows.remove(&path);
            self.dirty.remove(&path);
        }
        self.rows.get_mut(dir).expect("a directory to hide").kind = Kind::Hidden;
        if let Some(row) = self.rows.get_mut(tag) {
            row.kind = Kind::Tag;
            row.id = None;
        }
        self.dirty.remove(tag);
    }

    /// The row at `path` gone, with everything under it.
    fn remove(&mut self, path: &str) {
        match self.rows.get(path).map(|row| row.kind) {
            Some(Kind::File) if planted(Plant::KeepsRemoved) => return,
            Some(Kind::Dir | Kind::Hidden) => {
                // alloc: the prefix, once a directory gone
                let prefix = format!("{path}/");
                let under: Vec<String> = self
                    .rows
                    .range(prefix.clone()..)
                    .take_while(|(it, _)| it.starts_with(&prefix))
                    // clone: one path a row gone
                    .map(|(it, _)| it.clone())
                    .collect();
                for it in under {
                    self.rows.remove(&it);
                    self.dirty.remove(&it);
                }
            }
            _ => {}
        }
        self.rows.remove(path);
        self.dirty.remove(path);
    }

    pub fn finish(self) -> Swept {
        let dirty = self
            .dirty
            .into_iter()
            .filter(|path| self.rows.get(path).is_some_and(|row| row.kind == Kind::File && row.id.is_some()))
            .collect();
        Swept { old: self.old, rows: self.rows.into_values().collect(), dirty, counts: self.counts }
    }
}

/// The difference of two sorted row sets, for a step's reads.
pub fn changed(old: &[Row], new: &[Row]) -> Changes {
    let mut changes = Changes::default();
    let mut old = old.iter().filter(|row| row.kind.is_file_like()).peekable();
    let mut new = new.iter().filter(|row| row.kind.is_file_like()).peekable();
    loop {
        match (old.peek(), new.peek()) {
            (None, None) => break,
            (Some(o), Some(n)) if o.path == n.path => {
                if o.kind != n.kind || (o.id.is_some() && o.id != n.id) {
                    // clone: one path a change
                    changes.changed.push(n.path.clone());
                }
                old.next();
                new.next();
            }
            (Some(o), Some(n)) if o.path < n.path => {
                changes.removed.push(o.path.clone());
                old.next();
            }
            (Some(o), None) => {
                changes.removed.push(o.path.clone());
                old.next();
            }
            (_, Some(n)) => {
                changes.added.push(n.path.clone());
                new.next();
            }
        }
    }
    changes
}

/// The directory `path` would be the tag of (`a/b` for `a/b/CACHEDIR.TAG`); `None` for any
/// other name, and for a tag at the root, which never hides the checkout.
pub fn tag_of(path: &str) -> Option<&str> {
    let dir = path.strip_suffix(TAG_NAME)?.strip_suffix('/')?;
    (!dir.is_empty()).then_some(dir)
}

fn is_tag_name(path: &str) -> bool {
    tag_of(path).is_some()
}

/// Whether a file at `path` stamped `stamp` may be a tag: named so below the root, a regular
/// file at least as long as the signature.
fn tag_candidate(path: &str, stamp: &Stamp) -> bool {
    is_tag_name(path) && stamp.is_file() && stamp.size >= SIGNATURE.len() as u64
}

fn tag_path(dir: &str) -> String {
    // alloc: a tag's path, looked up
    format!("{dir}/{TAG_NAME}")
}
