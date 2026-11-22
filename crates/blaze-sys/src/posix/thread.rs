//! Threads, the dynamic loader, and what the process asks of its host (D180): pthreads and
//! their keys, `dlopen`/`dlsym`/`dladdr` and the image lists, `sysconf`, `uname`, the user
//! database, resource limits and usage, CPU affinity, and on macOS the Mach and libproc calls
//! the profilers read.
//!
//! On Linux nothing here references a C-library symbol: every call is the kernel's, by system
//! call ([`crate::linux::host`], [`crate::linux::getpid`], [`crate::linux::sample::gettid`]);
//! the page size and clock ticks are `crate::facts::Aux`'s, handed in. What has no kernel form (pthreads, the
//! dynamic loader, `getpwuid_r` and `passwd`, `sysconf`) is offered on macOS only; each file's
//! head says what a Linux caller uses instead. On macOS the floor is libSystem, declared here
//! or through the [`crate::macos::cold`] table (`getpid`, `geteuid`, `getrlimit`,
//! `pthread_create`, `sysctlbyname`, ...), which the functions here call or re-export with the
//! header's own signature.
//!
//! Each item keeps its C name and says what it replaces (`/// replaces libc::pthread_self`), so
//! a port greps `replaces libc::<name>`.
//!
//! Four files, all re-exported here so every path is `blaze_sys::posix::thread::<name>`:
//!
//! - `pthread`: on macOS `pthread_t`, its attribute, mutex and condition types, keys, the
//!   calling thread's stack, its name, its signal mask, QoS; on both, [`thread_id`].
//! - `dl` (macOS): `dlopen`, `dlsym`, `dlerror`, `dladdr` and `Dl_info`, `_dyld_*`.
//! - `host`: ids, the process group and session, `uname`,
//!   `rlimit`, `rusage`, Linux's `cpu_set_t` and affinity; macOS's `sysconf`, `confstr`,
//!   `passwd`, `AT_*` numbers and `sysctlbyname`.
//! - `mach` (macOS): Mach ports, `thread_info`, `task_info`, `mach_task_self`, libproc's
//!   `proc_pidinfo` and `proc_pid_rusage`.
//!
//! On macOS the C library's convention is kept in the declarations (a call returns -1 and sets
//! `errno`, or a pthread call returns the error's number); the typed wrappers beside them
//! return `Result<_, Errno>`, the error read through `crate::posix::signal::errno` (macOS
//! only). On Linux the typed wrappers take the error from the system call's result.
//!
//! Not declared here, on purpose: `syscall` and the `SYS_*` numbers. blaze-sys has no
//! `syscall(n, ...)` passthrough; each call is its typed wrapper in [`crate::linux`] and its
//! number is [`crate::linux::nr`]'s.

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    reason = "the declarations keep the C names, so a port and the man page read alike"
)]

#[cfg(target_os = "macos")]
mod dl;
mod host;
#[cfg(target_os = "macos")]
mod mach;
mod pthread;

#[cfg(target_os = "macos")]
pub use dl::*;
pub use host::*;
#[cfg(target_os = "macos")]
pub use mach::*;
pub use pthread::*;

/// The comparison with the `libc` crate: every type's size, alignment and read offsets, every
/// constant's value, and the signatures that are the same. Deleted with the `libc`
/// dev-dependency (D180).
#[cfg(test)]
mod tests;
