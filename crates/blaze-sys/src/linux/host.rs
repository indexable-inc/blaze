//! What the process asks of its host, by system call: its ids, its group and session, the
//! system's names (`uname`), resource limits (`prlimit64`) and CPU affinity. Each is the
//! kernel's own call, which the C library's function of the same name wraps; `posix::thread`
//! calls these on Linux.
//!
//! The process's id is [`super::getpid`], the calling thread's [`super::sample::gettid`], and a
//! signal to one thread (`pthread_kill`'s kernel form) `super::signal::tgkill`; they are not
//! declared again here. What has no kernel form is not here either: pthreads and
//! their keys, the dynamic loader (`dlsym`, `dl_iterate_phdr`), the user database
//! (`getpwuid_r`) and `sysconf` (its page size and clock ticks are the auxiliary vector's
//! `AT_PAGESZ` and `AT_CLKTCK`, held by `crate::facts::Aux` and handed in).
//!
//! Layouts are the kernel's (`include/uapi/linux/utsname.h`, `include/uapi/linux/resource.h`,
//! `include/uapi/asm-generic/resource.h`); x86-64 and aarch64 agree on all of them.

use super::{nr, raw};
use crate::Errno;

// ---------------------------------------------------------------------------------------------
// Ids, groups and sessions.
// ---------------------------------------------------------------------------------------------

/// `getuid`: this process's real user id. Cannot fail.
///
/// replaces libc::getuid
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getuid() -> u32 {
    // SAFETY: the call takes nothing and cannot fail.
    (unsafe { raw::syscall0(nr::GETUID) }) as u32
}

/// `geteuid`: this process's effective user id. Cannot fail.
///
/// replaces libc::geteuid
#[blaze::noalloc]
#[blaze::nopanic]
pub fn geteuid() -> u32 {
    // SAFETY: the call takes nothing and cannot fail.
    (unsafe { raw::syscall0(nr::GETEUID) }) as u32
}

/// `getppid`: the parent's process id (1, or a subreaper's, once the parent has exited).
/// Cannot fail.
///
/// replaces libc::getppid
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getppid() -> i32 {
    // SAFETY: the call takes nothing and cannot fail.
    (unsafe { raw::syscall0(nr::GETPPID) }) as i32
}

/// `getpgid(pid)`: the process group of `pid` (0: this process). [`Errno::Srch`] where there is
/// no such process.
///
/// replaces libc::getpgid
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getpgid(pid: i32) -> Result<i32, Errno> {
    // SAFETY: the call takes a number and no memory.
    raw::check(unsafe { raw::syscall1(nr::GETPGID, pid as isize as usize) }).map(|group| group as i32)
}

/// `setpgid(pid, group)`: `pid` (0: this process) moved to the process group `group` (0: a new
/// one it leads). [`Errno::Srch`] where `pid` is no child of the caller, [`Errno::Acces`] where
/// that child has already exec'd, [`Errno::Perm`] across sessions. Safe between a fork and an
/// exec: one instruction, no memory.
///
/// replaces libc::setpgid
#[blaze::noalloc]
#[blaze::nopanic]
pub fn setpgid(pid: i32, group: i32) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall2(nr::SETPGID, pid as isize as usize, group as isize as usize) }).map(|_| ())
}

/// `setsid`: a new session and group led by this process, with no controlling terminal; its
/// id. [`Errno::Perm`] where it already leads a group. Safe between a fork and an exec.
///
/// replaces libc::setsid
#[blaze::noalloc]
#[blaze::nopanic]
pub fn setsid() -> Result<i32, Errno> {
    // SAFETY: the call takes nothing.
    raw::check(unsafe { raw::syscall0(nr::SETSID) }).map(|session| session as i32)
}

// ---------------------------------------------------------------------------------------------
// uname.
// ---------------------------------------------------------------------------------------------

/// `_UTSNAME_LENGTH` / `__NEW_UTS_LEN + 1` (linux/utsname.h): each field's bytes, its NUL
/// among them.
pub const UTSNAME_FIELD: usize = 65;

/// `struct new_utsname` (linux/utsname.h), glibc's and musl's `struct utsname`: six
/// NUL-terminated fields of 65 bytes. The field names are the C ones, so a port reads alike.
///
/// replaces libc::utsname
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Utsname {
    /// The kernel's name, `Linux`.
    pub sysname: [u8; UTSNAME_FIELD],
    /// The host's name in its UTS namespace.
    pub nodename: [u8; UTSNAME_FIELD],
    /// The kernel's release (`6.8.0-...`).
    pub release: [u8; UTSNAME_FIELD],
    /// The kernel's build string.
    pub version: [u8; UTSNAME_FIELD],
    /// The machine (`x86_64`, `aarch64`).
    pub machine: [u8; UTSNAME_FIELD],
    /// The NIS domain name (`(none)` where unset).
    pub domainname: [u8; UTSNAME_FIELD],
}

impl Utsname {
    /// All zero: the out-parameter [`uname`] fills.
    pub const ZERO: Utsname = Utsname {
        sysname: [0; UTSNAME_FIELD],
        nodename: [0; UTSNAME_FIELD],
        release: [0; UTSNAME_FIELD],
        version: [0; UTSNAME_FIELD],
        machine: [0; UTSNAME_FIELD],
        domainname: [0; UTSNAME_FIELD],
    };
}

// linux/utsname.h (`struct new_utsname`), x86-64 and aarch64 alike.
const _: () = {
    assert!(size_of::<Utsname>() == 390);
    assert!(align_of::<Utsname>() == 1);
    assert!(core::mem::offset_of!(Utsname, sysname) == 0);
    assert!(core::mem::offset_of!(Utsname, nodename) == 65);
    assert!(core::mem::offset_of!(Utsname, release) == 130);
    assert!(core::mem::offset_of!(Utsname, version) == 195);
    assert!(core::mem::offset_of!(Utsname, machine) == 260);
    assert!(core::mem::offset_of!(Utsname, domainname) == 325);
};

/// `uname`: the system's names (the kernel's `newuname` on both architectures).
/// [`Errno::Fault`] cannot happen for the local it writes, so in practice it does not fail.
///
/// replaces libc::uname
#[blaze::noalloc]
#[blaze::nopanic]
pub fn uname() -> Result<Utsname, Errno> {
    let mut name = Utsname::ZERO;
    // SAFETY: the kernel writes one `struct new_utsname` (390 bytes, the layout asserted
    // above) into the live local given, and keeps no pointer to it.
    raw::check(unsafe { raw::syscall1(nr::UNAME, (&raw mut name).addr()) })?;
    Ok(name)
}

// ---------------------------------------------------------------------------------------------
// Resource limits.
// ---------------------------------------------------------------------------------------------

/// A resource `prlimit64` limits (asm-generic/resource.h: the same numbers on x86-64 and
/// aarch64).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resource(pub u32);

impl Resource {
    /// `RLIMIT_CPU`: CPU time in seconds.
    ///
    /// replaces libc::RLIMIT_CPU
    pub const CPU: Resource = Resource(0);
    /// `RLIMIT_FSIZE`: the largest file the process may write.
    ///
    /// replaces libc::RLIMIT_FSIZE
    pub const FSIZE: Resource = Resource(1);
    /// `RLIMIT_DATA`: the data segment and private mappings.
    ///
    /// replaces libc::RLIMIT_DATA
    pub const DATA: Resource = Resource(2);
    /// `RLIMIT_STACK`: the main thread's stack.
    ///
    /// replaces libc::RLIMIT_STACK
    pub const STACK: Resource = Resource(3);
    /// `RLIMIT_CORE`: a core dump's size.
    ///
    /// replaces libc::RLIMIT_CORE
    pub const CORE: Resource = Resource(4);
    /// `RLIMIT_NPROC`: the user's processes.
    ///
    /// replaces libc::RLIMIT_NPROC
    pub const NPROC: Resource = Resource(6);
    /// `RLIMIT_NOFILE`: open descriptors (one more than the highest number).
    ///
    /// replaces libc::RLIMIT_NOFILE
    pub const NOFILE: Resource = Resource(7);
    /// `RLIMIT_MEMLOCK`: locked memory.
    ///
    /// replaces libc::RLIMIT_MEMLOCK
    pub const MEMLOCK: Resource = Resource(8);
    /// `RLIMIT_AS`: the address space.
    ///
    /// replaces libc::RLIMIT_AS
    pub const AS: Resource = Resource(9);
}

/// `RLIM64_INFINITY` (linux/resource.h): no limit, all ones.
///
/// replaces libc::RLIM_INFINITY
pub const RLIM_INFINITY: u64 = !0;

/// `struct rlimit64` (linux/resource.h), the same bytes as glibc's and musl's 64-bit
/// `struct rlimit`: the soft limit, then the hard one. The field names are the C ones.
///
/// replaces libc::rlimit
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct Rlimit {
    /// The soft limit: what the process is held to now.
    pub rlim_cur: u64,
    /// The hard limit: what an unprivileged process may raise the soft one to.
    pub rlim_max: u64,
}

// linux/resource.h (`struct rlimit64`), x86-64 and aarch64 alike.
const _: () = {
    assert!(size_of::<Rlimit>() == 16);
    assert!(align_of::<Rlimit>() == 8);
    assert!(core::mem::offset_of!(Rlimit, rlim_cur) == 0);
    assert!(core::mem::offset_of!(Rlimit, rlim_max) == 8);
};

/// `prlimit64(pid, resource, new, old)`: the limits of `resource` for process `pid` (0: this
/// one) as they were, and, with `new`, set. [`Errno::Perm`] for raising a hard limit without the
/// privilege, [`Errno::Inval`] for a soft limit above the hard one or an unknown resource.
/// The one call behind the C library's `getrlimit`, `setrlimit` and `prlimit`.
///
/// replaces libc::getrlimit, libc::setrlimit and libc::prlimit
#[blaze::noalloc]
#[blaze::nopanic]
pub fn prlimit64(pid: i32, resource: Resource, new: Option<&Rlimit>) -> Result<Rlimit, Errno> {
    let mut old = Rlimit::default();
    let new = match new {
        Some(limit) => core::ptr::from_ref(limit).addr(),
        None => 0,
    };
    // SAFETY: `new` is null or a live `struct rlimit64` the kernel reads once; `old` is a live
    // local it writes once; neither is kept.
    raw::check(unsafe {
        raw::syscall4(nr::PRLIMIT64, pid as isize as usize, resource.0 as usize, new, (&raw mut old).addr())
    })?;
    Ok(old)
}

// ---------------------------------------------------------------------------------------------
// CPU affinity.
// ---------------------------------------------------------------------------------------------

/// `CPU_SETSIZE` (sched.h): the CPUs a [`CpuSet`] holds, 1024.
///
/// replaces libc::CPU_SETSIZE
pub const CPU_SETSIZE: usize = 1024;

/// A set of CPUs, one bit each, [`CPU_SETSIZE`] of them: the kernel's `cpumask` as the
/// affinity calls pass it, and glibc's `cpu_set_t` (bits/cpu-set.h).
///
/// replaces libc::cpu_set_t
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct CpuSet {
    bits: [u64; CPU_SETSIZE / 64],
}

// bits/cpu-set.h: 1024 bits in `unsigned long`s, x86-64 and aarch64 alike.
const _: () = {
    assert!(size_of::<CpuSet>() == 128);
    assert!(align_of::<CpuSet>() == 8);
};

impl CpuSet {
    /// No CPU (`CPU_ZERO`).
    ///
    /// replaces libc::CPU_ZERO
    pub const EMPTY: CpuSet = CpuSet { bits: [0; CPU_SETSIZE / 64] };

    /// `cpu` added (`CPU_SET`); a CPU at or past [`CPU_SETSIZE`] is not, and is `false`.
    ///
    /// replaces libc::CPU_SET
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn insert(&mut self, cpu: usize) -> bool {
        let Some(word) = self.bits.get_mut(cpu / 64) else { return false };
        // `rotate_left` by `cpu % 64` (below 64) is the shift, with no check in it.
        *word |= 1u64.rotate_left((cpu % 64) as u32);
        true
    }

    /// Whether `cpu` is in the set (`CPU_ISSET`).
    ///
    /// replaces libc::CPU_ISSET
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn contains(&self, cpu: usize) -> bool {
        self.bits.get(cpu / 64).is_some_and(|word| word & 1u64.rotate_left((cpu % 64) as u32) != 0)
    }

    /// How many CPUs are in it (`CPU_COUNT`).
    ///
    /// replaces libc::CPU_COUNT
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn count(&self) -> u32 {
        let mut total = 0u32;
        for word in &self.bits {
            total = total.wrapping_add(word.count_ones());
        }
        total
    }
}

/// `sched_setaffinity(thread, set)`: the CPUs thread `thread` (0: the calling one; a
/// [`super::sample::gettid`] id) may run on. [`Errno::Inval`] where `set` holds no CPU the
/// system has online, [`Errno::Srch`] for no such thread.
///
/// replaces libc::sched_setaffinity
#[blaze::noalloc]
#[blaze::nopanic]
pub fn sched_setaffinity(thread: i32, set: &CpuSet) -> Result<(), Errno> {
    // SAFETY: `set` is a live `CpuSet` of the size given, which the kernel reads and does not
    // keep.
    raw::check(unsafe {
        raw::syscall3(nr::SCHED_SETAFFINITY, thread as isize as usize, size_of::<CpuSet>(), core::ptr::from_ref(set).addr())
    })
    .map(|_| ())
}

/// `sched_getaffinity(thread)`: the CPUs thread `thread` (0: the calling one) may run on.
/// [`Errno::Inval`] on a system with more than [`CPU_SETSIZE`] possible CPUs. The kernel
/// returns the bytes it wrote (its mask's size), not 0 as the C library does; the bytes past
/// them stay [`CpuSet::EMPTY`]'s.
///
/// replaces libc::sched_getaffinity
#[blaze::noalloc]
#[blaze::nopanic]
pub fn sched_getaffinity(thread: i32) -> Result<CpuSet, Errno> {
    let mut set = CpuSet::EMPTY;
    // SAFETY: `set` is a live `CpuSet` of the size given, which the kernel writes at most that
    // many bytes of and does not keep.
    raw::check(unsafe {
        raw::syscall3(nr::SCHED_GETAFFINITY, thread as isize as usize, size_of::<CpuSet>(), (&raw mut set).addr())
    })?;
    Ok(set)
}
