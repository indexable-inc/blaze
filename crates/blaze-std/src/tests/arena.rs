//! An arena never hands out more than its limit: whatever appends a plan makes to bytes in a
//! named arena, the arena's use stays within its limit, an append past it is
//! `Stopped::OutOfMemory` naming the arena and changes nothing, and the bytes are exactly the
//! appends that were taken.
//!
//! Plant: [`Plant::ArenaUnlimited`], an arena made with no limit of its own.

use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, world};
use crate::{Plant, Stopped};

#[derive(Debug)]
struct Plan {
    seed: u64,
    limit: usize,
    appends: Vec<usize>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let limit = d.int(64, 8192) as usize;
    let appends = (0..d.int(1, 24)).map(|_| d.int(0, 2048) as usize).collect();
    Plan { seed, limit, appends }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let arena = io.mem().arena("test arena", plan.limit).map_err(|e| format!("harness: {e}"))?;
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

#[hegel::test(test_cases = 200)]
fn an_arena_never_hands_out_more_than_its_limit(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_an_arena_with_no_limit() {
    control(Plant::ArenaUnlimited, |seeds: &mut Seeds| plan(seeds), run);
}
