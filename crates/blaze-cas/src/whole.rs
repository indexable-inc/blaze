//! One whole file a live placed output (handoff-7 section 2): what names each whole file the
//! store keeps of an output (`placed/<content>`, a derived one's `.derived`, and a value kept
//! as a file of its own), and when one that nothing names goes.
//!
//! A whole file is named by every path last placed from it. A place that replaces a path's
//! file takes one name from the file the path held; a file with no name left is removed. Two
//! paths placed from one file (a program and its second path, two profiles of one output) are
//! two names, so replacing one leaves the file to the other: a file is never removed while a
//! second place still names it. A place from no whole file (a maker's runs, a patch, a second
//! path's link, a value's bytes) clears its path's name ([`Names::cleared`]).
//!
//! A pin names no whole file: a pin keeps records (the collector's roots), and a whole file is
//! a cache of a record's bytes, made again from it by the next place that wants it. So the
//! store's pins are the one home of what a pin keeps, and the bound on `placed/` is the
//! current outputs of the build dirs.
//!
//! What survives a crash is the *listing*: the names, and the key each path was placed from,
//! written whole after they change (`placed/names`). It is written with no sync
//! ([`blaze_core::Effect::WriteUnsynced`]): it is a cache of what keeps whole files, which a
//! place's answer never waits for, and an edit's place asks for no write that waits for a
//! device. So a crash may leave any part of it, and it ends in a check of all before it
//! ([`Names::decode`]): one that fails it is what a crash left of a write, which names nothing
//! (the open's sweep then keeps only the homes its head names) and is logged, not kept as a
//! problem: it is what a crash may leave, not a fault of the disk's.
//! The key a path holds is the collector's root and what lets go of a home once the path is
//! placed again ([`Names::hold`]), across a restart as within a run. So the protocol, one place:
//!
//! 1. [`Names::making`]: the file the place is made from (and a derived value's base) is named
//!    by the place in flight, in memory, before it is written or cloned from; nothing removes
//!    it meanwhile.
//! 2. [`Names::placed`]: the place is answered; the path's old file loses the path's name and
//!    the place in flight's name goes to the path. The listing is written ([`Names::listing`]).
//! 3. [`Names::doomed`] of the old file and the place's own is every one of them no name and
//!    no place in flight holds: each is removed (once no clone of it is in flight).
//!
//! The answer waits for no listing: every path holds a clone of its file (its own inode), so
//! a whole file is a cache that no path's bytes depend on, and a listing that a crash left
//! behind its places only keeps a file or two until those paths are placed again, or names a
//! file that is gone, which the next place that wants it makes again. The open reads the
//! listing ([`Names::decode`]) and removes the whole files of `placed/` it does not name
//! before any place is begun: a crash leaves at most the files of places it cut short, and
//! they go then.
//!
//! The invariant (`tests::whole`, a deterministic simulation that crashes at every step, and
//! `tests::place_crash` over the store's own routes): no file a live place (one answered)
//! names is removed, and once a place is answered and its removals done, no file outside the
//! names and the places in flight is on disk; so `placed/` holds the current outputs of each
//! path (the program and its debug file), never one for every build.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{alloc::Global, collections::BTreeMap};

use blaze_ids::{ContentId, crc32c as crc};

use crate::Key;

/// What a listing of the names starts with; the last byte is the layout's number: 2 lists
/// each path's key and ends in a check ([`Names::listing`]).
const NAMES_MAGIC: [u8; 8] = *b"BZPLACE2";

/// The magic of layout 1 (until 2026-11-13): paths and files, no keys and no check, written
/// with a sync. Still read: its paths name their files and hold no key.
const NAMES_MAGIC_1: [u8; 8] = *b"BZPLACE1";

/// An entry of a listing of layout 2 names a whole file.
const LISTS_FILE: u8 = 1;
/// An entry of a listing of layout 2 names a key.
const LISTS_KEY: u8 = 2;

/// What names each whole file: paths placed from it and places in flight making it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Names {
    /// The whole file each path was last placed from.
    paths: BTreeMap<String, ContentId, Global>, // alloc: the store's own table, a few names an output placed
    /// The key each path was last placed from (none for a maker's output, a patch): the
    /// collector's roots of placed outputs, and what is let go of when the path is placed
    /// from another ([`Names::hold`]). Listed, so it holds across an open.
    keys: BTreeMap<String, Key, Global>, // alloc: the store's own table, a name a path placed
    /// How many places in flight are made from each: in memory alone, never listed.
    making: BTreeMap<ContentId, u32, Global>, // alloc: the store's own table, the places in flight
    /// The paths changed since the last [`Names::take_changed`]: what says a listing is due.
    /// A place that leaves every path naming what it named (a maker's output at a path that
    /// named none, a place refused) writes no listing, so a build that places only made
    /// outputs asks for no write that waits for a sync.
    changed: bool,
    /// A planted bug for the simulation: a file's names kept as a set, as the collector's
    /// placed keys were (`collect::Collecting::placed`), so the file a path is replaced from
    /// goes with the path's name although a second path still names it.
    #[cfg(test)]
    pub(crate) plant_one_name: bool,
    /// The planted bug's memory: the file the last place replaced.
    #[cfg(test)]
    replaced: Option<ContentId>,
}

/// A place ended whose making was never counted ([`Names::making`]): the store's own books
/// disagree, so the step that met it stops the store naming the file and the path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct NotBegun {
    pub(crate) file: ContentId,
}

impl std::fmt::Display for NotBegun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a place made of {:x} ended, but none was begun (`Names::making`)", self.file)
    }
}

impl Names {
    /// A place in flight makes `file` (step 1): it is named until the place ends.
    pub(crate) fn making(&mut self, file: ContentId) {
        *self.making.entry(file).or_default() += 1;
    }

    /// A place in flight that named `file` ended ([`Names::making`]), naming no path by it:
    /// a derived value's base, once the place made of it is answered.
    pub(crate) fn made_from(&mut self, file: ContentId) -> Result<(), NotBegun> {
        match self.making.get_mut(&file) {
            Some(1) => {
                self.making.remove(&file);
            }
            Some(count) => *count -= 1,
            None => return Err(NotBegun { file }),
        }
        Ok(())
    }

    /// The place in flight of `file` at `path` was made (step 2): `path` names `file`, and
    /// what it named before loses that name, which is returned where it was another file.
    /// `made` false: the place failed, and the path keeps what it held. The listing to write
    /// next is [`Names::listing`].
    pub(crate) fn placed(
        &mut self,
        path: &str,
        file: ContentId,
        made: bool,
    ) -> Result<Option<ContentId>, NotBegun> {
        self.made_from(file)?;
        if !made {
            return Ok(None);
        }
        // alloc: once per path first placed
        let held = self.paths.insert(path.to_owned(), file);
        self.changed |= held != Some(file);
        let was = held.filter(|it| *it != file);
        #[cfg(test)]
        {
            self.replaced = was;
        }
        Ok(was)
    }

    /// `path` now holds a file made from no whole file of the store's (a maker's runs, a
    /// patch, a second path's link, a value's bytes): it names none, and the one it named
    /// loses that name, which is returned.
    pub(crate) fn cleared(&mut self, path: &str) -> Option<ContentId> {
        let was = self.paths.remove(path);
        self.changed |= was.is_some();
        #[cfg(test)]
        {
            self.replaced = was;
        }
        was
    }

    /// The names a listing read as the store opens, under the places in flight begun before
    /// it was read (which are in memory alone).
    pub(crate) fn opened(&mut self, listed: Names) {
        self.paths = listed.paths;
        self.keys = listed.keys;
    }

    /// `path` now holds `key` (none: a maker's output, a patch, a place refused that left it
    /// empty): the key it held before is returned where it was another, which no longer roots
    /// anything through this path.
    pub(crate) fn hold(&mut self, path: &str, key: Option<Key>) -> Option<Key> {
        let held = match key {
            // alloc: once per path first placed from a key
            Some(key) => self.keys.insert(path.to_owned(), key),
            None => self.keys.remove(path),
        };
        self.changed |= held != key;
        held.filter(|it| Some(*it) != key)
    }

    /// Whether a path holds `key`.
    pub(crate) fn holds(&self, key: &Key) -> bool {
        self.keys.values().any(|it| it == key)
    }

    /// The keys the paths hold, a key once for each path that holds it.
    pub(crate) fn held_keys(&self) -> impl Iterator<Item = &Key> {
        self.keys.values()
    }

    /// Whether the paths changed since this was last asked (or since the listing was read),
    /// and none from now: a listing is to be written when it says so. A listing that failed
    /// to be written is due again ([`Names::unwritten`]).
    pub(crate) fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// The listing asked last was not written: the next step writes the names again.
    pub(crate) fn unwritten(&mut self) {
        self.changed = true;
    }

    /// The file each placed path names, as listed.
    #[cfg(test)]
    pub(crate) fn paths_listed(&self) -> BTreeMap<String, ContentId> {
        // clone: the names, for whoever compares them (a test, a census)
        self.paths.clone()
    }

    /// How many names `file` holds: the paths placed from it and the places in flight
    /// making it.
    pub(crate) fn names(&self, file: &ContentId) -> u32 {
        let paths = self.paths.values().filter(|it| *it == file).count() as u32;
        paths + self.making.get(file).copied().unwrap_or(0)
    }

    /// How many places in flight are made from `file` ([`Names::making`]).
    pub(crate) fn in_flight(&self, file: &ContentId) -> u32 {
        self.making.get(file).copied().unwrap_or(0)
    }

    /// The files of `on_disk` that nothing names: what goes once the listing is durable.
    pub(crate) fn doomed(&self, on_disk: impl IntoIterator<Item = ContentId>) -> Vec<ContentId> {
        // alloc: the files to remove, a few a place
        let mut out: Vec<ContentId> = on_disk.into_iter().filter(|it| self.names(it) == 0).collect();
        // The planted bug: the file a path was replaced from goes whatever else names it.
        #[cfg(test)]
        out.extend(
            self.plant_one_name.then_some(self.replaced).flatten().filter(|it| !self.making.contains_key(it)),
        );
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The names as a listing, the same bytes for the same names: the magic, the count of
    /// paths, then each path in order (`u32` length, the path, a byte of [`LISTS_FILE`] and
    /// [`LISTS_KEY`], the file and the key it lists), then the CRC-32C of all before it.
    /// Places in flight are not listed: one not answered when a crash comes is undone.
    pub(crate) fn listing(&self) -> Vec<u8> {
        let mut all: Vec<&str> = self.paths.keys().chain(self.keys.keys()).map(String::as_str).collect(); // alloc: the paths listed, once a write
        all.sort_unstable();
        all.dedup();
        // alloc: the listing written, once a change of the names
        let mut out = Vec::with_capacity(16 + all.len() * 112);
        out.extend_from_slice(&NAMES_MAGIC);
        out.extend_from_slice(&(all.len() as u32).to_le_bytes());
        for path in all {
            let (file, key) = (self.paths.get(path), self.keys.get(path));
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
            let flags = u8::from(file.is_some()) * LISTS_FILE | u8::from(key.is_some()) * LISTS_KEY;
            out.push(flags);
            if let Some(file) = file {
                out.extend_from_slice(&file.0);
            }
            if let Some(key) = key {
                out.extend_from_slice(&key.0.0);
            }
        }
        let check = crc::crc32c(&out);
        out.extend_from_slice(&check.to_le_bytes());
        out
    }

    /// The names a listing holds; no place in flight. An empty listing (none written yet)
    /// holds no names. `Ok(None)`: bytes that are no listing of layout 1 and fail the check of
    /// layout 2, what a crash left of a write with no sync, which name nothing. An error names
    /// the offset and what is wrong there: a listing that passes its check (or one of layout
    /// 1, written with a sync) and does not read as one is no tear but a bug.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Names>, String> {
        if bytes.is_empty() {
            return Ok(Some(Names::default()));
        }
        let (layout, body) = match bytes.get(..8) {
            Some(magic) if magic == NAMES_MAGIC_1 => (1, bytes),
            Some(magic) if magic == NAMES_MAGIC && bytes.len() >= 12 => {
                let (body, check) = bytes.split_at(bytes.len() - 4);
                match check == &crc::crc32c(body).to_le_bytes()[..] {
                    true => (2, body),
                    false => return Ok(None),
                }
            }
            // Written with no sync, a listing a crash cut may hold any part of its bytes, its
            // magic among them: whatever fails the check names nothing.
            _ => return Ok(None),
        };
        let mut at = Reading { bytes: body, at: 8 };
        let mut names = Names::default();
        for _ in 0..at.u32()? {
            let len = at.u32()? as usize;
            let path = std::str::from_utf8(at.take(len)?)
                // alloc: an error
                .map_err(|why| format!("at {}: a path that is not UTF-8: {why}", at.at - len))?;
            let flags = match layout {
                1 => LISTS_FILE,
                _ => at.take(1)?[0],
            };
            if flags & !(LISTS_FILE | LISTS_KEY) != 0 || flags == 0 {
                // alloc: an error
                return Err(format!("at {}: the path {path} lists {flags:#x}, neither a file nor a key", at.at - 1));
            }
            if names.paths.contains_key(path) || names.keys.contains_key(path) {
                // alloc: an error
                return Err(format!("at {}: the path {path} is listed twice", at.at));
            }
            if flags & LISTS_FILE != 0 {
                let file = at.id()?;
                // alloc: once per path listed, at open
                names.paths.insert(path.to_owned(), file);
            }
            if flags & LISTS_KEY != 0 {
                let key = Key(at.id()?);
                // alloc: once per path listed, at open
                names.keys.insert(path.to_owned(), key);
            }
        }
        if at.at != body.len() {
            // alloc: an error
            return Err(format!("at {}: {} bytes follow the last path", at.at, body.len() - at.at));
        }
        Ok(Some(names))
    }
}

struct Reading<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reading<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(len).filter(|&end| end <= self.bytes.len()).ok_or_else(|| {
            // alloc: an error
            format!("at {}: it ends {len} bytes short of what it names", self.at)
        })?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn id(&mut self) -> Result<ContentId, String> {
        Ok(ContentId(self.take(32)?.try_into().expect("32 bytes")))
    }
}
