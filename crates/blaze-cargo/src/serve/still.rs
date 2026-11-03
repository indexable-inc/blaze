//! Whether anything can move a build ([`Stillness`]): the daemon's check, each turn, that turns a
//! build nothing can move into a failure within [`STILL_FOR`] rather than one the stall
//! watchdog finds minutes later, or never. Sans IO: the loop tells it the time on its driver's
//! clock and what it sees ([`Facts`]); it says what to do ([`Turn`]).
//!
//! A thread task the plane posted to the worker pool ends only as its pool job's `done` runs.
//! With the pool idle (every worker asleep, nothing queued) and none of its jobs parked, no job
//! of the pool can run again, so such a task can never end: its job was lost (the poolborrow
//! stall of 2026-11-02, a job dropped unrun, sat "running" for ten minutes). With jobs parked,
//! the task moves while a wait of theirs can end; once the loop is still too, a wait nothing in
//! the graph can end ([`blaze_graph::sched::stuck_while_quiet`]) is no movement either.

use std::time::Duration;

/// How long a build may have nothing that can move it before it fails: past the loop's longest
/// turn and a worker's wake, short of anyone waiting on it.
pub(super) const STILL_FOR: Duration = Duration::from_secs(2);

/// What the loop sees this turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Facts {
    /// The loop's own work can move it: a task ended and not taken, an effect pending or in
    /// flight, a job's write waiting on readers.
    pub(super) loop_moves: bool,
    /// Thread tasks on IO threads: each waits on files, the network or a process, which end on
    /// their own.
    pub(super) off_pool: usize,
    /// Thread tasks posted to the worker pool and not taken back.
    pub(super) on_pool: usize,
    /// Every worker of the pool sleeps with nothing queued. Read before anything the pool's jobs
    /// send: a job sends its end before its worker sleeps.
    pub(super) pool_idle: bool,
    /// The pool's jobs parked on a wait.
    pub(super) parked: usize,
}

/// Why nothing can move the build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Why {
    /// No thread task runs: what is open waits on the loop.
    NoTask,
    /// Thread tasks posted to the pool, with the pool idle and none of its jobs parked: their
    /// jobs were lost, and their `done` will never run.
    Lost { tasks: usize },
    /// Thread tasks posted to the pool whose jobs wait, or wait on jobs that wait, on what
    /// nothing will end: every wait of the graph is stuck with the loop still.
    Stuck { tasks: usize, parked: usize },
}

/// What the loop does this turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Turn {
    /// Something can move the build.
    Moves,
    /// Nothing can, from this turn: arm a timer of [`STILL_FOR`] so the loop wakes to look again.
    Began,
    /// Nothing can, for less than [`STILL_FOR`] so far; or nothing is open to move.
    Holds,
    /// Nothing could for `quiet_ns`, at least [`STILL_FOR`]: the build fails, naming `why`.
    Still { quiet_ns: u64, why: Why },
}

/// When the build last could move, while it cannot.
#[derive(Debug, Default)]
pub(super) struct Stillness {
    since: Option<u64>,
}

impl Stillness {
    /// One turn at `now_ns`: `stuck` says whether no wait of the wait graph can end with the loop
    /// still (asked only when the pool is idle with jobs parked and the loop and the IO threads
    /// are still), `open` whether a request has a node not settled (asked only as a still
    /// stretch begins).
    pub(super) fn tick(&mut self, now_ns: u64, facts: &Facts, stuck: impl FnOnce() -> bool, open: impl FnOnce() -> bool) -> Turn {
        let why = match why(facts, stuck) {
            None => {
                self.since = None;
                return Turn::Moves;
            }
            Some(why) => why,
        };
        let Some(since) = self.since else {
            if !open() {
                return Turn::Holds;
            }
            self.since = Some(now_ns);
            return Turn::Began;
        };
        let quiet_ns = now_ns.saturating_sub(since);
        if quiet_ns < u64::try_from(STILL_FOR.as_nanos()).unwrap_or(u64::MAX) {
            return Turn::Holds;
        }
        Turn::Still { quiet_ns, why }
    }

    /// Starts over: what the loop found still had nothing to name.
    pub(super) fn reset(&mut self) {
        self.since = None;
    }
}

/// Why nothing can move the build, or `None` when something can.
fn why(facts: &Facts, stuck: impl FnOnce() -> bool) -> Option<Why> {
    if facts.loop_moves || facts.off_pool > 0 || !facts.pool_idle {
        return None;
    }
    if facts.on_pool == 0 {
        return Some(Why::NoTask);
    }
    if facts.parked == 0 {
        return Some(Why::Lost { tasks: facts.on_pool });
    }
    stuck().then_some(Why::Stuck { tasks: facts.on_pool, parked: facts.parked })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// Turns of the loop every 50 ms of a virtual clock from 0, each told `facts`, until the
    /// stillness is said or 10 s of the clock went by: when it was said, and what.
    fn run(facts: Facts, stuck: bool) -> Option<(u64, Turn)> {
        let mut stillness = Stillness::default();
        let mut began = false;
        for turn in 0..200 {
            let now = turn * 50 * MS;
            match stillness.tick(now, &facts, || stuck, || true) {
                Turn::Began => began = true,
                still @ Turn::Still { .. } => {
                    assert!(began, "said still before the stretch began");
                    return Some((now, still));
                }
                Turn::Moves | Turn::Holds => {}
            }
        }
        None
    }

    /// The poolborrow stall (2026-11-02): a lost pool job's task stayed "running" with the pool
    /// idle and nothing parked, and the old check counted any running task as movement, so the
    /// build sat still for ten minutes. Now it fails at [`STILL_FOR`], naming the lost tasks.
    #[test]
    fn a_task_whose_pool_job_was_lost_is_still_within_the_bound() {
        let lost = Facts { on_pool: 1, pool_idle: true, parked: 0, ..Facts::default() };
        let (at, turn) = run(lost, false).expect("a lost job's task is still within 10 s");
        assert_eq!(at, 2_000 * MS, "said at STILL_FOR");
        assert_eq!(turn, Turn::Still { quiet_ns: 2_000 * MS, why: Why::Lost { tasks: 1 } });
    }

    /// A task whose pool job is parked on a wait that can end (an event a thread fires, a timed
    /// wait, a key another job makes) moves, however long; once no wait of the graph can end
    /// with the loop still, it is still, naming how many jobs are parked.
    #[test]
    fn a_task_whose_job_waits_moves_until_no_wait_can_end() {
        let parked = Facts { on_pool: 2, pool_idle: true, parked: 3, ..Facts::default() };
        assert_eq!(run(parked, false), None, "a job parked on a wait that can end is movement");
        let (_, turn) = run(parked, true).expect("a wait nothing can end is still");
        assert_eq!(turn, Turn::Still { quiet_ns: 2_000 * MS, why: Why::Stuck { tasks: 2, parked: 3 } });
    }

    /// Anything the loop, an IO thread or a busy pool can do moves the build, and a turn that
    /// moves starts the stretch over.
    #[test]
    fn work_anywhere_moves_and_starts_the_stretch_over() {
        let lost = Facts { on_pool: 1, pool_idle: true, ..Facts::default() };
        for moving in [
            Facts { loop_moves: true, ..lost },
            Facts { off_pool: 1, ..lost },
            Facts { pool_idle: false, ..lost },
        ] {
            assert_eq!(run(moving, true), None, "{moving:?}");
        }
        let mut stillness = Stillness::default();
        assert_eq!(stillness.tick(0, &lost, || false, || true), Turn::Began);
        assert_eq!(stillness.tick(1_900 * MS, &lost, || false, || true), Turn::Holds);
        assert_eq!(stillness.tick(1_950 * MS, &Facts { pool_idle: false, ..lost }, || false, || true), Turn::Moves);
        assert_eq!(stillness.tick(2_000 * MS, &lost, || false, || true), Turn::Began);
        assert_eq!(stillness.tick(3_999 * MS, &lost, || false, || true), Turn::Holds);
        assert!(matches!(stillness.tick(4_000 * MS, &lost, || false, || true), Turn::Still { .. }));
        // Nothing open is nothing to move.
        assert_eq!(Stillness::default().tick(0, &lost, || false, || false), Turn::Holds);
    }
}
