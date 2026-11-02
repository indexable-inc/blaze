//! A job's task frame is a thread-local carried with it: a job that parks inside a task may
//! resume on another worker, and its task's end puts the enclosing frame back on the thread it
//! ends on, so the enclosing task is the innermost again there.
#![cfg(not(blaze_check))]

use std::{
    ptr,
    thread::{self, ThreadId},
    time::Duration,
};

use blaze_graph::{pool::Pool, sched::Event, topology::Topology};
use blaze_mem::{Tag, frame};
use crossbeam_channel::unbounded;

/// Parked at once over four workers, a round's jobs.
const JOBS: usize = 64;

/// Rounds run until one has a job that resumed on a worker other than its own: where a wake
/// goes is the scheduler's choice, so the test runs until it crossed threads, within this.
const ROUNDS: usize = 16;

const LONG: Duration = Duration::from_secs(60);

fn message(panic: &(dyn std::any::Any + Send)) -> String {
    panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|it| (*it).to_owned())).unwrap_or_default()
}

/// The innermost task's frame on this thread, by address.
fn current() -> usize {
    frame::current(|it| ptr::from_ref(it).addr())
}

/// Each job opens a task, a task inside it, and parks in the inner one on an event; the events
/// fire once every job has parked, so the jobs resume wherever a worker is free. After each
/// inner task ends, the outer task's frame is current on the thread it ended on, a new task
/// starts inside it, and after the outer task ends the thread is in none. Rounds run until one
/// has a job that resumed on another worker.
#[test]
fn a_task_that_resumes_on_another_thread_ends_on_that_thread() {
    let pool = Pool::new(&Topology::flat(4), false).unwrap();
    for _ in 0..ROUNDS {
        if round(&pool) > 0 {
            return;
        }
    }
    panic!("no job of {ROUNDS} rounds of {JOBS} resumed on another worker, so none crossed threads");
}

/// One round: how many of its jobs resumed on a worker other than the one they parked on.
fn round(pool: &Pool) -> usize {
    let (parked_tx, parked_rx) = unbounded::<()>();
    let (done_tx, done_rx) = unbounded::<Result<bool, String>>();
    // alloc: a test's events, once a round
    let events: Vec<Event> = (0..JOBS).map(|_| Event::new()).collect();
    for event in &events {
        let waiter = event.waiter();
        let parked_tx = parked_tx.clone();
        let done_tx = done_tx.clone();
        pool.submit(
            None,
            move || {
                let moved = frame::task(Tag::Other, |outer| {
                    let outer = ptr::from_ref(outer).addr();
                    let moved = frame::task(Tag::Other, |inner| {
                        let inner = ptr::from_ref(inner).addr();
                        let parked: ThreadId = thread::current().id();
                        parked_tx.send(()).unwrap();
                        waiter.wait().expect("an event's wait closes no cycle");
                        assert_eq!(current(), inner, "the inner task's frame is current again where it resumed");
                        thread::current().id() != parked
                    });
                    assert_eq!(current(), outer, "the outer task's frame is current where the inner one ended");
                    frame::task(Tag::Other, |_| ());
                    moved
                });
                assert!(!frame::in_task(), "the outer task's end leaves no frame current where it ended");
                moved
            },
            move |it| _ = done_tx.send(it.map_err(|it| message(&*it))),
        );
    }
    for _ in 0..JOBS {
        parked_rx.recv_timeout(LONG).expect("each job parks within LONG");
    }
    drop(events);
    let mut moved = 0;
    for _ in 0..JOBS {
        let got = done_rx.recv_timeout(LONG).expect("each job ends within LONG");
        moved += usize::from(got.unwrap_or_else(|why| panic!("a job failed: {why}")));
    }
    moved
}
