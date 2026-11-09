//! A deterministic simulation test of a page store on reads into and writes from memory
//! ([`Effect::ReadInto`], [`Effect::WriteFrom`]): the shape the store's pool takes
//! (docs/v2/plan/design/store-pool.md, section 7), cut down to what proves the simulation's new
//! semantics catch the pool's bugs, and the scaffolding the pool's own DST grows from.
//!
//! The core ([`Pages`], sans-IO): pages of one file, each with a slot in a registered region at
//! a fixed address and a state word (unloaded, loading, loaded, corrupt) with its pins and
//! version. A put writes a page into its slot and commits it: the slot pinned, written to the
//! file from memory, the file synced, then the head (the count committed and each page's CRC32C)
//! written whole. A read of a loaded page copies it out; of an unloaded one, loads it with a read
//! into its slot and checks its CRC before it is loaded. The evictor poisons an unpinned loaded
//! slot with [`POISON`], as the pool's `os::evict` stand-in does. A crash loses the process's
//! memory (every slot poisoned) and the open reads the head again.
//!
//! The oracle, against a model of what was put: every read that hands back bytes hands back the
//! page's; a page is refused as corrupt only where the disk rotted under it; after every crash the
//! open finds exactly the pages whose head write completed; at the end every load has ended, and
//! every page reads back. Five planted bugs ([`Plant`]), each a safeguard of the pool's removed,
//! must each be caught on one of [`CONTROL_SEEDS`] seeded plans: a page loaded as its read is
//! asked (the pool's plant 4), a failed load kept (6), no block check on load (5), the evictor
//! ignoring the commit's pin (8), the head written before the data's sync (7).
//!
//! Replay: a failure names its plan, the simulator's seed in it; a control names a seed of
//! [`plan_of`] for [`REPLAY`].

use std::collections::{BTreeMap, BTreeSet};

use hegel::{TestCase, generators as gs};

use super::{Faults, Rng, SCRIBBLE, Sim};
use crate::{
    Cache, Completion, DIRECT_ALIGN, Done, Driver, Effect, FileName, Memory, Priority, Tag,
    wait::Deadline,
};

/// A page: a block, so its reads and writes could bypass the cache.
const PAGE: usize = DIRECT_ALIGN as usize;
/// Pages of the file, each a slot of the memory.
const PAGES: usize = 8;
/// What an evicted slot holds: the pool's DST poisons what it gives back with this.
const POISON: u8 = 0xDB;
/// What a put's bytes are below, so none is [`POISON`], [`SCRIBBLE`] or what rot writes.
const CONTENT: u8 = 0x40;
const ROT: u8 = 0xFF;
/// Most acts a plan draws.
const MOST_ACTS: u64 = 80;
/// Polls the end of a run may take to let everything in flight end (bounded-progress).
const DRAIN: u32 = 10_000;
/// Seeded plans a control searches for one its plant breaks.
const CONTROL_SEEDS: u64 = 256;
/// The seed [`replay_one_seed`] runs, and its plant: set them to what a failure names.
const REPLAY: (u64, Plant) = (0, Plant::None);

fn data() -> FileName {
    FileName::new("pages/data").expect("a name")
}

fn head() -> FileName {
    FileName::new("pages/head").expect("a name")
}

const _: () = assert!(CONTENT < SCRIBBLE && CONTENT < POISON && CONTENT < ROT);

// --- The core -------------------------------------------------------------------------------

/// A safeguard of the core's removed, for a control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// A page is loaded as its read is asked, not as it completes (the pool's plant 4).
    LoadedAtAsk,
    /// A failed load leaves its page loaded (the pool's plant 6).
    FailedLoadKept,
    /// A load does not check its page's CRC (the pool's plant 5).
    NoCheck,
    /// The evictor takes a page the commit has pinned (the pool's plant 8).
    EvictPinned,
    /// The head is written once the data's write completes, before its sync (the pool's plant
    /// 7).
    HeadBeforeSync,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Unloaded,
    Loading,
    Loaded,
    Corrupt,
}

#[derive(Clone, Copy, Debug)]
struct Word {
    state: State,
    pins: u32,
    /// Raised by every eviction: an optimistic copy checks it.
    version: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Opening {
    ReadingHead,
    Cutting,
    Open,
}

#[derive(Clone, Copy, Debug)]
enum Asked {
    Head,
    Cut,
    Write,
    Sync,
    Flip,
    Load(usize),
}

#[derive(Clone, Copy, Debug)]
struct Commit {
    page: usize,
    crc: u32,
}

/// What a read found.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Bytes(Vec<u8>),
    /// Its load is in flight, or was just asked.
    Waiting,
    Corrupt,
    /// Not committed.
    Absent,
}

struct Pages {
    plant: Plant,
    words: Vec<Word>,
    opening: Opening,
    committed: usize,
    crcs: Vec<u32>,
    commit: Option<Commit>,
    asked: BTreeMap<u64, Asked>,
    next: u64,
}

impl Pages {
    /// A process's open of the store, its memory fresh (poisoned): asks for the head.
    fn open(plant: Plant, memory: &mut Memory, effects: &mut Vec<Effect>) -> Pages {
        memory.fill(0, memory.len(), POISON);
        let word = Word { state: State::Unloaded, pins: 0, version: 0 };
        let mut pages = Pages {
            plant,
            words: vec![word; PAGES],
            opening: Opening::ReadingHead,
            committed: 0,
            crcs: Vec::new(),
            commit: None,
            asked: BTreeMap::new(),
            next: 0,
        };
        pages.ask(Asked::Head, effects, memory);
        pages
    }

    fn ask(&mut self, asked: Asked, effects: &mut Vec<Effect>, memory: &Memory) {
        let tag = Tag(self.next);
        self.next += 1;
        self.asked.insert(tag.0, asked);
        let span = |page: usize| memory.span((page * PAGE) as u64, PAGE as u32);
        let at = |page: usize| (page * PAGE) as u64;
        effects.push(match asked {
            Asked::Head => Effect::ReadAll { file: head(), tag },
            Asked::Cut => Effect::Truncate { file: data(), len: at(self.committed), tag },
            Asked::Write => {
                let page = self.commit.expect("a commit in flight").page;
                Effect::WriteFrom { file: data(), at: at(page), from: span(page), cache: Cache::Kept, tag }
            }
            Asked::Sync => Effect::Sync { file: data(), tag },
            Asked::Flip => {
                let commit = self.commit.expect("a commit in flight");
                let mut bytes = ((self.committed + 1) as u32).to_le_bytes().to_vec();
                for crc in self.crcs.iter().chain([&commit.crc]) {
                    bytes.extend_from_slice(&crc.to_le_bytes());
                }
                Effect::Write { file: head(), bytes, tag }
            }
            Asked::Load(page) => Effect::ReadInto {
                file: data(),
                at: at(page),
                into: span(page),
                cache: Cache::Kept,
                priority: Priority::Demand,
                tag,
            },
        });
    }

    /// `bytes` (a page) put as the next page and committed; `false` where the store is not
    /// open, a commit is in flight, or the file is full.
    fn put(&mut self, bytes: &[u8], memory: &mut Memory, effects: &mut Vec<Effect>) -> bool {
        if self.opening != Opening::Open || self.commit.is_some() || self.committed == PAGES {
            return false;
        }
        let page = self.committed;
        memory.write(page * PAGE, bytes);
        let word = &mut self.words[page];
        // What was just made stays loaded, pinned by its commit.
        word.state = State::Loaded;
        word.pins += 1;
        self.commit = Some(Commit { page, crc: blaze_ids::crc32c::crc32c(bytes) });
        self.ask(Asked::Write, effects, memory);
        true
    }

    fn read(&mut self, page: usize, memory: &Memory, effects: &mut Vec<Effect>) -> Outcome {
        if self.opening != Opening::Open || page >= self.committed {
            return Outcome::Absent;
        }
        let word = self.words[page];
        match word.state {
            State::Loaded => {
                let mut bytes = vec![0; PAGE];
                memory.read(page * PAGE, &mut bytes);
                // The optimistic copy's check: still loaded, the same version.
                let now = self.words[page];
                match now.state == State::Loaded && now.version == word.version {
                    true => Outcome::Bytes(bytes),
                    false => Outcome::Waiting,
                }
            }
            State::Loading => Outcome::Waiting,
            State::Corrupt => Outcome::Corrupt,
            State::Unloaded => {
                self.words[page].state = match self.plant {
                    Plant::LoadedAtAsk => State::Loaded,
                    _ => State::Loading,
                };
                self.ask(Asked::Load(page), effects, memory);
                Outcome::Waiting
            }
        }
    }

    /// The slot of `page` given back, poisoned; `false` where it is not loaded or is pinned.
    fn evict(&mut self, page: usize, memory: &mut Memory) -> bool {
        let word = &mut self.words[page];
        let pinned = word.pins > 0 && self.plant != Plant::EvictPinned;
        if word.state != State::Loaded || pinned {
            return false;
        }
        word.state = State::Unloaded;
        word.version += 1;
        memory.fill(page * PAGE, PAGE, POISON);
        true
    }

    fn complete(&mut self, completion: Completion, memory: &mut Memory, effects: &mut Vec<Effect>) {
        let Some(asked) = self.asked.remove(&completion.tag.0) else {
            panic!("a completion of no ask: {completion:?}");
        };
        let failed = matches!(completion.done, Done::Failed(_));
        match asked {
            // Each failed step of the open or the commit is asked again.
            Asked::Head | Asked::Cut | Asked::Write | Asked::Flip if failed => {
                self.ask(asked, effects, memory)
            }
            // A failed sync says nothing of what it covered: the write is made again.
            Asked::Sync if failed => self.ask(Asked::Write, effects, memory),
            Asked::Head => {
                let Done::Read(bytes) = completion.done else { panic!("{completion:?}") };
                if !bytes.is_empty() {
                    let count = u32::from_le_bytes(bytes[..4].try_into().expect("4 bytes"));
                    self.committed = count as usize;
                    self.crcs = bytes[4..]
                        .chunks(4)
                        .map(|it| u32::from_le_bytes(it.try_into().expect("4 bytes")))
                        .collect();
                    assert_eq!(self.crcs.len(), self.committed, "a head is written whole");
                }
                self.opening = Opening::Cutting;
                self.ask(Asked::Cut, effects, memory);
            }
            Asked::Cut => self.opening = Opening::Open,
            Asked::Write => match self.plant {
                Plant::HeadBeforeSync => self.ask(Asked::Flip, effects, memory),
                _ => self.ask(Asked::Sync, effects, memory),
            },
            Asked::Sync => self.ask(Asked::Flip, effects, memory),
            Asked::Flip => {
                let commit = self.commit.take().expect("a commit in flight");
                self.committed += 1;
                self.crcs.push(commit.crc);
                self.words[commit.page].pins -= 1;
            }
            Asked::Load(page) => {
                let word = &mut self.words[page];
                word.state = match completion.done {
                    Done::ReadInto { len } if len as usize == PAGE => {
                        let mut bytes = vec![0; PAGE];
                        memory.read(page * PAGE, &mut bytes);
                        let good = blaze_ids::crc32c::crc32c(&bytes) == self.crcs[page];
                        match good || self.plant == Plant::NoCheck {
                            true => State::Loaded,
                            false => State::Corrupt,
                        }
                    }
                    // Cut short: asked again by the next read.
                    Done::ReadInto { .. } => State::Unloaded,
                    _ if self.plant == Plant::FailedLoadKept => State::Loaded,
                    _ => State::Unloaded,
                };
            }
        }
    }

    fn loading(&self) -> usize {
        self.words.iter().filter(|it| it.state == State::Loading).count()
    }
}

// --- The plan -------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Act {
    /// A page whose bytes start at `fill` put and committed.
    Put { fill: u8 },
    Read { page: u8 },
    Evict { page: u8 },
    /// The clock on by `us`, and what is ready by then told to the core.
    Step { us: u16 },
    /// A committed page's bytes on disk rotted.
    Rot { page: u8 },
    /// The reads held so far let go.
    Release,
    Crash,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    fail_one_in: u32,
    fail_reads: bool,
    short_reads: bool,
    hold_reads: bool,
    acts: Vec<Act>,
}

/// A source of draws: Hegel's (shrunk, replayed by Hegel) or a seed's ([`plan_of`]).
trait Draw {
    /// In `lo..=hi`.
    fn int(&mut self, lo: u64, hi: u64) -> u64;

    fn coin(&mut self) -> bool {
        self.int(0, 1) == 1
    }
}

struct Drawn<'a>(&'a TestCase);

impl Draw for Drawn<'_> {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        self.0.draw(gs::integers::<u64>().min_value(lo).max_value(hi))
    }
}

impl Draw for Rng {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        match (hi - lo).checked_add(1) {
            Some(n) => lo + self.below(n),
            None => self.next_u64(),
        }
    }
}

/// A plan: the simulator's seed, the fault classes it turns on (a swarm, so a rare class is not
/// drowned by a common one), and its acts.
fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let fail_one_in = match d.coin() {
        true => d.int(4, 40) as u32,
        false => 0,
    };
    let (fail_reads, short_reads, hold_reads, rot) = (d.coin(), d.coin(), d.coin(), d.coin());
    let n = d.int(1, MOST_ACTS);
    let mut acts = Vec::new();
    for _ in 0..n {
        let page = d.int(0, PAGES as u64 - 1) as u8;
        acts.push(match d.int(0, 15) {
            0..=2 => Act::Put { fill: d.int(0, u64::from(CONTENT) - 1) as u8 },
            3..=6 => Act::Read { page },
            7..=8 => Act::Evict { page },
            9..=12 => Act::Step { us: d.int(0, 400) as u16 },
            13 if rot => Act::Rot { page },
            14 if hold_reads => Act::Release,
            15 => Act::Crash,
            _ => Act::Step { us: 0 },
        });
    }
    Plan { seed, fail_one_in, fail_reads, short_reads, hold_reads, acts }
}

fn plan_of(seed: u64) -> Plan {
    plan(&mut Rng::new(seed))
}

// --- A run ----------------------------------------------------------------------------------

/// Why a run failed: the oracle, or the harness itself.
#[derive(Debug)]
enum Broke {
    Oracle(String),
    Harness(String),
}

impl std::fmt::Display for Broke {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Broke::Oracle(why) => write!(f, "the oracle: {why}"),
            Broke::Harness(why) => write!(f, "the harness: {why}"),
        }
    }
}

#[allow(unsafe_code, reason = "test memory: under `Sim`, which touches it only inside poll and crash")]
fn memory() -> Memory {
    // SAFETY: `Memory::new`'s contract holds under `Sim` on the test's one thread; a broken
    // asker reads the simulation's scribble, which is what this test looks for.
    unsafe { Memory::new("the pages DST's slots", PAGES * PAGE) }.expect("a free region")
}

/// The bytes of a put whose first is `fill`: each below [`CONTENT`].
fn page_bytes(fill: u8) -> Vec<u8> {
    (0..PAGE).map(|at| fill.wrapping_add(at as u8) % CONTENT).collect()
}

struct World {
    sim: Sim,
    memory: Memory,
    pages: Pages,
    plant: Plant,
    effects: Vec<Effect>,
    out: Vec<Completion>,
    /// What each page committed or being committed holds, by page.
    model: Vec<Vec<u8>>,
    /// Pages whose bytes on disk rotted.
    rotted: BTreeSet<usize>,
}

impl World {
    fn new(plan: &Plan, plant: Plant) -> World {
        let mut sim = Sim::new(plan.seed, Faults { fail_one_in: plan.fail_one_in, ..Faults::default() });
        let mut memory = memory();
        let mut effects = Vec::new();
        let pages = Pages::open(plant, &mut memory, &mut effects);
        sim.submit(&mut effects);
        let mut world = World {
            sim,
            memory,
            pages,
            plant,
            effects,
            out: Vec::new(),
            model: Vec::new(),
            rotted: BTreeSet::new(),
        };
        world.faults(plan);
        world
    }

    /// The plan's targeted faults, on again after each crash.
    fn faults(&mut self, plan: &Plan) {
        let dir = FileName::new("pages").expect("a name");
        if plan.fail_reads {
            self.sim.fail_reads(dir.clone(), 3);
        }
        if plan.short_reads {
            self.sim.short_reads(dir, 3);
        }
        self.faults_on_reads(plan);
    }

    /// Reads into memory held, where the plan holds them.
    fn faults_on_reads(&mut self, plan: &Plan) {
        if plan.hold_reads {
            self.sim.hold_reads(FileName::new("pages").expect("a name"));
        }
    }

    /// The clock on by `us`, and what completed by then told to the core.
    fn step(&mut self, us: u16) {
        self.sim.advance(std::time::Duration::from_micros(u64::from(us)));
        self.sim.poll(&mut self.out, Deadline::PASSED);
        for completion in self.out.drain(..) {
            self.pages.complete(completion, &mut self.memory, &mut self.effects);
        }
        self.sim.submit(&mut self.effects);
    }

    /// What a read of `page` found, checked.
    fn read(&mut self, page: usize) -> Result<Outcome, Broke> {
        let outcome = self.pages.read(page, &self.memory, &mut self.effects);
        self.sim.submit(&mut self.effects);
        match &outcome {
            Outcome::Bytes(bytes) if *bytes != self.model[page] => {
                let first = bytes.iter().zip(&self.model[page]).position(|(a, b)| a != b);
                return Err(Broke::Oracle(format!(
                    "page {page} read bytes that were not put: first wrong at {first:?}, {:?}",
                    first.map(|at| bytes[at])
                )));
            }
            Outcome::Corrupt if !self.rotted.contains(&page) => {
                return Err(Broke::Oracle(format!(
                    "page {page} is refused as corrupt, and nothing rotted it"
                )));
            }
            _ => {}
        }
        Ok(outcome)
    }

    /// Steps until the open reads its head and cuts the file, then checks it found exactly the
    /// pages whose head write completed.
    fn reopened(&mut self, durable: usize) -> Result<(), Broke> {
        for _ in 0..DRAIN {
            if self.pages.opening == Opening::Open {
                return match self.pages.committed == durable {
                    true => Ok(()),
                    false => Err(Broke::Oracle(format!(
                        "the open found {} pages committed, and {durable} head writes completed",
                        self.pages.committed
                    ))),
                };
            }
            // The open reads its head whole and cuts the file: no read into memory to hold.
            self.step(100);
        }
        Err(Broke::Harness(format!("the open did not end in {DRAIN} polls")))
    }

    fn crash(&mut self, plan: &Plan) -> Result<(), Broke> {
        let durable = self.pages.committed;
        self.sim.crash();
        self.pages = Pages::open(self.plant, &mut self.memory, &mut self.effects);
        self.sim.submit(&mut self.effects);
        self.faults(plan);
        self.reopened(durable)
    }

    /// Everything in flight let end, then every committed page read back.
    fn drain(&mut self) -> Result<(), Broke> {
        for _ in 0..DRAIN {
            if self.pages.commit.is_none() && self.pages.loading() == 0 && self.sim.in_flight() == 0
            {
                break;
            }
            self.sim.release_reads(std::time::Duration::ZERO);
            self.step(100);
        }
        if self.pages.loading() > 0 || self.pages.commit.is_some() {
            return Err(Broke::Oracle(format!(
                "after {DRAIN} polls {} loads and a commit ({:?}) have not ended",
                self.pages.loading(),
                self.pages.commit
            )));
        }
        for page in 0..self.pages.committed {
            let mut read = false;
            for _ in 0..DRAIN {
                if !matches!(self.read(page)?, Outcome::Waiting) {
                    read = true;
                    break;
                }
                self.sim.release_reads(std::time::Duration::ZERO);
                self.step(100);
            }
            if !read {
                return Err(Broke::Oracle(format!("page {page} never read in {DRAIN} polls")));
            }
        }
        Ok(())
    }
}

/// `plan` run with `plant` planted, the oracle at every read and open and at the end.
fn run(plan: &Plan, plant: Plant) -> Result<(), Broke> {
    let mut world = World::new(plan, plant);
    world.reopened(0)?;
    for act in &plan.acts {
        match *act {
            Act::Put { fill } => {
                let bytes = page_bytes(fill);
                let page = world.pages.committed;
                if world.pages.put(&bytes, &mut world.memory, &mut world.effects) {
                    world.model.truncate(page);
                    world.model.push(bytes);
                    world.rotted.remove(&page);
                    world.sim.submit(&mut world.effects);
                }
            }
            Act::Read { page } => _ = world.read(page as usize)?,
            Act::Evict { page } => _ = world.pages.evict(page as usize, &mut world.memory),
            Act::Step { us } => world.step(us),
            Act::Rot { page } if (page as usize) < world.pages.committed => {
                let at = (page as usize * PAGE + 7) as u64;
                world.sim.rot(&data(), at, &[ROT; 9]);
                world.rotted.insert(page as usize);
            }
            Act::Rot { .. } => {}
            // Held again from now on: the reads asked after are held in their turn.
            Act::Release => {
                world.sim.release_reads(std::time::Duration::ZERO);
                world.faults_on_reads(plan);
            }
            Act::Crash => world.crash(plan)?,
        }
    }
    world.drain()?;
    world.crash(plan)?;
    world.drain()
}

// --- The tests ------------------------------------------------------------------------------

/// The invariant: whatever the faults, wherever the power goes out, a read hands back the bytes
/// put or a refusal the disk's rot explains, the open finds what completed, and every load ends.
#[hegel::test(test_cases = 200)]
fn pages_read_back_what_was_put_after_any_crash(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(broke) = run(&plan, Plant::None) {
        panic!("{broke}\nreplay: simulator seed {}, plan {plan:?}", plan.seed);
    }
}

/// A plant the oracle must catch on one of [`CONTROL_SEEDS`] seeded plans (test-the-cause on
/// the test itself): a green DST with a blind oracle proves nothing.
fn control(plant: Plant) {
    for seed in 0..CONTROL_SEEDS {
        match run(&plan_of(seed), plant) {
            Err(Broke::Oracle(why)) => {
                eprintln!("{plant:?} caught at seed {seed} (REPLAY = ({seed}, Plant::{plant:?})): {why}");
                return;
            }
            Err(broke @ Broke::Harness(_)) => panic!("seed {seed}, {plant:?}: {broke}"),
            Ok(()) => {}
        }
    }
    panic!(
        "the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, \
         are too weak"
    );
}

#[test]
fn the_oracle_catches_a_page_loaded_as_its_read_is_asked() {
    control(Plant::LoadedAtAsk);
}

#[test]
fn the_oracle_catches_a_failed_load_kept() {
    control(Plant::FailedLoadKept);
}

#[test]
fn the_oracle_catches_a_load_with_no_check() {
    control(Plant::NoCheck);
}

#[test]
fn the_oracle_catches_an_eviction_of_a_page_the_commit_pinned() {
    control(Plant::EvictPinned);
}

#[test]
fn the_oracle_catches_a_head_written_before_the_data_s_sync() {
    control(Plant::HeadBeforeSync);
}

/// One seeded plan, [`REPLAY`]'s: set it to a seed a failure names to run that case alone.
#[test]
fn replay_one_seed() {
    let (seed, plant) = REPLAY;
    if let Err(broke) = run(&plan_of(seed), plant) {
        panic!("seed {seed}, Plant::{plant:?}: {broke}");
    }
}
