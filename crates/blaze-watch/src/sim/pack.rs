//! The recording format: rows round-trip through a block exactly, several blocks follow one
//! another, and bytes that are not a block (cut short, or with any byte changed) are refused or
//! read as some rows, never a panic or an overread.

use std::alloc::Global;

use blaze_own::Vec as Own;
use blaze_sim::{Broke, Config, Tape, check};

use super::{harness, key, oracle};
use crate::pack::{self, Row};
use crate::ring::Event;

fn rows(tape: &Tape) -> std::vec::Vec<Row> {
    let mut n = tape.below(1 << 40, "first n");
    let mut at = tape.next_u64("first time");
    (0..tape.below(200, "rows"))
        .map(|_| {
            n = n.wrapping_add(tape.range(0..3, "n step"));
            at = at.wrapping_add(tape.below(1 << 20, "time step")).wrapping_sub(tape.below(4, "back"));
            let key = match tape.below(3, "key") {
                0 => [0; 32],
                k => key(k + tape.below(4, "which key")),
            };
            Row {
                lane: tape.below(8, "lane") as u32,
                n,
                event: Event {
                    kind: tape.below(24, "kind") as u16,
                    flags: tape.below(1 << 16, "flags") as u16,
                    detail: tape.next_u32("detail"),
                    at,
                    value: tape.next_u64("value"),
                    key,
                },
            }
        })
        .collect()
}

fn round_trip(tape: &Tape) -> Result<(), Broke> {
    let blocks: std::vec::Vec<_> = (0..tape.range(1..4, "blocks")).map(|_| rows(tape)).collect();
    let mut bytes = Own::new_in(Global);
    let mut scratch = Own::new_in(Global);
    pack::prelude(&mut bytes);
    for block in &blocks {
        pack::write(block, &mut bytes, &mut scratch, Global).map_err(harness)?;
    }
    let mut at = pack::open(&bytes).map_err(harness)?;
    let mut first_end = None;
    for block in &blocks {
        let mut got = Own::new_in(Global);
        at = pack::read(&bytes, at, &mut got).map_err(|bad| Broke::Oracle(format!("its own block was refused: {bad}")))?;
        first_end.get_or_insert(at);
        oracle(got[..] == block[..], || format!("a block of {} rows read back as {} others", block.len(), got.len()))?;
    }
    oracle(at == bytes.len(), || format!("the blocks ended at {at} of {} bytes", bytes.len()))?;
    // The first block cut short anywhere: refused.
    let first_end = first_end.ok_or_else(|| harness("no block was written"))?;
    let cut = 4 + tape.index(first_end - 4, "cut at");
    let mut got = Own::new_in(Global);
    let read = pack::read(&bytes[..cut], 4, &mut got);
    oracle(read.is_err(), || format!("a block cut at byte {cut} of {first_end} was read as {read:?}"))?;
    // Any byte changed: refused or read, never a panic.
    let at = 4 + tape.index(bytes.len() - 4, "changed byte");
    bytes[at] ^= 1 + tape.below(255, "change") as u8;
    let mut got = Own::new_in(Global);
    // Either answer is right here; what is checked is that reading returns at all.
    let _either = pack::read(&bytes, 4, &mut got);
    Ok(())
}

#[test]
fn recordings_round_trip_and_refuse_garbage() {
    let lap = super::Lap::new("pack: recordings round-trip");
    check(Config::cases(2000), |tape| lap.case(round_trip(tape)));
}
