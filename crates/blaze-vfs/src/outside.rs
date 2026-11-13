//! What changed outside every view (docs/v2/plan/design/vfs.md): the directories user
//! code reads that no view holds (the toolchain, the C compiler's, `/usr/include`, each traced
//! path's parent), watched so a check asks "has anything under this changed since then?" of a
//! table, not of the disk.
//!
//! - [`Dirs`] is the table, one per process ([`dirs`]): a generation counter bumped by every
//!   change heard and every watch confirmed, and for each directory two atomics, `since` (the
//!   generation it has been watched without a break since; [`UNCONFIRMED`] while it is not)
//!   and `changed` (the generation of the last change heard under it). A read ([`generation`],
//!   [`unchanged_since`]) is acquire loads only: no lock, no shared write, no allocation.
//! - [`want`] adds a directory to watch, once per directory per process.
//! - [`Outside`] is the sans-IO driver of the watches, as [`crate::Watcher`] is: a wanted
//!   directory becomes an [`Effect::Watch`], confirmed by an [`Effect::Stat`] of it asked
//!   right after, which every driver runs after the watch is in place (the order
//!   [`crate::Watcher::start`] relies on: each driver sets a watch up as it is submitted).
//!   A change bumps the generation and marks its directory; an end, a failure, a lost report
//!   or a change naming the directory itself (it was replaced or moved, or may have been)
//!   stops it vouching at once, and it is watched again.
//! - Events come after the change that made them (FSEvents' queue, a thread reading inotify),
//!   so a round of [`Effect::Flush`]es ([`Outside::flush`]) is the barrier: until every flush
//!   of the round is in, nothing vouches, and once it is, every change made before the round
//!   began has been heard. The daemon runs one round per request, before its checks.
//!
//! - **By path** ([`Dirs::only_since`]): the table also keeps the last [`CHANGES_KEPT`] changes
//!   heard, each with its generation and the path the system named, so a caller that knows
//!   which paths it rests on can tell a change to one of them from a change beside them in the
//!   same directory (a build's kept answer and another build's output placed next to its own).
//!   A lost report, a directory replaced and a watch that stopped vouching are kept as a change
//!   with no path, which no caller can set aside; a caller asking from before what is kept is
//!   told no.
//!
//! Sound: [`unchanged_since`]`(path, g)` is true only when some directory at or above `path`
//! was confirmed before `g` was read (`since <= g`: its watch was in place before whatever the
//! caller looked at after reading `g`), heard nothing after (`changed <= g`), and the last
//! round is in. A directory the driver's root cannot name is never confirmed: each check of
//! it looks at the disk, which is the accurate answer.

use std::{
    hash::Hasher as _,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use blaze_core::{Completion, Done, Effect, FileName, Tag};
use blaze_hash::LazyKey;
use blaze_mem::{AppendVec, MemError, RegionVec};

use crate::sync::{
    Lock,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

/// The `since` of a directory not watched without a break: above every generation, so it
/// vouches for none.
pub const UNCONFIRMED: u64 = u64::MAX;

/// Table `k` of the index has `TABLE << k` places, at most half full; the tables lie end to end
/// in one column, table `k` from place `TABLE * (2^k - 1)`.
const TABLE: usize = 64;
const TABLES: usize = 16;
const PLACES: usize = TABLE * ((1 << TABLES) - 1);
/// The most directories: what the last table holds half full. A build wants a few hundred
/// (most are covered by the toolchain's, the SDK's and `/usr/include`), so this is address space
/// only: 32 MB of entries, 32 MB of index and 64 MB of paths, committed as they fill.
const MOST: u32 = (TABLE << (TABLES - 1)) as u32 / 2;
/// The most bytes of the directories' paths together: 64 on average at [`MOST`].
const PATH_BYTES: usize = 64 * MOST as usize;

/// The most changes heard that the table keeps by path ([`Dirs::only_since`]): a build's
/// placements between two requests (a few thousand outputs at most, a path each). Past it the
/// oldest go, and a caller asking from before them is told no.
pub const CHANGES_KEPT: usize = 4096;

/// The changes heard, by path, oldest first ([`Dirs::only_since`]).
struct Changes {
    /// The generation of the newest change let go of: what is kept is every change after it.
    from: u64,
    /// Each change's generation and path; no path for one that names nothing a caller could
    /// set aside (a lost report, a directory replaced, a watch that stopped vouching).
    heard: std::collections::VecDeque<(u64, Option<PathBuf>)>,
}

/// A directory watched: its two generations, the directory wanted later that covers it, and
/// where its path is, as it was wanted. `since` and `changed` lead a 16-byte-aligned struct, so a
/// check's two loads are always on one cache line.
#[repr(C, align(16))]
struct Entry {
    since: AtomicU64,
    changed: AtomicU64,
    /// The number plus one of a directory above it wanted after it, which covers it; 0 for none.
    subsumed_by: AtomicU32,
    /// Its path's bytes: `paths[start..start + len]`.
    start: u32,
    len: u32,
    /// Someone asks of everything under it ([`Dirs::want`]): `changed` is kept for it. A
    /// directory wanted only for names in it ([`Dirs::watch_name`]) has a watch and no
    /// `changed`: what is heard under it moves the names it names, nothing else.
    recursive: AtomicBool,
}

#[cfg(not(blaze_check))]
const _: () = assert!(size_of::<Entry>() == 32);

/// A directory of a [`Dirs`]: its generations and its path.
#[derive(Clone, Copy)]
pub struct Dir<'d> {
    entry: &'d Entry,
    bytes: &'d [u8],
}

impl<'d> Dir<'d> {
    pub fn path(self) -> &'d Path {
        as_path(self.bytes)
    }

    /// The number of a directory above it wanted after it, whose watch covers it.
    pub fn subsumed_by(self) -> Option<u32> {
        self.entry.subsumed_by.load(Ordering::Acquire).checked_sub(1)
    }

    /// The generation it has been watched without a break since, or [`UNCONFIRMED`].
    pub fn since(self) -> u64 {
        self.entry.since.load(Ordering::Acquire)
    }

    /// The generation of the last change heard under it.
    pub fn changed(self) -> u64 {
        self.entry.changed.load(Ordering::Acquire)
    }
}

/// A path's bytes as [`Dirs::want`] kept them.
#[cfg(unix)]
fn as_path(bytes: &[u8]) -> &Path {
    use std::os::unix::ffi::OsStrExt;
    Path::new(std::ffi::OsStr::from_bytes(bytes))
}

/// A path's bytes as [`Dirs::want`] kept them: UTF-8 off Unix.
#[cfg(not(unix))]
fn as_path(bytes: &[u8]) -> &Path {
    Path::new(std::str::from_utf8(bytes).expect("a directory is wanted only as UTF-8"))
}

/// The counters every check reads, on a line pair of their own: the driver writes them once
/// per change heard, never per check.
#[repr(align(128))]
struct Hot {
    generation: AtomicU64,
    /// The last round of flushes is in: false while one is out, and until the first is in.
    settled: AtomicBool,
    /// The generation of the last change heard under any directory, and of the last watch
    /// that stopped vouching ([`Dirs::quiet_since`]).
    disturbed: AtomicU64,
}

/// The directories watched outside every view, append-only, in columns over reserved regions
/// tagged [`blaze_mem::Tag::Vfs`] that one writer (under `adding`) appends to and readers read
/// with no lock: never moved, so nothing is copied as they grow and a `&` into them lives as long
/// as the table. A directory is found by a hash of its bytes in an open-addressed index of
/// `u64` places (the hash's high half and the number plus one, 0 empty), grown by rehashing into
/// a table twice the size laid after it in `places`, the old ones kept for readers still probing
/// them. A lookup compares the bytes, so two paths of one hash are two directories.
pub struct Dirs {
    hot: Hot,
    /// One [`Outside`] drives the table at a time.
    driven: AtomicBool,
    /// How many directories are published: each below it is in `entries` and in the index.
    len: AtomicU32,
    /// Which table of `places` is in use.
    level: AtomicU32,
    entries: AppendVec<Entry>,
    /// The directories' paths, one after another.
    paths: AppendVec<u8>,
    places: AppendVec<AtomicU64>,
    /// Held to add a directory: cold, once per directory per process.
    adding: Lock<()>,
    /// The last changes heard, by path: written by the driver once a change heard, read only
    /// by a check that the generations alone refused ([`Dirs::only_since`]).
    changes: Lock<Changes>,
    /// The directories names were asked of ([`Dirs::watch_name`]), each directory above them
    /// too, by path: what was heard of each.
    names: Lock<blaze_hash::HashMap<Box<[u8]>, Named, std::alloc::Global>>,
}

/// What was heard of a directory a name was asked in, or of one above such a directory.
#[derive(Clone, Copy)]
struct Named {
    /// The generation it was first asked of at: what was heard before it is not here.
    since: u64,
    /// The generation of the last change heard that named the directory itself (it was made,
    /// removed, moved or replaced).
    own: u64,
    /// The generation of the last change heard that named an entry directly in it.
    direct: u64,
}

/// The places of table `level` in `places`.
fn span(level: usize) -> std::ops::Range<usize> {
    TABLE * ((1 << level) - 1)..TABLE * ((1 << (level + 1)) - 1)
}

/// What a directory's hash is keyed by, drawn at random on first use: the directories are
/// where user code (a build script, a proc macro, a traced read) looked, so a hash anyone can
/// compute would let it pick thousands with one tag, each then probing past all of them.
static DIR_KEY: LazyKey = LazyKey::new();

/// Marks the end of a part in a key's hash: the parts are hashed in turn, so one pass over a
/// path gives the tag of every directory above it.
const PART: u8 = 0xff;

/// Where a key's probe starts and the half of its hash a place keeps.
fn tag(key: &[u8]) -> u32 {
    let mut last = 0;
    prefixes(key, |_, tag| {
        last = tag;
        true
    });
    last
}

/// Calls `each(end, tag)` for each directory at or above `bytes`, shortest first: `bytes[..end]`
/// and its [`tag`], in one pass over the bytes. Stops when `each` says false.
fn prefixes(bytes: &[u8], mut each: impl FnMut(usize, u32) -> bool) {
    let mut hasher = DIR_KEY.get().hasher();
    let mut start = 0;
    for at in 0..=bytes.len() {
        if at < bytes.len() && !is_separator(bytes[at]) {
            continue;
        }
        hasher.write(&bytes[start..at]);
        hasher.write_u8(PART);
        start = at + 1;
        // The part before the first separator of an absolute path is the file system's root.
        if at > 0 && !each(at, (hasher.finish() >> 32) as u32) {
            return;
        }
    }
}

/// `path`'s bytes without trailing separators, as [`Dirs::want`] keys a directory.
fn trimmed(path: &Path) -> &[u8] {
    let mut bytes = path.as_os_str().as_encoded_bytes();
    while bytes.len() > 1 && is_separator(bytes[bytes.len() - 1]) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

/// The directory that covers a path, found once: a check of it is three loads on two lines, no
/// hash and no walk ([`Cover::unchanged_since`]). A directory found stays the same one for the
/// table's life, so a cover kept with an answer is good for as long as the answer is.
#[derive(Clone, Copy)]
pub struct Cover<'d> {
    dirs: &'d Dirs,
    entry: &'d Entry,
}

impl<'d> Cover<'d> {
    /// [`Dirs::unchanged_since`] of every path under this directory.
    ///
    /// A directory wanted above this one later takes its watch over (the driver ends this
    /// one's, which then never vouches again): the directory asked is then that one, and so
    /// on up. Until 2026-11-12 a cover kept with an answer stayed on the retired directory,
    /// so every file of glibc's `lib` (wanted first, then its package's directory for its
    /// `etc`) was looked at on the disk in every build for the daemon's life.
    pub fn unchanged_since(self, generation: u64) -> bool {
        if !self.dirs.hot.settled.load(Ordering::Acquire) {
            return false;
        }
        // Its own directory heard nothing, under a watch unbroken since: its own, or that of
        // the directory that took it over.
        if !self.entry.recursive.load(Ordering::Acquire)
            || self.entry.changed.load(Ordering::Acquire) > generation
        {
            return false;
        }
        let mut entry = self.entry;
        // Bounded: each directory above is a shorter path.
        loop {
            if entry.since.load(Ordering::Acquire) <= generation {
                return true;
            }
            match entry.subsumed_by.load(Ordering::Acquire) {
                0 => return false,
                _ if COVER_STAYS_ON_THE_RETIRED.get() => return false,
                by => entry = self.dirs.dir(by - 1).entry,
            }
        }
    }

    pub fn dir(self) -> Dir<'d> {
        self.dirs.view(self.entry)
    }
}

fn entry(tag: u32, raw: u32) -> u64 {
    (tag as u64) << 32 | (raw as u64 + 1)
}

fn is_separator(byte: u8) -> bool {
    byte.is_ascii() && std::path::is_separator(byte as char)
}

impl Dirs {
    /// An empty table, its columns reserved and nothing committed; stops when the address space
    /// cannot be had.
    pub fn new() -> Dirs {
        Dirs::try_new()
            .unwrap_or_else(|error| panic!("the table of directories outside views: {error}"))
    }

    pub fn try_new() -> Result<Dirs, MemError> {
        let tag = blaze_mem::Tag::Vfs;
        Ok(Dirs {
            hot: Hot {
                generation: AtomicU64::new(0),
                settled: AtomicBool::new(false),
                disturbed: AtomicU64::new(0),
            },
            driven: AtomicBool::new(false),
            len: AtomicU32::new(0),
            level: AtomicU32::new(0),
            entries: AppendVec::new(MOST as usize, tag)?,
            paths: AppendVec::new(PATH_BYTES, tag)?,
            places: AppendVec::new(PLACES, tag)?,
            adding: Lock::new("vfs.outside.adding", ()),
            changes: Lock::new(
                "vfs.outside.changes",
                // alloc: none until the first change heard
                Changes { from: 0, heard: std::collections::VecDeque::new() },
            ),
            names: Lock::new(
                "vfs.outside.names",
                blaze_hash::HashMap::new_in(std::alloc::Global),
            ),
        })
    }

    /// The generation now: one load. A caller stores it with what it looked at after, and asks
    /// [`Dirs::unchanged_since`] with it later.
    pub fn generation(&self) -> u64 {
        self.hot.generation.load(Ordering::Acquire)
    }

    /// Whether nothing at `path` can have changed since `generation` was read: a directory at
    /// or above it has been watched without a break since before then and heard of no change
    /// under it after, and the last round of flushes is in. False is "look at the disk".
    ///
    /// Ancestors are found by bytes, as `path` is named: ask only of a canonical path (no
    /// symlink in it, none of `.` or `..`), since a watch hears nothing of a symlink's target
    /// outside it, nor of a symlink above it being pointed elsewhere.
    pub fn unchanged_since(&self, path: &Path, generation: u64) -> bool {
        if !self.hot.settled.load(Ordering::Acquire) {
            return false;
        }
        let bytes = trimmed(path);
        // A directory asked of whole ([`Dirs::want`]) at or above the path that heard nothing
        // since, under a watch (its own, or one of a directory above it) unbroken since.
        let (mut watched, mut vouched) = (false, false);
        prefixes(bytes, |end, tag| {
            if let Some(dir) = self.find(&bytes[..end], tag) {
                watched |= dir.since() <= generation;
                vouched = watched
                    && dir.entry.recursive.load(Ordering::Acquire)
                    && dir.changed() <= generation;
            }
            !vouched
        });
        vouched
    }

    /// Asks the watches of the one name `name` (a file, a link, a directory, or a name that
    /// is not there): its directory is watched, and what is heard that names it, an entry
    /// directly in its directory, or a directory above it, is kept for
    /// [`Dirs::name_unchanged_since`]. Nothing is asked of what is under its directory
    /// otherwise: a home directory something writes under every second still vouches for
    /// `~/.gitconfig`. Until 2026-11-12 a name's directory was asked of whole, so a build
    /// script that looked at `/root/wt` made `/root` and everything under it (the daemon's
    /// own home) one directory that was never quiet, and no build was answered again
    /// without a walk. `name` is canonical but for its last part, as [`Dirs::want`] asks.
    pub fn watch_name(&self, name: &Path) {
        let Some(dir) = name.parent() else { return };
        self.want_as(dir, false);
        let bytes = trimmed(dir);
        let mut names = self.names.lock();
        prefixes(bytes, |end, _| {
            if !names.contains_key(&bytes[..end]) {
                let since = self.bump();
                // alloc: once per directory a name is asked in, and per directory above it
                names.insert(bytes[..end].into(), Named { since, own: 0, direct: 0 });
            }
            true
        });
    }

    /// Whether the name `name` ([`Dirs::watch_name`]) is what it was when `generation` was
    /// read: a watch at or above its directory is unbroken since, and since then nothing
    /// heard named it or an entry beside it, or a directory above it. False is "look at the
    /// disk". No allocation.
    pub fn name_unchanged_since(&self, name: &Path, generation: u64) -> bool {
        if !self.hot.settled.load(Ordering::Acquire) || NAMES_HEAR_NOTHING_BESIDE.get() {
            return false;
        }
        let Some(dir) = name.parent() else { return false };
        let bytes = trimmed(dir);
        let mut watched = false;
        prefixes(bytes, |end, tag| {
            watched = self.find(&bytes[..end], tag).is_some_and(|dir| dir.since() <= generation);
            !watched
        });
        if !watched {
            return false;
        }
        let names = self.names.lock();
        let mut same = true;
        prefixes(bytes, |end, _| {
            same = names.get(&bytes[..end]).is_some_and(|it| {
                it.since <= generation
                    && it.own <= generation
                    && (end < bytes.len() || it.direct <= generation)
            });
            same
        });
        same
    }

    /// The changes `files` (each named from `root`, the driver's) were heard at `generation`:
    /// each directory asked of whole at or above one is changed, and each name's directory
    /// ([`Dirs::watch_name`]) one names, or names an entry of. Whether any was.
    fn heard(&self, root: &Path, files: &[FileName], generation: u64, path: &mut PathBuf) -> bool {
        let mut hit = false;
        let mut names = self.names.lock();
        for file in files {
            // Whether this file was a change of anything asked of before it: kept by its
            // path then ([`Dirs::only_since`]), for whoever knows it is beside what it
            // rests on. A file that hit nothing moved nothing, and is not kept.
            let before = std::mem::replace(&mut hit, false);
            path.clear();
            path.push(root);
            path.push(file.as_str());
            let bytes = trimmed(path);
            prefixes(bytes, |end, tag| {
                if let Some(dir) = self.find(&bytes[..end], tag)
                    && dir.entry.recursive.load(Ordering::Acquire)
                {
                    dir.entry.changed.fetch_max(generation, Ordering::AcqRel);
                    hit = true;
                }
                true
            });
            if let Some(named) = names.get_mut(bytes) {
                named.own = generation;
                hit = true;
            }
            let parent = bytes.iter().rposition(|it| is_separator(*it)).map(|at| &bytes[..at.max(1)]);
            if let Some(named) = parent.and_then(|it| names.get_mut(it)) {
                named.direct = generation;
                hit = true;
            }
            if hit {
                self.hear(generation, Some(path.clone())); // clone: a path a change heard, kept
            }
            hit |= before;
        }
        hit
    }

    /// The directory to keep with an answer about `path`, so its later checks are
    /// [`Cover::unchanged_since`]: of the directories at or above it, the first confirmed one
    /// not covered by another, else the first not covered. `None` when none is wanted: each
    /// check of it looks at the disk. Cold: once per answer made, as `path` is stat'ed.
    pub fn cover(&self, path: &Path) -> Option<Cover<'_>> {
        let bytes = trimmed(path);
        let (mut confirmed, mut wanted) = (None, None);
        prefixes(bytes, |end, tag| {
            if let Some(dir) = self.find(&bytes[..end], tag)
                && dir.subsumed_by().is_none()
            {
                wanted = wanted.or(Some(dir));
                if dir.since() < UNCONFIRMED {
                    confirmed = Some(dir);
                }
            }
            confirmed.is_none()
        });
        confirmed.or(wanted).map(|dir: Dir<'_>| Cover { dirs: self, entry: dir.entry })
    }

    /// Whether nothing is to add for `key`: it is there (and asked of whole from now, when
    /// `recursive`), or a directory above it is, asked of whole when `recursive`.
    fn wanted(&self, key: &[u8], tag: u32, recursive: bool) -> bool {
        if let Some(dir) = self.find(key, tag) {
            if recursive && !dir.entry.recursive.load(Ordering::Acquire) {
                // What was heard under it before now is not in `changed`.
                dir.entry.changed.fetch_max(self.bump(), Ordering::AcqRel);
                dir.entry.recursive.store(true, Ordering::Release);
            }
            return true;
        }
        if !recursive {
            return self.covered(key);
        }
        let mut found = false;
        prefixes(key, |end, tag| {
            found = self
                .find(&key[..end], tag)
                .is_some_and(|dir| dir.entry.recursive.load(Ordering::Acquire));
            !found
        });
        found
    }

    /// Whether `key` or a directory above it is wanted.
    fn covered(&self, key: &[u8]) -> bool {
        let mut found = false;
        prefixes(key, |end, tag| {
            found = self.find(&key[..end], tag).is_some();
            !found
        });
        found
    }

    /// Adds `dir` to watch, if it is not there already: the driver watches it from its next
    /// [`Outside::start`] or round. The file system's root is not watched (a watch of every
    /// file on the machine would hear every write): a path that only it covers is looked at
    /// on the disk at each check. `dir` is canonical, as [`Dirs::unchanged_since`] asks.
    ///
    /// A watch hears everything under its directory, so a directory under one wanted already
    /// is not added, and one wanted above directories already there covers them: the driver
    /// ends their watches. A round then costs one flush per directory not under another (the
    /// toolchain, the SDK, `/usr/include`), not one per directory a trace named.
    pub fn want(&self, dir: &Path) {
        self.want_as(dir, true);
    }

    /// [`Dirs::want`], or, not `recursive`, a watch of `dir` for the names in it alone
    /// ([`Dirs::watch_name`]).
    fn want_as(&self, dir: &Path, recursive: bool) {
        let dir = dir.components().as_path();
        if dir.parent().is_none() {
            return;
        }
        // Off Unix a path is kept as UTF-8; one that is not is never named by a driver either
        // (`Outside::name`), so each check of it looks at the disk.
        #[cfg(not(unix))]
        if dir.to_str().is_none() {
            return;
        }
        let key = dir.as_os_str().as_encoded_bytes();
        let tag = tag(key);
        if self.wanted(key, tag, recursive) {
            return;
        }
        let _adding = self.adding.lock();
        if self.wanted(key, tag, recursive) {
            return;
        }
        let raw = self.len.load(Ordering::Relaxed);
        assert!(raw < MOST, "more than {MOST} directories watched outside every view");
        let start = self.paths.len();
        assert!(
            start as usize + key.len() <= PATH_BYTES,
            "more than {PATH_BYTES} bytes of directories watched outside every view"
        );
        for &byte in key {
            self.paths.append(byte);
        }
        // A directory asked of whole under one watched for its names: that one's watch is
        // its watch (the topmost such, which nothing covers).
        let paths = self.paths.as_slice();
        let above = self.entries.as_slice()[..raw as usize].iter().position(|other| {
            let bytes = &paths[other.start as usize..][..other.len as usize];
            other.subsumed_by.load(Ordering::Acquire) == 0
                && key.len() > bytes.len()
                && key.starts_with(bytes)
                && is_separator(key[bytes.len()])
        });
        // Published before the index names it, and the index before `len` counts it.
        let at = self.entries.append(Entry {
            since: AtomicU64::new(UNCONFIRMED),
            // Asked of whole from now: what was heard under it before is not in `changed`.
            changed: AtomicU64::new(if recursive { self.bump() } else { 0 }),
            subsumed_by: AtomicU32::new(above.map_or(0, |it| it as u32 + 1)),
            start,
            len: key.len() as u32,
            recursive: AtomicBool::new(recursive),
        });
        debug_assert_eq!(at, raw);
        let mut level = self.level.load(Ordering::Relaxed) as usize;
        if (raw as usize + 1) * 2 > TABLE << level {
            level = self.grow(level);
        }
        let table = self.table(level);
        let mask = table.len() - 1;
        let mut place = tag as usize & mask;
        while table[place].load(Ordering::Relaxed) != 0 {
            place = (place + 1) & mask;
        }
        table[place].store(entry(tag, raw), Ordering::Release);
        self.len.store(raw + 1, Ordering::Release);
        let paths = self.paths.as_slice();
        for other in &self.entries.as_slice()[..raw as usize] {
            let bytes = &paths[other.start as usize..][..other.len as usize];
            if bytes.len() > key.len() && bytes.starts_with(key) && is_separator(bytes[key.len()]) {
                _ = other.subsumed_by.compare_exchange(
                    0,
                    raw + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
        }
    }

    /// How many directories are wanted.
    pub fn len(&self) -> u32 {
        self.len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The directory numbered `at`, below [`Dirs::len`].
    pub fn dir(&self, at: u32) -> Dir<'_> {
        self.view(self.entries.get(at).expect("a directory below the table's length"))
    }

    fn view<'d>(&'d self, entry: &'d Entry) -> Dir<'d> {
        let bytes = &self.paths.as_slice()[entry.start as usize..][..entry.len as usize];
        Dir { entry, bytes }
    }

    /// Table `level`, its places appended (zero) if they are not yet: for the writer only.
    fn table(&self, level: usize) -> &[AtomicU64] {
        let span = span(level);
        while (self.places.len() as usize) < span.end {
            self.places.append(AtomicU64::new(0));
        }
        &self.places.as_slice()[span]
    }

    /// No lock, no write and no allocation: an index whose table is not appended whole yet
    /// holds nothing.
    fn find(&self, key: &[u8], tag: u32) -> Option<Dir<'_>> {
        let level = self.level.load(Ordering::Acquire) as usize;
        let table = self.places.as_slice().get(span(level))?;
        let mask = table.len() - 1;
        let mut place = tag as usize & mask;
        loop {
            match table[place].load(Ordering::Acquire) {
                0 => return None,
                entry if (entry >> 32) as u32 == tag => {
                    let entry = self
                        .entries
                        .get(entry as u32 - 1)
                        .expect("an index entry before its directory");
                    let dir = self.view(entry);
                    if dir.bytes == key {
                        return Some(dir);
                    }
                }
                _ => {}
            }
            place = (place + 1) & mask;
        }
    }

    /// Moves the index into a table twice its size and makes it the one in use, from the
    /// entries alone. Called while adding.
    fn grow(&self, level: usize) -> usize {
        assert!(level + 1 < TABLES, "an index of more than {MOST} directories");
        let (old, new) = (self.table(level), self.table(level + 1));
        let mask = new.len() - 1;
        for entry in old {
            let entry = entry.load(Ordering::Relaxed);
            if entry == 0 {
                continue;
            }
            let mut place = (entry >> 32) as usize & mask;
            while new[place].load(Ordering::Relaxed) != 0 {
                place = (place + 1) & mask;
            }
            new[place].store(entry, Ordering::Relaxed);
        }
        self.level.store(level as u32 + 1, Ordering::Release);
        level + 1
    }

    /// A generation after every one read so far.
    fn bump(&self) -> u64 {
        self.hot.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// A change was heard, or a watch stopped vouching: the generation it is, which
    /// [`Dirs::quiet_since`] compares with.
    fn disturb(&self) -> u64 {
        let generation = self.bump();
        self.hot.disturbed.fetch_max(generation, Ordering::AcqRel);
        generation
    }

    /// Whether nothing under any watched directory can have changed since `generation` was
    /// read: no change was heard after it, no watch stopped vouching after it, and the last
    /// round of flushes is in. One answer for everything the watches vouched for when
    /// `generation` was read, for a caller that kept no list of what that was: whatever
    /// [`Dirs::unchanged_since`] or a [`Cover`] said then of a path still holds. It says
    /// nothing of a path no watch covered then, nor of a directory wanted since. Two loads.
    pub fn quiet_since(&self, generation: u64) -> bool {
        self.hot.settled.load(Ordering::Acquire)
            && self.hot.disturbed.load(Ordering::Acquire) <= generation
    }

    fn settle(&self, settled: bool) {
        self.hot.settled.store(settled, Ordering::Release);
    }

    /// A change heard at `generation` kept by `path`; `None` for one no caller can set aside.
    fn hear(&self, generation: u64, path: Option<PathBuf>) {
        let mut changes = self.changes.lock();
        if changes.heard.len() == CHANGES_KEPT
            && let Some((gone, _)) = changes.heard.pop_front()
        {
            changes.from = changes.from.max(gone);
        }
        changes.heard.push_back((generation, path)); // alloc: grows to `CHANGES_KEPT`, then is used again
    }

    /// Whether everything heard under any watched directory since `generation` was read is a
    /// change `aside` sets aside, by its path: the last round of flushes is in, every change
    /// since then is still kept ([`CHANGES_KEPT`]), each names a path, and `aside` says yes to
    /// each. What [`Dirs::quiet_since`] says with no change heard at all, said of the changes
    /// a caller knows are beside what it rests on. It says nothing of whether a directory was
    /// watched then: ask [`Dirs::watched_since`] of each directory rested on.
    ///
    /// Cold: asked only where the generations refused, under the changes' lock, a compare a
    /// change heard since.
    pub fn only_since(&self, generation: u64, mut aside: impl FnMut(&Path) -> bool) -> bool {
        if !self.hot.settled.load(Ordering::Acquire) {
            return false;
        }
        let changes = self.changes.lock();
        changes.from <= generation
            && changes.heard.iter().filter(|(heard, _)| *heard > generation).all(|(_, path)| {
                path.as_deref().is_some_and(&mut aside)
            })
    }

    /// Whether a directory at or above `path` has been watched without a break since before
    /// `generation` was read, whatever was heard under it since, and the last round of
    /// flushes is in: [`Dirs::unchanged_since`] less its second half, for a caller that asks
    /// [`Dirs::only_since`] of what was heard. `path` is canonical, as there.
    pub fn watched_since(&self, path: &Path, generation: u64) -> bool {
        if !self.hot.settled.load(Ordering::Acquire) {
            return false;
        }
        let bytes = trimmed(path);
        // As [`Dirs::unchanged_since`] finds them: a directory asked of whole at or above the
        // path, under a watch (its own, or one of a directory above it) unbroken since.
        let (mut watched, mut whole) = (false, false);
        prefixes(bytes, |end, tag| {
            if let Some(dir) = self.find(&bytes[..end], tag) {
                watched |= dir.since() <= generation;
                whole = watched && dir.entry.recursive.load(Ordering::Acquire);
            }
            !whole
        });
        whole
    }
}

impl Default for Dirs {
    fn default() -> Dirs {
        Dirs::new()
    }
}

static DIRS: LazyLock<Dirs> = LazyLock::new(Dirs::new);

/// The one table of the process.
pub fn dirs() -> &'static Dirs {
    &*DIRS
}

/// [`Dirs::generation`] of the process's table.
pub fn generation() -> u64 {
    DIRS.generation()
}

/// [`Dirs::quiet_since`] of the process's table.
pub fn quiet_since(generation: u64) -> bool {
    DIRS.quiet_since(generation)
}

/// What a look at the disk was of ([`Look`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum LookKind {
    /// A file's content, links followed: the answer is its content's hash.
    File,
    /// The path itself, a link not followed: the answer is a hash of what it is (a
    /// directory, or a file's or a link's id and mode).
    Entry,
    /// The names in a directory: the answer is the hash of its listing.
    Listing,
}

impl std::fmt::Display for LookKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LookKind::File => "the file",
            LookKind::Entry => "the entry",
            LookKind::Listing => "the names in",
        })
    }
}

/// One look at the disk for what `path`, outside every view, is now (a read, a `stat`, a
/// listing), made because no watch vouched for the answer kept: the path, which question was
/// asked of it, what the disk said (`None`: nothing is there), and the caller that looked.
/// Made by each place that checks such a path (blaze-usercode's `inputs` and `roots`, base-db's
/// `IncludedFile::read_now`, blaze-lints' `profile::load`, the daemon's pinned native layouts)
/// and kept by the build it was made for ([`Looks`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Look {
    pub path: PathBuf,
    pub kind: LookKind,
    pub answer: Option<[u8; 32]>,
    pub by: &'static std::panic::Location<'static>,
}

/// The looks one build made, owned by that build: every place that looks at the disk is
/// handed the `Looks` of the build it looks for and pushes its look there, so no look is kept
/// by the process and none is another build's. A clone is the same list. No cap: a first
/// build after a daemon start looks at every native input and traced path once.
///
/// The invariant: a look is in the list of the build it was made for and of no other, each
/// distinct (path, kind, answer) once, in an order that depends on no thread's timing.
#[derive(Clone)]
pub struct Looks(Option<std::sync::Arc<Lock<Kept>>>);

/// A build's looks by (path, kind, answer), sorted: the same set from any interleaving.
type Kept = std::collections::BTreeMap<(PathBuf, LookKind, Option<[u8; 32]>), &'static std::panic::Location<'static>>;

impl Looks {
    /// An empty list, for one build.
    pub fn new() -> Looks {
        // alloc: once a build, the list its looks are kept in
        Looks(Some(std::sync::Arc::new(Lock::new("vfs.outside.looks", Kept::new()))))
    }

    /// A handle that keeps nothing, for a caller with no build (a check at a certificate
    /// hit, a test): such a caller uses the answer of its look itself.
    pub const fn none() -> Looks {
        Looks(None)
    }

    /// Whether this keeps what it is told ([`Looks::new`]).
    pub fn keeps(&self) -> bool {
        self.0.is_some()
    }

    /// Keeps a look of `kind` at `path` that said `answer`. A second look that said the same
    /// is the same row; one that said something else is a second row ([`Looks::twice`]).
    #[track_caller]
    pub fn push(&self, path: &Path, kind: LookKind, answer: Option<[u8; 32]>) {
        let Some(kept) = &self.0 else { return };
        let by = std::panic::Location::caller();
        // alloc: once a look at the disk, its path as the key it is kept by
        kept.lock().entry((path.to_path_buf(), kind, answer)).or_insert(by);
    }

    /// Keeps each of `looks` too: a build that used a task another build ran holds that
    /// task's looks as its own.
    pub fn extend(&self, looks: &[Look]) {
        let Some(kept) = &self.0 else { return };
        let mut kept = kept.lock();
        for look in looks {
            // clone: a look copied to a second build, once per look per build
            kept.entry((look.path.clone(), look.kind, look.answer)).or_insert(look.by);
        }
    }

    /// How many looks are kept.
    pub fn len(&self) -> usize {
        self.0.as_ref().map_or(0, |kept| kept.lock().len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every look kept, sorted by path, kind and answer.
    pub fn all(&self) -> Vec<Look> {
        let Some(kept) = &self.0 else { return Vec::new() }; // alloc: none
        // alloc: once a build's end, its looks; clone: each path, once
        kept.lock()
            .iter()
            .map(|((path, kind, answer), by)| Look { path: path.clone(), kind: *kind, answer: *answer, by: *by })
            .collect()
    }

    /// Each pair of looks in `looks` (sorted, as [`Looks::all`] gives them) of one path and
    /// kind that said two things: what such a build cannot be checked again by.
    pub fn twice(looks: &[Look]) -> impl Iterator<Item = (&Look, &Look)> {
        looks.windows(2).filter(|it| it[0].path == it[1].path && it[0].kind == it[1].kind).map(|it| (&it[0], &it[1]))
    }
}

impl Default for Looks {
    fn default() -> Looks {
        Looks::new()
    }
}

impl std::fmt::Debug for Looks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.all().iter()).finish()
    }
}

/// Each directory wanted at or above `path` with its state, beside `generation` and the
/// table's: why [`unchanged_since`] says what it says of a directory asked of whole, for a
/// person. Cold.
pub fn why_changed(path: &Path, generation: u64) -> String {
    let dirs = &*DIRS;
    let bytes = trimmed(path);
    // alloc: a reason, once a refusal said
    let mut said = format!(
        "asked at generation {generation}, now {}, settled {}:",
        dirs.generation(),
        dirs.hot.settled.load(Ordering::Acquire)
    );
    let mut any = false;
    prefixes(bytes, |end, tag| {
        if let Some(dir) = dirs.find(&bytes[..end], tag) {
            any = true;
            let since = match dir.since() {
                UNCONFIRMED => "unconfirmed".to_owned(), // alloc: as above
                since => since.to_string(),
            };
            said.push_str(&format!(
                " {} (watched since {since}, changed at {}, asked of whole: {}, taken over: {});",
                dir.path().display(),
                dir.changed(),
                dir.entry.recursive.load(Ordering::Acquire),
                dir.subsumed_by().is_some()
            ));
        }
        true
    });
    if !any {
        said.push_str(" no directory at or above it is wanted");
    }
    said
}

/// Why no watch vouches for `path` now, for a person: the directory wanted at or above it and
/// its two generations beside the table's, or that none is wanted. Cold: asked of the few
/// paths a build names as looked at.
pub fn why_looked(path: &Path) -> String {
    let dirs = &*DIRS;
    let settled = dirs.hot.settled.load(Ordering::Acquire);
    let now = dirs.generation();
    // alloc: a reason, once a path named
    match dirs.cover(path) {
        None => format!("no directory at or above it is wanted watched (of {} wanted; settled {settled})", dirs.len()),
        Some(cover) => {
            let named = path.parent().and_then(|dir| dirs.names.lock().get(trimmed(dir)).copied());
            let named = match named {
                // alloc: as above
                Some(it) => format!(
                    "; its directory's names asked of since {}, last named itself at {}, an entry of it at {}",
                    it.since, it.own, it.direct
                ),
                None => "; no name asked of in its directory".to_owned(), // alloc: as above
            };
            let dir = cover.dir();
            let since = match dir.since() {
                UNCONFIRMED => "unconfirmed".to_owned(), // alloc: as above
                since => since.to_string(),
            };
            format!(
                "under the wanted {}: watched since generation {since}, last change heard at {}, now {now}, settled {settled}{named}",
                dir.path().display(),
                dir.changed()
            )
        }
    }
}

/// [`Dirs::unchanged_since`] of the process's table.
pub fn unchanged_since(path: &Path, generation: u64) -> bool {
    DIRS.unchanged_since(path, generation)
}

/// [`Dirs::only_since`] of the process's table.
pub fn only_since(generation: u64, aside: impl FnMut(&Path) -> bool) -> bool {
    DIRS.only_since(generation, aside)
}

/// [`Dirs::watched_since`] of the process's table.
pub fn watched_since(path: &Path, generation: u64) -> bool {
    DIRS.watched_since(path, generation)
}

/// [`Dirs::cover`] of the process's table.
pub fn cover(path: &Path) -> Option<Cover<'static>> {
    DIRS.cover(path)
}

/// [`Dirs::watch_name`] of the process's table.
pub fn watch_name(name: &Path) {
    DIRS.watch_name(name)
}

/// [`Dirs::name_unchanged_since`] of the process's table.
pub fn name_unchanged_since(name: &Path, generation: u64) -> bool {
    DIRS.name_unchanged_since(name, generation)
}

/// [`Dirs::want`] of the process's table.
pub fn want(dir: &Path) {
    DIRS.want(dir)
}

/// How many tags a driver uses: four per directory (its watch, its stat, its flush).
pub const TAGS: u64 = 1 << 32;
const WATCH: u64 = 0;
const STAT: u64 = 1;
const FLUSH: u64 = 2;
/// The most rounds a directory whose watch or stat failed waits before it is asked again; the
/// wait doubles from one with each failure in a row.
const MOST_BACKOFF: u64 = 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// No watch in flight: never asked (or not nameable), failed, or ended.
    Idle,
    Watching,
    /// Its unwatch is asked: it is being watched again.
    Ending,
}

/// The driver's own state of a directory.
struct Watch {
    /// As the driver names it; `None` for one its root cannot name, never watched.
    file: Option<FileName>,
    state: State,
    /// Its stat is in flight.
    stat: bool,
    /// The round its flush in flight was asked in.
    flush: Option<u64>,
    /// It owes the round under way a flush asked in it.
    due: bool,
    /// Its watch was in place when its stat was answered, and has not broken since.
    confirmed: bool,
    /// Its watch is to be made again once nothing of it is in flight.
    restart: bool,
    /// A directory above it, which this driver watches, covers it: it is not watched again.
    retired: bool,
    /// The round before which a failed watch or stat is not asked again, and the next wait.
    retry: u64,
    backoff: u64,
}

/// The sans-IO driver of the watches of a [`Dirs`]: written as [`crate::Watcher`] is, it asks
/// with tags `tags..tags + `[`TAGS`] ([`Outside::owns`]) and is told of each with
/// [`Outside::complete`].
pub struct Outside<'d> {
    dirs: &'d Dirs,
    /// The driver's root: what the directories are named under.
    root: PathBuf,
    /// A changed file's whole path, as it is looked up: one buffer, grown to the longest.
    heard: PathBuf,
    tags: u64,
    /// One per directory of the table, by its number; never removed (a retired one stays).
    watches: RegionVec<Watch>,
    round: u64,
    /// The flushes the round under way waits for.
    left: u32,
}

impl<'d> Outside<'d> {
    /// The driver of `dirs`' watches, through a driver whose files are under `root` (`/`, so
    /// that every directory is nameable). One drives a table at a time. Stops when the address
    /// space for its watches cannot be had.
    pub fn new(dirs: &'d Dirs, root: impl Into<PathBuf>, tags: u64) -> Outside<'d> {
        let watches = RegionVec::new(MOST as usize, blaze_mem::Tag::Vfs)
            .unwrap_or_else(|error| panic!("the watches of directories outside views: {error}"));
        assert!(!dirs.driven.swap(true, Ordering::AcqRel), "another Outside drives this table");
        // alloc: once a driver, the buffer a changed file's path is looked up in
        Outside { dirs, root: root.into(), heard: PathBuf::with_capacity(1024), tags, watches, round: 0, left: 0 }
    }

    /// Whether a completion is of an effect this asked for.
    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.checked_sub(self.tags).is_some_and(|it| it < TAGS)
    }

    /// Whether the round under way is in: every flush it asked for has completed.
    pub fn flushed(&self) -> bool {
        self.left == 0
    }

    /// Watches each directory wanted since the last call that no directory above it covers.
    pub fn start(&mut self, effects: &mut Vec<Effect>) {
        let (first, len) = (self.watches.len(), self.dirs.len() as usize);
        while self.watches.len() < len {
            let file = self.name(self.dirs.dir(self.watches.len() as u32).path());
            self.watches.push(Watch {
                file,
                state: State::Idle,
                stat: false,
                flush: None,
                due: false,
                confirmed: false,
                restart: false,
                retired: false,
                retry: 0,
                backoff: 1,
            });
        }
        // A directory that covers one added with it comes after it: all are in first.
        for at in first..len {
            if !self.retire(at, effects) && self.watches[at].file.is_some() {
                self.watch(at, effects);
            }
        }
    }

    /// Retires the directory `at` if one above it that this driver can watch covers it, ending
    /// its watch once nothing of it is in flight. Whether it is retired.
    fn retire(&mut self, at: usize, effects: &mut Vec<Effect>) -> bool {
        if self.watches[at].retired {
            return true;
        }
        let dirs = self.dirs;
        let dir = dirs.dir(at as u32);
        let Some(by) = dir.subsumed_by() else { return false };
        if self.watches.get(by as usize).is_none_or(|it| it.file.is_none()) {
            return false;
        }
        let watch = &mut self.watches[at];
        watch.retired = true;
        watch.restart = false;
        unconfirm(dirs, dir, watch);
        self.tend(at, effects);
        true
    }

    /// A round: watches what is wanted and asks again what may now succeed, then flushes every
    /// confirmed watch. Until [`Outside::flushed`], nothing vouches; once it is, every change
    /// made before this was called has been heard. The daemon runs one per request, before
    /// the request's checks.
    pub fn flush(&mut self, effects: &mut Vec<Effect>) {
        self.start(effects);
        self.round += 1;
        self.left = 0;
        self.dirs.settle(false);
        let round = self.round;
        for at in 0..self.watches.len() {
            self.watches[at].due = false;
            if self.retire(at, effects) {
                continue;
            }
            let watch = &mut self.watches[at];
            if watch.file.is_none() || watch.restart {
                continue;
            }
            let idle = !watch.stat && watch.flush.is_none();
            match watch.state {
                State::Idle if idle && watch.retry <= round => self.watch(at, effects),
                State::Watching if watch.confirmed => {
                    watch.due = true;
                    self.left += 1;
                    if watch.flush.is_none() {
                        watch.flush = Some(round);
                        effects.push(Effect::Flush {
                            watch: self.tag(at, WATCH),
                            tag: self.tag(at, FLUSH),
                        });
                    }
                }
                State::Watching if !watch.stat && watch.retry <= round => self.stat(at, effects),
                _ => {}
            }
        }
        if self.left == 0 {
            self.dirs.settle(true);
        }
    }

    /// Takes a completion of its own.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) {
        let offset = completion.tag.0 - self.tags;
        let (at, kind) = ((offset >> 2) as usize, offset & 3);
        let dirs = self.dirs;
        let dir = dirs.dir(at as u32);
        let round = self.round;
        let watch = &mut self.watches[at];
        match (kind, completion.done) {
            // A wake that names nothing and lost nothing is no change under this directory.
            // Linux's watch is a mark of the whole file system (`blaze_core::fanotify`), so
            // every write anywhere on it wakes every watch, and one whose events were all
            // outside its directory completes with no file. Taken as a change, each build's
            // own outputs made every watched directory "changed" (the toolchain's, glibc's in
            // the nix store, which nothing writes): no answer was ever vouched for on Linux,
            // and no build was answered again without a walk (2026-11-12, `blaze_vfs::
            // outside::why_looked`: `watched since generation 6, last change heard at 3124,
            // now 3128`).
            (WATCH, Done::Changed { files, lost: false })
                if files.is_empty() && !EMPTY_WAKE_IS_A_CHANGE.get() => {}
            (WATCH, Done::Changed { files, lost }) => {
                // What it names: the directories asked of whole above each file, and the
                // names asked of that it names. Nothing else under this watch moved
                // ([`Dirs::quiet_since`]); what was lost is said below, by the watch's end.
                // Each file that moved something is kept by its path ([`Dirs::heard`],
                // [`Dirs::only_since`]).
                let generation = dirs.bump();
                if dirs.heard(&self.root, &files, generation, &mut self.heard) || lost {
                    dirs.hot.disturbed.fetch_max(generation, Ordering::AcqRel);
                }
                if lost {
                    // The names are not all of them: no caller sets that aside.
                    dirs.hear(generation, None);
                }
                if files.is_empty() && !lost {
                    // Only with `EMPTY_WAKE_IS_A_CHANGE` planted: the wake as a change of all.
                    dir.entry.changed.fetch_max(generation, Ordering::AcqRel);
                    dirs.hot.disturbed.fetch_max(generation, Ordering::AcqRel);
                    dirs.hear(generation, None);
                }
                let watch = &mut self.watches[at];
                let replaced = lost || watch.file.as_ref().is_some_and(|it| files.contains(it));
                if watch.retired {
                    // Its end is asked already; the directory above it hears the rest.
                } else if replaced {
                    // Replaced or moved, or may have been (what was dropped may be its own
                    // end): its watch may hear nothing more of what is at its name.
                    unconfirm(dirs, dir, watch);
                    watch.restart = true;
                } else if watch.state == State::Watching
                    && !watch.confirmed
                    && !watch.stat
                    && !watch.restart
                {
                    // Not there when it was asked of, or its stat failed: there now, perhaps.
                    self.stat(at, effects);
                }
            }
            (WATCH, done @ (Done::Unwatched | Done::Failed(_))) => {
                unconfirm(dirs, dir, watch);
                watch.state = State::Idle;
                if matches!(done, Done::Failed(_)) && !watch.restart {
                    back_off(watch, round);
                }
            }
            (WATCH, done) => unreachable!("a watch completed with {done:?}"),
            (STAT, done) => {
                watch.stat = false;
                match done {
                    Done::Stat(Some(meta))
                        if meta.dir
                            && watch.state == State::Watching
                            && !watch.confirmed
                            && !watch.restart
                            && !watch.retired =>
                    {
                        dir.entry.since.store(dirs.bump(), Ordering::Release);
                        watch.confirmed = true;
                        watch.backoff = 1;
                    }
                    Done::Stat(_) => {}
                    Done::Failed(_) => back_off(watch, round),
                    done => unreachable!("a stat completed with {done:?}"),
                }
            }
            (FLUSH, done) => {
                let asked = watch.flush.take().expect("a flush in flight");
                match done {
                    Done::Flushed => {}
                    Done::Failed(_) => {
                        unconfirm(dirs, dir, watch);
                        back_off(watch, round);
                    }
                    done => unreachable!("a flush completed with {done:?}"),
                }
                if watch.due {
                    if asked == round || !watch.confirmed || watch.state != State::Watching {
                        watch.due = false;
                        self.left -= 1;
                        if self.left == 0 {
                            dirs.settle(true);
                        }
                    } else {
                        // Asked in a round before this one: it covers less than this one needs.
                        watch.flush = Some(round);
                        effects.push(Effect::Flush {
                            watch: self.tag(at, WATCH),
                            tag: self.tag(at, FLUSH),
                        });
                    }
                }
            }
            (_, done) => unreachable!("an effect of a watch's completed with {done:?}"),
        }
        self.tend(at, effects);
    }

    /// Moves a watch to be made again on, or a retired one to its end, once nothing of it is in
    /// flight.
    fn tend(&mut self, at: usize, effects: &mut Vec<Effect>) {
        let watch = &mut self.watches[at];
        if !(watch.restart || watch.retired) || watch.stat || watch.flush.is_some() {
            return;
        }
        if watch.retired {
            if watch.state == State::Watching {
                watch.state = State::Ending;
                effects.push(Effect::Unwatch { tag: self.tag(at, WATCH) });
            }
            return;
        }
        match watch.state {
            State::Watching => {
                watch.state = State::Ending;
                effects.push(Effect::Unwatch { tag: self.tag(at, WATCH) });
            }
            State::Idle => {
                watch.restart = false;
                self.watch(at, effects);
            }
            State::Ending => {}
        }
    }

    /// Watches the directory `at`, and asks right after for the stat that confirms it.
    fn watch(&mut self, at: usize, effects: &mut Vec<Effect>) {
        let tag = self.tag(at, WATCH);
        let watch = &mut self.watches[at];
        // clone: the effect owns its name; once per watch made, never per round or check.
        let file = watch.file.clone().expect("a directory the driver names");
        watch.state = State::Watching;
        watch.confirmed = false;
        effects.push(Effect::Watch { dir: file, tag });
        self.stat(at, effects);
    }

    fn stat(&mut self, at: usize, effects: &mut Vec<Effect>) {
        let tag = self.tag(at, STAT);
        let watch = &mut self.watches[at];
        watch.stat = true;
        // clone: the effect owns its name; once per watch made or directory not yet there.
        let file = watch.file.clone().expect("a directory the driver names");
        effects.push(Effect::Stat { file, tag });
    }

    fn tag(&self, at: usize, kind: u64) -> Tag {
        Tag(self.tags + ((at as u64) << 2) + kind)
    }

    /// `path` as the driver names it, when its root can.
    fn name(&self, path: &Path) -> Option<FileName> {
        let rest = path.strip_prefix(&self.root).ok()?.to_str()?;
        if std::path::MAIN_SEPARATOR != '/' && rest.contains(std::path::MAIN_SEPARATOR) {
            return FileName::new(&rest.replace(std::path::MAIN_SEPARATOR, "/"));
        }
        FileName::new(rest)
    }
}

thread_local! {
    /// A planted bug, for a test's control: a name is never vouched for
    /// ([`Dirs::name_unchanged_since`]), as before names were asked of apart from their
    /// directories' whole trees a home directory's names never were. Never set outside a test.
    #[doc(hidden)]
    pub static NAMES_HEAR_NOTHING_BESIDE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

thread_local! {
    /// A planted bug, for a test's control: a cover asks only the directory it was found
    /// with, though a directory above took its watch over, as until 2026-11-12. Never set
    /// outside a test.
    #[doc(hidden)]
    pub static COVER_STAYS_ON_THE_RETIRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

thread_local! {
    /// A planted bug, for a test's control: a watch's wake that names no file and lost none
    /// is taken as a change under its directory, as until 2026-11-12. Never set outside a test.
    #[doc(hidden)]
    pub static EMPTY_WAKE_IS_A_CHANGE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Every directory stops vouching once nothing drives it.
impl Drop for Outside<'_> {
    fn drop(&mut self) {
        self.dirs.settle(false);
        for at in 0..self.watches.len() {
            self.dirs.dir(at as u32).entry.since.store(UNCONFIRMED, Ordering::Release);
        }
        self.dirs.driven.store(false, Ordering::Release);
    }
}

fn unconfirm(dirs: &Dirs, dir: Dir<'_>, watch: &mut Watch) {
    dir.entry.since.store(UNCONFIRMED, Ordering::Release);
    watch.confirmed = false;
    // What it vouched for is vouched for no more ([`Dirs::quiet_since`]), and no caller sets
    // that aside ([`Dirs::only_since`]).
    let heard = dirs.disturb();
    dirs.hear(heard, None);
}

fn back_off(watch: &mut Watch, round: u64) {
    watch.retry = round + watch.backoff;
    watch.backoff = (watch.backoff * 2).min(MOST_BACKOFF);
}

#[cfg(test)]
mod tests;
