//! A [`Driver`] on Linux's io_uring: one ring, shared with the kernel through memory, that the
//! thread which polls fills with requests and reaps completions from. The kernel takes the
//! requests on a thread of its own (`IORING_SETUP_SQPOLL`), so a busy driver submits with no
//! system call; an idle one lets that thread sleep and wakes it when it writes again.
//!
//! Reads, appends, syncs, writes at an offset, whole-file writes and renames are the ring's;
//! timers are a [`Wheel`] whose next deadline ends the ring's wait. A file is opened once,
//! kept open and registered in the ring's table of files, so requests name its slot. A whole
//! file's write is one linked chain (written, synced, renamed over the old, its directory
//! synced), and a rename is linked to its directories' syncs. A large aligned write at an
//! offset bypasses the page cache from a registered buffer. A truncation, which comes only in
//! recovery, is a call made where it is submitted.
//!
//! A spawned process is watched through the ring too: a poll of its pidfd for its exit, its
//! output pipes read and its input written by the ring. A listen's accepts (one multishot
//! request for all of them, each connection put straight into the table), a connect, a
//! connection's receives (one multishot request into a ring of provided buffers) and sends,
//! and a hangup's shutdown are the ring's; mapping a file is a call made where it is
//! submitted. A listing, which may take long, runs on a thread of its own, which wakes the ring
//! through an eventfd it reads.
//!
//! The ring is set up with the system calls themselves (`io_uring_setup`, `io_uring_enter`,
//! `io_uring_register` and `mmap`), the layouts as `linux/io_uring.h` has them. Linux 6.0 or
//! later: each feature's absence is an error naming it and the kernel that has it. Before 6.1
//! `statx` says no alignment for writes that bypass the cache, so none does.
//!
//! A request in flight is a slot of columns ([`ops`]): 32 bytes a completion reads, the buffer
//! it lends the kernel, and what only its submission and end read; its handle is the
//! `user_data` of its completions. The machinery that asks the kernel and steps completions is
//! in [`flight`]. A large send on a TCP connection goes from the asker's pages
//! (`IORING_OP_SEND_ZC`). docs/v2/plan/lanes/core/speed-uring.md has the costs per request and
//! how they compare with other runtimes.
//!
//! Why unsafe: the rings are memory shared with the kernel (`mmap` of the ring's file), read and
//! written through pointers at the offsets the kernel gives, and every request lends the kernel
//! a buffer or a name by address until it completes. Std's blocking calls are the safe version,
//! at one system call (and a thread blocked) per operation, where a ring takes a batch in one
//! `io_uring_enter` or none (`SQPOLL`). What comes back is checked before it names memory: a
//! completion's `user_data` is a handle, looked up in the slab by index and generation, never
//! a pointer; a provided buffer's id is checked against its group (`Provided::buffer`); and a
//! ring offset against its mapping (`Map::at`), in every build.

mod files;
mod flight;
mod ops;

use std::{
    cell::{RefCell, UnsafeCell},
    collections::VecDeque,
    ffi::CStr,
    fmt::Write as _,
    io,
    mem::ManuallyDrop,
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        unix::ffi::OsStrExt as _,
    },
    path::{Path, PathBuf},
    ptr,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering, fence},
    },
    time::{Duration, Instant},
};

use blaze_hash::HashMap;
use blaze_mem::{Handle, Slab, Tag as MemTag, scratch};
use blaze_radix as radix;

use self::ops::{Cold, Hot, Id, Kind};
use crate::{
    Command, Completion, Confine, Conn, Done, Driver, Effect, FileName, IoError, Names, RECV_MOST,
    Stamp, Tag,
    handoff::Claims,
    wheel::{self, Wheel},
};

const OP_FSYNC: u8 = 3;
const OP_WRITE_FIXED: u8 = 5;
const OP_POLL_ADD: u8 = 6;
const OP_ACCEPT: u8 = 13;
const OP_ASYNC_CANCEL: u8 = 14;
const OP_CONNECT: u8 = 16;
const OP_OPENAT: u8 = 18;
const OP_CLOSE: u8 = 19;
const OP_FILES_UPDATE: u8 = 20;
const OP_STATX: u8 = 21;
const OP_READ: u8 = 22;
const OP_WRITE: u8 = 23;
const OP_SEND: u8 = 26;
const OP_RECV: u8 = 27;
const OP_SHUTDOWN: u8 = 34;
const OP_RENAMEAT: u8 = 35;
const OP_UNLINKAT: u8 = 36;
const OP_MKDIRAT: u8 = 37;
const OP_LINKAT: u8 = 39;
const OP_SEND_ZC: u8 = 47;

const SETUP_SQPOLL: u32 = 1 << 1;
const SETUP_CQSIZE: u32 = 1 << 3;
const SETUP_SUBMIT_ALL: u32 = 1 << 7;
const SETUP_SINGLE_ISSUER: u32 = 1 << 12;
const FEAT_SINGLE_MMAP: u32 = 1 << 0;
const FEAT_NODROP: u32 = 1 << 1;
const FEAT_SQPOLL_NONFIXED: u32 = 1 << 7;
const FEAT_EXT_ARG: u32 = 1 << 8;
const ENTER_GETEVENTS: u32 = 1 << 0;
const ENTER_SQ_WAKEUP: u32 = 1 << 1;
const ENTER_EXT_ARG: u32 = 1 << 3;
/// In the submission ring's flags: the kernel's thread sleeps, to be woken by an enter.
const SQ_NEED_WAKEUP: u32 = 1 << 0;
/// In the submission ring's flags: completions wait in the kernel for room, flushed by an
/// enter that gets events.
const SQ_CQ_OVERFLOW: u32 = 1 << 1;
const OFF_SQ_RING: libc::off_t = 0;
const OFF_SQES: libc::off_t = 0x1000_0000;
/// How long the kernel's submission thread spins for work before it sleeps: an idle daemon
/// burns nothing, a busy one makes no system call to submit.
const SQ_THREAD_IDLE_MS: u32 = 50;

const SQE_FIXED_FILE: u8 = 1 << 0;
const SQE_IO_LINK: u8 = 1 << 2;
/// Run by the kernel's workers (io-wq) rather than tried inline on the submission thread.
const SQE_ASYNC: u8 = 1 << 4;
const SQE_BUFFER_SELECT: u8 = 1 << 5;
/// In an accept's `ioprio`: one request takes every connection until it is cancelled.
const ACCEPT_MULTISHOT: u16 = 1 << 0;
/// In a receive's `ioprio`: one request receives until it is cancelled or runs out of buffers.
const RECV_MULTISHOT: u16 = 1 << 1;
/// In an accept's file index: the kernel picks a free slot of the table's range for it.
const FILE_INDEX_ALLOC: u32 = u32::MAX;
/// In a completion's flags: it used the provided buffer named in the flags' upper half.
const CQE_F_BUFFER: u32 = 1 << 0;
/// In a completion's flags: the request goes on and will complete again.
const CQE_F_MORE: u32 = 1 << 1;
/// In a completion's flags: a zero-copy send's notification that the kernel reads its pages
/// no more.
const CQE_F_NOTIF: u32 = 1 << 3;
const CQE_BUFFER_SHIFT: u32 = 16;
/// In a cancel's flags: every request it matches, not the first, and every request at all.
const ASYNC_CANCEL_ALL: u32 = 1 << 0;
const ASYNC_CANCEL_ANY: u32 = 1 << 2;

const REGISTER_FILES2: libc::c_uint = 13;
const REGISTER_FILES_UPDATE2: libc::c_uint = 14;
const REGISTER_BUFFERS2: libc::c_uint = 15;
const REGISTER_PBUF_RING: libc::c_uint = 22;
const REGISTER_FILE_ALLOC_RANGE: libc::c_uint = 25;
const RSRC_REGISTER_SPARSE: u32 = 1 << 0;

/// The most slots of the table of registered files; fewer where `RLIMIT_NOFILE` is lower,
/// which the kernel holds the table to.
const TABLE: u32 = 1 << 16;
/// Registered buffers for writes that bypass the page cache, each a piece of such a write.
const DIRECT_BUFFERS: u16 = 4;
const DIRECT_PIECE: usize = 1 << 20;
/// A `WriteAt` of at least this many bytes, its offset and length aligned to its file
/// system's blocks, bypasses the page cache (`O_DIRECT`).
const DIRECT_LEAST: usize = 1 << 20;
/// The provided buffers a connection's receives take from, and the size of each.
const RECV_BUFFERS: u16 = 64;
const RECV_BUFFER: usize = 16 << 10;
const RECV_GROUP: u16 = 0;
/// Bytes received and not yet asked for, past which a connection stops receiving until they
/// are taken.
const INBOX: usize = 4 << 20;
/// A send on a TCP connection of at least this many bytes goes from the asker's pages
/// (`IORING_OP_SEND_ZC`, Linux 6.0): it saves the copy into the socket's buffers (~3 µs at
/// 10 GB/s for 32 KiB) and costs the pinning of its pages and a second completion, the
/// notification, of ~1 µs together; below ~10 KiB the copy is the cheaper (the kernel's
/// `msg_zerocopy.rst`). A Unix socket has no zero-copy sends, so its sends always copy.
const ZERO_COPY_LEAST: usize = 32 << 10;
/// `statx`'s mask bit of the alignment a write that bypasses the cache needs (Linux 6.1).
const STATX_DIOALIGN: u32 = 0x2000;
/// What a [`Stamp`] is made of, as `statx`'s mask asks for it (`linux/stat.h`; `libc` has these
/// only for some C libraries).
const STATX_STAMP: u32 = 0x0001 | 0x0002 | 0x0040 | 0x0080 | 0x0100 | 0x0200;
/// `statx`'s flag to answer from what the kernel has, never asking a network file system's
/// server first.
const AT_STATX_DONT_SYNC: i32 = 0x4000;
/// Names a stamper claims at once: enough to make the claim's atomic add and the reopen of a
/// chunk's first directory a percent or two of them (64 `statx`es are ~30 µs), few enough that
/// the cores' last chunks end within ~30 µs of each other, under 5% of 100 000 names' sweep on
/// 64 cores (docs/v2/plan/lanes/frontend/journal/linux.md, "Speed of light").
const STAMP_CHUNK: usize = 64;

/// Requests the ring holds at once, enough to keep an NVMe queue full; more wait in the
/// driver until there is room.
const ENTRIES: u32 = 4096;
/// Completions it holds unreaped: a multishot request may post many for one request.
const COMPLETIONS: u32 = 4 * ENTRIES;

/// What one read of a spawned process's pipe takes at most: a watch's read's size, so the two
/// share one pool of buffers.
const PIPE: usize = 64 << 10;
const _: () = assert!(PIPE == crate::watch::BUFFER);

/// The `user_data` of a request whose completion is nobody's: its index is above any
/// [`PENDING`] slot, so it names no request.
const NOBODY: u64 = u64::MAX;

/// The most requests in flight or waiting at once, files kept, connections, spawns and
/// watches: room reserved once, committed as it is used.
const PENDING: u32 = 1 << 20;
const KEPT: u32 = 1 << 20;
const CONNS: u32 = 1 << 20;
const SPAWNS: u32 = 1 << 16;
const WATCHES: u32 = 1 << 16;
const LISTENS: u32 = 1 << 16;
/// A request's slot is the `user_data`'s bits 3 to 31, beside its link.
const _: () = assert!(PENDING < 1 << 29);
/// Buffers, watches' tables and paths kept from what ended for what starts next.
const SPARES: usize = 64;

/// How long dropping the driver waits for the kernel to give back what it was lent.
const LETTING_GO: Duration = Duration::from_secs(5);

#[repr(C)]
#[derive(Default)]
struct SqOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default)]
struct CqOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: SqOffsets,
    cq_off: CqOffsets,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    op_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    splice_fd_in: i32,
    addr3: u64,
    pad: u64,
}

#[repr(C)]
struct Cqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

#[repr(C)]
struct GeteventsArg {
    sigmask: u64,
    sigmask_sz: u32,
    pad: u32,
    ts: u64,
}

#[repr(C)]
struct RsrcRegister {
    nr: u32,
    flags: u32,
    resv2: u64,
    data: u64,
    tags: u64,
}

#[repr(C)]
struct RsrcUpdate2 {
    offset: u32,
    resv: u32,
    data: u64,
    tags: u64,
    nr: u32,
    resv2: u32,
}

#[repr(C)]
struct BufReg {
    ring_addr: u64,
    ring_entries: u32,
    bgid: u16,
    flags: u16,
    resv: [u64; 3],
}

#[repr(C)]
struct FileIndexRange {
    off: u32,
    len: u32,
    resv: u64,
}

/// An entry of a provided buffer ring; the ring's tail is the `resv` of its first entry.
#[repr(C)]
struct ProvidedBuf {
    addr: u64,
    len: u32,
    bid: u16,
    resv: u16,
}

const _: () = assert!(size_of::<Params>() == 120);
const _: () = assert!(size_of::<Sqe>() == 64);
const _: () = assert!(size_of::<Cqe>() == 16);
const _: () = assert!(size_of::<RsrcRegister>() == 32);
const _: () = assert!(size_of::<RsrcUpdate2>() == 32);
const _: () = assert!(size_of::<BufReg>() == 40);
const _: () = assert!(size_of::<FileIndexRange>() == 16);
const _: () = assert!(size_of::<ProvidedBuf>() == 16);

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code, reason = "made by the kernel, read through a pointer")]
struct StatxTime {
    sec: i64,
    nsec: u32,
    _pad: i32,
}

/// `struct statx` of `linux/stat.h`, the same on every architecture and C library, with only
/// what a [`Stamp`] reads named.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code, reason = "made by the kernel, read through a pointer")]
struct Statx {
    mask: u32,
    /// `stx_blksize`, `stx_attributes`, `stx_nlink`, `stx_uid` and `stx_gid`.
    _head: [u32; 6],
    mode: u16,
    _pad: u16,
    ino: u64,
    size: u64,
    /// `stx_blocks` and `stx_attributes_mask`.
    _counts: [u64; 2],
    /// `stx_atime` and `stx_btime`.
    _times: [StatxTime; 2],
    ctime: StatxTime,
    mtime: StatxTime,
    _rest: [u64; 16],
}

const _: () = assert!(size_of::<Statx>() == 256);
const _: () = assert!(std::mem::offset_of!(Statx, mode) == 28);
const _: () = assert!(std::mem::offset_of!(Statx, ino) == 32);
const _: () = assert!(std::mem::offset_of!(Statx, ctime) == 96);
const _: () = assert!(std::mem::offset_of!(Statx, mtime) == 112);

impl Statx {
    /// The stamp [`crate::fs::stamp_of`] makes of a `stat` of the same file; unknown where the
    /// file system did not fill in all of it.
    fn stamp(&self) -> Stamp {
        if self.mask & STATX_STAMP != STATX_STAMP {
            return Stamp::UNKNOWN;
        }
        let ns = |t: &StatxTime| (t.sec as u64).wrapping_mul(1_000_000_000).wrapping_add(u64::from(t.nsec));
        Stamp {
            ino: self.ino,
            size: self.size,
            mtime_ns: ns(&self.mtime),
            ctime_ns: ns(&self.ctime),
            mode: u32::from(self.mode),
        }
    }
}

/// A name's stamp where its `statx` failed with `-res`: gone where nothing is there, unknown
/// otherwise, as [`crate::fs::stamps`] has it.
fn stamp_of_error(res: i32) -> Stamp {
    match -res {
        libc::ENOENT | libc::ENOTDIR => Stamp::GONE,
        _ => Stamp::UNKNOWN,
    }
}

/// `io_uring_register(2)` of `opcode` with `arg`, or an error naming what was asked and the
/// kernel that has it, where this one refuses it as unknown.
fn register<T>(ring: RawFd, opcode: libc::c_uint, arg: *const T, nr: libc::c_uint, what: &str, since: &str) -> io::Result<i64> {
    // SAFETY: `arg` points at what `opcode` reads (and the memory it names), alive for the call.
    let got = unsafe { libc::syscall(libc::SYS_io_uring_register, ring, opcode, arg, nr) };
    if got >= 0 {
        return Ok(got);
    }
    let error = io::Error::last_os_error();
    Err(match error.raw_os_error() {
        Some(libc::EINVAL | libc::EOPNOTSUPP) => io::Error::new(
            io::ErrorKind::Unsupported,
            // alloc: the error of a setup that failed.
            format!("io_uring {what}: {error}: Linux {since} or later has it, and this is Linux {}", crate::cgroup::kernel()),
        ),
        // alloc: the error of a setup that failed.
        _ => io::Error::new(error.kind(), format!("io_uring {what}: {error}")),
    })
}

/// An anonymous mapping of `len` bytes, page-aligned, unmapped when dropped.
fn anonymous(len: usize) -> io::Result<Map> {
    // SAFETY: a new private anonymous mapping: no file, no address hint, so it overlaps nothing
    // the program holds; the result is checked against `MAP_FAILED` below.
    let at = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(Map { at: at.cast(), len })
}

/// The table of registered files: requests name a slot of it (`IOSQE_FIXED_FILE`), so the
/// kernel takes no reference to a file per request. The first half is the driver's, for the
/// files it keeps open and the sockets it connects; the second is the kernel's, which accepts
/// put connections into directly.
struct Table {
    ring: Rc<OwnedFd>,
    /// Slots of the driver's half that are free.
    free: RefCell<Vec<u32>>,
    /// Slots of either half whose file was let go, to empty through the ring
    /// (`IORING_OP_CLOSE`); free again once that completes.
    closing: RefCell<Vec<u32>>,
    ours: u32,
}

impl Table {
    fn new(ring: &Rc<OwnedFd>) -> io::Result<Rc<Table>> {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `&mut limit` is a live, aligned `rlimit` the call writes and does not keep.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let size = u32::try_from(limit.rlim_cur).unwrap_or(u32::MAX).min(TABLE) & !1;
        if size < 2 {
            // alloc: the error of a setup that failed.
            return Err(io::Error::other(format!("RLIMIT_NOFILE is {}: no table of files fits", limit.rlim_cur)));
        }
        let fd = ring.as_raw_fd();
        let sparse = RsrcRegister { nr: size, flags: RSRC_REGISTER_SPARSE, resv2: 0, data: 0, tags: 0 };
        let nr = size_of::<RsrcRegister>() as libc::c_uint;
        register(fd, REGISTER_FILES2, &raw const sparse, nr, "a sparse table of registered files", "5.19")?;
        let ours = size / 2;
        let range = FileIndexRange { off: ours, len: size - ours, resv: 0 };
        register(fd, REGISTER_FILE_ALLOC_RANGE, &raw const range, 0, "a range of the file table for direct accepts", "6.0")?;
        // alloc: setup, once per driver: room for every slot, so a drop never grows it.
        let closing = RefCell::new(Vec::with_capacity(size as usize));
        // alloc: clone: setup, once per driver; `ring` is a reference count.
        Ok(Rc::new(Table { ring: ring.clone(), free: RefCell::new((0..ours).rev().collect()), closing, ours }))
    }

    /// Slot `slot` made `fd`'s file, or emptied with -1.
    fn set(&self, slot: u32, fd: RawFd) -> io::Result<()> {
        let fds = [fd];
        let update = RsrcUpdate2 { offset: slot, resv: 0, data: fds.as_ptr() as u64, tags: 0, nr: 1, resv2: 0 };
        let nr = size_of::<RsrcUpdate2>() as libc::c_uint;
        register(self.ring.as_raw_fd(), REGISTER_FILES_UPDATE2, &raw const update, nr, "updating the file table", "5.13")
            .map(|_| ())
    }

    /// `fd` put in a slot of the driver's half, if one is free.
    fn install(self: &Rc<Table>, fd: OwnedFd) -> io::Result<Fixed> {
        let slot = self.free.borrow_mut().pop();
        if let Some(slot) = slot
            && let Err(error) = self.set(slot, fd.as_raw_fd())
        {
            self.free.borrow_mut().push(slot);
            return Err(error);
        }
        // clone: a reference count.
        Ok(Fixed { fd: Some(fd), slot, table: Rc::clone(self) })
    }

    /// The connection an accept put in slot `slot` of the kernel's half.
    fn accepted(self: &Rc<Table>, slot: u32) -> Fixed {
        // clone: a reference count.
        Fixed { fd: None, slot: Some(slot), table: Rc::clone(self) }
    }
}

/// A file the ring's requests name: its slot of the [`Table`] where it has one, else its
/// descriptor. Kept by the one owner ([`Kept`], [`Connection`]) whose count of requests in
/// flight on it holds it, so its slot is emptied, and may be reused, only once no request can
/// name it any more.
struct Fixed {
    /// None for a connection an accept put straight into the table.
    fd: Option<OwnedFd>,
    /// None only where the driver's half of the table was full: the same file, through its
    /// descriptor, so the same bytes.
    slot: Option<u32>,
    table: Rc<Table>,
}

impl Fixed {
    /// The `fd` and flags of a request on it.
    fn target(&self) -> (RawFd, u8) {
        match (self.slot, &self.fd) {
            (Some(slot), _) => (slot as RawFd, SQE_FIXED_FILE),
            (None, Some(fd)) => (fd.as_raw_fd(), 0),
            (None, None) => unreachable!("a file of no slot has its descriptor"),
        }
    }

    fn fd(&self) -> RawFd {
        self.fd.as_ref().map_or(-1, |it| it.as_raw_fd())
    }
}

impl Fixed {
    /// A kept file's, until its open puts it in a slot.
    fn unopened(table: &Rc<Table>) -> Fixed {
        // clone: a reference count.
        Fixed { fd: None, slot: None, table: Rc::clone(table) }
    }
}

impl Drop for Fixed {
    /// Its slot emptied through the ring as the driver next writes to it: that drops the
    /// table's reference, so a socket accepted into it closes.
    fn drop(&mut self) {
        let Some(slot) = self.slot else { return };
        // alloc: none, its capacity is every slot's.
        self.table.closing.borrow_mut().push(slot);
    }
}

/// Registered buffers (`IORING_REGISTER_BUFFERS2`), page-aligned, for the pieces of writes
/// that bypass the page cache (`WRITE_FIXED`), which need memory aligned to the device's
/// blocks. Pinned once, so no request pins pages of its own; lent one per write by
/// [`ops::Lent`].
struct Registered {
    map: Map,
}

impl Registered {
    fn new(ring: RawFd) -> io::Result<Registered> {
        let map = anonymous(DIRECT_BUFFERS as usize * DIRECT_PIECE)?;
        let iovecs: Vec<libc::iovec> = (0..DIRECT_BUFFERS as usize)
            .map(|n| libc::iovec { iov_base: map.at.wrapping_add(n * DIRECT_PIECE).cast(), iov_len: DIRECT_PIECE })
            .collect(); // alloc: setup, once per driver.
        let buffers = RsrcRegister { nr: DIRECT_BUFFERS.into(), flags: 0, resv2: 0, data: iovecs.as_ptr() as u64, tags: 0 };
        let nr = size_of::<RsrcRegister>() as libc::c_uint;
        register(ring, REGISTER_BUFFERS2, &raw const buffers, nr, "registered buffers", "5.13").map_err(|error| {
            match error.raw_os_error() {
                Some(libc::ENOMEM) => io::Error::new(
                    error.kind(),
                    // alloc: the error of a setup that failed.
                    format!(
                        "{error}: pinning {} MiB of buffers needs RLIMIT_MEMLOCK of that much (ulimit -l) or CAP_IPC_LOCK",
                        DIRECT_BUFFERS as usize * DIRECT_PIECE >> 20
                    ),
                ),
                _ => error,
            }
        })?;
        Ok(Registered { map })
    }

    /// The start of buffer `index`, checked to be one of the [`DIRECT_BUFFERS`] in every build.
    fn at(&self, index: u16) -> *mut u8 {
        assert!(index < DIRECT_BUFFERS, "registered buffer {index} of {DIRECT_BUFFERS}");
        self.map.at.wrapping_add(index as usize * DIRECT_PIECE)
    }
}

/// The buffers a connection's multishot receive takes from (`IORING_REGISTER_PBUF_RING`): the
/// kernel picks one per completion, and each is given back once its bytes are copied out. The
/// buffers given back during a reap are published to the kernel together, one store of the
/// ring's tail for the batch ([`Provided::publish`]), so the cache line the kernel reads moves
/// once per reap, not once per buffer.
struct Provided {
    ring: Map,
    buffers: Map,
    tail: u16,
    /// The tail the kernel was last shown.
    published: u16,
}

impl Provided {
    fn new(fd: RawFd) -> io::Result<Provided> {
        let ring = anonymous(RECV_BUFFERS as usize * size_of::<ProvidedBuf>())?;
        let buffers = anonymous(RECV_BUFFERS as usize * RECV_BUFFER)?;
        let reg = BufReg {
            ring_addr: ring.at as u64,
            ring_entries: RECV_BUFFERS.into(),
            bgid: RECV_GROUP,
            flags: 0,
            resv: [0; 3],
        };
        register(fd, REGISTER_PBUF_RING, &raw const reg, 1, "a ring of provided buffers", "5.19")?;
        let mut provided = Provided { ring, buffers, tail: 0, published: 0 };
        for bid in 0..RECV_BUFFERS {
            provided.give(bid);
        }
        provided.publish();
        Ok(provided)
    }

    /// The start of buffer `bid`. `bid` comes from a completion's flags, the kernel's word, so
    /// it is checked against the group in every build (one compare per 16 KiB received): a
    /// larger id would hand out memory past `buffers`.
    fn buffer(&self, bid: u16) -> *mut u8 {
        assert!(bid < RECV_BUFFERS, "the kernel named provided buffer {bid} of a group of {RECV_BUFFERS}");
        self.buffers.at.wrapping_add(bid as usize * RECV_BUFFER)
    }

    /// The bytes the kernel put in buffer `bid`.
    fn bytes(&self, bid: u16, len: usize) -> &[u8] {
        let at = self.buffer(bid);
        // SAFETY: `at` starts buffer `bid` (checked to be in the group by `buffer`), and at most
        // `RECV_BUFFER` bytes of it are read (`len` is the kernel's count, clamped), all in
        // `buffers`, which the kernel wrote for this completion and does not touch again until
        // `give(bid)`, which needs `&mut self` and so cannot run while the slice borrows
        // `self`; if not, the slice aliases a kernel write.
        unsafe { std::slice::from_raw_parts(at, len.min(RECV_BUFFER)) }
    }

    /// Buffer `bid` the kernel's again once [`Provided::publish`] next runs: its entry written
    /// past the tail the kernel was shown, which it does not read until then.
    fn give(&mut self, bid: u16) {
        let mask = RECV_BUFFERS - 1;
        let entry: *mut ProvidedBuf = self.ring.at((self.tail & mask) as u32 * size_of::<ProvidedBuf>() as u32);
        let addr = self.buffer(bid) as u64;
        // SAFETY: `entry` is slot `tail & mask` of the ring `new` mapped for `RECV_BUFFERS`
        // entries; the kernel does not read it until `publish` stores a tail past it (at most
        // `RECV_BUFFERS` given back between publishes, as every buffer is given back once per
        // completion that names it); if it did, it would read a half-written entry.
        unsafe { (*entry).addr = addr };
        // SAFETY: as above.
        unsafe { (*entry).len = RECV_BUFFER as u32 };
        // SAFETY: as above.
        unsafe { (*entry).bid = bid };
        self.tail = self.tail.wrapping_add(1);
        debug_assert!(self.tail.wrapping_sub(self.published) <= RECV_BUFFERS);
    }

    /// Every buffer given back since the last publish shown to the kernel, with one store.
    fn publish(&mut self) {
        if self.tail == self.published {
            return;
        }
        self.published = self.tail;
        // SAFETY: the tail is the `resv` of the first entry, the kernel's to read; the release
        // publishes the entries `give` wrote before it.
        unsafe { (*self.ring.at::<AtomicU16>(14)).store(self.tail, Ordering::Release) };
    }

    /// Buffers given back and not yet shown to the kernel.
    #[cfg(test)]
    fn unpublished(&self) -> u16 {
        self.tail.wrapping_sub(self.published)
    }

    /// The tail the kernel reads.
    #[cfg(test)]
    fn shown(&self) -> u16 {
        // SAFETY: the tail word at the fixed `resv` offset 14 of the ring `new` mapped, which
        // only `publish` stores.
        unsafe { (*self.ring.at::<AtomicU16>(14)).load(Ordering::Acquire) }
    }
}

const _: () = assert!(RECV_BUFFERS.is_power_of_two());

/// What `statx` says of a descriptor's file: its device, and the alignment of offset and
/// length a write bypassing the page cache needs (0 where its file system has none).
#[cfg(test)]
fn direct_alignment(fd: RawFd) -> io::Result<(u64, u32)> {
    #[repr(C, align(8))]
    struct Statx([u8; 256]);
    let mut statx = Statx([0; 256]);
    // SAFETY: an open descriptor, an empty path with `AT_EMPTY_PATH`, and a buffer of the
    // size of `struct statx` to write.
    let got = unsafe {
        libc::syscall(libc::SYS_statx, fd, c"".as_ptr(), libc::AT_EMPTY_PATH, STATX_DIOALIGN, statx.0.as_mut_ptr())
    };
    if got != 0 {
        return Err(io::Error::last_os_error());
    }
    let word = |at: usize| u32::from_ne_bytes(statx.0[at..at + 4].try_into().expect("four bytes"));
    let device = (u64::from(word(136)) << 32) | u64::from(word(140));
    // `stx_dio_offset_align`, where the mask says it is filled in.
    let align = match word(0) & STATX_DIOALIGN {
        0 => 0,
        _ => word(156),
    };
    Ok((device, align))
}

/// A mapping of the ring's memory, unmapped when dropped.
struct Map {
    at: *mut u8,
    len: usize,
}

impl Map {
    fn new(fd: RawFd, len: usize, offset: libc::off_t) -> io::Result<Map> {
        // SAFETY: a shared mapping of the ring's file at the offset the kernel names for it.
        let at = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                offset,
            )
        };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Map { at: at.cast(), len })
    }

    /// The `T` at `offset`, which the kernel's offsets say is one. Checked against the mapping
    /// in every build (one compare, at setup or once per buffer given back), so a pointer it
    /// returns is always inside it; reading through one is the caller's `unsafe`.
    fn at<T>(&self, offset: u32) -> *mut T {
        assert!(
            offset as usize + size_of::<T>() <= self.len,
            "offset {offset} of a {}-byte value is past the {}-byte ring mapping",
            size_of::<T>(),
            self.len,
        );
        self.at.wrapping_add(offset as usize).cast()
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: the mapping `new` made, unmapped once.
        unsafe { libc::munmap(self.at.cast(), self.len) };
    }
}

/// The ring: its file, the mappings and where in them the kernel's counters are.
struct Ring {
    sq_head: *const AtomicU32,
    sq_tail: *const AtomicU32,
    sq_flags: *const AtomicU32,
    sq_mask: u32,
    sq_entries: u32,
    sqes: *mut Sqe,
    cq_head: *const AtomicU32,
    cq_tail: *const AtomicU32,
    cq_mask: u32,
    cqes: *const Cqe,
    /// The tail of the requests written, ahead of the kernel's until [`Ring::publish`] stores
    /// it there: one store, and one move of the cache line the kernel's thread polls, for every
    /// request of a step.
    sq_local: u32,
    /// The tail the kernel was last shown.
    sq_shown: u32,
    /// The kernel's head as last read: room is counted from it, read again only where the ring
    /// looks full.
    sq_seen: u32,
    /// Requests written to the ring since the kernel's thread was last told of them.
    unsubmitted: u32,
    // Dropped after the pointers into them are no longer used.
    _maps: Vec<Map>,
    /// Shared with the [`Table`], whose slots are emptied through it until the last goes.
    fd: Rc<OwnedFd>,
}

impl Ring {
    /// Submitted by a kernel thread of its own (`IORING_SETUP_SQPOLL`), which takes requests
    /// from the ring as they are written, with no system call, and sleeps after
    /// [`SQ_THREAD_IDLE_MS`] of nothing to do. For one issuing thread (a [`Uring`] is not
    /// `Send`; `IORING_SETUP_SINGLE_ISSUER`). With a thread of its own the kernel runs every
    /// completion's work there, so `IORING_SETUP_DEFER_TASKRUN`, `COOP_TASKRUN` and
    /// `TASKRUN_FLAG`, which say when the issuing thread runs it, mean nothing with SQPOLL, and
    /// the kernel refuses them with it.
    fn new(entries: u32, completions: u32) -> io::Result<Ring> {
        let mut params = Params {
            flags: SETUP_SQPOLL | SETUP_CQSIZE | SETUP_SUBMIT_ALL | SETUP_SINGLE_ISSUER,
            sq_thread_idle: SQ_THREAD_IDLE_MS,
            cq_entries: completions,
            ..Params::default()
        };
        // SAFETY: `params` is the `io_uring_params` the call reads and fills.
        let fd = unsafe {
            libc::syscall(libc::SYS_io_uring_setup, entries, &raw mut params as *mut libc::c_void)
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            let errno = error.raw_os_error().unwrap_or(0);
            let disabled = std::fs::read_to_string("/proc/sys/kernel/io_uring_disabled")
                .ok()
                .and_then(|it| it.trim().parse().ok());
            let kind = match errno {
                libc::ENOSYS | libc::EINVAL => io::ErrorKind::Unsupported,
                _ => error.kind(),
            };
            let why = setup_refused(errno, &error, disabled, &crate::cgroup::kernel());
            return Err(io::Error::new(kind, why));
        }
        // SAFETY: the call returned a file descriptor of its own.
        let fd = Rc::new(unsafe { OwnedFd::from_raw_fd(fd as RawFd) }); // alloc: setup, once per driver.
        let wanted = [
            (FEAT_SINGLE_MMAP, "IORING_FEAT_SINGLE_MMAP", "5.4"),
            (FEAT_NODROP, "IORING_FEAT_NODROP", "5.5"),
            (FEAT_SQPOLL_NONFIXED, "IORING_FEAT_SQPOLL_NONFIXED", "5.11"),
            (FEAT_EXT_ARG, "IORING_FEAT_EXT_ARG", "5.11"),
        ];
        if let Some((_, name, since)) = wanted.iter().find(|(bit, ..)| params.features & bit == 0) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                // alloc: the error of a setup that failed.
                format!("io_uring without {name}: Linux {since} or later has it, and this is Linux {}", crate::cgroup::kernel()),
            ));
        }
        let sq_len = params.sq_off.array as usize + params.sq_entries as usize * size_of::<u32>();
        let cq_len = params.cq_off.cqes as usize + params.cq_entries as usize * size_of::<Cqe>();
        let rings = Map::new(fd.as_raw_fd(), sq_len.max(cq_len), OFF_SQ_RING)?;
        let sqes =
            Map::new(fd.as_raw_fd(), params.sq_entries as usize * size_of::<Sqe>(), OFF_SQES)?;
        let (sq, cq) = (&params.sq_off, &params.cq_off);
        let array: *mut u32 = rings.at(sq.array);
        for index in 0..params.sq_entries {
            // SAFETY: `index < sq_entries` and the kernel's array at `sq.array` has
            // `sq_entries` `u32` slots in `rings`.
            let slot = unsafe { array.add(index as usize) };
            // SAFETY: `slot` is in the mapping, aligned (the kernel's offset) and not yet read by
            // the kernel, which waits for the first tail move.
            unsafe { slot.write(index) };
        }
        // SAFETY: the kernel put the submission ring's mask, a `u32` it never changes, at
        // `sq.ring_mask` of `rings`.
        let sq_mask = unsafe { *rings.at::<u32>(sq.ring_mask) };
        // SAFETY: as above, the completion ring's mask at `cq.ring_mask`.
        let cq_mask = unsafe { *rings.at::<u32>(cq.ring_mask) };
        Ok(Ring {
            sq_head: rings.at(sq.head),
            sq_tail: rings.at(sq.tail),
            sq_flags: rings.at(sq.flags),
            sq_mask,
            sq_entries: params.sq_entries,
            sqes: sqes.at(0),
            cq_head: rings.at(cq.head),
            cq_tail: rings.at(cq.tail),
            cq_mask,
            cqes: rings.at(cq.cqes),
            sq_local: 0,
            sq_shown: 0,
            sq_seen: 0,
            unsubmitted: 0,
            _maps: vec![rings, sqes], // alloc: setup, once per driver.
            fd,
        })
    }

    /// Writes `sqes` to the ring one after another, all or, where there is no room for all,
    /// none (a linked chain is links of requests next to each other). The kernel sees them at
    /// the next [`Ring::publish`].
    fn push(&mut self, sqes: &[Sqe]) -> bool {
        let wanted = sqes.len() as u32;
        if self.sq_local.wrapping_sub(self.sq_seen) + wanted > self.sq_entries {
            // SAFETY: `sq_head` points at the kernel's `AtomicU32` in `rings`, mapped while
            // `self` lives; the kernel moves it, so only atomic loads.
            self.sq_seen = unsafe { (*self.sq_head).load(Ordering::Acquire) };
            if self.sq_local.wrapping_sub(self.sq_seen) + wanted > self.sq_entries {
                return false;
            }
        }
        for (n, sqe) in sqes.iter().enumerate() {
            let at = self.sq_local.wrapping_add(n as u32) & self.sq_mask;
            // SAFETY: `at` is masked by `sq_mask`, so below `sq_entries`, the length of the SQE
            // array.
            let slot = unsafe { self.sqes.add(at as usize) };
            // SAFETY: slots from the kernel's tail on are not the kernel's until `publish`
            // stores a tail past them, and the room check above keeps them behind its head; if
            // it read one early it would see a torn request.
            unsafe { slot.write(*sqe) };
        }
        self.sq_local = self.sq_local.wrapping_add(wanted);
        self.unsubmitted += wanted;
        true
    }

    /// Shows the kernel every request written since it was last shown, with one store.
    fn publish(&mut self) {
        if self.sq_local == self.sq_shown {
            return;
        }
        self.sq_shown = self.sq_local;
        // SAFETY: `sq_tail` points at the kernel's `AtomicU32` in `rings`; only this thread
        // writes it, and the release publishes the entries `push` wrote before it.
        unsafe { (*self.sq_tail).store(self.sq_local, Ordering::Release) };
    }

    /// The submission ring's flags, read after every write of it is visible to the kernel's
    /// thread (the full fence pairs with the one the thread makes before it sleeps).
    fn flags(&self) -> u32 {
        fence(Ordering::SeqCst);
        // SAFETY: the kernel's counter at the offset it named.
        unsafe { (*self.sq_flags).load(Ordering::Relaxed) }
    }

    /// Wakes the kernel's thread where it sleeps with requests written since, and, with
    /// `wait`, waits up to it for one completion. Completions the kernel held for want of room
    /// are flushed to the ring by any enter that gets events.
    fn enter(&mut self, wait: Option<Duration>) -> io::Result<()> {
        self.publish();
        let flags = self.flags();
        // SAFETY: `sq_head` points at the kernel's `AtomicU32` in `rings`, mapped while `self`
        // lives; the kernel moves it, so only atomic loads.
        let head = unsafe { (*self.sq_head).load(Ordering::Acquire) };
        self.sq_seen = head;
        let queued = head != self.sq_local;
        let wake = queued && flags & SQ_NEED_WAKEUP != 0;
        self.unsubmitted = 0;
        let wait = wait.or((flags & SQ_CQ_OVERFLOW != 0).then_some(Duration::ZERO));
        if wait.is_none() && !wake {
            return Ok(());
        }
        let ts =
            wait.map(|it| Timespec { sec: it.as_secs() as i64, nsec: it.subsec_nanos() as i64 });
        let arg = GeteventsArg {
            sigmask: 0,
            sigmask_sz: 0,
            pad: 0,
            ts: ts.as_ref().map_or(0, |it| it as *const Timespec as u64),
        };
        let (min_complete, mut flags) = match wait {
            Some(_) => (1, ENTER_GETEVENTS | ENTER_EXT_ARG),
            None => (0, ENTER_EXT_ARG),
        };
        if wake {
            flags |= ENTER_SQ_WAKEUP;
        }
        loop {
            // SAFETY: `arg` and the timespec it points at outlive the call. With SQPOLL the
            // kernel's thread submits; the call only wakes it and waits.
            let entered = unsafe {
                libc::syscall(
                    libc::SYS_io_uring_enter,
                    self.fd.as_raw_fd(),
                    0,
                    min_complete,
                    flags,
                    &raw const arg,
                    size_of::<GeteventsArg>(),
                )
            };
            if entered >= 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                // The wait ended with nothing done.
                Some(libc::ETIME) => return Ok(()),
                // Completions to reap first.
                Some(libc::EBUSY | libc::EAGAIN) => return Ok(()),
                _ => return Err(error),
            }
        }
    }

    /// The completions posted and not yet reaped, from the first to one past the last: read
    /// each with [`Ring::cqe`], then give them all back with one [`Ring::consume`].
    fn completions(&self) -> (u32, u32) {
        // SAFETY: `cq_head` points at the kernel's `AtomicU32` in `rings`; only this thread
        // writes it.
        let head = unsafe { (*self.cq_head).load(Ordering::Relaxed) };
        // SAFETY: `cq_tail` as above; the kernel moves it, and the acquire pairs with its release
        // of the entries.
        let tail = unsafe { (*self.cq_tail).load(Ordering::Acquire) };
        (head, tail)
    }

    /// Completion `at` of [`Ring::completions`]: its `user_data`, its result and its flags
    /// ([`CQE_F_MORE`], [`CQE_F_BUFFER`] and the buffer's id, [`CQE_F_NOTIF`]).
    fn cqe(&self, at: u32) -> (u64, i32, u32) {
        // SAFETY: masked by `cq_mask`, so below the completion ring's `cq_entries`.
        let cqe = unsafe { self.cqes.add((at & self.cq_mask) as usize) };
        // SAFETY: entries from the head to the tail `completions` read are posted (its acquire)
        // and the kernel does not reuse one until `consume` stores a head past it; the copy is
        // made before that.
        let Cqe { user_data, res, flags } = unsafe { cqe.read() };
        (user_data, res, flags)
    }

    /// The completions before `head` the kernel's again, with one store for all of them: the
    /// kernel keeps what it posts meanwhile in its overflow list (`IORING_FEAT_NODROP`).
    fn consume(&mut self, head: u32) {
        // SAFETY: `cq_head` as in `completions`; the release orders the reads of the entries
        // before the kernel's reuse of them.
        unsafe { (*self.cq_head).store(head, Ordering::Release) };
    }

    /// Every completion the kernel has posted, in one pass.
    fn reap(&mut self, mut each: impl FnMut(u64, i32, u32)) {
        let (mut head, tail) = self.completions();
        if head == tail {
            return;
        }
        while head != tail {
            let (user_data, res, flags) = self.cqe(head);
            head = head.wrapping_add(1);
            each(user_data, res, flags);
        }
        self.consume(head);
    }
}

/// What blaze says when `io_uring_setup` fails with `errno`: what failed, the likely cause and
/// the fix. `disabled` is `/proc/sys/kernel/io_uring_disabled` (absent before Linux 6.6),
/// `kernel` the running kernel's release. On Linux the build runs on io_uring and on nothing
/// else, so this is where it stops.
fn setup_refused(errno: i32, error: &io::Error, disabled: Option<u8>, kernel: &str) -> String {
    let name = match errno {
        libc::ENOSYS => "ENOSYS",
        libc::EPERM => "EPERM",
        libc::EACCES => "EACCES",
        libc::EINVAL => "EINVAL",
        libc::ENOMEM => "ENOMEM",
        libc::EMFILE => "EMFILE",
        libc::ENFILE => "ENFILE",
        _ => "",
    };
    let seccomp = "a seccomp policy that refuses io_uring_setup, such as Docker's default profile \
                   (Docker 25 and later) or a systemd unit's SystemCallFilter";
    let allow = "run under a seccomp profile that allows io_uring_setup, io_uring_enter and \
                 io_uring_register (Docker: --security-opt seccomp=<that profile>)";
    // alloc: the error of a setup that failed, once; the rest writes into it.
    let mut out = String::with_capacity(512);
    out.push_str("blaze runs its build on io_uring on Linux and cannot start without it: io_uring_setup failed with ");
    _ = match name {
        "" => write!(out, "errno {errno} ({error}). Likely cause: "),
        name => write!(out, "{name} ({error}). Likely cause: "),
    };
    _ = match (errno, disabled) {
        (libc::EPERM | libc::EACCES, Some(2)) => write!(
            out,
            "the sysctl kernel.io_uring_disabled is 2, which disables io_uring for every process. \
             Fix: sysctl -w kernel.io_uring_disabled=0 as root (and set it in /etc/sysctl.d to keep it)"
        ),
        (libc::EPERM | libc::EACCES, Some(1)) => write!(
            out,
            "the sysctl kernel.io_uring_disabled is 1, which allows io_uring only to processes \
             in the group kernel.io_uring_group names or with CAP_SYS_ADMIN. \
             Fix: sysctl -w kernel.io_uring_disabled=0 as root, or add this user to the group \
             kernel.io_uring_group names"
        ),
        (libc::EPERM | libc::EACCES, _) => write!(out, "{seccomp}. Fix: {allow}"),
        (libc::ENOSYS, _) => write!(
            out,
            "this kernel (Linux {kernel}) has no io_uring: older than 5.1 or built without \
             CONFIG_IO_URING; or {seccomp}, answering ENOSYS. \
             Fix: run on Linux 6.0 or later built with io_uring, or {allow}"
        ),
        (libc::EINVAL, _) => write!(
            out,
            "this kernel (Linux {kernel}) lacks IORING_SETUP_SQPOLL with \
             IORING_SETUP_SINGLE_ISSUER, which Linux 6.0 and later have. \
             Fix: run on Linux 6.0 or later"
        ),
        (libc::ENOMEM, _) => write!(
            out,
            "the locked-memory limit (RLIMIT_MEMLOCK), which kernels before 5.12 charge a ring \
             to, is too small, or the machine is out of memory. \
             Fix: raise the limit (ulimit -l unlimited, or LimitMEMLOCK= for a systemd unit), or run \
             on Linux 6.0 or later"
        ),
        (libc::EMFILE | libc::ENFILE, _) => write!(
            out,
            "no file descriptor is left for the ring, in this process or the system. \
             Fix: raise the open-file limit (ulimit -n)"
        ),
        _ => write!(
            out,
            "an answer io_uring_setup is not known to give on Linux {kernel}. \
             Fix: check that the kernel is Linux 6.0 or later, that kernel.io_uring_disabled is 0, \
             and {allow}"
        ),
    };
    out.push('.');
    out
}

/// A watch's read and what it reads for, in [`Uring::watchings`], which is not freed while the
/// kernel may still write into `buffer` (see `Drop`).
struct Watching {
    tree: crate::watch::Tree,
    /// A buffer of [`Uring::buffers`].
    buffer: Vec<u8>,
    stopping: bool,
    /// Flushes waiting for this read, cancelled for them, to complete: the descriptor is
    /// drained then, while the ring's kernel thread holds no read of it.
    flushes: Vec<Tag>,
    /// The watched directory of an [`Effect::Journal`], whose batches are told as
    /// [`Done::Journaled`]; `None` for an [`Effect::Watch`].
    journal: Option<FileName>,
}

/// A file opened for reading and appending, kept open: under its name in [`Uring::names`]
/// while `named`, and kept while any request in flight uses it (its open among them). Opened
/// through the ring into its slot of the table, by which every request on it names it; until
/// `open`, they wait.
struct Kept {
    fixed: Fixed,
    users: u32,
    named: bool,
    open: bool,
    /// A request waiting for it makes it if missing.
    make: bool,
}

/// The bits of a `user_data` that are its link.
const LINK_MASK: u64 = 7;
const _: () = assert!(ops::LINKS as u64 <= LINK_MASK + 1);

/// The `user_data` of link `link` of the request `id`, a request of no chain its link 0: the
/// generation in the high half, the slot and the link in the low.
fn data(id: Id, link: usize) -> u64 {
    debug_assert!(id.index() < PENDING && (link as u64) <= LINK_MASK);
    u64::from(id.generation()) << 32 | u64::from(id.index()) << 3 | link as u64
}

/// The request and link of a `user_data` [`data`] made; any other names no request.
fn of_data(user_data: u64) -> (Id, usize) {
    let index = (user_data as u32) >> 3;
    (Handle::from_bits(user_data >> 32 << 32 | u64::from(index)), (user_data & LINK_MASK) as usize)
}

/// The handle a [`Conn`] is.
fn conn_of(conn: Conn) -> Handle<Connection> {
    let handle = Handle::from_bits(conn.0);
    // The slab takes an odd generation to mean its entry holds a value, so bits it never
    // issued (an even generation, which a free slot may have) become a handle past every slot.
    if handle.generation() % 2 == 1 { handle } else { Handle::from_bits(u64::MAX) }
}

const _: () = assert!(CONNS < u32::MAX, "u32::MAX is no connection's index");

/// The tag of a request that is no effect's own (a receive, a cancel).
const NOBODYS: Tag = Tag(u64::MAX);

/// Work done on threads of its own rather than through the ring (a listing, which may take
/// long): their completions, and the eventfd that wakes the ring when one is sent.
struct Offload {
    sender: crossbeam_channel::Sender<Completion>,
    receiver: crossbeam_channel::Receiver<Completion>,
    /// Shared with the threads, so a write of one that ends after the driver is dropped
    /// still reaches an open descriptor.
    wake: Arc<OwnedFd>,
    /// Work handed to threads whose completion has not been received.
    out: usize,
    /// The request reading `wake`, while there is one.
    reading: Option<Id>,
    /// What each read of `wake` reads into, never read itself: one for the driver's life, so
    /// a wait allocates nothing, written by the kernel through the cell's pointer alone. Not
    /// freed while the kernel may still write into it (see `Drop`).
    count: ManuallyDrop<Box<UnsafeCell<[u8; 8]>>>,
}

impl Offload {
    fn new() -> io::Result<Offload> {
        // SAFETY: no pointers.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let (sender, receiver) = crossbeam_channel::unbounded();
        // SAFETY: the call returned a descriptor of its own.
        let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) }); // alloc: setup, once per driver.
        // alloc: setup, once per driver.
        let count = ManuallyDrop::new(Box::new(UnsafeCell::new([0; 8])));
        Ok(Offload { sender, receiver, wake, out: 0, reading: None, count })
    }
}

/// What became of a request whose completion was reaped.
enum Step {
    /// The rest of it asked for again.
    Again,
    /// `done` told, and it asked for again: a watch's change, a listen's accept.
    Goes(Done),
    /// Kept as it is, not asked again: a chain with links still to complete, or a multishot
    /// request the kernel still holds.
    Held,
    /// It ended, with its effect's completion if it has one of its own.
    Ends(Option<Done>),
}

/// A spawn in flight: its process, until it has exited and both its pipes have ended.
struct Spawning {
    tag: Tag,
    pid: libc::pid_t,
    /// What a kill signals: the process itself, whatever its pid names by then.
    pidfd: OwnedFd,
    finish: crate::process::Finish,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Its requests in flight: its exit, its output pipes and its input.
    parts: usize,
    exited: bool,
    /// Why it completes failed, whatever it exits with.
    failed: Option<String>,
}

impl Spawning {
    /// Its completion, once its exit and both its pipes have ended: reaped, and made the
    /// [`crate::Exit`] its [`crate::process::Finish`] says.
    fn end(self) -> Done {
        let Spawning { pid, pidfd, finish, stdout, stderr, failed, .. } = self;
        // alloc: the error of a spawn that failed.
        let failed = failed.map(|why| format!("{}: {why}", finish.command.program.display()));
        // Reaps it, and removes what was made for it even where it failed whatever it says.
        let exited = finish.exited(pid, stdout, stderr);
        drop(pidfd);
        match (exited, failed) {
            (_, Some(why)) => Done::Failed(IoError(why)),
            // alloc: `Done::Exited`'s own box, one per spawn.
            (Ok(exit), None) => Done::Exited(Box::new(exit)),
            // alloc: the error of a spawn that failed.
            (Err(error), None) => Done::Failed(IoError(error.to_string())),
        }
    }
}

/// A connection a listen accepted or a connect made, in [`Uring::conns`]; its [`Conn`] is
/// its handle's bits, so one hung up and let go is told from the next in its slot.
struct Connection {
    socket: Fixed,
    /// Requests in flight on it (its receive, sends, its shutdown): it is let go once none is
    /// and it is hung up.
    users: usize,
    hung: bool,
    /// A TCP connection, whose large sends go from the asker's pages ([`ZERO_COPY_LEAST`]).
    tcp: bool,
    /// What its receive took and no `Recv` has yet, in order.
    inbox: VecDeque<u8>,
    /// The `Recv`s waiting for bytes, in the order they were submitted, with their most and
    /// the buffer each appends to.
    recvs: VecDeque<(Tag, usize, Vec<u8>)>,
    /// Its multishot receive, while the kernel holds one.
    receiving: Option<Id>,
    /// Its receive is being cancelled: the inbox holds [`INBOX`] bytes no `Recv` took.
    pausing: bool,
    /// How its receiving ended: the peer hung up (or reset it), or why it failed.
    ended: Option<Result<(), String>>,
}

/// A connect in flight, in [`Uring::connectings`], which is not freed while the kernel may
/// still read `at` (see `Drop`): its socket, given to the connection it makes.
struct Connecting {
    socket: Option<Fixed>,
    at: crate::net::SockAddr,
    tcp: bool,
}

/// An [`Effect::Stamp`] on the [`Stampers`]: its names claimed [`STAMP_CHUNK`] at a time, each
/// chunk's stamps written into `out` by the stamper that claimed it. The stamper that finishes
/// the last chunk sends the completion and wakes the ring.
struct Job {
    tag: Tag,
    root: Arc<Path>,
    /// The root, opened by the first stamper to claim a chunk (never the ring's thread), or
    /// why it could not be: the sweep then fails, naming it.
    dir: std::sync::OnceLock<Result<OwnedFd, i32>>,
    names: Arc<Names>,
    /// The chunks of `names`, [`STAMP_CHUNK`] each, claimed and finished by the stampers.
    claims: Claims,
    /// One stamp per name, written through `base`.
    out: UnsafeCell<Vec<Stamp>>,
    base: *mut Stamp,
    sender: crossbeam_channel::Sender<Completion>,
    wake: Arc<OwnedFd>,
}

// SAFETY: `out` is written through `base` at disjoint chunks, each only by the one thread whose
// `Claims::claim` returned it, and taken whole only by the thread whose `Claims::finish` counted
// the last chunk, which orders every writer's chunk before it (`Job::run`; the protocol is
// model-checked in `handoff`). Every other field is `Sync`.
unsafe impl Sync for Job {}
// SAFETY: as above; `base` points into `out`'s heap buffer, which no move of the job moves.
unsafe impl Send for Job {}

impl Job {
    fn new(tag: Tag, root: Arc<Path>, names: Arc<Names>, offload: &Offload) -> Job {
        // alloc: the sweep's result, one per sweep, handed to its asker.
        let mut out = vec![Stamp::UNKNOWN; names.len()];
        let base = out.as_mut_ptr();
        Job {
            tag,
            root,
            dir: std::sync::OnceLock::new(),
            // One at least, so a sweep of no names still opens the root.
            claims: Claims::new(names.len().div_ceil(STAMP_CHUNK).max(1)),
            names,
            out: UnsafeCell::new(out),
            base,
            sender: offload.sender.clone(), // clone: a channel's handle, one per sweep.
            wake: Arc::clone(&offload.wake),
        }
    }

    /// Claims chunks until none is left, stamping each; whoever finishes the last completes
    /// the job. `parent` is the caller's buffer for a directory's name.
    fn run(&self, parent: &mut Vec<u8>) {
        while let Some(chunk) = self.claims.claim() {
            let from = (chunk * STAMP_CHUNK).min(self.names.len());
            let to = (from + STAMP_CHUNK).min(self.names.len());
            let dir = self.dir.get_or_init(|| open_dir(&self.root).map_err(|it| it.raw_os_error().unwrap_or(libc::EIO)));
            if let Ok(dir) = dir {
                stamp_names(dir.as_raw_fd(), &self.names, from..to, parent, |at, stamp| {
                    assert!((from..to).contains(&at), "stamp {at} outside the chunk {from}..{to} claimed");
                    let slot = self.base.wrapping_add(at);
                    // SAFETY: `at` is in `from..to` (checked above), below `names.len()`, the
                    // length of `out`'s buffer, and in the chunk this thread alone claimed, so
                    // no other thread reads or writes `slot` until the last finish.
                    unsafe { slot.write(stamp) };
                });
            }
            if self.claims.finish() {
                // SAFETY: every chunk is finished and ordered before this (`Claims::finish`),
                // and no thread writes or reads `out` after: every claim is taken.
                let out = std::mem::take(unsafe { &mut *self.out.get() });
                let done = match dir {
                    Ok(_) => Done::Stamped(out),
                    // alloc: the error of a sweep that failed.
                    Err(errno) => Done::Failed(IoError(format!(
                        "{}: {}",
                        self.root.display(),
                        io::Error::from_raw_os_error(*errno)
                    ))),
                };
                if self.sender.send(Completion { tag: self.tag, done }).is_ok() {
                    wake_ring(&self.wake);
                }
            }
        }
    }
}

/// Threads, one per core, that stamp [`Effect::Stamp`]'s names with `statx` calls of their own.
/// The ring would prepare each `IORING_OP_STATX` on its one submission thread and run it on a
/// worker after, two handoffs per name that take longer than the `statx` itself
/// (docs/v2/plan/lanes/frontend/journal/linux.md, "Speed of light"). A job reaches stamper 0,
/// and stamper `i` hands it on to `2i + 1` and `2i + 2` before it starts, as far as the job has
/// chunks for: every core is at work after log2 of them wake-ups, none from the ring's thread
/// but the first. [`Effect::Entries`]' listings go to one stamper each, in turn, where a thread
/// of their own would cost as much to start as a listing of a hundred names.
struct Stampers {
    inboxes: Arc<[crossbeam_channel::Sender<ToStamp>]>,
    /// The stamper the next listing goes to.
    next: std::cell::Cell<usize>,
}

/// What a stamper is sent.
enum ToStamp {
    Sweep(Arc<Job>),
    /// A listing, for the stamper it is sent to alone.
    List(List),
    End,
}

/// An [`Effect::Entries`] on a stamper.
struct List {
    tag: Tag,
    root: Arc<Path>,
    dir: FileName,
    sender: crossbeam_channel::Sender<Completion>,
    wake: Arc<OwnedFd>,
}

impl List {
    fn run(self) {
        let done = match crate::fs::entries(&self.root.join(self.dir.as_str()), &self.dir) {
            Ok(entries) => Done::Entries(entries),
            // alloc: the error of a listing that failed.
            Err(error) => Done::Failed(IoError(error.to_string())),
        };
        if self.sender.send(Completion { tag: self.tag, done }).is_ok() {
            wake_ring(&self.wake);
        }
    }
}

impl Stampers {
    fn new() -> io::Result<Stampers> {
        let count = std::thread::available_parallelism().map_or(1, |it| it.get());
        let (senders, receivers): (Vec<crossbeam_channel::Sender<ToStamp>>, Vec<crossbeam_channel::Receiver<ToStamp>>) =
            (0..count).map(|_| crossbeam_channel::unbounded()).unzip();
        let stampers = Stampers { inboxes: senders.into(), next: std::cell::Cell::new(0) };
        for (me, inbox) in receivers.into_iter().enumerate() {
            let inboxes = Arc::clone(&stampers.inboxes);
            // Dropping `stampers` on a failure ends those already started.
            std::thread::Builder::new()
                .name(format!("blaze-stamp-{me}")) // alloc: setup, once per stamper.
                .spawn(move || stamper(me, &inbox, &inboxes))?;
        }
        Ok(stampers)
    }

    fn start(&self, job: Arc<Job>) {
        _ = self.inboxes[0].send(ToStamp::Sweep(job));
    }

    fn list(&self, list: List) {
        let at = self.next.get();
        self.next.set((at + 1) % self.inboxes.len());
        _ = self.inboxes[at].send(ToStamp::List(list));
    }
}

impl Drop for Stampers {
    /// Each stamper ends once it has passed the end on, after the work sent before it.
    fn drop(&mut self) {
        _ = self.inboxes[0].send(ToStamp::End);
    }
}

fn stamper(me: usize, inbox: &crossbeam_channel::Receiver<ToStamp>, inboxes: &[crossbeam_channel::Sender<ToStamp>]) {
    // alloc: one buffer per stamper, reused for every directory of every job.
    let mut parent = Vec::with_capacity(libc::PATH_MAX as usize);
    while let Ok(work) = inbox.recv() {
        let wanted = match &work {
            ToStamp::Sweep(job) => job.claims.chunks(),
            ToStamp::List(_) => 0,
            ToStamp::End => usize::MAX,
        };
        for next in [2 * me + 1, 2 * me + 2] {
            if next < wanted
                && let Some(it) = inboxes.get(next)
            {
                _ = it.send(match &work {
                    ToStamp::Sweep(job) => ToStamp::Sweep(Arc::clone(job)),
                    _ => ToStamp::End,
                });
            }
        }
        match work {
            ToStamp::Sweep(job) => job.run(&mut parent),
            ToStamp::List(list) => list.run(),
            ToStamp::End => return,
        }
    }
}

/// The stamps of `names[range]`, each handed to `put` with its index. Each is a `statx` of the
/// name's last part relative to its directory, opened once for each run of names in the same
/// one with `O_PATH`, which needs only the search permission a walk of the whole name would.
/// A directory that cannot be opened gives each of its names the stamp a `statx` of the whole
/// name would: gone for `ENOENT` and `ENOTDIR`, unknown otherwise.
fn stamp_names(
    root: RawFd,
    names: &Names,
    range: std::ops::Range<usize>,
    parent: &mut Vec<u8>,
    mut put: impl FnMut(usize, Stamp),
) {
    // The directory open, by its name and a NUL; `parent` empty before the first.
    parent.clear();
    let mut dir: Result<Option<OwnedFd>, Stamp> = Ok(None);
    for at in range {
        let name = names.with_nul(at);
        let (up, leaf) = match name.iter().rposition(|&it| it == b'/') {
            Some(slash) => (&name[..slash], &name[slash + 1..]),
            None => (&name[..0], name),
        };
        if parent.is_empty() || parent[..parent.len() - 1] != *up {
            parent.clear();
            parent.extend_from_slice(up);
            parent.push(0);
            dir = match up.is_empty() {
                true => Ok(None),
                false => open_path(root, parent).map(Some).map_err(stamp_of_error),
            };
        }
        let stamp = match &dir {
            Ok(None) => statx_at(root, leaf),
            Ok(Some(fd)) => statx_at(fd.as_raw_fd(), leaf),
            Err(stamp) => *stamp,
        };
        put(at, stamp);
    }
}

/// The last error of a call of this thread's, as a ring's result is: negative.
fn errno() -> i32 {
    -io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
}

/// The directory `name` (with its NUL) under `dir`, opened only to name it in calls relative
/// to it; the error as a negative errno.
fn open_path(dir: RawFd, name: &[u8]) -> Result<OwnedFd, i32> {
    debug_assert_eq!(name.last(), Some(&0));
    // SAFETY: an open directory and a NUL-terminated name; the result is a descriptor of its
    // own or -1.
    let fd = unsafe { libc::openat(dir, name.as_ptr().cast(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(errno());
    }
    // SAFETY: the call returned a descriptor of its own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The stamp of `leaf` (with its NUL) under `dir`, not followed through a symlink.
fn statx_at(dir: RawFd, leaf: &[u8]) -> Stamp {
    debug_assert_eq!(leaf.last(), Some(&0));
    let mut statx = std::mem::MaybeUninit::<Statx>::uninit();
    // SAFETY: an open directory, a NUL-terminated name, and room for a `struct statx`.
    let got = unsafe {
        libc::syscall(
            libc::SYS_statx,
            dir,
            leaf.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW | AT_STATX_DONT_SYNC,
            STATX_STAMP,
            statx.as_mut_ptr(),
        )
    };
    if got != 0 {
        return stamp_of_error(errno());
    }
    // SAFETY: the call succeeded, so it filled the `statx`.
    unsafe { statx.assume_init_ref() }.stamp()
}

/// Tells the ring's wait that work off the ring has sent its completion.
fn wake_ring(wake: &OwnedFd) {
    let one = 1u64.to_ne_bytes();
    // SAFETY: eight bytes, which an eventfd write takes, of an open eventfd.
    unsafe { libc::write(wake.as_raw_fd(), one.as_ptr().cast(), one.len()) };
}

pub struct Uring {
    start: Instant,
    /// Shared with the work done off the ring, so handing it out is a count, not a copy.
    root: Arc<Path>,
    ring: Ring,
    table: Rc<Table>,
    registered: Registered,
    /// Not freed while the kernel may still write into it (see `Drop`).
    provided: ManuallyDrop<Provided>,
    /// The registered buffers' lending to writes that bypass the page cache.
    lent: ops::Lent,
    /// Each file opened, for reading and appending, and the name each is kept under.
    kept: Slab<Kept>,
    names: HashMap<FileName, Handle<Kept>>,
    /// Every request in flight or waiting, by the handle its `user_data` carries ([`data`]).
    /// Not freed while the kernel may still read or write what a request holds (see `Drop`).
    ops: ManuallyDrop<ops::Ops>,
    /// The names of whole-file writes and renames in flight, which the kernel reads; not freed
    /// while it may (see `Drop`).
    paths: ManuallyDrop<ops::Paths>,
    /// Requests for which the ring had no room.
    waiting: VecDeque<Id>,
    /// Requests on kept files whose open is in flight, in the order they were asked.
    on_open: Vec<Id>,
    /// Requests waiting for slots of the table for their opens, in the order they asked.
    slot_waiting: VecDeque<Id>,
    /// Reads of files no request keeps that hold a descriptor ([`files::LOADS`]), and those
    /// waiting for room among them, in the order they asked.
    loading: usize,
    load_waiting: VecDeque<Id>,
    /// Closes of slots in flight.
    closes: usize,
    /// Slots of the table requests on paths hold.
    slots_held: usize,
    /// Writes that bypassed the page cache, ended.
    direct_writes: u64,
    /// Sends that went from the asker's pages (`IORING_OP_SEND_ZC`), ended.
    zero_copy_sends: u64,
    /// Completions to hand out at the next poll, in order.
    ready: Vec<Completion>,
    /// The request reading each watch's events, by the watch's tag.
    watches: HashMap<Tag, Id>,
    /// What each watch's read reads for. Not freed while the kernel may still write into a
    /// buffer of one (see `Drop`).
    watchings: ManuallyDrop<Slab<Watching>>,
    /// The request accepting each listen's connections, by the listen's tag.
    listens: HashMap<Tag, Id>,
    /// What each listen accepts on, by the handle in its accept's [`ops::Hot::of`].
    listeners: Slab<crate::net::Listener>,
    /// Each connect in flight, by the handle in its request's [`ops::Hot::of`]. Not freed while
    /// the kernel may still read an address of one (see `Drop`).
    connectings: ManuallyDrop<Slab<Connecting>>,
    spawns: Slab<Spawning>,
    /// Each spawn in flight by its tag, for a kill.
    spawning: HashMap<Tag, Handle<Spawning>>,
    conns: Slab<Connection>,
    /// By ticks since `start`; a wait of the ring ends at the next one due.
    timers: Wheel<Tag>,
    // dyn: the embedder's sandbox, called once per spawn, which makes a process anyway.
    confine: Option<Arc<dyn Confine>>,
    /// Where each sandboxed spawn's own directory is made: `root/.blaze-spawn` unless
    /// [`Uring::scratch`] says otherwise.
    spawn_dirs: PathBuf,
    /// Buffers of [`PIPE`] bytes, a watch's or a spawn's pipe's, kept from what ended for
    /// what starts next.
    buffers: Vec<Vec<u8>>,
    /// The tables of watches that ended, with their lists of flushes, for the next watch.
    trees: Vec<(crate::watch::Spare, Vec<Tag>)>,
    /// Made for the first [`Effect::Stamp`].
    stampers: Option<Stampers>,
    offload: Offload,
    in_flight: usize,
}

/// A temporary's number, counted from the top half so that it never meets one of
/// [`crate::fs::temporary_next_to`]'s, which count from 0.
static TEMPORARY: AtomicU64 = AtomicU64::new(1 << 63);

impl Uring {
    /// A driver whose files are under `root`, or why this kernel has no ring for it.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Uring> {
        let ring = Ring::new(ENTRIES, COMPLETIONS)?;
        let table = Table::new(&ring.fd)?;
        let registered = Registered::new(ring.fd.as_raw_fd())?;
        let provided = ManuallyDrop::new(Provided::new(ring.fd.as_raw_fd())?);
        fn slab<T>(max: u32) -> io::Result<Slab<T>> {
            Slab::new(max, MemTag::Io).map_err(io::Error::other)
        }
        let root: Arc<Path> = Arc::from(root.into());
        let spawn_dirs = root.join(".blaze-spawn"); // alloc: setup, once per driver.
        Ok(Uring {
            start: Instant::now(),
            root,
            ring,
            table,
            registered,
            provided,
            lent: ops::Lent::new(DIRECT_BUFFERS),
            kept: slab(KEPT)?,
            names: HashMap::default(),
            ops: ManuallyDrop::new(ops::Ops::new(PENDING).map_err(io::Error::other)?),
            paths: ManuallyDrop::new(ops::Paths::new().map_err(io::Error::other)?),
            // alloc: setup, once per driver: room for as many as the ring holds.
            waiting: VecDeque::with_capacity(ENTRIES as usize),
            // alloc: setup, once per driver: room for as many as the ring holds, grown only
            // past that.
            on_open: Vec::with_capacity(ENTRIES as usize),
            slot_waiting: VecDeque::with_capacity(ENTRIES as usize), // alloc: setup, as above.
            loading: 0,
            load_waiting: VecDeque::with_capacity(ENTRIES as usize), // alloc: setup, as above.
            closes: 0,
            slots_held: 0,
            direct_writes: 0,
            zero_copy_sends: 0,
            ready: Vec::new(), // alloc: empty, nothing allocated; reused after.
            watches: HashMap::default(),
            watchings: ManuallyDrop::new(slab(WATCHES)?),
            listens: HashMap::default(),
            listeners: slab(LISTENS)?,
            connectings: ManuallyDrop::new(slab(CONNS)?),
            spawns: slab(SPAWNS)?,
            spawning: HashMap::default(),
            conns: slab(CONNS)?,
            timers: Wheel::new(),
            confine: None,
            spawn_dirs,
            buffers: Vec::with_capacity(SPARES), // alloc: setup, once per driver.
            trees: Vec::with_capacity(SPARES),   // alloc: setup, once per driver.
            stampers: None,
            offload: Offload::new()?,
            in_flight: 0,
        })
    }

    /// The driver, making the sandbox of each spawn whose [`crate::Sandbox`] is on with
    /// `confine`.
    // dyn: the embedder's sandbox, set once.
    pub fn confined(mut self, confine: Arc<dyn Confine>) -> Uring {
        self.confine = Some(confine);
        self
    }

    /// The driver, making each sandboxed spawn's own directory under `dir`, made here.
    pub fn scratch(mut self, dir: impl Into<PathBuf>) -> io::Result<Uring> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        self.spawn_dirs = dir;
        Ok(self)
    }

    /// Every timer due, fired in the order they were due.
    fn fire_timers(&mut self) {
        let ready = &mut self.ready;
        self.timers.advance(wheel::ticks(self.start.elapsed()), |_, tag| {
            ready.push(Completion { tag, done: Done::Fired });
        });
    }

    /// Forgets the descriptor kept for `name`: the name now is another file.
    fn forget(&mut self, name: &FileName) {
        forget(&mut self.names, &mut self.kept, name);
    }

    /// The file kept under `name`, its open through the ring started where none is; `make`
    /// makes it if missing, with its directories. Requests on it wait for the open
    /// ([`Uring::prepared`]); where the driver's half of the table is full, the files kept
    /// are let go (each request in flight holds its own until it ends) to make room.
    fn file(&mut self, name: &FileName, make: bool) -> io::Result<Handle<Kept>> {
        if let Some(&file) = self.names.get(name) {
            let kept = self.kept.get_mut(file).expect("named files are kept");
            kept.make |= make && !kept.open;
            return Ok(file);
        }
        let fixed = Fixed::unopened(&self.table);
        // Held by its open until it ends.
        let file = self.kept.insert(Kept { fixed, users: 1, named: true, open: false, make });
        // clone: the name once per file opened, as the key it is kept under.
        self.names.insert(name.clone(), file);
        if let Err(error) = self.open(file, name, make) {
            self.names.remove(name);
            self.kept.remove(file);
            return Err(error);
        }
        Ok(file)
    }

    /// `file` held by one more request, until [`unkeep`].
    fn hold(&mut self, file: Handle<Kept>) -> Handle<Kept> {
        self.kept.get_mut(file).expect("a file just kept").users += 1;
        file
    }

    /// A buffer of [`PIPE`] bytes, kept from one that ended where there is one.
    fn buffer(&mut self) -> Vec<u8> {
        // alloc: a pipe's or a watch's buffer, where none that ended is kept for it.
        self.buffers.pop().unwrap_or_else(|| vec![0; PIPE])
    }

    /// `buffer` kept for the next that needs one, where there is room.
    fn give_back(&mut self, buffer: Vec<u8>) {
        if self.buffers.len() < SPARES && buffer.len() == PIPE {
            self.buffers.push(buffer);
        }
    }

    /// `effect` done by [`crate::threads::run`] on a thread of its own, its completion taken
    /// in at a poll once the thread wakes the ring.
    fn off_ring(&mut self, effect: Effect) -> io::Result<()> {
        let tag = effect.tag();
        self.off_ring_with(tag, move |root| crate::threads::run(root, effect))
    }

    /// `work`, given the root, done on a thread of its own for the effect `tag`, as
    /// [`Uring::off_ring`] does an effect.
    fn off_ring_with(
        &mut self,
        tag: Tag,
        work: impl FnOnce(&Path) -> io::Result<Done> + Send + 'static,
    ) -> io::Result<()> {
        let root = Arc::clone(&self.root);
        // clone: a channel's handle, for the thread to answer on.
        let sender = self.offload.sender.clone();
        let wake = Arc::clone(&self.offload.wake);
        std::thread::Builder::new().name("blaze-uring-off".into()).spawn(move || {
            // alloc: the error of work that failed.
            let done = work(&root).unwrap_or_else(|error| Done::Failed(IoError(error.to_string())));
            // A driver dropped meanwhile takes nothing.
            // The descriptor is kept open by this thread's `Arc`.
            if sender.send(Completion { tag, done }).is_ok() {
                wake_ring(&wake);
            }
        })?;
        self.offloaded(tag);
        Ok(())
    }

    /// The [`Stampers`], started for the first sweep or listing.
    fn make_stampers(&mut self) -> io::Result<()> {
        if self.stampers.is_none() {
            self.stampers = Some(Stampers::new()?);
        }
        Ok(())
    }

    /// Starts the listing of [`Effect::Entries`] `tag` of `dir` on a stamper.
    fn list(&mut self, tag: Tag, dir: FileName) -> io::Result<()> {
        self.make_stampers()?;
        let list = List {
            tag,
            root: Arc::clone(&self.root),
            dir,
            // clone: a channel's handle, one per listing.
            sender: self.offload.sender.clone(),
            wake: Arc::clone(&self.offload.wake),
        };
        self.offloaded(tag);
        self.stampers.as_ref().expect("made above").list(list);
        Ok(())
    }

    /// Starts the sweep of [`Effect::Stamp`] `tag` over `names` on the [`Stampers`], made for
    /// the first. Fails here only where the stampers cannot start; where the root cannot be
    /// opened, its completion says so.
    fn sweep(&mut self, tag: Tag, names: Arc<Names>) -> io::Result<()> {
        self.make_stampers()?;
        // alloc: one per sweep, shared by the stampers.
        let job = Arc::new(Job::new(tag, Arc::clone(&self.root), names, &self.offload));
        self.offloaded(tag);
        self.stampers.as_ref().expect("made above").start(job);
        Ok(())
    }

    /// The completions the threads off the ring have sent.
    fn take_offloaded(&mut self) {
        while let Ok(completion) = self.offload.receiver.try_recv() {
            self.offload.out -= 1;
            self.ready.push(completion);
        }
    }

    /// An effect that fails as it is submitted, with no request of its own to end.
    fn refuse(&mut self, tag: Tag, why: String) {
        self.in_flight += 1;
        self.ready.push(Completion { tag, done: Done::Failed(IoError(why)) });
    }

    /// A request of `spawn` ended; once all have, it completes.
    fn part_ended(&mut self, spawn: Handle<Spawning>) {
        let Some(it) = self.spawns.get_mut(spawn) else { return };
        it.parts -= 1;
        if it.parts == 0 {
            let it = self.spawns.remove(spawn).expect("in flight");
            self.spawning.remove(&it.tag);
            let tag = it.tag;
            self.ready.push(Completion { tag, done: it.end() });
        }
    }

    /// `spawn` completes failed, saying `why`; its process is killed so that it ends.
    fn spoil(&mut self, spawn: Handle<Spawning>, why: String) {
        let Some(spawn) = self.spawns.get_mut(spawn) else { return };
        spawn.failed.get_or_insert(why);
        if !spawn.exited {
            _ = kill(spawn.pidfd.as_raw_fd());
        }
    }

    fn kill(&mut self, tag: Tag) {
        let Some(&handle) = self.spawning.get(&tag) else { return };
        let spawn = self.spawns.get(handle).expect("a spawn in flight");
        if spawn.exited {
            return;
        }
        if let Err(error) = kill(spawn.pidfd.as_raw_fd()) {
            self.spoil(handle, format!("killing it: {error}")); // alloc: an error.
        }
    }

    /// `conn`, used by one more request until [`Uring::release`] says it ended, and whether it
    /// is TCP's; `None` for one that names nothing or is hung up, which nothing more uses.
    fn use_conn(&mut self, conn: Conn) -> Option<bool> {
        let connection = self.conns.get_mut(conn_of(conn)).filter(|it| !it.hung)?;
        connection.users += 1;
        Some(connection.tcp)
    }

    fn release(&mut self, conn: Conn) {
        let Some(connection) = self.conns.get_mut(conn_of(conn)) else { return };
        connection.users -= 1;
        if connection.hung && connection.users == 0 {
            self.conns.remove(conn_of(conn));
        }
    }

    /// A receive of at most `most` bytes on `conn`, appended to `into`: from what its receive
    /// took, in order, once there is any.
    fn recv(&mut self, conn: Conn, tag: Tag, most: usize, into: Vec<u8>) {
        let Some(connection) = self.conns.get_mut(conn_of(conn)).filter(|it| !it.hung) else {
            self.ready.push(Completion { tag, done: Done::unread(into, gone(conn)) });
            return;
        };
        connection.recvs.push_back((tag, most, into));
        self.deliver(conn);
    }

    /// Writes that bypassed the page cache (`O_DIRECT`) and ended, since the driver was made.
    pub fn direct_writes(&self) -> u64 {
        self.direct_writes
    }
}

/// SIGKILL to the process of `pidfd`; one that has exited already is left as it is.
fn kill(pidfd: RawFd) -> io::Result<()> {
    // SAFETY: no pointers but a null `siginfo`; a pidfd names its process even once reaped,
    // so the signal never reaches another that took its pid.
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            libc::SIGKILL,
            ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if sent == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        _ => Err(error),
    }
}

fn gone(conn: Conn) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        format!("{conn:?}: no such connection (never made, or hung up)"), // alloc: an error.
    )
}

/// Answers the flushes of the watch `watch`, whose read has completed and is not yet asked
/// again, so nothing else reads its descriptor: what is still queued on it is told first.
fn flushed(
    ready: &mut Vec<Completion>,
    watch: Tag,
    tree: &mut crate::watch::Tree,
    buffer: &mut [u8],
    flushes: &mut Vec<Tag>,
    journal: Option<&FileName>,
) {
    if flushes.is_empty() {
        return;
    }
    let drained =
        tree.drain(buffer, |done| ready.push(Completion { tag: watch, done: told(journal, done) }));
    for tag in flushes.drain(..) {
        let done = match &drained {
            Ok(()) => Done::Flushed,
            // alloc: an error path.
            Err(error) => Done::Failed(IoError(format!("flushing the watch: {error}"))),
        };
        ready.push(Completion { tag, done });
    }
}

/// What a watch's read says, as its effect tells it: for a journal, as the batch of a journal
/// on a system that keeps none.
fn told(journal: Option<&FileName>, done: Done) -> Done {
    match journal {
        Some(dir) => crate::journal::live(dir, done),
        None => done,
    }
}

/// Fails the flushes of a watch that ended first.
fn unflushed(ready: &mut Vec<Completion>, flushes: &mut Vec<Tag>) {
    for tag in flushes.drain(..) {
        let why = IoError("the watch ended before the flush".into());
        ready.push(Completion { tag, done: Done::Failed(why) });
    }
}

/// Forgets the descriptor kept for `name`: the name now is another file. It is closed once no
/// request in flight holds it.
fn forget(names: &mut HashMap<FileName, Handle<Kept>>, kept: &mut Slab<Kept>, name: &FileName) {
    let Some(file) = names.remove(name) else { return };
    let it = kept.get_mut(file).expect("named files are kept");
    it.named = false;
    if it.users == 0 {
        kept.remove(file);
    }
}

/// A request that held `file` ended.
fn unkeep(kept: &mut Slab<Kept>, file: Handle<Kept>) {
    let Some(it) = kept.get_mut(file) else { return };
    it.users -= 1;
    if it.users == 0 && !it.named {
        kept.remove(file);
    }
}

/// `f` of `name` under `root`, joined in the thread's scratch path.
fn under<R>(root: &Path, name: &FileName, f: impl FnOnce(&Path) -> R) -> R {
    scratch::with_path(|path| {
        path.push(root);
        path.push(name.as_str());
        f(path)
    })
}

/// `f` of `path` NUL-terminated, in the thread's scratch bytes.
fn with_c<R>(path: &Path, f: impl FnOnce(&CStr) -> io::Result<R>) -> io::Result<R> {
    scratch::with_bytes(|bytes| {
        bytes.extend_from_slice(path.as_os_str().as_bytes());
        bytes.push(0);
        let c = CStr::from_bytes_with_nul(&bytes[..])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))?;
        f(c)
    })
}

/// `path` opened with `flags`, made `0o644` where they make it.
fn open_c(path: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: a NUL-terminated path; the descriptor returned is this driver's.
    let fd = unsafe { libc::open(path.as_ptr(), flags, 0o644 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The directory `dir`, opened on a stamper's thread for calls relative to it.
fn open_dir(dir: &Path) -> io::Result<OwnedFd> {
    with_c(dir, |dir| open_c(dir, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC))
}

fn size_of_file(fd: RawFd) -> io::Result<u64> {
    // SAFETY: `libc::stat` is a C struct of integers and arrays of them, for which all-zero
    // bytes is a valid value.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: an open descriptor and a `stat` to write.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.st_size as u64)
}

/// Makes a TCP listener's connections, which they inherit it from, send each write at once.
fn no_delay(listener: &crate::net::Listener) -> io::Result<()> {
    if listener.is_unix() {
        return Ok(());
    }
    let on: libc::c_int = 1;
    // SAFETY: an open socket and an int option of the size given.
    let set = unsafe {
        libc::setsockopt(
            listener.fd(),
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            (&raw const on).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if set != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A socket made for a connect, its writes sent at once where it is TCP's, in the driver's
/// half of the table.
fn connect_socket(table: &Rc<Table>, domain: libc::c_int) -> io::Result<Fixed> {
    let fd = crate::net::stream_socket(domain)?;
    if domain != libc::AF_UNIX {
        let on: libc::c_int = 1;
        // SAFETY: an open socket and an int option of the size given.
        let set = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_NODELAY,
                (&raw const on).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if set != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    table.install(fd)
}

impl Driver for Uring {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            let tag = effect.tag();
            match effect {
                Effect::Unwatch { .. } => {
                    match self.watches.get(&tag).copied() {
                        Some(id) => self.stop(id),
                        None => self.refuse(tag, format!("nothing is watched as {tag:?}")), // alloc: an error.
                    }
                    continue;
                }
                Effect::Unlisten { .. } => {
                    match self.listens.get(&tag).copied() {
                        Some(id) => self.stop(id),
                        None => self.refuse(tag, format!("nothing is listened on as {tag:?}")), // alloc: an error.
                    }
                    continue;
                }
                Effect::Kill { .. } => {
                    self.kill(tag);
                    continue;
                }
                _ => {}
            }
            self.in_flight += 1;
            let started = match effect {
                Effect::Write { file, bytes, .. } => self.write(tag, file, bytes).map(|()| None),
                Effect::Clone { from, to, .. } => self.copy(tag, Kind::Clone, &from, &to).map(|()| None),
                Effect::Link { from, to, .. } => self.copy(tag, Kind::Link, &from, &to).map(|()| None),
                Effect::Stat { file, .. } => self.look(tag, Kind::Stat, &file).map(|()| None),
                list @ Effect::List { .. } => self.off_ring(list).map(|()| None),
                private @ Effect::Private { .. } => self.off_ring(private).map(|()| None),
                Effect::WriteAt { file, at, bytes, .. } => self.write_at(tag, &file, at, bytes).map(|()| None),
                Effect::Rename { from, to, .. } => self.rename(tag, &from, &to).map(|()| None),
                Effect::Mode { file, mode, .. } => {
                    under(&self.root, &file, |path| crate::fs::set_mode(path, mode)).map(|()| Some(Done::Moded))
                }
                // No io_uring op sets a file's times: one `utimensat` here on the driver's
                // thread, as a mode is set, its path in the thread's scratch buffers. A thread
                // of its own (`off_ring`) would cost a spawn per file for a call of microseconds.
                Effect::Times { file, modified_ns, .. } => {
                    under(&self.root, &file, |path| crate::fs::set_times(path, modified_ns)).map(|()| Some(Done::Timed))
                }
                // No io_uring op reads a link: one `readlink` here, as a file's times are set.
                Effect::ReadLink { file, .. } => crate::threads::read_link(&self.root, &file).map(Some),
                Effect::Remove { file, .. } => self.remove(tag, &file).map(|()| None),
                Effect::Watch { dir, .. } => self.watch(tag, dir, false).map(|()| None),
                // Linux keeps no journal across a restart: the first batch says what of the
                // cursor is lost, and the watch is live from its start.
                Effect::Journal { dir, since, .. } => {
                    let first = crate::journal::first_on_linux(&dir, &since);
                    self.watch(tag, dir, true).map(|()| {
                        self.ready.push(Completion { tag, done: first });
                        None
                    })
                }
                Effect::Stamp { files, .. } => self.sweep(tag, files).map(|()| None),
                Effect::Entries { dir, .. } => self.list(tag, dir).map(|()| None),
                Effect::Flush { watch, .. } => self.flush(watch, tag).map(|()| None),
                Effect::Unwatch { .. } | Effect::Unlisten { .. } | Effect::Kill { .. } => {
                    unreachable!("taken above")
                }
                // Of a file kept for appending, through its descriptor, after what was asked
                // of it before; of any other, the file at its name now, opened for the read
                // alone. Its buffer is the file's size, once it is open (a missing file's,
                // empty).
                Effect::ReadAll { file, .. } => match self.names.contains_key(&file) {
                    true => self.file(&file, false).map(|file| {
                        let file = self.hold(file);
                        let mut hot = Hot::new(tag, Kind::Read, file.to_bits());
                        hot.flags |= ops::WHOLE;
                        self.start(hot, ops::NO_BYTES, Cold::EMPTY);
                        None
                    }),
                    false => self.load(tag, &file, 0, None).map(|()| None),
                },
                Effect::Read { file, at, len, .. } => match self.names.contains_key(&file) {
                    true => self.file(&file, false).map(|file| {
                        let file = self.hold(file);
                        let hot = Hot::new(tag, Kind::Read, file.to_bits());
                        // alloc: the read's result, handed to its asker.
                        self.start(hot, Vec::with_capacity(len), Cold { at, ..Cold::EMPTY });
                        None
                    }),
                    false => self.load(tag, &file, at, Some(len)).map(|()| None),
                },
                // An empty one too, so it answers once the file it makes is there.
                Effect::Append { file, bytes, .. } => self.file(&file, true).map(|file| {
                    let file = self.hold(file);
                    self.start(Hot::new(tag, Kind::Append, file.to_bits()), bytes, Cold::EMPTY);
                    None
                }),
                Effect::Sync { file, .. } => self.file(&file, true).map(|file| {
                    let file = self.hold(file);
                    self.start(Hot::new(tag, Kind::Sync, file.to_bits()), ops::NO_BYTES, Cold::EMPTY);
                    None
                }),
                Effect::Truncate { file, len, .. } => self.file(&file, true).map(|file| {
                    let file = self.hold(file);
                    self.start(Hot::new(tag, Kind::Truncate, file.to_bits()), ops::NO_BYTES, Cold { at: len, ..Cold::EMPTY });
                    None
                }),
                Effect::Timer { after, .. } => {
                    let due = wheel::ticks(self.start.elapsed()).saturating_add(wheel::ticks(after));
                    self.timers.insert(due, tag);
                    Ok(None)
                }
                Effect::Map { file, .. } => self.look(tag, Kind::Map, &file).map(|()| None),
                Effect::Spawn { command, .. } => self.spawn(tag, command).map(|()| None),
                Effect::Listen { at: address, .. } => {
                    crate::net::listen(&self.root, &address).and_then(|listener| {
                        no_delay(&listener)?;
                        let listener = self.listeners.insert(listener);
                        let id = self.start(Hot::new(tag, Kind::Accept, listener.to_bits()), ops::NO_BYTES, Cold::EMPTY);
                        self.listens.insert(tag, id);
                        Ok(None)
                    })
                }
                Effect::Connect { at: address, .. } => crate::net::sockaddr(&self.root, &address)
                    .and_then(|address| {
                        let socket = connect_socket(&self.table, address.domain)?;
                        let tcp = address.domain != libc::AF_UNIX;
                        let connecting = self.connectings.insert(Connecting { socket: Some(socket), at: address, tcp });
                        self.start(Hot::new(tag, Kind::Connect, connecting.to_bits()), ops::NO_BYTES, Cold::EMPTY);
                        Ok(None)
                    }),
                Effect::Recv { conn, most, into, .. } => {
                    self.recv(conn, tag, most, into);
                    Ok(None)
                }
                Effect::Send { conn, bytes, .. } if bytes.is_empty() => {
                    Ok(Some(match self.conns.get(conn_of(conn)).is_some_and(|it| !it.hung) {
                        true => Done::Sent(bytes),
                        false => Done::unsent(bytes, gone(conn)),
                    }))
                }
                Effect::Send { conn, bytes, .. } => match self.use_conn(conn) {
                    Some(tcp) => {
                        let mut hot = Hot::new(tag, Kind::Send, conn.0);
                        if tcp && bytes.len() >= ZERO_COPY_LEAST {
                            hot.flags |= ops::ZERO_COPY;
                        }
                        self.start(hot, bytes, Cold::EMPTY);
                        Ok(None)
                    }
                    None => Ok(Some(Done::unsent(bytes, gone(conn)))),
                },
                Effect::Hangup { conn, .. } => self.hang_up(conn, tag).map(|()| None),
            };
            match started {
                Ok(None) => {}
                Ok(Some(done)) => self.ready.push(Completion { tag, done }),
                Err(error) => self
                    .ready
                    .push(Completion { tag, done: Done::Failed(IoError(error.to_string())) }), // alloc: an error.
            }
        }
        self.flush_waiting();
        self.submit_ring();
    }

    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize {
        let before = out.len();
        self.reap();
        self.fire_timers();
        out.append(&mut self.ready);
        if out.len() == before && self.in_flight > 0 {
            let deadline = Instant::now() + wait;
            while out.len() == before {
                let left = deadline.saturating_duration_since(Instant::now());
                let now = wheel::ticks(self.start.elapsed());
                let until_timer =
                    self.timers.next_deadline().map(|due| Duration::from_nanos(due.saturating_sub(now)));
                if let Err(error) = self.ring.enter(Some(until_timer.map_or(left, |it| it.min(left)))) {
                    self.fail_unsubmitted(error);
                    out.append(&mut self.ready);
                    break;
                }
                self.reap();
                self.fire_timers();
                out.append(&mut self.ready);
                if left.is_zero() {
                    break;
                }
            }
        }
        // A watch or a journal goes on after a change, and a listen after an accept.
        let ended = out[before..]
            .iter()
            .filter(|it| !matches!(it.done, Done::Changed { .. } | Done::Journaled(_) | Done::Accepted { .. }))
            .count();
        self.in_flight -= ended;
        out.len() - before
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn now(&self) -> Duration {
        self.start.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        Address, Confined, Cursor, FileName, Journaled, Lost, Sandbox, Traced, Untraceable,
        run_until_idle, threads::Threads,
    };

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// An offset into a ring's mapping is checked in every build: the last whole value is
    /// handed out, one byte further is refused, never a pointer past the mapping.
    #[test]
    fn a_ring_offset_past_its_mapping_is_refused() {
        let map = anonymous(4096).unwrap();
        let last: *mut u32 = map.at(4092);
        assert_eq!(last.addr() - map.at.addr(), 4092);
        let past = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| map.at::<u32>(4093)));
        assert!(past.is_err(), "a u32 at 4093 of 4096 bytes was handed out");
    }
    #[test]
    fn as_threads_do() {
        let file = FileName::new("vfs/journal").unwrap();
        fn script(driver: &mut impl Driver, file: &FileName) -> Vec<Done> {
            let mut effects =
                vec![Effect::Append { file: file.clone(), bytes: b"one".to_vec(), tag: Tag(1) }];
            let mut seen = Vec::new();
            run_until_idle(driver, &mut effects, |done, effects| {
                seen.push(done.done.clone());
                match done.tag {
                    Tag(1) => effects.push(Effect::Sync { file: file.clone(), tag: Tag(2) }),
                    Tag(2) => {
                        effects.push(Effect::Timer { after: Duration::from_millis(1), tag: Tag(3) })
                    }
                    Tag(3) => effects.push(Effect::Read {
                        file: file.clone(),
                        at: 1,
                        len: 10,
                        tag: Tag(4),
                    }),
                    Tag(4) => {
                        effects.push(Effect::Truncate { file: file.clone(), len: 2, tag: Tag(5) })
                    }
                    Tag(5) => effects.push(Effect::ReadAll { file: file.clone(), tag: Tag(6) }),
                    Tag(6) => effects.push(Effect::ReadAll {
                        file: FileName::new("missing").unwrap(),
                        tag: Tag(7),
                    }),
                    Tag(7) => effects.push(Effect::Read {
                        file: FileName::new("missing").unwrap(),
                        at: 0,
                        len: 1,
                        tag: Tag(8),
                    }),
                    _ => {}
                }
            });
            seen
        }
        let (ours, theirs) = (dir("uring"), dir("uring-threads"));
        let ring = script(&mut Uring::new(&ours).unwrap(), &file);
        let threads = script(&mut Threads::new(&theirs, 2), &file);
        assert_eq!(&ring[..7], &threads[..7]);
        assert_eq!(
            &ring[..7],
            &[
                Done::Appended,
                Done::Synced,
                Done::Fired,
                Done::Read(b"ne".to_vec()),
                Done::Truncated,
                Done::Read(b"on".to_vec()),
                Done::Read(Vec::new()),
            ]
        );
        assert!(matches!((&ring[7], &threads[7]), (Done::Failed(_), Done::Failed(_))));
        for it in [ours, theirs] {
            std::fs::remove_dir_all(it).unwrap();
        }
    }

    /// More requests than the ring holds, all in flight at once, each to its own file, with
    /// timers among them that fire in their order.
    #[test]
    fn more_than_the_ring_holds() {
        let root = dir("uring-many");
        let mut driver = Uring::new(&root).unwrap();
        let count = ENTRIES as u64 * 4;
        let mut effects: Vec<Effect> = (0..count)
            .map(|n| Effect::Append {
                file: FileName::new(&format!("f/{}", n % 64)).unwrap(),
                bytes: n.to_le_bytes().to_vec(),
                tag: Tag(n),
            })
            .collect();
        effects.extend((0..3).map(|n| Effect::Timer {
            after: Duration::from_millis(5 * (3 - n)),
            tag: Tag(count + n),
        }));
        let mut appended = 0;
        let mut fired = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| match done.done {
            Done::Appended => appended += 1,
            Done::Fired => fired.push(done.tag.0 - count),
            other => panic!("{other:?}"),
        });
        assert_eq!((appended, fired), (count, vec![2, 1, 0]));
        let mut total = 0;
        for n in 0..64 {
            total += std::fs::read(root.join(format!("f/{n}"))).unwrap().len();
        }
        assert_eq!(total as u64, count * 8);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_wait_ends_at_its_time() {
        let mut driver = Uring::new(dir("uring-wait")).unwrap();
        let mut effects = vec![Effect::Timer { after: Duration::from_secs(60), tag: Tag(1) }];
        driver.submit(&mut effects);
        let began = Instant::now();
        let mut out = Vec::new();
        assert_eq!(driver.poll(&mut out, Duration::from_millis(20)), 0);
        let waited = began.elapsed();
        assert!(
            waited >= Duration::from_millis(19) && waited < Duration::from_secs(5),
            "{waited:?}"
        );
        assert_eq!(driver.in_flight(), 1);
    }

    fn command(program: &str, args: &[&str], stdin: &[u8]) -> Command {
        Command {
            program: program.into(),
            args: args.iter().map(Into::into).collect(),
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            dir: "/".into(),
            stdin: stdin.to_vec(),
            sandbox: Sandbox::Off,
            memory: None,
        }
    }

    fn sh(script: &str, stdin: &[u8]) -> Box<Command> {
        Box::new(command("/bin/sh", &["-c", script], stdin))
    }

    /// A completion as two drivers must agree on it: without the memory measured, which
    /// differs from run to run, and a failure without its words.
    fn shown(done: &Done) -> String {
        let text = |bytes: &[u8]| match bytes.len() {
            0..100 => format!("{:?}", String::from_utf8_lossy(bytes)),
            len => format!("{len} bytes"),
        };
        match done {
            Done::Exited(exit) => format!(
                "exited code {:?} signal {:?} stdout {} stderr {} traced {:?}",
                exit.code,
                exit.signal,
                text(&exit.stdout),
                text(&exit.stderr),
                exit.traced
            ),
            Done::Mapped(mapping) => format!("mapped {}", text(mapping.bytes())),
            Done::Sent(bytes) => format!("sent, {} bytes back", bytes.len()),
            Done::Broken { buffer, .. } => format!("broken, {} bytes back", buffer.len()),
            Done::Failed(_) => "failed".into(),
            other => format!("{other:?}"),
        }
    }

    /// Processes with input, output, an exit code and a signal; one killed; one in a sandbox
    /// no driver was given the means to make; one whose program is not there.
    fn processes(driver: &mut impl Driver) -> BTreeMap<u64, Done> {
        let unconfined = Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: false, services: Vec::new() };
        let mut effects = vec![
            Effect::Spawn { command: sh("tr a-z A-Z; echo oops >&2; exit 3", b"hello\n"), tag: Tag(1) },
            Effect::Spawn { command: sh("kill -TERM $$", b""), tag: Tag(2) },
            Effect::Spawn { command: sh("exec sleep 60", b""), tag: Tag(3) },
            Effect::Spawn {
                command: sh("wc -c; head -c 200000 /dev/zero", &vec![b'x'; 300_000]),
                tag: Tag(4),
            },
            Effect::Spawn {
                command: Box::new(Command { sandbox: unconfined, ..command("/bin/true", &[], b"") }),
                tag: Tag(5),
            },
            Effect::Spawn {
                command: Box::new(command("/nonexistent/blaze-test", &[], b"")),
                tag: Tag(6),
            },
            // A kill of nothing in flight does nothing.
            Effect::Kill { tag: Tag(77) },
            Effect::Timer { after: Duration::from_millis(50), tag: Tag(7) },
        ];
        let mut seen = BTreeMap::new();
        run_until_idle(driver, &mut effects, |done, effects| {
            if done.tag == Tag(7) {
                effects.push(Effect::Kill { tag: Tag(3) });
            }
            seen.insert(done.tag.0, done.done);
        });
        assert_eq!(driver.in_flight(), 0);
        seen
    }

    #[test]
    fn processes_end_as_on_threads() {
        let ring = processes(&mut Uring::new(dir("uring-spawn")).unwrap());
        let exited = |tag: u64| match &ring[&tag] {
            Done::Exited(exit) => exit.clone(),
            other => panic!("{tag}: {other:?}"),
        };
        let one = exited(1);
        assert_eq!((one.code, one.signal), (Some(3), None));
        assert_eq!((&one.stdout[..], &one.stderr[..]), (&b"HELLO\n"[..], &b"oops\n"[..]));
        assert_eq!((exited(2).code, exited(2).signal), (None, Some(libc::SIGTERM)));
        assert_eq!(exited(3).signal, Some(libc::SIGKILL));
        let four = exited(4);
        assert_eq!(four.code, Some(0));
        assert!(four.stdout.starts_with(b"300000\n"), "{:?}", four.stdout.get(..10));
        assert_eq!(four.stdout.len(), 7 + 200_000);
        assert!(matches!(&ring[&5], Done::Failed(why) if why.0.contains("no Confine")), "{:?}", ring[&5]);
        assert!(
            matches!(&ring[&6], Done::Failed(why) if why.0.contains("/nonexistent/blaze-test")),
            "{:?}",
            ring[&6]
        );
        assert_eq!(ring[&7], Done::Fired);
        assert!(!ring.contains_key(&77));
        let threads = processes(&mut Threads::new(dir("uring-spawn-threads"), 2));
        let show = |it: &BTreeMap<u64, Done>| -> BTreeMap<u64, String> {
            it.iter().map(|(tag, done)| (*tag, shown(done))).collect()
        };
        assert_eq!(show(&ring), show(&threads));
    }

    struct Fake;

    fn fake_trace() -> Traced {
        Traced {
            files: vec![("/etc/hostname".into(), Some([7; 32])), ("/missing".into(), None)],
            dirs: vec![("/usr".into(), [1; 32])],
            env: vec![("HOME".into(), None)],
            untraceable: vec![Untraceable::Clock],
        }
    }

    impl Confine for Fake {
        fn confine(&self, command: &Command, scratch: &Path) -> Result<Confined, String> {
            assert!(scratch.is_dir(), "{}", scratch.display());
            Ok(Confined {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), format!("echo confined {}", command.args.len()).into()],
                env: Vec::new(),
                dir: "/".into(),
            })
        }

        fn traced(&self, _: &Command, scratch: &Path) -> Result<Traced, String> {
            assert!(scratch.is_dir(), "{}", scratch.display());
            Ok(fake_trace())
        }
    }

    /// A sandboxed spawn runs what its `Confine` says, and its trace is the sandbox's.
    #[test]
    fn a_confined_spawn_is_traced() {
        let root = dir("uring-confined");
        let mut driver = Uring::new(&root).unwrap().confined(Arc::new(Fake));
        let sandbox =
            Sandbox::On { read: vec!["/usr".into()], write: Vec::new(), network: false, trace: true, services: Vec::new() };
        let mut effects = vec![Effect::Spawn {
            command: Box::new(Command { sandbox, ..command("/usr/bin/env", &["a", "b"], b"") }),
            tag: Tag(1),
        }];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done.done));
        let [Done::Exited(exit)] = &seen[..] else { panic!("{seen:?}") };
        assert_eq!(exit.stdout, b"confined 2\n");
        assert_eq!(exit.traced, Some(fake_trace()));
        let left: Vec<_> = std::fs::read_dir(root.join(".blaze-spawn")).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A connection to the listen `Tag(1)`, submitted already, at `at`: a ping and a pong, each
    /// side hung up, a receive on a connection gone, the listen ended and an unlisten of
    /// nothing. What each step did, by its tag, with connections by their role.
    fn round_trip(driver: &mut impl Driver, at: Address) -> BTreeMap<u64, Vec<String>> {
        let mut seen: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        let (mut server, mut client, mut pinged) = (None, None, false);
        let mut effects = vec![Effect::Timer { after: Duration::from_millis(20), tag: Tag(2) }];
        run_until_idle(driver, &mut effects, |done, effects| {
            let tag = done.tag.0;
            let shown = match &done.done {
                Done::Accepted { conn } => {
                    server = Some(*conn);
                    "accepted".to_owned()
                }
                Done::Connected { conn } => {
                    client = Some(*conn);
                    "connected".to_owned()
                }
                other => shown(other),
            };
            seen.entry(tag).or_default().push(shown);
            if tag == 2 {
                effects.push(Effect::Connect { at: at.clone(), tag: Tag(3) });
            }
            let (Some(server), Some(client)) = (server, client) else { return };
            assert_ne!(server, client);
            if !pinged {
                pinged = true;
                effects.push(Effect::Send { conn: client, bytes: b"ping".to_vec(), tag: Tag(4) });
                effects.push(Effect::Recv { conn: server, most: 64, into: Vec::new(), tag: Tag(5) });
            }
            match tag {
                5 => {
                    effects.push(Effect::Send { conn: server, bytes: b"pong".to_vec(), tag: Tag(6) });
                    effects.push(Effect::Recv { conn: client, most: 64, into: Vec::new(), tag: Tag(7) });
                }
                7 => effects.push(Effect::Hangup { conn: client, tag: Tag(8) }),
                8 => {
                    effects.push(Effect::Recv { conn: server, most: 64, into: Vec::new(), tag: Tag(9) });
                    effects.push(Effect::Recv { conn: client, most: 64, into: Vec::new(), tag: Tag(10) });
                }
                9 => {
                    effects.push(Effect::Hangup { conn: server, tag: Tag(11) });
                    effects.push(Effect::Unlisten { tag: Tag(1) });
                    effects.push(Effect::Unlisten { tag: Tag(12) });
                }
                _ => {}
            }
        });
        assert_eq!(driver.in_flight(), 0);
        seen
    }

    fn round_tripped() -> BTreeMap<u64, Vec<String>> {
        let one = |it: &str| vec![it.to_owned()];
        let read = |it: &[u8]| one(&format!("{:?}", Done::Read(it.to_vec())));
        BTreeMap::from([
            (1, vec!["accepted".to_owned(), "Unlistened".to_owned()]),
            (2, one("Fired")),
            (3, one("connected")),
            (4, one("sent, 0 bytes back")),
            (5, read(b"ping")),
            (6, one("sent, 0 bytes back")),
            (7, read(b"pong")),
            (8, one("HungUp")),
            (9, read(b"")),
            (10, one("broken, 0 bytes back")),
            (11, one("HungUp")),
            (12, one("failed")),
        ])
    }

    #[test]
    fn a_unix_socket_round_trip_as_on_threads() {
        let at = Address::Unix(FileName::new("run/api.sock").unwrap());
        let root = dir("uring-unix");
        let mut ring = Uring::new(&root).unwrap();
        ring.submit(&mut vec![Effect::Listen { at: at.clone(), tag: Tag(1) }]);
        assert_eq!(ring.in_flight(), 1);
        assert_eq!(ring.local_addr(Tag(1)), None);
        let ours = round_trip(&mut ring, at.clone());
        assert_eq!(ours, round_tripped());
        assert!(!root.join("run/api.sock").exists());
        let mut threads = Threads::new(dir("uring-unix-threads"), 2);
        threads.submit(&mut vec![Effect::Listen { at: at.clone(), tag: Tag(1) }]);
        assert_eq!(ours, round_trip(&mut threads, at));
    }

    #[test]
    fn a_tcp_round_trip() {
        let mut ring = Uring::new(dir("uring-tcp")).unwrap();
        let any = Address::Tcp("127.0.0.1:0".parse().unwrap());
        ring.submit(&mut vec![Effect::Listen { at: any, tag: Tag(1) }]);
        assert_eq!(ring.in_flight(), 1);
        let at = ring.local_addr(Tag(1)).expect("listening");
        assert_ne!(at.port(), 0);
        assert_eq!(round_trip(&mut ring, Address::Tcp(at)), round_tripped());
        assert_eq!(ring.local_addr(Tag(1)), None);
        // Nothing listens there now.
        let mut effects = vec![Effect::Connect { at: Address::Tcp(at), tag: Tag(1) }];
        let mut seen = Vec::new();
        run_until_idle(&mut ring, &mut effects, |done, _| seen.push(done.done));
        assert!(matches!(&seen[..], [Done::Failed(_)]), "{seen:?}");
    }

    fn maps(driver: &mut impl Driver, root: &Path) -> BTreeMap<u64, String> {
        std::fs::create_dir_all(root.join("store")).unwrap();
        std::fs::write(root.join("store/pack"), b"packed").unwrap();
        std::fs::write(root.join("store/empty"), b"").unwrap();
        let name = |it: &str| FileName::new(it).unwrap();
        let mut effects = vec![
            Effect::Map { file: name("store/pack"), tag: Tag(1) },
            Effect::Map { file: name("store/empty"), tag: Tag(2) },
            Effect::Map { file: name("store/missing"), tag: Tag(3) },
        ];
        let mut seen = BTreeMap::new();
        run_until_idle(driver, &mut effects, |done, _| _ = seen.insert(done.tag.0, shown(&done.done)));
        seen
    }

    #[test]
    fn a_mapping_is_the_file_as_on_threads() {
        let root = dir("uring-map");
        let ours = maps(&mut Uring::new(&root).unwrap(), &root);
        assert_eq!(
            ours,
            BTreeMap::from([
                (1, "mapped \"packed\"".to_owned()),
                (2, "mapped \"\"".to_owned()),
                (3, "failed".to_owned()),
            ])
        );
        let root = dir("uring-map-threads");
        assert_eq!(ours, maps(&mut Threads::new(&root, 2), &root));
    }

    /// Listings done off the ring: many at once, one of a missing directory; each wakes the
    /// poll that waits, with a timer far off in flight beside them.
    #[test]
    fn listings_wake_the_ring_from_their_threads() {
        let root = dir("uring-list");
        for file in ["tree/a", "tree/b/c", "tree/b/d/e"] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, file).unwrap();
        }
        let mut driver = Uring::new(&root).unwrap();
        let name = |it: &str| FileName::new(it).unwrap();
        let mut effects: Vec<Effect> =
            (0..16).map(|n| Effect::List { dir: name("tree"), tag: Tag(n) }).collect();
        effects.push(Effect::List { dir: name("missing"), tag: Tag(16) });
        effects.push(Effect::Timer { after: Duration::from_secs(60), tag: Tag(17) });
        driver.submit(&mut effects);
        let started = Instant::now();
        let mut seen = BTreeMap::new();
        let mut done = Vec::new();
        while seen.len() < 17 && started.elapsed() < Duration::from_secs(20) {
            driver.poll(&mut done, Duration::from_secs(10));
            for completion in done.drain(..) {
                let shown = match completion.done {
                    Done::Listed(found) => {
                        found.iter().map(|(it, _)| it.as_str()).collect::<Vec<_>>().join(" ")
                    }
                    Done::Failed(_) => "failed".to_owned(),
                    other => panic!("{other:?}"),
                };
                seen.insert(completion.tag.0, shown);
            }
        }
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        let all = "tree/a tree/b tree/b/c tree/b/d tree/b/d/e";
        let mut expected: BTreeMap<u64, String> = (0..16).map(|n| (n, all.to_owned())).collect();
        expected.insert(16, "failed".to_owned());
        assert_eq!(seen, expected);
        assert_eq!(driver.in_flight(), 1);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Dropping the driver with a process, a listen and a receive in flight kills the process
    /// and takes everything back from the kernel.
    #[test]
    fn a_driver_dropped_with_work_in_flight() {
        let root = dir("uring-drop");
        let mut ring = Uring::new(&root).unwrap();
        let at = Address::Unix(FileName::new("api.sock").unwrap());
        ring.submit(&mut vec![
            Effect::Spawn { command: sh("exec sleep 60", b""), tag: Tag(1) },
            Effect::Listen { at: at.clone(), tag: Tag(2) },
            Effect::Connect { at, tag: Tag(3) },
            Effect::Timer { after: Duration::from_secs(60), tag: Tag(4) },
        ]);
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut client = None;
        while client.is_none() && Instant::now() < deadline {
            ring.poll(&mut out, Duration::from_millis(50));
            client = out.iter().find_map(|it| match it.done {
                Done::Connected { conn } => Some(conn),
                _ => None,
            });
        }
        let client = client.expect("connected");
        ring.submit(&mut vec![Effect::Recv { conn: client, most: 16, into: Vec::new(), tag: Tag(5) }]);
        let pid = ring.spawns.get(ring.spawning[&Tag(1)]).expect("in flight").pid;
        let began = Instant::now();
        drop(ring);
        assert!(began.elapsed() < LETTING_GO, "{:?}", began.elapsed());
        // Reaped: no process of this pid is a child of this one any more.
        // SAFETY: no pointers.
        assert_eq!(unsafe { libc::waitpid(pid, ptr::null_mut(), libc::WNOHANG) }, -1);
        assert!(!root.join("api.sock").exists());
    }

    /// A file kept open is in the driver's half of the table, and its reads and appends name
    /// the slot; forgetting it empties the slot for the next.
    #[test]
    fn kept_files_are_registered() {
        let root = dir("uring-fixed");
        let mut driver = Uring::new(&root).unwrap();
        let name = |it: &str| FileName::new(it).unwrap();
        let mut effects = vec![
            Effect::Append { file: name("a"), bytes: b"one".to_vec(), tag: Tag(1) },
            Effect::Append { file: name("b"), bytes: b"two".to_vec(), tag: Tag(2) },
        ];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done.done));
        assert_eq!(seen, [Done::Appended, Done::Appended]);
        let slot = |driver: &Uring, it: &str| driver.kept.get(driver.names[&name(it)]).expect("kept").fixed.slot;
        let (a, b) = (slot(&driver, "a").expect("a slot"), slot(&driver, "b").expect("a slot"));
        assert!(a != b && a < driver.table.ours && b < driver.table.ours, "{a} {b}");
        let free = driver.table.free.borrow().len();
        let mut effects = vec![
            Effect::Read { file: name("a"), at: 0, len: 3, tag: Tag(3) },
            Effect::Remove { file: name("b"), tag: Tag(4) },
        ];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done.done));
        assert_eq!(seen, [Done::Read(b"one".to_vec()), Done::Removed]);
        // The slot is emptied through the ring, a close no effect waits for.
        let deadline = Instant::now() + Duration::from_secs(5);
        while driver.table.free.borrow().len() < free + 1 && Instant::now() < deadline {
            driver.poll(&mut Vec::new(), Duration::from_millis(1));
        }
        assert_eq!((driver.table.free.borrow().len(), driver.closes), (free + 1, 0));
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The kernel's submission thread sleeps once idle; a request written after is still
    /// taken, the thread woken by the enter that says so.
    #[test]
    fn a_sleeping_submission_thread_is_woken() {
        let root = dir("uring-sqpoll");
        let mut driver = Uring::new(&root).unwrap();
        let file = FileName::new("log").unwrap();
        for n in 0..3u8 {
            blaze_spin::SpinWait::within(
                "the submission thread to sleep, past its idle time",
                Duration::from_millis(u64::from(SQ_THREAD_IDLE_MS) * 100),
                || driver.ring.flags() & SQ_NEED_WAKEUP != 0,
            );
            let began = Instant::now();
            let mut effects = vec![Effect::Append { file: file.clone(), bytes: vec![n], tag: Tag(n.into()) }];
            let mut seen = Vec::new();
            run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done.done));
            assert_eq!(seen, [Done::Appended]);
            assert!(began.elapsed() < Duration::from_secs(1), "{:?}", began.elapsed());
        }
        assert_eq!(std::fs::read(root.join("log")).unwrap(), [0, 1, 2]);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Aligned writes of a MiB or more, more at once than there are registered buffers, each
    /// read back as written; where the file system takes `O_DIRECT` they bypassed the cache.
    /// A write that is not aligned goes through the cache.
    #[test]
    fn aligned_writes_read_back_equal() {
        let root = dir("uring-direct");
        std::fs::create_dir_all(&root).unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let pack = FileName::new("store/pack").unwrap();
        let piece = 3 * DIRECT_PIECE / 2;
        let count = 2 * DIRECT_BUFFERS as usize;
        let bytes = |n: usize| -> Vec<u8> { (0..piece).map(|at| (at * 31 + n * 7) as u8).collect() };
        std::fs::create_dir_all(root.join("store")).unwrap();
        std::fs::write(root.join("store/pack"), b"").unwrap();
        let mut effects: Vec<Effect> = (0..count)
            .map(|n| Effect::WriteAt { file: pack.clone(), at: (n * piece) as u64, bytes: bytes(n), tag: Tag(n as u64) })
            .collect();
        let odd = (count * piece) as u64 + 7;
        effects.push(Effect::WriteAt { file: pack.clone(), at: odd, bytes: vec![9; DIRECT_PIECE + 1], tag: Tag(99) });
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done.done));
        assert_eq!(seen, vec![Done::Written; count + 1]);
        let file = std::fs::read(root.join("store/pack")).unwrap();
        for n in 0..count {
            assert!(file[n * piece..(n + 1) * piece] == bytes(n), "piece {n}");
        }
        assert!(file[odd as usize..].iter().all(|&it| it == 9));
        let fd = std::fs::File::open(root.join("store/pack")).unwrap();
        let (_, align) = direct_alignment(fd.as_raw_fd()).unwrap();
        let aligned = align != 0 && piece % align as usize == 0 && DIRECT_PIECE % align as usize == 0;
        let direct = if aligned { count as u64 } else { 0 };
        assert_eq!(driver.direct_writes(), direct, "alignment {align}");
        assert_eq!((driver.lent.free(), driver.lent.waiters().count()), (DIRECT_BUFFERS as usize, 0));
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Many sends, one after another, received by one multishot receive through a ring of
    /// fewer buffers than they fill, in order and whole.
    #[test]
    fn one_receive_takes_many_messages_through_few_buffers() {
        let root = dir("uring-multishot");
        let mut ring = Uring::new(&root).unwrap();
        let at = Address::Unix(FileName::new("s.sock").unwrap());
        let message = |n: usize| -> Vec<u8> { (0..4096).map(|at| (at + n) as u8).collect() };
        let messages = 4 * RECV_BUFFERS as usize * RECV_BUFFER / 4096;
        let mut effects = vec![
            Effect::Listen { at: at.clone(), tag: Tag(1) },
            Effect::Connect { at, tag: Tag(2) },
        ];
        let (mut server, mut client) = (None, None);
        let (mut sent, mut got) = (0, Vec::new());
        let mut tag = 10;
        run_until_idle(&mut ring, &mut effects, |done, effects| {
            let mut next = || {
                tag += 1;
                Tag(tag)
            };
            match &done.done {
                Done::Accepted { conn } => server = Some(*conn),
                Done::Connected { conn } => client = Some(*conn),
                Done::Sent(_) => sent += 1,
                Done::Read(bytes) => {
                    assert!(!bytes.is_empty(), "ended after {} bytes", got.len());
                    got.extend_from_slice(bytes);
                }
                Done::Unlistened | Done::HungUp => {}
                other => panic!("{other:?}"),
            }
            let (Some(server), Some(client)) = (server, client) else { return };
            if matches!(done.done, Done::Accepted { .. } | Done::Connected { .. }) {
                effects.push(Effect::Recv { conn: server, most: 64 << 10, into: Vec::new(), tag: next() });
                effects.push(Effect::Send { conn: client, bytes: message(0), tag: next() });
                return;
            }
            if matches!(done.done, Done::Sent(_)) && sent < messages {
                effects.push(Effect::Send { conn: client, bytes: message(sent), tag: next() });
            }
            if matches!(done.done, Done::Read(_)) {
                if got.len() < messages * 4096 {
                    effects.push(Effect::Recv { conn: server, most: 64 << 10, into: Vec::new(), tag: next() });
                } else {
                    effects.push(Effect::Hangup { conn: client, tag: next() });
                    effects.push(Effect::Hangup { conn: server, tag: next() });
                    effects.push(Effect::Unlisten { tag: Tag(1) });
                }
            }
        });
        assert_eq!(sent, messages);
        let wanted: Vec<u8> = (0..messages).flat_map(message).collect();
        assert!(got == wanted, "{} bytes of {}", got.len(), wanted.len());
        assert_eq!(ring.in_flight(), 0);
        assert!(ring.conns.is_empty());
        drop(ring);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A whole file's write is one linked chain: written, synced, renamed over the old and the
    /// directory synced. Where the rename fails the old is as it was and no temporary is left;
    /// a rename across directories is linked to both directories' syncs.
    #[test]
    fn a_linked_write_whose_rename_fails_leaves_the_old() {
        let root = dir("uring-linked");
        std::fs::create_dir_all(root.join("out/blocked")).unwrap();
        std::fs::write(root.join("out/blocked/kept"), b"old").unwrap();
        std::fs::write(root.join("out/file"), b"old").unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let name = |it: &str| FileName::new(it).unwrap();
        let mut effects = vec![
            Effect::Write { file: name("out/blocked"), bytes: b"new".to_vec(), tag: Tag(1) },
            Effect::Write { file: name("out/file"), bytes: vec![5; 3 << 20], tag: Tag(2) },
            Effect::Write { file: name("out/fresh"), bytes: Vec::new(), tag: Tag(3) },
        ];
        let mut seen = BTreeMap::new();
        run_until_idle(&mut driver, &mut effects, |done, _| _ = seen.insert(done.tag.0, shown(&done.done)));
        assert_eq!(
            seen,
            BTreeMap::from([(1, "failed".to_owned()), (2, "Written".to_owned()), (3, "Written".to_owned())])
        );
        assert_eq!(std::fs::read(root.join("out/blocked/kept")).unwrap(), b"old");
        assert_eq!(std::fs::read(root.join("out/file")).unwrap(), vec![5; 3 << 20]);
        assert_eq!(std::fs::read(root.join("out/fresh")).unwrap(), b"");
        let mut left: Vec<String> = std::fs::read_dir(root.join("out"))
            .unwrap()
            .map(|it| it.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["blocked", "file", "fresh"]);
        let mut effects = vec![
            Effect::Rename { from: name("out/file"), to: name("moved/file"), tag: Tag(4) },
            Effect::Rename { from: name("out/missing"), to: name("out/other"), tag: Tag(5) },
        ];
        let mut seen = BTreeMap::new();
        run_until_idle(&mut driver, &mut effects, |done, _| _ = seen.insert(done.tag.0, shown(&done.done)));
        assert_eq!(seen, BTreeMap::from([(4, "Renamed".to_owned()), (5, "failed".to_owned())]));
        assert_eq!(std::fs::read(root.join("moved/file")).unwrap().len(), 3 << 20);
        assert!(!root.join("out/file").exists());
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// `files` files of 8 bytes in directories of 100 under `src`, one of them executable; a
    /// file at the top; a symlink to a file, one to nothing and one to a directory; a directory
    /// that can be searched and not read, with a file in it. The names to stamp of it: every
    /// file (one through the directory's link), directories, the links, and names with nothing
    /// there (under a file, under a dangling link, through the directory's link). Undone by
    /// [`unsweep`].
    fn sweep_tree(root: &Path, files: usize) -> Arc<Names> {
        let mut names = Names::default();
        for n in 0..files {
            if n % 100 == 0 {
                std::fs::create_dir_all(root.join(format!("src/d{}", n / 100))).unwrap();
            }
            let name = format!("src/d{}/f{n}.rs", n / 100);
            std::fs::write(root.join(&name), (n as u64).to_le_bytes()).unwrap();
            assert!(names.push_str(&name));
        }
        std::os::unix::fs::symlink("d0/f0.rs", root.join("src/link")).unwrap();
        std::os::unix::fs::symlink("nowhere", root.join("src/dangling")).unwrap();
        std::os::unix::fs::symlink("d0", root.join("src/via")).unwrap();
        crate::fs::set_mode(&root.join("src/d0/f1.rs"), 0o755).unwrap();
        std::fs::write(root.join("top.rs"), b"top").unwrap();
        std::fs::create_dir_all(root.join("src/sealed")).unwrap();
        std::fs::write(root.join("src/sealed/inside.rs"), b"in").unwrap();
        crate::fs::set_mode(&root.join("src/sealed"), 0o111).unwrap();
        let others = ["src", "src/d0", "src/link", "src/dangling", "src/via", "src/via/f0.rs", "top.rs", "src/sealed", "src/sealed/inside.rs"];
        for name in GONE.iter().chain(&others) {
            assert!(names.push_str(name));
        }
        Arc::new(names)
    }

    /// Files of [`sweep_tree`]'s besides the `files` in `src/d*`.
    const OTHER_FILES: usize = 3;

    /// Removes what [`sweep_tree`] made.
    fn unsweep(root: &Path) {
        crate::fs::set_mode(&root.join("src/sealed"), 0o755).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    const GONE: [&str; 5] =
        ["src/missing", "src/d0/f0.rs/under-a-file", "gone/deep/name", "src/dangling/name", "src/via/missing"];

    /// The stampers' sweep, over many chunks and directories, gives name by name the stamps of
    /// the reference sweep's `fstatat`s of whole names: files, directories, links not followed
    /// (but followed as a directory on the way), the mode's bits, a directory searched and not
    /// read, and names with nothing there gone.
    #[test]
    fn a_sweep_stamps_as_the_reference_does() {
        let root = dir("uring-stamp");
        let names = sweep_tree(&root, 10_000);
        assert!(names.len() > 8 * STAMP_CHUNK);
        let reference = crate::fs::stamps(&root, &names).unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let mut effects = vec![Effect::Stamp { files: names.clone(), tag: Tag(1) }];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done));
        let [Completion { tag: Tag(1), done: Done::Stamped(stamps) }] = &seen[..] else { panic!("{seen:?}") };
        assert_eq!(stamps.len(), names.len());
        for (at, (ours, theirs)) in stamps.iter().zip(&reference).enumerate() {
            assert_eq!(ours, theirs, "{}", names.get(at));
        }
        let of = |name: &str| stamps[names.iter().position(|it| it == name).expect("stamped")];
        assert_eq!(stamps.iter().filter(|it| it.is_file()).count(), 10_000 + OTHER_FILES);
        assert!(stamps.iter().all(|it| !it.is_unknown()));
        assert!(of("src").is_dir() && of("src/d0").is_dir() && of("src/sealed").is_dir());
        assert!(of("src/link").is_link() && of("src/dangling").is_link() && of("src/via").is_link());
        assert_eq!(of("src/via/f0.rs"), of("src/d0/f0.rs"));
        assert!(of("src/d0/f1.rs").executable() && !of("src/d0/f0.rs").executable());
        assert_eq!((of("src/d0/f3.rs").size, of("top.rs").size, of("src/sealed/inside.rs").size), (8, 3, 2));
        for gone in GONE {
            assert_eq!(of(gone), Stamp::GONE, "{gone}");
        }
        assert_eq!(driver.in_flight(), 0);
        drop(driver);
        unsweep(&root);
    }

    /// The allocation budget of sweeps and listings: nothing per name on a stamper; on the
    /// ring's thread, the root's name to open, the job and its result per sweep (a restart's,
    /// not the warm loop's), however many names, and for either at most one block of a
    /// stamper's channel now and then. The wake's read allocates nothing.
    #[test]
    fn a_sweep_allocates_per_sweep_not_per_name() {
        let root = dir("uring-stamp-alloc");
        let names = sweep_tree(&root, 2_000);
        let fd = open_dir(&root).unwrap();
        let mut parent = Vec::with_capacity(libc::PATH_MAX as usize);
        let mut stamps = vec![Stamp::UNKNOWN; names.len()];
        blaze_alloc_count::assert_at_most(0, "stamp_names", || {
            stamp_names(fd.as_raw_fd(), &names, 0..names.len(), &mut parent, |at, stamp| stamps[at] = stamp);
        });
        assert!(stamps.iter().all(|it| !it.is_unknown()));
        let mut driver = Uring::new(&root).unwrap();
        for round in 0..3 {
            let mut effects = Vec::with_capacity(1);
            effects.push(Effect::Stamp { files: names.clone(), tag: Tag(round) });
            // The first starts the stampers.
            match round {
                0 => driver.submit(&mut effects),
                _ => blaze_alloc_count::assert_at_most(4, "submitting a sweep", || driver.submit(&mut effects)),
            }
            let mut seen = 0;
            run_until_idle(&mut driver, &mut Vec::new(), |done, _| {
                assert!(matches!(done.done, Done::Stamped(_)), "{done:?}");
                seen += 1;
            });
            assert_eq!(seen, 1);
        }
        for round in 0..3 {
            let mut effects = Vec::with_capacity(1);
            effects.push(Effect::Entries { dir: FileName::new("src/d0").unwrap(), tag: Tag(10 + round) });
            blaze_alloc_count::assert_at_most(1, "submitting a listing", || driver.submit(&mut effects));
            let mut seen = 0;
            run_until_idle(&mut driver, &mut Vec::new(), |done, _| {
                assert!(matches!(done.done, Done::Entries(_)), "{done:?}");
                seen += 1;
            });
            assert_eq!(seen, 1);
        }
        drop(driver);
        unsweep(&root);
    }

    /// Two sweeps in flight at once, with an empty one, an append, a timer and a listing of
    /// entries among them: each ends as the reference has it. A sweep of a root not there
    /// fails, naming it.
    #[test]
    fn sweeps_in_flight_together_and_with_other_effects() {
        let root = dir("uring-stamps");
        let names = sweep_tree(&root, 6_000);
        let mut reversed = Names::default();
        for at in (0..names.len()).rev() {
            assert!(reversed.push_str(names.get(at)));
        }
        let reference = crate::fs::stamps(&root, &names).unwrap();
        let src = FileName::new("src").unwrap();
        let entries = crate::fs::entries(&root.join("src"), &src).unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let mut effects = vec![
            Effect::Stamp { files: names.clone(), tag: Tag(1) },
            Effect::Append { file: FileName::new("log").unwrap(), bytes: b"x".to_vec(), tag: Tag(2) },
            Effect::Stamp { files: Arc::new(reversed), tag: Tag(3) },
            Effect::Stamp { files: Arc::default(), tag: Tag(4) },
            Effect::Timer { after: Duration::from_millis(1), tag: Tag(5) },
            Effect::Entries { dir: src, tag: Tag(6) },
            Effect::Entries { dir: FileName::new("src/missing").unwrap(), tag: Tag(7) },
        ];
        driver.submit(&mut effects);
        assert_eq!(driver.in_flight(), 7);
        assert_eq!(driver.offload.out, 4, "two sweeps and two listings off the ring");
        let mut seen = BTreeMap::new();
        run_until_idle(&mut driver, &mut Vec::new(), |done, _| _ = seen.insert(done.tag.0, done.done));
        assert!(seen[&1] == Done::Stamped(reference.clone()), "the first sweep");
        let mut backwards = reference;
        backwards.reverse();
        assert!(seen[&3] == Done::Stamped(backwards), "the reversed sweep");
        assert_eq!(seen[&2], Done::Appended);
        assert_eq!(seen[&4], Done::Stamped(Vec::new()));
        assert_eq!(seen[&5], Done::Fired);
        assert_eq!(seen[&6], Done::Entries(entries));
        assert!(matches!(&seen[&7], Done::Failed(_)), "{:?}", seen[&7]);
        assert_eq!(driver.in_flight(), 0);
        drop(driver);

        let nowhere = dir("uring-stamp-no-root");
        let mut driver = Uring::new(&nowhere).unwrap();
        let mut effects = vec![Effect::Stamp { files: names, tag: Tag(1) }];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done.done));
        let [Done::Failed(why)] = &seen[..] else { panic!("{seen:?}") };
        assert!(why.0.contains(&*nowhere.to_string_lossy()), "{why}");
        unsweep(&root);
    }

    /// docs/v2/plan/design/restore.md's budget, a warm sweep of 100 000 tracked names in 50 ms,
    /// and the speed of light (docs/v2/plan/lanes/frontend/journal/linux.md): the kernel's own
    /// work for every name, measured as one thread's plain `statx` calls of them, over the
    /// cores. The sweep comes within 10% of it; the reference sweep's time is printed beside.
    #[test]
    #[ignore = "a benchmark: makes 100 000 files and times sweeps of them"]
    fn a_sweep_of_100k_names_is_within_the_restart_budget() {
        let root = dir("uring-stamp-bench");
        let names = sweep_tree(&root, 100_000);
        let mut driver = Uring::new(&root).unwrap();
        let mut best = Duration::MAX;
        for round in 0..6 {
            let began = Instant::now();
            let mut effects = vec![Effect::Stamp { files: names.clone(), tag: Tag(round) }];
            let mut stamped = 0;
            run_until_idle(&mut driver, &mut effects, |done, _| match done.done {
                Done::Stamped(stamps) => stamped = stamps.len(),
                other => panic!("{other:?}"),
            });
            assert_eq!(stamped, names.len());
            // The first warms the caches, as a daemon's earlier life has.
            if round > 0 {
                best = best.min(began.elapsed());
            }
        }
        let began = Instant::now();
        crate::fs::stamps(&root, &names).unwrap();
        let threads = began.elapsed();
        let dir = open_dir(&root).unwrap();
        let mut one = Duration::MAX;
        for _ in 0..3 {
            let began = Instant::now();
            for at in 0..names.len() {
                std::hint::black_box(statx_at(dir.as_raw_fd(), names.with_nul(at)));
            }
            one = one.min(began.elapsed());
        }
        let cores = std::thread::available_parallelism().map_or(1, |it| it.get());
        let light = one / cores as u32;
        eprintln!(
            "100 000 names: the stampers {best:?}, the speed of light {light:?} ({one:?} on one \
             core of {cores}), the reference on threads {threads:?}"
        );
        assert!(best < Duration::from_millis(50), "{best:?}");
        assert!(best.as_secs_f64() * 0.9 <= light.as_secs_f64(), "{best:?} against {light:?}");
        drop(driver);
        unsweep(&root);
    }

    /// A journal on Linux first says the changes since a saved cursor are lost (and nothing is
    /// for no cursor), then tells a file written as a live batch, before a flush asked after
    /// it; it ends at an unwatch, as a watch does.
    #[test]
    fn a_journal_tells_what_is_lost_then_what_is_written() {
        for (since, lost) in [(Cursor::Sim { journal: 1, next: 9 }, true), (Cursor::None, false)] {
            let root = dir("uring-journal");
            std::fs::create_dir_all(root.join("src")).unwrap();
            let src = FileName::new("src").unwrap();
            let mut driver = Uring::new(&root).unwrap();
            driver.submit(&mut vec![Effect::Journal { dir: src.clone(), since, tag: Tag(7) }]);
            let mut done = Vec::new();
            driver.poll(&mut done, Duration::from_secs(5));
            let lost = if lost { vec![(src.clone(), Lost::NoJournal)] } else { Vec::new() };
            match done.remove(0) {
                Completion { tag: Tag(7), done: Done::Journaled(first) } => assert_eq!(
                    *first,
                    Journaled { files: Vec::new(), lost, cursor: Cursor::None, caught_up: true }
                ),
                other => panic!("{other:?}"),
            }
            assert!(done.is_empty(), "{done:?}");
            assert_eq!(driver.in_flight(), 1);
            let written = FileName::new("src/new.rs").unwrap();
            std::fs::write(root.join(written.as_str()), b"fn f() {}").unwrap();
            driver.submit(&mut vec![Effect::Flush { watch: Tag(7), tag: Tag(8) }]);
            let (mut heard, mut flushed) = (Vec::new(), false);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !flushed && Instant::now() < deadline {
                driver.poll(&mut done, Duration::from_millis(50));
                for it in done.drain(..) {
                    match (it.tag.0, it.done) {
                        (7, Done::Journaled(batch)) => {
                            assert!(batch.caught_up && batch.lost.is_empty(), "{batch:?}");
                            heard.extend(batch.files);
                        }
                        (8, Done::Flushed) => {
                            assert!(heard.contains(&written), "flushed first; {heard:?}");
                            flushed = true;
                        }
                        other => panic!("{other:?}"),
                    }
                }
            }
            assert!(flushed, "the flush never came; heard {heard:?}");
            let mut ended = false;
            run_until_idle(&mut driver, &mut vec![Effect::Unwatch { tag: Tag(7) }], |it, _| {
                match (it.tag.0, it.done) {
                    (7, Done::Journaled(_)) => {}
                    (7, Done::Unwatched) => ended = true,
                    other => panic!("{other:?}"),
                }
            });
            assert!(ended);
            assert_eq!(driver.in_flight(), 0);
            drop(driver);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// The columns of requests answer as a map of ids does, over random starts, completions and
    /// ends of many seeds: each `user_data` names its request and link, a completion steps its
    /// own request's progress and buffer alone, and one of a request that ended (a stale
    /// completion) names nothing, even once its slot holds another; [`NOBODY`] names nothing.
    #[test]
    fn the_columns_of_requests_agree_with_a_map() {
        // A request as the model has it: its tag, whose it is, its progress, its buffer, its offset.
        type Model = (Tag, u64, u64, Vec<u8>, u64);
        let shown = |ops: &ops::Ops, id: Id| -> Option<Model> {
            let hot = ops.get(id)?;
            Some((hot.tag, hot.of, hot.done, ops.buf(id).clone(), ops.cold(id).at))
        };
        for seed in 0..64 {
            let mut rng = crate::sim::Rng::new(seed);
            let mut ours = ops::Ops::new(PENDING).unwrap();
            let mut theirs: HashMap<u64, Model> = HashMap::default();
            let (mut live, mut ended): (Vec<(u64, Id)>, Vec<Id>) = (Vec::new(), Vec::new());
            for next in 0..2_000u64 {
                match rng.below(4) {
                    _ if live.is_empty() || (live.len() < 256 && rng.below(3) != 0) => {
                        let model = (Tag(next), next.wrapping_mul(7), 0, next.to_le_bytes().to_vec(), next * 3);
                        let cold = Cold { at: model.4, ..Cold::EMPTY };
                        let id = ours.insert(Hot::new(model.0, Kind::Read, model.1), model.3.clone(), cold);
                        theirs.insert(next, model);
                        live.push((next, id));
                    }
                    // A completion of a request in flight, by its `user_data`: its progress moves.
                    0 | 1 => {
                        let (key, id) = live[rng.below(live.len() as u64) as usize];
                        let link = rng.below(4) as usize;
                        let (found, at) = of_data(data(id, link));
                        assert_eq!((found, at), (id, link));
                        let got = rng.below(100);
                        ours.get_mut(found).expect("in flight").done += got;
                        ours.buf_mut(found).push(got as u8);
                        let model = theirs.get_mut(&key).expect("in flight");
                        model.2 += got;
                        model.3.push(got as u8);
                    }
                    _ => {
                        let (key, id) = live.swap_remove(rng.below(live.len() as u64) as usize);
                        let (hot, buf, cold) = ours.remove(id).expect("in flight");
                        let model = theirs.remove(&key).expect("in flight");
                        assert_eq!((hot.tag, hot.of, hot.done, buf, cold.at), model);
                        ended.push(id);
                    }
                }
                assert_eq!(ours.len(), theirs.len());
                if !ended.is_empty() {
                    let id = ended[rng.below(ended.len() as u64) as usize];
                    let link = rng.below(4) as usize;
                    assert!(ours.get(of_data(data(id, link)).0).is_none(), "{id:?} ended");
                }
                assert!(ours.get(of_data(NOBODY).0).is_none());
            }
            for (key, id) in &live {
                assert_eq!(shown(&ours, *id).as_ref(), theirs.get(key));
            }
            let reused = ended.iter().any(|old| live.iter().any(|(_, it)| it.index() == old.index()));
            assert!(reused, "seed {seed}: no slot of an ended request holds another");
        }
    }

    /// Each request in flight, by its handle.
    fn requests(driver: &Uring) -> Vec<Id> {
        driver.ops.iter().map(|(id, _)| id).collect()
    }

    /// A completion that comes after its request ended, once the request's slot holds another
    /// (a read's, a sync's, an unwatched watch's read's), is told from the other's and dropped:
    /// it completes nothing, and the other goes on to its own end.
    #[test]
    fn a_completion_of_an_ended_request_is_stale() {
        let root = dir("uring-stale");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("file"), b"bytes").unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let file = FileName::new("file").unwrap();
        // Kept open from here, so each request on it is its own alone.
        run_until_idle(&mut driver, &mut vec![Effect::Sync { file: file.clone(), tag: Tag(0) }], |_, _| {});
        driver.submit(&mut vec![Effect::Sync { file: file.clone(), tag: Tag(1) }]);
        let ids = requests(&driver);
        let [synced] = ids[..] else { panic!("{ids:?}") };
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut Vec::new(), |done, _| seen.push(done.done));
        assert_eq!(seen, [Done::Synced]);
        driver.submit(&mut vec![Effect::Read { file: file.clone(), at: 0, len: 5, tag: Tag(2) }]);
        let ids = requests(&driver);
        let [read] = ids[..] else { panic!("{ids:?}") };
        assert_eq!(read.index(), synced.index(), "the sync's slot is the read's");
        assert_ne!(read, synced);
        driver.completed(data(synced, 0), 0, 0);
        driver.completed(data(synced, 0), -libc::EIO, 0);
        assert!(driver.ready.is_empty(), "{:?}", driver.ready);
        assert!(driver.ops.contains(read));
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut Vec::new(), |done, _| seen.push(done.done));
        assert_eq!(seen, [Done::Read(b"bytes".to_vec())]);

        let src = FileName::new("src").unwrap();
        driver.submit(&mut vec![Effect::Watch { dir: src.clone(), tag: Tag(3) }]);
        let watched = driver.watches[&Tag(3)];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut vec![Effect::Unwatch { tag: Tag(3) }], |done, _| seen.push(done.done));
        let (last, rest) = seen.split_last().expect("unwatched");
        assert_eq!(last, &Done::Unwatched);
        assert!(rest.iter().all(|it| matches!(it, Done::Changed { .. })), "{seen:?}");
        // The two slots let go (the read's and its cancel's), both taken again.
        driver.submit(&mut vec![Effect::Watch { dir: src, tag: Tag(4) }, Effect::Sync { file, tag: Tag(5) }]);
        let reused = requests(&driver).into_iter().find(|it| it.index() == watched.index());
        let reused = reused.expect("the watch's read's slot is taken again");
        assert_ne!(reused, watched);
        driver.completed(data(watched, 0), 16, 0);
        driver.completed(data(watched, 0), -libc::ECANCELED, 0);
        assert!(driver.ready.is_empty(), "{:?}", driver.ready);
        assert!(driver.ops.contains(reused) && driver.watches.contains_key(&Tag(4)));
        let mut seen = BTreeMap::new();
        run_until_idle(&mut driver, &mut vec![Effect::Unwatch { tag: Tag(4) }], |done, _| {
            _ = seen.insert(done.tag.0, done.done)
        });
        assert_eq!((seen.get(&4), seen.get(&5)), (Some(&Done::Unwatched), Some(&Done::Synced)));
        assert_eq!(driver.in_flight(), 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Polls `driver` into `out` until `enough` says it holds what is waited for, or ten
    /// seconds pass.
    fn poll_until(driver: &mut Uring, out: &mut Vec<Completion>, mut enough: impl FnMut(&[Completion]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !enough(out) && Instant::now() < deadline {
            driver.poll(out, Duration::from_millis(50));
        }
    }

    /// The rounds before this one open what is kept and grow what is reused.
    const WARM: u64 = 3;

    /// docs/v2/plan/lanes/mem/core/linux.md's budget: a warm sync, stat and read of a file kept
    /// open allocate the read's bytes and nothing else.
    #[test]
    fn a_warm_cycle_allocates_the_read_alone() {
        let root = dir("uring-warm-file");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("cached"), b"cached bytes").unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let file = FileName::new("cached").unwrap();
        let mut out = Vec::with_capacity(16);
        for round in 0..2 * WARM {
            let mut effects = Vec::with_capacity(4);
            effects.push(Effect::Sync { file: file.clone(), tag: Tag(3 * round) });
            effects.push(Effect::Stat { file: file.clone(), tag: Tag(3 * round + 1) });
            effects.push(Effect::Read { file: file.clone(), at: 0, len: 6, tag: Tag(3 * round + 2) });
            out.clear();
            let ((), used) = blaze_alloc_count::measure(|| {
                driver.submit(&mut effects);
                poll_until(&mut driver, &mut out, |out| out.len() == 3);
            });
            out.sort_by_key(|it| it.tag.0);
            let [synced, stat, read] = &out[..] else { panic!("{out:?}") };
            assert_eq!((&synced.done, &read.done), (&Done::Synced, &Done::Read(b"cached".to_vec())));
            assert!(matches!(stat.done, Done::Stat(Some(_))), "{stat:?}");
            if round >= WARM {
                assert!(used.allocations <= 1, "round {round}: {} allocations ({} bytes), budget 1", used.allocations, used.bytes);
            }
        }
        assert_eq!(driver.in_flight(), 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A warm timer armed and fired allocates nothing.
    #[test]
    fn a_warm_timer_allocates_nothing() {
        let mut driver = Uring::new(dir("uring-warm-timer")).unwrap();
        let mut out = Vec::with_capacity(4);
        for round in 0..2 * WARM {
            let mut effects = Vec::with_capacity(1);
            effects.push(Effect::Timer { after: Duration::from_millis(1), tag: Tag(round) });
            out.clear();
            let ((), used) = blaze_alloc_count::measure(|| {
                driver.submit(&mut effects);
                poll_until(&mut driver, &mut out, |out| !out.is_empty());
            });
            assert_eq!(out.iter().map(|it| (it.tag, &it.done)).collect::<Vec<_>>(), [(Tag(round), &Done::Fired)]);
            if round >= WARM {
                assert_eq!(used.allocations, 0, "round {round}: {} bytes", used.bytes);
            }
        }
        assert_eq!(driver.in_flight(), 0);
    }

    /// A warm watch added, told of a file written, and removed allocates the payload of its
    /// [`Done::Changed`]s (each one's list and names) and nothing else: its tables, its buffer
    /// and its list of flushes are those of the watch before it.
    #[test]
    fn a_warm_watch_allocates_its_changes_alone() {
        let root = dir("uring-warm-watch");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let src = FileName::new("src").unwrap();
        let mut out = Vec::with_capacity(64);
        for round in 0..2 * WARM {
            let tag = Tag(round);
            let written = FileName::new(&format!("src/f{round}.rs")).unwrap();
            let path = root.join(written.as_str());
            let (mut watch, mut unwatch) = (Vec::with_capacity(1), Vec::with_capacity(1));
            watch.push(Effect::Watch { dir: src.clone(), tag });
            unwatch.push(Effect::Unwatch { tag });
            out.clear();
            let heard = |out: &[Completion]| {
                out.iter().any(|it| matches!(&it.done, Done::Changed { files, .. } if files.contains(&written)))
            };
            let unwatched = |out: &[Completion]| out.iter().any(|it| it.done == Done::Unwatched);
            let ((), added) = blaze_alloc_count::measure(|| driver.submit(&mut watch));
            std::fs::write(&path, b"fn f() {}").unwrap();
            let ((), told) = blaze_alloc_count::measure(|| poll_until(&mut driver, &mut out, heard));
            let ((), removed) = blaze_alloc_count::measure(|| {
                driver.submit(&mut unwatch);
                poll_until(&mut driver, &mut out, unwatched);
            });
            assert!(heard(&out) && unwatched(&out), "{out:?}");
            assert!(out.iter().all(|it| it.tag == tag), "{out:?}");
            let payload: u64 = out
                .iter()
                .map(|it| match &it.done {
                    Done::Changed { files, .. } if !files.is_empty() => 1 + files.len() as u64,
                    _ => 0,
                })
                .sum();
            let used = added.allocations + told.allocations + removed.allocations;
            if round >= WARM {
                assert!(
                    used <= payload,
                    "round {round}: {used} allocations (the add {}, its changes {}, the remove {}), \
                     the changes' payload {payload}",
                    added.allocations,
                    told.allocations,
                    removed.allocations
                );
            }
        }
        assert_eq!(driver.in_flight(), 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A warm send on a connection and a receive of it on the other side, each handed the
    /// buffer the last gave back, allocate nothing.
    #[test]
    fn a_warm_send_and_receive_into_buffers_handed_back_allocate_nothing() {
        let root = dir("uring-warm-conn");
        let mut ring = Uring::new(&root).unwrap();
        let at = Address::Unix(FileName::new("s.sock").unwrap());
        ring.submit(&mut vec![Effect::Listen { at: at.clone(), tag: Tag(1) }, Effect::Connect { at, tag: Tag(2) }]);
        let (mut server, mut client) = (None, None);
        let mut out = Vec::with_capacity(16);
        poll_until(&mut ring, &mut out, |out| {
            out.iter().any(|it| matches!(it.done, Done::Accepted { .. }))
                && out.iter().any(|it| matches!(it.done, Done::Connected { .. }))
        });
        for it in out.drain(..) {
            match it.done {
                Done::Accepted { conn } => server = Some(conn),
                Done::Connected { conn } => client = Some(conn),
                other => panic!("{other:?}"),
            }
        }
        let (server, client) = (server.expect("accepted"), client.expect("connected"));
        let (mut sending, mut receiving) = (Vec::with_capacity(64), Vec::with_capacity(64));
        let mut effects = Vec::with_capacity(8);
        for round in 0..2 * WARM {
            out.clear();
            let ((), used) = blaze_alloc_count::measure(|| {
                sending.extend_from_slice(b"ping");
                effects.push(Effect::Send { conn: client, bytes: std::mem::take(&mut sending), tag: Tag(10 + 2 * round) });
                effects.push(Effect::Recv {
                    conn: server,
                    most: 64,
                    into: std::mem::take(&mut receiving),
                    tag: Tag(11 + 2 * round),
                });
                ring.submit(&mut effects);
                let deadline = Instant::now() + Duration::from_secs(10);
                let (mut seen, mut sent, mut read) = (0, false, false);
                while (!read || !sent) && Instant::now() < deadline {
                    ring.poll(&mut out, Duration::from_millis(50));
                    for it in &mut out[seen..] {
                        match &mut it.done {
                            Done::Sent(bytes) => {
                                sent = true;
                                sending = std::mem::take(bytes);
                            }
                            Done::Read(bytes) if bytes.len() < 4 => {
                                let into = std::mem::take(bytes);
                                effects.push(Effect::Recv { conn: server, most: 64, into, tag: it.tag });
                            }
                            Done::Read(bytes) => {
                                read = true;
                                receiving = std::mem::take(bytes);
                            }
                            _ => {}
                        }
                    }
                    seen = out.len();
                    if !effects.is_empty() {
                        ring.submit(&mut effects);
                    }
                }
            });
            assert!(out.iter().all(|it| matches!(it.done, Done::Sent(_) | Done::Read(_))), "{out:?}");
            assert_eq!((&receiving[..], sending.len(), sending.capacity() >= 4), (&b"ping"[..], 0, true), "{out:?}");
            receiving.clear();
            if round >= WARM {
                assert_eq!(used.allocations, 0, "round {round}");
            }
        }
        // Bits no connection was given (an even generation, as a free slot may have) name none.
        let forged = Conn(server.0 ^ 1 << 32);
        let mut seen = Vec::new();
        let mut send = vec![Effect::Send { conn: forged, bytes: b"x".to_vec(), tag: Tag(5) }];
        run_until_idle(&mut ring, &mut send, |done, _| seen.push(done.done));
        assert!(matches!(&seen[..], [Done::Broken { buffer, .. }] if buffer.is_empty()), "{seen:?}");
        let mut effects = vec![
            Effect::Hangup { conn: client, tag: Tag(3) },
            Effect::Hangup { conn: server, tag: Tag(4) },
            Effect::Unlisten { tag: Tag(1) },
        ];
        run_until_idle(&mut ring, &mut effects, |_, _| {});
        assert_eq!(ring.in_flight(), 0);
        assert!(ring.conns.is_empty());
        drop(ring);
        _ = std::fs::remove_dir_all(root);
    }

    /// The calls of this thread that open, make, link, rename or unlink a name, counted from
    /// [`path_calls::forbid`] on: each traps (`SECCOMP_RET_TRAP`), is counted and fails with
    /// `EPERM`, as the budget tests count allocations. The ring's requests are no calls of the
    /// thread's, so they pass.
    mod path_calls {
        use std::sync::atomic::{AtomicU64, Ordering};

        static TRAPPED: AtomicU64 = AtomicU64::new(0);

        #[cfg(target_arch = "x86_64")]
        const ARCH: u32 = 0xc000_003e; // AUDIT_ARCH_X86_64
        #[cfg(target_arch = "aarch64")]
        const ARCH: u32 = 0xc000_00b7; // AUDIT_ARCH_AARCH64
        #[cfg(target_arch = "x86_64")]
        const CALLS: &[libc::c_long] = &[
            libc::SYS_open,
            libc::SYS_creat,
            libc::SYS_openat,
            libc::SYS_openat2,
            libc::SYS_mkdir,
            libc::SYS_mkdirat,
            libc::SYS_rmdir,
            libc::SYS_unlink,
            libc::SYS_unlinkat,
            libc::SYS_link,
            libc::SYS_linkat,
            libc::SYS_rename,
            libc::SYS_renameat,
            libc::SYS_renameat2,
        ];
        #[cfg(target_arch = "aarch64")]
        const CALLS: &[libc::c_long] = &[
            libc::SYS_openat,
            libc::SYS_openat2,
            libc::SYS_mkdirat,
            libc::SYS_unlinkat,
            libc::SYS_linkat,
            libc::SYS_renameat,
            libc::SYS_renameat2,
        ];

        extern "C" fn trapped(_: libc::c_int, _: *mut libc::siginfo_t, context: *mut libc::c_void) {
            TRAPPED.fetch_add(1, Ordering::Relaxed);
            let context = context.cast::<libc::ucontext_t>();
            // SAFETY: the context the kernel hands an `SA_SIGINFO` handler, of the thread whose
            // call trapped; the register written is the one the call returns in.
            #[cfg(target_arch = "x86_64")]
            unsafe {
                (*context).uc_mcontext.gregs[libc::REG_RAX as usize] = -i64::from(libc::EPERM)
            };
            // SAFETY: as above.
            #[cfg(target_arch = "aarch64")]
            unsafe {
                (*context).uc_mcontext.regs[0] = (-i64::from(libc::EPERM)) as u64
            };
        }

        /// From now on, this thread's calls on names trap; it should be one of the test's own.
        pub(super) fn forbid() {
            static HANDLER: std::sync::Once = std::sync::Once::new();
            HANDLER.call_once(|| {
                // SAFETY: `sigaction` is a C struct of integers, pointers and a mask, for which
                // all-zero bytes is a valid value; the fields read are set below.
                let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                action.sa_sigaction = trapped as usize;
                action.sa_flags = libc::SA_SIGINFO;
                // SAFETY: a handler that touches an atomic and its context alone.
                assert_eq!(unsafe { libc::sigaction(libc::SIGSYS, &action, std::ptr::null_mut()) }, 0);
            });
            const LD_W_ABS: u16 = 0x20;
            const JEQ_K: u16 = 0x15;
            const RET_K: u16 = 0x06;
            const ALLOW: u32 = 0x7fff_0000;
            const TRAP: u32 = 0x0003_0000;
            let insn = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
            let mut program = vec![
                insn(LD_W_ABS, 0, 0, 4), // the architecture
                insn(JEQ_K, 1, 0, ARCH),
                insn(RET_K, 0, 0, ALLOW),
                insn(LD_W_ABS, 0, 0, 0), // the call
            ];
            for &call in CALLS {
                program.extend([insn(JEQ_K, 0, 1, call as u32), insn(RET_K, 0, 0, TRAP)]);
            }
            program.push(insn(RET_K, 0, 0, ALLOW));
            let fprog = libc::sock_fprog { len: program.len() as u16, filter: program.as_mut_ptr() };
            // SAFETY: no pointers.
            assert_eq!(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }, 0);
            // SAFETY: a program of `len` instructions, which the call copies and does not keep.
            let set = unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &raw const fprog) };
            assert_eq!(set, 0, "{}", std::io::Error::last_os_error());
        }

        pub(super) fn count() -> u64 {
            TRAPPED.load(Ordering::Relaxed)
        }
    }

    /// A warm append, positioned write, whole file's write and rename allocate nothing and
    /// call nothing on a name: their bytes are the asker's, their names go in an arena, and
    /// every open, rename and unlink is a request of the ring's, each open into a slot of the
    /// table that a close through the ring empties after.
    #[test]
    fn warm_writes_and_renames_allocate_nothing() {
        let root = dir("uring-warm-writes");
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/block"), [0u8; 64]).unwrap();
        // On a thread of its own: the filter that counts calls on names stays with it.
        let ours = root.clone();
        std::thread::spawn(move || {
            let mut driver = Uring::new(&ours).unwrap();
            let name = |it: &str| FileName::new(it).unwrap();
            let (log, block, whole, moved) = (name("d/log"), name("d/block"), name("d/whole"), name("d/moved"));
            let mut out = Vec::with_capacity(16);
            for round in 0..2 * WARM {
                if round == WARM {
                    path_calls::forbid();
                }
                let at = 8 * round;
                let mut writes = Vec::with_capacity(4);
                writes.push(Effect::Append { file: log.clone(), bytes: b"line\n".to_vec(), tag: Tag(10 * round) });
                let bytes = round.to_le_bytes().to_vec();
                writes.push(Effect::WriteAt { file: block.clone(), at, bytes, tag: Tag(10 * round + 1) });
                let bytes = round.to_le_bytes().to_vec();
                writes.push(Effect::Write { file: whole.clone(), bytes, tag: Tag(10 * round + 2) });
                let mut renames = Vec::with_capacity(1);
                renames.push(Effect::Rename { from: whole.clone(), to: moved.clone(), tag: Tag(10 * round + 3) });
                out.clear();
                let calls = path_calls::count();
                let ((), used) = blaze_alloc_count::measure(|| {
                    driver.submit(&mut writes);
                    poll_until(&mut driver, &mut out, |out| out.len() == 3);
                    driver.submit(&mut renames);
                    poll_until(&mut driver, &mut out, |out| out.len() == 4);
                });
                let calls = path_calls::count() - calls;
                out.sort_by_key(|it| it.tag.0);
                let done: Vec<&Done> = out.iter().map(|it| &it.done).collect();
                assert_eq!(done, [&Done::Appended, &Done::Written, &Done::Written, &Done::Renamed], "round {round}");
                if round >= WARM {
                    assert_eq!(used.allocations, 0, "round {round}: {} bytes", used.bytes);
                    assert_eq!(calls, 0, "round {round}: calls on names on the driver's thread");
                }
            }
            assert_eq!((driver.paths.held(), driver.slots_held), ([0, 0], 0));
            assert_eq!(driver.in_flight(), 0);
        })
        .join()
        .unwrap();
        let last = 2 * WARM - 1;
        assert_eq!(std::fs::read(root.join("d/moved")).unwrap(), last.to_le_bytes());
        assert_eq!(std::fs::read(root.join("d/block")).unwrap()[8 * last as usize..][..8], last.to_le_bytes());
        assert_eq!(std::fs::read(root.join("d/log")).unwrap().len(), 5 * 2 * WARM as usize);
        assert!(!root.join("d/whole").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// An open through the ring that fails ends its request failed, the error naming the path
    /// it opened, and every link after it cancelled: a write at an offset of a missing file, a
    /// whole file's write and a rename into a directory that is a file, a read of a file under
    /// one. Each slot it took is free again after, and the files are as they were.
    #[test]
    fn a_failed_open_fails_its_chain_naming_the_path() {
        let root = dir("uring-open-fails");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("plain"), b"x").unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let free = driver.table.free.borrow().len();
        let name = |it: &str| FileName::new(it).unwrap();
        let mut effects = vec![
            Effect::WriteAt { file: name("missing"), at: 0, bytes: b"a".to_vec(), tag: Tag(1) },
            Effect::Write { file: name("plain/under"), bytes: b"b".to_vec(), tag: Tag(2) },
            Effect::Rename { from: name("plain"), to: name("plain/under"), tag: Tag(3) },
            Effect::Read { file: name("plain/under"), at: 0, len: 1, tag: Tag(4) },
        ];
        let mut seen = BTreeMap::new();
        run_until_idle(&mut driver, &mut effects, |done, _| _ = seen.insert(done.tag.0, done.done));
        let opened = |tag: u64, path: PathBuf| match &seen[&tag] {
            Done::Failed(IoError(why)) => {
                let path = path.display().to_string();
                assert!(why.starts_with(&format!("opening {path}: ")), "{tag}: {why} (wanted {path})");
            }
            other => panic!("{tag}: {other:?}"),
        };
        opened(1, root.join("missing"));
        opened(2, root.join("plain"));
        opened(3, root.join("plain"));
        opened(4, root.join("plain/under"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while (driver.table.free.borrow().len() < free || driver.closes > 0) && Instant::now() < deadline {
            driver.poll(&mut Vec::new(), Duration::from_millis(1));
        }
        assert_eq!((driver.table.free.borrow().len(), driver.closes, driver.slots_held), (free, 0, 0));
        assert_eq!(driver.paths.held(), [0, 0]);
        drop(driver);
        assert!(!root.join("missing").exists());
        assert_eq!(std::fs::read(root.join("plain")).unwrap(), b"x");
        std::fs::remove_dir_all(root).unwrap();
    }

    fn read_all(driver: &mut Uring, mut effects: Vec<Effect>) -> BTreeMap<u64, Done> {
        let mut seen = BTreeMap::new();
        run_until_idle(driver, &mut effects, |done, _| _ = seen.insert(done.tag.0, done.done));
        seen
    }

    /// A read of a file no request keeps is of the file at its name when it is asked, as the
    /// threads driver's: after a new file is renamed over the name (an editor's save, a
    /// checkout), the next read has the new bytes. The read keeps no descriptor, slot or name.
    #[test]
    fn a_read_is_of_the_file_at_its_name_now() {
        let root = dir("uring-read-now");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("src.rs"), b"old").unwrap();
        std::fs::write(root.join("empty"), b"").unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let free = driver.table.free.borrow().len();
        let name = |it: &str| FileName::new(it).unwrap();
        let seen = read_all(
            &mut driver,
            vec![
                Effect::ReadAll { file: name("src.rs"), tag: Tag(1) },
                Effect::Read { file: name("src.rs"), at: 1, len: 10, tag: Tag(2) },
                Effect::Read { file: name("src.rs"), at: 9, len: 1, tag: Tag(3) },
                Effect::ReadAll { file: name("missing"), tag: Tag(4) },
                Effect::ReadAll { file: name("empty"), tag: Tag(5) },
            ],
        );
        let reads = |seen: &BTreeMap<u64, Done>| -> Vec<Vec<u8>> {
            seen.values()
                .map(|it| match it {
                    Done::Read(bytes) => bytes.clone(),
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        assert_eq!(reads(&seen), [&b"old"[..], b"ld", b"", b"", b""]);
        std::fs::write(root.join("new"), b"new contents").unwrap();
        std::fs::rename(root.join("new"), root.join("src.rs")).unwrap();
        let seen = read_all(
            &mut driver,
            vec![
                Effect::ReadAll { file: name("src.rs"), tag: Tag(1) },
                Effect::Read { file: name("src.rs"), at: 4, len: 8, tag: Tag(2) },
            ],
        );
        assert_eq!(reads(&seen), [&b"new contents"[..], b"contents"]);
        assert_eq!(reads(&seen)[0], std::fs::read(root.join("src.rs")).unwrap());
        assert!(driver.kept.is_empty() && driver.names.is_empty());
        assert_eq!((driver.loading, driver.load_waiting.len()), (0, 0));
        assert_eq!((driver.table.free.borrow().len(), driver.slots_held), (free, 0));
        assert_eq!(driver.paths.held(), [0, 0]);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Reads of more files than may be open at once hold at most [`Uring::loads_most`]
    /// descriptors, the rest waiting in the order asked; once [`files::SPREAD_LEAST`] are open
    /// their opens go to the kernel's workers. Each read has what `std::fs::read` has.
    #[test]
    fn reads_of_many_files_hold_few_descriptors() {
        let root = dir("uring-many-reads");
        std::fs::create_dir_all(root.join("f")).unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let most = driver.loads_most();
        let count = 2 * most + 5;
        for n in 0..count {
            std::fs::write(root.join(format!("f/{n}")), n.to_string().repeat(n % 7)).unwrap();
        }
        let mut effects: Vec<Effect> = (0..count)
            .map(|n| Effect::ReadAll { file: FileName::new(&format!("f/{n}")).unwrap(), tag: Tag(n as u64) })
            .collect();
        driver.submit(&mut effects);
        assert_eq!((driver.loading, driver.load_waiting.len()), (most, count - most));
        let open = (driver.ops.iter())
            .find(|(id, hot)| hot.kind == Kind::Load && !driver.load_waiting.contains(id))
            .map(|(id, _)| id)
            .expect("an admitted read");
        let mut sqes = [Sqe::default(); ops::LINKS];
        driver.path_sqes(open, &mut sqes);
        assert_eq!(sqes[0].flags & SQE_ASYNC != 0, most >= files::SPREAD_LEAST);
        let (mut out, mut seen) = (Vec::new(), BTreeMap::new());
        let deadline = Instant::now() + Duration::from_secs(30);
        while driver.in_flight() > 0 && Instant::now() < deadline {
            driver.poll(&mut out, Duration::from_millis(50));
            assert!(driver.loading <= most, "{} reads open of {most}", driver.loading);
            seen.extend(out.drain(..).map(|it| (it.tag.0 as usize, it.done)));
        }
        assert_eq!(seen.len(), count);
        for (n, done) in seen {
            assert_eq!(done, Done::Read(std::fs::read(root.join(format!("f/{n}"))).unwrap()), "f/{n}");
        }
        assert_eq!((driver.loading, driver.load_waiting.len()), (0, 0));
        assert_eq!(driver.paths.held(), [0, 0]);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A warm read of a file no request keeps allocates its result alone and calls nothing on
    /// a name on the driver's thread: the open is the ring's, the size an `fstat` of the
    /// descriptor it opened.
    #[test]
    fn warm_reads_allocate_their_bytes_alone() {
        let root = dir("uring-warm-reads");
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/src.rs"), b"fn main() {}\n").unwrap();
        let ours = root.clone();
        std::thread::spawn(move || {
            let mut driver = Uring::new(&ours).unwrap();
            let file = FileName::new("d/src.rs").unwrap();
            let mut out = Vec::with_capacity(16);
            for round in 0..2 * WARM {
                if round == WARM {
                    path_calls::forbid();
                }
                let mut reads = Vec::with_capacity(2);
                reads.push(Effect::ReadAll { file: file.clone(), tag: Tag(2 * round) });
                reads.push(Effect::Read { file: file.clone(), at: 3, len: 4, tag: Tag(2 * round + 1) });
                out.clear();
                let calls = path_calls::count();
                let ((), used) = blaze_alloc_count::measure(|| {
                    driver.submit(&mut reads);
                    poll_until(&mut driver, &mut out, |out| out.len() == 2);
                });
                let calls = path_calls::count() - calls;
                out.sort_by_key(|it| it.tag.0);
                let done: Vec<&Done> = out.iter().map(|it| &it.done).collect();
                assert_eq!(done, [&Done::Read(b"fn main() {}\n".to_vec()), &Done::Read(b"main".to_vec())]);
                if round >= WARM {
                    assert!(used.allocations <= 2, "round {round}: {} allocations", used.allocations);
                    assert_eq!(calls, 0, "round {round}: calls on names on the driver's thread");
                }
            }
            assert!(driver.kept.is_empty());
            assert_eq!((driver.paths.held(), driver.loading), ([0, 0], 0));
        })
        .join()
        .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The provided buffers a pass of completions gives back are shown to the kernel together,
    /// with one store of the ring's tail after the pass, not one per buffer; between, the
    /// kernel's tail is as it was.
    #[test]
    fn provided_buffers_go_back_once_per_pass() {
        let root = dir("uring-provided");
        let mut ring = Uring::new(&root).unwrap();
        assert_eq!((ring.provided.unpublished(), ring.provided.shown()), (0, RECV_BUFFERS));
        let at = Address::Unix(FileName::new("s.sock").unwrap());
        ring.submit(&mut vec![Effect::Listen { at: at.clone(), tag: Tag(1) }, Effect::Connect { at, tag: Tag(2) }]);
        let mut out = Vec::new();
        poll_until(&mut ring, &mut out, |out| out.len() == 2);
        let conn = |it: &Done| match it {
            Done::Accepted { conn } | Done::Connected { conn } => *conn,
            other => panic!("{other:?}"),
        };
        out.sort_by_key(|it| it.tag.0);
        let (server, client) = (conn(&out[0].done), conn(&out[1].done));
        let sent = 5 * RECV_BUFFER / 2;
        ring.submit(&mut vec![
            Effect::Recv { conn: server, most: 64 << 10, into: Vec::new(), tag: Tag(3) },
            Effect::Send { conn: client, bytes: vec![7; sent], tag: Tag(4) },
        ]);
        let mut received = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        while received < sent && Instant::now() < deadline {
            ring.submit_ring();
            ring.ring.enter(Some(Duration::from_millis(50))).unwrap();
            let shown = ring.provided.shown();
            let (first, tail) = ring.ring.completions();
            let (mut head, mut given) = (first, 0);
            while head != tail {
                let (user_data, res, flags) = ring.ring.cqe(head);
                head = head.wrapping_add(1);
                if flags & CQE_F_BUFFER != 0 && res > 0 {
                    received += res as usize;
                    given += 1;
                }
                ring.completed(user_data, res, flags);
            }
            if head != first {
                ring.ring.consume(head);
            }
            assert_eq!((ring.provided.unpublished(), ring.provided.shown()), (given, shown));
            ring.provided.publish();
            assert_eq!((ring.provided.unpublished(), ring.provided.shown()), (0, shown.wrapping_add(given)));
        }
        assert_eq!(received, sent);
        let mut effects = vec![
            Effect::Hangup { conn: client, tag: Tag(5) },
            Effect::Hangup { conn: server, tag: Tag(6) },
            Effect::Unlisten { tag: Tag(1) },
        ];
        run_until_idle(&mut ring, &mut effects, |_, _| {});
        assert_eq!(ring.in_flight(), 0);
        drop(ring);
        _ = std::fs::remove_dir_all(root);
    }

    /// A large send on a TCP connection goes from the asker's pages: it ends once the kernel's
    /// notification says it is done with them, received whole, its buffer handed back empty.
    /// A small one copies.
    #[test]
    fn a_large_tcp_send_goes_from_the_askers_pages() {
        let mut ring = Uring::new(dir("uring-zero-copy")).unwrap();
        ring.submit(&mut vec![Effect::Listen { at: Address::Tcp("127.0.0.1:0".parse().unwrap()), tag: Tag(1) }]);
        let at = ring.local_addr(Tag(1)).expect("listening");
        ring.submit(&mut vec![Effect::Connect { at: Address::Tcp(at), tag: Tag(2) }]);
        let mut out = Vec::new();
        poll_until(&mut ring, &mut out, |out| out.len() == 2);
        let conn = |it: &Done| match it {
            Done::Accepted { conn } | Done::Connected { conn } => *conn,
            other => panic!("{other:?}"),
        };
        out.sort_by_key(|it| it.tag.0);
        let (server, client) = (conn(&out[0].done), conn(&out[1].done));
        let large: Vec<u8> = (0..4 << 20).map(|at: usize| (at * 13) as u8).collect();
        let whole = large.len() + 4;
        // The small send only once the large one ended: sends in flight together may interleave.
        let mut effects = vec![
            Effect::Send { conn: client, bytes: large.clone(), tag: Tag(3) },
            Effect::Recv { conn: server, most: 64 << 10, into: Vec::new(), tag: Tag(10) },
        ];
        let (mut got, mut sent, mut next) = (Vec::new(), Vec::new(), 11);
        run_until_idle(&mut ring, &mut effects, |done, effects| match done.done {
            Done::Sent(bytes) => {
                sent.push((done.tag, bytes.len(), bytes.capacity()));
                if done.tag == Tag(3) {
                    effects.push(Effect::Send { conn: client, bytes: b"tail".to_vec(), tag: Tag(4) });
                }
            }
            Done::Read(mut bytes) => {
                got.extend_from_slice(&bytes);
                bytes.clear();
                if got.len() < whole {
                    effects.push(Effect::Recv { conn: server, most: 64 << 10, into: bytes, tag: Tag(next) });
                    next += 1;
                } else {
                    effects.push(Effect::Hangup { conn: client, tag: Tag(5) });
                    effects.push(Effect::Hangup { conn: server, tag: Tag(6) });
                    effects.push(Effect::Unlisten { tag: Tag(1) });
                }
            }
            Done::HungUp | Done::Unlistened => {}
            other => panic!("{other:?}"),
        });
        assert_eq!(got.len(), whole);
        assert!(got[..large.len()] == large[..] && got[large.len()..] == b"tail"[..]);
        assert_eq!(sent.iter().map(|it| (it.0, it.1)).collect::<Vec<_>>(), [(Tag(3), 0), (Tag(4), 0)]);
        assert!(sent[0].2 >= large.len(), "the asker's own buffer handed back");
        assert_eq!(ring.zero_copy_sends(), 1);
        assert_eq!(ring.in_flight(), 0);
    }

    #[test]
    fn a_ring_refused_by_seccomp_says_what_failed_why_and_the_fix() {
        let error = io::Error::from_raw_os_error(libc::EPERM);
        assert_eq!(
            super::setup_refused(libc::EPERM, &error, Some(0), "6.8.0"),
            "blaze runs its build on io_uring on Linux and cannot start without it: \
             io_uring_setup failed with EPERM (Operation not permitted (os error 1)). \
             Likely cause: a seccomp policy that refuses io_uring_setup, such as Docker's \
             default profile (Docker 25 and later) or a systemd unit's SystemCallFilter. \
             Fix: run under a seccomp profile that allows io_uring_setup, io_uring_enter and \
             io_uring_register (Docker: --security-opt seccomp=<that profile>)."
        );
    }

    #[test]
    fn each_refusal_names_its_errno_cause_and_fix() {
        let says = |errno: i32, disabled: Option<u8>| {
            super::setup_refused(errno, &io::Error::from_raw_os_error(errno), disabled, "5.0.0")
        };
        let cases: [(i32, Option<u8>, &str, &str, &str); 7] = [
            (libc::EPERM, Some(2), "with EPERM", "kernel.io_uring_disabled is 2", "sysctl -w kernel.io_uring_disabled=0"),
            (libc::EPERM, Some(1), "with EPERM", "kernel.io_uring_group", "sysctl -w kernel.io_uring_disabled=0"),
            (libc::EPERM, None, "with EPERM", "seccomp", "io_uring_setup, io_uring_enter and io_uring_register"),
            (libc::ENOSYS, None, "with ENOSYS", "Linux 5.0.0", "Linux 6.0 or later"),
            (libc::EINVAL, None, "with EINVAL", "IORING_SETUP_SINGLE_ISSUER", "Linux 6.0 or later"),
            (libc::ENOMEM, None, "with ENOMEM", "RLIMIT_MEMLOCK", "ulimit -l"),
            (libc::EBADF, None, "with errno 9", "not known", "kernel.io_uring_disabled is 0"),
        ];
        for (errno, disabled, failed, cause, fix) in cases {
            let text = says(errno, disabled);
            let (head, rest) = text.split_once(" Likely cause: ").expect("a cause");
            let (because, how) = rest.split_once(" Fix: ").expect("a fix");
            assert!(head.contains(failed), "{text}");
            assert!(because.contains(cause), "{text}");
            assert!(how.contains(fix), "{text}");
        }
    }
}
