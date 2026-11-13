//! The lines of a memory limit, in one place: where the daemon lets go, where it gives back,
//! where a task that alone does not fit fails, and what the kernel is told. Every one is
//! derived here from the limit, so their order is one statement ([`Lines::ordered`]) and not
//! three constants in three crates.
//!
//! | line | at | what happens there | whose |
//! |---|---|---|---|
//! | [`Lines::release`] | limit - limit/8 | a build's front end releases what no task to come needs ([`crate::admission::LINE`]) | the daemon's |
//! | [`Lines::give`] | limit - limit/16 | the give-back ladder begins (`serve::job::giving`) | the daemon's |
//! | [`Lines::fail`] | limit - limit/32 | a task running alone, with nothing left to give, fails typed ([`crate::footprint::past_ceiling`]) | the daemon's, the typed exit |
//! | [`Kernel::throttle`] | nowhere (`memory.high` = `max`) | the kernel would slow every allocating thread | the kernel's |
//! | [`Kernel::kill`] | the limit | `memory.max` or jetsam ends the daemon; its own watch ends it typed (`MEMORY_CEILING`) | the kernel's and the guard's |
//!
//! The order that must hold: a line past which the kernel slows or stops the daemon's running
//! threads has a typed exit at or below it. On 2026-11-12 it did not: `memory.high` was 15/16
//! of the ceiling (139.69 of 149 GiB) and the lone task's failure 31/32 (144.34), so a cold
//! build of tempo sat between them for 43 minutes, its footprint flat at 141 to 142 GiB, every
//! allocating thread throttled and nothing able to fail, until systemd killed the daemon.
//!
//! Why the throttle is gone rather than moved up to the fail line: `memory.high` slows a
//! cgroup so that reclaim can catch up, and the daemon has nothing the kernel can reclaim (its
//! memory is anonymous and its cgroup has no swap, `memory.swap.max` = 0), so the throttle only
//! stretches time, which is the slow death docs/memory-budget.md is written against. Put at
//! the fail line it would still slow the loop's own thread, which is the one that takes that
//! exit. What holds the daemon under its limit is the daemon: these lines, read against the
//! footprint each turn. The kernel is told only where to kill
//! (docs/v2/plan/design/end-state/memory.md 4.2), and a kernel kill is a defect to count.

/// What the kernel is told of a limit ([`Lines::kernel`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kernel {
    /// Where the kernel slows every thread that allocates: cgroup v2's `memory.high`. `None`
    /// is `max`: nowhere.
    pub throttle: Option<u64>,
    /// Where the kernel ends the daemon: cgroup v2's `memory.max`, or macOS's fatal
    /// memorystatus limit.
    pub kill: u64,
}

/// The lines of one limit in bytes; `u64::MAX` is no limit, and every line of it is
/// `u64::MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lines {
    limit: u64,
}

/// The give-back line: the limit less this part of it, a sixteenth. Nothing gates an
/// allocation, so the ladder begins before the limit is reached.
const GIVE: u64 = 16;

/// The fail line: the limit less this part of it, a thirty-second, halfway from the give-back
/// line to the limit, so a task that alone does not fit has until the limit to reach a query
/// call and unwind before the ceiling's guard would end the daemon.
const FAIL: u64 = 32;

impl Lines {
    pub const fn of(limit: u64) -> Lines {
        Lines { limit }
    }

    /// The limit itself.
    pub const fn limit(self) -> u64 {
        self.limit
    }

    /// The limit less its `part`th; no limit stays none.
    const fn less(self, part: u64) -> u64 {
        match self.limit {
            u64::MAX => u64::MAX,
            limit => limit - limit / part,
        }
    }

    /// Where a build's front end releases ([`crate::admission::Pressure::squeezed`]).
    pub const fn release(self) -> u64 {
        self.less(crate::admission::LINE)
    }

    /// Where the give-back ladder begins: the limit its planner is told.
    pub const fn give(self) -> u64 {
        self.less(GIVE)
    }

    /// Where a task running alone, with nothing left to give, fails with a typed error naming
    /// it: the typed exit of every line above.
    pub const fn fail(self) -> u64 {
        self.less(FAIL)
    }

    /// What the kernel is told when this limit is the daemon's ceiling: where to kill, and
    /// no throttle under it (the module's doc says why).
    pub const fn kernel(self) -> Kernel {
        Kernel { throttle: None, kill: self.limit }
    }

    /// Whether the lines are in order with what the kernel is told.
    pub const fn ordered(self) -> bool {
        self.ordered_with(self.kernel())
    }

    /// Whether the lines are in order with `kernel`: the daemon lets go, then gives back, then
    /// fails the lone task, each at or before the next; and the kernel neither slows nor ends
    /// the daemon below that typed failure, nor ends it anywhere but at the limit.
    pub const fn ordered_with(self, kernel: Kernel) -> bool {
        let slows = match kernel.throttle {
            Some(at) => at,
            None => kernel.kill,
        };
        self.release() <= self.give()
            && self.give() <= self.fail()
            && self.fail() <= slows
            && slows <= kernel.kill
            && kernel.kill == self.limit
    }
}

/// Whether [`Lines::ordered`] holds for every limit of a range wide enough to meet each way the
/// divisions round: every byte count to 1024, every power of two with its two neighbours, every
/// whole GiB to 4096 (a machine of 4 TiB), and no limit. And that a larger limit never has a
/// lower fail line, so a limit under the ceiling (the machine ledger's, `--memory-gb`) fails
/// its lone task no later than the ceiling's own would.
const fn ordered_for_every_limit() -> bool {
    const fn holds(limit: u64) -> bool {
        Lines::of(limit).ordered()
            && (limit == 0 || Lines::of(limit - 1).fail() <= Lines::of(limit).fail())
    }
    let mut limit = 0;
    while limit <= 1024 {
        if !holds(limit) {
            return false;
        }
        limit += 1;
    }
    let mut bit = 0;
    while bit < 64 {
        let power = 1u64 << bit;
        if !holds(power - 1) || !holds(power) || !holds(power + 1) {
            return false;
        }
        bit += 1;
    }
    let mut gib = 1;
    while gib <= 4096 {
        if !holds(gib << 30) {
            return false;
        }
        gib += 1;
    }
    holds(u64::MAX - 1) && holds(u64::MAX)
}

// Evaluated by every build of this crate, not only by its tests: a line moved out of order
// does not compile.
const _: () = assert!(
    ordered_for_every_limit(),
    "a memory line is out of order: the kernel would slow or end the daemon below its typed failure (crates/blaze-graph/src/lines.rs)"
);

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// The invariant: for every limit, each line that slows or stops the daemon's running
    /// threads (the kernel's throttle, the kernel's kill) has a typed exit at or below it: the
    /// daemon lets go at or before it gives back, gives back at or before it fails a lone
    /// task, and the kernel acts nowhere below that failure and kills only at the limit.
    #[test]
    fn every_line_that_slows_the_daemon_has_a_typed_exit_at_or_below_it() {
        const { assert!(ordered_for_every_limit()) };
        // The ceiling of 2026-11-12, in the numbers its finding quotes.
        let lines = Lines::of(149 * GIB);
        assert_eq!(lines.give(), 149 * GIB - 149 * GIB / 16);
        assert_eq!(lines.fail(), 149 * GIB - 149 * GIB / 32);
        assert_eq!(lines.kernel(), Kernel { throttle: None, kill: 149 * GIB });
        assert!(lines.release() < lines.give() && lines.give() < lines.fail());
        assert!(lines.fail() < lines.limit());
        // No limit has no line.
        let none = Lines::of(u64::MAX);
        assert_eq!((none.release(), none.give(), none.fail()), (u64::MAX, u64::MAX, u64::MAX));
    }

    /// The planted bug is what the tree had: `memory.high` at 15/16 of the ceiling, under the
    /// fail line at 31/32. The order must refuse it for every limit where the two differ.
    #[test]
    fn the_oracle_catches_a_kernel_throttle_under_the_fail_line() {
        const fn refused_for_every_gib() -> bool {
            let mut gib = 1;
            while gib <= 4096 {
                let limit = gib << 30;
                let old = Kernel { throttle: Some(limit / 16 * 15), kill: limit };
                if Lines::of(limit).ordered_with(old) {
                    return false;
                }
                gib += 1;
            }
            true
        }
        const { assert!(refused_for_every_gib()) };
        let limit = 149 * GIB;
        let old = Kernel { throttle: Some(limit / 16 * 15), kill: limit };
        // 139.69 GiB under 144.34 GiB: the band of 2026-11-12.
        assert!(old.throttle.unwrap() < Lines::of(limit).fail());
        assert!(!Lines::of(limit).ordered_with(old));
        // A throttle at the fail line or above it is in order; a kill under the limit is not.
        let at_fail = Kernel { throttle: Some(Lines::of(limit).fail()), kill: limit };
        assert!(Lines::of(limit).ordered_with(at_fail));
        assert!(!Lines::of(limit).ordered_with(Kernel { throttle: None, kill: limit - 1 }));
    }
}
