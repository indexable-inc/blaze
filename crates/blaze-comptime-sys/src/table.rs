//! THE TABLE: every outside C name a compile-time child's code may be bound to, and how. It is
//! the whole wall (comptime-native.md, "The system layer"): the launcher binds an outside name
//! of a compile-time run ONLY through [`find`], so a name not here is refused at bind time
//! ([`BindRefused`], which the launcher reports with the record and symbol that named it) and is
//! never bound to the real C library by default.
//!
//! How the list was made (evidence, 2026-11-20): `nm -u --no-llvm-bc` over the objects of the
//! pinned nightly's (nightly-2026-10-27) `aarch64-apple-darwin` `libstd`, `libproc_macro`,
//! `libcore` and `libpanic_unwind` rlibs (199 names once the Mach-O leading `_` is taken off,
//! Rust's own symbols left out), which are the names unchanged std, `proc_macro` and the crates
//! over them reach on macOS arm64; then, for Linux arm64, the `libc::` functions std's
//! `sys` and `os` sources call that macOS does not import (`grep -ohE 'libc::[a-z_0-9]+\('`
//! over library/std/src of the same nightly, minus the macOS names) and glibc's `64` names std
//! binds on `target_env = "gnu"`, 58 names. The Linux rows are FROM SOURCE, not yet from `nm`:
//! the shelf on this Mac holds no `aarch64-unknown-linux-gnu` std, so the list is checked by
//! `nm -u` on the Linux shelf's libstd on the VM, and every name it shows that is not here is
//! a row to add (the table refuses it until then, by name). `getentropy` is kept for both: the
//! `getrandom` crate calls it on macOS. Eight rows come from the brief's list and from crates
//! over std rather than from std's imports: `arc4random_buf`, `execve`, `gettimeofday`,
//! `mach_absolute_time`, `posix_spawn`, `readdir_r`, `rewinddir`, and Linux's plain `realpath`.
//! 265 rows in all (166 for both systems, 42 macOS only, 57 Linux only): 77 the process's own,
//! 3 own after a check, 106 local, 53 asking, 25 refused, 1 data word.
//!
//! The rows are sorted by name's bytes (a test holds it); a name is the C name as the record
//! keeps it (no Mach-O `_`; a Darwin suffix such as `$DARWIN_EXTSN` kept).

#![blaze::sans_io]

use blaze_comptime::CallKind;

use crate::errno::{Errno, Os};

/// The systems a row is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum On {
    Mac,
    Linux,
    Both,
}

impl On {
    pub const fn has(self, os: Os) -> bool {
        matches!((self, os), (On::Both, _) | (On::Mac, Os::Mac) | (On::Linux, Os::Linux))
    }
}

/// How one C name is answered in a compile-time child.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum How {
    /// The process's own definition (the launcher's `dlsym`): it reaches nothing outside the
    /// process (memory, strings, arithmetic, unwinding, locks and thread-locals of the one
    /// thread). `malloc` is the child's own memory, bounded by the run's memory budget.
    Own,
    /// The process's own definition after the layer checks its arguments: `mmap` of anonymous
    /// private memory only, `mprotect` without `PROT_EXEC` (sys.rs [`crate::mmap_allowed`]).
    OwnChecked,
    /// The layer's answer from its own state ([`crate::Sys`]); nothing crosses. A descriptor,
    /// a directory stream, the environment once read, the working directory, a pipe, a spawned
    /// command's status, a fixed answer (`getpid`, `sysconf`) or `EBADF` for a socket call on
    /// a descriptor that cannot exist.
    Local,
    /// The layer asks the host with this protocol call (blaze-comptime call.rs).
    Asks(CallKind),
    /// Refused: the C failure value with this `errno`, and a `Call::Denied` so the host records
    /// the finding naming the function.
    Refused(Errno),
    /// A data symbol the layer defines (`environ`): its address is the layer's word.
    Data,
}

/// One row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Name {
    pub c: &'static str,
    pub on: On,
    pub how: How,
}

/// Why a name is not bound.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unbound {
    /// No row: the wall.
    NotInTable,
    /// A row for the other system only.
    OtherSystem { on: On },
}

/// A name the table does not bind, refused at bind time (rule 3: never bound to the real C
/// library instead). The launcher adds the record and symbol that named it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BindRefused<'n> {
    pub name: &'n str,
    pub why: Unbound,
}

impl core::fmt::Display for BindRefused<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.why {
            Unbound::NotInTable => write!(
                f,
                "the C name `{}` is not in a compile-time run's system table (blaze-comptime-sys table.rs): compile-time code reaches the system only through it",
                self.name
            ),
            Unbound::OtherSystem { on } => {
                write!(f, "the C name `{}` is in a compile-time run's system table for {on:?} only", self.name)
            }
        }
    }
}

/// The row of `name` on `os`, or why there is none.
pub fn find(name: &str, os: Os) -> Result<&'static Name, BindRefused<'_>> {
    match NAMES.binary_search_by(|row| row.c.as_bytes().cmp(name.as_bytes())) {
        Ok(at) if NAMES[at].on.has(os) => Ok(&NAMES[at]),
        Ok(at) => Err(BindRefused { name, why: Unbound::OtherSystem { on: NAMES[at].on } }),
        Err(_) => Err(BindRefused { name, why: Unbound::NotInTable }),
    }
}

const fn n(c: &'static str, on: On, how: How) -> Name {
    Name { c, on, how }
}

use CallKind as K;
use How::{Asks, Data, Local, Own, OwnChecked, Refused};
use On::{Both, Linux, Mac};

/// Every row, sorted by name.
pub const NAMES: &[Name] = &[
    n("CCRandomGenerateBytes", Mac, Asks(K::Random)),
    n("_NSGetArgc", Mac, Local),
    n("_NSGetArgv", Mac, Local),
    n("_NSGetEnviron", Mac, Local),
    n("_NSGetExecutablePath", Mac, Local),
    n("_Unwind_Backtrace", Both, Own),
    n("_Unwind_DeleteException", Both, Own),
    n("_Unwind_GetCFA", Both, Own),
    n("_Unwind_GetDataRelBase", Both, Own),
    n("_Unwind_GetIP", Both, Own),
    n("_Unwind_GetIPInfo", Both, Own),
    n("_Unwind_GetLanguageSpecificData", Both, Own),
    n("_Unwind_GetRegionStart", Both, Own),
    n("_Unwind_GetTextRelBase", Both, Own),
    n("_Unwind_RaiseException", Both, Own),
    n("_Unwind_Resume", Both, Own),
    n("_Unwind_SetGR", Both, Own),
    n("_Unwind_SetIP", Both, Own),
    n("__cxa_thread_atexit_impl", Linux, Own),
    // The thread's real `errno`: the layer writes its refusals there too (child.rs `Real`), so
    // the C library's own functions and the layer's agree on one cell.
    n("__errno_location", Linux, Own),
    n("__error", Mac, Own),
    n("__udivti3", Both, Own),
    n("__xpg_strerror_r", Linux, Own),
    // No images are listed: a backtrace names no file and no address (rule 25).
    n("_dyld_get_image_header", Mac, Local),
    n("_dyld_get_image_name", Mac, Local),
    n("_dyld_get_image_vmaddr_slide", Mac, Local),
    n("_dyld_image_count", Mac, Local),
    n("_exit", Both, Asks(K::Exit)),
    n("_tlv_atexit", Mac, Own),
    n("_tlv_bootstrap", Mac, Own),
    n("abort", Both, Asks(K::Exit)),
    n("accept", Both, Local),
    n("accept4", Linux, Local),
    n("arc4random_buf", Mac, Asks(K::Random)),
    n("bind", Both, Local),
    n("bzero", Both, Own),
    n("calloc", Both, Own),
    n("chdir", Both, Asks(K::Look)),
    n("chmod", Both, Refused(Errno::Perm)),
    n("chown", Both, Refused(Errno::Perm)),
    n("chroot", Both, Refused(Errno::Perm)),
    n("clock_getres", Linux, Local),
    n("clock_gettime", Both, Asks(K::Clock)),
    n("clock_nanosleep", Linux, Asks(K::Sleep)),
    n("close", Both, Local),
    n("closedir", Both, Local),
    n("confstr", Mac, Local),
    n("connect", Both, Local),
    n("copyfile_state_alloc", Mac, Local),
    n("copyfile_state_free", Mac, Local),
    n("copyfile_state_get", Mac, Local),
    n("dirfd", Both, Local),
    n("dispatch_release", Mac, Own),
    n("dispatch_semaphore_create", Mac, Own),
    n("dispatch_semaphore_signal", Mac, Own),
    n("dispatch_semaphore_wait", Mac, Own),
    n("dispatch_time", Mac, Own),
    n("dl_iterate_phdr", Linux, Local),
    n("dlclose", Both, Local),
    n("dlerror", Both, Local),
    n("dlopen", Both, Refused(Errno::NoEnt)),
    // Answered by THIS table (child.rs `address_of`): std's weak lookups (`weak!`, such as
    // `posix_spawn_file_actions_addchdir_np` on macOS) find a name's binding when it has a row
    // on this system and NULL when it has none, so the branch std takes is the table's, the same
    // on every host of one system. `dlsym` never reaches the real loader.
    n("dlsym", Both, Local),
    n("dup", Both, Local),
    n("dup2", Both, Local),
    n("environ", Linux, Data),
    // A process image is replaced only through `posix_spawn`'s one `Call::Spawn`.
    n("execve", Both, Refused(Errno::NoSys)),
    n("execvp", Both, Refused(Errno::NoSys)),
    n("exit", Both, Asks(K::Exit)),
    n("fchmod", Both, Refused(Errno::Perm)),
    n("fchmodat", Both, Refused(Errno::Perm)),
    n("fchown", Both, Refused(Errno::Perm)),
    // `ENOTSUP`, as on a volume that cannot clone: std then copies with `fcopyfile`.
    n("fclonefileat", Mac, Local),
    n("fcntl", Both, Local),
    n("fcopyfile", Mac, Asks(K::WriteFile)),
    n("fdatasync", Linux, Local),
    n("fdopendir", Both, Asks(K::Look)),
    n("flock", Both, Local),
    n("fork", Both, Refused(Errno::NoSys)),
    n("free", Both, Own),
    n("freeaddrinfo", Both, Local),
    n("fsetattrlist", Mac, Refused(Errno::Perm)),
    n("fstat", Both, Local),
    n("fstat64", Linux, Local),
    n("fstatat", Both, Asks(K::Look)),
    n("fstatat64", Linux, Asks(K::Look)),
    n("fsync", Linux, Local),
    n("ftruncate", Both, Asks(K::SetLen)),
    n("ftruncate64", Linux, Asks(K::SetLen)),
    n("futimens", Linux, Refused(Errno::Perm)),
    n("gai_strerror", Both, Own),
    n("getaddrinfo", Both, Asks(K::Socket)),
    n("getauxval", Linux, Local),
    n("getcwd", Both, Local),
    n("getentropy", Both, Asks(K::Random)),
    n("getenv", Both, Asks(K::Env)),
    n("getgid", Linux, Local),
    n("gethostname", Both, Local),
    n("getpeereid", Mac, Local),
    n("getpeername", Both, Local),
    n("getpgid", Linux, Local),
    n("getpid", Both, Local),
    n("getppid", Both, Local),
    n("getpwuid_r", Both, Local),
    n("getrandom", Linux, Asks(K::Random)),
    n("getsockname", Both, Local),
    n("getsockopt", Both, Local),
    n("gettid", Linux, Local),
    n("gettimeofday", Both, Asks(K::Clock)),
    n("getuid", Both, Local),
    n("gnu_get_libc_version", Linux, Local),
    n("ioctl", Both, Local),
    n("isatty", Linux, Local),
    n("kill", Both, Local),
    n("killpg", Both, Local),
    n("lchown", Both, Refused(Errno::Perm)),
    n("link", Linux, Refused(Errno::Perm)),
    n("linkat", Both, Refused(Errno::Perm)),
    n("listen", Both, Local),
    n("lseek", Both, Local),
    n("lseek64", Linux, Local),
    n("lstat", Both, Asks(K::Look)),
    n("lstat64", Linux, Asks(K::Look)),
    // Not imported by std (it reads `clock_gettime`), but by crates over it (`quanta`, C code):
    // the monotonic clock in nanoseconds, the timebase being 1/1.
    n("mach_absolute_time", Mac, Asks(K::Clock)),
    n("mach_error_string", Mac, Own),
    n("mach_timebase_info", Mac, Local),
    n("mach_wait_until", Mac, Asks(K::Sleep)),
    n("malloc", Both, Own),
    n("memalign", Linux, Own),
    n("memcmp", Both, Own),
    n("memcpy", Both, Own),
    n("memmove", Both, Own),
    n("memset", Both, Own),
    n("mkdir", Both, Asks(K::CreateDir)),
    n("mkdirat", Both, Asks(K::CreateDir)),
    n("mkfifo", Both, Refused(Errno::Perm)),
    n("mmap", Both, OwnChecked),
    n("mmap64", Linux, OwnChecked),
    n("mprotect", Both, OwnChecked),
    n("munmap", Both, Own),
    n("nanosleep", Both, Asks(K::Sleep)),
    n("open", Both, Asks(K::Look)),
    n("open64", Linux, Asks(K::Look)),
    n("openat", Both, Asks(K::Look)),
    n("openat64", Linux, Asks(K::Look)),
    n("opendir", Both, Asks(K::Look)),
    // `pause` waits for a signal that never comes: refused, not waited out (rule 18).
    n("pause", Mac, Refused(Errno::Intr)),
    n("pipe", Mac, Local),
    n("pipe2", Linux, Local),
    n("poll", Both, Local),
    n("posix_memalign", Both, Own),
    n("posix_spawn", Both, Asks(K::Spawn)),
    // Found by std's weak `dlsym` on macOS (it is not a link-time import there).
    n("posix_spawn_file_actions_addchdir_np", Both, Local),
    n("posix_spawn_file_actions_adddup2", Both, Local),
    n("posix_spawn_file_actions_destroy", Both, Local),
    n("posix_spawn_file_actions_init", Both, Local),
    n("posix_spawnattr_destroy", Both, Local),
    n("posix_spawnattr_init", Both, Local),
    n("posix_spawnattr_setflags", Both, Local),
    n("posix_spawnattr_setpgroup", Both, Local),
    n("posix_spawnattr_setsigdefault", Both, Local),
    n("posix_spawnp", Both, Asks(K::Spawn)),
    n("prctl", Linux, Local),
    n("pread", Both, Local),
    n("pread64", Linux, Local),
    n("preadv", Both, Local),
    n("preadv64", Linux, Local),
    n("pthread_attr_destroy", Both, Own),
    n("pthread_attr_getguardsize", Linux, Own),
    n("pthread_attr_getstack", Linux, Own),
    n("pthread_attr_init", Both, Own),
    n("pthread_attr_setstacksize", Both, Own),
    n("pthread_cond_broadcast", Both, Own),
    n("pthread_cond_destroy", Both, Own),
    n("pthread_cond_init", Linux, Own),
    n("pthread_cond_signal", Both, Own),
    n("pthread_cond_timedwait", Linux, Own),
    n("pthread_cond_timedwait_relative_np", Mac, Own),
    n("pthread_cond_wait", Both, Own),
    n("pthread_condattr_destroy", Linux, Own),
    n("pthread_condattr_init", Linux, Own),
    n("pthread_condattr_setclock", Linux, Own),
    n("pthread_create", Both, Asks(K::Thread)),
    n("pthread_detach", Both, Local),
    n("pthread_get_stackaddr_np", Mac, Own),
    n("pthread_get_stacksize_np", Mac, Own),
    n("pthread_getattr_np", Linux, Own),
    n("pthread_getspecific", Linux, Own),
    n("pthread_join", Both, Local),
    n("pthread_key_create", Linux, Own),
    n("pthread_key_delete", Linux, Own),
    n("pthread_mutex_destroy", Both, Own),
    n("pthread_mutex_init", Both, Own),
    n("pthread_mutex_lock", Both, Own),
    n("pthread_mutex_trylock", Both, Own),
    n("pthread_mutex_unlock", Both, Own),
    n("pthread_mutexattr_destroy", Both, Own),
    n("pthread_mutexattr_init", Both, Own),
    n("pthread_mutexattr_settype", Both, Own),
    n("pthread_self", Both, Own),
    n("pthread_setname_np", Both, Own),
    n("pthread_setspecific", Linux, Own),
    n("pthread_sigmask", Linux, Own),
    n("pthread_threadid_np", Mac, Local),
    n("pwrite", Both, Asks(K::WriteFile)),
    n("pwrite64", Linux, Asks(K::WriteFile)),
    n("pwritev", Both, Asks(K::WriteFile)),
    n("pwritev64", Linux, Asks(K::WriteFile)),
    n("read", Both, Local),
    n("readdir", Both, Local),
    n("readdir64", Linux, Local),
    n("readdir_r", Both, Local),
    // A root holds no link the run can see (blaze-roots lib.rs): a lookup, then `EINVAL` for
    // what exists (as for a file that is not a link) and the lookup's error for what does not.
    n("readlink", Both, Asks(K::Look)),
    n("readv", Both, Local),
    n("realloc", Both, Own),
    n("realpath", Linux, Asks(K::Look)),
    n("realpath$DARWIN_EXTSN", Mac, Asks(K::Look)),
    n("recv", Both, Local),
    n("recvfrom", Both, Local),
    n("recvmsg", Linux, Local),
    n("rename", Both, Asks(K::Rename)),
    n("renameat", Both, Asks(K::Rename)),
    n("renamex_np", Mac, Asks(K::Rename)),
    n("rewinddir", Both, Local),
    n("rmdir", Both, Asks(K::Remove)),
    n("sched_getaffinity", Linux, Local),
    n("sched_yield", Both, Local),
    n("send", Both, Local),
    n("sendmsg", Linux, Local),
    n("sendto", Both, Local),
    n("setattrlist", Mac, Refused(Errno::Perm)),
    n("setenv", Both, Local),
    n("setgid", Both, Refused(Errno::Perm)),
    n("setgroups", Both, Refused(Errno::Perm)),
    n("setpgid", Both, Refused(Errno::Perm)),
    n("setsid", Both, Refused(Errno::Perm)),
    n("setsockopt", Both, Local),
    n("setuid", Both, Refused(Errno::Perm)),
    n("shutdown", Both, Local),
    n("sigaction", Both, Local),
    n("sigaddset", Both, Own),
    n("sigaltstack", Both, Local),
    n("sigemptyset", Both, Own),
    n("signal", Both, Local),
    n("socket", Both, Asks(K::Socket)),
    n("socketpair", Both, Asks(K::Socket)),
    n("stat", Both, Asks(K::Look)),
    n("stat64", Linux, Asks(K::Look)),
    // `ENOSYS`, as on a kernel before 4.11: std then uses `fstatat64`.
    n("statx", Linux, Local),
    n("strerror_r", Both, Own),
    n("strlen", Both, Own),
    n("strnlen", Both, Own),
    n("symlink", Both, Refused(Errno::Perm)),
    // By number: `futex` is the process's own; every other number is `ENOSYS`.
    n("syscall", Linux, Local),
    n("sysconf", Both, Local),
    n("sysctlbyname", Mac, Local),
    n("sysdir_get_next_search_path_enumeration", Mac, Local),
    n("sysdir_start_search_path_enumeration", Mac, Local),
    n("unlink", Both, Asks(K::Remove)),
    n("unlinkat", Both, Asks(K::Remove)),
    n("unsetenv", Both, Local),
    n("utimensat", Linux, Refused(Errno::Perm)),
    n("waitid", Linux, Local),
    n("waitpid", Both, Local),
    n("write", Both, Asks(K::Write)),
    n("writev", Both, Asks(K::Write)),
];
