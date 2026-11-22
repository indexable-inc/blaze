//! [`Heat`] and [`Tier`] together, and the optimized record on a later run's load.
//!
//! The oracle of the first: a function is optimized exactly once when its count stub reaches
//! zero (however often it fires, and however late the events come), and never otherwise; a job
//! begun before a newer
//! snapshot is never filed or published. Of the second: an optimized record whose inlined
//! callee changed is never published, and the fast record is; one whose callees hold is.

use std::collections::{BTreeMap, BTreeSet};

use blaze_own::{AVec, Alloc, Vec as Own};
use blaze_sim::{Broke, Config, Tape, check, control};

use super::{harness, key, oracle};
use crate::heat::{Config as HeatConfig, Heat};
use crate::parent::{Config as ParentConfig, Effect as ParentEffect, Found, FoundOptimized, Function, Parent};
use crate::tier::{self, Callee, Inlined, Job, Optimized, Optimizer};
use crate::{CodeId, CodeRef, InstanceKey, Made, Nanos, Plant, RecordKey, SlotId, Snapshot, Tier, Why};

const SLOTS: u32 = 4;

fn instance(slot: SlotId) -> InstanceKey {
    InstanceKey(key(u64::from(slot.0)))
}

/// Gives each slot made hot to the tier; a second start of one is the oracle's finding.
fn give(hot: &mut AVec<'_, SlotId>, tier: &mut tier::Tier<'_>, effects: &mut AVec<'_, tier::Effect>) -> Result<(), Broke> {
    for slot in hot.drain(..) {
        match tier.hot(slot, instance(slot), effects) {
            Ok(()) => {}
            Err(tier::Error::Twice { slot }) => {
                return Err(Broke::Oracle(format!("slot {} was tiered twice", slot.0)));
            }
            Err(error) => return Err(harness(error)),
        }
    }
    Ok(())
}

fn heated(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut heat = Heat::new(SLOTS, HeatConfig { bound: 40 }, Alloc::global()).map_err(harness)?;
    heat.plant = plant;
    let mut tier = tier::Tier::new(SLOTS, 2, Snapshot(0), Alloc::global()).map_err(harness)?;
    // Each count stub's counter, as the stub counts it down, while its stub is in.
    let mut counters: [Option<u32>; SLOTS as usize] = [Some(heat.bound()); SLOTS as usize];
    let mut crossed = BTreeSet::new();
    let mut hot = Own::new_in(Alloc::global());
    let mut effects = Own::new_in(Alloc::global());
    let mut running: Vec<Job> = Vec::new();
    let mut optimized: BTreeMap<SlotId, u32> = BTreeMap::new();
    let mut snapshot = Snapshot(0);
    // Jobs whose optimizer lacked a record, and the records being made.
    let mut lacked = BTreeSet::new();
    let mut blocked = BTreeSet::new();
    let mut makes: Vec<RecordKey> = Vec::new();
    let steps = tape.range(1..200, "steps");
    let mut step = 0;
    while step < steps || !running.is_empty() || !makes.is_empty() {
        step += 1;
        let action = if step < steps { tape.below(6, "action") } else { 2 + 3 * (step % 2) };
        match action {
            0 | 1 => {
                let slot = SlotId(tape.below(u64::from(SLOTS), "slot") as u32);
                let by = tape.below(30, "calls") as u32;
                let Some(left) = counters[slot.index()] else { continue };
                if by < left {
                    counters[slot.index()] = Some(left - by);
                    continue;
                }
                // The stub reached zero; another thread's call may fire it again before the
                // control thread turns it off.
                crossed.insert(slot);
                heat.reached(slot, &mut hot).map_err(harness)?;
                if tape.coin("fired again") {
                    heat.reached(slot, &mut hot).map_err(harness)?;
                }
                if !heat.counted(slot) {
                    counters[slot.index()] = None;
                }
                give(&mut hot, &mut tier, &mut effects)?;
            }
            2 if !running.is_empty() => {
                let job = running.remove(tape.index(running.len(), "ended"));
                let mut inlined = Own::new_in(Alloc::global());
                inlined.push(Inlined { instance: InstanceKey(key(99)), code: CodeId(key(98)) });
                let lacks = [RecordKey(key(60 + u64::from(job.slot.0)))];
                let ended = if !lacked.contains(&job.slot) && tape.coin("lacks a record") {
                    lacked.insert(job.slot);
                    blocked.insert(job.slot);
                    tier::Ended::Need(&lacks)
                } else {
                    tier::Ended::Record(Optimized { code: CodeRef(key(500 + u64::from(job.slot.0))), inlined })
                };
                let stale = job.snapshot < snapshot;
                tier.done(job, ended, &mut effects).map_err(harness)?;
                for effect in &effects {
                    if let tier::Effect::Publish { slot, .. } | tier::Effect::File { slot } = effect {
                        oracle(!stale, || {
                            format!("slot {}'s job of an older snapshot was filed or published", slot.0)
                        })?;
                    }
                }
            }
            // An event queued before the stub was off, handed over late.
            3 if !crossed.is_empty() => {
                let late: Vec<SlotId> = crossed.iter().copied().collect();
                heat.reached(late[tape.index(late.len(), "late")], &mut hot).map_err(harness)?;
                give(&mut hot, &mut tier, &mut effects)?;
            }
            4 if tape.one_in(8, "a newer snapshot") => {
                snapshot = Snapshot(snapshot.0 + 1);
                tier.newer(snapshot).map_err(harness)?;
            }
            5 if !makes.is_empty() => {
                let made = makes.remove(tape.index(makes.len(), "made"));
                tier.made(made, Ok(()), &mut effects).map_err(harness)?;
            }
            _ => continue,
        }
        for effect in effects.drain(..) {
            match effect {
                tier::Effect::Make { key } => makes.push(key),
                // Entered again once what it lacked was made: the same job, not a second.
                tier::Effect::Optimize(job) if blocked.remove(&job.slot) => running.push(job),
                tier::Effect::Optimize(job) => {
                    let count = optimized.entry(job.slot).or_insert(0);
                    *count += 1;
                    let count = *count;
                    oracle(count == 1, || format!("slot {} was tiered twice", job.slot.0))?;
                    running.push(job);
                }
                tier::Effect::File { .. }
                | tier::Effect::Publish { .. }
                | tier::Effect::Failed { .. }
                | tier::Effect::Dropped { .. } => {}
            }
        }
    }
    // Every function the model saw cross the bound was given to the tier; no other was. Under a
    // newer snapshot a queued one may be dropped before it starts, so only the order is held then.
    for slot in 0..SLOTS {
        let slot = SlotId(slot);
        let started = optimized.contains_key(&slot);
        oracle(!started || crossed.contains(&slot), || {
            format!("slot {} was tiered without reaching the bound", slot.0)
        })?;
        oracle(snapshot.0 > 0 || started == crossed.contains(&slot), || {
            format!("slot {} reached the bound and was never tiered", slot.0)
        })?;
    }
    Ok(())
}

/// The invariant: a hot function is queued for optimization exactly once when its heat reaches the threshold, and an optimized record is used only when its inlined callees still match.
#[test]
fn a_hot_function_is_tiered_exactly_once() {
    check(Config::cases(500), |tape| heated(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_function_queued_twice() {
    control(Config::cases(4000), Plant::None, Plant::HotTwice, "tiered twice", heated);
}

/// An optimizer that inlines every callee it is given, as the identities it was told.
struct InlinesAll;

impl Optimizer for InlinesAll {
    fn optimize<'a>(
        &mut self,
        instance: InstanceKey,
        callees: &[Callee],
        alloc: Alloc<'a>,
    ) -> Result<Made<'a, Optimized<'a>>, Why> {
        let mut inlined = Own::with_capacity_in(callees.len(), alloc);
        inlined.extend(callees.iter().map(|callee| Inlined { instance: callee.instance, code: callee.code }));
        let mut code = instance.0;
        code[31] = 0xee;
        Ok(Made::Done(Optimized { code: CodeRef(code), inlined }))
    }
}

/// Run one tiers `f`, which calls `g` and `h`; run two loads `f` with `g` and `h` edited or not.
fn reloaded(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let (f, g, h) = (SlotId(0), SlotId(1), SlotId(2));
    let first = [
        Function { instance: instance(f), code: CodeId(key(10)) },
        Function { instance: instance(g), code: CodeId(key(11)) },
        Function { instance: instance(h), code: CodeId(key(12)) },
    ];
    // Run one: f is hot; the optimizer absorbs g and h at their identities of run one.
    let mut tier = tier::Tier::new(3, 1, Snapshot(0), Alloc::global()).map_err(harness)?;
    let mut effects = Own::new_in(Alloc::global());
    tier.hot(f, instance(f), &mut effects).map_err(harness)?;
    let Some(tier::Effect::Optimize(job)) = effects.pop() else {
        return Err(Broke::Harness("the tier did not start f".into()));
    };
    let callees = [
        Callee { instance: first[1].instance, code: first[1].code },
        Callee { instance: first[2].instance, code: first[2].code },
    ];
    let ended = match InlinesAll.optimize(job.instance, &callees, Alloc::global()) {
        Ok(Made::Done(optimized)) => tier::Ended::Record(optimized),
        Ok(Made::Need(_)) => return Err(Broke::Harness("InlinesAll lacks nothing".into())),
        Err(why) => tier::Ended::Failed(why),
    };
    tier.done(job, ended, &mut effects).map_err(harness)?;
    let record = tier.record(f).ok_or_else(|| Broke::Harness("f was not filed".into()))?;
    oracle(tier.callers_of(first[1].code) == [f], || "the reverse map misses g's caller".into())?;
    // Run two: g, h, both or neither edited.
    let mut second = first;
    let g_edited = tape.coin("g edited");
    let h_edited = tape.coin("h edited");
    if g_edited {
        second[1].code = CodeId(key(21));
    }
    if h_edited {
        second[2].code = CodeId(key(22));
    }
    let config = ParentConfig { workers: 2, compile_bound: Nanos(1 << 40), most_waiters: 8 };
    let mut parent = Parent::new(&second, config, Alloc::global()).map_err(harness)?;
    parent.plant = plant;
    let mut out = Own::new_in(Alloc::global());
    parent.start(&[f], &[], Nanos(0), &mut out).map_err(harness)?;
    out.clear();
    let fast = CodeRef(key(700));
    let found = Found {
        fast,
        callees: &[],
        optimized: Some(FoundOptimized { code: record.code, inlined: &record.inlined }),
    };
    parent.looked(f, Some(found), &mut out).map_err(harness)?;
    let published = out.iter().find_map(|effect| match effect {
        ParentEffect::Publish { slot, tier, code, .. } if *slot == f => Some((*tier, *code)),
        _ => None,
    });
    let stale = g_edited || h_edited;
    let want = if stale { (Tier::Fast, fast) } else { (Tier::Optimized, record.code) };
    oracle(published == Some(want), || {
        format!(
            "f ran {published:?} with g edited {g_edited}, h edited {h_edited}: a stale optimized record used, or a good one not"
        )
    })?;
    oracle(tier::usable(&record.inlined, &parent).is_ok() != stale, || "usable disagrees".into())
}

#[test]
fn an_optimized_record_with_a_changed_callee_is_not_used() {
    check(Config::cases(64), |tape| reloaded(tape, Plant::None));
}

#[test]
fn the_oracle_catches_an_optimized_record_trusted_unchecked() {
    control(Config::cases(64), Plant::None, Plant::TrustOptimized, "stale optimized", reloaded);
}
