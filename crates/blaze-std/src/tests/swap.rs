//! A `Swap`'s reader sees the value last published, and a replaced value is never dropped
//! while a task that was in when it was replaced is still in; by the time the `Swap` is
//! dropped every value it held has been dropped exactly once. Tasks enter and leave
//! (`reclaim::enter`, nested on this thread), read and publish in an order the tape draws. A
//! reader never reads a replaced value here, so the planted bug is seen by its drop, not by a
//! read of freed memory.
//!
//! Plant: [`Plant::SwapFreesAtOnce`], a publish that drops the value it replaced at once.

use std::{
    alloc::Global,
    sync::{Arc, Mutex},
};

use blaze_sim::{Config, Tape};

use super::{check, control, root, world};
use crate::{
    Plant,
    sync::{Swap, reclaim},
};

#[derive(Clone, Copy, Debug)]
enum Op {
    Enter,
    Leave,
    Publish,
    Read,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    ops: Vec<Op>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let ops = (0..tape.int_in(1..=40u64, "ops"))
        .map(|_| match tape.int_in(0..=3u64, "op") {
            0 => Op::Enter,
            1 => Op::Leave,
            2 => Op::Publish,
            _ => Op::Read,
        })
        .collect();
    Plan { seed, ops }
}

/// A published value, which says when it is dropped.
struct Value {
    id: u32,
    dropped: Arc<Mutex<Vec<u32>>>,
}

impl Drop for Value {
    fn drop(&mut self) {
        self.dropped.lock().expect("the test's drop log").push(self.id);
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let made = |id| Value { id, dropped: Arc::clone(&dropped) };
    let swap = Swap::planted("test.swap", made(0), Global, plant);
    let mut tasks: Vec<reclaim::Guard> = Vec::new();
    let (mut current, mut next) = (0u32, 1u32);
    // Values replaced while this thread was in a task, which it has not left since.
    let mut guarded: Vec<u32> = Vec::new();
    let mut wrong = None;
    for (step, op) in plan.ops.iter().enumerate() {
        match op {
            Op::Enter => tasks.push(reclaim::enter()),
            Op::Leave => {
                if tasks.pop().is_some() && tasks.is_empty() {
                    guarded.clear();
                }
            }
            Op::Publish => {
                let id = next;
                next += 1;
                if let Err(stopped) = swap.publish(&cx, made(id)) {
                    wrong = Some(format!("harness: step {step}: publish: {stopped}"));
                    break;
                }
                if !tasks.is_empty() {
                    guarded.push(current);
                }
                current = id;
            }
            Op::Read => {
                if let Some(task) = tasks.last() {
                    let seen = swap.read(task).id;
                    if seen != current {
                        wrong = Some(format!("step {step}: a reader saw value {seen}, and {current} was published last"));
                        break;
                    }
                }
            }
        }
        let gone = dropped.lock().expect("the test's drop log");
        if let Some(id) = guarded.iter().find(|id| gone.contains(id)) {
            wrong = Some(format!("step {step}: value {id} was dropped while a task in when it was replaced is still in"));
            break;
        }
        if gone.contains(&current) {
            wrong = Some(format!("step {step}: the published value {current} was dropped"));
            break;
        }
    }
    drop(tasks);
    drop(swap);
    if let Some(why) = wrong {
        return Err(why);
    }
    let mut gone = dropped.lock().expect("the test's drop log").clone();
    gone.sort_unstable();
    match gone == (0..next).collect::<Vec<_>>() {
        true => Ok(()),
        false => Err(format!("{next} values were published and the drops were {gone:?}, not each once")),
    }
}

/// The invariant: readers see the last publish, a replaced value outlives the tasks that could
/// read it, and every value is dropped once.
#[test]
fn a_swap_frees_a_value_only_after_its_readers() {
    blaze_sim::check(Config::cases(200), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_swap_that_frees_at_once() {
    control(Plant::SwapFreesAtOnce, plan, run);
}
