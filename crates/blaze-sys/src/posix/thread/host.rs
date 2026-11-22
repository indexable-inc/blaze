//! What the process asks of its host: its ids and its group and session (unistd.h), `uname`
//! (sys/utsname.h), resource limits and usage (sys/resource.h), CPU affinity on Linux
//! (sched.h), and on macOS `sysconf`, `confstr`, the user database (pwd.h), the `AT_*` numbers
//! (fcntl.h) and `sysctlbyname`. The page size and clock ticks are not looked up here: they
//! are `crate::facts::Aux`'s, handed in.
//!
//! On Linux every call here is the kernel's, by system call ([`crate::linux::host`],
//! [`crate::linux::getpid`]): no C-library symbol is referenced. What has no kernel form
//! (`sysconf`, `getpwuid_r` and `passwd`) is offered on macOS only. On macOS the functions are
//! libSystem's, declared here or through the cold table.

use core::ffi::c_int;

use crate::Errno;
use crate::posix::types::pid_t;
#[cfg(target_os = "macos")]
use crate::posix::types::uid_t;

#[cfg(target_os = "macos")]
use core::ffi::{c_char, c_long};

#[cfg(target_os = "macos")]
use crate::posix::signal::errno;
#[cfg(target_os = "macos")]
use crate::posix::types::{gid_t, size_t, time_t};

// ---------------------------------------------------------------------------------------------
// Ids, groups and sessions.
// ---------------------------------------------------------------------------------------------

/// `getpid`: this process's id. [`crate::linux::getpid`] (a system call) on Linux, the cold
/// table's `getpid` on macOS. Cannot fail.
///
/// replaces libc::getpid
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getpid() -> pid_t {
    #[cfg(target_os = "linux")]
    {
        crate::linux::getpid()
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `pid_t getpid(void)`: no arguments, no memory, cannot fail.
        unsafe { crate::macos::cold::getpid() }
    }
}

/// `geteuid`: this process's effective user id. The cold table's on macOS. Cannot fail.
///
/// replaces libc::geteuid
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn geteuid() -> uid_t {
    // SAFETY: `uid_t geteuid(void)`: no arguments, no memory, cannot fail.
    unsafe { crate::macos::cold::geteuid() }
}

/// `geteuid`, `getuid`, `getppid` on Linux: the system calls, which cannot fail.
///
/// replaces libc::geteuid, libc::getuid and libc::getppid (Linux)
/// replaces libc::getpgrp (blaze_sys::linux::host::getpgid(0) on Linux, the system call getpgrp is)
#[cfg(target_os = "linux")]
pub use crate::linux::host::{geteuid, getppid, getuid};

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `getppid` (unistd.h): the parent's process id (1, or a subreaper's, once the parent
    /// has exited). Cannot fail.
    ///
    /// replaces libc::getppid
    pub safe fn getppid() -> pid_t;

    /// `getuid` (unistd.h): this process's real user id. Cannot fail.
    ///
    /// replaces libc::getuid
    pub safe fn getuid() -> uid_t;

    /// `setpgid` (unistd.h): process `pid` (0: this one) moved to the group `group` (0: a new
    /// group led by it); -1 and `errno` on failure. macOS only: Linux's is
    /// [`crate::linux::host::setpgid`], typed.
    ///
    /// replaces libc::setpgid
    pub safe fn setpgid(pid: pid_t, group: pid_t) -> c_int;

    /// `setsid` (unistd.h): a new session and group led by this process, with no controlling
    /// terminal; its id, or -1 and `errno` (`EPERM` for a group leader). macOS only: Linux's is
    /// [`crate::linux::host::setsid`], typed.
    ///
    /// replaces libc::setsid
    pub safe fn setsid() -> pid_t;

    /// `sysconf` (unistd.h): a value of the system's configuration (`_SC_*`); -1 for a name
    /// it has no limit for (`errno` untouched) or an unknown name (`EINVAL`). macOS only:
    /// Linux has no such call. Its uses: `crate::facts::Aux::from_system()` (the page size and
    /// clock ticks) and `_SC_GETPW_R_SIZE_MAX` beside `getpwuid_r`.
    ///
    /// replaces libc::sysconf (macOS only; for `_SC_PAGESIZE` and `_SC_CLK_TCK` the replacement
    /// is `crate::facts::Aux`'s `page_size()` / `clock_ticks()`, which needs the runtime's `Aux`
    /// at the site: not for the port's first pass, leave such a site and list it)
    pub safe fn sysconf(name: c_int) -> c_long;
}

/// `setpgid(pid, group)`: `pid` (0: this process) moved to the process group `group` (0: a new
/// one it leads). [`Errno::Srch`] where `pid` is no child of the caller, [`Errno::Acces`]
/// where that child has already exec'd. Used by the spawn paths' own group (the child calls it
/// on itself, the parent too, whichever runs first). A system call on Linux.
///
/// replaces libc::setpgid (the typed form)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_process_group(pid: pid_t, group: pid_t) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::host::setpgid(pid, group)
    }
    #[cfg(target_os = "macos")]
    {
        if setpgid(pid, group) == 0 { Ok(()) } else { Err(errno()) }
    }
}

/// `setsid`: a new session led by this process; its id. [`Errno::Perm`] where it already
/// leads a group. Used between a fork and an exec: a system call on Linux, and on macOS it
/// reads only `errno`.
///
/// replaces libc::setsid (the typed form)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn new_session() -> Result<pid_t, Errno> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::host::setsid()
    }
    #[cfg(target_os = "macos")]
    {
        let id = setsid();
        if id < 0 { Err(errno()) } else { Ok(id) }
    }
}

// ---------------------------------------------------------------------------------------------
// sysconf and confstr (macOS).
// ---------------------------------------------------------------------------------------------

/// `_SC_PAGESIZE` (unistd.h): 29 on macOS.
///
/// replaces libc::_SC_PAGESIZE
#[cfg(target_os = "macos")]
pub const _SC_PAGESIZE: c_int = 29;

/// `_SC_CLK_TCK` (unistd.h): 3 on macOS.
///
/// replaces libc::_SC_CLK_TCK
#[cfg(target_os = "macos")]
pub const _SC_CLK_TCK: c_int = 3;

/// `_SC_GETPW_R_SIZE_MAX` (unistd.h): a buffer size `getpwuid_r` should do with, 71 on macOS.
///
/// replaces libc::_SC_GETPW_R_SIZE_MAX
#[cfg(target_os = "macos")]
pub const _SC_GETPW_R_SIZE_MAX: c_int = 71;

// The page size and the clock ticks a second are facts of the process, handed in: the
// runtime's `crate::facts::Aux` (`aux.page_size()`, `aux.clock_ticks()`), built once from the
// auxiliary vector on Linux and by `sysconf` in `Aux::from_system()` on macOS. No function
// here looks them up. `_SC_PAGESIZE` and `_SC_CLK_TCK` above stay for that one caller.

/// `_CS_DARWIN_USER_TEMP_DIR` (unistd.h, macOS): the user's own temporary directory
/// (`/var/folders/.../T/`).
///
/// replaces libc::_CS_DARWIN_USER_TEMP_DIR
#[cfg(target_os = "macos")]
pub const _CS_DARWIN_USER_TEMP_DIR: c_int = 65537;

/// `confstr(name, into, len)` (unistd.h): a configuration string, NUL-terminated and cut to
/// `len`, into `into`; the length it needs with its NUL, or 0 and `errno`. The cold table's
/// entry, re-exported.
///
/// replaces libc::confstr
#[cfg(target_os = "macos")]
pub use crate::macos::cold::confstr;

/// `sysctlbyname(name, old, old_len, new, new_len)` (sys/sysctl.h): a kernel value read into
/// `old` (its size in and out through `old_len`) and, with `new`, set. The cold table's entry,
/// re-exported; [`crate::macos::sysctl_u32`] reads a 32-bit one typed.
///
/// replaces libc::sysctlbyname
#[cfg(target_os = "macos")]
pub use crate::macos::cold::sysctlbyname;

/// `sysctlbyname` of a value of any size: its bytes into `into`, their count returned; `None`
/// where this macOS has no such name (`ENOENT`). [`Errno::NoMem`] where `into` is too small.
/// Used for `kern.bootsessionuuid`, `hw.memsize`, `machdep.cpu.brand_string` and the topology
/// counts.
///
/// replaces libc::sysctlbyname (the typed form)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn sysctl_bytes(name: &core::ffi::CStr, into: &mut [u8]) -> Result<Option<usize>, Errno> {
    let mut len = into.len();
    // SAFETY: `name` is a live NUL-terminated string; `into` is live memory of `len` bytes the
    // call writes at most `len` of, and `len` is written back; nothing new is set (null, 0).
    let said = unsafe {
        sysctlbyname(name.as_ptr(), into.as_mut_ptr().cast(), &raw mut len, core::ptr::null_mut(), 0)
    };
    if said != 0 {
        let errno = errno();
        return if errno == Errno::NoEnt { Ok(None) } else { Err(errno) };
    }
    Ok(Some(len))
}

// ---------------------------------------------------------------------------------------------
// uname.
// ---------------------------------------------------------------------------------------------

/// `struct utsname` (sys/utsname.h, Linux): the kernel's `struct new_utsname`, six
/// NUL-terminated fields of 65 bytes, as [`crate::linux::host::Utsname`] (its fields are `u8`,
/// not `c_char`: `.as_ptr().cast()` for a `CStr`).
///
/// replaces libc::utsname
#[cfg(target_os = "linux")]
pub type utsname = crate::linux::host::Utsname;

/// `struct utsname` (sys/utsname.h, macOS): five NUL-terminated fields of 256 bytes.
///
/// replaces libc::utsname
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct utsname {
    pub sysname: [c_char; 256],
    pub nodename: [c_char; 256],
    pub release: [c_char; 256],
    pub version: [c_char; 256],
    pub machine: [c_char; 256],
}

#[cfg(target_os = "macos")]
impl utsname {
    /// All zero: the out-parameter `uname` fills.
    pub const ZERO: utsname =
        utsname { sysname: [0; 256], nodename: [0; 256], release: [0; 256], version: [0; 256], machine: [0; 256] };
}

// sys/utsname.h, macOS (`_SYS_NAMELEN` 256).
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<utsname>() == 1280);
    assert!(align_of::<utsname>() == 1);
    assert!(core::mem::offset_of!(utsname, release) == 512);
    assert!(core::mem::offset_of!(utsname, machine) == 1024);
};

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `uname` (sys/utsname.h): the system's names into `name`; -1 and `errno` on failure.
    /// macOS only: Linux's is [`crate::linux::host::uname`], typed.
    ///
    /// replaces libc::uname
    pub fn uname(name: *mut utsname) -> c_int;
}

/// The system's names (`uname`): its kernel's name and release, the machine. A system call on
/// Linux. Used by the facts a proc-macro pin and the allocator's huge-page check read.
///
/// replaces libc::uname (the typed form)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn system_name() -> Result<utsname, Errno> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::host::uname()
    }
    #[cfg(target_os = "macos")]
    {
        let mut name = utsname::ZERO;
        // SAFETY: `uname` writes one `struct utsname` (bytes only) into the live local given.
        if unsafe { uname(&raw mut name) } != 0 {
            return Err(errno());
        }
        Ok(name)
    }
}

// ---------------------------------------------------------------------------------------------
// The user database (macOS: Linux has no kernel form; the home directory comes from the
// environment the caller is handed).
// ---------------------------------------------------------------------------------------------

/// `struct passwd` (pwd.h, macOS: the BSD one, with the change and expiry times and the
/// class). macOS only: the user database is the C library's, not the kernel's.
///
/// replaces libc::passwd
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct passwd {
    pub pw_name: *mut c_char,
    pub pw_passwd: *mut c_char,
    pub pw_uid: uid_t,
    pub pw_gid: gid_t,
    pub pw_change: time_t,
    pub pw_class: *mut c_char,
    pub pw_gecos: *mut c_char,
    pub pw_dir: *mut c_char,
    pub pw_shell: *mut c_char,
    pub pw_expire: time_t,
}

#[cfg(target_os = "macos")]
impl passwd {
    /// All null: the out-parameter `getpwuid_r` fills.
    pub const ZERO: passwd = passwd {
        pw_name: core::ptr::null_mut(),
        pw_passwd: core::ptr::null_mut(),
        pw_uid: 0,
        pw_gid: 0,
        pw_change: 0,
        pw_class: core::ptr::null_mut(),
        pw_gecos: core::ptr::null_mut(),
        pw_dir: core::ptr::null_mut(),
        pw_shell: core::ptr::null_mut(),
        pw_expire: 0,
    };
}

// pwd.h, macOS arm64.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<passwd>() == 72);
    assert!(align_of::<passwd>() == 8);
    assert!(core::mem::offset_of!(passwd, pw_uid) == 16);
    assert!(core::mem::offset_of!(passwd, pw_dir) == 48);
    assert!(core::mem::offset_of!(passwd, pw_shell) == 56);
};

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `getpwuid_r` (pwd.h): the user database's entry for `uid` into `entry`, its strings in
    /// `buffer` of `len` bytes, `*found` set to `entry` (or null where there is no such user);
    /// the error's number returned (`ERANGE`: a larger buffer is needed).
    ///
    /// replaces libc::getpwuid_r
    pub fn getpwuid_r(uid: uid_t, entry: *mut passwd, buffer: *mut c_char, len: size_t, found: *mut *mut passwd) -> c_int;

    /// `getpwuid` (pwd.h): the entry for `uid` in the library's own static storage, valid
    /// until the next call on any thread; null where there is none. Not thread-safe: prefer
    /// [`getpwuid_r`].
    ///
    /// replaces libc::getpwuid
    pub fn getpwuid(uid: uid_t) -> *mut passwd;
}

// ---------------------------------------------------------------------------------------------
// Resource limits and usage.
// ---------------------------------------------------------------------------------------------

/// `rlim_t` (sys/resource.h): a limit, 64 bits on every target blaze runs on.
///
/// replaces libc::rlim_t
pub type rlim_t = u64;

/// `struct rlimit` (sys/resource.h, Linux): the kernel's `struct rlimit64`, the soft limit and
/// the hard one, as [`crate::linux::host::Rlimit`].
///
/// replaces libc::rlimit
#[cfg(target_os = "linux")]
pub type rlimit = crate::linux::host::Rlimit;

/// `struct rlimit` (sys/resource.h, macOS): the soft limit and the hard one.
///
/// replaces libc::rlimit
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct rlimit {
    pub rlim_cur: rlim_t,
    pub rlim_max: rlim_t,
}

// sys/resource.h: two `rlim_t`s on macOS arm64 (Linux's is asserted in linux/host.rs).
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<rlimit>() == 16);
    assert!(align_of::<rlimit>() == 8);
    assert!(core::mem::offset_of!(rlimit, rlim_cur) == 0);
    assert!(core::mem::offset_of!(rlimit, rlim_max) == 8);
};

/// `RLIMIT_NOFILE` (sys/resource.h): open descriptors, 7 on Linux
/// ([`crate::linux::host::Resource::NOFILE`] is the typed one).
///
/// replaces libc::RLIMIT_NOFILE
#[cfg(target_os = "linux")]
pub const RLIMIT_NOFILE: c_int = 7;
/// `RLIMIT_NOFILE` (sys/resource.h): open descriptors, 8 on macOS.
///
/// replaces libc::RLIMIT_NOFILE
#[cfg(target_os = "macos")]
pub const RLIMIT_NOFILE: c_int = 8;

/// `RLIM_INFINITY` (sys/resource.h): no limit; all ones on Linux.
///
/// replaces libc::RLIM_INFINITY
#[cfg(target_os = "linux")]
pub use crate::linux::host::RLIM_INFINITY;
/// `RLIM_INFINITY` (sys/resource.h): no limit; `2^63 - 1` on macOS.
///
/// replaces libc::RLIM_INFINITY
#[cfg(target_os = "macos")]
pub const RLIM_INFINITY: rlim_t = 0x7fff_ffff_ffff_ffff;

/// `getrlimit` (sys/resource.h): the limits of `resource` (`RLIMIT_*`) into `limit`; -1 and
/// `errno` on failure. Through the cold table. macOS only: Linux's is
/// [`crate::linux::host::prlimit64`], typed.
///
/// # Safety
///
/// `limit` is writable for one `struct rlimit`.
///
/// replaces libc::getrlimit
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn getrlimit(resource: c_int, limit: *mut rlimit) -> c_int {
    // SAFETY: the caller's contract; the cold entry takes the `struct rlimit *` as `*mut c_void`.
    unsafe { crate::macos::cold::getrlimit(resource, limit.cast()) }
}

/// The soft and hard limits of `resource` (`RLIMIT_*`): `prlimit64(0, resource, NULL)` by
/// system call on Linux, `getrlimit` on macOS. [`Errno::Inval`] for an unknown resource. Used
/// by the rings and the store, which size their tables by [`RLIMIT_NOFILE`].
///
/// replaces libc::getrlimit (the typed form)
/// replaces libc::prlimit (blaze_sys::linux::host::prlimit64(pid, Resource, Option<&Rlimit>) on Linux, which this calls; macOS has no prlimit)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn resource_limit(resource: c_int) -> Result<rlimit, Errno> {
    #[cfg(target_os = "linux")]
    {
        let Ok(resource) = u32::try_from(resource) else { return Err(Errno::Inval) };
        crate::linux::host::prlimit64(0, crate::linux::host::Resource(resource), None)
    }
    #[cfg(target_os = "macos")]
    {
        let mut limit = rlimit::default();
        // SAFETY: `limit` is a live `struct rlimit` the call writes once.
        if unsafe { getrlimit(resource, &raw mut limit) } != 0 {
            return Err(errno());
        }
        Ok(limit)
    }
}

/// `RUSAGE_SELF` (sys/resource.h): every thread of the process, 0 on both systems.
/// [`crate::linux::Who::Process`] is Linux's typed form.
///
/// replaces libc::RUSAGE_SELF
pub const RUSAGE_SELF: c_int = 0;

/// `struct rusage` (sys/resource.h): Linux's is [`crate::linux::Rusage`] (the maximum
/// resident set in kilobytes).
///
/// replaces libc::rusage
#[cfg(target_os = "linux")]
pub type rusage = crate::linux::Rusage;

/// `struct rusage` (sys/resource.h): macOS's is [`crate::macos::Rusage`] (the maximum
/// resident set in bytes).
///
/// replaces libc::rusage
#[cfg(target_os = "macos")]
pub type rusage = crate::macos::Rusage;

/// What the whole process used since it began, every thread's: `getrusage(RUSAGE_SELF)`, by
/// system call on Linux ([`crate::linux::getrusage`]) and through the cold table on macOS.
///
/// replaces libc::getrusage(libc::RUSAGE_SELF, ..) (the typed form)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn process_rusage() -> Result<rusage, Errno> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::getrusage(crate::linux::Who::Process)
    }
    #[cfg(target_os = "macos")]
    {
        let mut usage = rusage::default();
        // SAFETY: `getrusage` writes one `struct rusage` (`crate::macos::Rusage`, its layout,
        // checked against libc's in macos.rs's tests) into the live local given.
        if unsafe { crate::macos::cold::getrusage(RUSAGE_SELF, (&raw mut usage).cast()) } != 0 {
            return Err(errno());
        }
        Ok(usage)
    }
}

// ---------------------------------------------------------------------------------------------
// CPU affinity (Linux).
// ---------------------------------------------------------------------------------------------

/// `CPU_SETSIZE` (sched.h): the CPUs a [`cpu_set_t`] holds, 1024.
///
/// replaces libc::CPU_SETSIZE
#[cfg(target_os = "linux")]
pub use crate::linux::host::CPU_SETSIZE;

/// `cpu_set_t` (sched.h): a set of CPUs, one bit each, [`CPU_SETSIZE`] of them, as
/// [`crate::linux::host::CpuSet`] (`insert`, `contains`, `count`, `EMPTY`).
///
/// replaces libc::cpu_set_t
#[cfg(target_os = "linux")]
pub type cpu_set_t = crate::linux::host::CpuSet;

/// `CPU_SET(cpu, set)` (sched.h) as a function, for a port that keeps the C form; a CPU past
/// [`CPU_SETSIZE`] is not added. [`cpu_set_t::insert`] is the typed form.
///
/// replaces libc::CPU_SET
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn CPU_SET(cpu: usize, set: &mut cpu_set_t) {
    set.insert(cpu);
}

/// The calling thread may run only on the CPUs of `set`: `sched_setaffinity(0)` by system
/// call. [`Errno::Inval`] where `set` holds no CPU the system has online. Used by the pool's
/// pinned workers.
///
/// replaces libc::sched_setaffinity (the typed form)
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_affinity(set: &cpu_set_t) -> Result<(), Errno> {
    crate::linux::host::sched_setaffinity(0, set)
}

/// The CPUs the calling thread may run on: `sched_getaffinity(0)` by system call.
/// [`Errno::Inval`] on a system with more than [`CPU_SETSIZE`] CPUs. Used by the topology's
/// count of usable CPUs.
///
/// replaces libc::sched_getaffinity (the typed form)
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn affinity() -> Result<cpu_set_t, Errno> {
    crate::linux::host::sched_getaffinity(0)
}

// ---------------------------------------------------------------------------------------------
// The `*at` calls' numbers on macOS (Linux's are `crate::linux::CWD` and `AtFlags`).
// ---------------------------------------------------------------------------------------------

/// `AT_FDCWD` (fcntl.h, macOS): the current directory as an `*at` call's directory, -2.
/// Linux's is [`crate::linux::CWD`].
///
/// replaces libc::AT_FDCWD
#[cfg(target_os = "macos")]
pub const AT_FDCWD: c_int = -2;

/// `AT_SYMLINK_NOFOLLOW` (fcntl.h, macOS): a symbolic link itself. Linux's is
/// `crate::linux::AtFlags::NO_FOLLOW`.
///
/// replaces libc::AT_SYMLINK_NOFOLLOW
#[cfg(target_os = "macos")]
pub const AT_SYMLINK_NOFOLLOW: c_int = 0x20;

/// `AT_SYMLINK_FOLLOW` (fcntl.h, macOS): `linkat` links what a link names. Linux's is
/// `crate::linux::AtFlags::FOLLOW`.
///
/// replaces libc::AT_SYMLINK_FOLLOW
#[cfg(target_os = "macos")]
pub const AT_SYMLINK_FOLLOW: c_int = 0x40;

/// `AT_REMOVEDIR` (fcntl.h, macOS): `unlinkat` removes an empty directory. Linux's is
/// `crate::linux::Unlink::Dir`.
///
/// replaces libc::AT_REMOVEDIR
#[cfg(target_os = "macos")]
pub const AT_REMOVEDIR: c_int = 0x80;
