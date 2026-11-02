//! When an exec that met `ETXTBSY` is made again: the file was just written, and a process
//! forked before its writer closed it still holds a copy of that descriptor until its own exec,
//! which is microseconds. Every spawn of blaze's that can meet it waits by this schedule: the
//! drivers' (`process`), and blaze's sandbox launcher's exec of the program. The same exec of
//! the same file each time, never another path or a copy; past [`MOST_NS`] it fails, naming
//! the file and the error.
//!
//! Waits double from [`FIRST_NS`] to [`CAP_NS`], the last one cut so that all of them add up to
//! [`MOST_NS`] exactly. Integers alone, so code that may not allocate or lock can follow it.

/// The first wait: about what the process holding the file takes to reach its exec.
pub const FIRST_NS: u64 = 50_000;
/// The longest single wait.
pub const CAP_NS: u64 = 50_000_000;
/// All the waits of one exec, together: past them the spawn fails.
pub const MOST_NS: u64 = 1_000_000_000;

/// The wait before the next exec, after `tried` waits that came to `waited` nanoseconds; `None`
/// once they have come to [`MOST_NS`].
pub const fn next(tried: u32, waited: u64) -> Option<u64> {
    if waited >= MOST_NS {
        return None;
    }
    let doubled = FIRST_NS << if tried < 20 { tried } else { 20 };
    let wait = if doubled < CAP_NS { doubled } else { CAP_NS };
    let left = MOST_NS - waited;
    Some(if wait < left { wait } else { left })
}

/// Waits out one wait of the schedule, `ns` from [`next`]: the one sleep of blaze's spawns, a
/// driver's timer, on a thread no job runs on (a driver's, or the launcher's before its exec).
/// A syscall alone, so the launcher may call it between its fork and its exec.
#[allow(clippy::disallowed_methods, reason = "the schedule's timer: no event says the writer's copy is closed")]
pub fn wait(ns: u64) {
    std::thread::sleep(std::time::Duration::from_nanos(ns));
}
