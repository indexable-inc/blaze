//! The loop's pulse as threads outside it read it ([`Pulse`]), and the judge of a loop that
//! stopped turning ([`LoopWatch`]), a sans-IO core the stop's guard (`serve::stop`) drives.
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

    #[test]
    fn the_pulse_counts_turns_and_keeps_the_last_progress() {
        let pulse = Pulse::default();
        pulse.turned(3);
        pulse.turned(3);
        pulse.turned(9);
        assert_eq!((pulse.turns(), pulse.moved()), (3, 9));
    }
}
