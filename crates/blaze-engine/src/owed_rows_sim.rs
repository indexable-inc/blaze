//! Owed rows (engine.rs invariants 7 and 13): a record right for one request only is kept, used
//! and owed; a failure stands by its key, by its reads, or is owed, as its stage says, never by
//! the length of its read list; the readers of an owed row are covered by its cell's digest and
//! are never owed for it.
//!
//! The model is one file of seven tasks over a world that also holds what no cell names:
//!
//! - `U` (Units) defines a cell from its text.
//! - `S0`, `S1` (Scripts): build scripts. `S0` reads `U`'s cell, `S1` reads nothing. A script
//!   whose text is odd asks git (`World::git`, read by no cell): its record is right for this
//!   request only, and its stage lists it ([`Stands::OwedNextRequest`]).
//! - `F` (Heads): a fetch, its stage saying how each failure stands. A machine fault this
//!   request (`World::fault`) is a failure on no read list, pushed [`Stands::OwedNextRequest`];
//!   the text 3 does not parse, a failure on no read list pushed [`Stands::ByKey`]; otherwise
//!   it reads `U` and fails, on that read ([`Stands::ByReads`]), when `U`'s text is odd.
//! - `R0`, `R1`, `R2` (Bodies): readers of `S0`; of `S1`; of `S0`, `S1` and `U`.
//!
//! The property (`owed_rows_hold_over_histories`), each request of a generated history run twice,
//! on one worker and on many or reversed:
//!
//! - THE OWED TASKS: the answer's owed tasks are exactly the model's: a task entered is owed when
//!   it was answered right for this request only (a script that asked git, `F` on a fault); a
//!   task the scope did not run is owed when its row is not under today's key (invariant 5);
//!   any other keeps what it had. `Report::owed_rows` counts those under today's key (c: a
//!   narrow request that does not reach an owed row leaves it owed).
//! - CERTIFIED: a request holding an owed task (a row or not) is never certified, and one that
//!   entered nothing and holds none is (b). Every owed task is held under today's plan: a key
//!   that moved while it was held, and moved back, leaves an owed ROW under today's key (the
//!   kept tape of 2026-11-18).
//! - RE-ENTERED: every task owed after the last request that this request's scope runs is
//!   entered, with [`Cause::Owed`] when its text and its reads stand (a, e); every task entered
//!   for `Owed` was owed (f: no reader is owed through what it read).
//! - FAILURES ON READS STAND: `F` failed on its read and nothing it read or its text moved: not
//!   entered (e).
//! - FAILURES BY KEY STAND: `F` refused by its key is not owed, is not entered while its text
//!   stands whatever else moved, and is entered for `KeyMoved` when the text moves.
//! - FRESH: after a request whose scope runs everything, every row equals a fresh build's of the
//!   same world (no fault), but `F` while it is owed for a fault (f: the digest covers the
//!   readers of an owed row; nothing else is needed for them to be right).
//! - ORDER: both runs' tables, reports, causes and stored pages' bytes are equal (d, rule 25).
//!
//! Controls, each caught by the words it names: `Plant::OwedBeforeUnowe` ("the owed tasks"),
//! `Plant::OwedRowCertified` ("certified while holding"), `Plant::PushedOwedLeftStanding` ("the
//! owed tasks").

use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::println;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_drive::cells::Cell;
use blaze_drive::key::{self, Key, Kind, Name, Tool};
use blaze_id::ContentId;
use blaze_own::Vec;
use blaze_sim::{Broke, Config as SimConfig, Tape, control};

use crate::engine::{
    Answer, Buffer, Cause, CellId, Done, Edit, Engine, EngineError, Entered, Finished, Phase, Plant, Planned, Pool, SVec, Scope, Stage, Stands, Task, Tools, View, drive,
};
use crate::last::LastAnswer;
use crate::pages::Kept;
use crate::sim::{Pools, alloc, oracle};

const TOOL: Tool = Tool(ContentId([101; 32]));
const FILE: Name = Name::new(Kind::FileItems, ContentId([102; 32]));
const U: Name = Name::new(Kind::Units, ContentId([103; 32]));
const S0: Name = Name::new(Kind::Script, ContentId([104; 32]));
const S1: Name = Name::new(Kind::Script, ContentId([105; 32]));
const F: Name = Name::new(Kind::Fetch, ContentId([106; 32]));
const R0: Name = Name::new(Kind::BodyCheck, ContentId([107; 32]));
const R1: Name = Name::new(Kind::BodyCheck, ContentId([108; 32]));
const R2: Name = Name::new(Kind::BodyCheck, ContentId([109; 32]));
const NAMES: [Name; 7] = [U, S0, S1, F, R0, R1, R2];
const WHOLE: Cell = Cell([1; 16]);

fn whole(of: Name) -> CellId {
    CellId { of, cell: WHOLE }
}

/// The texts, and what no cell names: git's answer and a fault this request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct World {
    units: u8,
    scripts: [u8; 2],
    git: u8,
    fetch: u8,
    fault: bool,
    readers: [u8; 3],
}

impl World {
    /// A script asks git when its text is odd: a function of its text, as a real script's
    /// questions are of its source.
    fn asks(&self, i: usize) -> bool {
        self.scripts[i] % 2 == 1
    }

    /// The fetch's text 3 does not parse: `F` refuses it, on no read, by its key alone.
    fn refuses(&self) -> bool {
        self.fetch == 3
    }

    /// `name`'s text: what its source, and so its key, is made from.
    fn text(&self, name: Name) -> u8 {
        match name {
            n if n == U => self.units,
            n if n == S0 => self.scripts[0],
            n if n == S1 => self.scripts[1],
            n if n == F => self.fetch,
            n if n == R0 => self.readers[0],
            n if n == R1 => self.readers[1],
            _ => self.readers[2],
        }
    }

    fn source(&self, name: Name) -> ContentId {
        key::digest("owed_rows_sim source", &[name.subject.0[0], self.text(name)])
    }

    fn plan(&self) -> [Planned; 7] {
        NAMES.map(|name| {
            let stage = match name.kind {
                Kind::Units => Stage::Units,
                Kind::Script => Stage::Scripts,
                Kind::Fetch => Stage::Heads,
                _ => Stage::Bodies,
            };
            Planned { name, phase: Phase::new(stage, 0), source: self.source(name), file: FILE }
        })
    }
}

/// What a request runs: Units always; scripts, the fetch and the readers as asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Runs {
    scripts: bool,
    fetch: bool,
    readers: bool,
}

const ALL: Runs = Runs { scripts: true, fetch: true, readers: true };

impl Scope for Runs {
    fn runs(&self, task: &Planned) -> bool {
        match task.name.kind {
            Kind::Script => self.scripts,
            Kind::Fetch => self.fetch,
            Kind::BodyCheck => self.readers,
            _ => true,
        }
    }
}

/// The model's tasks over the world and the view.
fn work<L: LastAnswer>(world: &World, view: View<'_, 'static, L>, task: &Task, buffer: &mut Buffer<'static>) {
    let a = alloc();
    let name = task.planned.name;
    let mut reads: SVec<'static, (CellId, ContentId)> = Vec::new_in(a);
    let mut defines: SVec<'static, (Cell, ContentId)> = Vec::new_in(a);
    let mut parts: StdVec<u8> = StdVec::from([world.text(name)]);
    let mut read = |id: CellId, parts: &mut StdVec<u8>| {
        let seen = view.cell(&id);
        reads.push((id, seen));
        parts.extend_from_slice(&seen.0);
    };
    if name == F {
        if world.fault {
            // A machine fault: a failure on no read list that no key covers, so the stage says
            // it is owed.
            let out = key::digest("owed_rows_sim fault", &parts);
            buffer.push(Done { name, key: task.key, out, defines, reads, findings: 1, failed: true }, Stands::OwedNextRequest);
            return;
        }
        if world.refuses() {
            // A parse refusal of its own text: a failure on no read list that its key covers.
            let out = key::digest("owed_rows_sim refusal", &parts);
            buffer.push(Done { name, key: task.key, out, defines, reads, findings: 1, failed: true }, Stands::ByKey);
            return;
        }
        read(whole(U), &mut parts);
        // A failure that is a function of its read: `U`'s cell is its text's.
        let failed = world.units % 2 == 1;
        let out = key::digest("owed_rows_sim fetch", &parts);
        if !failed {
            defines.push((WHOLE, out));
        }
        buffer.push(Done { name, key: task.key, out, defines, reads, findings: u32::from(failed), failed }, Stands::ByReads);
        return;
    }
    let mut stands = Stands::ByReads;
    match name {
        n if n == U => {}
        n if n == S0 || n == S1 => {
            let i = usize::from(n == S1);
            if i == 0 {
                read(whole(U), &mut parts);
            }
            if world.asks(i) {
                // A read no cell names: right for this request only.
                parts.push(world.git);
                stands = Stands::OwedNextRequest;
            }
        }
        n if n == R0 => read(whole(S0), &mut parts),
        n if n == R1 => read(whole(S1), &mut parts),
        _ => {
            read(whole(S0), &mut parts);
            read(whole(S1), &mut parts);
            read(whole(U), &mut parts);
        }
    }
    let out = key::digest("owed_rows_sim out", &parts);
    defines.push((WHOLE, out));
    buffer.push(Done { name, key: task.key, out, defines, reads, findings: 0, failed: false }, stands);
}

/// One request over `last`.
fn one(last: Answer<'static>, world: World, scope: Runs, mut pool: Pools, plant: Plant) -> Result<Finished<'static>, EngineError> {
    let plan = world.plan();
    let files: [(Name, &[Planned]); 1] = [(FILE, &plan[..])];
    let mut engine = Engine::new_in(Tools([TOOL; key::KINDS]), last, &Edit { files: &files }, scope, plant, alloc())?;
    let mut buffers: StdVec<Buffer<'static>> = (0..3).map(|_| Buffer::new_in(alloc())).collect();
    drive(&mut engine, &mut pool, &mut buffers, &|_, view, task, buffer| work(&world, view, task, buffer))?;
    engine.finish()
}

/// A row as the oracle compares it: everything but the request that made it.
type Seen = (Key, Phase, ContentId, StdVec<(Cell, ContentId)>, StdVec<(CellId, ContentId)>, u32, bool);

fn seen(answer: &Answer<'_>) -> BTreeMap<Name, Seen> {
    answer
        .rows
        .iter()
        .map(|(name, row)| (*name, (row.key, row.phase, row.out, row.defines.iter().copied().collect(), row.reads.iter().copied().collect(), row.findings, row.failed)))
        .collect()
}

/// The answer and its stored pages' root (pages.rs), the answer handed back for the next request.
fn stored(answer: Answer<'static>, causes: &[Entered]) -> (Answer<'static>, Key) {
    let kept = Kept::after(blaze_own::BTreeMap::new_in(alloc()), answer, causes, blaze_own::BTreeMap::new_in(alloc()));
    let root = crate::pages::encode(&kept, None, alloc()).root;
    (kept.answer, root)
}

/// The model's owed set and, for each task, the world it was last entered in.
struct Model {
    owed: BTreeSet<Name>,
    ran: BTreeMap<Name, World>,
}

impl Model {
    /// Whether `name`'s row is under `world`'s key: it was entered and its text is today's.
    fn keyed(&self, name: Name, world: &World) -> bool {
        self.ran.get(&name).is_some_and(|ran| ran.source(name) == world.source(name))
    }

    /// Whether `name`, owed and reached in `world`, must be entered for `Owed`: its row is under
    /// today's key and nothing it read moved.
    fn owed_cause(&self, name: Name, world: &World) -> bool {
        let Some(ran) = self.ran.get(&name) else { return false };
        if ran.source(name) != world.source(name) {
            return false;
        }
        match name {
            n if n == S0 => ran.units == world.units,
            n if n == S1 => true,
            // A fault and a refusal read nothing; else it read `U`.
            n if n == F => ran.fault || ran.refuses() || ran.units == world.units,
            // A reader owed by its own key's history: its reads are not modelled.
            _ => false,
        }
    }

    /// The request's effect: the tasks it entered, and those its scope held out off today's key.
    fn after(&mut self, world: &World, scope: &Runs, entered: &[Entered]) {
        let entered_names: BTreeSet<Name> = entered.iter().map(|one| one.name).collect();
        for planned in world.plan().iter() {
            let name = planned.name;
            if entered_names.contains(&name) {
                let owed = match name {
                    n if n == S0 => world.asks(0),
                    n if n == S1 => world.asks(1),
                    n if n == F => world.fault,
                    _ => false,
                };
                match owed {
                    true => self.owed.insert(name),
                    false => self.owed.remove(&name),
                };
                self.ran.insert(name, *world);
            } else if !scope.runs(planned) && !self.keyed(name, world) {
                self.owed.insert(name);
            }
        }
    }
}

fn first_world(tape: &Tape) -> World {
    World {
        units: tape.below(4, "units") as u8,
        scripts: [tape.below(4, "script 0") as u8, tape.below(4, "script 1") as u8],
        git: tape.below(3, "git") as u8,
        fetch: tape.below(4, "fetch") as u8,
        fault: tape.one_in(3, "a fault"),
        readers: [tape.below(3, "reader 0") as u8, tape.below(3, "reader 1") as u8, tape.below(3, "reader 2") as u8],
    }
}

fn edit(tape: &Tape, world: &World) -> World {
    let mut next = *world;
    // A quiet request: nothing edited, nothing asked of git moved, no fault.
    if tape.one_in(3, "a quiet request") {
        next.fault = false;
        return next;
    }
    if tape.one_in(4, "units edited") {
        next.units = tape.below(4, "units") as u8;
    }
    for i in 0..2 {
        if tape.one_in(4, "a script edited") {
            next.scripts[i] = tape.below(4, "its text") as u8;
        }
    }
    if tape.one_in(2, "git moved") {
        next.git = tape.below(3, "git") as u8;
    }
    if tape.one_in(5, "fetch edited") {
        next.fetch = tape.below(4, "fetch") as u8;
    }
    next.fault = tape.one_in(3, "a fault");
    for j in 0..3 {
        if tape.one_in(5, "a reader edited") {
            next.readers[j] = tape.below(3, "its text") as u8;
        }
    }
    next
}

/// One history, every oracle after every request.
fn histories(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = first_world(tape);
    let mut ours: Answer<'static> = Answer::new_in(alloc());
    let mut other: Answer<'static> = Answer::new_in(alloc());
    let mut model = Model { owed: BTreeSet::new(), ran: BTreeMap::new() };
    for k in 0..2 + tape.below(9, "requests") {
        if k > 0 {
            world = edit(tape, &world);
        }
        let scope = match tape.one_in(2, "a narrower scope") {
            true => Runs { scripts: tape.bool("scripts run"), fetch: tape.bool("the fetch runs"), readers: tape.bool("readers run") },
            false => ALL,
        };
        let pool = [Pools::Many(1 + tape.index(3, "workers")), Pools::Reversed][tape.index(2, "the other pool")];
        let a = one(ours, world, scope, Pools::One, plant).map_err(|error| Broke::Oracle(format!("request {k} failed: {error}")))?;
        let b = one(other, world, scope, pool, plant).map_err(|error| Broke::Oracle(format!("request {k} on {pool:?} failed: {error}")))?;
        let was = Model { owed: model.owed.clone(), ran: model.ran.clone() };
        model.after(&world, &scope, &a.causes);

        // The owed tasks, and the owed rows among them (a, c, e).
        let owed: BTreeSet<Name> = a.answer.owed_tasks.keys().copied().collect();
        oracle(owed == model.owed, || format!("request {k} ({world:?}, {scope:?}): the owed tasks are {owed:?}, the model's {:?}; entered {:?}", model.owed, a.causes))?;
        let rows = u32::try_from(model.owed.iter().filter(|name| model.keyed(**name, &world)).count()).unwrap_or(u32::MAX);
        oracle(a.report.owed_rows == rows, || format!("request {k}: {} owed rows reported, {rows} in the model", a.report.owed_rows))?;

        // Each owed task is held under today's plan: a key that moved while it was held re-plans
        // it (the kept tape of 2026-11-18: an edit out of scope and back).
        for (name, planned) in a.answer.owed_tasks.iter() {
            oracle(planned.source == world.source(*name), || format!("request {k}: {name:?} is held under the owed plan of another text ({planned:?})"))?;
        }

        // Certified (b): never while an owed task is held; always when nothing entered and none is.
        oracle(!(a.report.certified && !model.owed.is_empty()), || format!("request {k}: certified while holding {} owed task(s), {rows} of them rows: {:?}", model.owed.len(), model.owed))?;
        oracle(!(a.entered.is_empty() && model.owed.is_empty()) || a.report.certified, || format!("request {k}: nothing entered, nothing owed, and not certified: {:?}", a.report))?;

        // Re-entered (a, e, f).
        let plan = world.plan();
        for name in was.owed.iter() {
            let Some(planned) = plan.iter().find(|planned| planned.name == *name) else { continue };
            if !scope.runs(planned) {
                continue;
            }
            let entered = a.causes.iter().find(|one| one.name == *name);
            oracle(entered.is_some(), || format!("request {k}: {name:?} was owed, the scope runs it, and it was not entered: {:?}", a.causes))?;
            if was.owed_cause(*name, &world) {
                oracle(entered.is_some_and(|one| one.cause == Cause::Owed), || format!("request {k}: {name:?} was owed with nothing moved and entered for {entered:?}"))?;
            }
        }
        for one in a.causes.iter().filter(|one| one.cause == Cause::Owed) {
            oracle(was.owed.contains(&one.name), || format!("request {k}: {:?} entered for Owed and never owed: owed through what it read", one.name))?;
        }

        // A failure on its read stands until the read or its text moves (e).
        if let Some(ran) = was.ran.get(&F) {
            let on_reads = !ran.fault && !ran.refuses() && ran.units % 2 == 1;
            let stood = ran.units == world.units && ran.fetch == world.fetch && !was.owed.contains(&F);
            if on_reads && stood && scope.fetch {
                oracle(!a.entered.contains(&F), || format!("request {k}: the fetch failed on its read, nothing moved, and it was entered again"))?;
            }
            // A failure its key covers stands until the key moves, whatever else moved (the
            // MacroModule class in blaze-cli: a failure on no read list that is not a hole).
            let by_key = !ran.fault && ran.refuses() && !was.owed.contains(&F);
            if by_key && scope.fetch {
                let entered = a.causes.iter().find(|one| one.name == F);
                match ran.fetch == world.fetch {
                    true => oracle(entered.is_none(), || format!("request {k}: the fetch was refused by its key, the key stood, and it was entered for {entered:?}"))?,
                    false => oracle(entered.is_some_and(|one| one.cause == Cause::KeyMoved), || format!("request {k}: the fetch was refused by its key, the key moved, and it was entered for {entered:?}"))?,
                }
            }
        }

        // Fresh (f): a request that runs everything answers as a fresh build of the same world.
        if scope == ALL {
            let calm = World { fault: false, ..world };
            let fresh = one(Answer::new_in(alloc()), calm, ALL, Pools::One, Plant::None).map_err(|error| Broke::Harness(format!("a fresh build failed: {error}")))?;
            let (mut mine, theirs) = (seen(&a.answer), seen(&fresh.answer));
            // `F` owed holds this request's fault; a refusal by its key is compared like any row.
            if a.answer.owed_tasks.contains_key(&F) {
                mine.remove(&F);
            }
            for (name, row) in mine.iter() {
                oracle(theirs.get(name) == Some(row), || format!("request {k} ({world:?}): {name:?} differs from a fresh build: {row:?} against {:?}", theirs.get(name)))?;
            }
        }

        // Order (d): the other pool's run is the same bytes.
        oracle(a.report == b.report, || format!("request {k}: the pools' reports differ: {:?} and {:?}", a.report, b.report))?;
        oracle(a.causes[..] == b.causes[..] && a.entered[..] == b.entered[..], || format!("request {k}: the pools entered otherwise"))?;
        let (x, y) = (&a.answer, &b.answer);
        for (table, equal) in [
            ("rows", x.rows == y.rows),
            ("cells", x.cells == y.cells),
            ("reverse index", x.readers == y.readers),
            ("files", x.by_file == y.by_file),
            ("owed cells", x.owed_cells == y.owed_cells),
            ("owed tasks", x.owed_tasks == y.owed_tasks),
            ("rows with findings", x.with_findings == y.with_findings),
        ] {
            oracle(equal, || format!("request {k}: the pools' {table} differ"))?;
        }
        let (a_answer, a_root) = stored(a.answer, &a.causes);
        let (b_answer, b_root) = stored(b.answer, &b.causes);
        oracle(a_root == b_root, || format!("request {k}: the pools' stored pages differ: {a_root:?} and {b_root:?}"))?;
        ours = a_answer;
        other = b_answer;
    }
    Ok(())
}

/// Expected: "test owed_rows_sim::owed_rows_hold_over_histories ... ok"; the kept tapes
/// (sim-kept.tapes) replayed first.
#[test]
fn owed_rows_hold_over_histories() {
    blaze_sim::check_kept(SimConfig::cases(300), blaze_sim::sim_kept!(), |tape| histories(tape, Plant::None));
}

/// Expected: ok, and on standard error "plant OwedBeforeUnowe: caught at case k of 300".
#[test]
fn control_owed_before_unowe_is_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::OwedBeforeUnowe, "the owed tasks", histories);
}

/// Expected: ok, and on standard error "plant OwedRowCertified: caught at case k of 300".
#[test]
fn control_an_owed_row_certified_is_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::OwedRowCertified, "certified while holding", histories);
}

/// Expected: ok, and on standard error "plant PushedOwedLeftStanding: caught at case k of 300".
#[test]
fn control_a_result_pushed_owed_left_standing_is_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::PushedOwedLeftStanding, "the owed tasks", histories);
}

fn fixture(run: impl FnOnce() -> Result<(), Broke>) {
    if let Err(broke) = run() {
        panic!("{broke:?}");
    }
}

fn said(finished: &Finished<'static>) -> String {
    let causes: StdVec<String> = finished.causes.iter().map(|one| format!("{:?} {} identical {}", one.name.subject.0[0], one.cause.kind(), one.identical)).collect();
    format!("entered {causes:?}, owed rows {}, certified {}", finished.report.owed_rows, finished.report.certified)
}

/// The coordinator's real run in small: two scripts that ask git. Request 2, nothing moved:
/// the two scripts entered again for `Owed`, identical, and nothing else; not certified. Request
/// 3, git moved: the scripts and their readers. Request 4, the scripts no longer ask: entered
/// for their keys, no owed row. Request 5: the certificate.
///
/// Expected: five lines "request N: entered [...]"; request 2's names 104 and 105 alone, "owed
/// identical true"; request 5 "entered [], owed rows 0, certified true".
#[test]
fn two_scripts_asking_git_are_entered_again_and_nothing_else() {
    fixture(|| {
        let mut world = World { units: 0, scripts: [1, 3], git: 0, fetch: 0, fault: false, readers: [0, 0, 0] };
        let run = |last, world| one(last, world, ALL, Pools::Many(2), Plant::None).map_err(|error| Broke::Oracle(format!("{error}")));
        let first = run(Answer::new_in(alloc()), world)?;
        println!("request 1: {}", said(&first));
        oracle(first.report.owed_rows == 2 && !first.report.certified, || said(&first))?;
        let again = run(first.answer, world)?;
        println!("request 2: {}", said(&again));
        let only: StdVec<(Name, Cause, bool)> = again.causes.iter().map(|one| (one.name, one.cause, one.identical)).collect();
        oracle(only == StdVec::from([(S0, Cause::Owed, true), (S1, Cause::Owed, true)]), || format!("request 2: {}", said(&again)))?;
        oracle(again.report.owed_rows == 2 && !again.report.certified, || said(&again))?;
        world.git = 1;
        let moved = run(again.answer, world)?;
        println!("request 3: {}", said(&moved));
        let names: StdVec<Name> = moved.causes.iter().map(|one| one.name).collect();
        oracle(names == StdVec::from([S0, S1, R0, R1, R2]), || format!("request 3: {}", said(&moved)))?;
        oracle(moved.causes.iter().filter(|one| one.name.kind == Kind::BodyCheck).all(|one| matches!(one.cause, Cause::ReadMoved { read, .. } if read.of == S0 || read.of == S1)), || said(&moved))?;
        world.scripts = [2, 4];
        let kept = run(moved.answer, world)?;
        println!("request 4: {}", said(&kept));
        oracle(kept.report.owed_rows == 0 && kept.causes.iter().filter(|one| one.name.kind == Kind::Script).all(|one| one.cause == Cause::KeyMoved), || said(&kept))?;
        let quiet = run(kept.answer, world)?;
        println!("request 5: {}", said(&quiet));
        oracle(quiet.entered.is_empty() && quiet.report.certified, || said(&quiet))
    });
}

/// Invariant 7: a failure its key covers (a parse refusal, no read list, pushed `ByKey`) stands
/// and is certified, whatever else moves, until its key moves: blaze-cli's MacroModule class.
///
/// Expected: "refused: entered [... \"106 new ...\" ...], owed rows 0, certified false", then
/// "quiet: entered [], owed rows 0, certified true", "units: ..." without 106, "parsed: ..."
/// with "106 key moved".
#[test]
fn a_failure_by_its_key_stands_until_the_key_moves() {
    fixture(|| {
        let mut world = World { units: 0, scripts: [0, 2], git: 0, fetch: 3, fault: false, readers: [0, 0, 0] };
        let run = |last, world| one(last, world, ALL, Pools::One, Plant::None).map_err(|error| Broke::Oracle(format!("{error}")));
        let refused = run(Answer::new_in(alloc()), world)?;
        println!("refused: {}", said(&refused));
        oracle(refused.answer.rows.get(&F).is_some_and(|row| row.failed && row.reads.is_empty()) && !refused.answer.owed_tasks.contains_key(&F), || said(&refused))?;
        let quiet = run(refused.answer, world)?;
        println!("quiet: {}", said(&quiet));
        oracle(quiet.entered.is_empty() && quiet.report.certified, || format!("a failure by its key did not stand: {}", said(&quiet)))?;
        world.units = 1;
        let units = run(quiet.answer, world)?;
        println!("units: {}", said(&units));
        oracle(!units.entered.contains(&F), || format!("a failure by its key entered for a read it never made: {}", said(&units)))?;
        world.fetch = 0;
        let parsed = run(units.answer, world)?;
        println!("parsed: {}", said(&parsed));
        oracle(parsed.causes.iter().any(|one| one.name == F && one.cause == Cause::KeyMoved), || said(&parsed))
    });
}

/// Invariant 7: a fault (no read list, pushed owed) is entered again next request and stands
/// once answered; a failure on its read stands until the read moves.
///
/// Expected: "fault: ..." then "after: entered [\"106 owed identical false\"] ..." and so on; the
/// test passes.
#[test]
fn a_fault_is_owed_and_a_failure_on_reads_stands() {
    fixture(|| {
        let mut world = World { units: 0, scripts: [0, 2], git: 0, fetch: 0, fault: true, readers: [0, 0, 0] };
        let run = |last, world| one(last, world, ALL, Pools::One, Plant::None).map_err(|error| Broke::Oracle(format!("{error}")));
        let fault = run(Answer::new_in(alloc()), world)?;
        println!("fault: {}", said(&fault));
        oracle(fault.report.owed_rows == 1 && fault.answer.owed_tasks.contains_key(&F) && !fault.report.certified, || said(&fault))?;
        world.fault = false;
        let after = run(fault.answer, world)?;
        println!("after: {}", said(&after));
        let only: StdVec<(Name, Cause)> = after.causes.iter().map(|one| (one.name, one.cause)).collect();
        oracle(only == StdVec::from([(F, Cause::Owed)]) && after.report.owed_rows == 0, || format!("after the fault: {}", said(&after)))?;
        let quiet = run(after.answer, world)?;
        println!("quiet: {}", said(&quiet));
        oracle(quiet.report.certified, || said(&quiet))?;
        // A failure on its read: `U` odd.
        world.units = 1;
        let failing = run(quiet.answer, world)?;
        println!("failing: {}", said(&failing));
        oracle(failing.answer.rows.get(&F).is_some_and(|row| row.failed && !row.reads.is_empty()) && failing.report.owed_rows == 0, || said(&failing))?;
        let stood = run(failing.answer, world)?;
        println!("stood: {}", said(&stood));
        oracle(stood.report.certified, || format!("a failure on its read did not stand: {}", said(&stood)))?;
        world.units = 3;
        let reread = run(stood.answer, world)?;
        println!("reread: {}", said(&reread));
        oracle(reread.causes.iter().any(|one| one.name == F && matches!(one.cause, Cause::ReadMoved { read, .. } if read.of == U)), || said(&reread))
    });
}

/// A narrow request that does not reach an owed row leaves it owed and is not certified; the
/// request that reaches it enters it for `Owed` (c).
///
/// Expected: "narrow: entered [], owed rows 1, certified false", then "reached: entered
/// [\"104 owed identical true\"] ...".
#[test]
fn a_narrow_request_leaves_an_owed_row_owed() {
    fixture(|| {
        let world = World { units: 0, scripts: [1, 2], git: 0, fetch: 0, fault: false, readers: [0, 0, 0] };
        let first = one(Answer::new_in(alloc()), world, ALL, Pools::One, Plant::None).map_err(|error| Broke::Oracle(format!("{error}")))?;
        let narrow = Runs { scripts: false, fetch: true, readers: true };
        let held = one(first.answer, world, narrow, Pools::One, Plant::None).map_err(|error| Broke::Oracle(format!("{error}")))?;
        println!("narrow: {}", said(&held));
        oracle(held.entered.is_empty() && held.report.owed_rows == 1 && !held.report.certified && held.answer.owed_tasks.contains_key(&S0), || said(&held))?;
        let reached = one(held.answer, world, ALL, Pools::One, Plant::None).map_err(|error| Broke::Oracle(format!("{error}")))?;
        println!("reached: {}", said(&reached));
        let only: StdVec<(Name, Cause)> = reached.causes.iter().map(|one| (one.name, one.cause)).collect();
        oracle(only == StdVec::from([(S0, Cause::Owed)]), || said(&reached))
    });
}

/// A name on a buffer's owed list that the phase did not enter is refused by name.
///
/// Expected: the merge of the Units phase refuses with `Unplanned` naming R0.
#[test]
fn an_owed_name_the_phase_did_not_enter_is_refused() {
    let world = World { units: 0, scripts: [0, 0], git: 0, fetch: 0, fault: false, readers: [0, 0, 0] };
    let plan = world.plan();
    let files: [(Name, &[Planned]); 1] = [(FILE, &plan[..])];
    let mut engine = match Engine::new_in(Tools([TOOL; key::KINDS]), Answer::new_in(alloc()), &Edit { files: &files }, ALL, Plant::None, alloc()) {
        Ok(engine) => engine,
        Err(error) => panic!("the request was refused: {error}"),
    };
    let mut buffers: StdVec<Buffer<'static>> = StdVec::from([Buffer::new_in(alloc())]);
    match engine.next_phase() {
        Ok(Some(phase)) => assert_eq!(phase.stage, Stage::Units, "the first phase is the units'"),
        Ok(None) => panic!("no first phase"),
        Err(error) => panic!("no first phase: {error}"),
    }
    {
        let view = engine.view();
        let tasks = engine.tasks();
        Pools::One.run(tasks, &mut buffers, &|_: &(), task: &Task, buffer: &mut Buffer<'static>| {
            work(&world, view, task, buffer);
            buffer.owed.push(R0);
        });
    }
    match engine.merge(&mut buffers) {
        Err(EngineError::Unplanned { name }) => assert_eq!(name, R0, "the refusal names the owed task"),
        Err(error) => panic!("refused for another reason: {error}"),
        Ok(()) => panic!("an owed name the phase did not enter was taken"),
    }
}
