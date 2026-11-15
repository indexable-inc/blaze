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
//!
//! A job's write waiting is not movement of its own ([`Facts::write_waited_ns`]): while it
//! waits, the loop holds every new reading task of its job, so a reader parked on what only a
//! held task (or the write) makes never ends. That wait the graph cannot see as stuck (an event
//! some task fires, `macro.thread.replied` among them), so the stillness of a write is its
//! own: past [`WRITE_STILL_FOR`] of waiting, with the pool idle, no IO thread, no expansion
//! thread running and the loop still for [`STILL_FOR`], it is [`Why::WriteWaits`]. The bound is
//! checked here, each turn of the loop on its driver's clock (`Daemon::still`); the loop wakes
//! for it on the timer [`Turn::Began`] names.

use std::time::Duration;

/// How long a build may have nothing that can move it before it fails: past the loop's longest
/// turn and a worker's wake, short of anyone waiting on it.
pub(super) const STILL_FOR: Duration = Duration::from_secs(2);

/// How long a job's write may wait for its readers, with nothing else moving, before the
/// stretch counts toward [`STILL_FOR`]: a reader parked with the pool idle wakes only on what
/// a thread outside the pool fires (an expansion thread, an IO thread, the loop, each counted
/// in [`Facts`]) or on a timed wait of its own, and a timed wait in a reader that holds its
/// job's write back is itself the bug this names. Ten seconds is past any timed wait a task
/// of a build makes and well short of the minutes the watchdog takes.
pub(super) const WRITE_STILL_FOR: Duration = Duration::from_secs(10);

/// What the loop sees this turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Facts {
    /// The loop's own work can move it: a task ended and not taken, an effect pending or in
    /// flight.
    pub(super) loop_moves: bool,
    /// How long the longest-waiting job's write has waited for its readers, or on its own task
    /// (`job::Jobs::write_waited`); `None` while none waits. Movement only while younger than
    /// [`WRITE_STILL_FOR`].
    pub(super) write_waited_ns: Option<u64>,
    /// Expansion threads running a proc macro or with one queued for them
    /// (`blaze_macro_eval::expansion_counts`): each runs off the pool and ends on its own, or
    /// fires what a parked job waits on. A count the loop could not read counts as one.
    pub(super) expanding: usize,
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
    /// A job's write has waited `waited_ns`, past [`WRITE_STILL_FOR`], for readers whose tasks
    /// (`tasks` on the pool, `parked` of the pool's jobs parked) nothing moves, while the loop
    /// holds the tasks they may wait on.
    WriteWaits { tasks: usize, parked: usize, waited_ns: u64 },
}

/// What the loop does this turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Turn {
    /// Something can move the build.
    Moves,
    /// Nothing can, from this turn: arm a timer of `after` ([`STILL_FOR`], or longer for a
    /// write younger than [`WRITE_STILL_FOR`]) so the loop wakes to look again.
    Began { after: Duration },
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
    pub(super) fn tick(
        &mut self,
        now_ns: u64,
        facts: &Facts,
        stuck: impl FnOnce() -> bool,
        open: impl FnOnce() -> bool,
    ) -> Turn {
        let why = match why(facts, stuck) {
            None => {
                self.since = None;
                return Turn::Moves;
            }
            Some(why) => why,
        };
        // A write younger than its bound is still at the bound, not before.
        let young = match why {
            Why::WriteWaits { waited_ns, .. } => WRITE_STILL_FOR.saturating_sub(nanos(waited_ns)),
            Why::NoTask | Why::Lost { .. } | Why::Stuck { .. } => Duration::ZERO,
        };
        let Some(since) = self.since else {
            if !open() {
                return Turn::Holds;
            }
            self.since = Some(now_ns);
            return Turn::Began { after: STILL_FOR.max(young) };
        };
        let quiet_ns = now_ns.saturating_sub(since);
        if nanos(quiet_ns) < STILL_FOR {
            return Turn::Holds;
        }
        if !young.is_zero() {
            // The stretch turned into a young write's after its timer was armed: wake again
            // at the write's bound, one timer a wake.
            return Turn::Began { after: young };
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
    if facts.loop_moves || facts.off_pool > 0 || !facts.pool_idle || facts.expanding > 0 {
        return None;
    }
    if let Some(waited_ns) = facts.write_waited_ns {
        // Whatever the graph says of its waits: the loop holds the tasks a parked reader may
        // wait on, and the graph cannot see them.
        return Some(Why::WriteWaits { tasks: facts.on_pool, parked: facts.parked, waited_ns });
    }
    if facts.on_pool == 0 {
        return Some(Why::NoTask);
    }
    if facts.parked == 0 {
        return Some(Why::Lost { tasks: facts.on_pool });
    }
    stuck().then_some(Why::Stuck { tasks: facts.on_pool, parked: facts.parked })
}

fn nanos(ns: u64) -> Duration {
    Duration::from_nanos(ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// Turns of the loop every 50 ms of a virtual clock from 0, each told `facts`, until the
    /// stillness is said or 20 s of the clock went by: when it was said, and what.
    fn run(facts: Facts, stuck: bool) -> Option<(u64, Turn)> {
        run_with(|_| facts, stuck)
    }

    /// As [`run`], told the facts at each turn's time.
    fn run_with(facts: impl Fn(u64) -> Facts, stuck: bool) -> Option<(u64, Turn)> {
        let mut stillness = Stillness::default();
        let mut began = false;
        for turn in 0..400 {
            let now = turn * 50 * MS;
            match stillness.tick(now, &facts(now), || stuck, || true) {
                Turn::Began { .. } => began = true,
                still @ Turn::Still { .. } => {
                    assert!(began, "said still before the stretch began");
                    return Some((now, still));
                }
                Turn::Moves | Turn::Holds => {}
            }
        }
        None
    }

    /// A loop that turns once at 0 and then only when a timer it armed fires, as a loop with
    /// nothing else to wake it does: when it said still, and what; `None` past 60 s of the
    /// clock. Proves the timers alone bring the loop to its verdict.
    fn on_timers(facts: impl Fn(u64) -> Facts, stuck: bool) -> Option<(u64, Turn)> {
        let mut stillness = Stillness::default();
        let mut timers = vec![0u64];
        while let Some(now) = timers.iter().copied().min() {
            timers.retain(|it| *it != now);
            if now > 60_000 * MS {
                return None;
            }
            match stillness.tick(now, &facts(now), || stuck, || true) {
                Turn::Began { after } => {
                    timers.push(now + u64::try_from(after.as_nanos()).expect("a short timer"));
                }
                still @ Turn::Still { .. } => return Some((now, still)),
                Turn::Moves | Turn::Holds => {}
            }
        }
        None
    }

    const WRITE_MS: u64 = 10_000;

    /// The hang of 2026-11-12: a proc macro's codegen task parked on what only a task its
    /// job's loop held back could make, while the job's write waited on that task to end. The
    /// old check counted the waiting write as the loop's movement and the graph cannot see an
    /// event wait as stuck, so nothing ever said it. A write waiting past [`WRITE_STILL_FOR`],
    /// with its reader parked and nothing else moving, is still at its bound, on the loop's
    /// timers alone.
    ///
    /// The invariant: a write nothing can let apply fails its build within
    /// `WRITE_STILL_FOR + STILL_FOR` of its first wait, and at `WRITE_STILL_FOR` when the
    /// rest was already still. The control: the old fact (the waiting write as loop movement)
    /// never says it.
    #[test]
    fn a_write_held_back_by_a_parked_reader_is_still_at_its_bound() {
        // Waiting since 0: the reader's task is on the pool, parked; its wait is an event.
        let held = held_at;
        let said = Turn::Still {
            quiet_ns: WRITE_MS * MS,
            why: Why::WriteWaits { tasks: 1, parked: 1, waited_ns: WRITE_MS * MS },
        };
        assert_eq!(run_with(held, false), Some((WRITE_MS * MS, said)), "a loop that turns");
        assert_eq!(on_timers(held, false), Some((WRITE_MS * MS, said)), "a loop on its timers");
        // Waiting since 9 s when the rest goes still at 9 s: still STILL_FOR later, not before.
        let late = |now: u64| Facts { write_waited_ns: Some(now + 9_000 * MS), ..held(now) };
        let (at, _) = on_timers(late, false).expect("still");
        assert_eq!(at, 2_000 * MS, "a write past its bound is still at STILL_FOR");
        // The control, the planted bug: a waiting write as loop movement is never still.
        let old = |now: u64| Facts { loop_moves: true, write_waited_ns: None, ..held(now) };
        assert_eq!(run_with(old, false), None, "the old fact hides the hang");
    }

    /// A write that applies, a write whose readers work, or an expansion thread that runs is
    /// movement, however long the write waited before.
    #[test]
    fn a_write_that_can_apply_moves() {
        let held = held_at;
        // Applied at 5 s: its held tasks run on the pool from then.
        let applied = |now: u64| {
            if now < 5_000 * MS {
                held(now)
            } else {
                Facts { write_waited_ns: None, pool_idle: false, parked: 0, ..held(now) }
            }
        };
        assert_eq!(run_with(applied, false), None, "a write that applies");
        assert_eq!(on_timers(applied, false), None, "a write that applies, on the timers");
        let moving: [fn(u64) -> Facts; 4] = [
            |now: u64| Facts { pool_idle: false, ..held_at(now) },
            |now: u64| Facts { expanding: 1, ..held_at(now) },
            |now: u64| Facts { off_pool: 1, ..held_at(now) },
            |now: u64| Facts { loop_moves: true, ..held_at(now) },
        ];
        for moving in moving {
            assert_eq!(run_with(moving, false), None, "{:?}", moving(0));
        }
    }

    /// A write waiting since 0, its one reader's task on the pool and parked.
    fn held_at(now: u64) -> Facts {
        Facts {
            write_waited_ns: Some(now),
            on_pool: 1,
            pool_idle: true,
            parked: 1,
            ..Facts::default()
        }
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
        assert_eq!(
            turn,
            Turn::Still { quiet_ns: 2_000 * MS, why: Why::Stuck { tasks: 2, parked: 3 } }
        );
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
        assert_eq!(stillness.tick(0, &lost, || false, || true), Turn::Began { after: STILL_FOR });
        assert_eq!(stillness.tick(1_900 * MS, &lost, || false, || true), Turn::Holds);
        assert_eq!(
            stillness.tick(1_950 * MS, &Facts { pool_idle: false, ..lost }, || false, || true),
            Turn::Moves
        );
        assert_eq!(
            stillness.tick(2_000 * MS, &lost, || false, || true),
            Turn::Began { after: STILL_FOR }
        );
        assert_eq!(stillness.tick(3_999 * MS, &lost, || false, || true), Turn::Holds);
        assert!(matches!(stillness.tick(4_000 * MS, &lost, || false, || true), Turn::Still { .. }));
        // Nothing open is nothing to move.
        assert_eq!(Stillness::default().tick(0, &lost, || false, || false), Turn::Holds);
    }
}
