//! The pool's counters, for the daemon's memory line and `blaze explain memory`
//! (docs/v2/plan/design/store-pool.md, section 4, "With admission and the ceiling"): every
//! store's pool of the process summed, since a daemon holds two (its own and std's) and the
//! footprint is one number (rule `one-memory-measure`).
//!
//! What the pool holds resident is not counted here: it is `Tag::Pool`'s live bytes in
//! blaze-mem's own table, which the pool's region moves as pages are loaded and given back,
//! and which admission already reads. These are what that number cannot say: how much of it
//! is pinned (not the sweep's to take), and what moved through it.
//!
//! None is written on a hit. A pin already held by a reader's table writes nothing; the
//! first pin of a run by a table, a load and a sweep's batch each write one counter once.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bytes of the runs pinned now by readers' tables ([`crate::read::Owned`]) and by values
/// held past their task ([`crate::Pinned`]): whole pages, a page counted once per run that
/// holds it.
static PINNED: AtomicU64 = AtomicU64::new(0);
/// Bytes read into pools by demand loads, ever.
static LOADED: AtomicU64 = AtomicU64::new(0);
/// Bytes read into pools by prefetches, ever.
static PREFETCHED: AtomicU64 = AtomicU64::new(0);
/// Bytes sweeps gave back to the kernel, ever.
static EVICTED: AtomicU64 = AtomicU64::new(0);
/// Reads that waited at the pinned bound, ever.
static BOUND_WAITS: AtomicU64 = AtomicU64::new(0);

/// The pools of this process, as [`pool_counts`] read them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PoolCounts {
    /// Bytes of the pools' pages the process holds now: `Tag::Pool`'s live bytes, what the
    /// footprint counts.
    pub resident: u64,
    /// The most they should hold: `Tag::Pool`'s budget, its share of the daemon's ceiling
    /// (`blaze_graph::admission::set_budgets`); `u64::MAX` until it is set.
    pub budget: u64,
    /// Bytes of `resident` pinned by readers now: not the sweep's to take.
    pub pinned: u64,
    /// The most readers pin before a read waits for the store to let pins go
    /// ([`pinned_bound`]).
    pub pinned_bound: u64,
    /// Bytes demand loads read, ever.
    pub loaded: u64,
    /// Bytes prefetches read, ever.
    pub prefetched: u64,
    /// Bytes sweeps gave back, ever.
    pub evicted: u64,
    /// Reads that waited at the pinned bound, ever.
    pub bound_waits: u64,
}

/// The counters of every store's pool in this process.
pub fn pool_counts() -> PoolCounts {
    let budget = budget();
    PoolCounts {
        resident: blaze_mem::tag::snapshot().get(blaze_mem::Tag::Pool).live,
        budget,
        pinned: PINNED.load(Ordering::Relaxed),
        pinned_bound: pinned_bound(budget),
        loaded: LOADED.load(Ordering::Relaxed),
        prefetched: PREFETCHED.load(Ordering::Relaxed),
        evicted: EVICTED.load(Ordering::Relaxed),
        bound_waits: BOUND_WAITS.load(Ordering::Relaxed),
    }
}

/// The pools' budget: `Tag::Pool`'s, 16 parts in 64 (a quarter) of what the daemon runs
/// under once `blaze_graph::admission::set_budgets` has set it, `u64::MAX` before.
pub(crate) fn budget() -> u64 {
    blaze_mem::tag::budget(blaze_mem::Tag::Pool)
}

/// The least the pinned bound is, whatever the budget: a link's inputs and a few large
/// values pinned at once.
pub(crate) const PINNED_LEAST: u64 = 64 << 20;
/// The most the pinned bound is while no budget is set (a test, a tool).
pub(crate) const PINNED_UNSET: u64 = 1 << 30;

/// The pinned bound under `budget`: half of it, so that at least half of the pool's budget
/// is always the sweep's to take, and never under [`PINNED_LEAST`].
pub(crate) fn pinned_bound(budget: u64) -> u64 {
    match budget {
        u64::MAX => PINNED_UNSET,
        budget => (budget / 2).max(PINNED_LEAST),
    }
}

pub(crate) fn pinned(bytes: u64) {
    PINNED.fetch_add(bytes, Ordering::Relaxed);
}

pub(crate) fn unpinned(bytes: u64) {
    PINNED.fetch_sub(bytes, Ordering::Relaxed);
}

/// Bytes pinned now.
pub(crate) fn pinned_now() -> u64 {
    PINNED.load(Ordering::Relaxed)
}

pub(crate) fn loaded(bytes: u64) {
    LOADED.fetch_add(bytes, Ordering::Relaxed);
}

pub(crate) fn prefetched(bytes: u64) {
    PREFETCHED.fetch_add(bytes, Ordering::Relaxed);
}

pub(crate) fn evicted(bytes: u64) {
    EVICTED.fetch_add(bytes, Ordering::Relaxed);
}

pub(crate) fn bound_waited() {
    BOUND_WAITS.fetch_add(1, Ordering::Relaxed);
}
