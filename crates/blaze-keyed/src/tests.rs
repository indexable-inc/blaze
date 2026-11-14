//! The driver under deterministic simulation (rule 43, 64), on blaze-sim: three crates of four
//! items in two files each, edited as the tape chooses; every request runs the real driver, the
//! real record formats, the real [`Universe::answer`] and the real outside machine, with the
//! host's answers delivered in an order the tape chooses (a [`Flight`]), puts landing when
//! delivered, and the process killed at points the tape chooses.
//!
//! The model's tasks read what they are handed and ask by value-dependent shapes: a surface asks
//! a dependency's second signature when its first one's digest is odd, a body asks a second
//! signature of its own unit when the first one's is; bodies read their own unit's surface, a
//! dependency's, an absent export, what the asking unit sees, a const another body of the unit
//! makes, a const a dependency's body makes and a late variable.
//!
//! The oracle, after every request that finished:
//! - From scratch: an independent interpreter computes every record by value (it answers each
//!   ask from the model's values, never from a record or the store); every unit's manifest the
//!   certificate names holds, for its surface and every item, a record whose result is the
//!   fresh one's, and every read in it and every row of its dictionary is true now; its made
//!   table is the fresh one's.
//! - The store holds everything a certificate, a mark or a manifest names.
//! - No check after a proof (`rechecks` is zero); at most `max(workers, 1)` tasks run.
//! - A request after an answer with no edit is certified.
//! - Under [`Audit`], nothing faults without a plant.
//! - Refinement: `Resident` gives the same outcomes and leaves the same store as `RecordsOnly`.
//!
//! Every plant has a control that the oracle refuses by name, and `every_case_is_reached`
//! fails by name for a case no run reaches.

use std::{
    alloc::Global,
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
};

use blaze_format::Aligned;
use blaze_ids::ContentId;
use blaze_sim::{Broke, Config, Flight, Nanos, Tape};

use crate::{
    PLANT, Plant,
    driver::{Done, Driver, Effect, Filed, Input, Outcome, Plan, Ran, Step, TableRef, Task, UnitPlan},
    exec::Local,
    front::{FileItems, Table, Universe},
    kind::{
        Body, BodyInputs, Certificate, CertificateInputs, FileImage, FileInputs, Key, Kind, Late, LateRead, Namespace,
        Settings, Surface, SurfaceInputs, Tag, ToolId, UnitInputs, UnitManifest,
    },
    memo::{Audit, Memo, RecordsOnly, Resident},
    outside, planted,
    pool::Ticket,
    proof::{self, Checks, Snapshot},
    record::{self, ChunkRoot, Fact, FactKind, ManifestRoot, Owner, Route, StableId, Traced},
    store::Put,
};

const UNITS: usize = 3;
const ITEMS: usize = 4;
const DEPS: [&[u32]; UNITS] = [&[], &[0], &[0, 1]];
const WORKERS: u32 = 2;
/// The request's one outside ask, in the model's encoding: the whole tree.
const SHAPE: &[u8] = b"the model's tree";
const VARS: [&[u8]; 2] = [b"MODEL_A", b"MODEL_B"];

const TOOL: ToolId = ToolId { tool: ContentId([1; 32]), target: ContentId([4; 32]), namespace: Namespace::PUBLIC };
const SETTINGS: Settings = Settings(ContentId([2; 32]));

fn certificate() -> Key<Certificate> {
    Certificate::key(&CertificateInputs { tool: TOOL, base: ContentId([5; 32]), of: PhantomData })
}

fn digest(bytes: &[u8]) -> ContentId {
    record::digest(bytes)
}

fn unit_id(c: usize) -> ContentId {
    ContentId([0x10 + c as u8; 32])
}

fn owner(c: usize) -> Owner {
    Owner([0x10 + c as u8; 8])
}

fn unit_of(owner: Owner) -> Option<usize> {
    (0..UNITS).find(|&c| self::owner(c) == owner)
}

fn stable(c: usize, i: usize) -> StableId {
    StableId::of(format!("c{c}::item{i}").as_bytes(), 1)
}

fn sig_fact(c: usize, i: usize) -> ContentId {
    record::fact(FactKind::Signature, owner(c), 1, &stable(c, i), b"")
}

/// An export a unit has only while its first item's signature is 2: absent otherwise.
fn extra_fact(c: usize) -> ContentId {
    record::fact(FactKind::Signature, owner(c), 1, &StableId::of(format!("c{c}::extra").as_bytes(), 1), b"")
}

/// The const item 3 of unit `c` makes.
fn made_fact(c: usize) -> ContentId {
    record::fact(FactKind::ConstValue, owner(c), 2, &stable(c, 3), b"")
}

/// What the asking unit sees as a whole: no surface has its row, so its seen row answers it.
fn asker_fact() -> ContentId {
    record::fact(FactKind::WholeScope, Owner::ASKER, 3, &StableId(ContentId([9; 32])), b"")
}

fn late_fact(var: usize) -> ContentId {
    record::late_id(Late::Env as u8, VARS[var])
}

/// The late read of unit `c`: (the item that reads it, the variable).
fn late_of(c: usize) -> Option<(usize, usize)> {
    match c {
        1 => Some((0, 1)),
        2 => Some((1, 0)),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Item {
    sig: u8,
    body: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct World {
    items: [[Item; ITEMS]; UNITS],
    env: [Option<u8>; 2],
}

impl World {
    fn new() -> World {
        World { items: [[Item { sig: 0, body: 0 }; ITEMS]; UNITS], env: [None; 2] }
    }

    fn digest(&self) -> ContentId {
        let mut bytes = Vec::new();
        for unit in &self.items {
            for item in unit {
                bytes.extend_from_slice(&[item.sig, item.body]);
            }
        }
        for var in self.env {
            bytes.extend_from_slice(&[var.is_some() as u8, var.unwrap_or(0)]);
        }
        digest(&bytes)
    }

    fn sig_digest(&self, c: usize, i: usize) -> ContentId {
        digest(&[b's', self.items[c][i].sig])
    }

    fn made_value(&self, c: usize) -> ContentId {
        let item = self.items[c][3];
        digest(&[b'm', (item.sig + item.body) % 3])
    }

    fn env_value(&self, var: usize) -> Option<ContentId> {
        self.env[var].map(|v| digest(&[b'e', v]))
    }

    fn late_answer(&self, var: usize) -> ContentId {
        self.env_value(var).unwrap_or_else(|| digest(b"unset"))
    }

    /// Unit `c`'s surface rows, sorted by id.
    fn rows(&self, c: usize) -> Vec<Fact> {
        let mut rows: Vec<Fact> = (0..ITEMS).map(|i| Fact { id: sig_fact(c, i), digest: self.sig_digest(c, i) }).collect();
        if self.items[c][0].sig == 2 {
            rows.push(Fact { id: extra_fact(c), digest: digest(b"extra") });
        }
        let mut seen = vec![c as u8];
        seen.extend(self.items[c].iter().map(|item| item.sig));
        rows.push(Fact { id: record::seen_id(), digest: digest(&seen) });
        rows.sort_by_key(|fact| fact.id);
        rows
    }

    /// What unit `c`'s bodies make, sorted by id.
    fn made(&self, c: usize) -> Vec<Fact> {
        vec![Fact { id: made_fact(c), digest: self.made_value(c) }]
    }

    /// The answer now of `id` asked by unit `asker`'s surface (`body` false: its dependencies
    /// only) or a body: by value, from the model.
    fn value(&self, asker: usize, body: bool, id: &ContentId) -> Result<ContentId, String> {
        let visible = |d: usize| DEPS[asker].contains(&(d as u32)) || (body && d == asker);
        let route = record::route(id).map_err(|unrouted| format!("an unrouted ask {unrouted:?}"))?;
        Ok(match route {
            Route::Outside => match (0..2).find(|&var| late_fact(var) == *id) {
                Some(var) => self.late_answer(var),
                None => return Err("an outside ask of no variable".into()),
            },
            Route::Asker if !body => return Err("an ask of what the asker sees by a surface".into()),
            Route::Asker => {
                let rows = self.rows(asker);
                match rows.iter().find(|row| row.id == *id).or_else(|| rows.iter().find(|row| row.id == record::seen_id())) {
                    Some(row) => row.digest,
                    None => record::absent(id),
                }
            }
            Route::Surface(owner) => match unit_of(owner).filter(|&d| visible(d)) {
                Some(d) => self.rows(d).iter().find(|row| row.id == *id).map_or_else(|| record::absent(id), |row| row.digest),
                None => record::GONE,
            },
            Route::Made(owner) => match unit_of(owner).filter(|&d| visible(d)) {
                Some(d) => self.made(d).iter().find(|row| row.id == *id).map_or_else(|| record::absent(id), |row| row.digest),
                None => record::GONE,
            },
        })
    }

    fn body_key(&self, c: usize, i: usize) -> Key<Body> {
        let item = self.items[c][i];
        Body::key(&BodyInputs { tool: TOOL, settings: SETTINGS, unit: unit_id(c), item: stable(c, i), text: digest(&[b't', item.sig, item.body]) })
    }

    fn surface_key(&self, c: usize) -> Key<Surface> {
        let pieces: Vec<ContentId> = (0..ITEMS).map(|i| digest(&[b'p', i as u8, self.items[c][i].sig])).collect();
        Surface::key(&SurfaceInputs { tool: TOOL, settings: SETTINGS, unit: unit_id(c), pieces: &pieces })
    }

    fn file_key(&self, c: usize, f: usize) -> Key<FileImage> {
        let (a, b) = (self.items[c][2 * f], self.items[c][2 * f + 1]);
        FileImage::key(&FileInputs { tool: TOOL, edition: 2021, bytes: &[c as u8, f as u8, a.sig, a.body, b.sig, b.body] })
    }

    fn text_key(&self, c: usize) -> Key<UnitManifest> {
        let files = [self.file_key(c, 0), self.file_key(c, 1)];
        let late: Vec<LateRead<'_>> =
            late_of(c).map(|(_, var)| LateRead { late: Late::Env, name: VARS[var], value: self.env_value(var) }).into_iter().collect();
        UnitManifest::key(&UnitInputs { tool: TOOL, settings: SETTINGS, unit: unit_id(c), files: &files, late: &late })
    }

    /// Unit `c`'s files with their items, each file's in stable-id order.
    fn keyed(&self, c: usize) -> (Vec<FileItems>, Vec<(StableId, Key<Body>)>) {
        let mut files = Vec::new();
        let mut items = Vec::new();
        for f in 0..2 {
            let mut of: Vec<(StableId, Key<Body>)> = (2 * f..2 * f + 2).map(|i| (stable(c, i), self.body_key(c, i))).collect();
            of.sort_by_key(|(item, _)| *item);
            files.push(FileItems { image: self.file_key(c, f), at: items.len() as u32, len: of.len() as u32 });
            items.extend(of);
        }
        (files, items)
    }

    /// The late reads every task may ask, sorted by id.
    fn late(&self) -> Vec<Fact> {
        let mut late: Vec<Fact> = (0..2).map(|var| Fact { id: late_fact(var), digest: self.late_answer(var) }).collect();
        late.sort_by_key(|fact| fact.id);
        late
    }
}

type Ask<'a> = dyn FnMut(&ContentId) -> Result<ContentId, String> + 'a;

/// The reads a task asked, and the answers in the order asked.
#[derive(Default)]
struct Asked {
    reads: BTreeMap<ContentId, ContentId>,
    order: Vec<ContentId>,
    last: Option<ContentId>,
}

impl Asked {
    fn ask(&mut self, ask: &mut Ask<'_>, id: ContentId) -> Result<ContentId, String> {
        let answer = ask(&id)?;
        self.reads.insert(id, answer);
        self.order.push(answer);
        self.last = Some(id);
        Ok(answer)
    }

    /// The reads written, with the planted bugs of the driver-side tasks.
    fn reads(&self, plants: bool) -> Vec<Fact> {
        let mut reads: Vec<Fact> = self.reads.iter().map(|(id, digest)| Fact { id: *id, digest: *digest }).collect();
        if plants && planted(Plant::ReadDropped) {
            reads.retain(|read| Some(read.id) != self.last);
        }
        if plants && planted(Plant::AbsenceDropped) {
            reads.retain(|read| read.digest != record::absent(&read.id));
        }
        if plants && planted(Plant::MadeNotAsked) {
            reads.retain(|read| !matches!(record::route(&read.id), Ok(Route::Made(_))));
        }
        reads
    }

    /// `earlier`: the requests begun before the one this task runs in. Under
    /// [`Plant::TwoValuesUnderAddress`] the work is not a function of its key and reads: a run
    /// in any request but the first adds that count to its payload, so the first request files
    /// what a fresh build files and a later run of the same work makes other bytes at the same
    /// address (what only the audit's redo of a hit sees).
    fn payload(&self, own: &[u8], earlier: u32, plants: bool) -> Vec<u8> {
        let mut bytes = own.to_vec();
        self.order.iter().for_each(|answer| bytes.extend_from_slice(&answer.0));
        if plants && planted(Plant::TwoValuesUnderAddress) && earlier > 0 {
            bytes.extend_from_slice(&earlier.to_le_bytes());
        }
        digest(&bytes).0.to_vec()
    }
}

fn odd(answer: ContentId) -> bool {
    answer.0[0] & 1 == 1
}

/// Unit `c`'s surface, asking through `ask`.
fn surface_task(world: &World, c: usize, plants: bool, earlier: u32, ask: &mut Ask<'_>) -> Result<Vec<u8>, String> {
    let mut asked = Asked::default();
    for &d in DEPS[c] {
        if odd(asked.ask(ask, sig_fact(d as usize, 0))?) {
            asked.ask(ask, sig_fact(d as usize, 1))?;
        }
    }
    let mut own = vec![c as u8];
    own.extend(world.items[c].iter().map(|item| item.sig));
    let payload = asked.payload(&own, earlier, plants);
    Ok(record::write_read::<Surface>(world.surface_key(c), &asked.reads(plants), &world.rows(c), &payload).1)
}

/// Item `i` of unit `c`'s body, asking through `ask`.
fn body_task(world: &World, c: usize, i: usize, plants: bool, earlier: u32, ask: &mut Ask<'_>) -> Result<Vec<u8>, String> {
    let mut asked = Asked::default();
    if odd(asked.ask(ask, sig_fact(c, (i + 1) % ITEMS))?) {
        asked.ask(ask, sig_fact(c, (i + 2) % ITEMS))?;
    }
    if c > 0 && i < 3 {
        asked.ask(ask, sig_fact(DEPS[c][0] as usize, i))?;
    }
    asked.ask(ask, extra_fact(c))?;
    if i == 0 {
        asked.ask(ask, asker_fact())?;
    }
    if i == 2 {
        asked.ask(ask, made_fact(c))?;
    }
    if c == 1 && i == 3 {
        asked.ask(ask, made_fact(0))?;
    }
    if let Some((reader, var)) = late_of(c)
        && reader == i
    {
        asked.ask(ask, late_fact(var))?;
    }
    let item = world.items[c][i];
    let facts = if i == 3 { world.made(c) } else { Vec::new() };
    let payload = asked.payload(&[c as u8, i as u8, item.sig, item.body], earlier, plants);
    Ok(record::write_read::<Body>(world.body_key(c, i), &asked.reads(plants), &facts, &payload).1)
}

fn result_of(bytes: &[u8]) -> Result<ContentId, String> {
    let root = blaze_format::open::<Traced>(bytes).map_err(|error| format!("{error}"))?;
    record::result_digest(root, bytes).map_err(|error| format!("{error}"))
}

/// What a from-scratch build makes of one unit: by value.
struct Fresh {
    text: Key<UnitManifest>,
    surface: ContentId,
    /// By stable id: (key, result).
    bodies: BTreeMap<StableId, (Key<Body>, ContentId)>,
    made: Vec<Fact>,
}

fn fresh(world: &World) -> Result<Vec<Fresh>, String> {
    (0..UNITS)
        .map(|c| {
            let surface = surface_task(world, c, false, 0, &mut |id: &ContentId| world.value(c, false, id))?;
            let mut bodies = BTreeMap::new();
            for i in 0..ITEMS {
                let bytes = body_task(world, c, i, false, 0, &mut |id: &ContentId| world.value(c, true, id))?;
                bodies.insert(stable(c, i), (world.body_key(c, i), result_of(&bytes)?));
            }
            Ok(Fresh { text: world.text_key(c), surface: result_of(&surface)?, bodies, made: world.made(c) })
        })
        .collect()
}

enum Answer {
    Got(Ticket, Option<Aligned>),
    Answers(Ticket, Vec<ContentId>),
    Keyed(Ticket, Key<UnitManifest>, Key<Surface>, Vec<FileItems>, Vec<(StableId, Key<Body>)>),
    Wrote(Ticket, Aligned),
    Put(Ticket, Tag, ContentId, Aligned),
    Reads(Ticket),
}

type Disk = BTreeMap<(u16, ContentId), Aligned>;

struct Sim {
    world: World,
    disk: Disk,
    /// The manifests the last finished request's certificate names.
    last: [Option<Key<UnitManifest>>; UNITS],
    in_flight: u32,
    /// Requests begun, killed ones included.
    requests: u32,
}

impl Sim {
    fn new() -> Sim {
        Sim { world: World::new(), disk: Disk::new(), last: [None; UNITS], in_flight: 0, requests: 0 }
    }

    fn get(&self, tag: Tag, key: ContentId) -> Option<&Aligned> {
        self.disk.get(&(tag.0, key))
    }

    /// Runs `f` over the universe `refs` and `made` name, its tables read from the store.
    /// What goes wrong is the driver's (a table it names, the order it gives): the oracle's.
    fn with_universe<R>(&self, asker: u32, refs: &[TableRef], made: &[Fact], f: impl FnOnce(Universe<'_>) -> R) -> Result<R, Broke> {
        let mut held: Vec<(u32, Owner, Aligned, Vec<Fact>)> = Vec::new();
        for table in refs {
            // A table with no surface yet (the asker's own, before its surface settles) is
            // not in the universe.
            let Some(surface) = table.surface else { continue };
            let Some(bytes) = self.get(Surface::TAG, surface.id()) else {
                return Err(Broke::Oracle(format!("unit {}'s table names a surface the store does not hold", table.unit)));
            };
            let made = match table.manifest {
                None => made.to_vec(),
                Some(manifest) => {
                    let Some(root) = self.get(UnitManifest::TAG, manifest.id()) else {
                        return Err(Broke::Oracle(format!("unit {}'s table names a manifest the store does not hold", table.unit)));
                    };
                    let opened: &ManifestRoot = record::open::<UnitManifest>(root, &manifest).map_err(|e| Broke::Oracle(e.to_string()))?;
                    opened.made.follow(root).map_err(|e| Broke::Oracle(e.to_string()))?.to_vec()
                }
            };
            held.push((table.unit, table.owner, bytes.clone(), made));
        }
        let mut tables = Vec::new();
        for (unit, owner, surface, made) in &held {
            tables.push(Table::of(*unit, *owner, surface, made).map_err(|why| Broke::Oracle(format!("{why:?}")))?);
        }
        if tables.windows(2).any(|pair| pair[0].owner >= pair[1].owner) {
            return Err(Broke::Oracle("a universe not sorted by owner".into()));
        }
        let late = self.world.late();
        Ok(f(Universe { asker, tables: &tables, outside: &late }))
    }

    /// What goes wrong is the driver's (a task it starts, a universe it hands out): the
    /// oracle's.
    fn perform(&mut self, effect: &Effect<'_>) -> Result<Answer, Broke> {
        Ok(match *effect {
            Effect::Get { ticket, tag, key, .. } => Answer::Got(ticket, self.get(tag, key).cloned()),
            Effect::Outside { ticket, .. } => Answer::Answers(ticket, vec![self.world.digest()]),
            Effect::Answer { ticket, unit, asks, universe, made } => {
                let answers = self.with_universe(unit, universe, made, |universe| {
                    asks.iter()
                        .map(|id| universe.answer(id).map_err(|why| format!("an ask the universe refuses: {why:?}")))
                        .collect::<Result<Vec<ContentId>, String>>()
                })?;
                Answer::Answers(ticket, answers.map_err(Broke::Oracle)?)
            }
            Effect::Run { ticket, task, universe, made, .. } => {
                self.in_flight += 1;
                if self.in_flight > WORKERS {
                    return Err(Broke::Oracle(format!("{} tasks at once: more tasks than workers", self.in_flight)));
                }
                let (world, earlier) = (self.world, self.requests.saturating_sub(1));
                match task {
                    Task::Key { unit } => {
                        let c = unit as usize;
                        let (files, items) = world.keyed(c);
                        Answer::Keyed(ticket, world.text_key(c), world.surface_key(c), files, items)
                    }
                    Task::Surface { unit, key } => {
                        let c = unit as usize;
                        if key != world.surface_key(c) {
                            return Err(Broke::Oracle(format!("unit {c}'s surface asked under another key")));
                        }
                        let bytes = self
                            .with_universe(unit, universe, made, |universe| {
                                surface_task(&world, c, true, earlier, &mut |id: &ContentId| {
                                    universe.answer(id).map_err(|why| format!("{why:?}"))
                                })
                            })?
                            .map_err(Broke::Oracle)?;
                        Answer::Wrote(ticket, Aligned::copy_from(&bytes))
                    }
                    Task::Body { unit, item, key } => {
                        let c = unit as usize;
                        let Some(i) = (0..ITEMS).find(|&i| stable(c, i) == item) else {
                            return Err(Broke::Oracle(format!("a body task for no item of unit {c}")));
                        };
                        if key != world.body_key(c, i) {
                            return Err(Broke::Oracle(format!("unit {c} item {i}'s body asked under another key")));
                        }
                        let bytes = self
                            .with_universe(unit, universe, made, |universe| {
                                body_task(&world, c, i, true, earlier, &mut |id: &ContentId| {
                                    universe.answer(id).map_err(|why| format!("{why:?}"))
                                })
                            })?
                            .map_err(Broke::Oracle)?;
                        Answer::Wrote(ticket, Aligned::copy_from(&bytes))
                    }
                }
            }
            Effect::Put { ticket, tag, key, bytes, .. } => Answer::Put(ticket, tag, key, Aligned::copy_from(bytes)),
            Effect::Reads { ticket } => Answer::Reads(ticket),
        })
    }

    fn deliver<M: Memo>(&mut self, driver: &mut Driver<'_, '_, M, Local, Global>, answer: Answer) {
        match answer {
            Answer::Got(ticket, record) => driver.give(Input::Got { ticket, record: record.as_deref() }),
            Answer::Answers(ticket, now) => driver.give(Input::Answers { ticket, now: &now }),
            Answer::Keyed(ticket, manifest, surface, files, items) => {
                self.in_flight -= 1;
                let outcome = Ok(Done::Keyed { manifest, surface, files: &files, items: &items });
                driver.give(Input::Ran { ticket, outcome });
            }
            Answer::Wrote(ticket, record) => {
                self.in_flight -= 1;
                driver.give(Input::Ran { ticket, outcome: Ok(Done::Wrote { record: &record }) });
            }
            Answer::Put(ticket, tag, key, bytes) => {
                // A put lands when it is answered: a kill before drops it.
                let put = match self.disk.get(&(tag.0, key)) {
                    Some(held) if **held == *bytes => Put::Same,
                    Some(_) => Put::Other,
                    None => {
                        self.disk.insert((tag.0, key), bytes);
                        Put::Written
                    }
                };
                driver.give(Input::Put { ticket, put: Ok(put) });
            }
            Answer::Reads(ticket) => {
                let world = self.world.digest();
                driver.give(Input::Reads { ticket, shape: SHAPE, then: &[world], now: &[world] });
            }
        }
    }

    /// One request, to its outcome and what it ran; `None` when it was killed.
    fn request<M: Memo>(&mut self, memo: &mut M, tape: &Tape, kill: Option<u32>) -> Result<Option<Outcome>, Broke> {
        self.in_flight = 0;
        self.requests += 1;
        let plans: Vec<UnitPlan<'static>> =
            (0..UNITS).map(|c| UnitPlan { deps: DEPS[c], last: self.last[c], namespace: Namespace::LOCAL, owner: owner(c) }).collect();
        let outcome = proof::scope(|snap| self.drive(snap, memo, tape, kill, &plans))?;
        if let Some(outcome) = &outcome {
            self.last = self.check_end(outcome, M::AUDIT)?.map(Some);
        }
        Ok(outcome)
    }

    /// The host's answers are a [`Flight`]: each step the tape chooses whether one is delivered
    /// before the driver is asked again, and which; calm asks the driver first and delivers the
    /// oldest when it waits.
    fn drive<M: Memo>(&mut self, snap: Snapshot<'_>, memo: &mut M, tape: &Tape, kill: Option<u32>, plans: &[UnitPlan<'_>]) -> Result<Option<Outcome>, Broke> {
        let plan = Plan { certificate: certificate(), namespace: Namespace::LOCAL, units: plans };
        let mut driver =
            Driver::new(snap, memo, Local { workers: WORKERS }, plan, Global).map_err(|error| Broke::Harness(format!("{error:?}")))?;
        let mut flight: Flight<Answer> = Flight::new();
        let mut performed = 0u32;
        for _ in 0..50_000 {
            if !flight.is_empty() && tape.coin("deliver early") {
                let Some(answer) = flight.deliver(tape, Nanos(0)) else {
                    return Err(Broke::Harness("answers are in flight, yet none is due".into()));
                };
                self.deliver(&mut driver, answer);
                continue;
            }
            enum Act {
                Exit(Outcome),
                Asked(Answer),
                Wait,
            }
            let act = match driver.next() {
                Step::Exit(outcome) => Act::Exit(outcome.clone()),
                Step::Do(effect) => {
                    performed += 1;
                    if kill == Some(performed) {
                        // A kill: what was in flight is gone, unanswered puts with it.
                        flight.kill();
                        return Ok(None);
                    }
                    Act::Asked(self.perform(&effect)?)
                }
                Step::Wait(_) => Act::Wait,
            };
            match act {
                Act::Exit(outcome) => {
                    if let Outcome::Certified { .. } = outcome
                        && driver.certified().is_none()
                    {
                        return Err(Broke::Oracle("a certified outcome with no certificate proof".into()));
                    }
                    return Ok(Some(outcome));
                }
                Act::Asked(answer) => flight.ask(answer),
                Act::Wait => {
                    let Some(answer) = flight.deliver(tape, Nanos(0)) else {
                        return Err(Broke::Oracle("the driver waits with nothing asked".into()));
                    };
                    self.deliver(&mut driver, answer);
                }
            }
        }
        Err(Broke::Harness("no outcome in 50000 steps".into()))
    }

    /// The oracle at a request's end: the manifests the certificate names, each checked
    /// against a from-scratch build.
    fn check_end(&self, outcome: &Outcome, audit: bool) -> Result<[Key<UnitManifest>; UNITS], Broke> {
        let checks: Checks = match outcome {
            Outcome::Certified { checks } => *checks,
            Outcome::Built { ran, filed: Filed::Certificate } => ran.checks,
            Outcome::Built { ran, filed: Filed::Audited } if audit => ran.checks,
            other => return Err(Broke::Oracle(format!("the request ended {other:?}"))),
        };
        if checks.rechecks != 0 {
            return Err(Broke::Oracle(format!("{} checks of an item already proved: a check after a proof", checks.rechecks)));
        }
        let address = outside::address(certificate(), SHAPE, &[self.world.digest()]);
        let Some(bytes) = self.get(Certificate::TAG, address.id()) else {
            return Err(Broke::Oracle("an answer whose certificate the store does not hold".into()));
        };
        let root = record::open::<Certificate>(bytes, &address).map_err(|e| Broke::Oracle(e.to_string()))?;
        let payload = root.payload.follow(bytes).map_err(|e| Broke::Oracle(e.to_string()))?;
        if payload.len() != 32 * UNITS {
            return Err(Broke::Oracle("a certificate naming another number of manifests".into()));
        }
        let fresh = fresh(&self.world).map_err(Broke::Harness)?;
        let mut manifests = [Key::from_id(ContentId([0; 32])); UNITS];
        for c in 0..UNITS {
            let mut id = [0u8; 32];
            id.copy_from_slice(&payload[32 * c..32 * c + 32]);
            manifests[c] = Key::from_id(ContentId(id));
            self.check_unit(c, manifests[c], &fresh[c])?;
        }
        Ok(manifests)
    }

    /// A read the model cannot answer is the harness's; one not true now, the oracle's.
    fn true_now(&self, c: usize, body: bool, reads: &[Fact], what: &str) -> Result<(), Broke> {
        for read in reads {
            if self.world.value(c, body, &read.id).map_err(Broke::Harness)? != read.digest {
                return Err(Broke::Oracle(format!("unit {c}'s {what} holds a read that is not true now: not what a fresh build files")));
            }
        }
        Ok(())
    }

    fn check_unit(&self, c: usize, manifest: Key<UnitManifest>, fresh: &Fresh) -> Result<(), Broke> {
        let Some(bytes) = self.get(UnitManifest::TAG, manifest.id()) else {
            return Err(Broke::Oracle(format!("unit {c}'s manifest is named but the store does not hold it")));
        };
        let root: &ManifestRoot = record::open::<UnitManifest>(bytes, &manifest).map_err(|e| Broke::Oracle(e.to_string()))?;
        if root.key != fresh.text.id() {
            return Err(Broke::Oracle(format!("unit {c}'s manifest is of another text: not what a fresh build files")));
        }
        let Some(surface) = self.get(Surface::TAG, root.surface) else {
            return Err(Broke::Oracle(format!("unit {c}'s surface is named but the store does not hold it")));
        };
        if result_of(surface).map_err(Broke::Oracle)? != fresh.surface || root.surface_result != fresh.surface {
            return Err(Broke::Oracle(format!("unit {c}'s surface is not what a fresh build files")));
        }
        let surface_root = blaze_format::open::<Traced>(surface).map_err(|e| Broke::Oracle(e.to_string()))?;
        self.true_now(c, false, surface_root.reads.follow(surface).map_err(|e| Broke::Oracle(e.to_string()))?, "surface")?;
        let dictionary = root.dictionary.follow(bytes).map_err(|e| Broke::Oracle(e.to_string()))?;
        self.true_now(c, true, dictionary, "dictionary")?;
        if root.made.follow(bytes).map_err(|e| Broke::Oracle(e.to_string()))? != &fresh.made[..] {
            return Err(Broke::Oracle(format!("unit {c}'s made table is not what a fresh build files")));
        }
        let mut items = BTreeSet::new();
        for file in root.files.follow(bytes).map_err(|e| Broke::Oracle(e.to_string()))? {
            let Some(chunk) = self.get(crate::kind::UnitChunk::TAG, file.chunk) else {
                return Err(Broke::Oracle(format!("unit {c}'s chunk is named but the store does not hold it")));
            };
            let chunk_root: &ChunkRoot =
                record::open::<crate::kind::UnitChunk>(chunk, &Key::from_id(file.chunk)).map_err(|e| Broke::Oracle(e.to_string()))?;
            for entry in chunk_root.entries.follow(chunk).map_err(|e| Broke::Oracle(e.to_string()))? {
                let item = StableId(entry.item);
                let Some((key, result)) = fresh.bodies.get(&item) else {
                    return Err(Broke::Oracle(format!("unit {c}'s manifest names an item it does not have")));
                };
                if entry.key != key.id() {
                    return Err(Broke::Oracle(format!("unit {c}'s entry is under another key: not what a fresh build files")));
                }
                let Some(record) = self.get(Body::TAG, entry.address) else {
                    return Err(Broke::Oracle(format!("unit {c}'s body is named but the store does not hold it")));
                };
                if result_of(record).map_err(Broke::Oracle)? != *result || entry.result != *result {
                    return Err(Broke::Oracle(format!("unit {c}'s body is not what a fresh build files")));
                }
                let body_root = blaze_format::open::<Traced>(record).map_err(|e| Broke::Oracle(e.to_string()))?;
                self.true_now(c, true, body_root.reads.follow(record).map_err(|e| Broke::Oracle(e.to_string()))?, "body")?;
                items.insert(item);
            }
        }
        if items.len() != ITEMS {
            return Err(Broke::Oracle(format!("unit {c}'s manifest names {} items of {ITEMS}", items.len())));
        }
        Ok(())
    }

    fn apply(&mut self, op: Op) {
        match op {
            Op::Body(c, i, v) => self.world.items[c][i].body = v,
            Op::Sig(c, i, v) => self.world.items[c][i].sig = v,
            Op::Env(var, value) => self.world.env[var] = value,
            Op::Request { .. } | Op::Restart => {}
        }
    }

    /// The fresh records the store does not hold: (surfaces, bodies) a request must run.
    fn missing(&self) -> Result<(u32, u32), String> {
        let (mut surfaces, mut bodies) = (0, 0);
        for c in 0..UNITS {
            let surface = surface_task(&self.world, c, false, 0, &mut |id: &ContentId| self.world.value(c, false, id))?;
            let root = blaze_format::open::<Traced>(&surface).map_err(|e| e.to_string())?;
            surfaces += u32::from(self.get(Surface::TAG, root.head.key).is_none());
            for i in 0..ITEMS {
                let body = body_task(&self.world, c, i, false, 0, &mut |id: &ContentId| self.world.value(c, true, id))?;
                let root = blaze_format::open::<Traced>(&body).map_err(|e| e.to_string())?;
                bodies += u32::from(self.get(Body::TAG, root.head.key).is_none());
            }
        }
        Ok((surfaces, bodies))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Body(usize, usize, u8),
    Sig(usize, usize, u8),
    Env(usize, Option<u8>),
    Request { kill: Option<u32> },
    /// A new process: the resident memo starts empty.
    Restart,
}

const REQUEST: Op = Op::Request { kill: None };

/// Runs `ops` from an empty store, each process with a memo from `make`, the host's schedule
/// drawn from `tape`: the outcomes of the requests that finished, and the simulation.
fn run<M: Memo>(ops: &[Op], tape: &Tape, plant: Plant, kills: bool, make: impl Fn() -> M) -> Result<(Vec<Outcome>, Sim), Broke> {
    PLANT.set(plant);
    let result = (|| {
        let mut sim = Sim::new();
        let mut memo = make();
        let mut outcomes = Vec::new();
        let mut answered = false;
        for &op in ops {
            match op {
                Op::Request { kill } => match sim.request(&mut memo, tape, kill.filter(|_| kills))? {
                    Some(outcome) => {
                        let settled = matches!(outcome, Outcome::Certified { .. })
                            || (M::AUDIT && matches!(outcome, Outcome::Built { filed: Filed::Audited, .. }));
                        if answered && !settled {
                            return Err(Broke::Oracle(format!("a request with no edit after an answer ended {outcome:?}")));
                        }
                        answered = true;
                        outcomes.push(outcome);
                    }
                    None => {
                        answered = false;
                        memo = make();
                    }
                },
                Op::Restart => memo = make(),
                edit => {
                    sim.apply(edit);
                    answered = false;
                }
            }
        }
        Ok((outcomes, sim))
    })();
    PLANT.set(Plant::None);
    result
}

fn op(tape: &Tape) -> Op {
    let c = tape.int_in(0..=2u8, "unit") as usize;
    let i = tape.int_in(0..=3u8, "item") as usize;
    let v = tape.int_in(0..=3u8, "value");
    match tape.int_in(0..=8u8, "op") {
        0 | 1 => Op::Body(c, i, v),
        2 | 3 => Op::Sig(c, i, v),
        4 => Op::Env(i % 2, (v > 0).then_some(v)),
        5 => Op::Restart,
        6 => Op::Request { kill: Some(u32::from(tape.int_in(0..=120u8, "kill at")) + 1) },
        _ => REQUEST,
    }
}

/// Up to 30 ops, each its own span.
fn ops(tape: &Tape) -> Vec<Op> {
    tape.vec(0..=30, "ops", op)
}

const CAP: u64 = 1 << 20;

/// The invariant: under `RecordsOnly`, for any edits, restarts and kills in any order of the
/// host's answers, every unit's manifest a finished request names holds what a from-scratch
/// build by value makes, the store holds all it names, no check follows a proof, at most
/// `max(workers, 1)` tasks run, and a request after an answer with no edit is certified.
#[test]
fn the_driver_builds_what_a_fresh_build_builds() {
    blaze_sim::check(Config::cases(200), |tape| {
        let ops = ops(tape);
        run(&ops, tape, Plant::None, true, || RecordsOnly).map(|_| ())
    });
}

/// The invariant: the same under `Resident`, a memo kept across requests of one process.
#[test]
fn a_resident_driver_builds_what_a_fresh_build_builds() {
    blaze_sim::check(Config::cases(200), |tape| {
        let ops = ops(tape);
        run(&ops, tape, Plant::None, true, || Resident::new(CAP)).map(|_| ())
    });
}

/// The invariant: `Audit`, which redoes every hit and compares bytes, never faults without a
/// plant: every record found at an address is the one its work makes again.
#[test]
fn audit_never_faults_without_a_plant() {
    blaze_sim::check(Config::cases(200), |tape| {
        let ops = ops(tape);
        run(&ops, tape, Plant::None, true, || Audit).map(|_| ())
    });
}

/// Both memos run `ops` on the same schedule: each on its own tape of `seed`.
fn refines(ops: &[Op], seed: u64, plant: Plant) -> Result<(), Broke> {
    let (plain, plain_sim) = run(ops, &Tape::seeded(seed), plant, false, || RecordsOnly)?;
    let (resident, resident_sim) = run(ops, &Tape::seeded(seed), plant, false, || Resident::new(CAP))?;
    if plain != resident {
        return Err(Broke::Oracle(format!("outcomes differ: records-only {plain:?}, resident {resident:?}")));
    }
    let bytes = |sim: &Sim| sim.disk.iter().map(|(key, bytes)| (*key, bytes.to_vec())).collect::<Vec<_>>();
    if bytes(&plain_sim) != bytes(&resident_sim) {
        return Err(Broke::Oracle("the stores differ between records-only and resident".into()));
    }
    Ok(())
}

/// The refinement: `Resident` is observationally equal to `RecordsOnly`: the same outcomes (what
/// ran and the check counters included) and the same records written, for any edits and
/// restarts.
#[test]
fn resident_refines_records_only() {
    blaze_sim::check(Config::cases(200), |tape| {
        let ops = ops(tape);
        let seed = tape.next_u64("schedule seed");
        refines(&ops, seed, Plant::None)
    });
}

/// What a finding in one of a control's words is marked with, so one string names them all.
const SAID: &str = "in the control's words: ";

/// `ops` pass in every case as the code is, and with `plant` some case is refused by the oracle
/// saying one of `says`: [`blaze_sim::control`] over 16 schedules, as many as the seeds the
/// loop it replaced ran.
#[track_caller]
fn caught(ops: &[Op], plant: Plant, says: &[&str], check: impl Fn(&[Op], &Tape, Plant) -> Result<(), Broke>) {
    blaze_sim::control(Config::cases(16), Plant::None, plant, SAID, |tape, plant| match check(ops, tape, plant) {
        Err(Broke::Oracle(why)) if says.iter().any(|says| why.contains(says)) => Err(Broke::Oracle(format!("{SAID}{why}"))),
        ran => ran,
    });
}

fn plain(ops: &[Op], tape: &Tape, plant: Plant) -> Result<(), Broke> {
    run(ops, tape, plant, true, || RecordsOnly).map(|_| ())
}

fn audited(ops: &[Op], tape: &Tape, plant: Plant) -> Result<(), Broke> {
    run(ops, tape, plant, true, || Audit).map(|_| ())
}

fn resident(ops: &[Op], tape: &Tape, plant: Plant) -> Result<(), Broke> {
    run(ops, tape, plant, true, || Resident::new(CAP)).map(|_| ())
}

/// Every edit of every item, each followed by a request: what most controls run.
fn every_edit() -> Vec<Op> {
    let mut ops = vec![REQUEST];
    for c in 0..UNITS {
        for i in 0..ITEMS {
            for v in [1, 2, 0] {
                ops.extend([Op::Sig(c, i, v), REQUEST, Op::Body(c, i, v), REQUEST]);
            }
        }
    }
    ops.extend([Op::Env(0, Some(1)), REQUEST, Op::Env(1, Some(2)), REQUEST, Op::Env(0, None), REQUEST]);
    ops
}

/// A stale record settled: the from-scratch comparison refuses it.
const STALE: &[&str] = &["not what a fresh build files"];
/// A stale record settled, or one the driver's own checks refuse first (a record whose head is
/// not the address its key and reads make).
const STALE_OR_REFUSED: &[&str] = &["not what a fresh build files", "the request ended"];

#[test]
fn the_oracle_catches_a_manifest_key_that_leaves_out_late_inputs() {
    let ops = [REQUEST, Op::Env(0, Some(1)), REQUEST, Op::Env(1, Some(1)), REQUEST];
    caught(&ops, Plant::KeyLeavesOutLateInputs, STALE, plain);
}

#[test]
fn the_oracle_catches_a_dropped_read() {
    caught(&every_edit(), Plant::ReadDropped, STALE, plain);
}

#[test]
fn the_oracle_catches_a_dropped_absence() {
    caught(&every_edit(), Plant::AbsenceDropped, STALE, plain);
}

#[test]
fn the_oracle_catches_a_shared_cache_hit_that_does_not_replay_its_reads() {
    caught(&every_edit(), Plant::CacheHitNoReplay, STALE_OR_REFUSED, plain);
}

#[test]
fn the_oracle_catches_a_made_fact_not_asked() {
    caught(&every_edit(), Plant::MadeNotAsked, STALE, plain);
}

#[test]
fn the_oracle_catches_a_dictionary_that_misses_a_fact() {
    caught(&every_edit(), Plant::DictionaryMissesFact, STALE_OR_REFUSED, plain);
}

#[test]
fn the_oracle_catches_a_maker_settled_after_its_reader() {
    caught(&every_edit(), Plant::MakerAfterReader, STALE, plain);
}

#[test]
fn the_oracle_catches_an_overcommitted_pool() {
    caught(&[REQUEST], Plant::OverCommitsPool, &["more tasks than workers"], plain);
}

#[test]
fn the_audit_catches_two_values_under_one_address() {
    let ops = [REQUEST, Op::Body(0, 0, 1), REQUEST, Op::Body(0, 0, 0), REQUEST];
    caught(&ops, Plant::TwoValuesUnderAddress, &["Audit {"], audited);
}

#[test]
fn the_oracle_catches_a_manifest_filed_before_its_records() {
    let mut ops = vec![];
    for kill in (4..80).step_by(3) {
        ops.push(Op::Request { kill: Some(kill) });
    }
    ops.extend([REQUEST, Op::Body(0, 3, 1), REQUEST]);
    caught(&ops, Plant::ManifestBeforeRecords, &["the store does not hold"], plain);
}

#[test]
fn the_oracle_catches_a_stale_resident_entry() {
    // The stale mark is refused by the driver's key check (its head is another key), or, had
    // it the key, by the from-scratch comparison.
    let ops = [REQUEST, Op::Body(0, 0, 1), REQUEST, Op::Sig(1, 0, 1), REQUEST, Op::Env(0, Some(1)), REQUEST];
    caught(&ops, Plant::StaleResident, STALE_OR_REFUSED, resident);
}

#[test]
fn the_oracle_catches_a_lookup_under_a_held_mark() {
    // Units 0 and 1, below the edit, settle by their marks.
    let ops = [REQUEST, Op::Sig(2, 0, 1), REQUEST];
    caught(&ops, Plant::LooksUpUnderMark, &["a check after a proof"], plain);
}

/// Runs `ops` under `RecordsOnly` with no kill, and the last request alone: what it ran, and
/// the (surfaces, bodies) whose fresh records the store did not hold before it.
fn last_request(ops: &[Op], before_last: impl FnOnce(&mut Sim)) -> (Outcome, (u32, u32)) {
    let (init, last) = ops.split_at(ops.len() - 1);
    assert_eq!(last, [REQUEST]);
    let (_, mut sim) = run(init, &Tape::seeded(7), Plant::None, false, || RecordsOnly).unwrap();
    before_last(&mut sim);
    let missing = sim.missing().unwrap();
    let mut memo = RecordsOnly;
    let outcome = sim.request(&mut memo, &Tape::seeded(11), None).unwrap().unwrap();
    (outcome, missing)
}

fn built(outcome: &Outcome) -> Ran {
    match outcome {
        Outcome::Built { ran, filed: Filed::Certificate } => *ran,
        other => panic!("not built: {other:?}"),
    }
}

/// A no-op is one certificate lookup.
#[test]
fn a_no_op_runs_no_task() {
    let (outcome, _) = last_request(&[REQUEST, REQUEST], |_| {});
    assert!(matches!(outcome, Outcome::Certified { .. }), "{outcome:?}");
}

/// A dependency fact nobody read moves its unit's universe: the dependents miss their marks,
/// answer their dictionaries and file a mark each, nothing else.
#[test]
fn an_unrelated_edit_files_only_marks() {
    // Unit 0's fourth signature: read by unit 0's own bodies only; its const stays equal.
    let (outcome, missing) = last_request(&[REQUEST, Op::Sig(0, 3, 3), REQUEST], |_| {});
    let ran = built(&outcome);
    assert_eq!((ran.surfaces, ran.bodies), missing, "{ran:?}");
    assert_eq!((ran.stood, ran.manifests, ran.marked), (2, 1, 0), "{ran:?}");
}

/// A signature edit runs exactly the records whose address moved: the readers the postings
/// name, whose answers changed.
#[test]
fn a_signature_edit_redoes_exactly_its_readers() {
    for (c, i) in [(0, 1), (0, 0), (1, 2), (2, 3)] {
        let (outcome, missing) = last_request(&[REQUEST, Op::Sig(c, i, 1), REQUEST], |_| {});
        let ran = built(&outcome);
        assert_eq!((ran.surfaces, ran.bodies), missing, "unit {c} item {i}: {ran:?}");
        assert!(ran.bodies > 0, "unit {c} item {i}: {ran:?}");
    }
}

/// A body edit runs that body, and a body that reads the const it makes when the const moved.
#[test]
fn a_body_edit_runs_its_body() {
    let (outcome, missing) = last_request(&[REQUEST, Op::Body(1, 1, 1), REQUEST], |_| {});
    let ran = built(&outcome);
    assert_eq!((ran.surfaces, ran.bodies), (0, 1), "{ran:?}");
    assert_eq!(missing, (0, 1));
    // Unit 0, below the edit, and unit 2, whose view is unchanged (unit 1's surface and made
    // table are equal), settle by their marks.
    assert_eq!(ran.marked, 2, "{ran:?}");
}

/// An undone edit finds its old certificate at its old address.
#[test]
fn an_undo_files_nothing() {
    let (outcome, _) = last_request(&[REQUEST, Op::Body(0, 0, 1), REQUEST, Op::Body(0, 0, 0), REQUEST], |_| {});
    assert!(matches!(outcome, Outcome::Certified { .. }), "{outcome:?}");
}

/// Another machine's store, with no certificate: every unit is settled by its mark.
#[test]
fn a_store_with_no_certificate_settles_through_marks() {
    let (outcome, _) = last_request(&[REQUEST, REQUEST], |sim| sim.disk.retain(|(tag, _), _| *tag != Certificate::TAG.0));
    let ran = built(&outcome);
    assert_eq!((ran.marked, ran.surfaces, ran.bodies, ran.manifests), (3, 0, 0, 0), "{ran:?}");
}

/// An undone edit with no certificate and no mark of the old tree: every record is found at
/// its old address, nothing runs.
#[test]
fn an_undo_finds_its_records_by_address() {
    let ops = [REQUEST, Op::Body(0, 2, 1), Op::Sig(1, 1, 2), REQUEST, Op::Body(0, 2, 0), Op::Sig(1, 1, 0), REQUEST];
    let (outcome, missing) = last_request(&ops, |sim| {
        sim.disk.retain(|(tag, _), _| *tag != Certificate::TAG.0 && *tag != crate::kind::Mark::TAG.0)
    });
    let ran = built(&outcome);
    assert_eq!(missing, (0, 0));
    assert_eq!((ran.surfaces, ran.bodies), (0, 0), "{ran:?}");
    assert!(ran.found > 0, "{ran:?}");
}

/// Every case the oracle is meant to see is reached by the seeded runs below; a case no run
/// reaches fails by name.
#[test]
fn every_case_is_reached() {
    let mut cases: BTreeMap<&'static str, u32> = BTreeMap::new();
    for name in [
        "certified",
        "built",
        "killed",
        "marked",
        "stood",
        "found by address",
        "walked past a root",
        "made redone",
        "chunk reused",
        "manifest rebuilt",
        "late read moved",
    ] {
        cases.insert(name, 0);
    }
    // The plans of every run, from one tape; each run's schedule from a tape of its own seed.
    let plans = Tape::seeded(3);
    for seed in 0..48 {
        let mut ops = vec![REQUEST];
        for _ in 0..24 {
            let (c, i, v) = (plans.index(UNITS, "unit"), plans.index(ITEMS, "item"), plans.below(4, "value") as u8);
            ops.push(match plans.below(10, "op") {
                0..=3 => Op::Body(c, i, v),
                4..=6 => Op::Sig(c, i, v),
                7 => Op::Env(i % 2, (v > 0).then_some(v)),
                8 => Op::Request { kill: Some(plans.below(60, "kill at") as u32 + 1) },
                _ => REQUEST,
            });
            if matches!(ops.last(), Some(Op::Env(..))) {
                *cases.get_mut("late read moved").unwrap() += 1;
            }
            ops.push(REQUEST);
        }
        let (outcomes, _) =
            run(&ops, &Tape::seeded(seed), Plant::None, true, || RecordsOnly).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        let requests = ops.iter().filter(|op| matches!(op, Op::Request { .. })).count();
        *cases.get_mut("killed").unwrap() += (requests - outcomes.len()) as u32;
        for outcome in outcomes {
            let ran = match outcome {
                Outcome::Certified { .. } => {
                    *cases.get_mut("certified").unwrap() += 1;
                    continue;
                }
                Outcome::Built { ran, .. } => ran,
                other => panic!("seed {seed}: {other:?}"),
            };
            for (name, count) in [
                ("built", 1),
                ("marked", ran.marked),
                ("stood", ran.stood),
                ("found by address", ran.found),
                ("walked past a root", ran.walked),
                ("made redone", ran.redone),
                ("chunk reused", (ran.manifests * 2).saturating_sub(ran.chunks)),
                ("manifest rebuilt", ran.manifests),
            ] {
                *cases.get_mut(name).unwrap() += count;
            }
        }
    }
    let unreached: Vec<&str> = cases.iter().filter(|(_, count)| **count == 0).map(|(name, _)| *name).collect();
    assert!(unreached.is_empty(), "cases no run reached: {unreached:?} (counts {cases:?})");
}

#[test]
fn an_outside_value_moves_with_its_answers() {
    proof::scope(|snap| {
        let key = certificate();
        let mut checks = Checks::default();
        let mut lookup = outside::Lookup::new(snap, key);
        assert!(matches!(lookup.want(), outside::Want::ReadShape(at) if at == outside::shape_slot(key, 0)));
        assert_eq!(lookup.shape(None, &mut checks), Ok(None));
        assert!(matches!(lookup.found(), Some(outside::Found::Miss)));
        let then = [ContentId([7; 32])];
        let settled = outside::settle(key, SHAPE, &then, &then, &mut checks).unwrap();
        let filer = lookup.filer(settled, SHAPE).unwrap();
        assert!(matches!(filer.want(), outside::Want::PutShape(at) if at == outside::shape_slot(key, 0)));
        let moved = outside::address(key, SHAPE, &[ContentId([8; 32])]);
        assert_ne!(moved, settled.address);
        let raced = outside::settle(key, SHAPE, &then, &[ContentId([8; 32])], &mut checks);
        assert!(matches!(raced, Err(outside::Raced { at: 0 })));
    });
}

/// The invariant: a record's address is its key and the answers to its shape: equal answers
/// give the address back (an undo), a moved answer or an added read gives another.
#[test]
fn an_address_is_its_key_and_its_answers() {
    let key = Key::<Body>::from_id(ContentId([3; 32]));
    let read = |digest: u8| Fact { id: sig_fact(0, 0), digest: ContentId([digest; 32]) };
    let a = record::address(key, &[read(1)]);
    assert_eq!(a, record::address(key, &[read(1)]));
    assert_ne!(a, record::address(key, &[read(2)]));
    assert_ne!(a, record::address(key, &[read(1), Fact { id: sig_fact(0, 1), digest: ContentId([1; 32]) }]));
    let (at, bytes) = record::write_read::<Body>(key, &[read(1)], &[], b"payload");
    assert_eq!(at, a);
    assert_eq!(record::check_read::<Body>(&Aligned::copy_from(&bytes), key).map(|(at, _)| at), Ok(a));
}

/// The invariant: every fact id routes to the one table that answers it, from its bytes.
#[test]
fn a_fact_routes_by_its_owner_and_kind() {
    assert_eq!(record::route(&sig_fact(1, 0)), Ok(Route::Surface(owner(1))));
    assert_eq!(record::route(&made_fact(2)), Ok(Route::Made(owner(2))));
    assert_eq!(record::route(&asker_fact()), Ok(Route::Asker));
    assert_eq!(record::route(&late_fact(0)), Ok(Route::Outside));
    let mut unrouted = sig_fact(0, 0);
    unrouted.0[10] = 0;
    assert!(record::route(&unrouted).is_err());
}

fn drawn_id(tape: &Tape) -> ContentId {
    let mut id = [0u8; 32];
    for chunk in id.chunks_mut(8) {
        chunk.copy_from_slice(&tape.int::<u64>("id").to_le_bytes());
    }
    ContentId(id)
}

/// The invariant: a segment reference [`crate::store::SegmentRef::new`] made decodes from its
/// row to itself; a row of another length, with its padding not zero, or naming an index off
/// [`crate::store::ALIGN`], is refused.
#[test]
fn a_segment_ref_round_trips() {
    use crate::store::{ALIGN, ENTRY, FOOTER, HEADER, SegmentId, SegmentRef, WireError};

    blaze_sim::check(Config::cases(200), |tape| {
        let digest = SegmentId(drawn_id(tape));
        let index_at = tape.int_in(HEADER as u64 / ALIGN..=(1u64 << 24), "index at") * ALIGN;
        let count = tape.int_in(0..=(1u32 << 20), "count");
        let len = index_at + u64::from(count) * ENTRY as u64 + FOOTER as u64;
        let made = SegmentRef::new(digest, len, index_at, count).unwrap_or_else(|why| panic!("{why}"));
        let row = made.encode();
        assert_eq!(SegmentRef::decode(&row), Ok(made));
        assert_eq!((made.digest(), made.size(), made.index_at(), made.count()), (digest, len, index_at, count));
        assert!(matches!(SegmentRef::decode(&row[..SegmentRef::ROW - 1]), Err(WireError::Length { .. })));
        let mut padded = row;
        padded[SegmentRef::ROW - 1] = 1;
        assert_eq!(SegmentRef::decode(&padded), Err(WireError::Padding { at: 52 }));
        assert!(matches!(SegmentRef::new(digest, len + 1, index_at + 1, count), Err(WireError::Index { .. })));
        assert!(matches!(SegmentRef::new(digest, len + 1, index_at, count), Err(WireError::Index { .. })));
        assert!(SegmentRef::new(digest, len, HEADER as u64 - ALIGN, count).is_err(), "an index inside the header");
    });
}

/// A segment length is taken at both ends of its range and refused one past each.
#[test]
fn a_segment_len_is_bounded_at_both_ends() {
    use crate::store::{SEGMENT_MOST, SegmentLen, WireError};

    assert_eq!(SegmentLen::new(SegmentLen::LEAST).map(SegmentLen::get), Ok(SegmentLen::LEAST));
    assert_eq!(SegmentLen::new(SEGMENT_MOST).map(SegmentLen::get), Ok(SEGMENT_MOST));
    assert_eq!(SegmentLen::new(SegmentLen::LEAST - 1), Err(WireError::SegmentLen { len: SegmentLen::LEAST - 1 }));
    assert_eq!(SegmentLen::new(SEGMENT_MOST + 1), Err(WireError::SegmentLen { len: SEGMENT_MOST + 1 }));
}

/// The invariant: a statement decodes from its 200 bytes to itself; bytes cut short or
/// extended, a tier byte that names no tier, or padding not zero, are refused.
#[test]
fn a_statement_round_trips() {
    use crate::kind::Tier;
    use crate::store::{SegmentId, SegmentLen, Statement, WireError};

    blaze_sim::check(Config::cases(200), |tape| {
        let len = tape.int_in(SegmentLen::LEAST..=SegmentLen::MOST, "len");
        let mut signature = [0u8; 64];
        signature[..32].copy_from_slice(&drawn_id(tape).0);
        signature[32..].copy_from_slice(&drawn_id(tape).0);
        let statement = Statement {
            namespace: Namespace(tape.int::<u64>("namespace")),
            segment: SegmentId(drawn_id(tape)),
            len: SegmentLen::new(len).unwrap_or_else(|why| panic!("{why}")),
            toolchain: drawn_id(tape),
            records: tape.int::<u32>("records"),
            tier: if tape.int_in(0..=1u8, "tier") == 1 { Tier::Fast } else { Tier::Top },
            issued: tape.int::<u64>("issued"),
            signer: drawn_id(tape).0,
            signature,
        };
        let bytes = statement.encode();
        assert_eq!(&bytes[..Statement::BODY], &statement.body()[..]);
        assert_eq!(Statement::decode(&bytes), Ok(statement));
        let cut = usize::from(tape.int_in(0..=Statement::LEN as u16 - 1, "cut"));
        assert_eq!(Statement::decode(&bytes[..cut]), Err(WireError::Length { len: cut, wanted: Statement::LEN }));
        let mut extended = Vec::with_capacity_in(Statement::LEN + 1, Global);
        extended.extend_from_slice(&bytes);
        extended.push(0);
        assert_eq!(Statement::decode(&extended), Err(WireError::Length { len: Statement::LEN + 1, wanted: Statement::LEN }));
        let mut tier = bytes;
        tier[92] = 2;
        assert_eq!(Statement::decode(&tier), Err(WireError::Tier { found: 2 }));
        let mut padded = bytes;
        padded[94] = 1;
        assert_eq!(Statement::decode(&padded), Err(WireError::Padding { at: 94 }));
    });
}

/// The invariant: on a machine that is no trusted writer every package is filed in the
/// machine's own namespace, whatever its origin: a fully local private build names no shared
/// namespace. On a trusted writer, registry and toolchain packages are public and the rest the
/// organisation's.
#[test]
fn an_untrusted_machine_files_everything_locally() {
    use crate::kind::{Origin, namespace_of};

    blaze_sim::check(Config::cases(200), |tape| {
        let origin = match tape.int_in(0..=2u8, "origin") {
            0 => Origin::Registry { checksum: drawn_id(tape) },
            1 => Origin::Toolchain,
            _ => Origin::Private,
        };
        let org = Namespace(tape.int::<u64>("org"));
        assert_eq!(namespace_of(origin, org, false), Namespace::LOCAL);
        let trusted = namespace_of(origin, org, true);
        match origin {
            Origin::Registry { .. } | Origin::Toolchain => assert_eq!(trusted, Namespace::PUBLIC),
            Origin::Private => assert_eq!(trusted, org),
        }
    });
}

/// `TAGS` names every kind once: no tag twice, no name twice, each kind at its own tag.
#[test]
fn tags_has_no_duplicate() {
    use crate::kind::TAGS;

    let tags: BTreeSet<u16> = TAGS.iter().map(|(tag, _)| tag.0).collect();
    let names: BTreeSet<&str> = TAGS.iter().map(|(_, name)| *name).collect();
    assert_eq!((tags.len(), names.len()), (TAGS.len(), TAGS.len()), "a tag or a name listed twice in `TAGS`");
    assert!(!tags.contains(&0), "no kind is at tag 0");
    let mut sorted: Vec<u16> = tags.into_iter().collect();
    sorted.dedup();
    assert!(sorted.windows(2).all(|pair| pair[1] == pair[0] + 1), "the tags are not contiguous: {sorted:?}");
}
