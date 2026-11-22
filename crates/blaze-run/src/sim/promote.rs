//! [`Promote`] with sampled heat, the optimizing tier modelled as jobs that end in any order, and
//! [`Slots`] publishing each placement.
//!
//! The oracle: a slot is laid out in the hot region at most once, and only once its samples
//! reached the bound; every slot that reached it and whose record was made is laid out by the end;
//! placements never overlap, are aligned and stay inside the region; each is published by exactly
//! one store of its slot word, naming the new optimized body. The control: a batch kept after it
//! was laid out (`Plant::PromoteTwice`) lays a slot out again, and the oracle says "promoted twice".
//! A unit test pins the order: a hot callee follows its heavy caller, whatever order they came in.

use std::collections::BTreeSet;
use std::format;

use blaze_own::{AVec, Alloc, Vec as Own};
use blaze_sim::{Broke, Config, Tape, check, control};

use super::{harness, key, oracle};
use crate::heat::Config as HeatConfig;
use crate::promote::{Config as PromoteConfig, Placement, Promote, SlotEdge, SlotSamples};
use crate::slots::{Names, Slots, Write};
use crate::{CodeRef, Plant, SlotId, Tier};

const SLOTS: u32 = 6;
const BOUND: u32 = 30;
const HOT: PromoteConfig = PromoteConfig { hot_bytes: 1 << 16, align: 16, max_cluster: 1 << 20 };

fn optimized(slot: SlotId) -> CodeRef {
    CodeRef(key(1000 + u64::from(slot.0)))
}

fn fast(slot: SlotId) -> CodeRef {
    CodeRef(key(2000 + u64::from(slot.0)))
}

fn promoted(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut promote = Promote::new(SLOTS, HeatConfig { bound: BOUND }, HOT, Alloc::global()).map_err(harness)?;
    promote.plant = plant;
    let mut slots = Slots::new(SLOTS, 64, Alloc::global()).map_err(harness)?;
    let mut writes: AVec<'_, Write> = Own::new_in(Alloc::global());
    // Every slot runs fast code first, its slot word naming the fast body.
    for at in 0..SLOTS {
        slots.publish(SlotId(at), Tier::Fast, fast(SlotId(at)), &mut writes).map_err(harness)?;
    }
    writes.clear();
    let mut hot: AVec<'_, SlotId> = Own::new_in(Alloc::global());
    let mut optimizing: Vec<SlotId> = Vec::new();
    let mut made = BTreeSet::new();
    let mut laid: BTreeSet<SlotId> = BTreeSet::new();
    let mut spans: Vec<(u64, u64)> = Vec::new();
    let mut placements: AVec<'_, Placement> = Own::new_in(Alloc::global());
    let steps = tape.range(1..120, "steps");
    let mut step = 0;
    while step < steps || !optimizing.is_empty() || !made.is_empty() {
        step += 1;
        if step > steps + 1000 {
            return Err(Broke::Oracle(format!("never settled: made {made:?}, optimizing {optimizing:?}")));
        }
        let action = if step < steps { tape.below(3, "action") } else if !optimizing.is_empty() { 1 } else { 2 };
        match action {
            0 => {
                let mut rows = Vec::new();
                let mut edges = Vec::new();
                for at in 0..SLOTS {
                    if tape.coin("sampled") {
                        rows.push(SlotSamples { slot: SlotId(at), samples: tape.below(12, "samples") as u32 });
                    }
                }
                for _ in 0..tape.below(4, "edges") {
                    let caller = SlotId(tape.below(u64::from(SLOTS), "caller") as u32);
                    let callee = SlotId(tape.below(u64::from(SLOTS), "callee") as u32);
                    edges.push(SlotEdge { caller, callee, samples: tape.below(12, "edge samples") as u32 });
                }
                promote.sampled(&rows, &edges, &mut hot).map_err(harness)?;
                for slot in hot.drain(..) {
                    let samples = promote.samples(slot).unwrap_or(0);
                    oracle(samples >= u64::from(BOUND), || {
                        format!("slot {} was made hot with {samples} samples, under the bound", slot.0)
                    })?;
                    oracle(!optimizing.contains(&slot) && !made.contains(&slot) && !laid.contains(&slot), || {
                        format!("slot {} was made hot twice: promoted twice", slot.0)
                    })?;
                    optimizing.push(slot);
                }
            }
            1 if !optimizing.is_empty() => {
                let slot = optimizing.remove(tape.index(optimizing.len(), "ended"));
                let len = 4 * tape.range(1..200, "words");
                promote.ready(slot, optimized(slot), len).map_err(harness)?;
                made.insert(slot);
            }
            2 => {
                promote.place(&mut placements).map_err(harness)?;
                for placement in placements.drain(..) {
                    let slot = placement.slot;
                    oracle(laid.insert(slot), || format!("slot {} was laid out again: promoted twice", slot.0))?;
                    oracle(made.remove(&slot), || format!("slot {} was laid out with no record made", slot.0))?;
                    let end = placement.at + placement.len;
                    oracle(placement.at % HOT.align == 0 && end <= HOT.hot_bytes, || {
                        format!("slot {} at {}..{end}: unaligned or past the region", slot.0, placement.at)
                    })?;
                    for &(low, high) in &spans {
                        oracle(end <= low || placement.at >= high, || {
                            format!("slot {} at {}..{end} overlaps {low}..{high}", slot.0, placement.at)
                        })?;
                    }
                    spans.push((placement.at, end));
                    slots.publish(slot, Tier::Optimized, placement.code, &mut writes).map_err(harness)?;
                    let stores: Vec<Write> =
                        writes.drain(..).filter(|write| matches!(write, Write::Slot { .. })).collect();
                    let one = match stores[..] {
                        [Write::Slot { slot: at, word }] => {
                            at == slot && matches!(word.names(), Names::Code { tier: Tier::Optimized, .. })
                        }
                        _ => false,
                    };
                    oracle(one, || format!("slot {}'s promotion stored {stores:?}, not one slot word", slot.0))?;
                }
            }
            _ => continue,
        }
    }
    oracle(made.is_empty(), || format!("records made and never laid out: {made:?}"))?;
    Ok(())
}

/// The invariant: a sampled hot slot is promoted into the hot region at most once, with aligned non-overlapping placement inside the region.
/// Prints `test sim::promote::a_sampled_hot_function_is_promoted_once_into_the_hot_region ... ok`.
#[test]
fn a_sampled_hot_function_is_promoted_once_into_the_hot_region() {
    check(Config::cases(2000), |tape: &Tape| promoted(tape, Plant::None));
}

/// The control (rule 53). Prints
/// `test sim::promote::the_oracle_catches_a_slot_promoted_twice ... ok` and, on standard error,
/// "plant PromoteTwice: caught at case k of 2000".
#[test]
fn the_oracle_catches_a_slot_promoted_twice() {
    control(Config::cases(2000), Plant::None, Plant::PromoteTwice, "promoted twice", promoted);
}

/// The hot region's order: a callee whose heaviest caller calls it for more than a tenth of its
/// weight follows that caller, whichever was made ready first; a slot with no such caller keeps
/// its own place by density. Prints
/// `test sim::promote::a_hot_callee_follows_its_caller_in_the_hot_region ... ok`.
#[test]
fn a_hot_callee_follows_its_caller_in_the_hot_region() {
    for callee_first in [true, false] {
        let mut promote = Promote::new(4, HeatConfig { bound: 1 }, HOT, Alloc::global()).expect("a promotion");
        let mut hot: AVec<'_, SlotId> = Own::new_in(Alloc::global());
        let rows = [
            SlotSamples { slot: SlotId(0), samples: 10 },
            SlotSamples { slot: SlotId(1), samples: 50 },
            SlotSamples { slot: SlotId(2), samples: 40 },
        ];
        let edges = [SlotEdge { caller: SlotId(0), callee: SlotId(1), samples: 30 }];
        promote.sampled(&rows, &edges, &mut hot).expect("sampled");
        let ready = |promote: &mut Promote<'_>, slot: u32| {
            promote.ready(SlotId(slot), optimized(SlotId(slot)), 64).expect("ready");
        };
        if callee_first {
            ready(&mut promote, 1);
            ready(&mut promote, 2);
            ready(&mut promote, 0);
        } else {
            ready(&mut promote, 0);
            ready(&mut promote, 2);
            ready(&mut promote, 1);
        }
        let mut out: AVec<'_, Placement> = Own::new_in(Alloc::global());
        promote.place(&mut out).expect("placed");
        let order: Vec<u32> = out.iter().map(|it| it.slot.0).collect();
        // Slot 1 (the densest, 50 over 64 bytes) joins its caller 0's cluster (a call of 30 is
        // more than a tenth of 50), which then holds 60 over 128 bytes; slot 2 alone holds 40 over
        // 64, denser, so it comes first, then the cluster of 0 and 1 in its merge order.
        assert_eq!(order, [2, 0, 1], "callee first: {callee_first}");
        let offsets: Vec<u64> = out.iter().map(|it| it.at).collect();
        assert_eq!(offsets, [0, 64, 128]);
    }
}
