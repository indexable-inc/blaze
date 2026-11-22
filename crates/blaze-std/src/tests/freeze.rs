//! A frozen value is what was built, every handle reads the same one, the handle count is the
//! handles alive, and the value is dropped exactly once, when the last handle goes.
//!
//! Plant: [`Plant::FreezeLeaks`], a freeze that keeps a handle no one holds.

use std::{alloc::Global, cell::Cell};

use blaze_sim::{Config, Tape};

use super::{check, control};
use crate::{
    Plant,
    sync::{Building, Frozen},
};

#[derive(Clone, Copy, Debug)]
enum Op {
    Clone(u64),
    Drop(u64),
    Read(u64),
}

#[derive(Debug)]
struct Plan {
    built: Vec<u64>,
    ops: Vec<Op>,
}

fn plan(tape: &Tape) -> Plan {
    let n = tape.int_in(0..=8u64, "built");
    let built = (0..n).map(|_| tape.next_u64("built value")).collect();
    let ops = (0..tape.int_in(0..=24u64, "ops"))
        .map(|_| {
            let at = tape.next_u64("handle");
            match tape.int_in(0..=2u64, "op") {
                0 => Op::Clone(at),
                1 => Op::Drop(at),
                _ => Op::Read(at),
            }
        })
        .collect();
    Plan { built, ops }
}

/// The value built: `Freeze` (a reference to a `Cell` is), and counts its drops.
struct Table<'a> {
    rows: [u64; 8],
    len: usize,
    drops: &'a Cell<u32>,
}

impl Drop for Table<'_> {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let drops = Cell::new(0);
    let mut building = Building::planted(Table { rows: [0; 8], len: 0, drops: &drops }, Global, plant);
    for &value in &plan.built {
        let at = building.len;
        building.rows[at] = value;
        building.len += 1;
    }
    let mut handles: Vec<Frozen<Table<'_>, Global>> = vec![building.freeze()];
    for (step, op) in plan.ops.iter().enumerate() {
        match *op {
            Op::Clone(at) if !handles.is_empty() => {
                let copy = Frozen::clone(&handles[at as usize % handles.len()]);
                handles.push(copy);
            }
            Op::Drop(at) if !handles.is_empty() => drop(handles.remove(at as usize % handles.len())),
            Op::Read(at) if !handles.is_empty() => {
                let read = &handles[at as usize % handles.len()];
                if read.rows[..read.len] != plan.built[..] {
                    return Err(format!("step {step}: a handle read {:?}, not what was built", &read.rows[..read.len]));
                }
                if !Frozen::ptr_eq(read, &handles[0]) {
                    return Err(format!("step {step}: two handles of one freeze point at different values"));
                }
            }
            Op::Clone(_) | Op::Drop(_) | Op::Read(_) => {}
        }
        if let Some(first) = handles.first()
            && Frozen::handles(first) != handles.len()
        {
            return Err(format!("step {step}: {} handles alive, the value counts {}", handles.len(), Frozen::handles(first)));
        }
        if !handles.is_empty() && drops.get() != 0 {
            return Err(format!("step {step}: the value was dropped with {} handles alive", handles.len()));
        }
    }
    drop(handles);
    match drops.get() {
        1 => Ok(()),
        n => Err(format!("the last handle went and the value was dropped {n} times, not once")),
    }
}

/// The invariant: one value, read the same through every handle, dropped once with the last.
#[test]
fn a_frozen_value_is_shared_and_dropped_once() {
    blaze_sim::check(Config::cases(200), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_freeze_that_leaks() {
    control(Plant::FreezeLeaks, plan, run);
}
