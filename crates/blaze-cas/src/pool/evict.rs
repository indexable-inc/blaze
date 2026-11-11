//! The pool's sweep, as a sans-IO core: a second-chance sweep over the open files' pages, in
//! address order, taking the unpinned and unreferenced ones `Loaded -> Evicting` in batches of up
//! to [`BATCH`] and giving each batch back to the kernel in as few calls as the system allows
//! ([`super::Give::evict`]), then `Evicting -> Unloaded` (docs/v2/plan/design/store-pool.md,
//! section 4).
//!
//! - A page with its second-chance bit set (read again since it was loaded, or since the sweep
//!   last came by) has it cleared and is passed over. A load leaves the bit clear, so a page read
//!   once is taken on the first pass after its pin is let go.
//! - A pinned page is passed over; so is a page loading, being given back, or refused.
//! - An index page counts four times its bytes: the sweep takes it on one lap in four, since one
//!   index miss stalls every lookup it covers.
//! - The last page of a pack written in the last two commits is passed over ([`super::Tail`]).
//! - Bounded: one call visits at most twice the open pages, and ends in a typed outcome
//!   ([`Swept`]): the bytes freed, and whether it stopped short (rule `bounded-progress`).
//!
//! `Evicting` is what keeps a load from starting on a page whose bytes are about to be dropped:
//! the version is raised as the page is taken, so an optimistic copy begun before fails its
//! check, and no pin succeeds until the page is `Unloaded` and loaded again.
//!
//! This is the pool's half of the one evictor's interface (memory-pool.md):
//! `evict(bytes) -> freed` ([`Sweep::evict`]), and its charge and what of it can be
//! given back ([`census`]): what admission's gate reads as `evictable` and asks back with
//! `Step::Evict` before it waits or fails (the design's section 4). The plane's side of that
//! (`blaze-graph`'s footprint core) is not on main; nothing calls these from there yet. Ranking
//! the pool against the other caches by its cost to rebuild (its loads' latency a byte) is the
//! plane's too.
//!
//! The counters come from the same walk, once a sample, never from a count kept on a pin: a pin
//! writes its page's word and nothing else.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::sync::atomic::Ordering;

use super::{Give, PAGE, Plant, Pool, Prefetch, Rank, Referenced, State, Tail, Windows, Word, ranges_of};

/// The pool's pages as the plane and the counters ask of them: one walk of the open files'
/// state words ([`census`]). Bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Census {
    /// Pages in memory or on their way (`Loaded`, `Loading`, `Evicting`): the pool's charge,
    /// what `Tag::Pool` holds of the open files.
    pub(crate) loaded: u64,
    /// `Loaded` pages with a pin: not the sweep's to take, and charged to whoever holds them.
    pub(crate) pinned: u64,
    /// What an eviction could give back now: `Loaded`, unpinned, and not a pack's last page
    /// the next commit is likely to write into. A page read since the sweep last came by is
    /// counted: the sweep takes it on its second lap.
    pub(crate) evictable: u64,
    /// Prefetched and not yet read: guesses, the first the sweep takes.
    pub(crate) unread: u64,
}

/// The census of `pool`'s open files. It reads each committed page's word once and writes
/// nothing; the words move under it, so it is a sample, exact only while nothing else runs.
pub(crate) fn census<G: Give>(pool: &Pool<G>, windows: &Windows) -> Census {
    let mut census = Census::default();
    for file in windows.files() {
        let (first, pages) = (file.first_page(), file.pages());
        for page in first..first + pages {
            let word = pool.word_now(page);
            match word.state() {
                State::Loading | State::Evicting => census.loaded += PAGE,
                State::Loaded => {
                    census.loaded += PAGE;
                    let hot_tail = file.tail == Tail::Hot && page + 1 == first + pages;
                    match word.pins() {
                        0 if !hot_tail => census.evictable += PAGE,
                        0 => {}
                        _ => census.pinned += PAGE,
                    }
                    if word.prefetched() {
                        census.unread += PAGE;
                    }
                }
                State::Unloaded | State::Corrupt | State::Abandoned => {}
            }
        }
    }
    census
}

/// Pages one batch gives back at most: one `process_madvise` on Linux, one TLB flush.
pub(crate) const BATCH: usize = 256;

/// What one visit did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Visit {
    /// `page` taken `Loaded -> Evicting` into the batch.
    Taken { page: u64 },
    /// `page` passed over.
    Passed { page: u64 },
    /// The batch is full: give it back first.
    Full,
    /// No open file has a committed page.
    Empty,
}

/// How a call of [`Sweep::evict`] ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Swept {
    /// Bytes given back to the kernel.
    pub(crate) freed: u64,
    /// Pages visited.
    pub(crate) visited: u64,
    /// It stopped before freeing what was asked: every other page is pinned, referenced, loading
    /// or passed over, within its bound of visits.
    pub(crate) short: bool,
    /// The kernel refused a batch, why: its pages stay `Loaded` (memory, not correctness).
    pub(crate) refused: Option<String>,
}

/// The sweep's hand and its batch.
pub(crate) struct Sweep {
    /// The page it visits next.
    hand: u64,
    /// Laps of the open files so far.
    lap: u64,
    batch: Vec<u64>,
    /// Bytes it has given back to the kernel, ever: the `pool evicted bytes` counter.
    evicted: u64,
}

impl Default for Sweep {
    fn default() -> Sweep {
        Sweep::new()
    }
}

impl Sweep {
    pub(crate) fn new() -> Sweep {
        Sweep { hand: 0, lap: 0, batch: Vec::with_capacity(BATCH), evicted: 0 } // alloc: the sweep's batch, once
    }

    /// Bytes it has given back to the kernel, ever.
    pub(crate) fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Pages taken and not yet given back.
    pub(crate) fn batch(&self) -> &[u64] {
        &self.batch
    }

    /// One page visited, at the hand: taken into the batch, or passed over.
    pub(crate) fn visit<G: Give>(&mut self, pool: &Pool<G>, windows: &Windows) -> Visit {
        if self.batch.len() == BATCH {
            return Visit::Full;
        }
        let Some((page, file)) = windows.next_page(self.hand) else { return Visit::Empty };
        if page < self.hand {
            self.lap += 1;
        }
        self.hand = page + 1;
        let passed = Visit::Passed { page };
        let last = file.first_page() + file.pages() - 1;
        if (file.rank == Rank::Index && self.lap % 4 != 3) || (file.tail == Tail::Hot && page == last) {
            return passed;
        }
        let word = pool.word(page);
        let seen = Word(word.load(Ordering::Acquire));
        if seen.state() != State::Loaded {
            return passed;
        }
        if seen.referenced() {
            // Its second chance: a pin that comes meanwhile sets it again.
            _ = word.compare_exchange(seen.0, seen.unreferenced().0, Ordering::AcqRel, Ordering::Relaxed);
            return passed;
        }
        if seen.pins() > 0 && !pool.planted(Plant::EvictPinned) {
            return passed;
        }
        let version = match pool.planted(Plant::SameVersion) {
            true => seen.version(),
            false => seen.next_version(),
        };
        let next = match pool.planted(Plant::NoEvicting) {
            true => Word::unloaded(version),
            // Under the plant, the pins stay in the word, as a broken sweep would leave them.
            false if pool.planted(Plant::EvictPinned) => Word(Word::evicting(version).0 | u64::from(seen.pins())),
            false => Word::evicting(version),
        };
        match word.compare_exchange(seen.0, next.0, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => {
                if seen.prefetched() {
                    // A prefetched page nobody read: a wrong guess, counted.
                    pool.count_prefetch(Prefetch::Wasted, 1);
                }
                self.batch.push(page);
                Visit::Taken { page }
            }
            // A pin came first.
            Err(_) => passed,
        }
    }

    /// The batch given back: its pages' ranges merged in address order and handed to the kernel,
    /// then each page the kernel took `Evicting -> Unloaded` and each it kept `Loaded` again (at
    /// the raised version). The bytes given back.
    ///
    /// # Errors
    ///
    /// The kernel cannot give pages back at all: every page of the batch is `Loaded` again.
    pub(crate) fn give<G: Give>(&mut self, pool: &Pool<G>) -> Result<u64, String> {
        if self.batch.is_empty() {
            return Ok(0);
        }
        self.batch.sort_unstable();
        let ranges = ranges_of(&self.batch);
        // SAFETY: every page of the batch was taken `Loaded{pins: 0} -> Evicting` by this sweep,
        // so no pin holds it and none can succeed until it is `Unloaded`; each was loaded, so
        // made the process's, and not given back since.
        let given = unsafe { pool.give_back(&ranges) };
        let (given, refused) = match given {
            Ok(given) => (given as u64, None),
            // alloc: an error path
            Err(err) => (0, Some(err.to_string())),
        };
        let gone = given / PAGE;
        self.evicted += given;
        // The kernel took the pages of a prefix of the ranges, so of the sorted batch.
        for (i, &page) in self.batch.iter().enumerate() {
            let word = pool.word_now(page);
            if pool.planted(Plant::NoEvicting) {
                continue;
            }
            let next = match (i as u64) < gone {
                true => Word::unloaded(word.version()),
                false => Word::loaded(word.version(), 0, Referenced::Clear),
            };
            pool.word(page).store(next.0, Ordering::Release);
        }
        self.batch.clear();
        match refused {
            Some(why) => Err(why),
            None => Ok(given),
        }
    }

    /// At least `bytes` given back, if the sweep can find them within twice the open pages'
    /// visits: the pool's `evict(bytes) -> freed`.
    pub(crate) fn evict<G: Give>(&mut self, pool: &Pool<G>, windows: &Windows, bytes: u64) -> Swept {
        let bound = 2 * windows.open_pages();
        let mut swept = Swept { freed: 0, visited: 0, short: false, refused: None };
        let give = |sweep: &mut Sweep, swept: &mut Swept| match sweep.give(pool) {
            Ok(freed) => swept.freed += freed,
            Err(why) => swept.refused = Some(why),
        };
        while swept.freed < bytes && swept.refused.is_none() {
            if swept.visited >= bound {
                swept.short = true;
                break;
            }
            match self.visit(pool, windows) {
                Visit::Taken { .. } => {
                    swept.visited += 1;
                    if swept.freed + self.batch.len() as u64 * PAGE >= bytes {
                        give(self, &mut swept);
                    }
                }
                Visit::Passed { .. } => swept.visited += 1,
                Visit::Full => give(self, &mut swept),
                Visit::Empty => {
                    swept.short = true;
                    break;
                }
            }
        }
        give(self, &mut swept);
        swept.short |= swept.freed < bytes;
        swept
    }
}
