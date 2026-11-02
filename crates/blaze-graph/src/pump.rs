//! The rule for a wait inside a task (#1301, docs/v2/plan/lanes/core.md "Issues this lane takes in"): a
//! task that waits for what another task makes holds no thread while it waits, and a wait that
//! would close a cycle gives way at once with [`Cancelled`], so the task that asked unwinds,
//! releases what it holds and is posted again. No wait can deadlock: every wait is checked
//! against every other before it begins.
//!
//! Every task is a job of the scheduler ([`crate::sched`]): a pool's job on its fiber, or a
//! thread on no fiber, which is one job for its life. Who waits for whom is the scheduler's one
//! graph for the process: a task waits for the task that runs the key it asked
//! ([`Flights::get_or_run`]), and a scope's owner ([`crate::pool::Scope`], so every loop of
//! [`crate::par`]) for each of the scope's jobs that waits. A wait suspends the job (a fiber
//! switches out, a thread parks) on the scheduler's flight of the run it waits for, and resumes
//! once the runner has written its end.
//!
//! The graph costs nothing until a task waits. A run nobody waits for is claimed under its
//! key's bucket latch and ended with one compare-and-swap ([`table`]); its flight in the
//! scheduler is claimed by its first waiter, for it, and ended by the runner only if one was.
//!
//! Nothing here allocates once warm (docs/v2/plan/lanes/mem/core/plane.md) but a value's
//! clones: a waiter is a node on its own stack, linked into its run's entry, and the runner
//! writes each waiter its clone.

use std::cell::Cell;

use crate::{
    Cancelled, Key,
    sched::{self, CounterId, JobId, Wait},
    sync::{
        MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
        thread_local,
    },
};

#[cfg(all(test, blaze_check))]
mod model;
#[cfg(all(test, not(blaze_check)))]
mod oracle;
#[cfg(all(test, not(blaze_check)))]
mod sharded;
mod table;

use table::{Asked, Claim, Idle, Runs, Waiter};

/// The buckets of a [`Flights`]' first table (two runs each, 8 KiB): it grows only as its runs
/// under way at once need.
const LEAST: usize = 64;

/// The executor's one kind of lock, held only to claim or hand over a slot (a key's flight, a
/// wait's edge, a spare pool, a panic to raise later, a cap's bytes), never across a call into
/// work or the database: no guard lives across a query, so none can deadlock or invert a
/// priority (docs/v2/plan/design/concurrency.md, "No guard lives across a query call").
#[allow(clippy::disallowed_types)]
pub(crate) struct Lock<T>(crate::sync::Mutex<T>);

#[allow(clippy::disallowed_types)]
impl<T> Lock<T> {
    pub(crate) const fn new(value: T) -> Lock<T> {
        Lock(crate::sync::Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock()
    }

    /// [`Lock::lock`], said as a wait on `name` ([`crate::wait`]) only when another holds it.
    #[cfg(not(blaze_check))]
    pub(crate) fn lock_said(&self, name: &'static str) -> MutexGuard<'_, T> {
        match self.0.try_lock() {
            Some(held) => held,
            None => crate::wait::wait(crate::wait::WaitOn::Lock(name), None, || self.lock()),
        }
    }

    #[cfg(blaze_check)]
    pub(crate) fn lock_said(&self, _name: &'static str) -> MutexGuard<'_, T> {
        self.lock()
    }

    pub(crate) fn into_inner(self) -> T {
        self.0.into_inner()
    }
}

thread_local! {
    static GAVE_WAYS: Cell<u64> = const { Cell::new(0) };
}

/// How many waits on this thread have given way ([`Cancelled`] for a cycle): a loop tells an
/// item that unwound because it gave way, which it may run again where the cycle is its own,
/// from one that failed.
pub(crate) fn gave_way() -> u64 {
    GAVE_WAYS.with(Cell::get)
}

/// The switching job's give-ways, which move with it (a loop compares them before and after an
/// item, which may resume on another thread).
pub(crate) fn take_gave_ways() -> usize {
    GAVE_WAYS.with(|it| it.replace(0)) as usize
}

pub(crate) fn put_gave_ways(taken: usize) {
    GAVE_WAYS.with(|it| it.set(taken as u64));
}

fn give_way() -> Cancelled {
    GAVE_WAYS.with(|it| it.set(it.get() + 1));
    Cancelled
}

/// In-flight work by key: one run of a key at a time; whoever else asks for it waits, holding
/// no thread, and gets its value. Not a memo: once a run ends its entry goes; the caller keeps
/// values.
///
/// Who gives way: the asker whose wait would close the cycle, at once, and nobody else. When a
/// run ends cancelled (its task gave way, or it panicked) no partial value is given: each of
/// its waiters asks again, the first to ask runs the key itself and the others wait for it,
/// each wait checked again. A waiter gives way only for a cycle its own new wait would close,
/// so of a cycle exactly one task gives way, and the others go on.
///
/// A task that asks for a key it runs itself (the run of a key asks, through its own calls,
/// for the key) is a cycle of the keys themselves, not of the schedule: reposting cannot end
/// it, so it panics, naming the key, and the task fails.
///
/// The runs are in one table of buckets ([`table`]): a run nobody waits for is claimed under
/// its bucket's latch, ended with one compare-and-swap, and allocates nothing once the table is
/// warm.
pub struct Flights<V> {
    runs: Runs<V>,
}

impl<V: Clone + Send + Sync + 'static> Default for Flights<V> {
    fn default() -> Flights<V> {
        Flights::new()
    }
}

impl<V: Clone + Send + Sync + 'static> Flights<V> {
    pub fn new() -> Flights<V> {
        Flights { runs: Runs::new(LEAST) }
    }

    /// The value of `key`: run here by `run` when no one is running it, or waited for.
    /// `Err(Cancelled)` when waiting would close a cycle, or when `run` was cancelled; a run
    /// this caller waited for that was cancelled is asked again (see [`Flights`]).
    pub fn get_or_run(
        &self,
        key: Key,
        run: impl FnOnce() -> Result<V, Cancelled>,
    ) -> Result<V, Cancelled> {
        self.get_or_run_named(key, |it| it.push_str(&hex(&key)), run)
    }

    /// [`Flights::get_or_run`], a wait's span (see [`WaitSpans`]) naming what it waits for by
    /// what `named` writes, called only while a profiler spans waits.
    pub fn get_or_run_named(
        &self,
        key: Key,
        mut named: impl FnMut(&mut String),
        run: impl FnOnce() -> Result<V, Cancelled>,
    ) -> Result<V, Cancelled> {
        sched::miss();
        let me = task(sched::job_here());
        let claim = loop {
            let waiter = Waiter::new();
            let (number, runner) = match self.runs.ask(key, me, sched::innermost_scope(), &waiter, &mut claim_flight) {
                Asked::Run(claim) => break claim,
                Asked::Itself => {
                    panic!("key {} is asked by the task that runs it: its value depends on itself", hex(&key))
                }
                Asked::Wait { flight, runner } => (flight, runner),
            };
            let left = Left { runs: &self.runs, key, waiter: &waiter };
            let waiting = Waiting::begin(crate::wait::WaitKind::Key, &mut named, Some(runner));
            let got = wait(&waiter, flight(number));
            drop(waiting);
            drop(left);
            if let Some(value) = got? {
                return Ok(value);
            }
        };
        let mut ending = Ending { runs: &self.runs, claim, ended: false };
        let result = run();
        ending.end(result.as_ref().ok());
        result
    }

    /// As [`Flights::get_or_run`], but never waits: `None`, running nothing, when another task
    /// runs `key` now.
    pub fn run_if_idle(
        &self,
        key: Key,
        run: impl FnOnce() -> Result<V, Cancelled>,
    ) -> Option<Result<V, Cancelled>> {
        let claim = match self.runs.claim_if_idle(key, task(sched::job_here()), sched::innermost_scope()) {
            Idle::Claimed(claim) => claim,
            Idle::Busy => return None,
            Idle::Itself => {
                panic!("key {} is asked by the task that runs it: its value depends on itself", hex(&key))
            }
        };
        let mut ending = Ending { runs: &self.runs, claim, ended: false };
        let result = run();
        ending.end(result.as_ref().ok());
        Some(result)
    }

    /// Holds the run of `key` for this task until the [`Held`] ends, as though a closure of
    /// [`Flights::run_if_idle`] ran all that while: for a run that is a stretch between two
    /// calls (claimed, then let go of) rather than one call. Whoever asks for `key` meanwhile
    /// waits, holding no thread. `None` when another task runs `key` now.
    pub fn hold(&self, key: Key) -> Option<Held<'_, V>> {
        self.hold_named(key, |it| it.push_str(&hex(&key)))
    }

    /// [`Flights::hold`], the hold a span `hold` (see [`WaitSpans::held`]) naming what is held
    /// by what `named` writes and this task as its holder, so a convoy of waits behind it shows
    /// who holds them up and for how long.
    pub fn hold_named(&self, key: Key, mut named: impl FnMut(&mut String)) -> Option<Held<'_, V>> {
        let me = task(sched::job_here());
        let claim = match self.runs.claim_if_idle(key, me, sched::innermost_scope()) {
            Idle::Claimed(claim) => claim,
            Idle::Busy => return None,
            Idle::Itself => panic!("key {} is held by the task that holds it already", hex(&key)),
        };
        Some(Held { run: Ending { runs: &self.runs, claim, ended: false }, _span: Waiting::holding(&mut named, me) })
    }

    /// How many keys are running now.
    pub fn running(&self) -> usize {
        self.runs.running()
    }

    /// How many waiters `key`'s run has linked now: what a test holding a run waits for before
    /// it ends the run, so each asker is known to wait for it rather than find the key idle.
    pub fn waiters(&self, key: Key) -> usize {
        self.runs.waiters(&key)
    }
}

/// Claims a new flight in the scheduler for task `runner`'s run, begun inside `scope`, as its
/// first waiter links itself: the flight's number.
fn claim_flight(runner: u64, scope: Option<CounterId>) -> u64 {
    let number = FLIGHTS.fetch_add(1, Ordering::Relaxed);
    let claimed = sched::claim_for(flight(number), job(runner), scope);
    assert!(claimed, "flight {number} of a run by task {runner:#x} is new");
    number
}

/// The next run's flight number, from 1: each run waited for has its own, so a run that
/// starts while the last one's waiters are being woken never finds its flight taken.
static FLIGHTS: AtomicU64 = AtomicU64::new(1);

/// The scheduler's key of flight `number`: no content hash's.
fn flight(number: u64) -> Key {
    let mut key = [0u8; 32];
    key[..8].copy_from_slice(&number.to_le_bytes());
    key[8..].copy_from_slice(b"a run's flight, of pump.");
    Key(key)
}

/// Keeps a linked [`Waiter`] in place until its run is done with it: dropped before the run
/// ended (its wait gave way, or an unwind out of it), it unlinks the waiter, or waits for the
/// runner, which has taken the list, to write it its end.
struct Left<'a, V> {
    runs: &'a Runs<V>,
    key: Key,
    waiter: &'a Waiter<V>,
}

impl<V> Drop for Left<'_, V> {
    fn drop(&mut self) {
        self.runs.leave(&self.key, self.waiter);
    }
}

/// Ends a run: on its value, its cancel, or (dropped unended) its panic, which is a cancel to
/// its waiters.
struct Ending<'a, V: Clone + Send + Sync + 'static> {
    runs: &'a Runs<V>,
    claim: Claim<'a>,
    ended: bool,
}

impl<V: Clone + Send + Sync + 'static> Ending<'_, V> {
    fn end(&mut self, value: Option<&V>) {
        self.ended = true;
        if let Some(number) = self.runs.end(&self.claim, value) {
            sched::key_done(flight(number));
        }
    }
}

impl<V: Clone + Send + Sync + 'static> Drop for Ending<'_, V> {
    fn drop(&mut self) {
        if !self.ended {
            self.end(None);
        }
    }
}

/// A run [`Flights::hold`] holds: its waiters get `value` when it [`Held::end`]s, and ask again
/// when it is dropped unended, as for a run that was cancelled.
pub struct Held<'a, V: Clone + Send + Sync + 'static> {
    // Declared before the span: the waiters are let go before the hold's span closes.
    run: Ending<'a, V>,
    _span: Waiting,
}

impl<V: Clone + Send + Sync + 'static> Held<'_, V> {
    /// Ends the run on `value`, each of its waiters given a clone.
    pub fn end(mut self, value: &V) {
        self.run.end(Some(value));
    }
}

/// A task's id in a run's entry: its job's, from 1, so 0 is none.
fn task(job: JobId) -> u64 {
    u64::from(job.0) + 1
}

/// The job of a task's id.
fn job(task: u64) -> JobId {
    JobId(u32::try_from(task - 1).expect("a task id is a job's"))
}

/// What a profiler is told of a wait for a key another task runs, so that the time reads as a
/// span of its own naming what it waits for, not as the asker's: a convoy behind one slow
/// query shows as many waits on it.
#[derive(Clone, Copy)]
pub struct WaitSpans {
    /// Opens the wait's span on this thread, `kind` the [`crate::wait::WaitOn`] variant it is,
    /// `awaited` naming what it waits for and `owner` the task running it, when known: a
    /// number for `end`.
    pub begin: fn(kind: crate::wait::WaitKind, awaited: &str, owner: Option<u64>) -> u64,
    /// Closes the span `begin` gave, on whatever thread the wait ended on (a job on a fiber
    /// may resume on another).
    pub end: fn(u64),
    /// Opens the span of a hold on this thread ([`Flights::hold_named`]), `what` naming what is
    /// held and `holder` the task holding it: what a waiter's `owner` names. Closed by `end`.
    pub held: fn(what: &str, holder: u64) -> u64,
}

static WAIT_SPANS: OnceLock<WaitSpans> = OnceLock::new();

/// Spans every wait for a key from now on with `spans`; `Err` with them when spans were
/// given before.
pub fn span_waits(spans: WaitSpans) -> Result<(), WaitSpans> {
    WAIT_SPANS.set(spans)
}

/// A wait's span, open while this lives; nothing while no profiler spans waits. Every
/// blocking wait of a pool thread opens one: here for a key, through [`crate::wait`] for the rest.
pub(crate) struct Waiting(Option<(fn(u64), u64)>);

impl Waiting {
    pub(crate) fn begin(
        kind: crate::wait::WaitKind,
        named: &mut impl FnMut(&mut String),
        owner: Option<u64>,
    ) -> Waiting {
        Waiting(WAIT_SPANS.get().map(|spans| {
            // alloc: only while a profiler spans waits, once a wait, which suspends
            let mut awaited = String::new();
            named(&mut awaited);
            (spans.end, (spans.begin)(kind, &awaited, owner))
        }))
    }

    /// The span of a hold by `holder` of what `named` writes, open while this lives.
    fn holding(named: &mut impl FnMut(&mut String), holder: u64) -> Waiting {
        Waiting(WAIT_SPANS.get().map(|spans| {
            // alloc: only while a profiler spans waits, once a hold, which lasts a round
            let mut what = String::new();
            named(&mut what);
            (spans.end, (spans.held)(&what, holder))
        }))
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some((end, span)) = self.0 {
            end(span);
        }
    }
}

/// Waits for the run `waiter` is linked into, suspending this task on the run's `flight`: its
/// value, `None` when it was cancelled, or `Cancelled` when the wait would close a cycle. The
/// runner writes every waiter's end before it ends the flight, so once the wait is over the
/// end is there.
fn wait<V>(waiter: &Waiter<V>, flight: Key) -> Result<Option<V>, Cancelled> {
    if waiter.is_running() && sched::wait(Wait::Key(flight)).is_err() {
        return Err(give_way());
    }
    Ok(waiter.ended().unwrap_or_else(|| unreachable!("a wait on a run's flight ended before the run wrote its waiters")))
}

fn hex(key: &Key) -> String {
    // alloc: for the panic of a key that asks for itself, once, or a wait's span while profiling
    key.0.iter().take(8).fold(String::new(), |mut text, it| {
        use std::fmt::Write as _;
        write!(text, "{it:02x}").expect("writing to a String cannot fail");
        text
    })
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    use std::{
        sync::{
            Arc, Barrier,
            atomic::{AtomicU32, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use blaze_alloc_count::{Used, measure};
    use blaze_ids::rng::SplitMix64 as Rng;
    use crossbeam_channel::unbounded;

    use super::{Flights, LEAST};
    use crate::{
        Cancelled, Key,
        pool::Pool,
        sched::fiber::GLOBAL_TAKEN,
        topology::{Cpu, Topology},
    };

    const LONG: Duration = Duration::from_secs(60);

    fn key(n: u32) -> Key {
        let mut key = [0u8; 32];
        key[..4].copy_from_slice(&n.to_le_bytes());
        Key(key)
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        crate::spin::SpinWait::within(what, LONG, done);
    }

    /// Keys one after another, each run ended before the next: an ended run leaves nothing in
    /// the table, which never grows.
    #[test]
    fn keys_that_come_and_go_leave_the_table_as_it_was() {
        let flights = Flights::<u32>::new();
        assert_eq!(flights.runs.size(), (LEAST, 0));
        for n in 0..100_000u32 {
            assert_eq!(flights.get_or_run(key(n), || Ok(n)), Ok(n));
        }
        assert_eq!(flights.runs.size(), (LEAST, 0));
        assert_eq!(flights.running(), 0);
    }

    /// Runs held open while the table grows for them keep their runs and waiters: a key held
    /// while a thousand more are held and thousands of others come and go is still held, and
    /// its waiter still gets its value.
    #[test]
    fn a_grow_keeps_the_runs_under_way_and_their_waiters() {
        let flights = Flights::<u32>::new();
        let mut held = vec![flights.hold(key(1_000_000)).expect("nobody holds it")];
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                _ = crate::sched::job_here();
                flights.get_or_run(key(1_000_000), || panic!("the waiter ran a held key"))
            });
            until("the waiter to link itself", || flights.runs.waiters(&key(1_000_000)) == 1);
            held.extend((1..1000u32).map(|n| flights.hold(key(1_000_000 + n)).expect("nobody holds it")));
            let (buckets, _) = flights.runs.size();
            assert!(buckets > LEAST, "a thousand runs under way grew the table of {LEAST} buckets");
            for n in 0..20_000u32 {
                assert_eq!(flights.get_or_run(key(n), || Ok(n)), Ok(n));
            }
            assert_eq!(flights.running(), 1000);
            assert_eq!(flights.runs.waiters(&key(1_000_000)), 1);
            for (n, held) in held.into_iter().enumerate() {
                held.end(&(n as u32 + 7));
            }
            assert_eq!(waiter.join().unwrap(), Ok(7));
        });
        assert_eq!(flights.running(), 0);
    }

    /// Runs nobody waits for, nested and one after another, never take the scheduler's lock
    /// once the thread is a job of it.
    #[test]
    fn a_run_nobody_waits_for_takes_no_lock_of_the_process() {
        let flights = Flights::<u32>::new();
        assert_eq!(flights.get_or_run(key(u32::MAX), || Ok(0)), Ok(0));
        let before = GLOBAL_TAKEN.with(std::cell::Cell::get);
        for n in 0..1000u32 {
            let value = flights.get_or_run(key(n), || {
                flights.get_or_run(key(n + 1_000_000), || Ok(n)).map(|it| it + 1)
            });
            assert_eq!(value, Ok(n + 1));
        }
        assert_eq!(GLOBAL_TAKEN.with(std::cell::Cell::get), before);
        assert_eq!(flights.running(), 0);
    }

    /// A run ahead runs a key nobody runs, and gives way at once, running nothing, to a key
    /// another task runs; the key is free again after.
    #[test]
    fn a_run_if_idle_never_waits() {
        let flights = Arc::new(Flights::<u32>::new());
        assert_eq!(flights.run_if_idle(key(1), || Ok(7)), Some(Ok(7)));
        let (started, finish) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
        let runner = std::thread::spawn({
            let (flights, started, finish) = (Arc::clone(&flights), Arc::clone(&started), Arc::clone(&finish));
            move || {
                flights.get_or_run(key(2), || {
                    started.wait();
                    finish.wait();
                    Ok(2)
                })
            }
        });
        started.wait();
        let before = GLOBAL_TAKEN.with(std::cell::Cell::get);
        assert_eq!(flights.run_if_idle(key(2), || panic!("ran a key another task runs")), None);
        assert_eq!(GLOBAL_TAKEN.with(std::cell::Cell::get), before, "giving way takes no lock");
        finish.wait();
        assert_eq!(runner.join().unwrap(), Ok(2));
        assert_eq!(flights.run_if_idle(key(2), || Ok(3)), Some(Ok(3)));
        assert_eq!(flights.running(), 0);
    }

    /// A asks, while it runs `ka`, for `kb`, which B runs and which asks for `ka`: whichever
    /// asks second would close the cycle and gives way; the other runs the key given up itself
    /// and is done; the one that gave way, posted again, is done too.
    #[test]
    fn two_tasks_that_ask_for_each_others_keys_end_with_one_cancelled() {
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let flights = Arc::new(Flights::<u32>::new());
        let (tx, rx) = unbounded::<(char, Result<Result<u32, Cancelled>, &'static str>)>();
        let post = |name: char, barrier: Option<Arc<Barrier>>| {
            let (mine, theirs, add) = if name == 'a' { (1, 2, 2) } else { (2, 1, 1) };
            let (flights, tx) = (Arc::clone(&flights), tx.clone());
            pool.submit(
                None,
                move || {
                    flights.get_or_run(key(mine), || {
                        if let Some(barrier) = &barrier {
                            barrier.wait();
                        }
                        flights.get_or_run(key(theirs), || Ok(add)).map(|it| it + 10)
                    })
                },
                move |result| _ = tx.send((name, result.map_err(|_| "panicked"))),
            );
        };
        let barrier = Arc::new(Barrier::new(2));
        post('a', Some(Arc::clone(&barrier)));
        post('b', Some(barrier));
        let heard = || {
            let (name, result) = rx.recv_timeout(LONG).unwrap();
            (name, result.unwrap())
        };
        let first = heard();
        let second = heard();
        let (cancelled, done) = match (first.1, second.1) {
            (Err(Cancelled), Ok(value)) => (first.0, (second.0, value)),
            (Ok(value), Err(Cancelled)) => (second.0, (first.0, value)),
            other => panic!("not one cancelled and one done: {other:?}"),
        };
        // Each one's value is its own key's run over the other's key run inline: 2 + 10 for
        // A, 1 + 10 for B.
        let value_of = |name: char| if name == 'a' { 12 } else { 11 };
        assert_eq!(done.1, value_of(done.0));
        post(cancelled, None);
        let (name, again) = heard();
        assert_eq!(name, cancelled);
        assert_eq!(again, Ok(value_of(cancelled)));
        assert_eq!(flights.running(), 0);
    }

    /// Key 1 run on one thread while another waits for it: what each allocates, and what the
    /// waiter got. Each thread is a job of the scheduler before it is measured.
    fn miss_then_hit<V: Clone + Send + Sync + 'static>(flights: &Flights<V>, value: V) -> (Used, Used, V) {
        let at = key(1);
        let linked = || flights.runs.waiters(&at) > 0;
        std::thread::scope(|scope| {
            let runner = scope.spawn(move || {
                _ = crate::sched::job_here();
                measure(move || {
                    flights.get_or_run(at, move || {
                        until("the waiter to link itself", linked);
                        Ok(value)
                    })
                })
            });
            let waiter = scope.spawn(move || {
                _ = crate::sched::job_here();
                until("the run to begin", || flights.running() == 1);
                measure(|| flights.get_or_run(at, || panic!("the waiter ran the key")))
            });
            let (ran, by_runner) = runner.join().unwrap();
            let (got, by_waiter) = waiter.join().unwrap();
            let got = got.unwrap_or_else(|_| panic!("the wait was cancelled"));
            assert!(ran.is_ok());
            (by_runner, by_waiter, got)
        })
    }

    /// A run with a waiter, warm: neither thread allocates but for the value's clone the
    /// runner makes its waiter (none for a `u64`, one for a `Vec`).
    #[test]
    fn a_miss_then_a_hit_allocate_only_the_values_clone() {
        let numbers = Flights::<u64>::new();
        for round in 0..4 {
            miss_then_hit(&numbers, round);
        }
        let (runner, waiter, got) = miss_then_hit(&numbers, 99);
        assert_eq!((runner.allocations, waiter.allocations, got), (0, 0, 99), "{runner:?} {waiter:?}");

        let vectors = Flights::<Vec<u64>>::new();
        for round in 0..4 {
            miss_then_hit(&vectors, vec![round; 8]);
        }
        let (runner, waiter, got) = miss_then_hit(&vectors, vec![7; 8]);
        assert_eq!((runner.allocations, waiter.allocations), (1, 0), "the runner clones for its one waiter");
        assert_eq!(got, vec![7; 8]);
        assert_eq!(vectors.running(), 0);
    }

    /// Many waiters of one run, linked and unlinked as they come: each gets the run's value.
    #[test]
    fn every_waiter_of_a_run_gets_its_value() {
        let flights = Flights::<Vec<u32>>::new();
        let at = key(3);
        let waiting = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let runner = scope.spawn(|| {
                flights.get_or_run(at, || {
                    until("eight waiters", || flights.runs.waiters(&at) == 8);
                    Ok(vec![5, 6])
                })
            });
            let waiters: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        until("the run to begin", || flights.running() == 1);
                        waiting.fetch_add(1, Ordering::SeqCst);
                        flights.get_or_run(at, || Ok(Vec::new()))
                    })
                })
                .collect();
            assert_eq!(runner.join().unwrap(), Ok(vec![5, 6]));
            for waiter in waiters {
                assert_eq!(waiter.join().unwrap(), Ok(vec![5, 6]));
            }
        });
        assert_eq!(waiting.load(Ordering::SeqCst), 8);
        assert_eq!(flights.running(), 0);
    }

    #[test]
    #[should_panic(expected = "its value depends on itself")]
    fn a_key_that_asks_for_itself_panics() {
        let flights = Flights::<u32>::new();
        _ = flights.get_or_run(key(5), || flights.get_or_run(key(5), || Ok(1)));
    }

    /// Random tasks over random keys whose runs ask for keys after them, on two groups of two
    /// workers: no key runs twice at once, every value is the model's, every task ends (those
    /// that gave way posted again until done), and nothing is left running.
    #[test]
    fn random_waits_end_and_run_each_key_once_at_a_time() {
        const KEYS: usize = 24;
        let topology = Topology {
            cpus: (0..4).map(|id| Cpu { id, group: id / 2, socket: 0, node: 0, fast: true }).collect(),
        };
        for seed in 0..12u64 {
            let mut rng = Rng::new(seed);
            let deps: Vec<Vec<usize>> = (0..KEYS)
                .map(|at| {
                    let after = (KEYS - at - 1) as u64;
                    (0..rng.below(4).min(after)).map(|_| at + 1 + rng.below(after) as usize).collect()
                })
                .collect();
            let mut expected = vec![0u64; KEYS];
            for at in (0..KEYS).rev() {
                expected[at] = at as u64 * 1000 + deps[at].iter().map(|&it| expected[it]).sum::<u64>();
            }
            let tasks: Vec<Vec<usize>> = (0..64)
                .map(|_| (0..1 + rng.below(4)).map(|_| rng.below(KEYS as u64) as usize).collect())
                .collect();
            let spins: Vec<u32> = (0..KEYS).map(|_| rng.below(200) as u32).collect();
            let world = Arc::new(World {
                flights: Flights::new(),
                deps,
                expected,
                spins,
                running: (0..KEYS).map(|_| AtomicU32::new(0)).collect(),
            });
            let pool = Pool::new(&topology, false).unwrap();
            let (tx, rx) = unbounded::<(usize, Result<Result<(), Cancelled>, &'static str>)>();
            let post = |task: usize| {
                let (world, asks, tx) = (Arc::clone(&world), tasks[task].clone(), tx.clone());
                pool.submit(
                    None,
                    move || -> Result<(), Cancelled> {
                        for key in asks {
                            let value = world.get(key)?;
                            assert_eq!(value, world.expected[key]);
                        }
                        Ok(())
                    },
                    move |result| _ = tx.send((task, result.map_err(|_| "panicked"))),
                );
            };
            for task in 0..tasks.len() {
                post(task);
            }
            let mut done = 0;
            while done < tasks.len() {
                let heard = rx
                    .recv_timeout(LONG)
                    .unwrap_or_else(|_| panic!("seed {seed}: a task did not end"));
                match heard {
                    (_, Ok(Ok(()))) => done += 1,
                    (task, Ok(Err(Cancelled))) => post(task),
                    (task, Err(why)) => panic!("seed {seed}: task {task} {why}"),
                }
            }
            assert_eq!(world.flights.running(), 0, "seed {seed}");
            assert!(world.running.iter().all(|it| it.load(Ordering::SeqCst) == 0));
        }
    }

    struct World {
        flights: Flights<u64>,
        deps: Vec<Vec<usize>>,
        expected: Vec<u64>,
        spins: Vec<u32>,
        running: Vec<AtomicU32>,
    }

    impl World {
        fn get(&self, at: usize) -> Result<u64, Cancelled> {
            self.flights.get_or_run(key(at as u32), || {
                struct Out<'a>(&'a AtomicU32);
                impl Drop for Out<'_> {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                assert_eq!(self.running[at].fetch_add(1, Ordering::SeqCst), 0, "key {at} twice");
                let _out = Out(&self.running[at]);
                for spin in 0..self.spins[at] {
                    std::hint::black_box(spin);
                }
                let mut value = at as u64 * 1000;
                for &dep in &self.deps[at] {
                    value += self.get(dep)?;
                }
                Ok(value)
            })
        }
    }
}
