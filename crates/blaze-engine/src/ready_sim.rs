//! The ready driver's simulation (ready.rs R1 to R7, kinds.rs K1 to K4): sim.rs's model workspace
//! edited request after request, each request answered three times from its own last answer: by
//! the phase driver (`drive_growing`, sim.rs's `request`), and twice by the ready driver with the
//! completions of each tick returned in an order drawn from the tape. The oracle holds:
//!
//! - R7: the three answers' tables equal, rows with their serial (the phase driver is the oracle
//!   of what the answer is, never of which tasks ran: it enters made rows the ready driver need
//!   not, and some names twice, across waves, so `entered` and `causes` are not compared to it);
//! - R3: the two ready answers equal to the byte, their `entered` order and causes too;
//! - R2, one-sided: the ready driver enters each name at most once a request, and runs each at
//!   most once more than it parked.
//!
//! The generator leans on the case the phase path's waves got wrong (seed 26): a constant read
//! by several bodies whose record goes stale while one of its readers is edited too. Two bodies in
//! three read one of two shared constants (`biased_world`), and half the edits add an item to the
//! root crate, which every constant reads, together with a second text of a body that reads the
//! same constant (`biased_edit`); the other half are sim.rs's edits.
//!
//! Reused from sim.rs: the world generator (`first_world`, `edit`, `POISON`, `Item`), its scope
//! (`Crates`), its tasks (`work`, run over the engine's view and then replayed through the host),
//! its growth (`growth_of`) and the phase driver's request (`request`). Added there for this:
//! `pub(crate)` on `TOOL`, `names` and `growth_of`, so a kind can key and plan as sim.rs does.
//!
//! The controls are typed (D165): [`ReadyControl`] (blaze-sim's `plants!`) names, for each bug,
//! the [`ReadyBroken`] invariant it must trip, and one `controls` call judges them all: a read
//! answered before final (R1, the engine's `ReadAfterMerge`), a Need of a record no kind plans
//! (R5, refused as unmeetable), a merge in completion order (R3), a lost wake (R5, a hang). The
//! driver's bugs reach it through its plant seam ([`ReadyPlant`] as its `Planted` parameter); the
//! never-planned need is the model's own.
//!
//! How the model is a kind: `work` reads today's digests unrecorded (the host's source, which
//! answers as the engine's view does for the phase driver's workers), so its result is computed
//! first and its every read is then asked again through the run's [`Recorded`] host. A read the
//! host refuses makes the run a [`Ran::Need`] (K2); a read the host answers with another digest
//! is a refusal (K1). The host's read list, not `work`'s, is the result's (K1). The same kinds,
//! the same request and the same tables are shell_sim.rs's reference.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_drive::cells::Cell;
use blaze_drive::key::{self, Key, Kind, Name};
use blaze_id::ContentId;
use blaze_own::Vec;
use blaze_sim::{Broke, Broken, Config as SimConfig, Tape, check, controls};

use crate::code_pool::{Time, Worker};
use crate::engine::{Answer, Buffer, CellDigests, CellId, Edit, Engine, EngineError, Finished, Plant as EnginePlant, Planned, Row, SVec, Stage, Stands, Task, Tools};
use crate::kinds::{Ask, Defines, Failure, KindFacts, KindTable, Kinds, Out, Outcome, Ran, Recorded, Refusal, Source, TaskKind};
use crate::overlay::RowFacts;
use crate::ready::{Action, Error, Event, Host, Learned, Limits, Options, Ready, ReadyPlant, Returned, Stepped};
use crate::shell;
use crate::sim::{self, Crates, Item, Pools, World, alloc, oracle};

/// Today's digests, unrecorded: the host's source, which the model computes over before the
/// host replays its reads.
#[derive(Clone, Copy)]
struct Raw<'s>(&'s dyn Source);

impl CellDigests for Raw<'_> {
    fn cell(&self, id: &CellId) -> ContentId {
        self.0.digest(id)
    }

    fn row_failed(&self, name: Name) -> Option<bool> {
        self.0.row(name).map(|row| row.failed)
    }

    fn hit(&self, name: Name, key: Key) -> Option<RowFacts> {
        self.0.hit(name, key)
    }
}

/// One of the model's kinds: sim.rs's tasks of `facts.kind`.
pub(crate) struct Model<'w> {
    world: &'w World,
    facts: KindFacts,
    /// Control (b): every body also reads a cell of a record no kind plans.
    never_planned: bool,
    /// The tasks whose run panics (shell_sim.rs, rule 20): a run of one never returns.
    panics: &'w BTreeSet<Name>,
}

/// The cell control (b)'s bodies read: of a `Test` record, a kind the model never registers.
fn never_planned_cell() -> CellId {
    CellId { of: Name::new(Kind::Test, key::digest("ready sim never planned", &[])), cell: Cell([0; 16]) }
}

impl TaskKind<'static> for Model<'_> {
    fn facts(&self) -> KindFacts {
        self.facts
    }

    fn plan(&self, ask: Ask, out: &mut SVec<'static, Planned>) -> Result<(), Refusal> {
        match ask {
            Ask::Grown { by } if self.facts.kind == Kind::CrateScope => {
                let Some(c) = (0..self.world.crates.len()).find(|c| sim::names(*c) == by) else { return Err(Refusal::Unknown { name: by }) };
                out.extend_from_slice(&sim::growth_of(self.world, c));
                Ok(())
            }
            ask => Err(Refusal::CannotPlan { kind: self.facts.kind, ask }),
        }
    }

    fn run(&self, _cx: &(), src: &Recorded<'_, 'static>, task: &Task, out: &mut Out<'static>) -> Ran<'static> {
        let name = task.planned.name;
        if self.panics.contains(&name) {
            panic!("ready sim: the planted panic of {name:?}'s run");
        }
        let mut buffer = Buffer::new_in(alloc());
        sim::work(self.world, EnginePlant::None, Raw(src.source()), task, &mut buffer);
        if self.never_planned && name.kind == Kind::BodyCheck {
            let _refused = src.cell(&never_planned_cell());
        }
        let Some(done) = buffer.done.pop() else { return Ran::Failed(Refusal::Unknown { name }) };
        // The cells of the records this run made inline: their digests are the run's own.
        let mut made_cells: StdVec<(CellId, ContentId)> = StdVec::new();
        for made in buffer.made.iter() {
            for (cell, digest) in made.done.defines.iter() {
                made_cells.push((CellId { of: made.planned.name, cell: *cell }, *digest));
            }
            // Made because no row stood: the same question asked of the host, which answers it
            // only when the record is final (K2); a row standing now is another answer (K1).
            let Some((cell, _)) = made.done.defines.first() else { continue };
            let asked = CellId { of: made.planned.name, cell: *cell };
            if let Ok(Some(_)) = src.hit(&asked, made.done.key) {
                return Ran::Failed(Refusal::ReadTwoDigests { name, read: asked });
            }
        }
        // The body's own read list, placed as a body check's is (blaze-cli engine_bodies.rs
        // `row_of`, D187): computed over today's digests, then held to the host at once
        // ([`Recorded::placed`]: a read not final is a need, one with another digest is K1).
        let mut placed: SVec<'static, (CellId, ContentId)> = Vec::new_in(alloc());
        for (read, digest) in done.reads.iter() {
            if let Some((_, made)) = made_cells.iter().find(|(id, _)| id == read) {
                if made != digest {
                    return Ran::Failed(Refusal::ReadTwoDigests { name, read: *read });
                }
                src.read_made(*read, *digest);
                continue;
            }
            placed.push((*read, *digest));
        }
        if let Err(refusal) = src.placed(&placed) {
            return Ran::Failed(refusal);
        }
        if let Some(needs) = src.need() {
            return Ran::Need(needs);
        }
        for made in buffer.made.into_iter() {
            let host = src.record();
            for (read, digest) in made.done.reads.iter() {
                match host.cell(read) {
                    Ok(seen) if seen != *digest => return Ran::Failed(Refusal::ReadTwoDigests { name: made.planned.name, read: *read }),
                    Ok(_) | Err(_) => {}
                }
            }
            if let Some(needs) = host.need() {
                return Ran::Need(needs);
            }
            let reads = match host.reads() {
                Ok(reads) => reads,
                Err(refusal) => return Ran::Failed(refusal),
            };
            let outcome = match made.done.failed {
                true => Outcome::failed(made.done.out, Failure::ByReads, reads),
                false => match Defines::sorted(made.planned.name, made.done.defines) {
                    Ok(defines) => Outcome::made(made.done.out, defines, Stands::ByReads, reads),
                    Err(refusal) => return Ran::Failed(refusal),
                },
            };
            out.make(made.planned, made.done.key, outcome, made.done.findings);
        }
        let reads = match src.reads() {
            Ok(reads) => reads,
            Err(refusal) => return Ran::Failed(refusal),
        };
        out.set_findings(done.findings);
        // A failed body stands on its reads (engine.rs invariant 7), as sim.rs's does.
        Ran::Done(match done.failed {
            true => Outcome::failed(done.out, Failure::ByReads, reads),
            false => match Defines::sorted(name, done.defines) {
                Ok(defines) => Outcome::made(done.out, defines, Stands::ByReads, reads),
                Err(refusal) => return Ran::Failed(refusal),
            },
        })
    }
}

/// The model's kinds over `world`: sim.rs's stages, the crate's names growing heads and bodies.
pub(crate) fn models<'w>(world: &'w World, never_planned: bool, panics: &'w BTreeSet<Name>) -> StdVec<Model<'w>> {
    let facts = [
        KindFacts { kind: Kind::FileItems, stage: Stage::Outlines, grows_into: None },
        KindFacts { kind: Kind::CrateScope, stage: Stage::Names, grows_into: Some(Stage::Heads) },
        KindFacts { kind: Kind::Signature, stage: Stage::Heads, grows_into: None },
        KindFacts { kind: Kind::BodyCheck, stage: Stage::Bodies, grows_into: None },
        KindFacts { kind: Kind::Code, stage: Stage::Bodies, grows_into: None },
    ];
    facts.into_iter().map(|facts| Model { world, facts, never_planned, panics }).collect()
}

/// The kinds' facts and their registry's table.
pub(crate) fn table(models: &[Model<'_>]) -> Result<KindTable, Stop> {
    let facts: StdVec<KindFacts> = models.iter().map(|model| model.facts).collect();
    KindTable::new(&facts).map_err(|error| Stop::Harness(format!("{error}")))
}

/// What one ready request ran, per task: runs and parks (R2).
#[derive(Default)]
pub(crate) struct Counts {
    pub(crate) runs: BTreeMap<Name, u32>,
    pub(crate) parks: BTreeMap<Name, u32>,
}

/// Why a ready request did not answer.
pub(crate) enum Stop {
    /// The driver or the engine under it refused: a finding, typed.
    Ready(Error<'static>),
    /// The harness went wrong.
    Harness(String),
}

/// How one ready request is run: its plants, its limits and the tasks whose run panics.
#[derive(Clone, Copy)]
pub(crate) struct Run<'p> {
    pub(crate) planted: Planted,
    pub(crate) limits: Limits,
    /// Answered as the shell answers a run that unwound ([`shell::fault`]).
    pub(crate) panics: &'p BTreeSet<Name>,
}

/// One request through the single-threaded ready driver, each tick's completions in an order
/// the tape draws, every run taking one unit of time: shell_sim.rs's reference too.
pub(crate) fn ready_request(
    world: &World,
    answer: Answer<'static>,
    changed: &BTreeMap<Name, StdVec<Planned>>,
    scope: Crates,
    tape: &Tape,
    run: Run<'_>,
    counts: &mut Counts,
) -> Result<Finished<'static>, Stop> {
    let Run { planted, limits, panics } = run;
    // The model never panics here: a planted panic is answered before the run.
    let none = BTreeSet::new();
    let models = models(world, planted.never_planned, &none);
    let table = table(&models)?;
    let files: StdVec<(Name, &[Planned])> = changed.iter().map(|(name, plan)| (*name, plan.as_slice())).collect();
    let engine = Engine::new_in(Tools([sim::TOOL; key::KINDS]), answer, &Edit { files: &files }, scope, EnginePlant::None, alloc())
        .map_err(|error| Stop::Ready(Error::Engine(error)))?;
    let options = Options { alloc: alloc(), workers: 1 + tape.index(4, "workers"), kinds: table, limits };
    let ready = Ready::new(engine, options, planted.plant).map_err(Stop::Ready)?;
    let mut actions: SVec<'static, Action> = Vec::new_in(alloc());
    let mut running: StdVec<(Worker, Task)> = StdVec::new();
    let mut growing: Option<Name> = None;
    let mut time = 0u64;
    let mut stepped = ready.step(Time(time), Event::Start, &mut actions);
    loop {
        let ready = match stepped {
            Ok(Stepped::Going(ready)) => ready,
            Ok(Stepped::Quiet(quiet)) => return quiet.finish().map_err(|error| Stop::Ready(Error::Engine(error))),
            Err(error) => return Err(Stop::Ready(error)),
        };
        for action in actions.drain(..) {
            match action {
                Action::Run { worker, task } => running.push((worker, task)),
                Action::Grow { by } => growing = Some(by),
            }
        }
        time += 1;
        if let Some(by) = growing.take() {
            let Some(model) = models.iter().find(|model| model.facts.kind == by.kind) else {
                return Err(Stop::Harness(format!("no kind grows {by:?}")));
            };
            let mut planned: SVec<'static, Planned> = Vec::new_in(alloc());
            <Model<'_> as TaskKind<'static>>::plan(model, Ask::Grown { by }, &mut planned).map_err(|refusal| Stop::Harness(format!("{refusal}")))?;
            stepped = ready.step(Time(time), Event::Grown { by, planned: &planned }, &mut actions);
            continue;
        }
        if running.is_empty() {
            return Err(Stop::Harness(String::from("the driver is going with nothing running and nothing to grow")));
        }
        // The shuffle: which running task returns next.
        let (worker, task) = running.swap_remove(tape.index(running.len(), "the run that returns"));
        let name = task.planned.name;
        *counts.runs.entry(name).or_default() += 1;
        let (ran, out, learned) = if panics.contains(&name) {
            let (ran, out) = shell::fault(alloc());
            (ran, out, Learned::nothing(alloc()))
        } else {
            let facts = ready.facts();
            let host = Host::new(&facts);
            let mut out = Out::new_in(alloc());
            let kinds: StdVec<&dyn TaskKind<'static>> = models.iter().map(|model| model as &dyn TaskKind<'static>).collect();
            let kinds = Kinds::new(&kinds).map_err(|error| Stop::Harness(format!("{error}")))?;
            let ran = match kinds.get(name.kind) {
                Some(kind) => {
                    let src = host.recorded(name);
                    let ran = kind.run(&(), &src, &task, &mut out);
                    drop(src);
                    ran
                }
                None => Ran::Failed(Refusal::Unknown { name }),
            };
            (ran, out, host.learned())
        };
        if matches!(ran, Ran::Need(_)) {
            *counts.parks.entry(name).or_default() += 1;
        }
        let returned = Returned { worker, name, ran, out, took: Time(1), learned };
        stepped = ready.step(Time(time), Event::Ran(returned), &mut actions);
    }
}

/// The bugs a run plants: in the driver, through its plant seam, or in the model's kinds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Planted {
    pub(crate) plant: ReadyPlant,
    pub(crate) never_planned: bool,
}

pub(crate) const CLEAN: Planted = Planted { plant: ReadyPlant::None, never_planned: false };

/// The invariants the ready driver's oracle checks; each names its request.
#[derive(Clone, PartialEq, Eq, Debug)]
enum ReadyBroken {
    /// R7: the ready driver's tables or prints against the phase driver's.
    Phase { request: u32, text: String },
    /// R3: two orders of completion gave other bytes, entries or causes.
    Order { request: u32, text: String },
    /// R2: a name entered twice, or run more than once past its parks.
    Twice { request: u32, text: String },
    /// R1: a read answered before its producer was final, as the engine caught it.
    ReadBeforeFinal { request: u32, text: String },
    /// R5: a Need no merge can meet, refused as such.
    Unmeetable { request: u32, text: String },
    /// R5: nothing could run and something was left.
    Hang { request: u32, text: String },
    /// Any other stop of the ready driver.
    Stopped { request: u32, text: String },
}

impl fmt::Display for ReadyBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadyBroken::Phase { request, text } => write!(f, "R7: request {request}: {text}"),
            ReadyBroken::Order { request, text } => write!(f, "R3: request {request}: {text}"),
            ReadyBroken::Twice { request, text } => write!(f, "R2: request {request}: {text}"),
            ReadyBroken::ReadBeforeFinal { request, text } => write!(f, "R1: request {request}: {text}"),
            ReadyBroken::Unmeetable { request, text } => write!(f, "R5: request {request}: {text}"),
            ReadyBroken::Hang { request, text } => write!(f, "R5: request {request}: {text}"),
            ReadyBroken::Stopped { request, text } => write!(f, "request {request}: the ready driver stopped: {text}"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs the ready driver's controls plant.
    enum ReadyControl breaks ReadyBroken {
        /// The driver and the model as they are.
        None,
        /// Control (a): a live row's cells answered as final.
        ReadBeforeMerge => ReadyBroken::ReadBeforeFinal { .. },
        /// Control (b): every body also needs a cell of a record no kind plans.
        NeverPlanned => ReadyBroken::Unmeetable { .. },
        /// Control (c): a tick's results merged one by one in completion order.
        MergeInCompletionOrder => ReadyBroken::Order { .. },
        /// A parked task whose needs are all final never made ready again.
        LostWake => ReadyBroken::Hang { .. },
    }
}

impl ReadyControl {
    fn planted(self) -> Planted {
        match self {
            ReadyControl::None => CLEAN,
            ReadyControl::ReadBeforeMerge => Planted { plant: ReadyPlant::ReadBeforeMerge, never_planned: false },
            ReadyControl::NeverPlanned => Planted { plant: ReadyPlant::None, never_planned: true },
            ReadyControl::MergeInCompletionOrder => Planted { plant: ReadyPlant::MergeInCompletionOrder, never_planned: false },
            ReadyControl::LostWake => Planted { plant: ReadyPlant::LostWake, never_planned: false },
        }
    }
}

/// A stop of the ready driver as the invariant it is.
fn stopped(request: u32, stop: Stop) -> Broken<ReadyBroken> {
    let error = match stop {
        Stop::Ready(error) => error,
        Stop::Harness(why) => return Broken::Harness(format!("request {request}: {why}")),
    };
    let text = format!("{error}");
    Broken::Oracle(match error {
        Error::Engine(EngineError::ReadAfterMerge { .. }) => ReadyBroken::ReadBeforeFinal { request, text },
        Error::Unplanned { .. } | Error::NeedAnswered { .. } => ReadyBroken::Unmeetable { request, text },
        Error::Stalled { .. } | Error::Cycle { .. } | Error::PendingLeft { .. } => ReadyBroken::Hang { request, text },
        _ => ReadyBroken::Stopped { request, text },
    })
}

/// `Ok` when `held`, else the invariant `broken` names.
fn holds(held: bool, broken: impl FnOnce() -> ReadyBroken) -> Result<(), Broken<ReadyBroken>> {
    match held {
        true => Ok(()),
        false => Err(Broken::Oracle(broken())),
    }
}

/// `same_tables` as one of the ready driver's invariants.
fn tables_as(request: u32, what: &str, sides: [&str; 2], one: &Finished<'_>, other: &Finished<'_>, broken: fn(u32, String) -> ReadyBroken) -> Result<(), Broken<ReadyBroken>> {
    same_tables(request, what, sides, one, other).map_err(|broke| match broke {
        Broke::Oracle(text) => Broken::Oracle(broken(request, text)),
        Broke::Harness(text) => Broken::Harness(text),
    })
}

/// Body texts that read a shared constant: `body % 3 == 0` reads `konst(body % 4)` (sim.rs
/// `work`), so 0 and 12 read one constant and 3 and 15 another.
const SHARED: [u32; 4] = [0, 12, 3, 15];

/// The other body text that reads the same constant.
fn partner(body: u32) -> Option<u32> {
    match body {
        0 => Some(12),
        12 => Some(0),
        3 => Some(15),
        15 => Some(3),
        _ => None,
    }
}

/// sim.rs's first world, two bodies in three made readers of a shared constant.
pub(crate) fn biased_world(tape: &Tape) -> World {
    let mut world = sim::first_world(tape);
    for item in world.files.iter_mut().flat_map(|(_, items)| items.iter_mut()) {
        if !tape.one_in(3, "a body of its own") {
            item.body = SHARED[tape.index(SHARED.len(), "the shared constant")];
        }
    }
    world
}

/// Half the time a constant and one of its readers edited together, else sim.rs's edit. Every
/// constant reads the root crate's names (sim.rs `konst_read`): an item added to a file of crate
/// 0 moves them, so each constant's record is stale; a reader of a constant takes its partner
/// text, so its key moves and it reads the same constant again in the same request.
pub(crate) fn biased_edit(tape: &Tape, world: &World) -> World {
    if !tape.one_in(2, "a constant and its reader edited together") {
        return sim::edit(tape, world);
    }
    let mut next = world.clone();
    let roots: StdVec<usize> = (0..next.files.len()).filter(|f| next.files[*f].0 == 0).collect();
    if !roots.is_empty() {
        let f = roots[tape.index(roots.len(), "the root file edited")];
        let id = next.next_id;
        next.next_id += 1;
        let body = SHARED[tape.index(SHARED.len(), "the new item's constant")];
        next.files[f].1.push(Item { id, sig: tape.index(4, "sig") as u32, body, refs: StdVec::new() });
    }
    let readers: StdVec<(usize, usize)> = next
        .files
        .iter()
        .enumerate()
        .flat_map(|(f, (_, items))| items.iter().enumerate().filter(|(_, item)| partner(item.body).is_some()).map(move |(i, _)| (f, i)))
        .collect();
    if !readers.is_empty() {
        let (f, i) = readers[tape.index(readers.len(), "the reader edited")];
        let item = &mut next.files[f].1[i];
        if let Some(other) = partner(item.body) {
            item.body = other;
        }
    }
    next
}

/// Two answers' tables, named where they differ: the one comparison R7 and R3 make.
pub(crate) fn same_tables(serial: u32, what: &str, sides: [&str; 2], one: &Finished<'_>, other: &Finished<'_>) -> Result<(), Broke> {
    let (a, b) = (&one.answer, &other.answer);
    if a.rows != b.rows {
        return Err(Broke::Oracle(format!("request {serial}: {what}: the rows differ\n{}", rows_evidence(sides, one, other))));
    }
    for (table, equal) in [
        ("rows", a.rows == b.rows),
        ("cells", a.cells == b.cells),
        ("readers", a.readers == b.readers),
        ("files", a.by_file == b.by_file),
        ("owed cells", a.owed_cells == b.owed_cells),
        ("owed tasks", a.owed_tasks == b.owed_tasks),
        ("rows with findings", a.with_findings == b.with_findings),
        ("serial", a.serial == b.serial),
    ] {
        oracle(equal, || format!("request {serial}: {what}: the {table} differ"))?;
    }
    Ok(())
}

/// The rows that differ shown to act on (rule 19): counts (only on one side, on both and
/// different), then the first five by name, each field of each side with `*` on those that
/// differ, and each side's entries and causes for those names, with its `entered` length.
fn rows_evidence(sides: [&str; 2], one: &Finished<'_>, other: &Finished<'_>) -> String {
    let (a, b) = (&one.answer.rows, &other.answer.rows);
    let names: BTreeSet<Name> = a.keys().chain(b.keys()).copied().collect();
    let (mut only_a, mut only_b, mut changed) = (0u32, 0u32, 0u32);
    let mut shown: StdVec<Name> = StdVec::new();
    for name in names.iter() {
        let differs = match (a.get(name), b.get(name)) {
            (Some(_), None) => {
                only_a += 1;
                true
            }
            (None, Some(_)) => {
                only_b += 1;
                true
            }
            (Some(x), Some(y)) if x != y => {
                changed += 1;
                true
            }
            _ => false,
        };
        if differs && shown.len() < 5 {
            shown.push(*name);
        }
    }
    let [sa, sb] = sides;
    let mut text = format!(
        "  {only_a} rows only in the {sa} table, {only_b} only in the {sb} table, {changed} in both and different; {sa} entered {}, {sb} entered {}\n",
        one.entered.len(),
        other.entered.len()
    );
    for name in shown.iter() {
        text.push_str(&format!("  {name:?}:\n"));
        let fa = a.get(name).map(row_fields);
        let fb = b.get(name).map(row_fields);
        match (&fa, &fb) {
            (Some(fa), Some(fb)) => {
                for ((label, x), (_, y)) in fa.iter().zip(fb.iter()) {
                    match x == y {
                        true => text.push_str(&format!("      {label}: {x}\n")),
                        false => text.push_str(&format!("    * {label}: {sa} {x} | {sb} {y}\n")),
                    }
                }
            }
            _ => {
                for (side, fields) in [(sa, &fa), (sb, &fb)] {
                    match fields {
                        Some(fields) => {
                            text.push_str(&format!("    * {side}: present\n"));
                            for (label, x) in fields.iter() {
                                text.push_str(&format!("        {label}: {x}\n"));
                            }
                        }
                        None => text.push_str(&format!("    * {side}: no row\n")),
                    }
                }
            }
        }
        for (side, finished) in [(sa, one), (sb, other)] {
            let causes: StdVec<String> = finished.causes.iter().filter(|entered| entered.name == *name).map(|entered| format!("{:?} at {:?} identical {}", entered.cause, entered.phase, entered.identical)).collect();
            text.push_str(&format!("      {side} entered it {} times: {causes:?}\n", causes.len()));
        }
    }
    text
}

/// A row's fields as the evidence prints them, in a fixed order (`by_cell` is a function of
/// `reads` and left out).
fn row_fields(row: &Row<'_>) -> StdVec<(&'static str, String)> {
    StdVec::from([
        ("key", format!("{:?}", row.key)),
        ("phase", format!("{:?}", row.phase)),
        ("file", format!("{:?}", row.file)),
        ("source", format!("{:?}", row.source)),
        ("out", format!("{:?}", row.out)),
        ("serial", format!("{:?}", row.serial)),
        ("reads", format!("{:?}", row.reads.as_slice())),
        ("defines", format!("{:?}", row.defines.as_slice())),
        ("findings", format!("{}", row.findings)),
        ("inline", format!("{}", row.inline)),
        ("failed", format!("{}", row.failed)),
    ])
}

/// A history of requests, each answered by the phase driver and twice by the ready driver.
fn histories(tape: &Tape, planted: Planted) -> Result<(), Broken<ReadyBroken>> {
    let mut world = biased_world(tape);
    let crate_of = Crates::of(&world, BTreeSet::new()).crate_of;
    let everything: BTreeSet<usize> = (0..world.crates.len()).collect();
    let mut phase_answer = Some(Answer::new_in(alloc()));
    let mut ready_answer = Some(Answer::new_in(alloc()));
    let mut again_answer = Some(Answer::new_in(alloc()));
    let mut last_plans: BTreeMap<Name, StdVec<Planned>> = BTreeMap::new();
    for serial in 0..2 + tape.below(7, "requests") as u32 {
        if serial > 0 {
            world = biased_edit(tape, &world);
        }
        let asked: BTreeSet<usize> = match tape.one_in(2, "a narrower scope") {
            true => everything.iter().copied().filter(|_| tape.one_in(2, "a crate asked")).collect(),
            false => everything.clone(),
        };
        if tape.one_in(4, "a body poisoned") {
            let count: usize = world.files.iter().map(|(_, items)| items.len()).sum();
            if count > 0 {
                let at = tape.index(count, "the poisoned body");
                if let Some(item) = world.files.iter_mut().flat_map(|(_, items)| items.iter_mut()).nth(at) {
                    item.body = sim::POISON;
                }
            }
        }
        let scope = Crates { asked, crate_of: crate_of.clone() };
        let plans = world.plans();
        let changed: BTreeMap<Name, StdVec<Planned>> =
            plans.iter().filter(|(name, plan)| last_plans.get(*name) != Some(*plan)).map(|(name, plan)| (*name, plan.clone())).collect();
        last_plans = plans;
        let take = |answer: &mut Option<Answer<'static>>| answer.take().ok_or_else(|| Broken::Harness(String::from("no answer")));
        let phase = sim::request(&world, take(&mut phase_answer)?, &changed, scope.clone(), EnginePlant::None, Pools::One)
            .map_err(|error| Broken::Harness(format!("request {serial}: the phase driver failed: {error}")))?;
        let mut counts = Counts::default();
        let mut again_counts = Counts::default();
        let none = BTreeSet::new();
        let run = Run { planted, limits: Limits::SHIPPED, panics: &none };
        let ready = ready_request(&world, take(&mut ready_answer)?, &changed, scope.clone(), tape, run, &mut counts).map_err(|stop| stopped(serial, stop))?;
        let again = ready_request(&world, take(&mut again_answer)?, &changed, scope.clone(), tape, run, &mut again_counts).map_err(|stop| stopped(serial, stop))?;
        // R7: the ready driver's answer is the phase driver's, rows with their serial. Which
        // tasks each entered, and why, is not compared: that is each driver's own.
        tables_as(serial, "the ready driver against the phase driver", ["ready", "phase"], &ready, &phase, |request, text| ReadyBroken::Phase { request, text })?;
        // R3: two orders of completion give equal bytes, the order of entry and the causes too.
        tables_as(serial, "two orders of completion", ["first order", "second order"], &ready, &again, |request, text| ReadyBroken::Order { request, text })?;
        holds(ready.entered == again.entered, || ReadyBroken::Order {
            request: serial,
            text: format!("two orders of completion entered {:?} and {:?}", ready.entered, again.entered),
        })?;
        holds(ready.causes == again.causes, || ReadyBroken::Order { request: serial, text: String::from("two orders of completion gave other causes") })?;
        // R2, one-sided: each name entered at most once a request by the ready driver, and run at
        // most once more than a Need parked it.
        for (counted, finished) in [(&counts, &ready), (&again_counts, &again)] {
            let mut once: BTreeSet<Name> = BTreeSet::new();
            for name in finished.entered.iter() {
                holds(once.insert(*name), || ReadyBroken::Twice { request: serial, text: format!("the ready driver entered {name:?} twice") })?;
            }
            for (name, runs) in counted.runs.iter() {
                let parks = counted.parks.get(name).copied().unwrap_or(0);
                holds(*runs <= parks + 1, || ReadyBroken::Twice { request: serial, text: format!("{name:?} ran {runs} times and parked {parks}") })?;
            }
        }
        // The findings each driver prints for the scope (engine.rs invariant 6).
        let here: StdVec<(Name, u32)> = ready.answer.prints(&scope, alloc().local()).iter().copied().collect();
        let there: StdVec<(Name, u32)> = phase.answer.prints(&scope, alloc().local()).iter().copied().collect();
        holds(here == there, || ReadyBroken::Phase { request: serial, text: format!("the ready driver prints {here:?}, the phase driver {there:?}") })?;
        phase_answer = Some(phase.answer);
        ready_answer = Some(ready.answer);
        again_answer = Some(again.answer);
    }
    Ok(())
}

/// Expected: "test ready_sim::ready_answers_as_the_phase_driver ... ok".
#[test]
fn ready_answers_as_the_phase_driver() {
    check(SimConfig::cases(300), |tape| histories(tape, CLEAN));
}

/// Each plant of [`ReadyControl`] is caught as the invariant it names, and the clean run passes:
/// (a) a live row's cells answered as final, caught by the engine's one check
/// (`EngineError::ReadAfterMerge`) as R1; (b) a Need whose producer no kind plans, refused by a
/// typed error naming it (R5), never a hang; (c) results merged one by one in completion order,
/// whose `entered` differs for two shuffles of one request (R3); and a lost wake, a stall (R5).
#[test]
fn every_ready_plant_is_caught_by_its_invariant() {
    controls(SimConfig::cases(300), |tape, plant: ReadyControl| histories(tape, plant.planted()));
}
