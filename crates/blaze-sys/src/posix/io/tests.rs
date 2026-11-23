//! The comparison of this area with the `libc` crate: every layout, constant and declared C
//! signature here against libc's, once, and a few calls made for real. Deleted with the
//! `libc` dev-dependency (D180).

use core::ffi::{c_int, c_void};
use core::mem::offset_of;

use super::*;
use crate::Fd;

/// The flag sets' members and constants are libc's numbers.
#[test]
fn the_constants_are_libcs() {
    let pairs: &[(&str, i64, i64)] = &[
        ("PROT_READ", Prot::READ.bits() as i64, libc::PROT_READ as i64),
        ("PROT_WRITE", Prot::WRITE.bits() as i64, libc::PROT_WRITE as i64),
        ("PROT_EXEC", Prot::EXEC.bits() as i64, libc::PROT_EXEC as i64),
        ("PROT_NONE", Prot::NONE.bits() as i64, libc::PROT_NONE as i64),
        ("MAP_SHARED", MapFlags::SHARED.bits() as i64, libc::MAP_SHARED as i64),
        ("MAP_PRIVATE", MapFlags::PRIVATE.bits() as i64, libc::MAP_PRIVATE as i64),
        ("MAP_FIXED", MapFlags::FIXED.bits() as i64, libc::MAP_FIXED as i64),
        ("MAP_ANON", MapFlags::ANONYMOUS.bits() as i64, libc::MAP_ANON as i64),
        ("MAP_ANONYMOUS", MapFlags::ANONYMOUS.bits() as i64, libc::MAP_ANONYMOUS as i64),
        ("MAP_NORESERVE", MapFlags::NO_RESERVE.bits() as i64, libc::MAP_NORESERVE as i64),
        ("MS_ASYNC", MsyncFlags::ASYNC.bits() as i64, libc::MS_ASYNC as i64),
        ("MS_INVALIDATE", MsyncFlags::INVALIDATE.bits() as i64, libc::MS_INVALIDATE as i64),
        ("MS_SYNC", MsyncFlags::SYNC.bits() as i64, libc::MS_SYNC as i64),
        ("POLLIN", PollEvents::IN.bits() as i64, libc::POLLIN as i64),
        ("POLLPRI", PollEvents::PRI.bits() as i64, libc::POLLPRI as i64),
        ("POLLOUT", PollEvents::OUT.bits() as i64, libc::POLLOUT as i64),
        ("POLLERR", PollEvents::ERR.bits() as i64, libc::POLLERR as i64),
        ("POLLHUP", PollEvents::HUP.bits() as i64, libc::POLLHUP as i64),
        ("POLLNVAL", PollEvents::NVAL.bits() as i64, libc::POLLNVAL as i64),
        ("O_RDONLY", OpenFlags::READ.bits() as i64, libc::O_RDONLY as i64),
        ("O_WRONLY", OpenFlags::WRITE.bits() as i64, libc::O_WRONLY as i64),
        ("O_RDWR", OpenFlags::READ_WRITE.bits() as i64, libc::O_RDWR as i64),
        ("O_CREAT", OpenFlags::CREATE.bits() as i64, libc::O_CREAT as i64),
        ("O_EXCL", OpenFlags::EXCLUSIVE.bits() as i64, libc::O_EXCL as i64),
        ("O_NOCTTY", OpenFlags::NO_CONTROLLING_TTY.bits() as i64, libc::O_NOCTTY as i64),
        ("O_APPEND", OpenFlags::APPEND.bits() as i64, libc::O_APPEND as i64),
        ("O_NONBLOCK", OpenFlags::NONBLOCK.bits() as i64, libc::O_NONBLOCK as i64),
        ("O_DIRECTORY", OpenFlags::DIRECTORY.bits() as i64, libc::O_DIRECTORY as i64),
        ("O_NOFOLLOW", OpenFlags::NO_FOLLOW.bits() as i64, libc::O_NOFOLLOW as i64),
        ("O_CLOEXEC", OpenFlags::CLOSE_ON_EXEC.bits() as i64, libc::O_CLOEXEC as i64),
        ("AT_FDCWD", CWD.raw() as i64, libc::AT_FDCWD as i64),
        ("F_DUPFD", fd::F_DUPFD as i64, libc::F_DUPFD as i64),
        ("F_GETFD", fd::F_GETFD as i64, libc::F_GETFD as i64),
        ("F_SETFD", fd::F_SETFD as i64, libc::F_SETFD as i64),
        ("F_GETFL", fd::F_GETFL as i64, libc::F_GETFL as i64),
        ("F_SETFL", fd::F_SETFL as i64, libc::F_SETFL as i64),
        ("F_DUPFD_CLOEXEC", fd::F_DUPFD_CLOEXEC as i64, libc::F_DUPFD_CLOEXEC as i64),
        ("FD_CLOEXEC", fd::FD_CLOEXEC as i64, libc::FD_CLOEXEC as i64),
        ("FIONBIO", FIONBIO as i64, libc::FIONBIO as i64),
        ("SEEK_DATA", Whence::Data as i64, libc::SEEK_DATA as i64),
        ("SEEK_HOLE", Whence::Hole as i64, libc::SEEK_HOLE as i64),
        ("GETENTROPY_MAX", GETENTROPY_MAX as i64, 256),
    ];
    for &(name, ours, libcs) in pairs {
        assert_eq!(ours, libcs, "{name}");
    }
    let os: &[(&str, i64, i64)] = &os_constants();
    for &(name, ours, libcs) in os {
        assert_eq!(ours, libcs, "{name}");
    }
}

#[cfg(target_os = "linux")]
fn os_constants() -> Vec<(&'static str, i64, i64)> {
    vec![
        ("MAP_POPULATE", MapFlags::POPULATE.bits() as i64, libc::MAP_POPULATE as i64),
        ("O_DIRECT", OpenFlags::DIRECT.bits() as i64, libc::O_DIRECT as i64),
        ("O_PATH", OpenFlags::PATH.bits() as i64, libc::O_PATH as i64),
        ("EFD_CLOEXEC", EventFlags::CLOSE_ON_EXEC.bits() as i64, libc::EFD_CLOEXEC as i64),
        ("EFD_NONBLOCK", EventFlags::NONBLOCK.bits() as i64, libc::EFD_NONBLOCK as i64),
        ("MFD_CLOEXEC", MemfdFlags::CLOSE_ON_EXEC.bits() as i64, libc::MFD_CLOEXEC as i64),
        ("MFD_ALLOW_SEALING", MemfdFlags::ALLOW_SEALING.bits() as i64, libc::MFD_ALLOW_SEALING as i64),
        ("F_ADD_SEALS", fd::F_ADD_SEALS as i64, libc::F_ADD_SEALS as i64),
        ("F_SEAL_SHRINK", Seals::SHRINK.bits() as i64, libc::F_SEAL_SHRINK as i64),
        ("F_SEAL_GROW", Seals::GROW.bits() as i64, libc::F_SEAL_GROW as i64),
        ("F_SEAL_SEAL", Seals::SEAL.bits() as i64, libc::F_SEAL_SEAL as i64),
        ("F_SEAL_WRITE", Seals::WRITE.bits() as i64, libc::F_SEAL_WRITE as i64),
        ("FALLOC_FL_KEEP_SIZE", FallocateFlags::KEEP_SIZE.bits() as i64, libc::FALLOC_FL_KEEP_SIZE as i64),
        ("FALLOC_FL_PUNCH_HOLE", FallocateFlags::PUNCH_HOLE.bits() as i64, libc::FALLOC_FL_PUNCH_HOLE as i64),
        ("IN_CLOEXEC", InotifyFlags::CLOSE_ON_EXEC.bits() as i64, libc::IN_CLOEXEC as i64),
        ("IN_NONBLOCK", InotifyFlags::NONBLOCK.bits() as i64, libc::IN_NONBLOCK as i64),
        ("IN_CREATE", InotifyMask::CREATE.bits() as i64, libc::IN_CREATE as i64),
        ("IN_MOVED_TO", InotifyMask::MOVED_TO.bits() as i64, libc::IN_MOVED_TO as i64),
        ("IN_MOVED_FROM", InotifyMask::MOVED_FROM.bits() as i64, libc::IN_MOVED_FROM as i64),
        ("IN_DELETE", InotifyMask::DELETE.bits() as i64, libc::IN_DELETE as i64),
        ("IN_DELETE_SELF", InotifyMask::DELETE_SELF.bits() as i64, libc::IN_DELETE_SELF as i64),
        ("IN_MOVE_SELF", InotifyMask::MOVE_SELF.bits() as i64, libc::IN_MOVE_SELF as i64),
        ("IN_MODIFY", InotifyMask::MODIFY.bits() as i64, libc::IN_MODIFY as i64),
        ("IN_CLOSE_WRITE", InotifyMask::CLOSE_WRITE.bits() as i64, libc::IN_CLOSE_WRITE as i64),
        ("IN_ATTRIB", InotifyMask::ATTRIB.bits() as i64, libc::IN_ATTRIB as i64),
        ("IN_EXCL_UNLINK", InotifyMask::EXCLUDE_UNLINKED.bits() as i64, libc::IN_EXCL_UNLINK as i64),
        ("IN_Q_OVERFLOW", InotifyMask::OVERFLOW.bits() as i64, libc::IN_Q_OVERFLOW as i64),
        ("IN_IGNORED", InotifyMask::IGNORED.bits() as i64, libc::IN_IGNORED as i64),
        ("IN_ISDIR", InotifyMask::IS_DIR.bits() as i64, libc::IN_ISDIR as i64),
    ]
}

#[cfg(target_os = "macos")]
fn os_constants() -> Vec<(&'static str, i64, i64)> {
    vec![
        ("MAP_JIT", MapFlags::JIT.bits() as i64, libc::MAP_JIT as i64),
        ("MADV_RANDOM", Advice::Random.raw() as i64, libc::MADV_RANDOM as i64),
        ("MADV_SEQUENTIAL", Advice::Sequential.raw() as i64, libc::MADV_SEQUENTIAL as i64),
        ("MADV_WILLNEED", Advice::WillNeed.raw() as i64, libc::MADV_WILLNEED as i64),
        ("MADV_DONTNEED", Advice::DontNeed.raw() as i64, libc::MADV_DONTNEED as i64),
        ("MADV_FREE", Advice::Free.raw() as i64, libc::MADV_FREE as i64),
        ("MADV_FREE_REUSABLE", Advice::FreeReusable.raw() as i64, libc::MADV_FREE_REUSABLE as i64),
        ("MADV_FREE_REUSE", Advice::FreeReuse.raw() as i64, libc::MADV_FREE_REUSE as i64),
        ("O_DSYNC", OpenFlags::DATA_SYNC.bits() as i64, libc::O_DSYNC as i64),
        ("O_EVTONLY", OpenFlags::EVTONLY.bits() as i64, libc::O_EVTONLY as i64),
        ("EVFILT_READ", Filter::READ.0 as i64, libc::EVFILT_READ as i64),
        ("EVFILT_WRITE", Filter::WRITE.0 as i64, libc::EVFILT_WRITE as i64),
        ("EVFILT_VNODE", Filter::VNODE.0 as i64, libc::EVFILT_VNODE as i64),
        ("EVFILT_PROC", Filter::PROC.0 as i64, libc::EVFILT_PROC as i64),
        ("EVFILT_TIMER", Filter::TIMER.0 as i64, libc::EVFILT_TIMER as i64),
        ("EVFILT_USER", Filter::USER.0 as i64, libc::EVFILT_USER as i64),
        ("EV_ADD", KeventFlags::ADD.bits() as i64, libc::EV_ADD as i64),
        ("EV_DELETE", KeventFlags::DELETE.bits() as i64, libc::EV_DELETE as i64),
        ("EV_ENABLE", KeventFlags::ENABLE.bits() as i64, libc::EV_ENABLE as i64),
        ("EV_DISABLE", KeventFlags::DISABLE.bits() as i64, libc::EV_DISABLE as i64),
        ("EV_ONESHOT", KeventFlags::ONESHOT.bits() as i64, libc::EV_ONESHOT as i64),
        ("EV_CLEAR", KeventFlags::CLEAR.bits() as i64, libc::EV_CLEAR as i64),
        ("EV_RECEIPT", KeventFlags::RECEIPT.bits() as i64, libc::EV_RECEIPT as i64),
        ("EV_DISPATCH", KeventFlags::DISPATCH.bits() as i64, libc::EV_DISPATCH as i64),
        ("EV_ERROR", KeventFlags::ERROR.bits() as i64, libc::EV_ERROR as i64),
        ("EV_EOF", KeventFlags::EOF.bits() as i64, libc::EV_EOF as i64),
        ("NOTE_DELETE", NoteFlags::DELETE.bits() as i64, libc::NOTE_DELETE as i64),
        ("NOTE_WRITE", NoteFlags::WRITE.bits() as i64, libc::NOTE_WRITE as i64),
        ("NOTE_EXTEND", NoteFlags::EXTEND.bits() as i64, libc::NOTE_EXTEND as i64),
        ("NOTE_ATTRIB", NoteFlags::ATTRIB.bits() as i64, libc::NOTE_ATTRIB as i64),
        ("NOTE_LINK", NoteFlags::LINK.bits() as i64, libc::NOTE_LINK as i64),
        ("NOTE_RENAME", NoteFlags::RENAME.bits() as i64, libc::NOTE_RENAME as i64),
        ("NOTE_REVOKE", NoteFlags::REVOKE.bits() as i64, libc::NOTE_REVOKE as i64),
        ("NOTE_TRIGGER", NoteFlags::TRIGGER.bits() as i64, libc::NOTE_TRIGGER as i64),
        ("NOTE_EXITSTATUS", NoteFlags::EXIT_STATUS.bits() as i64, libc::NOTE_EXITSTATUS as i64),
        ("NOTE_EXIT", NoteFlags::EXIT.bits() as i64, libc::NOTE_EXIT as i64),
        ("F_PREALLOCATE", fd::F_PREALLOCATE as i64, libc::F_PREALLOCATE as i64),
        ("F_RDADVISE", fd::F_RDADVISE as i64, libc::F_RDADVISE as i64),
        ("F_NOCACHE", fd::F_NOCACHE as i64, libc::F_NOCACHE as i64),
        ("F_FULLFSYNC", fd::F_FULLFSYNC as i64, libc::F_FULLFSYNC as i64),
        ("F_BARRIERFSYNC", fd::F_BARRIERFSYNC as i64, libc::F_BARRIERFSYNC as i64),
        ("F_ALLOCATECONTIG", AllocateFlags::CONTIGUOUS.bits() as i64, libc::F_ALLOCATECONTIG as i64),
        ("F_ALLOCATEALL", AllocateFlags::ALL.bits() as i64, libc::F_ALLOCATEALL as i64),
        ("F_PEOFPOSMODE", PosMode::PHYSICAL_END.0 as i64, libc::F_PEOFPOSMODE as i64),
        ("F_VOLPOSMODE", PosMode::VOLUME.0 as i64, libc::F_VOLPOSMODE as i64),
        ("CLOCK_REALTIME", ClockId::Realtime.raw() as i64, libc::CLOCK_REALTIME as i64),
        ("CLOCK_MONOTONIC", ClockId::Monotonic.raw() as i64, libc::CLOCK_MONOTONIC as i64),
        ("CLOCK_MONOTONIC_RAW", ClockId::MonotonicRaw.raw() as i64, libc::CLOCK_MONOTONIC_RAW as i64),
        ("CLOCK_UPTIME_RAW", ClockId::UptimeRaw.raw() as i64, libc::CLOCK_UPTIME_RAW as i64),
        ("CLOCK_PROCESS_CPUTIME_ID", ClockId::ProcessCpu.raw() as i64, libc::CLOCK_PROCESS_CPUTIME_ID as i64),
        ("CLOCK_THREAD_CPUTIME_ID", ClockId::ThreadCpu.raw() as i64, libc::CLOCK_THREAD_CPUTIME_ID as i64),
    ]
}

/// Every `#[repr(C)]` type here is laid out as libc's.
#[test]
fn the_layouts_are_libcs() {
    macro_rules! same {
        ($ours:ty, $libcs:ty $(, $field:ident = $libc_field:ident)*) => {
            assert_eq!(size_of::<$ours>(), size_of::<$libcs>(), stringify!($ours));
            assert_eq!(align_of::<$ours>(), align_of::<$libcs>(), stringify!($ours));
            $(assert_eq!(offset_of!($ours, $field), offset_of!($libcs, $libc_field), concat!(stringify!($ours), ".", stringify!($field)));)*
        };
    }
    same!(PollFd, libc::pollfd);
    same!(Timespec, libc::timespec, sec = tv_sec, nsec = tv_nsec);
    same!(Timeval, libc::timeval, sec = tv_sec, usec = tv_usec);
    same!(IoSlice<'static>, libc::iovec);
    same!(IoSliceMut<'static>, libc::iovec);
    #[cfg(target_os = "macos")]
    {
        same!(Tm, libc::tm, sec = tm_sec, mday = tm_mday, year = tm_year, isdst = tm_isdst, gmtoff = tm_gmtoff, zone = tm_zone);
        same!(Kevent, libc::kevent, ident = ident, filter = filter, flags = flags, fflags = fflags, data = data, udata = udata);
        same!(TimeValue, libc::time_value_t, seconds = seconds, microseconds = microseconds);
        same!(Radvisory, libc::radvisory, offset = ra_offset, count = ra_count);
        same!(Fstore, libc::fstore_t, flags = fst_flags, posmode = fst_posmode, offset = fst_offset, length = fst_length, bytes_alloc = fst_bytesalloc);
    }
}

/// The C functions declared here have libc's signatures (a coercion of both to one type).
/// macOS: on Linux nothing here is declared against the C library (D180).
#[cfg(target_os = "macos")]
#[test]
fn the_declarations_are_libcs() {
    {
        let _: [unsafe extern "C" fn(c_int) -> c_int; 2] = [sys_fd::close, libc::close];
        let _: [unsafe extern "C" fn(c_int) -> c_int; 2] = [sys_fd::dup, libc::dup];
        let _: [unsafe extern "C" fn(c_int, c_int) -> c_int; 2] = [sys_fd::dup2, libc::dup2];
        let _: [unsafe extern "C" fn(*mut c_int) -> c_int; 2] = [sys_fd::pipe, libc::pipe];
        let _: [unsafe extern "C" fn(c_int, *const c_void, usize) -> isize; 2] = [sys_fd::write, libc::write];
        let _: [unsafe extern "C" fn(*mut c_void, usize, c_int, c_int, c_int, i64) -> *mut c_void; 2] = [sys_mem::mmap, libc::mmap];
        let _: [unsafe extern "C" fn(*mut c_void, usize) -> c_int; 2] = [sys_mem::munmap, libc::munmap];
        let _: [unsafe extern "C" fn(*mut c_void, usize, c_int) -> c_int; 2] = [sys_mem::mprotect, libc::mprotect];
        let _: [unsafe extern "C" fn(*mut c_void, usize, c_int) -> c_int; 2] = [sys_mem::msync, libc::msync];
    }
}

/// A pipe's ends: non-blocking set and cleared, close-on-exec set and read, a byte written
/// and polled for and read, a copy above 10.
#[test]
fn a_pipe_takes_the_fcntl_operations() {
    let (read_end, write_end) = pipe().expect("a pipe");
    let (r, w) = (read_end.fd(), write_end.fd());
    assert!(!status_flags(r).unwrap().contains(OpenFlags::NONBLOCK));
    set_nonblocking(r, true).unwrap();
    assert!(status_flags(r).unwrap().contains(OpenFlags::NONBLOCK));
    // SAFETY: an open descriptor; F_GETFL takes nothing.
    assert_ne!(unsafe { libc::fcntl(r.raw(), libc::F_GETFL) } & libc::O_NONBLOCK, 0, "libc sees it too");
    let mut nothing = [0u8; 1];
    assert_eq!(read(r, &mut nothing), Err(crate::Errno::Again));
    set_nonblocking(r, false).unwrap();
    assert!(!close_on_exec(w).unwrap());
    set_close_on_exec(w, true).unwrap();
    assert!(close_on_exec(w).unwrap());
    assert_eq!(write(w, b"x"), Ok(1));
    let mut fds = [PollFd::new(r, PollEvents::IN)];
    assert_eq!(poll(&mut fds, Some(Timespec { sec: 1, nsec: 0 })), Ok(1));
    assert!(fds[0].found().contains(PollEvents::IN));
    let mut byte = [0u8; 1];
    assert_eq!(read(r, &mut byte), Ok(1));
    assert_eq!(&byte, b"x");
    let copy = dup_at_least(w, 10, true).unwrap();
    assert!(copy.fd().raw() >= 10 && close_on_exec(copy.fd()).unwrap());
    fionbio(copy.fd(), true).unwrap();
    assert!(status_flags(w).unwrap().contains(OpenFlags::NONBLOCK), "one open file, one status");
    close(copy).unwrap();
    // A number no descriptor has: the check of whether one is open.
    assert_eq!(close_on_exec(Fd::new(1 << 20)), Err(crate::Errno::BadF));
    // An empty poll times out: 0 ready.
    let mut fds = [PollFd::new(r, PollEvents::IN)];
    assert_eq!(poll(&mut fds, Some(Timespec::ZERO)), Ok(0));
}

/// A page mapped, written, resident, protected and unmapped.
#[test]
fn a_mapping_maps_and_unmaps() {
    let len = 1 << 16;
    // SAFETY: a new private anonymous mapping, not fixed.
    let at = unsafe { mmap(None, len, Prot::READ.with(Prot::WRITE), MapFlags::PRIVATE.with(MapFlags::ANONYMOUS), None, 0) }.unwrap();
    // SAFETY: the mapping is `len` bytes, writable.
    unsafe { at.as_ptr().write(7) };
    let mut held = [0u8; 64];
    // SAFETY: 64 bytes cover 64 KiB at the smallest page of either system (4 KiB: 16 pages).
    unsafe { mincore(at, len, &mut held) }.unwrap();
    assert_eq!(held[0] & 1, 1, "the page written is resident");
    // SAFETY: nothing else uses the mapping; read-only is taken away below by the unmap.
    unsafe { mprotect(at, len, Prot::READ) }.unwrap();
    // SAFETY: as above.
    unsafe { madvise(at, len, Advice::WillNeed) }.unwrap();
    // SAFETY: nothing uses the mapping after.
    unsafe { munmap(at, len) }.unwrap();
}

/// The clock goes forward across a sleep; entropy fills and refuses more than 256 bytes; the
/// epoch's local time is in 1969 or 1970.
#[test]
fn clocks_sleeps_and_entropy() {
    let before = clock_gettime(ClockId::Monotonic).unwrap();
    nanosleep(Timespec { sec: 0, nsec: 2_000_000 }).unwrap();
    let after = clock_gettime(ClockId::Monotonic).unwrap();
    assert!(after > before);
    assert_eq!(nanosleep(Timespec { sec: 0, nsec: 1_000_000_000 }), Err(crate::Errno::Inval));
    let mut seed = [0u8; 32];
    getentropy(&mut seed).unwrap();
    assert_ne!(seed, [0u8; 32]);
    assert_eq!(getentropy(&mut [0u8; 257]), Err(crate::Errno::Inval));
    #[cfg(target_os = "macos")]
    {
        let epoch = localtime(0).unwrap();
        assert!(epoch.year == 69 || epoch.year == 70);
    }
}

/// Linux's raw `dup`, `dup2`, `preadv`, `fallocate`, `msync` and `mincore` answer as libc's
/// do on the same descriptors: `dup2(a, a)` is `a` for an open `a` and `EBADF` for a closed
/// one (where `dup3` would say `EINVAL`), and `preadv` from an offset fills its slices in order.
#[cfg(target_os = "linux")]
#[test]
fn the_raw_descriptor_calls_answer_as_libcs() {
    let file = memfd_create(crate::Path::new(c"blaze-sys-fdctl"), MemfdFlags::CLOSE_ON_EXEC).unwrap();
    let fd = file.fd();
    let raw: c_int = fd.raw();
    assert_eq!(pwrite(fd, b"hello world", 0), Ok(11));

    // dup2 onto itself: the number back, nothing changed; a closed number: EBADF, as libc.
    assert_eq!(dup2(fd, fd).map(Fd::raw), Ok(raw));
    // SAFETY: an open descriptor onto itself: no change.
    assert_eq!(unsafe { libc::dup2(raw, raw) }, raw);
    let closed = Fd::new(1 << 20);
    assert_eq!(dup2(closed, closed), Err(crate::Errno::BadF));
    // SAFETY: a number no descriptor has; the call only fails.
    assert_eq!(unsafe { libc::dup2(closed.raw(), closed.raw()) }, -1);
    assert_eq!(dup3(fd, fd, false), Err(crate::Errno::Inval), "dup3 refuses the same number");

    // dup, then dup2 onto that copy's number: the copy is of the same open file.
    let copy = dup(fd).unwrap();
    assert!(!close_on_exec(copy.fd()).unwrap());
    assert_eq!(dup2(fd, copy.fd()).map(Fd::raw), Ok(copy.fd().raw()));
    close(copy).unwrap();

    // preadv from offset 2 into 3 + 5 bytes, against libc's on the same file.
    let (mut a, mut b) = ([0u8; 3], [0u8; 5]);
    let read = preadv(fd, &mut [IoSliceMut::new(&mut a), IoSliceMut::new(&mut b)], 2).unwrap();
    let (mut la, mut lb) = ([0u8; 3], [0u8; 5]);
    let iov = [
        libc::iovec { iov_base: la.as_mut_ptr().cast::<c_void>(), iov_len: la.len() },
        libc::iovec { iov_base: lb.as_mut_ptr().cast::<c_void>(), iov_len: lb.len() },
    ];
    // SAFETY: two live iovecs over live buffers.
    let libcs = unsafe { libc::preadv(raw, iov.as_ptr(), 2, 2) };
    assert_eq!(read as isize, libcs);
    assert_eq!((&a, &b), (&la, &lb));
    assert_eq!((&a, &b), (b"llo", b" worl"));
    assert_eq!(preadv(fd, &mut [IoSliceMut::new(&mut a)], u64::MAX), Err(crate::Errno::Inval));

    // fallocate keeping the size, then a shared mapping written, synced and resident.
    fallocate(fd, FallocateFlags::KEEP_SIZE, 0, 8192).unwrap();
    let len = 4096;
    // SAFETY: a new shared mapping of the memfd, not fixed.
    let at = unsafe { mmap(None, len, Prot::READ.with(Prot::WRITE), MapFlags::SHARED, Some(fd), 0) }.unwrap();
    // SAFETY: the mapping is `len` bytes, writable.
    unsafe { at.as_ptr().write(b'H') };
    // SAFETY: the range is this mapping, page-aligned.
    unsafe { msync(at, len, MsyncFlags::SYNC) }.unwrap();
    let mut held = [0u8; 1];
    // SAFETY: one byte covers one page.
    unsafe { mincore(at, len, &mut held) }.unwrap();
    assert_eq!(held[0] & 1, 1, "the page written is resident");
    // SAFETY: nothing uses the mapping after.
    unsafe { munmap(at, len) }.unwrap();
    let mut first = [0u8; 1];
    assert_eq!(pread(fd, &mut first, 0), Ok(1));
    assert_eq!(&first, b"H", "the mapping's write reached the file");
}

/// The kqueue answers a user event it is asked to fire.
#[cfg(target_os = "macos")]
#[test]
fn a_kqueue_fires_a_user_event() {
    let kq = kqueue().unwrap();
    let add = Kevent { ident: 7, filter: Filter::USER, flags: KeventFlags::ADD.with(KeventFlags::CLEAR), fflags: NoteFlags::NONE, data: 0, udata: 42 };
    assert_eq!(kevent(kq.fd(), &[add], &mut [], None), Ok(0));
    let fire = Kevent { flags: KeventFlags::NONE, fflags: NoteFlags::TRIGGER, ..add };
    let mut out = [add; 1];
    assert_eq!(kevent(kq.fd(), &[fire], &mut out, Some(Timespec { sec: 1, nsec: 0 })), Ok(1));
    let got = out[0];
    let (ident, udata) = (got.ident, got.udata);
    assert_eq!((ident, udata), (7, 42));
    assert_eq!(kevent(kq.fd(), &[], &mut out, Some(Timespec::ZERO)), Ok(0), "cleared once reported");
}

/// `O_EVTONLY` opens a vnode descriptor purely for `EVFILT_VNODE` watching and delivers
/// `NOTE_WRITE`, `NOTE_EXTEND`, and `NOTE_DELETE` synchronously on a zero-timeout `kevent`.
#[cfg(target_os = "macos")]
#[test]
fn evtonly_vnode_watch_reports_mutations() {
    let dir = std::env::temp_dir().join(format!("blaze-sys-evtonly-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("watched.txt");
    std::fs::write(&path, b"a").unwrap();
    let cpath = std::ffi::CString::new(path.to_str().unwrap()).unwrap();

    let vnode = openat(CWD, crate::Path::new(&cpath), OpenFlags::EVTONLY.with(OpenFlags::CLOSE_ON_EXEC), 0).unwrap();
    let kq = kqueue().unwrap();
    let all_notes = NoteFlags::DELETE
        .with(NoteFlags::WRITE)
        .with(NoteFlags::EXTEND)
        .with(NoteFlags::ATTRIB)
        .with(NoteFlags::LINK)
        .with(NoteFlags::RENAME)
        .with(NoteFlags::REVOKE);
    let add = Kevent {
        ident: vnode.fd().raw() as usize,
        filter: Filter::VNODE,
        flags: KeventFlags::ADD.with(KeventFlags::CLEAR),
        fflags: all_notes,
        data: 0,
        udata: 99,
    };
    assert_eq!(kevent(kq.fd(), &[add], &mut [], None), Ok(0));

    // Extending the file synchronously triggers WRITE | EXTEND.
    std::fs::write(&path, b"abcdef").unwrap();
    let mut out = [add; 1];
    assert_eq!(kevent(kq.fd(), &[], &mut out, Some(Timespec::ZERO)), Ok(1));
    let got = out[0];
    let (fflags, udata) = (got.fflags, got.udata);
    assert_eq!(udata, 99);
    assert!(fflags.contains(NoteFlags::WRITE), "expected NOTE_WRITE, got {fflags:?}");
    assert!(fflags.contains(NoteFlags::EXTEND), "expected NOTE_EXTEND, got {fflags:?}");

    // Unlinking the file synchronously triggers DELETE.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(kevent(kq.fd(), &[], &mut out, Some(Timespec::ZERO)), Ok(1));
    let fflags = out[0].fflags;
    assert!(fflags.contains(NoteFlags::DELETE), "expected NOTE_DELETE, got {fflags:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `poll`'s milliseconds round up and cap.
#[cfg(target_os = "macos")]
#[test]
fn poll_millis_rounds_up() {
    assert_eq!(event::poll_millis(None), -1);
    assert_eq!(event::poll_millis(Some(Timespec { sec: 0, nsec: 1 })), 1);
    assert_eq!(event::poll_millis(Some(Timespec { sec: 2, nsec: 0 })), 2_000);
    assert_eq!(event::poll_millis(Some(Timespec { sec: -1, nsec: 0 })), 0);
    assert_eq!(event::poll_millis(Some(Timespec { sec: i64::MAX, nsec: 0 })), c_int::MAX);
}

/// A memfd takes seals: it can no longer grow.
#[cfg(target_os = "linux")]
#[test]
fn a_memfd_takes_seals() {
    let file = memfd_create(crate::Path::new(c"blaze-sys-io"), MemfdFlags::CLOSE_ON_EXEC.with(MemfdFlags::ALLOW_SEALING)).unwrap();
    ftruncate(file.fd(), 4096).unwrap();
    add_seals(file.fd(), Seals::GROW.with(Seals::SHRINK)).unwrap();
    assert_eq!(ftruncate(file.fd(), 8192), Err(crate::Errno::Perm));
}
