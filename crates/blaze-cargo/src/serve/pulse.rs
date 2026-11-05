//! The loop's pulse as threads outside it read it ([`Pulse`]), the judge of a loop that
//! stopped turning ([`LoopWatch`]) and the judge of a footprint against the daemon's memory
//! ceiling ([`CeilingWatch`]): sans-IO cores the stop's guard (`serve::stop`) drives.
//!
//! The loop is the one thread that reaps the daemon's children and hands their completions on
//! (blaze-core's drivers reap a child only as their `poll` sees it exit). A loop stuck inside a
//! turn (it applied a write whose maintain fanned out to pool workers deadlocked on a proc-macro
//! session, lane reaphang, 2026-11-03) left its build scripts and sandbox launchers zombies and
//! its clients at "Compiling", silently: its own watchdog, its memory log and its SIGUSR1 dump
//! all run between turns. The guard watches the turns from a thread of its own.

use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(all(unix, feature = "native"))]
use std::time::Duration;

/// The loop's progress, written by the loop at the end of each turn and read by the stop's
/// guard, which may be watching a loop that is stuck.
#[derive(Default, Debug)]
pub struct Pulse {
    /// The loop's own progress: its effects that ended on their own, the plane's nodes settled
    /// and the jobs' effects ended, never its idle turns or a client's requests.
    moved: AtomicU64,
    /// The turns it finished, idle ones too: an idle loop turns at least every 50 ms.
    turns: AtomicU64,
}

impl Pulse {
    /// A turn ended, with the loop's progress at `moved`.
    pub fn turned(&self, moved: u64) {
        self.moved.store(moved, Ordering::Relaxed);
        self.turns.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(all(unix, feature = "native"))]
    pub fn moved(&self) -> u64 {
        self.moved.load(Ordering::Relaxed)
    }

    #[cfg(all(unix, feature = "native"))]
    pub fn turns(&self) -> u64 {
        self.turns.load(Ordering::Relaxed)
    }
}

/// What one look at the loop's turns found.
#[cfg(all(unix, feature = "native"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// A turn ended since the last look: the loop turns.
    Turning,
    /// No turn ended since the last look, for less than the report's bound, or the stuck turn
    /// was reported already.
    Holds,
    /// No turn ended for `for_ns`, from `since_ns`, past the report's bound: said once a stuck
    /// turn, the loop having finished `turns` turns.
    Stuck { since_ns: u64, for_ns: u64, turns: u64 },
}

/// Judges the loop's turns, told the time and the count at each look: a turn that lasts past
/// `report` is reported once, and again only after the loop turned in between.
#[cfg(all(unix, feature = "native"))]
#[derive(Debug)]
pub struct LoopWatch {
    report_ns: u64,
    /// The turns at the last look that saw them move, and when.
    seen: Option<(u64, u64)>,
    said: bool,
}

#[cfg(all(unix, feature = "native"))]
impl LoopWatch {
    pub fn new(report: Duration) -> LoopWatch {
        LoopWatch {
            report_ns: u64::try_from(report.as_nanos()).unwrap_or(u64::MAX),
            seen: None,
            said: false,
        }
    }

    /// One look: `now_ns` on a monotonic clock, `turns` the loop's count ([`Pulse::turns`]).
    pub fn look(&mut self, now_ns: u64, turns: u64) -> Look {
        match self.seen {
            Some((seen, since_ns)) if seen == turns => {
                let for_ns = now_ns.saturating_sub(since_ns);
                if self.said || for_ns < self.report_ns {
                    return Look::Holds;
                }
                self.said = true;
                Look::Stuck { since_ns, for_ns, turns }
            }
            _ => {
                self.seen = Some((turns, now_ns));
                self.said = false;
                Look::Turning
            }
        }
    }
}

/// What one look at the daemon's footprint against its memory ceiling found
/// ([`CeilingWatch::look`]).
#[cfg(all(unix, feature = "native"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CeilingLook {
    /// Under the ceiling, and no nearer or further than the last look said.
    Under,
    /// Within 1/8 of the ceiling, the first look so since it was last under 3/4 of it: said
    /// once, so the log's last word on memory before a kill by the kernel says it was near.
    Near,
    /// Back under 3/4 of the ceiling after a look that said [`CeilingLook::Near`].
    Eased,
    /// Past the ceiling: the daemon ends ([`blaze_core::exit::Exit::MemoryCeiling`]).
    Passed,
}

/// Judges the daemon's footprint against its ceiling, told both at each look: past it, the
/// daemon ends; near it, or eased from near it, a line says so once. A ceiling of 0 is none,
/// and a footprint at the ceiling is not past it. Allocates nothing, so the guard may look
/// every 100 ms (`serve::ceiling::EVERY`).
#[cfg(all(unix, feature = "native"))]
#[derive(Debug, Default)]
pub struct CeilingWatch {
    /// The last look that was not under said near, and none has said eased since.
    near: bool,
}

#[cfg(all(unix, feature = "native"))]
impl CeilingWatch {
    /// One look: `footprint` the daemon's bytes as the system counts them
    /// (`raw::memory::own_footprint`), `ceiling` its ceiling's (`serve::ceiling::ceiling`).
    pub fn look(&mut self, footprint: u64, ceiling: u64) -> CeilingLook {
        if ceiling == 0 {
            self.near = false;
            return CeilingLook::Under;
        }
        if footprint > ceiling {
            return CeilingLook::Passed;
        }
        let near = footprint >= ceiling - ceiling / 8;
        let eased = footprint < ceiling / 4 * 3;
        match (self.near, near, eased) {
            (false, true, _) => {
                self.near = true;
                CeilingLook::Near
            }
            (true, _, true) => {
                self.near = false;
                CeilingLook::Eased
            }
            _ => CeilingLook::Under,
        }
    }
}

#[cfg(all(test, unix, feature = "native"))]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    /// The daemon's stuck loop, as the guard sees it: turns stop at 7, the report comes once
    /// at its bound, never again while the turn lasts, and anew once the loop turned and
    /// stuck again. Make `look` never say `Stuck` and the stuck loop goes unreported.
    #[test]
    fn a_turn_past_its_bound_is_reported_once_and_again_after_the_loop_turned() {
        let mut watch = LoopWatch::new(Duration::from_secs(60));
        let looks = [
            (0, 5),
            (S, 6),
            (2 * S, 7),
            (30 * S, 7),
            (61 * S, 7),
            (62 * S, 7),
            (500 * S, 7),
            (501 * S, 8),
            (502 * S, 8),
            (600 * S, 8),
        ];
        let said: Vec<Look> = looks.iter().map(|&(now, turns)| watch.look(now, turns)).collect();
        assert_eq!(
            said,
            [
                Look::Turning,
                Look::Turning,
                Look::Turning,
                Look::Holds,
                Look::Holds,
                Look::Stuck { since_ns: 2 * S, for_ns: 60 * S, turns: 7 },
                Look::Holds,
                Look::Turning,
                Look::Holds,
                Look::Stuck { since_ns: 501 * S, for_ns: 99 * S, turns: 8 },
            ]
        );
    }

    /// A loop that turns, however slowly between looks, is never stuck.
    #[test]
    fn a_loop_that_turns_between_looks_is_never_stuck() {
        let mut watch = LoopWatch::new(Duration::from_secs(1));
        for at in 0..100 {
            assert_eq!(watch.look(at * 10 * S, at), Look::Turning);
        }
    }

    const GIB: u64 = 1 << 30;

    /// The guard's decision on a daemon's footprint against a ceiling of 48 GiB: under it, or
    /// at it exactly, the daemon runs on; one byte past it, it ends. Near (42 GiB, 7/8) and back
    /// under 3/4 (36 GiB) are each said once a crossing, never on every look. Make `look` never
    /// say `Passed` and a daemon past its ceiling runs on; make it compare with `>=` and one
    /// exactly at its ceiling is ended.
    #[test]
    fn a_footprint_past_its_ceiling_ends_the_daemon_and_near_it_is_said_once() {
        let ceiling = 48 * GIB;
        let mut watch = CeilingWatch::default();
        let looks = [
            GIB,
            36 * GIB,
            42 * GIB - 1,
            42 * GIB,
            45 * GIB,
            ceiling,
            40 * GIB,
            36 * GIB,
            35 * GIB,
            30 * GIB,
            47 * GIB,
            ceiling + 1,
        ];
        let said: Vec<CeilingLook> = looks.iter().map(|&at| watch.look(at, ceiling)).collect();
        use CeilingLook::{Eased, Near, Passed, Under};
        assert_eq!(
            said,
            [Under, Under, Under, Near, Under, Under, Under, Under, Eased, Under, Near, Passed]
        );
        // No ceiling is no bound, whatever the footprint.
        assert_eq!(CeilingWatch::default().look(u64::MAX, 0), Under);
        // A tiny ceiling holds as a large one does.
        let mut small = CeilingWatch::default();
        assert_eq!(small.look(8, 8), Near);
        assert_eq!(small.look(9, 8), Passed);
    }

    #[test]
    fn the_pulse_counts_turns_and_keeps_the_last_progress() {
        let pulse = Pulse::default();
        pulse.turned(3);
        pulse.turned(3);
        pulse.turned(9);
        assert_eq!((pulse.turns(), pulse.moved()), (3, 9));
    }
}
