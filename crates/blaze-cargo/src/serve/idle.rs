//! When a daemon with nothing to do gives its free memory back.
//!
//! A build's end lets go of what the build no longer needs, and each thread's allocator gives
//! back what that freed as the thread next allocates or parks. What is left is what a thread
//! keeps warm for its next task: its frame shelf (`blaze_mem::frame::WARM`, 32 MiB a thread
//! at most, charged to no task) and the allocator's retired pages. Warm memory pays for itself
//! while edits follow one another; once the daemon has had nothing to do for [`AFTER`] it is
//! only footprint, so the loop asks for it back, once a stretch of idleness, on the purger's
//! thread and the workers' own as they park, never on a request's path. The next task after
//! that takes its pages from the kernel again: a page fault a page it touches.
//!
//! A core: plain data on the caller's clock, no IO.
//!
//! The invariant: [`Idle::look`] is true at most once between two looks that saw work, never
//! while work is seen, and never before the daemon has been idle for its wait ([`AFTER`] for
//! the warm memory); one made with [`Idle::every`] ([`POOL_AFTER`], the stores' pools) is true
//! again each time its wait has passed since it last was, and never sooner.

use std::time::Duration;

/// How long the daemon has nothing to do before it gives its warm memory back: past the gap
/// between the edits of someone typing and saving, short enough that a daemon left alone is
/// small within seconds.
pub(crate) const AFTER: Duration = Duration::from_secs(2);

/// How long the daemon has nothing to do before the stores' pools give back the pages
/// nothing read again (`Store::trim_pool`): longer than [`AFTER`], since a page given back is
/// a read when the next edit asks for it, where a warm frame given back is only a page fault.
/// Measured (hello-world, 2026-11-12, lane memstrat-1111): trimmed after [`AFTER`], a body
/// edit 3.5 s after the last was 121.2 and 129.0 ms where the same binary without the trim
/// was 111.5 to 117.4 over four runs.
pub(crate) const POOL_AFTER: Duration = Duration::from_secs(30);

/// Whether this stretch of idleness gave back yet.
pub(crate) struct Idle {
    /// How long a stretch has to be before it gives back.
    after: Duration,
    /// Whether it gives back again each `after` the stretch goes on ([`Idle::every`]).
    again: bool,
    /// When the daemon was first seen with nothing to do, since it last worked; or, of one
    /// that gives again, when it last gave.
    since: Option<Duration>,
    gave: bool,
}

impl Default for Idle {
    fn default() -> Idle {
        Idle::after(AFTER)
    }
}

impl Idle {
    /// One that gives back once the daemon has had nothing to do for `after`.
    pub(crate) const fn after(after: Duration) -> Idle {
        Idle { after, again: false, since: None, gave: false }
    }

    /// One that gives back each `after` the daemon goes on having nothing to do: what the
    /// stores' pools ask, whose sweep takes a page read again only on the lap after the one
    /// that passed it over, so a pool nothing reads is gone after two.
    pub(crate) const fn every(after: Duration) -> Idle {
        Idle { after, again: true, since: None, gave: false }
    }

    /// A look at `now`, with `working` whether any build or task is under way: `true` when
    /// the daemon should give its warm memory back now.
    #[blaze::pure]
    pub(crate) fn look(&mut self, now: Duration, working: bool) -> bool {
        if working {
            (self.since, self.gave) = (None, false);
            return false;
        }
        let since = *self.since.get_or_insert(now);
        if (self.gave && !self.again) || now.saturating_sub(since) < self.after {
            return false;
        }
        self.gave = true;
        if self.again {
            self.since = Some(now);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant: over any run of looks, a give-back comes only at a look that saw no
    /// work and whose idle stretch is at least [`AFTER`] old, and at most once a stretch.
    /// Checked against a model that keeps the stretch's start itself, over every pattern of
    /// work and gap a small alphabet makes.
    #[test]
    fn it_gives_back_once_a_stretch_and_only_after_the_wait() {
        for after in [AFTER, POOL_AFTER] {
            gives_back_once(after);
        }
    }

    /// The invariant of [`Idle::every`]: a give-back comes only at a look that saw no work,
    /// at least the wait after the stretch began and after the give-back before it, and at
    /// every such look. Against a model that keeps when it last gave, over the same runs.
    #[test]
    fn one_made_to_give_again_gives_each_wait_of_a_stretch_and_never_sooner() {
        let after = POOL_AFTER;
        let gaps = [Duration::from_millis(10), after.mul_f64(0.45), after];
        for run in 0..6u32.pow(6) {
            let (mut idle, mut now) = (Idle::every(after), Duration::ZERO);
            let mut last: Option<Duration> = None;
            let mut code = run;
            for _ in 0..6 {
                let (gap, working) = (gaps[(code % 3) as usize], code / 3 % 2 == 1);
                code /= 6;
                now += gap;
                let give = idle.look(now, working);
                if working {
                    assert!(!give, "run {run}: gave back while working");
                    last = None;
                    continue;
                }
                let since = *last.get_or_insert(now);
                assert_eq!(give, now - since >= after, "run {run}: {:?} since the stretch began or it last gave", now - since);
                if give {
                    last = Some(now);
                }
            }
        }
    }

    fn gives_back_once(after: Duration) {
        let gaps = [Duration::from_millis(10), after.mul_f64(0.45), after];
        // Every run of six steps, a step one of three gaps and working or not: 6^6 runs.
        for run in 0..6u32.pow(6) {
            let (mut idle, mut now) = (Idle::after(after), Duration::ZERO);
            let (mut since, mut gave): (Option<Duration>, u32) = (None, 0);
            let mut code = run;
            for _ in 0..6 {
                let (gap, working) = (gaps[(code % 3) as usize], code / 3 % 2 == 1);
                code /= 6;
                now += gap;
                let give = idle.look(now, working);
                if working {
                    assert!(!give, "run {run}: gave back while working");
                    (since, gave) = (None, 0);
                    continue;
                }
                let since = *since.get_or_insert(now);
                if give {
                    gave += 1;
                    assert!(now - since >= after, "run {run}: gave back {:?} into a stretch", now - since);
                }
                assert!(gave <= 1, "run {run}: gave back twice in one stretch");
                assert_eq!(gave == 1, now - since >= after, "run {run}: a stretch past the wait has given back, once");
            }
        }
    }
}
