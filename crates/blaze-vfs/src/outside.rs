//! What changed outside every view (docs/v2/plan/lanes/vfs/outside.md): the directories user
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
use blaze_hash::FixedHasher;
use blaze_mem::{AppendVec, MemError, RegionVec};

use crate::sync::{
    Mutex,
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
    adding: Mutex<()>,
}

/// The places of table `level` in `places`.
fn span(level: usize) -> std::ops::Range<usize> {
    TABLE * ((1 << level) - 1)..TABLE * ((1 << (level + 1)) - 1)
}

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
    let mut hasher = FixedHasher::default();
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
    pub fn unchanged_since(self, generation: u64) -> bool {
        self.dirs.hot.settled.load(Ordering::Acquire)
            && self.entry.since.load(Ordering::Acquire) <= generation
            && self.entry.changed.load(Ordering::Acquire) <= generation
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
        Dirs::try_new().unwrap_or_else(|error| panic!("the table of directories outside views: {error}"))
    }

    pub fn try_new() -> Result<Dirs, MemError> {
        let tag = blaze_mem::Tag::Vfs;
        Ok(Dirs {
            hot: Hot { generation: AtomicU64::new(0), settled: AtomicBool::new(false) },
            driven: AtomicBool::new(false),
            len: AtomicU32::new(0),
            level: AtomicU32::new(0),
            entries: AppendVec::new(MOST as usize, tag)?,
            paths: AppendVec::new(PATH_BYTES, tag)?,
            places: AppendVec::new(PLACES, tag)?,
            adding: Mutex::new(()),
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
        let mut vouched = false;
        prefixes(bytes, |end, tag| {
            vouched = self
                .find(&bytes[..end], tag)
                .is_some_and(|dir| dir.since() <= generation && dir.changed() <= generation);
            !vouched
        });
        vouched
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
        if self.covered(key) {
            return;
        }
        let _adding = self.adding.lock();
        if self.covered(key) {
            return;
        }
        let tag = tag(key);
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
        // Published before the index names it, and the index before `len` counts it.
        let at = self.entries.append(Entry {
            since: AtomicU64::new(UNCONFIRMED),
            changed: AtomicU64::new(0),
            subsumed_by: AtomicU32::new(0),
            start,
            len: key.len() as u32,
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
            if bytes.len() > key.len() && bytes.starts_with(key) && is_separator(bytes[key.len()])
            {
                _ = other.subsumed_by.compare_exchange(0, raw + 1, Ordering::AcqRel, Ordering::Acquire);
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
                    let entry = self.entries.get(entry as u32 - 1).expect("an index entry before its directory");
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

    fn settle(&self, settled: bool) {
        self.hot.settled.store(settled, Ordering::Release);
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

/// [`Dirs::unchanged_since`] of the process's table.
pub fn unchanged_since(path: &Path, generation: u64) -> bool {
    DIRS.unchanged_since(path, generation)
}

/// [`Dirs::cover`] of the process's table.
pub fn cover(path: &Path) -> Option<Cover<'static>> {
    DIRS.cover(path)
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
        Outside { dirs, root: root.into(), tags, watches, round: 0, left: 0 }
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
        unconfirm(dir, watch);
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
                        effects.push(Effect::Flush { watch: self.tag(at, WATCH), tag: self.tag(at, FLUSH) });
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
            (WATCH, Done::Changed { files, lost }) => {
                dir.entry.changed.fetch_max(dirs.bump(), Ordering::AcqRel);
                let replaced = lost || watch.file.as_ref().is_some_and(|it| files.contains(it));
                if watch.retired {
                    // Its end is asked already; the directory above it hears the rest.
                } else if replaced {
                    // Replaced or moved, or may have been (what was dropped may be its own
                    // end): its watch may hear nothing more of what is at its name.
                    unconfirm(dir, watch);
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
                unconfirm(dir, watch);
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
                        unconfirm(dir, watch);
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
                        effects.push(Effect::Flush { watch: self.tag(at, WATCH), tag: self.tag(at, FLUSH) });
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

fn unconfirm(dir: Dir<'_>, watch: &mut Watch) {
    dir.entry.since.store(UNCONFIRMED, Ordering::Release);
    watch.confirmed = false;
}

fn back_off(watch: &mut Watch, round: u64) {
    watch.retry = round + watch.backoff;
    watch.backoff = (watch.backoff * 2).min(MOST_BACKOFF);
}

#[cfg(test)]
mod tests;
