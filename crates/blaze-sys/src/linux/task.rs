//! A process's own calls by raw system call, so that `posix::proc` names no C-library symbol on
//! Linux (lane N5): a child's wait, `fork`, `execve`, `chdir`, `pause`, `unshare`,
//! `process_vm_readv`, a file's status (`newfstatat` and the kernel's `struct stat`), `statfs`,
//! `mknodat`, `fchownat`, `realpath` (through `/proc/self/fd`), the extended attributes,
//! `fadvise64`, `mount` and `umount2`, priorities, and `sysinfo`'s load averages.
//!
//! Each item says the C function it replaces (`/// replaces libc::<name>`); `posix::proc`
//! re-exports or calls these on Linux. What linux/ already had is used, not declared again:
//! `wait4_now` and `waitid` (a reap with its usage, a look by pidfd), `clone3_process`,
//! `execveat`, `statx`, `fstatfs`, `openat`, `readlinkat`, `ppoll`, and `fdctl::fallocate`
//! (`posix_fallocate` is it with `FallocateFlags::NONE`).
//!
//! # `fork` by system call, in a process that also has glibc
//!
//! [`fork`] is the kernel's `clone(SIGCHLD)`. glibc's `fork()` does more around the same call,
//! and the child of this one gets none of it:
//!
//! - no `pthread_atfork` handler runs, the program's or glibc's own. glibc's take its locks
//!   in the parent and remake them in the child: malloc's arenas, the stdio list, the dynamic
//!   loader's lock. With one thread in the parent no other thread can hold one, so nothing is
//!   lost; in a parent with other threads the child may hold a lock whose owner is gone, and
//!   malloc, stdio and `dlopen` can hang in it (after glibc's own `fork` malloc works there).
//! - the child's thread descriptor keeps the PARENT's thread id: glibc's `fork` has the kernel
//!   write the child's id into `THREAD_SELF->tid` (`CLONE_CHILD_SETTID`), this one does not.
//!   So in the child `raise`, `abort` and `pthread_kill(pthread_self())` signal a thread id
//!   that is not the child's (`tgkill` fails `ESRCH`; `abort` then ends by its fallback, not
//!   `SIGABRT`), and a PI or robust mutex records the wrong owner.
//! - glibc's pid cache: gone since glibc 2.25 (`getpid` is the system call), so nothing to
//!   reset there; older glibc would answer the parent's pid in the child.
//! - glibc's thread list and count (`__libc_single_threaded`, the stacks of the other threads)
//!   still name the parent's threads.
//!
//! So until it execs or exits the child does only what is async-signal-safe and touches no C
//! library state: this crate's system calls, `_exit`. The tree's callers of `libc::fork` and
//! whether each fits that are in lane N5's report.

use core::ffi::CStr;

use super::{
    AtFlags, CWD, Cloned, IoSliceMut, OpenFlags, Statfs, WaitOptions,
    fs::arg,
    nr, raw,
};
use crate::{Errno, Fd, Path};

/// `SIGCHLD` (asm-generic/signal.h and x86's: 17 on both architectures): the signal a forked
/// child sends its parent when it ends.
const SIGCHLD: usize = 17;

// ---- a child's wait ---------------------------------------------------------------------

/// `waitpid(pid, options)` as `wait4(pid, &status, options, NULL)`: the child that ended and
/// its wait status (read with `posix::proc`'s `W*` macros), or `None` where
/// [`WaitOptions::NO_HANG`] found none ended. `pid` > 0 is one child, -1 any, 0 the caller's
/// group. Of the options `wait4` takes [`WaitOptions::NO_HANG`]; the others are `waitid`'s
/// and the kernel refuses them with [`Errno::Inval`]. [`Errno::Child`] where there is no such
/// child; [`Errno::Intr`] is the caller's to retry.
/// replaces libc::waitpid
#[blaze::noalloc]
#[blaze::nopanic]
pub fn waitpid(pid: i32, options: WaitOptions) -> Result<Option<(i32, i32)>, Errno> {
    let mut status = 0i32;
    // SAFETY: `status` is a live `int` the kernel writes and does not keep; no `rusage` is
    // asked for (null).
    let reaped = raw::check(unsafe {
        raw::syscall4(
            nr::WAIT4,
            pid as isize as usize,
            core::ptr::from_mut(&mut status) as usize,
            options.bits() as usize,
            0,
        )
    })?;
    Ok(match reaped {
        0 => None,
        reaped => Some((reaped as i32, status)),
    })
}

// ---- making and replacing a process -----------------------------------------------------

/// `clone(SIGCHLD)`, the kernel's fork: a child with a copy of this process's memory and only
/// the calling thread, that sends `SIGCHLD` when it ends. [`Cloned::Child`] in the child,
/// [`Cloned::Parent`] with its pid in the caller. [`Errno::Again`] at the process limit,
/// [`Errno::NoMem`].
///
/// # Safety
///
/// The child runs no `pthread_atfork` handler and keeps glibc's thread descriptor of the
/// parent's thread (the module's doc): until it execs or exits it calls only what is
/// async-signal-safe and touches no C-library state (no allocator, no stdio, no `abort`).
/// replaces libc::fork
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn fork() -> Result<Cloned, Errno> {
    // `clone`'s arguments after the flags (stack, parent tid, child tid and tls, in an order
    // that differs by architecture) are all 0: no new stack, nothing written, no new thread
    // pointer, so the order does not matter.
    // SAFETY: no `CLONE_VM` and no stack: the child returns here on a copy of this stack and
    // this memory; the kernel reads no pointer. The caller answers for the child (above).
    let pid = raw::check(unsafe { raw::syscall5(nr::CLONE, SIGCHLD, 0, 0, 0, 0) })?;
    Ok(match pid {
        0 => Cloned::Child,
        pid => Cloned::Parent(pid as i32),
    })
}

/// `execve(path, argv, envp)`: this process replaced by the program `path`, given `argv` and
/// exactly the environment `envp` (nothing of this process's: there is no `environ` here, so
/// `execv`'s callers hand over the environment they mean). Made as `execveat(AT_FDCWD, path,
/// argv, envp, 0)`, which the kernel runs as `execve` (the path taken as it is). It returns
/// only when refused, with why.
/// replaces libc::execve
/// replaces libc::execv
#[blaze::noalloc]
#[blaze::nopanic]
pub fn execve(path: Path<'_>, argv: super::Args<'_>, envp: super::Args<'_>) -> Errno {
    super::execveat(CWD, path, argv, envp, AtFlags::NONE)
}

/// `chdir(path)`: the working directory made `path`. Async-signal-safe (one call, no memory):
/// a spawned child's step between its fork and its exec. [`Errno::NoEnt`],
/// [`Errno::NotDir`], [`Errno::Acces`].
/// replaces libc::chdir
#[blaze::noalloc]
#[blaze::nopanic]
pub fn chdir(path: Path<'_>) -> Result<(), Errno> {
    // SAFETY: `path` is a live NUL-terminated string the kernel reads and does not keep.
    raw::check(unsafe { raw::syscall1(nr::CHDIR, path.as_ptr() as usize) }).map(|_| ())
}

/// `pause()` as `ppoll` of no descriptor with no time limit and the thread's own mask: sleeps
/// until a signal's handler has run, and returns [`Errno::Intr`] (a signal that is ignored or
/// blocked does not end it). aarch64 has no `pause` call.
/// replaces libc::pause
#[blaze::noalloc]
#[blaze::nopanic]
pub fn pause() -> Errno {
    match super::ppoll(&mut [], None, None) {
        Err(errno) => errno,
        // No descriptor and no time limit: the call ends only by a signal.
        Ok(_) => Errno::Intr,
    }
}

flag_set! {
    /// The namespaces an [`unshare`] makes new (`CLONE_NEW*`, sched.h).
    Namespaces: u32 {
        /// `CLONE_NEWUSER`: a new user namespace.
        USER = 0x1000_0000;
        /// `CLONE_NEWNET`: a new network namespace.
        NET = 0x4000_0000;
    }
}

/// `unshare(namespaces)`: the calling thread put in new namespaces of these kinds.
/// [`Errno::Perm`] where unprivileged user namespaces are off; [`Errno::Inval`]. The native
/// proc-macro sandbox's step after its fork.
/// replaces libc::unshare
#[blaze::noalloc]
#[blaze::nopanic]
pub fn unshare(namespaces: Namespaces) -> Result<(), Errno> {
    // SAFETY: the call takes a number and no memory.
    raw::check(unsafe { raw::syscall1(nr::UNSHARE, namespaces.bits() as usize) }).map(|_| ())
}

/// `struct iovec` (linux/uio.h) over another process's memory: an address there and a length.
/// Not a borrow of this process's bytes, so the kernel alone reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct RemoteSlice {
    /// `iov_base`: the address in the other process.
    pub base: usize,
    /// `iov_len`: how many bytes.
    pub len: usize,
}

// linux/uio.h, 64-bit: `struct iovec` is a pointer and a `size_t`, 16 bytes.
const _: () = {
    assert!(size_of::<RemoteSlice>() == 16);
    assert!(align_of::<RemoteSlice>() == 8);
    assert!(core::mem::offset_of!(RemoteSlice, base) == 0);
    assert!(core::mem::offset_of!(RemoteSlice, len) == 8);
};

/// `process_vm_readv(pid, local, remote, 0)`: bytes of process `pid` (this one too) at
/// `remote` copied into `local`; how many. An unmapped address is [`Errno::Fault`], never a
/// fault of this process: async-signal-safe, a crash handler's read of a stack.
/// [`Errno::Srch`], [`Errno::Perm`].
/// replaces libc::process_vm_readv
#[blaze::noalloc]
#[blaze::nopanic]
pub fn process_vm_readv(pid: i32, local: &mut [IoSliceMut<'_>], remote: &[RemoteSlice]) -> Result<usize, Errno> {
    // SAFETY: `local` is `local.len()` live `iovec`s, each over live room this process lent
    // for writing; `remote` is `remote.len()` live `iovec`s the kernel reads and checks against
    // the other process's mappings; it keeps neither.
    raw::check(unsafe {
        raw::syscall6(
            nr::PROCESS_VM_READV,
            pid as isize as usize,
            local.as_mut_ptr() as usize,
            local.len(),
            remote.as_ptr() as usize,
            remote.len(),
            0,
        )
    })
}

// ---- a file's status ---------------------------------------------------------------------

/// The kernel's `struct stat` (arch/x86/include/uapi/asm/stat.h, x86-64): what `newfstatat`
/// fills. The kernel names the times `__kernel_ulong_t`; glibc's `struct stat` has the same
/// bytes with signed times, which these fields keep (`st_*` names, so a port is a change of
/// path).
/// replaces libc::stat
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Stat {
    /// `st_dev`: the device the file is on.
    pub st_dev: u64,
    /// `st_ino`: its inode.
    pub st_ino: u64,
    /// `st_nlink`: its links.
    pub st_nlink: u64,
    /// `st_mode`: its kind (`S_IFMT`) and permissions.
    pub st_mode: u32,
    /// `st_uid`: its owner.
    pub st_uid: u32,
    /// `st_gid`: its group.
    pub st_gid: u32,
    __pad0: u32,
    /// `st_rdev`: the device a device file is.
    pub st_rdev: u64,
    /// `st_size`: its length in bytes.
    pub st_size: i64,
    /// `st_blksize`: the best transfer size.
    pub st_blksize: i64,
    /// `st_blocks`: 512-byte blocks held.
    pub st_blocks: i64,
    /// `st_atime`: last read, seconds.
    pub st_atime: i64,
    /// Its nanoseconds.
    pub st_atime_nsec: i64,
    /// `st_mtime`: last written, seconds.
    pub st_mtime: i64,
    /// Its nanoseconds.
    pub st_mtime_nsec: i64,
    /// `st_ctime`: last changed (contents or status), seconds.
    pub st_ctime: i64,
    /// Its nanoseconds.
    pub st_ctime_nsec: i64,
    __unused: [i64; 3],
}

// arch/x86/include/uapi/asm/stat.h (`__x86_64__`): 144 bytes, the offsets glibc's
// bits/struct_stat.h has too.
#[cfg(target_arch = "x86_64")]
const _: () = {
    assert!(size_of::<Stat>() == 144);
    assert!(align_of::<Stat>() == 8);
    assert!(core::mem::offset_of!(Stat, st_dev) == 0);
    assert!(core::mem::offset_of!(Stat, st_ino) == 8);
    assert!(core::mem::offset_of!(Stat, st_nlink) == 16);
    assert!(core::mem::offset_of!(Stat, st_mode) == 24);
    assert!(core::mem::offset_of!(Stat, st_uid) == 28);
    assert!(core::mem::offset_of!(Stat, st_gid) == 32);
    assert!(core::mem::offset_of!(Stat, st_rdev) == 40);
    assert!(core::mem::offset_of!(Stat, st_size) == 48);
    assert!(core::mem::offset_of!(Stat, st_blksize) == 56);
    assert!(core::mem::offset_of!(Stat, st_blocks) == 64);
    assert!(core::mem::offset_of!(Stat, st_atime) == 72);
    assert!(core::mem::offset_of!(Stat, st_atime_nsec) == 80);
    assert!(core::mem::offset_of!(Stat, st_mtime) == 88);
    assert!(core::mem::offset_of!(Stat, st_mtime_nsec) == 96);
    assert!(core::mem::offset_of!(Stat, st_ctime) == 104);
    assert!(core::mem::offset_of!(Stat, st_ctime_nsec) == 112);
};

/// The kernel's `struct stat` (include/uapi/asm-generic/stat.h, aarch64): what `newfstatat`
/// fills, the same bytes as glibc's aarch64 `struct stat` (`st_*` names, so a port is a
/// change of path).
/// replaces libc::stat
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Stat {
    /// `st_dev`: the device the file is on.
    pub st_dev: u64,
    /// `st_ino`: its inode.
    pub st_ino: u64,
    /// `st_mode`: its kind (`S_IFMT`) and permissions.
    pub st_mode: u32,
    /// `st_nlink`: its links.
    pub st_nlink: u32,
    /// `st_uid`: its owner.
    pub st_uid: u32,
    /// `st_gid`: its group.
    pub st_gid: u32,
    /// `st_rdev`: the device a device file is.
    pub st_rdev: u64,
    __pad1: u64,
    /// `st_size`: its length in bytes.
    pub st_size: i64,
    /// `st_blksize`: the best transfer size.
    pub st_blksize: i32,
    __pad2: i32,
    /// `st_blocks`: 512-byte blocks held.
    pub st_blocks: i64,
    /// `st_atime`: last read, seconds.
    pub st_atime: i64,
    /// Its nanoseconds.
    pub st_atime_nsec: i64,
    /// `st_mtime`: last written, seconds.
    pub st_mtime: i64,
    /// Its nanoseconds.
    pub st_mtime_nsec: i64,
    /// `st_ctime`: last changed (contents or status), seconds.
    pub st_ctime: i64,
    /// Its nanoseconds.
    pub st_ctime_nsec: i64,
    __unused: [u32; 2],
}

// include/uapi/asm-generic/stat.h (64-bit): 128 bytes; these offsets.
#[cfg(target_arch = "aarch64")]
const _: () = {
    assert!(size_of::<Stat>() == 128);
    assert!(align_of::<Stat>() == 8);
    assert!(core::mem::offset_of!(Stat, st_dev) == 0);
    assert!(core::mem::offset_of!(Stat, st_ino) == 8);
    assert!(core::mem::offset_of!(Stat, st_mode) == 16);
    assert!(core::mem::offset_of!(Stat, st_nlink) == 20);
    assert!(core::mem::offset_of!(Stat, st_uid) == 24);
    assert!(core::mem::offset_of!(Stat, st_gid) == 28);
    assert!(core::mem::offset_of!(Stat, st_rdev) == 32);
    assert!(core::mem::offset_of!(Stat, st_size) == 48);
    assert!(core::mem::offset_of!(Stat, st_blksize) == 56);
    assert!(core::mem::offset_of!(Stat, st_blocks) == 64);
    assert!(core::mem::offset_of!(Stat, st_atime) == 72);
    assert!(core::mem::offset_of!(Stat, st_atime_nsec) == 80);
    assert!(core::mem::offset_of!(Stat, st_mtime) == 88);
    assert!(core::mem::offset_of!(Stat, st_mtime_nsec) == 96);
    assert!(core::mem::offset_of!(Stat, st_ctime) == 104);
    assert!(core::mem::offset_of!(Stat, st_ctime_nsec) == 112);
};

impl Stat {
    /// All zero, for a call to fill (in place of `mem::zeroed`).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn zeroed() -> Stat {
        // SAFETY: every field is an integer or an array of them, for which zero bytes are valid.
        unsafe { core::mem::zeroed() }
    }
}

/// `newfstatat(dir, path, flags)`: the status of `path` under `dir`
/// ([`AtFlags::NO_FOLLOW`] for a link itself; [`AtFlags::EMPTY_PATH`] with [`Path::EMPTY`]
/// for `dir` itself). [`Errno::NoEnt`], [`Errno::NotDir`], [`Errno::Acces`].
/// replaces libc::fstatat
/// replaces libc::stat
/// replaces libc::lstat
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fstatat(dir: Fd, path: Path<'_>, flags: AtFlags) -> Result<Stat, Errno> {
    let mut status = Stat::zeroed();
    // SAFETY: `path` is a live NUL-terminated string the kernel reads; `status` is a live
    // kernel `struct stat` (the layout asserted above) it writes; it keeps neither.
    raw::check(unsafe {
        raw::syscall4(
            nr::NEWFSTATAT,
            arg(dir),
            path.as_ptr() as usize,
            core::ptr::from_mut(&mut status) as usize,
            flags.bits() as usize,
        )
    })?;
    Ok(status)
}

/// `fstat(fd)` as `newfstatat(fd, "", AT_EMPTY_PATH)`: the status of the open file `fd` (one
/// call number on both architectures). [`Errno::BadF`].
/// replaces libc::fstat
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fstat(fd: Fd) -> Result<Stat, Errno> {
    fstatat(fd, Path::EMPTY, AtFlags::EMPTY_PATH)
}

/// `statfs(path)`: the file system `path` is on, the kernel's `struct statfs` (the same type
/// [`super::fstatfs`] fills). [`Errno::NoEnt`].
/// replaces libc::statfs
#[blaze::noalloc]
#[blaze::nopanic]
pub fn statfs(path: Path<'_>) -> Result<Statfs, Errno> {
    let mut stat = Statfs::ZERO;
    // SAFETY: `path` is a live NUL-terminated string the kernel reads; `stat` is a live `struct
    // statfs` (integers only, any bytes valid) it writes; it keeps neither.
    raw::check(unsafe {
        raw::syscall2(nr::STATFS, path.as_ptr() as usize, core::ptr::from_mut(&mut stat) as usize)
    })?;
    Ok(stat)
}

// ---- names, owners, links ----------------------------------------------------------------

/// `S_IFIFO` (linux/stat.h): the kind bits of a named pipe.
const S_IFIFO: u32 = 0o010_000;

/// `mknodat(dir, path, mode, dev)`: a node made at `path` under `dir`, of the kind and
/// permissions `mode` says (`S_IFIFO | 0o600`: a named pipe; a device kind takes `dev` and
/// privilege). [`Errno::Exist`].
/// replaces libc::mknodat
/// replaces libc::mknod
#[blaze::noalloc]
#[blaze::nopanic]
pub fn mknodat(dir: Fd, path: Path<'_>, mode: u32, dev: u32) -> Result<(), Errno> {
    // SAFETY: `path` is a live NUL-terminated string the kernel reads and does not keep.
    raw::check(unsafe {
        raw::syscall4(nr::MKNODAT, arg(dir), path.as_ptr() as usize, mode as usize, dev as usize)
    })
    .map(|_| ())
}

/// `mkfifo(path, mode)` as `mknodat(AT_FDCWD, path, S_IFIFO | mode, 0)`: a named pipe made at
/// `path` with permissions `mode` (less the umask). [`Errno::Exist`].
/// replaces libc::mkfifo
#[blaze::noalloc]
#[blaze::nopanic]
pub fn mkfifo(path: Path<'_>, mode: u32) -> Result<(), Errno> {
    mknodat(CWD, path, S_IFIFO | (mode & 0o7777), 0)
}

/// `fchownat(dir, path, owner, group, flags)`: the owner and group of `path` under `dir` set
/// (with [`AtFlags::EMPTY_PATH`] and [`Path::EMPTY`], `dir` itself; [`AtFlags::NO_FOLLOW`] a
/// link itself); `u32::MAX` (-1) for either leaves it as it is. [`Errno::Perm`].
/// replaces libc::fchownat
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fchownat(dir: Fd, path: Path<'_>, owner: u32, group: u32, flags: AtFlags) -> Result<(), Errno> {
    // SAFETY: `path` is a live NUL-terminated string the kernel reads and does not keep.
    raw::check(unsafe {
        raw::syscall5(
            nr::FCHOWNAT,
            arg(dir),
            path.as_ptr() as usize,
            owner as usize,
            group as usize,
            flags.bits() as usize,
        )
    })
    .map(|_| ())
}

/// `fchown(fd, owner, group)` as `fchownat(fd, "", owner, group, AT_EMPTY_PATH)`: the open
/// file's owner and group set; `u32::MAX` leaves one as it is. [`Errno::Perm`].
/// replaces libc::fchown
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fchown(fd: Fd, owner: u32, group: u32) -> Result<(), Errno> {
    fchownat(fd, Path::EMPTY, owner, group, AtFlags::EMPTY_PATH)
}

/// Which step of a [`realpath`] failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RealPathError {
    /// The open of the path itself (`openat(O_PATH)`): [`Errno::NoEnt`], [`Errno::NotDir`],
    /// [`Errno::Acces`], [`Errno::Loop`].
    Path(Errno),
    /// The read of its link under `/proc/self/fd`: [`Errno::NoEnt`] where procfs is not
    /// mounted (a sandbox without `/proc`), [`Errno::NameTooLong`] where the name does not fit
    /// the room given.
    Proc(Errno),
}

/// `/proc/self/fd/` and a descriptor's number, with room for its NUL.
const PROC_FD_ROOM: usize = 32;

/// `realpath(path)`: `path` with every link, `.` and `..` resolved, written into `into` and
/// lent back (no NUL). The kernel has no such call: the path is opened (`O_PATH`, following
/// its links, so every part must exist, as glibc's `realpath` requires) and the kernel's own
/// name of that open file read from `/proc/self/fd/N`. So it needs procfs mounted (a missing
/// one is [`RealPathError::Proc`] with [`Errno::NoEnt`]), and the name is as this process's
/// root sees it. `PATH_MAX` (4096) bytes of `into` hold any name.
/// replaces libc::realpath
#[blaze::noalloc]
#[blaze::nopanic]
pub fn realpath<'a>(path: Path<'_>, into: &'a mut [u8]) -> Result<&'a [u8], RealPathError> {
    let flags = OpenFlags::PATH.with(OpenFlags::CLOSE_ON_EXEC);
    let file = super::openat(CWD, path, flags, 0).map_err(RealPathError::Path)?;
    let mut text = [0u8; PROC_FD_ROOM];
    let len = proc_fd_name(file.fd().raw(), &mut text);
    let mut room = [0u8; PROC_FD_ROOM];
    let link = Path::in_buffer(text.get(..len).unwrap_or(&[]), &mut room).map_err(RealPathError::Proc)?;
    // `file` stays open across the read, so the name read is the one it was opened by; it is
    // closed when dropped, after.
    super::readlinkat(CWD, link, into).map_err(RealPathError::Proc)
}

/// `/proc/self/fd/<fd>` written into `into` (no NUL): how many bytes. `fd` is a descriptor
/// this process holds, so not negative and at most ten digits: 14 + 10 bytes fit.
#[blaze::noalloc]
#[blaze::nopanic]
fn proc_fd_name(fd: i32, into: &mut [u8; PROC_FD_ROOM]) -> usize {
    const PREFIX: &[u8] = b"/proc/self/fd/";
    let mut at = 0usize;
    while let (Some(&byte), Some(to)) = (PREFIX.get(at), into.get_mut(at)) {
        *to = byte;
        at = at.wrapping_add(1);
    }
    // The digits, last first, into their own room; then copied in order.
    let mut digits = [0u8; 10];
    let mut count = 0usize;
    let mut n = fd as u32;
    loop {
        if let Some(slot) = digits.get_mut(count) {
            *slot = b'0'.wrapping_add((n % 10) as u8);
        }
        count = count.wrapping_add(1);
        n /= 10;
        if n == 0 || count >= digits.len() {
            break;
        }
    }
    while count > 0 {
        count = count.wrapping_sub(1);
        if let (Some(&digit), Some(to)) = (digits.get(count), into.get_mut(at)) {
            *to = digit;
        }
        at = at.wrapping_add(1);
    }
    at
}

// ---- extended attributes ----------------------------------------------------------------

flag_set! {
    /// `setxattr`'s flags (linux/xattr.h).
    XattrFlags: u32 {
        /// `XATTR_CREATE`: refused ([`Errno::Exist`]) where the attribute is there.
        CREATE = 0x1;
        /// `XATTR_REPLACE`: refused ([`Errno::NoData`]) where it is not.
        REPLACE = 0x2;
    }
}

/// `setxattr(path, name, value, flags)`: the extended attribute `name` of `path` set to
/// `value`. [`Errno::OpNotSupp`] where the file system has none; [`Errno::NoSpc`].
/// replaces libc::setxattr
#[blaze::noalloc]
#[blaze::nopanic]
pub fn setxattr(path: Path<'_>, name: &CStr, value: &[u8], flags: XattrFlags) -> Result<(), Errno> {
    // SAFETY: `path` and `name` are live NUL-terminated strings and `value` live bytes of its
    // length, all read by the kernel and not kept.
    raw::check(unsafe {
        raw::syscall5(
            nr::SETXATTR,
            path.as_ptr() as usize,
            name.as_ptr() as usize,
            value.as_ptr() as usize,
            value.len(),
            flags.bits() as usize,
        )
    })
    .map(|_| ())
}

/// `removexattr(path, name)`: the extended attribute `name` of `path` removed.
/// [`Errno::NoData`] where it is not there.
/// replaces libc::removexattr
#[blaze::noalloc]
#[blaze::nopanic]
pub fn removexattr(path: Path<'_>, name: &CStr) -> Result<(), Errno> {
    // SAFETY: `path` and `name` are live NUL-terminated strings the kernel reads and does not
    // keep.
    raw::check(unsafe { raw::syscall2(nr::REMOVEXATTR, path.as_ptr() as usize, name.as_ptr() as usize) })
        .map(|_| ())
}

/// `fgetxattr(fd, name, into)`: the extended attribute `name` of the open file `fd`, its
/// length; its bytes written to `into` where `into` is not empty (an empty `into` asks the
/// length alone: whether it is there). [`Errno::NoData`] where it is absent,
/// [`Errno::Range`] where `into` is too small.
/// replaces libc::fgetxattr
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fgetxattr(fd: Fd, name: &CStr, into: &mut [u8]) -> Result<usize, Errno> {
    // SAFETY: `name` is a live NUL-terminated string the kernel reads; `into` is live room of
    // its length it writes at most that many bytes of (none for an empty one); it keeps
    // neither.
    raw::check(unsafe {
        raw::syscall4(nr::FGETXATTR, arg(fd), name.as_ptr() as usize, into.as_mut_ptr() as usize, into.len())
    })
}

// ---- advice and mounts --------------------------------------------------------------------

/// `fadvise64`'s advice (linux/fadvise.h, `POSIX_FADV_*` on a 64-bit target).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileAdvice {
    /// `POSIX_FADV_NORMAL`: no advice.
    Normal,
    /// `POSIX_FADV_RANDOM`: no read-ahead.
    Random,
    /// `POSIX_FADV_SEQUENTIAL`: a larger read-ahead.
    Sequential,
    /// `POSIX_FADV_WILLNEED`: the range read ahead now.
    WillNeed,
    /// `POSIX_FADV_DONTNEED`: the range's clean pages dropped.
    DontNeed,
    /// `POSIX_FADV_NOREUSE`: the range read once.
    NoReuse,
}

impl FileAdvice {
    /// The kernel's number.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn raw(self) -> u32 {
        match self {
            FileAdvice::Normal => 0,
            FileAdvice::Random => 1,
            FileAdvice::Sequential => 2,
            FileAdvice::WillNeed => 3,
            FileAdvice::DontNeed => 4,
            FileAdvice::NoReuse => 5,
        }
    }
}

/// `fadvise64(fd, offset, len, advice)`: advice on how the range (`len` 0: to the end) will be
/// read. The system call returns its error in the result, as every call here does (glibc's
/// `posix_fadvise` hands that back as its value). [`Errno::BadF`], [`Errno::SPipe`] for a
/// pipe, [`Errno::Inval`].
/// replaces libc::posix_fadvise
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fadvise64(fd: Fd, offset: i64, len: i64, advice: FileAdvice) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe {
        raw::syscall4(nr::FADVISE64, arg(fd), offset as usize, len as usize, advice.raw() as usize)
    })
    .map(|_| ())
}

flag_set! {
    /// `mount`'s flags (`MS_*`, linux/mount.h).
    MountFlags: u64 {
        /// `MS_RDONLY`.
        READ_ONLY = 0x1;
        /// `MS_NOSUID`.
        NO_SUID = 0x2;
        /// `MS_NODEV`.
        NO_DEV = 0x4;
        /// `MS_NOEXEC`.
        NO_EXEC = 0x8;
        /// `MS_REMOUNT`.
        REMOUNT = 0x20;
        /// `MS_BIND`.
        BIND = 0x1000;
        /// `MS_REC`.
        RECURSIVE = 0x4000;
        /// `MS_PRIVATE`.
        PRIVATE = 0x4_0000;
    }
}

/// `mount(source, target, fstype, flags, data)`: a file system mounted on `target` (`None`
/// for a source, kind or data the request does not use: a bind or a remount). Needs
/// `CAP_SYS_ADMIN` in the mount namespace's user namespace: [`Errno::Perm`].
/// replaces libc::mount
#[blaze::noalloc]
#[blaze::nopanic]
pub fn mount(
    source: Option<Path<'_>>,
    target: Path<'_>,
    fstype: Option<Path<'_>>,
    flags: MountFlags,
    data: Option<&CStr>,
) -> Result<(), Errno> {
    let source = source.map_or(0, |it| it.as_ptr() as usize);
    let fstype = fstype.map_or(0, |it| it.as_ptr() as usize);
    let data = data.map_or(0, |it| it.as_ptr() as usize);
    // SAFETY: each string given is live and NUL-terminated (null for none), read by the
    // kernel and not kept.
    raw::check(unsafe {
        raw::syscall5(nr::MOUNT, source, target.as_ptr() as usize, fstype, flags.bits() as usize, data)
    })
    .map(|_| ())
}

flag_set! {
    /// `umount2`'s flags (linux/fs.h).
    UnmountFlags: u32 {
        /// `MNT_FORCE`.
        FORCE = 0x1;
        /// `MNT_DETACH`: detached now, freed once unused.
        DETACH = 0x2;
        /// `MNT_EXPIRE`.
        EXPIRE = 0x4;
        /// `UMOUNT_NOFOLLOW`: `target` a link is refused.
        NO_FOLLOW = 0x8;
    }
}

/// `umount2(target, flags)`: the mount on `target` taken away. [`Errno::Busy`] in use without
/// [`UnmountFlags::DETACH`]; [`Errno::Perm`].
/// replaces libc::umount2
#[blaze::noalloc]
#[blaze::nopanic]
pub fn umount2(target: Path<'_>, flags: UnmountFlags) -> Result<(), Errno> {
    // SAFETY: `target` is a live NUL-terminated string the kernel reads and does not keep.
    raw::check(unsafe { raw::syscall2(nr::UMOUNT2, target.as_ptr() as usize, flags.bits() as usize) }).map(|_| ())
}

// ---- priorities and load ------------------------------------------------------------------

/// Which kind of id a [`getpriority`] or [`setpriority`] names (`PRIO_*`, linux/resource.h).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PriorityOf {
    /// `PRIO_PROCESS`: a thread id (0: the calling thread alone, on Linux).
    Process,
    /// `PRIO_PGRP`: a process group (0: the caller's).
    Group,
    /// `PRIO_USER`: every process of a user (0: the caller's).
    User,
}

impl PriorityOf {
    /// The kernel's number.
    #[blaze::noalloc]
    #[blaze::nopanic]
    const fn raw(self) -> usize {
        match self {
            PriorityOf::Process => 0,
            PriorityOf::Group => 1,
            PriorityOf::User => 2,
        }
    }
}

/// `getpriority(which, who)`: the nice value, -20 to 19 (the highest priority of those named).
/// The kernel's call answers `20 - nice` (1 to 40, so no value is an error's); this is the
/// nice value glibc's `getpriority` returns, and an error is only an error. [`Errno::Srch`].
/// replaces libc::getpriority
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getpriority(which: PriorityOf, who: u32) -> Result<i32, Errno> {
    // SAFETY: the call takes numbers and no memory.
    let said = raw::check(unsafe { raw::syscall2(nr::GETPRIORITY, which.raw(), who as usize) })?;
    // 1 to 40: neither the cast nor the subtraction can wrap.
    Ok(20i32.wrapping_sub(said as i32))
}

/// `setpriority(which, who, nice)`: the nice value set (the kernel clamps it to -20..=19).
/// [`Errno::Acces`] for a lower value without privilege; [`Errno::Srch`].
/// replaces libc::setpriority
#[blaze::noalloc]
#[blaze::nopanic]
pub fn setpriority(which: PriorityOf, who: u32, nice: i32) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall3(nr::SETPRIORITY, which.raw(), who as usize, nice as isize as usize) })
        .map(|_| ())
}

/// An I/O scheduling class (`IOPRIO_CLASS_*`, linux/ioprio.h): the top three bits of an
/// [`IoPriority`].
/// replaces blaze_core::raw::idle_io's private IOPRIO_CLASS_IDLE (IoClass::Idle)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IoClass {
    /// `IOPRIO_CLASS_NONE` (0): none set; the thread's CPU nice value decides.
    None,
    /// `IOPRIO_CLASS_RT` (1): real time (needs `CAP_SYS_ADMIN` to set).
    RealTime,
    /// `IOPRIO_CLASS_BE` (2): best effort, the default.
    BestEffort,
    /// `IOPRIO_CLASS_IDLE` (3): only when no other I/O wants the disk.
    Idle,
    /// A class this enum does not name (4 to 7), kept so a value read is put back unchanged.
    Unlisted(u8),
}

/// An I/O priority as the kernel holds it (linux/ioprio.h): a class and its data (the level
/// 0..=7 in the low three bits, and from 6.5 the hint in bits 3..=12), `class << 13 | data`.
/// replaces blaze_core::raw::idle_io's private IOPRIO_CLASS_IDLE, IOPRIO_CLASS_SHIFT and IOPRIO_WHO_PROCESS (the libc crate has no IOPRIO_* items): IoPriority { class: IoClass::.., data }, IoPriority::IDLE
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IoPriority {
    /// The class.
    pub class: IoClass,
    /// The class's data: 13 bits (a wider value is cut to them).
    pub data: u16,
}

impl IoPriority {
    /// `IOPRIO_CLASS_SHIFT`.
    const SHIFT: u32 = 13;
    /// The data's bits.
    const DATA: u32 = (1 << Self::SHIFT) - 1;

    /// The idle class, data 0: what blaze sets for background disk work.
    pub const IDLE: IoPriority = IoPriority { class: IoClass::Idle, data: 0 };

    /// The kernel's word.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn raw(self) -> u32 {
        let class: u32 = match self.class {
            IoClass::None => 0,
            IoClass::RealTime => 1,
            IoClass::BestEffort => 2,
            IoClass::Idle => 3,
            IoClass::Unlisted(class) => (class & 7) as u32,
        };
        // A rotation of a value under 8 by 13 is the shift, with no amount check in it (a
        // check is a panic path).
        class.rotate_left(Self::SHIFT) | (self.data as u32 & Self::DATA)
    }

    /// The priority the kernel's word names.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn from_raw(raw: u32) -> IoPriority {
        let class = match raw.rotate_right(Self::SHIFT) & 7 {
            0 => IoClass::None,
            1 => IoClass::RealTime,
            2 => IoClass::BestEffort,
            3 => IoClass::Idle,
            other => IoClass::Unlisted(other as u8),
        };
        IoPriority { class, data: (raw & Self::DATA) as u16 }
    }
}

/// `IOPRIO_WHO_PROCESS` with who 0: the calling thread.
const IOPRIO_WHO_THIS_THREAD: (usize, usize) = (1, 0);

/// `ioprio_get(IOPRIO_WHO_PROCESS, 0)`: the calling thread's I/O priority, which
/// [`ioprio_set`] puts back exactly. Used by `blaze_core::raw::idle_io`.
/// replaces libc::syscall(libc::SYS_ioprio_get) (for the calling thread, IOPRIO_WHO_PROCESS with who 0; as blaze_sys::linux::task::ioprio_get(); returns Result<IoPriority, Errno>, the site's negative check becomes the Err)
/// replaces libc::SYS_ioprio_get (the call; the number as data stays nr::IOPRIO_GET)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn ioprio_get() -> Result<IoPriority, Errno> {
    let (which, who) = IOPRIO_WHO_THIS_THREAD;
    // SAFETY: the call takes numbers and no memory.
    let raw = raw::check(unsafe { raw::syscall2(nr::IOPRIO_GET, which, who) })?;
    Ok(IoPriority::from_raw(raw as u32))
}

/// `ioprio_set(IOPRIO_WHO_PROCESS, 0, priority)`: the calling thread's I/O priority set.
/// [`Errno::Perm`] for [`IoClass::RealTime`] without the capability; [`Errno::Inval`] for a
/// class or level the kernel refuses.
/// replaces libc::syscall(libc::SYS_ioprio_set) (for the calling thread, IOPRIO_WHO_PROCESS with who 0; as blaze_sys::linux::task::ioprio_set(IoPriority); returns Result<(), Errno>)
/// replaces libc::SYS_ioprio_set (the call; the number as data stays nr::IOPRIO_SET)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn ioprio_set(priority: IoPriority) -> Result<(), Errno> {
    let (which, who) = IOPRIO_WHO_THIS_THREAD;
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall3(nr::IOPRIO_SET, which, who, priority.raw() as usize) }).map(|_| ())
}

#[cfg(test)]
mod ioprio_tests {
    use super::{IoClass, IoPriority, ioprio_get, ioprio_set};

    /// Every word round-trips, the idle class is 3 << 13, and the thread's priority set to
    /// idle reads back idle and is put back exactly.
    #[test]
    fn io_priority_round_trips() {
        for raw in [0u32, 1 << 13, (2 << 13) | 4, 3 << 13, (7 << 13) | 0x1fff] {
            assert_eq!(IoPriority::from_raw(raw).raw(), raw);
        }
        assert_eq!(IoPriority::IDLE.raw(), 3 << 13);
        let was = ioprio_get().unwrap();
        ioprio_set(IoPriority::IDLE).unwrap();
        assert_eq!(ioprio_get().unwrap().class, IoClass::Idle);
        // A kernel before 6.0 reads an unset priority as class NONE with level 4, which its
        // own `ioprio_set` refuses (NONE takes no data): put back the class NONE then.
        match ioprio_set(was) {
            Ok(()) => assert_eq!(ioprio_get().unwrap(), was),
            Err(crate::Errno::Inval) if was.class == IoClass::None => {
                ioprio_set(IoPriority { class: IoClass::None, data: 0 }).unwrap();
            }
            Err(other) => panic!("ioprio_set({was:?}): {other}"),
        }
    }
}

/// The kernel's `struct sysinfo` (linux/sysinfo.h, 64-bit): the machine's uptime, loads,
/// memory and process count.
/// replaces libc::sysinfo
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Sysinfo {
    /// `uptime`: seconds since boot.
    pub uptime: i64,
    /// `loads`: the 1, 5 and 15 minute load averages, in 1/65536 (`SI_LOAD_SHIFT` 16).
    pub loads: [u64; 3],
    /// `totalram`: usable memory, in `mem_unit`s.
    pub totalram: u64,
    /// `freeram`.
    pub freeram: u64,
    /// `sharedram`.
    pub sharedram: u64,
    /// `bufferram`.
    pub bufferram: u64,
    /// `totalswap`.
    pub totalswap: u64,
    /// `freeswap`.
    pub freeswap: u64,
    /// `procs`: processes and threads.
    pub procs: u16,
    _pad: u16,
    /// `totalhigh`: high memory (0 on a 64-bit target).
    pub totalhigh: u64,
    /// `freehigh`.
    pub freehigh: u64,
    /// `mem_unit`: the size in bytes of the memory fields' unit.
    pub mem_unit: u32,
}

// linux/sysinfo.h on a 64-bit target (`_f` is 0 bytes there): 112 bytes; these offsets.
const _: () = {
    assert!(size_of::<Sysinfo>() == 112);
    assert!(align_of::<Sysinfo>() == 8);
    assert!(core::mem::offset_of!(Sysinfo, uptime) == 0);
    assert!(core::mem::offset_of!(Sysinfo, loads) == 8);
    assert!(core::mem::offset_of!(Sysinfo, totalram) == 32);
    assert!(core::mem::offset_of!(Sysinfo, freeswap) == 72);
    assert!(core::mem::offset_of!(Sysinfo, procs) == 80);
    assert!(core::mem::offset_of!(Sysinfo, totalhigh) == 88);
    assert!(core::mem::offset_of!(Sysinfo, mem_unit) == 104);
};

impl Sysinfo {
    /// All zero, for the call to fill.
    pub const ZERO: Sysinfo = Sysinfo {
        uptime: 0,
        loads: [0; 3],
        totalram: 0,
        freeram: 0,
        sharedram: 0,
        bufferram: 0,
        totalswap: 0,
        freeswap: 0,
        procs: 0,
        _pad: 0,
        totalhigh: 0,
        freehigh: 0,
        mem_unit: 0,
    };

    /// The load averages over 1, 5 and 15 minutes, as `getloadavg` gives them.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn load_averages(&self) -> [f64; 3] {
        /// `1 << SI_LOAD_SHIFT`.
        const SCALE: f64 = 65536.0;
        let [one, five, fifteen] = self.loads;
        [one as f64 / SCALE, five as f64 / SCALE, fifteen as f64 / SCALE]
    }
}

/// `sysinfo()`: the machine's uptime, loads and memory. The load averages are the kernel's
/// own (`avenrun`), the numbers glibc's `getloadavg` reads from `/proc/loadavg`: with
/// [`Sysinfo::load_averages`] this replaces it with no file read.
/// replaces libc::sysinfo
/// replaces libc::getloadavg
#[blaze::noalloc]
#[blaze::nopanic]
pub fn sysinfo() -> Result<Sysinfo, Errno> {
    let mut info = Sysinfo::ZERO;
    // SAFETY: `info` is a live `struct sysinfo` (the layout asserted above) the kernel writes
    // and does not keep.
    raw::check(unsafe { raw::syscall1(nr::SYSINFO, core::ptr::from_mut(&mut info) as usize) })?;
    Ok(info)
}
