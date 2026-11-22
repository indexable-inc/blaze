//! A raw disk's facts on macOS (D182): the `DKIOC*` ioctls of sys/disk.h on a descriptor of
//! `/dev/rdiskN` the grant names. The facts query only: no raw writes on macOS yet.
//!
//! | fact | call | absent |
//! |---|---|---|
//! | logical block | `DKIOCGETBLOCKSIZE` | (always there) |
//! | physical block | `DKIOCGETPHYSICALBLOCKSIZE` | the logical block |
//! | size | `DKIOCGETBLOCKCOUNT` times the logical block | (always there) |
//! | forced unit access, unmap | `DKIOCGETFEATURES` | neither |
//!
//! macOS reports no atomic write unit and no queue depth for a disk: blaze takes one logical
//! block and a queue of 1. It reports no volatile cache either; blaze takes one (every Apple
//! SSD has a cache `F_FULLFSYNC` flushes).

use core::ffi::{c_int, c_ulong};

use crate::macos::cold;
use crate::{Errno, Fd};

/// `DKIOCGETBLOCKSIZE`: `_IOR('d', 24, uint32_t)`.
const DKIOCGETBLOCKSIZE: c_ulong = 0x4004_6418;
/// `DKIOCGETBLOCKCOUNT`: `_IOR('d', 25, uint64_t)`.
const DKIOCGETBLOCKCOUNT: c_ulong = 0x4008_6419;
/// `DKIOCGETFEATURES`: `_IOR('d', 76, uint32_t)`.
const DKIOCGETFEATURES: c_ulong = 0x4004_644c;
/// `DKIOCGETPHYSICALBLOCKSIZE`: `_IOR('d', 77, uint32_t)`.
const DKIOCGETPHYSICALBLOCKSIZE: c_ulong = 0x4004_644d;

/// `DK_FEATURE_UNMAP` (sys/disk.h).
pub const DK_FEATURE_UNMAP: u32 = 0x0000_0010;
/// `DK_FEATURE_FORCE_UNIT_ACCESS` (sys/disk.h).
pub const DK_FEATURE_FORCE_UNIT_ACCESS: u32 = 0x0000_0001;

/// What a raw disk says of itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DiskFacts {
    pub logical: u32,
    pub physical: u32,
    pub blocks: u64,
    /// `DKIOCGETFEATURES`' bits.
    pub features: u32,
}

/// One `DKIOC*` read of a `T` (an integer: any bytes valid).
///
/// # Safety
///
/// `request` is an `_IOR` request that writes exactly one `T`.
unsafe fn ask<T: Copy>(fd: Fd, request: c_ulong, zero: T) -> Result<T, Errno> {
    let mut value = zero;
    // SAFETY: the caller's contract: `request` writes one `T` through the pointer, which is a
    // live `T` here; `ioctl` passes the pointer on (an `_IOR` request is not `IOC_VOID`).
    let said: c_int = unsafe { cold::ioctl(fd.raw(), request, core::ptr::from_mut(&mut value)) };
    if said != 0 {
        // SAFETY: `__error` returns the calling thread's `errno`, valid while it runs.
        let code = unsafe { *crate::macos::ffi::__error() };
        return Err(Errno::from_raw(code as u16));
    }
    Ok(value)
}

/// `F_PREALLOCATE` (sys/fcntl.h).
const F_PREALLOCATE: c_int = 42;
/// `F_PUNCHHOLE` (sys/fcntl.h, 10.12).
const F_PUNCHHOLE: c_int = 99;
/// `F_ALLOCATEALL`: all of it or nothing.
const F_ALLOCATEALL: u32 = 0x4;
/// `F_PEOFPOSMODE`: the offset is from the physical end of the file.
const F_PEOFPOSMODE: c_int = 3;

/// `fstore_t` (sys/fcntl.h).
#[repr(C)]
struct Fstore {
    flags: u32,
    posmode: c_int,
    offset: i64,
    length: i64,
    bytesalloc: i64,
}

/// `fpunchhole_t` (sys/fcntl.h).
#[repr(C)]
struct Fpunchhole {
    flags: u32,
    reserved: u32,
    offset: i64,
    length: i64,
}

fn errno_of(said: c_int) -> Result<(), Errno> {
    match said {
        -1 => {
            // SAFETY: `__error` returns the calling thread's `errno`, valid while it runs.
            let code = unsafe { *crate::macos::ffi::__error() };
            Err(Errno::from_raw(code as u16))
        }
        _ => Ok(()),
    }
}

/// `fcntl(F_PREALLOCATE)`: `len` bytes allocated past the file's physical end, all or none
/// (`F_ALLOCATEALL`), not contiguous. The length is unchanged: `ftruncate` after sets it,
/// and APFS then reads the allocated range as zeros without writing them. Used by bstd's
/// block `OverFile` at its making.
#[blaze::nopanic]
pub fn preallocate(fd: Fd, len: u64) -> Result<(), Errno> {
    let mut store = Fstore { flags: F_ALLOCATEALL, posmode: F_PEOFPOSMODE, offset: 0, length: len as i64, bytesalloc: 0 };
    // SAFETY: F_PREALLOCATE reads and writes one `fstore_t` through the pointer, a live one here.
    errno_of(unsafe { crate::macos::ffi::fcntl(fd.raw(), F_PREALLOCATE, core::ptr::from_mut(&mut store)) })
}

/// `fcntl(F_PUNCHHOLE)`: `len` bytes at `at` deallocated, reading as zeros; both multiples of
/// the file system's block (APFS refuses others `EINVAL`).
#[blaze::nopanic]
pub fn punch_hole(fd: Fd, at: u64, len: u64) -> Result<(), Errno> {
    let hole = Fpunchhole { flags: 0, reserved: 0, offset: at as i64, length: len as i64 };
    // SAFETY: F_PUNCHHOLE reads one `fpunchhole_t` through the pointer, a live one here.
    errno_of(unsafe { crate::macos::ffi::fcntl(fd.raw(), F_PUNCHHOLE, core::ptr::from_ref(&hole)) })
}

/// The facts of the raw disk `fd`. A descriptor that is not a disk fails `ENOTTY`.
#[blaze::nopanic]
pub fn disk_facts(fd: Fd) -> Result<DiskFacts, Errno> {
    // SAFETY: each request writes one value of the type asked, as sys/disk.h declares it.
    unsafe {
        let logical: u32 = ask(fd, DKIOCGETBLOCKSIZE, 0u32)?;
        let blocks: u64 = ask(fd, DKIOCGETBLOCKCOUNT, 0u64)?;
        let physical: u32 = match ask(fd, DKIOCGETPHYSICALBLOCKSIZE, 0u32) {
            Ok(physical) if physical != 0 => physical,
            Ok(_) | Err(Errno::NoTty) => logical,
            Err(error) => return Err(error),
        };
        let features: u32 = match ask(fd, DKIOCGETFEATURES, 0u32) {
            Ok(features) => features,
            Err(Errno::NoTty) => 0,
            Err(error) => return Err(error),
        };
        Ok(DiskFacts { logical, physical, blocks, features })
    }
}
