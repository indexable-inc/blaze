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
    #[expect(
        blaze::tmp_path,
        reason = "blaze-sys sits below blaze-core, so its tests cannot use blaze_core::temp::TempDir; this Scratch removes its directory when it drops"
    )]
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

/// A pipe is not a terminal, and has no window size: `ENOTTY`, named.
#[test]
fn a_pipe_is_not_a_terminal() {
    let (read_end, write_end) = pipe2(PipeFlags::CLOSE_ON_EXEC).unwrap();
    for end in [&read_end, &write_end] {
        assert!(!is_terminal(end.fd()));
        assert_eq!(window_size(end.fd()), Err(Errno::NoTty));
    }
}

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
        ("LSEEK", libc::SYS_lseek),
        ("COPY_FILE_RANGE", libc::SYS_copy_file_range),
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
        ("GETRUSAGE", libc::SYS_getrusage),
        ("PERF_EVENT_OPEN", libc::SYS_perf_event_open),
        ("SCHED_YIELD", libc::SYS_sched_yield),
        ("INOTIFY_INIT1", libc::SYS_inotify_init1),
        ("INOTIFY_ADD_WATCH", libc::SYS_inotify_add_watch),
        ("INOTIFY_RM_WATCH", libc::SYS_inotify_rm_watch),
        ("FANOTIFY_INIT", libc::SYS_fanotify_init),
        ("FANOTIFY_MARK", libc::SYS_fanotify_mark),
        ("LINKAT", libc::SYS_linkat),
        ("SYMLINKAT", libc::SYS_symlinkat),
        ("READLINKAT", libc::SYS_readlinkat),
        ("FCHMOD", libc::SYS_fchmod),
        ("UTIMENSAT", libc::SYS_utimensat),
        ("FSTATFS", libc::SYS_fstatfs),
        ("FLOCK", libc::SYS_flock),
        ("EXECVEAT", libc::SYS_execveat),
        ("SETSOCKOPT", libc::SYS_setsockopt),
        ("LANDLOCK_CREATE_RULESET", libc::SYS_landlock_create_ruleset),
        ("LANDLOCK_ADD_RULE", libc::SYS_landlock_add_rule),
        ("LANDLOCK_RESTRICT_SELF", libc::SYS_landlock_restrict_self),
        ("PRLIMIT64", libc::SYS_prlimit64),
        ("IOPRIO_SET", libc::SYS_ioprio_set),
        ("IOPRIO_GET", libc::SYS_ioprio_get),
        ("RENAMEAT", libc::SYS_renameat),
        ("NAME_TO_HANDLE_AT", libc::SYS_name_to_handle_at),
        ("OPEN_BY_HANDLE_AT", libc::SYS_open_by_handle_at),
        ("PIDFD_SEND_SIGNAL", libc::SYS_pidfd_send_signal),
        ("GETTID", libc::SYS_gettid),
        ("MEMBARRIER", libc::SYS_membarrier),
        ("CLOSE_RANGE", libc::SYS_close_range),
        ("MBIND", libc::SYS_mbind),
        ("PROCESS_MADVISE", libc::SYS_process_madvise),
        ("SOCKETPAIR", libc::SYS_socketpair),
        ("GETSOCKNAME", libc::SYS_getsockname),
        ("GETSOCKOPT", libc::SYS_getsockopt),
        ("SENDMMSG", libc::SYS_sendmmsg),
        ("RECVMMSG", libc::SYS_recvmmsg),
        ("FCNTL", libc::SYS_fcntl),
        ("DUP", libc::SYS_dup),
        ("DUP3", libc::SYS_dup3),
        ("PREADV", libc::SYS_preadv),
        ("FALLOCATE", libc::SYS_fallocate),
        ("MEMFD_CREATE", libc::SYS_memfd_create),
        ("MINCORE", libc::SYS_mincore),
        ("MSYNC", libc::SYS_msync),
        ("UNAME", libc::SYS_uname),
        ("GETUID", libc::SYS_getuid),
        ("GETEUID", libc::SYS_geteuid),
        ("GETPPID", libc::SYS_getppid),
        ("GETPGID", libc::SYS_getpgid),
        ("SETPGID", libc::SYS_setpgid),
        ("SETSID", libc::SYS_setsid),
        ("TGKILL", libc::SYS_tgkill),
        ("SCHED_SETAFFINITY", libc::SYS_sched_setaffinity),
        ("SCHED_GETAFFINITY", libc::SYS_sched_getaffinity),
        ("SIGALTSTACK", libc::SYS_sigaltstack),
        ("RT_SIGRETURN", libc::SYS_rt_sigreturn),
        ("CLONE", libc::SYS_clone),
        ("CHDIR", libc::SYS_chdir),
        ("UNSHARE", libc::SYS_unshare),
        ("PROCESS_VM_READV", libc::SYS_process_vm_readv),
        ("NEWFSTATAT", libc::SYS_newfstatat),
        ("STATFS", libc::SYS_statfs),
        ("MKNODAT", libc::SYS_mknodat),
        ("FCHOWNAT", libc::SYS_fchownat),
        ("SETXATTR", libc::SYS_setxattr),
        ("REMOVEXATTR", libc::SYS_removexattr),
        ("FGETXATTR", libc::SYS_fgetxattr),
        ("FADVISE64", libc::SYS_fadvise64),
        ("MOUNT", libc::SYS_mount),
        ("UMOUNT2", libc::SYS_umount2),
        ("GETPRIORITY", libc::SYS_getpriority),
        ("SETPRIORITY", libc::SYS_setpriority),
        ("SYSINFO", libc::SYS_sysinfo),
        ("PWRITEV2", libc::SYS_pwritev2),
        ("FUTEX_WAITV", libc::SYS_futex_waitv),
        ("MEMFD_SECRET", libc::SYS_memfd_secret),
        // libc 0.2.191 names `SYS_cachestat` for other architectures only, not x86-64 or
        // aarch64: the kernel's number (both tables, 6.5), written here.
        ("CACHESTAT", 451),
        ("MSEAL", libc::SYS_mseal),
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
    // `libc::CLONE_CLEAR_SIGHAND` and `libc::CLONE_INTO_CGROUP` are typed as `c_int` in `libc`,
    // truncating `0x1_0000_0000` and `0x2_0000_0000` to 0.
    assert_eq!(CloneFlags::CLEAR_SIGHAND.bits(), 0x1_0000_0000u64);
    assert_eq!(CloneFlags::INTO_CGROUP.bits(), 0x2_0000_0000u64);
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
        (Errno::NoData, libc::ENODATA),
        (Errno::Proto, libc::EPROTO),
        (Errno::NoProtoOpt, libc::ENOPROTOOPT),
        (Errno::HostDown, libc::EHOSTDOWN),
        (Errno::NoNet, libc::ENONET),
        // On Linux `ENOTSUP == EOPNOTSUPP`, so `Errno::NotSup` holds the sentinel `65535`.
        (Errno::NotSup, 65535),
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

/// The symbols of `Effect::CopyData` exist and do what it rests on: `lseek` finds a file's
/// data and its holes (a file system that keeps no holes says the whole file is data, which
/// is still true of it), `copy_file_range` copies a range between two files at the offsets
/// given without moving either's position, and a file made by `ftruncate` plus a copy of the
/// data alone reads the same as its source. Each against libc's same call.
#[test]
fn a_files_data_is_found_and_copied_in_the_kernel() {
    const LEN: u64 = 1 << 20;
    const AT: u64 = 512 << 10;
    let scratch = Scratch::new("copy-data");
    let from = openat(scratch.fd(), Path::new(c"from"), CREATE, 0o644).unwrap();
    assert_eq!(ftruncate(from.fd(), LEN), Ok(()));
    assert_eq!(pwrite(from.fd(), &[7u8; 4096], AT), Ok(4096));
    // SAFETY: both calls take numbers and no memory.
    let (data, hole) = unsafe {
        (libc::lseek(from.fd().raw(), 0, libc::SEEK_DATA), libc::lseek(from.fd().raw(), AT as i64, libc::SEEK_HOLE))
    };
    let first = lseek(from.fd(), 0, Whence::Data).unwrap();
    assert_eq!(first as i64, data, "the first data is where libc finds it");
    assert!(first <= AT, "the data written at {AT} is found at or before it: {first}");
    let end = lseek(from.fd(), AT, Whence::Hole).unwrap();
    assert_eq!(end as i64, hole, "the data's end is where libc finds it");
    assert!((AT + 4096..=LEN).contains(&end), "{end}");
    assert_eq!(lseek(from.fd(), LEN, Whence::Data), Err(Errno::Nxio), "no data at the end");

    let to = openat(scratch.fd(), Path::new(c"to"), CREATE | OpenFlags::EXCLUSIVE, 0o644).unwrap();
    assert_eq!(ftruncate(to.fd(), LEN), Ok(()));
    let mut at = first;
    while at < end {
        let copied = copy_file_range(from.fd(), at, to.fd(), at, end - at).unwrap();
        assert!(copied > 0, "a copy inside the source's length moves bytes");
        at += copied as u64;
    }
    assert_eq!(copy_file_range(from.fd(), LEN, to.fd(), LEN, 4096), Ok(0), "nothing past the end");
    let mut back = [1u8; 8192];
    assert_eq!(pread(to.fd(), &mut back, AT - 2048), Ok(8192));
    assert_eq!(back[..2048], [0u8; 2048]);
    assert_eq!(back[2048..6144], [7u8; 4096]);
    assert_eq!(back[6144..], [0u8; 2048]);
    let len = statx(to.fd(), Path::EMPTY, AtFlags::EMPTY_PATH, StatxMask::BASIC).unwrap();
    assert_eq!(len.size, LEN);
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
    let at2 = SockAddr::unix(scratch.path.join("s2").to_str().unwrap().as_bytes()).unwrap();
    let listener = socket(Family::Unix, SockKind::Stream, SockFlags::CLOSE_ON_EXEC).unwrap();
    assert_eq!(bind(listener.fd(), &at), Ok(()));
    assert_eq!(bind(listener.fd(), &at2), Err(Errno::Inval), "a socket is bound once");
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
        assert_eq!(super::thread_name().map(|ours| ours == name), Ok(true));
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

/// The meter's reads: a thread's usage is its own (a busy second thread's time is not in this
/// one's), the process's holds both, and the perf group counts this thread's instructions or
/// is refused with an errno (a VM without a PMU), never a zero.
#[test]
fn the_meters_counters_are_this_threads() {
    let mine = getrusage(Who::Thread).unwrap();
    // SAFETY: libc's oracle writes one `struct rusage` into the local given.
    let mut oracle: libc::rusage = unsafe { core::mem::zeroed() };
    // SAFETY: as above.
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_THREAD, &raw mut oracle) }, 0);
    // `rest` is `ru_ixrss` on: `ru_minflt` is its fourth.
    assert!(oracle.ru_minflt >= mine.rest[3], "the oracle reads after: its minor faults are no fewer");
    let busy = std::thread::spawn(|| {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_millis(50) {
            core::hint::black_box(0u64);
        }
        getrusage(Who::Thread).unwrap()
    })
    .join()
    .unwrap();
    let after = getrusage(Who::Thread).unwrap();
    let micros = |t: [i64; 2]| t[0] * 1_000_000 + t[1];
    assert!(micros(busy.user) + micros(busy.system) >= 40_000, "the busy thread ran 50 ms");
    assert!(micros(after.user) + micros(after.system) - (micros(mine.user) + micros(mine.system)) < 40_000, "its time is not this thread's");
    let all = getrusage(Who::Process).unwrap();
    assert!(micros(all.user) + micros(all.system) >= micros(busy.user) + micros(busy.system));
    match perf_counters_open() {
        Ok(counters) => {
            let (before, _) = perf_counters_read(counters.instructions.fd()).unwrap();
            let mut spin = 0u64;
            for at in 0..1_000_000u64 {
                spin = core::hint::black_box(spin.wrapping_add(at));
            }
            let (after, cycles) = perf_counters_read(counters.instructions.fd()).unwrap();
            assert!(after > before + 1_000_000 && cycles > 0, "{before} {after} {cycles}");
        }
        Err(errno) => println!("perf_event_open refused here: {errno:?} (the meter prints instructions unavailable)"),
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

/// The flags of the 2026-11-20 calls (inotify, fanotify, links, times, locks) are libc's.
#[test]
fn the_notify_link_and_lock_constants_are_libcs() {
    assert_eq!(AtFlags::FOLLOW.bits() as libc::c_int, libc::AT_SYMLINK_FOLLOW);
    assert_eq!(InotifyFlags::CLOSE_ON_EXEC.bits() as libc::c_int, libc::IN_CLOEXEC);
    assert_eq!(InotifyFlags::NONBLOCK.bits() as libc::c_int, libc::IN_NONBLOCK);
    let inotify: &[(InotifyMask, u32, &str)] = &[
        (InotifyMask::ACCESS, libc::IN_ACCESS as u32, "IN_ACCESS"),
        (InotifyMask::MODIFY, libc::IN_MODIFY as u32, "IN_MODIFY"),
        (InotifyMask::ATTRIB, libc::IN_ATTRIB as u32, "IN_ATTRIB"),
        (InotifyMask::CLOSE_WRITE, libc::IN_CLOSE_WRITE as u32, "IN_CLOSE_WRITE"),
        (InotifyMask::CLOSE_NO_WRITE, libc::IN_CLOSE_NOWRITE as u32, "IN_CLOSE_NOWRITE"),
        (InotifyMask::OPEN, libc::IN_OPEN as u32, "IN_OPEN"),
        (InotifyMask::MOVED_FROM, libc::IN_MOVED_FROM as u32, "IN_MOVED_FROM"),
        (InotifyMask::MOVED_TO, libc::IN_MOVED_TO as u32, "IN_MOVED_TO"),
        (InotifyMask::CREATE, libc::IN_CREATE as u32, "IN_CREATE"),
        (InotifyMask::DELETE, libc::IN_DELETE as u32, "IN_DELETE"),
        (InotifyMask::DELETE_SELF, libc::IN_DELETE_SELF as u32, "IN_DELETE_SELF"),
        (InotifyMask::MOVE_SELF, libc::IN_MOVE_SELF as u32, "IN_MOVE_SELF"),
        (InotifyMask::UNMOUNT, libc::IN_UNMOUNT as u32, "IN_UNMOUNT"),
        (InotifyMask::OVERFLOW, libc::IN_Q_OVERFLOW as u32, "IN_Q_OVERFLOW"),
        (InotifyMask::IGNORED, libc::IN_IGNORED as u32, "IN_IGNORED"),
        (InotifyMask::ONLY_DIR, libc::IN_ONLYDIR as u32, "IN_ONLYDIR"),
        (InotifyMask::NO_FOLLOW, libc::IN_DONT_FOLLOW as u32, "IN_DONT_FOLLOW"),
        (InotifyMask::EXCLUDE_UNLINKED, libc::IN_EXCL_UNLINK as u32, "IN_EXCL_UNLINK"),
        (InotifyMask::MASK_CREATE, libc::IN_MASK_CREATE as u32, "IN_MASK_CREATE"),
        (InotifyMask::MASK_ADD, libc::IN_MASK_ADD as u32, "IN_MASK_ADD"),
        (InotifyMask::IS_DIR, libc::IN_ISDIR as u32, "IN_ISDIR"),
        (InotifyMask::ONE_SHOT, libc::IN_ONESHOT as u32, "IN_ONESHOT"),
    ];
    for &(ours, libcs, name) in inotify {
        assert_eq!(ours.bits(), libcs, "{name}");
    }
    let init: &[(FanInit, u32, &str)] = &[
        (FanInit::CLOSE_ON_EXEC, libc::FAN_CLOEXEC as u32, "FAN_CLOEXEC"),
        (FanInit::NONBLOCK, libc::FAN_NONBLOCK as u32, "FAN_NONBLOCK"),
        (FanInit::CLASS_NOTIF, libc::FAN_CLASS_NOTIF as u32, "FAN_CLASS_NOTIF"),
        (FanInit::CLASS_CONTENT, libc::FAN_CLASS_CONTENT as u32, "FAN_CLASS_CONTENT"),
        (FanInit::CLASS_PRE_CONTENT, libc::FAN_CLASS_PRE_CONTENT as u32, "FAN_CLASS_PRE_CONTENT"),
        (FanInit::UNLIMITED_QUEUE, libc::FAN_UNLIMITED_QUEUE as u32, "FAN_UNLIMITED_QUEUE"),
        (FanInit::UNLIMITED_MARKS, libc::FAN_UNLIMITED_MARKS as u32, "FAN_UNLIMITED_MARKS"),
        (FanInit::REPORT_TID, libc::FAN_REPORT_TID as u32, "FAN_REPORT_TID"),
        (FanInit::REPORT_FID, libc::FAN_REPORT_FID as u32, "FAN_REPORT_FID"),
        (FanInit::REPORT_DIR_FID, libc::FAN_REPORT_DIR_FID as u32, "FAN_REPORT_DIR_FID"),
        (FanInit::REPORT_NAME, libc::FAN_REPORT_NAME as u32, "FAN_REPORT_NAME"),
        (FanInit::REPORT_TARGET_FID, libc::FAN_REPORT_TARGET_FID as u32, "FAN_REPORT_TARGET_FID"),
    ];
    for &(ours, libcs, name) in init {
        assert_eq!(ours.bits(), libcs, "{name}");
    }
    let mark: &[(FanMark, u32, &str)] = &[
        (FanMark::ADD, libc::FAN_MARK_ADD as u32, "FAN_MARK_ADD"),
        (FanMark::REMOVE, libc::FAN_MARK_REMOVE as u32, "FAN_MARK_REMOVE"),
        (FanMark::NO_FOLLOW, libc::FAN_MARK_DONT_FOLLOW as u32, "FAN_MARK_DONT_FOLLOW"),
        (FanMark::ONLY_DIR, libc::FAN_MARK_ONLYDIR as u32, "FAN_MARK_ONLYDIR"),
        (FanMark::MOUNT, libc::FAN_MARK_MOUNT as u32, "FAN_MARK_MOUNT"),
        (FanMark::IGNORED_MASK, libc::FAN_MARK_IGNORED_MASK as u32, "FAN_MARK_IGNORED_MASK"),
        (FanMark::IGNORED_SURVIVES_MODIFY, libc::FAN_MARK_IGNORED_SURV_MODIFY as u32, "FAN_MARK_IGNORED_SURV_MODIFY"),
        (FanMark::FLUSH, libc::FAN_MARK_FLUSH as u32, "FAN_MARK_FLUSH"),
        (FanMark::FILESYSTEM, libc::FAN_MARK_FILESYSTEM as u32, "FAN_MARK_FILESYSTEM"),
    ];
    for &(ours, libcs, name) in mark {
        assert_eq!(ours.bits(), libcs, "{name}");
    }
    let mask: &[(FanMask, u64, &str)] = &[
        (FanMask::ACCESS, libc::FAN_ACCESS as u64, "FAN_ACCESS"),
        (FanMask::MODIFY, libc::FAN_MODIFY as u64, "FAN_MODIFY"),
        (FanMask::ATTRIB, libc::FAN_ATTRIB as u64, "FAN_ATTRIB"),
        (FanMask::CLOSE_WRITE, libc::FAN_CLOSE_WRITE as u64, "FAN_CLOSE_WRITE"),
        (FanMask::CLOSE_NO_WRITE, libc::FAN_CLOSE_NOWRITE as u64, "FAN_CLOSE_NOWRITE"),
        (FanMask::OPEN, libc::FAN_OPEN as u64, "FAN_OPEN"),
        (FanMask::MOVED_FROM, libc::FAN_MOVED_FROM as u64, "FAN_MOVED_FROM"),
        (FanMask::MOVED_TO, libc::FAN_MOVED_TO as u64, "FAN_MOVED_TO"),
        (FanMask::CREATE, libc::FAN_CREATE as u64, "FAN_CREATE"),
        (FanMask::DELETE, libc::FAN_DELETE as u64, "FAN_DELETE"),
        (FanMask::DELETE_SELF, libc::FAN_DELETE_SELF as u64, "FAN_DELETE_SELF"),
        (FanMask::MOVE_SELF, libc::FAN_MOVE_SELF as u64, "FAN_MOVE_SELF"),
        (FanMask::OPEN_EXEC, libc::FAN_OPEN_EXEC as u64, "FAN_OPEN_EXEC"),
        (FanMask::OVERFLOW, libc::FAN_Q_OVERFLOW as u64, "FAN_Q_OVERFLOW"),
        (FanMask::FS_ERROR, libc::FAN_FS_ERROR as u64, "FAN_FS_ERROR"),
        (FanMask::ON_CHILD, libc::FAN_EVENT_ON_CHILD as u64, "FAN_EVENT_ON_CHILD"),
        (FanMask::RENAME, libc::FAN_RENAME as u64, "FAN_RENAME"),
        (FanMask::ON_DIR, libc::FAN_ONDIR as u64, "FAN_ONDIR"),
    ];
    for &(ours, libcs, name) in mask {
        assert_eq!(ours.bits(), libcs, "{name}");
    }
    assert_eq!(FAN_METADATA_VERSION, libc::FANOTIFY_METADATA_VERSION as u8);
    assert_eq!(LockOp::SHARED.bits() as libc::c_int, libc::LOCK_SH);
    assert_eq!(LockOp::EXCLUSIVE.bits() as libc::c_int, libc::LOCK_EX);
    assert_eq!(LockOp::NONBLOCK.bits() as libc::c_int, libc::LOCK_NB);
    assert_eq!(LockOp::UNLOCK.bits() as libc::c_int, libc::LOCK_UN);
}

/// The structures of the 2026-11-20 calls are the kernel's: libc's where it has them, the
/// uapi headers' sizes for Landlock, which libc lacks.
#[test]
fn the_notify_statfs_and_landlock_layouts_are_the_kernels() {
    use core::mem::offset_of;
    assert_eq!(size_of::<Statfs>(), 120);
    assert_eq!(size_of::<Statfs>(), size_of::<libc::statfs>());
    assert_eq!(offset_of!(Statfs, kind), offset_of!(libc::statfs, f_type));
    assert_eq!(offset_of!(Statfs, block_size), offset_of!(libc::statfs, f_bsize));
    assert_eq!(offset_of!(Statfs, blocks), offset_of!(libc::statfs, f_blocks));
    assert_eq!(offset_of!(Statfs, blocks_available), offset_of!(libc::statfs, f_bavail));
    assert_eq!(offset_of!(Statfs, files_free), offset_of!(libc::statfs, f_ffree));
    assert_eq!(offset_of!(Statfs, fsid), offset_of!(libc::statfs, f_fsid));
    assert_eq!(offset_of!(Statfs, name_max), offset_of!(libc::statfs, f_namelen));
    assert_eq!(offset_of!(Statfs, fragment_size), offset_of!(libc::statfs, f_frsize));
    assert_eq!(INOTIFY_EVENT_HEADER, size_of::<libc::inotify_event>());
    assert_eq!(size_of::<FanEventMetadata>(), size_of::<libc::fanotify_event_metadata>());
    assert_eq!(offset_of!(FanEventMetadata, mask), offset_of!(libc::fanotify_event_metadata, mask));
    assert_eq!(offset_of!(FanEventMetadata, fd), offset_of!(libc::fanotify_event_metadata, fd));
    assert_eq!(offset_of!(FanEventMetadata, pid), offset_of!(libc::fanotify_event_metadata, pid));
    // linux/landlock.h: three `__u64`s; a packed `__u64` and `__s32`; two `__u64`s.
    assert_eq!(size_of::<RulesetAttr>(), 24);
    assert_eq!(size_of::<PathBeneath>(), 12);
    assert_eq!(size_of::<NetPort>(), 16);
    assert_eq!(AccessFs::ALL_ABI1.bits(), (1 << 13) - 1);
    assert_eq!(size_of::<Arg<'_>>(), size_of::<*const libc::c_char>());
    assert_eq!(size_of::<Timespec>(), size_of::<libc::timespec>(), "utimensat's two times");
}

/// A hard link, a symbolic link and its target, a mode, two times and a lock, each read back
/// through libc.
#[test]
fn links_modes_times_and_locks_are_set_and_read_back() {
    let scratch = Scratch::new("links");
    let file = openat(scratch.fd(), Path::new(c"a"), CREATE | OpenFlags::EXCLUSIVE, 0o644).unwrap();
    assert_eq!(write(file.fd(), b"linked"), Ok(6));

    assert_eq!(linkat(scratch.fd(), Path::new(c"a"), scratch.fd(), Path::new(c"b"), AtFlags::NONE), Ok(()));
    assert_eq!(
        linkat(scratch.fd(), Path::new(c"a"), scratch.fd(), Path::new(c"b"), AtFlags::NONE),
        Err(Errno::Exist),
        "nothing is replaced"
    );
    assert_eq!(std::fs::read(scratch.path.join("b")).unwrap(), b"linked");

    assert_eq!(symlinkat(Path::new(c"a"), scratch.fd(), Path::new(c"l")), Ok(()));
    let mut target = [0u8; 64];
    assert_eq!(readlinkat(scratch.fd(), Path::new(c"l"), &mut target), Ok(&b"a"[..]));
    let mut full = [0u8; 1];
    assert_eq!(readlinkat(scratch.fd(), Path::new(c"l"), &mut full), Err(Errno::NameTooLong), "a cut name is refused");
    assert_eq!(readlinkat(scratch.fd(), Path::new(c"a"), &mut target), Err(Errno::Inval), "not a link");
    assert_eq!(std::fs::read_link(scratch.path.join("l")).unwrap(), std::path::Path::new("a"));
    // A link that follows its symbolic link names the file, not the link.
    assert_eq!(linkat(scratch.fd(), Path::new(c"l"), scratch.fd(), Path::new(c"c"), AtFlags::FOLLOW), Ok(()));
    assert!(std::fs::symlink_metadata(scratch.path.join("c")).unwrap().is_file());

    assert_eq!(fchmod(file.fd(), 0o751), Ok(()));
    // SAFETY: all-zero bytes is a valid `stat`, which `fstat` fills.
    let mut stat: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: a live descriptor and a live `stat`.
    assert_eq!(unsafe { libc::fstat(file.fd().raw(), &mut stat) }, 0);
    assert_eq!(stat.st_mode & 0o7777, 0o751);

    let then = Timespec { sec: 1_000_000_000, nsec: 5 };
    assert_eq!(utimensat(scratch.fd(), Path::new(c"a"), FileTime::Keep, FileTime::At(then), AtFlags::NONE), Ok(()));
    // SAFETY: as above.
    assert_eq!(unsafe { libc::fstat(file.fd().raw(), &mut stat) }, 0);
    assert_eq!((stat.st_mtime, stat.st_mtime_nsec), (1_000_000_000, 5));
    let late = Timespec { sec: 0, nsec: 1_000_000_000 };
    assert_eq!(
        utimensat(scratch.fd(), Path::new(c"a"), FileTime::At(late), FileTime::Keep, AtFlags::NONE),
        Err(Errno::Inval),
        "a second's nanoseconds"
    );
    assert_eq!(utimensat(file.fd(), Path::EMPTY, FileTime::Now, FileTime::Now, AtFlags::EMPTY_PATH), Ok(()));
    // SAFETY: as above.
    assert_eq!(unsafe { libc::fstat(file.fd().raw(), &mut stat) }, 0);
    assert!(stat.st_mtime > 1_000_000_000, "now is after 2001");

    let other = openat(scratch.fd(), Path::new(c"a"), OpenFlags::READ | OpenFlags::CLOSE_ON_EXEC, 0).unwrap();
    assert_eq!(flock(file.fd(), LockOp::EXCLUSIVE | LockOp::NONBLOCK), Ok(()));
    assert_eq!(flock(other.fd(), LockOp::SHARED | LockOp::NONBLOCK), Err(Errno::Again), "held by another open");
    // SAFETY: a live descriptor and an operation.
    assert_eq!(unsafe { libc::flock(other.fd().raw(), libc::LOCK_SH | libc::LOCK_NB) }, -1);
    assert_eq!(libc_errno(), Errno::Again);
    assert_eq!(flock(file.fd(), LockOp::UNLOCK), Ok(()));
    assert_eq!(flock(other.fd(), LockOp::SHARED | LockOp::NONBLOCK), Ok(()));

    let ours = fstatfs(file.fd()).unwrap();
    // SAFETY: all-zero bytes is a valid `statfs`, which `fstatfs` fills.
    let mut theirs: libc::statfs = unsafe { core::mem::zeroed() };
    // SAFETY: a live descriptor and a live `statfs`.
    assert_eq!(unsafe { libc::fstatfs(file.fd().raw(), &mut theirs) }, 0);
    assert_eq!((ours.kind, ours.block_size, ours.name_max), (theirs.f_type as i64, theirs.f_bsize as i64, theirs.f_namelen as i64));
    assert_eq!(fstatfs(Fd::new(-1)), Err(Errno::BadF));
}

/// A created child is an inotify event naming it; a path not there is refused; the walk
/// reads a padded name and stops at a cut event.
#[test]
fn an_inotify_watch_reports_a_created_child() {
    let scratch = Scratch::new("inotify");
    let inotify = inotify_init1(InotifyFlags::CLOSE_ON_EXEC | InotifyFlags::NONBLOCK).unwrap();
    let dir = CString::new(scratch.path.to_str().unwrap()).unwrap();
    let watch = inotify_add_watch(inotify.fd(), Path::new(&dir), InotifyMask::CREATE | InotifyMask::ONLY_DIR).unwrap();
    let mut events = [0u8; 256];
    assert_eq!(read(inotify.fd(), &mut events), Err(Errno::Again), "nothing yet");
    drop(openat(scratch.fd(), Path::new(c"child"), CREATE, 0o644).unwrap());
    let filled = read(inotify.fd(), &mut events).unwrap();
    let got: std::vec::Vec<_> = InotifyEvents::new(&events[..filled]).collect();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0].watch, got[0].name), (watch, &b"child"[..]));
    assert!(got[0].mask.contains(InotifyMask::CREATE));
    assert_eq!(inotify_rm_watch(inotify.fd(), watch), Ok(()));
    assert_eq!(inotify_add_watch(inotify.fd(), Path::new(c"/no/such/blaze/dir"), InotifyMask::CREATE), Err(Errno::NoEnt));

    // A synthetic read: one event with a name padded to 8, then a header cut short.
    let mut bytes = std::vec::Vec::new();
    for word in [7u32, InotifyMask::DELETE.bits(), 0, 8] {
        bytes.extend_from_slice(&word.to_ne_bytes());
    }
    bytes.extend_from_slice(b"gone\0\0\0\0");
    bytes.extend_from_slice(&[1, 2, 3]);
    let walked: std::vec::Vec<_> = InotifyEvents::new(&bytes).collect();
    assert_eq!(walked, [InotifyEvent { watch: Watch(7), mask: InotifyMask::DELETE, cookie: 0, name: b"gone" }]);
}

/// fanotify answers as libc's call does here (a group, or the same refusal), and a group's
/// mark on a directory is taken; the walk refuses another version and a short length.
#[test]
fn fanotify_answers_as_libc_and_its_walk_refuses_what_it_cannot_read() {
    let flags = FanInit::REPORT_FID | FanInit::CLOSE_ON_EXEC | FanInit::NONBLOCK;
    // SAFETY: the call takes numbers.
    let libcs = unsafe { libc::fanotify_init(flags.bits(), libc::O_RDONLY as u32) };
    let libcs = if libcs < 0 { Err(libc_errno()) } else { Ok(libcs) };
    match (fanotify_init(flags, OpenFlags::READ), libcs) {
        (Ok(group), Ok(theirs)) => {
            // SAFETY: libc's descriptor, closed once.
            unsafe { libc::close(theirs) };
            let scratch = Scratch::new("fanotify");
            let mask = FanMask::CREATE | FanMask::MODIFY | FanMask::ON_CHILD;
            assert_eq!(fanotify_mark(group.fd(), FanMark::ADD, mask, scratch.fd(), Path::EMPTY), Ok(()));
        }
        (Err(ours), Err(theirs)) => {
            assert_eq!(ours, theirs);
            println!("fanotify_init refused here: {ours:?} (the watcher reports it)");
        }
        (ours, theirs) => panic!("ours {ours:?}, libc's {theirs:?}"),
    }

    let head = |version: u8, metadata_len: u16, event_len: u32| {
        let mut bytes = std::vec::Vec::new();
        bytes.extend_from_slice(&event_len.to_ne_bytes());
        bytes.extend_from_slice(&[version, 0]);
        bytes.extend_from_slice(&metadata_len.to_ne_bytes());
        bytes.extend_from_slice(&FanMask::CREATE.bits().to_ne_bytes());
        bytes.extend_from_slice(&(-1i32).to_ne_bytes());
        bytes.extend_from_slice(&42i32.to_ne_bytes());
        bytes
    };
    let mut one = head(FAN_METADATA_VERSION, 24, 28);
    one.extend_from_slice(&[9, 9, 9, 9]);
    let mut events = FanEvents::new(&one);
    assert_eq!(events.next(), Some(FanEvent { mask: FanMask::CREATE, fd: -1, pid: 42, info: &[9, 9, 9, 9] }));
    assert_eq!((events.next(), events.refused()), (None, false));
    let other = head(2, 24, 24);
    let mut events = FanEvents::new(&other);
    assert_eq!((events.next(), events.refused()), (None, true), "another version");
    let short = head(FAN_METADATA_VERSION, 24, 8);
    let mut events = FanEvents::new(&short);
    assert_eq!((events.next(), events.refused()), (None, true), "a length shorter than its head");
}

/// Each typed option reads back through libc's `getsockopt`; a count no `int` holds is refused
/// before the call.
#[test]
fn socket_options_are_set_and_read_back() {
    let get = |fd: Fd, level: libc::c_int, name: libc::c_int| {
        let mut value = [0 as libc::c_int; 2];
        let mut len = size_of_val(&value) as libc::socklen_t;
        // SAFETY: a live buffer of the length passed.
        let said = unsafe { libc::getsockopt(fd.raw(), level, name, value.as_mut_ptr().cast(), &mut len) };
        assert_eq!(said, 0);
        value
    };
    let tcp = socket(Family::V4, SockKind::Stream, SockFlags::CLOSE_ON_EXEC).unwrap();
    let fd = tcp.fd();
    assert_eq!(setsockopt(fd, SockOpt::NoDelay(true)), Ok(()));
    assert_eq!(get(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY)[0], 1);
    assert_eq!(setsockopt(fd, SockOpt::ReuseAddr(true)), Ok(()));
    assert_eq!(get(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR)[0], 1);
    assert_eq!(setsockopt(fd, SockOpt::ReusePort(true)), Ok(()));
    assert_eq!(get(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT)[0], 1);
    assert_eq!(setsockopt(fd, SockOpt::KeepAlive(true)), Ok(()));
    assert_eq!(get(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE)[0], 1);
    assert_eq!(setsockopt(fd, SockOpt::KeepIdle(30)), Ok(()));
    assert_eq!(get(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE)[0], 30);
    assert_eq!(setsockopt(fd, SockOpt::KeepInterval(7)), Ok(()));
    assert_eq!(get(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL)[0], 7);
    assert_eq!(setsockopt(fd, SockOpt::KeepCount(4)), Ok(()));
    assert_eq!(get(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT)[0], 4);
    assert_eq!(setsockopt(fd, SockOpt::Linger(Some(3))), Ok(()));
    assert_eq!(get(fd, libc::SOL_SOCKET, libc::SO_LINGER), [1, 3]);
    assert_eq!(setsockopt(fd, SockOpt::Linger(None)), Ok(()));
    assert_eq!(get(fd, libc::SOL_SOCKET, libc::SO_LINGER)[0], 0);
    assert_eq!(setsockopt(fd, SockOpt::RecvBuf(1 << 16)), Ok(()));
    assert!(get(fd, libc::SOL_SOCKET, libc::SO_RCVBUF)[0] >= 1 << 16, "the kernel doubles it");
    assert_eq!(setsockopt(fd, SockOpt::SendBuf(u32::MAX)), Err(Errno::Inval), "no int holds it");
    assert_eq!(setsockopt(fd, SockOpt::KeepIdle(1 << 31)), Err(Errno::Inval));
    let v6 = socket(Family::V6, SockKind::Stream, SockFlags::CLOSE_ON_EXEC).unwrap();
    assert_eq!(setsockopt(v6.fd(), SockOpt::V6Only(true)), Ok(()));
    assert_eq!(get(v6.fd(), libc::IPPROTO_IPV6, libc::IPV6_V6ONLY)[0], 1);
    let unix = socket(Family::Unix, SockKind::Stream, SockFlags::CLOSE_ON_EXEC).unwrap();
    assert_eq!(setsockopt(unix.fd(), SockOpt::PassCred(true)), Ok(()));
    assert_eq!(setsockopt(Fd::new(-1), SockOpt::NoDelay(true)), Err(Errno::BadF));
}

/// `sched_yield` answers; `kill_group` refuses the groups that are not one, signals this
/// process's own group with 0, and finds no group that is not there; `execveat` of a program
/// not there returns its refusal and this process goes on; a list not ended is refused.
#[test]
fn yield_group_signals_and_a_refused_exec() {
    assert_eq!(sched_yield(), Ok(()));
    assert_eq!(kill_group(0, 0), Err(Errno::Inval), "kill(0) is no group");
    assert_eq!(kill_group(1, 0), Err(Errno::Inval), "kill(-1) is every process");
    assert_eq!(kill_group(-5, 0), Err(Errno::Inval));
    // SAFETY: the call takes nothing.
    let own = unsafe { libc::getpgrp() };
    if own > 1 {
        assert_eq!(kill_group(own, 0), Ok(()), "signal 0 only checks");
    }
    assert_eq!(kill_group(i32::MAX, 0), Err(Errno::Srch));

    let program = Arg::new(c"/no/such/blaze/program");
    let argv = [program, Arg::END];
    let envp = [Arg::END];
    let (argv, envp) = (Args::new(&argv).unwrap(), Args::new(&envp).unwrap());
    let refused = execveat(CWD, Path::new(c"/no/such/blaze/program"), argv, envp, AtFlags::NONE);
    assert_eq!(refused, Errno::NoEnt);
    assert_eq!(Args::new(&[]).map(|_| ()), Err(Errno::Inval), "empty");
    assert_eq!(Args::new(&[program]).map(|_| ()), Err(Errno::Inval), "not ended");
    assert_eq!(Args::new(&[Arg::END, program, Arg::END]).map(|_| ()), Err(Errno::Inval), "ended early");
}

/// Landlock's ABI is libc's raw call's answer; where it is there, a ruleset is made and a rule
/// for a directory is added (no process is confined: a test cannot undo it).
#[test]
fn landlock_answers_as_the_raw_call_and_takes_a_rule() {
    // SAFETY: a null attribute of size 0 with the version flag reads no memory.
    let libcs = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, core::ptr::null::<u8>(), 0usize, 1u32) };
    let libcs = if libcs < 0 { Err(libc_errno()) } else { Ok(libcs as u32) };
    assert_eq!(landlock_abi(), libcs);
    let Ok(abi) = libcs else {
        println!("Landlock is not on here: {libcs:?} (the sandbox stops, naming it)");
        return;
    };
    assert!(abi >= 1);
    let handled = AccessFs::READ_FILE | AccessFs::READ_DIR | AccessFs::WRITE_FILE;
    let ruleset =
        landlock_create_ruleset(&RulesetAttr { handled_access_fs: handled.bits(), handled_access_net: 0, scoped: 0 })
            .unwrap();
    let scratch = Scratch::new("landlock");
    let rule = PathBeneath { allowed_access: (AccessFs::READ_FILE | AccessFs::READ_DIR).bits(), parent_fd: scratch.fd().raw() };
    assert_eq!(landlock_add_rule_path(ruleset.fd(), &rule), Ok(()));
    let unhandled = PathBeneath { allowed_access: AccessFs::EXECUTE.bits(), parent_fd: scratch.fd().raw() };
    assert_eq!(landlock_add_rule_path(ruleset.fd(), &unhandled), Err(Errno::Inval), "a right the ruleset does not handle");
}

// The slots' conformance (D185, rule 78): every form this kernel has, run on the same input,
// gives the same answer. A form the kernel lacks is skipped and named.

/// The facts every conformance test chooses from, printed once per test.
fn kernel_facts() -> super::probe::KernelFacts {
    let facts = super::probe::KernelFacts::probe().unwrap();
    println!("{facts}");
    facts
}

/// A file `name` in `scratch` holding `bytes`, opened for reading and writing.
fn scratch_file(scratch: &Scratch, name: &str, bytes: &[u8]) -> OwnedFd {
    std::fs::write(scratch.path.join(name), bytes).unwrap();
    let c = CString::new(name).unwrap();
    openat(scratch.fd(), Path::new(&c), OpenFlags::READ_WRITE | OpenFlags::CLOSE_ON_EXEC, 0).unwrap()
}

/// Every form of the several-words wait: a changed word is `Again`, a past deadline
/// `TimedOut`, and a store and wake from another thread ends the wait.
#[test]
fn every_wait_many_form_answers_alike() {
    use super::slot::{WaitMany, Watched};
    let facts = kernel_facts();
    for form in WaitMany::ALL {
        if !form.available(&facts) {
            println!("{form:?}: not on this kernel");
            continue;
        }
        let words = Arc::new([AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)]);
        let watched = [Watched { word: &words[0], expected: 0 }, Watched { word: &words[1], expected: 0 }];
        assert_eq!(form.wait(&[], &words[2], None), Err(Errno::Inval), "{form:?}: no words");
        assert_eq!(form.wait(&watched, &words[2], Some(Timespec::ZERO)), Err(Errno::TimedOut), "{form:?}: past deadline");
        words[1].store(1, Ordering::Release);
        assert_eq!(form.wait(&watched, &words[2], None), Err(Errno::Again), "{form:?}: changed word");
        words[1].store(0, Ordering::Release);
        let waker = Arc::clone(&words);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            waker[0].store(1, Ordering::Release);
            form.wake(&waker[0], &waker[2]).unwrap();
        });
        let watched = [Watched { word: &words[0], expected: 0 }, Watched { word: &words[1], expected: 0 }];
        // Woken, or the store landed first: either way the wait ends, with no deadline needed
        // beyond the test's own.
        let ended = form.wait(&watched, &words[2], None);
        assert!(matches!(ended, Ok(()) | Err(Errno::Again)), "{form:?}: {ended:?}");
        thread.join().unwrap();
    }
}

/// Every form of closing a range closes the descriptors at and above the first, and leaves
/// those below.
#[test]
fn every_close_range_form_answers_alike() {
    use super::slot::CloseRange;
    let facts = kernel_facts();
    let mut buffer = [0u8; 4096];
    for form in CloseRange::ALL {
        if !form.available(&facts) {
            println!("{form:?}: not on this kernel");
            continue;
        }
        // SAFETY: plain descriptor calls on a descriptor of this test.
        let base = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        assert!(base >= 0);
        // Numbers this test owns, under the common soft limit of 1024.
        // SAFETY: as above.
        let (kept, above, higher) = unsafe {
            (
                libc::fcntl(base, libc::F_DUPFD_CLOEXEC, 990),
                libc::fcntl(base, libc::F_DUPFD_CLOEXEC, 1000),
                libc::fcntl(base, libc::F_DUPFD_CLOEXEC, 1010),
            )
        };
        assert!(kept >= 990 && kept < 1000 && above >= 1000 && higher > above, "{kept} {above} {higher}");
        form.close_from(above as u32, &mut buffer).unwrap();
        // SAFETY: `F_GETFD` reads a descriptor's flags and touches no memory.
        let open = |fd: i32| unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0;
        assert!(open(kept) && open(base), "{form:?}: closed one below the range");
        assert!(!open(above) && !open(higher), "{form:?}: left one in the range");
        // SAFETY: the test's own descriptors, closed once.
        unsafe {
            libc::close(kept);
            libc::close(base);
        }
    }
}

/// Every form of starting a process makes a child with a pidfd that waits for its exit.
#[test]
fn every_start_process_form_answers_alike() {
    use super::slot::{StartProcess, Started};
    let facts = kernel_facts();
    for form in StartProcess::ALL {
        if !form.available(&facts) {
            println!("{form:?}: not on this kernel");
            continue;
        }
        // SAFETY: the child calls only `exit_group`, which is async-signal-safe.
        let (pid, pidfd) = match unsafe { form.fork() }.unwrap() {
            Started::Child => exit_group(7),
            Started::Parent { pid, pidfd } => (pid, pidfd),
        };
        let reaped = waitid(Children::Pidfd(pidfd.fd()), WaitOptions::EXITED);
        assert_eq!(reaped, Ok(Some(Waited { pid, ended: Ended::Exited(7) })), "{form:?}");
    }
}

/// Every form of a file's facts gives the same facts, for a file, a directory and a link.
#[test]
fn every_file_facts_form_answers_alike() {
    use super::slot::FileFacts;
    let facts = kernel_facts();
    let scratch = Scratch::new("slot-stat");
    drop(scratch_file(&scratch, "f", b"some bytes"));
    std::os::unix::fs::symlink("f", scratch.path.join("l")).unwrap();
    for (path, flags) in [(c"f", AtFlags::NONE), (c".", AtFlags::NONE), (c"l", AtFlags::NO_FOLLOW), (c"none", AtFlags::NONE)] {
        let answers = FileFacts::ALL.map(|form| form.available(&facts).then(|| form.stat(scratch.fd(), Path::new(path), flags)));
        let mut present = answers.iter().flatten();
        let first = present.next().unwrap();
        for other in present {
            assert_eq!(other, first, "{path:?}");
        }
    }
}

/// Every form of opening beneath a directory answers the contract alike: a file and a
/// directory open, a link anywhere is `Loop`, `..` and an absolute path `XDev`.
#[test]
fn every_open_beneath_form_answers_alike() {
    use super::slot::{FileFacts, OpenBeneath};
    let facts = kernel_facts();
    let scratch = Scratch::new("slot-beneath");
    std::fs::create_dir(scratch.path.join("d")).unwrap();
    std::fs::write(scratch.path.join("d/f"), b"x").unwrap();
    std::os::unix::fs::symlink("d", scratch.path.join("l")).unwrap();
    std::os::unix::fs::symlink("f", scratch.path.join("d/lf")).unwrap();
    let cases: [&[u8]; 12] =
        [b"d/f", b"d", b"d/", b"./d//f", b"l/f", b"d/lf", b"l", b"d/../d/f", b"/etc", b"", b"d/f/", b"none"];
    for path in cases {
        let mut answers = [None, None];
        for (form, answer) in OpenBeneath::ALL.into_iter().zip(&mut answers) {
            if !form.available(&facts) {
                continue;
            }
            let mut buffer = [0u8; 256];
            *answer = Some(form.open(scratch.fd(), path, OpenFlags::READ, 0, &mut buffer).map(|opened| {
                FileFacts::Fstatat.stat(opened.fd(), Path::EMPTY, AtFlags::EMPTY_PATH).unwrap().ino
            }));
        }
        let shown = String::from_utf8_lossy(path);
        if let [Some(one), Some(two)] = &answers {
            assert_eq!(one, two, "{shown}");
        }
        let any = answers.iter().flatten().next().unwrap();
        match path {
            b"l/f" | b"d/lf" | b"l" => assert_eq!(*any, Err(Errno::Loop), "{shown}"),
            b"d/../d/f" | b"/etc" => assert_eq!(*any, Err(Errno::XDev), "{shown}"),
            b"" | b"none" => assert_eq!(*any, Err(Errno::NoEnt), "{shown}"),
            b"d/f/" => assert_eq!(*any, Err(Errno::NotDir), "{shown}"),
            _ => assert!(any.is_ok(), "{shown}: {any:?}"),
        }
    }
}

/// Every form of signalling reaches the child: 0 finds it, `SIGKILL` ends it.
#[test]
fn every_signal_form_answers_alike() {
    use super::slot::Signal;
    let facts = kernel_facts();
    for form in Signal::ALL {
        if !form.available(&facts) {
            println!("{form:?}: not on this kernel");
            continue;
        }
        let args = CloneArgs { exit_signal: libc::SIGCHLD as u64, ..CloneArgs::default() };
        // SAFETY: the child only sleeps by a raw call and exits.
        let pid = match unsafe { clone3_process(&args) }.unwrap() {
            Cloned::Child => {
                _ = clock_nanosleep_until(ClockId::Monotonic, Timespec { sec: i64::MAX / 2, nsec: 0 });
                exit_group(1)
            }
            Cloned::Parent(pid) => pid,
        };
        let pidfd = pidfd_open(pid).unwrap();
        assert_eq!(form.send(pid, pidfd.fd(), 0), Ok(()), "{form:?}: is there");
        assert_eq!(form.send(pid, pidfd.fd(), libc::SIGKILL as u32), Ok(()), "{form:?}: kill");
        let reaped = waitid(Children::Pidfd(pidfd.fd()), WaitOptions::EXITED);
        assert_eq!(reaped, Ok(Some(Waited { pid, ended: Ended::Signaled(libc::SIGKILL) })), "{form:?}");
    }
}

/// Every form of copying copies the same bytes, the copy looped to the end.
#[test]
fn every_copy_form_answers_alike() {
    use super::slot::CopyRange;
    let facts = kernel_facts();
    let scratch = Scratch::new("slot-copy");
    let bytes: Vec<u8> = (0..200_000u32).map(|at| (at % 251) as u8).collect();
    let from = scratch_file(&scratch, "from", &bytes);
    for form in CopyRange::ALL {
        if !form.available(&facts) {
            println!("{form:?}: not on this kernel");
            continue;
        }
        let name = format!("to-{form:?}");
        let to = scratch_file(&scratch, &name, b"");
        let mut buffer = [0u8; 8192];
        let mut done = 0u64;
        loop {
            let left = bytes.len() as u64 - done;
            let step = form.step(from.fd(), done, to.fd(), done, left, &mut buffer).unwrap();
            if step == 0 {
                break;
            }
            done += step as u64;
        }
        assert_eq!(std::fs::read(scratch.path.join(&name)).unwrap(), bytes, "{form:?}");
    }
    assert_eq!(CopyRange::choose(&facts, false), Ok(CopyRange::Buffered), "across file systems");
}

/// Every form of counting resident pages counts the same pages of a file just written.
#[test]
fn every_resident_pages_form_answers_alike() {
    use super::slot::ResidentPages;
    use core::num::NonZeroUsize;
    let facts = kernel_facts();
    let scratch = Scratch::new("slot-resident");
    // SAFETY: `sysconf` reads a number.
    let page = NonZeroUsize::new(unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize).unwrap();
    let file = scratch_file(&scratch, "f", &vec![7u8; page.get() * 3]);
    let len = NonZeroUsize::new(page.get() * 3).unwrap();
    let mut held = [0u8; 3];
    let counts = ResidentPages::ALL
        .map(|form| form.available(&facts).then(|| form.count(file.fd(), 0, len, page, &mut held)));
    let mut present = counts.iter().flatten();
    let first = present.next().unwrap();
    assert_eq!(*first, Ok(3), "a file just written is in the cache");
    for other in present {
        assert_eq!(other, first);
    }
    let odd = ResidentPages::Mincore.count(file.fd(), 1, len, page, &mut held);
    assert_eq!(odd, Err(Errno::Inval), "an offset off a page");
}

/// The one-form slots: secret memory, sealing and the atomic write make what they say where
/// the kernel (or the file) has the fact, and are refused naming it where not.
#[test]
fn the_one_form_slots_answer_or_refuse_by_name() {
    use super::probe::Fact;
    use super::slot::{AtomicWrite, Refused, Seal, SecretMemory, Slot};
    let facts = kernel_facts();
    match SecretMemory::choose(&facts) {
        Ok(form) => drop(form.create().unwrap()),
        Err(refused) => assert_eq!(refused, Refused { slot: Slot::SecretMemory, fact: Fact::MemfdSecret }),
    }
    match Seal::choose(&facts) {
        Ok(form) => {
            // SAFETY: a new private anonymous page this test never unmaps but to see the seal.
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            // SAFETY: as above.
            let at = unsafe {
                mmap(None, page, Prot::READ, MapFlags::PRIVATE | MapFlags::ANONYMOUS, None, 0)
            }
            .unwrap();
            // SAFETY: the page is this test's, and kept for the life of the process.
            assert_eq!(unsafe { form.seal(at, page) }, Ok(()));
            // SAFETY: a sealed mapping refuses the unmap and stays.
            assert_eq!(unsafe { munmap(at, page) }, Err(Errno::Perm), "sealed");
        }
        Err(refused) => assert_eq!(refused, Refused { slot: Slot::Seal, fact: Fact::Mseal }),
    }
    assert_eq!(AtomicWrite::choose(None), Err(Refused { slot: Slot::AtomicWrite, fact: Fact::AtomicWrite }));
    let scratch = Scratch::new("slot-atomic");
    let file = scratch_file(&scratch, "f", b"");
    match AtomicWrite::choose(atomic_write_bounds(file.fd()).unwrap()) {
        Ok(slot) => {
            // Linux requires O_DIRECT and an aligned buffer for RWF_ATOMIC (`kiocb_set_rw_flags`
            // refuses buffered descriptors with EOPNOTSUPP).
            let direct = openat(
                scratch.fd(),
                Path::new(c"f"),
                OpenFlags::READ_WRITE | OpenFlags::CLOSE_ON_EXEC | OpenFlags::DIRECT,
                0,
            );
            // SAFETY: querying the host page size; no memory touched.
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let len = slot.min() as usize;
            let map_len = len.next_multiple_of(page);
            // SAFETY: a private anonymous page-aligned buffer, filled and unmapped below.
            let buf_ptr = unsafe {
                mmap(None, map_len, Prot::READ | Prot::WRITE, MapFlags::PRIVATE | MapFlags::ANONYMOUS, None, 0)
            }
            .unwrap();
            // SAFETY: `buf_ptr` points to `map_len >= len` freshly mapped writable bytes.
            let unit = unsafe { core::slice::from_raw_parts_mut(buf_ptr.as_ptr(), len) };
            unit.fill(1u8);
            match direct {
                Ok(direct_fd) => match slot.write(direct_fd.fd(), unit, 0) {
                    Ok(n) => assert_eq!(n, unit.len()),
                    Err(Errno::OpNotSupp) => println!("atomic write: refused EOPNOTSUPP on this filesystem"),
                    other => panic!("atomic write on O_DIRECT fd: {other:?}"),
                },
                Err(Errno::Inval | Errno::OpNotSupp) => println!("atomic write: O_DIRECT refused on this filesystem"),
                Err(errno) => panic!("opening O_DIRECT fd: {errno:?}"),
            }
            assert_eq!(slot.write(file.fd(), unit, 1), Err(Errno::Inval), "unaligned");
            // SAFETY: unmaps the private anonymous buffer mapped above.
            unsafe { munmap(buf_ptr, map_len) }.unwrap();
        }
        Err(refused) => println!("{refused}"),
    }
}

// ---------------------------------------------------------------------------------------------
// The wait slot, the ring futex fact and denied facts (D185).

/// Runs `body` in a fresh `clone3` child (one thread, its own copy of memory: no private
/// futex hash yet) and answers its exit status, waited for under a minute's bound.
fn in_fresh_child(body: fn() -> i32) -> i32 {
    let args = CloneArgs { exit_signal: libc::SIGCHLD as u64, ..CloneArgs::default() };
    // SAFETY: the child makes raw calls only and exits.
    let pid = match unsafe { clone3_process(&args) }.unwrap() {
        Cloned::Child => exit_group(body()),
        Cloned::Parent(pid) => pid,
    };
    let pidfd = pidfd_open(pid).unwrap();
    let mut fds = [PollFd::new(pidfd.fd(), PollEvents::IN)];
    assert_eq!(ppoll(&mut fds, Some(Timespec::from_duration(Duration::from_secs(60))), None), Ok(1), "the child ended");
    match waitid(Children::Pid(pid), WaitOptions::EXITED | WaitOptions::NO_HANG).unwrap() {
        Some(Waited { ended: Ended::Exited(status), .. }) => status,
        other => panic!("the child ended as {other:?}"),
    }
}

/// Each form of the wait slot this kernel has parks on a word and is woken by another thread's
/// store and wake, within a deadline: the direct form by `futex_wait_bitset`, the ring form by
/// a ring `FUTEX_WAIT` on a ring with the wait ring's flags (blaze-core's ring itself is not
/// reachable from here: its park is tested there).
#[test]
fn every_wait_form_parks_and_wakes_a_word() {
    use super::probe::{Completion, ProbeRing, Sqe};
    use super::slot::{Wait, WaitForm};
    let facts = kernel_facts();
    for form in Wait::ALL {
        let wait = match Wait::start(form, &facts) {
            Ok(wait) => wait,
            Err(refused) => {
                println!("{form:?}: {refused}");
                continue;
            }
        };
        assert_eq!(wait.form(), form);
        let word = Arc::new(AtomicU32::new(0));
        let waker = {
            let word = Arc::clone(&word);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                word.store(1, Ordering::Release);
                futex_wake(&word, 1).unwrap()
            })
        };
        let deadline = monotonic() + Duration::from_secs(60);
        let mut passes = 0u32;
        match form {
            WaitForm::Direct => {
                while word.load(Ordering::Acquire) == 0 {
                    passes += 1;
                    assert!(passes <= 64, "the direct wait woke 64 times with the word unchanged");
                    match futex_wait_bitset(&word, 0, Some(Timespec::from_duration(deadline)), FUTEX_ANY) {
                        Ok(()) | Err(Errno::Again | Errno::Intr) => {}
                        Err(errno) => panic!("the direct wait answered {errno}"),
                    }
                    assert!(monotonic() < deadline, "the direct wait passed its deadline");
                }
            }
            WaitForm::Ring => {
                // Made after `word`: closed before it goes.
                let mut ring = ProbeRing::new(4).unwrap();
                while word.load(Ordering::Acquire) == 0 {
                    passes += 1;
                    assert!(passes <= 64, "the ring wait woke 64 times with the word unchanged");
                    assert!(ring.push(Sqe::futex_wait(&word, 0, u64::from(passes))));
                    // SAFETY: the request names `word`, which outlives the ring.
                    match unsafe { ring.enter(1, 1, Duration::from_secs(5)) } {
                        Ok(_) | Err(Errno::Busy | Errno::Intr) => {}
                        Err(errno) => panic!("the ring wait answered {errno}"),
                    }
                    let mut room = [Completion::NONE; 4];
                    let got = ring.completions(&mut room);
                    assert!(got <= 1, "one request, {got} completions");
                    if let Some(done) = room.get(..got).and_then(<[Completion]>::first) {
                        // Woken (0), or the word had changed before the wait was queued.
                        assert!(done.res == 0 || done.errno() == Errno::Again, "the ring wait ended with {}", done.res);
                    } else {
                        panic!("the ring wait took 5 s with no completion and the word {}", word.load(Ordering::Acquire));
                    }
                    assert!(monotonic() < deadline, "the ring wait passed its deadline");
                }
                ring.close().unwrap();
            }
        }
        waker.join().unwrap();
        assert_eq!(word.load(Ordering::Acquire), 1, "{form:?}");
    }
}

/// The ring futex probe, run where it means something (a fresh child: no private futex hash
/// yet), answers one of its four facts and ends: never a `ProbeError`, never a hang (the
/// child is waited for under a minute; the probe's own bound is 100 ms). The fact agrees with
/// this process's on whether the ring futex is usable at all.
#[test]
fn the_ring_futex_probe_classifies_in_a_fresh_child() {
    use super::probe::{KernelFacts, RingFutex};
    fn probe_in_child() -> i32 {
        match KernelFacts::probe() {
            Ok(facts) => match facts.ring_futex() {
                RingFutex::Absent => 10,
                RingFutex::Denied => 11,
                RingFutex::Sound => 12,
                RingFutex::LostWake => 13,
            },
            Err(_) => 2,
        }
    }
    let facts = kernel_facts();
    let status = in_fresh_child(probe_in_child);
    let fact = match status {
        10 => RingFutex::Absent,
        11 => RingFutex::Denied,
        12 => RingFutex::Sound,
        13 => RingFutex::LostWake,
        2 => panic!("the ring futex probe answered a ProbeError in a fresh child"),
        other => panic!("the child ended with {other}"),
    };
    println!("ring futex in a fresh child: {}", fact.name());
    let usable = |it: RingFutex| matches!(it, RingFutex::Sound | RingFutex::LostWake);
    assert_eq!(usable(fact), usable(facts.ring_futex()), "child {fact:?}, this process {:?}", facts.ring_futex());
}

/// The classic BPF program of a filter that answers `pidfd_open` with `EPERM` and
/// `io_uring_setup` with `EACCES`, and allows every other call: a seccomp profile denying both.
fn deny_pidfd_and_ring() -> [SockFilter; 6] {
    const LOAD_NR: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS, of `seccomp_data.nr` at 0
    const JUMP_EQ: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const RETURN: u16 = 0x06; // BPF_RET | BPF_K
    const ALLOW: u32 = 0x7fff_0000; // SECCOMP_RET_ALLOW
    const ERRNO: u32 = 0x0005_0000; // SECCOMP_RET_ERRNO
    [
        SockFilter { code: LOAD_NR, jt: 0, jf: 0, k: 0 },
        SockFilter { code: JUMP_EQ, jt: 0, jf: 1, k: nr::PIDFD_OPEN as u32 },
        SockFilter { code: RETURN, jt: 0, jf: 0, k: ERRNO | u32::from(Errno::Perm.raw()) },
        SockFilter { code: JUMP_EQ, jt: 0, jf: 1, k: nr::IO_URING_SETUP as u32 },
        SockFilter { code: RETURN, jt: 0, jf: 0, k: ERRNO | u32::from(Errno::Acces.raw()) },
        SockFilter { code: RETURN, jt: 0, jf: 0, k: ALLOW },
    ]
}

/// Under a filter denying `pidfd_open` (`EPERM`) and `io_uring_setup` (`EACCES`), the probe
/// answers (no `ProbeError`) and reads each `Denied`: the ring, its wait flags, region and
/// futex fact, and `pidfd_open`; the wait slot then has no form and its refusal says denied.
/// In a child, so the test binary keeps its calls. The control: unfiltered, `pidfd_open` is
/// not denied.
#[test]
fn a_denied_probe_reads_denied() {
    use super::probe::{Fact, KernelFacts, Presence, RingFutex, RingState};
    use super::slot::{Refused, Slot, Wait, WaitStartError};
    fn probe_denied() -> i32 {
        let installed = prctl(Prctl::NoNewPrivs).and_then(|()| seccomp_filter(&deny_pidfd_and_ring(), SeccompFlags::NONE));
        if installed.is_err() {
            return 250;
        }
        let Ok(facts) = KernelFacts::probe() else {
            return 2;
        };
        let refused = WaitStartError::Refused {
            refused: Refused { slot: Slot::Wait, fact: Fact::PidfdOpen },
            presence: Presence::Denied,
        };
        let checks = [
            facts.presence(Fact::PidfdOpen) == Presence::Denied && !facts.pidfd_open(),
            facts.ring().state() == RingState::Denied(Errno::Acces),
            facts.presence(Fact::RingWaitFlags) == Presence::Denied
                && facts.presence(Fact::RingMemRegion) == Presence::Denied,
            facts.ring_futex() == RingFutex::Denied,
            Wait::choose(&facts) == Err(refused),
        ];
        match checks.iter().position(|passed| !passed) {
            Some(at) => 10 + at as i32,
            None => 0,
        }
    }
    match in_fresh_child(probe_denied) {
        0 => {}
        250 => panic!("the filter could not be installed"),
        2 => panic!("a denied probe answered a ProbeError"),
        failed => panic!("denied check {} failed (0: pidfd_open, 1: ring, 2: wait flags and region, 3: ring futex, 4: wait slot)", failed - 10),
    }
    let facts = kernel_facts();
    assert_ne!(facts.presence(Fact::PidfdOpen), Presence::Denied, "the control: unfiltered");
}
