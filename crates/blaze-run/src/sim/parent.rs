//! [`Parent`] against threads that need functions in any order, a store that has some records,
//! compiles that lack records and are entered again, lookups, compiles and makes that end in any
//! order, and compiles that fail.
//!
//! The oracle: one compile of a function is in flight at a time, and it is entered again only
//! once every record it lacked is made; each lacking record is made once; a function is published
//! once; every asker is answered, after the publish, and all askers of one function get the same
//! answer; a failed compile fails exactly its own callers with its typed error, and nothing stops
//! the run (rule 20); a waiting thread's function is never left idle while a worker is free (it
//! never waits behind speculation); the start is answered once.

use std::alloc::Global;
use std::collections::{BTreeMap, BTreeSet};

use blaze_sim::{Broke, Config, Tape, check, control};

use super::{harness, key, oracle};
use crate::parent::{Answer, Compiled, Config as ParentConfig, Effect, FastRecord, Found, Function, Parent};
use crate::{
    CodeId, CodeRef, FindingsRef, Hooks, InstanceKey, Nanos, Plant, RecordKey, SlotId, ThreadId,
    Version, Why,
};

const FUNCTIONS: u32 = 6;
const THREADS: u64 = 4;
const WORKERS: u32 = 2;
const KEYS: u64 = 3;

/// Function `n`'s code record: what the store holds or the compiler makes for it.
fn code_of(slot: SlotId) -> CodeRef {
    CodeRef(key(1000 + u64::from(slot.0)))
}

/// What function `n` calls: the next one, and the one after.
fn callees_of(slot: SlotId) -> [SlotId; 2] {
    [SlotId((slot.0 + 1) % FUNCTIONS), SlotId((slot.0 + 2) % FUNCTIONS)]
}

fn plain(slot: SlotId) -> Version {
    Version { slot, hooks: Hooks::NONE }
}

pub(super) fn functions() -> Vec<Function> {
    (0..FUNCTIONS)
        .map(|n| Function { instance: InstanceKey(key(u64::from(n))), code: CodeId(key(100 + u64::from(n))) })
        .collect()
}

/// The simulation's view of what is out, and what the oracle has seen.
#[derive(Default)]
struct World {
    lookups: Vec<SlotId>,
    compiles: Vec<SlotId>,
    makes: Vec<RecordKey>,
    made: BTreeSet<RecordKey>,
    /// What each blocked function's last compile lacked.
    blocked: BTreeMap<SlotId, Vec<RecordKey>>,
    waiting: BTreeMap<ThreadId, SlotId>,
    published: BTreeSet<SlotId>,
    answers: BTreeMap<SlotId, Answer>,
    starts: u32,
}

impl World {
    fn see(&mut self, effects: &mut Vec<Effect, Global>, failing: Option<SlotId>) -> Result<(), Broke> {
        for effect in effects.drain(..) {
            match effect {
                Effect::Lookup { version, .. } => self.lookups.push(version.slot),
                Effect::Compile { version, .. } => {
                    let slot = version.slot;
                    oracle(!self.compiles.contains(&slot), || format!("slot {} was compiled twice at once", slot.0))?;
                    if let Some(lacked) = self.blocked.remove(&slot) {
                        let early = lacked.iter().find(|it| !self.made.contains(*it));
                        oracle(early.is_none(), || {
                            format!("slot {}'s compile was re-entered before its records were made", slot.0)
                        })?;
                    }
                    self.compiles.push(slot);
                }
                Effect::Make { key, .. } => {
                    oracle(!self.made.contains(&key) && !self.makes.contains(&key), || {
                        format!("record {:?} was made twice", &key.0[..1])
                    })?;
                    self.makes.push(key);
                }
                Effect::Publish { slot, code, .. } => {
                    oracle(self.published.insert(slot), || format!("slot {} was published twice", slot.0))?;
                    oracle(code == code_of(slot), || format!("slot {} got another's code", slot.0))?;
                }
                Effect::Answer { thread, slot, answer } => {
                    oracle(self.waiting.remove(&thread) == Some(slot), || {
                        format!("thread {} was answered for slot {} it was not waiting on", thread.0, slot.0)
                    })?;
                    match answer {
                        Answer::Code { .. } => oracle(self.published.contains(&slot), || {
                            format!("thread {} was answered for slot {} before its publish", thread.0, slot.0)
                        })?,
                        Answer::Failed(error) => oracle(failing == Some(slot) && error.slot == slot, || {
                            format!("thread {} was failed for slot {}, which compiles", thread.0, slot.0)
                        })?,
                    }
                    let first = *self.answers.entry(slot).or_insert(answer);
                    oracle(first == answer, || format!("slot {}'s askers got two answers", slot.0))?;
                }
                Effect::Begin | Effect::NoStart(_) => self.starts += 1,
                Effect::Stop(error) => {
                    return Err(Broke::Oracle(format!("the run stopped at slot {}'s failed compile", error.slot.0)));
                }
                Effect::Untoggled { slot, .. } => {
                    return Err(Broke::Harness(format!("slot {} untoggled with no toggle", slot.0)));
                }
                Effect::Say(_) => {}
            }
        }
        Ok(())
    }

    /// No thread waits on a function nothing is doing while a worker is free.
    fn never_starved(&self) -> Result<(), Broke> {
        if self.compiles.len() >= WORKERS as usize {
            return Ok(());
        }
        for (thread, slot) in &self.waiting {
            let busy = self.lookups.contains(slot) || self.compiles.contains(slot) || self.blocked.contains_key(slot);
            oracle(busy, || {
                format!("thread {} waits on slot {}, which nothing is doing, with a worker free", thread.0, slot.0)
            })?;
        }
        Ok(())
    }
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let functions = functions();
    let config = ParentConfig { workers: WORKERS, compile_bound: Nanos(u64::MAX / 4), most_waiters: 64 };
    let mut parent = Parent::new(&functions, config, Global).map_err(harness)?;
    parent.plant = plant;
    let failing = tape.coin("a failure").then(|| SlotId(tape.below(u64::from(FUNCTIONS), "failing") as u32));
    let mut store: BTreeSet<SlotId> = (0..FUNCTIONS).filter(|_| tape.coin("in the store")).map(SlotId).collect();
    if let Some(failing) = failing {
        store.remove(&failing);
    }
    // What each function's compile reads that may not be made yet: shared, so makes coalesce.
    let lacks: Vec<Vec<RecordKey>> = (0..FUNCTIONS)
        .map(|_| (0..KEYS).filter(|_| tape.one_in(3, "lacks")).map(|n| RecordKey(key(50 + n))).collect())
        .collect();
    let mut world = World::default();
    let mut out = Vec::new_in(Global);
    let mut now = Nanos(0);
    let history: Vec<SlotId> = (0..FUNCTIONS).filter(|_| tape.one_in(3, "ran last time")).map(SlotId).collect();
    parent.start(&[SlotId(0)], &history, now, &mut out).map_err(harness)?;
    world.see(&mut out, failing)?;
    let steps = tape.range(1..120, "steps");
    let mut step = 0;
    // Past the drawn steps only deliveries, until nothing is out.
    while step < steps || !world.lookups.is_empty() || !world.compiles.is_empty() || !world.makes.is_empty() {
        step += 1;
        now = Nanos(now.0 + 1);
        let action = if step < steps { tape.below(5, "action") } else { 1 + step % 3 };
        match action {
            0 => {
                let thread = ThreadId(tape.below(THREADS, "thread"));
                if world.waiting.contains_key(&thread) {
                    continue;
                }
                let slot = SlotId(tape.below(u64::from(FUNCTIONS), "needed") as u32);
                world.waiting.insert(thread, slot);
                parent.need(thread, slot, now, &mut out).map_err(harness)?;
            }
            1 if !world.lookups.is_empty() => {
                let slot = world.lookups.remove(tape.index(world.lookups.len(), "lookup"));
                let callees = callees_of(slot);
                let found = store.contains(&slot).then(|| Found { fast: code_of(slot), callees: &callees, optimized: None });
                parent.looked(plain(slot), found, &mut out).map_err(harness)?;
            }
            2 if !world.compiles.is_empty() => {
                let slot = world.compiles.remove(tape.index(world.compiles.len(), "compile"));
                let mut callees = Vec::new_in(Global);
                callees.extend_from_slice(&callees_of(slot));
                let record = FastRecord { code: code_of(slot), callees, reads: Vec::new_in(Global) };
                let missing: Vec<RecordKey> =
                    lacks[slot.index()].iter().filter(|it| !world.made.contains(*it)).copied().collect();
                let ended = if failing == Some(slot) {
                    Compiled::Failed(Why::Refused(FindingsRef(key(7))))
                } else if !missing.is_empty() {
                    world.blocked.insert(slot, missing.clone());
                    Compiled::Need(&missing)
                } else {
                    store.insert(slot);
                    Compiled::Record(&record)
                };
                parent.compiled(plain(slot), ended, now, &mut out).map_err(harness)?;
            }
            3 if !world.makes.is_empty() => {
                let made = world.makes.remove(tape.index(world.makes.len(), "made"));
                world.made.insert(made);
                parent.made(made, Ok(()), &mut out).map_err(harness)?;
            }
            _ => continue,
        }
        world.see(&mut out, failing)?;
        world.never_starved()?;
    }
    oracle(world.waiting.is_empty(), || {
        format!("{} threads were never answered: {:?}", world.waiting.len(), world.waiting)
    })?;
    oracle(world.starts == 1, || format!("the start was answered {} times", world.starts))
}

#[test]
fn one_compile_one_answer_and_failures_fail_only_their_callers() {
    check(Config::cases(500), |tape| run(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_second_compile_of_one_function() {
    control(Config::cases(4000), Plant::None, Plant::CompileTwice, "compiled twice", run);
}

#[test]
fn the_oracle_catches_a_run_stopped_by_one_failure() {
    control(Config::cases(4000), Plant::None, Plant::StopOnFailure, "stopped", run);
}

#[test]
fn the_oracle_catches_a_compile_entered_before_what_it_lacked() {
    control(Config::cases(4000), Plant::None, Plant::ReenterEarly, "re-entered before", run);
}

/// A compile past its bound fails its askers with `Overran`; its late success still serves the
/// next caller.
#[test]
fn an_overrun_compile_fails_its_askers_and_a_late_success_serves_later_ones() {
    let functions = [Function { instance: InstanceKey(key(0)), code: CodeId(key(100)) }];
    let config = ParentConfig { workers: 2, compile_bound: Nanos(10), most_waiters: 8 };
    let mut parent = Parent::new(&functions, config, Global).expect("a parent");
    let mut out = Vec::new_in(Global);
    parent.start(&[], &[], Nanos(0), &mut out).expect("started");
    out.clear();
    parent.need(ThreadId(1), SlotId(0), Nanos(0), &mut out).expect("needed");
    parent.looked(plain(SlotId(0)), None, &mut out).expect("looked");
    out.clear();
    parent.tick(Nanos(11), &mut out);
    let failed = out.iter().any(|effect| {
        matches!(effect, Effect::Answer { answer: Answer::Failed(error), .. }
            if matches!(error.why, Why::Overran { .. }))
    });
    assert!(failed, "the asker was not failed past the bound: {out:?}");
    out.clear();
    let record = FastRecord { code: CodeRef(key(5)), callees: Vec::new_in(Global), reads: Vec::new_in(Global) };
    parent.compiled(plain(SlotId(0)), Compiled::Record(&record), Nanos(12), &mut out).expect("a late end");
    assert!(out.iter().any(|effect| matches!(effect, Effect::Publish { .. })), "{out:?}");
    out.clear();
    parent.need(ThreadId(2), SlotId(0), Nanos(13), &mut out).expect("needed again");
    assert!(matches!(out.as_slice(), [Effect::Answer { answer: Answer::Code { .. }, .. }]), "{out:?}");
}
