//! Change notification: inotify (a watch per directory, unprivileged) and fanotify (a mark
//! per file system, by file handle), what blaze-core's watchers (watch.rs, fanotify.rs) call
//! through libc today. A read of either descriptor is [`super::read`]; its bytes are walked by
//! [`InotifyEvents`] and [`FanEvents`], which copy nothing.

use super::{
    fs::{arg, owned, OpenFlags, OwnedFd},
    nr, raw,
};
use crate::{Errno, Fd, Path};

flag_set! {
    /// `inotify_init1`'s flags.
    InotifyFlags: u32 {
        /// `IN_NONBLOCK` (`O_NONBLOCK`).
        NONBLOCK = 0o4000;
        /// `IN_CLOEXEC` (`O_CLOEXEC`).
        CLOSE_ON_EXEC = 0o2000000;
    }
}

flag_set! {
    /// What an inotify watch reports, and what an event says happened.
    InotifyMask: u32 {
        /// `IN_ACCESS`.
        ACCESS = 0x1;
        /// `IN_MODIFY`.
        MODIFY = 0x2;
        /// `IN_ATTRIB`.
        ATTRIB = 0x4;
        /// `IN_CLOSE_WRITE`.
        CLOSE_WRITE = 0x8;
        /// `IN_CLOSE_NOWRITE`.
        CLOSE_NO_WRITE = 0x10;
        /// `IN_OPEN`.
        OPEN = 0x20;
        /// `IN_MOVED_FROM`.
        MOVED_FROM = 0x40;
        /// `IN_MOVED_TO`.
        MOVED_TO = 0x80;
        /// `IN_CREATE`.
        CREATE = 0x100;
        /// `IN_DELETE`.
        DELETE = 0x200;
        /// `IN_DELETE_SELF`.
        DELETE_SELF = 0x400;
        /// `IN_MOVE_SELF`.
        MOVE_SELF = 0x800;
        /// `IN_UNMOUNT` (an event only).
        UNMOUNT = 0x2000;
        /// `IN_Q_OVERFLOW` (an event only): events were lost; rescan.
        OVERFLOW = 0x4000;
        /// `IN_IGNORED` (an event only): the watch is gone.
        IGNORED = 0x8000;
        /// `IN_ONLYDIR`: watch `path` only if it is a directory.
        ONLY_DIR = 0x0100_0000;
        /// `IN_DONT_FOLLOW`.
        NO_FOLLOW = 0x0200_0000;
        /// `IN_EXCL_UNLINK`: no events for a child after it is unlinked.
        EXCLUDE_UNLINKED = 0x0400_0000;
        /// `IN_MASK_CREATE`: [`Errno::Exist`] where `path` is watched already.
        MASK_CREATE = 0x1000_0000;
        /// `IN_MASK_ADD`: add to the watch's mask, not replace it.
        MASK_ADD = 0x2000_0000;
        /// `IN_ISDIR` (an event only): the subject is a directory.
        IS_DIR = 0x4000_0000;
        /// `IN_ONESHOT`.
        ONE_SHOT = 0x8000_0000;
    }
}

/// An inotify watch: what [`inotify_add_watch`] returned, what an event names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Watch(pub i32);

/// `inotify_init1`: a new inotify instance. Used by blaze-core's directory watcher.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn inotify_init1(flags: InotifyFlags) -> Result<OwnedFd, Errno> {
    // SAFETY: the call takes a number and no memory.
    owned(unsafe { raw::syscall1(nr::INOTIFY_INIT1, flags.bits() as usize) })
}

/// `inotify_add_watch`: `path` (from the current directory, or absolute: inotify takes no
/// directory descriptor) watched for `mask`; the same path again is the same [`Watch`].
/// [`Errno::NoSpc`] past `max_user_watches`, which is the watcher's refusal to report.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn inotify_add_watch(inotify: Fd, path: Path<'_>, mask: InotifyMask) -> Result<Watch, Errno> {
    // SAFETY: `path` is a live NUL-terminated string the kernel reads and does not keep.
    let watch = raw::check(unsafe {
        raw::syscall3(nr::INOTIFY_ADD_WATCH, arg(inotify), path.as_ptr() as usize, mask.bits() as usize)
    })?;
    Ok(Watch(watch as i32))
}

/// `inotify_rm_watch`: the watch removed; an [`InotifyMask::IGNORED`] event follows.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn inotify_rm_watch(inotify: Fd, watch: Watch) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall2(nr::INOTIFY_RM_WATCH, arg(inotify), watch.0 as isize as usize) })
        .map(|_| ())
}

/// The size of `struct inotify_event` before its name: `wd`, `mask`, `cookie`, `len`.
pub const INOTIFY_EVENT_HEADER: usize = 16;

/// One event of an inotify read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InotifyEvent<'a> {
    /// The watch it is for; -1 for [`InotifyMask::OVERFLOW`].
    pub watch: Watch,
    pub mask: InotifyMask,
    /// Pairs a `MOVED_FROM` with its `MOVED_TO`.
    pub cookie: u32,
    /// The child's name under a watched directory, its NUL padding cut; empty for the watched
    /// thing itself.
    pub name: &'a [u8],
}

/// The events of one inotify read, in the kernel's order.
#[derive(Clone, Copy, Debug)]
pub struct InotifyEvents<'a> {
    left: &'a [u8],
}

impl<'a> InotifyEvents<'a> {
    /// The events in `read`, the bytes a [`super::read`] of an inotify descriptor filled.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn new(read: &'a [u8]) -> InotifyEvents<'a> {
        InotifyEvents { left: read }
    }
}

impl<'a> Iterator for InotifyEvents<'a> {
    type Item = InotifyEvent<'a>;

    /// The next event. An event that does not fit what is left ends the walk: the kernel
    /// writes whole events.
    fn next(&mut self) -> Option<InotifyEvent<'a>> {
        let header = self.left.first_chunk::<INOTIFY_EVENT_HEADER>()?;
        let word = |at: usize| header.get(at..at + 4).and_then(|it| it.first_chunk::<4>()).copied();
        let watch = i32::from_ne_bytes(word(0)?);
        let mask = u32::from_ne_bytes(word(4)?);
        let cookie = u32::from_ne_bytes(word(8)?);
        let len = u32::from_ne_bytes(word(12)?) as usize;
        let end = INOTIFY_EVENT_HEADER.checked_add(len)?;
        let padded = self.left.get(INOTIFY_EVENT_HEADER..end)?;
        let name = padded.get(..padded.iter().position(|&byte| byte == 0).unwrap_or(padded.len()))?;
        self.left = self.left.get(end..)?;
        Some(InotifyEvent { watch: Watch(watch), mask: InotifyMask::from_bits(mask), cookie, name })
    }
}

flag_set! {
    /// `fanotify_init`'s flags.
    /// replaces libc::FAN_CLOEXEC (blaze_sys::linux::FanInit::CLOSE_ON_EXEC)
    /// replaces libc::FAN_NONBLOCK (blaze_sys::linux::FanInit::NONBLOCK)
    /// replaces libc::FAN_CLASS_NOTIF (blaze_sys::linux::FanInit::CLASS_NOTIF)
    /// replaces libc::FAN_CLASS_CONTENT (blaze_sys::linux::FanInit::CLASS_CONTENT)
    /// replaces libc::FAN_CLASS_PRE_CONTENT (blaze_sys::linux::FanInit::CLASS_PRE_CONTENT)
    /// replaces libc::FAN_UNLIMITED_QUEUE (blaze_sys::linux::FanInit::UNLIMITED_QUEUE)
    /// replaces libc::FAN_UNLIMITED_MARKS (blaze_sys::linux::FanInit::UNLIMITED_MARKS)
    /// replaces libc::FAN_REPORT_TID (blaze_sys::linux::FanInit::REPORT_TID)
    /// replaces libc::FAN_REPORT_FID (blaze_sys::linux::FanInit::REPORT_FID)
    /// replaces libc::FAN_REPORT_DIR_FID (blaze_sys::linux::FanInit::REPORT_DIR_FID)
    /// replaces libc::FAN_REPORT_NAME (blaze_sys::linux::FanInit::REPORT_NAME)
    /// replaces libc::FAN_REPORT_TARGET_FID (blaze_sys::linux::FanInit::REPORT_TARGET_FID)
    FanInit: u32 {
        /// `FAN_CLOEXEC`.
        CLOSE_ON_EXEC = 0x1;
        /// `FAN_NONBLOCK`.
        NONBLOCK = 0x2;
        /// `FAN_CLASS_NOTIF` (0): notification only, no permission events.
        CLASS_NOTIF = 0x0;
        /// `FAN_CLASS_CONTENT`.
        CLASS_CONTENT = 0x4;
        /// `FAN_CLASS_PRE_CONTENT`.
        CLASS_PRE_CONTENT = 0x8;
        /// `FAN_UNLIMITED_QUEUE`.
        UNLIMITED_QUEUE = 0x10;
        /// `FAN_UNLIMITED_MARKS`.
        UNLIMITED_MARKS = 0x20;
        /// `FAN_REPORT_TID`.
        REPORT_TID = 0x100;
        /// `FAN_REPORT_FID`: events name a file handle, not an open descriptor (unprivileged
        /// from Linux 5.13 for inode marks).
        REPORT_FID = 0x200;
        /// `FAN_REPORT_DIR_FID`.
        REPORT_DIR_FID = 0x400;
        /// `FAN_REPORT_NAME`.
        REPORT_NAME = 0x800;
        /// `FAN_REPORT_TARGET_FID`.
        REPORT_TARGET_FID = 0x1000;
    }
}

flag_set! {
    /// `fanotify_mark`'s flags: one of `ADD`, `REMOVE` or `FLUSH`, and what is marked.
    /// replaces libc::FAN_MARK_ADD (blaze_sys::linux::FanMark::ADD)
    /// replaces libc::FAN_MARK_REMOVE (blaze_sys::linux::FanMark::REMOVE)
    /// replaces libc::FAN_MARK_DONT_FOLLOW (blaze_sys::linux::FanMark::NO_FOLLOW)
    /// replaces libc::FAN_MARK_ONLYDIR (blaze_sys::linux::FanMark::ONLY_DIR)
    /// replaces libc::FAN_MARK_MOUNT (blaze_sys::linux::FanMark::MOUNT)
    /// replaces libc::FAN_MARK_IGNORED_MASK (blaze_sys::linux::FanMark::IGNORED_MASK)
    /// replaces libc::FAN_MARK_IGNORED_SURV_MODIFY (blaze_sys::linux::FanMark::IGNORED_SURVIVES_MODIFY)
    /// replaces libc::FAN_MARK_FLUSH (blaze_sys::linux::FanMark::FLUSH)
    /// replaces libc::FAN_MARK_FILESYSTEM (blaze_sys::linux::FanMark::FILESYSTEM)
    FanMark: u32 {
        /// `FAN_MARK_ADD`.
        ADD = 0x1;
        /// `FAN_MARK_REMOVE`.
        REMOVE = 0x2;
        /// `FAN_MARK_DONT_FOLLOW`.
        NO_FOLLOW = 0x4;
        /// `FAN_MARK_ONLYDIR`.
        ONLY_DIR = 0x8;
        /// `FAN_MARK_MOUNT`: the mount `path` is on.
        MOUNT = 0x10;
        /// `FAN_MARK_IGNORED_MASK`.
        IGNORED_MASK = 0x20;
        /// `FAN_MARK_IGNORED_SURV_MODIFY`.
        IGNORED_SURVIVES_MODIFY = 0x40;
        /// `FAN_MARK_FLUSH`.
        FLUSH = 0x80;
        /// `FAN_MARK_FILESYSTEM`: the whole file system `path` is on (needs `CAP_SYS_ADMIN`).
        FILESYSTEM = 0x100;
    }
}

flag_set! {
    /// What a fanotify mark reports, and what an event says happened.
    /// replaces libc::FAN_ACCESS (blaze_sys::linux::FanMask::ACCESS)
    /// replaces libc::FAN_MODIFY (blaze_sys::linux::FanMask::MODIFY)
    /// replaces libc::FAN_ATTRIB (blaze_sys::linux::FanMask::ATTRIB)
    /// replaces libc::FAN_CLOSE_WRITE (blaze_sys::linux::FanMask::CLOSE_WRITE)
    /// replaces libc::FAN_CLOSE_NOWRITE (blaze_sys::linux::FanMask::CLOSE_NO_WRITE)
    /// replaces libc::FAN_OPEN (blaze_sys::linux::FanMask::OPEN)
    /// replaces libc::FAN_MOVED_FROM (blaze_sys::linux::FanMask::MOVED_FROM)
    /// replaces libc::FAN_MOVED_TO (blaze_sys::linux::FanMask::MOVED_TO)
    /// replaces libc::FAN_CREATE (blaze_sys::linux::FanMask::CREATE)
    /// replaces libc::FAN_DELETE (blaze_sys::linux::FanMask::DELETE)
    /// replaces libc::FAN_DELETE_SELF (blaze_sys::linux::FanMask::DELETE_SELF)
    /// replaces libc::FAN_MOVE_SELF (blaze_sys::linux::FanMask::MOVE_SELF)
    /// replaces libc::FAN_OPEN_EXEC (blaze_sys::linux::FanMask::OPEN_EXEC)
    /// replaces libc::FAN_Q_OVERFLOW (blaze_sys::linux::FanMask::OVERFLOW)
    /// replaces libc::FAN_FS_ERROR (blaze_sys::linux::FanMask::FS_ERROR)
    /// replaces libc::FAN_EVENT_ON_CHILD (blaze_sys::linux::FanMask::ON_CHILD)
    /// replaces libc::FAN_RENAME (blaze_sys::linux::FanMask::RENAME)
    /// replaces libc::FAN_ONDIR (blaze_sys::linux::FanMask::ON_DIR)
    FanMask: u64 {
        /// `FAN_ACCESS`.
        ACCESS = 0x1;
        /// `FAN_MODIFY`.
        MODIFY = 0x2;
        /// `FAN_ATTRIB`.
        ATTRIB = 0x4;
        /// `FAN_CLOSE_WRITE`.
        CLOSE_WRITE = 0x8;
        /// `FAN_CLOSE_NOWRITE`.
        CLOSE_NO_WRITE = 0x10;
        /// `FAN_OPEN`.
        OPEN = 0x20;
        /// `FAN_MOVED_FROM`.
        MOVED_FROM = 0x40;
        /// `FAN_MOVED_TO`.
        MOVED_TO = 0x80;
        /// `FAN_CREATE`.
        CREATE = 0x100;
        /// `FAN_DELETE`.
        DELETE = 0x200;
        /// `FAN_DELETE_SELF`.
        DELETE_SELF = 0x400;
        /// `FAN_MOVE_SELF`.
        MOVE_SELF = 0x800;
        /// `FAN_OPEN_EXEC`.
        OPEN_EXEC = 0x1000;
        /// `FAN_Q_OVERFLOW` (an event only): events were lost; rescan.
        OVERFLOW = 0x4000;
        /// `FAN_FS_ERROR`.
        FS_ERROR = 0x8000;
        /// `FAN_EVENT_ON_CHILD`.
        ON_CHILD = 0x0800_0000;
        /// `FAN_RENAME`.
        RENAME = 0x1000_0000;
        /// `FAN_ONDIR`.
        ON_DIR = 0x4000_0000;
    }
}

/// `fanotify_init`: a new fanotify group; `events` are the flags of the descriptors its
/// events open (none with [`FanInit::REPORT_FID`]). [`Errno::Perm`] without the capability the
/// class or mark needs. Used by blaze-core's file-system watcher.
/// replaces libc::syscall(libc::SYS_fanotify_init) (as blaze_sys::linux::fanotify_init(FanInit, OpenFlags); returns Result<OwnedFd, Errno>, the site's -1 check becomes the Err)
/// replaces libc::SYS_fanotify_init
/// replaces libc::fanotify_init (blaze_sys::linux::fanotify_init(FanInit, OpenFlags) -> Result<OwnedFd, Errno>)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fanotify_init(flags: FanInit, events: OpenFlags) -> Result<OwnedFd, Errno> {
    // SAFETY: the call takes numbers and no memory.
    owned(unsafe { raw::syscall2(nr::FANOTIFY_INIT, flags.bits() as usize, events.bits() as usize) })
}

/// `fanotify_mark`: `path` (in `dir`; [`Path::EMPTY`] marks `dir` itself) marked, as `flags`
/// say, for `mask`. [`Errno::XDev`] or [`Errno::OpNotSupp`] on a file system that gives no
/// handles, which the watcher reports by name.
/// replaces libc::syscall(libc::SYS_fanotify_mark) (as blaze_sys::linux::fanotify_mark(fanotify: Fd, FanMark, FanMask, dir: Fd, Path; Path::EMPTY for a null name); returns Result<(), Errno>)
/// replaces libc::SYS_fanotify_mark
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fanotify_mark(fanotify: Fd, flags: FanMark, mask: FanMask, dir: Fd, path: Path<'_>) -> Result<(), Errno> {
    // `Path::EMPTY` is passed as a null pointer: fanotify marks `dir` itself only then.
    let name = if path.bytes().is_empty() { 0 } else { path.as_ptr() as usize };
    // SAFETY: `path` is a live NUL-terminated string (or null) the kernel reads and does not
    // keep; the mask is one 64-bit argument on both 64-bit targets.
    raw::check(unsafe {
        raw::syscall5(nr::FANOTIFY_MARK, arg(fanotify), flags.bits() as usize, mask.bits() as usize, arg(dir), name)
    })
    .map(|_| ())
}

/// `struct fanotify_event_metadata`: the head of every fanotify event.
/// replaces libc::fanotify_event_metadata (blaze_sys::linux::FanEventMetadata, `reserved` private; or walk a read with blaze_sys::linux::FanEvents)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FanEventMetadata {
    /// The whole event's length, its info records included.
    pub event_len: u32,
    /// `FANOTIFY_METADATA_VERSION` (3).
    pub version: u8,
    reserved: u8,
    /// This head's length: where the info records start.
    pub metadata_len: u16,
    pub mask: u64,
    /// The event's open descriptor, or `FAN_NOFD` (-1) with `REPORT_FID`.
    pub fd: i32,
    pub pid: i32,
}

/// `FANOTIFY_METADATA_VERSION`.
/// replaces libc::FANOTIFY_METADATA_VERSION (blaze_sys::linux::FAN_METADATA_VERSION, a u8)
pub const FAN_METADATA_VERSION: u8 = 3;

/// One event of a fanotify read: its head, and its info records (`struct
/// fanotify_event_info_header` and what follows) as bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FanEvent<'a> {
    pub mask: FanMask,
    /// The event's own descriptor, which the reader must close; -1 with `REPORT_FID`.
    pub fd: i32,
    pub pid: i32,
    pub info: &'a [u8],
}

/// The events of one fanotify read, in the kernel's order. A head of another version, or one
/// whose lengths do not hold, ends the walk, and [`FanEvents::refused`] says it did.
#[derive(Clone, Copy, Debug)]
pub struct FanEvents<'a> {
    left: &'a [u8],
    refused: bool,
}

impl<'a> FanEvents<'a> {
    /// The events in `read`, the bytes a [`super::read`] of a fanotify descriptor filled.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn new(read: &'a [u8]) -> FanEvents<'a> {
        FanEvents { left: read, refused: false }
    }

    /// Whether the walk stopped at an event of a version this layout is not
    /// ([`FAN_METADATA_VERSION`]) or whose lengths are shorter than its head: the reader stops
    /// with an error naming it, never skips it.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn refused(&self) -> bool {
        self.refused
    }
}

/// The size of [`FanEventMetadata`].
const FAN_HEAD: usize = 24;

impl<'a> Iterator for FanEvents<'a> {
    type Item = FanEvent<'a>;

    fn next(&mut self) -> Option<FanEvent<'a>> {
        let head = self.left.first_chunk::<FAN_HEAD>()?;
        let event_len = u32::from_ne_bytes(*head.first_chunk::<4>()?) as usize;
        let version = *head.get(4)?;
        let metadata_len = u16::from_ne_bytes(*head.get(6..8)?.first_chunk::<2>()?) as usize;
        if version != FAN_METADATA_VERSION || metadata_len < FAN_HEAD || event_len < metadata_len {
            self.refused = true;
            self.left = &[];
            return None;
        }
        let mask = u64::from_ne_bytes(*head.get(8..16)?.first_chunk::<8>()?);
        let fd = i32::from_ne_bytes(*head.get(16..20)?.first_chunk::<4>()?);
        let pid = i32::from_ne_bytes(*head.get(20..24)?.first_chunk::<4>()?);
        let event = self.left.get(..event_len)?;
        let info = event.get(metadata_len..)?;
        self.left = self.left.get(event_len..)?;
        Some(FanEvent { mask: FanMask::from_bits(mask), fd, pid, info })
    }
}
