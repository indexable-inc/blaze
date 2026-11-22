//! Descriptor control by raw system call: `fcntl`'s operations, `dup`, `dup2`/`dup3`, the
//! `ioctl`s blaze makes, `preadv`, `fallocate`, `memfd_create`, and the two map calls that
//! report on or write back a mapping (`mincore`, `msync`). What `posix::io` named from the C
//! library on Linux (D180, no C library on Linux): `posix::io` re-exports these.
//!
//! `fcntl` is no item of its own: each command is a function named for what it does
//! ([`status_flags`], [`set_close_on_exec`], [`dup_at_least`], [`add_seals`]), its doc line
//! naming the `F_*` it replaces.

use core::{
    ffi::{c_int, c_ulong, c_void},
    ptr::NonNull,
};

use super::{
    IoSliceMut, OpenFlags, OwnedFd,
    fs::{arg, owned},
    nr, raw,
};
use crate::{Errno, Fd, Path};

// ---- fcntl ----

/// `F_DUPFD` (asm-generic/fcntl.h).
pub(crate) const F_DUPFD: c_int = 0;
/// `F_GETFD`.
pub(crate) const F_GETFD: c_int = 1;
/// `F_SETFD`.
pub(crate) const F_SETFD: c_int = 2;
/// `F_GETFL`.
pub(crate) const F_GETFL: c_int = 3;
/// `F_SETFL`.
pub(crate) const F_SETFL: c_int = 4;
/// `F_DUPFD_CLOEXEC` (linux/fcntl.h: `F_LINUX_SPECIFIC_BASE + 6`).
pub(crate) const F_DUPFD_CLOEXEC: c_int = 1030;
/// `F_ADD_SEALS` (linux/fcntl.h: `F_LINUX_SPECIFIC_BASE + 9`).
pub(crate) const F_ADD_SEALS: c_int = 1033;
/// `FD_CLOEXEC`: the one descriptor flag.
pub(crate) const FD_CLOEXEC: c_int = 1;

/// `fcntl(fd, command, arg)` for a command whose argument is a number (or none: 0 is passed
/// and not read).
#[blaze::noalloc]
#[blaze::nopanic]
fn fcntl_number(fd: Fd, command: c_int, value: usize) -> Result<usize, Errno> {
    // SAFETY: every command this file passes takes a number or nothing, and no memory.
    raw::check(unsafe { raw::syscall3(nr::FCNTL, arg(fd), command as usize, value) })
}

/// `fcntl(F_GETFL)`: the descriptor's status flags and access mode. replaces libc::fcntl with
/// libc::F_GETFL.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn status_flags(fd: Fd) -> Result<OpenFlags, Errno> {
    fcntl_number(fd, F_GETFL, 0).map(|flags| OpenFlags::from_bits(flags as u32))
}

/// `fcntl(F_SETFL)`: the descriptor's status flags set to `flags` (only `O_APPEND`,
/// `O_NONBLOCK`, `O_DIRECT` and `O_NOATIME` change; the access mode does not). replaces
/// libc::fcntl with libc::F_SETFL.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_status_flags(fd: Fd, flags: OpenFlags) -> Result<(), Errno> {
    fcntl_number(fd, F_SETFL, flags.bits() as usize).map(|_| ())
}

/// `fcntl(F_GETFL)` then `fcntl(F_SETFL)` with `O_NONBLOCK` added (`on`) or taken away, the
/// other flags kept; no second call where nothing changes. replaces the pair
/// libc::fcntl(fd, libc::F_GETFL) and libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_nonblocking(fd: Fd, on: bool) -> Result<(), Errno> {
    let now = status_flags(fd)?;
    let then = if on {
        now.with(OpenFlags::NONBLOCK)
    } else {
        OpenFlags::from_bits(now.bits() & !OpenFlags::NONBLOCK.bits())
    };
    if then == now {
        return Ok(());
    }
    set_status_flags(fd, then)
}

/// `fcntl(F_GETFD)`: whether the descriptor closes on exec; [`Errno::BadF`] for a number no
/// descriptor has (the check whether one is open). replaces libc::fcntl with libc::F_GETFD.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn close_on_exec(fd: Fd) -> Result<bool, Errno> {
    fcntl_number(fd, F_GETFD, 0).map(|flags| flags & FD_CLOEXEC as usize != 0)
}

/// `fcntl(F_SETFD, FD_CLOEXEC)` (`on`) or `fcntl(F_SETFD, 0)`: whether the descriptor closes
/// on exec. replaces libc::fcntl with libc::F_SETFD, libc::FD_CLOEXEC.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_close_on_exec(fd: Fd, on: bool) -> Result<(), Errno> {
    fcntl_number(fd, F_SETFD, if on { FD_CLOEXEC as usize } else { 0 }).map(|_| ())
}

/// `fcntl(F_DUPFD_CLOEXEC, lowest)` (`close_on_exec`) or `fcntl(F_DUPFD, lowest)`: a second
/// descriptor of the same open file, numbered `lowest` or the lowest free number above it;
/// [`Errno::Inval`] for a negative `lowest`. replaces libc::fcntl with libc::F_DUPFD_CLOEXEC,
/// libc::F_DUPFD.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn dup_at_least(fd: Fd, lowest: c_int, close_on_exec: bool) -> Result<OwnedFd, Errno> {
    let Ok(lowest) = usize::try_from(lowest) else {
        return Err(Errno::Inval);
    };
    let command = if close_on_exec { F_DUPFD_CLOEXEC } else { F_DUPFD };
    // SAFETY: both commands take a number and no memory.
    owned(unsafe { raw::syscall3(nr::FCNTL, arg(fd), command as usize, lowest) })
}

flag_set! {
    /// A memfd's seals (linux/fcntl.h `F_SEAL_*`): what may no longer be done to it.
    Seals: u32 {
        /// `F_SEAL_SEAL`: no more seals. replaces libc::F_SEAL_SEAL.
        SEAL = 0x0001;
        /// `F_SEAL_SHRINK`. replaces libc::F_SEAL_SHRINK.
        SHRINK = 0x0002;
        /// `F_SEAL_GROW`. replaces libc::F_SEAL_GROW.
        GROW = 0x0004;
        /// `F_SEAL_WRITE`. replaces libc::F_SEAL_WRITE.
        WRITE = 0x0008;
    }
}

/// `fcntl(F_ADD_SEALS)`: `seals` added to a memfd made with [`MemfdFlags::ALLOW_SEALING`];
/// [`Errno::Perm`] once `F_SEAL_SEAL` is set. replaces libc::fcntl with libc::F_ADD_SEALS.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn add_seals(fd: Fd, seals: Seals) -> Result<(), Errno> {
    fcntl_number(fd, F_ADD_SEALS, seals.bits() as usize).map(|_| ())
}

// ---- dup ----

/// `dup`: a second descriptor of the same open file, the lowest free number, not closed on
/// exec. replaces libc::dup.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn dup(fd: Fd) -> Result<OwnedFd, Errno> {
    // SAFETY: the call takes a number and no memory.
    owned(unsafe { raw::syscall1(nr::DUP, arg(fd)) })
}

/// `dup3`: the number `to` made a descriptor of `from`'s open file, closing what `to` was
/// first, closed on exec when `close_on_exec` says so; [`Errno::Inval`] where the two are the
/// same number (the kernel's refusal: [`dup2`] is the form that allows it). replaces
/// libc::dup3.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn dup3(from: Fd, to: Fd, close_on_exec: bool) -> Result<Fd, Errno> {
    let flags = if close_on_exec { OpenFlags::CLOSE_ON_EXEC.bits() as usize } else { 0 };
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall3(nr::DUP3, arg(from), arg(to), flags) }).map(|to| Fd::new(to as i32))
}

/// `dup2`: the number `to` made a descriptor of `from`'s open file, closing what `to` was
/// first (an owner of `to` now owns the copy); `to` itself, not closed on exec. Where the two
/// are the same number nothing changes and `to` is returned once `from` is checked open
/// (`fcntl(F_GETFD)`: [`Errno::BadF`] where it is not), as POSIX's `dup2` does; `dup3`, the
/// call both architectures have, would refuse it. replaces libc::dup2.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn dup2(from: Fd, to: Fd) -> Result<Fd, Errno> {
    if from.raw() == to.raw() {
        close_on_exec(from)?;
        return Ok(to);
    }
    dup3(from, to, false)
}

// ---- ioctl ----

/// `FIONBIO` (asm-generic/ioctls.h; x86-64 and aarch64 alike): the request of [`ioctl`] that
/// sets a descriptor's non-blocking mode from an `int`. replaces libc::FIONBIO.
pub const FIONBIO: c_ulong = 0x5421;

/// `ioctl(fd, request, arg)`: a device's own request; the value it returns (0 for most). The
/// typed requests have their own functions ([`fionbio`], [`super::window_size`],
/// [`super::is_terminal`], [`super::ficlone`]); this form is for a request that has none yet.
/// replaces libc::ioctl.
///
/// # Safety
///
/// `arg` is what `request` reads or writes, live for the call (or null for a request that
/// takes nothing).
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn ioctl(fd: Fd, request: c_ulong, arg_ptr: *mut c_void) -> Result<c_int, Errno> {
    // SAFETY: the caller's contract; the request is passed whole, as the kernel's `unsigned
    // int cmd` reads its low 32 bits.
    let result = raw::check(unsafe { raw::syscall3(nr::IOCTL, arg(fd), request as usize, arg_ptr as usize) })?;
    // An ioctl's value is the kernel's `long` result of an `int` handler.
    Ok(result as c_int)
}

/// `ioctl(FIONBIO)`: the descriptor's non-blocking mode set (`on`) or cleared in one call (a
/// socket's, where `fcntl` would take two). replaces libc::ioctl with libc::FIONBIO.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fionbio(fd: Fd, on: bool) -> Result<(), Errno> {
    let mut on = c_int::from(on);
    // SAFETY: `FIONBIO` reads one `int`, a live local, and keeps no pointer to it.
    raw::check(unsafe { raw::syscall3(nr::IOCTL, arg(fd), FIONBIO as usize, (&raw mut on) as usize) }).map(|_| ())
}

// ---- preadv ----

/// `preadv`: up to the slices' total from offset `at`, filling them in order; 0 at the end.
/// It may read fewer than asked (the caller continues). [`Errno::Inval`] past `i64::MAX` or
/// past the kernel's `UIO_MAXIOV` (1024) slices. replaces libc::preadv.
///
/// The offset is passed as the two `unsigned long` halves `pos_l`, `pos_h` the system call
/// takes on every architecture (fs/read_write.c, `pos_from_hilo`): on x86-64 and aarch64 a
/// `long` is 64 bits, so the kernel takes the whole offset from `pos_l` and shifts `pos_h`
/// out; the high half is passed as glibc's `LO_HI_LONG` passes it, `at >> 32`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn preadv(fd: Fd, into: &mut [IoSliceMut<'_>], at: u64) -> Result<usize, Errno> {
    if i64::try_from(at).is_err() {
        return Err(Errno::Inval);
    }
    let (low, high) = (at as usize, (at >> 32) as usize);
    // SAFETY: `into` is `into.len()` live `struct iovec`s (`IoSliceMut` is laid out as one),
    // each over live memory of its length the kernel writes at most that many bytes of;
    // nothing is kept after the call.
    raw::check(unsafe { raw::syscall5(nr::PREADV, arg(fd), into.as_mut_ptr() as usize, into.len(), low, high) })
}

// ---- fallocate ----

flag_set! {
    /// `fallocate`'s mode (linux/falloc.h).
    FallocateFlags: u32 {
        /// `FALLOC_FL_KEEP_SIZE`: the file's length does not change. replaces
        /// libc::FALLOC_FL_KEEP_SIZE.
        KEEP_SIZE = 0x01;
        /// `FALLOC_FL_PUNCH_HOLE`: the range's blocks freed, reading as zeros after; with
        /// `KEEP_SIZE` only. replaces libc::FALLOC_FL_PUNCH_HOLE.
        PUNCH_HOLE = 0x02;
    }
}

/// `fallocate`: the blocks of `at..at + len` of `fd` allocated, or (with
/// [`FallocateFlags::PUNCH_HOLE`]) freed. [`Errno::OpNotSupp`] where the file system cannot;
/// [`Errno::FBig`] past `i64::MAX`. replaces libc::fallocate.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fallocate(fd: Fd, flags: FallocateFlags, at: u64, len: u64) -> Result<(), Errno> {
    if i64::try_from(at).is_err() || i64::try_from(len).is_err() {
        return Err(Errno::FBig);
    }
    // SAFETY: the call takes numbers and no memory; a 64-bit `loff_t` is one register on both
    // architectures.
    raw::check(unsafe { raw::syscall4(nr::FALLOCATE, arg(fd), flags.bits() as usize, at as usize, len as usize) })
        .map(|_| ())
}

// ---- memfd ----

flag_set! {
    /// `memfd_create`'s flags (linux/memfd.h).
    MemfdFlags: u32 {
        /// `MFD_CLOEXEC`. replaces libc::MFD_CLOEXEC.
        CLOSE_ON_EXEC = 0x0001;
        /// `MFD_ALLOW_SEALING`. replaces libc::MFD_ALLOW_SEALING.
        ALLOW_SEALING = 0x0002;
    }
}

/// `memfd_create`: an anonymous file in memory, `name` shown in `/proc/self/fd` only.
/// replaces libc::memfd_create.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn memfd_create(name: Path<'_>, flags: MemfdFlags) -> Result<OwnedFd, Errno> {
    // SAFETY: `name` is a live NUL-terminated string the kernel reads and does not keep.
    owned(unsafe { raw::syscall2(nr::MEMFD_CREATE, name.as_ptr() as usize, flags.bits() as usize) })
}

// ---- mincore, msync ----

flag_set! {
    /// `msync`'s flags (asm-generic/mman-common.h).
    MsyncFlags: u32 {
        /// `MS_ASYNC`: the write-back is begun, not waited for. replaces libc::MS_ASYNC.
        ASYNC = 1;
        /// `MS_INVALIDATE`. replaces libc::MS_INVALIDATE.
        INVALIDATE = 2;
        /// `MS_SYNC`: the write-back is waited for. replaces libc::MS_SYNC.
        SYNC = 4;
    }
}

/// `msync`: the dirty pages of the shared file mapping `at..at + len` written back to the
/// file, waited for or begun as `flags` says. replaces libc::msync.
///
/// # Safety
///
/// `at..at + len` is a mapping of this process, `at` page-aligned; or, with
/// [`MsyncFlags::ASYNC`] alone, any page-aligned range: the call then touches no byte, and a
/// range with an unmapped page answers [`Errno::NoMem`] (the probe of whether it is mapped).
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn msync(at: NonNull<u8>, len: usize, flags: MsyncFlags) -> Result<(), Errno> {
    // SAFETY: the kernel reads the page tables of the range and writes back its pages; the
    // caller answers for the range.
    raw::check(unsafe { raw::syscall3(nr::MSYNC, at.as_ptr() as usize, len, flags.bits() as usize) }).map(|_| ())
}

/// `mincore`: which pages of `at..at + len` are resident, one byte a page into `held` (bit 0
/// set for a resident page). [`Errno::NoMem`] where part of the range is not mapped. replaces
/// libc::mincore.
///
/// # Safety
///
/// `held` has a byte for every page of the range: at least `len` divided by the page size,
/// rounded up. The kernel writes that many bytes and does not check the slice's length.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn mincore(at: NonNull<u8>, len: usize, held: &mut [u8]) -> Result<(), Errno> {
    // SAFETY: the kernel reads the page tables of the range and writes one byte a page into
    // `held`, which the caller sized; it keeps no pointer.
    raw::check(unsafe { raw::syscall3(nr::MINCORE, at.as_ptr() as usize, len, held.as_mut_ptr() as usize) }).map(|_| ())
}
