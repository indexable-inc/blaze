//! The `Threads` layout under real threads (std.md, "`Threads`"): the same results as
//! `Inline`, `must_overlap` making progress beside its spawner, one panic failing one task,
//! every task run exactly once at every pool size, stealing near first, background work kept
//! behind foreground work, a stop that names a worker left running, and `PerWorker`. The
//! deque and the idle protocol have their own model and stress tests (task/deque.rs,
//! task/idle.rs).

use std::{
    num::NonZeroUsize,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use alloc::{alloc::Global, sync::Arc};

use blaze_core::wait::Deadline;
use blaze_mem::{Frame, Tag};

use super::{
    Class, NotAWorker, PerWorker, Scope, TaskError, TaskWhy,
    per_worker::FoldPlant,
    threads::{Plants, Pool, Topology},
};
use crate::{
    Cx,
    io::{Io, Limits, NativeWorld},
    mem::TASK_FRAME,
    par::{self, Grain},
    rt::pool::{self, Running},
    sync::{RecvError, chan},
};

const BUDGET: Duration = Duration::from_secs(60);

const NAMES: [&str; 8] = ["t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7"];

fn started(topology: &Topology) -> Running {
    pool::start(topology, Plants::NONE).expect("the pool starts")
}

fn flat(workers: u32) -> Running {
    started(&Topology::flat(workers))
}

/// A root context of `io` on `pool`.
fn root_on(io: &Io, pool: &Pool) -> Cx {
    let frame = Frame::with_reserve(TASK_FRAME, Tag::Frames).expect("a root frame");
    Cx::root_on(Deadline::after(io.now(), BUDGET), frame, io.clone(), pool.clone())
}

/// A native world over the system's temporary directory: its waits are real, so a consumer
/// waits for its producer on another thread.
fn native() -> Io {
    let root = std::env::temp_dir();
    let world = NativeWorld {
        root: &root,
        threads: 2,
        env: crate::env::Vars::given(Vec::new()),
        args: crate::args::ArgList::given(Vec::new()),
        host_name: Arc::from(""),
        seed: Some(7),
        above: 0,
        limits: Limits::USUAL,
    };
    Io::native(world).expect("a native world")
}

/// Waits (bounded) until every worker of `pool` has its thread up and is parked: a
/// `must_overlap` then finds one whose thread runs it at once. A worker whose thread has not
/// started is counted parked too (task/idle.rs), hence the arrivals.
fn all_parked(pool: &Pool) {
    let until = Instant::now() + Duration::from_secs(5);
    while pool.arrived() != pool.workers() || pool.parked() != pool.workers() {
        assert!(Instant::now() < until, "the pool's workers did not all park within 5 s");
        std::thread::yield_now();
    }
}

fn stopped(running: Running) {
    if let Err(hung) = running.stop(Duration::from_secs(5)) {
        panic!("the pool did not stop: {hung}");
    }
}

/// Tasks `at..n` of `scope`, each spawned before the next and joined after it (a `Task` is
/// joined where it is spawned), their outcomes pushed in reverse.
fn fan(
    scope: &Scope<'_>,
    cx: &Cx,
    at: usize,
    n: usize,
    work: &(impl Fn(usize) -> Result<u64, String> + Sync),
    out: &mut Vec<Result<u64, TaskError>>,
) {
    if at == n {
        return;
    }
    let task = scope.may_overlap(NAMES[at], TASK_FRAME, move |_cx| work(at));
    fan(scope, cx, at + 1, n, work, out);
    out.push(task.join(cx));
}

/// Eight tasks' outcomes and the scope's, under `cx`'s layout; task 5 panics and 6 fails.
fn eight(cx: &Cx) -> (Vec<Result<u64, TaskError>>, Result<(), TaskError>) {
    let work = |at: usize| -> Result<u64, String> {
        match at {
            5 => panic!("task 5 panics (planted by the threads test)"),
            6 => Err("task 6 fails".to_string()),
            at => Ok((at as u64) * (at as u64)),
        }
    };
    let scope = cx.scope();
    let mut out = Vec::new();
    fan(&scope, cx, 0, NAMES.len(), &work, &mut out);
    out.reverse();
    (out, scope.join(cx))
}

/// A float sum by `par::reduce`: not associative, so equal only if the join order is fixed.
fn float_sum(cx: &Cx) -> f64 {
    let xs: Vec<f64> = (0..10_000).map(|i| 1.0 / (1.0 + f64::from(i))).collect();
    let scope = cx.scope();
    let grain = Grain::new(NonZeroUsize::new(64).expect("64"), TASK_FRAME);
    let sum = par::reduce(cx, &scope, &xs[..], grain, || 0.0, |sum, xs: &[f64]| sum + xs.iter().sum::<f64>(), |a, b| a + b);
    let joined = scope.join(cx);
    joined.expect("the par scope");
    sum.expect("the reduce")
}

#[test]
fn threads_give_inline_s_results_at_one_two_and_eight_workers() {
    let io = Io::sim(11);
    let inline = io.cx(BUDGET, TASK_FRAME).expect("an inline root");
    let (want, want_scope) = eight(&inline);
    let want_sum = float_sum(&inline);
    for workers in [1, 2, 8] {
        let running = flat(workers);
        let cx = root_on(&io, running.pool());
        let (got, got_scope) = eight(&cx);
        assert_eq!(got, want, "{workers} workers: each task's outcome");
        assert_eq!(got_scope, want_scope, "{workers} workers: the scope's failure, the lowest index");
        assert_eq!(float_sum(&cx).to_bits(), want_sum.to_bits(), "{workers} workers: par's sum, bit for bit");
        drop(cx);
        stopped(running);
    }
}

#[test]
fn a_panic_in_one_task_of_eight_leaves_seven_results_and_one_typed_error() {
    let running = flat(4);
    let io = Io::sim(3);
    let cx = root_on(&io, running.pool());
    let work = |at: usize| -> Result<u64, String> {
        if at == 3 {
            panic!("task 3 panics (planted by the threads test)");
        }
        Ok(at as u64 + 100)
    };
    let scope = cx.scope();
    let mut out = Vec::new();
    fan(&scope, &cx, 0, NAMES.len(), &work, &mut out);
    out.reverse();
    let joined = scope.join(&cx);
    let made: Vec<u64> = out.iter().filter_map(|it| it.as_ref().ok().copied()).collect();
    assert_eq!(made, [100, 101, 102, 104, 105, 106, 107], "seven results");
    match &out[3] {
        Err(TaskError { task: "t3", why: TaskWhy::Panicked(said), .. }) => assert!(said.contains("task 3 panics"), "{said}"),
        other => panic!("task 3's outcome: {other:?}"),
    }
    assert!(matches!(joined, Err(TaskError { task: "t3", .. })), "the scope fails with task 3: {joined:?}");
    drop(cx);
    stopped(running);
}

#[test]
fn must_overlap_runs_a_producer_beside_its_consumer_on_two_workers() {
    let running = flat(2);
    let io = native();
    let cx = root_on(&io, running.pool()).until(Deadline::after(io.now(), Duration::from_secs(10)));
    all_parked(running.pool());
    let jobs = chan::bounded::<u64, _>("test.threads.jobs", NonZeroUsize::new(4).expect("4"), Global);
    let scope = cx.scope();
    let producer = scope.must_overlap("produce", TASK_FRAME, |cx| -> Result<(), String> {
        for value in 0..1000u64 {
            jobs.send(cx, value).map_err(|error| format!("send {value}: {error}"))?;
        }
        jobs.close();
        Ok(())
    });
    let producer = match producer {
        Ok(task) => task,
        Err(refused) => panic!("two parked workers, and must_overlap refused: {refused}"),
    };
    let mut sum = 0;
    let consumed = loop {
        match jobs.recv(&cx) {
            Ok(value) => sum += value,
            Err(RecvError::Closed { .. }) => break Ok(sum),
            Err(other) => break Err(other),
        }
    };
    let produced = producer.join(&cx);
    let joined = scope.join(&cx);
    assert_eq!(consumed, Ok((0..1000).sum::<u64>()), "every value through a channel of four");
    assert_eq!(produced, Ok(()));
    assert_eq!(joined, Ok(()));
    drop(cx);
    stopped(running);
}

#[test]
fn every_task_runs_exactly_once_at_one_two_four_and_eight_workers() {
    const TASKS: usize = 100_000;
    let io = Io::sim(5);
    for workers in [1, 2, 4, 8] {
        let running = flat(workers);
        let cx = root_on(&io, running.pool());
        let ran: Vec<AtomicU32> = (0..TASKS).map(|_| AtomicU32::new(0)).collect();
        let began = Instant::now();
        let scope = cx.scope();
        let grain = Grain::new(NonZeroUsize::new(16).expect("16"), 64 << 10);
        let done = par::for_each(&cx, &scope, &ran[..], grain, |chunk: &[AtomicU32]| {
            for it in chunk {
                it.fetch_add(1, Ordering::Relaxed);
            }
        });
        let joined = scope.join(&cx);
        let took = began.elapsed();
        done.expect("the for_each");
        joined.expect("its scope");
        assert!(ran.iter().all(|it| it.load(Ordering::Relaxed) == 1), "{workers} workers: each item exactly once");
        // A timing, printed, not asserted (a shared machine's timing proves nothing).
        eprintln!("threads scaling: {TASKS} items in chunks of 16 on {workers} worker(s): {took:?}");
        drop(cx);
        stopped(running);
    }
}

#[test]
fn a_thief_tries_its_own_group_first() {
    let topology = Topology::groups(&[(Class::Foreground, 2), (Class::Foreground, 2)]);
    let running = started(&topology);
    let pool = running.pool();
    assert_eq!(&pool.victims(0)[..], [1, 3, 2], "worker 0: its group-mate, then the other group");
    assert_eq!(&pool.victims(3)[..], [2, 0, 1], "worker 3: its group-mate, then the other group");
    // The order a thief's `find` walks is checked on `find` itself, with no threads and a
    // planted control (threads/find_tests.rs, `a_thief_steals_from_its_own_group_before_another`).
    // Here, under real threads, one task fans out slow tasks onto its worker's deque and the
    // rest steal: every item runs once, and the steals are counted and printed, not asserted.
    // How many there are is the machine's scheduler's to decide (this thread's join steals from
    // that deque too, and a loaded machine may run no woken worker for the fan's length), and a
    // count of near steals cannot tell a thief that tries the other group first, which still
    // steals near while the other group's deques are empty.
    // Every worker up and parked, and the fan reserved to one of them (`must_overlap`): a
    // `may_overlap` fan would mostly be run by this thread's own join before a worker woke
    // (task/help.rs `finish`), and its items, spawned from outside the pool, would go to the
    // injection queues, from which nothing is stolen.
    all_parked(pool);
    let io = Io::sim(9);
    let cx = root_on(&io, pool);
    let scope = cx.scope();
    let ran: Vec<AtomicU32> = (0..256).map(|_| AtomicU32::new(0)).collect();
    let outer = scope.must_overlap("fan", TASK_FRAME, |cx| -> Result<(), String> {
        let inner = cx.scope();
        let grain = Grain::new(NonZeroUsize::new(1).expect("1"), 64 << 10);
        let done = par::for_each(cx, &inner, &ran[..], grain, |chunk: &[AtomicU32]| {
            for it in chunk {
                it.fetch_add(1, Ordering::Relaxed);
            }
            let until = Instant::now() + Duration::from_micros(50);
            while Instant::now() < until {
                core::hint::spin_loop();
            }
        });
        inner.join(cx).map_err(|error| error.to_string())?;
        done.map_err(|error| error.to_string())
    });
    let outer = match outer {
        Ok(task) => task,
        Err(refused) => panic!("four parked workers, and must_overlap refused: {refused}"),
    };
    let fanned = outer.join(&cx);
    let joined = scope.join(&cx);
    fanned.expect("the fan");
    joined.expect("its scope");
    let (mut near, mut far) = (0, 0);
    for at in 0..pool.workers() {
        let (n, f) = pool.steals(at);
        near += n;
        far += f;
    }
    eprintln!("threads steals: {near} from the thief's own group, {far} from another");
    assert!(ran.iter().all(|it| it.load(Ordering::Relaxed) == 1), "each fanned item ran exactly once");
    drop(cx);
    stopped(running);
}

#[test]
fn a_stop_names_the_worker_still_running_a_task_that_ignores_cancellation() {
    let running = flat(2);
    let pool = running.pool().clone();
    let io = Io::sim(17);
    let cx = root_on(&io, &pool);
    let release = AtomicBool::new(false);
    let began = AtomicBool::new(false);
    let scope = cx.scope();
    let stubborn = scope.may_overlap("stubborn", TASK_FRAME, |_cx| -> Result<(), String> {
        began.store(true, Ordering::SeqCst);
        // It looks at no cancellation and no deadline: only the test's flag ends it.
        let until = Instant::now() + Duration::from_secs(20);
        while !release.load(Ordering::Acquire) && Instant::now() < until {
            core::hint::spin_loop();
        }
        Ok(())
    });
    let until = Instant::now() + Duration::from_secs(5);
    while !began.load(Ordering::SeqCst) {
        assert!(Instant::now() < until, "the stubborn task did not start within 5 s");
        std::thread::yield_now();
    }
    let hung = running.stop(Duration::from_millis(100)).expect_err("a worker runs a task that ignores its stop");
    release.store(true, Ordering::Release);
    let ended = stubborn.join(&cx);
    let joined = scope.join(&cx);
    assert_eq!(hung.stuck.len(), 1, "one worker left: {hung}");
    assert_eq!(hung.stuck[0].task, "stubborn", "named by its task: {hung}");
    assert!(hung.to_string().contains("still running task stubborn"), "{hung}");
    assert_eq!(ended, Ok(()));
    assert_eq!(joined, Ok(()));
}

#[test]
fn per_worker_slots_sum_exactly_after_must_overlap_writers() {
    const ADDS: u64 = 10_000;
    let running = flat(4);
    let io = native();
    let cx = root_on(&io, running.pool());
    all_parked(running.pool());
    let mut slots = PerWorker::new_in(cx.workers(), Global, |_| 0u64);
    assert_eq!(slots.with(&cx, |slot| *slot += 1), Err(NotAWorker::Outside { span: "main" }), "not on a worker");
    {
        let slots = &slots;
        let add = |cx: &Cx| -> Result<(), String> {
            for _ in 0..ADDS {
                slots.with(cx, |slot| *slot += 1).map_err(|error| error.to_string())?;
            }
            Ok(())
        };
        let scope = cx.scope();
        let a = scope.must_overlap("add a", TASK_FRAME, add).expect("a parked worker for a");
        let b = scope.must_overlap("add b", TASK_FRAME, add).expect("a parked worker for b");
        let c = scope.must_overlap("add c", TASK_FRAME, add).expect("a parked worker for c");
        let ended = [a.join(&cx), b.join(&cx), c.join(&cx)];
        let joined = scope.join(&cx);
        assert!(ended.iter().all(Result::is_ok), "the writers: {ended:?}");
        joined.expect("their scope");
    }
    let all = slots.take_all();
    assert_eq!(all.iter().sum::<u64>(), 3 * ADDS, "every add, in some worker's slot: {:?}", &all[..]);
    assert!(slots.iter().all(|slot| *slot == 0), "take_all leaves each slot its default");
    drop(cx);
    stopped(running);
}

#[test]
fn a_second_entry_into_a_slot_on_one_worker_stops() {
    let running = flat(2);
    let io = Io::sim(19);
    let cx = root_on(&io, running.pool());
    let slots = PerWorker::new_in(cx.workers(), Global, |_| 0u32);
    let scope = cx.scope();
    let began = AtomicBool::new(false);
    let task = scope.may_overlap("reenters", TASK_FRAME, |cx| -> Result<(), String> {
        began.store(true, Ordering::SeqCst);
        // The closure captures the task's `Cx` (a closure that can wait could reach another
        // task of this worker the same way): the inner `with` is the second entry.
        slots.with(cx, |_| slots.with(cx, |slot| *slot += 1)).map_err(|error| error.to_string())?.map_err(|error| error.to_string())
    });
    // A join runs a task no worker has taken on the joining thread, which is no worker: wait
    // (bounded) for a worker to take it, as the subject is a worker's second entry.
    let until = Instant::now() + Duration::from_secs(5);
    while !began.load(Ordering::SeqCst) {
        assert!(Instant::now() < until, "no worker took the task within 5 s");
        std::thread::yield_now();
    }
    let ended = task.join(&cx);
    let _ = scope.join(&cx);
    match ended {
        Err(TaskError { why: TaskWhy::Panicked(said), .. }) => assert!(said.contains("entered twice"), "{said}"),
        other => panic!("a second entry: {other:?}"),
    }
    drop(cx);
    stopped(running);
}

/// Writers keep each slot's two halves equal except mid-write; `folds` folds look for a slot
/// whose halves differ. The number of torn slots seen.
fn torn_folds(plant: FoldPlant, folds: u32) -> u64 {
    let running = flat(4);
    let io = Io::sim(23);
    let cx = root_on(&io, running.pool());
    all_parked(running.pool());
    let slots = PerWorker::new_racy_in(cx.workers(), Global, |_| (AtomicU64::new(0), AtomicU64::new(0))).planted(plant);
    let stop = AtomicBool::new(false);
    let torn = AtomicU64::new(0);
    let next = AtomicU64::new(0);
    // One write: both halves of this worker's slot, a long spin between them. Each is its own
    // task, so a worker reaches a task boundary (where a fold's visit runs) between writes.
    let write = |cx: &Cx| -> Result<(), String> {
        let v = next.fetch_add(1, Ordering::Relaxed) + 1;
        let wrote = slots.with(cx, |(a, b)| {
            a.store(v, Ordering::Relaxed);
            for _ in 0..2000 {
                core::hint::spin_loop();
            }
            b.store(v, Ordering::Relaxed);
        });
        match wrote {
            // A write the root's join ran itself, off every worker: it has no slot to write.
            Ok(()) | Err(NotAWorker::Outside { .. }) => Ok(()),
            Err(other) => Err(other.to_string()),
        }
    };
    let scope = cx.scope();
    let folder = scope.must_overlap("folder", TASK_FRAME, |cx| -> Result<(), String> {
        let mut folded = Ok(());
        for _ in 0..folds {
            match slots.fold(cx, 0u64, |(a, b)| u64::from(a.load(Ordering::Relaxed) != b.load(Ordering::Relaxed)), |sum, it| sum + it) {
                Ok(seen) => {
                    torn.fetch_add(seen, Ordering::Relaxed);
                }
                Err(error) => {
                    folded = Err(error.to_string());
                    break;
                }
            }
        }
        // On every path: the root's writing loop ends on it.
        stop.store(true, Ordering::Release);
        folded
    });
    let folder = folder.expect("a parked worker for the folder");
    // The root keeps the other workers writing, eight tasks at a time, until the folds end.
    let until = Instant::now() + Duration::from_secs(30);
    let mut wrote = Ok(());
    while !stop.load(Ordering::Acquire) && wrote.is_ok() {
        assert!(Instant::now() < until, "the folds did not end within 30 s");
        let writers = cx.scope();
        let batch = [(); 8].map(|()| writers.may_overlap("write", 64 << 10, write));
        for task in batch {
            wrote = wrote.and(task.join(&cx).map_err(|error| error.to_string()));
        }
        wrote = wrote.and(writers.join(&cx).map_err(|error| error.to_string()));
    }
    let folded = folder.join(&cx);
    let joined = scope.join(&cx);
    folded.expect("the folds");
    wrote.expect("the writes");
    joined.expect("their scope");
    drop(cx);
    stopped(running);
    torn.load(Ordering::Relaxed)
}

#[test]
fn a_fold_never_sees_a_half_written_slot() {
    assert_eq!(torn_folds(FoldPlant::None, 500), 0, "each slot read on its own worker, between its tasks");
}

#[test]
fn the_fold_control_reading_slots_directly_sees_one_torn() {
    let torn = torn_folds(FoldPlant::ReadsDirectly, 20_000);
    assert!(torn > 0, "the planted fold read every slot from its own thread and never saw a write half done");
}

/// How many times [`counted`] started each worker's thread: only the lazy-start test's pools
/// use it.
static STARTS: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

/// The driver's starter, counted by worker.
fn counted(pool: &Pool, at: u32) {
    STARTS[at as usize].fetch_add(1, Ordering::SeqCst);
    pool::starter(pool, at);
}

fn four() -> Topology {
    Topology::flat(4)
}

/// The lazy start (task/threads.rs, `Pool::rouse`): two threads make the first spawns on a
/// pool nothing of which is made, released together by a barrier, in each of 200 rounds. No
/// worker's thread starts twice, and neither spawn is lost: each spawner waits (bounded) for a
/// worker to run its task before it joins it, since a join runs an unstarted task itself and
/// would hide a lost one. Then the stop waits for the last started worker's leave.
#[test]
fn two_first_spawns_at_once_start_no_worker_twice_and_neither_is_lost() {
    let io = native();
    for round in 0..200 {
        let before: [u32; 4] = core::array::from_fn(|at| STARTS[at].load(Ordering::SeqCst));
        let running = pool::lazy_on(four, counted, Plants::NONE);
        let gate = std::sync::Barrier::new(2);
        std::thread::scope(|threads| {
            for side in 0..2 {
                let (io, pool, gate) = (&io, running.pool(), &gate);
                threads.spawn(move || {
                    let cx = root_on(io, pool);
                    let ran = AtomicBool::new(false);
                    let ran = &ran;
                    let scope = cx.scope();
                    gate.wait();
                    let task = scope.may_overlap(NAMES[side], TASK_FRAME, move |_cx| {
                        ran.store(true, Ordering::SeqCst);
                        Ok::<u64, String>(0)
                    });
                    let until = Instant::now() + Duration::from_secs(5);
                    while !ran.load(Ordering::SeqCst) {
                        assert!(Instant::now() < until, "round {round}: first spawn {side} was lost: no worker ran it within 5 s");
                        std::thread::yield_now();
                    }
                    task.join(&cx).expect("the task");
                    scope.join(&cx).expect("its scope");
                });
            }
        });
        let starts: [u32; 4] = core::array::from_fn(|at| STARTS[at].load(Ordering::SeqCst) - before[at]);
        assert!(starts.iter().all(|&n| n <= 1), "round {round}: a worker's thread started more than once: {starts:?}");
        assert!(starts.iter().sum::<u32>() >= 1, "round {round}: no thread started, yet both tasks ran on workers");
        stopped(running);
    }
}

/// How many threads [`counted_eight`] started: only the start-as-asked test's pool uses it.
static EIGHT_STARTS: AtomicU32 = AtomicU32::new(0);

fn counted_eight(pool: &Pool, at: u32) {
    EIGHT_STARTS.fetch_add(1, Ordering::SeqCst);
    pool::starter(pool, at);
}

fn eight_flat() -> Topology {
    Topology::flat(8)
}

/// A worker's thread starts when a wake first reaches it, and a wake tries started workers
/// first (task/threads.rs, `Pool::rouse`, `Pool::wake_order`; bottom.md section 9: workers
/// start in proportion to queued work). A pool of eight given one task starts at most two
/// threads (the one that finds the task wakes one more, which finds nothing); given three more
/// that must all run at once (each waits, bounded, until all three have begun), at most four
/// in all. Each phase waits for its tasks to be running on workers before it joins, so the
/// joiner runs none inline. Control: with every worker's thread started at the first wake (the
/// form before), it fails at the one task with 8 of 8 started.
#[test]
fn workers_start_as_tasks_ask_not_one_a_core() {
    let io = native();
    let running = pool::lazy_on(eight_flat, counted_eight, Plants::NONE);
    let pool = running.pool();
    let cx = root_on(&io, pool);
    // Every started worker back asleep (bounded), then the count of threads started.
    let settled = |what: &str| {
        let until = Instant::now() + Duration::from_secs(5);
        while pool.arrived() != pool.started() || pool.parked() != pool.workers() {
            assert!(Instant::now() < until, "{what}: the started workers did not all park within 5 s");
            std::thread::yield_now();
        }
        assert_eq!(pool.started(), EIGHT_STARTS.load(Ordering::SeqCst), "{what}: the pool's count is the starter's");
        pool.started()
    };
    let wait_for = |count: &AtomicU32, n: u32, what: &str| {
        let until = Instant::now() + Duration::from_secs(5);
        while count.load(Ordering::SeqCst) < n {
            assert!(Instant::now() < until, "{what}: {} of {n} tasks began on workers within 5 s", count.load(Ordering::SeqCst));
            std::thread::yield_now();
        }
    };

    let began = AtomicU32::new(0);
    let scope = cx.scope();
    let one = scope.may_overlap("one", TASK_FRAME, |_cx| {
        began.fetch_add(1, Ordering::SeqCst);
        Ok::<u64, String>(1)
    });
    wait_for(&began, 1, "one task");
    one.join(&cx).expect("the one task");
    scope.join(&cx).expect("its scope");
    let after_one = settled("one task");
    assert!((1..=2).contains(&after_one), "one task: {after_one} of 8 workers' threads started, not 1 or 2");

    let began = AtomicU32::new(0);
    let began = &began;
    let scope = cx.scope();
    let tasks: Vec<_> = (0..3)
        .map(|at| {
            scope.may_overlap(NAMES[at], TASK_FRAME, move |_cx| {
                began.fetch_add(1, Ordering::SeqCst);
                let until = Instant::now() + Duration::from_secs(5);
                while began.load(Ordering::SeqCst) < 3 {
                    if Instant::now() >= until {
                        return Err(format!("task {at}: the three did not all begin within 5 s"));
                    }
                    std::hint::spin_loop();
                }
                Ok::<u64, String>(at as u64)
            })
        })
        .collect();
    wait_for(began, 3, "three tasks at once");
    for task in tasks {
        task.join(&cx).expect("each of the three");
    }
    scope.join(&cx).expect("their scope");
    let after_three = settled("three tasks at once");
    assert!((3..=4).contains(&after_three), "three tasks at once: {after_three} of 8 workers' threads started, not 3 or 4");
    stopped(running);
}

/// The threads of this process, as the platform counts them.
#[cfg(target_os = "macos")]
#[expect(unsafe_code, reason = "`proc_pidinfo`, the platform's count of a process's threads")]
fn threads_now() -> u32 {
    // Rule 45: libproc's `proc_pidinfo(pid, PROC_PIDTASKINFO = 4, 0, ...)` fills a
    // `struct proc_taskinfo` (<sys/proc_info.h>): six `uint64_t`, then twelve `int32_t` of
    // which the tenth is `pti_threadnum`, the task's thread count.
    #[repr(C)]
    struct TaskInfo {
        sizes_and_times: [u64; 6],
        counts: [i32; 12],
    }
    unsafe extern "C" {
        fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut TaskInfo, size: i32) -> i32;
    }
    let mut info = TaskInfo { sizes_and_times: [0; 6], counts: [0; 12] };
    let size = size_of::<TaskInfo>() as i32;
    let pid = std::process::id() as i32;
    // SAFETY: `info` is a `proc_taskinfo` the call writes at most `size` bytes of.
    let wrote = unsafe { proc_pidinfo(pid, 4, 0, &raw mut info, size) };
    assert_eq!(wrote, size, "proc_pidinfo(PROC_PIDTASKINFO) of this process");
    info.counts[9] as u32
}

/// The threads of this process, as the platform counts them: the entries of `/proc/self/task`.
#[cfg(target_os = "linux")]
fn threads_now() -> u32 {
    std::fs::read_dir("/proc/self/task").expect("/proc/self/task").count() as u32
}

/// Set in the child that [`a_program_that_spawns_nothing_starts_no_thread`] runs.
const NO_THREAD_CHILD: &str = "BSTD_TEST_NO_THREAD_CHILD";

/// A program of the blaze edition that spawns nothing starts no thread: its world is made
/// (the native driver, which starts its file threads at the first file effect), its `main`
/// makes and joins a scope (which reads the topology and makes the pool's tables), and the
/// platform's count of threads is the same at the end of `main` as before the world. The
/// entry exits the process, so the program runs in a child: this test binary again, running
/// only this test on one test thread, its status saying the counts matched.
#[test]
fn a_program_that_spawns_nothing_starts_no_thread() {
    if std::env::var_os(NO_THREAD_CHILD).is_some() {
        let before = threads_now();
        let root = std::env::temp_dir();
        let world = crate::rt::open_native(blaze_core::testing::os(), crate::args::ArgList::given(Vec::new()), crate::env::Vars::given(Vec::new()), &root, 0);
        crate::rt::run_io(world, |cx, _io| {
            let scope = cx.scope();
            scope.join(cx).expect("an empty scope");
            let after = threads_now();
            std::eprintln!("threads before the world {before}, at the end of main {after}");
            if after == before { crate::entry::Status::OK } else { crate::entry::Status::FAILED }
        });
        unreachable!("the entry exits the process");
    }
    let name = "task::tests::a_program_that_spawns_nothing_starts_no_thread";
    let mut child = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args([name, "--exact", "--test-threads=1", "--nocapture"])
        .env(NO_THREAD_CHILD, "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the child");
    let until = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child's status") {
            break status;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            panic!("the child did not exit within 30 s");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut said = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().expect("its standard error"), &mut said).expect("its words");
    assert!(status.success(), "the program that spawns nothing started a thread: {said}");
}

/// One batch of 64 tasks on `workers` workers, joined with `Batch::join_each`: task `key` makes
/// `key * key + 1`. The bytes are the results merged by key, or, for the control, in the order
/// the join handed them over. Also the scope's join counts.
fn batch_bytes(workers: u32, reversed: bool, by_arrival: bool) -> (Vec<u8>, super::JoinCounts) {
    const KEYS: u32 = 64;
    let io = Io::sim(5);
    let running = flat(workers);
    let cx = root_on(&io, running.pool());
    let scope = cx.scope();
    let mut batch = scope.batch::<(u32, u64), Global>(KEYS, Global).expect("room for the batch's ring");
    for at in 0..KEYS {
        let key = if reversed { KEYS - 1 - at } else { at };
        let index = batch.may_overlap("square", TASK_FRAME, move |_cx| Ok::<_, String>((key, u64::from(key) * u64::from(key) + 1)));
        assert_eq!(index.expect("room for a handle"), at, "an index is the spawn's place in its batch");
    }
    let mut seen = vec![0u32; KEYS as usize];
    let mut merged = vec![0u64; KEYS as usize];
    let mut arrived = Vec::new();
    batch.join_each(&cx, |joined| {
        seen[joined.index as usize] += 1;
        let (key, made) = joined.result.expect("no task fails");
        merged[key as usize] = made;
        arrived.push(made);
    });
    let counts = scope.join_counts();
    scope.join(&cx).expect("its scope");
    assert!(seen.iter().all(|it| *it == 1), "{workers} workers: every index delivered exactly once: {seen:?}");
    drop(cx);
    stopped(running);
    let order = if by_arrival { arrived } else { merged };
    (order.iter().flat_map(|it| it.to_le_bytes()).collect(), counts)
}

/// A batch join's output merged by key is the same bytes on one worker, on eight, and with the
/// spawns reversed (rule 25; parallel.md, "End state", point 8). The control merges in the
/// order results arrive: on one worker every task ended at its spawn, the join's sweep hands
/// them over as 0, 63, 62, .., and the bytes differ. Prints
/// `batch join: 8 workers: tasks ended 64: by workers .., by their joiner 0, by a helping join ..; parks .. (.. ns), depth-capped 0`
/// and `test task::tests::a_batch_join_merged_by_key_gives_equal_bytes_in_any_schedule ... ok`.
#[test]
fn a_batch_join_merged_by_key_gives_equal_bytes_in_any_schedule() {
    let (one, _) = batch_bytes(1, false, false);
    let (eight, counts) = batch_bytes(8, false, false);
    let (reversed, _) = batch_bytes(8, true, false);
    eprintln!("batch join: 8 workers: {counts}");
    assert_eq!(one, eight, "one worker and eight gave different bytes");
    assert_eq!(one, reversed, "reversed spawns gave different bytes");
    assert_eq!(counts.ended, 64, "every queued task is counted ended");
    assert_eq!(counts.by_workers() + counts.by_helper + counts.by_joiner, 64, "every task ran on a worker or on the joining thread");
    let (arrival, _) = batch_bytes(1, false, true);
    assert_ne!(one, arrival, "the control, merged in arrival order, was not caught");
}

/// The racy form's snapshot reads every slot from any thread, outside the pool too.
#[test]
fn a_racy_snapshot_reads_every_slot() {
    let slots: PerWorker<AtomicUsize, Global, super::Shared> = PerWorker::new_racy_in(3, Global, |_| AtomicUsize::new(1));
    let mut sum = 0;
    slots.snapshot_racy(|_, it| sum += it.load(Ordering::Relaxed));
    assert_eq!((slots.len(), sum), (3, 3));
}
