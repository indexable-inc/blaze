//! A started task never changes its thread (crate::task, "A started task never changes its
//! thread"; std.md, "Layouts"): every task, nested ones too, resumes from each of its waits on
//! the worker it was placed on at its start. `par`'s chunks are tasks of `Scope::may_overlap`, placed
//! the same way; its closures are given no `cx` to wait with.
//!
//! Plant: [`Plant::TaskMigrates`], a resume that moves the task to the next worker.

use std::time::Duration;

use blaze_core::lock::Lock;

use blaze_sim::{Config, Tape};

use super::{check, control, root, world};
use crate::{Cx, Plant, clock::Clock};

/// Each task's frame: they allocate nothing in it.
const SCRATCH: usize = 1 << 16;

#[derive(Debug)]
struct Plan {
    seed: u64,
    /// Each task's sleeps, in ms; a task with `nested` spawns one more inside its scope.
    tasks: Vec<(Vec<u64>, bool)>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let n = tape.int_in(1..=5u64, "tasks");
    let tasks = (0..n)
        .map(|_| {
            let sleeps = tape.int_in(1..=4u64, "sleeps");
            let sleeps = (0..sleeps).map(|_| tape.int_in(1..=20u64, "sleep ms")).collect();
            (sleeps, tape.coin("nested"))
        })
        .collect();
    Plan { seed, tasks }
}

/// Sleeps `ms` on `clock`, and says where the task resumed if not where it was placed.
fn sleep_in_place(cx: &Cx, clock: &Clock, ms: u64, what: &str) -> Result<(), String> {
    let placed = cx.worker();
    let _ = clock.sleep(cx, Duration::from_millis(ms));
    match cx.worker() {
        now if now == placed => Ok(()),
        now => Err(format!("{what}: placed on worker {placed}, resumed on {now} after a {ms} ms sleep")),
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let clock = io.need_clock().map_err(|e| format!("harness: {e}"))?;
    // Behind a lock: a task's closure is `Send`.
    let wrong: Lock<Option<String>> = Lock::new("test.wrong", None);
    let note = |found: Result<(), String>| {
        if let Err(why) = found {
            wrong.lock().get_or_insert(why);
        }
    };
    let scope = cx.scope();
    for (at, (sleeps, nested)) in plan.tasks.iter().enumerate() {
        let (clock, note) = (&clock, &note);
        let task = scope.may_overlap("placed", SCRATCH, move |cx| -> Result<(), String> {
            for &ms in sleeps {
                note(sleep_in_place(cx, clock, ms, &format!("task {at}")));
            }
            if *nested {
                let inner = cx.scope();
                let child = inner.may_overlap("nested", SCRATCH, |cx| -> Result<(), String> {
                    note(sleep_in_place(cx, clock, sleeps[0], &format!("task {at}'s nested task")));
                    Ok(())
                });
                let _ = child.join(cx);
                let _ = inner.join(cx);
            }
            Ok(())
        });
        let _ = task.join(&cx);
    }
    let joined = scope.join(&cx);
    if let Some(why) = wrong.lock().take() {
        return Err(why);
    }
    joined.map_err(|e| format!("the scope: {e}"))
}

/// The invariant: a started task resumes from each wait on the worker it started on.
#[test]
fn a_started_task_never_changes_its_worker() {
    blaze_sim::check(Config::cases(150), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_task_that_migrates() {
    control(Plant::TaskMigrates, plan, run);
}
