//! The clock a sans-IO core reads, as a reference model: time that never runs back.
//!
//! A core is handed the time (`blaze_core::wait::Clock`, a completion's `now`); it never asks
//! the kernel. So the world is one number, the last reading in nanoseconds, and its one rule:
//! the next reading is not earlier. A timer is the same rule from the other side: it is told
//! fired at a reading that is not before its deadline.

#![blaze::sans_io]

/// The reference model: a monotonic clock, as the last reading a core was given, in
/// nanoseconds since the clock's start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clock {
    /// The last reading.
    pub now: u64,
}

impl Clock {
    /// A clock at its start.
    pub const fn new() -> Clock {
        Clock { now: 0 }
    }

    /// Whether the clock may read `reading` next: any time that is not before the last one.
    /// The same reading twice is allowed; a clock's grain is coarser than a core's steps.
    pub const fn allows(&self, reading: u64) -> bool {
        reading >= self.now
    }

    /// The clock after it read `reading`.
    pub const fn read(&mut self, reading: u64) {
        self.now = reading;
    }

    /// Whether a timer set for `deadline` may be told fired at `reading`: at a time the clock
    /// may read that is not before the deadline. Nothing bounds how late.
    pub const fn fires(&self, deadline: u64, reading: u64) -> bool {
        self.allows(reading) && reading >= deadline
    }

    /// The deadline of a timer set now for `after` nanoseconds: `u64::MAX`, which no reading
    /// passes, where the sum does not fit.
    pub const fn deadline(&self, after: u64) -> u64 {
        self.now.saturating_add(after)
    }
}

impl Default for Clock {
    fn default() -> Clock {
        Clock::new()
    }
}

/// The invariant: a timer the clock allows fired did not fire early, and the clock after it
/// has not run back: for every clock, wait and reading, if a timer set now for `after` may be
/// told fired at `reading`, then `reading` is at least `after` past now (or the sum did not
/// fit and the reading is the last one there is), and after the reading the clock allows no
/// reading before it.
#[blaze::theorem]
pub fn a_timer_never_fires_early(now: u64, after: u64, reading: u64, later: u64) -> bool {
    let mut clock = Clock { now };
    let deadline = clock.deadline(after);
    if !clock.fires(deadline, reading) {
        return true;
    }
    let waited = reading - now;
    clock.read(reading);
    (waited >= after || reading == u64::MAX) && (!clock.allows(later) || later >= reading)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The theorem's function, run where it is stated, on the edges: the start, a wait that
    /// does not fit, the same reading twice.
    #[test]
    fn the_theorem_holds_where_it_is_run() {
        let edges = [0, 1, 2, 1_000, u64::MAX - 1, u64::MAX];
        for now in edges {
            for after in edges {
                for reading in edges {
                    for later in edges {
                        assert!(
                            a_timer_never_fires_early(now, after, reading, later),
                            "{now} {after} {reading} {later}"
                        );
                    }
                }
            }
        }
    }

    /// The planted bug: a timer told fired at any reading the clock allows, its deadline not
    /// looked at.
    fn early(now: u64, after: u64, reading: u64) -> bool {
        let clock = Clock { now };
        !clock.allows(reading) || reading - now >= after
    }

    #[test]
    fn the_oracle_catches_a_timer_fired_before_its_deadline() {
        // A timer for 10 ns told fired after 3: the clock allows the reading, the rule does not.
        assert!(!early(5, 10, 8));
        assert!(!Clock { now: 5 }.fires(Clock { now: 5 }.deadline(10), 8));
    }
}
