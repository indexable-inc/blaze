//! Re-execution counts, per ingredient: how many of its rows ran, how many were proved
//! unchanged without running, how many ran again in a view they had already run in, how many
//! ran because their value was evicted and asked for again (in any view), and how many values
//! the admission rule let go that recency alone would have kept, and how many values a cap's
//! sweep and an explicit drop let go. Process-wide, as ingredient numbers are.
//!
//! Each thread counts into a block of its own with a load and a store (no read-modify-write,
//! no cache line two threads write), found through one thread-local read on the run and
//! verify paths; a warm hit counts nothing. Blocks are never freed: one whose thread ended
//! goes to the next thread that counts, so its counts stay in the sums, and [`take`] walks
//! them with no reclamation. A take reports what each counter gained since the last one.

use std::{
    cell::Cell,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use super::ingredient::{self, MAX_INGREDIENTS};

/// A row ran.
pub(crate) const EXECUTED: usize = 0;
/// A row was proved unchanged in a view without running.
pub(crate) const VERIFIED: usize = 1;
/// A row ran in a view it had already run in.
pub(crate) const RERAN: usize = 2;
/// A row ran because its value was evicted and a reader asked for it ([`Cause::Evicted`]).
///
/// [`Cause::Evicted`]: super::Cause::Evicted
pub(crate) const EVICTED_ASKED: usize = 3;
/// A sweep evicted a value [`super::Eviction::Recency`] would have kept: an admission the
/// frequency rule refused.
pub(crate) const REFUSED: usize = 4;
/// A sweep evicted a value past the ingredient's cap.
pub(crate) const EVICTED: usize = 5;
/// `evict_values` or `evict_values_of` dropped a value.
pub(crate) const DROPPED: usize = 6;
const KINDS: usize = 7;

/// What one ingredient's rows did since the last [`take`].
///
/// No hit is counted: a warm hit is four loads and no shared write, and a count on it would be
/// the hot path's one store. `executed` against `evicted_asked` is what eviction cost.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    /// Runs.
    pub executed: u64,
    /// Rows proved unchanged in a view without running.
    pub verified: u64,
    /// Runs in a view the row had already run in.
    pub reran: u64,
    /// Runs for a value evicted earlier and asked for again: the time eviction cost.
    pub evicted_asked: u64,
    /// Values a sweep evicted that recency alone would have kept.
    pub refused: u64,
    /// Values a sweep evicted past the cap.
    pub evicted: u64,
    /// Values `evict_values` or `evict_values_of` dropped.
    pub dropped: u64,
}

impl Counts {
    fn of(gained: [u64; KINDS]) -> Counts {
        let [executed, verified, reran, evicted_asked, refused, evicted, dropped] = gained;
        Counts { executed, verified, reran, evicted_asked, refused, evicted, dropped }
    }
}

/// One ingredient's counts in one block, and what takes have reported of them.
#[derive(Default)]
struct Counters {
    now: [AtomicU64; KINDS],
    taken: [AtomicU64; KINDS],
}

/// One thread's counts, by ingredient. Only the thread holding it writes `now`.
struct Block {
    counters: Box<[Counters]>,
    /// Its thread ended: the next thread to count may take it.
    free: AtomicBool,
    next: OnceLock<&'static Block>,
}

static FIRST: OnceLock<&'static Block> = OnceLock::new();

fn blocks() -> impl Iterator<Item = &'static Block> {
    std::iter::successors(FIRST.get().copied(), |it| it.next.get().copied())
}

/// A block for this thread: a free one, or a new one appended to the chain.
#[cold]
fn claim() -> &'static Block {
    for block in blocks() {
        // Acquire: the counts its last thread stored are seen before this thread adds to them.
        if block.free.compare_exchange(true, false, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return block;
        }
    }
    // alloc: once per thread that counts, kept for the process and reused by later threads
    let made: &'static Block = Box::leak(Box::new(Block {
        // alloc: the same block's counters
        counters: std::iter::repeat_with(Counters::default).take(MAX_INGREDIENTS).collect(),
        free: AtomicBool::new(false),
        next: OnceLock::new(),
    }));
    let mut slot = &FIRST;
    while slot.set(made).is_err() {
        slot = &slot.get().copied().expect("a slot another thread set").next;
    }
    made
}

/// This thread's block, given back when the thread ends.
struct Held(Cell<Option<&'static Block>>);

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(block) = self.0.get() {
            // Release: this thread's counts are stored before another thread takes the block.
            block.free.store(true, Ordering::Release);
        }
    }
}

std::thread_local! {
    static HELD: Held = const { Held(Cell::new(None)) };
}

/// Adds one to counter `what` of `ingredient` on this thread.
///
/// Not inlined, and it ends before any wait: a job on a fiber may resume on another thread.
#[inline(never)]
pub(crate) fn count(ingredient: u32, what: usize) {
    add(ingredient, what, 1);
}

/// Adds `by` to counter `what` of `ingredient` on this thread, as [`count`] does one.
#[inline(never)]
pub(crate) fn add(ingredient: u32, what: usize, by: u64) {
    // A thread past its thread-locals' teardown counts nothing.
    let _ = HELD.try_with(|held| {
        let block = match held.0.get() {
            Some(block) => block,
            None => {
                let block = claim();
                held.0.set(Some(block));
                block
            }
        };
        if let Some(counters) = block.counters.get(ingredient as usize) {
            let at = &counters.now[what];
            at.store(at.load(Ordering::Relaxed) + by, Ordering::Relaxed);
        }
    });
}

/// What each ingredient's counts gained since the last take, summed over every thread, as
/// `each(name, counts)`; an ingredient that gained nothing is left out. Process-wide: two
/// databases' counts, and two overlapping builds', land in whichever take comes first.
pub fn take(mut each: impl FnMut(&'static str, Counts)) {
    let ingredients = (ingredient::registered() as usize + 1).min(MAX_INGREDIENTS);
    // alloc: once per take (a build's end), a word per kind per registered ingredient
    let mut gained = vec![[0u64; KINDS]; ingredients];
    for block in blocks() {
        for (gained, counters) in gained.iter_mut().zip(&block.counters[..ingredients]) {
            for what in 0..KINDS {
                let now = counters.now[what].load(Ordering::Relaxed);
                if now == 0 {
                    continue;
                }
                // `fetch_max`: of two takes at once, each reports only what it raised the mark
                // by, so no count is reported twice.
                let was = counters.taken[what].fetch_max(now, Ordering::Relaxed);
                gained[what] += now.saturating_sub(was);
            }
        }
    }
    for (index, gained) in gained.into_iter().enumerate().skip(1) {
        if gained.iter().any(|&it| it != 0) {
            each(ingredient::name(index as u32), Counts::of(gained));
        }
    }
}
