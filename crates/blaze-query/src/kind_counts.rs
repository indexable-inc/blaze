//! What each registered query of a [`Database`] did, as [`db::Counts`]: runs and proofs
//! without running, so this database's queries (the daemon's plan, `blaze_units::query::Plan`,
//! and the workspace load under it) have `query_counts` rows in a build's profile beside the
//! front end's (handoff-12 request 1's second part).
//!
//! Counted where a memo is made or proved, never on a hit (a view that resolved the key finds
//! it with no count): an atomic add a resolve, on a counter of its kind's. Of
//! [`db::Counts`]'s fields this database has `executed`, `verified`, `evicted_asked`,
//! `ran_new` and `ran_changed`; the rest are the front end's engine's own (its budget, its
//! read stacks, its validity census) and are 0 here, never estimated.

use crate::{
    Database, db,
    sync::atomic::{AtomicU64, Ordering},
};

pub(crate) const EXECUTED: usize = 0;
pub(crate) const VERIFIED: usize = 1;
pub(crate) const EVICTED_ASKED: usize = 2;
pub(crate) const RAN_NEW: usize = 3;
pub(crate) const RAN_CHANGED: usize = 4;
const KINDS: usize = 5;

/// One query kind's counters: what they reached, and what the last take reported.
pub(crate) struct KindCounts {
    now: [AtomicU64; KINDS],
    taken: [AtomicU64; KINDS],
}

impl KindCounts {
    pub(crate) fn new() -> KindCounts {
        KindCounts {
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
    /// database's own, unlike [`db::take_counts`], which is the process's.
    pub fn take_counts(&self, mut each: impl FnMut(&'static str, db::Counts)) {
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
                db::Counts {
                    executed: gained[EXECUTED],
                    verified: gained[VERIFIED],
                    evicted_asked: gained[EVICTED_ASKED],
                    ran_new: gained[RAN_NEW],
                    ran_changed: gained[RAN_CHANGED],
                    ..db::Counts::default()
                },
            );
        }
    }
}
