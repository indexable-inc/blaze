//! Driven pools ([`blaze_graph::pool::Pool::driven`]): the pool a target with no threads gets,
//! whose one worker runs on the thread that waits for its jobs. The scenarios are shared with
//! the seed's probe, which runs them built for `wasm64-blaze-wasip1`. The scheduler's wait
//! graph and stall check are the process's, so each test holds [`ONE`] and the binary is their
//! own.
#![cfg(not(blaze_check))]
#![allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use blaze_graph::{
    Key,
    pool::Pool,
    sched::{self, Wait},
    topology::Topology,
};
use blaze_core::lock::Lock;

#[path = "driven/scenarios.rs"]
mod scenarios;

/// One test at a time: the wait graph and its stall check are the whole process's.
static ONE: Lock<()> = Lock::new("graph.test.driven.one", ());

fn run(name: &str) {
    let _one = ONE.lock();
    let (_, scenario) =
        scenarios::ON_THE_SEED.iter().find(|(it, _)| *it == name).expect("a scenario of that name");
    scenario();
}

#[test]
fn jobs_run_on_the_thread_that_waits_for_them() {
    run("jobs_run_on_the_thread_that_waits_for_them");
}

#[test]
fn a_job_waiting_on_an_event_frees_the_one_worker() {
    run("a_job_waiting_on_an_event_frees_the_one_worker");
}

#[test]
fn a_wait_that_would_close_a_cycle_is_refused() {
    run("a_wait_that_would_close_a_cycle_is_refused");
}

#[test]
fn a_job_waits_for_a_nested_scope_without_holding_the_worker() {
    run("a_job_waits_for_a_nested_scope_without_holding_the_worker");
}

#[test]
fn a_job_of_one_pool_waits_for_another_pools_job() {
    run("a_job_of_one_pool_waits_for_another_pools_job");
}

#[test]
fn dropping_the_pool_runs_every_job_it_was_given() {
    run("dropping_the_pool_runs_every_job_it_was_given");
}

#[test]
fn a_job_that_gave_way_and_asks_again_at_once_ends() {
    run("a_job_that_gave_way_and_asks_again_at_once_ends");
}

/// The immediate retry on a pool of one worker with a thread of its own, as native code runs:
/// the job given way to is queued behind the one that gave way, as on a driven pool.
#[test]
fn a_job_that_gave_way_and_asks_again_at_once_ends_on_a_threaded_pool() {
    let _one = ONE.lock();
    let pool = Pool::new(&Topology::flat(1), false).expect("a pool of one thread");
    let [x, y] = scenarios::asked_again_at_once(&pool);
    scenarios::assert_gave_way_once(x, y);
}

#[test]
fn every_seed_scenario_has_a_test() {
    let source = include_str!("driven.rs");
    for (name, _) in scenarios::ON_THE_SEED {
        assert!(source.contains(&format!("fn {name}()")), "scenario {name} has no #[test] here");
    }
}

#[test]
fn a_timed_wait_ends_when_due() {
    let _one = ONE.lock();
    scenarios::a_timed_wait_ends_when_due();
}

/// A thread of no pool queues a job on a driven pool while its thread waits for a scope: the
/// waiting thread wakes and runs it.
#[test]
fn a_job_queued_from_another_thread_runs_on_the_driving_thread() {
    let _one = ONE.lock();
    let pool = Arc::new(Pool::driven(&Topology::flat(1)).expect("a driven pool"));
    let here = std::thread::current().id();
    let ran = Arc::new(AtomicUsize::new(0));
    pool.scope(|scope| {
        let event = sched::Event::new();
        let waiter = event.waiter();
        scope.submit(None, move || waiter.wait().expect("no cycle"), |it| it.expect("no panic"));
        let (pool, ran) = (Arc::clone(&pool), Arc::clone(&ran));
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            pool.submit(
                None,
                move || {
                    assert_eq!(std::thread::current().id(), here, "it runs on the driving thread");
                    ran.store(1, Ordering::SeqCst);
                    event.fire();
                },
                |it| it.expect("no panic"),
            );
        });
    });
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

/// A job claims a key and never ends it, waiting on a scope that nothing ends; another waits
/// for the key. With the one thread waiting on them, each fails with the stall, naming the
/// key, rather than the thread waiting forever.
#[test]
fn a_driven_pools_stall_fails_its_waiting_jobs() {
    let _one = ONE.lock();
    let pool = Arc::new(Pool::driven(&Topology::flat(1)).expect("a driven pool"));
    let at = Key([0x5a; 32]);
    let failed = Arc::new(Lock::new("graph.test.driven.failed", Vec::new()));
    let message = |panic: &(dyn std::any::Any + Send)| {
        panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|it| (*it).to_owned()))
            .unwrap_or_default()
    };
    pool.scope(|scope| {
        let failed_ = Arc::clone(&failed);
        scope.submit(
            None,
            move || {
                assert!(sched::claim(at), "the key is new");
                let counter = sched::open_scope();
                _ = sched::wait(Wait::Counter(counter));
            },
            move |it| failed_.lock().push(it.err().map(|panic| message(&*panic))),
        );
        let failed_ = Arc::clone(&failed);
        scope.submit(
            None,
            move || {
                _ = sched::wait(Wait::Key(at));
            },
            move |it| failed_.lock().push(it.err().map(|panic| message(&*panic))),
        );
    });
    let failed = failed.lock();
    assert_eq!(failed.len(), 2, "{failed:?}");
    for it in failed.iter() {
        let it = it.as_deref().unwrap_or("ended without failing");
        assert!(it.contains("stalled"), "{it}");
    }
}
