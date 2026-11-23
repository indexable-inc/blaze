//! Watching a tree with fanotify: one mark of each file system the watched directory spans
//! (`FAN_MARK_FILESYSTEM`), so nothing is added as directories appear, and events that name
//! the directory a change was in by a file handle and the file by its name
//! (`FAN_REPORT_DFID_NAME`, Linux 5.9). A handle is opened again (`open_by_handle_at`) to
//! learn where its directory is now; a directory gone since is named where it was when the
//! watch began if it is the watched one or above it, and dropped otherwise (the invariant
//! below).
//!
//! Needs `CAP_SYS_ADMIN` (the mark) and `CAP_DAC_READ_SEARCH` (opening handles):
//! [`Fanotify::new`] fails with a bare `EPERM` where the process lacks either, and
//! [`crate::watch::Tree`] watches with inotify then.
//!
//! What a watch keeps (where each directory is, by its handle) is in [`Dirs`] tables whose keys
//! and names are spans of a [`Strings`] column, reused by the next watch once this one ends
//! ([`Room`]): a watch, its events and its end allocate their completions' payload alone.
//!
//! Starting a watch walks nothing. It marks the file system of the watched directory, reads
//! `/proc/self/mountinfo` once to mark each other file system mounted at or under it, and
//! learns the handles of the watched directory and of each directory above it up to the root
//! (`Fanotify::start`). Every other directory's place is learned when an event first names it
//! (`Fanotify::place`): its handle opened then, and kept in a bounded cache after.
//!
//! The invariant (what an event in a directory gone and never learned may be dropped for): a
//! path whose state the watcher believes differs from the disk's has a shortest prefix whose
//! directory entry was unlinked (removed, or moved away) from a directory still at its place,
//! and that unlink is an event (`DELETE` or `MOVED_FROM`, with `ONDIR` for a directory) in that
//! live directory, whose handle opens. So the change is told by name at the top of what went,
//! and what was under it is gone with it: the watcher takes a directory named gone as
//! everything under it gone (blaze-vfs's `Batch::went`). What is dropped is only events inside
//! directories that no longer exist, which name nothing that is still there. Tested by
//! `an_rm_rf_of_a_subtree_never_seen_is_heard`, whose control drops the top deletion.
//!
//! The floor, and what this costs against it:
//! - Starting a watch: O(mount points on the machine) to read and parse mountinfo (bytes, in a
//!   kept buffer), plus a constant per file system mounted under the watched directory and per
//!   directory above it (an open, a `statfs`, a `name_to_handle_at`, a mark). Nothing per
//!   directory under it: the floor, since a watch must at least mark each file system it spans.
//! - An event: one hash lookup once its directory's place is known; the first time, one
//!   `open_by_handle_at`, one `readlink` of `/proc/self/fd` and one `close`. The floor of a
//!   watch told directories by handle.
//! - The cost not at the floor: one fanotify group per watch, each marking whole file systems,
//!   so each write anywhere on a marked file system is read and placed once per watch on it
//!   (N watches, N times), most of it dropped as outside ([`watch_counts`] says how many). The
//!   fix is one group per file system per driver, its events sorted to watches by path: a
//!   change of the drivers' watch tables, not made yet.
//!
//! Why unsafe: std has no fanotify, `name_to_handle_at`, `open_by_handle_at` or `fstatfs`, so
//! each is one system call here. The rest has std versions that cost per event: `fs::read_link`
//! and `fs::canonicalize` return a new `PathBuf` each (an allocation per directory named),
//! where these write into the watch's kept buffers. The event records themselves are parsed in
//! safe code (`parse`, `fid`): every length the kernel wrote is checked against the bytes
//! `read` returned before a slice of it is taken, and a record that does not fit ends the batch
//! as lost, never read past.

use std::{
    ffi::{CStr, OsStr},
    hash::Hasher as _,
    io::{self, Write as _},
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        unix::ffi::OsStrExt as _,
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_mem::{RegionVec, Tag as MemTag};
use blaze_sys::{Errno, Fd, Path as SysPath, linux, posix::proc::PATH_MAX};
use std::alloc::{Allocator, Global};

use crate::{
    Done, FileName,
    utf8::push_lossy,
    watch::{Batch, Span, Strings, is_under, join, valid},
};

/// Events read by every fanotify watch of the process, since it began.
static EVENTS: AtomicU64 = AtomicU64::new(0);
/// Of those, events dropped as outside the watch's directory (beside it, or on another part
/// of a marked file system), the gone ones among them.
static OUTSIDE: AtomicU64 = AtomicU64::new(0);
/// Directories an event named that were gone and never learned, each counted once (its place
/// is cached as outside after): what the module's invariant drops.
static GONE: AtomicU64 = AtomicU64::new(0);

/// What the process's fanotify watches have read: with one group per watch, each write on a
/// marked file system is read once per watch on it, so `outside` against `events` is what that
/// costs (the module's floor).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WatchCounts {
    /// Events read, by every watch.
    pub events: u64,
    /// Events dropped as outside their watch's directory.
    pub outside: u64,
    /// Directories named by an event that were gone and never learned.
    pub gone: u64,
}

/// The process's fanotify counts so far.
pub fn watch_counts() -> WatchCounts {
    WatchCounts {
        events: EVENTS.load(Ordering::Relaxed),
        outside: OUTSIDE.load(Ordering::Relaxed),
        gone: GONE.load(Ordering::Relaxed),
    }
}

/// The most bytes of `/proc/self/mountinfo` a watch's start reads: about 16 000 mounts. Past
/// it the start fails, naming the file, rather than mark the file systems of part of it.
const MOUNTINFO: usize = 4 << 20;

/// A bug planted for a test's control, each undoing one piece of the start without a walk.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// An event that a directory was deleted is dropped: the top of an `rm -rf` is not told.
    TopDeletionDropped,
    /// The start does not read mountinfo: a file system mounted under the watched directory
    /// is never marked.
    MountinfoUnread,
    /// The start walks every directory under the watched one, as before 2026-11-14.
    WalksAtStart,
}

#[cfg(test)]
thread_local! {
    static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

#[cfg(test)]
fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

const MODIFY: u64 = 0x2;
const ATTRIB: u64 = 0x4;
const CLOSE_WRITE: u64 = 0x8;
const MOVED_FROM: u64 = 0x40;
const MOVED_TO: u64 = 0x80;
const CREATE: u64 = 0x100;
const DELETE: u64 = 0x200;
const Q_OVERFLOW: u64 = 0x4000;
const ONDIR: u64 = 0x4000_0000;
const MASK: u64 = MODIFY | ATTRIB | CLOSE_WRITE | MOVED_FROM | MOVED_TO | CREATE | DELETE | ONDIR;

const INFO_DFID_NAME: u8 = 2;
const INFO_DFID: u8 = 3;

/// `struct fanotify_event_metadata`.
const METADATA: usize = 24;
/// `MAX_HANDLE_SZ`.
const MAX_HANDLE: usize = 128;
/// A [`Key`]'s most bytes: a file system's id, a handle's length and type, and its bytes.
const KEY: usize = 16 + MAX_HANDLE;

/// The most directories a [`Dirs`] table keeps: room reserved once, committed as they come.
const DIRS: usize = 1 << 24;

/// `struct file_handle` with room for the largest handle.
#[repr(C)]
struct Handle {
    bytes: u32,
    kind: i32,
    handle: [u8; MAX_HANDLE],
}

impl Handle {
    fn bytes(&self) -> &[u8] {
        &self.handle[..(self.bytes as usize).min(MAX_HANDLE)]
    }
}

/// A file system marked, and a directory of it to open its handles against.
struct Mount {
    fsid: [u8; 8],
    fd: OwnedFd,
}

/// Where a directory an event names is, as far as the watch cares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// Outside the root, or under it beside the watched directory (a `target`): its events
    /// are dropped.
    Outside,
    /// The watched directory or one under it: each of its events counts.
    Under,
    /// A directory above the watched one (the root among them): an event counts when its name
    /// leads to the watched directory.
    Above,
}

/// Whether `error` is of opening a directory gone, or made unreadable, since an event named it.
fn gone(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().and_then(|code| u16::try_from(code).ok()).map(Errno::from_raw),
        Some(Errno::NoEnt | Errno::NotDir | Errno::Stale | Errno::Acces | Errno::Loop)
    )
}

/// How many directories' places are kept before the cache starts again.
const SEEN: usize = 1 << 16;

/// Directories by key ([`Key`]), each with a value and a name relative to the root: the key
/// and the name one after the other in a [`Strings`]. A chain of entries per hash of a key, so
/// each lookup hashes the key once and compares the bytes of the entries of its hash.
struct Dirs<V> {
    /// The first entry of each hash.
    heads: Map<u64, u32, FixedState, Global>,
    entries: RegionVec<Entry<V>>,
    /// The first entry let go, the rest through their `next`.
    free: u32,
    len: usize,
    strings: Strings,
}

struct Entry<V> {
    /// The key, then the name; empty while let go.
    span: Span,
    key: u32,
    next: u32,
    value: V,
}

const NONE: u32 = u32::MAX;

fn hash(key: &[u8]) -> u64 {
    let mut hasher = blaze_hash::FixedHasher::default();
    hasher.write(key);
    hasher.finish()
}

impl<V: Copy> Dirs<V> {
    /// A table with room for `heads` hashes reserved, so it allocates nothing until it holds
    /// more.
    fn new(heads: usize) -> io::Result<Dirs<V>> {
        Ok(Dirs {
            // alloc: setup, once per room, reused by every watch after.
            heads: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: Some(heads) }),
            entries: RegionVec::new(DIRS, MemTag::Io).map_err(io::Error::other)?,
            free: NONE,
            len: 0,
            strings: Strings::new()?,
        })
    }

    fn len(&self) -> usize {
        self.len
    }

    fn key_of(&self, entry: &Entry<V>) -> &[u8] {
        &self.strings.bytes(entry.span)[..entry.key as usize]
    }

    fn name_of(&self, entry: &Entry<V>) -> &str {
        std::str::from_utf8(&self.strings.bytes(entry.span)[entry.key as usize..])
            .expect("pushed as a str")
    }

    fn find(&self, key: &[u8]) -> Option<u32> {
        let mut at = *self.heads.get(&hash(key))?;
        while at != NONE {
            let entry = &self.entries[at as usize];
            if self.key_of(entry) == key {
                return Some(at);
            }
            at = entry.next;
        }
        None
    }

    fn get(&self, key: &[u8]) -> Option<(V, &str)> {
        let entry = &self.entries[self.find(key)? as usize];
        Some((entry.value, self.name_of(entry)))
    }

    /// `key` kept with `value` and `name`, in place of what it had.
    fn insert(&mut self, key: &[u8], value: V, name: &str) {
        self.remove(key);
        if self.strings.wasteful() {
            self.squeeze();
        }
        let span = self.strings.push(key);
        let span = Span::joined(span, self.strings.push(name.as_bytes()));
        let entry = Entry { span, key: key.len() as u32, next: NONE, value };
        let at = match self.free {
            NONE => self.entries.push_id(entry),
            at => {
                self.free = self.entries[at as usize].next;
                self.entries[at as usize] = entry;
                at
            }
        };
        let head = self.heads.entry(hash(key)).or_insert(NONE);
        self.entries[at as usize].next = *head;
        *head = at;
        self.len += 1;
    }

    fn remove(&mut self, key: &[u8]) {
        let hash = hash(key);
        let Some(&first) = self.heads.get(&hash) else { return };
        let (mut before, mut at) = (NONE, first);
        while at != NONE {
            let entry = &self.entries[at as usize];
            let next = entry.next;
            if self.key_of(entry) != key {
                (before, at) = (at, next);
                continue;
            }
            match (before, next) {
                (NONE, NONE) => _ = self.heads.remove(&hash),
                (NONE, next) => _ = self.heads.insert(hash, next),
                (before, next) => self.entries[before as usize].next = next,
            }
            let span = std::mem::take(&mut self.entries[at as usize].span);
            self.strings.free(span);
            self.entries[at as usize].next = self.free;
            self.free = at;
            self.len -= 1;
            return;
        }
    }

    fn clear(&mut self) {
        self.heads.clear();
        self.entries.clear();
        self.free = NONE;
        self.len = 0;
        self.strings.clear();
    }

    /// The keys and names moved down over those let go.
    fn squeeze(&mut self) {
        let Dirs { entries, strings, .. } = self;
        for entry in entries.iter() {
            strings.keep(entry.span);
        }
        strings.squeeze();
        for entry in entries.iter_mut() {
            entry.span = strings.moved(entry.span);
        }
        strings.squeezed();
    }
}

pub(crate) struct Fanotify {
    fd: OwnedFd,
    room: Room,
}

/// A fanotify watch's tables and buffers, kept from one watch to the next.
pub(crate) struct Room {
    /// The driver's root with its links resolved, as the kernel names an opened handle.
    root: PathBuf,
    /// The watched directory, relative to the root.
    dir: String,
    mounts: Vec<Mount>,
    /// Where the watched directory and each directory above it up to the root were when the
    /// watch began, relative to the root: so an event in one gone since is still placed (the
    /// end of the watched directory, told where a directory above it went with it). Learned at
    /// the start, a handful; no directory under the watched one is (the module's invariant).
    known: Dirs<()>,
    /// The place of each directory an event named, so a batch of events opens each
    /// directory's handle once and none again until a directory moves: the mark covers the
    /// whole file system, and without it every write into a `target` or `/tmp` cost an
    /// `open_by_handle_at`, a `readlink` and a `close`. Dropped when any directory under the
    /// root moves and when events were lost, either of which can make a place untrue.
    seen: Dirs<Kind>,
    rel: String,
    /// Where an opened handle's file is, as `/proc/self/fd` says.
    link: Vec<u8>,
    batch: Batch,
    /// `/proc/self/mountinfo` as the start read it, at most [`MOUNTINFO`] bytes.
    mountinfo: Vec<u8>,
    /// The watched directory with its links resolved, as mountinfo names mount points.
    top: PathBuf,
    /// A path being opened, NUL-terminated; and a mount point, its escapes decoded.
    path: Vec<u8>,
    point: Vec<u8>,
    /// The system calls the last start made (a libc call that makes several, `realpath`'s,
    /// counted once): what `a_start_makes_a_constant_number_of_system_calls` bounds.
    calls: u32,
}

impl Room {
    fn new() -> io::Result<Room> {
        Ok(Room {
            root: PathBuf::new(),
            dir: String::new(),
            mounts: Vec::new_in(Global),
            known: Dirs::new(1 << 6)?,
            seen: Dirs::new(SEEN)?,
            rel: String::new(),
            link: Vec::new_in(Global),
            batch: Batch::default(),
            mountinfo: Vec::new_in(Global),
            top: PathBuf::new(),
            path: Vec::new_in(Global),
            point: Vec::new_in(Global),
            calls: 0,
        })
    }
}

impl Fanotify {
    /// Watches `dir` under `root` and everything under it, into `room`'s tables.
    pub(crate) fn new(
        root: &Path,
        dir: &FileName,
        nonblocking: bool,
        room: Option<Room>,
    ) -> io::Result<Fanotify> {
        let flags = linux::FanInit::CLASS_NOTIF
            | linux::FanInit::CLOSE_ON_EXEC
            | linux::FanInit::REPORT_DIR_FID
            | linux::FanInit::REPORT_NAME
            | if nonblocking { linux::FanInit::NONBLOCK } else { linux::FanInit::NONE };
        let event_flags = linux::OpenFlags::READ | linux::OpenFlags::CLOSE_ON_EXEC;
        let fd = linux::fanotify_init(flags, event_flags).map_err(|errno| {
            let error = io::Error::from_raw_os_error(i32::from(errno.raw()));
            match errno {
                Errno::Perm => error,
                Errno::Inval => io::Error::new(
                    io::ErrorKind::Unsupported,
                    // alloc: the error of a watch that failed.
                    format!(
                        "fanotify_init: {error}: Linux 5.9 or later reports a change's directory \
                         and name (FAN_REPORT_DFID_NAME), and this is Linux {}",
                        crate::cgroup::kernel()
                    ),
                ),
                _ => named(&error, "fanotify_init"),
            }
        })?;
        // SAFETY: into_raw transfers the new open descriptor's sole ownership.
        let fd = unsafe { OwnedFd::from_raw_fd(fd.into_raw()) };
        let mut room = match room {
            Some(room) => room,
            None => Room::new()?,
        };
        room.calls = 1;
        canonical(root, &mut room.root)
            .map_err(|error| named(&error, &format!("resolving {}", root.display())))?;
        room.calls += 1;
        room.dir.clear();
        room.dir.push_str(dir.as_str());
        let mut watch = Fanotify { fd, room };
        watch.start()?;
        Ok(watch)
    }

    /// Marks the file system of the watched directory and of each one mounted at or under it,
    /// and learns where the watched directory and each directory above it are: a constant
    /// number of system calls per file system and per directory above, and one read of
    /// mountinfo, whatever is under the watched directory (the module's floor).
    fn start(&mut self) -> io::Result<()> {
        let fanotify = self.fd.as_raw_fd();
        let Room { root, dir, mounts, known, mountinfo, top, path, point, calls, .. } =
            &mut self.room;
        // The watched directory, its file system marked first: a failure of its is the watch's.
        path.clear();
        join(path, root.as_os_str().as_bytes(), dir.as_bytes());
        canonical(Path::new(OsStr::from_bytes(path)), top).map_err(|error| {
            named(&error, &format!("resolving {}", String::from_utf8_lossy(path)))
        })?;
        *calls += 1;
        let c = c_path(path, top.as_os_str().as_bytes())?;
        let opened = crate::raw::open_dir(c)
            .map_err(|error| keep_eperm(error, &format!("opening {}", c.to_string_lossy())))?;
        *calls += 1;
        let (fsid, handle) = mark(fanotify, mounts, c, opened, calls)?;
        known.insert(Key::of(&fsid, handle.kind, handle.bytes()).bytes(), (), dir);
        // Each directory above it, to the root, by path: no descriptor, two calls each.
        let mut above = dir.as_str();
        while !above.is_empty() {
            above = above.rfind('/').map_or("", |at| &above[..at]);
            point.clear();
            join(point, root.as_os_str().as_bytes(), above.as_bytes());
            let c = c_path(path, point)?;
            let fsid = fsid_at(c).map_err(|error| {
                named(&error, &format!("statfs of {}", c.to_string_lossy()))
            })?;
            let handle = handle_at(c).map_err(|error| {
                keep_eperm(error, &format!("name_to_handle_at of {}", c.to_string_lossy()))
            })?;
            *calls += 2;
            // One on a file system not marked sends nothing to place.
            if mounts.iter().any(|it| it.fsid == fsid) {
                known.insert(Key::of(&fsid, handle.kind, handle.bytes()).bytes(), (), above);
            }
        }
        #[cfg(test)]
        if planted(Plant::WalksAtStart) {
            let mut walker = crate::watch::Walker::default();
            walker.walk(
                root.as_os_str().as_bytes(),
                dir,
                |_, _, _| {
                    // An open, a listing and a close at the least, counted as one.
                    *calls += 1;
                    Ok(crate::watch::Visit::List(None))
                },
                |_| {},
            )?;
        }
        #[cfg(test)]
        if planted(Plant::MountinfoUnread) {
            return Ok(());
        }
        // Each other file system mounted at or under it: an unmarked one sends no event, so
        // no event could ever lead to it.
        *calls += read_mountinfo(mountinfo)?;
        let top = top.as_os_str().as_bytes();
        mount_points(mountinfo, point, |point| {
            if point == top || !is_under_bytes(point, top) {
                return Ok(());
            }
            let c = c_path(path, point)?;
            let opened = match crate::raw::open_dir(c) {
                Ok(opened) => opened,
                // Unmounted, or not ours to open, since mountinfo was read: what is not there
                // (or cannot be read by anyone this process could watch for) sends nothing a
                // watch of this tree could name. Any other failure is the watch's.
                Err(error) if gone(&error) => return Ok(()),
                Err(error) => {
                    let what = format!(
                        "opening {}, mounted under the watched directory",
                        c.to_string_lossy()
                    );
                    return Err(keep_eperm(error, &what));
                }
            };
            *calls += 1;
            mark(fanotify, mounts, c, opened, calls).map(drop)
        })
    }

    /// Stops watching, its descriptor closed; its tables are the next watch's.
    pub(crate) fn end(self) -> Room {
        let Fanotify { fd, mut room } = self;
        drop(fd);
        room.mounts.clear();
        room.known.clear();
        room.seen.clear();
        room
    }

    pub(crate) fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// What the events in `bytes` say. An event in a directory whose place is known costs one
    /// hash lookup, and one beside the watched directory nothing more. A directory made or
    /// moved in is told by its own name, never walked: a directory moved in has no events of
    /// what was in it already, and its watcher lists a directory it is told appeared
    /// (blaze-vfs's `Watcher`, an [`crate::Effect::Entries`] off the loop).
    pub(crate) fn take(&mut self, bytes: &[u8]) -> Done {
        let mut lost = parse(bytes, |_, _| {});
        if lost {
            self.room.seen.clear();
        }
        let (mut events, mut outside) = (0u64, 0u64);
        let mut rel = std::mem::take(&mut self.room.rel);
        parse(bytes, |mask, record| {
            events += 1;
            let kind = match self.place(&record, &mut rel) {
                Ok(Kind::Outside) => {
                    outside += 1;
                    return;
                }
                Ok(kind) => kind,
                Err(_) => {
                    lost = true;
                    return;
                }
            };
            let moved = mask & ONDIR != 0 && mask & (MOVED_FROM | MOVED_TO) != 0;
            if moved {
                self.room.seen.clear();
            }
            if !matches!(record.name, b"" | b".") {
                if !rel.is_empty() {
                    rel.push('/');
                }
                push_lossy(&mut rel, record.name);
            }
            #[cfg(test)]
            if planted(Plant::TopDeletionDropped) && mask & (DELETE | ONDIR) == DELETE | ONDIR {
                return;
            }
            // A directory above the watched one removed, moved away or replaced: the watched
            // directory went with it, or is another one now. Told as a change of the watched
            // directory itself, which says so (`Effect::Watch`).
            if kind == Kind::Above
                && mask & ONDIR != 0
                && mask & (DELETE | MOVED_FROM | MOVED_TO) != 0
                && !rel.is_empty()
                && is_under(&self.room.dir, &rel)
                && rel != self.room.dir
            {
                self.room.batch.push(&self.room.dir);
                return;
            }
            if !valid(&rel) || (kind == Kind::Above && !is_under(&rel, &self.room.dir)) {
                return;
            }
            self.room.batch.push(&rel);
        });
        self.room.rel = rel;
        EVENTS.fetch_add(events, Ordering::Relaxed);
        OUTSIDE.fetch_add(outside, Ordering::Relaxed);
        self.room.batch.changed(lost)
    }

    /// The kind of place of the directory `record` names, with its name relative to the root
    /// written to `rel` (empty for [`Kind::Outside`]); its handle is opened only the first time.
    fn place(&mut self, record: &Record<'_>, rel: &mut String) -> io::Result<Kind> {
        rel.clear();
        if let Some((kind, name)) = self.room.seen.get(record.key) {
            rel.push_str(name);
            return Ok(kind);
        }
        let kind = self.resolve(record.fsid, record.key, record.kind, record.handle, rel)?;
        if kind == Kind::Outside {
            rel.clear();
        }
        if self.room.seen.len() >= SEEN {
            self.room.seen.clear();
        }
        self.room.seen.insert(record.key, kind, rel);
        Ok(kind)
    }

    /// Where the directory of `handle` (whose [`Key`] is `key`) is now, its name relative to the
    /// root written to `rel`: outside the root, or a directory gone that was not learned at the
    /// start, is [`Kind::Outside`]. An error where it cannot be said.
    fn resolve(
        &mut self,
        fsid: &[u8],
        key: &[u8],
        kind: i32,
        handle: &[u8],
        rel: &mut String,
    ) -> io::Result<Kind> {
        let Room { root, dir, mounts, known, link, .. } = &mut self.room;
        let Some(mount) = mounts.iter().find(|it| it.fsid[..] == *fsid) else {
            return Ok(Kind::Outside);
        };
        let found =
            open_by_handle(mount.fd.as_raw_fd(), kind, handle).and_then(|fd| path_of(&fd, link));
        match found {
            Ok(len) => {
                let Ok(under) = Path::new(OsStr::from_bytes(&link[..len])).strip_prefix(&*root)
                else {
                    return Ok(Kind::Outside);
                };
                push_lossy(rel, under.as_os_str().as_bytes());
                Ok(classify(dir, rel))
            }
            // The invariant (the module's): an event in a directory gone since it was made
            // names nothing still there. Whatever of the watched tree went with that directory
            // went at the unlink of the topmost directory removed or moved away, and that is
            // an event in its parent, which is still at its place and so opens; the watched
            // directory and those above it are learned at the start, so the end of the watched
            // directory is placed even where its parent went too. So a directory gone that was
            // never learned is dropped as outside, and its place is cached so (`place`).
            Err(error) if matches!(
                error.raw_os_error().and_then(|code| u16::try_from(code).ok()).map(Errno::from_raw),
                Some(Errno::Stale | Errno::NoEnt)
            ) => {
                let Some(((), name)) = known.get(key) else {
                    GONE.fetch_add(1, Ordering::Relaxed);
                    return Ok(Kind::Outside);
                };
                rel.push_str(name);
                Ok(classify(dir, rel))
            }
            Err(error) => Err(error),
        }
    }
}

/// The directory open as `opened` (at `path`): its file system marked unless it is already,
/// with `opened` kept as the mount's descriptor then (dropped otherwise); its file system's id
/// and handle. Each system call made is counted in `calls`.
fn mark<A: Allocator>(
    fanotify: RawFd,
    mounts: &mut Vec<Mount, A>,
    path: &CStr,
    opened: OwnedFd,
    calls: &mut u32,
) -> io::Result<([u8; 8], Handle)> {
    let raw = opened.as_raw_fd();
    let fsid = fsid(raw)
        .map_err(|error| named(&error, &format!("statfs of {}", path.to_string_lossy())))?;
    let handle = handle_of(raw).map_err(|error| no_handles(error, path))?;
    *calls += 2;
    if mounts.iter().any(|it| it.fsid == fsid) {
        return Ok((fsid, handle));
    }
    let marked = linux::fanotify_mark(
        Fd::new(fanotify),
        linux::FanMark::ADD | linux::FanMark::FILESYSTEM,
        linux::FanMask::from_bits(MASK),
        linux::CWD,
        SysPath::new(path),
    );
    *calls += 1;
    if let Err(errno) = marked {
        let error = io::Error::from_raw_os_error(i32::from(errno.raw()));
        return Err(match errno {
            Errno::XDev | Errno::OpNotSupp | Errno::NoDev => io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "fanotify cannot watch the file system of {} by file handles: {error}",
                    path.to_string_lossy()
                ),
            ),
            _ => keep_eperm(error, &format!("fanotify_mark of {}", path.to_string_lossy())),
        });
    }
    // Opening a handle needs CAP_DAC_READ_SEARCH, which marking did not: asked here, once per
    // file system, so a process without it fails the start (and watches with inotify) rather
    // than fail each event.
    open_by_handle(raw, handle.kind, handle.bytes()).map_err(|error| {
        keep_eperm(error, &format!("open_by_handle_at in {}", path.to_string_lossy()))
    })?;
    *calls += 2;
    mounts.push(Mount { fsid, fd: opened });
    Ok((fsid, handle))
}

/// A failure of `name_to_handle_at` of `path`, named: a file system that gives no handles
/// says that fanotify cannot watch it; a bare `EPERM` is kept, which says a privilege is
/// missing.
fn no_handles(error: io::Error, path: &CStr) -> io::Error {
    match error.raw_os_error() {
        Some(code) if code == i32::from(Errno::OpNotSupp.raw()) => io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "the file system of {} gives no file handles, which watching it with fanotify \
                 needs: {error}",
                path.to_string_lossy()
            ),
        ),
        _ => keep_eperm(error, &format!("name_to_handle_at of {}", path.to_string_lossy())),
    }
}

/// `bytes` NUL-terminated in `into`, as a C string.
fn c_path<'a, A: Allocator>(into: &'a mut Vec<u8, A>, bytes: &[u8]) -> io::Result<&'a CStr> {
    into.clear();
    into.extend_from_slice(bytes);
    into.push(0);
    CStr::from_bytes_with_nul(&into[..]).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{}: a path with a NUL in it", String::from_utf8_lossy(bytes)),
        )
    })
}

/// Reads `/proc/self/mountinfo` whole into `into`, at most [`MOUNTINFO`] bytes: how many
/// system calls that took.
fn read_mountinfo<A: Allocator>(into: &mut Vec<u8, A>) -> io::Result<u32> {
    use std::io::Read as _;
    const FILE: &str = "/proc/self/mountinfo";
    let mut file = std::fs::File::open(FILE).map_err(|error| named(&error, FILE))?;
    let mut calls = 1;
    let mut len = 0;
    into.clear();
    loop {
        if len == into.len() {
            if into.len() >= MOUNTINFO {
                return Err(io::Error::other(format!(
                    "{FILE} is over {MOUNTINFO} bytes, the most a watch's start reads to find \
                     the file systems mounted under the watched directory"
                )));
            }
            into.resize((2 * into.len()).clamp(16 << 10, MOUNTINFO), 0);
        }
        calls += 1;
        match file.read(&mut into[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            // A signal before any byte was read: the read is asked again, as `read(2)` says.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(named(&error, FILE)),
        }
    }
    into.truncate(len);
    // The close.
    Ok(calls + 1)
}

/// Hands `each` the mount point of every line of `mountinfo` (`/proc/self/mountinfo`'s
/// bytes, proc(5)): the fifth field, its octal escapes (`\040` a space, `\011` a tab, `\012` a
/// newline, `\134` a backslash) decoded into `point`. A line with no fifth field is an error
/// naming it.
fn mount_points<A: Allocator>(
    mountinfo: &[u8],
    point: &mut Vec<u8, A>,
    mut each: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    for line in mountinfo.split(|&it| it == b'\n').filter(|it| !it.is_empty()) {
        let Some(field) = line.split(|&it| it == b' ').nth(4) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "a line of /proc/self/mountinfo with no mount point: {}",
                    String::from_utf8_lossy(line)
                ),
            ));
        };
        point.clear();
        let mut at = 0;
        while at < field.len() {
            let octal = |it: u8| (b'0'..=b'7').contains(&it).then(|| it - b'0');
            let escape = match field.get(at..at + 4) {
                Some(&[b'\\', a, b, c]) => match (octal(a), octal(b), octal(c)) {
                    (Some(a), Some(b), Some(c)) if a <= 3 => Some(a << 6 | b << 3 | c),
                    _ => None,
                },
                _ => None,
            };
            match escape {
                Some(byte) => {
                    point.push(byte);
                    at += 4;
                }
                None => {
                    point.push(field[at]);
                    at += 1;
                }
            }
        }
        each(&point[..])?;
    }
    Ok(())
}

/// Whether `path` is under `dir` (both absolute, as bytes): `dir` then a `/` and more.
fn is_under_bytes(path: &[u8], dir: &[u8]) -> bool {
    match dir {
        b"/" => path.len() > 1 && path[0] == b'/',
        _ => path.strip_prefix(dir).is_some_and(|rest| rest.first() == Some(&b'/')),
    }
}

/// The kind of place of the directory `rel` (relative to the root, "" for the root) for a
/// watch of `dir`.
fn classify(dir: &str, rel: &str) -> Kind {
    if valid(rel) && is_under(rel, dir) {
        return Kind::Under;
    }
    match rel.is_empty() || dir.strip_prefix(rel).is_some_and(|rest| rest.starts_with('/')) {
        true => Kind::Above,
        false => Kind::Outside,
    }
}

/// A directory's handle and name in an event.
#[derive(Debug, PartialEq, Eq)]
struct Record<'a> {
    fsid: &'a [u8],
    /// The file system's id and the `struct file_handle`, as the event carries them: the
    /// directory's [`Key`], read in place.
    key: &'a [u8],
    kind: i32,
    handle: &'a [u8],
    name: &'a [u8],
}

/// Hands each event in `bytes`, as `read` gave them, to `each` with its mask and its
/// directory's record; whether changes were lost: the queue overflowed, or an event could not
/// be read.
fn parse<'a>(bytes: &'a [u8], mut each: impl FnMut(u64, Record<'a>)) -> bool {
    let mut lost = false;
    let mut at = 0;
    while at + METADATA <= bytes.len() {
        let len = u32_at(bytes, at) as usize;
        let metadata_len = u16_at(bytes, at + 6) as usize;
        let mask = u64_at(bytes, at + 8);
        if len < METADATA || metadata_len < METADATA || metadata_len > len || at + len > bytes.len()
        {
            return true;
        }
        let event = &bytes[at..at + len];
        at += len;
        if mask & Q_OVERFLOW != 0 {
            lost = true;
            continue;
        }
        let mut record = None;
        let mut info = metadata_len;
        while info + 4 <= event.len() {
            let kind = event[info];
            let info_len = u16_at(event, info + 2) as usize;
            if info_len < 4 || info + info_len > event.len() {
                break;
            }
            let body = &event[info + 4..info + info_len];
            info += info_len;
            if kind == INFO_DFID_NAME || kind == INFO_DFID {
                record = fid(body, kind == INFO_DFID_NAME);
            }
        }
        match record {
            Some(record) => each(mask, record),
            None => lost = true,
        }
    }
    lost || at != bytes.len()
}

/// `struct fanotify_event_info_fid` after its header: the file system's id, a
/// `struct file_handle`, and of a named record the name, NUL-terminated.
fn fid(body: &[u8], named: bool) -> Option<Record<'_>> {
    let fsid = body.get(..8)?;
    let size = u32_at(body.get(..16)?, 8) as usize;
    let kind = i32::from_ne_bytes(body[12..16].try_into().ok()?);
    let handle = body.get(16..16 + size)?;
    let key = &body[..16 + size];
    let name = match named {
        true => {
            let rest = &body[16 + size..];
            &rest[..rest.iter().position(|&it| it == 0)?]
        }
        false => &[],
    };
    Some(Record { fsid, key, kind, handle, name })
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes(bytes[at..at + 2].try_into().expect("two bytes"))
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

/// A directory's key: its file system's id, then its `struct file_handle` (length, type,
/// bytes), laid out as an event's record carries them; made on the stack.
struct Key {
    bytes: [u8; KEY],
    len: usize,
}

impl Key {
    #[expect(
        blaze::narrowed_len,
        reason = "`handle` is at most MAX_HANDLE (128) bytes: `Handle::bytes` slices to it and the \
                  copy into the fixed KEY-byte array below would refuse more"
    )]
    fn of(fsid: &[u8], kind: i32, handle: &[u8]) -> Key {
        let mut key = Key { bytes: [0; KEY], len: 0 };
        let size = u32::try_from(handle.len()).expect("a handle of at most MAX_HANDLE bytes");
        for part in [fsid, &size.to_ne_bytes()[..], &kind.to_ne_bytes()[..], handle] {
            key.bytes[key.len..key.len + part.len()].copy_from_slice(part);
            key.len += part.len();
        }
        key
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// The id of the file system of `fd`, as fanotify's events carry it.
fn fsid(fd: RawFd) -> io::Result<[u8; 8]> {
    let stat = linux::fstatfs(Fd::new(fd))
        .map_err(|error| io::Error::from_raw_os_error(i32::from(error.raw())))?;
    Ok(fsid_of(&stat))
}

/// The id of the file system that holds `path`, as fanotify's events carry it.
fn fsid_at(path: &CStr) -> io::Result<[u8; 8]> {
    linux::task::statfs(SysPath::new(path))
        .map(|stat| fsid_of(&stat))
        .map_err(|error| io::Error::from_raw_os_error(i32::from(error.raw())))
}

fn fsid_of(stat: &linux::Statfs) -> [u8; 8] {
    let words = stat.fsid;
    let mut fsid = [0; 8];
    fsid[..4].copy_from_slice(&words[0].to_ne_bytes());
    fsid[4..].copy_from_slice(&words[1].to_ne_bytes());
    fsid
}

/// The handle of the directory open as `fd`, on the stack.
fn handle_of(fd: RawFd) -> io::Result<Handle> {
    name_to_handle(fd, c"", linux::AtFlags::EMPTY_PATH.bits() as core::ffi::c_int)
}

/// The handle of the directory at `path`, links followed, on the stack: no descriptor opened,
/// so a directory this process may only pass through is had too.
fn handle_at(path: &CStr) -> io::Result<Handle> {
    name_to_handle(linux::CWD.raw(), path, linux::AtFlags::FOLLOW.bits() as core::ffi::c_int)
}

fn name_to_handle(fd: RawFd, path: &CStr, flags: core::ffi::c_int) -> io::Result<Handle> {
    let mut handle = Handle { bytes: MAX_HANDLE as u32, kind: 0, handle: [0; MAX_HANDLE] };
    let mut mount_id: core::ffi::c_int = 0;
    // SAFETY: a descriptor (open, or `AT_FDCWD`) with a NUL-terminated path (empty with
    // `AT_EMPTY_PATH`), a handle with room for `bytes` bytes, and an int for the mount's id.
    let got = unsafe {
        libc::syscall(
            linux::nr::NAME_TO_HANDLE_AT as core::ffi::c_long,
            fd,
            path.as_ptr(),
            &raw mut handle,
            &raw mut mount_id,
            flags,
        )
    };
    if got != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

/// The file of a handle, opened as a path only, against the file system of `mount`.
fn open_by_handle(mount: RawFd, kind: i32, bytes: &[u8]) -> io::Result<OwnedFd> {
    if bytes.len() > MAX_HANDLE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "a file handle longer than any"));
    }
    let mut handle = Handle { bytes: bytes.len() as u32, kind, handle: [0; MAX_HANDLE] };
    handle.handle[..bytes.len()].copy_from_slice(bytes);
    // SAFETY: an open descriptor and a handle of `bytes` bytes; a descriptor of its own or -1.
    let fd = unsafe {
        libc::syscall(
            linux::nr::OPEN_BY_HANDLE_AT as core::ffi::c_long,
            mount,
            &raw mut handle,
            linux::OpenFlags::PATH.with(linux::OpenFlags::CLOSE_ON_EXEC).bits() as core::ffi::c_int,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call returned a descriptor of its own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Where the file open as `fd` is now, read into `link` (grown once to `PATH_MAX`): how many of
/// its bytes; `ESTALE` for one removed since.
fn path_of(fd: &OwnedFd, link: &mut Vec<u8>) -> io::Result<usize> {
    let mut proc = [0u8; 32];
    _ = write!(&mut proc[..], "/proc/self/fd/{}\0", fd.as_raw_fd());
    let proc = CStr::from_bytes_until_nul(&proc).expect("a NUL written after the digits");
    if link.len() < PATH_MAX as usize {
        link.resize(PATH_MAX as usize, 0);
    }
    let target = linux::readlinkat(linux::CWD, SysPath::new(proc), link)
        .map_err(|error| io::Error::from_raw_os_error(i32::from(error.raw())))?;
    match target.ends_with(b" (deleted)") {
        true => Err(io::Error::from_raw_os_error(i32::from(Errno::Stale.raw()))),
        false => Ok(target.len()),
    }
}

/// `root` with its links resolved (`realpath(3)`, as [`std::fs::canonicalize`] does), into
/// `into`.
fn canonical(root: &Path, into: &mut PathBuf) -> io::Result<()> {
    let mut resolved = [0 as core::ffi::c_char; PATH_MAX as usize];
    blaze_mem::scratch::with_bytes(|c| {
        c.extend_from_slice(root.as_os_str().as_bytes());
        c.push(0);
        let c = CStr::from_bytes_with_nul(&c[..])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))?;
        // SAFETY: a NUL-terminated path and `PATH_MAX` bytes, which `realpath` writes at most.
        if unsafe { libc::realpath(c.as_ptr(), resolved.as_mut_ptr()) }.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `realpath` succeeded, so `resolved` holds a NUL-terminated path.
        let resolved = unsafe { CStr::from_ptr(resolved.as_ptr()) };
        into.as_mut_os_string().clear();
        into.as_mut_os_string().push(OsStr::from_bytes(resolved.to_bytes()));
        Ok(())
    })
}

/// `error` with what was being done, except a bare `EPERM`, which says a privilege is missing.
fn keep_eperm(error: io::Error, what: &str) -> io::Error {
    match error.raw_os_error() {
        Some(code) if code == i32::from(Errno::Perm.raw()) => error,
        _ => named(&error, what),
    }
}

fn named(error: &io::Error, what: &str) -> io::Error {
    // alloc: the error of a watch that failed.
    io::Error::new(error.kind(), format!("{what}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        time::Duration,
    };

    use super::*;
    use crate::wait::Clock as _;

    fn event(mask: u64, info: &[u8]) -> Vec<u8> {
        let len = METADATA + info.len();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(len as u32).to_ne_bytes());
        bytes.extend_from_slice(&[3, 0]);
        bytes.extend_from_slice(&(METADATA as u16).to_ne_bytes());
        bytes.extend_from_slice(&mask.to_ne_bytes());
        bytes.extend_from_slice(&(-1i32).to_ne_bytes());
        bytes.extend_from_slice(&42i32.to_ne_bytes());
        bytes.extend_from_slice(info);
        bytes
    }

    fn dfid_name(fsid: [u8; 8], kind: i32, handle: &[u8], name: &[u8]) -> Vec<u8> {
        let mut body = fsid.to_vec();
        body.extend_from_slice(&(handle.len() as u32).to_ne_bytes());
        body.extend_from_slice(&kind.to_ne_bytes());
        body.extend_from_slice(handle);
        body.extend_from_slice(name);
        body.push(0);
        while (body.len() + 4) % 4 != 0 {
            body.push(0);
        }
        let mut info = vec![INFO_DFID_NAME, 0];
        info.extend_from_slice(&((body.len() + 4) as u16).to_ne_bytes());
        info.extend_from_slice(&body);
        info
    }

    fn parsed(bytes: &[u8]) -> (Vec<(u64, Record<'_>)>, bool) {
        let mut events = Vec::new();
        let lost = parse(bytes, |mask, record| events.push((mask, record)));
        (events, lost)
    }

    #[test]
    fn events_are_read_with_their_directory_and_name() {
        let fsid = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut bytes = event(CREATE, &dfid_name(fsid, 1, &[9; 8], b"lib.rs"));
        bytes.extend(event(DELETE | ONDIR, &dfid_name(fsid, 1, &[7; 12], b"sub")));
        let (events, lost) = parsed(&bytes);
        assert!(!lost);
        let (file, dir) = (Key::of(&fsid, 1, &[9; 8]), Key::of(&fsid, 1, &[7; 12]));
        assert_eq!(
            events,
            vec![
                (
                    CREATE,
                    Record {
                        fsid: &fsid,
                        key: file.bytes(),
                        kind: 1,
                        handle: &[9; 8],
                        name: b"lib.rs"
                    }
                ),
                (
                    DELETE | ONDIR,
                    Record {
                        fsid: &fsid,
                        key: dir.bytes(),
                        kind: 1,
                        handle: &[7; 12],
                        name: b"sub"
                    }
                ),
            ]
        );
    }

    #[test]
    fn a_directory_is_placed_by_where_it_is_against_the_watched_one() {
        let dir = "crates/a";
        assert_eq!(classify(dir, "crates/a"), Kind::Under);
        assert_eq!(classify(dir, "crates/a/src"), Kind::Under);
        assert_eq!(classify(dir, ""), Kind::Above);
        assert_eq!(classify(dir, "crates"), Kind::Above);
        assert_eq!(classify(dir, "crates/ab"), Kind::Outside);
        assert_eq!(classify(dir, "crate"), Kind::Outside);
        assert_eq!(classify(dir, "target"), Kind::Outside);
    }

    /// The table of directories answers as a map of owned keys and names does, over the same
    /// random inserts, replacements, removals and lookups of keys whose hashes collide (a
    /// handful of handle bytes), many seeds, squeezes included; once warm it allocates nothing.
    #[test]
    fn dirs_agree_with_a_map_of_owned_keys() {
        for seed in 0..32 {
            let mut rng = crate::sim::Rng::new(seed);
            let mut ours: Dirs<u64> = Dirs::new(64).unwrap();
            let mut theirs: BTreeMap<Vec<u8>, (u64, String)> = BTreeMap::new();
            for step in 0..4_000u64 {
                let handle = [rng.below(8) as u8, rng.below(8) as u8];
                let key = Key::of(&[seed as u8; 8], 1, &handle[..1 + rng.below(2) as usize]);
                match rng.below(3) {
                    0 => {
                        ours.remove(key.bytes());
                        theirs.remove(key.bytes());
                    }
                    _ => {
                        let name = format!("d/{step}{}", "n".repeat(rng.below(30) as usize));
                        ours.insert(key.bytes(), step, &name);
                        theirs.insert(key.bytes().to_vec(), (step, name));
                    }
                }
                assert_eq!(ours.len(), theirs.len());
                assert_eq!(
                    ours.get(key.bytes()),
                    theirs.get(key.bytes()).map(|(at, name)| (*at, name.as_str()))
                );
            }
            for (key, (at, name)) in &theirs {
                assert_eq!(ours.get(key), Some((*at, name.as_str())));
            }
            let key = Key::of(&[seed as u8; 8], 1, &[0]);
            // Grows the squeeze's list to every entry kept, as a warm table's is.
            ours.squeeze();
            blaze_alloc_count::assert_at_most(0, "a warm insert, lookup and removal", || {
                ours.insert(key.bytes(), 1, "d/warm");
                assert_eq!(ours.get(key.bytes()), Some((1, "d/warm")));
                ours.remove(key.bytes());
                assert_eq!(ours.get(key.bytes()), None);
            });
        }
    }

    /// Events of directories whose places are known are read without opening a handle: those
    /// beside the watched directory are dropped, those above it count only on the way to it,
    /// and a directory moved drops every place, so what comes after it is found anew (here,
    /// with no file system marked, as outside).
    #[test]
    fn known_places_answer_until_a_directory_moves() {
        let root = crate::temp::TempDir::for_test("blaze-fanotify-seen-");
        let fd = OwnedFd::from(std::fs::File::open(root.path()).unwrap());
        let mut room = Room::new().unwrap();
        room.root = root.path().to_path_buf();
        room.dir.push_str("src");
        let mut watch = Fanotify { fd, room };
        let fsid = [3; 8];
        let (under, beside, top) = ([1; 8], [2; 8], [4; 8]);
        watch.room.seen.insert(Key::of(&fsid, 1, &under).bytes(), Kind::Under, "src/a");
        watch.room.seen.insert(Key::of(&fsid, 1, &beside).bytes(), Kind::Outside, "");
        watch.room.seen.insert(Key::of(&fsid, 1, &top).bytes(), Kind::Above, "");
        let mut bytes = event(CREATE, &dfid_name(fsid, 1, &under, b"x.rs"));
        bytes.extend(event(MODIFY, &dfid_name(fsid, 1, &under, b"x.rs")));
        bytes.extend(event(CLOSE_WRITE, &dfid_name(fsid, 1, &beside, b"out.o")));
        bytes.extend(event(MODIFY, &dfid_name(fsid, 1, &top, b"src")));
        bytes.extend(event(MODIFY, &dfid_name(fsid, 1, &top, b"Cargo.toml")));
        bytes.extend(event(MOVED_TO | ONDIR, &dfid_name(fsid, 1, &under, b"moved")));
        bytes.extend(event(MODIFY, &dfid_name(fsid, 1, &under, b"after.rs")));
        let Done::Changed { files, lost } = watch.take(&bytes) else { unreachable!() };
        assert!(!lost);
        let names: Vec<&str> = files.iter().map(FileName::as_str).collect();
        assert_eq!(names, ["src", "src/a/moved", "src/a/x.rs"]);
        assert_eq!(watch.room.seen.len(), 1);
        assert_eq!(
            watch.room.seen.get(Key::of(&fsid, 1, &under).bytes()),
            Some((Kind::Outside, ""))
        );
        drop(watch);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_overflow_or_a_short_event_is_lost() {
        let fsid = [0; 8];
        let mut bytes = event(Q_OVERFLOW, &[]);
        bytes.extend(event(MODIFY, &dfid_name(fsid, 1, &[1; 8], b"a")));
        let (events, lost) = parsed(&bytes);
        assert!(lost);
        assert_eq!(events.len(), 1);
        let (events, lost) = parsed(&bytes[..bytes.len() - 3]);
        assert!(lost);
        assert!(events.is_empty());
        // An event with no record of its directory cannot be named.
        assert_eq!(parsed(&event(MODIFY, &[])), (Vec::new(), true));
    }

    /// Mount points are the fifth field of each line, their escapes decoded (a space is
    /// `\040`), and only those strictly under the watched directory are its submounts.
    #[test]
    fn mountinfo_mount_points_are_read_with_their_escapes() {
        let fixture = b"22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n\
            36 22 0:31 / /root/my\\040tree rw - tmpfs tmpfs rw\n\
            37 36 0:32 / /root/my\\040tree/sub\\011tab\\134x rw - tmpfs tmpfs rw\n\
            38 22 0:33 /mnt1 /root/my\\040treeish rw,noatime master:1 - ext3 /dev/root rw\n\
            39 22 0:34 / /root/odd\\9 rw - tmpfs tmpfs rw\n";
        let mut point = Vec::new_in(Global);
        let mut points = Vec::new_in(Global);
        mount_points(fixture, &mut point, |it| {
            points.push(String::from_utf8(it.to_vec()).unwrap());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            points,
            ["/", "/root/my tree", "/root/my tree/sub\ttab\\x", "/root/my treeish", "/root/odd\\9"]
        );
        let top = b"/root/my tree";
        let under: Vec<&String> =
            points.iter().filter(|it| is_under_bytes(it.as_bytes(), top)).collect();
        assert_eq!(under, ["/root/my tree/sub\ttab\\x"]);
        assert!(is_under_bytes(b"/root", b"/") && !is_under_bytes(b"/", b"/"));
        let broken = mount_points(b"22 1 8:1 /\n", &mut point, |_| Ok(()));
        assert!(broken.unwrap_err().to_string().contains("no mount point"));
    }

    /// A watch of `dir` under `root`, or `None`, said, where the process may not use fanotify
    /// (the inotify path is tested by the drivers' tests then).
    fn privileged(root: &Path, dir: &FileName) -> Option<Fanotify> {
        match Fanotify::new(root, dir, true, None) {
            Ok(it) => Some(it),
            Err(error) if error.raw_os_error() == Some(i32::from(Errno::Perm.raw())) => {
                eprintln!("skipped: fanotify needs CAP_SYS_ADMIN and CAP_DAC_READ_SEARCH: {error}");
                None
            }
            Err(error) => panic!("{error}"),
        }
    }

    /// What `watch` hears until it hears `until`, which the caller changes last, or ten
    /// seconds pass: a watch hears changes in the order they were made, so what it has not
    /// heard by then it never will.
    fn listen(watch: &mut Fanotify, until: &FileName) -> BTreeSet<FileName> {
        let mut heard = BTreeSet::new();
        let mut buffer = vec![0u8; crate::watch::BUFFER];
        let clock = crate::wait::Monotonic::new();
        let deadline = crate::wait::Deadline::after(clock.now(), Duration::from_secs(10));
        while !heard.contains(until) {
            let mut fds = [libc::pollfd {
                fd: watch.raw(),
                events: linux::PollEvents::IN.bits(),
                revents: 0,
            }];
            let waited =
                crate::wait::readable("a test's fanotify events", &mut fds, Some(deadline), &clock)
                    .unwrap();
            if matches!(waited, crate::wait::Waited::Deadline) {
                break;
            }
            if let Ok(n @ 1..) = linux::read(Fd::new(watch.raw()), &mut buffer) {
                let Done::Changed { files, .. } = watch.take(&buffer[..n]) else {
                    unreachable!()
                };
                heard.extend(files);
            }
        }
        assert!(heard.contains(until), "{until} never heard: {heard:?}");
        heard
    }

    /// The invariant (the module's): an `rm -rf` of a subtree the watch never saw an event in
    /// is heard as its top directory's name, though every directory it removed is gone and
    /// was never learned. The control, the top deletion dropped, hears nothing of it.
    #[test]
    fn an_rm_rf_of_a_subtree_never_seen_is_heard() {
        fn heard(plant: Plant) -> Option<bool> {
            let root = crate::temp::TempDir::for_test("blaze-fanotify-rm-");
            for file in ["src/a/b/c/one.rs", "src/a/b/d/two.rs", "src/a/e/three.rs"] {
                std::fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
                std::fs::write(root.join(file), b"fn f() {}").unwrap();
            }
            let src = FileName::new("src").unwrap();
            let mut watch = privileged(&root, &src)?;
            assert_eq!(watch.room.seen.len(), 0, "the start learned a place under the watch");
            std::fs::remove_dir_all(root.join("src/a")).unwrap();
            let marker = FileName::new("src/marker.rs").unwrap();
            std::fs::write(root.join(marker.as_str()), b"").unwrap();
            PLANT.set(plant);
            let heard = listen(&mut watch, &marker);
            PLANT.set(Plant::None);
            assert!(heard.iter().all(|it| it.is_under(&src)), "{heard:?}");
            drop(watch);
            std::fs::remove_dir_all(root).unwrap();
            Some(heard.contains(&FileName::new("src/a").unwrap()))
        }
        let Some(told) = heard(Plant::None) else { return };
        assert!(told, "the removal of src/a was not told");
        assert_eq!(heard(Plant::TopDeletionDropped), Some(false), "the control was not caught");
    }

    /// Where the process may mount: a change on a file system mounted below the watched
    /// directory before the watch began is heard, the mount found in mountinfo; the control,
    /// mountinfo not read, never hears it.
    #[test]
    fn a_change_on_a_submount_below_the_watched_directory_is_heard() {
        fn heard(plant: Plant) -> Option<bool> {
            let root = crate::temp::TempDir::for_test("blaze-fanotify-mount-");
            std::fs::create_dir_all(root.join("src/sub")).unwrap();
            let sub = std::ffi::CString::new(root.join("src/sub").as_os_str().as_bytes()).unwrap();
            let tmpfs = SysPath::new(c"tmpfs");
            if let Err(error) = linux::task::mount(
                Some(tmpfs),
                SysPath::new(&sub),
                Some(tmpfs),
                linux::task::MountFlags::NONE,
                None,
            ) {
                let error = io::Error::from_raw_os_error(i32::from(error.raw()));
                eprintln!("skipped: mounting a tmpfs: {error}");
                return None;
            }
            let src = FileName::new("src").unwrap();
            PLANT.set(plant);
            let watch = Fanotify::new(&root, &src, true, None);
            PLANT.set(Plant::None);
            let unmount = || {
                let unmounted =
                    linux::task::umount2(SysPath::new(&sub), linux::task::UnmountFlags::DETACH);
                assert_eq!(unmounted.map_or(-1, |()| 0), 0);
            };
            let mut watch = match watch {
                Ok(it) => it,
                Err(error) => {
                    unmount();
                    match error.raw_os_error() == Some(i32::from(Errno::Perm.raw()))
                        || error.kind() == io::ErrorKind::Unsupported
                    {
                        // A kernel whose tmpfs has no fsid fanotify takes (before 6.7) cannot
                        // be marked, which fails the watch; said, and passed over here.
                        true => {
                            eprintln!("skipped: fanotify of a tmpfs: {error}");
                            return None;
                        }
                        false => panic!("{error}"),
                    }
                }
            };
            std::fs::write(root.join("src/sub/lib.rs"), b"fn f() {}").unwrap();
            let marker = FileName::new("src/marker.rs").unwrap();
            std::fs::write(root.join(marker.as_str()), b"").unwrap();
            let heard = listen(&mut watch, &marker);
            drop(watch);
            unmount();
            std::fs::remove_dir_all(root).unwrap();
            Some(heard.contains(&FileName::new("src/sub/lib.rs").unwrap()))
        }
        let Some(told) = heard(Plant::None) else { return };
        assert!(told, "the change on the submount was not heard");
        assert_eq!(heard(Plant::MountinfoUnread), Some(false), "the control was not caught");
    }

    /// The outcome: starting a watch of a tree of 100 000 directories makes a constant number
    /// of system calls (counted by the start itself) and learns no place under the watched
    /// directory, so the thread that starts it never waits on the tree. The control, the walk
    /// put back, makes one per directory.
    #[test]
    fn a_start_makes_a_constant_number_of_system_calls() {
        /// What a start may make, whatever is under the watched directory: fanotify_init, two
        /// realpaths, the open and mark of the watched directory's file system (five), two per
        /// directory above it (one here), and mountinfo's open, reads and close.
        const MOST: u32 = 32;
        let root = crate::temp::TempDir::for_test("blaze-fanotify-big-");
        for a in 0..100 {
            for b in 0..1000 {
                std::fs::create_dir_all(root.join(format!("src/d{a}/e{b}"))).unwrap();
            }
        }
        let src = FileName::new("src").unwrap();
        let begun = std::time::Instant::now();
        let Some(watch) = privileged(&root, &src) else { return };
        let took = begun.elapsed();
        let calls = watch.room.calls;
        eprintln!("a start of a watch of 100 100 directories: {calls} system calls, {took:?}");
        assert!(calls <= MOST, "{calls} system calls");
        assert!(watch.room.known.len() <= 2 && watch.room.seen.len() == 0);
        let room = watch.end();
        PLANT.set(Plant::WalksAtStart);
        let walked = Fanotify::new(&root, &src, true, Some(room)).unwrap();
        PLANT.set(Plant::None);
        assert!(walked.room.calls > MOST, "the control was not caught: {}", walked.room.calls);
        drop(walked);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Where the process may: a file made under the watched directory, in a directory made
    /// after the watch began, is heard of, and one outside it is not; a watch made again from
    /// the first's room hears the same.
    #[test]
    fn hears_what_is_under_the_watched_directory_only() {
        let root = crate::temp::TempDir::for_test("blaze-fanotify-");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let src = FileName::new("src").unwrap();
        let mut room = None;
        for round in 0..2 {
            let mut watch = match Fanotify::new(&root, &src, true, room.take()) {
                Ok(it) => it,
                Err(error) if error.raw_os_error() == Some(i32::from(Errno::Perm.raw())) => {
                    eprintln!(
                        "skipped: fanotify needs CAP_SYS_ADMIN and CAP_DAC_READ_SEARCH: {error}"
                    );
                    return;
                }
                Err(error) => panic!("{error}"),
            };
            let deep = format!("src/deep{round}");
            std::fs::create_dir_all(root.join(&deep)).unwrap();
            std::fs::write(root.join(&deep).join("lib.rs"), b"fn f() {}").unwrap();
            std::fs::write(root.join("outside"), b"not watched").unwrap();
            let mut heard = BTreeSet::new();
            let mut buffer = vec![0u8; crate::watch::BUFFER];
            // One deadline for the whole watch, on a clock made as it begins.
            let clock = crate::wait::Monotonic::new();
            let deadline = crate::wait::Deadline::after(clock.now(), Duration::from_secs(10));
            let want = FileName::new(&format!("{deep}/lib.rs")).unwrap();
            while !heard.contains(&want) {
                let mut fds = [libc::pollfd {
                    fd: watch.raw(),
                    events: linux::PollEvents::IN.bits(),
                    revents: 0,
                }];
                // Until an event comes or the deadline passes; the watch reads nonblocking.
                let waited = crate::wait::readable(
                    "the fanotify watch's events",
                    &mut fds,
                    Some(deadline),
                    &clock,
                )
                .unwrap();
                if matches!(waited, crate::wait::Waited::Deadline) {
                    break;
                }
                if let Ok(n @ 1..) = linux::read(Fd::new(watch.raw()), &mut buffer) {
                    let Done::Changed { files, .. } = watch.take(&buffer[..n]) else {
                        unreachable!()
                    };
                    heard.extend(files);
                }
            }
            assert!(heard.contains(&want), "{heard:?}");
            assert!(heard.contains(&FileName::new(&deep).unwrap()), "{heard:?}");
            assert!(heard.iter().all(|it| it.is_under(&src)), "{heard:?}");
            room = Some(watch.end());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
