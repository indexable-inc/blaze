//! A process's priority (`getpriority`, `setpriority`) and the machine's load
//! (`getloadavg`).

use core::ffi::c_int;

#[cfg(target_os = "macos")]
use super::said;
use crate::Errno;
#[cfg(target_os = "macos")]
use crate::posix::types::id_t;

/// The kind of id `getpriority` and `setpriority` take: glibc's `__priority_which_t`
/// (`unsigned int`), macOS's `int`.
#[cfg(target_os = "linux")]
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type priority_which_t = core::ffi::c_uint;

/// The kind of id `getpriority` and `setpriority` take: glibc's `__priority_which_t`
/// (`unsigned int`), macOS's `int`.
#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type priority_which_t = c_int;

/// `PRIO_PROCESS` (sys/resource.h): the id is a process's (0, this one; on Linux, the calling
/// thread alone).
/// replaces libc::PRIO_PROCESS
pub const PRIO_PROCESS: priority_which_t = 0;

/// The kernel's forms on Linux, by raw system call: `getpriority` (the nice value, converted
/// from the kernel's `20 - nice`), `setpriority`, both taking a [`PriorityOf`]; `sysinfo`,
/// whose loads are `getloadavg`'s (see [`load_average`]).
/// replaces libc::getpriority
/// replaces libc::setpriority
/// replaces libc::getloadavg
#[cfg(target_os = "linux")]
pub use crate::linux::task::{PriorityOf, Sysinfo, getpriority, setpriority, sysinfo};

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `int getpriority(int which, id_t who)` (sys/resource.h): the nice value, -20 to 19.
    /// -1 is a value too: clear `errno` first to tell it from an error.
    /// replaces libc::getpriority
    pub fn getpriority(which: priority_which_t, who: id_t) -> c_int;

    /// `int setpriority(int which, id_t who, int prio)` (sys/resource.h): the nice value set.
    /// replaces libc::setpriority
    pub fn setpriority(which: priority_which_t, who: id_t, prio: c_int) -> c_int;

    /// `int getloadavg(double loadavg[], int nelem)` (stdlib.h): up to `nelem` (3) load
    /// averages written; how many, or -1.
    /// replaces libc::getloadavg
    pub fn getloadavg(loadavg: *mut f64, nelem: c_int) -> c_int;
}

/// `setpriority(PRIO_PROCESS, 0, nice)`: this process's (on Linux, this thread's) nice value
/// set. [`Errno::Acces`] for a lower value without privilege.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_nice(nice: c_int) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    return setpriority(PriorityOf::Process, 0, nice);
    // SAFETY: the call takes numbers and no memory.
    #[cfg(target_os = "macos")]
    return said(unsafe { setpriority(PRIO_PROCESS, 0, nice) }).map(drop);
}

/// The machine's load averages over 1, 5 and 15 minutes: the kernel's, from `sysinfo` (no
/// file read). `None` where the call failed (a filter that refuses it).
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn load_average() -> Option<[f64; 3]> {
    sysinfo().ok().map(|info| info.load_averages())
}

/// The machine's load averages over 1, 5 and 15 minutes: `getloadavg` into three. `None`
/// where the system could not say all three.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn load_average() -> Option<[f64; 3]> {
    let mut averages = [0.0f64; 3];
    // SAFETY: `averages` is three live doubles; the call writes at most the three it is told.
    let got = unsafe { getloadavg(averages.as_mut_ptr(), 3) };
    (got == 3).then_some(averages)
}
