//! The pool's loads, as a sans-IO core (rule `sans-io-cores`): told the files it reads
//! ([`Loads::open`]) and the stretches its readers claimed ([`Loads::ask`]), it hands back
//! [`Effect::ReadInto`]s straight into the pages' slots and takes their [`Completion`]s. It checks
//! every block it read against its file's checks before a page is `Loaded`, zeroes what lies past
//! the committed bytes, and gives back the pages of a load that failed, so nothing it did not
//! check is ever served (docs/v2/plan/design/store-pool.md, "A miss" and "Verification on load").
//!
//! A load is one claimed stretch of one file's pages, in one of [`LOADS`] slots, read in as few
//! reads of up to [`READ_MOST`] as its length needs. The slot is its asker's (the driver keeps
//! them free and fires each one's event, `drive`); the tag of each read names the slot and the
//! read, so a completion finds its load with no table.
//!
//! A read's blocks are all checked: every page with a block that fails is `Corrupt`, not only the
//! first such page of the read.
//!
//! Each page's transition is the word's: `Loading -> Loaded` with the asker's pin in the same
//! store, `Loading -> Evicting -> Unloaded` for a failed or short read (its pages given back
//! first), `Loading -> Evicting -> Corrupt` for a block that failed its check, `Loading ->
//! Abandoned` for a read the driver abandoned to the kernel.
//!
//! **Prefetch** (the design's section 5) is the same read into the same slots, asked by nobody:
//! the core keeps a queue of runs a plan gave it ([`Loads::prefetch`]) and issues from its front
//! ([`Loads::pump`]) in load slots of its own (the last [`PREFETCH_IN_FLIGHT`] of the table, so a
//! demand read never waits for a slot a prefetch holds), and only while
//!
//! - no demand load is in flight: the priority is the pool's own, since an NVMe drive's default
//!   scheduler ignores the idle class a prefetch read also carries ([`Priority::Idle`]);
//! - fewer than [`PREFETCH_IN_FLIGHT`] prefetch loads are;
//! - the pages prefetched and not yet read, in flight or loaded and still marked, stay within the
//!   budget ([`Loads::set_budget`]). A read of a prefetched page clears its mark and makes room
//!   for the next, so the queue is replayed at the pace its guesses are read, and a build that
//!   has turned elsewhere stops pulling it.
//!
//! A page a prefetch loads is published with no pin and its mark set. The queue is dropped once
//! the pages wasted since its plan (taken by the sweep unread) pass the pages read by
//! [`WASTE_SLACK`]: a replay that is wrong more than it is right ends.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    ops::Range,
    sync::atomic::Ordering,
};

use blaze_core::{ABANDONED_SPAN, Cache, Completion, Done, Effect, FileName, IoError, Priority, Tag};

use super::{
    BLOCK, Give, PAGE, PER_WINDOW, Plant, Pool, Prefetch, PrefetchCounts, Referenced, Run, State, Window, Word,
    block_check,
};

/// Loads in flight at most: a fixed table, each slot with its reusable event (the driver's).
pub(crate) const LOADS: usize = 4096;

/// Prefetch loads in flight at most, each one read of up to [`READ_MOST`]: 16 MiB, which a
/// demand read that arrives behind them waits a few milliseconds for at worst.
pub(crate) const PREFETCH_IN_FLIGHT: usize = 16;

/// The load slots askers take, from the first: the rest are the core's own, for prefetch.
pub(crate) const DEMAND_SLOTS: usize = LOADS - PREFETCH_IN_FLIGHT;

/// Pages a plan's prefetches may be wasted past those read before its queue is dropped: 4 MiB.
pub(crate) const WASTE_SLACK: u64 = (4 << 20) / PAGE;

/// Whether `slot` is one of the core's own, for prefetch: a page `Loading` in it has no asker.
pub(crate) fn prefetch_slot(slot: u16) -> bool {
    slot as usize >= DEMAND_SLOTS
}

/// The most one read reads: 16 pages, a size an NVMe drive takes as one command at full speed.
pub(crate) const READ_MOST: u64 = 1 << 20;

const PAGES_PER_READ: u64 = READ_MOST / PAGE;

/// Reads one load is split into at most: a tag holds the slot above this many reads.
const READS_SHIFT: u32 = 20;

const _: () = assert!(READ_MOST % PAGE == 0 && LOADS <= 1 << 12 && LOADS <= u16::MAX as usize);

/// A claimed stretch to load: `pages` pages of one file from the pool's page `first`, every one
/// `Loading` in `slot` and made the process's by its asker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Load {
    pub(crate) slot: u16,
    pub(crate) first: u64,
    pub(crate) pages: u32,
}

/// A load that ended: its asker's to wake. A prefetch's ([`prefetch_slot`]) has no asker: its
/// slot's event is fired for the readers that found its pages loading and wait on it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Ended {
    pub(crate) slot: u16,
    /// `Ok` when every page is `Loaded` and pinned for the asker; else its first failure, and
    /// the pages that read well are `Loaded` and pinned for the asker all the same. (A
    /// prefetch's pages are pinned for nobody.)
    pub(crate) outcome: Result<(), LoadFailure>,
}

/// A file the core reads: what the store opened in the pool, with its blocks' checks.
#[derive(Clone, Debug)]
pub(crate) struct Opened {
    pub(crate) window: Window,
    pub(crate) name: FileName,
    /// One check per [`BLOCK`] of its committed bytes ([`super::checks_of`]).
    pub(crate) checks: Vec<u32>,
}

/// Why a load failed, naming the file and where.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum LoadFailure {
    /// The read failed: EIO, a descriptor gone. Its pages are `Unloaded` again, and the next
    /// reader tries again.
    Read { file: FileName, at: u64, len: u32, why: IoError },
    /// The read ended short of what is committed: the file was cut by something other than blaze.
    Short { file: FileName, at: u64, read: u32, committed: u64 },
    /// A block failed its check: its page is `Corrupt`.
    Block { file: FileName, block: u64, page: u64 },
    /// The driver failed with the read in flight ([`ABANDONED_SPAN`]): its pages are never used again.
    Abandoned { file: FileName, at: u64, why: IoError },
    /// The stretch is in no file the core was told of, or past what its file commits.
    NoFile { first: u64, pages: u32 },
}

impl fmt::Display for LoadFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadFailure::Read { file, at, len, why } => {
                write!(f, "reading {len} bytes at {at} of {file} into the store's pool failed: {why}")
            }
            LoadFailure::Short { file, at, read, committed } => write!(
                f,
                "{file} is corrupt: a read at {at} into the store's pool got {read} bytes of the {committed} \
                 committed there; the file was cut by something other than blaze"
            ),
            LoadFailure::Block { file, block, page } => write!(
                f,
                "{file} is corrupt: block {block} (bytes {}..{}) failed its check as page {page} of the store's \
                 pool was loaded",
                block * BLOCK,
                (block + 1) * BLOCK
            ),
            LoadFailure::Abandoned { file, at, why } => write!(
                f,
                "a read at {at} of {file} into the store's pool was abandoned by its driver, and its pages are \
                 never used again: {why}"
            ),
            LoadFailure::NoFile { first, pages } => write!(
                f,
                "a load of {pages} pages from page {first} of the store's pool is in no file the pool was told of, \
                 or past what that file commits"
            ),
        }
    }
}

/// How each page of a load ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PageEnd {
    Good,
    /// Read failed, or short: given back and `Unloaded`.
    Again,
    /// A block failed: given back and `Corrupt`.
    Bad,
    /// Never touched again.
    Abandoned,
}

/// A slot of the table.
struct Flight {
    load: Option<Load>,
    /// The first window of its file.
    file: u32,
    reads_left: u32,
    /// Each page's end, by its place in the stretch.
    ends: Vec<PageEnd>,
    failure: Option<LoadFailure>,
}

struct File {
    window: Window,
    name: FileName,
    checks: Vec<u32>,
}

impl File {
    fn last_page(&self) -> u64 {
        self.window.first_page() + self.window.pages()
    }
}

/// One read of a load: its pages, where in its file, and how much of it is committed.
#[derive(Clone, Copy, Debug)]
struct Part {
    first: u64,
    pages: u64,
    /// The file's byte it starts at.
    at: u64,
    /// Its committed bytes: what it must read.
    want: u64,
    /// What it asks: `want` rounded up to whole blocks, so it may bypass the page cache.
    len: u64,
}

/// The core: the files it reads, and the table of loads in flight.
pub(crate) struct Loads {
    /// Its tags are `tags..tags + 2^32`.
    tags: u64,
    /// Whether its reads go around the page cache: the store's do (`O_DIRECT`, `F_NOCACHE`), so
    /// the bytes are not held twice; a test on a file system that cannot bypass it keeps it.
    cache: Cache,
    files: BTreeMap<u32, File>,
    table: Vec<Flight>,
    in_flight: usize,
    /// The pages a finished load gives back, reused.
    giving: Vec<u64>,
    /// Give-backs of a failed load's pages the kernel refused: memory kept, not correctness.
    refused: u64,
    /// Demand loads in flight: while there is one, no prefetch is issued.
    demand: usize,
    /// The plan's runs not yet issued, in the order to read them.
    queue: VecDeque<Run>,
    /// The prefetch slots no load is in.
    slots: Vec<u16>,
    /// Prefetch loads in flight, and their pages.
    prefetching: usize,
    prefetching_pages: u64,
    /// Prefetch loads in flight at most: [`PREFETCH_IN_FLIGHT`], fewer in a test.
    most: usize,
    /// Bytes prefetched and not yet read at most. None until it is told ([`Loads::set_budget`]):
    /// with no room known there is no prefetch.
    budget: u64,
    /// The pool's counts as the queue's plan was given: what is wasted and read since is the
    /// plan's.
    base: PrefetchCounts,
    /// Plans dropped for waste, runs dropped for a file retired or a kernel that refused their
    /// pages, and prefetch loads that failed, so far.
    stopped: u64,
    dropped: u64,
    failed: u64,
}

impl Loads {
    /// A core asking with tags `tags..tags + 2^32`, its reads through `cache`.
    pub(crate) fn new(tags: u64, cache: Cache) -> Loads {
        let flight = || Flight { load: None, file: 0, reads_left: 0, ends: Vec::new(), failure: None }; // alloc: the table, once
        Loads {
            tags,
            cache,
            files: BTreeMap::new(),
            table: (0..LOADS).map(|_| flight()).collect(), // alloc: the table, once
            in_flight: 0,
            giving: Vec::new(), // alloc: once, kept
            refused: 0,
            demand: 0,
            queue: VecDeque::new(), // alloc: empty, allocates nothing
            slots: (DEMAND_SLOTS..LOADS).map(|it| it as u16).collect(), // alloc: the prefetch slots, once
            prefetching: 0,
            prefetching_pages: 0,
            most: PREFETCH_IN_FLIGHT,
            budget: 0,
            base: PrefetchCounts::default(),
            stopped: 0,
            dropped: 0,
            failed: 0,
        }
    }

    /// Demand loads in flight.
    pub(crate) fn demand_in_flight(&self) -> usize {
        self.demand
    }

    /// Runs of the plan not yet issued.
    pub(crate) fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Plans dropped for waste, runs dropped unissued, and prefetch loads that failed, so far.
    pub(crate) fn prefetch_stops(&self) -> (u64, u64, u64) {
        (self.stopped, self.dropped, self.failed)
    }

    /// The bytes prefetched and not yet read may reach `bytes`: the smaller of an eighth of the
    /// ceiling and the room the plane last published, told by the driver.
    pub(crate) fn set_budget(&mut self, bytes: u64) {
        self.budget = bytes;
    }

    /// At most `loads` prefetch loads in flight: a test's, to meet the bound with few pages.
    #[cfg(test)]
    pub(crate) fn prefetch_at_most(&mut self, loads: usize) {
        assert!(loads <= PREFETCH_IN_FLIGHT, "{loads} prefetch loads at once, of {PREFETCH_IN_FLIGHT} slots");
        self.most = loads;
    }

    /// A plan's runs, in the order to read them, in place of what was queued: a prefetch asked
    /// and not yet issued is dropped. What is read and wasted from now on is this plan's.
    pub(crate) fn prefetch<G: Give>(&mut self, pool: &Pool<G>, runs: impl IntoIterator<Item = Run>) {
        self.queue.clear();
        self.queue.extend(runs);
        self.base = pool.prefetch_counts();
    }

    /// The front run of the queue shortened by `pages`, and gone when none is left.
    fn passed(&mut self, pages: u64) {
        let front = self.queue.front_mut().expect("a run at the queue's front");
        front.first += pages;
        front.pages -= pages as u32;
        if front.pages == 0 {
            self.queue.pop_front();
        }
    }

    /// Prefetch loads issued from the queue's front, into `effects`, while the rules of the
    /// module's docs let one be: called by the driver once every demand ask it holds is asked,
    /// and again as loads end. How many it issued.
    ///
    /// Bounded: each turn issues a load or shortens the queue, or it returns.
    pub(crate) fn pump<G: Give>(&mut self, pool: &Pool<G>, effects: &mut Vec<Effect>, ended: &mut Vec<Ended>) -> u32 {
        if self.queue.is_empty() {
            return 0;
        }
        let counts = pool.prefetch_counts();
        let wasted = counts.wasted.saturating_sub(self.base.wasted);
        let read = (counts.hit + counts.late).saturating_sub(self.base.hit + self.base.late);
        if wasted > read + WASTE_SLACK {
            self.queue.clear();
            self.stopped += 1;
            return 0;
        }
        let mut issued = 0;
        while let Some(&run) = self.queue.front() {
            if run.pages == 0 {
                self.queue.pop_front();
                continue;
            }
            let demand = self.demand > 0 && !pool.planted(Plant::PrefetchFirst);
            if demand || self.prefetching >= self.most {
                break;
            }
            // The run as far as its file commits: a file retired since the plan is not read.
            let Some(end) = self.file_of(run.first, 1).map(File::last_page) else {
                self.queue.pop_front();
                self.dropped += 1;
                continue;
            };
            let unread = pool.prefetch_counts().unread() + self.prefetching_pages;
            let room = (self.budget / PAGE).saturating_sub(unread);
            if room == 0 {
                break;
            }
            let most = u64::from(run.pages).min(PAGES_PER_READ).min(end - run.first).min(room);
            let slot = *self.slots.last().expect("a free prefetch slot while fewer loads than slots are in flight");
            let mut claimed = 0;
            while claimed < most && pool.claim(run.first + claimed, slot).is_ok() {
                claimed += 1;
            }
            if claimed == 0 {
                // Loading or loaded already, or refused: not asked again.
                self.passed(1);
                continue;
            }
            self.passed(claimed);
            let stretch = Run { first: run.first, pages: claimed as u32 };
            if pool.reuse_run(stretch).is_err() {
                for page in stretch.pages() {
                    pool.unclaim(page);
                }
                self.dropped += 1;
                continue;
            }
            self.slots.pop();
            let load = Load { slot, first: stretch.first, pages: stretch.pages };
            let asked = self.issue(pool, load, Priority::Idle, effects, ended);
            assert!(asked, "a prefetch of a file the loads core knows was refused: {load:?}");
            self.prefetching += 1;
            self.prefetching_pages += claimed;
            issued += 1;
        }
        issued
    }

    pub(crate) fn owns(&self, tag: Tag) -> bool {
        tag.0.checked_sub(self.tags).is_some_and(|it| it < 1 << 32)
    }

    /// The load slot `tag`, one of its own, names.
    pub(crate) fn slot_of(&self, tag: Tag) -> Option<u16> {
        self.owns(tag).then(|| ((tag.0 - self.tags) >> READS_SHIFT) as u16)
    }

    /// Loads in flight.
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Give-backs of a failed load's pages the kernel refused, so far.
    pub(crate) fn refused(&self) -> u64 {
        self.refused
    }

    /// A file it may read from now on.
    pub(crate) fn open(&mut self, opened: Opened) {
        let Opened { window, name, checks } = opened;
        let blocks = window.len.div_ceil(BLOCK);
        assert_eq!(
            checks.len() as u64,
            blocks,
            "{name} opened in the store's pool with {} checks for its {blocks} blocks",
            checks.len()
        );
        self.files.insert(window.first, File { window, name, checks });
    }

    /// The file at `window.first` committed more: `window` as it is now ([`super::Append::commit`]),
    /// and the checks of its new blocks ([`super::Append::checks`]). Told before a reader can
    /// learn the new length, so a load of a new page finds its checks.
    ///
    /// A load in flight is not disturbed: what a read of a page must read changes only for the
    /// page the committed bytes ended in, which the commit pinned, so no load of it is in flight.
    pub(crate) fn grow(&mut self, window: Window, checks: Vec<u32>) {
        let file = self
            .files
            .get_mut(&window.first)
            .unwrap_or_else(|| panic!("{:?} grown in the store pool's loads, which were not told of it", window.id));
        assert!(
            file.window.id == window.id && file.window.len <= window.len,
            "{} of {} bytes grown to {:?} of {} bytes",
            file.name,
            file.window.len,
            window.id,
            window.len
        );
        file.checks.extend(checks);
        assert_eq!(
            file.checks.len() as u64,
            window.len.div_ceil(BLOCK),
            "{} grown to {} bytes with the wrong number of checks",
            file.name,
            window.len
        );
        file.window = window;
    }

    /// The file at window `first` read no more: retired.
    pub(crate) fn close(&mut self, first: u32) {
        self.files.remove(&first);
    }

    /// Whether it knows the file the page `page` is in.
    pub(crate) fn knows(&self, page: u64) -> bool {
        self.file_of(page, 1).is_some()
    }

    fn file_of(&self, first: u64, pages: u32) -> Option<&File> {
        let window = u32::try_from(first / PER_WINDOW).ok()?;
        let (_, file) = self.files.range(..=window).next_back()?;
        let inside = file.window.first_page() <= first && first + u64::from(pages) <= file.last_page();
        inside.then_some(file)
    }

    /// The `n`th read of `load` of `file`.
    fn part(file: &File, load: Load, n: u64) -> Part {
        let first = load.first + n * PAGES_PER_READ;
        let pages = (u64::from(load.pages) - n * PAGES_PER_READ).min(PAGES_PER_READ);
        let at = (first - file.window.first_page()) * PAGE;
        let want = (pages * PAGE).min(file.window.len - at);
        Part { first, pages, at, want, len: want.next_multiple_of(BLOCK) }
    }

    fn tag(&self, slot: u16, n: u64) -> Tag {
        Tag(self.tags + (u64::from(slot) << READS_SHIFT | n))
    }

    /// `load` asked by a reader: its reads, into `effects`. A load of no file it knows ends at
    /// once, failed, into `ended`.
    pub(crate) fn ask<G: Give>(&mut self, pool: &Pool<G>, load: Load, effects: &mut Vec<Effect>, ended: &mut Vec<Ended>) {
        assert!(
            !prefetch_slot(load.slot),
            "a reader's load asked in slot {}, one of the loads core's own for prefetch",
            load.slot
        );
        if self.issue(pool, load, Priority::Demand, effects, ended) {
            self.demand += 1;
        }
    }

    /// `load`'s reads, at `priority`, into `effects`: whether it is in flight. A load of no file
    /// the core knows ends at once, failed, into `ended`, and is not.
    fn issue<G: Give>(
        &mut self,
        pool: &Pool<G>,
        load: Load,
        priority: Priority,
        effects: &mut Vec<Effect>,
        ended: &mut Vec<Ended>,
    ) -> bool {
        assert!(load.pages >= 1, "an empty load in slot {}", load.slot);
        assert!(
            self.table[load.slot as usize].load.is_none(),
            "load slot {} of the store's pool asked again while its load is in flight",
            load.slot
        );
        let Some(file) = self.file_of(load.first, load.pages) else {
            // Nothing was read into them: the asker's claim is undone, its pages given back.
            let failure = LoadFailure::NoFile { first: load.first, pages: load.pages };
            self.giving.clear();
            self.giving.extend(load.first..load.first + u64::from(load.pages));
            for &page in &self.giving {
                let word = pool.word_now(page);
                pool.word(page).store(Word::evicting(word.version()).0, Ordering::Release);
            }
            if !give_back(pool, &self.giving, |_| Word::unloaded) {
                self.refused += 1;
            }
            ended.push(Ended { slot: load.slot, outcome: Err(failure) });
            return false;
        };
        if pool.planted(Plant::LoadedAtAsk) {
            // Published as the read is asked: a pin can take the page while the read writes it.
            for page in load.first..load.first + u64::from(load.pages) {
                let word = pool.word_now(page);
                pool.word(page).store(Word::loaded(word.version(), 0, Referenced::Clear).0, Ordering::Release);
            }
        }
        let reads = u64::from(load.pages).div_ceil(PAGES_PER_READ);
        assert!(reads < 1 << READS_SHIFT, "a load of {} pages: more reads than a tag can name", load.pages);
        let name = &file.name;
        for n in 0..reads {
            let part = Loads::part(file, load, n);
            effects.push(Effect::ReadInto {
                file: name.clone(), // clone: a count of the name
                at: part.at,
                into: pool.span(part.first, part.len as u32),
                cache: self.cache,
                priority,
                tag: self.tag(load.slot, n),
            });
        }
        let first_window = file.window.first;
        let flight = &mut self.table[load.slot as usize];
        flight.load = Some(load);
        flight.file = first_window;
        flight.reads_left = reads as u32;
        flight.ends.clear();
        flight.ends.resize(load.pages as usize, PageEnd::Good);
        flight.failure = None;
        self.in_flight += 1;
        true
    }

    /// A read of a load completed; once its load's last has, the load ends into `ended`.
    pub(crate) fn complete<G: Give>(&mut self, pool: &Pool<G>, completion: Completion, ended: &mut Vec<Ended>) {
        assert!(self.owns(completion.tag), "the store pool's loads told of {completion:?}, not theirs");
        let rel = completion.tag.0 - self.tags;
        let (slot, n) = ((rel >> READS_SHIFT) as usize, rel & ((1 << READS_SHIFT) - 1));
        let flight = &self.table[slot];
        let Some(load) = flight.load else {
            panic!("the store pool's load slot {slot} told of {completion:?} with no load in flight");
        };
        let file = self
            .files
            .get(&flight.file)
            .unwrap_or_else(|| panic!("load slot {slot} of the store's pool read a file retired under it"));
        let part = Loads::part(file, load, n);
        let from = (part.first - load.first) as usize;
        let pages = from..from + part.pages as usize;
        let (end, failure) = match completion.done {
            Done::ReadInto { len } if u64::from(len) < part.want => (
                PageEnd::Again,
                Some(LoadFailure::Short { file: file.name.clone(), at: part.at, read: len, committed: part.want }), // clone: a count of the name
            ),
            Done::ReadInto { .. } => {
                // What lies past the committed bytes reads as zeros, whatever the read or an
                // earlier tenant of the pages left there.
                pool.zero(part.first * PAGE + part.want, part.pages * PAGE - part.want);
                // Every block is checked, not only up to the first that fails: one read covers
                // up to `PAGES_PER_READ` pages, and each page with a bad block is `Corrupt`. The
                // load's failure names the first.
                let flight = &mut self.table[slot];
                if !pool.planted(Plant::NoCheck) {
                    for (block, page) in check(pool, file, part) {
                        flight.ends[(page - load.first) as usize] = PageEnd::Bad;
                        if flight.failure.is_none() {
                            flight.failure = Some(LoadFailure::Block { file: file.name.clone(), block, page }); // clone: a count of the name
                        }
                        if pool.planted(Plant::FirstBadBlock) {
                            // Only the first bad block's page is refused: a later page of the
                            // same read is published with its rotted block.
                            break;
                        }
                    }
                }
                (PageEnd::Good, None)
            }
            Done::Failed(why) if why.0.contains(ABANDONED_SPAN) => (
                PageEnd::Abandoned,
                Some(LoadFailure::Abandoned { file: file.name.clone(), at: part.at, why }), // clone: a count of the name
            ),
            Done::Failed(why) => (
                PageEnd::Again,
                Some(LoadFailure::Read { file: file.name.clone(), at: part.at, len: part.len as u32, why }), // clone: a count of the name
            ),
            done => panic!("a read into the store's pool completed with {done:?}"),
        };
        let flight = &mut self.table[slot];
        if end != PageEnd::Good {
            flight.ends[pages].fill(end);
        }
        if let Some(failure) = failure {
            flight.failure.get_or_insert(failure);
        }
        flight.reads_left -= 1;
        if flight.reads_left == 0 {
            self.finish(pool, slot, ended);
        }
    }

    /// Every read of the load in `slot` is done: each page moved to how it ended, and the load
    /// ended.
    fn finish<G: Give>(&mut self, pool: &Pool<G>, slot: usize, ended: &mut Vec<Ended>) {
        let flight = &mut self.table[slot];
        let load = flight.load.take().expect("a load in flight");
        let mut failure = flight.failure.take();
        if pool.planted(Plant::FailedLoadKept) {
            for end in &mut flight.ends {
                if *end == PageEnd::Again {
                    *end = PageEnd::Good;
                }
            }
            failure = failure.filter(|it| !matches!(it, LoadFailure::Read { .. } | LoadFailure::Short { .. }));
        }
        self.giving.clear();
        let prefetch = prefetch_slot(load.slot);
        let mut good = 0;
        for (i, &end) in flight.ends.iter().enumerate() {
            let page = load.first + i as u64;
            let word = pool.word_now(page);
            let next = match end {
                // Nobody asked: no pin, and marked until its first read.
                PageEnd::Good if prefetch => {
                    good += 1;
                    Word::prefetched_page(word.version())
                }
                // The asker's pin, set as the page is published. Its second-chance bit stays
                // clear: the next read that finds it loaded sets it.
                PageEnd::Good => Word::loaded(word.version(), 1, Referenced::Clear),
                PageEnd::Again | PageEnd::Bad => {
                    self.giving.push(page);
                    Word::evicting(word.version())
                }
                PageEnd::Abandoned => Word::abandoned(word.version()),
            };
            assert!(
                word.state() == State::Loading || pool.planted(Plant::LoadedAtAsk),
                "page {page} of the store's pool, loaded in slot {slot}, is {word:?}"
            );
            pool.word(page).store(next.0, Ordering::Release);
        }
        let ends = &flight.ends;
        let first = load.first;
        let given = give_back(pool, &self.giving, |page| match ends[(page - first) as usize] {
            PageEnd::Bad => Word::corrupt,
            PageEnd::Good | PageEnd::Again | PageEnd::Abandoned => Word::unloaded,
        });
        if !given {
            self.refused += 1;
        }
        self.in_flight -= 1;
        match prefetch {
            true => {
                pool.count_prefetch(Prefetch::Loaded, good);
                self.prefetching -= 1;
                self.prefetching_pages -= u64::from(load.pages);
                self.slots.push(load.slot);
                self.failed += u64::from(failure.is_some());
            }
            false => self.demand -= 1,
        }
        ended.push(Ended { slot: load.slot, outcome: failure.map_or(Ok(()), Err) });
    }
}

/// Every block of `part` that fails its check, with its page, in the file's order. All of them:
/// a read covers several pages, and a caller that stopped at the first would publish a later
/// page with a bad block unrefused (planted bug 10, [`Plant::FirstBadBlock`]).
#[expect(
    blaze::sans_io_core,
    reason = "the CRC-32C is a pure function of the bytes; the syscall is std's one-time CPU feature detection (sysctl on macOS), read once and cached, never IO the core decides on"
)]
fn check<'a, G: Give>(pool: &'a Pool<G>, file: &'a File, part: Part) -> impl Iterator<Item = (u64, u64)> + 'a {
    let blocks = part.want.div_ceil(BLOCK);
    (0..blocks).filter_map(move |b| {
        let len = BLOCK.min(part.want - b * BLOCK);
        let at = part.first * PAGE + b * BLOCK;
        // SAFETY: the part's pages are `Loading` in this load, whose read into them completed:
        // nothing else writes them until this core publishes them.
        #[allow(unsafe_code, reason = "a completed read's bytes, checked")]
        let bytes = unsafe { pool.bytes(at, len) };
        let block = part.at / BLOCK + b;
        let good = file.checks.get(block as usize).is_some_and(|&it| it == block_check(bytes));
        (!good).then_some((block, part.first + b * BLOCK / PAGE))
    })
}

/// `pages` (ascending, each `Evicting`) given back to the kernel, then each moved to the word
/// `then` makes of its version: `false` when the kernel kept some. Those stay resident and are
/// moved all the same: that costs memory, not correctness (their next load writes them whole).
pub(super) fn give_back<G: Give>(pool: &Pool<G>, pages: &[u64], then: impl Fn(u64) -> fn(u64) -> Word) -> bool {
    if pages.is_empty() {
        return true;
    }
    let ranges: Vec<Range<usize>> = super::ranges_of(pages);
    // SAFETY: every page is `Evicting` with no pin (a load's pages are never pinned before they
    // are published), and was made the process's by its asker before the read.
    let given = unsafe { pool.give_back(&ranges) };
    let whole: usize = ranges.iter().map(Range::len).sum();
    for &page in pages {
        let word = pool.word_now(page);
        pool.word(page).store(then(page)(word.version()).0, Ordering::Release);
    }
    given.is_ok_and(|it| it == whole)
}
