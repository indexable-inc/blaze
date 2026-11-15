//! The driver under deterministic simulation (rule 43, 64), on blaze-sim: four crates of four
//! items in two files each (unit 3 beside unit 1, both over unit 0), edited as the tape chooses; every request runs the real driver, the
//! real record formats, the real [`Universe::answer`] and the real outside machine, with the
//! host's answers delivered in an order the tape chooses (a [`Flight`]), puts landing when
//! delivered, and the process killed at points the tape chooses.
//!
//! The model's tasks read what they are handed and ask by value-dependent shapes: a surface asks
//! a dependency's second signature when its first one's digest is odd, a body asks a second
//! signature of its own unit when the first one's is; bodies read their own unit's surface, a
//! dependency's, an absent export, what the asking unit sees, a const another body of the unit
//! makes, a const a dependency's body makes and a late variable, which each unit sees with its
//! own value from its own late table. A unit's bodies may be not asked (`Bodies::NotAsked`):
//! then a fresh build runs none of them, and none may run.
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
//! - With programs (two, walked by a model walker through the driver's plan, code, span and
//!   link tasks): each kept program manifest the certificate names holds the rows a
//!   from-scratch build by value makes, the store holds every plan, code and span table it
//!   names, its dictionary and its span tables' reads are true now, and its linker state is the
//!   image of the fresh rows with each row's item at the line a fresh build prints (the section
//!   "programs" below). A comment added above an item moves its line and nothing else.
//! - Keep-going: a failed task fails only what depends on it, and the request's end carries
//!   every failure.
//!
//! Every plant has a control that the oracle refuses by name, and `every_case_is_reached`
//! fails by name for a case no run reaches.

use std::{
    alloc::Global,
    collections::{BTreeMap, BTreeSet, VecDeque},
    marker::PhantomData,
};

use blaze_format::Aligned;
use blaze_ids::ContentId;
use blaze_sim::{Broke, Config, Flight, Nanos, Tape};

use crate::{
    PLANT, Plant,
    driver::{
        Bodies, Done, Driver, Effect, Failure, Filed, Input, NO_UNIT, Outcome, Plan, Ran, Step, TableRef, Task, TaskFailed, UnitPlan,
        certificate_with, manifest_of,
    },
    exec::Local,
    front::{FileItems, Patch, PatchBase, PatchWrite, Table, Universe},
    kind::{
        Body, BodyInputs, Certificate, CertificateInputs, CodeRecord, CodeRecordInputs, FileImage, FileInputs, Key, Kind, Late,
        LateRead, Namespace, PlanRecord, PlanRecordInputs, ProgramManifest, Settings, SpanTable, SpanTableInputs, Steps, Surface,
        SurfaceInputs, Tag, ToolId, UnitInputs, UnitManifest,
    },
    memo::{Audit, Memo, RecordsOnly, Resident},
    outside, planted,
    pool::Ticket,
    program::{Kept, Planned, ProgramPlan, Row, SpanAt, WalkAsk, WalkEnd, Walker},
    proof::{self, Checks, Snapshot},
    record::{self, ChunkRoot, Fact, FactKind, KeptRow, KeptSpan, ManifestRoot, Owner, PlanEdge, Route, StableId, Traced},
    store::Put,
};

const UNITS: usize = 4;
const ITEMS: usize = 4;
/// Unit 3 depends on unit 0 alone, so it is keyed and examined beside unit 1, and its const
/// reads unit 0's as unit 1's does: two readers of one maker in units in flight at once.
const DEPS: [&[u32]; UNITS] = [&[], &[0], &[0, 1], &[0]];
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

/// Item `i` of unit `c` by its text id, as the key task gives it (blaze-unitkey's, made from
/// text alone): no fact subject.
fn stable(c: usize, i: usize) -> StableId {
    StableId::of(format!("c{c}::item{i}").as_bytes(), 1)
}

/// Item `i` of unit `c` as a fact subject (the front's item key): its first eight bytes its
/// unit's owner. Facts about the item are of it; its surface's maker row maps it to
/// [`stable`].
fn subject(c: usize, i: usize) -> StableId {
    let mut id = StableId::of(format!("c{c}::subject{i}").as_bytes(), 1);
    id.0.0[..8].copy_from_slice(&owner(c).0);
    id
}

fn sig_fact(c: usize, i: usize) -> ContentId {
    record::fact(FactKind::Signature, owner(c), 1, &subject(c, i), b"")
}

/// An export a unit has only while its first item's signature is 2: absent otherwise.
fn extra_fact(c: usize) -> ContentId {
    record::fact(FactKind::Signature, owner(c), 1, &StableId::of(format!("c{c}::extra").as_bytes(), 1), b"")
}

/// The const item 3 of unit `c` makes.
fn made_fact(c: usize) -> ContentId {
    record::fact(FactKind::ConstValue, owner(c), 2, &subject(c, 3), b"")
}

/// Item `i` of unit `c`'s layout: a surface row read by plans alone.
fn layout_fact(c: usize, i: usize) -> ContentId {
    record::fact(FactKind::Layout, owner(c), 4, &subject(c, i), b"")
}

/// What the asking unit sees as a whole: no surface has its row, so its seen row answers it.
fn asker_fact() -> ContentId {
    record::fact(FactKind::WholeScope, Owner::ASKER, 3, &StableId(ContentId([9; 32])), b"")
}

fn late_fact(var: usize) -> ContentId {
    record::late_id(Late::Env as u8, VARS[var])
}

/// The name a program prints for file `f` of unit `c`.
fn file_name(c: usize, f: usize) -> Vec<u8> {
    format!("c{c}/f{f}.rs").into_bytes()
}

/// The span file fact of file `f` of unit `c`: its unit's, by the name the program prints.
fn span_file(c: usize, f: usize) -> ContentId {
    record::span_file_id(owner(c), &record::printed_name(&file_name(c, f)))
}

/// The late reads of each unit: (the item that reads it, the variable). Units 0, 1 and 2 all
/// read variable 0 under its one id, and each sees its own value of it
/// ([`World::env_value`]), as each crate sees its own `CARGO_PKG_VERSION`.
const LATE: [&[(usize, usize)]; UNITS] = [&[(0, 0)], &[(0, 0)], &[(1, 0), (2, 1)], &[]];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Item {
    sig: u8,
    body: u8,
    /// The item's layout: a surface row only plans read (no body does), so an edit of it moves
    /// a plan's read and nothing a unit's records read.
    layout: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct World {
    items: [[Item; ITEMS]; UNITS],
    env: [Option<u8>; 2],
    /// Whether each unit's bodies are asked: the request's plan, not its tree.
    bodies: [Bodies; UNITS],
    /// Bodies whose task fails, and plans whose task fails: the keep-going cases.
    poison: [[bool; ITEMS]; UNITS],
    poison_plan: [[bool; ITEMS]; UNITS],
    /// Items the key task keys and the def map does not reach (text only an unused macro arm
    /// holds): their surface has no maker row of them, and no body task may be asked of them.
    unreached: [[bool; ITEMS]; UNITS],
    /// Per unit, per file, the comment lines above each of its two items: text no key but the
    /// file's covers, which moves the items' lines ([`World::line`]) and nothing they mean.
    comments: [[[u8; 2]; 2]; UNITS],
}

impl World {
    fn new() -> World {
        World {
            items: [[Item { sig: 0, body: 0, layout: 0 }; ITEMS]; UNITS],
            env: [None; 2],
            bodies: [Bodies::Asked; UNITS],
            poison: [[false; ITEMS]; UNITS],
            poison_plan: [[false; ITEMS]; UNITS],
            unreached: [[false; ITEMS]; UNITS],
            comments: [[[0; 2]; 2]; UNITS],
        }
    }

    /// Item `i` of unit `c`'s line in its file: the first item of a file below the comments
    /// above it, the second one line below the first and the comments above it.
    fn line(&self, c: usize, i: usize) -> u8 {
        let [above_first, above_second] = self.comments[c][i / 2];
        match i % 2 {
            0 => 1 + above_first,
            _ => 1 + above_first + 1 + above_second,
        }
    }

    /// Every item's line.
    fn lines(&self) -> [[u8; ITEMS]; UNITS] {
        let mut lines = [[0; ITEMS]; UNITS];
        for (c, unit) in lines.iter_mut().enumerate() {
            for (i, line) in unit.iter_mut().enumerate() {
                *line = self.line(c, i);
            }
        }
        lines
    }

    /// The fact a span table of unit `c` reads of its file `f`, with its digest now: the file's
    /// image key, as the driver answers it from the key task's rows.
    fn span_file(&self, c: usize, f: usize) -> Fact {
        Fact { id: span_file(c, f), digest: record::span_file_digest(&self.file_key(c, f)) }
    }

    /// The digest now of `id`, a span file fact of some unit's file; `None` for any other id.
    fn span_file_value(&self, id: &ContentId) -> Option<ContentId> {
        (0..UNITS).flat_map(|c| (0..2).map(move |f| (c, f))).find(|&(c, f)| span_file(c, f) == *id).map(|(c, f)| self.span_file(c, f).digest)
    }

    /// Item `i` of unit `c`'s span table: its file's printed name and its line.
    fn span_table(&self, c: usize, i: usize) -> Vec<u8> {
        let mut table = b"spans".to_vec();
        table.extend_from_slice(&file_name(c, i / 2));
        table.push(self.line(c, i));
        table
    }

    fn asked(&self, c: usize) -> bool {
        self.bodies[c] == Bodies::Asked
    }

    /// The plan's units.
    fn plans(&self, last: &[Option<Key<UnitManifest>>; UNITS]) -> Vec<UnitPlan<'static>> {
        (0..UNITS)
            .map(|c| UnitPlan { deps: DEPS[c], last: last[c], namespace: Namespace::LOCAL, owner: owner(c), bodies: self.bodies[c] })
            .collect()
    }

    fn digest(&self) -> ContentId {
        let mut bytes = Vec::new();
        for unit in &self.items {
            for item in unit {
                bytes.extend_from_slice(&[item.sig, item.body, item.layout]);
            }
        }
        for var in self.env {
            bytes.extend_from_slice(&[var.is_some() as u8, var.unwrap_or(0)]);
        }
        bytes.extend(self.unreached.iter().flatten().map(|&unreached| unreached as u8));
        bytes.extend(self.comments.iter().flatten().flatten());
        digest(&bytes)
    }

    fn sig_digest(&self, c: usize, i: usize) -> ContentId {
        digest(&[b's', self.items[c][i].sig])
    }

    fn layout_digest(&self, c: usize, i: usize) -> ContentId {
        digest(&[b'l', self.items[c][i].layout])
    }

    fn made_value(&self, c: usize) -> ContentId {
        let item = self.items[c][3];
        digest(&[b'm', (item.sig + item.body) % 3])
    }

    /// Variable `var` as unit `c` sees it: each unit its own value.
    fn env_value(&self, c: usize, var: usize) -> Option<ContentId> {
        self.env[var].map(|v| digest(&[b'e', c as u8, v]))
    }

    fn late_answer(&self, c: usize, var: usize) -> ContentId {
        self.env_value(c, var).unwrap_or_else(|| digest(b"unset"))
    }

    /// Unit `c`'s surface rows, sorted by id: each item's signature and the maker row of each
    /// item the def map reaches (every item has a body; the row's digest is the item's text id).
    fn rows(&self, c: usize) -> Vec<Fact> {
        let mut rows: Vec<Fact> = (0..ITEMS).map(|i| Fact { id: sig_fact(c, i), digest: self.sig_digest(c, i) }).collect();
        rows.extend(
            (0..ITEMS)
                .filter(|&i| !self.unreached[c][i])
                .map(|i| Fact { id: record::maker_row(&subject(c, i)), digest: stable(c, i).0 }),
        );
        rows.extend((0..ITEMS).map(|i| Fact { id: layout_fact(c, i), digest: self.layout_digest(c, i) }));
        if self.items[c][0].sig == 2 {
            rows.push(Fact { id: extra_fact(c), digest: digest(b"extra") });
        }
        let mut seen = vec![c as u8];
        seen.extend(self.items[c].iter().map(|item| item.sig));
        rows.push(Fact { id: record::seen_id(), digest: digest(&seen) });
        rows.sort_by_key(|fact| fact.id);
        rows
    }

    /// What unit `c`'s bodies make, sorted by id, asked or not: a reader of a unit not asked is
    /// answered its maker's value, by demand.
    fn made(&self, c: usize) -> Vec<Fact> {
        vec![Fact { id: made_fact(c), digest: self.made_value(c) }]
    }

    /// The made table unit `c`'s manifest files: nothing when its bodies are not asked (a
    /// demanded maker's facts are the request's, never its unit's manifest's).
    fn filed_made(&self, c: usize) -> Vec<Fact> {
        match self.asked(c) {
            true => self.made(c),
            false => Vec::new(),
        }
    }

    /// The answer now of `id` asked by unit `asker`'s surface (`body` false: its dependencies
    /// only) or a body: by value, from the model.
    fn value(&self, asker: usize, body: bool, id: &ContentId) -> Result<ContentId, String> {
        let visible = |d: usize| DEPS[asker].contains(&(d as u32)) || (body && d == asker);
        let route = record::route(id).map_err(|unrouted| format!("an unrouted ask {unrouted:?}"))?;
        Ok(match route {
            Route::Outside => match LATE[asker].iter().find(|&&(_, var)| late_fact(var) == *id) {
                Some(&(_, var)) => self.late_answer(asker, var),
                None => return Err("an outside ask of no variable its unit reads".into()),
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

    /// The steps a body of unit `c` runs, and its task's key: a unit asked checks its bodies; a
    /// unit not asked makes the ones demanded of it for their MIR alone.
    fn body_steps(&self, c: usize, i: usize) -> (Steps, Key<Body>) {
        let steps = if self.asked(c) { Steps::Checked } else { Steps::Mir };
        (steps, steps.key(self.body_key(c, i), stable(c, i)))
    }

    fn surface_key(&self, c: usize) -> Key<Surface> {
        let pieces: Vec<ContentId> = (0..ITEMS).map(|i| digest(&[b'p', i as u8, self.items[c][i].sig, self.items[c][i].layout])).collect();
        Surface::key(&SurfaceInputs { tool: TOOL, settings: SETTINGS, unit: unit_id(c), pieces: &pieces })
    }

    fn file_key(&self, c: usize, f: usize) -> Key<FileImage> {
        let (a, b) = (self.items[c][2 * f], self.items[c][2 * f + 1]);
        let [above_a, above_b] = self.comments[c][f];
        let bytes = [c as u8, f as u8, above_a, a.sig, a.body, a.layout, above_b, b.sig, b.body, b.layout];
        FileImage::key(&FileInputs { tool: TOOL, edition: 2021, bytes: &bytes })
    }

    fn text_key(&self, c: usize) -> Key<UnitManifest> {
        let files = [self.file_key(c, 0), self.file_key(c, 1)];
        let late: Vec<LateRead<'_>> =
            LATE[c].iter().map(|&(_, var)| LateRead { late: Late::Env, name: VARS[var], value: self.env_value(c, var) }).collect();
        UnitManifest::key(&UnitInputs { tool: TOOL, settings: SETTINGS, unit: unit_id(c), files: &files, late: &late })
    }

    /// The manifest key unit `c` is filed under: its text key, and whether its bodies are asked.
    fn manifest_key(&self, c: usize) -> Key<UnitManifest> {
        manifest_of(self.text_key(c), self.bodies[c])
    }

    /// Unit `c`'s files with their items, each file's in stable-id order, and each file's
    /// printed name.
    fn keyed(&self, c: usize) -> (Vec<FileItems>, Vec<(StableId, Key<Body>)>, Vec<ContentId>) {
        let mut files = Vec::new();
        let mut items = Vec::new();
        let mut names = Vec::new();
        for f in 0..2 {
            let mut of: Vec<(StableId, Key<Body>)> = (2 * f..2 * f + 2).map(|i| (stable(c, i), self.body_key(c, i))).collect();
            of.sort_by_key(|(item, _)| *item);
            files.push(FileItems { image: self.file_key(c, f), at: items.len() as u32, len: of.len() as u32 });
            items.extend(of);
            names.push(record::printed_name(&file_name(c, f)));
        }
        (files, items, names)
    }

    /// Unit `c`'s late table, as the host hands it: the late reads its key covers, with its own
    /// values, sorted by id, each once.
    fn late(&self, c: usize) -> Vec<Fact> {
        let mut late: Vec<Fact> = LATE[c].iter().map(|&(_, var)| Fact { id: late_fact(var), digest: self.late_answer(c, var) }).collect();
        late.sort_by_key(|fact| fact.id);
        late.dedup_by_key(|fact| fact.id);
        late
    }

    /// The answer now of `id` asked by a plan, by value: a crate's fact from its surface,
    /// whichever crate (a plan is of no unit and sees every crate of the request).
    fn plan_value(&self, id: &ContentId) -> Result<ContentId, String> {
        match record::route(id).map_err(|unrouted| format!("an unrouted ask {unrouted:?}"))? {
            Route::Surface(owner) => Ok(match unit_of(owner) {
                Some(d) => self.rows(d).iter().find(|row| row.id == *id).map_or_else(|| record::absent(id), |row| row.digest),
                None => record::GONE,
            }),
            _ => Err("a plan asked a fact no surface holds".into()),
        }
    }

    /// Item `i` of unit `c`'s body's result, by value: what a plan reads of it, asked or not.
    fn body_result(&self, c: usize, i: usize) -> Result<ContentId, String> {
        result_of(&body_task(self, c, i, self.body_key(c, i), false, 0, &mut |id: &ContentId| self.value(c, true, id))?)
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
/// Item `i` of unit `c`'s body record under `key`: the model's two steps make one record, so a
/// MIR-alone body and a checked one differ by their key alone.
fn body_task(world: &World, c: usize, i: usize, key: Key<Body>, plants: bool, earlier: u32, ask: &mut Ask<'_>) -> Result<Vec<u8>, String> {
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
    // Each unit's const reads the const of its last dependency: unit 2's reads unit 1's, whose
    // reads unit 0's, so with units 0 and 1 not asked one demand demands another; unit 3's
    // reads unit 0's too, beside unit 1's.
    if let Some(&below) = DEPS[c].last()
        && i == 3
    {
        asked.ask(ask, made_fact(below as usize))?;
    }
    for &(reader, var) in LATE[c] {
        if reader == i {
            asked.ask(ask, late_fact(var))?;
        }
    }
    let item = world.items[c][i];
    let mut facts = if i == 3 { world.made(c) } else { Vec::new() };
    if plants && planted(Plant::BodyMakesSurfaceRow) && i == 3 {
        // A row of its unit's surface, which no body may make.
        facts.push(Fact { id: sig_fact(c, 0), digest: digest(b"made by a body") });
        facts.sort_by_key(|fact| fact.id);
    }
    let payload = asked.payload(&[c as u8, i as u8, item.sig, item.body], earlier, plants);
    Ok(record::write_read::<Body>(key, &asked.reads(plants), &facts, &payload).1)
}

fn result_of(bytes: &[u8]) -> Result<ContentId, String> {
    let root = blaze_format::open::<Traced>(bytes).map_err(|error| format!("{error}"))?;
    record::result_digest(root, bytes).map_err(|error| format!("{error}"))
}

/// What a from-scratch build makes of one unit: by value.
struct Fresh {
    text: Key<UnitManifest>,
    surface: ContentId,
    /// By stable id: (key, result), the result `None` for a silent item (no record).
    bodies: BTreeMap<StableId, (Key<Body>, Option<ContentId>)>,
    made: Vec<Fact>,
}

fn fresh(world: &World) -> Result<Vec<Fresh>, String> {
    (0..UNITS)
        .map(|c| {
            let surface = surface_task(world, c, false, 0, &mut |id: &ContentId| world.value(c, false, id))?;
            let mut bodies = BTreeMap::new();
            // A unit whose bodies are not asked has none: a fresh build runs none of them. An
            // item the def map does not reach is silent: keyed, filed, never run.
            for i in (0..ITEMS).filter(|_| world.asked(c)) {
                if world.unreached[c][i] {
                    bodies.insert(stable(c, i), (world.body_key(c, i), None));
                    continue;
                }
                let bytes = body_task(world, c, i, world.body_key(c, i), false, 0, &mut |id: &ContentId| world.value(c, true, id))?;
                bodies.insert(stable(c, i), (world.body_key(c, i), Some(result_of(&bytes)?)));
            }
            Ok(Fresh { text: world.manifest_key(c), surface: result_of(&surface)?, bodies, made: world.filed_made(c) })
        })
        .collect()
}

// ---- programs ----
//
// Two programs, rooted at item 0 of units 2 and 3. A plan reads its body, its item's signature
// and layout (and the next item's layout when the signature's digest is odd); its edges go to
// the same item of its unit's last dependency, at the layout's parity, and to its unit's next
// item when its body's result is odd. So a body edit moves a plan's result, its code and what is
// reached; a layout edit moves a plan's read, and by its parity a callee's instance; a signature
// edit moves both. A plan is named by the digest of its ask (the item's subject and its type
// arguments), and its code is keyed by its description (the body's result and the layout's
// parity: the template identity), both by the model walker, as the front end's walker does.
// Code is opaque bytes of the description, given its callees' plans; the linker's state is every
// row's (instance, code). The oracle builds each program from scratch by value (a breadth-first
// walk over plans made from the model's values, never from a record) and holds the kept manifest
// a request files to it row by row, each row's code by its content, its dictionary to what is
// true now, and its linker state to the image of the rows. Each item a row is of has one span
// table (its file's printed name and its line), made by its owner's span task from its file's
// span file fact; the linker's state holds each such item's position, which a comment above it
// moves, and the oracle holds them to a fresh build's.

const PROGRAMS: usize = 2;
/// Each program's root: the item its walk starts at, with no type arguments.
const ROOTS: [(usize, usize); PROGRAMS] = [(2, 0), (3, 0)];
const PSETTINGS: ContentId = ContentId([6; 32]);

fn program_id(p: usize) -> ContentId {
    ContentId([0x40 + p as u8; 32])
}

fn program_plans(last: &[Option<Key<ProgramManifest>>; PROGRAMS]) -> Vec<ProgramPlan> {
    (0..PROGRAMS).map(|p| ProgramPlan { program: program_id(p), settings: PSETTINGS, namespace: Namespace::LOCAL, last: last[p] }).collect()
}

/// The model front end's plan ask of item `i` of unit `c` at `args`: the item's subject, then
/// the arguments' bytes.
fn plan_ask(c: usize, i: usize, args: &[u8]) -> Vec<u8> {
    let mut ask = subject(c, i).0.0.to_vec();
    ask.extend_from_slice(args);
    ask
}

/// The item and arguments of a [`plan_ask`].
fn ask_of_plan(ask: &[u8]) -> Option<((usize, usize), &[u8])> {
    let (item, args) = ask.split_first_chunk::<32>()?;
    Some((item_of(StableId(ContentId(*item)))?, args))
}

/// A plan's key is its name, the digest of its ask (as the front end names a plan).
fn plan_key(c: usize, i: usize, args: &[u8]) -> Key<PlanRecord> {
    PlanRecord::key(&PlanRecordInputs { name: record::digest(&plan_ask(c, i, args)) })
}

/// The walker's code key of `instance`, whose plan's description is `describe`: the model's
/// template identity, the whole description (the body's result and the layout's parity), which
/// is all the code is made of (a callee is a place, not a byte of the code). With
/// [`Plant::CodeKeyLeavesOutBody`] the template is the instance and the parity, leaving out the
/// body's result: code a body edit moves is found under the key it had.
fn code_key(instance: Key<PlanRecord>, describe: &[u8]) -> Key<CodeRecord> {
    let mut template = b"template".to_vec();
    match planted(Plant::CodeKeyLeavesOutBody) {
        true => {
            template.extend_from_slice(&instance.id().0);
            template.extend_from_slice(describe.get(32..).unwrap_or_default());
        }
        false => template.extend_from_slice(describe),
    }
    CodeRecord::key(&CodeRecordInputs { template: record::digest(&template) })
}

/// The item a fact subject names.
fn item_of(item: StableId) -> Option<(usize, usize)> {
    (0..UNITS).flat_map(|c| (0..ITEMS).map(move |i| (c, i))).find(|&(c, i)| subject(c, i) == item)
}

/// The plan of item `i` of unit `c` at `args`, whose body's result is `mir`, asking through
/// `ask`: its address and record. Its result covers the body and the layout's parity only, so
/// a moved signature, or a layout of the same parity, moves its address and not its code.
/// What a poisoned task fails with.
const POISONED: TaskFailed = TaskFailed::Refused { what: "a poisoned task" };

/// The body other than its own that item `i` of unit `c`'s plan reads: item 2's reads its unit's
/// const (item 3), as a const it evaluates, when the def map reaches it. The front end is not
/// given it at first, so it refuses with [`TaskFailed::NoBody`] and is run again given it.
fn also_reads(world: &World, c: usize, i: usize) -> Option<StableId> {
    (i == 2 && !world.unreached[c][3]).then(|| subject(c, 3))
}

/// Item `i` of unit `c`'s plan for `args`, its body's result `mir` and the result `also` of the
/// other body it reads ([`also_reads`]), asking through `ask`.
fn plan_task(c: usize, i: usize, args: &[u8], mir: ContentId, also: Option<ContentId>, ask: &mut Ask<'_>) -> Result<(ContentId, Vec<u8>), String> {
    let mut asked = Asked::default();
    let sig = asked.ask(ask, sig_fact(c, i))?;
    let layout = asked.ask(ask, layout_fact(c, i))?;
    if odd(sig) {
        asked.ask(ask, layout_fact(c, (i + 1) % ITEMS))?;
    }
    let mut reads = asked.reads(false);
    reads.push(Fact { id: record::mir_id(&subject(c, i)), digest: mir });
    if let Some(also) = also {
        reads.push(Fact { id: record::mir_id(&subject(c, 3)), digest: also });
    }
    reads.sort_by_key(|fact| fact.id);
    let parity = layout.0[0] & 1;
    let mut callees: Vec<(StableId, Vec<u8>)> = Vec::new();
    if let Some(&below) = DEPS[c].last() {
        callees.push((subject(below as usize, i), vec![parity]));
    }
    if odd(mir) && i + 1 < ITEMS {
        callees.push((subject(c, i + 1), args.to_vec()));
    }
    let (mut edges, mut bytes) = (Vec::new(), Vec::new());
    for (item, of) in &callees {
        edges.push(PlanEdge { item: item.0, args_at: bytes.len() as u32, args_len: of.len() as u32, kind: 0, pad: 0, pad2: 0 });
        bytes.extend_from_slice(of);
    }
    let mut describe = mir.0.to_vec();
    describe.extend(also.iter().flat_map(|also| also.0));
    describe.push(parity);
    let payload = record::write_plan_payload(&edges, &bytes, &describe, Global);
    let (address, record) = record::write_read::<PlanRecord>(plan_key(c, i, args), &reads, &[], &payload);
    Ok((address.id(), record))
}

/// The code of the plan record `plan` under `key`: opaque bytes of the plan's description.
fn code_task(key: Key<CodeRecord>, plan: &[u8]) -> Result<Vec<u8>, String> {
    let root = blaze_format::open::<Traced>(plan).map_err(|e| e.to_string())?;
    let payload = Aligned::copy_from(root.payload.follow(plan).map_err(|e| e.to_string())?);
    let view = record::read_plan_payload(&payload).map_err(|e| e.to_string())?;
    Ok(code_bytes(key, view.describe))
}

/// The code record under `key` of a plan described by `describe`.
fn code_bytes(key: Key<CodeRecord>, describe: &[u8]) -> Vec<u8> {
    let mut payload = b"code".to_vec();
    payload.extend_from_slice(describe);
    record::write_opaque(&key, &payload)
}

/// The instances the edges of the plan record `plan` name, sorted, each once.
fn edge_instances(plan: &[u8]) -> Result<Vec<ContentId>, String> {
    let root = blaze_format::open::<Traced>(plan).map_err(|e| e.to_string())?;
    let payload = Aligned::copy_from(root.payload.follow(plan).map_err(|e| e.to_string())?);
    let view = record::read_plan_payload(&payload).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for edge in view.edges {
        let args = view.args_of(edge).ok_or("an edge's arguments out of bounds")?;
        let (d, j) = item_of(StableId(edge.item)).ok_or("an edge to no item")?;
        out.push(plan_key(d, j, args).id());
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// The model linker's state: every row's (instance, code), sorted by instance.
fn image_bytes(rows: &BTreeMap<ContentId, ContentId>) -> Vec<u8> {
    rows.iter().flat_map(|(instance, code)| instance.0.into_iter().chain(code.0)).collect()
}

/// The model program's image of the linker state `state`: a header, then the state. Not the
/// state's own bytes, so a name by the image's content is not a name by the state's.
fn image_file(state: &[u8]) -> Vec<u8> {
    let mut file = b"image".to_vec();
    file.extend_from_slice(state);
    file
}

/// The model linker's patch from image `old` to image `new`: each 64-byte block that differs,
/// over a clone of the old image, or over an empty file when there is none.
fn patch_between(old: &[u8], new: &[u8]) -> Patch<Global> {
    let (mut writes, mut bytes) = (Vec::new_in(Global), Vec::new_in(Global));
    for at in (0..new.len()).step_by(64) {
        let end = (at + 64).min(new.len());
        if old.get(at..end) != Some(&new[at..end]) {
            writes.push(PatchWrite { at: at as u64, from: bytes.len() as u32, len: (end - at) as u32 });
            bytes.extend_from_slice(&new[at..end]);
        }
    }
    let base = match old.is_empty() {
        true => PatchBase::Empty,
        false => PatchBase::Clone,
    };
    Patch { base, len: new.len() as u64, writes, bytes }
}

/// `patch` applied to `file` as the host applies it: each write over its base, then cut to
/// length.
fn apply_patch(file: &mut Vec<u8>, patch: &Patch<Global>) {
    if patch.base == PatchBase::Empty {
        file.clear();
    }
    for write in &patch.writes {
        let (at, len) = (write.at as usize, write.len as usize);
        if file.len() < at + len {
            file.resize(at + len, 0);
        }
        file[at..at + len].copy_from_slice(&patch.bytes[write.from as usize..write.from as usize + len]);
    }
    file.resize(patch.len as usize, 0);
}

/// Item `i` of unit `c`'s one span table's anchor: of its subject, never its place.
fn anchor(c: usize, i: usize) -> ContentId {
    let mut bytes = b"anchor".to_vec();
    bytes.extend_from_slice(&subject(c, i).0.0);
    digest(&bytes)
}

/// The span table the code of an instance of item `i` of unit `c` names, as the walker gives it.
fn span_at(c: usize, i: usize) -> SpanAt {
    let anchor = anchor(c, i);
    SpanAt { anchor, key: SpanTable::key(&SpanTableInputs { tool: TOOL, anchor }), owner: subject(c, i) }
}

/// Owner item `i` of unit `c`'s span task: its reads (its file's span file fact) and its one
/// table.
fn span_task(world: &World, c: usize, i: usize) -> (Vec<Fact>, Vec<(ContentId, Vec<u8>)>) {
    (vec![world.span_file(c, i / 2)], vec![(anchor(c, i), world.span_table(c, i))])
}

/// The item instance `id` is of, over every argument the model's plans pass (none, or a
/// layout's parity).
fn instance_item(id: &ContentId) -> Option<(usize, usize)> {
    let args: [&[u8]; 3] = [&[], &[0], &[1]];
    (0..UNITS)
        .flat_map(|c| (0..ITEMS).map(move |i| (c, i)))
        .find(|&(c, i)| args.iter().any(|args| plan_key(c, i, args).id() == *id))
}

/// The 64 bytes between the model linker's rows and its positions.
const POSITIONS: [u8; 64] = [0xff; 64];

/// The model linker's state: its rows ([`image_bytes`]), then each item's position as (anchor,
/// the digest of its span table), sorted by anchor.
fn linker_bytes(rows: &BTreeMap<ContentId, ContentId>, positions: &BTreeMap<ContentId, ContentId>) -> Vec<u8> {
    let mut bytes = image_bytes(rows);
    bytes.extend_from_slice(&POSITIONS);
    bytes.extend_from_slice(&image_bytes(positions));
    bytes
}

/// The rows and positions of the model linker's state; an empty state has neither.
type Linker = (BTreeMap<ContentId, ContentId>, BTreeMap<ContentId, ContentId>);

fn linker_of(bytes: &[u8]) -> Result<Linker, String> {
    if bytes.is_empty() {
        return Ok((BTreeMap::new(), BTreeMap::new()));
    }
    let Some(at) = bytes.chunks(64).position(|chunk| chunk == &POSITIONS[..]) else {
        return Err("a linker state with no positions".into());
    };
    Ok((image_of(&bytes[..64 * at])?, image_of(&bytes[64 * at + 64..])?))
}

fn image_of(bytes: &[u8]) -> Result<BTreeMap<ContentId, ContentId>, String> {
    if bytes.len() % 64 != 0 {
        return Err(format!("a linker state of {} bytes, not whole rows", bytes.len()));
    }
    let id = |bytes: &[u8]| {
        let mut id = [0u8; 32];
        id.copy_from_slice(bytes);
        ContentId(id)
    };
    Ok(bytes.chunks(64).map(|pair| (id(&pair[..32]), id(&pair[32..]))).collect())
}

/// One row of a program a fresh build makes.
struct FreshRow {
    item: StableId,
    plan: ContentId,
    result: ContentId,
    /// The plan's description: what its code is made of, whatever key the walker filed it at.
    describe: Vec<u8>,
    targets: Vec<ContentId>,
}

/// Program `p` built from scratch by value: by instance.
fn fresh_program(world: &World, p: usize) -> Result<BTreeMap<ContentId, FreshRow>, String> {
    let mut rows = BTreeMap::new();
    let (c, i) = ROOTS[p];
    let mut queue = vec![(c, i, Vec::new())];
    while let Some((c, i, args)) = queue.pop() {
        let instance = plan_key(c, i, &args).id();
        if rows.contains_key(&instance) {
            continue;
        }
        let mir = world.body_result(c, i)?;
        let also = also_reads(world, c, i).map(|_| world.body_result(c, 3)).transpose()?;
        let (plan, bytes) = plan_task(c, i, &args, mir, also, &mut |id: &ContentId| world.plan_value(id))?;
        let result = result_of(&bytes)?;
        let root = blaze_format::open::<Traced>(&bytes).map_err(|e| e.to_string())?;
        let view = record::read_plan_payload(root.payload.follow(&bytes).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let mut targets = Vec::new();
        for edge in view.edges {
            let args = view.args_of(edge).ok_or("an edge's arguments out of bounds")?;
            let (d, j) = item_of(StableId(edge.item)).ok_or("an edge to no item")?;
            targets.push(plan_key(d, j, args).id());
            queue.push((d, j, args.to_vec()));
        }
        // A row's targets are its code ask's callees: sorted, each once (the driver's contract).
        targets.sort_unstable();
        targets.dedup();
        rows.insert(instance, FreshRow { item: subject(c, i), plan, result, describe: view.describe.to_vec(), targets });
    }
    Ok(rows)
}

/// One program's model walk: from its root along every plan's edges, each instance once, and
/// the code of each once every plan is in (a code ask's callees must be). It walks whole every
/// request, which the contract allows (it asks every moved instance it still reaches, and
/// more); the driver answers what did not move from the kept rows, so the walk costs reads, not
/// runs. It names plans and keys code as the front end's walker does: a plan by the digest of
/// its ask, code by the plan's description (the template identity).
#[derive(Default)]
struct ModelProgram {
    /// Plan asks by id: the item, its instance and its ask bytes.
    plans: BTreeMap<u32, (StableId, Key<PlanRecord>, Vec<u8>)>,
    asked: BTreeSet<Vec<u8>>,
    queue: VecDeque<u32>,
    /// Plan asks not answered: no code is asked before they are.
    pending: u32,
    /// Per instance planned, in order: its code key, callees (sorted, each once) and span
    /// tables (its item's one).
    codes: VecDeque<(Key<PlanRecord>, Key<CodeRecord>, Vec<Key<PlanRecord>>, Vec<SpanAt>)>,
    /// The code ask handed out last, whose callees and span tables the walker's borrow lends.
    lent: Vec<Key<PlanRecord>>,
    lent_spans: Vec<SpanAt>,
    /// Code asks not answered: the instance each is of.
    code_of: BTreeMap<u32, Key<PlanRecord>>,
    rows: BTreeMap<ContentId, (Key<PlanRecord>, Option<Key<CodeRecord>>)>,
    kept: Vec<ContentId>,
    next: u32,
    out: Vec<Row>,
    gone: Vec<Key<PlanRecord>>,
}

impl ModelProgram {
    fn ask_plan(&mut self, (c, i): (usize, usize), args: &[u8]) {
        let ask = plan_ask(c, i, args);
        if self.asked.insert(ask.clone()) {
            let id = self.next;
            self.next += 1;
            self.plans.insert(id, (subject(c, i), plan_key(c, i, args), ask));
            self.queue.push_back(id);
            self.pending += 1;
        }
    }
}

#[derive(Default)]
struct ModelWalk {
    programs: [ModelProgram; PROGRAMS],
    /// Every item's line this request: what [`Plant::CodeKeyedByPlace`] keys code by.
    lines: [[u8; ITEMS]; UNITS],
}

const NO_ASK: TaskFailed = TaskFailed::Refused { what: "the model walk was answered an ask it did not make" };

impl Walker for ModelWalk {
    fn begin(&mut self, program: u32, kept: Kept<'_>, moved: &[Key<PlanRecord>], _units: &[Key<UnitManifest>]) -> Result<(), TaskFailed> {
        let walk = self.programs.get_mut(program as usize).ok_or(NO_ASK)?;
        *walk = ModelProgram::default();
        if moved.windows(2).any(|pair| pair[0].id() >= pair[1].id()) || moved.iter().any(|key| kept.row(&key.id()).is_none()) {
            return Err(TaskFailed::Refused { what: "moved instances out of order, or not kept" });
        }
        walk.kept = kept.rows.iter().map(|row| row.instance).collect();
        walk.ask_plan(ROOTS[program as usize], &[]);
        Ok(())
    }

    fn ask(&mut self, program: u32) -> Option<WalkAsk<'_>> {
        let walk = self.programs.get_mut(program as usize)?;
        if let Some(ask) = walk.queue.pop_front() {
            let (item, instance, bytes) = walk.plans.get(&ask)?;
            // A kept instance is asked by the ask its row keeps (`walk.kept` is sorted).
            let bytes = match walk.kept.binary_search(&instance.id()) {
                Ok(_) => None,
                Err(_) => Some(&bytes[..]),
            };
            return Some(WalkAsk::Plan { ask, instance: *instance, bytes, body: Some(*item) });
        }
        if walk.pending > 0 {
            return None;
        }
        let (instance, key, callees, spans) = walk.codes.pop_front()?;
        let ask = walk.next;
        walk.next += 1;
        walk.code_of.insert(ask, instance);
        walk.lent = callees;
        walk.lent_spans = spans;
        Some(WalkAsk::Code { ask, instance, key, tier: 0, callees: &walk.lent, statics: &[], spans: &walk.lent_spans })
    }

    fn planned(&mut self, program: u32, ask: u32, plan: Planned<'_>) -> Result<(), TaskFailed> {
        let lines = self.lines;
        let walk = self.programs.get_mut(program as usize).ok_or(NO_ASK)?;
        let (item, instance, _) = walk.plans.get(&ask).ok_or(NO_ASK)?;
        if plan.instance != *instance {
            return Err(TaskFailed::Refused { what: "a plan answered with another instance than its ask's" });
        }
        let (c, i) = item_of(*item).ok_or(NO_ASK)?;
        if walk.rows.insert(plan.instance.id(), (plan.instance, None)).is_some() {
            return Err(TaskFailed::Refused { what: "one instance answered to two plan asks" });
        }
        walk.pending -= 1;
        let payload = Aligned::copy_from(plan.payload);
        let view = record::read_plan_payload(&payload).map_err(|_| TaskFailed::Refused { what: "a plan payload that does not open" })?;
        let mut callees = Vec::new();
        for edge in view.edges {
            let args = view.args_of(edge).ok_or(TaskFailed::Refused { what: "an edge's arguments out of bounds" })?;
            let callee = item_of(StableId(edge.item)).ok_or(TaskFailed::Refused { what: "an edge to no item" })?;
            callees.push(plan_key(callee.0, callee.1, args));
            walk.ask_plan(callee, args);
        }
        callees.sort_unstable_by_key(|key| key.id());
        callees.dedup();
        // The plant: code keyed by its item's line too, so a comment above the item moves it.
        let mut code = code_key(plan.instance, view.describe);
        if planted(Plant::CodeKeyedByPlace) {
            let mut placed = code.id().0.to_vec();
            placed.push(lines[c][i]);
            code = CodeRecord::key(&CodeRecordInputs { template: record::digest(&placed) });
        }
        walk.codes.push_back((plan.instance, code, callees, vec![span_at(c, i)]));
        Ok(())
    }

    fn coded(&mut self, program: u32, ask: u32, code: Key<CodeRecord>) -> Result<(), TaskFailed> {
        let walk = self.programs.get_mut(program as usize).ok_or(NO_ASK)?;
        let instance = walk.code_of.remove(&ask).ok_or(NO_ASK)?;
        let row = walk.rows.get_mut(&instance.id()).ok_or(NO_ASK)?;
        row.1 = Some(code);
        Ok(())
    }

    fn end(&mut self, program: u32) -> Result<WalkEnd<'_>, TaskFailed> {
        let walk = self.programs.get_mut(program as usize).ok_or(NO_ASK)?;
        if !walk.queue.is_empty() || !walk.codes.is_empty() || !walk.code_of.is_empty() {
            return Err(TaskFailed::Refused { what: "a walk ended with asks not answered" });
        }
        let rows: Option<Vec<Row>> = walk.rows.values().map(|&(instance, code)| code.map(|code| Row { instance, code })).collect();
        walk.out = rows.ok_or(TaskFailed::Refused { what: "a walk ended with a row with no code" })?;
        let gone: Vec<Key<PlanRecord>> = walk.kept.iter().filter(|id| !walk.rows.contains_key(*id)).map(|id| Key::from_id(*id)).collect();
        walk.gone = gone;
        Ok(WalkEnd { rows: &walk.out, gone: &walk.gone })
    }
}

enum Answer {
    Got(Ticket, Option<Aligned>),
    Answers(Ticket, Vec<ContentId>),
    Keyed(Ticket, Key<UnitManifest>, Key<Surface>, Vec<FileItems>, Vec<(StableId, Key<Body>)>, Vec<ContentId>),
    Wrote(Ticket, Aligned),
    /// An owner's span task: its reads and its tables by anchor.
    Spans(Ticket, Vec<Fact>, Vec<(ContentId, Vec<u8>)>),
    /// A task that failed: a poisoned body or plan, or a plan that read a body not given.
    Failed(Ticket, TaskFailed),
    Put(Ticket, Tag, ContentId, Aligned),
    Reads(Ticket),
    Linked(Ticket, Vec<u8>),
    /// A program's image made, under its name: it lands when answered, as a put does.
    Imaged(Ticket, ContentId, Vec<u8>),
    /// A program's path placed to name an image: it lands when answered.
    Placed(Ticket, u32, ContentId),
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
    /// Whether requests build the programs.
    programs: bool,
    /// The kept program manifests the last finished request's certificate names.
    last_programs: [Option<Key<ProgramManifest>>; PROGRAMS],
    /// What the last finished request's driver gave: its failures, and each program's manifest.
    failures: Vec<Failure>,
    program_keys: Vec<Option<Key<ProgramManifest>>>,
    /// The store's program images, by name (a kept manifest's key).
    images: BTreeMap<ContentId, Vec<u8>>,
    /// The patch each program's link step returned in this request, held as the host holds it
    /// until the driver asks the image made.
    patches: [Option<Patch<Global>>; PROGRAMS],
    /// The image each program's path names, the one the user runs.
    paths: [Option<ContentId>; PROGRAMS],
}

impl Sim {
    fn new() -> Sim {
        Sim {
            images: BTreeMap::new(),
            patches: [const { None }; PROGRAMS],
            paths: [None; PROGRAMS],
            world: World::new(),
            disk: Disk::new(),
            last: [None; UNITS],
            in_flight: 0,
            requests: 0,
            programs: false,
            last_programs: [None; PROGRAMS],
            failures: Vec::new(),
            program_keys: Vec::new(),
        }
    }

    fn program_plans(&self) -> Vec<ProgramPlan> {
        match self.programs {
            true => program_plans(&self.last_programs),
            false => Vec::new(),
        }
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
        // The asker's own late table; under the plant, unit 1 is handed unit 0's.
        let of = match planted(Plant::LateOfAnotherUnit) && asker == 1 {
            true => 0,
            false => asker as usize,
        };
        // A program's asks are of no unit: no late table (a plan may read no late fact).
        let late = match of < UNITS {
            true => self.world.late(of),
            false => Vec::new(),
        };
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
            Effect::Run { ticket, task, universe, made, bytes, plans, bodies, .. } => {
                self.in_flight += 1;
                if self.in_flight > WORKERS {
                    return Err(Broke::Oracle(format!("{} tasks at once: more tasks than workers", self.in_flight)));
                }
                let (world, earlier) = (self.world, self.requests.saturating_sub(1));
                match task {
                    Task::Key { unit } => {
                        let c = unit as usize;
                        let (files, items, names) = world.keyed(c);
                        Answer::Keyed(ticket, world.text_key(c), world.surface_key(c), files, items, names)
                    }
                    Task::Spans { program, unit, owner } => {
                        let Some((c, i)) = item_of(owner).filter(|_| self.programs && (program as usize) < PROGRAMS) else {
                            return Err(Broke::Oracle("a span task of no item, or of no program".into()));
                        };
                        if unit as usize != c {
                            return Err(Broke::Oracle(format!("unit {c} item {i}'s span task run as another unit's")));
                        }
                        let (reads, tables) = span_task(&world, c, i);
                        Answer::Spans(ticket, reads, tables)
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
                    Task::Body { unit, item, key, steps } => {
                        let c = unit as usize;
                        let Some(i) = (0..ITEMS).find(|&i| stable(c, i) == item) else {
                            return Err(Broke::Oracle(format!("a body task for no item of unit {c}")));
                        };
                        // Of a unit not asked, only a maker may run, demanded: item 3, whose
                        // const another unit reads. No fact of items 0..3 is read elsewhere but
                        // by a plan, which reads every body it reaches.
                        if !world.asked(c) && i != 3 && !self.programs {
                            return Err(Broke::Oracle(format!("a body of a unit not asked was run (unit {c} item {i}): no reader demanded it")));
                        }
                        if world.unreached[c][i] {
                            return Err(Broke::Oracle(format!(
                                "a body of an item its surface does not name was asked (unit {c} item {i}): the front end has no such body"
                            )));
                        }
                        if (steps, key) != world.body_steps(c, i) {
                            return Err(Broke::Oracle(format!("unit {c} item {i}'s body asked under other steps or another key")));
                        }
                        if world.poison[c][i] {
                            return Ok(Answer::Failed(ticket, POISONED));
                        }
                        let bytes = self
                            .with_universe(unit, universe, made, |universe| {
                                body_task(&world, c, i, key, true, earlier, &mut |id: &ContentId| {
                                    universe.answer(id).map_err(|why| format!("{why:?}"))
                                })
                            })?
                            .map_err(Broke::Oracle)?;
                        Answer::Wrote(ticket, Aligned::copy_from(&bytes))
                    }
                    Task::Plan { program, key } => {
                        let Some(((c, i), args)) = ask_of_plan(bytes).filter(|_| self.programs && (program as usize) < PROGRAMS) else {
                            return Err(Broke::Oracle("a plan task of no item, or of no program".into()));
                        };
                        if key != plan_key(c, i, args) {
                            return Err(Broke::Oracle(format!("unit {c} item {i}'s plan asked under another name than its ask's")));
                        }
                        // It is given bodies sorted, each once, of its item and of the one other
                        // it reads, each from the record the driver names: filed before.
                        let also = also_reads(&world, c, i);
                        if bodies.windows(2).any(|pair| pair[0].item >= pair[1].item)
                            || bodies.iter().any(|body| body.item != subject(c, i) && Some(body.item) != also)
                        {
                            return Err(Broke::Oracle(format!("unit {c} item {i}'s plan is given bodies out of order, twice, or of no item it reads")));
                        }
                        let read = |item: StableId| -> Result<Option<ContentId>, Broke> {
                            let Some(body) = bodies.iter().find(|body| body.item == item) else { return Ok(None) };
                            let Some(record) = self.get(Body::TAG, body.address.id()) else {
                                return Err(Broke::Oracle(format!("unit {c} item {i}'s plan is given a body the store does not hold")));
                            };
                            result_of(record).map(Some).map_err(Broke::Oracle)
                        };
                        let Some(mir) = read(subject(c, i))? else {
                            return Err(Broke::Oracle(format!("unit {c} item {i}'s plan is given no body of its item")));
                        };
                        // The other body it reads, when not given: the front end's refusal, which
                        // the driver answers by finding it and running the plan again.
                        let also = match also {
                            None => None,
                            Some(item) => match read(item)? {
                                Some(result) => Some(result),
                                None => return Ok(Answer::Failed(ticket, TaskFailed::NoBody { item })),
                            },
                        };
                        if world.poison_plan[c][i] {
                            return Ok(Answer::Failed(ticket, POISONED));
                        }
                        let (_, record) = self
                            .with_universe(NO_UNIT, universe, made, |universe| {
                                plan_task(c, i, args, mir, also, &mut |id: &ContentId| universe.answer(id).map_err(|why| format!("{why:?}")))
                            })?
                            .map_err(Broke::Oracle)?;
                        Answer::Wrote(ticket, Aligned::copy_from(&record))
                    }
                    Task::Code { key, .. } => {
                        if !self.programs {
                            return Err(Broke::Oracle("a code task of a request that builds no program".into()));
                        }
                        // It is given exactly its callees' plans, each held at its address.
                        let callees = edge_instances(bytes).map_err(Broke::Oracle)?;
                        if !callees.iter().copied().eq(plans.iter().map(|at| at.instance.id())) {
                            return Err(Broke::Oracle("a code task given other plans than its callees'".into()));
                        }
                        if plans.iter().any(|at| self.get(PlanRecord::TAG, at.address).is_none()) {
                            return Err(Broke::Oracle("a code task given a callee plan the store does not hold".into()));
                        }
                        // It opens its plan's snapshot: the plan's universe, and given bodies
                        // sorted, each once, each one its plan read with the result it read.
                        if universe.is_empty() {
                            return Err(Broke::Oracle("a code task given no universe to open its plan's snapshot in".into()));
                        }
                        let root = blaze_format::open::<Traced>(bytes).map_err(|e| Broke::Oracle(e.to_string()))?;
                        let reads = root.reads.follow(bytes).map_err(|e| Broke::Oracle(e.to_string()))?;
                        if bodies.windows(2).any(|pair| pair[0].item >= pair[1].item) {
                            return Err(Broke::Oracle("a code task given bodies out of order or twice".into()));
                        }
                        for body in bodies {
                            let read = reads.iter().find(|read| read.id == record::mir_id(&body.item));
                            let record = self.get(Body::TAG, body.address.id()).ok_or_else(|| Broke::Oracle("a code task given a body the store does not hold".into()))?;
                            if read.map(|read| read.digest) != Some(result_of(record).map_err(Broke::Oracle)?) {
                                return Err(Broke::Oracle("a code task given a body its plan did not read, or not as its plan read it".into()));
                            }
                        }
                        // The model's code reads every body its plan read: one not given is the
                        // front end's refusal, which the driver answers by giving it.
                        let mir = |fact: &&Fact| record::kind_of(&fact.id) == Some(FactKind::Mir);
                        for fact in reads.iter().filter(mir) {
                            if bodies.iter().all(|body| record::mir_id(&body.item) != fact.id) {
                                let item = (0..UNITS)
                                    .flat_map(|c| (0..ITEMS).map(move |i| subject(c, i)))
                                    .find(|item| record::mir_id(item) == fact.id)
                                    .ok_or_else(|| Broke::Oracle("a plan read the body of no item".into()))?;
                                return Ok(Answer::Failed(ticket, TaskFailed::NoBody { item }));
                            }
                        }
                        Answer::Wrote(ticket, Aligned::copy_from(&code_task(key, bytes).map_err(Broke::Oracle)?))
                    }
                }
            }
            Effect::Put { ticket, tag, key, bytes, .. } => Answer::Put(ticket, tag, key, Aligned::copy_from(bytes)),
            Effect::Reads { ticket } => Answer::Reads(ticket),
            Effect::Link { ticket, program, changes, plans, spans, payload } => {
                if !self.programs || program as usize >= PROGRAMS {
                    return Err(Broke::Oracle("a link step of no program".into()));
                }
                if changes.windows(2).any(|pair| pair[0].instance.id() >= pair[1].instance.id()) {
                    return Err(Broke::Oracle("a link step given rows out of instance order".into()));
                }
                let planned = |instance: ContentId| plans.binary_search_by_key(&instance, |at| at.instance.id()).is_ok();
                if changes.iter().any(|change| change.now.is_some() && !planned(change.instance.id())) {
                    return Err(Broke::Oracle("a link step without the plan of a row that came or moved".into()));
                }
                if plans.iter().any(|at| self.get(PlanRecord::TAG, at.address).is_none()) {
                    return Err(Broke::Oracle("a link step given a plan the store does not hold".into()));
                }
                let (mut image, mut positions) = linker_of(payload).map_err(Broke::Oracle)?;
                for change in changes {
                    let instance = change.instance.id();
                    if image.get(&instance).copied() != change.was.map(|was| was.id()) {
                        return Err(Broke::Oracle("a link step given a row whose old code is not what it linked".into()));
                    }
                    match change.now {
                        Some(now) => {
                            if self.get(CodeRecord::TAG, now.id()).is_none() {
                                return Err(Broke::Oracle("a link step given code the store does not hold".into()));
                            }
                            image.insert(instance, now.id());
                        }
                        None => {
                            image.remove(&instance);
                        }
                    }
                }
                // Positions: an item no row is of any more goes with its rows; each table given
                // (one that came, or whose bytes moved) is read from the store at its address.
                let mut live = BTreeSet::new();
                for instance in image.keys() {
                    let (c, i) = instance_item(instance).ok_or_else(|| Broke::Oracle("a linked row of no item".into()))?;
                    live.insert(anchor(c, i));
                }
                positions.retain(|anchor, _| live.contains(anchor));
                if spans.windows(2).any(|pair| pair[0].anchor >= pair[1].anchor) {
                    return Err(Broke::Oracle("a link step given span tables out of anchor order, or twice".into()));
                }
                for at in spans {
                    if !live.contains(&at.anchor) {
                        return Err(Broke::Oracle("a link step given a span table no row is of".into()));
                    }
                    let Some(record) = self.get(SpanTable::TAG, at.address.id()) else {
                        return Err(Broke::Oracle("a link step given a span table the store does not hold".into()));
                    };
                    let root = blaze_format::open::<Traced>(record).map_err(|e| Broke::Oracle(e.to_string()))?;
                    let table = root.payload.follow(record).map_err(|e| Broke::Oracle(e.to_string()))?;
                    positions.insert(at.anchor, digest(table));
                }
                if live.iter().any(|anchor| !positions.contains_key(anchor)) {
                    return Err(Broke::Oracle("a link step not given the span table of a row that came".into()));
                }
                let state = linker_bytes(&image, &positions);
                // The patch from the old state's image to the new one's: the host holds it. With
                // no old state the image is made from nothing, so the patch writes all of it.
                let old = match payload.is_empty() {
                    true => Vec::new(),
                    false => image_file(payload),
                };
                self.patches[program as usize] = Some(patch_between(&old, &image_file(&state)));
                Answer::Linked(ticket, state)
            }
            Effect::Image { ticket, program, from, to, patched } => {
                if !self.programs || program as usize >= PROGRAMS {
                    return Err(Broke::Oracle("an image of no program".into()));
                }
                // A clone of the image named `from` (the store must hold it), the patch applied.
                let mut bytes = match from {
                    None => Vec::new(),
                    Some(from) => match self.images.get(&from.id()) {
                        Some(held) => held.clone(),
                        None => return Err(Broke::Oracle(format!("program {program}'s image made from one the store does not hold"))),
                    },
                };
                if patched {
                    let Some(patch) = self.patches[program as usize].take() else {
                        return Err(Broke::Oracle(format!("program {program}'s image patched with no link step's patch in this request")));
                    };
                    // A patch over a clone needs the image it clones; the host refuses one with none.
                    if patch.base == PatchBase::Clone && from.is_none() {
                        return Err(Broke::Oracle(format!("program {program}'s image patched over a clone of no image")));
                    }
                    apply_patch(&mut bytes, &patch);
                }
                // The plant: the host names the image by its bytes, not by the key it was given.
                let name = match planted(Plant::ImageNamedByContent) {
                    true => digest(&bytes),
                    false => to.id(),
                };
                if self.images.get(&name).is_some_and(|held| *held != bytes) {
                    return Err(Broke::Oracle(format!("program {program}'s image made again under its name with other bytes")));
                }
                Answer::Imaged(ticket, name, bytes)
            }
            Effect::Place { ticket, program, image } => {
                if !self.programs || program as usize >= PROGRAMS {
                    return Err(Broke::Oracle("a place of no program".into()));
                }
                Answer::Placed(ticket, program, image.id())
            }
        })
    }

    fn deliver<M: Memo>(&mut self, driver: &mut Driver<'_, '_, M, Local, Global, ModelWalk>, answer: Answer) {
        match answer {
            Answer::Got(ticket, record) => driver.give(Input::Got { ticket, record: record.as_deref() }),
            Answer::Answers(ticket, now) => driver.give(Input::Answers { ticket, now: &now }),
            Answer::Keyed(ticket, manifest, surface, files, items, names) => {
                self.in_flight -= 1;
                let outcome = Ok(Done::Keyed { manifest, surface, files: &files, items: &items, names: &names });
                driver.give(Input::Ran { ticket, outcome });
            }
            Answer::Spans(ticket, reads, tables) => {
                self.in_flight -= 1;
                let tables: Vec<(ContentId, &[u8])> = tables.iter().map(|(anchor, table)| (*anchor, &table[..])).collect();
                driver.give(Input::Ran { ticket, outcome: Ok(Done::Spans { reads: &reads, tables: &tables }) });
            }
            Answer::Wrote(ticket, record) => {
                self.in_flight -= 1;
                driver.give(Input::Ran { ticket, outcome: Ok(Done::Wrote { record: &record }) });
            }
            Answer::Failed(ticket, failed) => {
                self.in_flight -= 1;
                driver.give(Input::Ran { ticket, outcome: Err(failed) });
            }
            Answer::Linked(ticket, linker) => driver.give(Input::Linked { ticket, outcome: Ok(&linker[..]) }),
            Answer::Imaged(ticket, name, bytes) => {
                // The image lands when its making is answered: a kill before drops it.
                self.images.insert(name, bytes);
                driver.give(Input::Imaged { ticket, outcome: Ok(()) });
            }
            Answer::Placed(ticket, program, image) => {
                self.paths[program as usize] = Some(image);
                driver.give(Input::Placed { ticket, outcome: Ok(()) });
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
        self.patches = [const { None }; PROGRAMS];
        let plans = self.world.plans(&self.last);
        let programs = self.program_plans();
        let outcome = proof::scope(|snap| self.drive(snap, memo, tape, kill, &plans, &programs))?;
        // After every request, killed ones included: each program's path names an image the
        // store holds, of a kept manifest that is durable (a place is the last effect, after the
        // manifest naming its image).
        for (p, path) in self.paths.iter().enumerate() {
            if let Some(image) = *path {
                self.check_image(p, image, "path")?;
            }
        }
        if let Some(outcome) = &outcome {
            let key = certificate_with(&Plan { certificate: certificate(), namespace: Namespace::LOCAL, units: &plans }, &programs);
            let (units, kept) = self.check_end(outcome, M::AUDIT, key, programs.len())?;
            self.last = units.map(Some);
            // A certified request files nothing: the certificate it found is the one filed for
            // this world before, and names the kept manifests filed then. After an edit undone,
            // those can be other bytes of the same rows than the last request's (its manifest
            // was filed against the unit manifests of the edit), and naming them is right. An
            // audited request walks every program from scratch, so it is not held to this.
            let built = matches!(outcome, Outcome::Built { filed: Filed::Certificate, .. });
            for (p, key) in kept.into_iter().enumerate() {
                // A request that builds files a program's kept manifest again only when a row of
                // it moved (its plan's address, result, code, targets or ask) or a span table
                // did (its address or bytes): an edit no plan and no table reads, and a request
                // repeated, write nothing of the program.
                if built
                    && let Some(was) = self.last_programs[p]
                    && was != key
                    && self.kept_rows(was)? == self.kept_rows(key)?
                    && self.kept_spans(was)? == self.kept_spans(key)?
                {
                    return Err(Broke::Oracle(format!("program {p}'s kept manifest was filed again with no row moved")));
                }
                // Every answer, a certified one too, leaves the path naming the image of the kept
                // manifest it certified.
                if self.paths[p] != Some(key.id()) {
                    return Err(Broke::Oracle(format!("program {p}'s path does not name the image of what the answer certified")));
                }
                self.last_programs[p] = Some(key);
            }
        }
        Ok(outcome)
    }

    /// The host's answers are a [`Flight`]: each step the tape chooses whether one is delivered
    /// before the driver is asked again, and which; calm asks the driver first and delivers the
    /// oldest when it waits.
    fn drive<M: Memo>(
        &mut self,
        snap: Snapshot<'_>,
        memo: &mut M,
        tape: &Tape,
        kill: Option<u32>,
        plans: &[UnitPlan<'_>],
        programs: &[ProgramPlan],
    ) -> Result<Option<Outcome>, Broke> {
        let plan = Plan { certificate: certificate(), namespace: Namespace::LOCAL, units: plans };
        let walk = ModelWalk { lines: self.world.lines(), ..ModelWalk::default() };
        let mut driver = Driver::build(snap, memo, Local { workers: WORKERS }, plan, programs, walk, None, Global)
            .map_err(|error| Broke::Harness(format!("{error:?}")))?;
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
                    self.failures = driver.failures().cloned().collect();
                    self.program_keys = (0..programs.len() as u32).map(|p| driver.program_manifest(p)).collect();
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
    fn check_end(
        &self,
        outcome: &Outcome,
        audit: bool,
        certificate: Key<Certificate>,
        programs: usize,
    ) -> Result<([Key<UnitManifest>; UNITS], Vec<Key<ProgramManifest>>), Broke> {
        let checks: Checks = match outcome {
            Outcome::Certified { checks } => *checks,
            Outcome::Built { ran, filed: Filed::Certificate } => ran.checks,
            Outcome::Built { ran, filed: Filed::Audited } if audit => ran.checks,
            other => return Err(Broke::Oracle(format!("the request ended {other:?}"))),
        };
        if checks.rechecks != 0 {
            return Err(Broke::Oracle(format!("{} checks of an item already proved: a check after a proof", checks.rechecks)));
        }
        let address = outside::address(certificate, SHAPE, &[self.world.digest()]);
        let Some(bytes) = self.get(Certificate::TAG, address.id()) else {
            return Err(Broke::Oracle("an answer whose certificate the store does not hold".into()));
        };
        let root = record::open::<Certificate>(bytes, &address).map_err(|e| Broke::Oracle(e.to_string()))?;
        let payload = root.payload.follow(bytes).map_err(|e| Broke::Oracle(e.to_string()))?;
        if payload.len() != 32 * (UNITS + programs) {
            return Err(Broke::Oracle("a certificate naming another number of manifests".into()));
        }
        let fresh = fresh(&self.world).map_err(Broke::Harness)?;
        let named = |at: usize| {
            let mut id = [0u8; 32];
            id.copy_from_slice(&payload[32 * at..32 * at + 32]);
            ContentId(id)
        };
        let mut manifests = [Key::from_id(ContentId([0; 32])); UNITS];
        for c in 0..UNITS {
            manifests[c] = Key::from_id(named(c));
            self.check_unit(c, manifests[c], &fresh[c])?;
        }
        let mut kept = Vec::new();
        for p in 0..programs {
            let key = Key::from_id(named(UNITS + p));
            self.check_program(p, key)?;
            kept.push(key);
        }
        Ok((manifests, kept))
    }

    /// The rows of the kept manifest `key`, each with its targets and ask, by value.
    fn kept_rows(&self, key: Key<ProgramManifest>) -> Result<Vec<(KeptRow, Vec<ContentId>, Vec<u8>)>, Broke> {
        let oracle = |e: &dyn std::fmt::Display| Broke::Oracle(format!("a kept manifest: {e}"));
        let bytes = self.get(ProgramManifest::TAG, key.id()).ok_or_else(|| Broke::Oracle("a kept manifest named is not held".into()))?;
        let root = record::open::<ProgramManifest>(bytes, &key).map_err(|e| oracle(&e))?;
        let parts = record::read_kept(root.payload.follow(bytes).map_err(|e| oracle(&e))?).map_err(|e| oracle(&e))?;
        let kept = Kept::of(&parts);
        Ok(parts
            .rows
            .iter()
            .map(|row| {
                // Positions into the shared tables are not the row's content.
                let mut at = *row;
                (at.shape_at, at.target_at, at.ask_at) = (0, 0, 0);
                (at, kept.targets_of(row).to_vec(), kept.ask_of(row).to_vec())
            })
            .collect())
    }

    /// The span tables of the kept manifest `key`, each with its reads, by value.
    fn kept_spans(&self, key: Key<ProgramManifest>) -> Result<Vec<(KeptSpan, Vec<Fact>)>, Broke> {
        let oracle = |e: &dyn std::fmt::Display| Broke::Oracle(format!("a kept manifest: {e}"));
        let bytes = self.get(ProgramManifest::TAG, key.id()).ok_or_else(|| Broke::Oracle("a kept manifest named is not held".into()))?;
        let root = record::open::<ProgramManifest>(bytes, &key).map_err(|e| oracle(&e))?;
        let parts = record::read_kept(root.payload.follow(bytes).map_err(|e| oracle(&e))?).map_err(|e| oracle(&e))?;
        Ok(parts
            .spans
            .iter()
            .map(|span| {
                // Positions into the shared reads are not the table's content.
                let mut at = *span;
                (at.read_at, at.read_len) = (0, 0);
                (at, parts.span_reads_of(span).to_vec())
            })
            .collect())
    }

    /// That `image`, which program `p`'s `what` names, is a durable kept manifest's key and the
    /// store holds under it the image of that manifest's linker state.
    fn check_image(&self, p: usize, image: ContentId, what: &str) -> Result<(), Broke> {
        let oracle = |e: &dyn std::fmt::Display| Broke::Oracle(format!("program {p}: {e}"));
        let Some(bytes) = self.get(ProgramManifest::TAG, image) else {
            return Err(Broke::Oracle(format!("program {p}'s {what} names an image whose kept manifest is not durable")));
        };
        let key = Key::<ProgramManifest>::from_id(image);
        let root = record::open::<ProgramManifest>(bytes, &key).map_err(|e| oracle(&e))?;
        let parts = record::read_kept(root.payload.follow(bytes).map_err(|e| oracle(&e))?).map_err(|e| oracle(&e))?;
        match self.images.get(&image) {
            None => Err(Broke::Oracle(format!("program {p}'s {what} names an image the store does not hold"))),
            Some(held) if *held != image_file(parts.linker) => {
                Err(Broke::Oracle(format!("program {p}'s {what} names an image that is not of its kept linker state")))
            }
            Some(_) => Ok(()),
        }
    }

    /// Program `p`'s kept manifest against a fresh build of it: row by row, the store holding
    /// each plan and code it names, every fact its dictionary holds true now, and the linker's
    /// state the image of the fresh rows.
    fn check_program(&self, p: usize, key: Key<ProgramManifest>) -> Result<(), Broke> {
        let oracle = |e: &dyn std::fmt::Display| Broke::Oracle(format!("program {p}: {e}"));
        let Some(bytes) = self.get(ProgramManifest::TAG, key.id()) else {
            return Err(Broke::Oracle(format!("program {p}'s kept manifest is named but the store does not hold it")));
        };
        let root = record::open::<ProgramManifest>(bytes, &key).map_err(|e| oracle(&e))?;
        let payload = root.payload.follow(bytes).map_err(|e| oracle(&e))?;
        let parts = record::read_kept(payload).map_err(|e| oracle(&e))?;
        let fresh = fresh_program(&self.world, p).map_err(Broke::Harness)?;
        if parts.program != program_id(p) {
            return Err(Broke::Oracle(format!("program {p}'s kept manifest is another program's")));
        }
        // The store holds the program's image under the kept manifest's key.
        self.check_image(p, key.id(), "kept manifest")?;
        if parts.rows.len() != fresh.len() {
            return Err(Broke::Oracle(format!(
                "program {p}'s kept manifest has {} rows of {}: not what a fresh build files",
                parts.rows.len(),
                fresh.len()
            )));
        }
        let kept = Kept::of(&parts);
        for (row, (instance, fresh)) in parts.rows.iter().zip(&fresh) {
            let same = row.instance == *instance
                && row.item == fresh.item.0
                && row.plan == fresh.plan
                && row.result == fresh.result
                && kept.targets_of(row) == &fresh.targets[..];
            if !same {
                return Err(Broke::Oracle(format!("program {p}'s row is not what a fresh build files")));
            }
            let Some(plan) = self.get(PlanRecord::TAG, row.plan) else {
                return Err(Broke::Oracle(format!("program {p}'s plan is named but the store does not hold it")));
            };
            if result_of(plan).map_err(Broke::Oracle)? != row.result {
                return Err(Broke::Oracle(format!("program {p}'s plan record is not what a fresh build files")));
            }
            // The code key is the walker's; what the oracle holds is the code under it: made of
            // what a fresh plan describes now.
            let Some(code) = self.get(CodeRecord::TAG, row.code) else {
                return Err(Broke::Oracle(format!("program {p}'s code is named but the store does not hold it")));
            };
            if **code != *code_bytes(Key::from_id(row.code), &fresh.describe) {
                return Err(Broke::Oracle(format!("program {p}'s code is not what a fresh build files")));
            }
        }
        for fact in parts.dictionary {
            let now = match record::kind_of(&fact.id) {
                Some(FactKind::Mir) => {
                    let found = (0..UNITS).flat_map(|c| (0..ITEMS).map(move |i| (c, i))).find(|&(c, i)| record::mir_id(&subject(c, i)) == fact.id);
                    let Some((c, i)) = found else {
                        return Err(Broke::Oracle(format!("program {p}'s dictionary holds a body of no item")));
                    };
                    self.world.body_result(c, i).map_err(Broke::Harness)?
                }
                _ => self.world.plan_value(&fact.id).map_err(Broke::Harness)?,
            };
            if now != fact.digest {
                return Err(Broke::Oracle(format!("program {p}'s dictionary holds a read that is not true now: not what a fresh build files")));
            }
        }
        // The rows were checked against the fresh ones above, their code by content.
        let image: BTreeMap<ContentId, ContentId> = parts.rows.iter().map(|row| (row.instance, row.code)).collect();
        let (linked, positions) = linker_of(parts.linker).map_err(|why| oracle(&why))?;
        if linked != image {
            return Err(Broke::Oracle(format!(
                "program {p}'s linker state is not the fresh rows' image: the link step was not given a row that moved"
            )));
        }
        // Each item a fresh row is of, at the line a fresh build prints for it.
        let mut items = BTreeSet::new();
        for row in fresh.values() {
            items.insert(item_of(row.item).ok_or_else(|| Broke::Harness("a fresh row of no item".into()))?);
        }
        let fresh_positions: BTreeMap<ContentId, ContentId> =
            items.iter().map(|&(c, i)| (anchor(c, i), digest(&self.world.span_table(c, i)))).collect();
        if positions != fresh_positions {
            return Err(Broke::Oracle(format!("program {p}'s image holds a position a fresh build does not")));
        }
        // Its span tables: one per item a row is of, each held at the address its key and reads
        // make, its table a fresh one, its reads true now.
        let anchors: BTreeSet<ContentId> = parts.spans.iter().map(|span| span.anchor).collect();
        let fresh_anchors: BTreeSet<ContentId> = fresh_positions.keys().copied().collect();
        if anchors != fresh_anchors || anchors.len() != parts.spans.len() {
            return Err(Broke::Oracle(format!("program {p}'s kept manifest names other span tables: not what a fresh build files")));
        }
        for span in parts.spans {
            let Some(&(c, i)) = items.iter().find(|&&(c, i)| anchor(c, i) == span.anchor) else {
                return Err(Broke::Oracle(format!("program {p}'s span table is of no item")));
            };
            if (Key::from_id(span.key), StableId(span.owner)) != (span_at(c, i).key, subject(c, i)) {
                return Err(Broke::Oracle(format!("program {p}'s span table is kept under another key or owner")));
            }
            let reads = parts.span_reads_of(span);
            if reads.iter().any(|read| self.world.span_file_value(&read.id) != Some(read.digest)) {
                return Err(Broke::Oracle(format!("program {p}'s span table holds a read that is not true now: not what a fresh build files")));
            }
            let Some(record) = self.get(SpanTable::TAG, span.address) else {
                return Err(Broke::Oracle(format!("program {p}'s span table is named but the store does not hold it")));
            };
            let (at, root) = record::check_read::<SpanTable>(record, Key::from_id(span.key)).map_err(|e| oracle(&e))?;
            if at.id() != span.address || at != record::address::<SpanTable>(Key::from_id(span.key), reads) {
                return Err(Broke::Oracle(format!("program {p}'s span table is not at the address its key and reads make")));
            }
            if root.payload.follow(record).map_err(|e| oracle(&e))? != &self.world.span_table(c, i)[..] {
                return Err(Broke::Oracle(format!("program {p}'s span table is not what a fresh build files")));
            }
            if record::result_digest(root, record).map_err(|e| oracle(&e))? != span.digest {
                return Err(Broke::Oracle(format!("program {p}'s span table is kept with another digest than its record's")));
            }
        }
        Ok(())
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
                let Some(result) = result else {
                    if entry.address != record::SILENT || entry.result != record::SILENT || entry.shape_len != 0 {
                        return Err(Broke::Oracle(format!("unit {c}'s silent item is filed with a record: not what a fresh build files")));
                    }
                    items.insert(item);
                    continue;
                };
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
        if items.len() != fresh.bodies.len() {
            return Err(Broke::Oracle(format!("unit {c}'s manifest names {} items of {}", items.len(), fresh.bodies.len())));
        }
        Ok(())
    }

    fn apply(&mut self, op: Op) {
        match op {
            Op::Body(c, i, v) => self.world.items[c][i].body = v,
            Op::Sig(c, i, v) => self.world.items[c][i].sig = v,
            Op::Layout(c, i, v) => self.world.items[c][i].layout = v,
            Op::Env(var, value) => self.world.env[var] = value,
            Op::Bodies(c, bodies) => self.world.bodies[c] = bodies,
            Op::Unreach(c, i) => self.world.unreached[c][i] = true,
            Op::Comment(c, f, k, n) => self.world.comments[c][f][k] = n,
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
            for i in (0..ITEMS).filter(|&i| self.world.asked(c) && !self.world.unreached[c][i]) {
                let key = self.world.body_key(c, i);
                let body = body_task(&self.world, c, i, key, false, 0, &mut |id: &ContentId| self.world.value(c, true, id))?;
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
    /// An item's layout: a surface row only plans read.
    Layout(usize, usize, u8),
    Env(usize, Option<u8>),
    /// Whether the request asks unit `c`'s bodies. The drawn ops toggle units 0 and 1, whose
    /// consts units 1 and 2 read: a maker demanded, and with both not asked, a maker that
    /// demands another; unit 0's is read by units 1 and 3 at once.
    Bodies(usize, Bodies),
    /// Item `i` of unit `c` becomes text the def map does not reach (its surface names it no
    /// more): set before a unit's first request, as a flip is not yet re-examined (driver doc,
    /// "Silent items").
    Unreach(usize, usize),
    /// `n` comment lines above item `k` of file `f` of unit `c`: its file's text moves, and the
    /// lines of the items below the comment, and nothing any item means.
    Comment(usize, usize, usize, u8),
    Request { kill: Option<u32> },
    /// A new process: the resident memo starts empty.
    Restart,
}

const REQUEST: Op = Op::Request { kill: None };

/// Runs `ops` from an empty store, each process with a memo from `make`, the host's schedule
/// drawn from `tape`: the outcomes of the requests that finished, and the simulation.
fn run<M: Memo>(ops: &[Op], tape: &Tape, plant: Plant, kills: bool, make: impl Fn() -> M) -> Result<(Vec<Outcome>, Sim), Broke> {
    run_with(ops, tape, plant, kills, false, make)
}

/// [`run`], every request also building the programs when `programs`.
fn run_with<M: Memo>(
    ops: &[Op],
    tape: &Tape,
    plant: Plant,
    kills: bool,
    programs: bool,
    make: impl Fn() -> M,
) -> Result<(Vec<Outcome>, Sim), Broke> {
    PLANT.set(plant);
    let result = (|| {
        let mut sim = Sim::new();
        sim.programs = programs;
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
    let c = tape.int_in(0..=(UNITS - 1) as u8, "unit") as usize;
    let i = tape.int_in(0..=3u8, "item") as usize;
    let v = tape.int_in(0..=3u8, "value");
    // A comment edit is drawn last, so a tape drawn before it was one replays as it did.
    match tape.int_in(0..=10u8, "op") {
        0 | 1 => Op::Body(c, i, v),
        2 | 3 => Op::Sig(c, i, v),
        4 => Op::Env(i % 2, (v > 0).then_some(v)),
        5 => Op::Restart,
        6 => Op::Request { kill: Some(u32::from(tape.int_in(0..=120u8, "kill at")) + 1) },
        7 => Op::Layout(c, i, v),
        9 => Op::Bodies(c.min(1), if v % 2 == 0 { Bodies::Asked } else { Bodies::NotAsked }),
        10 => Op::Comment(c, i / 2, i % 2, v),
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
///
/// The invariant: a run (an item with no shape to try, or whose candidate shapes all missed)
/// starts only when the request is quiet: no work is out but held runs, so no item of any unit
/// is still looking up and no demanded maker is unsettled with work out. The held runs then
/// start together as a wave, each reading the made facts as they stood when it started; when
/// the request is quiet again, what the wave made stale is redone in the next. So what runs,
/// what it reads and what is filed do not depend on the order answers arrive in, and a memo
/// that answers at once and the store that answers late agree. Controls:
/// `the_oracle_catches_a_run_before_its_units_lookups` (no hold),
/// `the_oracle_catches_a_cold_run_before_the_request_is_quiet` (a wave started while work is
/// out) and `a_wave_that_never_starts_ends_stalled` (the stall bound).
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

/// The invariant (in the simulation's oracle): each unit's late reads are answered from its
/// own table: units 0 and 1 read variable 0 under one id and see their own values. Control:
/// the host hands unit 1 unit 0's table, and unit 1's record is not what a fresh build files.
#[test]
fn the_oracle_catches_a_late_table_of_another_unit() {
    let ops = [REQUEST, Op::Env(0, Some(1)), REQUEST, Op::Env(0, Some(2)), REQUEST];
    caught(&ops, Plant::LateOfAnotherUnit, STALE, plain);
}

/// The invariant: a body that makes a fact it may not make (a row of its unit's surface) fails
/// the request by name, never has the row dropped. Control: the planted body is refused as
/// `NotMade`.
#[test]
fn the_driver_refuses_a_fact_a_body_may_not_make() {
    caught(&[REQUEST], Plant::BodyMakesSurfaceRow, &["NotMade"], plain);
}

/// The invariant (in the simulation's host): no body of a unit whose bodies are not asked is
/// run, and its manifest names none. Control: the driver keeps them, and the host sees one run.
#[test]
fn the_oracle_catches_a_body_of_a_unit_not_asked() {
    let ops = [Op::Bodies(1, Bodies::NotAsked), REQUEST, Op::Body(1, 0, 1), REQUEST, Op::Bodies(1, Bodies::Asked), REQUEST];
    caught(&ops, Plant::RunsUnaskedBodies, &["a body of a unit not asked was run"], plain);
}

/// The invariant: a keyed item its surface does not name (text only an unused arm reaches) gets
/// no body task, and its unit is checked clean with it filed silent, cold, after an edit of it
/// (its key moves, so it is examined again) and after an edit of a neighbour in its file.
/// Control: the driver asks a body of every keyed item, and the host is asked the one the
/// front end does not have.
#[test]
fn an_item_the_surface_does_not_name_gets_no_body_task() {
    let ops = [Op::Unreach(1, 1), REQUEST, Op::Body(1, 1, 1), REQUEST, Op::Body(1, 0, 1), REQUEST];
    caught(&ops, Plant::AsksUnnamedBodies, &["a body of an item its surface does not name was asked"], plain);
}

/// Demands across units not asked: unit 2's const reads unit 1's, which reads unit 0's, and
/// both are not asked, then the maker below is edited, then asked again.
fn demand_ops() -> [Op; 8] {
    [
        Op::Bodies(0, Bodies::NotAsked),
        Op::Bodies(1, Bodies::NotAsked),
        REQUEST,
        Op::Body(0, 3, 1),
        REQUEST,
        Op::Bodies(0, Bodies::Asked),
        Op::Sig(1, 3, 2),
        REQUEST,
    ]
}

/// The invariant: a reader of a fact a unit not asked makes is answered its maker's value, the
/// maker demanded (and a maker it reads, demanded in turn), and that unit's manifest files no
/// made row. Control: the driver demands no maker, and unit 2's const read unit 1's as
/// `absent`: not what a fresh build files.
#[test]
fn the_oracle_catches_a_demanded_makers_fact_answered_absent() {
    caught(&demand_ops(), Plant::DemandAnsweredAbsent, STALE, plain);
}

/// The invariant: going from asked to not asked files the made table a fresh build without
/// bodies files, empty, never the last manifest's rows (the stale rows of the failing tape:
/// `REQUEST`, unit 1 not asked, `REQUEST`). Control: the driver keeps the last rows of a maker
/// the unit no longer checks, a demanded maker's rows filed in its unit's manifest.
#[test]
fn the_oracle_catches_a_demanded_makers_rows_filed_in_its_unit() {
    let ops = [REQUEST, Op::Bodies(1, Bodies::NotAsked), REQUEST, Op::Body(1, 3, 1), REQUEST];
    caught(&ops, Plant::DemandedRowsFiled, &["made table is not what a fresh build files"], plain);
}

/// The refinement under demand: the resident memo builds what records-only builds, the
/// demanded makers found in it on the later requests.
#[test]
fn demanded_makers_refine_under_a_resident_memo() {
    for seed in 0..8 {
        refines(&demand_ops(), seed, Plant::None).unwrap_or_else(|why| panic!("seed {seed}: {why:?}"));
    }
}

/// Both memos over `ops`, on one schedule seed the tape draws.
fn refining(ops: &[Op], tape: &Tape, plant: Plant) -> Result<(), Broke> {
    refines(ops, tape.next_u64("schedule seed"), plant)
}

/// The invariant: an item whose lookups missed runs only once no item of its unit is looking
/// up, so the runs and their answers are the same whether the store answers late or a
/// resident memo at once. The tape is the one `resident_refines_records_only` shrank to: unit
/// 1 not asked (its const, demanded by unit 2, run and kept in the resident memo), then asked:
/// none of its items has a shape but the const's, whose record a lookup finds; item 2 reads
/// it. Control: a run starts while a lookup is in flight, item 2 runs before the const is
/// found under records-only (and is redone), after it under the resident memo.
#[test]
fn the_oracle_catches_a_run_before_its_units_lookups() {
    let ops = [Op::Bodies(1, Bodies::NotAsked), REQUEST, Op::Bodies(1, Bodies::Asked), REQUEST];
    caught(&ops, Plant::RunBeforeLookups, &["outcomes differ", "the stores differ"], refining);
}

/// The invariant: a cold run waits for the request to be quiet, so whether a maker it reads
/// was settled first does not depend on answer order. Two cases:
/// - Within a unit, the ops `resident_refines_records_only` shrank to at seed 12 (before unit
///   3 was added): units 0 and 1 not asked (unit 2's reader demands unit 1's const, which
///   demands unit 0's), then unit 1 asked: its const's record is found, its probe gated on
///   unit 0's const (maker rows read, then the demanded lookup); item 2 reads the const.
/// - Across units, the case the hold per unit left open: unit 0 not asked, its const read by
///   units 1 and 3, which are examined at once. Unit 1's const is edited and undone, so the
///   last request demands unit 0's const for it (its dictionary's row, its last shape); unit 3
///   was not asked the request before, so its dictionary names no made row and gates nothing,
///   and its const, edited to a body never built, is cold: it reads unit 0's const as `absent`
///   or its value by whether unit 1's demand settled first.
///
/// Control: the held runs start whenever nothing is ready, effects still out: under
/// records-only the reader runs before the maker settles (and is redone), under the resident
/// memo after.
#[test]
fn the_oracle_catches_a_cold_run_before_the_request_is_quiet() {
    let edit = Op::Body(0, 0, 0);
    let within = [
        edit,
        edit,
        edit,
        edit,
        Op::Bodies(1, Bodies::NotAsked),
        edit,
        edit,
        Op::Bodies(0, Bodies::NotAsked),
        REQUEST,
        Op::Bodies(1, Bodies::Asked),
        edit,
        REQUEST,
    ];
    let across = [
        Op::Bodies(0, Bodies::NotAsked),
        REQUEST,
        Op::Body(1, 3, 1),
        Op::Bodies(3, Bodies::NotAsked),
        REQUEST,
        Op::Body(1, 3, 0),
        Op::Bodies(3, Bodies::Asked),
        Op::Body(3, 3, 2),
        REQUEST,
    ];
    for ops in [&within[..], &across[..]] {
        caught(ops, Plant::ColdRunBeforeQuiet, &["outcomes differ", "the stores differ"], refining);
    }
}

/// The invariant: a driver that stops making progress ends with a typed outcome, never a hang.
/// Control: the wave never starts, so the first request's held runs wait forever; the driver's
/// stall bound ends it `Stalled`, which the oracle refuses by name.
#[test]
fn a_wave_that_never_starts_ends_stalled() {
    caught(&[REQUEST], Plant::WaveNeverStarts, &["the request ended Stalled"], plain);
}

fn programs(ops: &[Op], tape: &Tape, plant: Plant) -> Result<(), Broke> {
    run_with(ops, tape, plant, true, true, || RecordsOnly).map(|_| ())
}

/// The invariant: with both programs built, for any edits (bodies, signatures, layouts, late
/// reads, bodies not asked), restarts and kills in any order of the host's answers, under either
/// memo, every program's kept manifest a finished request names holds the rows a from-scratch
/// build makes (instance, item, plan address and result, code, callees), the store holds each
/// plan and code it names, every read it keeps is true now, and the linker's state is the image
/// of the fresh rows; and a request after an answer with no edit is certified.
#[test]
fn programs_build_what_a_fresh_build_builds() {
    blaze_sim::check(Config::cases(200), |tape| {
        let ops = ops(tape);
        match tape.coin("resident memo") {
            true => run_with(&ops, tape, Plant::None, true, true, || Resident::new(CAP)).map(|_| ()),
            false => programs(&ops, tape, Plant::None),
        }
    });
}

/// The case job link5 found at seed 26 (shrunk): a program request; an edit of item 1; a request
/// killed at step 43, after its units' manifests and marks were durable and before its kept
/// manifest and certificate; then a program request. The edited unit is settled by its mark
/// (nothing moved since the killed request's manifest), and its bodies were taken as standing
/// for the kept rows, which were answered against the manifest before the edit.
const KILLED_BEFORE_KEPT: &str = "1 0*4 1 0*4 1 0*4 1 0*4 1 0*3 8 1 0*4 1 0*4 1 0*4 1 0*4 1 0*4 1 0*4 1 0*4 1 0*4 1 0*4 1 0*4 1 0*4 1 0 1 1 0 1 0*3 6 43 1 0*4 1 0*4 1 0*4 1 0*3 8";

/// [`programs_build_what_a_fresh_build_builds`]'s case on the tape `text`, under `plant`.
fn programs_on(text: &str, plant: Plant) -> Result<(), Broke> {
    let values = blaze_sim::text::parse(text).map_err(|why| Broke::Harness(format!("not a tape: {why:?}")))?;
    let tape = Tape::replay(&values);
    let ops = ops(&tape);
    match tape.coin("resident memo") {
        true => run_with(&ops, &tape, plant, true, true, || Resident::new(CAP)).map(|_| ()),
        false => programs(&ops, &tape, plant),
    }
}

#[test]
fn a_body_edit_is_seen_after_a_request_killed_before_its_kept_manifest() {
    if let Err(why) = programs_on(KILLED_BEFORE_KEPT, Plant::None) {
        panic!("the regression tape failed: {why:?}");
    }
}

/// Control (rule 23, the cause removed): kept rows' body reads stand whenever the unit is
/// settled by its mark, whichever manifest they were answered against; the same tape fails.
#[test]
fn the_oracle_catches_kept_rows_trusting_a_mark_after_a_kill() {
    match programs_on(KILLED_BEFORE_KEPT, Plant::KeptRowsTrustUnitMarks) {
        Err(Broke::Oracle(why)) if STALE.iter().any(|says| why.contains(says)) => {}
        other => panic!("the planted bug was not found: {other:?}"),
    }
}

/// Control: a kept manifest is filed again when a unit manifest it names moved, though no row
/// did, so an edit no plan reads writes the program's manifest.
#[test]
fn the_oracle_catches_a_kept_manifest_filed_again_with_no_row_moved() {
    caught(&every_edit(), Plant::KeptRefiledForUnits, &["filed again with no row moved"], programs);
}

/// The last kill point [`kill_sweep`] tries: past the effects of a two-program request after a
/// body edit, so the last runs are not killed at all.
const SWEEP_TO: u32 = 300;

/// A program request; a body edit; a request killed at its `k`-th effect, for every `k` up to
/// [`SWEEP_TO`], each from an empty store; then a request. Every kill point between two durable
/// effects is met, the path oracle checked after each request.
fn kill_sweep(_: &[Op], tape: &Tape, plant: Plant) -> Result<(), Broke> {
    for k in 1..=SWEEP_TO {
        let ops = [REQUEST, Op::Body(0, 0, 1), Op::Request { kill: Some(k) }, REQUEST];
        run_with(&ops, tape, plant, true, true, || RecordsOnly)?;
    }
    Ok(())
}

/// The invariant at every kill point: after any request, killed or not, each program's path
/// names the image of a durable kept manifest, and after an answer, the image of the kept
/// manifest the answer certified.
#[test]
fn the_path_names_a_certified_image_at_every_kill_point() {
    if let Err(why) = kill_sweep(&[], &Tape::replay(&[]), Plant::None) {
        panic!("the kill sweep failed: {why:?}");
    }
}

/// Control: the path is placed as soon as the image is made, before its kept manifest is
/// durable; a kill between leaves it naming an image no manifest names.
#[test]
fn the_oracle_catches_a_place_before_the_kept_manifest() {
    caught(&[], Plant::PlacedBeforeKept, &["kept manifest is not durable"], kill_sweep);
}

/// Control: a certified answer places no path; after a request killed between its certificate
/// and its place, the next answer, certified, leaves the old image at the path.
#[test]
fn the_oracle_catches_a_certified_answer_that_does_not_place() {
    caught(&[], Plant::CertifiedNotPlaced, &["does not name the image of what the answer certified"], kill_sweep);
}

/// Control: the host names the image by the digest of its bytes, not by the kept manifest's key
/// it was given, so the kept manifest names an image the store does not hold.
#[test]
fn the_oracle_catches_an_image_named_by_its_content() {
    caught(&[REQUEST], Plant::ImageNamedByContent, &["names an image the store does not hold"], programs);
}

/// Control: a kept plan's reads of crates' facts are never answered again, so a plan whose
/// layout moved is found at its kept address, with the old layout in its reads.
#[test]
fn the_oracle_catches_a_plan_kept_after_a_layout_it_read_moved() {
    let ops = [REQUEST, Op::Layout(2, 0, 1), REQUEST, Op::Layout(0, 0, 2), REQUEST, Op::Layout(3, 0, 3), REQUEST];
    caught(&ops, Plant::PlanKeptAfterLayoutMoved, STALE, programs);
}

/// Control: code keyed by its instance, not by its plan's content, so a moved body's code is
/// found under the old key.
#[test]
fn the_oracle_catches_code_found_after_a_body_it_took_in_moved() {
    let ops = [REQUEST, Op::Body(2, 0, 1), REQUEST, Op::Body(3, 0, 2), REQUEST];
    caught(&ops, Plant::CodeKeyLeavesOutBody, STALE, programs);
}

/// Control: the link step is given the rows that came and went, not those whose code moved.
#[test]
fn the_oracle_catches_a_link_step_not_given_a_row_that_moved() {
    let ops = [REQUEST, Op::Body(2, 0, 1), REQUEST, Op::Body(3, 0, 2), REQUEST];
    caught(&ops, Plant::LinkMissesMovedRow, &["the link step was not given a row that moved"], programs);
}

/// Control: a body of a unit not asked, read by a kept plan, is never demanded again, so its
/// edit leaves the plan at its kept address.
#[test]
fn the_oracle_catches_a_demanded_body_not_rerun_when_its_text_moved() {
    let ops = [Op::Bodies(0, Bodies::NotAsked), REQUEST, Op::Body(0, 0, 1), REQUEST, Op::Body(0, 0, 2), REQUEST];
    caught(&ops, Plant::DemandedBodyNotRerun, STALE, programs);
}

/// Control: a body of a unit not asked, made for its MIR alone, is keyed by its item and not by
/// its text, so after its text moved a plan is served the record made before.
#[test]
fn the_oracle_catches_a_plan_served_a_body_made_before_its_text_moved() {
    let ops = [Op::Bodies(0, Bodies::NotAsked), REQUEST, Op::Body(0, 0, 1), REQUEST, Op::Body(0, 0, 2), REQUEST];
    caught(&ops, Plant::MirKeyLeavesOutText, STALE, programs);
}

/// One request with programs over a fresh store whose world `poison` sets: its outcome, the
/// driver's failures and program manifests in the simulation.
fn poisoned(tape: &Tape, poison: impl FnOnce(&mut World)) -> Result<(Option<Outcome>, Sim), Broke> {
    let mut sim = Sim::new();
    sim.programs = true;
    sim.requests = 1;
    poison(&mut sim.world);
    let plans = sim.world.plans(&sim.last);
    let programs = sim.program_plans();
    let outcome = proof::scope(|snap| sim.drive(snap, &mut RecordsOnly, tape, None, &plans, &programs))?;
    Ok((outcome, sim))
}

/// The keep-going cases. Two bodies of independent units fail: the request ends failed carrying
/// both, by unit. A plan of program 0 fails: the request ends failed carrying that one failure,
/// and program 1 files a kept manifest a fresh build makes.
fn keep_going(_: &[Op], tape: &Tape, plant: Plant) -> Result<(), Broke> {
    PLANT.set(plant);
    let result = (|| {
        let (outcome, sim) = poisoned(tape, |world| {
            world.poison[1][1] = true;
            world.poison[3][1] = true;
        })?;
        if !matches!(outcome, Some(Outcome::Failed { .. })) {
            return Err(Broke::Oracle(format!("a request with failing bodies ended {outcome:?}")));
        }
        let units: BTreeSet<u32> = sim
            .failures
            .iter()
            .filter_map(|why| match *why {
                Failure::Task { tag, unit, .. } if tag.0 == Body::TAG.0 => Some(unit),
                _ => None,
            })
            .collect();
        if units != BTreeSet::from([1, 3]) || sim.failures.len() != 2 {
            return Err(Broke::Oracle(format!("the request's end does not carry every failure: {:?}", sim.failures)));
        }
        let (outcome, sim) = poisoned(tape, |world| world.poison_plan[2][0] = true)?;
        if !matches!(outcome, Some(Outcome::Failed { .. })) {
            return Err(Broke::Oracle(format!("a request with a failing plan ended {outcome:?}")));
        }
        let plan_failed = matches!(sim.failures[..], [Failure::Task { tag, .. }] if tag.0 == PlanRecord::TAG.0);
        if !plan_failed {
            return Err(Broke::Oracle(format!("the request's end does not carry every failure: {:?}", sim.failures)));
        }
        match sim.program_keys[..] {
            [None, Some(key)] => sim.check_program(1, key),
            _ => Err(Broke::Oracle(format!("program 1 filed no kept manifest beside program 0's failure: {:?}", sim.program_keys))),
        }
    })();
    PLANT.set(Plant::None);
    result
}

/// The invariant: a failed task fails what depends on it and nothing else; every independent
/// task goes on, and the request's end carries every failure. Control: the first failure ends
/// the request.
#[test]
fn a_failure_stops_only_what_depends_on_it() {
    caught(&[REQUEST], Plant::FailureStopsAll, &["does not carry every failure"], keep_going);
}

/// The floor of a body edit: that body, the plans that read it (and any it newly reaches), their
/// code, and one link step, of the one program that reaches it.
#[test]
fn a_body_edit_redoes_its_plans_their_code_and_one_link() {
    let (_, mut sim) = run_with(&[REQUEST, Op::Body(3, 0, 1)], &Tape::seeded(7), Plant::None, false, true, || RecordsOnly).unwrap();
    let outcome = sim.request(&mut RecordsOnly, &Tape::seeded(11), None).unwrap().unwrap();
    let ran = built(&outcome);
    assert_eq!((ran.bodies, ran.links), (1, 1), "{ran:?}");
    assert!(ran.plans >= 1 && ran.codes == ran.plans, "{ran:?}");
}

/// The floor of a comment added above an item: its unit keyed again, the span task of each
/// owner in its file a program reaches, and one link step, of the one program that reaches it;
/// no surface, body, plan or code. A comment above unit 3's first item moves the lines of both
/// items of its file; unit 3 is program 1's root's and no item of program 0's.
fn comment_floor(_: &[Op], tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let ops = [REQUEST, Op::Comment(3, 0, 0, 2), REQUEST];
    let (outcomes, _) = run_with(&ops, tape, plant, false, true, || RecordsOnly)?;
    let ran = match outcomes.last() {
        Some(Outcome::Built { ran, filed: Filed::Certificate }) => *ran,
        other => return Err(Broke::Oracle(format!("a comment edit ended {other:?}"))),
    };
    if (ran.surfaces, ran.bodies, ran.plans, ran.codes) != (0, 0, 0, 0) {
        return Err(Broke::Oracle(format!("a comment edit ran a plan or code, a surface or a body: {ran:?}")));
    }
    if ran.spans == 0 || ran.links != 1 {
        return Err(Broke::Oracle(format!("a comment edit ran no span task, or not one link step: {ran:?}")));
    }
    Ok(())
}

/// The invariant: a comment edit costs its owners' span tasks and one link step, and nothing
/// of a plan or code. Control: code keyed by its item's line, so the edit reruns it.
#[test]
fn a_comment_edit_runs_span_tasks_and_one_link_and_no_plan_or_code() {
    caught(&[], Plant::CodeKeyedByPlace, &["a comment edit ran a plan or code"], comment_floor);
}

/// Comments added above items of program 1's root's unit, then of program 0's: what the span
/// controls run.
const COMMENTS: [Op; 5] = [REQUEST, Op::Comment(3, 0, 0, 2), REQUEST, Op::Comment(2, 0, 0, 1), REQUEST];

/// A position that is not a fresh build's: the oracle's words for a stale image.
const STALE_POSITION: &[&str] = &["a position a fresh build does not"];

/// Control: a kept span table stands without its file reads answered again, so after a comment
/// above its owner the image keeps the old line.
#[test]
fn the_oracle_catches_a_span_table_found_after_its_files_bases_moved() {
    caught(&COMMENTS, Plant::SpanFoundAfterBasesMoved, STALE_POSITION, programs);
}

/// Control: no link step runs when only span tables moved, so the image keeps the old line.
#[test]
fn the_oracle_catches_a_link_step_skipped_when_only_span_tables_moved() {
    caught(&COMMENTS, Plant::SpanOnlyLinkSkipped, STALE_POSITION, programs);
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
    // Units 1, 2 and 3 stand; unit 0's manifest is rebuilt.
    assert_eq!((ran.stood, ran.manifests, ran.marked), (3, 1, 0), "{ran:?}");
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
    // Unit 0, below the edit, unit 2, whose view is unchanged (unit 1's surface and made table
    // are equal), and unit 3, beside it, settle by their marks.
    assert_eq!(ran.marked, 3, "{ran:?}");
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
    assert_eq!((ran.marked, ran.surfaces, ran.bodies, ran.manifests), (UNITS as u32, 0, 0, 0), "{ran:?}");
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
