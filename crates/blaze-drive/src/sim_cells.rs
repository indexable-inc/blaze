//! Part reads, simulated (drive.md, "Part reads"; cells.rs). A toy world of one table, a
//! `ModuleScope` record whose payload is its sorted cells, each a 16-byte cell and the 32-byte
//! digest it holds; its kind's lookup is a binary search there. Its readers:
//! - `Signature` tasks read cell A, `ImplBucket` tasks read cell B, `CrateScope` tasks read cell
//!   C (absent until a case adds it), each by part;
//! - `Expansion` tasks read the table whole.
//!
//! Cases, each against the request's own plants:
//! - a change of cell B reruns the B readers and the whole readers, and the A and C readers stand
//!   (plants: a moved cell dirties nobody; every cell reader dirtied as if whole);
//! - a change of cell A reruns the A readers, and the B readers stand;
//! - cell C appearing reruns its absent-cell readers;
//! - an unchanged repeat runs nothing.
//!
//! Every test's expected line is "test sim_cells::<name> ... ok"; each control passes when its
//! plant is caught, with the message it names.

use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_id::ContentId;
use blaze_own::{Alloc, Vec};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::Plant;
use crate::cells::{Cell, CellRead, CellRefused, Cells, Seen};
use crate::condition::Space;
use crate::key::{self, Key, Kind, Name, Tool};
use crate::record::{self, Finding, Locate, Part, PathOf, Stored};
use crate::request::{
    Answer, Ask, BatchId, Command, Config, Effect, Event, FileEntry, Gate, Outcome, Output, Ran, Request, Task,
    TaskFailure, Ticket, Want,
};

const MOST_EVENTS: u32 = 100_000;
/// One cell of the table's payload: the cell, then its digest.
const ENTRY: usize = 16 + 32;

const A: Cell = Cell([0xA; 16]);
const B: Cell = Cell([0xB; 16]);
const C: Cell = Cell([0xC; 16]);

/// The table's kind's lookup: a binary search over its sorted entries.
fn cell_of(payload: &[u8], cell: Cell) -> Result<Seen, CellRefused> {
    if payload.len() % ENTRY != 0 {
        return Err(CellRefused { what: "a sim table's payload is whole entries" });
    }
    let entries: StdVec<&[u8]> = payload.chunks_exact(ENTRY).collect();
    match entries.binary_search_by(|entry| entry[..16].cmp(&cell.0[..])) {
        Ok(at) => {
            let mut digest = [0; 32];
            digest.copy_from_slice(&entries[at][16..]);
            Ok(Seen::Is(ContentId(digest)))
        }
        Err(_) => Ok(Seen::Absent),
    }
}

/// How many readers of each sort, and what each cell holds.
#[derive(Clone, Debug)]
struct World {
    cells: BTreeMap<Cell, u64>,
    a_readers: usize,
    b_readers: usize,
    c_readers: usize,
    whole_readers: usize,
}

fn table() -> Name {
    Name::new(Kind::ModuleScope, key::digest("sim cells table", b""))
}

fn reader(kind: Kind, i: usize) -> Name {
    Name::new(kind, key::digest("sim cells reader", format!("{} {i}", kind.name()).as_bytes()))
}

/// The cell a reader of `kind` reads; `None` for a whole reader.
fn cell_read_by(kind: Kind) -> Option<Cell> {
    match kind {
        Kind::Signature => Some(A),
        Kind::ImplBucket => Some(B),
        Kind::CrateScope => Some(C),
        _ => None,
    }
}

impl World {
    fn readers(&self) -> StdVec<Name> {
        let mut names = StdVec::new();
        for (kind, n) in [(Kind::Signature, self.a_readers), (Kind::ImplBucket, self.b_readers), (Kind::CrateScope, self.c_readers), (Kind::Expansion, self.whole_readers)] {
            names.extend((0..n).map(|i| reader(kind, i)));
        }
        names
    }

    fn payload(&self) -> StdVec<u8> {
        let mut bytes = StdVec::new();
        for (cell, value) in self.cells.iter() {
            bytes.extend_from_slice(&cell.0);
            bytes.extend_from_slice(&key::digest("sim cells value", &value.to_le_bytes()).0);
        }
        bytes
    }

    fn units_source(&self) -> ContentId {
        key::digest("sim cells units", format!("{self:?}").as_bytes())
    }
}

// --- The fake executor ------------------------------------------------------------------------

struct Made {
    name: Name,
    out: ContentId,
    reads: StdVec<(Name, ContentId)>,
    cells: StdVec<CellRead>,
    parts: StdVec<Part>,
    payload: StdVec<u8>,
}

enum Answered {
    Done(Made),
    Need(StdVec<Want>),
}

fn made(name: Name, out: ContentId) -> Made {
    Made { name, out, reads: StdVec::new(), cells: StdVec::new(), parts: StdVec::new(), payload: StdVec::new() }
}

fn execute(world: &World, store: &BTreeMap<Key, StdVec<u8>>, task: &Task<'static>) -> Result<Answered, Broke> {
    let name = task.name;
    if name.kind == Kind::Units {
        let mut m = made(name, key::digest("sim cells units out", b""));
        let source = key::digest("sim cells table source", format!("{:?}", world.cells).as_bytes());
        m.parts.push(Part { name: table(), locate: Locate::Source(source), place: 0 });
        for read in world.readers() {
            let source = key::digest("sim cells reader source", &read.subject.0);
            m.parts.push(Part { name: read, locate: Locate::Source(source), place: 0 });
        }
        m.parts.sort_by_key(|part| part.name);
        return Ok(Answered::Done(m));
    }
    if name == table() {
        let payload = world.payload();
        let mut m = made(name, key::digest("sim cells table out", &payload));
        m.payload = payload;
        return Ok(Answered::Done(m));
    }
    let Some(input) = task.inputs.iter().find(|input| input.name == table()) else {
        return Ok(Answered::Need(std::vec![Want { name: table(), home: Name::units() }]));
    };
    match cell_read_by(name.kind) {
        None => {
            let mut m = made(name, key::digest("sim cells whole reader out", &input.out.0));
            m.reads.push((table(), input.out));
            Ok(Answered::Done(m))
        }
        Some(cell) => {
            let bytes = store.get(&input.at).ok_or(Broke::Harness(format!("{name} was handed a table record the store lacks")))?;
            let payload = match record::open(bytes) {
                Ok(Stored::Whole(view)) => view.payload,
                _ => return Err(Broke::Harness(format!("{name} was handed no whole table record"))),
            };
            let seen = cell_of(payload, cell).map_err(|refused| Broke::Harness(String::from(refused.what)))?;
            let mut m = made(name, key::digest("sim cells cell reader out", format!("{name} {seen:?}").as_bytes()));
            m.cells.push(CellRead::new(table(), cell, seen));
            Ok(Answered::Done(m))
        }
    }
}

// --- One request, driven ----------------------------------------------------------------------

#[derive(Default)]
struct Ended {
    answer: Option<Answer>,
    /// Every name a batch ran this request.
    ran: BTreeSet<Name>,
    last: Option<StdVec<u8>>,
    failures: StdVec<TaskFailure>,
}

impl Ended {
    fn ran_of(&self, kind: Kind) -> usize {
        self.ran.iter().filter(|name| name.kind == kind).count()
    }

    /// How many of `kind` stood: not dirtied, so not even looked up by value.
    fn standing(&self, kind: Kind) -> u32 {
        self.answer.map_or(0, |answer| {
            let mut profile = answer.profile;
            profile.of(kind).standing
        })
    }
}

fn never_an_error(_: u16) -> bool {
    false
}

fn config(workers: u32) -> Config {
    let tools = Kind::ALL.map(|kind| Tool(key::digest("sim cells kind tool", &[kind as u8])));
    Config { tool: Tool(key::digest("sim cells tool", b"")), tools, workers, most_entries: 32, most_jobs: 1 << 16, is_error: never_an_error }
}

const ASK: Ask<'static> = Ask { command: Command::Check, args: b"", gate: Gate::Beside, space: Space::NONE, program: None, facts: &[] };

fn request(tape: &Tape, world: &World, plant: Plant, store: &mut BTreeMap<Key, StdVec<u8>>, last: Option<&[u8]>) -> Result<Ended, Broke> {
    let manifest = b"Cargo.toml".to_vec();
    let files = [FileEntry { path: &manifest, id: world.units_source(), changed: false, manifest: true }];
    let workers = [1, 2, 16][tape.index(3, "workers")];
    let mut req = Request::planted_in(config(workers), ASK, plant, blaze_own::testing::heap().alloc()).with_cells(Cells::NONE.with(Kind::ModuleScope, cell_of));
    let mut out: Vec<Effect<'static>, Alloc<'static>> = Vec::new_in(blaze_own::testing::heap().alloc());
    let mut ended = Ended::default();
    let mut gets: StdVec<Key> = StdVec::new();
    let mut batches: StdVec<(BatchId, StdVec<(Ticket, Task<'static>)>)> = StdVec::new();
    let mut first = Some(Event::Start { files: &files, gone: &[], last, units: world.units_source(), cold: false, graph: None });
    for _ in 0..MOST_EVENTS {
        let stepped = match first.take() {
            Some(event) => req.step(event, &mut out),
            None => {
                let total = gets.len() + batches.len();
                if total == 0 {
                    return Err(Broke::Oracle(String::from("the request stalled: nothing out and no answer")));
                }
                let pick = tape.index(total, "which completes");
                if pick < gets.len() {
                    let key = gets.swap_remove(pick);
                    let bytes = store.get(&key).cloned();
                    req.step(Event::Got { key, bytes: bytes.as_deref() }, &mut out)
                } else {
                    let (id, tasks) = batches.swap_remove(pick - gets.len());
                    let mut answered = StdVec::new();
                    for (ticket, task) in tasks.iter() {
                        answered.push((*ticket, execute(world, store, task)?));
                    }
                    let no_findings: [Finding<'_>; 0] = [];
                    let no_paths: [PathOf<'_>; 0] = [];
                    let outputs: StdVec<Output<'_>> = answered
                        .iter()
                        .filter_map(|(_, answer)| match answer {
                            Answered::Done(m) => Some(Output {
                                name: m.name,
                                out: m.out,
                                reads: &m.reads,
                                cells: &m.cells,
                                parts: &m.parts,
                                paths: &no_paths,
                                findings: &no_findings,
                                payload: &m.payload,
                            }),
                            Answered::Need(_) => None,
                        })
                        .collect();
                    let mut at = 0;
                    let mut results = StdVec::new();
                    for (ticket, answer) in answered.iter() {
                        let ran = match answer {
                            Answered::Need(wants) => Ran::Need(wants),
                            Answered::Done(_) => {
                                at += 1;
                                Ran::Done(&outputs[at - 1..at])
                            }
                        };
                        results.push(Outcome { ticket: *ticket, ran, in_place: 0, ns: 1_000 });
                    }
                    req.step(Event::Ran { batch: id, results: &results }, &mut out)
                }
            }
        };
        if let Err(error) = stepped {
            return Err(Broke::Oracle(format!("the request failed: {error}")));
        }
        for effect in out.drain(..) {
            match effect {
                Effect::Get { key } => gets.push(key),
                Effect::Put { key, bytes } => match store.get(&key) {
                    Some(held) if held.as_slice() != bytes.as_slice() => {
                        return Err(Broke::Oracle(format!("two values under one key {key} (rule 21)")));
                    }
                    Some(_) => {}
                    None => {
                        store.insert(key, bytes.as_slice().to_vec());
                    }
                },
                Effect::Run { batch } => {
                    let mut tasks = StdVec::new();
                    for (ticket, task) in batch.tasks.into_iter() {
                        ended.ran.insert(task.name);
                        tasks.push((ticket, task));
                    }
                    batches.push((batch.id, tasks));
                }
                Effect::Publish(_) | Effect::Unpublish(_) | Effect::Print(_) => {}
                Effect::WriteLast { bytes } => ended.last = Some(bytes.as_slice().to_vec()),
                Effect::Done(answer) => ended.answer = Some(answer),
                Effect::Open { name, .. } => return Err(Broke::Harness(format!("{name} was opened, and the sim has no files"))),
                Effect::StartProgram { .. } | Effect::Stop { .. } => {
                    return Err(Broke::Harness(String::from("`check` started or stopped a program")));
                }
            }
        }
        if ended.answer.is_some() {
            if !gets.is_empty() || !batches.is_empty() {
                return Err(Broke::Oracle(String::from("answered with work still out")));
            }
            ended.failures = req.failures().to_vec();
            if !ended.failures.is_empty() {
                return Err(Broke::Oracle(format!("tasks failed: {:?}", ended.failures)));
            }
            return Ok(ended);
        }
    }
    Err(Broke::Harness(format!("more than {MOST_EVENTS} events")))
}

/// `before`, then `after` over the same store: the second request's ending.
fn edited(tape: &Tape, plant: Plant, before: &World, after: &World) -> Result<Ended, Broke> {
    let mut store = BTreeMap::new();
    let first = request(tape, before, plant, &mut store, None)?;
    request(tape, after, plant, &mut store, first.last.as_deref())
}

/// Cells A and B, and one to three readers of each sort.
fn draw(tape: &Tape) -> World {
    let mut cells = BTreeMap::new();
    cells.insert(A, 1);
    cells.insert(B, 2);
    World {
        cells,
        a_readers: 1 + tape.index(3, "A readers"),
        b_readers: 1 + tape.index(3, "B readers"),
        c_readers: 1 + tape.index(3, "C readers"),
        whole_readers: 1 + tape.index(3, "whole readers"),
    }
}

/// A control: the clean request passes and the plant is caught with `words`.
fn caught(plant: Plant, words: &str, case: fn(&Tape, Plant) -> Result<(), Broke>) {
    control(SimConfig::cases(300), Plant::None, plant, words, case);
}

/// `kind`'s readers in `world`, all of which ran (`moved`) or all of which stood.
fn expect(ended: &Ended, world: &World, kind: Kind, moved: bool) -> Result<(), Broke> {
    let n = match kind {
        Kind::Signature => world.a_readers,
        Kind::ImplBucket => world.b_readers,
        Kind::CrateScope => world.c_readers,
        _ => world.whole_readers,
    };
    let ran = ended.ran_of(kind);
    match moved {
        true if ran != n => Err(Broke::Oracle(format!("a reader whose cell moved stood: {ran} of {n} {} readers ran", kind.name()))),
        false if ran != 0 => Err(Broke::Oracle(format!("a reader of an unmoved cell ran: {ran} of {n} {} readers", kind.name()))),
        false if ended.standing(kind) != n as u32 => Err(Broke::Oracle(format!(
            "a reader of an unmoved cell was dirtied: {} of {n} {} readers stood",
            ended.standing(kind),
            kind.name()
        ))),
        _ => Ok(()),
    }
}

// --- Case 1: a change of cell B ----------------------------------------------------------------

fn b_changes(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let before = draw(tape);
    let mut after = before.clone();
    after.cells.insert(B, 20);
    let ended = edited(tape, plant, &before, &after)?;
    expect(&ended, &after, Kind::ImplBucket, true)?;
    expect(&ended, &after, Kind::Expansion, true)?;
    expect(&ended, &after, Kind::Signature, false)?;
    expect(&ended, &after, Kind::CrateScope, false)
}

/// Expected: "test sim_cells::a_change_of_cell_b_leaves_the_cell_a_readers_standing ... ok".
#[test]
fn a_change_of_cell_b_leaves_the_cell_a_readers_standing() {
    check(SimConfig::cases(200), |tape| b_changes(tape, Plant::None));
}

/// Expected: "test sim_cells::control_a_moved_cell_not_dirtying_its_reader_is_caught ... ok".
#[test]
fn control_a_moved_cell_not_dirtying_its_reader_is_caught() {
    caught(Plant::CellMovedNotDirtied, "a reader whose cell moved stood", b_changes);
}

/// Expected: "test sim_cells::control_cell_readers_dirtied_as_whole_is_caught ... ok".
#[test]
fn control_cell_readers_dirtied_as_whole_is_caught() {
    caught(Plant::CellsAsWhole, "a reader of an unmoved cell was dirtied", b_changes);
}

// --- Case 2: a change of cell A ----------------------------------------------------------------

fn a_changes(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let before = draw(tape);
    let mut after = before.clone();
    after.cells.insert(A, 10);
    let ended = edited(tape, plant, &before, &after)?;
    expect(&ended, &after, Kind::Signature, true)?;
    expect(&ended, &after, Kind::Expansion, true)?;
    expect(&ended, &after, Kind::ImplBucket, false)
}

/// Expected: "test sim_cells::a_change_of_cell_a_reruns_its_readers ... ok".
#[test]
fn a_change_of_cell_a_reruns_its_readers() {
    check(SimConfig::cases(200), |tape| a_changes(tape, Plant::None));
}

// --- Case 3: an absent cell appears ------------------------------------------------------------

fn c_appears(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let before = draw(tape);
    let mut after = before.clone();
    after.cells.insert(C, 3);
    let ended = edited(tape, plant, &before, &after)?;
    expect(&ended, &after, Kind::CrateScope, true)?;
    expect(&ended, &after, Kind::Signature, false)?;
    expect(&ended, &after, Kind::ImplBucket, false)
}

/// Expected: "test sim_cells::an_absent_cell_appearing_reruns_its_readers ... ok".
#[test]
fn an_absent_cell_appearing_reruns_its_readers() {
    check(SimConfig::cases(200), |tape| c_appears(tape, Plant::None));
}

/// Expected: "test sim_cells::control_an_appearing_cell_not_dirtying_its_reader_is_caught ... ok".
#[test]
fn control_an_appearing_cell_not_dirtying_its_reader_is_caught() {
    caught(Plant::CellMovedNotDirtied, "a reader whose cell moved stood", c_appears);
}

// --- Case 4: an unchanged repeat ---------------------------------------------------------------

fn unchanged(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let world = draw(tape);
    let ended = edited(tape, plant, &world, &world)?;
    if !ended.ran.is_empty() {
        return Err(Broke::Oracle(format!("an unchanged repeat ran {} tasks", ended.ran.len())));
    }
    Ok(())
}

/// Expected: "test sim_cells::an_unchanged_repeat_runs_nothing ... ok".
#[test]
fn an_unchanged_repeat_runs_nothing() {
    check(SimConfig::cases(200), |tape| unchanged(tape, Plant::None));
}

// --- The lookup itself -------------------------------------------------------------------------

/// Expected: "test sim_cells::a_cell_lookup_answers_absent_and_refuses_a_torn_payload ... ok".
#[test]
fn a_cell_lookup_answers_absent_and_refuses_a_torn_payload() {
    let cells = [(A, 1), (B, 2), (C, 3)].into_iter().collect();
    let world = World { cells, a_readers: 0, b_readers: 0, c_readers: 0, whole_readers: 0 };
    let payload = world.payload();
    assert_eq!(cell_of(&payload, Cell([0xD; 16])), Ok(Seen::Absent));
    assert_eq!(cell_of(&payload, B), Ok(Seen::Is(key::digest("sim cells value", &2u64.to_le_bytes()))));
    assert!(cell_of(&payload[..ENTRY + 1], A).is_err());
}
