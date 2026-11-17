//! A crate's scope read by cell, simulated (drive.md, "Names at module grain" point 3: "READS ARE
//! PER CELL"; lane CELL, 2026-11-17). The case it is for: a private fn added to one file made
//! every `Signature` and `BodyCheck` of its crate again (signature_grain_tests.rs
//! `a_private_item_added_makes_itself_alone`: (7, 7) where the floor is (1, 1)), because every
//! body read its crate's `CrateScope` WHOLE and the scope's out covers its inner names.
//!
//! The toy crate: modules 0..[`MODULES`] and a prelude ([`PRELUDE`]), each with explicit items
//! by (namespace, name), each public or private; a module's glob imports (one level, other
//! modules); impl buckets. Its `CrateScope` record's payload is the scope's CELL TABLE, sorted
//! (16-byte cell, 32-byte digest) rows, and the kind's lookup [`cell_of`] is a binary search in
//! it. The cells, each [`At`]:
//! - `Own(m, ns, x)`: what `m` itself binds, public or private (a lookup from inside `m`);
//! - `Exported(m, ns, x)`: what `m` binds publicly (a lookup through a glob of `m`, or the
//!   prelude's): a private item added or changed does not move it;
//! - `Globs(m)`: `m`'s glob sources;
//! - `Bucket(b)`: an impl bucket's impls.
//!
//! A body asks names in its module and impl buckets. The rules ([`resolve`], one home for both
//! the body and the oracle): a block local, then `Own`, then the globs (`Globs`, then each
//! source's `Exported`: one definition is the answer, two are `Ambiguous`), then the prelude's
//! `Exported`, else `NotFound`. Every cell the rules consulted is a read, `Absent` included.
//!
//! The ORACLE, after every request of a history of edits (an item added, removed, changed or
//! made public or private; a glob added or removed; an impl added; nothing):
//! - SAME AS FRESH: each body's out, as the request settled it, equals the out the rules give
//!   over today's crate;
//! - THE FLOOR: every body whose consulted cells moved (its trace: every (cell, digest) the
//!   rules consulted over today's crate) and that no earlier request made with today's trace
//!   is made, and a body whose cells stood is never made. A body whose cells came back to an
//!   earlier record's may be made again (the walk over earlier shapes finds it, not the cells):
//!   noted on the tape.
//!
//! PLANTED BUGS, each a recorder that leaves a cell out (the oracle must catch every one):
//! - a lookup that found nothing records nothing (the name appearing is missed);
//! - a name found through a glob or the prelude records no `Absent` read of its own module (a
//!   nearer item shadowing it is missed);
//! - an `Absent` glob source records nothing (a new glob-imported item shadowing the prelude,
//!   or making the name ambiguous, is missed);
//! - an impl bucket asked records nothing (an impl added to it is missed);
//! - the drive's own: every cell reader dirtied as if it read the scope whole (the floor
//!   catches it).
//!
//! Every test's expected line is "test sim_scope_cells::<name> ... ok"; each control passes when
//! its plant is caught, with the words it names.

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
    Ask, BatchId, Command, Config, Effect, Event, FileEntry, Gate, Outcome, Output, Ran, Request, Task, Ticket, Want,
};

const MOST_EVENTS: u32 = 100_000;
/// One row of the cell table: the cell, then its digest.
const ENTRY: usize = 16 + 32;
/// The crate's modules, then the prelude's index.
const MODULES: u8 = 4;
const PRELUDE: u8 = MODULES;
/// Names are `0..NAMES` in each namespace: few, so lookups meet.
const NAMES: u8 = 4;
const BUCKETS: u8 = 2;
/// At most this many impls a bucket: its digest holds them inline.
const MOST_IMPLS: usize = 12;

const NOT_FRESH: &str = "a body stood though its answer moved";
const UNDER_FLOOR: &str = "a body stood though a cell its rules consult moved";
const OVER_FLOOR: &str = "a body whose consulted cells stood";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Ns {
    Types = 0,
    Values = 1,
}

/// One cell of the scope's table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum At {
    Own(u8, Ns, u8),
    Exported(u8, Ns, u8),
    Globs(u8),
    Bucket(u8),
}

fn cell(at: At) -> Cell {
    let bytes = match at {
        At::Own(m, ns, x) => [1, m, ns as u8, x],
        At::Exported(m, ns, x) => [2, m, ns as u8, x],
        At::Globs(m) => [3, m, 0, 0],
        At::Bucket(b) => [4, b, 0, 0],
    };
    let digest = key::digest("sim scope cells cell", &bytes);
    let mut cell = [0; 16];
    cell.copy_from_slice(&digest.0[..16]);
    Cell(cell)
}

/// The scope's kind's lookup: a binary search over the sorted rows.
fn cell_of(payload: &[u8], cell: Cell) -> Result<Seen, CellRefused> {
    if payload.len() % ENTRY != 0 {
        return Err(CellRefused { what: "a sim scope's payload is whole rows" });
    }
    let rows: StdVec<&[u8]> = payload.chunks_exact(ENTRY).collect();
    match rows.binary_search_by(|row| row[..16].cmp(&cell.0[..])) {
        Ok(at) => {
            let mut digest = [0; 32];
            digest.copy_from_slice(&rows[at][16..]);
            Ok(Seen::Is(ContentId(digest)))
        }
        Err(_) => Ok(Seen::Absent),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Item {
    value: u16,
    public: bool,
}

fn item_digest(item: Item) -> ContentId {
    let mut d = [0; 32];
    d[0] = 1;
    d[1] = u8::from(item.public);
    d[2..4].copy_from_slice(&item.value.to_le_bytes());
    ContentId(d)
}

fn globs_digest(globs: &BTreeSet<u8>) -> ContentId {
    let mut d = [0; 32];
    d[0] = 2;
    d[1] = globs.len() as u8;
    for (i, src) in globs.iter().enumerate() {
        d[2 + i] = *src;
    }
    ContentId(d)
}

fn bucket_digest(impls: &BTreeSet<u16>) -> ContentId {
    let mut d = [0; 32];
    d[0] = 3;
    d[1] = impls.len() as u8;
    for (i, value) in impls.iter().enumerate() {
        d[2 + 2 * i..4 + 2 * i].copy_from_slice(&value.to_le_bytes());
    }
    ContentId(d)
}

#[derive(Clone, Debug, Default)]
struct Module {
    items: BTreeMap<(Ns, u8), Item>,
    globs: BTreeSet<u8>,
}

#[derive(Clone, Debug)]
struct Body {
    module: u8,
    /// Value names bound in the body's own blocks: the nearest scope, read from its own text.
    locals: BTreeSet<u8>,
    asks: StdVec<(Ns, u8)>,
    buckets: StdVec<u8>,
}

#[derive(Clone, Debug)]
struct World {
    /// `MODULES` modules, then the prelude.
    modules: StdVec<Module>,
    buckets: BTreeMap<u8, BTreeSet<u16>>,
    bodies: BTreeMap<u32, Body>,
    /// The next value no draw or edit has made.
    serial: u16,
}

fn scope() -> Name {
    Name::new(Kind::CrateScope, key::digest("sim scope cells scope", b""))
}

fn body_name(id: u32) -> Name {
    Name::new(Kind::BodyCheck, key::digest("sim scope cells body", &id.to_le_bytes()))
}

impl World {
    /// Today's cell, straight from the crate: the second home of the table, against which the
    /// payload's rows are checked.
    fn seen(&self, at: At) -> Seen {
        let module = |m: u8| &self.modules[usize::from(m)];
        match at {
            At::Own(m, ns, x) => module(m).items.get(&(ns, x)).map_or(Seen::Absent, |item| Seen::Is(item_digest(*item))),
            At::Exported(m, ns, x) => {
                module(m).items.get(&(ns, x)).filter(|item| item.public).map_or(Seen::Absent, |item| Seen::Is(item_digest(*item)))
            }
            At::Globs(m) => match module(m).globs.is_empty() {
                true => Seen::Absent,
                false => Seen::Is(globs_digest(&module(m).globs)),
            },
            At::Bucket(b) => match self.buckets.get(&b) {
                Some(impls) if !impls.is_empty() => Seen::Is(bucket_digest(impls)),
                _ => Seen::Absent,
            },
        }
    }

    fn payload(&self) -> StdVec<u8> {
        let mut rows: StdVec<(Cell, ContentId)> = StdVec::new();
        for (m, module) in self.modules.iter().enumerate() {
            let m = m as u8;
            for (&(ns, x), &item) in module.items.iter() {
                rows.push((cell(At::Own(m, ns, x)), item_digest(item)));
                if item.public {
                    rows.push((cell(At::Exported(m, ns, x)), item_digest(item)));
                }
            }
            if !module.globs.is_empty() {
                rows.push((cell(At::Globs(m)), globs_digest(&module.globs)));
            }
        }
        for (&b, impls) in self.buckets.iter() {
            if !impls.is_empty() {
                rows.push((cell(At::Bucket(b)), bucket_digest(impls)));
            }
        }
        rows.sort_by_key(|row| row.0);
        let mut bytes = StdVec::new();
        for (cell, digest) in rows {
            bytes.extend_from_slice(&cell.0);
            bytes.extend_from_slice(&digest.0);
        }
        bytes
    }

    fn units_source(&self) -> ContentId {
        // Not the serial: a counter of the edits, not the crate's text.
        key::digest("sim scope cells units", format!("{:?} {:?} {:?}", self.modules, self.buckets, self.bodies).as_bytes())
    }

    fn body_source(id: u32, body: &Body) -> ContentId {
        key::digest("sim scope cells body source", format!("{id} {body:?}").as_bytes())
    }
}

// --- The rules: one home for the body and the oracle -------------------------------------------

/// Where a lookup's cells come from: the record handed to the body, or the crate itself.
trait Look {
    fn seen(&self, at: At) -> Result<Seen, Broke>;
}

impl Look for World {
    fn seen(&self, at: At) -> Result<Seen, Broke> {
        Ok(World::seen(self, at))
    }
}

/// A body's view: its `CrateScope` record's payload, searched in place.
struct Payload<'p>(&'p [u8]);

impl Look for Payload<'_> {
    fn seen(&self, at: At) -> Result<Seen, Broke> {
        cell_of(self.0, cell(at)).map_err(|refused| Broke::Harness(String::from(refused.what)))
    }
}

/// What one asked name resolved to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Got {
    Local,
    Found { from: u8, value: u16 },
    Ambiguous,
    NotFound,
}

/// One cell consulted, for the ask at `ask` (names first, then buckets).
#[derive(Clone, Copy, Debug)]
struct Read {
    at: At,
    seen: Seen,
    ask: usize,
}

struct Resolved {
    got: StdVec<Got>,
    impls: StdVec<StdVec<u16>>,
    reads: StdVec<Read>,
}

fn item_of(seen: Seen) -> Option<Item> {
    match seen {
        Seen::Is(d) => Some(Item { public: d.0[1] == 1, value: u16::from_le_bytes([d.0[2], d.0[3]]) }),
        Seen::Absent => None,
    }
}

fn resolve(look: &impl Look, body: &Body) -> Result<Resolved, Broke> {
    let mut reads = StdVec::new();
    let mut got = StdVec::new();
    let m = body.module;
    for (ask, &(ns, x)) in body.asks.iter().enumerate() {
        let mut read = |at: At| -> Result<Seen, Broke> {
            let seen = look.seen(at)?;
            reads.push(Read { at, seen, ask });
            Ok(seen)
        };
        if ns == Ns::Values && body.locals.contains(&x) {
            got.push(Got::Local);
            continue;
        }
        if let Some(item) = item_of(read(At::Own(m, ns, x))?) {
            got.push(Got::Found { from: m, value: item.value });
            continue;
        }
        let mut found: BTreeSet<(u8, u16)> = BTreeSet::new();
        if let Seen::Is(d) = read(At::Globs(m))? {
            let sources = &d.0[2..2 + usize::from(d.0[1])];
            for &src in sources {
                if let Some(item) = item_of(read(At::Exported(src, ns, x))?) {
                    found.insert((src, item.value));
                }
            }
        }
        let answer = match found.len() {
            0 => match item_of(read(At::Exported(PRELUDE, ns, x))?) {
                Some(item) => Got::Found { from: PRELUDE, value: item.value },
                None => Got::NotFound,
            },
            1 => {
                let (from, value) = *found.iter().next().ok_or(Broke::Harness(String::from("one found and none")))?;
                Got::Found { from, value }
            }
            _ => Got::Ambiguous,
        };
        got.push(answer);
    }
    let mut impls = StdVec::new();
    for (i, &b) in body.buckets.iter().enumerate() {
        let at = At::Bucket(b);
        let seen = look.seen(at)?;
        reads.push(Read { at, seen, ask: body.asks.len() + i });
        impls.push(match seen {
            Seen::Is(d) => (0..usize::from(d.0[1])).map(|k| u16::from_le_bytes([d.0[2 + 2 * k], d.0[3 + 2 * k]])).collect(),
            Seen::Absent => StdVec::new(),
        });
    }
    Ok(Resolved { got, impls, reads })
}

fn out_of(resolved: &Resolved) -> ContentId {
    key::digest("sim scope cells body out", format!("{:?} {:?}", resolved.got, resolved.impls).as_bytes())
}

/// Every (cell, digest) the rules consulted, sorted: what the body's answer is a function of.
fn trace(resolved: &Resolved) -> StdVec<(Cell, [u8; 32])> {
    let mut rows: StdVec<(Cell, [u8; 32])> = resolved.reads.iter().map(|read| (cell(read.at), read.seen.digest().0)).collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

// --- The plants ---------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Planted {
    None,
    /// The drive's own plant.
    Drive(Plant),
    /// A lookup that found nothing records none of its reads.
    FailedLeftOut,
    /// A name found through a glob or the prelude records no `Absent` read of its own module.
    NearerLeftOut,
    /// A glob source where the name is absent records nothing.
    GlobAbsentLeftOut,
    /// An impl bucket asked records nothing.
    BucketLeftOut,
}

/// The reads the body's recorder keeps, under `planted`.
fn recorded(planted: Planted, body: &Body, resolved: &Resolved) -> StdVec<Read> {
    let got = |ask: usize| resolved.got.get(ask).copied();
    resolved
        .reads
        .iter()
        .copied()
        .filter(|read| match planted {
            Planted::FailedLeftOut => got(read.ask) != Some(Got::NotFound),
            Planted::NearerLeftOut => !(matches!(read.at, At::Own(..))
                && read.seen == Seen::Absent
                && matches!(got(read.ask), Some(Got::Found { from, .. }) if from != body.module)),
            Planted::GlobAbsentLeftOut => !(matches!(read.at, At::Exported(src, ..) if src != PRELUDE) && read.seen == Seen::Absent),
            Planted::BucketLeftOut => !matches!(read.at, At::Bucket(_)),
            Planted::None | Planted::Drive(_) => true,
        })
        .collect()
}

// --- The fake executor ------------------------------------------------------------------------

struct Made {
    name: Name,
    out: ContentId,
    cells: StdVec<CellRead>,
    parts: StdVec<Part>,
    payload: StdVec<u8>,
}

enum Answered {
    Done(Made),
    Need(StdVec<Want>),
}

fn made(name: Name, out: ContentId) -> Made {
    Made { name, out, cells: StdVec::new(), parts: StdVec::new(), payload: StdVec::new() }
}

fn execute(world: &World, store: &BTreeMap<Key, StdVec<u8>>, task: &Task<'static>, planted: Planted) -> Result<Answered, Broke> {
    let name = task.name;
    if name.kind == Kind::Units {
        let mut m = made(name, key::digest("sim scope cells units out", b""));
        m.parts.push(Part { name: scope(), locate: Locate::Source(key::digest("sim scope cells scope source", &world.payload())), place: 0 });
        for (&id, body) in world.bodies.iter() {
            m.parts.push(Part { name: body_name(id), locate: Locate::Source(World::body_source(id, body)), place: 0 });
        }
        m.parts.sort_by_key(|part| part.name);
        return Ok(Answered::Done(m));
    }
    if name == scope() {
        let payload = world.payload();
        let mut m = made(name, key::digest("sim scope cells scope out", &payload));
        m.payload = payload;
        return Ok(Answered::Done(m));
    }
    let Some(body) = world.bodies.iter().find(|(id, _)| body_name(**id) == name).map(|(_, body)| body) else {
        return Err(Broke::Harness(format!("{name} is no task of the sim")));
    };
    let Some(input) = task.inputs.iter().find(|input| input.name == scope()) else {
        return Ok(Answered::Need(std::vec![Want { name: scope(), home: Name::units() }]));
    };
    let bytes = store.get(&input.at).ok_or(Broke::Harness(format!("{name} was handed a scope record the store lacks")))?;
    let payload = match record::open(bytes) {
        Ok(Stored::Whole(view)) => view.payload,
        _ => return Err(Broke::Harness(format!("{name} was handed no whole scope record"))),
    };
    let resolved = resolve(&Payload(payload), body)?;
    let mut m = made(name, out_of(&resolved));
    m.cells = recorded(planted, body, &resolved).into_iter().map(|read| CellRead::new(scope(), cell(read.at), read.seen)).collect();
    Ok(Answered::Done(m))
}

// --- One request, driven ----------------------------------------------------------------------

#[derive(Default)]
struct Ended {
    /// Every name a batch ran this request.
    ran: BTreeSet<Name>,
    /// The `last` this request wrote, else the one it was given.
    last: Option<StdVec<u8>>,
    /// Each body's out as the request settled it; `None` for one it never looked at (it stood,
    /// and holds the out it had: [`history`] carries that over).
    outs: BTreeMap<u32, Option<ContentId>>,
    /// What the request knew at its end, for a failing assertion: which cells of each body's
    /// trace it looked up, and the scope's and bodies' counts.
    probe: String,
    /// How many bodies stood: not dirtied, so not even decided by value.
    standing: u32,
}

impl Ended {
    fn bodies_made(&self, world: &World) -> BTreeSet<u32> {
        world.bodies.keys().copied().filter(|id| self.ran.contains(&body_name(*id))).collect()
    }
}

fn never_an_error(_: u16) -> bool {
    false
}

fn config(workers: u32) -> Config {
    let tools = Kind::ALL.map(|kind| Tool(key::digest("sim scope cells kind tool", &[kind as u8])));
    Config { tool: Tool(key::digest("sim scope cells tool", b"")), tools, workers, most_entries: 32, most_jobs: 1 << 16, is_error: never_an_error }
}

const ASK: Ask<'static> = Ask { command: Command::Check, args: b"", gate: Gate::Beside, space: Space::NONE, program: None, facts: &[] };

fn request(tape: &Tape, world: &World, planted: Planted, store: &mut BTreeMap<Key, StdVec<u8>>, last: Option<&[u8]>) -> Result<Ended, Broke> {
    let manifest = b"Cargo.toml".to_vec();
    let files = [FileEntry { path: &manifest, id: world.units_source(), changed: false, manifest: true }];
    let workers = [1, 2, 16][tape.index(3, "workers")];
    let plant = match planted {
        Planted::Drive(plant) => plant,
        _ => Plant::None,
    };
    let mut req = Request::planted_in(config(workers), ASK, plant, blaze_own::testing::heap().alloc()).with_cells(Cells::NONE.with(Kind::CrateScope, cell_of));
    let mut out: Vec<Effect<'static>, Alloc<'static>> = Vec::new_in(blaze_own::testing::heap().alloc());
    // A request that writes no `last` (its certificate stood) leaves the one it was given.
    let mut ended = Ended { last: last.map(<[u8]>::to_vec), ..Ended::default() };
    let mut answered_at_all = false;
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
                        answered.push((*ticket, execute(world, store, task, planted)?));
                    }
                    let no_reads: [(Name, ContentId); 0] = [];
                    let no_findings: [Finding<'_>; 0] = [];
                    let no_paths: [PathOf<'_>; 0] = [];
                    let outputs: StdVec<Output<'_>> = answered
                        .iter()
                        .filter_map(|(_, answer)| match answer {
                            Answered::Done(m) => Some(Output {
                                name: m.name,
                                out: m.out,
                                reads: &no_reads,
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
                Effect::Done(_) => answered_at_all = true,
                Effect::Open { name, .. } => return Err(Broke::Harness(format!("{name} was opened, and the sim has no files"))),
                Effect::StartProgram { .. } | Effect::Stop { .. } => {
                    return Err(Broke::Harness(String::from("`check` started or stopped a program")));
                }
            }
        }
        if answered_at_all {
            if !gets.is_empty() || !batches.is_empty() {
                return Err(Broke::Oracle(String::from("answered with work still out")));
            }
            if !req.failures().is_empty() {
                return Err(Broke::Oracle(format!("tasks failed: {:?}", req.failures())));
            }
            for (&id, body) in world.bodies.iter() {
                ended.outs.insert(id, req.settled_input(body_name(id)).map(|input| input.out));
                let known: StdVec<bool> = resolve(world, body)?.reads.iter().map(|read| req.cell_now(scope(), cell(read.at)).is_some()).collect();
                ended.probe.push_str(&format!("body {id} cells known {known:?}; "));
            }
            let mut profile = *req.profile();
            let pages = profile.pages;
            let scope_counts = *profile.of(Kind::CrateScope);
            let body_counts = *profile.of(Kind::BodyCheck);
            ended.standing = body_counts.standing;
            ended.probe.push_str(&format!("scope {scope_counts:?}; body {body_counts:?}; pages {pages}"));
            return Ok(ended);
        }
    }
    Err(Broke::Harness(format!("more than {MOST_EVENTS} events")))
}

// --- Worlds and edits ---------------------------------------------------------------------------

fn ns_of(tape: &Tape) -> Ns {
    match tape.coin("namespace") {
        true => Ns::Values,
        false => Ns::Types,
    }
}

fn draw_body(tape: &Tape) -> Body {
    let module = tape.index(usize::from(MODULES), "body module") as u8;
    let locals = (0..NAMES).filter(|_| tape.one_in(4, "local")).collect();
    let asks = (0..1 + tape.index(3, "asks")).map(|_| (ns_of(tape), tape.index(usize::from(NAMES), "asked name") as u8)).collect();
    let buckets = match tape.coin("asks a bucket") {
        true => std::vec![tape.index(usize::from(BUCKETS), "bucket asked") as u8],
        false => StdVec::new(),
    };
    Body { module, locals, asks, buckets }
}

fn draw(tape: &Tape) -> World {
    let mut serial = 1;
    let mut modules: StdVec<Module> = (0..=MODULES).map(|_| Module::default()).collect();
    for (m, module) in modules.iter_mut().enumerate() {
        for ns in [Ns::Types, Ns::Values] {
            for x in 0..NAMES {
                if tape.one_in(3, "item") {
                    let public = m == usize::from(PRELUDE) || tape.coin("public");
                    module.items.insert((ns, x), Item { value: serial, public });
                    serial += 1;
                }
            }
        }
        if m < usize::from(MODULES) {
            module.globs = (0..MODULES).filter(|src| usize::from(*src) != m && tape.one_in(3, "glob")).collect();
        }
    }
    let mut buckets = BTreeMap::new();
    for b in 0..BUCKETS {
        let impls: BTreeSet<u16> = (0..tape.index(3, "impls")).map(|k| 500 + 10 * u16::from(b) + k as u16).collect();
        buckets.insert(b, impls);
    }
    let bodies = (0..2 + tape.index(4, "bodies") as u32).map(|id| (id, draw_body(tape))).collect();
    World { modules, buckets, bodies, serial }
}

/// One edit of `world`, and what it was.
fn edit(tape: &Tape, world: &World) -> (World, &'static str) {
    let mut next = world.clone();
    let fresh = next.serial;
    next.serial += 1;
    let m = tape.index(usize::from(MODULES), "edited module");
    let held: StdVec<(Ns, u8)> = next.modules[m].items.keys().copied().collect();
    let what = match tape.index(8, "edit") {
        0 => {
            let (ns, x) = (ns_of(tape), tape.index(usize::from(NAMES), "added name") as u8);
            let public = tape.coin("added public");
            next.modules[m].items.insert((ns, x), Item { value: fresh, public });
            // An item added brings its own body, which asks what it asks.
            if tape.coin("added with a body") {
                let id = next.bodies.keys().next_back().map_or(0, |last| last + 1);
                let mut body = draw_body(tape);
                body.module = m as u8;
                next.bodies.insert(id, body);
            }
            match public {
                true => "a public item added",
                false => "a private item added",
            }
        }
        1 | 2 | 3 if held.is_empty() => "nothing (no item to edit)",
        1 => {
            let at = held[tape.index(held.len(), "removed item")];
            next.modules[m].items.remove(&at);
            "an item removed"
        }
        2 => {
            let at = held[tape.index(held.len(), "changed item")];
            if let Some(item) = next.modules[m].items.get_mut(&at) {
                item.value = fresh;
            }
            "an item changed"
        }
        3 => {
            let at = held[tape.index(held.len(), "item made public or private")];
            if let Some(item) = next.modules[m].items.get_mut(&at) {
                item.public = !item.public;
            }
            "an item made public or private"
        }
        4 => {
            let src = tape.index(usize::from(MODULES), "glob source") as u8;
            if usize::from(src) != m {
                next.modules[m].globs.insert(src);
            }
            "a glob added"
        }
        5 => {
            let globs: StdVec<u8> = next.modules[m].globs.iter().copied().collect();
            if !globs.is_empty() {
                next.modules[m].globs.remove(&globs[tape.index(globs.len(), "glob removed")]);
            }
            "a glob removed"
        }
        6 => {
            let b = tape.index(usize::from(BUCKETS), "bucket") as u8;
            let impls = next.buckets.entry(b).or_default();
            if impls.len() < MOST_IMPLS {
                impls.insert(fresh);
            }
            "an impl added"
        }
        _ => "nothing",
    };
    (next, what)
}

// --- The oracle -------------------------------------------------------------------------------

/// Each body's trace over `world` by the rules.
fn traces(world: &World) -> Result<BTreeMap<u32, StdVec<(Cell, [u8; 32])>>, Broke> {
    let mut all = BTreeMap::new();
    for (&id, body) in world.bodies.iter() {
        all.insert(id, trace(&resolve(world, body)?));
    }
    Ok(all)
}

/// Same as fresh, then the floor, for the request `ended` over `world`. `before` is each body's
/// trace in the last world (none for a body new now); `made` each body's traces some request made
/// it with.
fn judge(
    tape: &Tape,
    world: &World,
    ended: &Ended,
    before: &BTreeMap<u32, StdVec<(Cell, [u8; 32])>>,
    made: &BTreeMap<u32, BTreeSet<StdVec<(Cell, [u8; 32])>>>,
    what: &str,
    strict: bool,
) -> Result<BTreeMap<u32, StdVec<(Cell, [u8; 32])>>, Broke> {
    let after = traces(world)?;
    for (&id, body) in world.bodies.iter() {
        let fresh = out_of(&resolve(world, body)?);
        let settled = ended.outs.get(&id).copied().flatten();
        if settled != Some(fresh) {
            return Err(Broke::Oracle(format!("{NOT_FRESH}: body {id} after {what}: settled {settled:?}, fresh {fresh:?}")));
        }
    }
    let ran = ended.bodies_made(world);
    let moved: BTreeSet<u32> = after.iter().filter(|(id, now)| before.get(id) != Some(now)).map(|(id, _)| *id).collect();
    let must: BTreeSet<u32> = moved.iter().copied().filter(|id| !made.get(id).is_some_and(|seen| seen.contains(&after[id]))).collect();
    if let Some(id) = must.difference(&ran).next() {
        return Err(Broke::Oracle(format!("{UNDER_FLOOR}: body {id} after {what} (made {ran:?}, floor {must:?})")));
    }
    if let Some(id) = ran.difference(&moved).next() {
        return Err(Broke::Oracle(format!("{OVER_FLOOR} was made: body {id} after {what} (made {ran:?}, floor {must:?})")));
    }
    // When the scope moved, every body whose cells stood stands: not even decided by value.
    let unmoved = world.bodies.len() - moved.len();
    if ended.ran.contains(&scope()) && (ended.standing as usize) < unmoved {
        return Err(Broke::Oracle(format!("{OVER_FLOOR} was dirtied: {} of {unmoved} such bodies stood after {what}", ended.standing)));
    }
    // A body whose cells came back to what an earlier record holds is made again today: the
    // decision by value reads its last row's shape, which is not that record's (a glob removed,
    // then put back). Finding it is the walk over earlier shapes (request.rs `Walk`, lane TV),
    // not the cells: noted, not refused.
    let again: BTreeSet<u32> = ran.difference(&must).copied().collect();
    if !again.is_empty() {
        if strict {
            return Err(Broke::Oracle(format!("{AGAIN}: bodies {again:?} after {what}")));
        }
        tape.note(format_args!("after {what}: made again over earlier cells {again:?}"));
    }
    Ok(after)
}

const AGAIN: &str = "a body made again though an earlier record of it saw today's cells";

/// A history: the drawn crate built, then one to four edits, each a request on one store.
fn history(tape: &Tape, planted: Planted) -> Result<(), Broke> {
    history_judged(tape, planted, false)
}

/// [`history`], a body made again over an earlier record's cells refused when `strict`.
fn history_judged(tape: &Tape, planted: Planted, strict: bool) -> Result<(), Broke> {
    let mut world = draw(tape);
    let mut store = BTreeMap::new();
    let first = request(tape, &world, planted, &mut store, None)?;
    let mut made: BTreeMap<u32, BTreeSet<StdVec<(Cell, [u8; 32])>>> = BTreeMap::new();
    let mut before = judge(tape, &world, &first, &BTreeMap::new(), &made, "the first build", strict)?;
    for (id, trace) in before.iter() {
        made.entry(*id).or_default().insert(trace.clone());
    }
    let mut last = first.last;
    let mut outs = first.outs;
    for _ in 0..1 + tape.index(4, "edits") {
        let (next, what) = edit(tape, &world);
        let mut ended = request(tape, &next, planted, &mut store, last.as_deref())?;
        // A body the request never looked at stood: it holds the out the last request left.
        for (id, out) in ended.outs.iter_mut() {
            if out.is_none() {
                *out = outs.get(id).copied().flatten();
            }
        }
        outs = ended.outs.clone();
        let after = judge(tape, &next, &ended, &before, &made, what, strict)?;
        for id in ended.bodies_made(&next) {
            made.entry(id).or_default().insert(after[&id].clone());
        }
        if what.starts_with("nothing") && !ended.ran.is_empty() {
            return Err(Broke::Oracle(format!("an unchanged repeat ran {} tasks", ended.ran.len())));
        }
        tape.note(format_args!("{what}: made {:?}", ended.bodies_made(&next)));
        world = next;
        before = after;
        last = ended.last;
    }
    Ok(())
}

/// The directed case of the brief: a crate of one module and two bodies, one asking `(Types, 0)`
/// and one `(Types, 1)`, no item anywhere; a private `(Types, 0)` added to the module, after
/// `repeats` unchanged requests. Each request's bodies made, in order.
fn private_added(repeats: usize) -> Result<StdVec<(BTreeSet<u32>, String)>, Broke> {
    let tape = Tape::calm();
    let body = |x: u8| Body { module: 0, locals: BTreeSet::new(), asks: std::vec![(Ns::Types, x)], buckets: StdVec::new() };
    let mut world = World {
        modules: (0..=MODULES).map(|_| Module::default()).collect(),
        buckets: BTreeMap::new(),
        bodies: [(0, body(0)), (1, body(1))].into_iter().collect(),
        serial: 1,
    };
    let mut store = BTreeMap::new();
    let mut ended = request(&tape, &world, Planted::None, &mut store, None)?;
    let mut made = std::vec![(ended.bodies_made(&world), ended.probe.clone())];
    for _ in 0..repeats {
        ended = request(&tape, &world, Planted::None, &mut store, ended.last.as_deref())?;
        made.push((ended.bodies_made(&world), ended.probe.clone()));
    }
    world.modules[0].items.insert((Ns::Types, 0), Item { value: 7, public: false });
    ended = request(&tape, &world, Planted::None, &mut store, ended.last.as_deref())?;
    made.push((ended.bodies_made(&world), ended.probe.clone()));
    Ok(made)
}

/// Expected: "test sim_scope_cells::a_private_item_added_makes_its_askers_alone ... ok", printing
/// `private added: [{0, 1}, {0}]; after a repeat: [{0, 1}, {}, {0}]`.
#[test]
fn a_private_item_added_makes_its_askers_alone() {
    let direct = private_added(0).expect("the requests answer");
    let repeated = private_added(1).expect("the requests answer");
    std::println!("private added: {direct:?}; after a repeat: {repeated:?}");
    let floor: BTreeSet<u32> = [0].into_iter().collect();
    assert_eq!(direct.last().map(|m| &m.0), Some(&floor), "the add makes the one body that asked its name: {direct:?}; after a repeat {repeated:?}");
    assert_eq!(repeated.last().map(|m| &m.0), Some(&floor), "an unchanged request between changes nothing of that: {repeated:?}; direct {direct:?}");
}

/// FOR LANE DR (request.rs's walk), kept `#[ignore]` until it passes: the histories of
/// [`scope_edits_make_the_floor_and_equal_a_fresh_build`] with a body made again over cells
/// an earlier record of it saw refused (`AGAIN`). Fails today at seed 40, least tape
/// `0*9 1 0*30 1 0*14 2 0*9 1 0*17 1 0*25 2 0 5 0*16 3 1 0*11 4 2` (`BLAZE_SIM_TAPE`): two
/// bodies in module 1, a glob of module 2 in it; "a glob removed: made {0, 2}", "an item
/// removed: made {}" (a request that made no body between), then "a glob added": bodies
/// {0, 2} made again though their first records saw today's cells. With no request between
/// (the test below) the first record is found: the walk keeps only the shape of the request
/// just before.
#[test]
#[ignore = "lane DR: a body whose cells came back to an earlier record's is made again"]
fn scope_edits_find_an_earlier_record_over_its_cells() {
    check(SimConfig::cases(300), |tape| history_judged(tape, Planted::None, true));
}

/// The simplest return passes today: module 1 holds a public item 0, module 0 globs it, and
/// body 0 in module 0 asks it; the glob removed (made), then put back: the first record
/// answers and nothing is made.
#[test]
fn a_glob_put_back_finds_the_first_record() {
    let tape = Tape::calm();
    let mut world = World {
        modules: (0..=MODULES).map(|_| Module::default()).collect(),
        buckets: BTreeMap::new(),
        bodies: [(0, Body { module: 0, locals: BTreeSet::new(), asks: std::vec![(Ns::Types, 0)], buckets: StdVec::new() })].into_iter().collect(),
        serial: 1,
    };
    world.modules[1].items.insert((Ns::Types, 0), Item { value: 7, public: true });
    world.modules[0].globs.insert(1);
    let mut store = BTreeMap::new();
    let run = |world: &World, store: &mut BTreeMap<Key, StdVec<u8>>, last: Option<&[u8]>| request(&tape, world, Planted::None, store, last).expect("the request answers");
    let mut ended = run(&world, &mut store, None);
    let mut made = std::vec![ended.bodies_made(&world)];
    world.modules[0].globs.remove(&1);
    ended = run(&world, &mut store, ended.last.as_deref());
    made.push(ended.bodies_made(&world));
    world.modules[0].globs.insert(1);
    ended = run(&world, &mut store, ended.last.as_deref());
    made.push(ended.bodies_made(&world));
    assert_eq!(made.last(), Some(&BTreeSet::new()), "the glob put back made the body again: {made:?}");
}

/// A control: the clean history passes and the plant is caught with `words`.
fn caught(planted: Planted, words: &str) {
    control(SimConfig::cases(300), Planted::None, planted, words, history);
}

/// Expected: "test sim_scope_cells::scope_edits_make_the_floor_and_equal_a_fresh_build ... ok".
#[test]
fn scope_edits_make_the_floor_and_equal_a_fresh_build() {
    check(SimConfig::cases(300), |tape| history(tape, Planted::None));
}

/// Expected: "test sim_scope_cells::control_a_failed_lookup_left_out_is_caught ... ok".
#[test]
fn control_a_failed_lookup_left_out_is_caught() {
    caught(Planted::FailedLeftOut, NOT_FRESH);
}

/// Expected: "test sim_scope_cells::control_a_nearer_absent_left_out_is_caught ... ok".
#[test]
fn control_a_nearer_absent_left_out_is_caught() {
    caught(Planted::NearerLeftOut, NOT_FRESH);
}

/// Expected: "test sim_scope_cells::control_a_glob_source_absent_left_out_is_caught ... ok".
#[test]
fn control_a_glob_source_absent_left_out_is_caught() {
    caught(Planted::GlobAbsentLeftOut, NOT_FRESH);
}

/// Expected: "test sim_scope_cells::control_a_bucket_left_out_is_caught ... ok".
#[test]
fn control_a_bucket_left_out_is_caught() {
    caught(Planted::BucketLeftOut, NOT_FRESH);
}

/// Expected: "test sim_scope_cells::control_cell_readers_dirtied_as_whole_is_caught ... ok".
#[test]
fn control_cell_readers_dirtied_as_whole_is_caught() {
    caught(Planted::Drive(Plant::CellsAsWhole), OVER_FLOOR);
}
