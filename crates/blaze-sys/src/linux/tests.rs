//! Every wrapper on the real kernel, with the `libc` crate as the oracle where the same call
//! can be made both ways: the number table, the constants and the layouts against libc's, and
//! each call's result against libc's own.

use core::sync::atomic::{AtomicU32, Ordering};
use std::{ffi::CString, sync::Arc, time::Duration};

use super::*;
use crate::{Errno, Fd, Path, Timespec};

/// A directory of this test's own under the system's temporary one, removed when dropped.
struct Scratch {
    path: std::path::PathBuf,
    dir: OwnedFd,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let path = std::env::temp_dir().join(format!("blaze-sys-{name}-{}", getpid()));
        _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        let c = CString::new(path.to_str().unwrap()).unwrap();
        let flags = OpenFlags::READ | OpenFlags::DIRECTORY | OpenFlags::CLOSE_ON_EXEC;
        let dir = openat(CWD, Path::new(&c), flags, 0).unwrap();
        Scratch { path, dir }
    }

    fn fd(&self) -> Fd {
        self.dir.fd()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.path);
    }
}

const CREATE: OpenFlags = OpenFlags::READ_WRITE.with(OpenFlags::CREATE).with(OpenFlags::CLOSE_ON_EXEC);

/// libc's `errno`, as an [`Errno`].
fn libc_errno() -> Errno {
    Errno::from_raw(u16::try_from(std::io::Error::last_os_error().raw_os_error().unwrap()).unwrap())
}

#[test]
fn the_number_table_is_libcs() {
    let libcs: &[(&str, libc::c_long)] = &[
        ("READ", libc::SYS_read),
        ("WRITE", libc::SYS_write),
        ("CLOSE", libc::SYS_close),
        ("PREAD64", libc::SYS_pread64),
        ("PWRITE64", libc::SYS_pwrite64),
        ("OPENAT", libc::SYS_openat),
        ("OPENAT2", libc::SYS_openat2),
        ("FSYNC", libc::SYS_fsync),
        ("FDATASYNC", libc::SYS_fdatasync),
        ("SYNC_FILE_RANGE", libc::SYS_sync_file_range),
        ("FTRUNCATE", libc::SYS_ftruncate),
        ("RENAMEAT2", libc::SYS_renameat2),
        ("UNLINKAT", libc::SYS_unlinkat),
        ("MKDIRAT", libc::SYS_mkdirat),
        ("STATX", libc::SYS_statx),
        ("GETDENTS64", libc::SYS_getdents64),
        ("IOCTL", libc::SYS_ioctl),
        ("MMAP", libc::SYS_mmap),
        ("MUNMAP", libc::SYS_munmap),
        ("MADVISE", libc::SYS_madvise),
        ("MPROTECT", libc::SYS_mprotect),
        ("FUTEX", libc::SYS_futex),
        ("PPOLL", libc::SYS_ppoll),
        ("PIDFD_OPEN", libc::SYS_pidfd_open),
        ("WAITID", libc::SYS_waitid),
        ("WAIT4", libc::SYS_wait4),
        ("CLOCK_GETTIME", libc::SYS_clock_gettime),
        ("CLOCK_NANOSLEEP", libc::SYS_clock_nanosleep),
        ("CLONE3", libc::SYS_clone3),
        ("EXIT", libc::SYS_exit),
        ("EXIT_GROUP", libc::SYS_exit_group),
        ("GETRANDOM", libc::SYS_getrandom),
        ("IO_URING_SETUP", libc::SYS_io_uring_setup),
        ("IO_URING_ENTER", libc::SYS_io_uring_enter),
        ("IO_URING_REGISTER", libc::SYS_io_uring_register),
        ("SOCKET", libc::SYS_socket),
        ("CONNECT", libc::SYS_connect),
        ("BIND", libc::SYS_bind),
        ("LISTEN", libc::SYS_listen),
        ("ACCEPT4", libc::SYS_accept4),
        ("SENDMSG", libc::SYS_sendmsg),
        ("RECVMSG", libc::SYS_recvmsg),
        ("SHUTDOWN", libc::SYS_shutdown),
        ("PIPE2", libc::SYS_pipe2),
        ("EVENTFD2", libc::SYS_eventfd2),
        ("PRCTL", libc::SYS_prctl),
        ("SECCOMP", libc::SYS_seccomp),
        ("RT_SIGACTION", libc::SYS_rt_sigaction),
        ("RT_SIGPROCMASK", libc::SYS_rt_sigprocmask),
        ("GETPID", libc::SYS_getpid),
        ("KILL", libc::SYS_kill),
    ];
    // Every row of the table is checked, and in the table's order: a call added to one and
    // not the other fails here.
    assert_eq!(nr::TABLE.len(), libcs.len(), "a call is in one table and not the other");
    for (&(name, number), &(libc_name, libcs)) in nr::TABLE.iter().zip(libcs) {
        assert_eq!(name, libc_name, "the tables are in different orders");
        assert_eq!(number as libc::c_long, libcs, "SYS_{name}");
    }
}

#[test]
fn the_constants_are_libcs() {
    let open: &[(OpenFlags, libc::c_int, &str)] = &[
        (OpenFlags::READ, libc::O_RDONLY, "O_RDONLY"),
        (OpenFlags::WRITE, libc::O_WRONLY, "O_WRONLY"),
        (OpenFlags::READ_WRITE, libc::O_RDWR, "O_RDWR"),
        (OpenFlags::CREATE, libc::O_CREAT, "O_CREAT"),
        (OpenFlags::EXCLUSIVE, libc::O_EXCL, "O_EXCL"),
        (OpenFlags::NO_CONTROLLING_TTY, libc::O_NOCTTY, "O_NOCTTY"),
        (OpenFlags::TRUNCATE, libc::O_TRUNC, "O_TRUNC"),
        (OpenFlags::APPEND, libc::O_APPEND, "O_APPEND"),
        (OpenFlags::NONBLOCK, libc::O_NONBLOCK, "O_NONBLOCK"),
        (OpenFlags::DATA_SYNC, libc::O_DSYNC, "O_DSYNC"),
        (OpenFlags::DIRECT, libc::O_DIRECT, "O_DIRECT"),
        (OpenFlags::DIRECTORY, libc::O_DIRECTORY, "O_DIRECTORY"),
        (OpenFlags::NO_FOLLOW, libc::O_NOFOLLOW, "O_NOFOLLOW"),
        (OpenFlags::NO_ACCESS_TIME, libc::O_NOATIME, "O_NOATIME"),
        (OpenFlags::CLOSE_ON_EXEC, libc::O_CLOEXEC, "O_CLOEXEC"),
        (OpenFlags::PATH, libc::O_PATH, "O_PATH"),
    ];
    for &(ours, libcs, name) in open {
        assert_eq!(ours.bits() as libc::c_int, libcs, "{name}");
    }
    assert_eq!(CWD.raw(), libc::AT_FDCWD);
    assert_eq!(Resolve::BENEATH.bits(), libc::RESOLVE_BENEATH);
    assert_eq!(Resolve::NO_MAGICLINKS.bits(), libc::RESOLVE_NO_MAGICLINKS);
    assert_eq!(Resolve::NO_SYMLINKS.bits(), libc::RESOLVE_NO_SYMLINKS);
    assert_eq!(Resolve::NO_XDEV.bits(), libc::RESOLVE_NO_XDEV);
    assert_eq!(Resolve::IN_ROOT.bits(), libc::RESOLVE_IN_ROOT);
    assert_eq!(RenameFlags::NO_REPLACE.bits(), libc::RENAME_NOREPLACE);
    assert_eq!(RenameFlags::EXCHANGE.bits(), libc::RENAME_EXCHANGE);
    assert_eq!(AtFlags::NO_FOLLOW.bits() as libc::c_int, libc::AT_SYMLINK_NOFOLLOW);
    assert_eq!(AtFlags::EMPTY_PATH.bits() as libc::c_int, libc::AT_EMPTY_PATH);
    assert_eq!(StatxMask::BASIC.bits(), libc::STATX_BASIC_STATS);
    assert_eq!(StatxMask::SIZE.bits(), libc::STATX_SIZE);
    assert_eq!(StatxMask::INO.bits(), libc::STATX_INO);
    assert_eq!(StatxMask::MTIME.bits(), libc::STATX_MTIME);
    assert_eq!(StatxMask::CTIME.bits(), libc::STATX_CTIME);
    assert_eq!(StatxMask::BTIME.bits(), libc::STATX_BTIME);
    assert_eq!(SyncRange::WRITE.bits(), libc::SYNC_FILE_RANGE_WRITE);
    assert_eq!(SyncRange::WAIT_BEFORE.bits(), libc::SYNC_FILE_RANGE_WAIT_BEFORE);
    assert_eq!(SyncRange::WAIT_AFTER.bits(), libc::SYNC_FILE_RANGE_WAIT_AFTER);
    assert_eq!(Prot::READ.bits() as libc::c_int, libc::PROT_READ);
    assert_eq!(Prot::WRITE.bits() as libc::c_int, libc::PROT_WRITE);
    assert_eq!(Prot::EXEC.bits() as libc::c_int, libc::PROT_EXEC);
    assert_eq!(MapFlags::SHARED.bits() as libc::c_int, libc::MAP_SHARED);
    assert_eq!(MapFlags::PRIVATE.bits() as libc::c_int, libc::MAP_PRIVATE);
    assert_eq!(MapFlags::FIXED.bits() as libc::c_int, libc::MAP_FIXED);
    assert_eq!(MapFlags::ANONYMOUS.bits() as libc::c_int, libc::MAP_ANONYMOUS);
    assert_eq!(MapFlags::NO_RESERVE.bits() as libc::c_int, libc::MAP_NORESERVE);
    assert_eq!(MapFlags::POPULATE.bits() as libc::c_int, libc::MAP_POPULATE);
    assert_eq!(MapFlags::STACK.bits() as libc::c_int, libc::MAP_STACK);
    assert_eq!(MapFlags::FIXED_NO_REPLACE.bits() as libc::c_int, libc::MAP_FIXED_NOREPLACE);
    assert_eq!(PollEvents::IN.bits(), libc::POLLIN);
    assert_eq!(PollEvents::OUT.bits(), libc::POLLOUT);
    assert_eq!(PollEvents::ERR.bits(), libc::POLLERR);
    assert_eq!(PollEvents::HUP.bits(), libc::POLLHUP);
    assert_eq!(PollEvents::NVAL.bits(), libc::POLLNVAL);
    assert_eq!(WaitOptions::NO_HANG.bits() as libc::c_int, libc::WNOHANG);
    assert_eq!(WaitOptions::EXITED.bits() as libc::c_int, libc::WEXITED);
    assert_eq!(WaitOptions::NO_WAIT.bits() as libc::c_int, libc::WNOWAIT);
    assert_eq!(CloneFlags::PIDFD.bits(), libc::CLONE_PIDFD as u64);
    assert_eq!(CloneFlags::CLEAR_SIGHAND.bits(), libc::CLONE_CLEAR_SIGHAND as u64);
    assert_eq!(CloneFlags::INTO_CGROUP.bits(), libc::CLONE_INTO_CGROUP as u64);
    assert_eq!(SockFlags::NONBLOCK.bits() as libc::c_int, libc::SOCK_NONBLOCK);
    assert_eq!(SockFlags::CLOSE_ON_EXEC.bits() as libc::c_int, libc::SOCK_CLOEXEC);
    assert_eq!(MsgFlags::DONT_WAIT.bits() as libc::c_int, libc::MSG_DONTWAIT);
    assert_eq!(MsgFlags::NO_SIGNAL.bits() as libc::c_int, libc::MSG_NOSIGNAL);
    assert_eq!(MsgFlags::PEEK.bits() as libc::c_int, libc::MSG_PEEK);
    assert_eq!(MsgFlags::WAIT_ALL.bits() as libc::c_int, libc::MSG_WAITALL);
    assert_eq!(PipeFlags::CLOSE_ON_EXEC.bits() as libc::c_int, libc::O_CLOEXEC);
    assert_eq!(EventFlags::NONBLOCK.bits() as libc::c_int, libc::EFD_NONBLOCK);
    assert_eq!(EventFlags::CLOSE_ON_EXEC.bits() as libc::c_int, libc::EFD_CLOEXEC);
    assert_eq!(EventFlags::SEMAPHORE.bits() as libc::c_int, libc::EFD_SEMAPHORE);
    assert_eq!(RandomFlags::NONBLOCK.bits(), libc::GRND_NONBLOCK);
    assert_eq!(ETIME.raw() as libc::c_int, libc::ETIME);
}

#[test]
fn the_layouts_are_the_kernels() {
    assert_eq!(size_of::<Statx>(), 256);
    assert_eq!(size_of::<Statx>(), size_of::<libc::statx>());
    assert_eq!(core::mem::offset_of!(Statx, ino), core::mem::offset_of!(libc::statx, stx_ino));
    assert_eq!(core::mem::offset_of!(Statx, size), core::mem::offset_of!(libc::statx, stx_size));
    assert_eq!(core::mem::offset_of!(Statx, mtime), core::mem::offset_of!(libc::statx, stx_mtime));
    assert_eq!(core::mem::offset_of!(Statx, ctime), core::mem::offset_of!(libc::statx, stx_ctime));
    assert_eq!(core::mem::offset_of!(Statx, mnt_id), core::mem::offset_of!(libc::statx, stx_mnt_id));
    assert_eq!(size_of::<OpenHow>(), size_of::<libc::open_how>());
    assert_eq!(size_of::<Timespec>(), size_of::<libc::timespec>());
    assert_eq!(size_of::<PollFd>(), size_of::<libc::pollfd>());
    assert_eq!(size_of::<Rusage>(), size_of::<libc::rusage>());
    assert_eq!(size_of::<CloneArgs>(), 88);
    assert_eq!(size_of::<CloneRange>(), size_of::<libc::file_clone_range>());
    assert_eq!(size_of::<UringParams>(), 120);
    assert_eq!(size_of::<SockFilter>(), size_of::<libc::sock_filter>());
    assert_eq!(size_of::<UnixAddr>(), size_of::<libc::sockaddr_un>());
    assert_eq!(size_of::<V4Addr>(), size_of::<libc::sockaddr_in>());
    assert_eq!(size_of::<V6Addr>(), size_of::<libc::sockaddr_in6>());
    assert_eq!(size_of::<IoSlice<'_>>(), size_of::<libc::iovec>());
    assert_eq!(size_of::<SigSet>(), 8);
}

#[test]
fn an_errno_round_trips() {
    for &(errno, name, number) in Errno::LISTED {
        assert_eq!(errno.raw(), number, "{name}");
        assert_eq!(Errno::from_raw(number), errno, "{name}");
        assert_eq!(errno.name(), name);
    }
    for code in 0..=4095u16 {
        assert_eq!(Errno::from_raw(code).raw(), code);
    }
    // No two listed errors share a number: a number names one error.
    for (i, &(_, name, number)) in Errno::LISTED.iter().enumerate() {
        for &(_, other, again) in &Errno::LISTED[i + 1..] {
            assert_ne!(number, again, "{name} and {other}");
        }
    }
}

#[test]
fn the_errno_table_is_libcs() {
    let libcs: &[(Errno, libc::c_int)] = &[
        (Errno::Perm, libc::EPERM),
        (Errno::NoEnt, libc::ENOENT),
        (Errno::Srch, libc::ESRCH),
        (Errno::Intr, libc::EINTR),
        (Errno::Io, libc::EIO),
        (Errno::Nxio, libc::ENXIO),
        (Errno::TooBig, libc::E2BIG),
        (Errno::NoExec, libc::ENOEXEC),
        (Errno::BadF, libc::EBADF),
        (Errno::Child, libc::ECHILD),
        (Errno::Again, libc::EAGAIN),
        (Errno::NoMem, libc::ENOMEM),
        (Errno::Acces, libc::EACCES),
        (Errno::Fault, libc::EFAULT),
        (Errno::Busy, libc::EBUSY),
        (Errno::Exist, libc::EEXIST),
        (Errno::XDev, libc::EXDEV),
        (Errno::NoDev, libc::ENODEV),
        (Errno::NotDir, libc::ENOTDIR),
        (Errno::IsDir, libc::EISDIR),
        (Errno::Inval, libc::EINVAL),
        (Errno::NFile, libc::ENFILE),
        (Errno::MFile, libc::EMFILE),
        (Errno::NoTty, libc::ENOTTY),
        (Errno::TxtBsy, libc::ETXTBSY),
        (Errno::FBig, libc::EFBIG),
        (Errno::NoSpc, libc::ENOSPC),
        (Errno::SPipe, libc::ESPIPE),
        (Errno::RoFs, libc::EROFS),
        (Errno::MLink, libc::EMLINK),
        (Errno::Pipe, libc::EPIPE),
        (Errno::Range, libc::ERANGE),
        (Errno::Deadlk, libc::EDEADLK),
        (Errno::NameTooLong, libc::ENAMETOOLONG),
        (Errno::NoLck, libc::ENOLCK),
        (Errno::NoSys, libc::ENOSYS),
        (Errno::NotEmpty, libc::ENOTEMPTY),
        (Errno::Loop, libc::ELOOP),
        (Errno::Overflow, libc::EOVERFLOW),
        (Errno::NotSock, libc::ENOTSOCK),
        (Errno::MsgSize, libc::EMSGSIZE),
        (Errno::ProtoNoSupport, libc::EPROTONOSUPPORT),
        (Errno::OpNotSupp, libc::EOPNOTSUPP),
        (Errno::AfNoSupport, libc::EAFNOSUPPORT),
        (Errno::AddrInUse, libc::EADDRINUSE),
        (Errno::AddrNotAvail, libc::EADDRNOTAVAIL),
        (Errno::NetDown, libc::ENETDOWN),
        (Errno::NetUnreach, libc::ENETUNREACH),
        (Errno::ConnAborted, libc::ECONNABORTED),
        (Errno::ConnReset, libc::ECONNRESET),
        (Errno::NoBufs, libc::ENOBUFS),
        (Errno::IsConn, libc::EISCONN),
        (Errno::NotConn, libc::ENOTCONN),
        (Errno::TimedOut, libc::ETIMEDOUT),
        (Errno::ConnRefused, libc::ECONNREFUSED),
        (Errno::HostUnreach, libc::EHOSTUNREACH),
        (Errno::Already, libc::EALREADY),
        (Errno::InProgress, libc::EINPROGRESS),
        (Errno::Stale, libc::ESTALE),
        (Errno::DQuot, libc::EDQUOT),
        (Errno::Canceled, libc::ECANCELED),
    ];
    assert_eq!(libcs.len(), Errno::LISTED.len(), "an error is in one table and not the other");
    for &(errno, libcs) in libcs {
        assert_eq!(libc::c_int::from(errno.raw()), libcs, "{}", errno.name());
    }
}

/// Open, write, read back, rename, stat, list, remove: each result against libc's same call.
#[test]
fn a_file_is_written_read_renamed_and_removed() {
    let scratch = Scratch::new("file");
    let file = openat(scratch.fd(), Path::new(c"a"), CREATE | OpenFlags::EXCLUSIVE, 0o644).unwrap();
    assert_eq!(pwrite(file.fd(), b"hello, kernel", 0), Ok(13));
    assert_eq!(write(file.fd(), b"!"), Ok(1), "a plain write is at the position, which is 0");
    let mut back = [0u8; 32];
    assert_eq!(pread(file.fd(), &mut back, 0), Ok(13));
    assert_eq!(&back[..13], b"!ello, kernel");
    assert_eq!(pread(file.fd(), &mut back, 13), Ok(0), "a read at the end is empty");
    // libc reads the same bytes through the same descriptor.
    let mut libcs = [0u8; 32];
    // SAFETY: a live buffer of the length passed.
    let n = unsafe { libc::pread(file.fd().raw(), libcs.as_mut_ptr().cast(), 32, 0) };
    assert_eq!((n, &libcs[..13]), (13, &back[..13]));
    assert_eq!(fsync(file.fd()), Ok(()));
    assert_eq!(fdatasync(file.fd()), Ok(()));
    assert_eq!(sync_file_range(file.fd(), 0, 0, SyncRange::WRITE), Ok(()));
    assert_eq!(ftruncate(file.fd(), 5), Ok(()));

    let ours = statx(file.fd(), Path::EMPTY, AtFlags::EMPTY_PATH, StatxMask::BASIC).unwrap();
    // SAFETY: all-zero bytes is a valid `stat`, which `fstat` fills.
    let theirs = unsafe {
        let mut stat: libc::stat = core::mem::zeroed();
        assert_eq!(libc::fstat(file.fd().raw(), &mut stat), 0);
        stat
    };
    assert_eq!(ours.size, 5);
    assert_eq!(ours.size, theirs.st_size as u64);
    assert_eq!(ours.ino, theirs.st_ino as u64);
    assert_eq!(u32::from(ours.mode), theirs.st_mode as u32);
    assert_eq!(ours.mtime.sec, theirs.st_mtime as i64);
    assert_eq!(i64::from(ours.mtime.nsec), theirs.st_mtime_nsec as i64);
    assert!(!ours.is_dir());
    assert_eq!(close(file), Ok(()));

    // A name taken is refused with the typed error, as libc's call sets errno.
    assert_eq!(
        openat(scratch.fd(), Path::new(c"a"), CREATE | OpenFlags::EXCLUSIVE, 0o644).map(|_| ()),
        Err(Errno::Exist)
    );
    // SAFETY: a live NUL-terminated path.
    assert_eq!(unsafe { libc::openat(scratch.fd().raw(), c"a".as_ptr(), libc::O_CREAT | libc::O_EXCL, 0o644) }, -1);
    assert_eq!(libc_errno(), Errno::Exist);

    assert_eq!(mkdirat(scratch.fd(), Path::new(c"d"), 0o755), Ok(()));
    assert_eq!(mkdirat(scratch.fd(), Path::new(c"d"), 0o755), Err(Errno::Exist));
    assert_eq!(
        renameat2(scratch.fd(), Path::new(c"a"), scratch.fd(), Path::new(c"d/b"), RenameFlags::NO_REPLACE),
        Ok(())
    );
    assert_eq!(
        statx(scratch.fd(), Path::new(c"a"), AtFlags::NONE, StatxMask::BASIC).map(|_| ()),
        Err(Errno::NoEnt)
    );
    assert_eq!(statx(scratch.fd(), Path::new(c"d/b"), AtFlags::NONE, StatxMask::SIZE).unwrap().size, 5);
    assert!(statx(scratch.fd(), Path::new(c"d"), AtFlags::NONE, StatxMask::BASIC).unwrap().is_dir());

    // The listing is what std's is.
    let dir = openat(scratch.fd(), Path::new(c"d"), OpenFlags::READ | OpenFlags::DIRECTORY, 0).unwrap();
    let mut buffer = [0u8; 4096];
    let mut names: Vec<(Vec<u8>, EntryKind)> = getdents64(dir.fd(), &mut buffer)
        .unwrap()
        .map(|entry| (entry.name.to_vec(), entry.kind))
        .collect();
    names.sort_by(|(a, _), (b, _)| a.cmp(b));
    assert_eq!(names.iter().map(|(name, _)| name.as_slice()).collect::<Vec<_>>(), [&b"."[..], b"..", b"b"]);
    assert!(matches!(names[2].1, EntryKind::Regular | EntryKind::Unknown));
    assert_eq!(getdents64(dir.fd(), &mut buffer).unwrap().count(), 0, "the directory's end");

    assert_eq!(unlinkat(scratch.fd(), Path::new(c"d"), Unlink::Dir), Err(Errno::NotEmpty));
    assert_eq!(unlinkat(scratch.fd(), Path::new(c"d/b"), Unlink::File), Ok(()));
    assert_eq!(unlinkat(scratch.fd(), Path::new(c"d/b"), Unlink::File), Err(Errno::NoEnt));
    assert_eq!(unlinkat(scratch.fd(), Path::new(c"d"), Unlink::Dir), Ok(()));
}

/// The beneath-only open, as `beneath.rs` relies on it: a path that climbs out or an absolute
/// link is `EXDEV`; a link that stays beneath is followed.
#[test]
fn an_open_beneath_refuses_what_leaves_its_directory() {
    let scratch = Scratch::new("beneath");
    std::fs::create_dir(scratch.path.join("in")).unwrap();
    std::fs::write(scratch.path.join("in/file"), b"x").unwrap();
    std::fs::write(scratch.path.join("outside"), b"y").unwrap();
    std::os::unix::fs::symlink("file", scratch.path.join("in/stays")).unwrap();
    std::os::unix::fs::symlink("../outside", scratch.path.join("in/climbs")).unwrap();
    std::os::unix::fs::symlink(scratch.path.join("outside"), scratch.path.join("in/absolute")).unwrap();
    let dir = openat(scratch.fd(), Path::new(c"in"), OpenFlags::READ | OpenFlags::DIRECTORY, 0).unwrap();
    let open = |path: &core::ffi::CStr| open_beneath(dir.fd(), Path::new(path), OpenFlags::READ, 0).map(|_| ());
    assert_eq!(open(c"file"), Ok(()));
    assert_eq!(open(c"stays"), Ok(()));
    assert_eq!(open(c"../outside"), Err(Errno::XDev));
    assert_eq!(open(c"climbs"), Err(Errno::XDev));
    assert_eq!(open(c"absolute"), Err(Errno::XDev));
    assert_eq!(open(c"/etc"), Err(Errno::XDev));
    assert_eq!(open(c"missing"), Err(Errno::NoEnt));
}

#[test]
fn a_path_is_made_in_a_buffer_or_refused() {
    let mut buffer = [0xffu8; 8];
    assert_eq!(Path::in_buffer(b"abc", &mut buffer).unwrap().bytes(), b"abc");
    assert_eq!(Path::in_buffer(b"1234567", &mut buffer).unwrap().bytes(), b"1234567");
    assert_eq!(Path::in_buffer(b"12345678", &mut buffer).map(|_| ()), Err(Errno::NameTooLong));
    assert_eq!(Path::in_buffer(b"a\0b", &mut buffer).map(|_| ()), Err(Errno::Inval));
    assert_eq!(Path::in_buffer(b"", &mut buffer).unwrap().bytes(), b"");
}

/// A clone shares the source's bytes where the file system can, and is the typed refusal
/// where it cannot: never a copy.
#[test]
fn a_clone_shares_or_is_refused() {
    let scratch = Scratch::new("clone");
    let from = openat(scratch.fd(), Path::new(c"from"), CREATE, 0o644).unwrap();
    assert_eq!(pwrite(from.fd(), &[7u8; 8192], 0), Ok(8192));
    fsync(from.fd()).unwrap();
    let to = openat(scratch.fd(), Path::new(c"to"), CREATE, 0o644).unwrap();
    match ficlone(to.fd(), from.fd()) {
        Ok(()) => {
            let mut back = [0u8; 8192];
            assert_eq!(pread(to.fd(), &mut back, 0), Ok(8192));
            assert_eq!(back, [7u8; 8192]);
            let range = CloneRange::new(from.fd(), 0, 4096, 4096);
            assert_eq!(ficlonerange(to.fd(), &range), Ok(()));
        }
        Err(refused) => {
            assert!(
                matches!(refused, Errno::OpNotSupp | Errno::Inval | Errno::XDev | Errno::NoTty),
                "a file system that cannot share blocks said {refused}"
            );
            let size = statx(to.fd(), Path::EMPTY, AtFlags::EMPTY_PATH, StatxMask::SIZE).unwrap().size;
            assert_eq!(size, 0, "a refused clone copied nothing");
        }
    }
}

#[test]
fn memory_is_mapped_protected_advised_and_unmapped() {
    const LEN: usize = 4 * 4096;
    // SAFETY: an anonymous private mapping at an address the kernel picks; the test owns it
    // and makes no reference that outlives a call below.
    unsafe {
        let at = mmap(None, LEN, Prot::READ | Prot::WRITE, MapFlags::PRIVATE | MapFlags::ANONYMOUS, None, 0)
            .unwrap();
        assert_eq!(at.as_ptr() as usize % 4096, 0);
        at.as_ptr().write_bytes(0xab, LEN);
        assert_eq!(madvise(at, 4096, Advice::DontNeed), Ok(()));
        assert_eq!(at.as_ptr().read(), 0, "a page given back reads as zero");
        assert_eq!(at.as_ptr().add(4096).read(), 0xab);
        assert_eq!(mprotect(at, 4096, Prot::READ), Ok(()));
        assert_eq!(mprotect(at, 4096, Prot::READ | Prot::WRITE), Ok(()));
        assert_eq!(munmap(at, LEN), Ok(()));
        // A length of 0 is refused, as libc's call is.
        assert_eq!(munmap(at, 0), Err(Errno::Inval));
        assert_eq!(libc::munmap(at.as_ptr().cast(), 0), -1);
        assert_eq!(libc_errno(), Errno::Inval);
    }
}

fn monotonic() -> Duration {
    clock_gettime(ClockId::Monotonic).unwrap().to_duration().unwrap()
}

#[test]
fn the_clock_is_libcs_and_does_not_go_back() {
    let before = monotonic();
    let mut libcs = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a timespec for the call to write.
    assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut libcs) }, 0);
    let libcs = Duration::new(libcs.tv_sec as u64, libcs.tv_nsec as u32);
    let after = monotonic();
    assert!(before <= libcs && libcs <= after, "{before:?} <= {libcs:?} <= {after:?}");
    let wall = clock_gettime(ClockId::Realtime).unwrap();
    // SAFETY: a null pointer asks only for the return value.
    let libcs = unsafe { libc::time(core::ptr::null_mut()) };
    assert!((wall.sec - libcs as i64).abs() <= 2);
}

#[test]
fn a_sleep_ends_once_its_time_has_come() {
    let start = monotonic();
    let at = start + Duration::from_millis(25);
    assert_eq!(clock_nanosleep_until(ClockId::Monotonic, Timespec::from_duration(at)), Ok(()));
    let end = monotonic();
    assert!(end >= at && end < at + Duration::from_secs(30), "slept from {start:?} to {end:?}");
    // A time already past ends at once.
    assert_eq!(clock_nanosleep_until(ClockId::Monotonic, Timespec::ZERO), Ok(()));
}

#[test]
fn a_futex_wait_ends_at_its_deadline_or_its_wake() {
    let word = Arc::new(AtomicU32::new(0));
    // The word holds something else: no wait.
    assert_eq!(futex_wait_bitset(&word, 1, None, FUTEX_ANY), Err(Errno::Again));
    // A deadline 30 ms away, with no wake.
    let start = monotonic();
    let at = Timespec::from_duration(start + Duration::from_millis(30));
    assert_eq!(futex_wait_bitset(&word, 0, Some(at), FUTEX_ANY), Err(Errno::TimedOut));
    assert!(monotonic() >= start + Duration::from_millis(30));
    // No waiter: a wake wakes none.
    assert_eq!(futex_wake(&word, 1), Ok(0));
    // A wake ends a wait whose deadline is a minute away. The waker stores first, so a wait
    // that starts after it sees the word changed (`EAGAIN`) and one before it is woken.
    let waker = {
        let word = Arc::clone(&word);
        std::thread::spawn(move || {
            clock_nanosleep_until(
                ClockId::Monotonic,
                Timespec::from_duration(monotonic() + Duration::from_millis(30)),
            )
            .unwrap();
            word.store(1, Ordering::SeqCst);
            futex_wake(&word, 1).unwrap();
        })
    };
    let start = monotonic();
    let at = Timespec::from_duration(start + Duration::from_secs(60));
    // Bounded: each pass is one wait to the same absolute time, and the word changes once.
    for _ in 0..64 {
        if word.load(Ordering::SeqCst) != 0 {
            break;
        }
        match futex_wait_bitset(&word, 0, Some(at), FUTEX_ANY) {
            Ok(()) | Err(Errno::Again | Errno::Intr) => {}
            Err(other) => panic!("a futex wait a minute long ended with {other}"),
        }
    }
    assert_eq!(word.load(Ordering::SeqCst), 1);
    assert!(monotonic() < start + Duration::from_secs(50), "woken by the wake, not the deadline");
    waker.join().unwrap();
}

#[test]
fn a_poll_ends_at_its_time_and_is_ready_once_written() {
    let (read_end, write_end) = pipe2(PipeFlags::CLOSE_ON_EXEC).unwrap();
    let mut fds = [PollFd::new(read_end.fd(), PollEvents::IN)];
    let left = Timespec::from_duration(Duration::from_millis(20));
    assert_eq!(ppoll(&mut fds, Some(left), None), Ok(0));
    assert_eq!(write(write_end.fd(), b"x"), Ok(1));
    assert_eq!(ppoll(&mut fds, Some(Timespec::from_duration(Duration::from_secs(60))), Some(SigSet::EMPTY)), Ok(1));
    assert!(fds[0].found().contains(PollEvents::IN));
    // libc's poll of the same descriptor agrees.
    let mut libcs = libc::pollfd { fd: read_end.fd().raw(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one live pollfd.
    assert_eq!(unsafe { libc::poll(&mut libcs, 1, 0) }, 1);
    assert_eq!(libcs.revents, fds[0].found().bits());
    let mut byte = [0u8; 1];
    assert_eq!(read(read_end.fd(), &mut byte), Ok(1));
    // The writer closed: the read end polls hung up and reads the end.
    close(write_end).unwrap();
    assert_eq!(ppoll(&mut fds, Some(left), None), Ok(1));
    assert!(fds[0].found().contains(PollEvents::HUP));
    assert_eq!(read(read_end.fd(), &mut byte), Ok(0));
}

#[test]
fn an_eventfd_counts() {
    let event = eventfd2(3, EventFlags::CLOSE_ON_EXEC | EventFlags::NONBLOCK).unwrap();
    let mut count = [0u8; 8];
    assert_eq!(read(event.fd(), &mut count), Ok(8));
    assert_eq!(u64::from_ne_bytes(count), 3);
    assert_eq!(read(event.fd(), &mut count), Err(Errno::Again));
}

/// A child by `clone3`, waited for by its pidfd and reaped by `waitid`.
#[test]
fn a_child_by_clone3_is_waited_for_and_reaped() {
    let mut pidfd: i32 = -1;
    let args = CloneArgs {
        flags: CloneFlags::PIDFD.bits(),
        pidfd: core::ptr::from_mut(&mut pidfd) as u64,
        exit_signal: libc::SIGCHLD as u64,
        ..CloneArgs::default()
    };
    // SAFETY: the child calls only `exit_group`, which is async-signal-safe; `pidfd` is a
    // live `int` for the kernel to write.
    let pid = match unsafe { clone3_process(&args) }.unwrap() {
        Cloned::Child => exit_group(7),
        Cloned::Parent(pid) => pid,
    };
    assert!(pid > 0 && pidfd >= 0);
    // SAFETY: the kernel returned the pidfd to this call alone.
    let pidfd = unsafe { OwnedFd::from_raw(pidfd) };
    let mut fds = [PollFd::new(pidfd.fd(), PollEvents::IN)];
    assert_eq!(ppoll(&mut fds, Some(Timespec::from_duration(Duration::from_secs(60))), None), Ok(1));
    // Looked at and left: still there to reap.
    let looked = waitid(Children::Pid(pid), WaitOptions::EXITED | WaitOptions::NO_HANG | WaitOptions::NO_WAIT);
    assert_eq!(looked, Ok(Some(Waited { pid, ended: Ended::Exited(7) })));
    let reaped = waitid(Children::Pidfd(pidfd.fd()), WaitOptions::EXITED | WaitOptions::NO_HANG);
    assert_eq!(reaped, Ok(Some(Waited { pid, ended: Ended::Exited(7) })));
    // Reaped: it is no child any more, by either call.
    assert_eq!(waitid(Children::Pid(pid), WaitOptions::EXITED | WaitOptions::NO_HANG), Err(Errno::Child));
    assert_eq!(wait4_now(pid).map(|_| ()), Err(Errno::Child));
    // A thread is not this call's to make.
    let thread = CloneArgs { flags: 0x100, ..CloneArgs::default() };
    // SAFETY: refused before any call.
    assert_eq!(unsafe { clone3_process(&thread) }, Err(Errno::Inval));
}

/// A child that is killed is reaped by `wait4` with the signal in its status, as libc
/// decodes it.
#[test]
fn a_killed_child_is_reaped_with_its_signal() {
    let args = CloneArgs { exit_signal: libc::SIGCHLD as u64, ..CloneArgs::default() };
    // SAFETY: the child only sleeps by a raw call and exits.
    let pid = match unsafe { clone3_process(&args) }.unwrap() {
        Cloned::Child => {
            _ = clock_nanosleep_until(
                ClockId::Monotonic,
                Timespec { sec: i64::MAX / 2, nsec: 0 },
            );
            exit_group(1)
        }
        Cloned::Parent(pid) => pid,
    };
    assert_eq!(wait4_now(pid).map(|it| it.map(|_| ())), Ok(None), "a running child is not reaped");
    assert_eq!(kill(pid, libc::SIGKILL as u32), Ok(()));
    let pidfd = pidfd_open(pid).unwrap();
    let mut fds = [PollFd::new(pidfd.fd(), PollEvents::IN)];
    assert_eq!(ppoll(&mut fds, Some(Timespec::from_duration(Duration::from_secs(60))), None), Ok(1));
    let (status, _rusage) = wait4_now(pid).unwrap().expect("a killed child that polled exited");
    assert!(libc::WIFSIGNALED(status));
    assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
    assert_eq!(kill(pid, 0), Err(Errno::Srch));
}

#[test]
fn random_bytes_come_whole_and_differ() {
    let (mut a, mut b) = ([0u8; 64], [0u8; 64]);
    assert_eq!(getrandom(&mut a, RandomFlags::NONE), Ok(64));
    assert_eq!(getrandom(&mut b, RandomFlags::NONBLOCK), Ok(64));
    assert_ne!(a, b, "two draws of 64 bytes were equal");
    assert_ne!(a, [0u8; 64]);
}

#[test]
fn a_stream_is_sent_and_received_over_a_unix_socket() {
    let scratch = Scratch::new("net");
    let at = SockAddr::unix(scratch.path.join("s").to_str().unwrap().as_bytes()).unwrap();
    let listener = socket(Family::Unix, SockKind::Stream, SockFlags::CLOSE_ON_EXEC).unwrap();
    assert_eq!(bind(listener.fd(), &at), Ok(()));
    assert_eq!(bind(listener.fd(), &at), Err(Errno::Inval), "a socket is bound once");
    assert_eq!(listen(listener.fd(), 8), Ok(()));
    let client = socket(Family::Unix, SockKind::Stream, SockFlags::CLOSE_ON_EXEC).unwrap();
    assert_eq!(connect(client.fd(), &at), Ok(()));
    let server = accept4(listener.fd(), SockFlags::CLOSE_ON_EXEC).unwrap();
    let parts = [IoSlice::new(b"two "), IoSlice::new(b"parts")];
    assert_eq!(sendmsg(client.fd(), &parts, MsgFlags::NO_SIGNAL), Ok(9));
    let (mut head, mut tail) = ([0u8; 4], [0u8; 16]);
    let mut into = [IoSliceMut::new(&mut head), IoSliceMut::new(&mut tail)];
    assert_eq!(recvmsg(server.fd(), &mut into, MsgFlags::NONE), Ok(9));
    assert_eq!((&head, &tail[..5]), (b"two ", &b"parts"[..]));
    assert_eq!(shutdown(client.fd(), Shutdown::Write), Ok(()));
    let mut into = [IoSliceMut::new(&mut tail)];
    assert_eq!(recvmsg(server.fd(), &mut into, MsgFlags::NONE), Ok(0), "the peer's end");
    // Nothing listens at a name that is not there, and an address too long is refused here.
    let nowhere = SockAddr::unix(scratch.path.join("none").to_str().unwrap().as_bytes()).unwrap();
    assert_eq!(connect(client.fd(), &nowhere), Err(Errno::NoEnt));
    assert_eq!(SockAddr::unix(&[b'a'; 108]).map(|_| ()), Err(Errno::NameTooLong));
    // A loopback address with nothing listening refuses the connection.
    let tcp = socket(Family::V4, SockKind::Stream, SockFlags::CLOSE_ON_EXEC).unwrap();
    assert_eq!(connect(tcp.fd(), &SockAddr::v4([127, 0, 0, 1], 1)), Err(Errno::ConnRefused));
}

#[test]
fn a_signal_mask_and_a_disposition_are_set_and_read_back() {
    let usr1 = libc::SIGUSR1 as u32;
    let before = rt_sigprocmask(MaskHow::Block, SigSet::EMPTY.with(usr1)).unwrap();
    let blocked = rt_sigprocmask(MaskHow::Set, before).unwrap();
    assert_eq!(blocked, before.with(usr1));
    // libc reads the same mask the raw call set.
    assert_eq!(rt_sigprocmask(MaskHow::Block, SigSet::EMPTY.with(usr1)).unwrap(), before);
    // SAFETY: a zeroed `sigset_t` is valid, and the call fills it.
    let libcs_has = unsafe {
        let mut set: libc::sigset_t = core::mem::zeroed();
        assert_eq!(libc::pthread_sigmask(libc::SIG_SETMASK, core::ptr::null(), &mut set), 0);
        libc::sigismember(&set, libc::SIGUSR1)
    };
    assert_eq!(libcs_has, 1);
    rt_sigprocmask(MaskHow::Set, before).unwrap();

    assert_eq!(rt_sigaction(usr1, Disposition::Ignore), Ok(()));
    // SAFETY: a zeroed `sigaction` is valid, and the call fills it.
    let handler = unsafe {
        let mut old: libc::sigaction = core::mem::zeroed();
        assert_eq!(libc::sigaction(libc::SIGUSR1, core::ptr::null(), &mut old), 0);
        old.sa_sigaction
    };
    assert_eq!(handler, libc::SIG_IGN);
    assert_eq!(rt_sigaction(usr1, Disposition::Default), Ok(()));
    assert_eq!(rt_sigaction(libc::SIGKILL as u32, Disposition::Ignore), Err(Errno::Inval));
}

#[test]
fn a_thread_is_named() {
    std::thread::spawn(|| {
        assert_eq!(prctl(Prctl::Name(c"blaze-sys-test")), Ok(()));
        let mut name = [0u8; 16];
        // SAFETY: `PR_GET_NAME` writes up to 16 bytes into a live buffer.
        assert_eq!(unsafe { libc::prctl(libc::PR_GET_NAME, name.as_mut_ptr()) }, 0);
        assert_eq!(&name[..15], b"blaze-sys-test\0");
    })
    .join()
    .unwrap();
}

/// The classic BPF program of a filter that answers `io_uring_setup` with `ENOSYS` and allows
/// every other call: what blaze's sandbox does to io_uring.
fn refuse_io_uring() -> [SockFilter; 4] {
    const LOAD_NR: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS, of `seccomp_data.nr` at 0
    const JUMP_EQ: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const RETURN: u16 = 0x06; // BPF_RET | BPF_K
    const ALLOW: u32 = 0x7fff_0000; // SECCOMP_RET_ALLOW
    const ERRNO: u32 = 0x0005_0000; // SECCOMP_RET_ERRNO
    [
        SockFilter { code: LOAD_NR, jt: 0, jf: 0, k: 0 },
        SockFilter { code: JUMP_EQ, jt: 0, jf: 1, k: nr::IO_URING_SETUP as u32 },
        SockFilter { code: RETURN, jt: 0, jf: 0, k: ERRNO | u32::from(Errno::NoSys.raw()) },
        SockFilter { code: RETURN, jt: 0, jf: 0, k: ALLOW },
    ]
}

/// Under a filter that refuses it, `io_uring_setup` gives the typed `ENOSYS`; the filter is
/// installed in a child, so the test binary keeps its ring. The control: the same call in a
/// child with no filter is not `ENOSYS` where the kernel has io_uring.
#[test]
fn io_uring_setup_refused_by_a_filter_is_the_typed_errno() {
    /// Runs `io_uring_setup` in a child, under the filter or not: 0 for a ring, else the
    /// errno's number (as the exit status, which holds 0 to 255).
    fn in_child(filtered: bool) -> i32 {
        let args = CloneArgs { exit_signal: libc::SIGCHLD as u64, ..CloneArgs::default() };
        // SAFETY: the child makes raw calls only and exits.
        let pid = match unsafe { clone3_process(&args) }.unwrap() {
            Cloned::Child => {
                if filtered {
                    let installed = prctl(Prctl::NoNewPrivs)
                        .and_then(|()| seccomp_filter(&refuse_io_uring(), SeccompFlags::NONE));
                    if installed.is_err() {
                        exit_group(250);
                    }
                }
                let mut params = UringParams::default();
                match io_uring_setup(4, &mut params) {
                    Ok(_ring) => exit_group(0),
                    Err(errno) => exit_group(i32::from(errno.raw()).min(249)),
                }
            }
            Cloned::Parent(pid) => pid,
        };
        let pidfd = pidfd_open(pid).unwrap();
        let mut fds = [PollFd::new(pidfd.fd(), PollEvents::IN)];
        assert_eq!(ppoll(&mut fds, Some(Timespec::from_duration(Duration::from_secs(60))), None), Ok(1));
        match waitid(Children::Pid(pid), WaitOptions::EXITED | WaitOptions::NO_HANG).unwrap() {
            Some(Waited { ended: Ended::Exited(status), .. }) => status,
            other => panic!("the child of the io_uring test ended as {other:?}"),
        }
    }
    assert_eq!(in_child(true), i32::from(Errno::NoSys.raw()), "the filter's answer, typed");
    let unfiltered = in_child(false);
    // A host that itself refuses io_uring (a container's own filter, or a sysctl) cannot run
    // the control; say so, and do not call that a pass of it.
    if unfiltered == i32::from(Errno::NoSys.raw()) || unfiltered == i32::from(Errno::Perm.raw()) {
        eprintln!("control skipped: this host refuses io_uring_setup itself ({unfiltered})");
    } else {
        assert_eq!(unfiltered, 0, "io_uring_setup with no filter");
    }
}

/// A ring is made, entered with nothing to do, and a wait with nothing in flight ends at its
/// time with `ETIME`.
#[test]
fn a_ring_is_set_up_and_entered() {
    let mut params = UringParams::default();
    let ring = match io_uring_setup(4, &mut params) {
        Ok(ring) => ring,
        Err(Errno::NoSys | Errno::Perm) => {
            eprintln!("skipped: this host refuses io_uring_setup");
            return;
        }
        Err(other) => panic!("io_uring_setup: {other}"),
    };
    assert!(params.sq_entries >= 4 && params.cq_entries >= params.sq_entries);
    // SAFETY: the ring holds no submission.
    unsafe {
        assert_eq!(io_uring_enter(ring.fd(), 0, 0, EnterFlags::NONE, None), Ok(0));
        let wait = EnterWait { left: Some(Timespec::from_duration(Duration::from_millis(20))), mask: None };
        assert_eq!(io_uring_enter(ring.fd(), 0, 1, EnterFlags::GET_EVENTS, Some(&wait)), Err(ETIME));
        assert_eq!(io_uring_register(ring.fd(), Register::UnregisterFiles), Err(Errno::Nxio));
        let files = [-1i32; 2];
        assert_eq!(io_uring_register(ring.fd(), Register::Files(&files)), Ok(()));
        assert_eq!(io_uring_register(ring.fd(), Register::UnregisterFiles), Ok(()));
    }
}

#[test]
fn a_raw_result_is_a_value_or_an_errno() {
    assert_eq!(raw::check(0), Ok(0));
    assert_eq!(raw::check(usize::MAX - 4095), Ok(usize::MAX - 4095), "the last value that is no error");
    assert_eq!(raw::check(usize::MAX), Err(Errno::Perm));
    assert_eq!(raw::check((-2isize) as usize), Err(Errno::NoEnt));
    assert_eq!(raw::check((-4095isize) as usize), Err(Errno::Unlisted(4095)));
    // A bad descriptor, by both routes.
    assert_eq!(fsync(Fd::new(-1)), Err(Errno::BadF));
    // SAFETY: the call takes a number.
    assert_eq!(unsafe { libc::fsync(-1) }, -1);
    assert_eq!(libc_errno(), Errno::BadF);
}
