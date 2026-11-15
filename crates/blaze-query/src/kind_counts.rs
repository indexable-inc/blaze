//! What each registered query of a [`Database`] did, as [`KindCounts`]: runs and proofs
//! without running, so this database's queries (the daemon's plan, `blaze_units::query::Plan`,
//! and the workspace load under it) have `query_counts` rows in a build's profile (handoff-12
//! request 1's second part).
//!
//! Counted where a memo is made or proved, never on a hit (a view that resolved the key finds
//! it with no count): an atomic add a resolve, on a counter of its kind's. The profile's
//! `query_counts` columns this database does not count (the deleted tracked engine's budget,
//! read stacks and validity census) are 0 there, never estimated.

use crate::{
    Database,
    sync::atomic::{AtomicU64, Ordering},
};

pub(crate) const EXECUTED: usize = 0;
pub(crate) const VERIFIED: usize = 1;
pub(crate) const EVICTED_ASKED: usize = 2;
pub(crate) const RAN_NEW: usize = 3;
pub(crate) const RAN_CHANGED: usize = 4;
const KINDS: usize = 5;

/// What one query kind gained since the last take ([`Database::take_counts`]).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct KindCounts {
    /// Runs.
    pub executed: u64,
    /// Memos proved unchanged in a view without running.
    pub verified: u64,
    /// Runs for a value evicted earlier and asked for again: the time eviction cost.
    pub evicted_asked: u64,
    /// Of `executed`, first runs of a key.
    pub ran_new: u64,
    /// Of `executed`, runs because a read changed since the memo last ran.
    pub ran_changed: u64,
}

/// One query kind's counters: what they reached, and what the last take reported.
pub(crate) struct KindCounters {
    now: [AtomicU64; KINDS],
    taken: [AtomicU64; KINDS],
}

impl KindCounters {
    pub(crate) fn new() -> KindCounters {
        KindCounters {
            now: std::array::from_fn(|_| AtomicU64::new(0)),
            taken: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    pub(crate) fn count(&self, what: usize) {
        self.now[what].fetch_add(1, Ordering::Relaxed);
    }

    /// What each counter gained since the last take. Of two takes at once, each reports only
    /// what it raised the mark by (`fetch_max`), so no count is reported twice.
    fn take(&self) -> [u64; KINDS] {
        std::array::from_fn(|what| {
            let now = self.now[what].load(Ordering::Relaxed);
            let was = self.taken[what].fetch_max(now, Ordering::Relaxed);
            now.saturating_sub(was)
        })
    }
}

impl Database {
    /// What each registered query gained since the last take, as `each(name, counts)` in the
    /// order the kinds were registered; a kind that gained nothing is left out. This
    /// database's own.
    pub fn take_counts(&self, mut each: impl FnMut(&'static str, KindCounts)) {
        for &kind in &self.kinds.registered {
            let Some(info) = self.kinds.get(kind) else {
                unreachable!("kind {} is listed as registered, and has no row", kind.0)
            };
            let gained = info.counts.take();
            if gained.iter().all(|&it| it == 0) {
                continue;
            }
            each(
                info.name,
                KindCounts {
                    executed: gained[EXECUTED],
                    verified: gained[VERIFIED],
                    evicted_asked: gained[EVICTED_ASKED],
                    ran_new: gained[RAN_NEW],
                    ran_changed: gained[RAN_CHANGED],
                },
            );
        }
    }
}
