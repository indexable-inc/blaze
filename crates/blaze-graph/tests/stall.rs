//! The scheduler's stall: once every pool is idle and no wait in the process's graph can end,
//! each waiting job fails naming what it waits for. The check is of the whole process, so any
//! other test's live job (a client thread idle on a channel, a pool still busy) holds it off;
//! these tests run in a binary of their own, where the only jobs are theirs.
#![cfg(not(blaze_check))]

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
    spin::SpinWait,
    topology::Topology,
};
use crossbeam_channel::{RecvTimeoutError, Sender, unbounded};

/// Far past a stall's detection (the pool's two seconds idle, then the check).
const LONG: Duration = Duration::from_secs(60);

/// Past a test's own bounds (its waits of [`LONG`]) and the pool's drop after a failure, which
/// waits for parked jobs with no bound of its own.
const TEST: Duration = Duration::from_secs(150);

fn key(n: u32) -> Key {
    let mut key = [0u8; 32];
    key[..4].copy_from_slice(&n.to_le_bytes());
    Key(key)
}

fn message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|it| (*it).to_owned()))
        .unwrap_or_default()
}

/// A test's deadline, watched from a thread of its own: declared first, it is dropped last,
/// after the pool, so a failure that then blocks in the pool's drop aborts naming the test
/// rather than holding the run.
struct Deadline(#[allow(dead_code, reason = "held for its drop, which ends the watch")] Sender<()>);

impl Deadline {
    fn new(test: &'static str) -> Deadline {
        let (ended, watched) = unbounded::<()>();
        std::thread::spawn(move || {
            if watched.recv_timeout(TEST) == Err(RecvTimeoutError::Timeout) {
                // stderr itself: an abort ends nothing the harness keeps.
                _ = std::io::Write::write_fmt(
                    &mut std::io::stderr(),
                    format_args!(
                        "{test} ran past {TEST:?}: blocked where no bound of its own reaches\n"
                    ),
                );
                std::process::abort();
            }
        });
        Deadline(ended)
    }
}

/// A job claims a key and then waits on a scope of its own that nothing will end; another
/// waits for the key, which nobody will finish. Once the pool is idle a while, each fails
/// with the stall, naming the key and who claimed it, rather than both waiting forever;
/// the pool goes on running jobs.
#[test]
fn a_job_waiting_on_a_key_nobody_will_finish_fails_naming_the_key() {
    let _deadline = Deadline::new("a_job_waiting_on_a_key_nobody_will_finish_fails_naming_the_key");
    let pool = Pool::new(&Topology::flat(2), false).unwrap();
    let (tx, rx) = unbounded::<Result<(), String>>();
    let at = key(7_000);
    let claimed = Arc::new(AtomicUsize::new(0));
    {
        let (tx, claimed) = (tx.clone(), Arc::clone(&claimed));
        pool.submit(
            None,
            move || {
                assert!(sched::claim(at), "the key is new");
                claimed.store(1, Ordering::SeqCst);
                let never = sched::open_scope();
                let _ = sched::wait(Wait::Counter(never));
            },
            move |it| _ = tx.send(it.map_err(|it| message(&*it))),
        );
    }
    SpinWait::within("the key to be claimed", LONG, || claimed.load(Ordering::SeqCst) == 1);
    {
        let tx = tx.clone();
        pool.submit(
            None,
            move || {
                let _ = sched::wait(Wait::Key(at));
            },
            move |it| _ = tx.send(it.map_err(|it| message(&*it))),
        );
    }
    let named = at.0.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write as _;
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
        text
    });
    for _ in 0..2 {
        let failed = rx
            .recv_timeout(LONG)
            .expect("a stalled job fails within LONG")
            .expect_err("a stalled job fails");
        assert!(failed.starts_with("stalled: "), "{failed}");
        assert!(
            failed.contains(&format!("key {named}, claimed by job ")),
            "the stall names the key: {failed}"
        );
    }
    pool.submit(None, || (), move |it| _ = tx.send(it.map_err(|it| message(&*it))));
    assert_eq!(
        rx.recv_timeout(LONG).expect("the pool runs a job after the stall"),
        Ok(()),
        "the pool ran no job after the stall"
    );
}

/// A job waits inside a scope on a key whose claimer waits on nothing that ends: the stall
/// fails the scope's job and the claimer, but the scope's caller, waiting on its scope's job,
/// is left until that job ended and returns as a scope does, never unwinding while a job
/// borrowing its frame runs.
#[test]
fn a_stall_inside_a_scope_fails_its_job_before_its_caller() {
    let _deadline = Deadline::new("a_stall_inside_a_scope_fails_its_job_before_its_caller");
    let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
    let (tx, rx) = unbounded::<Result<(), String>>();
    let at = key(7_100);
    let claimed = Arc::new(AtomicUsize::new(0));
    {
        let (tx, claimed) = (tx.clone(), Arc::clone(&claimed));
        pool.submit(
            None,
            move || {
                assert!(sched::claim(at), "the key is new");
                claimed.store(1, Ordering::SeqCst);
                let never = sched::open_scope();
                let _ = sched::wait(Wait::Counter(never));
            },
            move |it| _ = tx.send(it.map_err(|it| message(&*it))),
        );
    }
    SpinWait::within("the key to be claimed", LONG, || claimed.load(Ordering::SeqCst) == 1);
    let ended = Arc::new(AtomicUsize::new(0));
    {
        let (tx, ended) = (tx.clone(), ended);
        let scoped = Arc::clone(&pool);
        pool.submit(
            None,
            move || {
                let (job_tx, job_rx) = unbounded::<Result<(), String>>();
                let borrowed = &ended;
                scoped.scope(|scope| {
                    scope.submit(
                        None,
                        move || {
                            let _ = sched::wait(Wait::Key(at));
                            borrowed.store(1, Ordering::SeqCst);
                        },
                        move |it| _ = job_tx.send(it.map_err(|it| message(&*it))),
                    );
                });
                let failed =
                    job_rx.try_recv().expect("the scope returned once its job ended").unwrap_err();
                assert!(failed.starts_with("stalled: "), "{failed}");
                assert_eq!(ended.load(Ordering::SeqCst), 0, "the scope's job failed at its wait");
            },
            move |it| _ = tx.send(it.map_err(|it| message(&*it))),
        );
    }
    let mut got = [
        rx.recv_timeout(LONG).expect("the first of the two ends within LONG"),
        rx.recv_timeout(LONG).expect("the second of the two ends within LONG"),
    ];
    got.sort();
    assert!(got[0].is_ok(), "the scope's caller saw its job's stall: {got:?}");
    assert!(
        got[1].as_ref().is_err_and(|it| it.starts_with("stalled: ")),
        "the key's claimer fails: {got:?}"
    );
    pool.submit(None, || (), move |it| _ = tx.send(it.map_err(|it| message(&*it))));
    assert_eq!(
        rx.recv_timeout(LONG).expect("the pool runs a job after the stall"),
        Ok(()),
        "the pool ran no job after the stall"
    );
}

fn hex(key: Key) -> String {
    key.0.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write as _;
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
        text
    })
}

/// The poolborrow stall (2026-11-02): the daemon's loop thread joins the wait graph at its
/// first query (a client, a thread on no fiber) and then runs, waiting on nothing in it, for
/// the process's life. Its job held the pool's stall check off for ten minutes while the pool's
/// jobs waited on a scope a lost job never ended. With such a client in the graph, the stall
/// is still found within the bound, naming the key and its claimer.
#[test]
fn a_client_thread_in_the_graph_does_not_hide_a_stall_of_the_pools_jobs() {
    let _deadline =
        Deadline::new("a_client_thread_in_the_graph_does_not_hide_a_stall_of_the_pools_jobs");
    let (joined_tx, joined) = unbounded::<()>();
    let (stop_tx, stop) = unbounded::<()>();
    let client = std::thread::spawn(move || {
        let _ = sched::job_here();
        joined_tx.send(()).expect("the test waits for the client to join");
        // Runs on, outside the graph, as the daemon's loop does between its queries.
        let _ = stop.recv();
    });
    joined.recv_timeout(LONG).expect("the client joins the graph");
    let pool = Pool::new(&Topology::flat(2), false).unwrap();
    let (tx, rx) = unbounded::<Result<(), String>>();
    let at = key(7_200);
    let claimed = Arc::new(AtomicUsize::new(0));
    {
        let (tx, claimed) = (tx.clone(), Arc::clone(&claimed));
        pool.submit(
            None,
            move || {
                assert!(sched::claim(at), "the key is new");
                claimed.store(1, Ordering::SeqCst);
                // A scope a lost job never ends: its count is never given back.
                let never = sched::open_scope();
                let _ = sched::wait(Wait::Counter(never));
            },
            move |it| _ = tx.send(it.map_err(|it| message(&*it))),
        );
    }
    SpinWait::within("the key to be claimed", LONG, || claimed.load(Ordering::SeqCst) == 1);
    {
        let tx = tx.clone();
        pool.submit(
            None,
            move || _ = sched::wait(Wait::Key(at)),
            move |it| _ = tx.send(it.map_err(|it| message(&*it))),
        );
    }
    for _ in 0..2 {
        let failed = rx
            .recv_timeout(LONG)
            .expect("a stalled job fails within LONG though a client runs in the graph")
            .expect_err("a stalled job fails");
        assert!(failed.starts_with("stalled: "), "{failed}");
        assert!(
            failed.contains(&format!("key {}, claimed by job ", hex(at))),
            "the stall names the key: {failed}"
        );
    }
    stop_tx.send(()).expect("the client waits to be stopped");
    client.join().expect("the client ends");
}

/// A pool job waits for a key a client (this test's thread, as the daemon's loop holding a
/// flight) claimed and does not end. The pool's own check finds no stall, since the client may
/// end it; once the outside says it is still ([`sched::stall_while_quiet`], as the daemon's
/// loop does when nothing can move its build), the wait fails with the stall, naming the key,
/// its claimer and the waiter.
#[test]
fn a_wait_on_a_key_a_still_client_holds_fails_once_the_outside_says_it_is_quiet() {
    let _deadline = Deadline::new(
        "a_wait_on_a_key_a_still_client_holds_fails_once_the_outside_says_it_is_quiet",
    );
    let pool = Pool::new(&Topology::flat(2), false).unwrap();
    let (tx, rx) = unbounded::<Result<(), String>>();
    let at = key(7_300);
    assert!(sched::claim(at), "the key is new");
    pool.submit(
        None,
        move || _ = sched::wait(Wait::Key(at)),
        move |it| _ = tx.send(it.map_err(|it| message(&*it))),
    );
    SpinWait::within("the waiter to park", LONG, || pool.parked() == 1);
    // Past the pool's own check (two seconds idle): a client's key is no stall to it.
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)),
        Err(RecvTimeoutError::Timeout),
        "the pool's check failed a wait a client may end"
    );
    let mut waits = None;
    SpinWait::within("the quiet check to find the stall", LONG, || {
        waits = sched::stall_while_quiet();
        waits.is_some()
    });
    let said = waits.expect("found").to_string();
    assert!(
        said.contains(&format!("waits for key {}, claimed by job ", hex(at))),
        "the stall names the key and its claimer: {said}"
    );
    let failed =
        rx.recv_timeout(LONG).expect("the waiter fails within LONG").expect_err("the waiter fails");
    assert!(failed.starts_with("stalled: "), "{failed}");
    assert!(
        failed.contains(&format!("key {}, claimed by job ", hex(at))),
        "the waiter's failure names the key: {failed}"
    );
    sched::key_done(at);
}
