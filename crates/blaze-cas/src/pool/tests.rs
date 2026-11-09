//! The pool's deterministic simulation test (docs/v2/plan/design/store-pool.md, section 7): the
//! real word, reading, pins, pin set, optimistic reads, loads core, sweep and windows, over files
//! the test writes, driven by `blaze_core::sim::Sim`. The store does not read through the pool yet
//! (lane 4), so the files stand in for its packs: records of drawn sizes laid out at 16 bytes,
//! padded to a block, each file written whole and durable, with its blocks' checks.
//!
//! What it draws, as one plan (Hegel's, shrunk; or a seed's, for the controls and replay):
//!
//! - files and their records (16 bytes to several pages, a file longer than a window taking a
//!   run of them; one in four an index file, which the sweep weighs four times);
//! - actors and schedule: three readers, each a pinned read held across later acts and let go,
//!   a read into the task's pin set (let go at a task end), or an optimistic read in its three
//!   steps (look, copy, look again); the sweep, a page at a time or a whole eviction, and its
//!   batch given back as its own step; the loader's completions, delayed and shuffled. Which
//!   takes its next step is drawn, at the granularity of word operations (`Sim` is one thread,
//!   so the test steps each machine itself; two threads' atomics interleaving at every operation
//!   waits for `hegel::sim`'s scheduled atomics);
//! - faults, as a swarm: failed reads, short reads, held reads, any effect failed once in a
//!   while, and a block of a file changed on disk (rot);
//! - the commit: an append to a file, as the store's commit makes one. It pins the file's last
//!   committed page (a pinned read, loading it if it was evicted), writes its new records into
//!   the file's window, writes them from there (`WriteFrom`), syncs, and only then makes them the
//!   file's ([`Append`]). The test's files stand in for the head: an append is committed, in the
//!   model and in the pool, as its sync completes. One that fails, or is in flight at a crash, is
//!   not;
//! - file retirements (its windows freed and taken by a new file), crashes and reopens.
//!
//! The test's [`Poison`] stands in for the kernel: what it is given back it poisons with `0xDB`
//! (for Linux's zeros and macOS's anything), and it counts the pages the process holds.
//!
//! The oracle, at every act and after every crash, against the files' bytes:
//!
//! 1. Every pinned read (held, or in the pin set) and every optimistic read that validates hands
//!    back exactly the record's bytes; a read that fails fails with a typed error its faults
//!    explain (a failed or short read where those are on, a corrupt block where the disk rotted).
//! 2. No page is given back while it is pinned or `Loading`: every page given back is `Evicting`
//!    with no pin, and none is made the process's while it already is.
//! 3. After any crash and reopen, every record reads back its bytes, or is refused for a block
//!    that rotted: every record of a committed append among them, and none of one that was not.
//! 4. The pages the process holds (what `Tag::Pool` counts) are exactly the pages `Loading`,
//!    `Loaded`, `Evicting` or `Abandoned`; the pins in the words are exactly the guards' alive.
//! 5. Every load ends, and every reading ends, within a bound of polls.
//!
//! (Section 7's sixth, the prefetch budget, is lane 6's.)
//!
//! Planted bugs 1 to 6 and 8 ([`Plant`]), each a safeguard of the pool's removed, must each be caught on
//! one of [`CONTROL_SEEDS`] seeded plans. Replay: a failure names its plan and the simulator's
//! seed in it; a control names a seed of [`plan_of`] for [`REPLAY`].

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    rc::Rc,
    time::Duration,
};

use blaze_core::{
    Cache, Completion, Done, Driver, Effect, FileName, Tag,
    sim::{Faults, Rng, SCRIBBLE, Sim},
    wait::Deadline,
};
use blaze_mem::MemError;
use hegel::{TestCase, generators as gs};

use super::{
    Append, BLOCK, Ended, FileId, Give, Kernel, LoadFailure, Loads, Opened, Optimistic, PAGE, PinSet, Pins,
    Plant, Pool, PoolError, Rank, ReadStep, Reading, Seen, State, WINDOW, Window, Windows, checks_of,
    drive::Loader,
    evict::{Sweep, Visit},
};

/// Windows of the test's pool: 4 pages each (a test's `PACK_MOST`), room for every file a run
/// makes after its retirements.
const WINDOWS: u32 = 64;
const READERS: usize = 3;
/// The commit's place among the actors, after the readers.
const COMMITTER: usize = READERS;
/// The tags of the commit's writes, from this one, and of its sync: none a load's.
const WRITE_TAG: u64 = 1000;
const SYNC_TAG: u64 = 999;
/// Load slots the test hands its readers: few, so a reader meets a full table.
const SLOTS: u16 = 3;
/// What a page given back holds: the design's stand-in for the kernel's zeros or anything.
const POISON: u8 = 0xDB;
const ROT: u8 = 0xFF;
/// Every byte a record holds is in `1..CONTENT`: never a page's zeros, the simulation's scribble,
/// the poison or the rot.
const CONTENT: u8 = 0x40;
const TAGS: u64 = 1 << 40;
const MOST_ACTS: u64 = 120;
/// Polls the end of a run may take to let everything in flight end (bounded-progress).
const DRAIN: u32 = 4000;
/// Tries a record gets to read back through failing and short reads.
const TRIES: u32 = 64;
/// The most an optimistic read copies: the store's are 16 to 96 bytes.
const OPTIMISTIC_MOST: u64 = 256;
/// Seeded plans a control searches for one its plant breaks.
const CONTROL_SEEDS: u64 = 1024;
/// The seed [`replay_one_seed`] runs, and its plant: set them to what a failure names.
const REPLAY: (u64, Plant) = (0, Plant::None);

const _: () = assert!(CONTENT < SCRIBBLE && CONTENT < POISON && CONTENT < ROT);
const _: () = assert!(WRITE_TAG < TAGS && SYNC_TAG < WRITE_TAG);

type Handle = Rc<Pool<Poison>>;

fn dir() -> FileName {
    FileName::new("pool").expect("a name")
}

// --- The kernel's stand-in -------------------------------------------------------------------

/// What the kernel would do with the pool's pages, checked: a page made the process's is
/// counted, one given back is poisoned and uncounted, and either done to a page in the wrong
/// state is the oracle's (its second and fourth rules).
#[derive(Default)]
struct Poison {
    held: RefCell<BTreeSet<u64>>,
    broke: RefCell<Option<String>>,
}

impl Poison {
    fn note(&self, why: String) {
        self.broke.borrow_mut().get_or_insert(why);
    }
}

impl Give for Poison {
    fn reuse(&self, _: &Pool<Self>, from: usize, len: usize) -> Result<(), MemError> {
        for page in from as u64 / PAGE..(from + len) as u64 / PAGE {
            if !self.held.borrow_mut().insert(page) {
                self.note(format!("page {page} made the process's while it already was: given back late, or never"));
            }
        }
        Ok(())
    }

    unsafe fn evict(&self, pool: &Pool<Self>, ranges: &[Range<usize>]) -> Result<usize, MemError> {
        let mut given = 0;
        for range in ranges {
            for page in range.start as u64 / PAGE..range.end as u64 / PAGE {
                let word = pool.word_now(page);
                if word.state() != State::Evicting || word.pins() > 0 {
                    self.note(format!("page {page} given back while {word:?}"));
                }
                if !self.held.borrow_mut().remove(&page) {
                    self.note(format!("page {page} given back while it was not the process's"));
                }
                pool.fill(page, POISON);
            }
            given += range.len();
        }
        Ok(given)
    }
}

// --- The plan --------------------------------------------------------------------------------

/// A file to write: its records' sizes, the first byte of their pattern, and how the sweep
/// weighs it.
#[derive(Clone, Debug)]
struct Spec {
    sizes: Vec<u64>,
    fill: u8,
    rank: Rank,
}

#[derive(Clone, Copy, Debug)]
enum Act {
    /// A pinned read of a record started (or the reader's next step, if it is busy): held as a
    /// guard, or kept in the task's pin set.
    Read { reader: u8, file: u8, rec: u8, set: bool },
    /// An optimistic read of a record's first bytes started.
    Optimistic { reader: u8, file: u8, rec: u8 },
    /// One reader's next step.
    Advance { reader: u8 },
    AdvanceAll,
    /// A held read let go.
    Release { reader: u8 },
    /// The task's pin set let go: a task end, or a frame reset between items.
    TaskEnd,
    /// The sweep's whole call, for a page's bytes.
    Evict,
    /// The sweep's next page.
    Visit,
    /// The sweep's batch given back.
    Give,
    /// The clock on by `us`, and what completed told to the loads core.
    Step { us: u16 },
    /// A block of a file changed on disk.
    Rot { file: u8, block: u8 },
    /// The reads held so far let go.
    ReleaseReads,
    /// A file retired, and a new one made (taking its windows, maybe).
    Retire { file: u8 },
    /// The commit's next step: an append to a file begun, or the one under way stepped.
    Append { file: u8 },
    Crash,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    fail_one_in: u32,
    fail_reads: bool,
    short_reads: bool,
    hold_reads: bool,
    rot: bool,
    files: Vec<Spec>,
    spares: Vec<Spec>,
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

fn spec(d: &mut impl Draw) -> Spec {
    let records = d.int(1, 3);
    let sizes = (0..records)
        .map(|_| match d.int(0, 5) {
            0..=2 => d.int(16, 512),
            3 | 4 => d.int(4 << 10, 40 << 10),
            _ => d.int(60 << 10, 200 << 10),
        })
        .collect();
    let rank = match d.int(0, 3) {
        0 => Rank::Index,
        _ => Rank::Plain,
    };
    Spec { sizes, fill: d.int(0, u64::from(CONTENT) - 2) as u8, rank }
}

/// A plan: the simulator's seed, the fault classes it turns on (a swarm, so a rare class is not
/// drowned by a common one), its files, and its acts.
fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let fail_one_in = match d.coin() {
        true => d.int(4, 40) as u32,
        false => 0,
    };
    let (fail_reads, short_reads, hold_reads, rot) = (d.coin(), d.coin(), d.coin(), d.coin());
    let files = (0..d.int(1, 3)).map(|_| spec(d)).collect();
    let spares = (0..2).map(|_| spec(d)).collect();
    let n = d.int(1, MOST_ACTS);
    let mut acts = Vec::new();
    for _ in 0..n {
        let reader = d.int(0, READERS as u64 - 1) as u8;
        let (file, rec) = (d.int(0, 2) as u8, d.int(0, 2) as u8);
        acts.push(match d.int(0, 25) {
            0..=3 => Act::Read { reader, file, rec, set: false },
            4 | 5 => Act::Read { reader, file, rec, set: true },
            6 | 7 => Act::Optimistic { reader, file, rec },
            8..=11 => Act::Advance { reader },
            12 => Act::AdvanceAll,
            13 => Act::Release { reader },
            14 => Act::TaskEnd,
            15 | 16 => Act::Evict,
            17 => Act::Visit,
            18 => Act::Give,
            19 | 20 => Act::Step { us: d.int(0, 400) as u16 },
            21 if rot => Act::Rot { file, block: d.int(0, 255) as u8 },
            21 if hold_reads => Act::ReleaseReads,
            21 => Act::Step { us: 0 },
            22 => Act::Retire { file },
            23 | 24 => Act::Append { file },
            _ => Act::Crash,
        });
    }
    Plan { seed, fail_one_in, fail_reads, short_reads, hold_reads, rot, files, spares, acts }
}

fn plan_of(seed: u64) -> Plan {
    plan(&mut Rng::new(seed))
}

/// The bytes of a file of `spec`'s records, padded to a block, and each record's place.
fn file_bytes(spec: &Spec) -> (Vec<u8>, Vec<(u64, u64)>) {
    let (mut bytes, mut records) = (Vec::new(), Vec::new());
    for (i, &len) in spec.sizes.iter().enumerate() {
        let at = bytes.len().next_multiple_of(16);
        bytes.resize(at, 0);
        let start = u64::from(spec.fill) + i as u64 * 7;
        bytes.extend((0..len).map(|k| 1 + ((start + k * 3) % u64::from(CONTENT - 1)) as u8));
        records.push((at as u64, len));
    }
    bytes.resize(bytes.len().next_multiple_of(BLOCK as usize), 0);
    (bytes, records)
}

// --- A run -----------------------------------------------------------------------------------

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

/// A file the test wrote: what the store would know of a pack.
struct File {
    id: FileId,
    name: FileName,
    bytes: Vec<u8>,
    records: Vec<(u64, u64)>,
    rank: Rank,
    window: Window,
    /// Its blocks changed on disk.
    rotted: BTreeSet<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Keep {
    Hold,
    Set,
    /// The commit's pin of a file's last committed page, for its append.
    Tail,
}

enum Actor {
    Idle,
    Reading { file: usize, rec: usize, keep: Keep, reading: Reading },
    Holding { file: usize, rec: usize, pins: Pins<Poison, Handle> },
    /// An optimistic read that looked at its words.
    Begun { file: usize, rec: usize, at: u64, len: usize, seen: Seen },
    /// One that copied its bytes, to look again.
    Copied { file: usize, rec: usize, seen: Seen, copy: Vec<u8> },
    /// The commit's append: its records' bytes in the file's window, their writes asked, then
    /// their sync.
    Appending {
        file: usize,
        append: Append<Poison, Handle>,
        bytes: Vec<u8>,
        /// The new records' places in the file.
        records: Vec<(u64, u64)>,
        checks: Vec<u32>,
        /// Writes not yet answered.
        writes: u32,
        failed: bool,
        syncing: bool,
    },
}

impl Actor {
    fn file(&self) -> Option<usize> {
        match self {
            Actor::Idle => None,
            Actor::Reading { file, .. }
            | Actor::Holding { file, .. }
            | Actor::Begun { file, .. }
            | Actor::Copied { file, .. }
            | Actor::Appending { file, .. } => Some(*file),
        }
    }
}

fn fresh(plant: Plant) -> Result<Handle, Broke> {
    Pool::planted_with(WINDOWS, Poison::default(), plant).map(Rc::new).map_err(|err| Broke::Harness(err.to_string()))
}

/// `bytes` of the file `id` opened in the pool, as the store opens a pack.
fn open(
    windows: &mut Windows,
    loads: &mut Loads,
    pool: &Pool<Poison>,
    id: FileId,
    name: &FileName,
    bytes: &[u8],
    rank: Rank,
) -> Result<Window, Broke> {
    let window = windows.open(pool, id, bytes.len() as u64, rank).map_err(|err| Broke::Harness(err.to_string()))?;
    loads.open(Opened { window, name: name.clone(), checks: checks_of(bytes) });
    Ok(window)
}

struct World {
    sim: Sim,
    actors: Vec<Actor>,
    set: PinSet<Poison, Handle>,
    /// The records read into the set, to look at every act.
    set_held: Vec<(usize, usize)>,
    pool: Handle,
    plant: Plant,
    windows: Windows,
    loads: Loads,
    sweep: Sweep,
    files: Vec<File>,
    spares: usize,
    next_id: u32,
    /// Load slots no reading holds.
    free: Vec<u16>,
    /// The reader whose load each slot is.
    owners: BTreeMap<u16, usize>,
    effects: Vec<Effect>,
    out: Vec<Completion>,
    ended: Vec<Ended>,
}

impl Drop for World {
    fn drop(&mut self) {
        // A run that stops mid-read drops its readings with the process's memory.
        for actor in &mut self.actors {
            if let Actor::Reading { reading, .. } = std::mem::replace(actor, Actor::Idle) {
                reading.lost();
            }
        }
    }
}

impl World {
    fn new(plan: &Plan, plant: Plant) -> Result<World, Broke> {
        let faults = Faults { fail_one_in: plan.fail_one_in, ..Faults::default() };
        let pool = fresh(plant)?;
        let mut world = World {
            sim: Sim::new(plan.seed, faults),
            actors: (0..=READERS).map(|_| Actor::Idle).collect(),
            set: PinSet::new(Rc::clone(&pool)),
            set_held: Vec::new(),
            pool,
            plant,
            windows: Windows::new(WINDOWS),
            loads: Loads::new(TAGS, Cache::Bypassed),
            sweep: Sweep::new(),
            files: Vec::new(),
            spares: 0,
            next_id: 0,
            free: (0..SLOTS).collect(),
            owners: BTreeMap::new(),
            effects: Vec::new(),
            out: Vec::new(),
            ended: Vec::new(),
        };
        for spec in &plan.files {
            let file = world.make(spec)?;
            world.files.push(file);
        }
        world.faults(plan);
        Ok(world)
    }

    /// A new file of `spec` written, durably, and opened.
    fn make(&mut self, spec: &Spec) -> Result<File, Broke> {
        let (bytes, records) = file_bytes(spec);
        let n = self.next_id;
        self.next_id += 1;
        let id = FileId::Pack(n);
        let name = FileName::new(&format!("pool/pack-{n}")).expect("a name");
        self.sim.change(&name, &bytes);
        let window = open(&mut self.windows, &mut self.loads, &self.pool, id, &name, &bytes, spec.rank)?;
        Ok(File { id, name, bytes, records, rank: spec.rank, window, rotted: BTreeSet::new() })
    }

    /// The plan's targeted faults, on again after each crash.
    fn faults(&mut self, plan: &Plan) {
        if plan.fail_reads {
            self.sim.fail_reads(dir(), 3);
        }
        if plan.short_reads {
            self.sim.short_reads(dir(), 3);
        }
        if plan.hold_reads {
            self.sim.hold_reads(dir());
        }
    }

    /// A drawn file and record, of those there are.
    fn pick(&self, file: u8, rec: u8) -> (usize, usize) {
        let file = file as usize % self.files.len();
        (file, rec as usize % self.files[file].records.len())
    }

    /// The pool's byte record `rec` of `file` starts at, and its length.
    fn place(&self, file: usize, rec: usize) -> (u64, u64) {
        let file = &self.files[file];
        let (at, len) = file.records[rec];
        (file.window.at(at), len)
    }

    fn record(&self, file: usize, rec: usize) -> &[u8] {
        let (at, len) = self.files[file].records[rec];
        &self.files[file].bytes[at as usize..(at + len) as usize]
    }

    /// The oracle's first rule for one read's bytes: exactly the record's (or its prefix).
    fn same(&self, file: usize, rec: usize, bytes: &[u8], what: &str) -> Result<(), Broke> {
        let want = &self.record(file, rec)[..bytes.len()];
        if bytes == want {
            return Ok(());
        }
        let first = bytes.iter().zip(want).position(|(a, b)| a != b);
        Err(Broke::Oracle(format!(
            "{what} of record {rec} of {} handed back bytes that were not put: first wrong at {first:?}, \
             {:#04x?} where {:#04x?} was put",
            self.files[file].name,
            first.map(|at| bytes[at]),
            first.map(|at| want[at]),
        )))
    }

    /// Whether a block that rotted lies in a page of record `rec` of `file`.
    fn rotted(&self, file: usize, rec: usize) -> bool {
        let (at, len) = self.files[file].records[rec];
        let pages = at / PAGE..=(at + len - 1) / PAGE;
        self.files[file].rotted.iter().any(|block| pages.contains(&(block * BLOCK / PAGE)))
    }

    /// The oracle's first rule for a read that failed: a fault the plan turned on explains it.
    fn explained(&self, plan: &Plan, file: usize, rec: usize, error: &PoolError) -> Result<(), Broke> {
        let ours = &self.files[file];
        let explained = match error {
            PoolError::Load(LoadFailure::Read { .. }) => plan.fail_reads || plan.fail_one_in > 0,
            PoolError::Load(LoadFailure::Short { .. }) => plan.short_reads,
            PoolError::Load(LoadFailure::Block { file: name, block, .. }) => {
                *name == ours.name && ours.rotted.contains(block)
            }
            PoolError::Corrupt { page } => {
                let page = page - ours.window.first_page();
                ours.rotted.iter().any(|block| block * BLOCK / PAGE == page)
            }
            PoolError::Load(LoadFailure::Abandoned { .. } | LoadFailure::NoFile { .. })
            | PoolError::Reserve { .. }
            | PoolError::Full { .. }
            | PoolError::Busy { .. }
            | PoolError::Give { .. }
            | PoolError::Reuse { .. }
            | PoolError::Abandoned { .. }
            | PoolError::NotFresh { .. }
            | PoolError::Starved { .. }
            | PoolError::LoadsFull { .. } => false,
        };
        match explained {
            true => Ok(()),
            false => Err(Broke::Oracle(format!(
                "a read of record {rec} of {} failed, and no fault the plan drew explains it: {error}",
                ours.name
            ))),
        }
    }

    /// The oracle's rules that hold at every act: the guards' bytes, what was given back and
    /// taken, what the process holds, and the pins.
    fn check(&self) -> Result<(), Broke> {
        if let Some(why) = self.pool.give().broke.borrow().clone() {
            return Err(Broke::Oracle(why));
        }
        let mut guards = self.set.pages();
        for actor in &self.actors {
            match actor {
                Actor::Reading { reading, .. } => guards += u64::from(reading.held()),
                Actor::Holding { file, rec, pins } => {
                    guards += u64::from(pins.run().pages);
                    self.same(*file, *rec, pins.bytes(), "a pinned read held")?;
                }
                Actor::Appending { append, .. } => guards += append.pinned(),
                Actor::Idle | Actor::Begun { .. } | Actor::Copied { .. } => {}
            }
        }
        for &(file, rec) in &self.set_held {
            let (at, len) = self.place(file, rec);
            match self.set.find(at, len) {
                Some(bytes) => self.same(file, rec, bytes, "a read the task's pin set holds")?,
                None => {
                    return Err(Broke::Oracle(format!(
                        "the task's pin set lost record {rec} of {}",
                        self.files[file].name
                    )));
                }
            }
        }
        let (mut held, mut pins) = (0u64, 0u64);
        for page in 0..self.pool.total() {
            let word = self.pool.word_now(page);
            match word.state() {
                State::Loading | State::Loaded | State::Evicting | State::Abandoned => held += 1,
                State::Unloaded | State::Corrupt => {}
            }
            if matches!(word.state(), State::Loaded | State::Evicting) {
                pins += u64::from(word.pins());
            }
        }
        let resident = self.pool.give().held.borrow().len() as u64;
        if held != resident {
            return Err(Broke::Oracle(format!(
                "{held} pages are loading, loaded or being given back, and the process holds {resident}: \
                 Tag::Pool would not count them"
            )));
        }
        if pins != guards {
            return Err(Broke::Oracle(format!("the pages' words hold {pins} pins, and the guards alive {guards}")));
        }
        Ok(())
    }

    fn slot_back(&mut self, reading: &mut Reading) {
        if let Some(slot) = reading.take_slot() {
            self.free.push(slot);
        }
    }

    /// Reader `i`'s next step.
    fn advance(&mut self, plan: &Plan, i: usize) -> Result<(), Broke> {
        let next = match std::mem::replace(&mut self.actors[i], Actor::Idle) {
            Actor::Reading { file, rec, keep, mut reading } => match reading.step(&self.pool) {
                ReadStep::Pinned(pins) => {
                    self.slot_back(&mut reading);
                    drop(reading);
                    if keep != Keep::Tail {
                        self.same(file, rec, pins.bytes(), "a pinned read")?;
                    }
                    match keep {
                        Keep::Tail => self.begin_append(plan, file, Some(pins))?,
                        Keep::Hold => Actor::Holding { file, rec, pins },
                        Keep::Set => match self.set.keep(pins) {
                            Ok(bytes) => {
                                self.same(file, rec, bytes, "a read into the task's pin set")?;
                                self.set_held.push((file, rec));
                                Actor::Idle
                            }
                            Err(pins) => Actor::Holding { file, rec, pins },
                        },
                    }
                }
                ReadStep::Failed(error) => {
                    self.slot_back(&mut reading);
                    drop(reading);
                    self.explained(plan, file, rec, &error)?;
                    Actor::Idle
                }
                ReadStep::NeedSlot => {
                    // A full table: the reader waits for a slot (here, its next step).
                    if let Some(slot) = self.free.pop() {
                        reading.give_slot(slot);
                    }
                    Actor::Reading { file, rec, keep, reading }
                }
                ReadStep::Ask(load) => {
                    self.owners.insert(load.slot, i);
                    self.loads.ask(&self.pool, load, &mut self.effects, &mut self.ended);
                    Actor::Reading { file, rec, keep, reading }
                }
                // Its load, another's, or the sweep's batch: looked at again at its next step.
                ReadStep::Pending | ReadStep::Wait { .. } | ReadStep::Spin { .. } => {
                    Actor::Reading { file, rec, keep, reading }
                }
            },
            Actor::Begun { file, rec, at, len, seen } => {
                let mut copy = vec![0; len];
                self.pool.copy(at, &mut copy);
                Actor::Copied { file, rec, seen, copy }
            }
            Actor::Copied { file, rec, seen, copy } => {
                if self.pool.still(&seen) {
                    self.same(file, rec, &copy, "an optimistic read that validated")?;
                    self.pool.touch(&seen);
                }
                Actor::Idle
            }
            held @ (Actor::Idle | Actor::Holding { .. } | Actor::Appending { .. }) => held,
        };
        self.actors[i] = next;
        self.sim.submit(&mut self.effects);
        self.deliver()
    }

    /// Each load that ended told to the reading that asked it.
    fn deliver(&mut self) -> Result<(), Broke> {
        let mut ended = std::mem::take(&mut self.ended);
        for Ended { slot, outcome } in ended.drain(..) {
            let Some(i) = self.owners.remove(&slot) else {
                return Err(Broke::Harness(format!("load slot {slot} ended with no reader")));
            };
            match &mut self.actors[i] {
                Actor::Reading { reading, .. } => reading.loaded(&self.pool, outcome),
                _ => return Err(Broke::Harness(format!("load slot {slot} ended for reader {i}, which reads nothing"))),
            }
        }
        self.ended = ended;
        Ok(())
    }

    /// The clock on by `us`, and what completed by then told to the loads core.
    fn step(&mut self, us: u64) -> Result<(), Broke> {
        self.sim.advance(Duration::from_micros(us));
        self.sim.poll(&mut self.out, Deadline::PASSED);
        let mut out = std::mem::take(&mut self.out);
        for completion in out.drain(..) {
            match self.loads.owns(completion.tag) {
                true => self.loads.complete(&self.pool, completion, &mut self.ended),
                false => self.written(completion)?,
            }
        }
        self.out = out;
        self.deliver()?;
        self.sim.submit(&mut self.effects);
        Ok(())
    }

    fn read(&mut self, plan: &Plan, reader: u8, file: u8, rec: u8, keep: Keep) -> Result<(), Broke> {
        let i = reader as usize % READERS;
        if matches!(self.actors[i], Actor::Idle) {
            let (file, rec) = self.pick(file, rec);
            let (at, len) = self.place(file, rec);
            self.actors[i] = Actor::Reading { file, rec, keep, reading: Reading::new(at, len) };
        }
        self.advance(plan, i)
    }

    fn optimistic(&mut self, plan: &Plan, reader: u8, file: u8, rec: u8) -> Result<(), Broke> {
        let i = reader as usize % READERS;
        if !matches!(self.actors[i], Actor::Idle) {
            return self.advance(plan, i);
        }
        let (file, rec) = self.pick(file, rec);
        let (at, len) = self.place(file, rec);
        let len = len.min(OPTIMISTIC_MOST) as usize;
        // A miss is a pinned read's to load: this one ends.
        if let Ok(seen) = self.pool.begin(at, len) {
            self.actors[i] = Actor::Begun { file, rec, at, len, seen };
        }
        Ok(())
    }

    /// The commit's next step: an append to a drawn file begun (its last committed page pinned
    /// first, when its committed bytes end inside one), or the one under way stepped.
    fn append(&mut self, plan: &Plan, file: u8) -> Result<(), Broke> {
        if matches!(self.actors[COMMITTER], Actor::Idle) {
            let (file, _) = self.pick(file, 0);
            let window = self.files[file].window;
            self.actors[COMMITTER] = match Append::<Poison, Handle>::tail_of(&window) {
                Some((at, len)) => Actor::Reading { file, rec: 0, keep: Keep::Tail, reading: Reading::new(at, len) },
                None => self.begin_append(plan, file, None)?,
            };
        }
        self.advance(plan, COMMITTER)
    }

    /// The commit's append to `file` begun, `tail` its pin of the file's last committed page: a
    /// spare's records written into the file's window after what is committed, and their writes
    /// asked. No append when the file's windows have no room for them (the store would begin
    /// another pack).
    fn begin_append(&mut self, plan: &Plan, file: usize, tail: Option<Pins<Poison, Handle>>) -> Result<Actor, Broke> {
        let spec = &plan.spares[self.spares % plan.spares.len()];
        self.spares += 1;
        let (bytes, records) = file_bytes(spec);
        let ours = &self.files[file];
        let window = ours.window;
        if window.len + bytes.len() as u64 > u64::from(window.count) * WINDOW {
            return Ok(Actor::Idle);
        }
        let mut append = Append::begin(Rc::clone(&self.pool), window, tail);
        append.put(&bytes).map_err(|err| Broke::Harness(err.to_string()))?;
        let checks = append.checks();
        let parts = append.parts();
        for (n, &(at, from)) in parts.iter().enumerate() {
            self.effects.push(Effect::WriteFrom {
                file: ours.name.clone(),
                at,
                from,
                cache: Cache::Bypassed,
                tag: Tag(WRITE_TAG + n as u64),
            });
        }
        let records = records.into_iter().map(|(at, len)| (window.len + at, len)).collect();
        Ok(Actor::Appending { file, append, bytes, records, checks, writes: parts.len() as u32, failed: false, syncing: false })
    }

    /// A completion of the commit's: a write of its append, or its sync. Once every write has
    /// completed, the sync is asked; once the sync has, the append is the file's, in the pool
    /// and in the model (the head's flip). A write or a sync that failed gives the append up:
    /// nothing of it was committed.
    fn written(&mut self, completion: Completion) -> Result<(), Broke> {
        let actor = std::mem::replace(&mut self.actors[COMMITTER], Actor::Idle);
        let Actor::Appending { file, append, bytes, records, checks, mut writes, mut failed, syncing } = actor else {
            return Err(Broke::Harness(format!("a completion neither a load nor the commit asked for: {completion:?}")));
        };
        let good = !matches!(completion.done, Done::Failed(_));
        let next = match (syncing, good) {
            (true, true) => {
                let window = append.commit(&mut self.windows);
                self.loads.grow(window, checks);
                let ours = &mut self.files[file];
                ours.window = window;
                ours.bytes.extend_from_slice(&bytes);
                ours.records.extend(records);
                Actor::Idle
            }
            // Dropped, its fresh pages given back.
            (true, false) => Actor::Idle,
            (false, _) => {
                writes -= 1;
                failed |= !good;
                match (writes, failed) {
                    (0, true) => Actor::Idle,
                    (0, false) => {
                        self.effects.push(Effect::Sync { file: self.files[file].name.clone(), tag: Tag(SYNC_TAG) });
                        Actor::Appending { file, append, bytes, records, checks, writes, failed, syncing: true }
                    }
                    _ => Actor::Appending { file, append, bytes, records, checks, writes, failed, syncing },
                }
            }
        };
        self.actors[COMMITTER] = next;
        Ok(())
    }

    fn rot(&mut self, file: u8, block: u8) {
        let (file, _) = self.pick(file, 0);
        let ours = &mut self.files[file];
        let block = u64::from(block) % (ours.bytes.len() as u64 / BLOCK);
        self.sim.rot(&ours.name, block * BLOCK + 7, &[ROT; 9]);
        ours.rotted.insert(block);
    }

    /// A file retired, if nothing reads it, and a new one made in its place.
    fn retire(&mut self, plan: &Plan, file: u8) -> Result<(), Broke> {
        let (file, _) = self.pick(file, 0);
        let read = self.actors.iter().any(|it| it.file() == Some(file)) || self.set_held.iter().any(|it| it.0 == file);
        if read {
            return Ok(());
        }
        let (id, first, name) = (self.files[file].id, self.files[file].window.first, self.files[file].name.clone());
        match self.windows.retire(&self.pool, id) {
            Ok(()) => {}
            Err(PoolError::Busy { .. }) => return Ok(()),
            Err(err) => return Err(Broke::Harness(err.to_string())),
        }
        self.loads.close(first);
        self.sim.delete(&name);
        let spec = plan.spares[self.spares % plan.spares.len()].clone();
        self.spares += 1;
        self.files[file] = self.make(&spec)?;
        Ok(())
    }

    /// Power lost: the process's memory gone, its pool with it, and the store opened again over
    /// the files on disk.
    fn crash(&mut self, plan: &Plan) -> Result<(), Broke> {
        for actor in &mut self.actors {
            if let Actor::Reading { reading, .. } = std::mem::replace(actor, Actor::Idle) {
                reading.lost();
            }
        }
        self.sim.crash();
        let pool = fresh(self.plant)?;
        self.set = PinSet::new(Rc::clone(&pool));
        self.set_held.clear();
        self.pool = pool;
        self.windows = Windows::new(WINDOWS);
        self.loads = Loads::new(TAGS, Cache::Bypassed);
        self.sweep = Sweep::new();
        self.free = (0..SLOTS).collect();
        self.owners.clear();
        self.effects.clear();
        self.out.clear();
        self.ended.clear();
        for file in &mut self.files {
            file.window =
                open(&mut self.windows, &mut self.loads, &self.pool, file.id, &file.name, &file.bytes, file.rank)?;
        }
        self.faults(plan);
        Ok(())
    }

    fn act(&mut self, plan: &Plan, act: Act) -> Result<(), Broke> {
        match act {
            Act::Read { reader, file, rec, set } => {
                let keep = if set { Keep::Set } else { Keep::Hold };
                self.read(plan, reader, file, rec, keep)
            }
            Act::Optimistic { reader, file, rec } => self.optimistic(plan, reader, file, rec),
            Act::Advance { reader } => self.advance(plan, reader as usize % READERS),
            Act::AdvanceAll => (0..READERS).try_for_each(|i| self.advance(plan, i)),
            Act::Release { reader } => {
                let i = reader as usize % READERS;
                if matches!(self.actors[i], Actor::Holding { .. }) {
                    self.actors[i] = Actor::Idle;
                }
                Ok(())
            }
            Act::TaskEnd => {
                self.set.reset();
                self.set_held.clear();
                Ok(())
            }
            Act::Evict => match self.sweep.evict(&self.pool, &self.windows, PAGE).refused {
                Some(why) => Err(Broke::Harness(why)),
                None => Ok(()),
            },
            Act::Visit => match self.sweep.visit(&self.pool, &self.windows) {
                Visit::Full => self.sweep.give(&self.pool).map(drop).map_err(Broke::Harness),
                Visit::Taken { .. } | Visit::Passed { .. } | Visit::Empty => Ok(()),
            },
            Act::Give => self.sweep.give(&self.pool).map(drop).map_err(Broke::Harness),
            Act::Step { us } => self.step(u64::from(us)),
            Act::Rot { file, block } => {
                self.rot(file, block);
                Ok(())
            }
            Act::ReleaseReads => {
                self.sim.release_reads(Duration::ZERO);
                if plan.hold_reads {
                    self.sim.hold_reads(dir());
                }
                Ok(())
            }
            Act::Retire { file } => self.retire(plan, file),
            Act::Append { file } => self.append(plan, file),
            Act::Crash => self.crash(plan),
        }
    }

    /// Everything in flight let end: every load, every reading, the sweep's batch (the oracle's
    /// fifth rule).
    fn drain(&mut self, plan: &Plan) -> Result<(), Broke> {
        for _ in 0..DRAIN {
            let busy = self.actors.iter().any(|it| {
                matches!(it, Actor::Reading { .. } | Actor::Begun { .. } | Actor::Copied { .. } | Actor::Appending { .. })
            });
            if !busy && self.loads.in_flight() == 0 && self.sweep.batch().is_empty() {
                return Ok(());
            }
            self.sim.release_reads(Duration::ZERO);
            self.sweep.give(&self.pool).map_err(Broke::Harness)?;
            self.step(100)?;
            for i in 0..self.actors.len() {
                self.advance(plan, i)?;
            }
            self.check()?;
        }
        Err(Broke::Oracle(format!(
            "after {DRAIN} polls {} loads and some readings have not ended",
            self.loads.in_flight()
        )))
    }

    /// Every record of every file read back (the oracle's third rule): its bytes, through any
    /// failing or short reads within [`TRIES`], or refused for a block that rotted.
    fn read_back(&mut self, plan: &Plan) -> Result<(), Broke> {
        for actor in &mut self.actors {
            if matches!(actor, Actor::Holding { .. }) {
                *actor = Actor::Idle;
            }
        }
        self.set.reset();
        self.set_held.clear();
        for file in 0..self.files.len() {
            for rec in 0..self.files[file].records.len() {
                let mut read = false;
                for _ in 0..TRIES {
                    let (at, len) = self.place(file, rec);
                    self.actors[0] = Actor::Reading { file, rec, keep: Keep::Hold, reading: Reading::new(at, len) };
                    let mut ended = false;
                    for _ in 0..DRAIN {
                        self.advance(plan, 0)?;
                        self.check()?;
                        match &self.actors[0] {
                            Actor::Holding { .. } => (read, ended) = (true, true),
                            Actor::Idle => ended = true,
                            _ => {}
                        }
                        if ended {
                            break;
                        }
                        self.sim.release_reads(Duration::ZERO);
                        self.sweep.give(&self.pool).map_err(Broke::Harness)?;
                        self.step(100)?;
                    }
                    self.actors[0] = Actor::Idle;
                    if !ended {
                        return Err(Broke::Oracle(format!(
                            "a read of record {rec} of {} did not end in {DRAIN} polls",
                            self.files[file].name
                        )));
                    }
                    if read || self.rotted(file, rec) {
                        break;
                    }
                }
                if !read && !self.rotted(file, rec) {
                    return Err(Broke::Oracle(format!(
                        "record {rec} of {} never read back in {TRIES} tries, and no rot explains it",
                        self.files[file].name
                    )));
                }
            }
        }
        Ok(())
    }
}

/// `plan` run with `plant` planted, the oracle at every act, then everything let end and read
/// back, then a crash, a reopen and every record read back again.
fn run(plan: &Plan, plant: Plant) -> Result<(), Broke> {
    let mut world = World::new(plan, plant)?;
    world.check()?;
    for &act in &plan.acts {
        world.act(plan, act)?;
        world.check()?;
    }
    world.drain(plan)?;
    world.read_back(plan)?;
    world.crash(plan)?;
    world.check()?;
    world.drain(plan)?;
    world.read_back(plan)
}

// --- The tests -------------------------------------------------------------------------------

/// The invariant: whatever the faults, the schedule, the evictions and the crashes, a read hands
/// back the bytes its file holds or a refusal its faults explain, nothing pinned or loading is
/// given back, the process holds exactly the pages in use, and every load ends.
#[hegel::test(test_cases = 200)]
fn the_pool_reads_back_what_its_files_hold(tc: TestCase) {
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
        "the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}

#[test]
fn the_oracle_catches_an_eviction_that_keeps_the_version() {
    control(Plant::SameVersion);
}

#[test]
fn the_oracle_catches_an_eviction_of_a_pinned_page() {
    control(Plant::EvictPinned);
}

#[test]
fn the_oracle_catches_pages_unloaded_before_they_are_given_back() {
    control(Plant::NoEvicting);
}

#[test]
fn the_oracle_catches_a_page_loaded_as_its_read_is_asked() {
    control(Plant::LoadedAtAsk);
}

#[test]
fn the_oracle_catches_a_load_with_no_block_checks() {
    control(Plant::NoCheck);
}

#[test]
fn the_oracle_catches_a_failed_load_kept() {
    control(Plant::FailedLoadKept);
}

#[test]
fn the_oracle_catches_a_commit_that_does_not_pin_its_file_s_last_page() {
    control(Plant::UnpinnedTail);
}

/// An append with no faults, by hand: its records are not the file's until its sync completes,
/// then read back from the page the commit wrote with no read of the disk, and again from the
/// disk after the page is given back.
#[test]
fn an_append_is_the_file_s_once_its_sync_completes_and_reads_back() {
    let plan = quiet_plan();
    let mut world = World::new(&plan, Plant::None).unwrap_or_else(|broke| panic!("{broke}"));
    let before = world.files[0].records.len();
    world.append(&plan, 0).unwrap_or_else(|broke| panic!("{broke}"));
    for _ in 0..DRAIN {
        if matches!(world.actors[COMMITTER], Actor::Idle) {
            break;
        }
        assert_eq!(world.files[0].records.len(), before, "an append is not the file's before its sync");
        world.step(100).unwrap_or_else(|broke| panic!("{broke}"));
        world.advance(&plan, COMMITTER).unwrap_or_else(|broke| panic!("{broke}"));
        world.check().unwrap_or_else(|broke| panic!("{broke}"));
    }
    assert_eq!(world.files[0].records.len(), before + 1, "the append was committed");
    assert_eq!(world.files[0].window.len, world.files[0].bytes.len() as u64);
    // The new record, pinned with no load: its page is the one the commit wrote.
    let page = world.files[0].window.first_page();
    let (at, len) = world.place(0, before);
    let mut reading = Reading::new(at, len);
    match reading.step(&world.pool) {
        ReadStep::Pinned(pins) => {
            world.same(0, before, pins.bytes(), "a read of an appended record").unwrap_or_else(|broke| panic!("{broke}"));
        }
        _ => panic!("the appended record's page is not loaded as its commit ends"),
    }
    drop(reading);
    // Given back, and read again from the disk, block checks and all.
    let swept = world.sweep.evict(&world.pool, &world.windows, u64::MAX);
    assert_eq!(swept.refused, None);
    assert_eq!(world.pool.word_now(page).state(), State::Unloaded, "{swept:?}");
    world.read_back(&plan).unwrap_or_else(|broke| panic!("{broke}"));
}

/// One seeded plan, [`REPLAY`]'s: set it to a seed a failure names to run that case alone.
#[test]
fn replay_one_seed() {
    let (seed, plant) = REPLAY;
    if let Err(broke) = run(&plan_of(seed), plant) {
        panic!("seed {seed}, Plant::{plant:?}: {broke}");
    }
}

/// A plan of one file of one short record, with no acts and no faults: for the tests that step a
/// world by hand.
fn quiet_plan() -> Plan {
    let spec = Spec { sizes: vec![300], fill: 3, rank: Rank::Plain };
    Plan {
        seed: 7,
        fail_one_in: 0,
        fail_reads: false,
        short_reads: false,
        hold_reads: false,
        rot: false,
        files: vec![spec.clone()],
        spares: vec![spec],
        acts: Vec::new(),
    }
}

/// Reader 0 reads record 0 of file 0 to its end and lets its pins go.
fn read_once(world: &mut World, plan: &Plan) {
    world.read(plan, 0, 0, 0, Keep::Hold).unwrap_or_else(|broke| panic!("{broke}"));
    for _ in 0..DRAIN {
        if matches!(world.actors[0], Actor::Holding { .. }) {
            world.actors[0] = Actor::Idle;
            return;
        }
        world.step(100).unwrap_or_else(|broke| panic!("{broke}"));
        world.advance(plan, 0).unwrap_or_else(|broke| panic!("{broke}"));
    }
    panic!("a read with no faults did not end in {DRAIN} polls");
}

/// The sweep's policy (the design's section 4): a page read once is taken on the first pass after
/// its pin is let go; one pinned again, or read optimistically, since it was loaded has its lap.
#[test]
fn the_sweep_takes_a_page_read_once_and_passes_over_one_read_again() {
    let plan = quiet_plan();
    let mut world = World::new(&plan, Plant::None).unwrap_or_else(|broke| panic!("{broke}"));
    let page = world.files[0].window.first_page();
    let visit = |world: &mut World| world.sweep.visit(&world.pool, &world.windows);

    read_once(&mut world, &plan);
    assert_eq!(visit(&mut world), Visit::Taken { page }, "read once");
    world.sweep.give(&world.pool).unwrap();

    read_once(&mut world, &plan);
    read_once(&mut world, &plan);
    assert_eq!(visit(&mut world), Visit::Passed { page }, "pinned again since its load");
    assert_eq!(visit(&mut world), Visit::Taken { page }, "its lap over");
    world.sweep.give(&world.pool).unwrap();

    read_once(&mut world, &plan);
    let (at, _) = world.place(0, 0);
    let mut out = [0u8; 16];
    assert_eq!(world.pool.read_optimistic(at, &mut out), Optimistic::Read);
    world.same(0, 0, &out, "an optimistic read").unwrap_or_else(|broke| panic!("{broke}"));
    assert_eq!(visit(&mut world), Visit::Passed { page }, "read optimistically since its load");
    assert_eq!(visit(&mut world), Visit::Taken { page }, "its lap over");
    world.sweep.give(&world.pool).unwrap();
    world.check().unwrap_or_else(|broke| panic!("{broke}"));
}

/// The loader's thread end to end on a real file system: a file's records pinned through it,
/// loaded by its driver's reads and checked, every page given back by the kernel (the real
/// `Region::evict`), and pinned again, loading again.
#[test]
fn the_loader_pins_a_file_s_records_and_loads_them_again_after_eviction() {
    let root = std::env::temp_dir().join(format!("blaze-pool-loader-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("pool")).unwrap();
    let spec = Spec { sizes: vec![300, 30 << 10, 150 << 10, 5000], fill: 5, rank: Rank::Plain };
    let (bytes, records) = file_bytes(&spec);
    std::fs::write(root.join("pool/pack-0"), &bytes).unwrap();
    let pool = Pool::new(8, Kernel).unwrap();
    let mut windows = Windows::new(8);
    let window = windows.open(&pool, FileId::Pack(0), bytes.len() as u64, Rank::Plain).unwrap();
    let theirs = root.clone();
    let made = move || Ok(blaze_core::threads::Threads::new(theirs, 2));
    let loader = Loader::start(pool, made, TAGS, Cache::Kept).unwrap();
    loader.open(Opened { window, name: FileName::new("pool/pack-0").unwrap(), checks: checks_of(&bytes) });
    for round in 0..2 {
        for &(at, len) in &records {
            let pins = loader.pin(window.at(at), len).unwrap_or_else(|err| panic!("round {round}: {err}"));
            assert_eq!(pins.bytes(), &bytes[at as usize..(at + len) as usize], "round {round}, record at {at}");
        }
        let swept = Sweep::new().evict(loader.pool(), &windows, u64::MAX);
        assert_eq!(swept.refused, None, "round {round}");
        assert_eq!(loader.pool().held(), 0, "round {round}: every page given back ({swept:?})");
    }
    drop(loader);
    _ = std::fs::remove_dir_all(&root);
}
