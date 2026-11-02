//! The self-build's deadlock of 2026-10-31 (self7: all 18 workers parked, 14 in
//! `hir_ty::variance`), reduced. A task runs a key and, inside it, waits in a
//! [`Pool::scope`] for its scope's jobs (`base_db::Db::run_all`). A job of the scope, on another
//! worker, asks for that key: its wait is checked against the graph of waits, but the scope's
//! owner publishes no edge to the jobs it waits for, so the walk from the key's runner ends there,
//! the wait is let through, and the job and the owner wait for each other with nothing to pump.
//! The pump's rule (crates/blaze-graph/src/pump.rs): the asker whose wait would close a cycle gives
//! way, so the job gets `Cancelled` and the owner finishes.
#![cfg(not(blaze_check))]

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use blaze_graph::{Cancelled, Key, pool::Pool, pump::Flights, spin::SpinWait, topology::Topology};

/// Far longer than either run takes; a run not done by then is the deadlock.
const LONG: Duration = Duration::from_secs(20);

fn key(n: u32) -> Key {
    let mut key = [0u8; 32];
    key[..4].copy_from_slice(&n.to_le_bytes());
    Key(key)
}

/// Runs key 1 and, inside it, a scope of one job that asks for key 1, made to start on another
/// thread than this one: what the job got, and what this run made of it.
fn run_key_waiting_on_a_scope_job_that_asks_for_it(pool: &Pool, flights: &Flights<u32>) -> Result<u32, Cancelled> {
    flights.get_or_run(key(1), || {
        let started = AtomicBool::new(false);
        let mut asked = None;
        pool.scope(|scope| {
            scope.submit(
                None,
                || {
                    started.store(true, Ordering::SeqCst);
                    flights.get_or_run(key(1), || Ok(7))
                },
                |it| asked = Some(it),
            );
            // Busy here, so this thread doesn't run the job: another worker does.
            SpinWait::within("a worker to start the scope's job", LONG, || started.load(Ordering::SeqCst));
        });
        match asked.expect("a scope runs its job before it returns") {
            Ok(Err(Cancelled)) => Ok(1),
            Ok(Ok(value)) => panic!("the scope's job got {value} for a key its scope's owner runs"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

fn done_in_time(rx: &crossbeam_channel::Receiver<Result<u32, Cancelled>>, owner: &str) -> Result<u32, Cancelled> {
    rx.recv_timeout(LONG).unwrap_or_else(|_| {
        panic!(
            "no answer in {} s: the scope's job waits for key 1, whose runner ({owner}) waits for the \
             scope's job, and every worker is parked (a cycle the pump's check let through)",
            LONG.as_secs()
        )
    })
}

/// The owner is a worker's job, as `Db::run_all` inside a query is in a build.
#[test]
fn no_wait_chain_through_a_scope_leaves_every_worker_parked() {
    let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
    let flights = Arc::new(Flights::<u32>::new());
    let (tx, rx) = crossbeam_channel::unbounded();
    {
        let (inner, flights) = (pool.clone(), flights.clone());
        pool.submit(
            None,
            move || run_key_waiting_on_a_scope_job_that_asks_for_it(&inner, &flights),
            move |it| _ = tx.send(it.unwrap()),
        );
    }
    assert_eq!(done_in_time(&rx, "a worker's job"), Ok(1), "the scope's job gives way");
    assert_eq!(flights.running(), 0);
}

/// The owner is a thread of no pool, a task of its own for the check.
#[test]
fn no_wait_chain_through_a_clients_scope_leaves_every_worker_parked() {
    let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
    let flights = Arc::new(Flights::<u32>::new());
    let (tx, rx) = crossbeam_channel::unbounded();
    {
        let (pool, flights) = (pool.clone(), flights.clone());
        // Detached: on the deadlock it never returns, and the test fails on the timeout instead.
        thread::spawn(move || _ = tx.send(run_key_waiting_on_a_scope_job_that_asks_for_it(&pool, &flights)));
    }
    assert_eq!(done_in_time(&rx, "a client thread"), Ok(1), "the scope's job gives way");
    assert_eq!(flights.running(), 0);
}
