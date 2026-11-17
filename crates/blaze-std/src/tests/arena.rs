//! An arena never hands out more than its limit: whatever appends a plan makes to bytes in a
//! named arena, the arena's use stays within its limit, an append past it is
//! `Stopped::OutOfMemory` naming the arena and changes nothing, and the bytes are exactly the
//! appends that were taken.
//!
//! Plant: [`Plant::ArenaUnlimited`], an arena made with no limit of its own.

use blaze_sim::{Config, Tape};

use super::{check, control, world};
use crate::{Plant, Stopped};

#[derive(Debug)]
struct Plan {
    seed: u64,
    limit: usize,
    appends: Vec<usize>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let limit = tape.int_in(64..=8192u64, "limit") as usize;
    let appends = (0..tape.int_in(1..=24u64, "appends")).map(|_| tape.int_in(0..=2048u64, "append len") as usize).collect();
    Plan { seed, limit, appends }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let arena = io.need_mem().map_err(|e| format!("harness: {e}"))?.arena("test arena", plan.limit).map_err(|e| format!("harness: {e}"))?;
    let mut bytes = arena.bytes();
    let mut model: Vec<u8> = Vec::new();
    for (at, &len) in plan.appends.iter().enumerate() {
        let chunk = vec![at as u8; len];
        match bytes.try_extend(&chunk) {
            Ok(()) => model.extend_from_slice(&chunk),
            Err(Stopped::OutOfMemory { arena: "test arena", asked }) if asked >= len => {}
            Err(other) => return Err(format!("append {at} of {len} bytes stopped with {other}")),
        }
        if arena.used() > plan.limit {
            return Err(format!(
                "after append {at} of {len} bytes the arena uses {} bytes, past its limit {}",
                arena.used(),
                plan.limit
            ));
        }
        if bytes.as_slice() != model.as_slice() {
            return Err(format!("after append {at} the bytes are not the appends that were taken"));
        }
    }
    Ok(())
}

/// The invariant: an arena never hands out more than its limit. Whatever appends are made to
/// bytes in a named arena, its use stays within its limit, an append past it is
/// `Stopped::OutOfMemory` naming the arena and changes nothing, and the bytes are exactly the
/// appends that were taken.
#[test]
fn an_arena_never_hands_out_more_than_its_limit() {
    blaze_sim::check(Config::cases(200), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_an_arena_with_no_limit() {
    control(Plant::ArenaUnlimited, plan, run);
}
