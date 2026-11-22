//! A task has one lock-order token: `cx.lock_order()` gives it when it is in and `None` while
//! it is out, and it comes back when dropped; ranked locks taken with it, and with the tokens
//! their guards lend, hold what was written. Takes and drops of the token, and nested locks of
//! two ranks, interleave as the tape draws. That a lock taken out of rank order does not
//! compile is the `const` check in `RankedLock::lock`, which no run can show.
//!
//! Plant: [`Plant::OrderTokenTwice`], a context that gives its token while it is out.

use blaze_sim::{Config, Tape};

use super::{check, control, root, world};
use crate::{
    Plant,
    sync::{Order, Rank, RankedLock, Unranked},
};

enum Store {}
impl Rank for Store {
    const LEVEL: u32 = 10;
    const NAME: &'static str = "test.store";
}

enum Index {}
impl Rank for Index {
    const LEVEL: u32 = 20;
    const NAME: &'static str = "test.index";
}

#[derive(Clone, Copy, Debug)]
enum Op {
    /// Asks for the token, and keeps it if given.
    Take,
    /// Drops the token, if held.
    Give,
    /// With the token held: both locks, in rank order, each adding this.
    Both(u64),
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    ops: Vec<Op>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let ops = (0..tape.int_in(1..=30u64, "ops"))
        .map(|_| match tape.int_in(0..=2u64, "op") {
            0 => Op::Take,
            1 => Op::Give,
            _ => Op::Both(tape.int_in(0..=9u64, "add")),
        })
        .collect();
    Plan { seed, ops }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let store: RankedLock<u64, Store> = RankedLock::new("test.store", 0);
    let index: RankedLock<u64, Index> = RankedLock::new("test.index", 0);
    let mut token: Option<Order<'_, Unranked>> = None;
    let mut sum = 0u64;
    for (step, op) in plan.ops.iter().enumerate() {
        match *op {
            Op::Take => match (cx.lock_order(), token.is_some()) {
                (Some(given), false) => token = Some(given),
                (None, true) => {}
                (Some(_second), true) => {
                    return Err(format!("step {step}: a second lock-order token was given while one is out"));
                }
                (None, false) => return Err(format!("step {step}: no token was given though none is out")),
            },
            Op::Give => drop(token.take()),
            Op::Both(add) => {
                if let Some(order) = token.as_mut() {
                    let mut low = store.lock(order).map_err(|e| format!("harness: {e}"))?;
                    *low += add;
                    let mut high = index.lock(low.order()).map_err(|e| format!("harness: {e}"))?;
                    *high += add;
                    sum += add;
                    if *high != sum {
                        return Err(format!("step {step}: the index lock holds {}, the adds {sum}", *high));
                    }
                }
            }
        }
    }
    drop(token);
    match cx.lock_order() {
        Some(_) => Ok(()),
        None => Err("the token did not come back when dropped".to_owned()),
    }
}

/// The invariant: one token a task, back when dropped, and ranked locks hold what was written.
#[test]
fn a_task_has_one_lock_order_token() {
    blaze_sim::check(Config::cases(200), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_second_token() {
    control(Plant::OrderTokenTwice, plan, run);
}
