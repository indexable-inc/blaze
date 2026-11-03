//! The seed runner's own calls, module `blaze_seed` (crates/blaze-wasi/src/seed.rs): blaze's
//! ABI for what WASI preview 1's calls don't give its host (blaze-seed-run), every change to the
//! file system among them. Andrew's decision (2026-11-02): the seed's host need not be WASI, so
//! where p1 lacks a call or shapes it awkwardly, the runtime calls these.
//!
//! Every pointer, length, offset and time is 64 bits, a descriptor or flag 32; each answers 0
//! or a WASI errno, the numbers `libc`'s `E*` are on WASI. A path is absolute, `(ptr, len)`, not
//! NUL-terminated: [`crate::rt::path::absolute_at`] makes it. Descriptors are the p1 calls'.

use core::ffi::c_int;

use crate::rt::path::Bytes;

/// A time argument that leaves the time as it is.
pub(crate) const KEEP: u64 = u64::MAX;

/// A time argument that sets the time to the host's clock.
pub(crate) const NOW: u64 = u64::MAX - 1;

mod raw {
    #[link(wasm_import_module = "blaze_seed")]
    unsafe extern "C" {
        pub(super) fn create_dir(path: *const u8, len: usize) -> i32;
        pub(super) fn remove_dir(path: *const u8, len: usize) -> i32;
        pub(super) fn unlink(path: *const u8, len: usize) -> i32;
        pub(super) fn rename(from: *const u8, from_len: usize, to: *const u8, to_len: usize) -> i32;
        pub(super) fn hard_link(from: *const u8, from_len: usize, to: *const u8, to_len: usize) -> i32;
        pub(super) fn symlink(target: *const u8, target_len: usize, path: *const u8, len: usize) -> i32;
        pub(super) fn set_times(path: *const u8, len: usize, follow: u32, atime: u64, mtime: u64) -> i32;
        pub(super) fn fd_set_times(fd: u32, atime: u64, mtime: u64) -> i32;
        pub(super) fn set_len(fd: u32, len: u64) -> i32;
        pub(super) fn sync(fd: u32, data_only: u32) -> i32;
        pub(super) fn pread(fd: u32, buf: *mut u8, len: usize, offset: u64, read: *mut u64) -> i32;
        pub(super) fn pwrite(fd: u32, buf: *const u8, len: usize, offset: u64, wrote: *mut u64) -> i32;
    }
}

fn ok(r: i32) -> Result<(), c_int> {
    crate::ok(r)
}

pub(crate) unsafe fn create_dir(path: Bytes) -> Result<(), c_int> {
    let (p, n) = path.wasi();
    ok(raw::create_dir(p as *const u8, n))
}

pub(crate) unsafe fn remove_dir(path: Bytes) -> Result<(), c_int> {
    let (p, n) = path.wasi();
    ok(raw::remove_dir(p as *const u8, n))
}

pub(crate) unsafe fn unlink(path: Bytes) -> Result<(), c_int> {
    let (p, n) = path.wasi();
    ok(raw::unlink(p as *const u8, n))
}

pub(crate) unsafe fn rename(from: Bytes, to: Bytes) -> Result<(), c_int> {
    let ((f, fl), (t, tl)) = (from.wasi(), to.wasi());
    ok(raw::rename(f as *const u8, fl, t as *const u8, tl))
}

/// A hard link at `to` of `from` itself: a symbolic link at `from` is not followed.
pub(crate) unsafe fn hard_link(from: Bytes, to: Bytes) -> Result<(), c_int> {
    let ((f, fl), (t, tl)) = (from.wasi(), to.wasi());
    ok(raw::hard_link(f as *const u8, fl, t as *const u8, tl))
}

/// A symbolic link at `path` holding `target` as given.
pub(crate) unsafe fn symlink(target: &[u8], path: Bytes) -> Result<(), c_int> {
    let (p, n) = path.wasi();
    ok(raw::symlink(target.as_ptr(), target.len(), p as *const u8, n))
}

/// `path`'s access and modification times, each [`KEEP`], [`NOW`] or nanoseconds since the
/// epoch.
pub(crate) unsafe fn set_times(path: Bytes, follow: bool, atime: u64, mtime: u64) -> Result<(), c_int> {
    let (p, n) = path.wasi();
    ok(raw::set_times(p as *const u8, n, u32::from(follow), atime, mtime))
}

pub(crate) unsafe fn fd_set_times(fd: c_int, atime: u64, mtime: u64) -> Result<(), c_int> {
    ok(raw::fd_set_times(fd as u32, atime, mtime))
}

pub(crate) unsafe fn set_len(fd: c_int, len: u64) -> Result<(), c_int> {
    ok(raw::set_len(fd as u32, len))
}

/// Flushes `fd` to its device: its data, and unless `data_only` its metadata.
pub(crate) unsafe fn sync(fd: c_int, data_only: bool) -> Result<(), c_int> {
    ok(raw::sync(fd as u32, u32::from(data_only)))
}

/// Reads at `offset` into `buf`, leaving the position: how many were read.
pub(crate) unsafe fn pread(fd: c_int, buf: *mut u8, len: usize, offset: u64) -> Result<usize, c_int> {
    let mut read = 0u64;
    ok(raw::pread(fd as u32, buf, len, offset, &raw mut read))?;
    Ok(read as usize)
}

/// Writes `buf` at `offset`, leaving the position: how many were written.
pub(crate) unsafe fn pwrite(fd: c_int, buf: *const u8, len: usize, offset: u64) -> Result<usize, c_int> {
    let mut wrote = 0u64;
    ok(raw::pwrite(fd as u32, buf, len, offset, &raw mut wrote))?;
    Ok(wrote as usize)
}
