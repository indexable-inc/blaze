//! The named strategies against programs whose bug needs a known number of preemptions or
//! ordering constraints, against a bug planted in the strategy itself, and against the default
//! search (S6, test.md "End state: the search itself"). Each test's doc holds the lines it
//! prints; counts marked "to verify" were reasoned, not run.

use std::{cell::Cell as StdCell, rc::Rc as StdRc};

use super::{Refused, Runs, Searched, Strategy, priority_runs};
use crate::{
    Builder,
    path::schedules::Plant,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    thread,
};

fn builder() -> Builder {
    let mut builder = Builder::new();
    builder.max_preemptions = Some(3);
    builder.random = None;
    builder.replay = None;
    builder
}

/// A two-thread counter with a lost update: each thread reads the count under the lock, lets
/// it go, and writes the count it read plus one under the lock again. A Mutex, not an atomic,
/// so the value read is the last one written (no weak-memory choice): the bug is the
/// interleaving alone. Within one preemption it needs the closure's own thread (thread 0)
/// preempted between its read and its write; the spawned thread can run first only by a
/// preemption of thread 0 and would then need a second one.
fn lost_update() {
    let n = Arc::new(Mutex::new(0u32));
    let other = {
        let n = n.clone();
        thread::spawn(move || {
            let v = *n.lock().unwrap();
            *n.lock().unwrap() = v + 1;
        })
    };
    let v = *n.lock().unwrap();
    *n.lock().unwrap() = v + 1;
    other.join().unwrap();
    assert_eq!(*n.lock().unwrap(), 2, "a lost update");
}

/// (a)'s claim as a value: bound 0 finds nothing, bound 1 finds the lost update, and the
/// schedule line replays it. `Err`: which part failed.
fn lost_update_found(plant: Plant) -> Result<(Searched, Searched, Searched), String> {
    let builder = builder();
    let run = |bound| {
        builder
            .search_planted(Strategy::Preemption { bound }, plant, lost_update)
            .unwrap_or_else(|refused| panic!("{refused}"))
    };
    let none = run(0);
    let one = run(1);
    if let Some(found) = &none.found {
        return Err(format!("bound 0 found it: {}", found.what));
    }
    let Some(found) = &one.found else {
        return Err(format!("bound 1 did not find the lost update: {one}"));
    };
    if !found.what.contains("a lost update") {
        return Err(format!("bound 1 found something else: {}", found.what));
    }
    let again = builder
        .replay_line(&found.schedule, lost_update)
        .unwrap_or_else(|refused| panic!("{refused}"));
    match &again.found {
        Some(again_found)
            if again_found.what == found.what && again_found.schedule == found.schedule => {}
        _ => return Err(format!("replaying `{}` did not fail the same way: {again}", found.schedule)),
    }
    Ok((none, one, again))
}

/// (a) Prints (counts to verify):
/// `blaze-check preemption bound 0: 1 schedules, nothing found`
/// `blaze-check preemption bound 1: <m> schedules, found at schedule <m>: thread 0 ... panicked: assertion `left == right` failed: a lost update ...`
/// `blaze-check schedule v1 <trail>`
/// `blaze-check replay: 1 schedules, found at schedule 1: <the same what>`
#[test]
fn a_lost_update_needs_one_preemption_and_replays_from_its_line() {
    let (none, one, again) = lost_update_found(Plant::None).unwrap_or_else(|why| panic!("{why}"));
    println!("{none}");
    println!("{one}");
    let found = one.found.as_ref().expect("bound 1 found it");
    println!("{}", found.schedule);
    println!("{again}");
    assert!(found.schedule.starts_with("blaze-check schedule v1 "), "{}", found.schedule);
    assert!(found.report.ends_with(&found.schedule), "{}", found.report);
    assert!(none.executions < one.executions, "{none} / {one}");
    assert_eq!(again.executions, 1, "{again}");
}

/// (c) The planted bug: a preemption bound that never preempts the first thread (thread 0)
/// misses the lost update, so (a)'s check fails. Prints:
/// `planted SpareFirstThread: (a) fails: bound 1 did not find the lost update: blaze-check preemption bound 1: <n> schedules, nothing found`
#[test]
fn a_bound_that_spares_the_first_thread_misses_the_lost_update() {
    match lost_update_found(Plant::SpareFirstThread) {
        Ok((_, one, _)) => panic!("the planted bug went unseen: {one}"),
        Err(why) => {
            println!("planted SpareFirstThread: (a) fails: {why}");
            assert!(why.starts_with("bound 1 did not find"), "{why}");
        }
    }
}

/// Three threads; the bug needs three ordering constraints: A's write of `x` before B reads it
/// (so B writes `s = 1`), B's write of `s = 1` before A reads `s`, and A's read before B's
/// write of `s = 2`. A third thread C counts on its own. Mutexes, so every read is the last
/// write (no weak-memory choice). With no priority drop each thread runs until it ends (they
/// never wait on each other's locks long enough to interleave: a thread holds a lock only while
/// it runs), so depth 1 can never find it; depth 3 (two drops: A after writing `x`, B after
/// writing `s = 1`) can.
fn depth_three() {
    let x = Arc::new(Mutex::new(0u32));
    let s = Arc::new(Mutex::new(0u32));
    let z = Arc::new(AtomicUsize::new(0));
    let a = {
        let (x, s) = (x.clone(), s.clone());
        thread::spawn(move || {
            *x.lock().unwrap() = 1;
            let seen = *s.lock().unwrap();
            assert_ne!(seen, 1, "an ordering bug of depth 3");
        })
    };
    let b = thread::spawn(move || {
        let read = *x.lock().unwrap();
        if read == 1 {
            *s.lock().unwrap() = 1;
        }
        *s.lock().unwrap() = 2;
    });
    let c = thread::spawn(move || {
        z.fetch_add(1, Relaxed);
        z.fetch_add(1, Relaxed);
    });
    a.join().unwrap();
    b.join().unwrap();
    c.join().unwrap();
}

/// (b) Prints (counts to verify):
/// `blaze-check priority schedules depth 3 seed 7: <i> schedules (4 threads, <k> steps, budget <r> runs), found at schedule <i>: thread 1 ... panicked: ... an ordering bug of depth 3`
/// `blaze-check priority schedules depth 1 seed 7: <r> schedules (4 threads, <k> steps, budget <r> runs), nothing found`
/// where `r = priority_runs(4, k, 3, 2)`, about `4 * k^2 * 4.61`.
#[test]
fn priority_schedules_find_a_depth_three_bug() {
    let builder = builder();
    let chance = Runs::Chance { nines: 2, cap: 10_000_000 };
    let deep = builder
        .search(Strategy::Priority { depth: 3, runs: chance, seed: 7 }, depth_three)
        .unwrap_or_else(|refused| panic!("{refused}"));
    println!("{deep}");
    let budget = deep.priority.expect("a priority search has a budget");
    assert_eq!(budget.runs, priority_runs(budget.threads, budget.steps, 3, 2));
    let found = deep.found.as_ref().unwrap_or_else(|| panic!("depth 3 missed it: {deep}"));
    assert!(found.what.contains("an ordering bug of depth 3"), "{}", found.what);
    let flat = builder
        .search(
            Strategy::Priority { depth: 1, runs: Runs::Fixed(budget.runs), seed: 7 },
            depth_three,
        )
        .unwrap_or_else(|refused| panic!("{refused}"));
    println!("{flat}");
    assert!(flat.found.is_none(), "depth 1 found it: {flat}");
    assert_eq!(flat.executions, budget.runs, "{flat}");
}

/// The run count for a chance is integer arithmetic: n * k^(d-1) * nines * ln 10, rounded up.
/// Prints nothing.
#[test]
fn priority_runs_follow_the_bound() {
    // 2 threads, depth 1: 2 * 2 * 2.302585093 = 9.21 -> 10.
    assert_eq!(priority_runs(2, 50, 1, 2), 10);
    // 4 threads, 10 steps, depth 3: 400 * 2 * 2.302585093 = 1842.07 -> 1843.
    assert_eq!(priority_runs(4, 10, 3, 2), 1843);
    assert_eq!(priority_runs(8, u32::MAX, 9, 9), u64::MAX);
}

/// A schedule line of another version or form is refused by name. Prints nothing.
#[test]
fn a_schedule_line_of_another_version_is_refused() {
    let builder = builder();
    let refused = builder.replay_line("blaze-check schedule v2 0011", || ()).unwrap_err();
    assert_eq!(refused, Refused::ScheduleVersion { found: "v2".to_owned() });
    let refused = builder.replay_line("CHECK_REPLAY=0011", || ()).unwrap_err();
    assert!(matches!(refused, Refused::NotAScheduleLine { .. }), "{refused}");
}

/// (d) The default search is unchanged: `Builder::check` and `Strategy::Dpor` run the same
/// number of executions of `rmws_never_lose_an_update`'s model (tests.rs), counted by the
/// closure's own runs. Prints:
/// `Builder::check: <n> executions; blaze-check bounded DPOR: <n> schedules, nothing found`
/// (n to verify, then to pin here as a literal).
#[test]
fn dpor_explores_what_check_explores() {
    fn rmws(runs: &StdRc<StdCell<u64>>) -> impl Fn() + 'static {
        let runs = StdRc::clone(runs);
        move || {
            runs.set(runs.get() + 1);
            let n = Arc::new(AtomicUsize::new(0));
            let others: Vec<_> = (0..2)
                .map(|_| {
                    let n = n.clone();
                    thread::spawn(move || {
                        n.fetch_add(1, Relaxed);
                    })
                })
                .collect();
            n.fetch_add(1, Relaxed);
            for other in others {
                other.join().unwrap();
            }
            assert_eq!(n.load(Relaxed), 3);
        }
    }
    let builder = builder();
    let by_check = StdRc::new(StdCell::new(0));
    builder.check(rmws(&by_check));
    let by_search = StdRc::new(StdCell::new(0));
    let searched =
        builder.search(Strategy::Dpor, rmws(&by_search)).unwrap_or_else(|refused| panic!("{refused}"));
    println!("Builder::check: {} executions; {searched}", by_check.get());
    assert!(searched.found.is_none(), "{searched}");
    assert_eq!(searched.executions, by_search.get());
    assert_eq!(by_check.get(), by_search.get(), "{searched}");
    // The pin: `RMWS_EXECUTIONS` is the count `Builder::check` ran when this file was written.
    // 0 until a run sets it: this assertion's message names the count to put here.
    assert_eq!(
        by_check.get(),
        RMWS_EXECUTIONS,
        "pin: Builder::check ran {} executions of the rmws model (search: {}); set \
         RMWS_EXECUTIONS to it",
        by_check.get(),
        by_search.get()
    );
}

/// The executions `Builder::check` runs of `rmws_never_lose_an_update`'s model at
/// `max_preemptions = Some(3)`: to be set from the first run's failure message.
const RMWS_EXECUTIONS: u64 = 26;
