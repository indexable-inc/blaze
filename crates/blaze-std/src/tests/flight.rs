//! `Flight`'s table computes a key at most once at a time: askers of a running key wait for
//! that computation, an asker is told to wait only for a running computation or a kept
//! success, a failure is forgotten so the next asker computes again, the table never holds more
//! than its capacity, and it is full only when every key in it is running. Askers and
//! finishes of the sans-IO table interleave as the tape draws; one example then runs the
//! driver: kept, failed and retried, panicked and retried, and a wait that ends at its
//! deadline.
//!
//! Plant: [`Plant::FlightComputesTwice`], a table that starts a second computation of a key
//! that is running.

use std::{
    alloc::Global,
    cell::Cell,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    time::Duration,
};

use blaze_sim::{Config, Tape};

use super::{check, control, root, world};
use crate::{
    Plant, Stopped,
    sync::{
        Flight, FlightError,
        flight::{Ask, Table},
    },
};

#[derive(Clone, Copy, Debug)]
enum Op {
    Ask(u8),
    /// Finishes the running computation at this index (modulo how many run).
    Finish { pick: u64, ok: bool },
}

#[derive(Debug)]
struct Plan {
    capacity: usize,
    ops: Vec<Op>,
}

fn plan(tape: &Tape) -> Plan {
    let capacity = tape.int_in(1..=4u64, "capacity") as usize;
    let ops = (0..tape.int_in(1..=40u64, "ops"))
        .map(|_| match tape.int_in(0..=2u64, "op") {
            0 | 1 => Op::Ask(tape.int_in(0..=5u8, "key")),
            _ => Op::Finish { pick: tape.next_u64("which"), ok: !tape.one_in(3, "fails") },
        })
        .collect();
    Plan { capacity, ops }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let capacity = NonZeroUsize::new(plan.capacity).expect("a plan's capacity is at least one");
    let mut table: Table<u8, (), Global> = Table::with_capacity_in(capacity, Global, plant);
    // The model: computations told to run and not finished, and successes finished.
    let mut computing: Vec<(u8, u64)> = Vec::new();
    let mut succeeded: Vec<(u8, u64)> = Vec::new();
    for (step, op) in plan.ops.iter().enumerate() {
        match *op {
            Op::Ask(key) => match table.ask(key, || ()) {
                Ask::Compute { attempt, .. } => {
                    if computing.iter().any(|&(k, _)| k == key) {
                        return Err(format!("step {step}: key {key} is computed again while it runs"));
                    }
                    computing.push((key, attempt));
                }
                Ask::Wait { attempt, .. } => {
                    if !computing.contains(&(key, attempt)) && !succeeded.contains(&(key, attempt)) {
                        return Err(format!("step {step}: an asker of {key} waits on {attempt}, which neither runs nor succeeded"));
                    }
                }
                Ask::Full => {
                    if computing.len() != plan.capacity {
                        return Err(format!("step {step}: full with {} of {} running", computing.len(), plan.capacity));
                    }
                }
            },
            Op::Finish { pick, ok } if !computing.is_empty() => {
                let (key, attempt) = computing.remove(pick as usize % computing.len());
                table.finish(&key, attempt, ok);
                if ok {
                    succeeded.push((key, attempt));
                }
            }
            Op::Finish { .. } => {}
        }
        if table.len() > plan.capacity {
            return Err(format!("step {step}: the table holds {} keys, its capacity {}", table.len(), plan.capacity));
        }
        let mut running: Vec<(u8, u64)> = table.running().map(|(&k, a)| (k, a)).collect();
        running.sort_unstable();
        if running.windows(2).any(|two| two[0].0 == two[1].0) {
            return Err(format!("step {step}: two computations of one key run: {running:?}"));
        }
    }
    Ok(())
}

/// The invariant: a key is computed at most once at a time, and the table stays in its bound.
#[test]
fn a_flight_computes_a_key_once_at_a_time() {
    blaze_sim::check(Config::cases(300), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_flight_that_computes_twice() {
    control(Plant::FlightComputesTwice, plan, run);
}

/// The driver once: a success is kept, a failure is shared and forgotten, a panic frees its
/// key, and an asker of its own running key waits until its deadline.
#[test]
fn a_flight_keeps_successes_and_retries_failures() {
    let io = world(7, Plant::None);
    let cx = root(&io);
    let clock = io.need_clock().expect("a simulated clock");
    let flight: Flight<u8, u32, String, Global> =
        Flight::with_capacity_in("test.flight", NonZeroUsize::new(2).expect("two"), Global);
    let computed = Cell::new(0u32);
    let count = |value: u32| {
        computed.set(computed.get() + 1);
        Ok(value)
    };
    assert_eq!(flight.ask(&cx, 1, |_| count(10)), Ok(10));
    assert_eq!(flight.ask(&cx, 1, |_| count(11)), Ok(10), "a kept success is not computed again");
    assert_eq!(computed.get(), 1);

    let failed = flight.ask(&cx, 2, |_| Err("no".to_owned()));
    assert_eq!(failed, Err(FlightError::Failed { flight: "test.flight", error: "no".to_owned() }));
    assert_eq!(flight.ask(&cx, 2, |_| count(20)), Ok(20), "a failed key is computed again");

    let panicked = catch_unwind(AssertUnwindSafe(|| flight.ask(&cx, 3, |_| panic!("a planted panic of the flight test"))));
    assert!(panicked.is_err());
    assert_eq!(flight.ask(&cx, 3, |_| count(30)), Ok(30), "a panicked key is computed again");

    let brief = cx.until(clock.after(Duration::from_millis(5)));
    let inner = flight.ask(&brief, 4, |cx| match flight.ask(cx, 4, |_| count(41)) {
        Err(FlightError::Stopped(Stopped::Deadline { .. })) => count(40),
        other => panic!("an asker of its own running key should wait to its deadline: {other:?}"),
    });
    assert_eq!(inner, Ok(40));
}
