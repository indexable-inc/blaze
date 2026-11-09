//! Cancelling a scope ends its waits with `Stopped::Cancelled`: once a task of a scope fails
//! or panics, or the scope is cancelled, every wait of its later tasks (a sleep, a read) ends
//! with `Cancelled`, the ones before it ended as they would have, and the scope returns its
//! first failure, naming the task.
//!
//! Plant: [`Plant::WaitIgnoresCancel`], a wait that does not look at its task's cancellation.

use std::time::Duration;

use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, path, root, world};
use crate::{
    Plant, Stopped,
    clock::SleepError,
    task::{TaskError, TaskWhy},
};

const NAMES: [&str; 6] = ["t0", "t1", "t2", "t3", "t4", "t5"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Waits,
    Fails,
    Panics,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    tasks: Vec<Kind>,
    /// The scope is cancelled just before this task is spawned.
    cancel_at: Option<u64>,
    sleep_ms: u64,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let n = d.int(1, NAMES.len() as u64);
    let tasks = (0..n)
        .map(|_| match d.int(0, 5) {
            0 => Kind::Fails,
            1 if d.one_in(3) => Kind::Panics,
            _ => Kind::Waits,
        })
        .collect();
    let cancel_at = d.one_in(3).then(|| d.int(0, n - 1));
    Plan { seed, tasks, cancel_at, sleep_ms: d.int(0, 50) }
}

/// How one waiting task's two waits ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ended {
    Done,
    Cancelled,
    Other,
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let clock = io.clock();
    let dir = io.fs().open_dir(&cx, &path("data")).map_err(|e| format!("harness: {e}"))?;
    dir.write(&cx, &path("f"), b"bytes").map_err(|e| format!("harness: {e}"))?;
    let mut ended: Vec<(usize, Ended, Ended)> = Vec::new();
    let result = cx.scope(|s| {
        for (at, &kind) in plan.tasks.iter().enumerate() {
            if plan.cancel_at == Some(at as u64) {
                s.cancel();
            }
            let ended = &mut ended;
            let (clock, dir) = (&clock, &dir);
            s.spawn(NAMES[at], move |cx| -> Result<(), String> {
                match kind {
                    Kind::Fails => return Err("this task fails".to_owned()),
                    Kind::Panics => panic!("this task panics (a planted panic of the cancel test)"),
                    Kind::Waits => {}
                }
                let slept = match clock.sleep(cx, Duration::from_millis(plan.sleep_ms)) {
                    Ok(()) => Ended::Done,
                    Err(SleepError::Stopped(Stopped::Cancelled { .. })) => Ended::Cancelled,
                    Err(_) => Ended::Other,
                };
                let mut into = cx.bytes();
                let read = match dir.read_all(cx, &path("f"), &mut into) {
                    Ok(_) => Ended::Done,
                    Err(error) => match error.stopped() {
                        Some(Stopped::Cancelled { .. }) => Ended::Cancelled,
                        _ => Ended::Other,
                    },
                };
                ended.push((at, slept, read));
                Ok(())
            });
        }
    });
    // The first task whose spawn found the scope cancelled, or that cancelled it.
    let first_failure = plan.tasks.iter().position(|kind| *kind != Kind::Waits);
    let cancelled_from = match (first_failure.map(|at| at + 1), plan.cancel_at.map(|at| at as usize)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    for &(at, slept, read) in &ended {
        let want = match cancelled_from {
            Some(from) if at >= from => Ended::Cancelled,
            _ => Ended::Done,
        };
        if slept != want || read != want {
            return Err(format!(
                "task {at} slept {slept:?} and read {read:?}, but the scope's cancellation says {want:?} (cancelled from task {cancelled_from:?})"
            ));
        }
    }
    match (first_failure, result) {
        (None, Ok(())) => Ok(()),
        (Some(at), Err(TaskError { scope: "main", task, why })) if task == NAMES[at] => {
            let fits = match plan.tasks[at] {
                Kind::Fails => why == TaskWhy::Failed("this task fails".to_owned()),
                Kind::Panics => matches!(why, TaskWhy::Panicked(_)),
                Kind::Waits => false,
            };
            if fits { Ok(()) } else { Err(format!("task {at} failed the scope with {why:?}")) }
        }
        (want, got) => Err(format!("the scope's first failure should be task {want:?}; it returned {got:?}")),
    }
}

#[hegel::test(test_cases = 150)]
fn cancelling_a_scope_ends_its_waits_with_cancelled(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_a_wait_that_ignores_cancellation() {
    control(Plant::WaitIgnoresCancel, |seeds: &mut Seeds| plan(seeds), run);
}
