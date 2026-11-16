//! `par`'s results are the sequential ones whatever ran first: `reduce` with a join that is
//! not commutative (concatenation) gives the input back in order, `map_into` writes item `i`'s
//! result at `out[i]`, `for_each` over two columns visits each index exactly once, and ragged
//! columns are refused before anything runs. The simulation draws which half of each cut runs
//! first, so the plans explore the orders.
//!
//! Plant: [`Plant::ParJoinsInRunOrder`], a reduce that joins its halves in the order they ran.

use std::num::NonZeroUsize;

use blaze_sim::{Config, Tape};

use super::{check, control, root, world};
use crate::{
    Cx, Plant,
    par::{self, Grain, ParError},
    task::Scope,
};

/// Each chunk's task's frame: the tests' chunks allocate nothing in it.
const SCRATCH: usize = 1 << 16;

#[derive(Debug)]
struct Plan {
    seed: u64,
    items: Vec<u64>,
    grain: usize,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let n = tape.int_in(0..=96u64, "items");
    let items = (0..n).map(|_| tape.int_in(0..=1000u64, "item")).collect();
    let grain = tape.int_in(1..=12u64, "grain") as usize;
    Plan { seed, items, grain }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let grain = Grain::new(NonZeroUsize::new(plan.grain).expect("a plan's grain is at least one"), SCRATCH);
    // A scope is linear: nothing returns while it is live, so the finding waits for its join.
    let scope = cx.scope();
    let found = parallel(plan, &cx, &scope, grain);
    let joined = scope.join(&cx);
    found?;
    joined.map_err(|error| format!("the scope: {error}"))
}

fn parallel(plan: &Plan, cx: &Cx, scope: &Scope<'_>, grain: Grain) -> Result<(), String> {
    let items = &plan.items[..];
    let joined = par::reduce(
        cx,
        scope,
        items,
        grain,
        Vec::new,
        |mut got, chunk: &[u64]| {
            got.extend_from_slice(chunk);
            got
        },
        |mut left, right| {
            left.extend(right);
            left
        },
    )
    .map_err(|error| format!("reduce: {error}"))?;
    if joined != plan.items {
        return Err(format!("reduce by concatenation gave {joined:?}, not the input in order"));
    }

    let mut doubled = vec![0u64; items.len()];
    par::map_into(cx, scope, items, &mut doubled, grain, |item| item * 2).map_err(|error| format!("map_into: {error}"))?;
    if let Some(at) = (0..items.len()).find(|&at| doubled[at] != items[at] * 2) {
        return Err(format!("map_into wrote {} at {at}, for item {}", doubled[at], items[at]));
    }

    let mut visits = vec![0u32; items.len()];
    par::for_each(cx, scope, (&mut visits[..], items), grain, |(visits, _items): (&mut [u32], &[u64])| {
        for visit in visits {
            *visit += 1;
        }
    })
    .map_err(|error| format!("for_each: {error}"))?;
    if let Some(at) = visits.iter().position(|&visit| visit != 1) {
        return Err(format!("for_each visited index {at} {} times", visits[at]));
    }

    let mut short = vec![0u64; items.len() + 1];
    match par::map_into(cx, scope, items, &mut short, grain, |item| *item) {
        Err(ParError::Ragged { .. }) => Ok(()),
        other => Err(format!("columns of {} and {} items were not refused: {other:?}", items.len(), short.len())),
    }
}

/// The invariant: `par`'s results are the sequential ones in every order the halves ran.
#[test]
fn par_gives_the_sequential_result_in_every_order() {
    blaze_sim::check(Config::cases(200), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_reduce_that_joins_in_run_order() {
    control(Plant::ParJoinsInRunOrder, plan, run);
}
