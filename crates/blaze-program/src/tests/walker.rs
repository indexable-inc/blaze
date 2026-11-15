//! The driver's walker ([`crate::walker`]) held to today's walk's oracle: a model program is
//! built whole, then edited (a body, a call added or dropped, a function removed or brought
//! back), and after every edit the delta walk from the kept rows and the moved instances,
//! with its answers in an order drawn from the tape, leaves a kept manifest equal to a whole
//! walk's of the same revision: every row's code, plan result and targets. The driver is
//! modelled as the seam's protocol says (`blaze_keyed::program`), its diff by the seam's own
//! `changes`. A kept instance is asked with no bytes and answered from its kept row's ask, as
//! the driver does; the last functions are statics, which a code ask and a kept row give apart
//! from the callees. Each plan names span tables (one always, a second at an odd version, an
//! anchor given twice and out of order), and every code ask must carry exactly them, sorted,
//! each once; the kept row keeps them, so a delta's equal a whole walk's. A planted walker that
//! takes nothing as moved is caught, one that leaves a span table out of a code ask is caught,
//! and a one-body edit asks the plan and code of that body alone.

use std::collections::{BTreeMap, BTreeSet};

use blaze_ids::ContentId;
use blaze_keyed::driver::TaskFailed;
use blaze_keyed::kind::{CodeRecord, Key, PlanRecord};
use blaze_keyed::program::{Kept, Planned, RowChange, SpanAt, WalkAsk, Walker, changes};
use blaze_keyed::record::{KeptRow, StableId};
use blaze_sim::{Config, Tape};

use super::world::settings;
use crate::Plant;
use crate::codec::Digest;
use crate::key::Tier;
use crate::walker::{Edge, ProgramWalker, Reader, Root, Target};

const FNS: u8 = 8;

/// The instances from this index on are statics: an edge to one reads its address.
const FIRST_STATIC: u8 = 6;

#[derive(Clone, Debug)]
struct ModelFn {
    version: u32,
    calls: Vec<u8>,
    alive: bool,
}

/// The program: function 0 is the root; a call is an edge (to a function or a static alike).
#[derive(Clone, Debug)]
struct Model {
    fns: Vec<ModelFn>,
}

fn instance(index: u8) -> ContentId {
    Digest::of("blaze program walker test instance 1", &[index])
}

fn index_of(id: &ContentId) -> Option<u8> {
    (0..FNS).find(|index| instance(*index) == *id)
}

/// The span table `table` of function `index`, as the reader keys it: owned by the function.
fn span_at(index: u8, table: u8) -> SpanAt {
    let anchor = Digest::of("blaze program walker test anchor 1", &[index, table]);
    let key = Key::from_id(Digest::of("blaze program walker test span table 1", &anchor.0));
    SpanAt { anchor, key, owner: StableId(instance(index)) }
}

/// The span tables the plan of `index` at `version` names, sorted by anchor, each once: table
/// 0 always, table 1 at an odd version (so an edit adds or drops one).
fn spans_of(index: u8, version: u32) -> Vec<SpanAt> {
    let mut spans = vec![span_at(index, 0)];
    if version % 2 == 1 {
        spans.push(span_at(index, 1));
    }
    spans.sort_unstable_by_key(|span| span.anchor);
    spans
}

impl Model {
    fn generate(tape: &Tape) -> Model {
        let mut fns = Vec::new();
        for index in 0..FNS {
            let mut calls = Vec::new();
            for _ in 0..tape.below(3, "calls") {
                let callee = tape.below(u64::from(FNS), "callee") as u8;
                if callee != index && !calls.contains(&callee) {
                    calls.push(callee);
                }
            }
            fns.push(ModelFn { version: 0, calls, alive: true });
        }
        Model { fns }
    }

    /// The plan's payload: the version, then each callee's index.
    fn payload(&self, index: u8) -> Vec<u8> {
        let function = &self.fns[index as usize];
        let mut out = function.version.to_le_bytes().to_vec();
        out.extend_from_slice(&function.calls);
        out
    }

    fn result(&self, index: u8) -> ContentId {
        Digest::of("blaze program walker test plan 1", &self.payload(index))
    }

    fn alive(&self) -> Vec<u8> {
        (0..FNS).filter(|index| self.fns[*index as usize].alive).collect()
    }

    fn edit(&mut self, tape: &Tape) {
        let alive = self.alive();
        let pick = alive[tape.index(alive.len(), "edited function")] as usize;
        match tape.below(5, "edit") {
            0 => self.fns[pick].version += 1,
            1 => {
                let callee = alive[tape.index(alive.len(), "new callee")];
                if callee as usize != pick && !self.fns[pick].calls.contains(&callee) {
                    self.fns[pick].calls.push(callee);
                    self.fns[pick].version += 1;
                }
            }
            2 => {
                if !self.fns[pick].calls.is_empty() {
                    let at = tape.index(self.fns[pick].calls.len(), "dropped call");
                    self.fns[pick].calls.remove(at);
                    self.fns[pick].version += 1;
                }
            }
            3 if pick != 0 => {
                self.fns[pick].alive = false;
                for function in &mut self.fns {
                    if let Some(at) = function.calls.iter().position(|callee| *callee as usize == pick) {
                        function.calls.remove(at);
                        function.version += 1;
                    }
                }
            }
            _ => {
                let dead: Vec<u8> = (0..FNS).filter(|index| !self.fns[*index as usize].alive).collect();
                if !dead.is_empty() {
                    let back = dead[tape.index(dead.len(), "brought back")];
                    self.fns[back as usize] = ModelFn { version: self.fns[back as usize].version + 1, calls: Vec::new(), alive: true };
                    self.fns[pick].calls.push(back);
                    self.fns[pick].version += 1;
                }
            }
        }
    }
}

/// The front end's reader of the model's plans.
struct ModelReader;

impl Reader for ModelReader {
    fn edges<E: Extend<Edge>, B: Extend<u8>>(
        &self,
        _instance: Key<PlanRecord>,
        payload: &[u8],
        edges: &mut E,
        asks: &mut B,
    ) -> Result<(), TaskFailed> {
        let Some(calls) = payload.get(4..) else {
            return Err(TaskFailed::Refused { what: "a model plan shorter than its version" });
        };
        for callee in calls {
            let kind = if *callee >= FIRST_STATIC { Target::Static } else { Target::Call };
            edges.extend([Edge { instance: Key::from_id(instance(*callee)), kind, body: None, ask_len: 1 }]);
            asks.extend([*callee]);
        }
        Ok(())
    }

    /// The code is the instance, its tier and its own plan: never a callee's key.
    fn code_key(
        &self,
        instance: Key<PlanRecord>,
        tier: Tier,
        plan: &[u8],
        _targets: &[&[u8]],
    ) -> Result<Key<CodeRecord>, TaskFailed> {
        let mut bytes = instance.id().0.to_vec();
        bytes.extend_from_slice(&tier.number().to_le_bytes());
        bytes.extend_from_slice(plan);
        Ok(Key::from_id(Digest::of("blaze program walker test code 1", &bytes)))
    }

    /// The plan's tables off its instance and version, given out of order and each twice, as
    /// a plan naming one anchor from two places would: the walker sorts and keeps each once.
    fn spans<E: Extend<SpanAt>>(
        &self,
        instance: Key<PlanRecord>,
        plan: &[u8],
        spans: &mut E,
    ) -> Result<(), TaskFailed> {
        let (Some(index), Some(version)) = (index_of(&instance.id()), plan.get(..4)) else {
            return Err(TaskFailed::Refused { what: "a model plan of no function, or shorter than its version" });
        };
        let version = u32::from_le_bytes([version[0], version[1], version[2], version[3]]);
        let tables = spans_of(index, version);
        spans.extend(tables.iter().rev().copied());
        spans.extend(tables.iter().copied());
        Ok(())
    }
}

/// A kept row as the driver keeps it.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Entry {
    result: ContentId,
    code: Key<CodeRecord>,
    tier: u32,
    callees: Vec<ContentId>,
    statics: Vec<ContentId>,
    /// The span tables its code ask named.
    spans: Vec<SpanAt>,
    /// The ask its plan task was given.
    ask: Vec<u8>,
}

type Manifest = BTreeMap<ContentId, Entry>;

/// An ask, copied out of the walker.
enum Owned {
    Plan {
        ask: u32,
        instance: Key<PlanRecord>,
        bytes: Option<Vec<u8>>,
    },
    Code {
        ask: u32,
        instance: Key<PlanRecord>,
        key: Key<CodeRecord>,
        tier: u32,
        callees: Vec<ContentId>,
        statics: Vec<ContentId>,
        spans: Vec<SpanAt>,
    },
}

/// What one walk did.
struct Driven {
    manifest: Manifest,
    changes: Vec<RowChange>,
    planned: BTreeSet<u8>,
    coded: BTreeSet<u8>,
}

fn failed(what: &str, error: TaskFailed) -> String {
    format!("{what}: {error:?}")
}

/// One request: the walker begun over `kept` with the instances whose plan result moved,
/// driven to its end with answers in the tape's order, its rows diffed by the seam's `changes`
/// and applied to the kept manifest.
fn drive(model: &Model, kept: &Manifest, plant: Plant, tape: &Tape) -> Result<Driven, String> {
    let mut rows = Vec::new();
    let mut targets = Vec::new();
    let mut asks = Vec::new();
    for (id, entry) in kept {
        rows.push(KeptRow {
            instance: *id,
            item: *id,
            plan: *id,
            result: entry.result,
            code: entry.code.id(),
            shape_at: 0,
            shape_len: 0,
            target_at: targets.len() as u32,
            target_len: (entry.callees.len() + entry.statics.len()) as u32,
            call_len: entry.callees.len() as u32,
            tier: entry.tier,
            ask_at: asks.len() as u32,
            ask_len: entry.ask.len() as u32,
        });
        targets.extend_from_slice(&entry.callees);
        targets.extend_from_slice(&entry.statics);
        asks.extend_from_slice(&entry.ask);
    }
    let kept_view = Kept { rows: &rows, targets: &targets, asks: &asks };
    let mut moved = Vec::new();
    for (id, entry) in kept {
        let Some(index) = index_of(id) else { return Err("a kept row of no model function".to_string()) };
        if model.fns[index as usize].alive && model.result(index) != entry.result {
            moved.push(Key::<PlanRecord>::from_id(*id));
        }
    }

    let mut walker = ProgramWalker::new(ModelReader, settings(), 1_000_000, plant);
    let root_ask = [0u8];
    let program = walker.add_program(&[Root { instance: Key::from_id(instance(0)), ask: &root_ask, body: None }]);
    walker.begin(program, kept_view, &moved, &[]).map_err(|error| failed("begin", error))?;
    let mut out = Vec::new();
    let (mut asked_with, mut coded_with) = (BTreeMap::new(), BTreeMap::new());
    let (mut planned, mut coded) = (BTreeSet::new(), BTreeSet::new());
    let ids = |keys: &[Key<PlanRecord>]| keys.iter().map(|key| key.id()).collect::<Vec<_>>();
    for _ in 0..100_000 {
        while let Some(ask) = walker.ask(program) {
            out.push(match ask {
                WalkAsk::Plan { ask, instance, bytes, .. } => Owned::Plan { ask, instance, bytes: bytes.map(<[u8]>::to_vec) },
                WalkAsk::Code { ask, instance, key, tier, callees, statics, spans } => Owned::Code {
                    ask,
                    instance,
                    key,
                    tier,
                    callees: ids(callees),
                    statics: ids(statics),
                    spans: spans.to_vec(),
                },
            });
        }
        if out.is_empty() {
            break;
        }
        match out.swap_remove(tape.index(out.len(), "answered next")) {
            Owned::Plan { ask, instance: asked, bytes } => {
                // No bytes: the kept row's ask, as the driver gives it; refused for an
                // instance no kept row holds.
                let bytes = match bytes {
                    Some(bytes) => {
                        if kept_view.row(&asked.id()).is_some() {
                            return Err("a kept instance asked with bytes, not by its kept ask".to_string());
                        }
                        bytes
                    }
                    None => match kept_view.row(&asked.id()) {
                        Some(row) => kept_view.ask_of(row).to_vec(),
                        None => return Err("a plan asked with no bytes of an instance no kept row holds".to_string()),
                    },
                };
                let index = match bytes[..] {
                    [index] if index < FNS && instance(index) == asked.id() => index,
                    _ => return Err("a plan asked with an ask that is not its instance's".to_string()),
                };
                if !model.fns[index as usize].alive {
                    return Err(format!("the plan of removed function {index} was asked"));
                }
                planned.insert(index);
                asked_with.insert(asked.id(), bytes);
                let payload = model.payload(index);
                let plan = Planned { instance: asked, result: model.result(index), payload: &payload };
                walker.planned(program, ask, plan).map_err(|error| failed("planned", error))?;
            }
            Owned::Code { ask, instance: asked, key, tier, callees, statics, spans } => {
                let Some(index) = index_of(&asked.id()) else { return Err("code asked of no instance".to_string()) };
                if !coded.insert(index) {
                    return Err(format!("the code of {index} was asked twice"));
                }
                let split = callees.iter().all(|id| index_of(id).is_some_and(|it| it < FIRST_STATIC))
                    && statics.iter().all(|id| index_of(id).is_some_and(|it| it >= FIRST_STATIC));
                if !split {
                    return Err(format!("the code ask of {index} gives a static as a callee or a callee as a static"));
                }
                let named = spans_of(index, model.fns[index as usize].version);
                if spans != named {
                    return Err(format!("function {index}: its code ask names the span tables {spans:?}, its plan {named:?}"));
                }
                coded_with.insert(asked.id(), (tier, callees, statics, spans));
                walker.coded(program, ask, key).map_err(|error| failed("coded", error))?;
            }
        }
    }
    let end = walker.end(program).map_err(|error| failed("end", error))?;
    let mut changed = Vec::new();
    changes(kept_view, end.rows, end.gone, &mut changed);
    let mut manifest = kept.clone();
    for gone in end.gone {
        manifest.remove(&gone.id());
    }
    for row in end.rows {
        let id = row.instance.id();
        let (Some(index), Some((tier, callees, statics, spans)), Some(ask)) =
            (index_of(&id), coded_with.get(&id), asked_with.get(&id))
        else {
            return Err("a row the walk gave whose plan or code was not asked".to_string());
        };
        let entry = Entry {
            result: model.result(index),
            code: row.code,
            tier: *tier,
            callees: callees.clone(),
            statics: statics.clone(),
            spans: spans.clone(),
            ask: ask.clone(),
        };
        manifest.insert(id, entry);
    }
    Ok(Driven { manifest, changes: changed, planned, coded })
}

/// The first row two manifests differ in, by function.
fn difference(delta: &Manifest, fresh: &Manifest) -> Option<String> {
    for index in 0..FNS {
        let id = instance(index);
        let (a, b) = (delta.get(&id), fresh.get(&id));
        if a != b {
            return Some(format!("function {index}: the delta keeps {a:?}, a whole walk {b:?}"));
        }
    }
    None
}

/// One edit, walked as a delta and checked against a whole walk; the delta's manifest kept.
fn step(model: &Model, kept: &Manifest, plant: Plant, tape: &Tape) -> Result<Driven, String> {
    let delta = drive(model, kept, plant, tape)?;
    let fresh = drive(model, &Manifest::new(), Plant::None, tape)?;
    if let Some(why) = difference(&delta.manifest, &fresh.manifest) {
        return Err(why);
    }
    for change in &delta.changes {
        if change.was.is_some() != kept.contains_key(&change.instance.id()) {
            return Err(format!("{change:?} does not match the kept rows"));
        }
    }
    Ok(delta)
}

fn run(tape: &Tape, steps: usize, plant: Plant) -> Result<(), String> {
    let mut model = Model::generate(tape);
    let mut kept = drive(&model, &Manifest::new(), plant, tape).map_err(|why| format!("first build: {why}"))?.manifest;
    for at in 0..steps {
        model.edit(tape);
        kept = step(&model, &kept, plant, tape).map_err(|why| format!("step {at}: {why}"))?.manifest;
    }
    Ok(())
}

/// The oracle holds over many programs, edits and answer orders.
#[test]
fn delta_walks_keep_a_whole_walks_rows() {
    blaze_sim::check(Config::cases(64), |tape| run(tape, 10, Plant::None));
}

/// `0 → 1 → 2` and `0 → 3`, built whole.
fn chain() -> Model {
    let function = |calls: &[u8]| ModelFn { version: 0, calls: calls.to_vec(), alive: true };
    let mut fns = vec![function(&[1, 3]), function(&[2]), function(&[]), function(&[])];
    fns.extend((4..FNS).map(|_| ModelFn { version: 0, calls: Vec::new(), alive: false }));
    Model { fns }
}

/// The floor: one body edit asks the plan and the code of that instance alone. Its caller is
/// not walked again (no code takes a callee in), and its kept ask names it without a caller's
/// plan.
#[test]
fn a_body_edit_asks_only_its_body() {
    let tape = Tape::replay(&[]);
    let mut model = chain();
    let kept = drive(&model, &Manifest::new(), Plant::None, &tape).unwrap().manifest;
    model.fns[2].version += 1;
    let delta = step(&model, &kept, Plant::None, &tape).unwrap();
    assert_eq!(delta.planned, BTreeSet::from([2]), "planned beyond the edit");
    assert_eq!(delta.coded, BTreeSet::from([2]), "coded beyond the edit");
    assert_eq!(delta.changes.len(), 1, "{:?}", delta.changes);
}

/// `0 → 1 → 6`, `6` a static: a code ask and the kept row give it as a static, and an edit of
/// the static asks its plan and code alone.
#[test]
fn a_static_is_given_as_a_static() {
    let tape = Tape::replay(&[]);
    let function = |calls: &[u8], alive: bool| ModelFn { version: 0, calls: calls.to_vec(), alive };
    let mut fns: Vec<ModelFn> = (0..FNS).map(|_| function(&[], false)).collect();
    fns[0] = function(&[1], true);
    fns[1] = function(&[6], true);
    fns[6] = function(&[], true);
    let mut model = Model { fns };
    let kept = drive(&model, &Manifest::new(), Plant::None, &tape).unwrap().manifest;
    let row = &kept[&instance(1)];
    assert!(row.callees.is_empty() && row.statics == [instance(6)], "{row:?}");
    assert_eq!(row.tier, Tier::Edit.number());
    model.fns[6].version += 1;
    let delta = step(&model, &kept, Plant::None, &tape).unwrap();
    assert_eq!((delta.planned, delta.coded), (BTreeSet::from([6]), BTreeSet::from([6])));
}

/// The control, by its cause: a walker that takes nothing as moved keeps 2's stale row.
#[test]
fn a_delta_that_keeps_a_stale_row_is_caught() {
    let tape = Tape::replay(&[]);
    let mut model = chain();
    let kept = drive(&model, &Manifest::new(), Plant::None, &tape).unwrap().manifest;
    model.fns[2].version += 1;
    let error = match step(&model, &kept, Plant::DeltaKeepsStale, &tape) {
        Ok(_) => panic!("a delta that kept a stale row matched a whole walk"),
        Err(error) => error,
    };
    assert!(error.contains("function 2"), "{error}");
}

/// The control for span tables, by its cause: a walker whose code ask leaves its last table
/// out is caught at 2's code ask (2 at version 1 names two tables).
#[test]
fn a_code_ask_that_leaves_a_span_table_out_is_caught() {
    let tape = Tape::replay(&[]);
    let mut model = chain();
    let kept = drive(&model, &Manifest::new(), Plant::None, &tape).unwrap().manifest;
    assert_eq!(kept[&instance(2)].spans, spans_of(2, 0));
    model.fns[2].version += 1;
    assert_eq!(spans_of(2, 1).len(), 2);
    let error = match step(&model, &kept, Plant::SpanLeftOut, &tape) {
        Ok(_) => panic!("a code ask that left a span table out was taken"),
        Err(error) => error,
    };
    assert!(error.contains("function 2") && error.contains("span tables"), "{error}");
}
