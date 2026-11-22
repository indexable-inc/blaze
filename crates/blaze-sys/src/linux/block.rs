//! Block devices and preallocation (D182): what bstd's `block` capability asks of Linux. The
//! facts of a device opened from the `/dev` path its grant names (never discovered here), the
//! preallocation of a file standing in for one, and the positional write that says how durable
//! and how atomic it is.
//!
//! Each fact's call, the kernel it needs, and what blaze takes where the kernel is older or the
//! device does not say (the conservative value: never one that promises more):
//!
//! | fact | call | kernel | absent |
//! |---|---|---|---|
//! | logical block | `ioctl(BLKSSZGET)` | 2.6 | (always there) |
//! | physical block | `ioctl(BLKPBSZGET)` | 2.6.32 | the logical block |
//! | size | `ioctl(BLKGETSIZE64)` | 2.6 | (always there) |
//! | atomic write unit | `statx(STATX_WRITE_ATOMIC)`'s `stx_atomic_write_unit_max` | 6.11 (block devices; xfs and ext4 files 6.13) | one logical block |
//! | queue depth | `/sys/dev/block/M:m/queue/nr_requests` | 2.6 | 1 |
//! | volatile cache | `/sys/dev/block/M:m/queue/write_cache` (`write back`) | 4.7 | a volatile cache |
//!
//! And the write: `pwritev2` (4.6) with `RWF_DSYNC` (4.7: forced unit access where the device
//! has it, else a cache flush after the write) for a durable write, `RWF_ATOMIC` (6.11) for a
//! write the facts say is one atomic unit; `O_DIRECT` is [`super::OpenFlags::DIRECT`] at the
//! open. A kernel without `RWF_ATOMIC` fails such a write `EOPNOTSUPP`; nothing is written
//! without the flag in its place (rule no-fallback): the facts of a kernel that old say one
//! block, so no caller asks.

use super::fs::{arg, owned};
use super::{OwnedFd, nr, raw};
use crate::{Errno, Fd, Path};

/// `BLKSSZGET` (linux/fs.h, `_IO(0x12, 104)`): the logical block, an `int`.
const BLKSSZGET: usize = 0x1268;
/// `BLKPBSZGET` (`_IO(0x12, 123)`): the physical block, an `unsigned int`.
const BLKPBSZGET: usize = 0x127b;
/// `BLKGETSIZE64` (`_IOR(0x12, 114, size_t)`, on a 64-bit target): the size in bytes, a `u64`.
const BLKGETSIZE64: usize = 0x8008_1272;
/// `BLKDISCARD` (`_IO(0x12, 119)`): a range, two `u64`s (offset, length), discarded.
const BLKDISCARD: usize = 0x1277;

/// `RWF_DSYNC` (linux/fs.h): this write durable when it returns, as `O_DSYNC` for one call.
pub const RWF_DSYNC: u32 = 0x0000_0002;
/// `RWF_ATOMIC` (linux/fs.h, 6.11): this write lands whole or not at all; its length a power of
/// two within the device's atomic unit bounds and its offset aligned to its length.
pub const RWF_ATOMIC: u32 = 0x0000_0040;

// Preallocation is `super::fdctl::fallocate` with `FallocateFlags` (one home, rule 30).

/// The logical block of the block device `fd`, in bytes.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn block_logical(fd: Fd) -> Result<u32, Errno> {
    let mut size: i32 = 0;
    // SAFETY: BLKSSZGET writes one `int` through the pointer given, a live `i32` here.
    raw::check(unsafe { raw::syscall3(nr::IOCTL, arg(fd), BLKSSZGET, core::ptr::from_mut(&mut size) as usize) })?;
    Ok(size as u32)
}

/// The physical block of the block device `fd`, in bytes.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn block_physical(fd: Fd) -> Result<u32, Errno> {
    let mut size: u32 = 0;
    // SAFETY: BLKPBSZGET writes one `unsigned int` through the pointer given, a live `u32`.
    raw::check(unsafe { raw::syscall3(nr::IOCTL, arg(fd), BLKPBSZGET, core::ptr::from_mut(&mut size) as usize) })?;
    Ok(size)
}

/// The size of the block device `fd`, in bytes.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn block_size(fd: Fd) -> Result<u64, Errno> {
    let mut size: u64 = 0;
    // SAFETY: BLKGETSIZE64 writes one `u64` (a `size_t` on the 64-bit targets blaze-sys
    // builds for) through the pointer given, a live `u64`.
    raw::check(unsafe { raw::syscall3(nr::IOCTL, arg(fd), BLKGETSIZE64, core::ptr::from_mut(&mut size) as usize) })?;
    Ok(size)
}

/// `len` bytes at `at` of the block device `fd` discarded (`BLKDISCARD`). What a read of them
/// gives after is the device's (its facts say whether zeros); the kernel drops their cached
/// pages first.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn block_discard(fd: Fd, at: u64, len: u64) -> Result<(), Errno> {
    let range: [u64; 2] = [at, len];
    // SAFETY: BLKDISCARD reads two `u64`s through the pointer given, a live array of two.
    raw::check(unsafe { raw::syscall3(nr::IOCTL, arg(fd), BLKDISCARD, range.as_ptr() as usize) }).map(|_| ())
}

/// The atomic write bounds `statx` reports of the file `fd` (`STATX_WRITE_ATOMIC`, 6.11):
/// `(unit_min, unit_max)` in bytes, `None` where the kernel or the file does not report them
/// (an older kernel leaves the mask bit clear; a device without atomic writes reports 0).
///
/// Needs [`super::Statx`]'s atomic fields and [`super::StatxMask::WRITE_ATOMIC`]: the D182
/// edit to `linux/fs.rs` that names them (they are in its `spare3` today).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn atomic_write_bounds(fd: Fd) -> Result<Option<(u32, u32)>, Errno> {
    let stat = super::statx(fd, Path::EMPTY, super::AtFlags::EMPTY_PATH, super::StatxMask::WRITE_ATOMIC)?;
    let said = stat.mask & super::StatxMask::WRITE_ATOMIC.bits() != 0;
    Ok(match said && stat.atomic_write_unit_max != 0 {
        true => Some((stat.atomic_write_unit_min, stat.atomic_write_unit_max)),
        false => None,
    })
}

/// `struct iovec`: one buffer of a vectored call.
#[repr(C)]
struct Iovec {
    base: usize,
    len: usize,
}

/// `pwritev2` of one buffer: `bytes` at `at` of `fd` with `flags` ([`RWF_DSYNC`],
/// [`RWF_ATOMIC`]). It may write fewer than asked, except with `RWF_ATOMIC`, which writes all or
/// fails. Unknown flags fail `EOPNOTSUPP` (a kernel before the flag).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn pwrite_flagged(fd: Fd, bytes: &[u8], at: u64, flags: u32) -> Result<usize, Errno> {
    let one = Iovec { base: bytes.as_ptr() as usize, len: bytes.len() };
    // SAFETY: one live `iovec` naming `bytes`, which the kernel only reads; it keeps neither.
    // The offset is passed as the low and high halves the 64-bit ABI reads as one register
    // (`pos_l`, `pos_h`: the high half is ignored on a 64-bit kernel).
    raw::check(unsafe {
        raw::syscall6(
            nr::PWRITEV2,
            arg(fd),
            core::ptr::from_ref(&one) as usize,
            1,
            at as usize,
            0,
            flags as usize,
        )
    })
}

/// The device numbers of the block device `fd` (`statx`'s `rdev`), for its sysfs folder.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn block_numbers(fd: Fd) -> Result<(u32, u32), Errno> {
    let stat = super::statx(fd, Path::EMPTY, super::AtFlags::EMPTY_PATH, super::StatxMask::TYPE)?;
    Ok((stat.rdev_major, stat.rdev_minor))
}

/// The longest sysfs path [`sysfs_queue`] makes: `/sys/dev/block/4294967295:4294967295/queue/`
/// and the longest name it reads, and the NUL.
const SYSFS_PATH: usize = 96;

/// The names under a device's `queue/` that [`sysfs_queue`] reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueueFact {
    /// `nr_requests`: how many requests the block layer queues to it.
    Requests,
    /// `write_cache`: `write back` (a volatile cache) or `write through` (4.7).
    WriteCache,
    /// `atomic_write_unit_max_bytes` (6.11).
    AtomicUnitMax,
}

impl QueueFact {
    const fn name(self) -> &'static [u8] {
        match self {
            QueueFact::Requests => b"nr_requests",
            QueueFact::WriteCache => b"write_cache",
            QueueFact::AtomicUnitMax => b"atomic_write_unit_max_bytes",
        }
    }
}

/// The first line of `/sys/dev/block/<major>:<minor>/queue/<fact>` into `into`, how many bytes
/// (the newline cut); `Errno::NoEnt` where the kernel has no such file (older than the fact).
/// The path is made in a fixed buffer: nothing is allocated.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn sysfs_queue(numbers: (u32, u32), fact: QueueFact, into: &mut [u8]) -> Result<usize, Errno> {
    let mut path = [0u8; SYSFS_PATH];
    let mut at = 0;
    let mut put = |bytes: &[u8], path: &mut [u8; SYSFS_PATH]| {
        // Bounded by the type: every part fits (`SYSFS_PATH`'s sum, with the NUL).
        for &byte in bytes {
            if at + 1 < SYSFS_PATH {
                path[at] = byte;
                at += 1;
            }
        }
    };
    let mut digits = [0u8; 10];
    put(b"/sys/dev/block/", &mut path);
    put(decimal(numbers.0, &mut digits), &mut path);
    put(b":", &mut path);
    put(decimal(numbers.1, &mut digits), &mut path);
    put(b"/queue/", &mut path);
    put(fact.name(), &mut path);
    // `path` ends in its NUL: the buffer was zeros and `put` never writes its last byte.
    let Ok(text) = core::ffi::CStr::from_bytes_until_nul(&path) else {
        return Err(Errno::NameTooLong);
    };
    let fd: OwnedFd = owned(open_read(text))?;
    let n = super::read(fd.fd(), into)?;
    let line = into[..n].iter().position(|&byte| byte == b'\n').unwrap_or(n);
    Ok(line)
}

/// `openat(AT_FDCWD, path, O_RDONLY | O_CLOEXEC)`, the kernel's raw result for [`owned`].
/// Whole paths from the process's root: sysfs is not beneath any grant's folder, and only
/// these fixed names under `/sys/dev/block` are opened.
#[blaze::noalloc]
#[blaze::nopanic]
fn open_read(path: &core::ffi::CStr) -> usize {
    let flags = super::OpenFlags::READ.with(super::OpenFlags::CLOSE_ON_EXEC);
    // SAFETY: `path` is a live NUL-terminated string the kernel reads and does not keep.
    unsafe { raw::syscall4(nr::OPENAT, arg(super::CWD), path.as_ptr() as usize, flags.bits() as usize, 0) }
}

/// `n` in decimal, in `into`'s last bytes: the slice of them.
#[blaze::noalloc]
#[blaze::nopanic]
fn decimal(mut n: u32, into: &mut [u8; 10]) -> &[u8] {
    let mut at = into.len();
    loop {
        at -= 1;
        into[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 || at == 0 {
            break;
        }
    }
    &into[at..]
}
