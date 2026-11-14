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

/// What the schedule promises of the wait it gives after waits that came to `waited`
/// nanoseconds: nothing once they have come to [`MOST_NS`]; else a wait that is positive, at
/// most [`CAP_NS`], at most what is left of [`MOST_NS`], and at least [`FIRST_NS`] unless it
/// is the last one, cut to end on the bound exactly.
pub const fn bounded(waited: u64, wait: Option<u64>) -> bool {
    match wait {
        None => waited >= MOST_NS,
        Some(wait) => {
            waited < MOST_NS
                && wait > 0
                && wait <= CAP_NS
                && wait <= MOST_NS - waited
                && (wait >= FIRST_NS || waited + wait == MOST_NS)
        }
    }
}

/// The invariant (proved: crates/blaze-core/lean/BlazeCore/Busy.lean): from any state, the
/// wait [`next`] gives is [`bounded`]. So every wait of an exec is positive and never passes
/// what is left of [`MOST_NS`]: the waits of [`while_busy`] end, on the bound exactly, in at
/// most `MOST_NS / FIRST_NS + 1` tries from wherever they are (rule bounded-progress), and no
/// arithmetic of [`next`] overflows or panics. Proved for every `tried` and `waited`, where
/// the test it stands beside followed one run from the start. Its control is
/// `the_oracle_catches_a_last_wait_not_cut`.
#[blaze::theorem]
pub fn next_is_bounded(tried: u32, waited: u64) -> bool {
    bounded(waited, next(tried, waited))
}

/// The reference model: the schedule as the loop its words describe, the first wait doubled
/// once for each wait made, kept to the cap each time, and cut to what is left of the bound.
/// [`next`] is this with the doubling done by one shift.
pub const fn doubling(tried: u32, waited: u64) -> Option<u64> {
    if waited >= MOST_NS {
        return None;
    }
    let mut wait = FIRST_NS;
    let mut doubled = 0;
    while doubled < tried {
        wait = if wait * 2 < CAP_NS { wait * 2 } else { CAP_NS };
        doubled += 1;
    }
    let left = MOST_NS - waited;
    Some(if wait < left { wait } else { left })
}

/// Whether two schedules give the same wait.
pub const fn same(wait: Option<u64>, reference: Option<u64>) -> bool {
    match wait {
        None => match reference {
            None => true,
            Some(_) => false,
        },
        Some(wait) => match reference {
            Some(reference) => wait == reference,
            None => false,
        },
    }
}

/// The refinement (proved: crates/blaze-core/lean/BlazeCore/Busy.lean): [`next`] is
/// [`doubling`], for every number of tries (past the shift's limit of 20 too) and every time
/// waited (past the bound too). Its control is `the_oracle_catches_a_doubling_never_capped`.
#[blaze::theorem]
pub fn next_is_the_doubling_schedule(tried: u32, waited: u64) -> bool {
    same(next(tried, waited), doubling(tried, waited))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A wait that runs past the bound, where the schedule cuts its last wait to end on it:
    /// [`bounded`] refuses it, so [`next_is_bounded`] is not true of every schedule.
    #[test]
    fn the_oracle_catches_a_last_wait_not_cut() {
        assert!(bounded(MOST_NS - 10, next(30, MOST_NS - 10)));
        assert!(!bounded(MOST_NS - 10, Some(CAP_NS)));
        assert!(!bounded(0, Some(0)));
        assert!(!bounded(0, None));
    }

    /// A doubling that is never kept to the cap is not the reference's wait.
    #[test]
    fn the_oracle_catches_a_doubling_never_capped() {
        assert!(same(next(11, 0), doubling(11, 0)));
        assert!(!same(Some(FIRST_NS << 11), doubling(11, 0)));
        assert!(!same(None, doubling(11, 0)));
    }

    /// The theorem functions run as built code on a few arguments: what Lean proves of their
    /// models for every argument.
    #[test]
    fn the_theorems_hold_where_they_are_run() {
        for tried in [0, 1, 9, 10, 19, 20, 21, 79] {
            for waited in [0, 1, FIRST_NS, MOST_NS - CAP_NS, MOST_NS - 1, MOST_NS, MOST_NS + 1, u64::MAX] {
                assert!(next_is_bounded(tried, waited), "{tried} tries, {waited} ns");
                assert!(next_is_the_doubling_schedule(tried, waited), "{tried} tries, {waited} ns");
            }
        }
    }
}
