//! Fairness after a give-way. A job whose wait for a key's run would have closed a cycle gives
//! way ([`crate::Cancelled`]): it unwinds and lets go of the runs it held, which wakes the job
//! that waited for them. Its caller may ask again at once, in a loop. Asked again before the job
//! it gave way to has run (on one worker, or on any pool whose workers are all busy, that job is
//! runnable but not running), it claims back what it let go of and waits for the other's run
//! again; the other, resumed, asks for what this one holds, its wait closes the cycle, and it
//! gives way in turn. The two give way to each other forever.
//!
//! So a give-way is remembered with the job that made it ([`GaveWay`]: the run it gave way to,
//! the key that run makes and the job making it), and that job's next ask
//! ([`crate::pump::Flights::get_or_run`]) first waits for that run to end
//! (`sched::before_claim`). Salsa keeps the same order for a claim given up in a cycle: it
//! transfers the claim to the query it gave way to (`ClaimGuard::transfer`,
//! `SyncOwner::Transferred`), so no other thread can take it until that query completes. Here
//! nothing is transferred: the job that gave way takes nothing until the run it gave way to has
//! ended, which is the same guarantee for every caller, whether it asks again at once, after a
//! scope (as [`crate::par`] does), or never.
//!
//! When that wait would itself close a cycle (the run it gave way to waits, through others, on
//! what this job still holds), only this job can move, so it goes on and claims as it did
//! before this rule, and gives way again where it must. The job's give-ways count in a row,
//! whatever run each was to (two blocked runs given way to in turn count as one streak), until
//! a retry's wait for its run really ends: that run ended, which is progress, and the job's
//! record goes. At [`GIVE_WAYS_WITHOUT_PROGRESS`] the ask fails with
//! [`NoProgress`], naming the key's query and both jobs: a caller that asks again while it holds
//! what the other waits for can never end, and the bug is where it loops (rule 18).
//!
//! A loop that catches an item's give-way to run that item again later ([`crate::par`]'s, the
//! test database's) goes on to its next item, not the same ask: it says so
//! ([`crate::pump::rerun_later`]), and that one give-way is taken back from the streak
//! ([`GaveWay::set_aside`]). The record stays, so the next ask still waits for the run where it
//! can, and the loop's rerun after it finds that run ended. Without that, a loop of 65 items that
//! each give way to one run on one job would fail at its 65th, though every item and rerun can
//! end. Only that give-way is taken back: a retry loop around such a loop still counts the
//! rerun's give-way that reaches it, once a pass, and meets the bound.
//!
//! [`GaveWay`] is the decision, a state machine with no IO (rule 14). The world keeps each job's
//! by its id, dropped as the job ends; the driver (`sched::host`) waits.

use std::fmt;

use super::JobId;
use crate::Key;

/// The most give-ways in a row a job makes to one run, each after its wait for that run to end
/// was refused, before its next ask fails with [`NoProgress`].
pub(crate) const GIVE_WAYS_WITHOUT_PROGRESS: u32 = 64;

/// A job's last give-way, kept while it may ask again.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct GaveWay {
    /// The key whose run it gave way to, as the caller asked it ([`crate::pump::Flights`]'s).
    pub(crate) asked: Key,
    /// What the caller named that key (a query and its key), for the bound's error.
    pub(crate) named: Box<str>,
    /// That run's flight in the scheduler: what the retry waits for.
    pub(crate) flight: Key,
    /// The job that gave way.
    pub(crate) job: JobId,
    /// The job running `asked`, which (through others) waited on `job`.
    pub(crate) to: JobId,
    /// `to` as the observer numbers it at the give-way (generation << 32 | slot): the retry's
    /// wait names that job, never a later one given its id.
    pub(crate) owner: u64,
    /// The job's give-ways in a row, counting this one, to whatever run: since its last retry
    /// whose wait ended, each retry's wait refused, less those a loop set aside.
    pub(crate) streak: u32,
}

/// The run a job gave way to, as [`GaveWay::after`] is told it.
pub(crate) struct Run {
    pub(crate) asked: Key,
    pub(crate) named: Box<str>,
    pub(crate) flight: Key,
    pub(crate) to: JobId,
    pub(crate) owner: u64,
}

impl GaveWay {
    /// `job` gave way: its wait for `run` closed a cycle. `last` is the give-way it remembered
    /// (its retry's wait for that run was refused, or it never asked since): its streak goes on,
    /// whatever run this one is to.
    pub(crate) fn after(last: Option<GaveWay>, job: JobId, run: Run) -> GaveWay {
        let streak = match last {
            Some(last) if last.job == job => last.streak.saturating_add(1),
            _ => 1,
        };
        let Run { asked, named, flight, to, owner } = run;
        GaveWay { asked, named, flight, job, to, owner, streak }
    }

    /// A loop caught the give-way to run its item again later, and goes on to another item:
    /// that is not the same ask again, so this give-way is taken back from the streak, and only
    /// it. The next ask still waits for the run first.
    pub(crate) fn set_aside(&mut self) {
        self.streak = self.streak.saturating_sub(1);
    }

    /// The job's wait for the run to end, before it claimed anything, would have closed a cycle:
    /// it goes on (only it can move), unless this is the bound's give-way in a row to the run.
    ///
    /// # Errors
    ///
    /// [`NoProgress`] at [`GIVE_WAYS_WITHOUT_PROGRESS`] give-ways in a row.
    pub(crate) fn refused(&self) -> Result<(), NoProgress> {
        if self.streak < GIVE_WAYS_WITHOUT_PROGRESS {
            return Ok(());
        }
        Err(NoProgress {
            asked: self.asked,
            named: self.named.clone(), // alloc: an error's name, once
            job: self.job,
            to: self.to,
            give_ways: self.streak,
        })
    }

    /// What the job's wait for the run to end waits for, as its span says (rule 15).
    pub(crate) fn name_wait(&self, out: &mut String) {
        use fmt::Write as _;
        // Writing to a `String` does not fail.
        _ = write!(out, "a retry after giving way: the run of {} by job {}", self.named, self.to.0);
    }
}

/// A job asks again after giving way, and can never end: [`GIVE_WAYS_WITHOUT_PROGRESS`] times in
/// a row it gave way to the run of `asked` (`named`) by `to`, and each time it asked again that
/// run still waited, through others, on what `job` holds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct NoProgress {
    pub(crate) asked: Key,
    /// What the caller named `asked`: the query and its key.
    pub(crate) named: Box<str>,
    pub(crate) job: JobId,
    pub(crate) to: JobId,
    pub(crate) give_ways: u32,
}

impl fmt::Display for NoProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "job {} gave way {} times in a row to the run of {} (key {}) by job {}, and each time it asked again that run \
             still waited on what job {} holds: a caller that asks again while holding it can never end; it must let go \
             of what it holds (end its job, or give way further) before it asks again",
            self.job.0,
            self.give_ways,
            self.named,
            Hex(&self.asked),
            self.to.0,
            self.job.0,
        )
    }
}

impl std::error::Error for NoProgress {}

/// A key's first 8 bytes in hex, as the pump names keys.
struct Hex<'a>(&'a Key);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.0.iter().take(8).try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const X: JobId = JobId(1);
    const Y: JobId = JobId(2);
    const Z: JobId = JobId(3);

    fn key(n: u8) -> Key {
        Key([n; 32])
    }

    fn run(asked: u8, flight: u8, to: JobId) -> Run {
        Run {
            asked: key(asked),
            named: format!("Chain({asked})").into(),
            flight: key(flight),
            to,
            owner: u64::from(to.0),
        }
    }

    /// A transcript of one job's give-ways, with no scheduler: a give-way to a new run starts a
    /// streak of 1; one to the same run after a refused retry continues it; the retry goes on
    /// until the bound's give-way, which fails naming the query and both jobs.
    #[test]
    fn give_ways_in_a_row_to_one_run_end_in_no_progress_at_the_bound() {
        let first = GaveWay::after(None, Y, run(1, 10, X));
        assert_eq!(first.streak, 1);
        assert_eq!(first.refused(), Ok(()), "one refused retry goes on");
        let mut last = first;
        for streak in 2..GIVE_WAYS_WITHOUT_PROGRESS {
            last = GaveWay::after(Some(last), Y, run(1, 10, X));
            assert_eq!((last.streak, last.refused()), (streak, Ok(())));
        }
        last = GaveWay::after(Some(last), Y, run(1, 10, X));
        let stuck = last.refused().expect_err("the bound's give-way in a row");
        assert_eq!(
            stuck,
            NoProgress {
                asked: key(1),
                named: "Chain(1)".into(),
                job: Y,
                to: X,
                give_ways: GIVE_WAYS_WITHOUT_PROGRESS
            }
        );
        let said = stuck.to_string();
        assert!(
            said.contains("job 2 gave way 64 times")
                && said.contains("Chain(1) (key 0101010101010101) by job 1"),
            "{said}"
        );
    }

    /// The streak is the job's: a give-way to another run goes on with it, so give-ways
    /// alternating between two blocked runs reach the bound; only another job's starts anew.
    #[test]
    fn give_ways_to_other_runs_go_on_with_the_streak() {
        let mut last = GaveWay::after(None, Y, run(1, 10, X));
        for streak in 2..=GIVE_WAYS_WITHOUT_PROGRESS {
            let to = if streak % 2 == 0 { run(2, 11, Z) } else { run(1, 10, X) };
            last = GaveWay::after(Some(last), Y, to);
            assert_eq!(last.streak, streak);
        }
        assert_eq!(last.refused().map_err(|it| it.give_ways), Err(GIVE_WAYS_WITHOUT_PROGRESS));
        assert_eq!(GaveWay::after(Some(last), X, run(1, 10, Y)).streak, 1, "another job");
    }

    /// A set-aside takes back only the give-way it set aside: a retry loop around a loop whose
    /// item gives way and is set aside, its rerun giving way again, counts one a pass.
    #[test]
    fn a_set_aside_takes_back_one_give_way() {
        let mut last: Option<GaveWay> = None;
        for pass in 1..=GIVE_WAYS_WITHOUT_PROGRESS {
            let mut item = GaveWay::after(last, Y, run(1, 10, X));
            item.set_aside();
            let rerun = GaveWay::after(Some(item), Y, run(1, 10, X));
            assert_eq!(rerun.streak, pass);
            last = Some(rerun);
        }
        assert!(last.expect("the last pass").refused().is_err(), "the bound ends the retry loop");
    }

    /// A loop's give-ways, each set aside for a rerun, never reach the bound however many items
    /// give way to the one run; the record stays for the next ask's wait.
    #[test]
    fn give_ways_a_loop_sets_aside_never_reach_the_bound() {
        let mut last = None;
        for _ in 0..4 * GIVE_WAYS_WITHOUT_PROGRESS {
            let mut it = GaveWay::after(last, Y, run(1, 10, X));
            assert_eq!((it.streak, it.refused()), (1, Ok(())));
            it.set_aside();
            last = Some(it);
        }
        assert_eq!(last.map(|it| (it.flight, it.streak)), Some((key(10), 0)));
    }

    #[test]
    fn its_wait_names_the_query_and_the_job_it_gave_way_to() {
        let mut said = String::new();
        GaveWay::after(None, Y, run(0xab, 10, X)).name_wait(&mut said);
        assert_eq!(said, "a retry after giving way: the run of Chain(171) by job 1");
    }
}
