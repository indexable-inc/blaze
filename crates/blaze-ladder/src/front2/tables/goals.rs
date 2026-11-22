//! The request's table of trait goals shared by every body task (blaze-solve share.rs, plan
//! row A4): one concurrent map for a request's workers, keyed by the digest of a goal's whole
//! identity, holding its canonical answer and its read list. A body task is handed it through
//! its frame (task.rs `BodyFrame::goals`); its solver looks each root goal up before solving
//! it, takes an entry only when the entry's reads answer the same in the body's own facts, and
//! puts the final goals it solved.
//!
//! Lives for one request and holds only what bodies rebuild (rule 70): nothing of it is kept.
//! Capped ([`GoalTableOpts::most`], rule 18): a full table refuses a put by type and the goal
//! stays the body's.

use core::sync::atomic::{AtomicUsize, Ordering};
use std::alloc::Global;
use std::vec::Vec;

use blaze_solve::Certainty;
use blaze_solve::assembly::Via;
use blaze_solve::facts::Read;
use blaze_solve::share::{GoalKey, GoalShare, ShareOpts, ShareRefused, SharedAns, SharedGoal};

/// The most goals a request's table holds by default: a million-line workspace's bodies ask
/// some 10^6 distinct root goals; each row is some hundred bytes. AT THE CAP a put is refused
/// by type (`ShareRefused::Full`, counted in the solver's `shared_refused_full`) and the goal
/// stays the body's; nothing is ever evicted, so a row once put answers to the request's end.
pub const MOST_GOALS: usize = 1 << 21;

/// [`GoalTable::new`]'s options.
#[derive(Clone, Copy, Debug)]
pub struct GoalTableOpts {
    /// The request's sharing options: the audit, the planted bug (blaze-solve `ShareOpts`).
    pub share: ShareOpts,
    /// The most rows ([`MOST_GOALS`] outside tests).
    pub most: usize,
}

/// One shared goal, owned.
struct Row {
    words: Vec<u32, Global>,
    ans: Option<(Certainty, Via)>,
    ans_words: Vec<u32, Global>,
    required_depth: u32,
    overflow: bool,
    reads: Vec<Read, Global>,
    reads_digest: u64,
}

/// The request's table (module docs).
pub struct GoalTable {
    opts: GoalTableOpts,
    /// Rows put (counted before the insert, given back when it lost: never past `most`).
    len: AtomicUsize,
    rows: blaze_core::SharedMap<u128, Row, Global>,
}

impl GoalTable {
    pub fn new(opts: GoalTableOpts) -> GoalTable {
        GoalTable { opts, len: AtomicUsize::new(0), rows: blaze_core::SharedMap::new_in("ladder.goals", Global) }
    }

    /// The rows the table holds.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl GoalShare for GoalTable {
    fn opts(&self) -> ShareOpts {
        self.opts.share
    }

    fn get(&self, key: GoalKey<'_>, see: &mut dyn FnMut(SharedGoal<'_>)) -> Result<bool, ShareRefused> {
        let rows = self.rows.pin();
        let Some(r) = rows.get(&key.digest) else { return Ok(false) };
        if r.words[..] != *key.words {
            return Err(ShareRefused::Collision { digest: key.digest });
        }
        let ans = r.ans.map(|(certainty, via)| SharedAns { certainty, via, words: &r.ans_words });
        see(SharedGoal { ans, required_depth: r.required_depth, overflow: r.overflow, reads: &r.reads, reads_digest: r.reads_digest });
        Ok(true)
    }

    fn put(&self, key: GoalKey<'_>, goal: SharedGoal<'_>) -> Result<bool, ShareRefused> {
        let rows = self.rows.pin();
        if let Some(r) = rows.get(&key.digest) {
            return if r.words[..] == *key.words { Ok(false) } else { Err(ShareRefused::Collision { digest: key.digest }) };
        }
        let most = self.opts.most;
        if self.len.fetch_add(1, Ordering::Relaxed) >= most {
            self.len.fetch_sub(1, Ordering::Relaxed);
            return Err(ShareRefused::Full { most });
        }
        let mut words = Vec::with_capacity_in(key.words.len(), Global);
        words.extend_from_slice(key.words);
        let mut ans_words = Vec::new_in(Global);
        if let Some(a) = goal.ans {
            ans_words.extend_from_slice(a.words);
        }
        let mut reads = Vec::with_capacity_in(goal.reads.len(), Global);
        reads.extend_from_slice(goal.reads);
        let row = Row {
            words,
            ans: goal.ans.map(|a| (a.certainty, a.via)),
            ans_words,
            required_depth: goal.required_depth,
            overflow: goal.overflow,
            reads,
            reads_digest: goal.reads_digest,
        };
        match rows.try_insert(key.digest, row) {
            Ok(_) => Ok(true),
            // Another worker put it first: its row stays (equal by the key, rule 25).
            Err(lost) => {
                self.len.fetch_sub(1, Ordering::Relaxed);
                if lost.current.words[..] == *key.words { Ok(false) } else { Err(ShareRefused::Collision { digest: key.digest }) }
            }
        }
    }
}
