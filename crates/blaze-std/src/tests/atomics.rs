//! The named atomics against a model, in the order the tape draws: a `PublishOnce` keeps its
//! first value and gives every later one back, a `Counter` totals its adds and `take` empties
//! it, a `Flag` says whether it was set, and a `Seq` steps up by one from each step taken.
//! These are their meanings, on one thread; their memory orderings, chosen inside each type,
//! are argued in its doc and not tested here (bstd's simulation runs one thread; blaze-check's
//! interleaving of atomics is not wired into bstd).
//!
//! Plant: [`Plant::PublishOnceOverwrites`], a second `set` that replaces the published value.

use blaze_sim::{Config, Tape};

use super::{check, control};
use crate::{
    Plant,
    sync::{Counter, Flag, PublishOnce, Seq},
};

#[derive(Clone, Copy, Debug)]
enum Op {
    Set(u64),
    Get,
    Add(u64),
    Take,
    Raise,
    Advance,
}

#[derive(Debug)]
struct Plan {
    ops: Vec<Op>,
}

fn plan(tape: &Tape) -> Plan {
    let ops = (0..tape.int_in(1..=40u64, "ops"))
        .map(|_| match tape.int_in(0..=5u64, "op") {
            0 => Op::Set(tape.next_u64("value")),
            1 => Op::Get,
            2 => Op::Add(tape.int_in(0..=100u64, "add")),
            3 => Op::Take,
            4 => Op::Raise,
            _ => Op::Advance,
        })
        .collect();
    Plan { ops }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let once = PublishOnce::new();
    let (counter, flag, seq) = (Counter::new(), Flag::new(), Seq::new());
    let (mut first, mut total, mut raised, mut steps) = (None, 0u64, false, 0u64);
    for (step, op) in plan.ops.iter().enumerate() {
        match *op {
            Op::Set(value) => match (once.set_planted(value, plant), first) {
                (Ok(()), None) => first = Some(value),
                (Err(back), Some(_)) if back == value => {}
                (got, had) => return Err(format!("step {step}: set {value} with {had:?} published gave {got:?}")),
            },
            Op::Get => {
                if once.get().copied() != first {
                    return Err(format!("step {step}: get gave {:?}, the first set {first:?}", once.get()));
                }
            }
            Op::Add(n) => {
                if counter.add(n) != total {
                    return Err(format!("step {step}: an add saw a count other than {total}"));
                }
                total += n;
            }
            Op::Take => {
                if counter.take() != total || counter.get() != 0 {
                    return Err(format!("step {step}: take did not give {total} and leave none"));
                }
                total = 0;
            }
            Op::Raise => {
                if flag.set() != raised || !flag.is_set() {
                    return Err(format!("step {step}: the flag, set {raised} before, said otherwise"));
                }
                raised = true;
            }
            Op::Advance => {
                steps += 1;
                if seq.advance() != steps || seq.now() != steps {
                    return Err(format!("step {step}: the sequence did not step to {steps}"));
                }
            }
        }
    }
    match (once.into_inner(), first) {
        (got, want) if got == want => Ok(()),
        (got, want) => Err(format!("into_inner gave {got:?}, the first set {want:?}")),
    }
}

/// The invariant: each named atomic means what its doc says.
#[test]
fn the_named_atomics_mean_what_they_say() {
    blaze_sim::check(Config::cases(200), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_publish_once_that_overwrites() {
    control(Plant::PublishOnceOverwrites, plan, run);
}
