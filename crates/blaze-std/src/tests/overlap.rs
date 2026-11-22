//! May overlap against must overlap (crate::task, "May overlap, or must"):
//!
//! - A producer and a consumer of one bounded channel are correct only if both run at once.
//!   Under the one-thread executor (the simulation's `Inline` layout, and a scope narrowed to
//!   one worker) `must_overlap` refuses the producer with a typed `NoParallelism` before
//!   anything waits: the virtual clock has not moved. The same producer started with
//!   `may_overlap` runs inline, fills the channel and stops at its deadline on the virtual
//!   clock, its next value given back: the deadlock `must_overlap` names up front.
//! - `may_overlap` gives the sequential result in a scope narrowed to one worker and in one
//!   with the layout's workers.
//!
//! The pair passing under two workers of the `Threads` layout is in task/tests.rs: the
//! simulation runs `Inline` only.

use std::{alloc::Global, num::{NonZeroU32, NonZeroUsize}, time::Duration};

use blaze_sim::{Config, Tape};

use super::{root, world};
use crate::{
    Cx, Plant, Stopped,
    clock::Clock,
    mem::TASK_FRAME,
    sync::{SendError, chan},
    task::{Class, LAYOUT, Layout, NoParallelism, Scope},
};

#[derive(Debug)]
struct Plan {
    seed: u64,
    capacity: usize,
    /// How many values the producer sends: more than the capacity, so a consumer is needed.
    values: u32,
    /// Whether the scope is narrowed to one worker first.
    narrowed: bool,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let capacity = tape.int_in(1..=4u64, "capacity") as usize;
    let values = (capacity as u64 + tape.int_in(1..=8u64, "past the capacity")) as u32;
    let narrowed = tape.coin("narrowed");
    Plan { seed, capacity, values, narrowed }
}

fn run(plan: &Plan) -> Result<(), String> {
    let io = world(plan.seed, Plant::None);
    let cx = root(&io);
    let clock = io.need_clock().map_err(|e| format!("harness: {e}"))?;
    let capacity = NonZeroUsize::new(plan.capacity).expect("a plan's capacity is at least one");
    let jobs = chan::bounded::<u32, _>("test.jobs", capacity, Global);
    let scope = cx.scope();
    if plan.narrowed {
        scope.narrow_workers(NonZeroU32::MIN, Class::Foreground);
    }
    let found = pair(plan, &cx, &scope, &jobs, &clock);
    let joined = scope.join(&cx);
    found?;
    joined.map_err(|error| format!("the scope: {error}"))
}

fn pair(plan: &Plan, cx: &Cx, scope: &Scope<'_>, jobs: &chan::Chan<u32, Global>, clock: &Clock) -> Result<(), String> {
    let produce = |cx: &Cx| -> Result<u32, String> {
        for value in 0..plan.values {
            match jobs.send(cx, value) {
                Ok(()) => {}
                Err(SendError::Stopped { value: back, stopped: Stopped::Deadline { .. } }) if back == value => return Ok(value),
                Err(other) => return Err(format!("send {value}: {other}")),
            }
        }
        Ok(plan.values)
    };
    // Must overlap: refused at once, the producer never run, the virtual clock unmoved.
    let before = cx.io().now();
    let mut ran = false;
    let refused = scope.must_overlap("produce", TASK_FRAME, |cx| {
        ran = true;
        produce(cx)
    });
    let want = NoParallelism { scope: "main", task: "produce", needed: 2, have: 1, layout: Layout::Inline };
    match refused {
        Err(got) if got == want => {}
        Err(got) => return Err(format!("must_overlap refused with {got:?}, not {want:?}")),
        Ok(task) => {
            let _ = task.join(cx);
            return Err(format!("must_overlap ran with {} worker(s)", scope.workers()));
        }
    }
    if ran || cx.io().now() != before || !jobs.is_empty() {
        return Err(format!("the refused producer ran ({ran}), time moved or a value went in ({})", jobs.len()));
    }
    // May overlap: inline, so it fills the channel and its next send stops at its deadline.
    let task = scope.may_overlap("produce inline", TASK_FRAME, |cx| {
        produce(&cx.until(clock.after(Duration::from_millis(50))))
    });
    let stopped_at = task.join(cx).map_err(|error| format!("the inline producer: {error}"))?;
    if stopped_at != plan.capacity as u32 || jobs.len() != plan.capacity {
        return Err(format!(
            "the inline producer stopped at value {stopped_at} with {} held; the capacity is {}",
            jobs.len(),
            plan.capacity
        ));
    }
    if cx.io().now() < before + Duration::from_millis(50) {
        return Err("the inline producer's send stopped before its deadline".to_owned());
    }
    Ok(())
}

#[test]
fn must_overlap_refuses_a_producer_and_consumer_under_one_worker() {
    assert_eq!(LAYOUT.workers(), 1);
    blaze_sim::check(Config::cases(100), |tape| {
        let plan = plan(tape);
        if let Err(why) = run(&plan) {
            panic!("{why}\nreplay: plan {plan:?}");
        }
    });
}

/// Each task's result, in spawn order, from a scope narrowed to `workers` (or not narrowed).
fn squares(seed: u64, n: u32, workers: Option<NonZeroU32>) -> Result<Vec<u64>, String> {
    let io = world(seed, Plant::None);
    let cx = root(&io);
    let scope = cx.scope();
    if let Some(workers) = workers {
        scope.narrow_workers(workers, Class::Foreground);
    }
    let mut out = Vec::new();
    let mut failed = None;
    for at in 0..n {
        let task = scope.may_overlap("square", TASK_FRAME, |cx| -> Result<u64, String> {
            cx.check().map_err(|stopped| stopped.to_string())?;
            Ok(u64::from(at) * u64::from(at))
        });
        match task.join(&cx) {
            Ok(square) => out.push(square),
            Err(error) => failed = failed.or(Some(error.to_string())),
        }
    }
    let joined = scope.join(&cx);
    if let Some(failed) = failed {
        return Err(failed);
    }
    joined.map_err(|error| error.to_string())?;
    Ok(out)
}

#[test]
fn may_overlap_gives_one_result_under_one_worker_and_many() {
    blaze_sim::check(Config::cases(100), |tape| {
        let seed = tape.next_u64("sim seed");
        let n = tape.int_in(0..=24u32, "tasks");
        let sequential: Vec<u64> = (0..u64::from(n)).map(|at| at * at).collect();
        let one = squares(seed, n, Some(NonZeroU32::MIN));
        let many = squares(seed, n, None);
        assert_eq!(one.as_ref(), Ok(&sequential), "narrowed to one worker; seed {seed}");
        assert_eq!(many.as_ref(), Ok(&sequential), "the layout's workers; seed {seed}");
    });
}
