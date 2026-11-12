//! When a daemon with nothing to do gives its free memory back.
//!
//! A build's end lets go of what the build no longer needs, and each thread's allocator gives
//! back what that freed as the thread next allocates or parks. What is left is what a thread
//! keeps warm for its next task: its frame shelf (`blaze_mem::frame::WARM`, 32 MiB a thread
//! at most, charged to no task) and the allocator's retired pages. Warm memory pays for itself
//! while edits follow one another (so does a freed page: [`warm`]); once the daemon has had
//! nothing to do for [`AFTER`] it is
//! only footprint, so the loop asks for it back, once a stretch of idleness, on the purger's
//! thread and the workers' own as they park, never on a request's path. The next task after
//! that takes its pages from the kernel again: a page fault a page it touches.
//!
//! A core: plain data on the caller's clock, no IO.
//!
//! The invariant: [`Idle::look`] is true at most once between two looks that saw work, never
//! while work is seen, and never before the daemon has been idle for [`AFTER`].

use std::time::Duration;

/// How long the daemon has nothing to do before it gives its warm memory back: past the gap
/// between the edits of someone typing and saving, short enough that a daemon left alone is
/// small within seconds.
pub(crate) const AFTER: Duration = Duration::from_secs(5);

/// The daemon keeps freed pages warm between builds while its footprint is under one part in
/// this many of its limit ([`warm`]).
pub(crate) const WARM_UNDER: u64 = 8;

/// How long the allocator itself keeps a freed page while the daemon keeps them warm
/// ([`warm`]): longer than any stretch of work without a pause of [`AFTER`], so the allocator
/// never gives a page back on a build's path of its own accord, and every return is one the
/// daemon asked for (the idle give-back, or memory running short). Not [`AFTER`]: mimalloc
/// purges a page past its delay as some thread next takes or frees one, which is on the next
/// edit's path (measured: with five seconds here an edit 3.5 s after the last took 155 ms,
/// against 117 with every freed page given back at once).
pub(crate) const KEPT_FREED: Duration = Duration::from_secs(3600);

/// How long the allocator keeps a freed page, in milliseconds, for a daemon whose footprint is
/// `footprint` under the limit `limit`: [`KEPT_FREED`] while it holds under one part in
/// [`WARM_UNDER`] of the limit, so the pages one edit's build freed are the next one's with no
/// page fault and the idle give-back returns them; 0, at once, past that, where freed pages
/// kept are memory admission cannot grant (docs/memory-footprint.md, step 3).
#[blaze::pure]
pub(crate) fn warm(footprint: u64, limit: u64) -> u64 {
    match footprint < limit / WARM_UNDER {
        true => KEPT_FREED.as_millis() as u64,
        false => 0,
    }
}

/// Whether this stretch of idleness gave back yet.
#[derive(Default)]
pub(crate) struct Idle {
    /// When the daemon was first seen with nothing to do, since it last worked.
    since: Option<Duration>,
    gave: bool,
}

impl Idle {
    /// A look at `now`, with `working` whether any build or task is under way: `true` when
    /// the daemon should give its warm memory back now.
    #[blaze::pure]
    pub(crate) fn look(&mut self, now: Duration, working: bool) -> bool {
        if working {
            *self = Idle::default();
            return false;
        }
        let since = *self.since.get_or_insert(now);
        if self.gave || now.saturating_sub(since) < AFTER {
            return false;
        }
        self.gave = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freed_pages_are_kept_warm_only_while_the_daemon_holds_little() {
        let limit = 96 << 30;
        assert_eq!(warm(4 << 30, limit), 3_600_000);
        assert_eq!(warm(limit / WARM_UNDER, limit), 0, "at the line: at once");
        assert_eq!(warm(80 << 30, limit), 0);
        assert_eq!(warm(0, 0), 0, "no limit known: at once");
    }

    /// The invariant: over any run of looks, a give-back comes only at a look that saw no
    /// work and whose idle stretch is at least [`AFTER`] old, and at most once a stretch.
    /// Checked against a model that keeps the stretch's start itself, over every pattern of
    /// work and gap a small alphabet makes.
    #[test]
    fn it_gives_back_once_a_stretch_and_only_after_the_wait() {
        let gaps = [Duration::from_millis(10), Duration::from_millis(900), AFTER];
        // Every run of six steps, a step one of three gaps and working or not: 6^6 runs.
        for run in 0..6u32.pow(6) {
            let (mut idle, mut now) = (Idle::default(), Duration::ZERO);
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
                    assert!(now - since >= AFTER, "run {run}: gave back {:?} into a stretch", now - since);
                }
                assert!(gave <= 1, "run {run}: gave back twice in one stretch");
                assert_eq!(gave == 1, now - since >= AFTER, "run {run}: a stretch past the wait has given back, once");
            }
        }
    }
}
