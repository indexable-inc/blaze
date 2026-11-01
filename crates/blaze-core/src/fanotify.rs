//! Watching a tree with fanotify: one mark of each file system the watched directory spans
//! (`FAN_MARK_FILESYSTEM`), so nothing is added as directories appear, and events that name
//! the directory a change was in by a file handle and the file by its name
//! (`FAN_REPORT_DFID_NAME`, Linux 5.9). A handle is opened again (`open_by_handle_at`) to
//! learn where its directory is now; a directory gone since is named where it was last seen.
//!
//! Needs `CAP_SYS_ADMIN` (the mark) and `CAP_DAC_READ_SEARCH` (opening handles):
//! [`Fanotify::new`] fails with a bare `EPERM` where the process lacks either, and
//! [`crate::watch::Tree`] watches with inotify then.
//!
//! What a watch keeps (where each directory is, by its handle) is in [`Dirs`] tables whose keys
//! and names are spans of a [`Strings`] column, reused by the next watch once this one ends
//! ([`Room`]): a watch, its events and its end allocate their completions' payload alone.
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
};

use blaze_hash::HashMap;
use blaze_mem::{RegionVec, Tag as MemTag};

use crate::{
    Done, FileName,
    utf8::push_lossy,
    watch::{Batch, Span, Strings, Visit, Walker, is_under, valid},
};

const CLASS_NOTIF: u32 = 0x0;
const CLOEXEC: u32 = 0x1;
const NONBLOCK: u32 = 0x2;
const REPORT_DIR_FID: u32 = 0x400;
const REPORT_NAME: u32 = 0x800;

const MARK_ADD: u32 = 0x1;
const MARK_FILESYSTEM: u32 = 0x100;

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
        error.raw_os_error(),
        Some(libc::ENOENT | libc::ENOTDIR | libc::ESTALE | libc::EACCES | libc::ELOOP)
    )
}

/// How many directories' places are kept before the cache starts again.
const SEEN: usize = 1 << 16;

/// Directories by key ([`Key`]), each with a value and a name relative to the root: the key
/// and the name one after the other in a [`Strings`]. A chain of entries per hash of a key, so
/// each lookup hashes the key once and compares the bytes of the entries of its hash.
struct Dirs<V> {
    /// The first entry of each hash.
    heads: HashMap<u64, u32>,
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
            heads: HashMap::with_capacity_and_hasher(heads, Default::default()),
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
        std::str::from_utf8(&self.strings.bytes(entry.span)[entry.key as usize..]).expect("pushed as a str")
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
    /// Where each directory under `dir` was last seen, relative to the root.
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
    walker: Walker,
}

impl Room {
    fn new() -> io::Result<Room> {
        Ok(Room {
            root: PathBuf::new(), // alloc: none, filled once and reused.
            dir: String::new(),   // alloc: none, filled once and reused.
            mounts: Vec::new(),   // alloc: none, an empty vector, grown once and reused.
            known: Dirs::new(1 << 10)?,
            seen: Dirs::new(SEEN)?,
            rel: String::new(), // alloc: none, filled once and reused.
            link: Vec::new(),   // alloc: none, an empty vector, grown once and reused.
            batch: Batch::default(),
            walker: Walker::default(),
        })
    }
}

impl Fanotify {
    /// Watches `dir` under `root` and everything under it, into `room`'s tables.
    pub(crate) fn new(root: &Path, dir: &FileName, nonblocking: bool, room: Option<Room>) -> io::Result<Fanotify> {
        let flags = CLASS_NOTIF
            | CLOEXEC
            | REPORT_DIR_FID
            | REPORT_NAME
            | if nonblocking { NONBLOCK } else { 0 };
        let event_flags = (libc::O_RDONLY | libc::O_CLOEXEC) as libc::c_uint;
        // SAFETY: no pointers; a descriptor of its own or -1.
        let fd = unsafe { libc::syscall(libc::SYS_fanotify_init, flags, event_flags) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::EPERM) => error,
                Some(libc::EINVAL) => io::Error::new(
                    io::ErrorKind::Unsupported,
                    // alloc: the error of a watch that failed.
                    format!(
                        "fanotify_init: {error}: Linux 5.9 or later reports a change's directory \
                         and name (FAN_REPORT_DFID_NAME), and this is Linux {}",
                        crate::cgroup::kernel()
                    ),
                ),
                _ => named(error, "fanotify_init"),
            });
        }
        // SAFETY: the call returned a descriptor of its own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        let mut room = match room {
            Some(room) => room,
            None => Room::new()?,
        };
        // alloc: the error of a watch that failed.
        canonical(root, &mut room.root).map_err(|error| named(error, &format!("resolving {}", root.display())))?;
        room.dir.clear();
        room.dir.push_str(dir.as_str());
        let mut watch = Fanotify { fd, room };
        watch.learn(dir.as_str(), true, false)?;
        Ok(watch)
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

    /// What the events in `bytes` say, learning each directory they say was made or moved in.
    /// An event in a directory whose place is known costs one hash lookup, and one beside the
    /// watched directory nothing more.
    pub(crate) fn take(&mut self, bytes: &[u8]) -> Done {
        let mut lost = parse(bytes, |_, _| {});
        if lost {
            self.room.seen.clear();
        }
        let mut rel = std::mem::take(&mut self.room.rel);
        parse(bytes, |mask, record| {
            let kind = match self.place(&record, &mut rel) {
                Ok(Kind::Outside) => return,
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
            if !valid(&rel) || (kind == Kind::Above && !is_under(&rel, &self.room.dir)) {
                return;
            }
            if mask & ONDIR != 0 && mask & (CREATE | MOVED_TO) != 0 && self.learn(&rel, false, true).is_err() {
                lost = true;
            }
            self.room.batch.push(&rel);
        });
        self.room.rel = rel;
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

    /// Learns the handle of `rel` and of every directory under it, marking each file system
    /// met, and adds each file found to the batch with `found`: a directory moved in has no
    /// events of what was in it already. Every error of the watched directory itself (`top`)
    /// is the watch's; below it, a directory gone or unreadable is passed over.
    fn learn(&mut self, rel: &str, top: bool, found: bool) -> io::Result<()> {
        let fanotify = self.fd.as_raw_fd();
        let Room { root, dir, mounts, known, seen, batch, walker, .. } = &mut self.room;
        walker.walk(
            root.as_os_str().as_bytes(),
            rel,
            |path, rel, first| match learn_one(fanotify, mounts, dir, known, seen, path, rel) {
                Ok(listed) => Ok(Visit::List(listed)),
                Err(error) if top && first => Err(match gone(&error) {
                    // alloc: the error of a watch that failed.
                    true => named(error, &format!("opening {}", path.to_string_lossy())),
                    false => error,
                }),
                Err(error) if gone(&error) => Ok(Visit::Skip),
                Err(error) => Err(error),
            },
            |file| {
                if found {
                    batch.push(file);
                }
            },
        )
    }

    /// Where the directory of `handle` (whose [`Key`] is `key`) is now, its name relative to the
    /// root written to `rel`: outside the root, or a directory gone that was never under the
    /// watched one, is [`Kind::Outside`]. An error where it cannot be said.
    fn resolve(&mut self, fsid: &[u8], key: &[u8], kind: i32, handle: &[u8], rel: &mut String) -> io::Result<Kind> {
        let Room { root, dir, mounts, known, link, .. } = &mut self.room;
        let Some(mount) = mounts.iter().find(|it| it.fsid[..] == *fsid) else {
            return Ok(Kind::Outside);
        };
        let found = open_by_handle(mount.fd.as_raw_fd(), kind, handle).and_then(|fd| path_of(&fd, link));
        match found {
            Ok(len) => {
                let Ok(under) = Path::new(OsStr::from_bytes(&link[..len])).strip_prefix(&*root) else {
                    known.remove(key);
                    return Ok(Kind::Outside);
                };
                push_lossy(rel, under.as_os_str().as_bytes());
                let place = classify(dir, rel);
                match place {
                    Kind::Under => known.insert(key, (), rel),
                    _ => known.remove(key),
                }
                Ok(place)
            }
            Err(error) if matches!(error.raw_os_error(), Some(libc::ESTALE | libc::ENOENT)) => {
                let Some(((), name)) = known.get(key) else { return Ok(Kind::Outside) };
                rel.push_str(name);
                Ok(classify(dir, rel))
            }
            Err(error) => Err(error),
        }
    }
}

/// The directory at `path` (`rel` under the root) known by its handle, its file system marked
/// if it was not: the descriptor opened for it, to list it, unless its mount keeps it.
fn learn_one(
    fanotify: RawFd,
    mounts: &mut Vec<Mount>,
    dir: &str,
    known: &mut Dirs<()>,
    seen: &mut Dirs<Kind>,
    path: &CStr,
    rel: &str,
) -> io::Result<Option<OwnedFd>> {
    let opened = match crate::raw::open_dir(path) {
        Ok(opened) => opened,
        // Kept bare so `learn` can pass over a directory gone below the watched one.
        Err(error) => {
            return Err(match gone(&error) {
                true => error,
                // alloc: the error of a watch that failed.
                false => keep_eperm(error, &format!("opening {}", path.to_string_lossy())),
            });
        }
    };
    let raw = opened.as_raw_fd();
    // alloc: the error of a watch that failed.
    let fsid = fsid(raw).map_err(|error| named(error, &format!("statfs of {}", path.to_string_lossy())))?;
    let handle = handle_of(raw).map_err(|error| match error.raw_os_error() {
        Some(libc::EOPNOTSUPP) => io::Error::new(
            io::ErrorKind::Unsupported,
            // alloc: the error of a watch that failed.
            format!(
                "the file system of {} gives no file handles, which watching it with \
                 fanotify needs: {error}",
                path.to_string_lossy()
            ),
        ),
        // alloc: the error of a watch that failed.
        _ => keep_eperm(error, &format!("name_to_handle_at of {}", path.to_string_lossy())),
    })?;
    let mut listed = Some(opened);
    if !mounts.iter().any(|it| it.fsid == fsid) {
        // SAFETY: a fanotify descriptor and a NUL-terminated path.
        let marked = unsafe {
            libc::syscall(
                libc::SYS_fanotify_mark,
                fanotify,
                MARK_ADD | MARK_FILESYSTEM,
                MASK,
                libc::AT_FDCWD,
                path.as_ptr(),
            )
        };
        if marked != 0 {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::EXDEV | libc::EOPNOTSUPP | libc::ENODEV) => io::Error::new(
                    io::ErrorKind::Unsupported,
                    // alloc: the error of a watch that failed.
                    format!(
                        "fanotify cannot watch the file system of {} by file handles: {error}",
                        path.to_string_lossy()
                    ),
                ),
                // alloc: the error of a watch that failed.
                _ => keep_eperm(error, &format!("fanotify_mark of {}", path.to_string_lossy())),
            });
        }
        // Opening a handle needs CAP_DAC_READ_SEARCH, which marking did not.
        open_by_handle(raw, handle.kind, handle.bytes())
            // alloc: the error of a watch that failed.
            .map_err(|error| keep_eperm(error, &format!("open_by_handle_at in {}", path.to_string_lossy())))?;
        // The mount keeps this descriptor; the walk lists the directory through one of its own.
        mounts.push(Mount { fsid, fd: listed.take().expect("opened above") });
    }
    let key = Key::of(&fsid, handle.kind, handle.bytes());
    let place = classify(dir, rel);
    if place == Kind::Under {
        known.insert(key.bytes(), (), rel);
    }
    if seen.len() < SEEN {
        seen.insert(key.bytes(), place, if place == Kind::Outside { "" } else { rel });
    }
    Ok(listed)
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
        if len < METADATA || metadata_len < METADATA || metadata_len > len || at + len > bytes.len() {
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
    // SAFETY: all zeros is a valid `statfs`; the call fills it.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: an open descriptor and a `statfs` to write.
    if unsafe { libc::fstatfs(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fsid_t` is two `int`s (`__kernel_fsid_t`), whose field libc keeps private, so no
    // safe read of it exists; `transmute` checks the sizes match, and every bit pattern is an
    // `[i32; 2]`.
    let words: [i32; 2] = unsafe { std::mem::transmute(stat.f_fsid) };
    let mut fsid = [0; 8];
    fsid[..4].copy_from_slice(&words[0].to_ne_bytes());
    fsid[4..].copy_from_slice(&words[1].to_ne_bytes());
    Ok(fsid)
}

/// The handle of the directory open as `fd`, on the stack.
fn handle_of(fd: RawFd) -> io::Result<Handle> {
    let mut handle = Handle { bytes: MAX_HANDLE as u32, kind: 0, handle: [0; MAX_HANDLE] };
    let mut mount_id: libc::c_int = 0;
    // SAFETY: an open descriptor with an empty path, a handle with room for `bytes` bytes,
    // and an int for the mount's id.
    let got = unsafe {
        libc::syscall(
            libc::SYS_name_to_handle_at,
            fd,
            c"".as_ptr(),
            &raw mut handle,
            &raw mut mount_id,
            libc::AT_EMPTY_PATH,
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
            libc::SYS_open_by_handle_at,
            mount,
            &raw mut handle,
            libc::O_PATH | libc::O_CLOEXEC,
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
    if link.len() < libc::PATH_MAX as usize {
        link.resize(libc::PATH_MAX as usize, 0);
    }
    // SAFETY: a NUL-terminated path, and `link.len()` bytes to write the link into, which the
    // call writes at most.
    let got = unsafe { libc::readlink(proc.as_ptr(), link.as_mut_ptr().cast(), link.len()) };
    if got < 0 {
        return Err(io::Error::last_os_error());
    }
    let len = got as usize;
    if len == link.len() {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    match link[..len].ends_with(b" (deleted)") {
        true => Err(io::Error::from_raw_os_error(libc::ESTALE)),
        false => Ok(len),
    }
}

/// `root` with its links resolved (`realpath(3)`, as [`std::fs::canonicalize`] does), into
/// `into`.
fn canonical(root: &Path, into: &mut PathBuf) -> io::Result<()> {
    let mut resolved = [0 as libc::c_char; libc::PATH_MAX as usize];
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
        Some(libc::EPERM) => error,
        _ => named(error, what),
    }
}

fn named(error: io::Error, what: &str) -> io::Error {
    // alloc: the error of a watch that failed.
    io::Error::new(error.kind(), format!("{what}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        time::{Duration, Instant},
    };

    use super::*;

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
                    Record { fsid: &fsid, key: file.bytes(), kind: 1, handle: &[9; 8], name: b"lib.rs" }
                ),
                (
                    DELETE | ONDIR,
                    Record { fsid: &fsid, key: dir.bytes(), kind: 1, handle: &[7; 12], name: b"sub" }
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
            let mut theirs: HashMap<Vec<u8>, (u64, String)> = HashMap::default();
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
                assert_eq!(ours.get(key.bytes()), theirs.get(key.bytes()).map(|(at, name)| (*at, name.as_str())));
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
        let root = std::env::temp_dir().join(format!("blaze-fanotify-seen-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let fd = OwnedFd::from(std::fs::File::open(&root).unwrap());
        let mut room = Room::new().unwrap();
        room.root = root.clone();
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
        assert_eq!(watch.room.seen.get(Key::of(&fsid, 1, &under).bytes()), Some((Kind::Outside, "")));
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

    /// Where the process may: a file made under the watched directory, in a directory made
    /// after the watch began, is heard of, and one outside it is not; a watch made again from
    /// the first's room hears the same.
    #[test]
    fn hears_what_is_under_the_watched_directory_only() {
        let root = std::env::temp_dir().join(format!("blaze-fanotify-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let src = FileName::new("src").unwrap();
        let mut room = None;
        for round in 0..2 {
            let mut watch = match Fanotify::new(&root, &src, true, room.take()) {
                Ok(it) => it,
                Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                    eprintln!("skipped: fanotify needs CAP_SYS_ADMIN and CAP_DAC_READ_SEARCH: {error}");
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
            let deadline = Instant::now() + Duration::from_secs(10);
            let want = FileName::new(&format!("{deep}/lib.rs")).unwrap();
            while !heard.contains(&want) && Instant::now() < deadline {
                let mut fds = [libc::pollfd { fd: watch.raw(), events: libc::POLLIN, revents: 0 }];
                // SAFETY: one pollfd of an open descriptor.
                unsafe { libc::poll(fds.as_mut_ptr(), 1, 100) };
                // SAFETY: the buffer has room for what is read.
                let n = unsafe { libc::read(watch.raw(), buffer.as_mut_ptr().cast(), buffer.len()) };
                if n > 0 {
                    let Done::Changed { files, .. } = watch.take(&buffer[..n as usize]) else {
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
