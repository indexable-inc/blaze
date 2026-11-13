//! The store's side of its buffer pool (docs/v2/plan/design/store-pool.md, lane 4b): the pool's
//! reservation, the table of windows and the loads, which of the store's files are open in
//! them, and what a reader does with them.
//!
//! - **Packs.** A pack is opened in the pool with its blocks' checks as its tier opens
//!   ([`Hot::pack_opened`]), and retired with its file ([`Hot::pack_removed`]) once no reader
//!   can reach it: after a grace begun when the view without it was published
//!   ([`Hot::published`]), and once no pin holds a page of it.
//! - **The commit writes through the pool.** A commit's new bytes of a pack go into the pack's
//!   window at the offsets they will have on disk ([`Hot::begin_append`], then
//!   [`crate::pool::Append::put`]), are written from there, and are the pack's loaded pages
//!   once the head names them ([`Hot::committed`]).
//! - **Readers pin.** A read of a pack's bytes pins their pages ([`Side::pin`]): a hit is one
//!   compare-and-swap a page, and a miss is loaded as [`crate::Loader`] says.
//!   - `Thread`: the pool's loader ([`crate::pool::drive`]) reads on a driver of its own, and
//!     the reading task waits for its load in a span naming the pages (`store pool load`).
//!     Before a load that would take the pool past its budget, the reader runs the sweep
//!     ([`Side::make_room`]), so the pool's resident bytes stay within the budget plus what
//!     is pinned.
//!   - `Pumped`: the store has no thread, and a read cannot carry out effects. So the store
//!     loads what each pack commits as the pack opens, as effects its driver completes
//!     ([`Hot::load_whole`]), the way it asked for a mapping before; the commit's pages are
//!     loaded as they are written; and nothing is given back unless a test asks
//!     ([`Hot::evict`]). A read that still finds a page not in memory is refused, naming it.
//!   - A caller that does drive effects where it reads (a test on the simulation) can also
//!     pin through a [`Pump`] ([`Hot::pin_pack`]).
//! - **The budget** is `Tag::Pool`'s, set with every other subsystem's by
//!   `blaze_graph::admission::set_budgets` ([`crate::pool::counts::budget`]).
//!
//! The pool is built under the model checker too (`cfg(blaze_check)`), with `std`'s atomics:
//! a model run schedules the store's tables and segments, not the pool's state words.

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use blaze_core::{Cache, Completion, Effect, FileName, Tag, lock::Lock};
use blaze_reclaim::Grace;

use crate::{
    pool::{
        Append, Ended, FileId, Kernel, Load, LoadFailure, Loads, Opened, PAGE, Pins, Pool,
        PoolError, Rank, ReadStep, Reading, Run, Tail, WINDOW, Window, Windows, counts,
        drive,
        evict::{Census, Sweep, Swept, census},
        loads::DEMAND_SLOTS,
    },
    pump::{Pump, PumpError},
};

/// One pack's append in a commit: what the commit holds until its head is written.
pub(crate) type PackAppend = Append<Kernel, Arc<Pool<Kernel>>>;

/// A run of a pack's pages pinned for as long as it lives, wherever it goes.
pub(crate) type HeldPins = Pins<Kernel, Arc<Pool<Kernel>>>;

/// The load slot a pumped read loads in: a pumped store's reads are one at a time (the store
/// is its caller's alone while it pumps), so one slot serves them all.
const PUMPED_SLOT: u16 = 0;

/// The load slot a pumped store loads its packs in as they open, one stretch at a time. The
/// rest are the commit's, for the pages its appends pin.
const OPEN_SLOT: u16 = 1;

/// Pages of one stretch of a pack loaded as it opens: 16 MiB a load.
const OPEN_PAGES: u64 = 256;

/// Times a page's read is asked again as a pack opens, after it failed (the driver's error,
/// not a block's check), before the page is left out of memory and refused to its readers:
/// as many as the store's other IO is tried (`FAILURES_MOST`).
const OPEN_TRIES: u32 = crate::FAILURES_MOST;

/// Threads the loader's driver reads with where it reads on threads (kqueue, IOCP): a queue
/// deep enough for an NVMe drive.
const LOADER_THREADS: usize = 16;

/// What a sweep a reader runs gives back past what its load needs, so the misses after it
/// find room without a sweep each: a sixty-fourth of the budget, at most this.
const ROOM_SLACK: u64 = 64 << 20;

/// What prefetch may hold unread while no budget is set (a test, a tool).
const PREFETCH_UNSET: u64 = 64 << 20;

/// A pack as the pool names it: its tier, which opening of its tier it is of (another
/// writer's tier is opened afresh as its head changes, beside the one readers still read),
/// and its place in its tier's head.
fn pack_id(tier: usize, opening: u8, pack: usize) -> FileId {
    debug_assert!(tier < 1 << 8 && pack < crate::PACKS_MOST);
    FileId::Pack((tier << 24 | usize::from(opening) << 16 | pack) as u32)
}

/// The tier and opening of a pack's id.
fn tier_of(id: FileId) -> Option<(usize, u8)> {
    match id {
        FileId::Pack(id) => Some(((id >> 24) as usize, (id >> 16) as u8)),
        _ => None,
    }
}

/// Why a pinned read of a pack's bytes failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum PinError {
    /// The store has no pool: its reservation was refused, or its loader did not start.
    NoPool(String),
    /// The pack is not open in the pool, or the bytes asked for run past what it commits.
    NotCommitted { tier: usize, pack: usize, offset: u64, len: u64, committed: Option<u64> },
    /// A page of a pumped store's is not in memory, and a read of it can load nothing.
    NotLoaded { at: u64, len: u64 },
    Pool(PoolError),
    Pump(PumpError),
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinError::NoPool(why) => write!(f, "{why}"),
            PinError::NotCommitted { tier, pack, offset, len, committed: Some(committed) } => write!(
                f,
                "bytes {offset}..{} of pack {pack} of tier {tier}, which commits {committed}, are \
                 not the store pool's to read",
                offset + len
            ),
            PinError::NotCommitted { tier, pack, offset, len, committed: None } => write!(
                f,
                "bytes {offset}..{} of pack {pack} of tier {tier}, which is not open in the \
                 store's pool",
                offset + len
            ),
            PinError::NotLoaded { at, len } => write!(
                f,
                "bytes {at}..{} of the store's pool are not in memory, and this store's misses \
                 are pumped (Loader::Pumped): it loads what a pack commits as the pack opens, \
                 and a read loads nothing",
                at + len
            ),
            PinError::Pool(err) => write!(f, "{err}"),
            PinError::Pump(err) => write!(f, "{err}"),
        }
    }
}

/// `load` asked of the loads core and carried out by `pump`: how it ended.
fn pumped(
    pool: &Pool<Kernel>,
    loads: &mut Loads,
    pump: &mut dyn Pump,
    load: Load,
) -> Result<Result<(), LoadFailure>, PumpError> {
    // alloc: a pumped miss, a test's or a tool's, never the daemon's
    let (mut effects, mut ended) = (Vec::new(), Vec::new());
    loads.ask(pool, load, &mut effects, &mut ended);
    // A load of no file the core knows ended as it was asked, with no read.
    if ended.is_empty() {
        pump.pump(&mut effects, &mut |completion| {
            // Another's: the driver's other effects, or a read of a commit's own load (its
            // append's last page), which the pump keeps for the store.
            if loads.slot_of(completion.tag) != Some(load.slot) {
                return Some(completion);
            }
            loads.complete(pool, completion, &mut ended);
            None
        })?;
    }
    let Ended { outcome, .. } =
        ended.pop().expect("a pump that returns Ok has completed every read of the load");
    Ok(outcome)
}

/// The table of windows and the sweep, behind one lock: the store changes the table (a file
/// opened, grown by a commit, retired), and the sweep walks it, from the store or from a
/// reader making room for its load. No hit takes it: a reader finds its record's pages from
/// the view it loaded.
struct Gate {
    windows: Windows,
    sweep: Sweep,
}

/// What the store and its readers share of the pool.
pub(crate) struct Side {
    pool: Arc<Pool<Kernel>>,
    /// The loader's thread, of a store whose misses it loads ([`crate::Loader::Thread`]).
    loader: Option<drive::Loader<Kernel>>,
    gate: Lock<Gate>,
}

impl Side {
    /// The `len` (at least one) bytes at the pool's byte `at` pinned: in memory already (one
    /// compare-and-swap a page), or loaded now by the loader's thread while the calling task
    /// waits in a span naming the pages. The bytes stay as they are while the guard lives.
    ///
    /// # Errors
    ///
    /// The load failed (a read failed, a block failed its check: [`PoolError`] names the
    /// file and the block), or a pumped store's page is not in memory.
    pub(crate) fn pin(&self, at: u64, len: u64) -> Result<HeldPins, PinError> {
        let mut reading = Reading::new(at, len);
        match reading.step(&self.pool) {
            ReadStep::Pinned(pins) => return Ok(pins),
            ReadStep::Failed(error) => return Err(PinError::Pool(error)),
            // A miss, or a page on its way in or out: the pins taken so far go back, and
            // the loader takes the read from its first page.
            _ => reading.give_up(&*self.pool),
        }
        let Some(loader) = &self.loader else { return Err(PinError::NotLoaded { at, len }) };
        self.make_room(u64::from(Run::of(at, len).pages) * PAGE);
        loader.pin_through(&self.pool, at, len).map_err(PinError::Pool)
    }

    /// Room for `bytes` more of the pool's pages within its budget, made by the sweep where
    /// there is none: what a load does first. The sweep gives back what is neither pinned
    /// nor read since it last came by; when that is not enough the load goes on all the
    /// same, so the pool's resident bytes never pass its budget by more than what is pinned,
    /// each pack's last page a commit is about to write into, and the loads in flight.
    pub(crate) fn make_room(&self, bytes: u64) -> Option<Swept> {
        let budget = counts::budget();
        if budget == u64::MAX {
            return None;
        }
        // Every pool of the process is held to the one budget: what they hold between them
        // is the tag's live bytes.
        let held = budget.saturating_sub(blaze_mem::tag::headroom(blaze_mem::Tag::Pool));
        if held.saturating_add(bytes) <= budget {
            return None;
        }
        let span = tracing::info_span!(
            target: "blaze",
            "store pool evict",
            held,
            budget,
            load = bytes,
            freed = tracing::field::Empty,
        );
        let _in = span.enter();
        let mut gate = self.gate.lock();
        let Gate { windows, sweep } = &mut *gate;
        let swept = sweep.make_room(&*self.pool, windows, held, budget, bytes, (budget / 64).min(ROOM_SLACK))?;
        drop(gate);
        span.record("freed", &swept.freed);
        counts::evicted(swept.freed);
        Some(swept)
    }

    /// At least `bytes` of the pool's pages given back, if the sweep finds them: the pool's
    /// `evict(bytes) -> freed`.
    pub(crate) fn evict(&self, bytes: u64) -> Swept {
        let span = tracing::info_span!(
            target: "blaze",
            "store pool evict",
            asked = bytes,
            freed = tracing::field::Empty,
            visited = tracing::field::Empty,
        );
        let _in = span.enter();
        let mut gate = self.gate.lock();
        let Gate { windows, sweep } = &mut *gate;
        let swept = sweep.evict(&*self.pool, windows, bytes);
        drop(gate);
        span.record("freed", &swept.freed);
        span.record("visited", &swept.visited);
        counts::evicted(swept.freed);
        swept
    }

    /// One lap of the sweep over the pool's pages ([`Sweep::lap`]): what was not read again
    /// since the sweep last came by is given back, what was stays for one lap more.
    pub(crate) fn trim(&self) -> Swept {
        let span = tracing::info_span!(
            target: "blaze",
            "store pool trim",
            freed = tracing::field::Empty,
            visited = tracing::field::Empty,
        );
        let _in = span.enter();
        let mut gate = self.gate.lock();
        let Gate { windows, sweep } = &mut *gate;
        let swept = sweep.lap(&*self.pool, windows);
        drop(gate);
        span.record("freed", &swept.freed);
        span.record("visited", &swept.visited);
        counts::evicted(swept.freed);
        swept
    }

    /// The pool's pages as one walk of their state words finds them: what is loaded, what
    /// of it is pinned, and what a sweep could give back now.
    pub(crate) fn census(&self) -> Census {
        let gate = self.gate.lock();
        census(&*self.pool, &gate.windows)
    }

    /// The most prefetch may hold loaded and unread: the smaller of an eighth of the pool's
    /// budget and the room left in it, so a prefetch never makes a demand load sweep.
    pub(crate) fn prefetch_budget() -> u64 {
        match counts::budget() {
            u64::MAX => PREFETCH_UNSET,
            budget => (budget / 8).min(blaze_mem::tag::headroom(blaze_mem::Tag::Pool)),
        }
    }

    /// `runs`, in the order they will be read, handed to the loader as its plan, in place of
    /// the plan before: read ahead of the demand reads, a megabyte a read, only while no
    /// demand load is in flight and within the prefetch budget. Whether there is a loader
    /// to hand them to (a pumped store has loaded its packs already).
    pub(crate) fn prefetch(&self, runs: Vec<Run>) -> bool {
        let Some(loader) = &self.loader else { return false };
        counts::prefetched(runs.iter().map(|it| u64::from(it.pages) * PAGE).sum());
        loader.set_budget(Side::prefetch_budget());
        loader.prefetch(runs);
        true
    }
}

/// A pack of a pumped store being loaded whole as it opens, a stretch at a time.
struct OpenLoad {
    tier: usize,
    /// Which opening of its tier the pack is of.
    opening: u8,
    pack: usize,
    /// Why the first of its pages that could not be loaded could not.
    failed: Option<String>,
    /// Reads of the page at `next` that failed, while it is read a page at a time.
    tries: u32,
    window: Window,
    /// The pack's byte loaded up to, and where the stretch being read ends.
    next: u64,
    stretch: u64,
    /// Below it, a page a stretch: a stretch that failed is read again a page at a time,
    /// so one page that cannot be read (its block failed its check) keeps none of the
    /// others out of memory.
    single: u64,
    reading: Option<Reading>,
}

/// What a completion of one of the store's own loads brought.
pub(crate) enum Loaded {
    /// An append of the commit in flight to own pack `.0` begun, or why it cannot be.
    Append(usize, Result<PackAppend, String>),
    /// What pack `pack` of the opening `opening` of tier `tier` commits is in memory, but
    /// for the pages that failed their load, each of which a read of it is refused by:
    /// `failed` is why the first did.
    Opened { tier: usize, opening: u8, pack: usize, failed: Option<String> },
}

/// The pool, the windows of the files open in it, and the loads that read them.
struct Open {
    side: Arc<Side>,
    /// A pumped store's loads core, whose reads the store's own driver carries out. A store
    /// with a loader has none: its loader's thread holds the one core.
    loads: Option<Loads>,
    /// The load slots no commit's append holds.
    free: Vec<u16>,
    /// Appends waiting for the load of their pack's last committed page: the load's slot, the
    /// pack, and the read that asked it.
    tails: Vec<(u16, usize, Reading)>,
    /// The packs a pumped store is loading whole, the front one in flight.
    opens: VecDeque<OpenLoad>,
    /// Commits made, and for each own pack written in the last two the commit that wrote it:
    /// its last page is the sweep's to pass over ([`Tail::Hot`]).
    commits: u64,
    written: BTreeMap<usize, u64>,
    /// Which opening each tier's packs are of ([`pack_id`]).
    openings: Vec<u8>,
    /// Files removed, waiting to be retired: each with the grace begun once a view without
    /// it was published (`None` until then), after which no reader can ask for its pages.
    retiring: Vec<(FileId, Option<Grace>)>,
}

impl Open {
    /// Which opening tier `tier`'s packs are of: its first, until it is opened afresh.
    fn opening(&self, tier: usize) -> u8 {
        self.openings.get(tier).copied().unwrap_or(0)
    }

    fn id(&self, tier: usize, pack: usize) -> FileId {
        pack_id(tier, self.opening(tier), pack)
    }

    fn file_opened(&mut self, opened: Opened) {
        match (&self.side.loader, &mut self.loads) {
            (Some(loader), _) => loader.open(opened),
            (None, Some(loads)) => loads.open(opened),
            (None, None) => unreachable!("a pool with neither a loader nor a loads core"),
        }
    }

    fn file_grown(&mut self, window: Window, checks: Vec<u32>) {
        match (&self.side.loader, &mut self.loads) {
            (Some(loader), _) => loader.grow(window, checks),
            (None, Some(loads)) => loads.grow(window, checks),
            (None, None) => unreachable!("a pool with neither a loader nor a loads core"),
        }
    }

    fn file_closed(&mut self, first: u32) {
        match (&self.side.loader, &mut self.loads) {
            (Some(loader), _) => loader.close(first),
            (None, Some(loads)) => loads.close(first),
            (None, None) => unreachable!("a pool with neither a loader nor a loads core"),
        }
    }

    /// Every file waiting to be retired whose grace has passed and no page of which is
    /// pinned or loading, retired: its pages given back, its windows free. The rest wait.
    fn retire_passed(&mut self) {
        if self.retiring.is_empty() {
            return;
        }
        let side = Arc::clone(&self.side); // clone: a count, once per call with something to retire
        let mut closed = Vec::new(); // alloc: the files retired now, a few
        {
            let mut gate = side.gate.lock();
            self.retiring.retain(|&(id, grace)| {
                if !grace.is_some_and(Grace::passed) {
                    return true;
                }
                let Some(window) = gate.windows.get(id).copied() else { return false };
                match gate.windows.retire(&*side.pool, id) {
                    Ok(()) => {
                        closed.push(window.first);
                        false
                    }
                    // Pinned, loading or being given back: later.
                    Err(_) => true,
                }
            });
        }
        for first in closed {
            self.file_closed(first);
        }
    }

    /// The pinned read `reading` of the last committed page of own pack `pack` taken as far as
    /// it goes, on a pumped store: `Ok(Some)` once the page is pinned and the append begun,
    /// `Ok(None)` while its load is in flight (asked into `effects`; [`Hot::loaded`] takes it
    /// on).
    fn advance(
        &mut self,
        pack: usize,
        mut reading: Reading,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<PackAppend>, String> {
        let side = Arc::clone(&self.side); // clone: a count, once per append begun on a page not whole
        let id = self.id(0, pack);
        let mut spin = blaze_spin::SpinWait::new();
        // Bounded by the reading: it pins the page, asks a load or counts a wait each turn.
        loop {
            match reading.step(&side.pool) {
                ReadStep::Pinned(pins) => {
                    self.free.extend(reading.take_slot());
                    let window = *side
                        .gate
                        .lock()
                        .windows
                        .get(id)
                        .expect("a pack whose last page was pinned is open");
                    // clone: a count of the pool, held by the append until its commit ends
                    return Ok(Some(Append::begin(side.pool.clone(), window, Some(pins))));
                }
                ReadStep::Failed(error) => {
                    self.free.extend(reading.take_slot());
                    // alloc: the error of a commit that fails
                    return Err(error.to_string());
                }
                ReadStep::NeedSlot => match self.free.pop() {
                    Some(slot) => reading.give_slot(slot),
                    None => {
                        reading.give_up(&*side.pool);
                        // alloc: the error of a commit that fails
                        return Err(format!(
                            "every load slot of the store's pool ({DEMAND_SLOTS}) is held by a \
                             pack this commit appends to"
                        ));
                    }
                },
                ReadStep::Ask(load) => {
                    // alloc: empty unless the load ends as it is asked
                    let mut ended = Vec::new();
                    let loads = self.loads.as_mut().expect("a pumped store has its loads core");
                    loads.ask(&*side.pool, load, effects, &mut ended);
                    match ended.pop() {
                        // In no file the loads core knows: ended as it was asked, failed.
                        Some(Ended { outcome, .. }) => reading.loaded(&*side.pool, outcome),
                        None => {
                            // alloc: a commit's appends waiting on a load, a few
                            self.tails.push((load.slot, pack, reading));
                            return Ok(None);
                        }
                    }
                }
                ReadStep::Pending => unreachable!("a load in flight is kept in `tails`"),
                // The sweep is giving the page back, or a pumped read is loading it: the
                // reading bounds how often.
                ReadStep::Wait { .. } | ReadStep::Spin { .. } => {
                    if !spin.spin() {
                        spin = blaze_spin::SpinWait::new();
                    }
                }
            }
        }
    }

    /// The packs a pumped store is loading whole taken as far as they go: each one done is
    /// in `out`, and the first not done has its next stretch's read in `effects`.
    fn advance_opens(&mut self, effects: &mut Vec<Effect>, out: &mut Vec<Loaded>) {
        let side = Arc::clone(&self.side); // clone: a count, once per completion of an opening's load
        let pool: &Pool<Kernel> = &side.pool;
        let mut spin = blaze_spin::SpinWait::new();
        // Bounded: each turn ends a pack's load, moves it a stretch on, asks a read (and
        // returns while it is in flight) or counts a wait of its reading.
        while let Some(front) = self.opens.front_mut() {
            let Some(reading) = &mut front.reading else {
                if front.next >= front.window.len {
                    let done = self.opens.pop_front().expect("the front, looked at above");
                    out.push(Loaded::Opened {
                        tier: done.tier,
                        opening: done.opening,
                        pack: done.pack,
                        failed: done.failed,
                    });
                    continue;
                }
                let pages = match front.next < front.single {
                    true => 1,
                    false => OPEN_PAGES,
                };
                front.stretch = ((front.next / PAGE + pages) * PAGE).min(front.window.len);
                let mut reading =
                    Reading::new(front.window.at(front.next), front.stretch - front.next);
                reading.give_slot(OPEN_SLOT);
                front.reading = Some(reading);
                continue;
            };
            match reading.step(&side.pool) {
                ReadStep::Pinned(pins) => {
                    // In memory: nothing holds it, and nothing gives a pumped store's pages
                    // back but a test's sweep.
                    drop(pins);
                    reading.take_slot();
                    front.reading = None;
                    front.next = front.stretch;
                    front.tries = 0;
                }
                ReadStep::Failed(error) => {
                    reading.take_slot();
                    front.reading = None;
                    // alloc: the error of a pack that cannot be read, once a pack
                    front.failed.get_or_insert_with(|| error.to_string());
                    // A read the driver failed leaves its page unloaded, to be asked again; a
                    // block that failed its check, or a page abandoned, is refused for good.
                    let again = matches!(
                        error,
                        PoolError::Load(LoadFailure::Read { .. } | LoadFailure::Short { .. })
                    );
                    if front.stretch - front.next > PAGE {
                        // Again, a page at a time, so the pages around the one that failed
                        // are loaded.
                        front.single = front.stretch;
                    } else if again && front.tries + 1 < OPEN_TRIES {
                        front.tries += 1;
                    } else {
                        // The page is refused to whoever reads it, naming it.
                        front.next = front.stretch;
                        front.tries = 0;
                    }
                }
                ReadStep::NeedSlot => unreachable!("an opening's reading holds its slot"),
                ReadStep::Ask(load) => {
                    // alloc: empty unless the load ends as it is asked
                    let mut ended = Vec::new();
                    let loads = self.loads.as_mut().expect("a pumped store has its loads core");
                    loads.ask(pool, load, effects, &mut ended);
                    match ended.pop() {
                        Some(Ended { outcome, .. }) => reading.loaded(pool, outcome),
                        None => return,
                    }
                }
                ReadStep::Pending => return,
                ReadStep::Wait { .. } | ReadStep::Spin { .. } => {
                    if !spin.spin() {
                        spin = blaze_spin::SpinWait::new();
                    }
                }
            }
        }
    }
}

/// The store's hot tier.
pub(crate) struct Hot {
    open: Result<Open, String>,
}

impl Hot {
    /// The pool reserved, with nothing open in it, its misses loaded as `loader` says: by a
    /// thread of its own on this target's native driver over the loader's root, or by the
    /// store's own driver, its loads asking with tags `load_tags..load_tags + 2^32`. A
    /// reservation the system refused, or a loader that did not start, is kept as why
    /// ([`Hot::refused`]): the store stops, naming it, and nothing is read another way. The
    /// pool reserves `windows` windows ([`crate::pool::WINDOWS`] for a store; fewer for a test's).
    pub(crate) fn new(load_tags: u64, loader: &crate::Loader, windows: u32) -> Hot {
        let open = Pool::new(windows, Kernel)
            // alloc: the error that stops the store
            .map_err(|err| err.to_string())
            .and_then(|pool| {
                // alloc: the pool's count, once per store
                let pool = Arc::new(pool);
                let (loader, loads) = match loader {
                    crate::Loader::Pumped => {
                        // Around the page cache: the pool is the one copy of what it reads.
                        (None, Some(Loads::new(load_tags, Cache::Bypassed)))
                    }
                    crate::Loader::Thread { root } => {
                        // clone: the loader's thread owns its driver's root, once per store
                        let root: PathBuf = root.clone();
                        let started = drive::Loader::start_shared(
                            // clone: a count of the pool, the loader's
                            pool.clone(),
                            move || blaze_core::native(&root, LOADER_THREADS),
                            load_tags,
                            Cache::Bypassed,
                        );
                        // alloc: the error that stops the store
                        (Some(started.map_err(|err| err.to_string())?), None)
                    }
                };
                let gate = Gate { windows: Windows::new(windows), sweep: Sweep::new() };
                Ok(Open {
                    // alloc: what the store and its readers share of the pool, once per store
                    side: Arc::new(Side { pool, loader, gate: Lock::new("store.pool.windows", gate) }),
                    loads,
                    // alloc: the commit's load slots, once per store
                    free: (OPEN_SLOT + 1..DEMAND_SLOTS as u16).rev().collect(),
                    // alloc: empty, allocates nothing
                    tails: Vec::new(),
                    opens: VecDeque::new(),
                    commits: 0,
                    written: BTreeMap::new(),
                    // alloc: empty, allocates nothing
                    openings: Vec::new(),
                    // alloc: empty, allocates nothing
                    retiring: Vec::new(),
                })
            });
        Hot { open }
    }

    /// Why there is no pool, when its reservation was refused or its loader did not start.
    pub(crate) fn refused(&self) -> Option<&str> {
        self.open.as_ref().err().map(String::as_str)
    }

    /// What readers share of it.
    pub(crate) fn side(&self) -> Option<Arc<Side>> {
        // clone: a count, once per store opened or reader's view made
        self.open.as_ref().ok().map(|open| Arc::clone(&open.side))
    }

    /// Whether its misses are loaded by the store's own driver ([`crate::Loader::Pumped`]).
    pub(crate) fn pumped(&self) -> bool {
        self.open.as_ref().is_ok_and(|open| open.loads.is_some())
    }

    /// Which opening of tier `tier` its packs are of now ([`Hot::tier_closed`] begins the
    /// next).
    pub(crate) fn opening_of(&self, tier: usize) -> Option<u8> {
        self.open.as_ref().ok().map(|open| open.opening(tier))
    }

    /// Whether `tag` is one its loads ask with on the store's driver.
    pub(crate) fn owns(&self, tag: Tag) -> bool {
        self.open.as_ref().is_ok_and(|open| open.loads.as_ref().is_some_and(|it| it.owns(tag)))
    }

    /// Pack `pack` of tier `tier`, the file `file`, commits `committed` bytes whose blocks'
    /// checks are `checks`: opened in the pool, as its tier opens. The pool's byte its first
    /// byte lives at, for as long as it is open.
    ///
    /// # Errors
    ///
    /// The pool has no windows left for it: why, for the store to stop on, naming the file.
    pub(crate) fn pack_opened(
        &mut self,
        tier: usize,
        pack: usize,
        file: &FileName,
        committed: u64,
        checks: &[u32],
    ) -> Result<u64, String> {
        // clone: the error that stops the store
        let open = self.open.as_mut().map_err(|why| why.clone())?;
        let id = open.id(tier, pack);
        let side = Arc::clone(&open.side); // clone: a count, once per pack opened
        let window = side
            .gate
            .lock()
            .windows
            .open(&*side.pool, id, committed, Rank::Plain)
            // alloc: the error that stops the store
            .map_err(|err| err.to_string())?;
        // clone: the loads keep the name for their reads, and the checks to check them by
        open.file_opened(Opened { window, name: file.clone(), checks: checks.to_vec() });
        Ok(window.at(0))
    }

    /// What pack `pack` of tier `tier` commits asked into memory whole, a stretch at a time,
    /// on a pumped store: its reads go into `effects`, and [`Hot::loaded`] says once it is
    /// in. `false` when there is nothing to wait for: the store's misses are its loader's, or
    /// the pack is empty or not open.
    pub(crate) fn load_whole(&mut self, tier: usize, pack: usize, effects: &mut Vec<Effect>) -> bool {
        let Ok(open) = self.open.as_mut() else { return false };
        if open.loads.is_none() {
            return false;
        }
        let (id, opening) = (open.id(tier, pack), open.opening(tier));
        let Some(window) = open.side.gate.lock().windows.get(id).copied() else { return false };
        if window.len == 0 {
            return false;
        }
        let idle = open.opens.is_empty();
        let load =
            OpenLoad { tier, opening, pack, failed: None, tries: 0, window, next: 0, stretch: 0, single: 0, reading: None };
        // alloc: a pack a pumped store opens, until it is loaded
        open.opens.push_back(load);
        if idle {
            // alloc: empty: the pack asked just now is not done as it is asked
            let mut done = Vec::new();
            open.advance_opens(effects, &mut done);
            // Done as it was asked (no page of it could be asked for): no completion will
            // come for it, so there is nothing to wait for.
            return done.is_empty();
        }
        true
    }

    /// A commit's append to own pack `pack`, the file `file`, begun: the pack will commit
    /// `to` bytes. A pack not yet in the pool (made by this commit) is opened empty, with
    /// windows for `to`. `Ok(None)` when the page its committed bytes end in must be loaded
    /// first by the store's own driver (a pumped store): the read is asked into `effects`,
    /// and [`Hot::loaded`] gives the append once it completes. A store with a loader waits
    /// for that page here, in a span naming it.
    ///
    /// # Errors
    ///
    /// Why the append cannot begin: no pool, no windows, a pack that would outgrow the
    /// windows it has, or its last page refused (a block of it failed its check).
    pub(crate) fn begin_append(
        &mut self,
        pack: usize,
        file: &FileName,
        to: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<PackAppend>, String> {
        // clone: the error of a commit that fails
        let open = self.open.as_mut().map_err(|why| why.clone())?;
        let id = open.id(0, pack);
        let side = Arc::clone(&open.side); // clone: a count, once per pack a commit writes
        let found = side.gate.lock().windows.get(id).copied();
        let window = match found {
            Some(window) => window,
            None => {
                let window = side
                    .gate
                    .lock()
                    .windows
                    .open_room(&*side.pool, id, 0, to, Rank::Plain)
                    // alloc: the error of a commit that fails
                    .map_err(|err| err.to_string())?;
                // clone: the loads keep the name for their reads
                // alloc: empty, allocates nothing
                open.file_opened(Opened { window, name: file.clone(), checks: Vec::new() });
                window
            }
        };
        if to > u64::from(window.count) * WINDOW {
            // alloc: the error of a commit that fails
            return Err(format!(
                "it would commit {to} bytes, past the {} windows of the store's pool it was \
                 opened with at {} bytes",
                window.count, window.len
            ));
        }
        match PackAppend::tail_of(&window) {
            // clone: a count of the pool, held by the append until its commit ends
            None => Ok(Some(Append::begin(side.pool.clone(), window, None))),
            Some((at, len)) if side.loader.is_some() => {
                // alloc: the error of a commit that fails
                let pins = side.pin(at, len).map_err(|err| err.to_string())?;
                // clone: a count of the pool, held by the append until its commit ends
                Ok(Some(Append::begin(side.pool.clone(), window, Some(pins))))
            }
            Some((at, len)) => open.advance(pack, Reading::new(at, len), effects),
        }
    }

    /// A completion of one of its loads' reads, on a pumped store: each append whose pack's
    /// last page it was the load of, begun, or why it cannot be; and each pack whose
    /// committed bytes are now in memory.
    pub(crate) fn loaded(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Loaded> {
        // alloc: empty unless a load ended; then what it frees, a few
        let mut out = Vec::new();
        let Ok(open) = self.open.as_mut() else { return out };
        let side = Arc::clone(&open.side); // clone: a count, once per completion of a load
        let pool: &Pool<Kernel> = &side.pool;
        // alloc: empty unless a load ended
        let mut ended = Vec::new();
        let Some(loads) = open.loads.as_mut() else { return out };
        loads.complete(pool, completion, &mut ended);
        // A prefetch's load ended, or a demand load whose end lets one be issued: the plan
        // goes on.
        loads.pump(pool, effects, &mut ended);
        for Ended { slot, outcome } in ended {
            if slot == OPEN_SLOT {
                if let Some(reading) = open.opens.front_mut().and_then(|it| it.reading.as_mut()) {
                    reading.loaded(pool, outcome);
                }
                open.advance_opens(effects, &mut out);
                continue;
            }
            let Some(at) = open.tails.iter().position(|it| it.0 == slot) else { continue };
            let (_, pack, mut reading) = open.tails.swap_remove(at);
            reading.loaded(pool, outcome);
            match open.advance(pack, reading, effects) {
                Ok(Some(append)) => out.push(Loaded::Append(pack, Ok(append))),
                // Evicted again before it was pinned: its load is asked again.
                Ok(None) => {}
                Err(why) => out.push(Loaded::Append(pack, Err(why))),
            }
        }
        out
    }

    /// The head names what `append` wrote of own pack `pack`: its pages are the pack's loaded
    /// pages, `checks` (those of its new blocks) what their reloads are checked by. The
    /// pool's byte the pack's first byte lives at.
    pub(crate) fn committed(&mut self, pack: usize, append: PackAppend, checks: &[u32]) -> u64 {
        let open = self.open.as_mut().expect("an append is of a store with a pool");
        let id = open.id(0, pack);
        let side = Arc::clone(&open.side); // clone: a count, once per pack a commit wrote
        let window = {
            let mut gate = side.gate.lock();
            let window = append.commit(&mut gate.windows);
            gate.windows.set_tail(id, Tail::Hot);
            window
        };
        // alloc: the loads keep the new blocks' checks beside the rest
        open.file_grown(window, checks.to_vec());
        open.written.insert(pack, open.commits);
        window.at(0)
    }

    /// A commit ended: the last page of a pack not written in the last two is the sweep's
    /// again, and what was removed and can no longer be reached is retired.
    pub(crate) fn commit_ended(&mut self) {
        let Ok(open) = self.open.as_mut() else { return };
        open.commits += 1;
        let opening = open.opening(0);
        let commits = open.commits;
        {
            let mut gate = open.side.gate.lock();
            open.written.retain(|&pack, &mut at| {
                let hot = at + 2 >= commits;
                if !hot {
                    gate.windows.set_tail(pack_id(0, opening, pack), Tail::Cold);
                }
                hot
            });
        }
        open.retire_passed();
    }

    /// Pack `pack` of tier `tier` is removed: its pages are given back and its windows freed
    /// once no reader can reach it ([`Hot::published`]). One never opened is nothing to do.
    pub(crate) fn pack_removed(&mut self, tier: usize, pack: usize) {
        let Ok(open) = self.open.as_mut() else { return };
        let id = open.id(tier, pack);
        let known = open.side.gate.lock().windows.get(id).is_some();
        if !known || open.retiring.iter().any(|it| it.0 == id) {
            return;
        }
        // alloc: a pack removed, until it is retired
        open.retiring.push((id, None));
        if tier == 0 {
            open.written.remove(&pack);
        }
    }

    /// Every pack of tier `tier` as it is open now is removed, and the tier's packs opened
    /// from here on are of its next opening: another writer's store, whose head changed, is
    /// opened afresh beside the packs readers still read of it.
    ///
    /// # Errors
    ///
    /// The tier has been opened afresh 255 times over packs that are still pinned: why.
    pub(crate) fn tier_closed(&mut self, tier: usize) -> Result<(), String> {
        let Ok(open) = self.open.as_mut() else { return Ok(()) };
        let now = open.opening(tier);
        let ids: Vec<FileId> = {
            let gate = open.side.gate.lock();
            // alloc: a tier opened afresh, its packs listed once
            gate.windows.files().map(|it| it.id).filter(|&id| tier_of(id) == Some((tier, now))).collect()
        };
        if ids.is_empty() {
            return Ok(());
        }
        for id in ids {
            if !open.retiring.iter().any(|it| it.0 == id) {
                // alloc: a pack removed, until it is retired
                open.retiring.push((id, None));
            }
        }
        if tier == 0 {
            open.written.clear();
        }
        // The next opening no file of the tier still holds a window under.
        let held: Vec<u8> = {
            let gate = open.side.gate.lock();
            // alloc: a tier opened afresh, the openings of it still held
            gate.windows.files().filter_map(|it| tier_of(it.id)).filter(|it| it.0 == tier).map(|it| it.1).collect()
        };
        let next = (1..=u8::MAX).map(|step| now.wrapping_add(step)).find(|it| !held.contains(it));
        match next {
            Some(next) => {
                if open.openings.len() <= tier {
                    // alloc: once per tier opened afresh, a byte
                    open.openings.resize(tier + 1, 0);
                }
                open.openings[tier] = next;
                Ok(())
            }
            // alloc: the error that stops the opening
            None => Err(format!(
                "tier {tier} of the store has been opened afresh 255 times over packs readers \
                 still pin in its pool: none of its openings is free"
            )),
        }
    }

    /// A view was published: what was removed before it is out of every view a task that
    /// begins from now on can load, so its grace begins; and what was removed before an
    /// earlier one and has passed its grace is retired.
    pub(crate) fn published(&mut self) {
        let Ok(open) = self.open.as_mut() else { return };
        if open.retiring.is_empty() {
            return;
        }
        let mut grace = None;
        for it in &mut open.retiring {
            if it.1.is_none() {
                it.1 = Some(*grace.get_or_insert_with(Grace::start));
            }
        }
        open.retire_passed();
    }

    /// The `len` (at least one) bytes at `offset` of pack `pack` of tier `tier` pinned: in
    /// memory already, or loaded now by `pump`, each block checked as it is. The bytes stay as
    /// they are while the guard lives.
    ///
    /// A pump that stalls leaves its load in flight: the pages it claimed stay `Loading`, and a
    /// later read of them ends starved, naming the page. The store's driver has stopped; so has
    /// the store.
    pub(crate) fn pin_pack(
        &mut self,
        pump: &mut dyn Pump,
        tier: usize,
        pack: usize,
        offset: u64,
        len: u64,
    ) -> Result<Pins<Kernel, &Pool<Kernel>>, PinError> {
        // clone: the error of a read
        let open = self.open.as_mut().map_err(|why| PinError::NoPool(why.clone()))?;
        let id = open.id(tier, pack);
        let found = match open.retiring.iter().any(|it| it.0 == id) {
            true => None,
            false => open.side.gate.lock().windows.get(id).copied(),
        };
        let committed = found.map(|it| it.len);
        let window = found
            .filter(|it| len >= 1 && offset.checked_add(len).is_some_and(|end| end <= it.len))
            .ok_or(PinError::NotCommitted { tier, pack, offset, len, committed })?;
        let pool: &Pool<Kernel> = &open.side.pool;
        let Some(loads) = open.loads.as_mut() else {
            return Err(PinError::NoPool(
                "the store's misses are its loader's (Loader::Thread): it has no loads core to pump".into(),
            ));
        };
        let mut reading = Reading::new(window.at(offset), len);
        let mut spin = blaze_spin::SpinWait::new();
        // Bounded by the reading: it pins a page or asks a load each turn, and counts its
        // waits to `pool::WAITS_MOST`.
        loop {
            match reading.step(&pool) {
                ReadStep::Pinned(pins) => {
                    reading.take_slot();
                    return Ok(pins);
                }
                ReadStep::Failed(error) => {
                    reading.take_slot();
                    return Err(PinError::Pool(error));
                }
                ReadStep::NeedSlot => reading.give_slot(PUMPED_SLOT),
                ReadStep::Ask(load) => match pumped(pool, loads, pump, load) {
                    Ok(outcome) => reading.loaded(pool, outcome),
                    Err(stalled) => {
                        // Its load is in flight for good: its claim and its slot go with it.
                        std::mem::forget(reading);
                        return Err(PinError::Pump(stalled));
                    }
                },
                ReadStep::Pending => {
                    unreachable!("a pumped load has ended by the time it is looked at")
                }
                // A page being given back, or loading in a pump that stalled: the reading
                // bounds how often.
                ReadStep::Wait { .. } | ReadStep::Spin { .. } => {
                    if !spin.spin() {
                        spin = blaze_spin::SpinWait::new();
                    }
                }
            }
        }
    }

    /// `runs` handed to a pumped store's loads core as its prefetch plan, within `budget`
    /// bytes prefetched and not yet read, and issued as far as the plan's rules let it: the
    /// reads go into `effects`, and [`Hot::loaded`] takes their completions and issues the
    /// rest. How many loads it issued now. (A store with a loader plans through
    /// [`Side::prefetch`].)
    pub(crate) fn prefetch_pumped(&mut self, runs: Vec<Run>, budget: u64, effects: &mut Vec<Effect>) -> u32 {
        let Ok(open) = self.open.as_mut() else { return 0 };
        let pool: &Pool<Kernel> = &open.side.pool;
        let Some(loads) = open.loads.as_mut() else { return 0 };
        counts::prefetched(runs.iter().map(|it| u64::from(it.pages) * PAGE).sum());
        loads.set_budget(budget);
        loads.prefetch(pool, runs);
        // alloc: empty: a prefetch's load has no asker to tell
        let mut ended = Vec::new();
        loads.pump(pool, effects, &mut ended)
    }

    /// At least `bytes` of the pool's pages given back, if the sweep finds them: the pool's
    /// `evict(bytes) -> freed`.
    pub(crate) fn evict(&mut self, bytes: u64) -> Option<Swept> {
        let open = self.open.as_mut().ok()?;
        Some(open.side.evict(bytes))
    }

    /// One lap of the sweep ([`Side::trim`]).
    pub(crate) fn trim(&mut self) -> Option<Swept> {
        let open = self.open.as_mut().ok()?;
        Some(open.side.trim())
    }

    /// The pool's census ([`Side::census`]).
    pub(crate) fn census(&self) -> Option<Census> {
        self.open.as_ref().ok().map(|open| open.side.census())
    }

    /// The committed length the pool holds of pack `pack` of tier `tier`: a test's look.
    #[cfg(test)]
    pub(crate) fn pack_open(&self, tier: usize, pack: usize) -> Option<u64> {
        let open = self.open.as_ref().ok()?;
        let id = open.id(tier, pack);
        if open.retiring.iter().any(|it| it.0 == id) {
            return None;
        }
        open.side.gate.lock().windows.get(id).map(|it| it.len)
    }
}
