//! The one spin of blaze's waits (docs/v2/plan/design/concurrency.md): a wait for a step another
//! running thread is in the middle of (a slot it is writing, a list it is walking) spins a few
//! rounds, doubling, then yields its thread each round. A wait for anything longer is the
//! scheduler's (`blaze_graph::sched::wait` on a key, a counter or a job) or a lock's, which park.
//! A crate of its own, with no dependency, so that every crate below the scheduler spins the
//! same way.
//!
//! Under `--cfg blaze_check` each round is a schedule point of the model checker, so a wait that
//! never ends is a finding there, not a hang.

use std::time::{Duration, Instant};

#[cfg(blaze_check)]
use blaze_check::{hint, thread};
#[cfg(not(blaze_check))]
use std::{hint, thread};

/// Rounds that spin, the `n`th `2^n` times, before rounds yield.
const SPINS: u32 = 6;

/// Rounds that yield before [`SpinWait::spin`] says to park.
const YIELDS: u32 = 10;

/// Rounds of [`SpinWait::until`] before it gives up: seconds of yields, far past any step it
/// is for.
const LIMIT: u32 = 1 << 24;

/// A bounded back-off for one wait.
#[derive(Default)]
pub struct SpinWait {
    round: u32,
}

impl SpinWait {
    pub const fn new() -> SpinWait {
        SpinWait { round: 0 }
    }

    /// One more round: `false` once the wait has spun and yielded its bound, when the caller
    /// should park on something that wakes it.
    #[allow(clippy::disallowed_methods, reason = "the one bounded spin, which the ban points every other spin to")]
    pub fn spin(&mut self) -> bool {
        if self.round >= SPINS + YIELDS {
            return false;
        }
        if self.round < SPINS {
            for _ in 0..1u32 << self.round {
                hint::spin_loop();
            }
        } else {
            thread::yield_now();
        }
        self.round += 1;
        true
    }

    /// One more round of spinning, never yielding: `false` once the spins are done, for a wait
    /// that has something to park on and spins only in case what it waits for is about to end.
    pub fn pause(&mut self) -> bool {
        self.round < SPINS && self.spin()
    }

    /// One more round of spinning, never yielding, the rounds doubling to the longest: for a
    /// spin its caller caps by the clock (a window after a wake, in which what it waits for is
    /// likely), which then parks on something that wakes it.
    #[allow(clippy::disallowed_methods, reason = "the one bounded spin, which the ban points every other spin to")]
    pub fn relax(&mut self) {
        let round = self.round.min(SPINS - 1);
        for _ in 0..1u32 << round {
            hint::spin_loop();
        }
        self.round = self.round.saturating_add(1);
    }

    /// Waits until `done`, for a step that a thread now running ends in a bounded time and that
    /// has nothing to park on (`what`, for the panic): spins, then yields the thread each round.
    /// Past [`LIMIT`] rounds the step is not bounded after all, and it panics naming `what`.
    pub fn until(what: &str, mut done: impl FnMut() -> bool) {
        let mut spin = SpinWait::new();
        let mut rounds = 0u32;
        while !done() {
            rounds += 1;
            assert!(rounds < LIMIT, "a wait for {what} spun and yielded {LIMIT} rounds: the step it waits for is stuck");
            if !spin.spin() {
                thread::yield_now();
            }
        }
    }

    /// Waits until `done`, as [`SpinWait::until`] does, for at most `long` of the wall clock: for
    /// a test or a tool waiting on another thread's progress, which may take longer than a step.
    /// Past `long` it panics naming `what`.
    #[allow(clippy::disallowed_methods, reason = "a test's or a tool's limit on the wall clock, which no core reads")]
    pub fn within(what: &str, long: Duration, mut done: impl FnMut() -> bool) {
        let until = Instant::now() + long;
        let mut spin = SpinWait::new();
        while !done() {
            assert!(Instant::now() < until, "timed out after {long:?} waiting for {what}");
            if !spin.spin() {
                thread::yield_now();
            }
        }
    }
}

/// Keeps this thread busy for `long`, as a job computing that long would: for a test or a
/// bench that needs a run under way on its thread for a while, which a sleep would fake by
/// holding the thread idle.
#[allow(clippy::disallowed_methods, reason = "a test's or a bench's wall-clock work, which no core reads")]
pub fn busy(long: Duration) {
    let until = Instant::now() + long;
    while Instant::now() < until {
        std::hint::black_box(());
    }
}
