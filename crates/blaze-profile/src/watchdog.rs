//! The stall watchdog: turns a build that stopped moving into an error naming where, within a
//! bound, instead of a daemon at 0% CPU that only `sample` can explain.
//!
//! [`Watchdog`] is sans IO: the daemon's loop tells it, each turn, the time on its driver's
//! clock (virtual under the simulation), its counts of [`Progress`] (the worker pool's jobs
//! ended; queries completed, memos proved current and loops' items ended; the plane's tasks
//! and units' steps ended: work on what the build
//! waits for, never the daemon's or the pool's housekeeping), and whether work only the daemon's own threads can finish is under way (tasks on its
//! workers; a wait on a child process or on I/O is not the daemon's to finish). It fires on no
//! progress, never on elapsed time: a step or a span that runs long while anything else moves
//! is no stall. When none of the counts moves for the bound while work is under way,
//! [`Watchdog::tick`] says so once, and [`stall`] reads every thread's open spans (what each
//! awaits, and who runs it) and the flight recorder into a [`Stall`], the error the daemon
//! fails the requests with.

use std::{fmt, time::Duration};

use crate::flight::{self, OpenSpan, ThreadNow};

/// How much of the flight recorder a stall keeps: the stretch it was quiet and some before.
const KEPT: Duration = Duration::from_secs(120);

pub struct Watchdog {
    bound_ns: u64,
    seen: Progress,
    /// When the work under way last moved, while there is some.
    quiet_since: Option<u64>,
    /// Said for the present quiet stretch already.
    said: bool,
}

/// What the daemon counts as progress. Each count only grows; work moved when any of them did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// The worker pool's jobs that ran to their end (`blaze_graph::pool::Pool::ended`). Not
    /// spans closed: the daemon's loop turns and the workers' `search`, `idle` and `switch`
    /// spans close on and on while a build stands still, as they did through the poolborrow
    /// stall's ten minutes (2026-11-02).
    pub jobs: u64,
    /// Query work ended (`blaze_query::db::completed`): queries brought up to date under their
    /// flight, run or verified; rows the claim-free verification marked current; and items of
    /// a loop over the pool (`run_all`, `par_for_each`) that ran to their end. The last two
    /// are what a daemon does after a restart, every worker inside one loop's items proving
    /// restored memos: counted as queries run under a flight alone, that was no progress, and
    /// the stop's guard ended such a daemon as stalled (2026-11-11). A worker parked on a
    /// wait, or blocked on a lock, ends none of them.
    pub queries: u64,
    /// The plane's nodes that settled and the jobs' own effects that ended (a placement, a
    /// look, a run of user code's IO): its tasks and units' steps.
    pub steps: u64,
    /// The daemon's looks that found work running though nothing ended: a worker inside a
    /// job's code (`blaze_graph::pool::Pool::working`), as one long compile is, a run of
    /// user code (a build script, a test) alive, or a download that got a byte since the last
    /// look (`blaze_fetch::downloads`: a cold build's LLVM, minutes at a slow link, is work
    /// under way). A pool whose workers only search, sleep and check for stalls never moves
    /// it, nor does a download that gets nothing (it stops itself, naming its address, after
    /// `blaze_fetch::progress::STALL`).
    pub working: u64,
}

impl fmt::Display for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} jobs ended, {} queries completed, {} tasks or steps ended and {} looks found work running",
            self.jobs, self.queries, self.steps, self.working
        )
    }
}

/// No progress from `since_ns` for `for_ns`, on the clock the watchdog was told, with the
/// counts where they stayed (`at`) and the bound that was reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quiet {
    pub since_ns: u64,
    pub for_ns: u64,
    pub at: Progress,
    pub bound_ns: u64,
}

impl Watchdog {
    /// The bound `blaze serve` gives it unless `--stall-bound` names another: longer than any
    /// stretch in which no job ends, no query completes and no task ends anywhere in the
    /// daemon should last, on a loaded host too.
    pub const BOUND: Duration = Duration::from_secs(120);

    pub fn new(bound: Duration) -> Watchdog {
        Watchdog {
            bound_ns: u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX),
            seen: Progress::default(),
            quiet_since: None,
            said: false,
        }
    }

    /// How long no progress may last while work is under way.
    pub fn bound(&self) -> Duration {
        Duration::from_nanos(self.bound_ns)
    }

    /// One turn: `now_ns` on the loop's clock, `progress` the counts, `busy` whether the
    /// daemon's threads have work under way. `Some` once a stretch in which no count moved
    /// reaches the bound, and not again until progress resumes.
    pub fn tick(&mut self, now_ns: u64, progress: Progress, busy: bool) -> Option<Quiet> {
        if progress != self.seen || !busy {
            self.seen = progress;
            self.quiet_since = busy.then_some(now_ns);
            self.said = false;
            return None;
        }
        let since = *self.quiet_since.get_or_insert(now_ns);
        let for_ns = now_ns.saturating_sub(since);
        if self.said || for_ns < self.bound_ns {
            return None;
        }
        self.said = true;
        Some(Quiet { since_ns: since, for_ns, at: progress, bound_ns: self.bound_ns })
    }
}

/// What stayed still, as a stall says it: every count of [`Progress`].
pub const STILL: &str = "no job ended, no query completed, no task or step ended, no job's code or run of user code was running and no download got a byte";

/// A build that stopped: how long nothing moved and the bound it reached, the counts where
/// they stayed, how many tasks were under way, each thread's open spans, and the flight
/// recorder's Chrome trace of the time up to it.
pub struct Stall {
    pub quiet_ns: u64,
    pub bound_ns: u64,
    pub at: Progress,
    pub running: usize,
    pub threads: Vec<ThreadNow>,
    pub trace: Vec<u8>,
}

/// The stall `quiet` found, with `running` tasks under way: every thread read now.
pub fn stall(quiet: Quiet, running: usize) -> Stall {
    Stall {
        quiet_ns: quiet.for_ns,
        bound_ns: quiet.bound_ns,
        at: quiet.at,
        running,
        threads: flight::threads_now(),
        trace: flight::dump(KEPT),
    }
}

impl fmt::Display for Stall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = |ns: u64| ns as f64 / 1e9;
        write!(
            f,
            "blaze stalled: {STILL} for {:.1} s while {} tasks ran (the stall bound is {:.0} s, \
             `blaze serve --stall-bound SECS` sets it); it got to {}",
            secs(self.quiet_ns),
            self.running,
            secs(self.bound_ns),
            self.at
        )?;
        if self.threads.is_empty() {
            return f.write_str("; no thread is inside a span");
        }
        f.write_str("; the threads inside spans:")?;
        threads(f, &self.threads)
    }
}

/// What every thread is inside now, read on demand rather than on a stall: for a client that
/// sees nothing finish (`blaze debug running`, a harness's own watchdog) while the daemon's
/// counts still move.
#[derive(Debug)]
pub struct Running {
    pub threads: Vec<ThreadNow>,
}

/// Every thread read now.
pub fn running() -> Running {
    Running { threads: flight::threads_now() }
}

impl fmt::Display for Running {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.threads.len() {
            0 => f.write_str("blaze running: no thread is inside a span"),
            n => {
                write!(f, "blaze running: {n} threads inside spans:")?;
                threads(f, &self.threads)
            }
        }
    }
}

/// Each thread on a line of its own: its spans outermost first, each with what it is of.
fn threads(f: &mut fmt::Formatter<'_>, threads: &[ThreadNow]) -> fmt::Result {
    for thread in threads {
        f.write_str("\n  ")?;
        let spans = thread.open.iter().map(|span| SpanSaid {
            name: span.name,
            function: span.function.as_deref(),
            krate: span.krate.as_deref(),
            unit: span.unit.as_deref(),
            awaits: span.awaits.as_deref(),
            owner: span.owner,
            for_ns: i64::try_from(span.for_ns).unwrap_or(i64::MAX),
        });
        thread_said(f, &thread.name, thread.tid, spans)?;
        if thread.deeper > 0 {
            write!(f, " > {} more", thread.deeper)?;
        }
    }
    Ok(())
}

/// `name `function` of crate, unit u, awaiting what, run by owner (12.3 s)`, each part only when
/// the span has it.
pub fn open_span(f: &mut impl fmt::Write, span: &OpenSpan) -> fmt::Result {
    let said = SpanSaid {
        name: span.name,
        function: span.function.as_deref(),
        krate: span.krate.as_deref(),
        unit: span.unit.as_deref(),
        awaits: span.awaits.as_deref(),
        owner: span.owner,
        for_ns: i64::try_from(span.for_ns).unwrap_or(i64::MAX),
    };
    write!(f, "{said}")
}

/// An open span as it is said ([`open_span`]), from a live thread or from a history's
/// reading of a daemon that died (`slow::thread_line`): one form for both.
#[derive(Clone, Copy, Debug)]
pub struct SpanSaid<'a> {
    pub name: &'a str,
    pub function: Option<&'a str>,
    pub krate: Option<&'a str>,
    pub unit: Option<&'a str>,
    pub awaits: Option<&'a str>,
    pub owner: Option<u64>,
    pub for_ns: i64,
}

impl fmt::Display for SpanSaid<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)?;
        if let Some(function) = self.function {
            write!(f, " `{function}`")?;
        }
        if let Some(krate) = self.krate {
            write!(f, " of {krate}")?;
        }
        if let Some(unit) = self.unit {
            write!(f, ", unit {unit}")?;
        }
        if let Some(awaits) = self.awaits {
            write!(f, ", awaiting {awaits}")?;
        }
        if let Some(owner) = self.owner {
            write!(f, ", run by task {owner}")?;
        }
        write!(f, " ({:.1} s)", self.for_ns as f64 / 1e9)
    }
}

/// A thread's line ([`threads`]): `name (tid 7): a > b`, of any spans said as [`SpanSaid`].
pub fn thread_said<'a>(
    f: &mut impl fmt::Write,
    name: &str,
    tid: u64,
    spans: impl IntoIterator<Item = SpanSaid<'a>>,
) -> fmt::Result {
    write!(f, "{name} (tid {tid}):")?;
    for (i, span) in spans.into_iter().enumerate() {
        f.write_str(if i == 0 { " " } else { " > " })?;
        write!(f, "{span}")?;
    }
    Ok(())
}

impl fmt::Debug for Stall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stall")
            .field("quiet_ns", &self.quiet_ns)
            .field("bound_ns", &self.bound_ns)
            .field("at", &self.at)
            .field("running", &self.running)
            .field("threads", &self.threads)
            .field("trace_bytes", &self.trace.len())
            .finish()
    }
}

impl std::error::Error for Stall {}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use blaze_core::lock::{Lock, Signal};

    use super::*;

    const SEC: u64 = 1_000_000_000;

    /// A job of the request: inside its span, it waits for the next job, which waits for the
    /// one after, around to itself, until the test lets them go.
    fn job(k: usize, ready: &Barrier, gate: &(Lock<bool>, Signal)) {
        let _span = match k {
            0 => tracing::info_span!(target: "blaze", "job a").entered(),
            1 => tracing::info_span!(target: "blaze", "job b").entered(),
            _ => tracing::info_span!(target: "blaze", "job c").entered(),
        };
        let _wait = tracing::info_span!(target: "blaze", "waits on the next job").entered();
        ready.wait();
        let (released, cv) = gate;
        let mut released = released.lock();
        while !*released {
            cv.wait(&mut released);
        }
    }

    #[test]
    fn a_request_whose_jobs_all_wait_on_each_other_fails_with_the_stall_error_instead_of_hanging() {
        crate::install();
        const JOBS: usize = 3;
        let ready = Arc::new(Barrier::new(JOBS + 1));
        let gate = Arc::new((Lock::new("profile.test.watchdog.gate", false), Signal::new("profile.test.watchdog.opened")));
        let workers: Vec<_> = (0..JOBS)
            .map(|k| {
                let (ready, gate) = (ready.clone(), gate.clone());
                std::thread::Builder::new()
                    .name(format!("worker {k}"))
                    .spawn(move || job(k, &ready, &gate))
                    .unwrap()
            })
            .collect();
        ready.wait();
        // The daemon's loop on a virtual clock, a turn each 100 ms: the jobs never end, so the
        // count of tasks ended stays where it is.
        let mut watchdog = Watchdog::new(Watchdog::BOUND);
        let mut now = 0;
        let quiet = loop {
            assert!(
                now < 3600 * SEC,
                "an hour of the clock went by with no stall said: the request hangs"
            );
            let progress = Progress { jobs: 11, queries: 7, steps: 3, working: 0 };
            if let Some(quiet) = watchdog.tick(now, progress, true) {
                break quiet;
            }
            now += SEC / 10;
        };
        let error = stall(quiet, JOBS);
        let (released, cv) = &*gate;
        *released.lock() = true;
        cv.notify_all();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(error.quiet_ns >= Watchdog::BOUND.as_nanos() as u64, "{error}");
        assert!(
            error.quiet_ns < Watchdog::BOUND.as_nanos() as u64 + SEC,
            "said within a turn of the bound: {error}"
        );
        let text = error.to_string();
        for name in [
            "blaze stalled: no job ended, no query completed, no task or step ended, no job's code or run of user code was running and no download got a byte for",
            "the stall bound is 120 s, `blaze serve --stall-bound SECS` sets it",
            "7 queries completed, 3 tasks or steps ended and 0 looks found work running",
            "job a",
            "job b",
            "job c",
            "waits on the next job",
            "worker 0",
            "worker 2",
            "3 tasks ran",
        ] {
            assert!(text.contains(name), "the stall names `{name}`: {text}");
        }
        let stuck = error
            .threads
            .iter()
            .filter(|it| it.open.iter().any(|span| span.name.starts_with("job ")))
            .count();
        assert_eq!(stuck, JOBS, "{text}");
        let trace: serde_json::Value = serde_json::from_slice(&error.trace).unwrap();
        assert!(trace["traceEvents"].is_array());
    }

    /// The poolborrow stall (2026-11-02): a pool job waits for good on what a lost job would have
    /// made, and the build stands still for ten minutes, while the daemon's `loop turn` spans
    /// and the pool's `search` and `idle` spans (its last sleeper's stall check every two
    /// seconds) close on. Counted as spans closed, that housekeeping was progress and the
    /// watchdog never fired; counted as the pool's jobs ended, it is said at the bound.
    #[test]
    fn a_stall_whose_housekeeping_spans_close_on_is_said_at_the_bound() {
        use blaze_graph::{
            Key,
            pool::Pool,
            sched::{self, Wait},
            topology::Topology,
        };
        crate::install();
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let lost = Key([0x1b; 32]);
        assert!(sched::claim(lost), "the key is new");
        // Ends the key however the test ends, so a failed assertion fails the test rather than
        // leaving the pool's drop waiting on the parked job.
        struct Done(Option<Key>);
        impl Drop for Done {
            fn drop(&mut self) {
                if let Some(key) = self.0.take() {
                    sched::key_done(key);
                }
            }
        }
        let mut done = Done(Some(lost));
        let (tx, rx) = crossbeam_channel::unbounded::<()>();
        pool.submit(None, move || _ = sched::wait(Wait::Key(lost)), move |_| _ = tx.send(()));
        blaze_graph::spin::SpinWait::within("the job to park", Duration::from_secs(60), || {
            pool.parked() == 1
        });
        let bound = Duration::from_secs(10);
        let (mut by_jobs, mut by_spans) = (Watchdog::new(bound), Watchdog::new(bound));
        let (spans_before, mut now, mut said) = (flight::progress(), 0, None);
        // The loop's turns on a virtual clock, a turn each 100 ms of it and 25 ms of the
        // host's, past the pool's two-second stall check: 15 s of the clock.
        for _ in 0..150 {
            drop(tracing::info_span!("loop turn").entered());
            let jobs = Progress { jobs: pool.ended(), queries: 7, steps: 3, working: 0 };
            let spans = Progress { jobs: flight::progress(), queries: 7, steps: 3, working: 0 };
            said = said.or(by_jobs.tick(now, jobs, true));
            assert_eq!(by_spans.tick(now, spans, true), None, "spans closed are progress");
            now += SEC / 10;
            assert!(rx.recv_timeout(Duration::from_millis(25)).is_err(), "the parked job ended");
        }
        assert!(flight::progress() > spans_before, "housekeeping spans closed throughout");
        let quiet =
            said.expect("the stall is said within the bound, though housekeeping spans close on");
        assert_eq!(quiet.for_ns, bound.as_nanos() as u64, "said at the bound: {quiet:?}");
        if let Some(key) = done.0.take() {
            sched::key_done(key);
        }
        rx.recv_timeout(Duration::from_secs(60)).expect("the job ends once its key is done");
    }

    #[test]
    fn work_that_keeps_moving_within_the_bound_never_stalls() {
        let mut rng = blaze_ids::rng::SplitMix64::new(0x2545_f491_4f6c_dd1d);
        let mut next = move || rng.next_u64();
        for _ in 0..64 {
            let mut watchdog = Watchdog::new(Duration::from_secs(10));
            let (mut now, mut progress) = (0, Progress::default());
            for _ in 0..2000 {
                now += next() % (2 * SEC);
                // Moves at least every 10 s minus a turn; is idle (nothing under way) at times.
                let quiet_for = next() % (8 * SEC);
                let busy = next() % 5 != 0;
                assert_eq!(watchdog.tick(now, progress, busy), None);
                now += quiet_for;
                assert_eq!(watchdog.tick(now, progress, busy), None);
                // One count moves at a time, the others standing still for as long as they
                // like: any of them is progress.
                match next() % 3 {
                    0 => progress.jobs += 1 + next() % 3,
                    1 => progress.queries += 1,
                    _ => progress.steps += 1,
                }
            }
        }
    }

    /// The self-build profile's stalls (2026-11-02): one unit's step (lowering the workspace)
    /// ran for minutes, and one span (`impls publish` of core) for a minute on a starved
    /// thread, while other work moved. Neither is a stall; only all counts still is.
    #[test]
    fn a_long_step_or_span_while_anything_else_moves_is_no_stall() {
        let mut watchdog = Watchdog::new(Duration::from_secs(10));
        let mut progress = Progress { jobs: 0, queries: 0, steps: 4, working: 0 };
        // Ten minutes in one step: no step ends, queries complete every 9 s and no job ends.
        for at in 0..=600 {
            if at % 9 == 0 {
                progress.queries += 1;
            }
            assert_eq!(watchdog.tick(at * SEC, progress, true), None, "at {at} s");
        }
        // Then a span that runs for minutes: no query completes and no step ends, but other
        // workers' jobs end.
        for at in 601..=900 {
            if at % 9 == 0 {
                progress.jobs += 1;
            }
            assert_eq!(watchdog.tick(at * SEC, progress, true), None, "at {at} s");
        }
        // Then nothing moves: said at the bound, with the counts where they stayed.
        assert_eq!(watchdog.tick(909 * SEC, progress, true), None);
        let quiet = watchdog.tick(910 * SEC, progress, true).expect("said at the bound");
        assert_eq!(
            quiet,
            Quiet { since_ns: 900 * SEC, for_ns: 10 * SEC, at: progress, bound_ns: 10 * SEC }
        );
    }

    /// What a daemon's workers did by one look of the stop's guard (`serve::stop`'s
    /// `watch_turns`), while its loop is inside one turn: a write whose maintain runs a loop's
    /// items on every worker.
    #[derive(Clone, Copy, Default)]
    struct Did {
        /// Pool jobs ended: none while the loop lasts, as each of its jobs runs many items.
        jobs: u64,
        /// Queries run or verified under their flight.
        flown: u64,
        /// Rows the claim-free verification marked current.
        marked: u64,
        /// The loop's items that ran to their end.
        items: u64,
    }

    /// `blaze_query::db::completed` of `did`, as it counts.
    fn completed(did: Did) -> u64 {
        did.flown + did.marked + did.items
    }

    /// The planted bug: `completed` as it counted until 2026-11-11, queries under a flight
    /// alone.
    fn completed_under_a_flight_alone(did: Did) -> u64 {
        did.flown
    }

    /// The guard's looks, one each 100 ms for 300 s of a virtual clock, at a daemon restarted
    /// on an earlier daemon's state whose loop does not turn: its 64 workers each inside a
    /// job of one loop, proving restored memos claim-free (`moving`: each second every worker
    /// ends an item and marks ten rows) or all parked on each other (not `moving`). The count
    /// of queries is `count`'s; no job ends, no query runs under a flight and `working` is the
    /// guard's constant 0. What the watchdog said, if it did.
    fn restarted_daemon(count: fn(Did) -> u64, moving: bool) -> Option<Quiet> {
        let mut watchdog = Watchdog::new(Watchdog::BOUND);
        let mut did = Did { jobs: 11, flown: 7, ..Did::default() };
        let mut said = None;
        for look in 0..3000u64 {
            if moving && look % 10 == 0 {
                did.items += 64;
                did.marked += 640;
            }
            let progress = Progress { jobs: did.jobs, queries: count(did), steps: 3, working: 0 };
            // The loop has not turned since the first look: work is under way throughout.
            said = said.or(watchdog.tick(look * (SEC / 10), progress, true));
        }
        said
    }

    /// The invariant: a daemon whose workers end a loop's items or prove memos current is
    /// never said to be stalled, however long no pool job ends and no query runs under a
    /// flight; one whose workers are all parked is said to be, at the bound.
    #[test]
    fn workers_ending_loop_items_or_proving_memos_are_progress_and_parked_ones_stall() {
        assert_eq!(restarted_daemon(completed, true), None, "items ended and rows marked");
        let quiet = restarted_daemon(completed, false).expect("every worker parked: a stall");
        let bound = Watchdog::BOUND.as_nanos() as u64;
        assert_eq!((quiet.since_ns, quiet.for_ns, quiet.bound_ns), (0, bound, bound), "{quiet:?}");
        assert_eq!(quiet.at, Progress { jobs: 11, queries: 7, steps: 3, working: 0 });
    }

    /// Counted as queries under a flight alone, the same working daemon is ended as stalled:
    /// the stall of 2026-11-11, `0 looks found work running` with every worker in `run_all`.
    #[test]
    fn the_oracle_catches_a_count_of_queries_under_a_flight_alone() {
        let quiet = restarted_daemon(completed_under_a_flight_alone, true)
            .expect("items ended and rows marked are not counted: said stalled");
        assert_eq!(quiet.for_ns, Watchdog::BOUND.as_nanos() as u64, "{quiet:?}");
    }

    #[test]
    fn a_stall_is_said_once_a_quiet_stretch_and_again_after_progress() {
        let at = |jobs| Progress { jobs, queries: 0, steps: 0, working: 0 };
        let quiet = |since_ns, jobs| {
            Some(Quiet { since_ns, for_ns: 10 * SEC, at: at(jobs), bound_ns: 10 * SEC })
        };
        let mut watchdog = Watchdog::new(Duration::from_secs(10));
        assert_eq!(watchdog.bound(), Duration::from_secs(10));
        assert_eq!(watchdog.tick(0, at(5), true), None);
        assert_eq!(watchdog.tick(9 * SEC, at(5), true), None);
        assert_eq!(watchdog.tick(10 * SEC, at(5), true), quiet(0, 5));
        assert_eq!(watchdog.tick(30 * SEC, at(5), true), None);
        // Nothing under way is no stall, however long.
        assert_eq!(watchdog.tick(31 * SEC, at(6), false), None);
        assert_eq!(watchdog.tick(500 * SEC, at(6), false), None);
        assert_eq!(watchdog.tick(501 * SEC, at(6), true), None);
        assert_eq!(watchdog.tick(511 * SEC, at(6), true), quiet(501 * SEC, 6));
    }

    #[test]
    fn running_names_the_query_key_each_worker_is_inside_and_what_it_waits_on() {
        crate::install();
        let ready = Barrier::new(2);
        let gate = (Lock::new("profile.test.watchdog.gate", false), Signal::new("profile.test.watchdog.opened"));
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("worker running".to_owned())
                .spawn_scoped(scope, || {
                    let _unit =
                        tracing::info_span!(target: "blaze", "unit bodies", krate = "sonic-rs")
                            .entered();
                    let _query = tracing::info_span!(
                        target: "blaze",
                        "infer_query",
                        function = "sonic_rs::serde::de::Deserializer::parse",
                        unit = "sonic_rs.3"
                    )
                    .entered();
                    let awaits = crate::span::Name::of("trait_solve `T: Sized`");
                    let on = blaze_graph::wait::WaitKind::Key;
                    let _wait =
                        crate::span::Wait { awaits, function: awaits, owner: Some(7), on }.open();
                    ready.wait();
                    let (released, cv) = &gate;
                    let mut released = released.lock();
                    while !*released {
                        cv.wait(&mut released);
                    }
                })
                .unwrap();
            ready.wait();
            let running = running();
            let text = running.to_string();
            *gate.0.lock() = true;
            gate.1.notify_all();
            let line = text
                .lines()
                .find(|it| it.contains("worker running"))
                .unwrap_or_else(|| panic!("{text}"));
            for said in [
                "unit bodies of sonic-rs (",
                "infer_query `sonic_rs::serde::de::Deserializer::parse`, unit sonic_rs.3 (",
                "wait `trait_solve `T: Sized``, awaiting trait_solve `T: Sized`, run by task 7 (",
            ] {
                assert!(line.contains(said), "the line says `{said}`: {line}");
            }
            assert!(text.starts_with("blaze running: "), "{text}");
        });
    }
}
