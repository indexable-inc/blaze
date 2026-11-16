//! The `Threads` layout under real threads (std.md, "`Threads`"): the same results as
//! `Inline`, `must_overlap` making progress beside its spawner, one panic failing one task,
//! every task run exactly once at every pool size, stealing near first, background work kept
//! behind foreground work, a stop that names a worker left running, and `PerWorker`. The
//! deque and the idle protocol have their own model and stress tests (task/deque.rs,
//! task/idle.rs).

use std::{
    num::{NonZeroU32, NonZeroUsize},
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
        env: Vec::new(),
        args: Vec::new(),
        host_name: Arc::from(""),
        seed: 7,
        limits: Limits::USUAL,
    };
    Io::native(world).expect("a native world")
}

/// Waits (bounded) until every worker of `pool` is parked: a `must_overlap` then finds one.
fn all_parked(pool: &Pool) {
    let until = Instant::now() + Duration::from_secs(5);
    while pool.parked() != pool.workers() {
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
    // Counted: one task fans out slow tasks onto its worker's deque; the rest steal.
    let io = Io::sim(9);
    let cx = root_on(&io, pool);
    let scope = cx.scope();
    let outer = scope.may_overlap("fan", TASK_FRAME, |cx| -> Result<(), String> {
        let xs: Vec<u32> = (0..256).collect();
        let inner = cx.scope();
        let grain = Grain::new(NonZeroUsize::new(1).expect("1"), 64 << 10);
        let done = par::for_each(cx, &inner, &xs[..], grain, |_x: &[u32]| {
            let until = Instant::now() + Duration::from_micros(50);
            while Instant::now() < until {
                core::hint::spin_loop();
            }
        });
        inner.join(cx).map_err(|error| error.to_string())?;
        done.map_err(|error| error.to_string())
    });
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
    assert!(near > 0, "a thief stole from its own group at least once ({near} near, {far} far)");
    drop(cx);
    stopped(running);
}

#[test]
fn a_background_task_never_starts_on_a_foreground_worker_while_foreground_work_is_queued() {
    let running = flat(2);
    let io = Io::sim(13);
    let cx = root_on(&io, running.pool());
    let gate = AtomicBool::new(false);
    let busy = AtomicU32::new(0);
    let order = AtomicU32::new(0);
    let block = |_cx: &Cx| -> Result<(), String> {
        busy.fetch_add(1, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(10);
        while !gate.load(Ordering::Acquire) {
            if Instant::now() > until {
                return Err("the gate never opened".to_string());
            }
            core::hint::spin_loop();
        }
        Ok(())
    };
    let note = |_cx: &Cx| -> Result<u32, String> { Ok(order.fetch_add(1, Ordering::SeqCst)) };
    let blockers = cx.scope();
    let a = blockers.may_overlap("block a", TASK_FRAME, block);
    let b = blockers.may_overlap("block b", TASK_FRAME, block);
    let until = Instant::now() + Duration::from_secs(5);
    while busy.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < until, "both workers did not take a blocker within 5 s");
        std::thread::yield_now();
    }
    let background = cx.scope();
    background.narrow_workers(NonZeroU32::MAX, Class::Background);
    let late = background.may_overlap("background", TASK_FRAME, note);
    let foreground = cx.scope();
    let f0 = foreground.may_overlap("f0", TASK_FRAME, note);
    let f1 = foreground.may_overlap("f1", TASK_FRAME, note);
    let f2 = foreground.may_overlap("f2", TASK_FRAME, note);
    gate.store(true, Ordering::Release);
    // Nothing is joined until the workers have run all four: a join runs an unstarted task
    // itself, which would take it off the queues the order is about.
    let until = Instant::now() + Duration::from_secs(10);
    while order.load(Ordering::SeqCst) < 4 {
        assert!(Instant::now() < until, "the workers did not run the four tasks within 10 s");
        std::thread::yield_now();
    }
    let fs = [f0.join(&cx), f1.join(&cx), f2.join(&cx)];
    let bg = late.join(&cx);
    let (a, b) = (a.join(&cx), b.join(&cx));
    let joined = [foreground.join(&cx), background.join(&cx), blockers.join(&cx)];
    assert_eq!(a, Ok(()));
    assert_eq!(b, Ok(()));
    assert!(joined.iter().all(Result::is_ok), "the scopes: {joined:?}");
    let fs: Vec<u32> = fs.into_iter().map(|it| it.expect("a foreground task")).collect();
    let bg = bg.expect("the background task");
    assert!(fs.iter().all(|&f| f < bg), "background started at {bg}, foreground at {fs:?}");
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
    let task = scope.may_overlap("reenters", TASK_FRAME, |cx| -> Result<(), String> {
        // The closure captures the task's `Cx` (a closure that can wait could reach another
        // task of this worker the same way): the inner `with` is the second entry.
        slots.with(cx, |_| slots.with(cx, |slot| *slot += 1)).map_err(|error| error.to_string())?.map_err(|error| error.to_string())
    });
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

/// The racy form's snapshot reads every slot from any thread, outside the pool too.
#[test]
fn a_racy_snapshot_reads_every_slot() {
    let slots: PerWorker<AtomicUsize, Global, super::Shared> = PerWorker::new_racy_in(3, Global, |_| AtomicUsize::new(1));
    let mut sum = 0;
    slots.snapshot_racy(|_, it| sum += it.load(Ordering::Relaxed));
    assert_eq!((slots.len(), sum), (3, 3));
}
