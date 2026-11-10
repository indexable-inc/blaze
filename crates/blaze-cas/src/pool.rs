//! The store's hot tier: a buffer pool of blaze's own, in the style of vmcache
//! (docs/v2/plan/design/store-pool.md). This module is its shared state and what a reader does
//! with it; the loads core ([`loads`]), the sweep ([`evict`]) and the loader's thread
//! ([`drive`]) are its submodules.
//!
//! - **Layout.** One reservation of address space, [`Pool::new`]: a sparse region tagged
//!   `Tag::Pool`, carved into windows of [`WINDOW`] bytes. Every file read in place gets a window,
//!   or a run of them ([`Windows`]), and its byte `at` lives at `base + window * WINDOW + at`, so
//!   an evicted page comes back at the same address and a record over several pages is one slice.
//! - **The state word.** One `AtomicU64` per page of the reservation ([`Word`]), in a second
//!   sparse region (tagged `Tag::Store`, counted by the prefix of windows ever used, so
//!   `Tag::Pool` is exactly the pool's resident pages). Every transition is one compare-and-swap.
//! - **Pinned reads** ([`Reading`], [`Pins`], [`PinSet`]) hand out a reference: a pin is a
//!   count in the word, not a thread's slot, so it may be held across a fiber wait and moved.
//! - **Optimistic reads** ([`Pool::read_optimistic`]) copy bytes out with
//!   [`blaze_mem::racy::copy_out`] and check the word did not move: a seqlock that writes a
//!   shared line once a page a lap of the sweep ([`Pool::touch`]) and nothing after.
//!
//! Records are immutable, so no page is ever dirty: a page is loaded by an explicit read into its
//! slot ([`blaze_core::Effect::ReadInto`]) that its asker yields on, checked block by block
//! (CRC32C per 4 KiB) before it is `Loaded`, and given back by the sweep with `MADV_DONTNEED`
//! or `MADV_FREE_REUSABLE` ([`blaze_mem::Region::evict`]), never written back.
//!
//! What the kernel does to the pages (making them the process's again, giving them back) goes
//! through [`Give`], a type parameter: [`Kernel`] in a build, the DST's poisoning stand-in in
//! its test. So the cores reach no system call of their own (rule `sans-io-cores`).
//!
//! A commit writes through the pool too ([`append`]): its new bytes go into their file's window,
//! are written from there, and are the file's loaded pages once the commit is durable.
//!
//! Prefetch is the pool's own ([`prefetch`], and the loads core's queue): the same reads into the
//! same slots, issued only while no demand load is in flight and within a budget of pages
//! prefetched and not yet read, each page counted once as loaded and once as hit, late or wasted
//! ([`Pool::prefetch_counts`]).
//!
//! Proof: `pool/tests.rs`, a deterministic simulation test of the real word, reading, loads
//! core, sweep, append and prefetch under `blaze_core::sim::Sim`, with planted bugs 1 to 6, 8
//! and 9 of the design's section 7 and a tenth of this module's ([`Plant`]).

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fmt,
    marker::PhantomData,
    ops::{Deref, Range},
    sync::atomic::{AtomicU64, Ordering, fence},
};

use blaze_core::{Registered, Span, Spans};
use blaze_ids::ContentId;
use blaze_mem::{MemError, Region};

pub(crate) mod append;
pub(crate) mod drive;
pub(crate) mod evict;
pub(crate) mod loads;
pub(crate) mod prefetch;
#[cfg(all(test, not(blaze_check)))]
mod tests;

pub(crate) use append::Append;
pub(crate) use loads::{Ended, Load, LoadFailure, Loads, Opened};

/// A page of the pool: 64 KiB, 16 base pages on Linux, 4 on macOS arm64, the image's default
/// group, and a size an NVMe drive reads at full speed in one command.
pub(crate) const PAGE: u64 = 64 << 10;

/// A window: the address range one store file's bytes live in, as long as a pack may grow
/// (`PACK_MOST`). A file longer than that (a record with a pack of its own) takes a run of them.
pub(crate) const WINDOW: u64 = crate::PACK_MOST;

/// Pages of one window.
pub(crate) const PER_WINDOW: u64 = WINDOW / PAGE;

/// The block a page is checked by on load: the store's (`disk::BLOCK`), which every commit pads
/// its packs to, so a block never changes once committed.
pub(crate) const BLOCK: u64 = crate::disk::BLOCK;

/// The windows of a store's reservation: 4 TiB of address space at 1 GiB a window.
pub(crate) const WINDOWS: u32 = 4096;

/// Waits and spins one reading takes before it ends as [`PoolError::Starved`]: far more than a
/// page is ever evicted and loaded again under one reader (rule `bounded-progress`).
pub(crate) const WAITS_MOST: u32 = 1024;

/// Runs a [`PinSet`] holds before a read pins on its own.
pub(crate) const PIN_SET: usize = 32;

const _: () = assert!(WINDOW % PAGE == 0 && PAGE % BLOCK == 0 && PER_WINDOW >= 1);

// --- The state word ----------------------------------------------------------------------------

/// What a page is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    /// Not in memory: the one asker that moves it to `Loading` loads it.
    Unloaded,
    /// A read into its slot is asked or in flight: no one reads, writes or gives back its bytes.
    Loading,
    /// Its bytes are its file's, checked: pins and optimistic reads take it.
    Loaded,
    /// The sweep took it and is giving it back: no pin succeeds, and a load waits.
    Evicting,
    /// A block of it failed its check: refused every time it is read, until its file is retired.
    Corrupt,
    /// A read into it failed with the driver's ring gone ([`blaze_core::ABANDONED_SPAN`]): the kernel
    /// may still write it, so it is never loaded, given back or reused again.
    Abandoned,
}

/// Whether a page's second-chance bit is set, so the sweep passes it over once. A load leaves it
/// clear, and the first pin or optimistic read that finds the page loaded sets it: a page read
/// once is the sweep's on its first pass, one read again has its lap (the design's section 4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Referenced {
    Set,
    Clear,
}

/// A page's state word: what `State` it is, its pins, its CLOCK bit, whether a prefetch loaded it,
/// and a version every eviction raises.
///
/// | bits | field |
/// |---|---|
/// | 0..20 | `pins` while `Loaded`; the load's slot while `Loading` |
/// | 20..23 | `state` |
/// | 23 | `referenced` |
/// | 24 | `prefetched` |
/// | 25..64 | `version` |
///
/// The low field holds the load's slot while the page is `Loading` (no pin can be held then), so
/// a reader that finds a page another is loading knows which load to wait on with no table to
/// search. A zeroed word is `Unloaded` at version 0: a window's fresh words need no setup.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Word(pub(crate) u64);

impl Word {
    const LOW: u64 = (1 << 20) - 1;
    const STATE_SHIFT: u32 = 20;
    const STATE: u64 = 0b111 << Word::STATE_SHIFT;
    const REFERENCED: u64 = 1 << 23;
    const PREFETCHED: u64 = 1 << 24;
    const VERSION_SHIFT: u32 = 25;
    const VERSION: u64 = (1 << (64 - Word::VERSION_SHIFT)) - 1;
    /// The most pins a page holds: one more is a bug, and panics naming the page.
    pub(crate) const MOST_PINS: u32 = Word::LOW as u32;

    fn of(state: State, version: u64, low: u64) -> Word {
        let code = match state {
            State::Unloaded => 0,
            State::Loading => 1,
            State::Loaded => 2,
            State::Evicting => 3,
            State::Corrupt => 4,
            State::Abandoned => 5,
        };
        debug_assert!(low <= Word::LOW);
        Word((version & Word::VERSION) << Word::VERSION_SHIFT | code << Word::STATE_SHIFT | low)
    }

    pub(crate) fn unloaded(version: u64) -> Word {
        Word::of(State::Unloaded, version, 0)
    }

    pub(crate) fn loading(version: u64, slot: u16) -> Word {
        Word::of(State::Loading, version, u64::from(slot))
    }

    pub(crate) fn loaded(version: u64, pins: u32, referenced: Referenced) -> Word {
        let word = Word::of(State::Loaded, version, u64::from(pins));
        match referenced {
            Referenced::Set => Word(word.0 | Word::REFERENCED),
            Referenced::Clear => word,
        }
    }

    pub(crate) fn evicting(version: u64) -> Word {
        Word::of(State::Evicting, version, 0)
    }

    pub(crate) fn corrupt(version: u64) -> Word {
        Word::of(State::Corrupt, version, 0)
    }

    pub(crate) fn abandoned(version: u64) -> Word {
        Word::of(State::Abandoned, version, 0)
    }

    /// A page a prefetch loaded: `Loaded` with no pin, its second-chance bit clear, and marked
    /// until its first read.
    pub(crate) fn prefetched_page(version: u64) -> Word {
        Word(Word::of(State::Loaded, version, 0).0 | Word::PREFETCHED)
    }

    pub(crate) fn state(self) -> State {
        match (self.0 & Word::STATE) >> Word::STATE_SHIFT {
            0 => State::Unloaded,
            1 => State::Loading,
            2 => State::Loaded,
            3 => State::Evicting,
            4 => State::Corrupt,
            5 => State::Abandoned,
            code => panic!("a pool page's state word {:#x} holds state code {code}, which no state has", self.0),
        }
    }

    /// Pins held, of a `Loaded` page (of an `Evicting` one only under a planted bug).
    pub(crate) fn pins(self) -> u32 {
        match self.state() {
            State::Loading => 0,
            _ => (self.0 & Word::LOW) as u32,
        }
    }

    /// The load slot of a `Loading` page.
    pub(crate) fn slot(self) -> u16 {
        (self.0 & Word::LOW) as u16
    }

    pub(crate) fn version(self) -> u64 {
        self.0 >> Word::VERSION_SHIFT
    }

    /// The version an eviction moves it to.
    pub(crate) fn next_version(self) -> u64 {
        (self.version() + 1) & Word::VERSION
    }

    pub(crate) fn referenced(self) -> bool {
        self.0 & Word::REFERENCED != 0
    }

    pub(crate) fn prefetched(self) -> bool {
        self.0 & Word::PREFETCHED != 0
    }

    /// The same word, its CLOCK bit cleared.
    pub(crate) fn unreferenced(self) -> Word {
        Word(self.0 & !Word::REFERENCED)
    }
}

impl fmt::Debug for Word {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state();
        write!(f, "{state:?} v{}", self.version())?;
        match state {
            State::Loading => write!(f, " slot {}", self.slot())?,
            State::Loaded | State::Evicting => write!(f, " pins {}", self.pins())?,
            State::Unloaded | State::Corrupt | State::Abandoned => {}
        }
        if self.referenced() {
            write!(f, " referenced")?;
        }
        if self.prefetched() {
            write!(f, " prefetched")?;
        }
        Ok(())
    }
}

// --- Planted bugs ------------------------------------------------------------------------------

/// A safeguard of the pool's removed, for the DST's controls (the design's section 7, bugs 1 to
/// 6, 8 and 9, and a tenth found by the DST itself): each is caught by the oracle within its case budget, or the oracle is too weak. Only a
/// test's pool plants one ([`Pool::planted`] is `false` in every build of blaze).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    #[cfg(test)]
    None,
    /// 1: an eviction keeps the version, so a stale optimistic copy validates.
    SameVersion,
    /// 2: an eviction ignores pins.
    EvictPinned,
    /// 3: `Loaded -> Unloaded` before the pages are given back (no `Evicting`), so a reload's bytes
    /// are dropped.
    NoEvicting,
    /// 4: `Loading -> Loaded` published as the read is asked, before it completes.
    LoadedAtAsk,
    /// 5: the block checks are skipped on load, so a torn or rotted block is served.
    NoCheck,
    /// 6: a failed read leaves its pages `Loaded`.
    FailedLoadKept,
    /// 8: a commit writes into its file's last committed page without pinning it, so the sweep
    /// gives the page back in the middle of the write.
    UnpinnedTail,
    /// 9: a prefetch read is issued while a demand load is in flight.
    PrefetchFirst,
    /// 10 (not in the design's list): a load's block checks stop at the first block that fails,
    /// so a later page of the same read with a rotted block is published and served.
    FirstBadBlock,
}

// --- Counting prefetched pages -------------------------------------------------------------------

/// What a pin found of its page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Pin {
    Plain,
    /// It is the first read of a page a prefetch loaded: the pin cleared the page's mark.
    Prefetched,
}

/// How a prefetched page is counted: once as `Loaded`, then exactly once as one of the others.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Prefetch {
    /// A prefetch read it and published it, marked.
    Loaded,
    /// Its first read found it loaded.
    Hit,
    /// Its first read waited on the prefetch's load: asked too late to save the wait, and the
    /// read was still not issued twice.
    Late,
    /// The sweep or a retirement took it unread.
    Wasted,
}

/// The pool's count of prefetched pages. Each is written once in a prefetched page's life (never
/// on a hit of a page already read), by whoever moves the page's word.
#[derive(Default)]
struct Prefetched {
    loaded: AtomicU64,
    hit: AtomicU64,
    late: AtomicU64,
    wasted: AtomicU64,
}

/// [`Pool::prefetch_counts`]: pages, since the pool was made.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct PrefetchCounts {
    pub(crate) loaded: u64,
    pub(crate) hit: u64,
    pub(crate) late: u64,
    pub(crate) wasted: u64,
}

impl PrefetchCounts {
    /// Pages prefetched and not yet read or taken: those still marked. (Saturating: the four are
    /// read one after another while other threads count.)
    pub(crate) fn unread(self) -> u64 {
        self.loaded.saturating_sub(self.hit + self.late + self.wasted)
    }
}

// --- Giving pages to and taking them from the kernel -------------------------------------------

/// What the pool asks of the kernel for its pages: a type parameter, so the cores that call it
/// reach no system call of their own, and the DST stands in for it.
pub(crate) trait Give: Sized {
    /// Makes `[from, from + len)` of the pool's pages the process's again before a load writes
    /// them ([`Region::reuse`]).
    fn reuse(&self, pool: &Pool<Self>, from: usize, len: usize) -> Result<(), MemError>;

    /// Gives `ranges` of the pool's pages back to the kernel, in order, and says how many bytes went:
    /// those of a prefix of the ranges, each whole ([`Region::evict`]).
    ///
    /// # Safety
    ///
    /// Every page of the ranges is `Evicting` with no pin, so nothing holds a reference into it, and
    /// each was made the process's by [`Give::reuse`] and not given back since.
    unsafe fn evict(&self, pool: &Pool<Self>, ranges: &[Range<usize>]) -> Result<usize, MemError>;
}

/// The kernel's: [`Region::reuse`] and [`Region::evict`] of the pool's pages (Linux
/// `process_madvise(MADV_DONTNEED)` of the batch, macOS `MADV_FREE_REUSABLE` per range).
pub(crate) struct Kernel;

impl Give for Kernel {
    fn reuse(&self, pool: &Pool<Self>, from: usize, len: usize) -> Result<(), MemError> {
        pool.pages.reuse(from, len)
    }

    unsafe fn evict(&self, pool: &Pool<Self>, ranges: &[Range<usize>]) -> Result<usize, MemError> {
        // SAFETY: the caller's contract is `Region::evict`'s: nothing references the pages, and
        // each was reused and not given back since.
        unsafe { pool.pages.evict(ranges) }
    }
}

// --- The pool ----------------------------------------------------------------------------------

/// The pool's reservation, its pages' state words, and its registration for reads into it.
pub(crate) struct Pool<G: Give> {
    /// Dropped first: no span of the pages resolves once they are unmapped.
    spans: Registered,
    pages: Region,
    words: Region,
    /// Pages of the reservation, each with a word.
    total: u64,
    give: G,
    /// Give-backs the kernel refused outside the sweep's batches (an append given up): memory
    /// kept, counted.
    refused: AtomicU64,
    prefetched: Prefetched,
    #[cfg(test)]
    plant: Plant,
}

impl<G: Give> Pool<G> {
    /// A pool of `windows` windows ([`WINDOWS`] in a store), its pages and words reserved and nothing
    /// in memory, registered for reads into it.
    pub(crate) fn new(windows: u32, give: G) -> Result<Pool<G>, PoolError> {
        let total = u64::from(windows) * PER_WINDOW;
        let reserving = |what: &'static str, err: MemError| PoolError::Reserve {
            what,
            windows,
            // alloc: an error path
            why: err.to_string(),
        };
        let pages = Region::reserve_sparse((total * PAGE) as usize, blaze_mem::Tag::Pool)
            .map_err(|err| reserving("its pages", err))?;
        let words = Region::reserve_sparse(total as usize * size_of::<u64>(), blaze_mem::Tag::Store)
            .map_err(|err| reserving("its pages' state words", err))?;
        // SAFETY: the pages are this pool's, every byte readable and writable for the region's
        // life (a sparse reservation), and the registration is dropped before the region (field
        // order). Reads into a span are asked only for pages `Loading`, which nothing else
        // touches, and the pool is dropped only once its loader has none in flight.
        let spans = unsafe { Spans::register("the store's pool", pages.base(), pages.reserved()) }
            // alloc: an error path
            .map_err(|full| PoolError::Reserve { what: "a registration", windows, why: full.to_string() })?;
        #[cfg(test)]
        let plant = Plant::None;
        let (refused, prefetched) = (AtomicU64::new(0), Prefetched::default());
        Ok(Pool { spans, pages, words, total, give, refused, prefetched, #[cfg(test)] plant })
    }

    /// A test's pool with `plant` planted.
    #[cfg(test)]
    pub(crate) fn planted_with(windows: u32, give: G, plant: Plant) -> Result<Pool<G>, PoolError> {
        let mut pool = Pool::new(windows, give)?;
        pool.plant = plant;
        Ok(pool)
    }

    /// Whether `plant` is planted: never outside a test.
    #[inline(always)]
    pub(crate) fn planted(&self, plant: Plant) -> bool {
        #[cfg(test)]
        let planted = self.plant == plant;
        #[cfg(not(test))]
        let planted = {
            _ = plant;
            false
        };
        planted
    }

    pub(crate) fn give(&self) -> &G {
        &self.give
    }

    /// Pages of the reservation.
    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// The bytes of its pages the process holds now ([`Kernel`]'s count; a stand-in counts its own).
    pub(crate) fn held(&self) -> usize {
        self.pages.held()
    }

    /// The word of `page`.
    #[allow(unsafe_code, reason = "the state words are atomics laid over a reserved region")]
    pub(crate) fn word(&self, page: u64) -> &AtomicU64 {
        assert!(page < self.total, "page {page} of a store pool of {} pages", self.total);
        // SAFETY: the words region is a sparse reservation of `total` words, every byte readable
        // and writable for its life (which `&self` bounds); the offset is inside it and 8-aligned
        // (the region is page-aligned); every access to a word is atomic.
        unsafe {
            AtomicU64::from_ptr(self.words.base().as_ptr().add(page as usize * size_of::<u64>()).cast())
        }
    }

    /// `page`'s word now.
    pub(crate) fn word_now(&self, page: u64) -> Word {
        Word(self.word(page).load(Ordering::Acquire))
    }

    /// The span of `len` bytes from `page`'s first, for a read into it.
    pub(crate) fn span(&self, page: u64, len: u32) -> Span {
        self.spans.span(page * PAGE, len)
    }

    /// The span of `len` bytes from the pool's byte `at`, for a write from it.
    pub(crate) fn span_at(&self, at: u64, len: u32) -> Span {
        self.spans.span(at, len)
    }

    /// Give-backs the kernel refused outside the sweep's batches, so far.
    pub(crate) fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    fn count_refused(&self) {
        self.refused.fetch_add(1, Ordering::Relaxed);
    }

    /// Prefetched pages so far, and how each ended: `loaded = hit + late + wasted + unread`.
    pub(crate) fn prefetch_counts(&self) -> PrefetchCounts {
        let of = |it: &AtomicU64| it.load(Ordering::Relaxed);
        let counts = &self.prefetched;
        PrefetchCounts { loaded: of(&counts.loaded), hit: of(&counts.hit), late: of(&counts.late), wasted: of(&counts.wasted) }
    }

    /// `pages` prefetched pages counted as `what`, by whoever just moved their words.
    pub(crate) fn count_prefetch(&self, what: Prefetch, pages: u64) {
        let counts = &self.prefetched;
        let count = match what {
            Prefetch::Loaded => &counts.loaded,
            Prefetch::Hit => &counts.hit,
            Prefetch::Late => &counts.late,
            Prefetch::Wasted => &counts.wasted,
        };
        count.fetch_add(pages, Ordering::Relaxed);
    }

    /// The address of the pool's byte `at`.
    fn addr(&self, at: u64) -> *mut u8 {
        assert!(at <= self.total * PAGE, "byte {at} of a store pool of {} pages", self.total);
        self.pages.base().as_ptr().wrapping_add(at as usize)
    }

    /// The words region counts the prefix of `windows` windows' words as the store's.
    fn words_used(&self, windows: u32) -> Result<(), MemError> {
        self.words.ensure(windows as usize * PER_WINDOW as usize * size_of::<u64>())
    }

    // --- Pins ---

    /// One pin of `page`, if it is `Loaded`: one compare-and-swap that adds to its pins, sets its
    /// second-chance bit and clears its `prefetched` mark, and whether it cleared one (the caller
    /// counts the page, [`Pool::count_prefetch`]). Otherwise the word that refused it.
    pub(crate) fn try_pin(&self, page: u64) -> Result<Pin, Word> {
        let word = self.word(page);
        let mut seen = word.load(Ordering::Acquire);
        // Lock-free, not a wait: a compare that fails means another pin or the sweep moved the
        // word, so the loop ends once the word holds still for one compare.
        loop {
            let now = Word(seen);
            if now.state() != State::Loaded {
                return Err(now);
            }
            assert!(
                now.pins() < Word::MOST_PINS,
                "page {page} of the store's pool has {} pins: one more would overflow its word",
                now.pins()
            );
            let next = Word::loaded(now.version(), now.pins() + 1, Referenced::Set);
            match word.compare_exchange_weak(seen, next.0, Ordering::Acquire, Ordering::Acquire) {
                Ok(_) if now.prefetched() => return Ok(Pin::Prefetched),
                Ok(_) => return Ok(Pin::Plain),
                Err(moved) => seen = moved,
            }
        }
    }

    /// One pin of `page` given back: one `fetch_sub`.
    pub(crate) fn unpin(&self, page: u64) {
        let before = Word(self.word(page).fetch_sub(1, Ordering::Release));
        // An `Evicting` page holds pins only under a planted bug (`Plant::EvictPinned`). The
        // DST's oracle names it as the page is given back, and its run ends there with its
        // guards lost, not given back (`Pins::lost`), so no pin of that page comes here after.
        assert!(
            matches!(before.state(), State::Loaded | State::Evicting) && before.pins() > 0,
            "a pin of page {page} of the store's pool given back, whose word is {before:?}: given back twice"
        );
    }

    /// Pins of every page of `run` given back.
    pub(crate) fn unpin_run(&self, run: Run) {
        for page in run.pages() {
            self.unpin(page);
        }
    }

    /// `Unloaded -> Loading` of `page` for the load in `slot`: `Ok` when this asker won it and loads it,
    /// else the word that refused it.
    pub(crate) fn claim(&self, page: u64, slot: u16) -> Result<(), Word> {
        let word = self.word(page);
        let seen = Word(word.load(Ordering::Acquire));
        if seen.state() != State::Unloaded {
            return Err(seen);
        }
        let next = Word::loading(seen.version(), slot);
        word.compare_exchange(seen.0, next.0, Ordering::AcqRel, Ordering::Acquire).map(drop).map_err(Word)
    }

    /// A claimed page given up before its read was asked: `Loading -> Unloaded`.
    fn unclaim(&self, page: u64) {
        let word = self.word_now(page);
        assert_eq!(word.state(), State::Loading, "page {page} of the store's pool given up unclaimed: {word:?}");
        self.word(page).store(Word::unloaded(word.version()).0, Ordering::Release);
    }

    /// `page`, which no file commits yet, taken for a commit's append ([`Append`]): made the
    /// process's and moved `Unloaded -> Loaded` with the commit's pin, its second-chance bit set.
    /// Its bytes are anything until the commit writes them; no reader can ask for it and the
    /// sweep does not visit it until its file commits it ([`Windows::grow`]).
    ///
    /// # Errors
    ///
    /// [`PoolError::NotFresh`] when the page is not `Unloaded` (nothing changed), and
    /// [`PoolError::Reuse`] when the kernel refused it.
    pub(crate) fn fresh(&self, page: u64) -> Result<(), PoolError> {
        let seen = self.word_now(page);
        if seen.state() != State::Unloaded {
            return Err(PoolError::NotFresh { page, word: seen });
        }
        self.reuse_run(Run { first: page, pages: 1 }).map_err(|err| PoolError::Reuse {
            page,
            pages: 1,
            // alloc: an error path
            why: err.to_string(),
        })?;
        let next = Word::loaded(seen.version(), 1, Referenced::Set);
        // Nothing else moves the word of a page no file commits: the swap only says so.
        self.word(page)
            .compare_exchange(seen.0, next.0, Ordering::AcqRel, Ordering::Acquire)
            .map(drop)
            .map_err(|now| PoolError::NotFresh { page, word: Word(now) })
    }

    /// `bytes` written at the pool's byte `at` with relaxed atomic stores: a commit's bytes,
    /// into pages it pins.
    ///
    /// # Safety
    ///
    /// Every page the bytes touch is pinned by the caller, and no file commits any byte of
    /// them: no reader holds a reference to them, and no load reads into them.
    #[allow(unsafe_code, reason = "atomic stores into the pool's pages")]
    pub(crate) unsafe fn copy_in(&self, at: u64, bytes: &[u8]) {
        assert!(
            at + bytes.len() as u64 <= self.total * PAGE,
            "writing bytes {at}..{} of a store pool of {} pages",
            at + bytes.len() as u64,
            self.total
        );
        // SAFETY: inside the reservation (asserted), mapped readable and writable; by the
        // caller's contract nothing holds a plain reference to the bytes, and an optimistic copy
        // of the same pages' committed bytes is of other bytes, and atomic.
        unsafe { blaze_mem::racy::copy_in(self.addr(at), bytes) }
    }

    /// The pages of `run` made the process's before a load writes them.
    fn reuse_run(&self, run: Run) -> Result<(), MemError> {
        self.give.reuse(self, (run.first * PAGE) as usize, (u64::from(run.pages) * PAGE) as usize)
    }

    /// `ranges` of pages given back; the bytes that went (a prefix of the ranges).
    ///
    /// # Safety
    ///
    /// As [`Give::evict`].
    pub(crate) unsafe fn give_back(&self, ranges: &[Range<usize>]) -> Result<usize, MemError> {
        // SAFETY: the caller's.
        unsafe { self.give.evict(self, ranges) }
    }

    /// The bytes `[at, at + len)` of pinned or `Loading` pages, as a slice.
    ///
    /// # Safety
    ///
    /// Every page the bytes touch is pinned by the caller for the slice's life, or `Loading` with
    /// its read completed and the caller its loader: nothing writes them meanwhile (records are
    /// immutable; a load writes only `Loading` pages; the sweep gives back only unpinned
    /// `Evicting` ones; a commit writes only past what is committed).
    #[allow(unsafe_code, reason = "a pinned run of the pool's pages, as a slice")]
    pub(crate) unsafe fn bytes(&self, at: u64, len: u64) -> &[u8] {
        assert!(at + len <= self.total * PAGE, "bytes {at}..{} of a store pool of {} pages", at + len, self.total);
        // SAFETY: inside the reservation (asserted), mapped readable for the region's life, and
        // by the caller's contract unchanged while the slice lives.
        unsafe { std::slice::from_raw_parts(self.addr(at), len as usize) }
    }

    /// `[at, at + len)` zeroed with relaxed atomic stores: what a load leaves past its file's committed
    /// bytes, since pages given back on macOS may hold anything.
    #[allow(unsafe_code, reason = "atomic stores into the pool's pages")]
    pub(crate) fn zero(&self, at: u64, len: u64) {
        assert!(at + len <= self.total * PAGE, "zeroing bytes {at}..{} of a store pool of {} pages", at + len, self.total);
        // SAFETY: inside the reservation (asserted), mapped readable and writable; the pages are
        // `Loading` (the caller loads them), so no pinned reference to them exists, and an optimistic
        // copy racing it is atomic.
        unsafe { blaze_mem::racy::zero(self.addr(at), len as usize) }
    }

    /// `page` filled with `byte`: the DST's stand-in for the kernel dropping it.
    #[cfg(test)]
    #[allow(unsafe_code, reason = "atomic stores into the pool's pages")]
    pub(crate) fn fill(&self, page: u64, byte: u8) {
        let chunk = [byte; BLOCK as usize];
        for block in 0..PAGE / BLOCK {
            let at = page * PAGE + block * BLOCK;
            assert!(at + BLOCK <= self.total * PAGE, "filling page {page} past the pool");
            // SAFETY: inside the reservation, mapped readable and writable; written atomically,
            // so an optimistic copy racing it is defined.
            unsafe { blaze_mem::racy::copy_in(self.addr(at), &chunk) };
        }
    }

    // --- Optimistic reads ---

    /// The words of the pages under `[at, at + len)` (at most two: an optimistic read is at most a
    /// page long), each seen `Loaded` with an acquire load; else the word of the first that is not.
    pub(crate) fn begin(&self, at: u64, len: usize) -> Result<Seen, Word> {
        assert!(len >= 1 && len as u64 <= PAGE, "an optimistic read of {len} bytes: 1 to a page");
        let run = Run::of(at, len as u64);
        let mut seen = Seen { first: run.first, words: [0; 2], pages: run.pages as u8 };
        for (i, page) in run.pages().enumerate() {
            let word = self.word_now(page);
            if word.state() != State::Loaded {
                return Err(word);
            }
            seen.words[i] = word.0;
        }
        Ok(seen)
    }

    /// `out.len()` bytes at `at` copied out with relaxed atomic loads: defined while the kernel drops
    /// or a load writes the pages underneath, and good only if [`Pool::still`] says so after.
    #[allow(unsafe_code, reason = "a racy copy out of the pool's pages")]
    pub(crate) fn copy(&self, at: u64, out: &mut [u8]) {
        assert!(
            at + out.len() as u64 <= self.total * PAGE,
            "copying bytes {at}..{} of a store pool of {} pages",
            at + out.len() as u64,
            self.total
        );
        // SAFETY: inside the reservation (asserted), every byte of which is mapped readable and
        // writable for the region's life whether given back or not; every write that may race
        // the copy is atomic (`racy::zero`, `racy::copy_in`) or the kernel's.
        unsafe { blaze_mem::racy::copy_out(self.addr(at), out) }
    }

    /// Whether the pages `seen` saw are still `Loaded` at the same version: the second look of the
    /// seqlock, after an acquire fence.
    pub(crate) fn still(&self, seen: &Seen) -> bool {
        fence(Ordering::Acquire);
        (0..seen.pages as usize).all(|i| {
            let then = Word(seen.words[i]);
            let now = Word(self.word(seen.first + i as u64).load(Ordering::Relaxed));
            now.state() == State::Loaded && now.version() == then.version()
        })
    }

    /// The pages `seen` saw marked as read since the sweep last came by, where they are not marked
    /// already: one compare-and-swap a page a lap of the sweep, and nothing written after. For a
    /// copy that validated, so a page read only optimistically (every index page) is not taken
    /// while it is in use. A swap that fails is not tried again: a pin or the sweep moved the word,
    /// and the next read marks it. A page a prefetch loaded has its mark cleared in the same
    /// swap, and is counted as hit: this is its first read.
    pub(crate) fn touch(&self, seen: &Seen) {
        for i in 0..seen.pages as usize {
            let word = self.word(seen.first + i as u64);
            let now = Word(word.load(Ordering::Relaxed));
            if now.state() == State::Loaded && (!now.referenced() || now.prefetched()) {
                let next = (now.0 | Word::REFERENCED) & !Word::PREFETCHED;
                let swapped = word.compare_exchange(now.0, next, Ordering::Release, Ordering::Relaxed).is_ok();
                if swapped && now.prefetched() {
                    self.count_prefetch(Prefetch::Hit, 1);
                }
            }
        }
    }

    /// `out.len()` bytes at `at` (at most a page) copied out with no pin: the store's index
    /// probes, record headers and block checks. It writes a shared line only to mark a page the
    /// sweep has unmarked ([`Pool::touch`]).
    pub(crate) fn read_optimistic(&self, at: u64, out: &mut [u8]) -> Optimistic {
        let seen = match self.begin(at, out.len()) {
            Ok(seen) => seen,
            Err(word) => return Optimistic::Miss(word),
        };
        self.copy(at, out);
        match self.still(&seen) {
            true => {
                self.touch(&seen);
                Optimistic::Read
            }
            false => Optimistic::Moved,
        }
    }
}

/// What [`Pool::begin`] saw of an optimistic read's pages.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Seen {
    first: u64,
    words: [u64; 2],
    pages: u8,
}

/// How an optimistic read went.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Optimistic {
    /// The bytes are good.
    Read,
    /// A page is not `Loaded`: a miss, loaded by a pinned read.
    Miss(Word),
    /// A page was evicted, or loaded again, during the copy: look again.
    Moved,
}

// --- Runs and windows ----------------------------------------------------------------------------

/// The pages a record covers: `at / PAGE ..= (at + len - 1) / PAGE` of the pool.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Run {
    pub(crate) first: u64,
    pub(crate) pages: u32,
}

impl Run {
    /// The run of `len` (at least one) bytes at the pool's byte `at`.
    pub(crate) fn of(at: u64, len: u64) -> Run {
        assert!(len >= 1, "the run of an empty record at byte {at} of the store's pool");
        let first = at / PAGE;
        let last = (at + len - 1) / PAGE;
        Run { first, pages: (last - first + 1) as u32 }
    }

    pub(crate) fn pages(self) -> Range<u64> {
        self.first..self.first + u64::from(self.pages)
    }

    pub(crate) fn contains(self, other: Run) -> bool {
        self.first <= other.first && other.first + u64::from(other.pages) <= self.first + u64::from(self.pages)
    }
}

/// A store file read in place, by what the store calls it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum FileId {
    /// A pack, by its number in the head.
    Pack(u32),
    /// One of the two index slots.
    Index(u8),
    /// A file of its own (4 MiB or more), by its content.
    External(ContentId),
}

/// How the sweep weighs a file's pages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rank {
    /// One read to rebuild.
    Plain,
    /// An index page: one miss stalls every lookup it covers, so it counts four times its bytes
    /// (the sweep takes it on one lap in four).
    Index,
}

/// Whether the sweep passes over a pack's last page: one written in the last two commits, which
/// the next commit is likely to write into (lane 4 sets it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tail {
    Cold,
    Hot,
}

/// A file's windows and what is committed of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Window {
    pub(crate) id: FileId,
    /// Its first window; it has `count` consecutive ones.
    pub(crate) first: u32,
    pub(crate) count: u32,
    /// The bytes committed, read in place.
    pub(crate) len: u64,
    pub(crate) rank: Rank,
    pub(crate) tail: Tail,
}

impl Window {
    /// Its first page in the pool.
    pub(crate) fn first_page(&self) -> u64 {
        u64::from(self.first) * PER_WINDOW
    }

    /// The pages its committed bytes cover.
    pub(crate) fn pages(&self) -> u64 {
        self.len.div_ceil(PAGE)
    }

    /// The pool's byte at which the file's byte `offset` lives.
    pub(crate) fn at(&self, offset: u64) -> u64 {
        self.first_page() * PAGE + offset
    }

    /// The run of a record of `len` bytes at the file's `offset`, inside what is committed.
    pub(crate) fn run(&self, offset: u64, len: u64) -> Run {
        assert!(
            offset + len <= self.len,
            "a record at {offset}..{} of {:?}, which commits {} bytes",
            offset + len,
            self.id,
            self.len
        );
        Run::of(self.at(offset), len)
    }
}

/// The table from a store file to its windows, and the free windows. The store's to change; a
/// reader finds a record's run from its file's [`Window`].
///
/// A window comes back to the free list only once every page of its file is given back, none
/// pinned or loading ([`Windows::retire`]); the store retires a file only once no reader can
/// reach it (after a grace of [`blaze_reclaim`]), so a run of a retired file is never asked.
pub(crate) struct Windows {
    /// Each open file, by its first window: what the sweep walks, in address order.
    by_first: BTreeMap<u32, Window>,
    ids: BTreeMap<FileId, u32>,
    free: BTreeSet<u32>,
    /// Windows below it have been used, and their words are counted.
    next: u32,
    total: u32,
    /// Pages of every open file's committed bytes.
    open_pages: u64,
}

impl Windows {
    pub(crate) fn new(total: u32) -> Windows {
        Windows {
            by_first: BTreeMap::new(),
            ids: BTreeMap::new(),
            free: BTreeSet::new(),
            next: 0,
            total,
            open_pages: 0,
        }
    }

    pub(crate) fn get(&self, id: FileId) -> Option<&Window> {
        self.ids.get(&id).and_then(|first| self.by_first.get(first))
    }

    /// Each open file, in address order: what a census walks.
    pub(crate) fn files(&self) -> impl Iterator<Item = &Window> {
        self.by_first.values()
    }

    /// Pages of every open file's committed bytes: what the sweep's bound is twice of.
    pub(crate) fn open_pages(&self) -> u64 {
        self.open_pages
    }

    /// The first committed page at or after `page` of any open file, with its file, wrapping to the
    /// first open file's first page past the last: the sweep's next page.
    pub(crate) fn next_page(&self, page: u64) -> Option<(u64, &Window)> {
        let window = u32::try_from(page / PER_WINDOW).unwrap_or(u32::MAX);
        let holding = self.by_first.range(..=window).next_back().map(|(_, it)| it);
        if let Some(file) = holding.filter(|it| page < it.first_page() + it.pages()) {
            return Some((page.max(file.first_page()), file));
        }
        let after = self.by_first.range(window.saturating_add(1)..).map(|(_, it)| it);
        let mut files = after.chain(self.by_first.values());
        files.find(|it| it.pages() > 0).map(|it| (it.first_page(), it))
    }

    /// A window, or a run of them, for `id`, whose `len` bytes are committed: from the free list
    /// when it has a run long enough, else past every window used. Its pages are `Unloaded`.
    ///
    /// # Errors
    ///
    /// [`PoolError::Full`] when the reservation has no run of windows that long left.
    pub(crate) fn open<G: Give>(&mut self, pool: &Pool<G>, id: FileId, len: u64, rank: Rank) -> Result<Window, PoolError> {
        self.open_room(pool, id, len, len, rank)
    }

    /// As [`Windows::open`], with windows for `room` bytes (at least `len`): a file a commit is
    /// about to append to up to `room`, which must lie in one run of windows from its first
    /// commit (a pack of one record longer than a window).
    ///
    /// # Errors
    ///
    /// [`PoolError::Full`] when the reservation has no run of windows that long left.
    pub(crate) fn open_room<G: Give>(
        &mut self,
        pool: &Pool<G>,
        id: FileId,
        len: u64,
        room: u64,
        rank: Rank,
    ) -> Result<Window, PoolError> {
        assert!(!self.ids.contains_key(&id), "{id:?} opened in the store's pool twice");
        let count = (room.max(len).div_ceil(WINDOW).max(1)) as u32;
        let free = self.free.iter().copied().find(|&first| (first..first + count).all(|it| self.free.contains(&it)));
        let first = match free {
            Some(first) => {
                for it in first..first + count {
                    self.free.remove(&it);
                }
                first
            }
            None if self.next.checked_add(count).is_some_and(|end| end <= self.total) => {
                let first = self.next;
                pool.words_used(first + count).map_err(|err| PoolError::Reserve {
                    what: "its pages' state words",
                    windows: self.total,
                    // alloc: an error path
                    why: err.to_string(),
                })?;
                self.next += count;
                first
            }
            None => {
                return Err(PoolError::Full { id, windows: count, live: self.by_first.len() as u32, total: self.total });
            }
        };
        let window = Window { id, first, count, len, rank, tail: Tail::Cold };
        self.by_first.insert(first, window);
        self.ids.insert(id, first);
        self.open_pages += window.pages();
        Ok(window)
    }

    /// `id`'s committed bytes grown to `len` by a commit ([`Append::commit`]), inside its
    /// windows: its new pages are the sweep's to visit and a reader's to ask for from now on. Its
    /// window as it is now.
    pub(crate) fn grow(&mut self, id: FileId, len: u64) -> Window {
        let first = *self.ids.get(&id).unwrap_or_else(|| panic!("{id:?} grown in the store's pool, where it is not open"));
        let window = self.by_first.get_mut(&first).expect("an open file has its window");
        assert!(
            window.len <= len && len <= u64::from(window.count) * WINDOW,
            "{id:?}, which commits {} bytes in {} windows, grown to {len}",
            window.len,
            window.count
        );
        let before = window.pages();
        window.len = len;
        self.open_pages += window.pages() - before;
        *window
    }

    /// The open file whose committed pages hold the pool's page `page`.
    pub(crate) fn file_at(&self, page: u64) -> Option<&Window> {
        let window = u32::try_from(page / PER_WINDOW).ok()?;
        let (_, file) = self.by_first.range(..=window).next_back()?;
        (page < file.first_page() + file.pages()).then_some(file)
    }

    /// Whether the sweep passes over `id`'s last page.
    pub(crate) fn set_tail(&mut self, id: FileId, tail: Tail) {
        if let Some(window) = self.ids.get(&id).and_then(|first| self.by_first.get_mut(first)) {
            window.tail = tail;
        }
    }

    /// `id`'s pages given back and its windows freed: every `Loaded` page, unpinned, taken
    /// `Loaded -> Evicting` and given back, then every page `Unloaded` at a raised version, so a stale
    /// optimistic copy fails its check. Windows with an `Abandoned` page (the kernel may still write
    /// it) never come back.
    ///
    /// # Errors
    ///
    /// [`PoolError::Busy`] when a page is pinned, loading or being evicted: nothing changed, and
    /// the store retires it later. [`PoolError::Give`] when the kernel refused the pages: they stay
    /// `Loaded`.
    pub(crate) fn retire<G: Give>(&mut self, pool: &Pool<G>, id: FileId) -> Result<(), PoolError> {
        let window = *self.get(id).unwrap_or_else(|| panic!("{id:?} retired from the store's pool, where it is not open"));
        let pages = window.first_page()..window.first_page() + window.pages();
        let mut taken = Vec::new(); // alloc: a retirement, rare, its pages listed once
        let mut abandoned = false;
        let busy = |page: u64, word: Word| PoolError::Busy { id, page, word };
        for page in pages.clone() {
            let seen = pool.word_now(page);
            match seen.state() {
                State::Unloaded | State::Corrupt => {}
                State::Abandoned => abandoned = true,
                State::Loaded if seen.pins() == 0 => {
                    let next = Word::evicting(seen.next_version());
                    match pool.word(page).compare_exchange(seen.0, next.0, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(_) => {
                            if seen.prefetched() {
                                pool.count_prefetch(Prefetch::Wasted, 1);
                            }
                            taken.push(page);
                        }
                        Err(now) => {
                            release(pool, &taken);
                            return Err(busy(page, Word(now)));
                        }
                    }
                }
                State::Loaded | State::Loading | State::Evicting => {
                    release(pool, &taken);
                    return Err(busy(page, seen));
                }
            }
        }
        let ranges = ranges_of(&taken);
        if !ranges.is_empty() {
            // SAFETY: every page of the ranges was moved `Loaded{pins: 0} -> Evicting` above, so no pin
            // holds it and none can succeed; each was loaded, so reused, and not given back since.
            let given = unsafe { pool.give_back(&ranges) }.map_err(|err| {
                release(pool, &taken);
                // alloc: an error path
                PoolError::Give { why: err.to_string() }
            })?;
            let whole: usize = ranges.iter().map(Range::len).sum();
            if given < whole {
                // The kernel kept a suffix: it is resident still, and its file is going, so the
                // bytes are counted until the window comes back.
                tracing::warn!(?id, given, whole, "the store's pool retired a file the kernel kept pages of");
            }
        }
        for page in pages {
            let seen = pool.word_now(page);
            if seen.state() != State::Abandoned {
                pool.word(page).store(Word::unloaded(seen.next_version()).0, Ordering::Release);
            }
        }
        self.by_first.remove(&window.first);
        self.ids.remove(&id);
        self.open_pages -= window.pages();
        if !abandoned {
            self.free.extend(window.first..window.first + window.count);
        }
        Ok(())
    }
}

/// Pages taken `Evicting` by a retirement that cannot go on, `Loaded` again (at the raised version).
fn release<G: Give>(pool: &Pool<G>, taken: &[u64]) {
    for &page in taken {
        let seen = pool.word_now(page);
        pool.word(page).store(Word::loaded(seen.version(), 0, Referenced::Clear).0, Ordering::Release);
    }
}

/// `pages` (ascending) as byte ranges of the pool, adjacent ones merged: one `iovec` each.
pub(crate) fn ranges_of(pages: &[u64]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new(); // alloc: a batch's ranges, at most its pages
    for &page in pages {
        let at = (page * PAGE) as usize;
        match ranges.last_mut() {
            Some(last) if last.end == at => last.end += PAGE as usize,
            _ => ranges.push(at..at + PAGE as usize),
        }
    }
    ranges
}

/// The check of one block of a file's committed bytes (its last may be short): what a load
/// compares each block it read against, and the store's `checks-<n>` holds per block.
pub(crate) fn block_check(bytes: &[u8]) -> u32 {
    crate::checks::of_block(bytes)
}

/// The checks of every block of `bytes`, a file's committed bytes.
pub(crate) fn checks_of(bytes: &[u8]) -> Vec<u32> {
    crate::checks::of(bytes)
}

// --- Pinned reads ----------------------------------------------------------------------------------

/// The pins of a record's run, given back when dropped: what a holder keeps past its task (a
/// codegen unit's inputs, a link's), and what the store's `Pinned` holds (lane 4). Its bytes stay
/// as they are while it lives, so a reference into them is sound for its life.
///
/// `P` is how it reaches the pool: a borrow (`&Pool`) for a value read within a task, a count
/// (`Arc`, or the DST's `Rc`) for one held past it.
pub(crate) struct Pins<G: Give, P: Deref<Target = Pool<G>>> {
    pool: P,
    at: u64,
    len: u64,
    /// Its pins are its own to give back (not handed to a [`PinSet`]).
    owned: bool,
    give: PhantomData<fn() -> G>,
}

impl<G: Give, P: Deref<Target = Pool<G>>> Pins<G, P> {
    /// The record's bytes.
    #[allow(unsafe_code, reason = "a pinned run's bytes")]
    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY: every page of the run is pinned while `self` lives (this guard holds the pins),
        // so nothing writes or gives them back, and the slice borrows `self`.
        unsafe { self.pool.bytes(self.at, self.len) }
    }

    pub(crate) fn run(&self) -> Run {
        Run::of(self.at, self.len)
    }

    /// The pool's byte the record starts at.
    pub(crate) fn at(&self) -> u64 {
        self.at
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Dropped with the process's memory as a run of the DST ends, its pins gone with the pool.
    #[cfg(test)]
    pub(crate) fn lost(mut self) {
        self.owned = false;
    }

    /// The pins, no longer this guard's to give back: a [`PinSet`] takes them.
    fn disown(mut self) -> Run {
        self.owned = false;
        self.run()
    }
}

impl<G: Give, P: Deref<Target = Pool<G>>> Drop for Pins<G, P> {
    fn drop(&mut self) {
        if self.owned {
            self.pool.unpin_run(Run::of(self.at, self.len));
        }
    }
}

impl<G: Give, P: Deref<Target = Pool<G>>> fmt::Debug for Pins<G, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pins({}..{} of the store's pool)", self.at, self.at + self.len)
    }
}

/// The runs one task pinned, at most [`PIN_SET`], kept in the task's frame: a second read of a
/// page the task pinned finds it here and touches no shared word. Its pins go when the task ends
/// or resets its frame between items ([`PinSet::reset`], or its drop), whichever comes first; a
/// read past a full set holds its own [`Pins`].
pub(crate) struct PinSet<G: Give, P: Deref<Target = Pool<G>> + Clone> {
    pool: P,
    runs: [Cell<Run>; PIN_SET],
    held: Cell<u8>,
    give: PhantomData<fn() -> G>,
}

impl<G: Give, P: Deref<Target = Pool<G>> + Clone> PinSet<G, P> {
    pub(crate) fn new(pool: P) -> PinSet<G, P> {
        PinSet {
            pool,
            runs: std::array::from_fn(|_| Cell::new(Run { first: 0, pages: 0 })),
            held: Cell::new(0),
            give: PhantomData,
        }
    }

    /// The bytes `[at, at + len)` of the pool, if a run the set holds covers them.
    #[allow(unsafe_code, reason = "a held run's bytes")]
    pub(crate) fn find(&self, at: u64, len: u64) -> Option<&[u8]> {
        let want = Run::of(at, len);
        let held = &self.runs[..self.held.get() as usize];
        held.iter().any(|run| run.get().contains(want)).then(|| {
            // SAFETY: the pages are pinned by this set until `reset` or drop, each of which takes
            // `&mut self`, so none happens while the slice (which borrows `self`) lives.
            unsafe { self.pool.bytes(at, len) }
        })
    }

    /// `pins`' run kept by the set until it resets, and its bytes; the guard back when the set is
    /// full.
    #[allow(unsafe_code, reason = "a held run's bytes")]
    pub(crate) fn keep(&self, pins: Pins<G, P>) -> Result<&[u8], Pins<G, P>> {
        let held = self.held.get() as usize;
        if held == PIN_SET {
            return Err(pins);
        }
        let (at, len) = (pins.at, pins.len);
        self.runs[held].set(pins.disown());
        self.held.set(held as u8 + 1);
        // SAFETY: as in `find`: the set holds the run's pins until a `&mut self` call.
        Ok(unsafe { self.pool.bytes(at, len) })
    }

    /// Its pins gone with the process's memory as a run of the DST ends: none given back.
    #[cfg(test)]
    pub(crate) fn lost(&mut self) {
        self.held.set(0);
    }

    /// Runs held.
    pub(crate) fn len(&self) -> usize {
        self.held.get() as usize
    }

    /// Pages held, counting a page each run holds.
    pub(crate) fn pages(&self) -> u64 {
        self.runs[..self.len()].iter().map(|it| u64::from(it.get().pages)).sum()
    }

    /// Every pin let go: the task ended, or reset its frame between items.
    pub(crate) fn reset(&mut self) {
        for run in &self.runs[..self.held.get() as usize] {
            self.pool.unpin_run(run.get());
        }
        self.held.set(0);
    }
}

impl<G: Give, P: Deref<Target = Pool<G>> + Clone> Drop for PinSet<G, P> {
    fn drop(&mut self) {
        self.reset();
    }
}

/// A pinned read of one record as a machine the asker steps ([`Reading::step`]): pins its pages in
/// order, claims a stretch of unloaded ones for one load, waits on a load another asker made,
/// and ends pinned or failed. The loader's thread runs it blocking on events (`drive`); the DST
/// steps it one word operation at a time.
///
/// It holds the pins of its run's first [`Reading::held`] pages. A load it asked pins each page it
/// loads for it (the loader's pin, set in the same compare-and-swap as `Loaded`), so a page it
/// loaded cannot be evicted before it looks.
pub(crate) struct Reading {
    at: u64,
    len: u64,
    run: Run,
    /// Pages of the run pinned, from its first.
    pinned: u32,
    /// The load slot it holds, given by its asker ([`Reading::give_slot`]).
    slot: Option<u16>,
    asked: Option<Asked>,
    /// Waits and spins so far: at [`WAITS_MOST`] it is starved.
    waits: u32,
    /// The page it last waited for a prefetch's load of: its pin of it, if it is the page's
    /// first, is a late hit.
    late: Option<u64>,
    /// Prefetched pages it was the first to read.
    prefetched: u32,
}

#[derive(Debug)]
struct Asked {
    /// The stretch of the run asked, from its page `from` (the first not pinned).
    from: u32,
    pages: u32,
    ended: Option<Result<(), LoadFailure>>,
}

/// What a reading asks its asker to do next.
pub(crate) enum ReadStep<G: Give, P: Deref<Target = Pool<G>>> {
    /// Every page pinned: the guard.
    Pinned(Pins<G, P>),
    /// A load slot, for a stretch it is about to claim: [`Reading::give_slot`], then step again.
    NeedSlot,
    /// Its claimed stretch, to hand the loads core; then [`Reading::loaded`] once it ends.
    Ask(Load),
    /// Its load is in flight: wait for its end.
    Pending,
    /// `page` is being loaded by another asker's load in `slot`: wait on that load, then step again.
    Wait { page: u64, slot: u16 },
    /// `page` is being given back: the sweep's batch takes microseconds, so spin (bounded), then
    /// step again.
    Spin { page: u64 },
    Failed(PoolError),
}

impl Reading {
    /// A read of the `len` (at least one) bytes at the pool's byte `at`.
    pub(crate) fn new(at: u64, len: u64) -> Reading {
        Reading { at, len, run: Run::of(at, len), pinned: 0, slot: None, asked: None, waits: 0, late: None, prefetched: 0 }
    }

    pub(crate) fn run(&self) -> Run {
        self.run
    }

    /// Pages of its run it holds pins of now.
    pub(crate) fn held(&self) -> u32 {
        self.pinned
    }

    /// Prefetched pages it was the first to read, so far: each made room for the next prefetch.
    pub(crate) fn prefetched(&self) -> u32 {
        self.prefetched
    }

    /// Whether a load it asked has not ended.
    pub(crate) fn pending(&self) -> bool {
        self.asked.as_ref().is_some_and(|it| it.ended.is_none())
    }

    /// The load slot it asked for.
    pub(crate) fn give_slot(&mut self, slot: u16) {
        assert!(self.slot.is_none(), "a reading given load slot {slot} holds one already");
        self.slot = Some(slot);
    }

    /// The load slot it holds, back to its asker: once it ended (pinned or failed), or to give
    /// up. Never while its load is in flight.
    pub(crate) fn take_slot(&mut self) -> Option<u16> {
        assert!(!self.pending(), "a reading's load slot taken back while its load is in flight");
        self.slot.take()
    }

    /// How its load ended. A good one has pinned every page of its stretch for it.
    pub(crate) fn loaded<G: Give>(&mut self, pool: &Pool<G>, outcome: Result<(), LoadFailure>) {
        let asked = self.asked.as_mut().expect("a reading told of a load it did not ask");
        assert!(asked.ended.is_none(), "a reading told twice of its load");
        match outcome {
            Ok(()) => {
                self.pinned = asked.from + asked.pages;
                self.asked = None;
            }
            Err(failure) => {
                // Its pages that loaded well hold the loader's pin for it: let them go.
                let first = self.run.first + u64::from(asked.from);
                for page in first..first + u64::from(asked.pages) {
                    if pool.word_now(page).state() == State::Loaded {
                        pool.unpin(page);
                    }
                }
                asked.ended = Some(Err(failure));
            }
        }
    }

    /// Its pins let go, and it ends: its asker gives it up (it could not get a slot). Never
    /// while its load is in flight.
    pub(crate) fn give_up<G: Give>(&mut self, pool: &Pool<G>) {
        assert!(!self.pending(), "a reading given up while its load is in flight");
        self.asked = None;
        self.release(pool);
    }

    /// Dropped with the process's memory at a crash, its pins gone with the pool: the DST's.
    #[cfg(test)]
    pub(crate) fn lost(mut self) {
        self.pinned = 0;
    }

    /// Its pins let go.
    fn release<G: Give>(&mut self, pool: &Pool<G>) {
        pool.unpin_run(Run { first: self.run.first, pages: self.pinned });
        self.pinned = 0;
    }

    /// Fails with `error`, its pins let go.
    fn fail<G: Give, P: Deref<Target = Pool<G>>>(&mut self, pool: &Pool<G>, error: PoolError) -> ReadStep<G, P> {
        self.release(pool);
        ReadStep::Failed(error)
    }

    /// One more wait or spin, or starved.
    fn waited<G: Give, P: Deref<Target = Pool<G>>>(&mut self, pool: &Pool<G>, page: u64, step: ReadStep<G, P>) -> ReadStep<G, P> {
        self.waits += 1;
        match self.waits >= WAITS_MOST {
            true => self.fail(pool, PoolError::Starved { page, waits: self.waits }),
            false => step,
        }
    }

    /// Its next step: as many pages pinned as are `Loaded` in order, then what it needs.
    pub(crate) fn step<G: Give, P: Deref<Target = Pool<G>> + Clone>(&mut self, handle: &P) -> ReadStep<G, P> {
        let pool: &Pool<G> = handle;
        if let Some(asked) = &mut self.asked {
            match asked.ended.take() {
                None => return ReadStep::Pending,
                Some(Ok(())) => unreachable!("a good load ends the ask in `loaded`"),
                Some(Err(failure)) => {
                    self.asked = None;
                    return self.fail(pool, PoolError::Load(failure));
                }
            }
        }
        // Each turn pins a page or returns: at most the run's pages turns, and a claim lost to
        // another asker counts as a wait.
        while self.pinned < self.run.pages {
            let page = self.run.first + u64::from(self.pinned);
            let word = match pool.try_pin(page) {
                Ok(pin) => {
                    if pin == Pin::Prefetched {
                        let how = match self.late == Some(page) {
                            true => Prefetch::Late,
                            false => Prefetch::Hit,
                        };
                        pool.count_prefetch(how, 1);
                        self.prefetched += 1;
                    }
                    self.pinned += 1;
                    continue;
                }
                Err(word) => word,
            };
            match word.state() {
                State::Loaded => continue,
                State::Unloaded => {
                    let Some(slot) = self.slot else { return ReadStep::NeedSlot };
                    let mut claimed = 0u32;
                    while self.pinned + claimed < self.run.pages
                        && pool.claim(page + u64::from(claimed), slot).is_ok()
                    {
                        claimed += 1;
                    }
                    if claimed == 0 {
                        // Another asker claimed it first: look again.
                        if let ReadStep::Failed(error) = self.waited::<G, P>(pool, page, ReadStep::Pending) {
                            return ReadStep::Failed(error);
                        }
                        continue;
                    }
                    let stretch = Run { first: page, pages: claimed };
                    if let Err(err) = pool.reuse_run(stretch) {
                        for it in stretch.pages() {
                            pool.unclaim(it);
                        }
                        // alloc: an error path
                        return self.fail(pool, PoolError::Reuse { page, pages: claimed, why: err.to_string() });
                    }
                    self.asked = Some(Asked { from: self.pinned, pages: claimed, ended: None });
                    return ReadStep::Ask(Load { slot, first: page, pages: claimed });
                }
                State::Loading => {
                    let slot = word.slot();
                    if loads::prefetch_slot(slot) {
                        self.late = Some(page);
                    }
                    return self.waited(pool, page, ReadStep::Wait { page, slot });
                }
                State::Evicting => return self.waited(pool, page, ReadStep::Spin { page }),
                State::Corrupt => return self.fail(pool, PoolError::Corrupt { page }),
                State::Abandoned => return self.fail(pool, PoolError::Abandoned { page }),
            }
        }
        let pins = Pins { pool: handle.clone(), at: self.at, len: self.len, owned: true, give: PhantomData };
        // The guard holds them now.
        self.pinned = 0;
        ReadStep::Pinned(pins)
    }
}

impl Drop for Reading {
    fn drop(&mut self) {
        // A reading ends pinned (its guard holds the pins) or failed (it let them go); one
        // dropped in between would leak its pins, which nothing else can give back.
        if self.pinned > 0 && !std::thread::panicking() {
            panic!(
                "a reading of {}..{} of the store's pool dropped holding {} pins: step it to its end",
                self.at,
                self.at + self.len,
                self.pinned
            );
        }
    }
}

// --- Errors --------------------------------------------------------------------------------------

/// Why the pool could not do what it was asked: each one an error for the one task that read
/// (rule `panics-fail-one-job`), naming the page, file or load (rule `errors-name-the-thing`). The
/// store's reader adds the key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum PoolError {
    /// The reservation, its words or its registration was refused.
    Reserve { what: &'static str, windows: u32, why: String },
    /// No run of `windows` free windows is left for `id`.
    Full { id: FileId, windows: u32, live: u32, total: u32 },
    /// A page of `id` is pinned, loading or being evicted: it cannot be retired now.
    Busy { id: FileId, page: u64, word: Word },
    /// The kernel refused to give pages back.
    Give { why: String },
    /// The kernel refused to make a stretch's pages the process's before a load.
    Reuse { page: u64, pages: u32, why: String },
    /// A load failed.
    Load(LoadFailure),
    /// `page` failed a block check on an earlier load and is refused until its file is retired.
    Corrupt { page: u64 },
    /// `page` was abandoned to the kernel by a failed driver and is never used again.
    Abandoned { page: u64 },
    /// `page`, past what its file commits, is not `Unloaded` as a commit's append takes it.
    NotFresh { page: u64, word: Word },
    /// `page` was evicted and loaded again under one reading more times than any build does.
    Starved { page: u64, waits: u32 },
    /// Every load slot stayed taken through every wait for one.
    LoadsFull { waits: u32 },
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let page_of = |page: u64| (page / PER_WINDOW, (page % PER_WINDOW) * PAGE);
        match self {
            PoolError::Reserve { what, windows, why } => write!(
                f,
                "the store's pool of {windows} windows of {WINDOW} bytes could not reserve {what}: {why}"
            ),
            PoolError::Full { id, windows, live, total } => write!(
                f,
                "the store's pool has no run of {windows} free windows for {id:?}: {live} files hold windows of its \
                 {total} (pool::WINDOWS); the reservation is too small for this store"
            ),
            PoolError::Busy { id, page, word } => {
                let (window, at) = page_of(*page);
                write!(f, "{id:?} cannot be retired from the store's pool now: its page at {at} (window {window}) is {word:?}")
            }
            PoolError::Give { why } => write!(f, "the store's pool could not give pages back to the kernel: {why}"),
            PoolError::Reuse { page, pages, why } => {
                let (window, at) = page_of(*page);
                write!(f, "the store's pool could not take {pages} pages from byte {at} of window {window} to load them: {why}")
            }
            PoolError::Load(failure) => write!(f, "{failure}"),
            PoolError::Corrupt { page } => {
                let (window, at) = page_of(*page);
                write!(f, "the store's pool page at {at} of window {window} failed a block check when it was loaded and is refused")
            }
            PoolError::Abandoned { page } => {
                let (window, at) = page_of(*page);
                write!(
                    f,
                    "the store's pool page at {at} of window {window} was abandoned to the kernel by a failed read ({})",
                    blaze_core::ABANDONED_SPAN
                )
            }
            PoolError::NotFresh { page, word } => {
                let (window, at) = page_of(*page);
                write!(
                    f,
                    "a commit could not take the store's pool page at {at} of window {window} to append to its                      file: no file commits it, and it is {word:?}"
                )
            }
            PoolError::Starved { page, waits } => {
                let (window, at) = page_of(*page);
                write!(
                    f,
                    "a read of the store's pool page at {at} of window {window} waited {waits} times (pool::WAITS_MOST) \
                     and never found it loaded: it is evicted as fast as it is loaded"
                )
            }
            PoolError::LoadsFull { waits } => write!(
                f,
                "the store's pool had no free load slot of {} (pool::loads::DEMAND_SLOTS) through {waits} waits for one",
                loads::DEMAND_SLOTS
            ),
        }
    }
}

impl std::error::Error for PoolError {}
