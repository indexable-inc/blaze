//! What a driven pool ([`Pool::driven`]) must do, as plain functions: the native test binary
//! (`tests/driven.rs`) runs each, and so does the seed's probe, built for `wasm64-blaze-wasip1`
//! and run under `blaze-seed-run`, where the pool is driven because the target has no threads.
//! None starts a thread, sleeps or catches a panic (the seed aborts on one): each asserts, and
//! an assert that fails ends the run naming the scenario.

use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use blaze_graph::{
    Cancelled, Key,
    pool::Pool,
    pump::Flights,
    sched::{self, Event, Wait},
    topology::Topology,
};
use blaze_core::lock::Lock;

fn key(n: u32) -> Key {
    let mut key = [0u8; 32];
    key[..4].copy_from_slice(&n.to_le_bytes());
    key[4..].copy_from_slice(b"a key of driven pool tests!!");
    Key(key)
}

fn pool() -> Arc<Pool> {
    Arc::new(Pool::driven(&Topology::flat(1)).expect("a driven pool"))
}

/// Every scenario that holds on the seed, by name.
pub const ON_THE_SEED: &[(&str, fn())] = &[
    ("jobs_run_on_the_thread_that_waits_for_them", jobs_run_on_the_thread_that_waits_for_them),
    (
        "a_job_waiting_on_an_event_frees_the_one_worker",
        a_job_waiting_on_an_event_frees_the_one_worker,
    ),
    ("a_wait_that_would_close_a_cycle_is_refused", a_wait_that_would_close_a_cycle_is_refused),
    (
        "a_job_waits_for_a_nested_scope_without_holding_the_worker",
        a_job_waits_for_a_nested_scope_without_holding_the_worker,
    ),
    (
        "a_job_of_one_pool_waits_for_another_pools_job",
        a_job_of_one_pool_waits_for_another_pools_job,
    ),
    (
        "dropping_the_pool_runs_every_job_it_was_given",
        dropping_the_pool_runs_every_job_it_was_given,
    ),
    (
        "a_job_that_gave_way_and_asks_again_at_once_ends",
        a_job_that_gave_way_and_asks_again_at_once_ends,
    ),
];

/// A scope's jobs run on the thread that waits for the scope, as the pool's one worker; a job
/// asked for its worker is told 0.
pub fn jobs_run_on_the_thread_that_waits_for_them() {
    let pool = pool();
    let here = std::thread::current().id();
    let sum = AtomicU64::new(0);
    pool.scope(|scope| {
        for n in 1..=100u64 {
            let (sum, pool) = (&sum, &pool);
            scope.submit(
                None,
                move || {
                    assert_eq!(
                        std::thread::current().id(),
                        here,
                        "job {n} runs on the waiting thread"
                    );
                    assert_eq!(pool.worker(), Some(0), "job {n} runs as the one worker");
                    sum.fetch_add(n, Ordering::Relaxed);
                },
                |ran| ran.expect("no job panics"),
            );
        }
    });
    assert_eq!(sum.load(Ordering::Relaxed), 5050, "every job ran before the scope returned");
    assert_eq!(pool.worker(), None, "the waiting thread is no worker once the scope returned");
    assert_eq!(pool.install(|| pool.worker()), Some(0), "install runs on the worker");
}

/// A job that waits on an event parks and frees the one worker: the job it submitted runs,
/// sees it parked, and fires the event, which resumes it.
pub fn a_job_waiting_on_an_event_frees_the_one_worker() {
    let pool = pool();
    let seen = Arc::new(AtomicUsize::new(usize::MAX));
    let order = Arc::new(Lock::new("graph.test.driven.order", Vec::new()));
    pool.scope(|scope| {
        let (pool, seen, order) = (Arc::clone(&pool), Arc::clone(&seen), Arc::clone(&order));
        scope.submit(
            None,
            move || {
                let event = Event::new();
                let waiter = event.waiter();
                {
                    let (pool_, seen, order) =
                        (Arc::clone(&pool), Arc::clone(&seen), Arc::clone(&order));
                    pool.submit(
                        None,
                        move || {
                            order.lock().push("the firing job runs");
                            seen.store(pool_.parked(), Ordering::SeqCst);
                            event.fire();
                        },
                        |ran| ran.expect("the firing job does not panic"),
                    );
                }
                order.lock().push("the waiting job waits");
                waiter.wait().expect("a wait on an event closes no cycle");
                order.lock().push("the waiting job resumed");
            },
            |ran| ran.expect("the waiting job does not panic"),
        );
    });
    assert_eq!(seen.load(Ordering::SeqCst), 1, "the waiting job was parked while the other ran");
    assert_eq!(
        *order.lock(),
        ["the waiting job waits", "the firing job runs", "the waiting job resumed"]
    );
}

/// Two jobs each make a key and then wait for the other's: the wait that would close the cycle
/// is refused, naming it, rather than both waiting forever; the refused job ends its key, and
/// the other's wait for it ends.
pub fn a_wait_that_would_close_a_cycle_is_refused() {
    let pool = pool();
    let (a, b) = (key(1), key(2));
    let outcomes = Arc::new(Lock::new("graph.test.driven.outcomes", Vec::new()));
    pool.scope(|scope| {
        let (pool, outcomes) = (Arc::clone(&pool), Arc::clone(&outcomes));
        scope.submit(
            None,
            move || {
                assert!(sched::claim(a), "key a is new");
                let made = Event::new();
                let b_claimed = made.waiter();
                {
                    let outcomes = Arc::clone(&outcomes);
                    pool.submit(
                        None,
                        move || {
                            assert!(sched::claim(b), "key b is new");
                            made.fire();
                            let waited = sched::wait(Wait::Key(a));
                            outcomes.lock().push(("b waits for a", waited.is_ok()));
                            sched::key_done(b);
                        },
                        |ran| ran.expect("b's job does not panic"),
                    );
                }
                b_claimed.wait().expect("a wait on an event closes no cycle");
                let waited = sched::wait(Wait::Key(b));
                outcomes.lock().push(("a waits for b", waited.is_ok()));
                sched::key_done(a);
            },
            |ran| ran.expect("a's job does not panic"),
        );
    });
    // b's job is the pool's, not the scope's: the pool's drop runs it to its end.
    drop(Arc::into_inner(pool).expect("the jobs let go of the pool"));
    assert_eq!(
        *outcomes.lock(),
        [("a waits for b", false), ("b waits for a", true)],
        "a's wait closed the cycle"
    );
}

/// A job that opens a scope of its own on the same pool waits for it switched out: the one
/// worker runs the scope's jobs meanwhile, nested three deep.
pub fn a_job_waits_for_a_nested_scope_without_holding_the_worker() {
    fn nest(pool: &Pool, depth: u32) -> u64 {
        if depth == 0 {
            return 1;
        }
        let sum = AtomicU64::new(0);
        pool.scope(|scope| {
            for _ in 0..3 {
                let sum = &sum;
                scope.submit(
                    None,
                    move || sum.fetch_add(nest(pool, depth - 1), Ordering::Relaxed),
                    |ran| {
                        ran.expect("a nested job does not panic");
                    },
                );
            }
        });
        sum.load(Ordering::Relaxed)
    }
    let pool = pool();
    assert_eq!(pool.install(|| nest(&pool, 3)), 27);
}

/// A job of one driven pool waits for a job of another on the same thread: the thread drives
/// both, so the other pool's job runs while the first waits.
pub fn a_job_of_one_pool_waits_for_another_pools_job() {
    let (outer, inner) = (pool(), pool());
    let got = outer.install(|| inner.install(|| inner.worker().map(|it| it + 40)));
    assert_eq!(got, Some(40));
}

/// Dropping the pool runs what it was given: jobs submitted with no scope, one of them waiting
/// on an event a later one fires.
pub fn dropping_the_pool_runs_every_job_it_was_given() {
    let ran = Arc::new(AtomicUsize::new(0));
    {
        let pool = pool();
        let event = Event::new();
        let waiter = event.waiter();
        let ran_ = Arc::clone(&ran);
        pool.submit(
            None,
            move || {
                waiter.wait().expect("a wait on an event closes no cycle");
                ran_.fetch_add(100, Ordering::SeqCst);
            },
            |it| it.expect("the waiting job does not panic"),
        );
        for _ in 0..10 {
            let ran = Arc::clone(&ran);
            pool.submit(
                None,
                move || ran.fetch_add(1, Ordering::SeqCst),
                |it| _ = it.expect("no job panics"),
            );
        }
        pool.submit(None, move || event.fire(), |it| it.expect("the firing job does not panic"));
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "a driven pool runs nothing until its thread waits"
        );
    }
    assert_eq!(ran.load(Ordering::SeqCst), 110, "every job ran before the drop returned");
}

/// How many times a job of [`asked_again_at_once`] asks before it stops and says it never
/// ended: the deadline of a livelock, which gives way this often within milliseconds.
pub const ASKS: u32 = 1000;

/// Where the first two runs of [`asked_again_at_once`] meet: the first waits until the second
/// began, so each holds its key before it asks the other's.
struct Meeting {
    runs: AtomicUsize,
    event: Lock<Option<Event>>,
}

impl Meeting {
    fn meet(&self) {
        match self.runs.fetch_add(1, Ordering::SeqCst) {
            0 => {
                let event = Event::new();
                let waiter = event.waiter();
                *self.event.lock() = Some(event);
                waiter.wait().expect("a wait on an event closes no cycle");
            }
            1 => self.event.lock().take().expect("the first run left its event").fire(),
            _ => {}
        }
    }
}

/// Asks for `mine`, whose run asks for `theirs` (made, when this job runs it, as 0), until it
/// gets a value, asking again at once each time it gives way, as a caller that loops on
/// [`Cancelled`] does: the value and how many times it gave way, or `Err` after [`ASKS`] asks.
fn ask_until_done(
    flights: &Flights<u64>,
    meeting: &Meeting,
    mine: Key,
    theirs: Key,
) -> Result<(u64, u32), u32> {
    for gave_way in 0..ASKS {
        let got = flights.get_or_run(mine, || {
            meeting.meet();
            Ok(flights.get_or_run(theirs, || Ok(0))? + 1)
        });
        match got {
            Ok(value) => return Ok((value, gave_way)),
            Err(Cancelled) => {}
        }
    }
    Err(ASKS)
}

/// Two jobs of `pool` each run a key and ask for the other's, so their waits cross and one gives
/// way; each asks again at once until it gets a value. On one worker (or any pool whose workers
/// are all busy) the job that gave way used to claim its key back before the job it gave way to
/// resumed, and the two gave way to each other forever. A job that gave way now waits for the
/// run it gave way to to end before it claims anything: exactly one gives way, once, and each
/// gets 1. What each job came to.
pub fn asked_again_at_once(pool: &Pool) -> [Result<(u64, u32), u32>; 2] {
    let flights = Flights::<u64>::new();
    let meeting = Meeting { runs: AtomicUsize::new(0), event: Lock::new("graph.test.driven.event", None) };
    let (a, b) = (key(11), key(12));
    let (mut x, mut y) = (None, None);
    pool.scope(|scope| {
        let (flights, meeting, x, y) = (&flights, &meeting, &mut x, &mut y);
        scope.submit(
            None,
            move || ask_until_done(flights, meeting, a, b),
            |it| *x = Some(it.expect("x does not panic")),
        );
        scope.submit(
            None,
            move || ask_until_done(flights, meeting, b, a),
            |it| *y = Some(it.expect("y does not panic")),
        );
    });
    [x.expect("x ran"), y.expect("y ran")]
}

/// [`asked_again_at_once`] on a driven pool, the seed's.
pub fn a_job_that_gave_way_and_asks_again_at_once_ends() {
    let [x, y] = asked_again_at_once(&pool());
    assert_gave_way_once(x, y);
}

/// Both jobs got 1, and of the two exactly one gave way, once.
pub fn assert_gave_way_once(x: Result<(u64, u32), u32>, y: Result<(u64, u32), u32>) {
    let (Ok((in_x, x_gave)), Ok((in_y, y_gave))) = (x, y) else {
        panic!(
            "a job never ended: it gave way {ASKS} times, asking again at once (x {x:?}, y {y:?})"
        );
    };
    assert_eq!((in_x, in_y), (1, 1), "each job's value");
    assert_eq!(x_gave + y_gave, 1, "exactly one gave way, once: x {x_gave}, y {y_gave}");
}

/// A job's timed wait ends once its time is due, the driving thread asleep meanwhile. Not on
/// the seed, whose runner serves no sleep (`poll_oneoff`).
pub fn a_timed_wait_ends_when_due() {
    let pool = pool();
    let waited = pool.install(|| {
        let from = sched::now();
        sched::wait(Wait::Until(from + 20_000_000)).expect("a timed wait closes no cycle");
        sched::now() - from
    });
    assert!(waited >= 20_000_000, "the wait ended after {waited} ns, before its 20 ms");
}
