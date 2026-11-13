//! The driver under deterministic simulation (rule 43, 64): a model of three crates whose items
//! call each other by name, edited at random; every request runs the real driver, the real key
//! functions, the real text graph and the real record formats, with the host's answers delivered
//! in a seeded random order and the process killed at random points.
//!
//! The oracle:
//! - Keys: every key the model computes maps to one meaning for the whole run (two meanings
//!   under one key is a key that misses an input).
//! - Hits: a record the store answers is the meaning of its key; a manifest hit names only
//!   records the store holds.
//! - Reuse: a record filed under a moved key (its read list verified) is the meaning of the new
//!   key, which a fresh build computes from scratch.
//! - Work: no task runs for a record the store holds; at most `max(workers, 1)` tasks run.
//! - Ends: a built request filed the certificate and the fresh manifests; a request after it
//!   with no edit is certified; nothing fails or stalls.
//! - Refinement: `Resident` gives the same outcomes and leaves the same store as `RecordsOnly`.

use std::{alloc::Global, collections::BTreeMap};

use blaze_format::Aligned;
use blaze_ids::ContentId;
use blaze_rng::SplitMix64;
use hegel::generators as gs;

use crate::{
    PLANT, Plant,
    driver::{Config, Done, Driver, Effect, Input, Outcome, Plan, Ran, SURFACE_ITEM, Step, Task, UnitPlan},
    kind::{
        Body, BodyInputs, FileImage, FileInputs, Key, Kind, Late, LateRead, NameRead, Settings, Surface,
        SurfaceInputs, Tag, ToolId, UnitInputs, UnitManifest,
    },
    memo::{Memo, RecordsOnly, Resident},
    planted,
    pool::Ticket,
    record::{self, Entry, Fact, FactKind, StableId, Traced},
    store::Put,
    text::{CrateNodes, Graph, NameHash, Node, name_of},
};

const NAMES: [&str; 5] = ["new", "a", "b", "from", "run"];
const VARS: [&str; 2] = ["MODEL_A", "MODEL_B"];
const DEPS: [&[u32]; 3] = [&[], &[0], &[0, 1]];
const WORKERS: u32 = 2;

struct Spec {
    name: usize,
    calls: &'static [usize],
    env: Option<usize>,
    inline: bool,
}

const fn spec(name: usize, calls: &'static [usize], env: Option<usize>, inline: bool) -> Spec {
    Spec { name, calls, env, inline }
}

const SPECS: [[Spec; 3]; 3] = [
    [spec(0, &[], None, true), spec(1, &[0], None, false), spec(3, &[1], Some(0), false)],
    [spec(2, &[0], None, false), spec(4, &[2, 1], None, false), spec(0, &[], None, true)],
    [spec(4, &[2, 3], None, false), spec(1, &[4], None, false), spec(2, &[0], Some(1), false)],
];

const TOOL: ToolId = ToolId(ContentId([1; 32]));
const SETTINGS: Settings = Settings(ContentId([2; 32]));
const SYSROOT: ContentId = ContentId([3; 32]);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Item {
    present: bool,
    sig: u8,
    param: u8,
    body: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Body(usize, usize, u8),
    Sig(usize, usize, u8),
    Param(usize, usize, u8),
    Toggle(usize, usize),
    Env(usize, Option<u8>),
    Request { kill: Option<u32> },
    /// A new process: the resident memo starts empty.
    Restart,
}

const REQUEST: Op = Op::Request { kill: None };

fn digest(bytes: &[u8]) -> ContentId {
    record::digest(bytes)
}

fn aligned(bytes: Vec<u8>) -> Aligned {
    Aligned::copy_from(&bytes)
}

fn stable(c: usize, i: usize) -> StableId {
    StableId::of(format!("c{c}::{}", NAMES[SPECS[c][i].name]).as_bytes(), 1)
}

fn surface_stable(c: usize) -> StableId {
    StableId::of(format!("c{c}").as_bytes(), 2)
}

/// What a record of `tag` means, apart from its key and read list.
fn meaning(tag: Tag, bytes: &[u8]) -> Result<ContentId, String> {
    if tag == Body::TAG || tag == Surface::TAG {
        let root = blaze_format::open::<Traced>(bytes).map_err(|error| format!("{error}"))?;
        return record::result_digest(root, bytes).map_err(|error| format!("{error}"));
    }
    Ok(ContentId::of("blaze-keyed model meaning", bytes))
}

struct FreshUnit {
    manifest: Key<UnitManifest>,
    manifest_bytes: Vec<u8>,
    surface: Key<Surface>,
    surface_bytes: Vec<u8>,
    /// (item index, stable id, key, record).
    bodies: Vec<(usize, StableId, Key<Body>, Vec<u8>)>,
}

#[derive(Default)]
struct Disk {
    records: BTreeMap<(u16, [u8; 32]), Aligned>,
    cert: Option<ContentId>,
    last: Vec<Key<UnitManifest>>,
}

struct Model {
    items: [[Item; 3]; 3],
    env: [Option<u8>; 2],
    disk: Disk,
    meanings: BTreeMap<(u16, [u8; 32]), ContentId>,
    in_flight: u32,
}

enum Answer {
    Certified(Ticket, bool),
    Got(Ticket, Option<Aligned>),
    Verified(Ticket, bool),
    Keyed(Ticket, Key<UnitManifest>, Key<Surface>, Vec<(StableId, Key<Body>)>),
    Wrote(Ticket, Aligned),
    Put(Ticket, Put),
    Cert(Ticket),
}

impl Model {
    fn new() -> Model {
        let item = Item { present: true, sig: 0, param: 0, body: 0 };
        Model {
            items: [[item; 3]; 3],
            env: [None; 2],
            disk: Disk::default(),
            meanings: BTreeMap::new(),
            in_flight: 0,
        }
    }

    fn apply(&mut self, op: Op) {
        match op {
            Op::Body(c, i, v) => self.items[c][i].body = v,
            Op::Sig(c, i, v) => self.items[c][i].sig = v,
            Op::Param(c, i, v) => self.items[c][i].param = v,
            Op::Toggle(c, i) => self.items[c][i].present = !self.items[c][i].present,
            Op::Env(v, value) => self.env[v] = value,
            Op::Request { .. } | Op::Restart => {}
        }
    }

    fn world(&self) -> ContentId {
        let mut bytes = Vec::new();
        for c in 0..3 {
            bytes.extend(self.file_bytes(c));
        }
        bytes.extend(self.env.iter().map(|value| value.map_or(0, |v| v + 1)));
        digest(&bytes)
    }

    fn file_bytes(&self, c: usize) -> Vec<u8> {
        self.items[c].iter().flat_map(|item| [item.present as u8, item.sig, item.param, item.body]).collect()
    }

    fn resolve(&self, c: usize, name: usize) -> Option<(usize, usize)> {
        let find = |d: usize| (0..3).find(|&i| self.items[d][i].present && SPECS[d][i].name == name).map(|i| (d, i));
        find(c).or_else(|| DEPS[c].iter().find_map(|&d| find(d as usize)))
    }

    fn piece(&self, c: usize, i: usize) -> Vec<u8> {
        let (item, spec) = (self.items[c][i], &SPECS[c][i]);
        let mut piece = vec![spec.name as u8, item.sig, item.param, spec.inline as u8];
        if spec.inline {
            piece.push(item.body);
            piece.extend(spec.calls.iter().map(|&n| n as u8));
        }
        piece
    }

    fn env_value(&self, v: usize) -> Option<ContentId> {
        self.env[v].map(|value| digest(&[value]))
    }

    fn surface_own(&self, c: usize) -> ContentId {
        let mut bytes = Vec::new();
        for i in (0..3).filter(|&i| self.items[c][i].present) {
            bytes.extend(digest(&self.piece(c, i)).0);
        }
        digest(&bytes)
    }

    /// Every fact of unit `c` now: what a read list is checked against.
    fn facts_now(&self, c: usize) -> Vec<Fact> {
        let mut facts = vec![Fact {
            id: record::fact_id(FactKind::Own, &surface_stable(c), b""),
            digest: self.surface_own(c),
        }];
        for i in (0..3).filter(|&i| self.items[c][i].present) {
            facts.extend(self.body_reads(c, i));
        }
        facts
    }

    fn body_reads(&self, c: usize, i: usize) -> Vec<Fact> {
        let (item, spec, id) = (self.items[c][i], &SPECS[c][i], stable(c, i));
        let own = digest(&[c as u8, i as u8, item.sig, item.param, item.body, spec.inline as u8]);
        let mut reads = vec![Fact { id: record::fact_id(FactKind::Own, &id, b""), digest: own }];
        if let Some(v) = spec.env {
            let value = self.env[v].map_or(0, |value| value + 1);
            reads.push(Fact { id: record::fact_id(FactKind::Late, &id, VARS[v].as_bytes()), digest: digest(&[value]) });
        }
        for &n in spec.calls {
            let bound = match self.resolve(c, n) {
                Some((d, j)) => digest(&[1, d as u8, j as u8, self.items[d][j].sig]),
                None => digest(&[0]),
            };
            reads.push(Fact { id: record::fact_id(FactKind::Binding, &id, NAMES[n].as_bytes()), digest: bound });
        }
        reads
    }

    fn body_record(&self, c: usize, i: usize, key: &Key<Body>, drop_read: bool) -> Vec<u8> {
        let reads = self.body_reads(c, i);
        let mut value = Vec::new();
        reads.iter().for_each(|read| value.extend(read.digest.0));
        let payload = ContentId::of("blaze-keyed model body", &value).0;
        let kept = match drop_read && !SPECS[c][i].calls.is_empty() {
            true => &reads[..reads.len() - 1],
            false => &reads[..],
        };
        record::write_traced(key, kept, &[], &payload)
    }

    /// Every key and record a from-scratch build makes now.
    fn fresh(&self) -> Result<Vec<FreshUnit>, String> {
        let present = |c: usize| (0..3).filter(move |&i| self.items[c][i].present);
        let pieces: Vec<Vec<Vec<u8>>> = (0..3).map(|c| present(c).map(|i| self.piece(c, i)).collect()).collect();
        let names: Vec<Vec<Vec<NameHash>>> = (0..3)
            .map(|c| {
                present(c)
                    .map(|i| match SPECS[c][i].inline {
                        true => SPECS[c][i].calls.iter().map(|&n| name_of(NAMES[n].as_bytes())).collect(),
                        false => Vec::new(),
                    })
                    .collect()
            })
            .collect();
        let keys: Vec<Vec<[NameHash; 1]>> =
            (0..3).map(|c| present(c).map(|i| [name_of(NAMES[SPECS[c][i].name].as_bytes())]).collect()).collect();
        let nodes: Vec<Vec<Node<'_>>> = (0..3)
            .map(|c| {
                (0..pieces[c].len())
                    .map(|n| Node { piece: &pieces[c][n], names: &names[c][n], keys: &keys[c][n], always: false, coarse: false })
                    .collect()
            })
            .collect();
        let crates: Vec<CrateNodes<'_>> = (0..3)
            .map(|c| CrateNodes { identity: ContentId([10 + c as u8; 32]), nodes: &nodes[c], deps: DEPS[c] })
            .collect();
        let graph = Graph::build(&crates, Global).map_err(|error| error.to_string())?;
        let mut scratch = Vec::new_in(Global);
        let mut units: Vec<FreshUnit> = Vec::new();
        for c in 0..3 {
            let piece_ids: Vec<ContentId> = pieces[c].iter().map(|piece| digest(piece)).collect();
            let dep_surfaces: Vec<Key<Surface>> = DEPS[c].iter().map(|&d| units[d as usize].surface).collect();
            let surface = Surface::key(&SurfaceInputs {
                tool: TOOL,
                settings: SETTINGS,
                pieces: &piece_ids,
                deps: &dep_surfaces,
                late: &[],
            });
            let own = Fact { id: record::fact_id(FactKind::Own, &surface_stable(c), b""), digest: self.surface_own(c) };
            let exports: Vec<Fact> = present(c)
                .map(|i| Fact {
                    id: record::fact_id(FactKind::Signature, &stable(c, i), b""),
                    digest: digest(&[self.items[c][i].sig]),
                })
                .collect();
            let surface_bytes = record::write_traced(&surface, &[own], &exports, &[]);
            let always = graph.always(c as u32).map_err(|error| error.to_string())?;
            let mut bodies = Vec::new();
            let mut late_all = Vec::new();
            for i in present(c) {
                let spec = &SPECS[c][i];
                let mut calls: Vec<usize> = spec.calls.to_vec();
                calls.sort_by_key(|&n| name_of(NAMES[n].as_bytes()));
                calls.dedup();
                let mut reads = Vec::new();
                for n in calls {
                    let name = name_of(NAMES[n].as_bytes());
                    let closure = graph.name_closure(c as u32, name, &mut scratch).map_err(|error| error.to_string())?;
                    reads.push(NameRead { name, closure });
                }
                let late: Vec<LateRead<'_>> = spec
                    .env
                    .map(|v| LateRead { late: Late::Env, name: VARS[v].as_bytes(), value: self.env_value(v) })
                    .into_iter()
                    .collect();
                late_all.extend(late.iter().copied());
                let item = self.items[c][i];
                let text = digest(&[c as u8, i as u8, item.sig, item.param, item.body, spec.inline as u8]);
                let key = Body::key(&BodyInputs {
                    tool: TOOL,
                    settings: SETTINGS,
                    sysroot: SYSROOT,
                    text,
                    names: &reads,
                    always,
                    late: &late,
                    expansions: &[],
                });
                bodies.push((i, stable(c, i), key, self.body_record(c, i, &key, false)));
            }
            let bytes = self.file_bytes(c);
            let file = FileImage::key(&FileInputs { tool: TOOL, edition: 2024, bytes: &bytes });
            let dep_manifests: Vec<Key<UnitManifest>> = DEPS[c].iter().map(|&d| units[d as usize].manifest).collect();
            let manifest = UnitManifest::key(&UnitInputs {
                tool: TOOL,
                settings: SETTINGS,
                files: &[file],
                late: &late_all,
                deps: &dep_manifests,
            });
            let mut entries = vec![Entry { item: SURFACE_ITEM.0, key: surface.id(), tag: Surface::TAG.0, pad: 0 }];
            entries.extend(bodies.iter().map(|(_, item, key, _)| Entry { item: item.0, key: key.id(), tag: Body::TAG.0, pad: 0 }));
            let manifest_bytes = record::write_manifest(&manifest, &entries);
            units.push(FreshUnit { manifest, manifest_bytes, surface, surface_bytes, bodies });
        }
        Ok(units)
    }

    /// Files every key a fresh build makes under its meaning; a second meaning is a key that
    /// misses an input.
    fn register(&mut self, fresh: &[FreshUnit]) -> Result<(), String> {
        let mut file = |tag: Tag, key: ContentId, bytes: &[u8]| -> Result<(), String> {
            let means = meaning(tag, &aligned(bytes.to_vec()))?;
            match self.meanings.insert((tag.0, key.0), means) {
                Some(before) if before != means => {
                    Err(format!("two values under one key of kind {}: a key that misses an input", tag.0))
                }
                _ => Ok(()),
            }
        };
        for unit in fresh {
            file(UnitManifest::TAG, unit.manifest.id(), &unit.manifest_bytes)?;
            file(Surface::TAG, unit.surface.id(), &unit.surface_bytes)?;
            for (_, _, key, bytes) in &unit.bodies {
                file(Body::TAG, key.id(), bytes)?;
            }
        }
        Ok(())
    }

    fn check_meaning(&self, tag: Tag, key: ContentId, bytes: &[u8], what: &str) -> Result<(), String> {
        let means = meaning(tag, bytes)?;
        match self.meanings.get(&(tag.0, key.0)) {
            Some(fresh) if *fresh == means => Ok(()),
            Some(_) => Err(format!("{what} of kind {} is not what a fresh build files under its key", tag.0)),
            None => Err(format!("{what} of kind {} under a key no fresh build made", tag.0)),
        }
    }

    fn perform(&mut self, effect: &Effect<'_>, fresh: &[FreshUnit]) -> Result<Answer, String> {
        Ok(match *effect {
            Effect::Certify { ticket } => Answer::Certified(ticket, self.disk.cert == Some(self.world())),
            Effect::Get { ticket, tag, key, .. } => {
                let record = self.disk.records.get(&(tag.0, key.0)).cloned();
                if let Some(bytes) = &record {
                    self.check_meaning(tag, key, bytes, "a hit")?;
                    if tag == UnitManifest::TAG {
                        let root = record::open::<UnitManifest>(bytes, &Key::from_id(key)).map_err(|e| e.to_string())?;
                        for entry in root.entries.follow(bytes).map_err(|e| e.to_string())? {
                            if !self.disk.records.contains_key(&(entry.tag, entry.key.0)) {
                                return Err("a manifest hit names a record the store lacks".into());
                            }
                        }
                    }
                }
                Answer::Got(ticket, record)
            }
            Effect::Verify { ticket, unit, reads, .. } => {
                let now = self.facts_now(unit as usize);
                let equal = reads.iter().all(|read| now.iter().any(|fact| fact == read));
                Answer::Verified(ticket, equal)
            }
            Effect::Run { ticket, task } => {
                self.in_flight += 1;
                if self.in_flight > WORKERS.max(1) {
                    return Err(format!("{} tasks run at once on {WORKERS} workers", self.in_flight));
                }
                match task {
                    Task::Key { unit } => {
                        let unit_fresh = &fresh[unit as usize];
                        let bodies = unit_fresh.bodies.iter().map(|(_, item, key, _)| (*item, *key)).collect();
                        Answer::Keyed(ticket, unit_fresh.manifest, unit_fresh.surface, bodies)
                    }
                    Task::Surface { unit, key } => {
                        if self.disk.records.contains_key(&(Surface::TAG.0, key.id().0)) {
                            return Err("a surface task ran for a record the store holds".into());
                        }
                        Answer::Wrote(ticket, aligned(fresh[unit as usize].surface_bytes.clone()))
                    }
                    Task::Body { unit, item, key } => {
                        if self.disk.records.contains_key(&(Body::TAG.0, key.id().0)) {
                            return Err("a body task ran for a record the store holds".into());
                        }
                        let c = unit as usize;
                        let Some(&(i, ..)) = fresh[c].bodies.iter().find(|(_, id, ..)| *id == item) else {
                            return Err("a body task for an item the unit lacks".into());
                        };
                        Answer::Wrote(ticket, aligned(self.body_record(c, i, &key, planted(Plant::ReadDropped))))
                    }
                }
            }
            Effect::Put { ticket, tag, key, bytes } => {
                self.check_meaning(tag, key, bytes, "a put")?;
                let put = match self.disk.records.get(&(tag.0, key.0)) {
                    Some(held) if **held == *bytes => Put::Same,
                    Some(_) => Put::Other,
                    None => {
                        self.disk.records.insert((tag.0, key.0), Aligned::copy_from(bytes));
                        Put::Written
                    }
                };
                Answer::Put(ticket, put)
            }
            Effect::WriteCertificate { ticket, units } => {
                self.disk.cert = Some(self.world());
                self.disk.last = units.to_vec();
                Answer::Cert(ticket)
            }
        })
    }

    fn deliver<M: Memo>(&mut self, driver: &mut Driver<'_, M, Global>, answer: Answer) {
        match answer {
            Answer::Certified(ticket, hit) => driver.give(Input::Certified { ticket, hit }),
            Answer::Got(ticket, record) => driver.give(Input::Got { ticket, record: record.as_deref() }),
            Answer::Verified(ticket, equal) => driver.give(Input::Verified { ticket, equal }),
            Answer::Keyed(ticket, manifest, surface, bodies) => {
                self.in_flight -= 1;
                let outcome = Ok(Done::Keyed { manifest, surface, bodies: &bodies });
                driver.give(Input::Ran { ticket, outcome });
            }
            Answer::Wrote(ticket, record) => {
                self.in_flight -= 1;
                driver.give(Input::Ran { ticket, outcome: Ok(Done::Wrote { record: &record }) });
            }
            Answer::Put(ticket, put) => driver.give(Input::Put { ticket, put: Ok(put) }),
            Answer::Cert(ticket) => driver.give(Input::CertificateWritten { ticket, result: Ok(()) }),
        }
    }

    /// One request, to its outcome; `None` when it was killed.
    fn request<M: Memo>(&mut self, memo: &mut M, rng: &mut SplitMix64, kill: Option<u32>) -> Result<Option<Outcome>, String> {
        let fresh = self.fresh()?;
        self.register(&fresh)?;
        self.in_flight = 0;
        let plans: Vec<UnitPlan<'_>> =
            (0..3).map(|c| UnitPlan { deps: DEPS[c], last: self.disk.last.get(c).copied() }).collect();
        let mut driver = Driver::new(memo, Plan { units: &plans }, Config { workers: WORKERS }, Global)
            .map_err(|error| format!("{error:?}"))?;
        let mut queue: Vec<Answer> = Vec::new();
        let mut performed = 0u32;
        for _ in 0..20_000 {
            if !queue.is_empty() && rng.coin() {
                let answer = queue.swap_remove(rng.index(queue.len()));
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
                        return Ok(None);
                    }
                    Act::Asked(self.perform(&effect, &fresh)?)
                }
                Step::Wait(_) => Act::Wait,
            };
            match act {
                Act::Exit(outcome) => {
                    self.check_end(&outcome, &fresh)?;
                    return Ok(Some(outcome));
                }
                Act::Asked(answer) => queue.push(answer),
                Act::Wait if queue.is_empty() => return Err("the driver waits with nothing asked".into()),
                Act::Wait => {
                    let answer = queue.swap_remove(rng.index(queue.len()));
                    self.deliver(&mut driver, answer);
                }
            }
        }
        Err("no outcome in 20000 steps".into())
    }

    fn check_end(&self, outcome: &Outcome, fresh: &[FreshUnit]) -> Result<(), String> {
        match outcome {
            Outcome::Built { .. } | Outcome::Certified => {
                if self.disk.cert != Some(self.world()) {
                    return Err("an answer with no certificate of the tree".into());
                }
                for (c, unit) in fresh.iter().enumerate() {
                    if self.disk.last.get(c) != Some(&unit.manifest) {
                        return Err(format!("unit {c}'s filed manifest is not a fresh build's"));
                    }
                    if !self.disk.records.contains_key(&(UnitManifest::TAG.0, unit.manifest.id().0)) {
                        return Err(format!("unit {c}'s manifest is not in the store"));
                    }
                }
                Ok(())
            }
            other => Err(format!("the request ended {other:?}")),
        }
    }
}

/// Runs `ops` from an empty store, each process with a memo from `make`; the outcomes of the
/// requests that finished.
fn run<M: Memo>(ops: &[Op], seed: u64, plant: Plant, kills: bool, make: impl Fn() -> M) -> Result<(Vec<Outcome>, Model), String> {
    PLANT.set(plant);
    let result = (|| {
        let mut model = Model::new();
        let mut rng = SplitMix64::new(seed);
        let mut memo = make();
        let mut outcomes = Vec::new();
        let mut last_built = false;
        for &op in ops {
            match op {
                Op::Request { kill } => {
                    let kill = kill.filter(|_| kills);
                    match model.request(&mut memo, &mut rng, kill)? {
                        Some(outcome) => {
                            if last_built && outcome != Outcome::Certified {
                                return Err(format!("a request with no edit after an answer ended {outcome:?}"));
                            }
                            last_built = true;
                            outcomes.push(outcome);
                        }
                        None => {
                            last_built = false;
                            memo = make();
                        }
                    }
                }
                Op::Restart => memo = make(),
                edit => {
                    model.apply(edit);
                    last_built = false;
                }
            }
        }
        Ok((outcomes, model))
    })();
    PLANT.set(Plant::None);
    result
}

#[hegel::composite]
fn op(tc: &hegel::TestCase) -> Op {
    let c = tc.draw(gs::integers::<u8>().max_value(2)) as usize;
    let i = tc.draw(gs::integers::<u8>().max_value(2)) as usize;
    let v = tc.draw(gs::integers::<u8>().max_value(2));
    match tc.draw(gs::integers::<u8>().max_value(9)) {
        0 => Op::Body(c, i, v),
        1 => Op::Sig(c, i, v),
        2 => Op::Param(c, i, v),
        3 => Op::Toggle(c, i),
        4 => Op::Env(i % 2, (v > 0).then_some(v)),
        5 => Op::Restart,
        6 => Op::Request { kill: Some(u32::from(tc.draw(gs::integers::<u8>().max_value(60))) + 1) },
        _ => REQUEST,
    }
}

const CAP: u64 = 1 << 20;

/// The invariant: under `RecordsOnly`, for any edits, restarts and kills in any order of the
/// host's answers, every hit and every reuse is what a fresh build files, no task runs for a
/// record the store holds, at most `max(workers, 1)` run, and a request after an answer with no
/// edit is certified.
#[hegel::test(test_cases = 200)]
fn a_hit_is_only_what_a_fresh_build_files(tc: hegel::TestCase) {
    let ops: Vec<Op> = tc.draw(gs::vecs(op()).max_size(30));
    let seed = tc.draw(gs::integers::<u64>());
    if let Err(why) = run(&ops, seed, Plant::None, true, || RecordsOnly) {
        panic!("{why}");
    }
}

/// The invariant: the same under `Resident`, a memo kept across requests of one process.
#[hegel::test(test_cases = 200)]
fn a_resident_hit_is_only_what_a_fresh_build_files(tc: hegel::TestCase) {
    let ops: Vec<Op> = tc.draw(gs::vecs(op()).max_size(30));
    let seed = tc.draw(gs::integers::<u64>());
    if let Err(why) = run(&ops, seed, Plant::None, true, || Resident::new(CAP)) {
        panic!("{why}");
    }
}

fn refines(ops: &[Op], seed: u64, plant: Plant) -> Result<(), String> {
    let (plain, plain_model) = run(ops, seed, plant, false, || RecordsOnly)?;
    let (resident, resident_model) = run(ops, seed, plant, false, || Resident::new(CAP))?;
    if plain != resident {
        return Err(format!("outcomes differ: records-only {plain:?}, resident {resident:?}"));
    }
    let bytes = |model: &Model| model.disk.records.iter().map(|(key, bytes)| (*key, bytes.to_vec())).collect::<Vec<_>>();
    if bytes(&plain_model) != bytes(&resident_model) {
        return Err("the stores differ between records-only and resident".into());
    }
    Ok(())
}

/// The refinement: `Resident` is observationally equal to `RecordsOnly`: the same outcomes, the
/// same tasks run and the same records written, for any edits and restarts.
#[hegel::test(test_cases = 200)]
fn resident_refines_records_only(tc: hegel::TestCase) {
    let ops: Vec<Op> = tc.draw(gs::vecs(op()).max_size(30));
    let seed = tc.draw(gs::integers::<u64>());
    if let Err(why) = refines(&ops, seed, Plant::None) {
        panic!("{why}");
    }
}

/// `ops` pass as the code is, and with `plant` some seed's run is refused saying `says`.
fn caught(ops: &[Op], plant: Plant, says: &str, check: impl Fn(&[Op], u64, Plant) -> Result<(), String>) {
    let mut caught = false;
    for seed in 0..16 {
        check(ops, seed, Plant::None).unwrap();
        if let Err(why) = check(ops, seed, plant) {
            assert!(why.contains(says), "{plant:?}: {why}");
            caught = true;
        }
    }
    assert!(caught, "the oracle missed {plant:?}");
}

fn plain(ops: &[Op], seed: u64, plant: Plant) -> Result<(), String> {
    run(ops, seed, plant, true, || RecordsOnly).map(|_| ())
}

#[test]
fn the_oracle_catches_a_key_that_leaves_out_a_name() {
    let ops = [REQUEST, Op::Sig(1, 2, 1), REQUEST];
    caught(&ops, Plant::KeyLeavesOutNames, "a key that misses an input", plain);
}

#[test]
fn the_oracle_catches_a_manifest_key_that_leaves_out_late_inputs() {
    let ops = [REQUEST, Op::Env(0, Some(1)), REQUEST];
    caught(&ops, Plant::KeyLeavesOutLateInputs, "a key that misses an input", plain);
}

#[test]
fn the_oracle_catches_a_dropped_read() {
    let ops = [REQUEST, Op::Sig(1, 2, 1), REQUEST];
    caught(&ops, Plant::ReadDropped, "not what a fresh build files", plain);
}

#[test]
fn the_oracle_catches_an_overcommitted_pool() {
    caught(&[REQUEST], Plant::OverCommitsPool, "tasks run at once", plain);
}

#[test]
fn the_oracle_catches_a_manifest_filed_before_its_records() {
    let mut caught = false;
    for kill in 1..80 {
        let ops = [Op::Request { kill: Some(kill) }, REQUEST];
        for seed in 0..4 {
            plain(&ops, seed, Plant::None).unwrap();
            if let Err(why) = plain(&ops, seed, Plant::ManifestBeforeRecords) {
                assert!(why.contains("lacks"), "{why}");
                caught = true;
            }
        }
    }
    assert!(caught, "the oracle missed a manifest filed before its records");
}

#[test]
fn the_oracle_catches_a_stale_resident_entry() {
    let ops = [REQUEST, Op::Env(0, Some(1)), REQUEST];
    caught(&ops, Plant::StaleResident, "", refines);
}

fn ran_of(ops: &[Op]) -> Ran {
    let (outcomes, _) = run(ops, 7, Plant::None, false, || RecordsOnly).unwrap();
    match outcomes.last() {
        Some(Outcome::Built { ran }) => *ran,
        other => panic!("the last request ended {other:?}"),
    }
}

#[test]
fn a_no_op_runs_no_task() {
    let (outcomes, _) = run(&[REQUEST, REQUEST], 1, Plant::None, false, || RecordsOnly).unwrap();
    assert_eq!(outcomes.last(), Some(&Outcome::Certified));
    let (outcomes, _) = run(&[REQUEST, Op::Restart, REQUEST], 1, Plant::None, false, || RecordsOnly).unwrap();
    assert_eq!(outcomes.last(), Some(&Outcome::Certified));
}

/// A private body edit redoes that body alone: no surface, nothing reused, no other body.
#[test]
fn a_private_body_edit_redoes_one_body() {
    let ran = ran_of(&[REQUEST, Op::Body(2, 1, 5), REQUEST]);
    assert_eq!((ran.bodies, ran.surfaces, ran.reused), (1, 0, 0), "{ran:?}");
}

/// A parameter rename of an inline fn moves its callers' keys, and their read lists, which read
/// only its signature, file the old results under the new keys.
#[test]
fn a_parameter_rename_reuses_its_callers() {
    let ran = ran_of(&[REQUEST, Op::Param(0, 0, 3), REQUEST]);
    assert_eq!(ran.bodies, 1, "{ran:?}");
    assert!(ran.reused >= 3, "{ran:?}");
}

/// The invariant: a name that appears under a body's reach moves the body's key.
#[test]
fn an_added_name_moves_the_key() {
    let identity = ContentId([9; 32]);
    let a = [name_of(b"a")];
    let b = [name_of(b"b")];
    let user = Node { piece: b"fn user", names: &b, keys: &a, always: false, coarse: false };
    let other = Node { piece: b"fn b", names: &[], keys: &b, always: false, coarse: false };
    let alone = [user];
    let both = [user, other];
    let graph = |nodes: &[Node<'_>]| {
        Graph::build(&[CrateNodes { identity, nodes, deps: &[] }], Global).unwrap().closure(0, 0).unwrap()
    };
    assert_ne!(graph(&alone), graph(&both));
}

/// The invariant: the members of a cycle share one closure, and an edit to either moves both.
#[test]
fn a_cycle_is_one_component() {
    let identity = ContentId([9; 32]);
    let (a, b) = ([name_of(b"a")], [name_of(b"b")]);
    let closures = |text: &'static [u8]| {
        let nodes = [
            Node { piece: b"fn a", names: &b, keys: &a, always: false, coarse: false },
            Node { piece: text, names: &a, keys: &b, always: false, coarse: false },
        ];
        let graph = Graph::build(&[CrateNodes { identity, nodes: &nodes, deps: &[] }], Global).unwrap();
        (graph.closure(0, 0).unwrap(), graph.closure(0, 1).unwrap())
    };
    let (a1, b1) = closures(b"fn b");
    let (a2, _) = closures(b"fn b2");
    assert_eq!(a1, b1);
    assert_ne!(a1, a2);
}
