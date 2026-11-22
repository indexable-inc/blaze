//! Making and replacing processes (on Linux by system call, `crate::linux::task`; on macOS
//! through libSystem): `fork`, the `exec` calls, `chdir`, `pause`, Linux's `CLONE_*` bits and
//! `unshare`, macOS's `posix_spawn` family and its process's arguments (`_NSGetArgc` and the
//! rest), and Linux's `process_vm_readv`.
//!
//! On Linux every call is the kernel's (`crate::linux::task`, beside
//! `crate::linux::clone3_process` and `crate::linux::execveat`); the C library's variadic
//! `clone` and `execv` (which reads `environ`) are not offered there.

#[cfg(target_os = "macos")]
use core::ffi::c_char;
use core::ffi::c_int;

#[cfg(target_os = "macos")]
use super::{returned, said};
#[cfg(target_os = "macos")]
use crate::{Errno, Path, posix::types::pid_t};

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `pid_t fork(void)` (unistd.h): a child with a copy of this process and only the calling
    /// thread; 0 in the child, its pid in the parent, -1 and `errno`. Until it execs or exits
    /// the child may call only what is async-signal-safe.
    /// replaces libc::fork
    pub fn fork() -> pid_t;

    /// `int execve(const char *path, char *const argv[], char *const envp[])` (unistd.h): this
    /// process replaced by `path`; returns only when refused (-1 and `errno`).
    /// replaces libc::execve
    pub fn execve(path: *const c_char, argv: *const *const c_char, envp: *const *const c_char) -> c_int;

    /// `int execv(const char *path, char *const argv[])` (unistd.h): `execve` with this
    /// process's environment.
    /// replaces libc::execv
    pub fn execv(path: *const c_char, argv: *const *const c_char) -> c_int;

    /// `int chdir(const char *path)` (unistd.h): the working directory changed.
    /// replaces libc::chdir
    pub fn chdir(path: *const c_char) -> c_int;

    /// `int pause(void)` (unistd.h): sleeps until a signal's handler has run; always -1 with
    /// `EINTR`.
    /// replaces libc::pause
    pub fn pause() -> c_int;
}

/// The kernel's forms on Linux, by raw system call: `fork` (`clone(SIGCHLD)`: unsafe, its
/// child's limits in `crate::linux::task`'s doc), `execve` (with the environment handed over:
/// the replacement of `execv` too, as Linux has no `environ` here), `chdir`, `pause`.
/// replaces libc::fork
/// replaces libc::execve
/// replaces libc::execv
/// replaces libc::chdir
/// replaces libc::pause
#[cfg(target_os = "linux")]
pub use crate::linux::task::{chdir, execve, fork, pause};

/// `chdir`: the working directory made `path`. Used by a spawned child between its fork and
/// its exec (async-signal-safe: one call, no memory). On Linux the system call itself.
#[cfg(target_os = "linux")]
pub use crate::linux::task::chdir as change_dir;

/// `chdir`: the working directory made `path`. Used by a spawned child between its fork and
/// its exec (async-signal-safe: one call, no memory).
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn change_dir(path: Path<'_>) -> Result<(), Errno> {
    // SAFETY: `path` is a live NUL-terminated string the call reads and does not keep.
    said(unsafe { chdir(path.as_ptr().cast()) }).map(drop)
}

/// `CLONE_VM` (sched.h): the child shares the caller's memory.
/// replaces libc::CLONE_VM
#[cfg(target_os = "linux")]
pub const CLONE_VM: c_int = 0x100;

/// `CLONE_VFORK` (sched.h): the caller sleeps until the child execs or exits.
/// replaces libc::CLONE_VFORK
#[cfg(target_os = "linux")]
pub const CLONE_VFORK: c_int = 0x4000;

/// `CLONE_PIDFD` (sched.h): a pidfd of the child written to the parent-tid argument. The same
/// bit as `crate::linux::CloneFlags::PIDFD` (clone3's 64-bit form).
/// replaces libc::CLONE_PIDFD
#[cfg(target_os = "linux")]
pub const CLONE_PIDFD: c_int = 0x1000;

/// `CLONE_NEWUSER` (sched.h): a new user namespace (for `clone` and `unshare`).
/// replaces libc::CLONE_NEWUSER
#[cfg(target_os = "linux")]
pub const CLONE_NEWUSER: c_int = 0x1000_0000;

/// `CLONE_NEWNET` (sched.h): a new network namespace (for `clone` and `unshare`).
/// replaces libc::CLONE_NEWNET
#[cfg(target_os = "linux")]
pub const CLONE_NEWNET: c_int = 0x4000_0000;

// libc::clone (the C library's variadic `clone`: a child running `entry(arg)` on its own stack)
// is not offered on Linux: it is glibc's assembly around the system call, and the kernel's
// `clone3` is the call (`crate::linux::clone3_process` for a fork that runs code before its
// exec). Its caller, blaze-core/src/process/spawn.rs:492 (a `CLONE_VM | CLONE_VFORK` child),
// needs `clone3` with `CLONE_VM` and a stack switch, which is bstd::rt's; see lane N5's report.

/// `unshare` and `process_vm_readv` by system call, typed; [`Namespaces`] the namespaces an
/// unshare makes new (`CLONE_NEWUSER`, `CLONE_NEWNET`).
/// replaces libc::unshare
/// replaces libc::process_vm_readv
#[cfg(target_os = "linux")]
pub use crate::linux::task::{Namespaces, RemoteSlice, process_vm_readv, unshare};

/// `unshare(namespaces)`: the calling thread put in new namespaces of these kinds, the name
/// this module first gave [`unshare`]. The native proc-macro sandbox's step after its fork.
#[cfg(target_os = "linux")]
pub use crate::linux::task::unshare as unshare_namespaces;

/// `posix_spawnattr_t` (spawn.h, macOS): an opaque pointer `posix_spawnattr_init` fills.
/// replaces libc::posix_spawnattr_t
#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type posix_spawnattr_t = *mut core::ffi::c_void;

/// `posix_spawn_file_actions_t` (spawn.h, macOS): an opaque pointer
/// `posix_spawn_file_actions_init` fills.
/// replaces libc::posix_spawn_file_actions_t
#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type posix_spawn_file_actions_t = *mut core::ffi::c_void;

/// `POSIX_SPAWN_SETSIGDEF` (spawn.h, macOS): the signals of `posix_spawnattr_setsigdefault`
/// are reset to their defaults in the child.
/// replaces libc::POSIX_SPAWN_SETSIGDEF
#[cfg(target_os = "macos")]
pub const POSIX_SPAWN_SETSIGDEF: c_int = 0x0004;

/// `POSIX_SPAWN_SETSIGMASK` (spawn.h, macOS): the child starts with
/// `posix_spawnattr_setsigmask`'s mask.
/// replaces libc::POSIX_SPAWN_SETSIGMASK
#[cfg(target_os = "macos")]
pub const POSIX_SPAWN_SETSIGMASK: c_int = 0x0008;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `int posix_spawn(pid_t *pid, const char *path, const posix_spawn_file_actions_t *,
    /// const posix_spawnattr_t *, char *const argv[], char *const envp[])` (spawn.h): the
    /// program `path` started as a child; returns the error (no `errno`).
    /// replaces libc::posix_spawn
    pub fn posix_spawn(
        pid: *mut pid_t,
        path: *const c_char,
        file_actions: *const posix_spawn_file_actions_t,
        attr: *const posix_spawnattr_t,
        argv: *const *mut c_char,
        envp: *const *mut c_char,
    ) -> c_int;

    /// `int posix_spawnattr_init(posix_spawnattr_t *)` (spawn.h); returns the error.
    /// replaces libc::posix_spawnattr_init
    pub fn posix_spawnattr_init(attr: *mut posix_spawnattr_t) -> c_int;

    /// `int posix_spawnattr_destroy(posix_spawnattr_t *)` (spawn.h); returns the error.
    /// replaces libc::posix_spawnattr_destroy
    pub fn posix_spawnattr_destroy(attr: *mut posix_spawnattr_t) -> c_int;

    /// `int posix_spawnattr_setflags(posix_spawnattr_t *, short)` (spawn.h): `POSIX_SPAWN_*`.
    /// replaces libc::posix_spawnattr_setflags
    pub fn posix_spawnattr_setflags(attr: *mut posix_spawnattr_t, flags: core::ffi::c_short) -> c_int;

    /// `int posix_spawnattr_setsigdefault(posix_spawnattr_t *, const sigset_t *)` (spawn.h).
    /// replaces libc::posix_spawnattr_setsigdefault
    pub fn posix_spawnattr_setsigdefault(attr: *mut posix_spawnattr_t, signals: *const crate::posix::signal::sigset_t) -> c_int;

    /// `int posix_spawnattr_setsigmask(posix_spawnattr_t *, const sigset_t *)` (spawn.h).
    /// replaces libc::posix_spawnattr_setsigmask
    pub fn posix_spawnattr_setsigmask(attr: *mut posix_spawnattr_t, mask: *const crate::posix::signal::sigset_t) -> c_int;

    /// `int posix_spawn_file_actions_init(posix_spawn_file_actions_t *)` (spawn.h).
    /// replaces libc::posix_spawn_file_actions_init
    pub fn posix_spawn_file_actions_init(actions: *mut posix_spawn_file_actions_t) -> c_int;

    /// `int posix_spawn_file_actions_destroy(posix_spawn_file_actions_t *)` (spawn.h).
    /// replaces libc::posix_spawn_file_actions_destroy
    pub fn posix_spawn_file_actions_destroy(actions: *mut posix_spawn_file_actions_t) -> c_int;

    /// `int posix_spawn_file_actions_adddup2(posix_spawn_file_actions_t *, int fd, int newfd)`
    /// (spawn.h): the child's `newfd` is this process's `fd`.
    /// replaces libc::posix_spawn_file_actions_adddup2
    pub fn posix_spawn_file_actions_adddup2(actions: *mut posix_spawn_file_actions_t, fd: c_int, new_fd: c_int) -> c_int;
}

/// A `posix_spawn_file_actions_t`, initialized, destroyed when dropped: a spawn's descriptor
/// moves. macOS.
#[cfg(target_os = "macos")]
pub struct SpawnActions(posix_spawn_file_actions_t);

#[cfg(target_os = "macos")]
impl SpawnActions {
    /// `posix_spawn_file_actions_init`: an empty list. [`Errno::NoMem`].
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn new() -> Result<SpawnActions, Errno> {
        let mut actions: posix_spawn_file_actions_t = core::ptr::null_mut();
        // SAFETY: `actions` is a live pointer the call fills with its own list (libSystem's
        // allocation, freed by `posix_spawn_file_actions_destroy` in `Drop`).
        returned(unsafe { posix_spawn_file_actions_init(&raw mut actions) })?;
        Ok(SpawnActions(actions))
    }

    /// `posix_spawn_file_actions_adddup2`: the child's `to` made a copy of this process's
    /// `from`. [`Errno::BadF`] for a negative number.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn dup2(&mut self, from: crate::Fd, to: crate::Fd) -> Result<(), Errno> {
        // SAFETY: `self.0` is a list `init` made and `Drop` has not destroyed.
        returned(unsafe { posix_spawn_file_actions_adddup2(&raw mut self.0, from.raw(), to.raw()) })
    }

    /// The pointer `posix_spawn` takes.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn as_ptr(&self) -> *const posix_spawn_file_actions_t {
        &raw const self.0
    }
}

#[cfg(target_os = "macos")]
impl Drop for SpawnActions {
    fn drop(&mut self) {
        // SAFETY: the list `init` made, destroyed once, here; its only error is `EINVAL` for a
        // list never made, which this type cannot hold.
        _ = unsafe { posix_spawn_file_actions_destroy(&raw mut self.0) };
    }
}

/// A `posix_spawnattr_t`, initialized, destroyed when dropped: a spawn's flags and signal
/// sets. macOS.
#[cfg(target_os = "macos")]
pub struct SpawnAttr(posix_spawnattr_t);

#[cfg(target_os = "macos")]
impl SpawnAttr {
    /// `posix_spawnattr_init`: the defaults. [`Errno::NoMem`].
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn new() -> Result<SpawnAttr, Errno> {
        let mut attr: posix_spawnattr_t = core::ptr::null_mut();
        // SAFETY: as `SpawnActions::new`, for an attribute object.
        returned(unsafe { posix_spawnattr_init(&raw mut attr) })?;
        Ok(SpawnAttr(attr))
    }

    /// `posix_spawnattr_setflags`: `POSIX_SPAWN_*` bits (they fit a `short`).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn set_flags(&mut self, flags: core::ffi::c_short) -> Result<(), Errno> {
        // SAFETY: `self.0` is an object `init` made and `Drop` has not destroyed.
        returned(unsafe { posix_spawnattr_setflags(&raw mut self.0, flags) })
    }

    /// `posix_spawnattr_setsigdefault`: the signals reset to their defaults in the child (with
    /// [`POSIX_SPAWN_SETSIGDEF`]).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn set_signal_defaults(&mut self, signals: &crate::posix::signal::sigset_t) -> Result<(), Errno> {
        // SAFETY: as `set_flags`; `signals` is a live set the call copies.
        returned(unsafe { posix_spawnattr_setsigdefault(&raw mut self.0, signals) })
    }

    /// `posix_spawnattr_setsigmask`: the child's signal mask (with [`POSIX_SPAWN_SETSIGMASK`]).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn set_signal_mask(&mut self, mask: &crate::posix::signal::sigset_t) -> Result<(), Errno> {
        // SAFETY: as `set_signal_defaults`.
        returned(unsafe { posix_spawnattr_setsigmask(&raw mut self.0, mask) })
    }

    /// The pointer `posix_spawn` takes.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn as_ptr(&self) -> *const posix_spawnattr_t {
        &raw const self.0
    }
}

#[cfg(target_os = "macos")]
impl Drop for SpawnAttr {
    fn drop(&mut self) {
        // SAFETY: as `SpawnActions`' drop, for the attribute object.
        _ = unsafe { posix_spawnattr_destroy(&raw mut self.0) };
    }
}

/// `_NSGetArgc` (crt_externs.h), from the table of calls found at first use: the address of
/// this process's `argc`.
/// replaces libc::_NSGetArgc
#[cfg(target_os = "macos")]
pub use crate::macos::cold::_NSGetArgc;

/// `_NSGetArgv` (crt_externs.h), from the table of calls found at first use: the address of
/// this process's `argv`.
/// replaces libc::_NSGetArgv
#[cfg(target_os = "macos")]
pub use crate::macos::cold::_NSGetArgv;

/// `_NSGetEnviron` (crt_externs.h), from the table of calls found at first use: the address
/// of this process's `environ`, which a zygote sets for its child.
/// replaces libc::_NSGetEnviron
#[cfg(target_os = "macos")]
pub use crate::macos::cold::_NSGetEnviron;
