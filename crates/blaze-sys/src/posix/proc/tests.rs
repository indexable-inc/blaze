//! The `libc` crate as the oracle for this module: every `#[repr(C)]` type's size, alignment
//! and read offsets, every constant's value and every error number, the declarations' types
//! by fn-pointer coercion, and the status macros on every low word. Deleted with the `libc`
//! dev-dependency (D180).

#[cfg(target_os = "macos")]
use core::ffi::c_char;
use core::ffi::c_int;
use core::mem::offset_of;

use super::*;
use crate::Errno;

/// Size and alignment of ours against libc's.
macro_rules! same_shape {
    ($ours:ty, $theirs:ty) => {
        assert_eq!(size_of::<$ours>(), size_of::<$theirs>(), concat!("size of ", stringify!($ours)));
        assert_eq!(align_of::<$ours>(), align_of::<$theirs>(), concat!("align of ", stringify!($ours)));
    };
}

/// Each named field at libc's offset.
macro_rules! same_offsets {
    ($ours:ty, $theirs:ty, $($field:ident),*) => {
        $(assert_eq!(
            offset_of!($ours, $field),
            offset_of!($theirs, $field),
            concat!(stringify!($ours), ".", stringify!($field)),
        );)*
    };
}

/// Each constant equal to libc's, compared as `i64`.
macro_rules! same_values {
    ($($ours:expr => $theirs:expr),* $(,)?) => {
        $(assert_eq!($ours as i64, $theirs as i64, stringify!($ours));)*
    };
}

#[test]
fn the_layouts_are_libcs() {
    same_shape!(stat, libc::stat);
    same_offsets!(stat, libc::stat, st_dev, st_ino, st_nlink, st_mode, st_uid, st_gid, st_rdev, st_size, st_blksize, st_blocks, st_atime, st_atime_nsec, st_mtime, st_mtime_nsec, st_ctime, st_ctime_nsec);
    #[cfg(target_os = "linux")]
    {
        same_shape!(statfs, libc::statfs);
        same_shape!(Elf64_Phdr, libc::Elf64_Phdr);
        same_offsets!(Elf64_Phdr, libc::Elf64_Phdr, p_type, p_flags, p_offset, p_vaddr, p_paddr, p_filesz, p_memsz, p_align);
        same_shape!(Sysinfo, libc::sysinfo);
        same_offsets!(Sysinfo, libc::sysinfo, uptime, loads, totalram, freeram, sharedram, bufferram, totalswap, freeswap, procs, totalhigh, freehigh, mem_unit);
        same_shape!(RemoteSlice, libc::iovec);
        assert_eq!(offset_of!(RemoteSlice, base), offset_of!(libc::iovec, iov_base));
        assert_eq!(offset_of!(RemoteSlice, len), offset_of!(libc::iovec, iov_len));
    }
    #[cfg(target_os = "macos")]
    {
        same_offsets!(stat, libc::stat, st_birthtime, st_flags, st_gen);
        same_shape!(statfs, libc::statfs);
        same_offsets!(statfs, libc::statfs, f_bsize, f_iosize, f_blocks, f_bfree, f_bavail, f_files, f_ffree, f_fsid, f_owner, f_type, f_flags, f_fssubtype, f_fstypename, f_mntonname, f_mntfromname, f_flags_ext);
        same_shape!(attrlist, libc::attrlist);
        same_offsets!(attrlist, libc::attrlist, bitmapcount, reserved, commonattr, volattr, dirattr, fileattr, forkattr);
        same_shape!(fstore_t, libc::fstore_t);
        same_offsets!(fstore_t, libc::fstore_t, fst_flags, fst_posmode, fst_offset, fst_length, fst_bytesalloc);
        // libc's `radvisory` is unpacked (16 bytes); the header packs it to 4 (12 bytes), and
        // the kernel reads the two fields at these offsets either way.
        same_offsets!(radvisory, libc::radvisory, ra_offset, ra_count);
        assert_eq!(size_of::<posix_spawnattr_t>(), size_of::<libc::posix_spawnattr_t>());
        assert_eq!(size_of::<posix_spawn_file_actions_t>(), size_of::<libc::posix_spawn_file_actions_t>());
    }
}

#[test]
fn the_constants_are_libcs() {
    same_values! {
        WNOHANG => libc::WNOHANG, WEXITED => libc::WEXITED, WNOWAIT => libc::WNOWAIT,
        P_PID => libc::P_PID,
        S_IFMT => libc::S_IFMT, S_IFDIR => libc::S_IFDIR, S_IFREG => libc::S_IFREG,
        S_IFLNK => libc::S_IFLNK, S_IFIFO => libc::S_IFIFO, S_IFSOCK => libc::S_IFSOCK,
        S_IFBLK => libc::S_IFBLK, S_IFCHR => libc::S_IFCHR,
        DT_DIR => libc::DT_DIR, DT_UNKNOWN => libc::DT_UNKNOWN,
        STDOUT_FILENO => libc::STDOUT_FILENO, PATH_MAX => libc::PATH_MAX,
        PRIO_PROCESS => libc::PRIO_PROCESS,
    }
    #[cfg(target_os = "linux")]
    same_values! {
        CLONE_VM => libc::CLONE_VM, CLONE_VFORK => libc::CLONE_VFORK,
        CLONE_PIDFD => libc::CLONE_PIDFD, CLONE_NEWUSER => libc::CLONE_NEWUSER,
        CLONE_NEWNET => libc::CLONE_NEWNET,
        POSIX_FADV_WILLNEED => libc::POSIX_FADV_WILLNEED, MNT_DETACH => libc::MNT_DETACH,
        PT_LOAD => libc::PT_LOAD, PF_R => libc::PF_R, PF_X => libc::PF_X,
        Namespaces::USER.bits() => libc::CLONE_NEWUSER, Namespaces::NET.bits() => libc::CLONE_NEWNET,
        FileAdvice::Normal.raw() => libc::POSIX_FADV_NORMAL,
        FileAdvice::Random.raw() => libc::POSIX_FADV_RANDOM,
        FileAdvice::Sequential.raw() => libc::POSIX_FADV_SEQUENTIAL,
        FileAdvice::WillNeed.raw() => libc::POSIX_FADV_WILLNEED,
        FileAdvice::DontNeed.raw() => libc::POSIX_FADV_DONTNEED,
        FileAdvice::NoReuse.raw() => libc::POSIX_FADV_NOREUSE,
        MountFlags::READ_ONLY.bits() => libc::MS_RDONLY, MountFlags::NO_SUID.bits() => libc::MS_NOSUID,
        MountFlags::NO_DEV.bits() => libc::MS_NODEV, MountFlags::NO_EXEC.bits() => libc::MS_NOEXEC,
        MountFlags::REMOUNT.bits() => libc::MS_REMOUNT, MountFlags::BIND.bits() => libc::MS_BIND,
        MountFlags::RECURSIVE.bits() => libc::MS_REC, MountFlags::PRIVATE.bits() => libc::MS_PRIVATE,
        UnmountFlags::FORCE.bits() => libc::MNT_FORCE, UnmountFlags::DETACH.bits() => libc::MNT_DETACH,
        UnmountFlags::EXPIRE.bits() => libc::MNT_EXPIRE, UnmountFlags::NO_FOLLOW.bits() => libc::UMOUNT_NOFOLLOW,
        XattrFlags::CREATE.bits() => libc::XATTR_CREATE, XattrFlags::REPLACE.bits() => libc::XATTR_REPLACE,
        AT_NULL => libc::AT_NULL,
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    same_values! {
        HWCAP_ASIMD => libc::HWCAP_ASIMD, HWCAP_AES => libc::HWCAP_AES, HWCAP_PMULL => libc::HWCAP_PMULL,
        HWCAP_SHA1 => libc::HWCAP_SHA1, HWCAP_SHA2 => libc::HWCAP_SHA2, HWCAP_CRC32 => libc::HWCAP_CRC32,
        HWCAP_ATOMICS => libc::HWCAP_ATOMICS, HWCAP_SHA3 => libc::HWCAP_SHA3,
        HWCAP_SHA512 => libc::HWCAP_SHA512, HWCAP_SVE => libc::HWCAP_SVE,
    }
    #[cfg(target_os = "macos")]
    same_values! {
        POSIX_SPAWN_SETSIGDEF => libc::POSIX_SPAWN_SETSIGDEF,
        POSIX_SPAWN_SETSIGMASK => libc::POSIX_SPAWN_SETSIGMASK,
        MNT_LOCAL => libc::MNT_LOCAL, MNT_IGNORE_OWNERSHIP => libc::MNT_IGNORE_OWNERSHIP,
        ATTR_BIT_MAP_COUNT => libc::ATTR_BIT_MAP_COUNT, ATTR_CMN_NAME => libc::ATTR_CMN_NAME,
        ATTR_CMN_DEVID => libc::ATTR_CMN_DEVID, ATTR_CMN_OBJTYPE => libc::ATTR_CMN_OBJTYPE,
        ATTR_CMN_MODTIME => libc::ATTR_CMN_MODTIME, ATTR_CMN_CHGTIME => libc::ATTR_CMN_CHGTIME,
        ATTR_CMN_ACCESSMASK => libc::ATTR_CMN_ACCESSMASK, ATTR_CMN_FILEID => libc::ATTR_CMN_FILEID,
        ATTR_CMN_RETURNED_ATTRS => libc::ATTR_CMN_RETURNED_ATTRS,
        ATTR_DIR_DATALENGTH => libc::ATTR_DIR_DATALENGTH,
        ATTR_FILE_DATALENGTH => libc::ATTR_FILE_DATALENGTH,
        THREAD_BASIC_INFO => libc::THREAD_BASIC_INFO,
        THREAD_BASIC_INFO_COUNT => libc::THREAD_BASIC_INFO_COUNT,
        TH_STATE_RUNNING => libc::TH_STATE_RUNNING,
        MACH_TASK_BASIC_INFO => libc::MACH_TASK_BASIC_INFO,
        MACH_TASK_BASIC_INFO_COUNT => libc::MACH_TASK_BASIC_INFO_COUNT,
        PROC_PIDLISTFDS => libc::PROC_PIDLISTFDS, PROC_PIDTASKINFO => libc::PROC_PIDTASKINFO,
        PROX_FDTYPE_VNODE => libc::PROX_FDTYPE_VNODE, PROX_FDTYPE_SOCKET => libc::PROX_FDTYPE_SOCKET,
        VM_FLAGS_SUPERPAGE_SIZE_2MB => libc::VM_FLAGS_SUPERPAGE_SIZE_2MB,
        QOS_CLASS_USER_INTERACTIVE => libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE,
        QOS_CLASS_USER_INITIATED => libc::qos_class_t::QOS_CLASS_USER_INITIATED,
        QOS_CLASS_DEFAULT => libc::qos_class_t::QOS_CLASS_DEFAULT,
        QOS_CLASS_UTILITY => libc::qos_class_t::QOS_CLASS_UTILITY,
        QOS_CLASS_BACKGROUND => libc::qos_class_t::QOS_CLASS_BACKGROUND,
        QOS_CLASS_UNSPECIFIED => libc::qos_class_t::QOS_CLASS_UNSPECIFIED,
        XATTR_CREATE => libc::XATTR_CREATE, XATTR_REPLACE => libc::XATTR_REPLACE,
    }
}

/// Every `libc::E*` this module's doc maps, at libc's number on this target.
#[test]
fn the_error_numbers_are_libcs() {
    let pairs: &[(Errno, c_int)] = &[
        (Errno::Perm, libc::EPERM), (Errno::NoEnt, libc::ENOENT), (Errno::Srch, libc::ESRCH),
        (Errno::Intr, libc::EINTR), (Errno::Io, libc::EIO), (Errno::Nxio, libc::ENXIO),
        (Errno::BadF, libc::EBADF), (Errno::Child, libc::ECHILD), (Errno::Again, libc::EAGAIN),
        (Errno::Again, libc::EWOULDBLOCK), (Errno::NoMem, libc::ENOMEM), (Errno::Acces, libc::EACCES),
        (Errno::Fault, libc::EFAULT), (Errno::Busy, libc::EBUSY), (Errno::Exist, libc::EEXIST),
        (Errno::XDev, libc::EXDEV), (Errno::NoDev, libc::ENODEV), (Errno::NotDir, libc::ENOTDIR),
        (Errno::IsDir, libc::EISDIR), (Errno::Inval, libc::EINVAL), (Errno::NFile, libc::ENFILE),
        (Errno::MFile, libc::EMFILE), (Errno::NoTty, libc::ENOTTY), (Errno::FBig, libc::EFBIG),
        (Errno::NoSpc, libc::ENOSPC), (Errno::Pipe, libc::EPIPE), (Errno::Range, libc::ERANGE),
        (Errno::NameTooLong, libc::ENAMETOOLONG), (Errno::NoSys, libc::ENOSYS),
        (Errno::Loop, libc::ELOOP), (Errno::OpNotSupp, libc::EOPNOTSUPP),
        (Errno::NetDown, libc::ENETDOWN), (Errno::NetUnreach, libc::ENETUNREACH),
        (Errno::ConnAborted, libc::ECONNABORTED), (Errno::ConnReset, libc::ECONNRESET),
        (Errno::NoBufs, libc::ENOBUFS), (Errno::NotConn, libc::ENOTCONN),
        (Errno::TimedOut, libc::ETIMEDOUT), (Errno::HostUnreach, libc::EHOSTUNREACH),
        (Errno::Already, libc::EALREADY), (Errno::InProgress, libc::EINPROGRESS),
        (Errno::Stale, libc::ESTALE), (Errno::Canceled, libc::ECANCELED),
        (Errno::NoData, libc::ENODATA), (Errno::Proto, libc::EPROTO),
        (Errno::NoProtoOpt, libc::ENOPROTOOPT), (Errno::HostDown, libc::EHOSTDOWN),
        // Lane D2's rows.
        (Errno::TooBig, libc::E2BIG), (Errno::NoExec, libc::ENOEXEC), (Errno::TxtBsy, libc::ETXTBSY),
        (Errno::SPipe, libc::ESPIPE), (Errno::RoFs, libc::EROFS), (Errno::MLink, libc::EMLINK),
        (Errno::Deadlk, libc::EDEADLK), (Errno::NoLck, libc::ENOLCK), (Errno::NotEmpty, libc::ENOTEMPTY),
        (Errno::Overflow, libc::EOVERFLOW), (Errno::NotSock, libc::ENOTSOCK), (Errno::MsgSize, libc::EMSGSIZE),
        (Errno::ProtoNoSupport, libc::EPROTONOSUPPORT), (Errno::AfNoSupport, libc::EAFNOSUPPORT),
        (Errno::AddrInUse, libc::EADDRINUSE), (Errno::AddrNotAvail, libc::EADDRNOTAVAIL),
        (Errno::IsConn, libc::EISCONN), (Errno::ConnRefused, libc::ECONNREFUSED), (Errno::DQuot, libc::EDQUOT),
    ];
    for &(ours, theirs) in pairs {
        assert_eq!(ours.raw() as c_int, theirs, "{}", ours.name());
        assert_eq!(Errno::from_raw(theirs as u16), ours, "{}", ours.name());
    }
    #[cfg(target_os = "linux")]
    {
        assert_eq!(Errno::NoNet.raw() as c_int, libc::ENONET);
        // Linux's ENOTSUP is EOPNOTSUPP's number: it reads as `OpNotSupp`.
        assert_eq!(Errno::from_raw(libc::ENOTSUP as u16), Errno::OpNotSupp);
        assert_eq!(crate::linux::ETIME.raw() as c_int, libc::ETIME);
    }
    #[cfg(target_os = "macos")]
    assert_eq!(Errno::NotSup.raw() as c_int, libc::ENOTSUP);
}

/// The status macros agree with libc's on every low 16 bits (the bits they read).
#[test]
fn the_status_macros_are_libcs() {
    for status in 0..=0xffff_i32 {
        assert_eq!(WIFEXITED(status), libc::WIFEXITED(status), "WIFEXITED({status:#x})");
        assert_eq!(WIFSIGNALED(status), libc::WIFSIGNALED(status), "WIFSIGNALED({status:#x})");
        if WIFEXITED(status) {
            assert_eq!(WEXITSTATUS(status), libc::WEXITSTATUS(status), "WEXITSTATUS({status:#x})");
        }
        if WIFSIGNALED(status) {
            assert_eq!(WTERMSIG(status), libc::WTERMSIG(status), "WTERMSIG({status:#x})");
        }
    }
}

/// The device-number macros agree with libc's.
#[test]
fn the_device_numbers_are_libcs() {
    #[cfg(target_os = "linux")]
    for (ma, mi) in [(0u32, 0u32), (8, 1), (259, 3), (0xfff, 0xff), (0x1000, 0x100), (0xffff_ffff, 0xffff_ffff)] {
        let dev = makedev(ma, mi);
        assert_eq!(dev, libc::makedev(ma, mi));
        assert_eq!((major(dev), minor(dev)), (libc::major(dev), libc::minor(dev)));
        assert_eq!((major(dev), minor(dev)), (ma, mi));
    }
    #[cfg(target_os = "macos")]
    for (ma, mi) in [(0i32, 0i32), (1, 4), (16, 0x12_3456), (0x7f, 0xff_ffff)] {
        let dev = makedev(ma, mi);
        assert_eq!(dev, libc::makedev(ma, mi));
        assert_eq!((major(dev), minor(dev)), (libc::major(dev), libc::minor(dev)));
    }
}

/// The declarations whose types are libc's, by coercion to libc's fn-pointer types. macOS:
/// on Linux nothing here is a C declaration (the kernel's calls are compared by what they
/// answer, below).
#[cfg(target_os = "macos")]
#[test]
fn the_signatures_are_libcs() {
    let _: unsafe extern "C" fn(libc::pid_t, *mut c_int, c_int) -> libc::pid_t = waitpid;
    let _: unsafe extern "C" fn() -> libc::pid_t = fork;
    let _: unsafe extern "C" fn(*const c_char, *const *const c_char) -> c_int = execv;
    let _: unsafe extern "C" fn(*const c_char, *const *const c_char, *const *const c_char) -> c_int = execve;
    let _: unsafe extern "C" fn(*const c_char) -> c_int = chdir;
    let _: unsafe extern "C" fn() -> c_int = pause;
    let _: unsafe extern "C" fn(*const c_char, libc::mode_t) -> c_int = mkfifo;
    let _: unsafe extern "C" fn(c_int, libc::uid_t, libc::gid_t) -> c_int = fchown;
    let _: unsafe extern "C" fn(*const c_char, *mut c_char) -> *mut c_char = realpath;
    let _: unsafe extern "C" fn(*mut f64, c_int) -> c_int = getloadavg;
    let _: unsafe extern "C" fn(priority_which_t, libc::id_t) -> c_int = getpriority;
    let _: unsafe extern "C" fn(priority_which_t, libc::id_t, c_int) -> c_int = setpriority;
    let _: unsafe extern "C" fn(*const c_char, *const c_char, u32) -> c_int = clonefile;
    let _: unsafe extern "C" fn(*const c_char, *const c_char, c_int) -> c_int = removexattr;
    let _: unsafe extern "C" fn(*mut posix_spawn_file_actions_t, c_int, c_int) -> c_int = posix_spawn_file_actions_adddup2;
}

/// The kernel's calls answer what glibc's do: the nice value (glibc converts the kernel's
/// `20 - nice` too), the load averages (glibc reads `/proc/loadavg`, two decimals of the same
/// `avenrun`; a little apart, as time passes between the reads), `realpath` of `/proc/self/..`
/// and of `/`, and `statfs` of `/` (its kind and id).
#[cfg(target_os = "linux")]
#[test]
fn the_kernel_calls_answer_as_libcs() {
    // SAFETY: errno cleared, then one call that takes numbers.
    let theirs = unsafe {
        *libc::__errno_location() = 0;
        libc::getpriority(libc::PRIO_PROCESS, 0)
    };
    assert_eq!(getpriority(PriorityOf::Process, 0), Ok(theirs));

    let ours = load_average().expect("sysinfo");
    let mut theirs = [0.0f64; 3];
    // SAFETY: three live doubles.
    assert_eq!(unsafe { libc::getloadavg(theirs.as_mut_ptr(), 3) }, 3);
    for (ours, theirs) in ours.iter().zip(theirs) {
        assert!((ours - theirs).abs() < 0.5, "load {ours} against libc's {theirs}");
    }

    for path in [c"/proc/self/..", c"/", c"/proc/self/fd/.."] {
        let mut room = [0u8; 4096];
        let ours = realpath(crate::Path::new(path), &mut room).expect("realpath");
        let mut resolved = [0 as libc::c_char; 4096];
        // SAFETY: a NUL-terminated path and `PATH_MAX` bytes of room.
        let got = unsafe { libc::realpath(path.as_ptr(), resolved.as_mut_ptr()) };
        assert!(!got.is_null(), "libc's realpath");
        // SAFETY: libc wrote a NUL-terminated string into `resolved`.
        let theirs = unsafe { core::ffi::CStr::from_ptr(resolved.as_ptr()) };
        assert_eq!(ours, theirs.to_bytes(), "{path:?}");
    }
    assert_eq!(
        realpath(crate::Path::new(c"/no/such/blaze/path"), &mut [0u8; 64]),
        Err(RealPathError::Path(Errno::NoEnt)),
    );

    let ours = statfs(crate::Path::new(c"/")).expect("statfs");
    // SAFETY: zero bytes are a valid `libc::statfs`, which the call writes.
    let mut theirs: libc::statfs = unsafe { core::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a live `statfs`.
    assert_eq!(unsafe { libc::statfs(c"/".as_ptr(), &raw mut theirs) }, 0);
    assert_eq!(ours.kind, theirs.f_type as i64);
}

/// `fork` on each system: whether this is the child. Linux's is the kernel's `clone(SIGCHLD)`.
///
/// # Safety
///
/// The child calls only what is async-signal-safe until it exits.
unsafe fn forked() -> Option<libc::pid_t> {
    // SAFETY: the caller's.
    #[cfg(target_os = "linux")]
    return match unsafe { fork() }.expect("fork") {
        crate::linux::Cloned::Child => None,
        crate::linux::Cloned::Parent(pid) => Some(pid),
    };
    // SAFETY: the caller's.
    #[cfg(target_os = "macos")]
    let child = unsafe { fork() };
    #[cfg(target_os = "macos")]
    assert!(child >= 0, "fork");
    #[cfg(target_os = "macos")]
    return (child != 0).then_some(child);
}

/// A child that exits 3 reads back as exited with 3 through [`wait_pid`], and one ended by
/// `SIGKILL` as signaled by 9; a pid with no child is [`Errno::Child`].
#[test]
fn a_child_is_waited_for() {
    // SAFETY: the child calls only `_exit`, which is async-signal-safe.
    let Some(child) = (unsafe { forked() }) else {
        // SAFETY: the child ends here.
        unsafe { libc::_exit(3) };
    };
    let (reaped, status) = wait_pid(child, WaitFlags::NONE).expect("waitpid").expect("a child");
    assert_eq!(reaped, child);
    assert_eq!(status.exit_code(), Some(3));
    assert_eq!(status.signal(), None);

    // SAFETY: the child calls only `pause`, until the signal ends it.
    let Some(child) = (unsafe { forked() }) else {
        loop {
            #[cfg(target_os = "linux")]
            let _ = pause();
            // SAFETY: sleeps until a signal.
            #[cfg(target_os = "macos")]
            unsafe {
                pause()
            };
        }
    };
    // SAFETY: a signal to the child just made.
    assert_eq!(unsafe { libc::kill(child, libc::SIGKILL) }, 0);
    let (_, status) = wait_pid(child, WaitFlags::NONE).expect("waitpid").expect("a child");
    assert_eq!(status.signal(), Some(libc::SIGKILL));

    assert_eq!(wait_pid(child, WaitFlags::NO_HANG), Err(Errno::Child));
}

/// `stat_fd` of `/` equals libc's `fstat` of it in the fields read, and `stat_at(fd, ".")`
/// names the same inode.
#[test]
fn stat_fd_is_libcs_fstat() {
    let dir = c"/";
    // SAFETY: a NUL-terminated path; the descriptor is closed below.
    let fd = unsafe { libc::open(dir.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    assert!(fd >= 0, "open /");
    let ours = stat_fd(crate::Fd::new(fd)).expect("fstat");
    // SAFETY: zero bytes are a valid `libc::stat`, which the call writes.
    let mut theirs: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: an open descriptor and a live `stat`.
    assert_eq!(unsafe { libc::fstat(fd, &raw mut theirs) }, 0);
    assert_eq!((ours.st_dev as i64, ours.st_ino as u64, ours.st_mode as u32), (theirs.st_dev as i64, theirs.st_ino as u64, theirs.st_mode as u32));
    assert_eq!(ours.st_mode & S_IFMT, S_IFDIR);
    let at = stat_at(crate::Fd::new(fd), crate::Path::new(c"."), 0).expect("fstatat");
    assert_eq!(at.st_ino, ours.st_ino);
    // SAFETY: the descriptor opened above, closed once.
    unsafe { libc::close(fd) };
}
