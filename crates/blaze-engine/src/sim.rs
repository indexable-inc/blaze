//! The engine's history simulations (engine.rs, invariants 1 to 12): a model workspace of crates,
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
    Answer, Buffer, Cause, CellDigests, CellId, Done, Edit, Engine, EngineError, Everything, Finished, Gen, Made, Phase, Plant, Planned, Pool, SMap, SVec, Scope, Stage,
    Task, View, drive, drive_growing,
};
use crate::last::LastAnswer;
use crate::overlay::Fold;
use crate::pages::{Kept, PagesPlant, Root};

pub(crate) const TOOL: Tool = Tool(ContentId([7; 32]));

pub(crate) fn alloc() -> SharedAlloc<'static> {
    blaze_own::testing::heap().shared()
}

pub(crate) fn oracle(holds: bool, why: impl FnOnce() -> String) -> Result<(), Broke> {
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
/// An outline's macro calls: the items of the file that derive, with their signatures.
const MACROS: u8 = 11;
/// A names record's expansion of one item's derive.
const GEN: u8 = 12;
/// A body text whose check fails.
pub(crate) const POISON: u32 = 13;

/// Whether an item with this signature carries a derive, whose expansion the crate's names make
/// and grow into a head and a body (invariant 11). Never below 4: the fixtures' signatures.
fn derives(sig: u32) -> bool {
    sig % 8 >= 4
}

fn outline(f: usize) -> Name {
    Name::new(Kind::FileItems, d("sim engine file", &[f as u64]))
}
fn manifest(c: usize) -> Name {
    Name::new(Kind::Units, d("sim engine manifest", &[c as u64]))
}
pub(crate) fn names(c: usize) -> Name {
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
/// The head of the item a derive on item `id` makes: grown by its crate's names.
fn genhead(id: u32) -> Name {
    Name::new(Kind::Signature, d("sim engine gen head", &[u64::from(id)]))
}
/// The body of the item a derive on item `id` makes.
fn genbody(id: u32) -> Name {
    Name::new(Kind::BodyCheck, d("sim engine gen body", &[u64::from(id)]))
}

/// The source of the inline constant `v`: what its key is made from.
fn konst_source(v: u32) -> ContentId {
    d("sim engine const source", &[u64::from(v)])
}

/// The cell every constant reads: the root crate's names (crate 0 is in every world). No body
/// reads it: a body reads the constant's value alone (a hit is one read, invariant 12), so an
/// edit that moves it re-validates the constant, and the body is entered for the constant's
/// value when that moved.
fn konst_read() -> CellId {
    CellId { of: names(0), cell: cell(ALL, 0) }
}

/// The record of the constant `v` made over the view: its value is a function of what it read.
fn make_konst<V: CellDigests + Copy>(v: u32, view: V) -> Done<'static> {
    let a = alloc();
    let read = konst_read();
    let seen = view.cell(&read);
    let value = d("sim engine const value", &[u64::from(v), idd(&seen)]);
    let mut defines = Vec::new_in(a);
    defines.push((cell(VALUE, 0), value));
    let mut reads = Vec::new_in(a);
    reads.push((read, seen));
    let made = konst(v);
    Done { name: made, key: key::task_key(TOOL, made, &konst_source(v)), out: value, defines, reads, findings: 0, failed: false }
}

/// What a names task grows: for each item of crate `c` that derives, the head and the body of
/// the item its expansion makes, keyed from that expansion's text (invariant 11).
pub(crate) fn growth_of(world: &World, c: usize) -> StdVec<Planned> {
    let by = names(c);
    let mut grown = StdVec::new();
    for (f, item) in world.items() {
        if world.files[f].0 != c || !derives(item.sig) {
            continue;
        }
        let source = d("sim engine gen text", &[u64::from(item.id), u64::from(item.sig)]);
        grown.push(Planned { name: genhead(item.id), phase: Phase::new(Stage::Heads, 0), source, file: by });
        grown.push(Planned { name: genbody(item.id), phase: Phase::new(Stage::Bodies, 0), source, file: by });
    }
    grown
}

/// The driver's half of growth: each names task merged grows its crate's expansions, whole.
pub(crate) fn grow_names<L: LastAnswer, R: crate::readers::ReaderIndex<'static>>(world: &World, engine: &mut Engine<'static, impl Scope, L, R>) -> Result<(), EngineError> {
    let merged: StdVec<Name> = engine.merged().to_vec();
    for name in merged.iter() {
        if name.kind != Kind::CrateScope {
            continue;
        }
        let Some(c) = (0..world.crates.len()).find(|c| names(*c) == *name) else { continue };
        engine.grow(*name, &growth_of(world, c))?;
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Item {
    pub(crate) id: u32,
    pub(crate) sig: u32,
    pub(crate) body: u32,
    pub(crate) refs: StdVec<u32>,
}

#[derive(Clone, Debug)]
pub(crate) struct World {
    /// Each crate's dependencies, all of lower index.
    pub(crate) crates: StdVec<StdVec<usize>>,
    /// Each file's crate and items.
    pub(crate) files: StdVec<(usize, StdVec<Item>)>,
    pub(crate) next_id: u32,
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
    pub(crate) fn plans(&self) -> BTreeMap<Name, StdVec<Planned>> {
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
pub(crate) struct Crates {
    pub(crate) asked: BTreeSet<usize>,
    pub(crate) crate_of: BTreeMap<Name, usize>,
}

impl Crates {
    /// The scope of `asked` over `world`: a grown task's file is its grower, the crate's names,
    /// so its bodies are the crate's too.
    pub(crate) fn of(world: &World, asked: BTreeSet<usize>) -> Crates {
        let crate_of = (0..world.files.len()).map(|f| (outline(f), world.files[f].0)).chain((0..world.crates.len()).map(|c| (names(c), c))).collect();
        Crates { asked, crate_of }
    }
}

impl Scope for Crates {
    fn runs(&self, task: &Planned) -> bool {
        task.phase.stage != Stage::Bodies || self.crate_of.get(&task.file).is_none_or(|c| self.asked.contains(c))
    }
}

/// The pools the simulation drives the engine with.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Pools {
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

/// The model's tasks, over the world's text and the view's cells. `plant` is
/// [`Plant::HitWithoutReads`] alone when a body drops its read of the constant it hit or made.
pub(crate) fn work<V: CellDigests + Copy>(world: &World, plant: Plant, view: V, task: &Task, buffer: &mut Buffer<'static>) {
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
    // A grown head or body (invariant 11): the item a derive made, read from its crate's names.
    let grown = world.items().find(|(_, item)| genhead(item.id) == name || genbody(item.id) == name);
    match (name.kind, grown) {
        (Kind::Signature | Kind::BodyCheck, Some((f, item))) => {
            let c = world.files[f].0;
            if !derives(item.sig) {
                // Planned and no derive: only a growth kept past its grower (a planted bug).
                failed = true;
                findings = 1;
            } else if genhead(item.id) == name {
                let expansion = read(CellId { of: names(c), cell: cell(GEN, item.id) }, &mut reads);
                defines.push((cell(HEAD, 0), d("sim engine gen head cell", &[idd(&expansion)])));
            } else {
                out_parts.push(idd(&read(CellId { of: genhead(item.id), cell: cell(HEAD, 0) }, &mut reads)));
            }
        }
        (Kind::Signature | Kind::BodyCheck, None) if world.items().all(|(_, item)| head(item.id) != name && body(item.id) != name) => {
            // A grown task whose item is gone: only a growth kept past its grower.
            failed = true;
            findings = 1;
        }
        (Kind::Code, _) => {
            // Entered as a task: only under `Plant::RemakesUnneeded`; made from its name, the
            // bytes its makers make.
            if let Some(v) = (0..4).find(|v| konst(*v) == name) {
                buffer.done.push(Done { key: task.key, ..make_konst(v, view) });
                return;
            }
            failed = true;
        }
        (Kind::FileItems, _) => {
            let f = (0..world.files.len()).find(|f| outline(*f) == name).expect("a file");
            let items = &world.files[f].1;
            let mut ids: StdVec<u64> = items.iter().map(|item| u64::from(item.id)).collect();
            ids.sort_unstable();
            defines.push((cell(LIST, 0), d("sim engine list", &ids)));
            let mut calls: StdVec<u64> = StdVec::new();
            for item in items {
                let refs = World::refs_digest(&item.refs);
                defines.push((cell(SIG, item.id), d("sim engine sig cell", &[u64::from(item.sig), refs])));
                defines.push((cell(BODY, item.id), d("sim engine body cell", &[u64::from(item.body), refs])));
                if derives(item.sig) {
                    calls.extend_from_slice(&[u64::from(item.id), u64::from(item.sig)]);
                }
            }
            defines.push((cell(MACROS, 0), d("sim engine macros", &calls)));
        }
        (Kind::CrateScope, _) => {
            let c = (0..world.crates.len()).find(|c| names(*c) == name).expect("a crate");
            let mut all: StdVec<u64> = StdVec::new();
            for f in (0..world.files.len()).filter(|f| world.files[*f].0 == c) {
                read(CellId { of: outline(f), cell: cell(LIST, 0) }, &mut reads);
                read(CellId { of: outline(f), cell: cell(MACROS, 0) }, &mut reads);
                for item in world.files[f].1.iter() {
                    defines.push((cell(DECLARED, item.id), d("sim engine declared", &[u64::from(item.id), c as u64])));
                    all.push(u64::from(item.id));
                    if derives(item.sig) {
                        defines.push((cell(GEN, item.id), d("sim engine expansion", &[u64::from(item.id), u64::from(item.sig)])));
                    }
                }
            }
            for dep in world.crates[c].iter() {
                all.push(idd(&read(CellId { of: names(*dep), cell: cell(ALL, 0) }, &mut reads)));
            }
            defines.push((cell(ALL, 0), d("sim engine all", &all)));
        }
        (Kind::Signature, _) => {
            let (f, item) = world.items().find(|(_, item)| head(item.id) == name).expect("an item");
            let c = world.files[f].0;
            let mut parts = StdVec::from([idd(&read(CellId { of: outline(f), cell: cell(SIG, item.id) }, &mut reads))]);
            for r in item.refs.iter() {
                let owner = world.find(*r).map_or(c, |(rf, _)| world.files[rf].0);
                parts.push(idd(&read(CellId { of: names(owner), cell: cell(DECLARED, *r) }, &mut reads)));
            }
            defines.push((cell(HEAD, 0), d("sim engine head", &parts)));
        }
        (Kind::BodyCheck, _) => {
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
                // A constant the plan could not see, shared by key: found standing (a hit) or
                // made inline; either way one read, of its value, and none of its own reads,
                // which the constant's record keeps (invariant 12, the plan's point 6).
                if item.body % 3 == 0 {
                    let v = item.body % 4;
                    let made = konst(v);
                    let value_cell = CellId { of: made, cell: cell(VALUE, 0) };
                    let key = key::task_key(TOOL, made, &konst_source(v));
                    let dropped = plant == Plant::HitWithoutReads;
                    let value = match view.hit(made, key) {
                        Some(_) if dropped => view.cell(&value_cell),
                        Some(_) => read(value_cell, &mut reads),
                        None => {
                            let done = make_konst(v, view);
                            let value = done.out;
                            if !dropped {
                                reads.push((value_cell, value));
                            }
                            let planned = Planned { name: made, phase: task.planned.phase, source: konst_source(v), file: made };
                            buffer.made.push(Made { planned, done, by: name });
                            value
                        }
                    };
                    out_parts.push(idd(&value));
                }
            }
        }
        (kind, _) => unreachable!("the model plans no {kind:?} {name:?}"),
    }
    defines.sort_unstable_by_key(|(cell, _)| *cell);
    reads.sort_unstable_by_key(|(id, _)| *id);
    reads.dedup_by_key(|(id, _)| *id);
    for (_, digest) in defines.iter() {
        out_parts.push(idd(digest));
    }
    buffer.done.push(Done { name, key: task.key, out: d("sim engine out", &out_parts), defines, reads, findings, failed });
}

/// One request on the engine, over the last answer in any form `L` (last_sim.rs runs the same
/// history over two forms and holds them equal).
pub(crate) fn request<L: Fold<'static> + Sync>(
    world: &World,
    answer: L,
    changed: &BTreeMap<Name, StdVec<Planned>>,
    scope: Crates,
    plant: Plant,
    pool: Pools,
) -> Result<Finished<'static, L::Out>, EngineError> {
    let files: StdVec<(Name, &[Planned])> = changed.iter().map(|(name, plan)| (*name, plan.as_slice())).collect();
    let mut engine = Engine::new_in(crate::engine::Tools([TOOL; key::KINDS]), answer, &Edit { files: &files }, scope, plant, alloc())?;
    let mut buffers: StdVec<Buffer<'static>> = (0..4).map(|_| Buffer::new_in(alloc())).collect();
    drive_growing(&mut engine, &mut pool.clone(), &mut buffers, &|_, view, task, buffer| work(world, plant, view, task, buffer), &mut |engine| {
        grow_names(world, engine)
    })?;
    engine.finish()
}

pub(crate) fn first_world(tape: &Tape) -> World {
    let crates: StdVec<StdVec<usize>> = (0..1 + tape.index(3, "crates"))
        .map(|c| (0..c).filter(|_dep| tape.one_in(2, "a dependency")).collect())
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
    // Signatures 4 to 7 derive (invariant 11): an edit's `sig += 1` adds and removes expansions.
    let item = Item { id, sig: tape.index(8, "sig") as u32, body: tape.index(16, "body") as u32, refs };
    world.files[f].1.push(item);
}

pub(crate) fn edit(tape: &Tape, world: &World) -> World {
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
    histories_paged(tape, plant, mix, PagesPlant::None)
}

/// Every mix of [`histories`], a bug planted in the stored answer's form (pages_sim.rs).
pub(crate) fn histories_every(tape: &Tape, pages: PagesPlant) -> Result<(), Broke> {
    histories_paged(tape, Plant::None, EVERY, pages)
}

/// [`histories`] with a bug planted in the stored answer's form.
fn histories_paged(tape: &Tape, plant: Plant, mix: Mix, pages: PagesPlant) -> Result<(), Broke> {
    let mut history = History::new(first_world(tape), plant);
    history.pages = pages;
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
    /// The stored answer (pages.rs): every request starts from the last one's pages, decoded,
    /// so each oracle below also holds a restore to a fresh build (rule 26).
    store: BTreeMap<Key, StdVec<u8>>,
    root: Option<Root<'static>>,
    causes: SMap<'static, Name, (Gen, Cause)>,
    pages: PagesPlant,
}

/// What one request of a history did, for a fixture to look at further.
struct Step {
    entered: StdVec<Name>,
    causes: StdVec<crate::engine::Entered>,
    prints: StdVec<(Name, u32)>,
    report: crate::engine::Report,
}

impl History {
    fn new(world: World, plant: Plant) -> History {
        let crate_of = Crates::of(&world, BTreeSet::new()).crate_of;
        History {
            world,
            plant,
            crate_of,
            answer: Some(Answer::new_in(alloc())),
            last_plans: BTreeMap::new(),
            serial: 0,
            store: BTreeMap::new(),
            root: None,
            causes: blaze_own::BTreeMap::new_in(alloc()),
            pages: PagesPlant::None,
        }
    }

    /// `kept` stored as pages over the last root, then read back: what the next request starts
    /// from. The read equals the write in bytes (its own encoding names the same root).
    fn store_and_restore(&mut self, kept: Kept<'static>, serial: u32) -> Result<Kept<'static>, Broke> {
        let encoded = crate::pages::encode_planted(&kept, self.root.as_ref(), self.pages, alloc());
        for (key, bytes) in encoded.puts.iter() {
            self.store.insert(*key, bytes.iter().copied().collect());
        }
        let store = &self.store;
        let restored = crate::pages::decode(encoded.root, |key| store.get(&key).map(|bytes| crate::pages::Sealed::hashed(key, bytes)), alloc())
            .map_err(|error| Broke::Oracle(format!("request {serial}: its stored answer does not read back: {error}")))?;
        let again = crate::pages::encode_planted(&restored, self.root.as_ref(), self.pages, alloc());
        oracle(again.root == encoded.root, || format!("request {serial}: the stored answer read back encodes to another root"))?;
        let (was, now) = (&kept.answer, &restored.answer);
        for (table, equal) in [
            ("rows", was.rows == now.rows),
            ("cells", was.cells == now.cells),
            ("files", was.by_file == now.by_file),
            ("owed cells", was.owed_cells == now.owed_cells),
            ("owed tasks", was.owed_tasks == now.owed_tasks),
            ("serial", was.serial == now.serial),
        ] {
            oracle(equal, || format!("request {serial}: the stored answer read back differs in its {table}"))?;
        }
        oracle(restored.answer.readers == kept.answer.readers, || format!("request {serial}: the reverse index made again differs from the engine's"))?;
        oracle(restored.answer.with_findings == kept.answer.with_findings, || format!("request {serial}: the rows with findings made again differ"))?;
        self.root = Some(crate::pages::decode_root_hashed(encoded.root, &self.store[&encoded.root], alloc()).map_err(|error| Broke::Harness(format!("{error}")))?);
        Ok(restored)
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
        // Changed and still needed (invariant 10): a made record stays only while a row reads it.
        for (name, row) in ours.answer.rows.iter() {
            if !row.inline {
                continue;
            }
            let read = row.defines.iter().any(|(c, _)| ours.answer.readers.get(&CellId { of: *name, cell: *c }).is_some_and(|readers| readers.iter().any(|r| r != name)));
            oracle(read, || format!("request {serial}: the made record {name:?} is kept and no row reads it"))?;
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
        // The cause of every entered task, derived here from the last answer and a fresh build
        // alone: no row was new; else a moved key; else the first read, in the row's read order,
        // whose cell a fresh build holds otherwise; else owed. Identical: its out is its last
        // row's.
        oracle(ours.causes.len() == ours.entered.len(), || format!("request {serial}: {} causes for {} entered", ours.causes.len(), ours.entered.len()))?;
        let counted: u32 = ours.report.stages.iter().map(|stage| stage.entered).sum();
        oracle(counted == ours.report.entered, || format!("request {serial}: stages count {counted}, entered {}", ours.report.entered))?;
        for (at, one) in ours.causes.iter().enumerate() {
            oracle(ours.entered[at] == one.name, || format!("request {serial}: cause {at} is {:?}'s, entered {:?}", one.name, ours.entered[at]))?;
            if one.name.kind == Kind::Code {
                // A made record is entered made by its user (inline), or re-validated by its own
                // reads, "the record keeps its own reads" (the plan's point 6): then only when one
                // moved, for the first in its row's read order that a fresh build holds otherwise
                // (invariants 4 and 9), so never beyond the floor.
                let revalidated = before
                    .get(&one.name)
                    .and_then(|old| old.6.iter().find(|(id, saw)| fresh.answer.cell(id) != *saw))
                    .map(|(id, saw)| Cause::ReadMoved { read: *id, was: *saw, now: fresh.answer.cell(id) });
                oracle(one.cause == Cause::Inline || Some(one.cause) == revalidated, || {
                    format!("request {serial}: the made record {:?} entered for {:?}, the model says Inline or {revalidated:?}", one.name, one.cause)
                })?;
                continue;
            }
            let Some(now) = there.get(&one.name) else { continue };
            let expected = match before.get(&one.name) {
                None => Cause::New,
                Some(old) if old.0 != now.0 => Cause::KeyMoved,
                Some(old) => match old.6.iter().find(|(id, saw)| fresh.answer.cell(id) != *saw) {
                    Some((id, saw)) => Cause::ReadMoved { read: *id, was: *saw, now: fresh.answer.cell(id) },
                    None => Cause::Owed,
                },
            };
            oracle(one.cause == expected, || format!("request {serial}: {:?} entered for {:?}, the model says {expected:?}", one.name, one.cause))?;
            let identical = before.get(&one.name).is_some_and(|old| old.4 == now.4);
            oracle(one.identical == identical, || format!("request {serial}: {:?} identical {} , the model says {identical}", one.name, one.identical))?;
        }
        // Nothing changed and nothing owed: the certificate.
        // A failed row stands like any: the request after a failure is the certificate too.
        if changed.is_empty() && !owed_before {
            oracle(ours.report.entered == 0, || format!("request {serial}: nothing changed and {} entered", ours.report.entered))?;
        }
        let step = Step { entered: ours.entered.iter().copied().collect(), causes: ours.causes.iter().copied().collect(), prints: here_prints, report: ours.report };
        let causes = core::mem::replace(&mut self.causes, blaze_own::BTreeMap::new_in(alloc()));
        // A record key a row: the driver's, here one function of the row, so the section is
        // written and read back with every answer.
        let mut records = blaze_own::BTreeMap::new_in(alloc());
        for (name, row) in ours.answer.rows.iter() {
            records.insert(*name, blaze_drive::key::Key(ContentId::of("sim record key", &[row.key.0.0, row.out.0].concat())));
        }
        let kept = Kept::after(causes, ours.answer, &ours.causes, records);
        let restored = self.store_and_restore(kept, serial)?;
        self.causes = restored.causes;
        self.answer = Some(restored.answer);
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

/// The cause of each entered task (E6 telemetry): an edited body is entered for its key; the
/// reader of an edited signature for that signature's cell, with the digests it saw and sees;
/// a fresh history's tasks are new.
#[test]
fn each_entered_task_says_why() {
    fixture(|| {
        let mut history = History::new(two_crates(1), Plant::None);
        let first = history.request(crates(&[0, 1]), Pools::One)?;
        oracle(first.causes.iter().all(|one| one.cause == Cause::New), || format!("a fresh history: {:?}", first.causes))?;
        let cause_of = |step: &Step, name: Name| step.causes.iter().find(|one| one.name == name).map(|one| one.cause);
        history.world.files[0].1[0].body = 4;
        let edited = history.request(crates(&[0, 1]), Pools::One)?;
        oracle(cause_of(&edited, body(0)) == Some(Cause::KeyMoved), || format!("the edited body: {:?}", edited.causes))?;
        oracle(cause_of(&edited, body(1)).is_none(), || format!("a body that reads no moved cell: {:?}", edited.causes))?;
        history.world.files[0].1[0].sig = 3;
        let signed = history.request(crates(&[0, 1]), Pools::One)?;
        oracle(cause_of(&signed, head(0)) == Some(Cause::KeyMoved), || format!("the edited signature: {:?}", signed.causes))?;
        let moved = CellId { of: head(0), cell: cell(HEAD, 0) };
        oracle(
            matches!(cause_of(&signed, body(1)), Some(Cause::ReadMoved { read, was, now }) if read == moved && was != now),
            || format!("the edited signature's reader: {:?}", signed.causes),
        )
    });
}

#[test]
fn control_a_wrong_cause_is_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::WrongCause, "", |tape, plant| histories(tape, plant, EVERY));
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

// ---- Read lists in read order (invariant 9, plan row D7) ----

/// The outline's switch: A or B.
const SWITCH: u8 = 14;
/// Cells of the outline that never move, read after the switch.
const STABLE: u8 = 15;

fn switch_digest(a: bool) -> ContentId {
    d("sim engine switch", &[u64::from(a)])
}

/// File 0's plan in the switch world: its outline defines the switch (A when `a`) and `extra`
/// stable cells; in A it also plans `head(1)`; `body(2)` is planned always.
fn switch_plan(a: bool, extra: u32) -> StdVec<Planned> {
    let file = outline(0);
    let mut plan = StdVec::from([Planned { name: file, phase: Phase::new(Stage::Outlines, 0), source: d("sim engine switch file", &[u64::from(a), u64::from(extra)]), file }]);
    if a {
        plan.push(Planned { name: head(1), phase: Phase::new(Stage::Heads, 0), source: d("sim engine switch head", &[]), file });
    }
    plan.push(Planned { name: body(2), phase: Phase::new(Stage::Bodies, 0), source: d("sim engine switch body", &[]), file });
    plan
}

/// The switch world's tasks. `body(2)` reads the switch first and, only when it says A,
/// `head(1)`'s cell second (a question formed from the first answer), then the stable cells:
/// its read list is in that order, never sorted.
fn switch_work(a: bool, extra: u32, view: View<'_, 'static>, task: &Task, buffer: &mut Buffer<'static>) {
    let name = task.planned.name;
    let al = alloc();
    let mut defines: SVec<'static, (Cell, ContentId)> = Vec::new_in(al);
    let mut reads: SVec<'static, (CellId, ContentId)> = Vec::new_in(al);
    if name == outline(0) {
        defines.push((cell(SWITCH, 0), switch_digest(a)));
        for i in 0..extra {
            defines.push((cell(STABLE, i), d("sim engine stable", &[u64::from(i)])));
        }
        defines.sort_unstable_by_key(|(c, _)| *c);
    } else if name == head(1) {
        defines.push((cell(HEAD, 0), d("sim engine switch head cell", &[])));
    } else {
        let switch = CellId { of: outline(0), cell: cell(SWITCH, 0) };
        let seen = view.cell(&switch);
        reads.push((switch, seen));
        if seen == switch_digest(true) {
            let second = CellId { of: head(1), cell: cell(HEAD, 0) };
            reads.push((second, view.cell(&second)));
        }
        for i in 0..extra {
            let stable = CellId { of: outline(0), cell: cell(STABLE, i) };
            reads.push((stable, view.cell(&stable)));
        }
    }
    let mut parts: StdVec<u64> = reads.iter().map(|(_, seen)| idd(seen)).collect();
    parts.extend(defines.iter().map(|(_, digest)| idd(digest)));
    buffer.done.push(Done { name, key: task.key, out: d("sim engine switch out", &parts), defines, reads, findings: 0, failed: false });
}

/// One request on the engine alone, every task in scope. The read-order cases do not go
/// through the stored answer: its pages still refuse a read list out of sorted order
/// (pages.rs `decode_page`, lane RD6's file).
fn direct(
    answer: Answer<'static>,
    files: &[(Name, &[Planned])],
    plant: Plant,
    pool: Pools,
    work: &(dyn Fn(View<'_, 'static>, &Task, &mut Buffer<'static>) + Sync),
) -> Result<Finished<'static>, EngineError> {
    let mut engine = Engine::new_in(crate::engine::Tools([TOOL; key::KINDS]), answer, &Edit { files }, Everything, plant, alloc())?;
    let mut buffers: StdVec<Buffer<'static>> = (0..2).map(|_| Buffer::new_in(alloc())).collect();
    drive(&mut engine, &mut pool.clone(), &mut buffers, &|_, view, task, buffer| work(view, task, buffer))?;
    engine.finish()
}

/// The switch world in A, then edited to B (the switch moves and `head(1)`, the second read's
/// definer, leaves the plan): `body(2)` is entered for its first read, the walk looks up that
/// read alone, and its rows keep the reads in read order.
fn read_order(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let extra = tape.index(4, "reads after the second") as u32;
    let pool = [Pools::One, Pools::Many(2), Pools::Reversed][tape.index(3, "the pool")];
    let switch = CellId { of: outline(0), cell: cell(SWITCH, 0) };
    let mut answer = Answer::new_in(alloc());
    for (at, a) in [true, false].into_iter().enumerate() {
        let plan = switch_plan(a, extra);
        let files = [(outline(0), plan.as_slice())];
        let finished = direct(answer, &files, plant, pool, &|view, task, buffer| switch_work(a, extra, view, task, buffer))
            .map_err(|error| Broke::Oracle(format!("request {at} failed: {error}")))?;
        let reads: StdVec<(CellId, ContentId)> = finished.answer.rows.get(&body(2)).map(|row| row.reads.to_vec()).unwrap_or_default();
        let want = 1 + usize::from(a) + extra as usize;
        oracle(reads.len() == want && reads.first().map(|(id, _)| *id) == Some(switch), || {
            format!("request {at}: body(2)'s reads are not its {want} reads in read order: {reads:?}")
        })?;
        if at == 1 {
            let cause = finished.causes.iter().find(|one| one.name == body(2)).map(|one| one.cause);
            oracle(matches!(cause, Some(Cause::ReadMoved { read, was, now }) if read == switch && was == switch_digest(true) && now == switch_digest(false)), || {
                format!("body(2) was entered for {cause:?}, not for its first read, the switch")
            })?;
            oracle(finished.report.compared == 1, || format!("{} reads looked up: a read after the first that moved was looked up", finished.report.compared))?;
        }
        answer = finished.answer;
    }
    Ok(())
}

/// Expected: "test sim::a_later_read_is_never_looked_up ... ok".
#[test]
fn a_later_read_is_never_looked_up() {
    check(SimConfig::cases(50), |tape| read_order(tape, Plant::None));
}

#[test]
fn control_a_later_read_checked_first_is_caught() {
    control(SimConfig::cases(50), Plant::None, Plant::LaterReadFirst, "", read_order);
}

// ---- Changed and still needed (invariant 10) ----

/// A file emptied: its tasks' rows retire, and the reader of their cells is entered for the
/// read whose definer went.
#[test]
fn a_gone_definer_enters_its_readers() {
    fixture(|| {
        let mut history = History::new(two_crates(1), Plant::None);
        history.request(crates(&[0, 1]), Pools::One)?;
        history.world.files[0].1.clear();
        let step = history.request(crates(&[0, 1]), Pools::One)?;
        let cause = step.causes.iter().find(|one| one.name == body(1)).map(|one| one.cause);
        oracle(cause.is_some_and(|cause| cause.definer_gone()), || format!("body(1) entered for {cause:?}: {:?}", step.causes))?;
        oracle(step.report.retired >= 2, || format!("{} retired", step.report.retired))?;
        let rows = history.answer.as_ref().map_or(0, |answer| [head(0), body(0)].iter().filter(|name| answer.rows.contains_key(*name)).count());
        oracle(rows == 0, || format!("{rows} rows of the emptied file kept"))
    });
}

/// A body that made a constant stops making it in the same edit that moves the constant's read:
/// the constant is retired, never made again (it is not in any plan and nobody reads it).
#[test]
fn a_record_nobody_needs_is_retired_not_remade() {
    fixture(|| {
        let world = World { crates: StdVec::from([StdVec::new()]), files: StdVec::from([(0, StdVec::from([it(0, 0, 3, &[])]))]), next_id: 1 };
        let mut history = History::new(world, Plant::None);
        history.request(crates(&[0]), Pools::One)?;
        let made = history.answer.as_ref().is_some_and(|answer| answer.rows.contains_key(&konst(3)));
        oracle(made, || String::from("body 3 made no constant"))?;
        history.world.files[0].1[0].body = 1;
        history.world.files[0].1.push(it(1, 0, 1, &[]));
        history.world.next_id = 2;
        let step = history.request(crates(&[0]), Pools::One)?;
        oracle(!step.entered.contains(&konst(3)), || format!("the constant nobody needs was made again: {:?}", step.entered))?;
        let kept = history.answer.as_ref().is_some_and(|answer| answer.rows.contains_key(&konst(3)));
        oracle(!kept && step.report.retired >= 1, || format!("the constant kept: {kept}, {} retired", step.report.retired))
    });
}

#[test]
fn control_a_needless_remake_is_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::RemakesUnneeded, "", |tape, plant| histories(tape, plant, EVERY));
}

// ---- The plan grows inside a request (invariant 11, plan row D1) ----

/// A derive's expansion is grown into a head and a body; nothing changed is the certificate
/// without growing; an edit to the derive re-keys them; dropping the derive retires them.
#[test]
fn expansions_grow_and_retire() {
    fixture(|| {
        let world = World { crates: StdVec::from([StdVec::new()]), files: StdVec::from([(0, StdVec::from([it(0, 4, 1, &[])]))]), next_id: 1 };
        let mut history = History::new(world, Plant::None);
        let first = history.request(crates(&[0]), Pools::One)?;
        oracle(first.entered.contains(&genhead(0)) && first.entered.contains(&genbody(0)), || format!("not grown: {:?}", first.entered))?;
        oracle(first.report.grown == 2, || format!("grown {}", first.report.grown))?;
        let quiet = history.request(crates(&[0]), Pools::One)?;
        oracle(quiet.report.certified && quiet.report.grown == 0, || format!("nothing changed: {:?}, {:?}", quiet.entered, quiet.report))?;
        history.world.files[0].1[0].sig = 5;
        let edited = history.request(crates(&[0]), Pools::One)?;
        let cause = edited.causes.iter().find(|one| one.name == genhead(0)).map(|one| one.cause);
        oracle(cause == Some(Cause::KeyMoved), || format!("the edited expansion's head: {cause:?}"))?;
        history.world.files[0].1[0].sig = 8;
        let dropped = history.request(crates(&[0]), Pools::One)?;
        oracle(!dropped.entered.contains(&genhead(0)) && !dropped.entered.contains(&genbody(0)), || format!("a retired growth entered: {:?}", dropped.entered))?;
        let rows = history.answer.as_ref().map_or(0, |answer| [genhead(0), genbody(0)].iter().filter(|name| answer.rows.contains_key(*name)).count());
        oracle(rows == 0 && dropped.report.retired >= 2, || format!("{rows} grown rows kept, {} retired", dropped.report.retired))?;
        let again = history.request(crates(&[0]), Pools::One)?;
        oracle(again.report.certified, || format!("after the retirement: {:?}", again.entered))
    });
}

/// Growth from the switch world's `head(1)` once its phase merged: into a phase passed it is
/// refused, and a grower grows once.
#[test]
fn growing_a_passed_phase_is_refused() {
    let plan = switch_plan(true, 0);
    let files = [(outline(0), plan.as_slice())];
    let tools = crate::engine::Tools([TOOL; key::KINDS]);
    let attempt = |grown: &[Planned], twice: bool| {
        let mut engine = Engine::new_in(tools, Answer::new_in(alloc()), &Edit { files: &files }, Everything, Plant::None, alloc()).expect("planned");
        let mut buffers: StdVec<Buffer<'static>> = (0..1).map(|_| Buffer::new_in(alloc())).collect();
        drive_growing(&mut engine, &mut Pools::One, &mut buffers, &|_, view, task, buffer| switch_work(true, 0, view, task, buffer), &mut |engine| {
            if !engine.merged().contains(&head(1)) {
                return Ok(());
            }
            engine.grow(head(1), grown)?;
            if twice {
                engine.grow(head(1), grown)?;
            }
            Ok(())
        })
    };
    let late = [Planned { name: head(7), phase: Phase::new(Stage::Heads, 0), source: d("sim engine late", &[]), file: head(1) }];
    let refused = attempt(&late, false);
    assert!(matches!(refused, Err(EngineError::GrownLate { name, .. }) if name == head(7)), "growth into the phase that ran: {refused:?}");
    let in_time = [Planned { name: body(7), phase: Phase::new(Stage::Bodies, 0), source: d("sim engine in time", &[]), file: head(1) }];
    let twice = attempt(&in_time, true);
    assert!(matches!(twice, Err(EngineError::GrownTwice { by }) if by == head(1)), "a second growth: {twice:?}");
}

#[test]
fn control_growth_kept_past_its_grower_is_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::GrownNotRetired, "", |tape, plant| histories(tape, plant, EVERY));
}

// ---- Needs made inline, every hit a read (invariants 8 and 12) ----

/// Body 1 makes a constant whose one read (the root crate's names) no body reads. "A hit is ONE
/// read, of the hit record's result; the record keeps its own reads" (the plan's point 6): an
/// item added to crate 0 moves that read, the constant is re-validated for it, its value moves,
/// and the body is entered after it, for the constant's value; a later edit of the body finds the
/// constant standing (a hit) and reads its value alone.
#[test]
fn a_hit_is_one_read() {
    fixture(|| {
        let world = World {
            crates: StdVec::from([StdVec::new(), StdVec::from([0])]),
            files: StdVec::from([(0, StdVec::from([it(0, 0, 1, &[])])), (1, StdVec::from([it(1, 0, 3, &[0])]))]),
            next_id: 2,
        };
        let mut history = History::new(world, Plant::None);
        history.request(crates(&[0, 1]), Pools::One)?;
        history.world.files[0].1.push(it(2, 0, 1, &[]));
        history.world.next_id = 3;
        let value = CellId { of: konst(3), cell: cell(VALUE, 0) };
        let moved = history.request(crates(&[0, 1]), Pools::One)?;
        let cause_of = |step: &Step, name: Name| step.causes.iter().find(|one| one.name == name).map(|one| one.cause);
        let konst_cause = cause_of(&moved, konst(3));
        oracle(matches!(konst_cause, Some(Cause::ReadMoved { read, .. }) if read == konst_read()), || format!("konst(3) entered for {konst_cause:?}, not its own read"))?;
        let cause = cause_of(&moved, body(1));
        oracle(matches!(cause, Some(Cause::ReadMoved { read, was, now }) if read == value && was != now), || format!("body(1) entered for {cause:?}, not the constant's value"))?;
        let at = |name: Name| moved.entered.iter().position(|n| *n == name);
        oracle(matches!((at(konst(3)), at(body(1))), (Some(k), Some(b)) if k < b), || format!("body(1) entered before the constant it reads: {:?}", moved.entered))?;
        history.world.files[1].1[0].body = 15;
        let hit = history.request(crates(&[0, 1]), Pools::One)?;
        oracle(hit.entered.contains(&body(1)) && !hit.entered.contains(&konst(3)), || format!("the standing constant was not a hit: {:?}", hit.entered))?;
        let answer = history.answer.as_ref().ok_or_else(|| Broke::Harness(String::from("no answer")))?;
        let one_read = answer.rows.get(&body(1)).is_some_and(|row| row.reads.iter().any(|(id, _)| *id == value) && row.reads.iter().all(|(id, _)| *id != konst_read()));
        oracle(one_read, || String::from("the hit is not one read of the constant's value"))?;
        let kept = answer.rows.get(&konst(3)).is_some_and(|row| row.reads.iter().any(|(id, _)| *id == konst_read()));
        oracle(kept, || String::from("the constant's record did not keep its own read"))?;
        let quiet = history.request(crates(&[0, 1]), Pools::One)?;
        oracle(quiet.report.certified, || format!("after the hit: {:?}", quiet.entered))
    });
}

#[test]
fn control_a_hit_without_its_reads_is_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::HitWithoutReads, "", |tape, plant| histories(tape, plant, EVERY));
}

// ---- Waves inside a stage (invariant 2, the plan's point 3) ----

/// File 0's plan in the chain world: its outline defines one signature cell; heads 1 to `n`
/// each read the one before (head 1 the outline's cell), all planned at `Heads` wave 0; a body
/// reads the last head.
fn chain_plan(sig: u32, n: u32) -> StdVec<Planned> {
    let file = outline(0);
    let mut plan = StdVec::from([Planned { name: file, phase: Phase::new(Stage::Outlines, 0), source: d("sim engine chain file", &[u64::from(sig)]), file }]);
    for k in 1..=n {
        plan.push(Planned { name: head(k), phase: Phase::new(Stage::Heads, 0), source: d("sim engine chain head", &[u64::from(k)]), file });
    }
    plan.push(Planned { name: body(0), phase: Phase::new(Stage::Bodies, 0), source: d("sim engine chain body", &[]), file });
    plan
}

/// The chain world's tasks: each head's cell is a function of the one cell it reads.
fn chain_work(sig: u32, n: u32, view: View<'_, 'static>, task: &Task, buffer: &mut Buffer<'static>) {
    let name = task.planned.name;
    let al = alloc();
    let mut defines: SVec<'static, (Cell, ContentId)> = Vec::new_in(al);
    let mut reads: SVec<'static, (CellId, ContentId)> = Vec::new_in(al);
    if name == outline(0) {
        defines.push((cell(SIG, 0), d("sim engine chain sig", &[u64::from(sig)])));
    } else {
        let read = match (1..=n).find(|k| head(*k) == name) {
            Some(1) => CellId { of: outline(0), cell: cell(SIG, 0) },
            Some(k) => CellId { of: head(k - 1), cell: cell(HEAD, 0) },
            None => CellId { of: head(n), cell: cell(HEAD, 0) },
        };
        let seen = view.cell(&read);
        reads.push((read, seen));
        if name != body(0) {
            defines.push((cell(HEAD, 0), d("sim engine chain head cell", &[idd(&seen)])));
        }
    }
    let mut parts: StdVec<u64> = reads.iter().map(|(_, seen)| idd(seen)).collect();
    parts.extend(defines.iter().map(|(_, digest)| idd(digest)));
    buffer.done.push(Done { name, key: task.key, out: d("sim engine chain out", &parts), defines, reads, findings: 0, failed: false });
}

/// "Inside Heads and Bodies a row's wave is one more than the highest same-stage row it read,
/// so a head that reads a head ... [is] ordered and never refused or left stale" (the plan's
/// point 3). A chain of 2 to 4 heads, each reading the one before, all planned at wave 0: after
/// every request no row is stale (each read is today's digest); an edit of the outline enters
/// each head once, after the head it reads, then the body; nothing changed is the certificate.
fn same_stage(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let n = 2 + tape.index(3, "heads in the chain") as u32;
    let pool = [Pools::One, Pools::Many(2), Pools::Reversed][tape.index(3, "the pool")];
    let mut answer = Answer::new_in(alloc());
    for (at, sig) in [0u32, 1, 1].into_iter().enumerate() {
        let plan = chain_plan(sig, n);
        let changed = [(outline(0), plan.as_slice())];
        let none: [(Name, &[Planned]); 0] = [];
        let files: &[(Name, &[Planned])] = if at == 2 { &none[..] } else { &changed[..] };
        let finished = direct(answer, files, plant, pool, &|view, task, buffer| chain_work(sig, n, view, task, buffer))
            .map_err(|error| Broke::Oracle(format!("request {at} failed: {error}")))?;
        for (name, row) in finished.answer.rows.iter() {
            for (id, seen) in row.reads.iter() {
                let today = finished.answer.cell(id);
                oracle(today == *seen, || format!("request {at}: {name:?} is stale: it saw {seen:?} of {id:?}, today {today:?}"))?;
            }
        }
        if at == 1 {
            let mut want: StdVec<Name> = (1..=n).map(head).collect();
            want.push(body(0));
            let entered: StdVec<Name> = finished.entered.iter().copied().filter(|name| *name != outline(0)).collect();
            oracle(entered == want, || format!("request 1: entered {entered:?}; each head once, after the head it reads, then the body: {want:?}"))?;
        }
        if at == 2 {
            oracle(finished.report.certified, || format!("request 2: nothing changed and {:?} entered", finished.entered))?;
        }
        answer = finished.answer;
    }
    Ok(())
}

/// Expected: "test sim::a_same_stage_reader_enters_after_its_definer ... ok".
#[test]
fn a_same_stage_reader_enters_after_its_definer() {
    check(SimConfig::cases(50), |tape| same_stage(tape, Plant::None));
}

#[test]
fn control_a_same_stage_reader_left_stale_is_caught() {
    control(SimConfig::cases(50), Plant::None, Plant::SameStageStale, "", same_stage);
}
