//! When an exec that met `ETXTBSY` is made again: the file was just written, and a process
//! forked before its writer closed it still holds a copy of that descriptor until its own exec,
//! which is microseconds. Every spawn of blaze's that can meet it waits by this schedule: the
//! drivers' (`process`), `blaze debug sql`'s DuckDB, and blaze's sandbox launcher's exec of the
//! program. The same exec of the same file each time, never another path or a copy; past
//! [`MOST_NS`] it fails, naming the file and the error.
//!
//! Waits double from [`FIRST_NS`] to [`CAP_NS`], the last one cut so that all of them add up to
//! [`MOST_NS`] exactly. Integers alone, so code that may not allocate or lock can follow it.

use std::io;

use crate::wait::Clock as _;

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

/// Waits out one wait of the schedule, `ns` from [`next`]: the sleep of blaze's sandbox
/// launcher before its exec, which stays off io_uring (its seccomp filter refuses it to what it
/// launches) and may make nothing between its fork and its exec. A syscall alone.
#[allow(
    clippy::disallowed_methods,
    reason = "the schedule's timer: no event says the writer's copy is closed"
)]
pub fn wait(ns: u64) {
    std::thread::sleep(std::time::Duration::from_nanos(ns));
}

/// `start` made again for as long as its exec fails with `ETXTBSY`, waiting as [`next`]
/// says, each wait a [`crate::wait::sleep_until`] (a ring timeout on Linux). In the parent,
/// never in a child: each try is a new child, whose descriptors are
/// copied from the parent's as they are then. A Linux child that retried its own `execve`
/// would keep the copy of the writer's descriptor it was made with, and never run. The
/// calling thread waits, as it waits for the spawn anyway: at most [`MOST_NS`] in all,
/// and only while the file is busy.
pub fn while_busy<T>(mut start: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let (mut tried, mut waited) = (0, 0);
    loop {
        let error = match start() {
            Ok(it) => return Ok(it),
            Err(error) => error,
        };
        // Of every system call a spawn makes, only exec says `ETXTBSY`; by kind, since the
        // Linux spawn names the step that failed.
        if error.kind() != io::ErrorKind::ExecutableFileBusy {
            return Err(error);
        }
        let Some(ns) = next(tried, waited) else {
            return Err(io::Error::new(
                error.kind(),
                // alloc: an error path.
                format!(
                    "still open for writing after {} ms of tries: {error}",
                    MOST_NS / 1_000_000
                ),
            ));
        };
        // A driver's thread or a spawn's caller, never the launcher (which follows the
        // schedule with [`wait`]): the deadline wait, a ring timeout on Linux.
        let clock = crate::wait::Monotonic::new();
        let until = crate::wait::Deadline::after(clock.now(), std::time::Duration::from_nanos(ns));
        crate::wait::sleep_until(
            "an exec's file to be let go by its writer (ETXTBSY)",
            until,
            &clock,
        )
        .map_err(io::Error::from)?;
        tried += 1;
        waited += ns;
    }
}
