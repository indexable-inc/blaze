//! The engine's history simulations (engine.rs, invariants 1 to 8): a model workspace of crates,
//! files and items is edited request after request, under scopes that grow and shrink, with a
//! failing body now and then and records made inline; each request's answer is held to a fresh
//! build of the same text and scope, and what it entered to the floor.

use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_drive::cells::Cell;
use blaze_drive::key::{self, Key, Kind, Name, Tool};
use blaze_id::ContentId;
use blaze_own::{SharedAlloc, Vec};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::engine::{
    Answer, Buffer, CellId, Done, Edit, Engine, EngineError, Finished, Made, Phase, Plant, Planned, Pool, SVec, Scope, Stage, Task, View, drive,
};

const TOOL: Tool = Tool(ContentId([7; 32]));

fn alloc() -> SharedAlloc<'static> {
    blaze_own::testing::heap().shared()
}

fn oracle(holds: bool, why: impl FnOnce() -> String) -> Result<(), Broke> {
    match holds {
        true => Ok(()),
        false => Err(Broke::Oracle(why())),
    }
}

fn d(what: &'static str, parts: &[u64]) -> ContentId {
    let mut bytes = StdVec::with_capacity(parts.len() * 8);
    for part in parts {
        bytes.extend_from_slice(&part.to_le_bytes());
    }
    key::digest(what, &bytes)
}

fn idd(id: &ContentId) -> u64 {
    u64::from_le_bytes([id.0[0], id.0[1], id.0[2], id.0[3], id.0[4], id.0[5], id.0[6], id.0[7]])
}

fn cell(tag: u8, id: u32) -> Cell {
    let mut bytes = [0u8; 16];
    bytes[0] = tag;
    bytes[1..5].copy_from_slice(&id.to_le_bytes());
    Cell(bytes)
}

const LIST: u8 = 0;
const SIG: u8 = 1;
const BODY: u8 = 2;
const DECLARED: u8 = 3;
const HEAD: u8 = 5;
const VALUE: u8 = 7;
const ALL: u8 = 9;
/// A body text whose check fails.
const POISON: u32 = 13;

fn outline(f: usize) -> Name {
    Name::new(Kind::FileItems, d("sim engine file", &[f as u64]))
}
fn manifest(c: usize) -> Name {
    Name::new(Kind::Units, d("sim engine manifest", &[c as u64]))
}
fn names(c: usize) -> Name {
    Name::new(Kind::CrateScope, d("sim engine names", &[c as u64]))
}
fn head(id: u32) -> Name {
    Name::new(Kind::Signature, d("sim engine head", &[u64::from(id)]))
}
fn body(id: u32) -> Name {
    Name::new(Kind::BodyCheck, d("sim engine body", &[u64::from(id)]))
}
fn konst(v: u32) -> Name {
    Name::new(Kind::Code, d("sim engine const", &[u64::from(v)]))
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Item {
    id: u32,
    sig: u32,
    body: u32,
    refs: StdVec<u32>,
}

#[derive(Clone, Debug)]
struct World {
    /// Each crate's dependencies, all of lower index.
    crates: StdVec<StdVec<usize>>,
    /// Each file's crate and items.
    files: StdVec<(usize, StdVec<Item>)>,
    next_id: u32,
}

impl World {
    fn items(&self) -> impl Iterator<Item = (usize, &Item)> {
        self.files.iter().enumerate().flat_map(|(f, (_, items))| items.iter().map(move |item| (f, item)))
    }

    fn find(&self, id: u32) -> Option<(usize, &Item)> {
        self.items().find(|(_, item)| item.id == id)
    }

    fn depth(&self, c: usize) -> u16 {
        self.crates[c].iter().map(|dep| self.depth(*dep) + 1).max().unwrap_or(0)
    }

    fn refs_digest(refs: &[u32]) -> u64 {
        idd(&d("sim engine refs", &refs.iter().map(|r| u64::from(*r)).collect::<StdVec<_>>()))
    }

    fn plan_of_file(&self, f: usize) -> StdVec<Planned> {
        let file = outline(f);
        let items = &self.files[f].1;
        let mut text: StdVec<u64> = StdVec::new();
        for item in items {
            text.extend_from_slice(&[u64::from(item.id), u64::from(item.sig), u64::from(item.body), World::refs_digest(&item.refs)]);
        }
        let mut plan = StdVec::new();
        plan.push(Planned { name: file, phase: Phase::new(Stage::Outlines, 0), source: d("sim engine file text", &text), file });
        for item in items {
            let refs = World::refs_digest(&item.refs);
            plan.push(Planned { name: head(item.id), phase: Phase::new(Stage::Heads, 0), source: d("sim engine sig", &[u64::from(item.sig), refs]), file });
            plan.push(Planned { name: body(item.id), phase: Phase::new(Stage::Bodies, 0), source: d("sim engine body text", &[u64::from(item.body), refs]), file });
        }
        plan
    }

    fn plan_of_manifest(&self, c: usize) -> StdVec<Planned> {
        let deps: StdVec<u64> = self.crates[c].iter().map(|dep| *dep as u64).collect();
        let file = manifest(c);
        StdVec::from([Planned { name: names(c), phase: Phase::new(Stage::Names, self.depth(c)), source: d("sim engine crate", &deps), file }])
    }

    /// Every file's and manifest's plan, by its name.
    fn plans(&self) -> BTreeMap<Name, StdVec<Planned>> {
        let mut plans = BTreeMap::new();
        for f in 0..self.files.len() {
            plans.insert(outline(f), self.plan_of_file(f));
        }
        for c in 0..self.crates.len() {
            plans.insert(manifest(c), self.plan_of_manifest(c));
        }
        plans
    }
}

/// The scope: bodies of the asked crates; every name, head and outline (a body's reads).
#[derive(Clone)]
struct Crates {
    asked: BTreeSet<usize>,
    crate_of: BTreeMap<Name, usize>,
}

impl Scope for Crates {
    fn runs(&self, task: &Planned) -> bool {
        task.phase.stage != Stage::Bodies || self.crate_of.get(&task.file).is_none_or(|c| self.asked.contains(c))
    }
}

/// The pools the simulation drives the engine with.
#[derive(Clone, Copy, Debug)]
enum Pools {
    One,
    Many(usize),
    Reversed,
}

impl<'a> Pool<'a> for Pools {
    type Cx = ();

    fn run(&mut self, tasks: &[Task], buffers: &mut [Buffer<'a>], work: &(dyn Fn(&(), &Task, &mut Buffer<'a>) + Sync)) {
        match *self {
            Pools::One => tasks.iter().for_each(|task| work(&(), task, &mut buffers[0])),
            Pools::Many(n) => tasks.iter().enumerate().for_each(|(at, task)| work(&(), task, &mut buffers[at % n])),
            Pools::Reversed => tasks.iter().rev().for_each(|task| work(&(), task, &mut buffers[0])),
        }
    }
}

/// The model's tasks, over the world's text and the view's cells.
fn work(world: &World, view: View<'_, 'static>, task: &Task, buffer: &mut Buffer<'static>) {
    let name = task.planned.name;
    let a = alloc();
    let mut defines: SVec<'static, (Cell, ContentId)> = Vec::new_in(a);
    let mut reads: SVec<'static, (CellId, ContentId)> = Vec::new_in(a);
    let mut findings = 0;
    let mut failed = false;
    let read = |id: CellId, reads: &mut SVec<'static, (CellId, ContentId)>| {
        let seen = view.cell(&id);
        reads.push((id, seen));
        seen
    };
    let mut out_parts: StdVec<u64> = StdVec::new();
    match name.kind {
        Kind::FileItems => {
            let f = (0..world.files.len()).find(|f| outline(*f) == name).expect("a file");
            let items = &world.files[f].1;
            let mut ids: StdVec<u64> = items.iter().map(|item| u64::from(item.id)).collect();
            ids.sort_unstable();
            defines.push((cell(LIST, 0), d("sim engine list", &ids)));
            for item in items {
                let refs = World::refs_digest(&item.refs);
                defines.push((cell(SIG, item.id), d("sim engine sig cell", &[u64::from(item.sig), refs])));
                defines.push((cell(BODY, item.id), d("sim engine body cell", &[u64::from(item.body), refs])));
            }
        }
        Kind::CrateScope => {
            let c = (0..world.crates.len()).find(|c| names(*c) == name).expect("a crate");
            let mut all: StdVec<u64> = StdVec::new();
            for f in (0..world.files.len()).filter(|f| world.files[*f].0 == c) {
                read(CellId { of: outline(f), cell: cell(LIST, 0) }, &mut reads);
                for item in world.files[f].1.iter() {
                    defines.push((cell(DECLARED, item.id), d("sim engine declared", &[u64::from(item.id), c as u64])));
                    all.push(u64::from(item.id));
                }
            }
            for dep in world.crates[c].iter() {
                all.push(idd(&read(CellId { of: names(*dep), cell: cell(ALL, 0) }, &mut reads)));
            }
            defines.push((cell(ALL, 0), d("sim engine all", &all)));
        }
        Kind::Signature => {
            let (f, item) = world.items().find(|(_, item)| head(item.id) == name).expect("an item");
            let c = world.files[f].0;
            let mut parts = StdVec::from([idd(&read(CellId { of: outline(f), cell: cell(SIG, item.id) }, &mut reads))]);
            for r in item.refs.iter() {
                let owner = world.find(*r).map_or(c, |(rf, _)| world.files[rf].0);
                parts.push(idd(&read(CellId { of: names(owner), cell: cell(DECLARED, *r) }, &mut reads)));
            }
            defines.push((cell(HEAD, 0), d("sim engine head", &parts)));
        }
        Kind::BodyCheck => {
            let (f, item) = world.items().find(|(_, item)| body(item.id) == name).expect("an item");
            out_parts.push(idd(&read(CellId { of: outline(f), cell: cell(BODY, item.id) }, &mut reads)));
            for r in item.refs.iter() {
                out_parts.push(idd(&read(CellId { of: head(*r), cell: cell(HEAD, 0) }, &mut reads)));
            }
            // A failure is a function of what the body read, like a typeck panic on its text.
            if item.body == POISON {
                failed = true;
                findings = 1;
            } else {
                if item.body % 5 == 0 {
                    findings = 1;
                }
                // A constant the plan could not see, made inline and shared by key.
                if item.body % 3 == 0 {
                    let v = item.body % 4;
                    let made = konst(v);
                    let value = d("sim engine const value", &[u64::from(v)]);
                    reads.push((CellId { of: made, cell: cell(VALUE, 0) }, value));
                    out_parts.push(idd(&value));
                    let planned = Planned { name: made, phase: task.planned.phase, source: d("sim engine const source", &[u64::from(v)]), file: made };
                    let mut made_defines = Vec::new_in(a);
                    made_defines.push((cell(VALUE, 0), value));
                    buffer.made.push(Made {
                        planned,
                        done: Done {
                            name: made,
                            key: key::task_key(TOOL, made, &planned.source),
                            out: value,
                            defines: made_defines,
                            reads: Vec::new_in(a),
                            findings: 0,
                            failed: false,
                        },
                    });
                }
            }
        }
        _ => unreachable!("the model plans no {name:?}"),
    }
    defines.sort_unstable_by_key(|(cell, _)| *cell);
    reads.sort_unstable_by_key(|(id, _)| *id);
    reads.dedup_by_key(|(id, _)| *id);
    for (_, digest) in defines.iter() {
        out_parts.push(idd(digest));
    }
    buffer.done.push(Done { name, key: task.key, out: d("sim engine out", &out_parts), defines, reads, findings, failed });
}

/// One request on the engine.
fn request(
    world: &World,
    answer: Answer<'static>,
    changed: &BTreeMap<Name, StdVec<Planned>>,
    scope: Crates,
    plant: Plant,
    pool: Pools,
) -> Result<Finished<'static>, EngineError> {
    let files: StdVec<(Name, &[Planned])> = changed.iter().map(|(name, plan)| (*name, plan.as_slice())).collect();
    let mut engine = Engine::new_in(TOOL, answer, &Edit { files: &files }, scope, plant, alloc())?;
    let mut buffers: StdVec<Buffer<'static>> = (0..4).map(|_| Buffer::new_in(alloc())).collect();
    drive(&mut engine, &mut pool.clone(), &mut buffers, &|_, view, task, buffer| work(world, view, task, buffer))?;
    engine.finish()
}

fn first_world(tape: &Tape) -> World {
    let crates: StdVec<StdVec<usize>> = (0..1 + tape.index(3, "crates"))
        .map(|c| (0..c).filter(|dep| tape.one_in(2, "a dependency")).collect())
        .collect();
    let mut world = World { crates, files: StdVec::new(), next_id: 0 };
    for c in 0..world.crates.len() {
        for _ in 0..1 + tape.index(2, "files") {
            world.files.push((c, StdVec::new()));
        }
    }
    for f in 0..world.files.len() {
        for _ in 0..1 + tape.index(3, "items") {
            add_item(tape, &mut world, f);
        }
    }
    world
}

/// The ids a file of crate `c` may refer to: its crate's and its dependencies'.
fn visible(world: &World, c: usize) -> StdVec<u32> {
    world.items().filter(|(f, _)| world.files[*f].0 == c || world.crates[c].contains(&world.files[*f].0)).map(|(_, item)| item.id).collect()
}

fn add_item(tape: &Tape, world: &mut World, f: usize) {
    let id = world.next_id;
    world.next_id += 1;
    let seen = visible(world, world.files[f].0);
    let refs: StdVec<u32> = seen.into_iter().filter(|_| tape.one_in(3, "a reference")).collect();
    let item = Item { id, sig: tape.index(4, "sig") as u32, body: tape.index(16, "body") as u32, refs };
    world.files[f].1.push(item);
}

fn edit(tape: &Tape, world: &World) -> World {
    let mut next = world.clone();
    let f = tape.index(next.files.len(), "the file edited");
    let len = next.files[f].1.len();
    match tape.index(6, "the edit") {
        0 => {}
        1 if len > 0 => {
            let i = tape.index(len, "the item");
            next.files[f].1[i].sig += 1;
        }
        2 if len > 0 => {
            let i = tape.index(len, "the item");
            next.files[f].1[i].body = tape.index(16, "body") as u32;
        }
        3 if len > 0 => {
            let i = tape.index(len, "the item");
            let seen = visible(&next, next.files[f].0);
            next.files[f].1[i].refs = seen.into_iter().filter(|_| tape.one_in(3, "a reference")).collect();
        }
        4 if len > 0 => {
            let i = tape.index(len, "the item removed");
            next.files[f].1.remove(i);
        }
        _ => add_item(tape, &mut next, f),
    }
    next
}

/// A row as the oracle compares it: everything but the request that made it.
type Seen = (Key, Phase, Name, ContentId, ContentId, StdVec<(Cell, ContentId)>, StdVec<(CellId, ContentId)>, u32, bool, bool);

fn seen(answer: &Answer<'_>) -> BTreeMap<Name, Seen> {
    answer
        .rows
        .iter()
        .map(|(name, row)| {
            (*name, (row.key, row.phase, row.file, row.source, row.out, row.defines.to_vec(), row.reads.to_vec(), row.findings, row.inline, row.failed))
        })
        .collect()
}

/// What may be held to a fresh build's rows: every row but a body the scope does not run.
fn compared(name: Name, row: &Seen, scope: &Crates) -> bool {
    row.8 || scope.runs(&Planned { name, phase: row.1, source: row.3, file: row.2 })
}

#[derive(Clone, Copy)]
struct Mix {
    shrink: bool,
    fails: bool,
    pools: bool,
}

const EVERY: Mix = Mix { shrink: true, fails: true, pools: true };

fn histories(tape: &Tape, plant: Plant, mix: Mix) -> Result<(), Broke> {
    let mut history = History::new(first_world(tape), plant);
    let everything: BTreeSet<usize> = (0..history.world.crates.len()).collect();
    for serial in 0..2 + tape.below(9, "requests") {
        if serial > 0 {
            history.world = edit(tape, &history.world);
        }
        let asked: BTreeSet<usize> = match mix.shrink && tape.one_in(2, "a narrower scope") {
            true => everything.iter().copied().filter(|_| tape.one_in(2, "a crate asked")).collect(),
            false => everything.clone(),
        };
        if mix.fails && tape.one_in(4, "a body poisoned") {
            let count = history.world.items().count();
            if count > 0 {
                let at = tape.index(count, "the poisoned body");
                if let Some(item) = history.world.files.iter_mut().flat_map(|(_, items)| items.iter_mut()).nth(at) {
                    item.body = POISON;
                }
            }
        }
        let pool = match mix.pools {
            true => [Pools::One, Pools::Many(1 + tape.index(4, "workers")), Pools::Reversed][tape.index(3, "the pool")],
            false => Pools::One,
        };
        history.request(asked, pool)?;
    }
    Ok(())
}

/// One history: the world as it stands, the last answer and the plans it was made from.
struct History {
    world: World,
    plant: Plant,
    crate_of: BTreeMap<Name, usize>,
    answer: Option<Answer<'static>>,
    last_plans: BTreeMap<Name, StdVec<Planned>>,
    serial: u32,
}

/// What one request of a history did, for a fixture to look at further.
struct Step {
    entered: StdVec<Name>,
    prints: StdVec<(Name, u32)>,
    report: crate::engine::Report,
}

impl History {
    fn new(world: World, plant: Plant) -> History {
        let crate_of = (0..world.files.len()).map(|f| (outline(f), world.files[f].0)).collect();
        History { world, plant, crate_of, answer: Some(Answer::new_in(alloc())), last_plans: BTreeMap::new(), serial: 0 }
    }

    /// One request on today's world, held to a fresh build of it under the same scope.
    fn request(&mut self, asked: BTreeSet<usize>, pool: Pools) -> Result<Step, Broke> {
        let serial = self.serial;
        self.serial += 1;
        let world = &self.world;
        let plant = self.plant;
        let answer = self.answer.take().ok_or_else(|| Broke::Harness(String::from("no answer")))?;
        let scope = Crates { asked, crate_of: self.crate_of.clone() };
        let plans = world.plans();
        let changed: BTreeMap<Name, StdVec<Planned>> =
            plans.iter().filter(|(name, plan)| self.last_plans.get(*name) != Some(*plan)).map(|(name, plan)| (*name, plan.clone())).collect();
        let before = seen(&answer);
        let owed_before = !answer.owed_cells.is_empty() || !answer.owed_tasks.is_empty();
        let ours = request(world, answer, &changed, scope.clone(), plant, pool).map_err(|error| Broke::Oracle(format!("request {serial} failed: {error}")))?;
        let fresh = request(world, Answer::new_in(alloc()), &plans, scope.clone(), Plant::None, Pools::One)
            .map_err(|error| Broke::Harness(format!("a fresh build failed: {error}")))?;
        self.last_plans = plans;
        let here = seen(&ours.answer);
        let there = seen(&fresh.answer);
        // The table: every row a fresh build of the same text and scope has, equal, failed rows
        // too; nothing more but out-of-scope bodies and inline records.
        for (name, row) in there.iter() {
            oracle(here.get(name) == Some(row), || format!("request {serial}: {name:?} differs from a fresh build: here {:?}, fresh {row:?}", here.get(name)))?;
        }
        for (name, row) in here.iter() {
            if !compared(*name, row, &scope) || row.8 {
                continue;
            }
            oracle(there.contains_key(name), || format!("request {serial}: {name:?} has a row a fresh build lacks"))?;
        }
        // Prints: the scope's findings alone (invariant 6).
        let here_prints: StdVec<(Name, u32)> = ours.answer.prints(&scope, alloc().local()).iter().copied().collect();
        let there_prints: StdVec<(Name, u32)> = fresh.answer.prints(&scope, alloc().local()).iter().copied().collect();
        oracle(here_prints == there_prints, || format!("request {serial}: prints {here_prints:?}, fresh {there_prints:?}"))?;
        // Entries equal results, and what was entered is the floor: a task the scope runs whose
        // key moved, that had no row, or a cell of whose read moved since its row.
        oracle(ours.report.entered == ours.report.results, || format!("request {serial}: {:?}", ours.report))?;
        let owes = |name: &Name, now: &Seen| match before.get(name) {
            None => true,
            Some(old) => old.0 != now.0 || old.6.iter().any(|(id, saw)| fresh.answer.cell(id) != *saw),
        };
        let entered: BTreeSet<Name> = ours.entered.iter().copied().collect();
        for name in entered.iter() {
            if name.kind == Kind::Code {
                continue;
            }
            let Some(now) = there.get(name) else {
                return Err(Broke::Oracle(format!("request {serial}: {name:?} entered and a fresh build has no row of it")));
            };
            oracle(owes(name, now), || format!("request {serial}: {name:?} entered beyond the floor"))?;
        }
        for (name, now) in there.iter() {
            if now.8 || !compared(*name, now, &scope) {
                continue;
            }
            oracle(!owes(name, now) || entered.contains(name), || format!("request {serial}: {name:?} owed and not entered"))?;
        }
        // Nothing changed and nothing owed: the certificate.
        // A failed row stands like any: the request after a failure is the certificate too.
        if changed.is_empty() && !owed_before {
            oracle(ours.report.entered == 0, || format!("request {serial}: nothing changed and {} entered", ours.report.entered))?;
        }
        let step = Step { entered: ours.entered.iter().copied().collect(), prints: here_prints, report: ours.report };
        self.answer = Some(ours.answer);
        Ok(step)
    }
}

/// Expected: "test sim::histories_answer_as_fresh_builds ... ok".
#[test]
fn histories_answer_as_fresh_builds() {
    check(SimConfig::cases(300), |tape| histories(tape, Plant::None, EVERY));
}

/// Lane DR's findings 1, 3 and 4 (2026-11-17): a scope that shrinks and grows; a changed file
/// outside it is still planned, a reader held out of it is entered when reached, and never takes
/// its own row's reads.
#[test]
fn a_shrinking_scope_answers_as_fresh_builds() {
    check(SimConfig::cases(300), |tape| histories(tape, Plant::None, Mix { shrink: true, fails: false, pools: false }));
}

/// Finding 6: a request with a failing body is compared too, its other results merged, and the
/// request after it is the certificate.
#[test]
fn a_failing_body_leaves_the_rest_answered() {
    check(SimConfig::cases(300), |tape| histories(tape, Plant::None, Mix { shrink: false, fails: true, pools: false }));
}

/// One worker, many, reversed: equal answers (rule 25).
#[test]
fn pools_of_any_shape_answer_alike() {
    check(SimConfig::cases(300), |tape| histories(tape, Plant::None, Mix { shrink: false, fails: false, pools: true }));
}

fn it(id: u32, sig: u32, body: u32, refs: &[u32]) -> Item {
    Item { id, sig, body, refs: refs.to_vec() }
}

/// Crate 0 with item 0 (file 0); crate 1, depending on it, with item 1 (file 1) whose body
/// calls item 0. Bodies 1, 2, 4 and 7 make no finding and no inline record.
fn two_crates(body0: u32) -> World {
    World { crates: StdVec::from([StdVec::new(), StdVec::from([0])]), files: StdVec::from([(0, StdVec::from([it(0, 0, body0, &[])])), (1, StdVec::from([it(1, 0, 2, &[0])]))]), next_id: 2 }
}

fn crates(asked: &[usize]) -> BTreeSet<usize> {
    asked.iter().copied().collect()
}

fn fixture(run: impl FnOnce() -> Result<(), Broke>) {
    if let Err(broke) = run() {
        panic!("{broke:?}");
    }
}

/// Finding 1: a request that does not run a changed file's bodies still plans the file, so its
/// outline and heads are made today and its bodies are owed to the request that runs them.
#[test]
fn finding_1_a_changed_file_out_of_scope_is_planned() {
    fixture(|| {
        let mut history = History::new(two_crates(1), Plant::None);
        history.request(crates(&[0, 1]), Pools::One)?;
        history.world.files[0].1[0].body = 4;
        history.world.files[0].1[0].sig = 1;
        let held = history.request(crates(&[1]), Pools::One)?;
        oracle(held.entered.contains(&outline(0)) && held.entered.contains(&head(0)), || format!("the changed file was not planned: {:?}", held.entered))?;
        oracle(!held.entered.contains(&body(0)) && held.entered.contains(&body(1)), || format!("scope not kept: {:?}", held.entered))?;
        let reached = history.request(crates(&[0]), Pools::One)?;
        oracle(reached.entered == StdVec::from([body(0)]), || format!("the held body: {:?}", reached.entered))
    });
}

/// Finding 2: a part is never entered before its file's outline is made again in the request.
#[test]
fn finding_2_parts_enter_after_their_outline() {
    fixture(|| {
        let mut history = History::new(two_crates(1), Plant::None);
        history.request(crates(&[0, 1]), Pools::Reversed)?;
        history.world.files[0].1.remove(0);
        history.world.files[0].1.push(it(2, 0, 4, &[]));
        history.world.files[1].1[0].refs = StdVec::from([2]);
        let step = history.request(crates(&[0, 1]), Pools::Reversed)?;
        let at = |name: Name| step.entered.iter().position(|n| *n == name);
        let home = at(outline(0)).ok_or_else(|| Broke::Oracle(String::from("the outline was not made again")))?;
        for part in [head(2), body(2), head(1), body(1), names(0)] {
            oracle(at(part).is_some_and(|p| p > home), || format!("{part:?} entered before its home: {:?}", step.entered))?;
        }
        oracle(at(head(0)).is_none() && at(body(0)).is_none(), || format!("a removed item entered: {:?}", step.entered))
    });
}

/// Findings 3 and 4: a reader held out of scope while what it read moved, twice, is entered
/// once when the scope reaches it, reads today's cells (never its own row's), and the owed cell
/// is retired; the request after is the certificate.
#[test]
fn findings_3_and_4_a_held_reader_is_entered_when_reached() {
    fixture(|| {
        let world = World { crates: StdVec::from([StdVec::new()]), files: StdVec::from([(0, StdVec::from([it(0, 0, 1, &[]), it(1, 0, 2, &[0])]))]), next_id: 2 };
        let mut history = History::new(world, Plant::None);
        history.request(crates(&[0]), Pools::One)?;
        for sig in 1..3 {
            history.world.files[0].1[0].sig = sig;
            let held = history.request(crates(&[]), Pools::One)?;
            oracle(!held.entered.contains(&body(1)) && held.entered.contains(&head(0)), || format!("held: {:?}", held.entered))?;
        }
        let reached = history.request(crates(&[0]), Pools::One)?;
        oracle(reached.entered.contains(&body(1)), || format!("the held reader was not entered: {:?}", reached.entered))?;
        let owed = history.answer.as_ref().map_or(0, |answer| answer.owed_cells.len() + answer.owed_tasks.len());
        oracle(owed == 0, || format!("{owed} still owed after the scope reached every reader"))?;
        let quiet = history.request(crates(&[0]), Pools::One)?;
        oracle(quiet.report.certified, || format!("not the certificate: {:?}", quiet.entered))
    });
}

/// Finding 5: printing is decided by the scope; a finding outside it stays in the answer
/// unprinted and is printed, without entering anything, when a request reaches it.
#[test]
fn finding_5_prints_follow_the_scope() {
    fixture(|| {
        let mut history = History::new(two_crates(5), Plant::None);
        let all = history.request(crates(&[0, 1]), Pools::One)?;
        oracle(all.prints.iter().any(|(n, _)| *n == body(0)), || format!("the finding was not printed: {:?}", all.prints))?;
        let other = history.request(crates(&[1]), Pools::One)?;
        oracle(other.prints.is_empty() && other.report.certified, || format!("out of scope: {:?}, {:?}", other.prints, other.entered))?;
        let back = history.request(crates(&[0]), Pools::One)?;
        oracle(back.prints.iter().any(|(n, _)| *n == body(0)) && back.entered.is_empty(), || format!("carried: {:?}, {:?}", back.prints, back.entered))
    });
}

/// Finding 6 and invariant 7: a request with a failing body merges the rest and is compared like
/// any; the failure is a row that prints, so the request after it is the certificate (tempo, tag
/// g090627); fixing the body enters it and nothing else.
#[test]
fn finding_6_a_failure_is_a_row() {
    fixture(|| {
        let mut history = History::new(two_crates(1), Plant::None);
        history.request(crates(&[0, 1]), Pools::One)?;
        history.world.files[0].1[0].body = POISON;
        history.world.files[1].1[0].body = 7;
        let failing = history.request(crates(&[0, 1]), Pools::Many(2))?;
        oracle(failing.report.failed == 1 && failing.entered.contains(&body(1)), || format!("{:?}: {:?}", failing.report, failing.entered))?;
        oracle(failing.prints.iter().any(|(n, _)| *n == body(0)), || format!("the failure did not print: {:?}", failing.prints))?;
        let quiet = history.request(crates(&[0, 1]), Pools::One)?;
        oracle(quiet.report.certified && quiet.prints == failing.prints, || format!("after the failure: {:?}, {:?}", quiet.entered, quiet.prints))?;
        history.world.files[0].1[0].body = 4;
        let fixed = history.request(crates(&[0, 1]), Pools::One)?;
        oracle(fixed.entered == StdVec::from([outline(0), body(0)]), || format!("the fix: {:?}", fixed.entered))
    });
}

#[test]
fn control_a_failure_not_kept_is_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::FailureNotKept, "", |tape, plant| histories(tape, plant, EVERY));
}

#[test]
fn control_forgotten_owed_cells_are_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::OwedForgotten, "", |tape, plant| histories(tape, plant, EVERY));
}

#[test]
fn control_reads_not_compared_are_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::ReadsNotCompared, "", |tape, plant| histories(tape, plant, EVERY));
}

#[test]
fn control_dropped_inline_records_are_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::MadeDropped, "", |tape, plant| histories(tape, plant, EVERY));
}
