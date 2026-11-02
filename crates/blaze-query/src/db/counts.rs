//! Re-execution counts, per ingredient: how many of its rows ran, how many were proved
//! unchanged without running, and how many ran again in a view they had already run in (a
//! value evicted and asked for again). Process-wide, as ingredient numbers are.
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

/// One ingredient's counts in one block, and what takes have reported of them.
#[derive(Default)]
struct Counters {
    now: [AtomicU64; 3],
    taken: [AtomicU64; 3],
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
            at.store(at.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
    });
}

/// What each ingredient's counts gained since the last take, summed over every thread, as
/// `each(name, executed, verified, reran)`; an ingredient that gained nothing is left out.
pub(crate) fn take(mut each: impl FnMut(&'static str, u64, u64, u64)) {
    let ingredients = (ingredient::registered() as usize + 1).min(MAX_INGREDIENTS);
    // alloc: once per take (a build's end), a word triple per registered ingredient
    let mut gained = vec![[0u64; 3]; ingredients];
    for block in blocks() {
        for (gained, counters) in gained.iter_mut().zip(&block.counters[..ingredients]) {
            for what in 0..3 {
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
    for (index, [executed, verified, reran]) in gained.into_iter().enumerate().skip(1) {
        if executed != 0 || verified != 0 || reran != 0 {
            each(ingredient::name(index as u32), executed, verified, reran);
        }
    }
}
