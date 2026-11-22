//! The C library's scalar typedefs (`pid_t`, `off_t`, `socklen_t`, ...), per system, for the
//! `#[repr(C)]` types and declarations of the other `posix` areas and for the crates that port
//! from `libc::`. Each is the 64-bit target's: glibc on Linux (aarch64 and x86-64, the same on
//! both) and libSystem on macOS (arm64).
//!
//! The C scalar types themselves (`c_int`, `c_long`, `c_char`, `c_void`, ...) are not declared
//! here: `libc::c_int` and its kin are `core::ffi::c_int` and its kin, re-exported below one
//! per line so each says what it replaces; a port may name either path.
//!
//! Sources: glibc's `bits/types.h` and `bits/typesizes.h`; macOS's `sys/_types.h`,
//! `sys/_types/*.h`, `arm/_types.h` and `i386/_types.h`.

#![allow(non_camel_case_types, reason = "the C library's own names, so a port from `libc::` changes only the path")]

/// C's `int` (32 bits on every target): core's own.
/// replaces libc::c_int (blaze_sys::posix::types::c_int, which is core::ffi::c_int)
pub use core::ffi::c_int;

/// C's `unsigned int`: core's own.
/// replaces libc::c_uint (blaze_sys::posix::types::c_uint, which is core::ffi::c_uint)
pub use core::ffi::c_uint;

/// C's `long` (64 bits on the 64-bit Unix targets): core's own.
/// replaces libc::c_long (blaze_sys::posix::types::c_long, which is core::ffi::c_long)
pub use core::ffi::c_long;

/// C's `unsigned long`: core's own.
/// replaces libc::c_ulong (blaze_sys::posix::types::c_ulong, which is core::ffi::c_ulong)
pub use core::ffi::c_ulong;

/// C's `short` (16 bits): core's own.
/// replaces libc::c_short (blaze_sys::posix::types::c_short, which is core::ffi::c_short)
pub use core::ffi::c_short;

/// C's `char` (signed on macOS arm64 and x86-64 Linux, unsigned on aarch64 Linux): core's own,
/// which follows the target the same way.
/// replaces libc::c_char (blaze_sys::posix::types::c_char, which is core::ffi::c_char)
pub use core::ffi::c_char;

/// C's `void` behind a pointer: core's own.
/// replaces libc::c_void (blaze_sys::posix::types::c_void, which is core::ffi::c_void)
pub use core::ffi::c_void;

/// A process id: `int` on both systems (`__pid_t`; `__darwin_pid_t`).
/// replaces libc::pid_t
pub type pid_t = i32;

/// A user id: `unsigned int` on both (`__uid_t`; `__darwin_uid_t`).
/// replaces libc::uid_t
pub type uid_t = u32;

/// A group id: `unsigned int` on both (`__gid_t`; `__darwin_gid_t`).
/// replaces libc::gid_t
pub type gid_t = u32;

/// The id `waitid` and `getpriority` take, a pid, uid or gid: `unsigned int` on both
/// (`__id_t`; `__darwin_id_t`).
/// replaces libc::id_t
pub type id_t = c_uint;

/// A file offset: 64 bits on both (glibc's `__off_t` on a 64-bit target; `__darwin_off_t`).
/// replaces libc::off_t
pub type off_t = i64;

/// Seconds since the epoch: 64 bits on both (`__time_t`, `long`; `__darwin_time_t`, `long`).
/// replaces libc::time_t
pub type time_t = i64;

/// Microseconds, a `timeval`'s `tv_usec`: `long` on Linux (`__suseconds_t`).
/// replaces libc::suseconds_t
#[cfg(target_os = "linux")]
pub type suseconds_t = i64;

/// Microseconds, a `timeval`'s `tv_usec`: `__int32_t` on macOS (`__darwin_suseconds_t`), so a
/// macOS `timeval` is 16 bytes with 4 of padding.
/// replaces libc::suseconds_t
#[cfg(target_os = "macos")]
pub type suseconds_t = i32;

/// The size of an object: `size_t`, a `usize` on every target.
/// replaces libc::size_t
pub type size_t = usize;

/// A size or a -1: `ssize_t`, an `isize` on every target.
/// replaces libc::ssize_t
pub type ssize_t = isize;

/// An integer as wide as a pointer: `uintptr_t`.
/// replaces libc::uintptr_t
pub type uintptr_t = usize;

/// A socket address's length: `unsigned int` on both (`__socklen_t`; `__darwin_socklen_t`).
/// replaces libc::socklen_t
pub type socklen_t = u32;

/// A signal handler as a word: `SIG_DFL` (0), `SIG_IGN` (1), `SIG_ERR` (all ones) or a
/// function's address, which is what the `sa_sigaction` of a `sigaction` and `signal` take and
/// give on both systems.
/// replaces libc::sighandler_t
pub type sighandler_t = usize;

/// A socket address's family: `unsigned short` on Linux (`sa_family_t`, bits/sockaddr.h).
/// replaces libc::sa_family_t
#[cfg(target_os = "linux")]
pub type sa_family_t = u16;

/// A socket address's family: `__uint8_t` on macOS (sys/_types/_sa_family_t.h), after the
/// one-byte `sa_len`.
/// replaces libc::sa_family_t
#[cfg(target_os = "macos")]
pub type sa_family_t = u8;

/// The count of a `poll`: `unsigned long` on Linux (sys/poll.h).
/// replaces libc::nfds_t
#[cfg(target_os = "linux")]
pub type nfds_t = core::ffi::c_ulong;

/// The count of a `poll`: `unsigned int` on macOS (poll.h).
/// replaces libc::nfds_t
#[cfg(target_os = "macos")]
pub type nfds_t = c_uint;

/// A clock's id: `int` on Linux (`__clockid_t`).
/// replaces libc::clockid_t
#[cfg(target_os = "linux")]
pub type clockid_t = core::ffi::c_int;

/// A clock's id: an enum, `unsigned int` wide, on macOS (time.h's `clockid_t`).
/// replaces libc::clockid_t
#[cfg(target_os = "macos")]
pub type clockid_t = c_uint;

/// A device number: 64 bits on Linux (`__dev_t`).
/// replaces libc::dev_t
#[cfg(target_os = "linux")]
pub type dev_t = u64;

/// A device number: `__int32_t` on macOS (`__darwin_dev_t`).
/// replaces libc::dev_t
#[cfg(target_os = "macos")]
pub type dev_t = i32;

/// A file's mode bits: `unsigned int` on Linux (`__mode_t`).
/// replaces libc::mode_t
#[cfg(target_os = "linux")]
pub type mode_t = u32;

/// A file's mode bits: `__uint16_t` on macOS (`__darwin_mode_t`).
/// replaces libc::mode_t
#[cfg(target_os = "macos")]
pub type mode_t = u16;

/// The typedefs compared with the `libc` crate's, once: a typedef that differs fails to
/// coerce. Deleted with the `libc` dev-dependency (D180).
#[cfg(test)]
mod tests {
    use super::*;

    /// Each typedef is the same type as `libc`'s: a function from one to the other is the
    /// identity, which type-checks only when they are equal.
    #[test]
    fn every_typedef_is_libcs() {
        fn same<T>(value: T) -> T {
            value
        }
        let _: libc::pid_t = same::<pid_t>(0);
        let _: libc::uid_t = same::<uid_t>(0);
        let _: libc::gid_t = same::<gid_t>(0);
        let _: libc::id_t = same::<id_t>(0);
        let _: libc::off_t = same::<off_t>(0);
        let _: libc::time_t = same::<time_t>(0);
        let _: libc::suseconds_t = same::<suseconds_t>(0);
        let _: libc::size_t = same::<size_t>(0);
        let _: libc::ssize_t = same::<ssize_t>(0);
        let _: libc::uintptr_t = same::<uintptr_t>(0);
        let _: libc::socklen_t = same::<socklen_t>(0);
        let _: libc::sighandler_t = same::<sighandler_t>(0);
        let _: libc::sa_family_t = same::<sa_family_t>(0);
        let _: libc::nfds_t = same::<nfds_t>(0);
        let _: libc::clockid_t = same::<clockid_t>(0);
        let _: libc::dev_t = same::<dev_t>(0);
        let _: libc::mode_t = same::<mode_t>(0);
        let _: libc::c_int = same::<c_int>(0);
        let _: libc::c_uint = same::<c_uint>(0);
        let _: libc::c_long = same::<c_long>(0);
        let _: libc::c_ulong = same::<c_ulong>(0);
        let _: libc::c_short = same::<c_short>(0);
        let _: libc::c_char = same::<c_char>(0);
        let _: *const libc::c_void = same::<*const c_void>(core::ptr::null());
    }
}
