//! The system call numbers blaze uses, per architecture, as data.
//!
//! Linux's system call ABI is stable: a number never changes once released. The values are
//! the kernel's `arch/x86/entry/syscalls/syscall_64.tbl` and `include/uapi/asm-generic/unistd.h`
//! (aarch64). Every row is checked against the `libc` crate's `SYS_*` constant by the test
//! `the_number_table_is_libcs`, so a wrong number fails a test and never a build of user code.
//!
//! What replaces each `libc::SYS_*` the tree names. A number used as data (a seccomp filter's
//! list, a probe) becomes `blaze_sys::linux::nr::<NAME>` below; a `libc::syscall` of it becomes
//! the typed wrapper named, which returns `Result<_, Errno>` (the site's `-1`/errno check
//! becomes the `Err`) and takes `Fd`/`OwnedFd` for descriptors. "no wrapper yet" means the
//! number is here and the typed call is still to be written (lane D1's MISSING list).
//! replaces libc::SYS_futex: nr::FUTEX; replaces libc::syscall(libc::SYS_futex) by linux::futex_wait_bitset (FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG) and linux::futex_wake (FUTEX_WAKE | FUTEX_PRIVATE_FLAG); the shared FUTEX_WAIT (relative time) and FUTEX_WAKE by linux::futex_wait_shared and linux::futex_wake_shared; the shared FUTEX_WAIT_BITSET (absolute time) by linux::futex_wait_bitset_shared
//! replaces libc::SYS_io_uring_register: nr::IO_URING_REGISTER; replaces libc::syscall(libc::SYS_io_uring_register) by linux::io_uring_register (Register: buffers, files, FILES2, FILES_UPDATE2, FILE_ALLOC_RANGE, PBUF_RING, UNREGISTER_PBUF_RING, ENABLE_RINGS, MEM_REGION; other opcodes: no variant yet)
//! replaces libc::SYS_io_uring_setup: nr::IO_URING_SETUP; replaces libc::syscall(libc::SYS_io_uring_setup) by linux::io_uring_setup
//! replaces libc::SYS_io_uring_enter: nr::IO_URING_ENTER; replaces libc::syscall(libc::SYS_io_uring_enter) by linux::io_uring_enter (with IORING_ENTER_EXT_ARG_REG: linux::io_uring_enter_reg_wait)
//! replaces libc::SYS_ioprio_set: nr::IOPRIO_SET; replaces libc::syscall(libc::SYS_ioprio_set) by linux::task::ioprio_set (the calling thread)
//! replaces libc::SYS_ioprio_get: nr::IOPRIO_GET; replaces libc::syscall(libc::SYS_ioprio_get) by linux::task::ioprio_get (the calling thread)
//! replaces libc::SYS_unlinkat: nr::UNLINKAT; replaces libc::syscall(libc::SYS_unlinkat) by linux::unlinkat
//! replaces libc::SYS_renameat2: nr::RENAMEAT2; replaces libc::syscall(libc::SYS_renameat2) by linux::renameat2
//! replaces libc::SYS_renameat: nr::RENAMEAT; replaces libc::syscall(libc::SYS_renameat) by linux::renameat2 with no RenameFlags
//! replaces libc::SYS_pidfd_send_signal: nr::PIDFD_SEND_SIGNAL; replaces libc::syscall(libc::SYS_pidfd_send_signal) by linux::pidfd_send_signal (a form chosen by kernel: linux::slot::Signal)
//! replaces libc::SYS_pidfd_open: nr::PIDFD_OPEN; replaces libc::syscall(libc::SYS_pidfd_open) by linux::pidfd_open
//! replaces libc::SYS_perf_event_open: nr::PERF_EVENT_OPEN; replaces libc::syscall(libc::SYS_perf_event_open) by linux::perf_counters_open (only this thread's instructions+cycles group) and linux::perf_event_open (a general PerfEventAttr, pid, cpu, group)
//! replaces libc::SYS_openat2: nr::OPENAT2; replaces libc::syscall(libc::SYS_openat2) by linux::openat2 (and linux::open_beneath)
//! replaces libc::SYS_openat: nr::OPENAT; replaces libc::syscall(libc::SYS_openat) by linux::openat
//! replaces libc::SYS_mkdirat: nr::MKDIRAT; replaces libc::syscall(libc::SYS_mkdirat) by linux::mkdirat
//! replaces libc::SYS_linkat: nr::LINKAT; replaces libc::syscall(libc::SYS_linkat) by linux::linkat
//! replaces libc::SYS_gettid: nr::GETTID; replaces libc::syscall(libc::SYS_gettid) by linux::sample::gettid (returns i32, cannot fail)
//! replaces libc::SYS_statx: nr::STATX; replaces libc::syscall(libc::SYS_statx) by linux::statx
//! replaces libc::SYS_seccomp: nr::SECCOMP; replaces libc::syscall(libc::SYS_seccomp) by linux::seccomp_filter (SECCOMP_SET_MODE_FILTER)
//! replaces libc::SYS_process_madvise: nr::PROCESS_MADVISE; replaces libc::syscall(libc::SYS_process_madvise) by linux::process_madvise
//! replaces libc::SYS_ppoll: nr::PPOLL; replaces libc::syscall(libc::SYS_ppoll) by linux::ppoll
//! replaces libc::SYS_open_by_handle_at: nr::OPEN_BY_HANDLE_AT; replaces libc::syscall(libc::SYS_open_by_handle_at) by linux::open_by_handle_at
//! replaces libc::SYS_name_to_handle_at: nr::NAME_TO_HANDLE_AT; replaces libc::syscall(libc::SYS_name_to_handle_at) by linux::name_to_handle_at
//! replaces libc::SYS_membarrier: nr::MEMBARRIER; replaces libc::syscall(libc::SYS_membarrier) by linux::membarrier
//! replaces libc::SYS_mbind: nr::MBIND; replaces libc::syscall(libc::SYS_mbind) by linux::mbind
//! replaces libc::SYS_landlock_create_ruleset: nr::LANDLOCK_CREATE_RULESET; replaces libc::syscall(libc::SYS_landlock_create_ruleset) by linux::landlock_create_ruleset (the version probe: linux::landlock_abi)
//! replaces libc::SYS_landlock_add_rule: nr::LANDLOCK_ADD_RULE; replaces libc::syscall(libc::SYS_landlock_add_rule) by linux::landlock_add_rule_path and linux::landlock_add_rule_port
//! replaces libc::SYS_landlock_restrict_self: nr::LANDLOCK_RESTRICT_SELF; replaces libc::syscall(libc::SYS_landlock_restrict_self) by linux::landlock_restrict_self
//! replaces libc::SYS_getdents64: nr::GETDENTS64; replaces libc::syscall(libc::SYS_getdents64) by linux::getdents64
//! replaces libc::SYS_fanotify_mark: nr::FANOTIFY_MARK; replaces libc::syscall(libc::SYS_fanotify_mark) by linux::fanotify_mark
//! replaces libc::SYS_fanotify_init: nr::FANOTIFY_INIT; replaces libc::syscall(libc::SYS_fanotify_init) by linux::fanotify_init
//! replaces libc::SYS_close_range: nr::CLOSE_RANGE; replaces libc::syscall(libc::SYS_close_range) by linux::slot::CloseRange (chosen once from KernelFacts; CloseRange::Call.close_from(first, &mut []) is the call itself)
//! replaces libc::SYS_write: nr::WRITE; replaces libc::syscall(libc::SYS_write) by linux::write
//! replaces libc::SYS_read: nr::READ; replaces libc::syscall(libc::SYS_read) by linux::read
//! replaces libc::SYS_close: nr::CLOSE; replaces libc::syscall(libc::SYS_close) by linux::close (or dropping a linux::OwnedFd)
//! replaces libc::SYS_pread64: nr::PREAD64; replaces libc::syscall(libc::SYS_pread64) by linux::pread
//! replaces libc::SYS_pwrite64: nr::PWRITE64; replaces libc::syscall(libc::SYS_pwrite64) by linux::pwrite
//! replaces libc::SYS_preadv: nr::PREADV; replaces libc::syscall(libc::SYS_preadv) by linux::fdctl::preadv
//! replaces libc::SYS_fsync: nr::FSYNC; replaces libc::syscall(libc::SYS_fsync) by linux::fsync
//! replaces libc::SYS_fdatasync: nr::FDATASYNC; replaces libc::syscall(libc::SYS_fdatasync) by linux::fdatasync
//! replaces libc::SYS_sync_file_range: nr::SYNC_FILE_RANGE; replaces libc::syscall(libc::SYS_sync_file_range) by linux::sync_file_range
//! replaces libc::SYS_ftruncate: nr::FTRUNCATE; replaces libc::syscall(libc::SYS_ftruncate) by linux::ftruncate
//! replaces libc::SYS_lseek: nr::LSEEK; replaces libc::syscall(libc::SYS_lseek) by linux::lseek
//! replaces libc::SYS_copy_file_range: nr::COPY_FILE_RANGE; replaces libc::syscall(libc::SYS_copy_file_range) by linux::copy_file_range
//! replaces libc::SYS_ioctl: nr::IOCTL; replaces libc::syscall(libc::SYS_ioctl) by linux::fdctl::ioctl (typed forms: linux::fdctl::fionbio, linux::ficlone, linux::ficlonerange, linux::window_size, linux::is_terminal)
//! replaces libc::SYS_mmap: nr::MMAP; replaces libc::syscall(libc::SYS_mmap) by linux::mmap
//! replaces libc::SYS_munmap: nr::MUNMAP; replaces libc::syscall(libc::SYS_munmap) by linux::munmap
//! replaces libc::SYS_madvise: nr::MADVISE; replaces libc::syscall(libc::SYS_madvise) by linux::madvise
//! replaces libc::SYS_mprotect: nr::MPROTECT; replaces libc::syscall(libc::SYS_mprotect) by linux::mprotect
//! replaces libc::SYS_msync: nr::MSYNC; replaces libc::syscall(libc::SYS_msync) by linux::fdctl::msync
//! replaces libc::SYS_mincore: nr::MINCORE; replaces libc::syscall(libc::SYS_mincore) by linux::fdctl::mincore
//! replaces libc::SYS_memfd_create: nr::MEMFD_CREATE; replaces libc::syscall(libc::SYS_memfd_create) by linux::fdctl::memfd_create
//! replaces libc::SYS_fallocate: nr::FALLOCATE; replaces libc::syscall(libc::SYS_fallocate) by linux::fdctl::fallocate
//! replaces libc::SYS_fcntl: nr::FCNTL; replaces libc::syscall(libc::SYS_fcntl) by the typed linux::fdctl operations (status_flags, set_status_flags, set_nonblocking, close_on_exec, set_close_on_exec, dup_at_least, add_seals)
//! replaces libc::SYS_dup: nr::DUP; replaces libc::syscall(libc::SYS_dup) by linux::fdctl::dup
//! replaces libc::SYS_dup3: nr::DUP3; replaces libc::syscall(libc::SYS_dup3) by linux::fdctl::dup3 (and linux::fdctl::dup2)
//! replaces libc::SYS_pipe2: nr::PIPE2; replaces libc::syscall(libc::SYS_pipe2) by linux::pipe2
//! replaces libc::SYS_eventfd2: nr::EVENTFD2; replaces libc::syscall(libc::SYS_eventfd2) by linux::eventfd2
//! replaces libc::SYS_getrandom: nr::GETRANDOM; replaces libc::syscall(libc::SYS_getrandom) by linux::getrandom
//! replaces libc::SYS_symlinkat: nr::SYMLINKAT; replaces libc::syscall(libc::SYS_symlinkat) by linux::symlinkat
//! replaces libc::SYS_readlinkat: nr::READLINKAT; replaces libc::syscall(libc::SYS_readlinkat) by linux::readlinkat
//! replaces libc::SYS_fchmod: nr::FCHMOD; replaces libc::syscall(libc::SYS_fchmod) by linux::fchmod
//! replaces libc::SYS_utimensat: nr::UTIMENSAT; replaces libc::syscall(libc::SYS_utimensat) by linux::utimensat
//! replaces libc::SYS_fstatfs: nr::FSTATFS; replaces libc::syscall(libc::SYS_fstatfs) by linux::fstatfs
//! replaces libc::SYS_statfs: nr::STATFS; replaces libc::syscall(libc::SYS_statfs) by linux::task::statfs
//! replaces libc::SYS_flock: nr::FLOCK; replaces libc::syscall(libc::SYS_flock) by linux::flock
//! replaces libc::SYS_newfstatat: nr::NEWFSTATAT; replaces libc::syscall(libc::SYS_newfstatat) by linux::task::fstatat (and linux::task::fstat)
//! replaces libc::SYS_mknodat: nr::MKNODAT; replaces libc::syscall(libc::SYS_mknodat) by linux::task::mknodat (and linux::task::mkfifo)
//! replaces libc::SYS_fchownat: nr::FCHOWNAT; replaces libc::syscall(libc::SYS_fchownat) by linux::task::fchownat (and linux::task::fchown)
//! replaces libc::SYS_setxattr: nr::SETXATTR; replaces libc::syscall(libc::SYS_setxattr) by linux::task::setxattr
//! replaces libc::SYS_removexattr: nr::REMOVEXATTR; replaces libc::syscall(libc::SYS_removexattr) by linux::task::removexattr
//! replaces libc::SYS_fgetxattr: nr::FGETXATTR; replaces libc::syscall(libc::SYS_fgetxattr) by linux::task::fgetxattr
//! replaces libc::SYS_fadvise64: nr::FADVISE64; replaces libc::syscall(libc::SYS_fadvise64) by linux::task::fadvise64
//! replaces libc::SYS_mount: nr::MOUNT; replaces libc::syscall(libc::SYS_mount) by linux::task::mount
//! replaces libc::SYS_umount2: nr::UMOUNT2; replaces libc::syscall(libc::SYS_umount2) by linux::task::umount2
//! replaces libc::SYS_chdir: nr::CHDIR; replaces libc::syscall(libc::SYS_chdir) by linux::task::chdir
//! replaces libc::SYS_unshare: nr::UNSHARE; replaces libc::syscall(libc::SYS_unshare) by linux::task::unshare
//! replaces libc::SYS_process_vm_readv: nr::PROCESS_VM_READV; replaces libc::syscall(libc::SYS_process_vm_readv) by linux::task::process_vm_readv
//! replaces libc::SYS_getpriority: nr::GETPRIORITY; replaces libc::syscall(libc::SYS_getpriority) by linux::task::getpriority
//! replaces libc::SYS_setpriority: nr::SETPRIORITY; replaces libc::syscall(libc::SYS_setpriority) by linux::task::setpriority
//! replaces libc::SYS_sysinfo: nr::SYSINFO; replaces libc::syscall(libc::SYS_sysinfo) by linux::task::sysinfo
//! replaces libc::SYS_clone: nr::CLONE; replaces libc::syscall(libc::SYS_clone) by linux::task::fork (SIGCHLD only)
//! replaces libc::SYS_clone3: nr::CLONE3; replaces libc::syscall(libc::SYS_clone3) by linux::clone3_process
//! replaces libc::SYS_wait4: nr::WAIT4; replaces libc::syscall(libc::SYS_wait4) by linux::wait4_now (WNOHANG) and linux::task::waitpid
//! replaces libc::SYS_waitid: nr::WAITID; replaces libc::syscall(libc::SYS_waitid) by linux::waitid
//! replaces libc::SYS_exit: nr::EXIT; replaces libc::syscall(libc::SYS_exit) by linux::exit_thread
//! replaces libc::SYS_exit_group: nr::EXIT_GROUP; replaces libc::syscall(libc::SYS_exit_group) by linux::exit_group
//! replaces libc::SYS_execveat: nr::EXECVEAT; replaces libc::syscall(libc::SYS_execveat) by linux::execveat (execve's form: linux::task::execve)
//! replaces libc::SYS_getpid: nr::GETPID; replaces libc::syscall(libc::SYS_getpid) by linux::getpid
//! replaces libc::SYS_kill: nr::KILL; replaces libc::syscall(libc::SYS_kill) by linux::kill (a group: linux::kill_group)
//! replaces libc::SYS_tgkill: nr::TGKILL; replaces libc::syscall(libc::SYS_tgkill) by linux::signal::tgkill (and linux::signal::raise)
//! replaces libc::SYS_sched_yield: nr::SCHED_YIELD; replaces libc::syscall(libc::SYS_sched_yield) by linux::sched_yield
//! replaces libc::SYS_prctl: nr::PRCTL; replaces libc::syscall(libc::SYS_prctl) by linux::prctl (PR_GET_AUXV: linux::auxv::prctl_get_auxv)
//! replaces libc::SYS_rt_sigaction: nr::RT_SIGACTION; replaces libc::syscall(libc::SYS_rt_sigaction) by linux::rt_sigaction (and linux::signal::get_action, linux::signal::set_action)
//! replaces libc::SYS_rt_sigprocmask: nr::RT_SIGPROCMASK; replaces libc::syscall(libc::SYS_rt_sigprocmask) by linux::rt_sigprocmask
//! replaces libc::SYS_rt_sigreturn: nr::RT_SIGRETURN; no call of its own: linux::signal::set_action installs blaze-sys's restorer, which makes it
//! replaces libc::SYS_sigaltstack: nr::SIGALTSTACK; replaces libc::syscall(libc::SYS_sigaltstack) by linux::signal::alt_stack and linux::signal::set_alt_stack
//! replaces libc::SYS_getrusage: nr::GETRUSAGE; replaces libc::syscall(libc::SYS_getrusage) by linux::getrusage
//! replaces libc::SYS_clock_gettime: nr::CLOCK_GETTIME; replaces libc::syscall(libc::SYS_clock_gettime) by linux::clock_gettime (a hot path: linux::clock_gettime_vdso)
//! replaces libc::SYS_clock_nanosleep: nr::CLOCK_NANOSLEEP; replaces libc::syscall(libc::SYS_clock_nanosleep) by linux::clock_nanosleep_until (TIMER_ABSTIME only)
//! replaces libc::SYS_inotify_init1: nr::INOTIFY_INIT1; replaces libc::syscall(libc::SYS_inotify_init1) by linux::inotify_init1
//! replaces libc::SYS_inotify_add_watch: nr::INOTIFY_ADD_WATCH; replaces libc::syscall(libc::SYS_inotify_add_watch) by linux::inotify_add_watch
//! replaces libc::SYS_inotify_rm_watch: nr::INOTIFY_RM_WATCH; replaces libc::syscall(libc::SYS_inotify_rm_watch) by linux::inotify_rm_watch
//! replaces libc::SYS_socket: nr::SOCKET; replaces libc::syscall(libc::SYS_socket) by linux::socket
//! replaces libc::SYS_socketpair: nr::SOCKETPAIR; replaces libc::syscall(libc::SYS_socketpair) by linux::sock::socketpair
//! replaces libc::SYS_connect: nr::CONNECT; replaces libc::syscall(libc::SYS_connect) by linux::connect
//! replaces libc::SYS_bind: nr::BIND; replaces libc::syscall(libc::SYS_bind) by linux::bind
//! replaces libc::SYS_listen: nr::LISTEN; replaces libc::syscall(libc::SYS_listen) by linux::listen
//! replaces libc::SYS_accept4: nr::ACCEPT4; replaces libc::syscall(libc::SYS_accept4) by linux::accept4
//! replaces libc::SYS_shutdown: nr::SHUTDOWN; replaces libc::syscall(libc::SYS_shutdown) by linux::shutdown
//! replaces libc::SYS_sendmsg: nr::SENDMSG; replaces libc::syscall(libc::SYS_sendmsg) by linux::sendmsg (a whole header: linux::sock::sendmsg_header)
//! replaces libc::SYS_recvmsg: nr::RECVMSG; replaces libc::syscall(libc::SYS_recvmsg) by linux::recvmsg (a whole header: linux::sock::recvmsg_header)
//! replaces libc::SYS_sendmmsg: nr::SENDMMSG; replaces libc::syscall(libc::SYS_sendmmsg) by linux::sock::sendmmsg
//! replaces libc::SYS_recvmmsg: nr::RECVMMSG; replaces libc::syscall(libc::SYS_recvmmsg) by linux::sock::recvmmsg
//! replaces libc::SYS_setsockopt: nr::SETSOCKOPT; replaces libc::syscall(libc::SYS_setsockopt) by linux::setsockopt (option bytes: linux::sock::setsockopt_raw)
//! replaces libc::SYS_getsockopt: nr::GETSOCKOPT; replaces libc::syscall(libc::SYS_getsockopt) by linux::sock::getsockopt_raw
//! replaces libc::SYS_getsockname: nr::GETSOCKNAME; replaces libc::syscall(libc::SYS_getsockname) by linux::sock::getsockname
//! replaces libc::SYS_uname: nr::UNAME; replaces libc::syscall(libc::SYS_uname) by linux::host::uname
//! replaces libc::SYS_getuid: nr::GETUID; replaces libc::syscall(libc::SYS_getuid) by linux::host::getuid
//! replaces libc::SYS_geteuid: nr::GETEUID; replaces libc::syscall(libc::SYS_geteuid) by linux::host::geteuid
//! replaces libc::SYS_getppid: nr::GETPPID; replaces libc::syscall(libc::SYS_getppid) by linux::host::getppid
//! replaces libc::SYS_getpgid: nr::GETPGID; replaces libc::syscall(libc::SYS_getpgid) by linux::host::getpgid
//! replaces libc::SYS_setpgid: nr::SETPGID; replaces libc::syscall(libc::SYS_setpgid) by linux::host::setpgid
//! replaces libc::SYS_setsid: nr::SETSID; replaces libc::syscall(libc::SYS_setsid) by linux::host::setsid
//! replaces libc::SYS_prlimit64: nr::PRLIMIT64; replaces libc::syscall(libc::SYS_prlimit64) by linux::host::prlimit64 (a confined child's step: linux::confine::prlimit)
//! replaces libc::SYS_sched_setaffinity: nr::SCHED_SETAFFINITY; replaces libc::syscall(libc::SYS_sched_setaffinity) by linux::host::sched_setaffinity
//! replaces libc::SYS_sched_getaffinity: nr::SCHED_GETAFFINITY; replaces libc::syscall(libc::SYS_sched_getaffinity) by linux::host::sched_getaffinity
//! replaces libc::SYS_open: nr::OPEN (x86-64 only, as data: a seccomp filter's list); a call: linux::openat with linux::CWD
//! replaces libc::SYS_creat: nr::CREAT (x86-64 only, as data); a call: linux::openat with linux::CWD and OpenFlags::CREATE | WRITE | TRUNCATE
//! replaces libc::SYS_mkdir: nr::MKDIR (x86-64 only, as data); a call: linux::mkdirat with linux::CWD
//! replaces libc::SYS_rmdir: nr::RMDIR (x86-64 only, as data); a call: linux::unlinkat with linux::CWD and Unlink::Dir
//! replaces libc::SYS_unlink: nr::UNLINK (x86-64 only, as data); a call: linux::unlinkat with linux::CWD and Unlink::File
//! replaces libc::SYS_link: nr::LINK (x86-64 only, as data); a call: linux::linkat with linux::CWD
//! replaces libc::SYS_rename: nr::RENAME (x86-64 only, as data); a call: linux::renameat2 with linux::CWD and no RenameFlags

macro_rules! numbers {
    ($($name:ident = $x86_64:literal, $aarch64:literal;)*) => {
        $(
            #[doc = concat!("`", stringify!($name), "`.")]
            #[cfg(target_arch = "x86_64")]
            pub const $name: usize = $x86_64;
            #[doc = concat!("`", stringify!($name), "`.")]
            #[cfg(target_arch = "aarch64")]
            pub const $name: usize = $aarch64;
        )*

        /// Every call with its name and its number on this architecture.
        pub const TABLE: &[(&str, usize)] = &[$((stringify!($name), $name),)*];
    };
}

numbers! {
    //                    x86-64  aarch64
    READ                = 0,      63;
    WRITE               = 1,      64;
    CLOSE               = 3,      57;
    PREAD64             = 17,     67;
    PWRITE64            = 18,     68;
    OPENAT              = 257,    56;
    OPENAT2             = 437,    437;
    FSYNC               = 74,     82;
    FDATASYNC           = 75,     83;
    SYNC_FILE_RANGE     = 277,    84;
    FTRUNCATE           = 77,     46;
    LSEEK               = 8,      62;
    COPY_FILE_RANGE     = 326,    285;
    RENAMEAT2           = 316,    276;
    UNLINKAT            = 263,    35;
    MKDIRAT             = 258,    34;
    STATX               = 332,    291;
    GETDENTS64          = 217,    61;
    IOCTL               = 16,     29;
    MMAP                = 9,      222;
    MUNMAP              = 11,     215;
    MADVISE             = 28,     233;
    MPROTECT            = 10,     226;
    FUTEX               = 202,    98;
    PPOLL               = 271,    73;
    PIDFD_OPEN          = 434,    434;
    WAITID              = 247,    95;
    WAIT4               = 61,     260;
    CLOCK_GETTIME       = 228,    113;
    CLOCK_NANOSLEEP     = 230,    115;
    CLONE3              = 435,    435;
    EXIT                = 60,     93;
    EXIT_GROUP          = 231,    94;
    GETRANDOM           = 318,    278;
    IO_URING_SETUP      = 425,    425;
    IO_URING_ENTER      = 426,    426;
    IO_URING_REGISTER   = 427,    427;
    SOCKET              = 41,     198;
    CONNECT             = 42,     203;
    BIND                = 49,     200;
    LISTEN              = 50,     201;
    ACCEPT4             = 288,    242;
    SENDMSG             = 46,     211;
    RECVMSG             = 47,     212;
    SHUTDOWN            = 48,     210;
    PIPE2               = 293,    59;
    EVENTFD2            = 290,    19;
    PRCTL               = 157,    167;
    SECCOMP             = 317,    277;
    RT_SIGACTION        = 13,     134;
    RT_SIGPROCMASK      = 14,     135;
    GETPID              = 39,     172;
    KILL                = 62,     129;
    GETRUSAGE           = 98,     165;
    PERF_EVENT_OPEN     = 298,    241;
    SCHED_YIELD         = 24,     124;
    INOTIFY_INIT1       = 294,    26;
    INOTIFY_ADD_WATCH   = 254,    27;
    INOTIFY_RM_WATCH    = 255,    28;
    FANOTIFY_INIT       = 300,    262;
    FANOTIFY_MARK       = 301,    263;
    LINKAT              = 265,    37;
    SYMLINKAT           = 266,    36;
    READLINKAT          = 267,    78;
    FCHMOD              = 91,     52;
    UTIMENSAT           = 280,    88;
    FSTATFS             = 138,    44;
    FLOCK               = 73,     32;
    EXECVEAT            = 322,    281;
    SETSOCKOPT          = 54,     208;
    LANDLOCK_CREATE_RULESET = 444, 444;
    LANDLOCK_ADD_RULE   = 445,    445;
    LANDLOCK_RESTRICT_SELF = 446, 446;
    PRLIMIT64           = 302,    261;
    IOPRIO_SET          = 251,    30;
    IOPRIO_GET          = 252,    31;
    RENAMEAT            = 264,    38;
    NAME_TO_HANDLE_AT   = 303,    264;
    OPEN_BY_HANDLE_AT   = 304,    265;
    PIDFD_SEND_SIGNAL   = 424,    424;
    GETTID              = 186,    178;
    MEMBARRIER          = 324,    283;
    CLOSE_RANGE         = 436,    436;
    MBIND               = 237,    235;
    PROCESS_MADVISE     = 440,    440;
    SOCKETPAIR          = 53,     199;
    GETSOCKNAME         = 51,     204;
    GETSOCKOPT          = 55,     209;
    SENDMMSG            = 307,    269;
    RECVMMSG            = 299,    243;
    FCNTL               = 72,     25;
    DUP                 = 32,     23;
    DUP3                = 292,    24;
    PREADV              = 295,    69;
    FALLOCATE           = 285,    47;
    MEMFD_CREATE        = 319,    279;
    MINCORE             = 27,     232;
    MSYNC               = 26,     227;
    UNAME               = 63,     160;
    GETUID              = 102,    174;
    GETEUID             = 107,    175;
    GETPPID             = 110,    173;
    GETPGID             = 121,    155;
    SETPGID             = 109,    154;
    SETSID              = 112,    157;
    TGKILL              = 234,    131;
    SCHED_SETAFFINITY   = 203,    122;
    SCHED_GETAFFINITY   = 204,    123;
    SIGALTSTACK         = 131,    132;
    RT_SIGRETURN        = 15,     139;
    CLONE               = 56,     220;
    CHDIR               = 80,     49;
    UNSHARE             = 272,    97;
    PROCESS_VM_READV    = 310,    270;
    NEWFSTATAT          = 262,    79;
    STATFS              = 137,    43;
    MKNODAT             = 259,    33;
    FCHOWNAT            = 260,    54;
    SETXATTR            = 188,    5;
    REMOVEXATTR         = 197,    14;
    FGETXATTR           = 193,    10;
    FADVISE64           = 221,    223;
    MOUNT               = 165,    40;
    UMOUNT2             = 166,    39;
    GETPRIORITY         = 140,    141;
    SETPRIORITY         = 141,    140;
    SYSINFO             = 99,     179;
    PWRITEV2            = 328,    287;
    FUTEX_WAITV         = 449,    449;
    MEMFD_SECRET        = 447,    447;
    CACHESTAT           = 451,    451;
    MSEAL               = 462,    462;
}

/// The x86-64 calls on a path from the current directory, which aarch64 never had (it has
/// only the `*at` forms). Numbers as data only: a seccomp filter that must refuse every way
/// to make or remove a name on x86-64 lists these beside the `*at` ones. blaze calls none of
/// them (each row says the call it makes instead).
#[cfg(target_arch = "x86_64")]
mod x86_64_only {
    /// `open` (2). A call: `linux::openat` with `linux::CWD`.
    /// replaces libc::SYS_open
    pub const OPEN: usize = 2;
    /// `creat` (85). A call: `linux::openat` with `linux::CWD` and `OpenFlags::CREATE | WRITE | TRUNCATE`.
    /// replaces libc::SYS_creat
    pub const CREAT: usize = 85;
    /// `mkdir` (83). A call: `linux::mkdirat` with `linux::CWD`.
    /// replaces libc::SYS_mkdir
    pub const MKDIR: usize = 83;
    /// `rmdir` (84). A call: `linux::unlinkat` with `linux::CWD` and `Unlink::Dir`.
    /// replaces libc::SYS_rmdir
    pub const RMDIR: usize = 84;
    /// `link` (86). A call: `linux::linkat` with `linux::CWD`.
    /// replaces libc::SYS_link
    pub const LINK: usize = 86;
    /// `unlink` (87). A call: `linux::unlinkat` with `linux::CWD` and `Unlink::File`.
    /// replaces libc::SYS_unlink
    pub const UNLINK: usize = 87;
    /// `rename` (82). A call: `linux::renameat2` with `linux::CWD` and no `RenameFlags`.
    /// replaces libc::SYS_rename
    pub const RENAME: usize = 82;

    /// Every x86-64-only call with its name and number.
    pub const TABLE: &[(&str, usize)] = &[
        ("OPEN", OPEN),
        ("CREAT", CREAT),
        ("MKDIR", MKDIR),
        ("RMDIR", RMDIR),
        ("LINK", LINK),
        ("UNLINK", UNLINK),
        ("RENAME", RENAME),
    ];
}

#[cfg(target_arch = "x86_64")]
pub use x86_64_only::{CREAT, LINK, MKDIR, OPEN, RENAME, RMDIR, UNLINK};
/// Every x86-64-only call with its name and number (x86-64 only).
#[cfg(target_arch = "x86_64")]
pub use x86_64_only::TABLE as X86_64_ONLY;

/// The x86-64-only rows are libc's `SYS_*` numbers.
#[cfg(all(test, target_arch = "x86_64"))]
mod x86_64_only_tests {
    #[test]
    fn the_x86_64_only_rows_are_libcs() {
        let libcs: &[(&str, libc::c_long)] = &[
            ("OPEN", libc::SYS_open),
            ("CREAT", libc::SYS_creat),
            ("MKDIR", libc::SYS_mkdir),
            ("RMDIR", libc::SYS_rmdir),
            ("LINK", libc::SYS_link),
            ("UNLINK", libc::SYS_unlink),
            ("RENAME", libc::SYS_rename),
        ];
        assert_eq!(libcs.len(), super::X86_64_ONLY.len());
        for ((name, ours), (theirs_name, theirs)) in super::X86_64_ONLY.iter().zip(libcs) {
            assert_eq!(name, theirs_name);
            assert_eq!(*ours, *theirs as usize, "{name}");
        }
    }
}
