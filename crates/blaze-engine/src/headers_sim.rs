//! The producer header index across both drivers' commits (D187; headers.rs `Headers`; rule 43).
//! A history of edits to the engine simulation's world (sim.rs), each request answered over the
//! last committed answer read from a model store, page-backed (`Answer::from_pages`, the reader
//! here: pages and, when the index is current, its shards), then committed by a writer the tape
//! draws: the phase path (pages over the last root, `Headers::after` rewriting the shards its rows
//! sit on) or the `Ready` driver's commit (pages alone, the index left `Absent`). The commit puts
//! every page, then every shard, then flips the one pointer naming the root and the index; a crash
//! may come at any put or at the flip, and then nothing flips: the next request starts from the
//! last committed answer, its edit still on disk.
//!
//! The `Ready` driver's answer equals the phase driver's (ready_sim.rs R7), so the request is run
//! by the phase driver here and the writer drawn is only the commit's form: what this simulation
//! holds is the stored form, not the driver.
//!
//! The oracles, after every commit:
//! 1. Index equals pages: every name the committed answer or the last one held, and a few it never
//!    held, gets from the stored form the header and the planner row its own page holds (a current
//!    shard is a second index of the pages' rows; an absent index reads the page itself).
//! 2. Fresh: the committed answer, decoded whole, holds every row a fresh build of today's world
//!    holds, equal in all but the request that made it.
//! 3. No shard under Absent: a request over an absent index asks no shard, and a ready commit
//!    writes none (the bar's "0 header shards read or written").
//!
//! The control plants [`HeadersPlant::ReadyKeepsShards`]: a ready commit that names the last
//! shards as current. A later lookup then reads a shard older than the pages, and oracle 1 says
//! so ("differs from its page"). The planted bug the brief first named, the phase path reading a
//! shard under `Absent`, cannot be written: `Absent` holds no key (rule 76).
//!
//! [`a_ready_edit_reads_no_shard_and_pages_its_names_alone`] is the cost claim: one body edit in a
//! 938-item crate, committed by the ready writer, and the next edit read over the absent index,
//! touches only the pages of the names the request is about, and no shard.
//!
//! The engine's last answer must live for `'static` (sim.rs `request` folds into an
//! `Answer<'static>`), so each request's snapshot of the store and its reader are leaked: a test's
//! memory, a few kilobytes a request.

use core::sync::atomic::{AtomicU64, Ordering};
use std::boxed::Box;
use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::println;
use std::string::String;
use std::sync::Mutex;
use std::vec::Vec as StdVec;

use blaze_drive::answers::page_of;
use blaze_drive::cells::Cell;
use blaze_drive::key::{Key, Kind, Name};
use blaze_id::ContentId;
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::StoredPages;
use crate::engine::{Answer, CellId, Gen, Phase, Plant, Planned, Row};
use crate::last::LastError;
use crate::pages::{
    Header, Headers, HeadersPlant, IndexedHeaders, Kept, PagesError, ROOT_PAGE, Root, Sealed, VerifiedPage, Writer, decode, decode_root,
    encode, encode_changes,
};
use crate::sim::{Crates, Item, Pools, World, alloc, edit, first_world, oracle, request};

/// The model store: every record put, by key (content-addressed: an orphan of a crashed write is
/// harmless), and the one pointer, flipped last.
#[derive(Default)]
struct Store {
    records: BTreeMap<Key, StdVec<u8>>,
    pointer: Option<Pointer>,
}

/// What the pointer names: the answer's root and its header index.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Pointer {
    root: Key,
    headers: Headers,
}

/// The stored answer as a request reads it: the root, every page opened (hashed: no store proves
/// them here), the shards when the index is current, and what was asked of them.
struct Reader<'b> {
    root: Root<'b>,
    headers: Headers,
    pages: StdVec<Option<VerifiedPage<'b>>>,
    /// Each slot's shard; empty when the index is absent.
    shards: StdVec<Option<IndexedHeaders<'b>>>,
    /// The pages a lookup reached, by index.
    touched: Mutex<BTreeSet<u32>>,
    /// Every ask of a shard, answered or refused.
    shard_reads: AtomicU64,
}

fn harness(what: &str, error: PagesError) -> Broke {
    Broke::Harness(format!("{what}: {error}"))
}

impl<'b> Reader<'b> {
    fn open(records: &'b BTreeMap<Key, StdVec<u8>>, pointer: &Pointer) -> Result<Reader<'b>, Broke> {
        let sealed = |key: Key, page: u32| -> Result<Sealed<'b>, Broke> {
            let bytes = records.get(&key).ok_or_else(|| Broke::Oracle(format!("the pointer reaches {key:?} (page {page}), which the store does not hold")))?;
            Sealed::hashed(key, bytes).map_err(|torn| harness("a stored record", torn.at(page)))
        };
        let root = decode_root(sealed(pointer.root, ROOT_PAGE)?, alloc()).map_err(|error| harness("the root", error))?;
        let mut pages = StdVec::with_capacity(root.pages.len());
        for (index, &key) in root.pages.iter().enumerate() {
            pages.push(match key == Key::default() {
                true => None,
                false => Some(VerifiedPage::open_whole(sealed(key, index as u32)?, index as u32, root.bits).map_err(|error| harness("a page", error))?),
            });
        }
        let mut shards = StdVec::new();
        if let Headers::Current(keys) = &pointer.headers {
            for (index, &key) in keys.keys.iter().enumerate() {
                shards.push(match key == Key::default() {
                    true => None,
                    false => Some(IndexedHeaders::open_whole(sealed(key, index as u32)?, index).map_err(|error| harness("a shard", error))?),
                });
            }
        }
        Ok(Reader { root, headers: pointer.headers.clone(), pages, shards, touched: Mutex::new(BTreeSet::new()), shard_reads: AtomicU64::new(0) })
    }

    fn page_at(&self, index: usize) -> Result<Option<&VerifiedPage<'b>>, PagesError> {
        self.touched.lock().expect("no reader panics holding it").insert(index as u32);
        Ok(self.pages.get(index).and_then(|page| page.as_ref()))
    }

    fn shard(&self, index: usize) -> Result<Option<&IndexedHeaders<'b>>, PagesError> {
        self.shard_reads.fetch_add(1, Ordering::Relaxed);
        match &self.headers {
            Headers::Current(_) => Ok(self.shards.get(index).and_then(|shard| shard.as_ref())),
            Headers::Absent { since } => Err(PagesError::HeadersAbsent { page: index as u32, since: *since }),
        }
    }

    fn planner_row(&self, name: Name) -> Result<Option<Row<'static>>, PagesError> {
        self.headers.planner_row(name, |index| self.shard(index), || self.page(name), alloc())
    }

    /// The answer the engine starts from: the pages, and the root's sparse columns (blaze-cli
    /// engine_answer.rs `Stored::answer`).
    fn answer(&'b self) -> Answer<'b> {
        let mut answer = Answer::from_pages(self, self.root.serial, alloc());
        answer.owed_tasks.extend(self.root.owed_tasks.iter().map(|(&name, &plan)| (name, plan)));
        answer.owed_cells.extend(self.root.owed_cells.iter().map(|(&id, &at)| (id, at)));
        answer.with_findings.extend(self.root.findings.keys().copied());
        answer
    }

    fn touched(&self) -> BTreeSet<u32> {
        self.touched.lock().expect("no reader panics holding it").clone()
    }
}

impl StoredPages for Reader<'_> {
    fn page(&self, name: Name) -> Result<Option<&VerifiedPage<'_>>, PagesError> {
        self.page_at(page_of(name, self.root.bits) as usize)
    }

    fn header(&self, name: Name) -> Result<Option<Header>, PagesError> {
        self.headers.header(name, |index| self.shard(index), || self.page(name))
    }

    fn finding(&self, name: Name) -> Option<u32> {
        self.root.findings.get(&name).copied()
    }

    fn visit_rows(&self, visit: &mut dyn FnMut(Name) -> Result<(), LastError>) -> Result<(), LastError> {
        for page in self.pages.iter().flatten() {
            for name in page.row_names() {
                visit(name)?;
            }
        }
        Ok(())
    }

    fn refusal(&self) -> Option<PagesError> {
        None
    }
}

/// A row as oracle 2 compares it: everything but the request that made it.
type Seen = (Key, Phase, Name, ContentId, ContentId, StdVec<(Cell, ContentId)>, StdVec<(CellId, ContentId)>, u32, bool, bool);

fn seen(row: &Row<'_>) -> Seen {
    (row.key, row.phase, row.file, row.source, row.out, row.defines.to_vec(), row.reads.to_vec(), row.findings, row.inline, row.failed)
}

/// What one request did, for the oracles and the cost fixture.
struct Step {
    /// The writer of the commit, or `None` when a crash cut it before the flip.
    committed: Option<Writer>,
    /// Shard records the commit put.
    shard_puts: usize,
    /// What the request's reader was asked: the pages reached, shard asks, and the index it read.
    touched: BTreeSet<u32>,
    shard_reads: u64,
    read_headers: Option<Headers>,
    /// The pages of the root the request read, and its paging bits.
    pages: usize,
    bits: u8,
    entered: StdVec<Name>,
}

/// A history: today's world, the store, and the plans of the last committed answer.
struct History {
    world: World,
    store: Store,
    last_plans: BTreeMap<Name, StdVec<Planned>>,
    plant: HeadersPlant,
}

impl History {
    fn new(world: World, plant: HeadersPlant) -> History {
        History { world, store: Store::default(), last_plans: BTreeMap::new(), plant }
    }

    fn scope(&self) -> Crates {
        Crates::of(&self.world, (0..self.world.crates.len()).collect())
    }

    /// One request over the last committed answer, committed by `writer` unless a crash at put
    /// `crash` (counted over the pages, then the shards, then the flip) cuts it.
    fn request(&mut self, serial: u64, writer: Writer, crash: Option<usize>) -> Result<Step, Broke> {
        let plans = self.world.plans();
        let changed: BTreeMap<Name, StdVec<Planned>> =
            plans.iter().filter(|(name, plan)| self.last_plans.get(*name) != Some(*plan)).map(|(name, plan)| (*name, plan.clone())).collect();
        // The snapshot the request reads, for `'static` (the module docs).
        let reader: Option<&'static Reader<'static>> = match &self.store.pointer {
            Some(pointer) => {
                let records: &'static BTreeMap<Key, StdVec<u8>> = Box::leak(Box::new(self.store.records.clone()));
                let leaked: &'static Reader<'static> = Box::leak(Box::new(Reader::open(records, pointer)?));
                Some(leaked)
            }
            None => None,
        };
        let last = match reader {
            Some(reader) => reader.answer(),
            None => Answer::new_in(alloc()),
        };
        let ours = request(&self.world, last, &changed, self.scope(), Plant::None, Pools::One)
            .map_err(|error| Broke::Oracle(format!("request {serial}: the engine over the stored answer failed: {error}")))?;
        let entered: StdVec<Name> = ours.entered.iter().copied().collect();
        let kept = Kept::after(crate::SMap::new_in(alloc()), ours.answer, &ours.causes, crate::SMap::new_in(alloc()));
        let encoded = match reader {
            Some(reader) => encode_changes(&kept, &reader.root, alloc(), |k| self.store.records.get(&k).map(|b| Sealed::hashed(k, b)))
                .map_err(|error| Broke::Oracle(format!("request {serial}: the pages' commit: {error}")))?,
            None => encode(&kept, None, alloc()),
        };
        let (headers, shard_puts) = Headers::after(
            reader.map(|reader| &reader.headers),
            &kept,
            writer,
            kept.answer.serial,
            self.plant,
            |index| match reader {
                Some(reader) => reader.shard(index),
                None => Ok(None),
            },
            alloc(),
        )
        .map_err(|error| Broke::Oracle(format!("request {serial}: the header index's commit: {error}")))?;
        let step = |committed| Step {
            committed,
            shard_puts: shard_puts.len(),
            touched: reader.map(|reader| reader.touched()).unwrap_or_default(),
            shard_reads: reader.map_or(0, |reader| reader.shard_reads.load(Ordering::Relaxed)),
            read_headers: reader.map(|reader| reader.headers.clone()),
            pages: reader.map_or(0, |reader| reader.pages.len()),
            bits: reader.map_or(0, |reader| reader.root.bits),
            entered: entered.clone(),
        };
        // The durable order: every page and the root, then every shard, then the flip.
        let mut at = 0usize;
        for (key, bytes) in encoded.puts.iter().chain(shard_puts.iter()) {
            if crash == Some(at) {
                return Ok(step(None));
            }
            self.store.records.insert(*key, bytes.iter().copied().collect());
            at += 1;
        }
        if crash == Some(at) {
            return Ok(step(None));
        }
        self.store.pointer = Some(Pointer { root: encoded.root, headers });
        self.last_plans = plans;
        Ok(step(Some(writer)))
    }

    /// Oracles 1 and 2 over the committed answer (the module docs). `before`: the names the last
    /// committed answer held.
    fn held(&self, serial: u64, before: &BTreeSet<Name>) -> Result<BTreeSet<Name>, Broke> {
        let pointer = self.store.pointer.as_ref().ok_or_else(|| Broke::Harness(String::from("no commit to hold")))?;
        let records = &self.store.records;
        let decoded = decode(pointer.root, |key| records.get(&key).map(|bytes| Sealed::hashed(key, bytes)), alloc())
            .map_err(|error| Broke::Oracle(format!("request {serial}: the committed answer does not read back: {error}")))?;
        let now: BTreeSet<Name> = decoded.answer.rows.keys().copied().collect();
        // Oracle 2: a fresh build of today's world, every row equal.
        let fresh = request(&self.world, Answer::new_in(alloc()), &self.world.plans(), self.scope(), Plant::None, Pools::One)
            .map_err(|error| Broke::Harness(format!("a fresh build failed: {error}")))?;
        for (name, row) in fresh.answer.rows.iter() {
            let ours = decoded.answer.rows.get(name).map(seen);
            oracle(ours.as_ref() == Some(&seen(row)), || format!("request {serial}: {name:?} differs from a fresh build: stored {ours:?}, fresh {:?}", seen(row)))?;
        }
        // Oracle 1: the index against the pages, for every name either answer held and a few
        // neither did (a lookup of nothing is nothing either way).
        let reader = Reader::open(records, pointer)?;
        let strangers = (0..3u64).map(|i| Name::new(Kind::Units, ContentId::of("headers sim stranger", &i.to_le_bytes())));
        for name in now.iter().chain(before.iter()).copied().chain(strangers) {
            let page = reader.page(name).map_err(|error| harness("a page", error))?;
            let want = page.and_then(|page| Header::from_page(page, name));
            let got = reader.header(name).map_err(|error| Broke::Oracle(format!("request {serial}: {name:?}'s header under {:?}: {error}", reader.headers)))?;
            oracle(got == want, || format!("request {serial}: {name:?}'s header under {:?} differs from its page: index {got:?}, page {want:?}", reader.headers))?;
            let want_row = page.and_then(|page| page.row(name)).filter(|_| full(name)).map(|row| row.to_owned(alloc()));
            let got_row = reader.planner_row(name).map_err(|error| Broke::Oracle(format!("request {serial}: {name:?}'s planner row: {error}")))?;
            oracle(got_row == want_row, || format!("request {serial}: {name:?}'s planner row under {:?} differs from its page", reader.headers))?;
        }
        if let Headers::Absent { .. } = reader.headers {
            let asked = reader.shard_reads.load(Ordering::Relaxed);
            oracle(asked == 0, || format!("request {serial}: {asked} shard asks of an absent index"))?;
        }
        Ok(now)
    }
}

/// The kinds whose shard carries the full row (headers.rs `full`).
fn full(name: Name) -> bool {
    matches!(name.kind, Kind::Units | Kind::UnitGraph | Kind::Fetch | Kind::Script)
}

fn histories(tape: &Tape, plant: HeadersPlant) -> Result<(), Broke> {
    let mut history = History::new(first_world(tape), plant);
    let mut before: BTreeSet<Name> = BTreeSet::new();
    for serial in 0..2 + tape.index(9, "requests") as u64 {
        if serial > 0 {
            history.world = edit(tape, &history.world);
        }
        let writer = match tape.one_in(2, "the ready driver commits") {
            true => Writer::Ready,
            false => Writer::Phases,
        };
        // A crash at a put or at the flip; a point past the flip is no crash.
        let crash = tape.one_in(4, "a crash").then(|| tape.index(64, "the put the crash cuts"));
        let absent_before = matches!(history.store.pointer.as_ref().map(|pointer| &pointer.headers), Some(Headers::Absent { .. }));
        let step = history.request(serial, writer, crash)?;
        // Oracle 3: an absent index is asked no shard, and a ready commit puts none.
        if absent_before {
            oracle(step.shard_reads == 0, || format!("request {serial}: {} shard asks over an absent index", step.shard_reads))?;
        }
        if step.committed == Some(Writer::Ready) && step.read_headers.is_some() {
            oracle(step.shard_puts == 0, || format!("request {serial}: a ready commit put {} shards", step.shard_puts))?;
        }
        if step.committed.is_some() {
            let after = &history.store.pointer.as_ref().expect("just committed").headers;
            // The index's states: a cold write is current; a ready commit over any index leaves
            // it absent since its first such request; a phase commit keeps an absent one absent.
            let expected_absent = match (&step.read_headers, step.committed) {
                (None, _) => false,
                (Some(Headers::Absent { .. }), _) => true,
                (Some(Headers::Current(_)), Some(Writer::Ready)) => plant == HeadersPlant::None,
                (Some(Headers::Current(_)), _) => false,
            };
            oracle(matches!(after, Headers::Absent { .. }) == expected_absent, || format!("request {serial}: the index after the commit is {after:?}"))?;
            before = history.held(serial, &before)?;
        }
    }
    Ok(())
}

/// The refinement: across both phase and ready commits and crashes, the producer header index answers identically to reading the pages directly and never serves stale shards.
/// Expected: "test headers_sim::the_header_index_answers_as_the_pages_across_both_writers ... ok".
#[test]
fn the_header_index_answers_as_the_pages_across_both_writers() {
    check(SimConfig::cases(300), |tape| histories(tape, HeadersPlant::None));
}

/// The planted bug: a ready commit names the last shards as current. A later lookup reads a shard
/// older than the pages, and oracle 1 says so.
#[test]
fn the_oracle_catches_a_ready_commit_that_keeps_the_shards() {
    control(SimConfig::cases(300), HeadersPlant::None, HeadersPlant::ReadyKeepsShards, "differs from its page", histories);
}

/// One crate of 134 files, seven items each (sig `id % 4`: no derive; body 1; no references):
/// cold (current index), then a body edit committed by the ready writer (the index absent from
/// there), then another body edit read over the absent index. That read asks no shard, writes
/// none, and reaches only the pages of the names the request is about: the changed file's
/// planned tasks and the file itself, the tasks entered, the definers of those names' reads, and
/// the readers of what the entered tasks define, in the last answer and today's.
/// Expected: "pages touched T of P, all within the B pages of the request's names; 0 shard asks".
#[test]
fn a_ready_edit_reads_no_shard_and_pages_its_names_alone() {
    let mut next = 0u32;
    let mut files: StdVec<(usize, StdVec<Item>)> = StdVec::new();
    for _ in 0..134 {
        let mut items = StdVec::new();
        for _ in 0..7 {
            items.push(Item { id: next, sig: next % 4, body: 1, refs: StdVec::new() });
            next += 1;
        }
        files.push((0, items));
    }
    let world = World { crates: StdVec::from([StdVec::new()]), files, next_id: next };
    let mut history = History::new(world, HeadersPlant::None);
    let cold = history.request(0, Writer::Phases, None).expect("the cold request");
    assert_eq!(cold.committed, Some(Writer::Phases));
    let before = history.held(0, &BTreeSet::new()).expect("the cold answer holds");
    history.world.files[40].1[3].body = 2;
    let ready = history.request(1, Writer::Ready, None).expect("the ready edit");
    assert_eq!(ready.shard_puts, 0, "a ready commit writes no shard");
    let before = history.held(1, &before).expect("the ready commit holds");
    assert!(matches!(history.store.pointer.as_ref().map(|pointer| &pointer.headers), Some(Headers::Absent { since: Gen(_) })));
    // The last answer, whole, for the bound.
    let records = &history.store.records;
    let last_root = history.store.pointer.as_ref().expect("committed").root;
    let last = decode(last_root, |key| records.get(&key).map(|bytes| Sealed::hashed(key, bytes)), alloc()).expect("the last answer reads");
    let last_plans = history.last_plans.clone();
    history.world.files[90].1[5].body = 3;
    let plans = history.world.plans();
    let step = history.request(2, Writer::Ready, None).expect("the edit over the absent index");
    assert_eq!(step.shard_reads, 0, "an absent index is asked no shard");
    assert_eq!(step.shard_puts, 0, "a ready commit writes no shard");
    history.held(2, &before).expect("the second ready commit holds");
    let fresh = request(&history.world, Answer::new_in(alloc()), &plans, history.scope(), Plant::None, Pools::One).expect("a fresh build");
    // The request's names, and the bound's pages.
    let mut names: BTreeSet<Name> = BTreeSet::new();
    for (file, plan) in plans.iter().filter(|(file, plan)| last_plans.get(*file) != Some(*plan)) {
        names.insert(*file);
        names.extend(plan.iter().map(|planned| planned.name));
        names.extend(last_plans.get(file).into_iter().flatten().map(|planned| planned.name));
    }
    names.extend(step.entered.iter().copied());
    let mut bound: BTreeSet<Name> = names.clone();
    for answer in [&last.answer, &fresh.answer] {
        for name in names.iter() {
            if let Some(row) = answer.rows.get(name) {
                // A read's definer, and the page of that cell's readers (pages.rs
                // `readers_slot`): a reader joining or leaving a list rewrites that page.
                bound.extend(row.reads.iter().map(|(id, _)| id.of));
                bound.extend(row.reads.iter().map(|(id, _)| crate::pages::readers_slot(id)));
                bound.insert(row.file);
                for (cell, _) in row.defines.iter() {
                    let id = CellId { of: *name, cell: *cell };
                    bound.insert(crate::pages::readers_slot(&id));
                    if let Some(readers) = answer.readers.get(&id) {
                        bound.extend(readers.iter().copied());
                    }
                }
            }
        }
    }
    // Paged as the root the request read was.
    let pages: BTreeSet<u32> = bound.iter().map(|name| page_of(*name, step.bits)).collect();
    let beyond: StdVec<u32> = step.touched.difference(&pages).copied().collect();
    println!(
        "pages touched {} of {}, all within the {} pages of the request's names ({} names); {} shard asks",
        step.touched.len(),
        step.pages,
        pages.len(),
        bound.len(),
        step.shard_reads
    );
    assert!(step.pages >= 4, "the fixture pages by several pages: {}", step.pages);
    assert!(beyond.is_empty(), "pages {beyond:?} touched beyond the pages of the request's names {pages:?}");
}
