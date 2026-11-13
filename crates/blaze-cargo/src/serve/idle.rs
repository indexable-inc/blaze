//! What a daemon with nothing to do holds: everything the next edit needs, or nothing.
//!
//! The rule (2026-11-13, lane X1 of the memory wave, claude-opus-5-5):
//!
//! 1. **While it stays, an idle daemon lets go only of what costs nothing to make again.** Once
//!    it has had nothing to do for [`AFTER`]: each thread's warm frames
//!    (`blaze_mem::frame::WARM`, 32 MiB a thread at most), the pages its allocator keeps freed
//!    (a purge, on the purger's thread and the workers' own as they park), blaze-mem's warm
//!    chunks and the names the flight reader keeps of past generations (`job::giving`). The
//!    stores' pools are not trimmed for idleness: their pages are the next edit's reads, and
//!    an exit returns them with the rest. The next task takes those
//!    pages from the kernel again: a page fault a page. Nothing the next edit would make again
//!    is dropped for idleness: no kept front end, none of its values, no kept walk of
//!    codegen's, no link image, no process table. So a body edit at any point of a stretch the
//!    daemon stays for costs what a body edit with no idle costs.
//! 2. **It exits once nobody is in an edit loop** ([`exit_due`], through `Daemon::stop`, which
//!    writes its image and its kept front ends' members first): no client or launcher
//!    connected, nothing undurable, and idle for [`exit_after`] of what the next edit would
//!    pay again without it (the dearest kept front end's lowering from nothing): 64 times
//!    that, at least [`EXIT_LEAST`] and at most [`EXIT_MOST`]. The first edit after an exit
//!    then pays at most a sixty-fourth of the pause it followed in a remake, and the start of
//!    a daemon besides. A daemon that built nothing waits [`EXIT_UNWORKED`].
//!
//! Why not a budget the daemon holds itself to while it stays (decision D29's 48 MiB, held
//! until 2026-11-13 by the give-back ladder past 30 s): no drop reaches it. A hello-world
//! daemon held 3,140 MB a second after a body edit and 2,764 MB 45 s after the last
//! (`harness/v2/versus.py`, the Mac, 2026-11-13; Relax holds 0 and peaks at 50 MB a run),
//! and its body edit after 60 s idle took 150.2 ms where one with no idle took 42.3: the
//! ladder had dropped the front end, which is dear to remake, and the bytes stayed, since a
//! value freed on the loop's thread into a worker's mimalloc heap leaves the footprint only
//! once that worker collects (`blaze_mem::tag::give_back_heap`), and the type interner, the
//! solver's caches, codegen's kept walks and link images are process statics no front end's
//! drop frees (/tmp/plan/5-memory.md). A process that exits returns every byte by
//! construction, and the next command starts one, or answers alone where nothing changed
//! (`blaze_certify::solo`).
//!
//! A core: plain data on the caller's clock, no IO.
//!
//! The invariant: [`Idle::look`] is true at most once between two looks that saw work, never
//! while work is seen, and never before the daemon has been idle for its wait ([`AFTER`] for
//! the warm memory).

use std::time::Duration;

/// How long the daemon has nothing to do before it gives its warm memory back: past the gap
/// between the edits of someone typing and saving, short enough that a daemon left alone is
/// small within seconds.
pub(crate) const AFTER: Duration = Duration::from_secs(2);

/// The least a daemon in an edit loop waits, idle, before it exits: 30 s, past the gap
/// between the edits of someone typing, saving and reading what the build said.
pub(crate) const EXIT_LEAST: Duration = Duration::from_secs(30);

/// The most a daemon in an edit loop waits, idle, before it exits: 30 minutes, and the wait of
/// one that keeps a front end whose remake it never measured.
pub(crate) const EXIT_MOST: Duration = Duration::from_secs(30 * 60);

/// How many times what the next edit would pay again without it a daemon in an edit loop
/// waits, idle, before it exits: 64, so that edit pays at most a sixty-fourth of the pause.
pub(crate) const EXIT_TIMES: u32 = 64;

/// How long a daemon that only answered hits or served launchers waits before it exits: it is
/// in no edit loop, and what it answered a client answers alone from then on
/// (`blaze_certify::solo`).
pub(crate) const EXIT_UNWORKED: Duration = EXIT_LEAST;

/// How long a daemon that built something or took a save waits, idle, before it exits, where
/// `remake` is what the next edit would pay again without it: the dearest kept front end's
/// last lowering from nothing to kept (`job::giving`'s `Jobs::exit_wait`; zero where none is
/// kept). [`EXIT_TIMES`] times that, at least [`EXIT_LEAST`] and at most [`EXIT_MOST`]. The
/// wait shortens by itself as a remake gets cheap: a hello-world's 100 ms remake waits the
/// 30 s floor; a reth front end's minutes wait the half hour. The daemon's own start, which
/// that edit pays too, is not in `remake`: it is no measurement of this daemon's.
pub(crate) fn exit_after(remake: Duration) -> Duration {
    remake.saturating_mul(EXIT_TIMES).clamp(EXIT_LEAST, EXIT_MOST)
}

/// Whether a daemon idle for `idle_for` exits now: no client or launcher is connected to it
/// (`launchers`, the connections open at the API's socket and the launchers': a daemon never
/// hangs up on a connected client, a live feed's or a waiting script's, for being idle),
/// nothing it holds is undurable (`undurable`: a save, an image's place, a slot being written),
/// and it has waited `wait`: [`exit_after`] of its remake where it built something or took a
/// save since its start, [`EXIT_UNWORKED`] where not (`Jobs::exit_wait`). A plain function of
/// what the loop knows: the loop asks it where it makes its idle looks and exits through
/// `Daemon::stop`. A daemon started to exit once nothing holds it decides that apart
/// ([`super::unheld`]).
pub(crate) fn exit_due(idle_for: Duration, wait: Duration, launchers: usize, undurable: bool) -> bool {
    launchers == 0 && !undurable && idle_for >= wait
}

/// Whether this stretch of idleness gave back yet.
pub(crate) struct Idle {
    /// How long a stretch has to be before it gives back.
    after: Duration,
    /// When the daemon was first seen with nothing to do, since it last worked.
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
        Idle { after, since: None, gave: false }
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
        if self.gave || now.saturating_sub(since) < self.after {
            return false;
        }
        self.gave = true;
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
        gives_back_once(AFTER);
    }

    /// The invariant: a daemon exits only with no launcher connected, nothing undurable, and
    /// past its wait, and always then. Checked against the rule spelled out, over every pairing
    /// of a few idle lengths around a few waits with each condition. Control: a rule that
    /// ignores `launchers` exits with a launcher connected, which the oracle catches.
    #[test]
    fn a_daemon_exits_only_with_nothing_connected_or_undurable_and_past_its_wait() {
        let tick = Duration::from_millis(1);
        let waits = [EXIT_UNWORKED, exit_after(Duration::from_secs(20)), EXIT_MOST];
        let oracle = |due: &dyn Fn(Duration, Duration, usize, bool) -> bool| -> Option<String> {
            for wait in waits {
                for idle_for in [Duration::ZERO, wait - tick, wait, wait * 2] {
                    for launchers in [0, 1, 3] {
                        for undurable in [false, true] {
                            let wanted = launchers == 0 && !undurable && idle_for >= wait;
                            if due(idle_for, wait, launchers, undurable) != wanted {
                                // alloc: a test's failure, once
                                return Some(format!(
                                    "idle {idle_for:?} of a wait of {wait:?}, {launchers} launchers, undurable {undurable}: wanted {wanted}"
                                ));
                            }
                        }
                    }
                }
            }
            None
        };
        assert_eq!(oracle(&exit_due), None);
        let ignores_launchers = |idle_for: Duration, wait: Duration, _: usize, undurable: bool| exit_due(idle_for, wait, 0, undurable);
        assert!(oracle(&ignores_launchers).is_some(), "the control: a rule that ignores the launchers is caught");
    }

    /// The invariant of [`exit_after`]: 64 times the remake, within [`EXIT_LEAST`] and
    /// [`EXIT_MOST`], so the first edit after an exit pays at most a sixty-fourth of the pause
    /// in a remake wherever the clamp does not hold the wait shorter (it never does: the floor
    /// only lengthens it). Control: a constant wait of [`EXIT_LEAST`] lets a 20 s remake be
    /// paid after a 30 s pause, which the oracle catches.
    #[test]
    fn a_daemon_dear_to_remake_waits_longer_before_it_exits() {
        let oracle = |wait_of: &dyn Fn(Duration) -> Duration| -> Option<String> {
            for remake in [Duration::ZERO, Duration::from_millis(100), Duration::from_secs(20), Duration::from_secs(600)] {
                let wait = wait_of(remake);
                if wait < EXIT_LEAST || wait > EXIT_MOST {
                    return Some(format!("a remake of {remake:?} waits {wait:?}, outside its bounds")); // alloc: a test's failure
                }
                if wait < EXIT_MOST && wait < remake.saturating_mul(EXIT_TIMES) {
                    return Some(format!("a remake of {remake:?} waits only {wait:?}")); // alloc: a test's failure
                }
            }
            None
        };
        assert_eq!(oracle(&exit_after), None);
        assert_eq!(exit_after(Duration::from_millis(100)), EXIT_LEAST, "hello-world's remake waits the floor");
        assert!(oracle(&|_| EXIT_LEAST).is_some(), "the control: a constant wait is caught");
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
