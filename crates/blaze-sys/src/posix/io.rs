//! Descriptors, memory maps, waits on descriptors, clocks and randomness: the part of the C
//! library's surface (D180) that `libc::mmap`, `libc::poll`, `libc::fcntl`, `libc::kevent`,
//! `libc::clock_gettime` and their constants and structs stood for.
//!
//! One path for both systems where the concept is the same: `posix::io::mmap`,
//! `posix::io::OpenFlags`, `posix::io::poll` name the same operation on Linux and macOS, with
//! each system's own numbers behind them. On Linux, where [`crate::linux`] already has the call
//! by raw system call, the item here IS that one (`pub use`), so nothing is declared twice
//! (rule 30); on macOS it is declared here over libSystem, through the [`crate::macos::cold`]
//! table where the symbol is in it. A call one system lacks (`kqueue` on Linux, `memfd_create`
//! on macOS) is under that system's `cfg` only, and a caller names the system it is for.
//!
//! On Linux this module names no C-library symbol (D180): every item is a raw system call of
//! [`crate::linux`] (`fcntl`, `dup`, `dup2`, `ioctl`, `preadv`, `fallocate`, `memfd_create`,
//! `mincore` and `msync` in `linux::fdctl`), re-exported. `localtime` has no kernel form (the
//! zone database is the C library's reading) and is macOS-only.
//!
//! Every item says what it replaces (`/// replaces libc::mmap`); a re-export says it for the
//! item it re-exports. Errors are [`crate::Errno`]: from the raw call's result on Linux, from
//! `errno` (`posix::signal::errno`, macOS) after a C call that said -1.

mod event;
mod fd;
mod mem;
mod random;
mod real;
mod time;

pub use event::*;
pub use fd::*;
pub use mem::*;
pub use random::*;
pub use real::*;
pub use time::*;

#[cfg(target_os = "macos")]
use core::ffi::c_int;

#[cfg(target_os = "macos")]
use crate::Errno;

/// A C call's `int` result: the value, or the thread's `errno` where it said -1 (any negative
/// value: none of these calls returns another). macOS: Linux has no C call and no `errno`.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
fn said(result: c_int) -> Result<c_int, Errno> {
    if result < 0 { Err(super::signal::errno()) } else { Ok(result) }
}

/// A C call's `ssize_t` result: the count, or the thread's `errno` where it said -1. macOS.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
fn counted(result: isize) -> Result<usize, Errno> {
    if result < 0 { Err(super::signal::errno()) } else { Ok(result as usize) }
}

/// A count of items passed to a C call as its `int`: [`Errno::Inval`] past `c_int::MAX`, which
/// the call would refuse the same way. macOS.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
fn c_count(len: usize) -> Result<c_int, Errno> {
    c_int::try_from(len).map_err(|_| Errno::Inval)
}

#[cfg(test)]
mod tests;
