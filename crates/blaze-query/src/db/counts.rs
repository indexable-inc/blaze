//! Re-execution counts, per ingredient: how many of its rows ran, how many were proved
//! unchanged without running, how many ran again in a view they had already run in, how many
//! ran because their value was evicted and asked for again (in any view), and how many values
//! a budget's pass and an explicit drop let go; and its runs' reads: recorded, kept by the
//! rows, and the most one run held. For an input, how many times a setter wrote one of its
//! fields: what a build changed, which names the input behind every run that follows. And
//! why each run ran, by its [`Cause`]: first runs, a read that changed, an untracked read, a
//! cycle (an evicted value is `evicted_asked`), so "which kinds reran, and why" is a count
//! with no per-run record. Process-wide, as ingredient numbers are.
//!
//! [`Cause`]: super::Cause
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
/// A pass let a value go to keep its tag's values under their budget (`super::budget`).
pub(crate) const EVICTED: usize = 4;
/// `evict_values` or `evict_values_of` dropped a value.
pub(crate) const DROPPED: usize = 5;
/// Reads a run recorded (before repeats were dropped), counted as it stored its edges.
const READS_RECORDED: usize = 6;
/// Reads a run's row kept: its distinct reads.
const READS_KEPT: usize = 7;
/// A setter wrote a field of an input (`super::input`), equal value or not: each write opens a
/// view, so it is what every run after it traces back to.
pub(crate) const WRITTEN: usize = 8;
/// A row ran for the first time ([`Cause::New`]).
///
/// [`Cause::New`]: super::Cause::New
pub(crate) const RAN_NEW: usize = 9;
/// A row ran because one of its reads changed ([`Cause::Changed`]).
///
/// [`Cause::Changed`]: super::Cause::Changed
pub(crate) const RAN_CHANGED: usize = 10;
/// A row ran because its last run made an untracked read ([`Cause::Untracked`]).
///
/// [`Cause::Untracked`]: super::Cause::Untracked
pub(crate) const RAN_UNTRACKED: usize = 11;
/// A row ran for a cycle: a stale provisional row, or a verify that re-entered a cycle
/// ([`Cause::Cycle`]).
///
/// [`Cause::Cycle`]: super::Cause::Cycle
pub(crate) const RAN_CYCLE: usize = 12;
const KINDS: usize = 13;

/// The most reads one run of each ingredient held on its thread's edge stack at its end since
/// the last [`take`], which resets it: a mark, not a sum, so one word an ingredient shared by
/// every thread, written only when a run passes it.
static MOST_READS: [AtomicU64; MAX_INGREDIENTS] = [const { AtomicU64::new(0) }; MAX_INGREDIENTS];

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
    /// Values a pass let go to keep their tag's values under its budget.
    pub evicted: u64,
    /// Values `evict_values` or `evict_values_of` dropped.
    pub dropped: u64,
    /// Reads its runs recorded, repeats of an earlier read in the same run included (a read
    /// equal to the run's last is never recorded): with `reads_kept`, how often its runs read
    /// the same rows again.
    pub reads_recorded: u64,
    /// Reads its runs' rows kept: each run's distinct reads, summed.
    pub reads_kept: u64,
    /// The most entries one of its runs held on its thread's edge stack at once (repeats are
    /// dropped once a run holds `max(4096, 2 x kept)`): what one run cost the stack.
    pub reads_most: u64,
    /// Of an input: the times a setter wrote one of its fields, equal value or not (each opens
    /// a view). What a build changed: a no-op build's are the inputs it set anyway.
    pub written: u64,
    /// Of `executed`, first runs: a cold build's runs, and on a warm one the rows it never
    /// had (a restore that lost them shows here).
    pub ran_new: u64,
    /// Of `executed`, runs because a read changed since the row last ran: what an edit should
    /// cause, and nothing else on a no-op build.
    pub ran_changed: u64,
    /// Of `executed`, runs because the row's last run made an untracked read
    /// (`report_untracked_read`): it runs in every view, so a no-op build's runs are these.
    pub ran_untracked: u64,
    /// Of `executed`, runs for a cycle: a stale provisional row, or a verify that re-entered
    /// a cycle. A cycle whose results are never reused runs here on every build.
    pub ran_cycle: u64,
}

impl Counts {
    fn of(gained: [u64; KINDS], reads_most: u64) -> Counts {
        let [
            executed,
            verified,
            reran,
            evicted_asked,
            evicted,
            dropped,
            reads_recorded,
            reads_kept,
            written,
            ran_new,
            ran_changed,
            ran_untracked,
            ran_cycle,
        ] = gained;
        Counts {
            executed,
            verified,
            reran,
            evicted_asked,
            evicted,
            dropped,
            reads_recorded,
            reads_kept,
            reads_most,
            written,
            ran_new,
            ran_changed,
            ran_untracked,
            ran_cycle,
        }
    }
}

/// One run's reads, as it stored its edges ([`reads`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Reads {
    /// Recorded: held, and the repeats dropped as the stack filled.
    pub(crate) recorded: u64,
    /// Kept by its row: its distinct reads.
    pub(crate) kept: u64,
    /// The most it held on the edge stack at once ([`super::local::Frame::held_most`]).
    pub(crate) most: u64,
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
    with_counters(ingredient, |counters| bump(counters, what, by));
}

/// Counts one run's reads to `ingredient`, once the run stored its edges.
#[inline(never)]
pub(crate) fn reads(ingredient: u32, reads: Reads) {
    with_counters(ingredient, |counters| {
        bump(counters, READS_RECORDED, reads.recorded);
        bump(counters, READS_KEPT, reads.kept);
    });
    if let Some(most) = MOST_READS.get(ingredient as usize)
        && most.load(Ordering::Relaxed) < reads.most
    {
        most.fetch_max(reads.most, Ordering::Relaxed);
    }
}

fn bump(counters: &Counters, what: usize, by: u64) {
    let at = &counters.now[what];
    at.store(at.load(Ordering::Relaxed) + by, Ordering::Relaxed);
}

/// Lends `f` this thread's counters of `ingredient`.
fn with_counters(ingredient: u32, f: impl FnOnce(&Counters)) {
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
            f(counters);
        }
    });
}

/// What each ingredient's counts gained since the last take, summed over every thread, and the
/// most reads one of its runs held since then, as `each(name, counts)`; an ingredient that
/// gained nothing is left out. Process-wide: two databases' counts, and two overlapping
/// builds', land in whichever take comes first.
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
        let most = MOST_READS[index].swap(0, Ordering::Relaxed);
        if most != 0 || gained.iter().any(|&it| it != 0) {
            each(ingredient::name(index as u32), Counts::of(gained, most));
        }
    }
}
