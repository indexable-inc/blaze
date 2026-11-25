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

use core::alloc::Allocator;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::alloc::Global;

use blaze_own::Vec;
use blaze_solve::{Certainty, Def};
use blaze_solve::assembly::Via;
use blaze_solve::facts::{Question, Read};
pub use blaze_solve::share::{GoalShare, ReplayMode, ShareOpts};
use blaze_solve::share::{GoalKey, ShareRefused, SharedAns, SharedGoal};
use blaze_solve::term::Shape;

/// The most goals a request's table holds by default: a million-line workspace's bodies ask
/// some 10^6 distinct root goals; each row is some hundred bytes. AT THE CAP a put is refused
/// by type (`ShareRefused::Full`, counted in the solver's `shared_refused_full`) and the goal
/// stays the body's; nothing is ever evicted, so a row once put answers to the request's end.
pub const MOST_GOALS: usize = 1 << 21;

/// Sharing options for a group-scoped table (one member crate's bodies, identical crate scope
/// and immutable records).
pub fn share_opts_same_scope() -> ShareOpts {
    ShareOpts { replay: ReplayMode::SameScope, ..ShareOpts::default() }
}

/// Whether `d` (first 16 bytes of a `blaze_id::ItemKey`, big-endian) carries [`blaze_id::ItemKey::INNER`],
/// marking an item declared inside a body (front2 locals.rs).
fn is_inner_def(d: Def) -> bool {
    d.0.to_be_bytes()[8] & blaze_id::ItemKey::INNER != 0
}

fn shape_is_body_local(shape: Shape) -> bool {
    match shape {
        Shape::Closure(_) | Shape::Coroutine(_) => true,
        Shape::Adt(d) | Shape::Foreign(d) | Shape::FnDef(d) | Shape::Dynamic(d) => is_inner_def(d),
        _ => false,
    }
}

/// Whether `read` depends on body-local state (an opaque's hidden type, a `where const` condition,
/// or an item declared inside a body whose key sets `ItemKey::INNER`). Such goals are never
/// published into a group's shared table so another body's `share_replay` can never observe a
/// body-local item or pollute its read list before hitting `Refuse::Scope` (rules 21, 25).
fn read_is_body_local(read: &Read) -> bool {
    match read.question {
        Question::Opaque(_) | Question::OpaqueHidden(_) | Question::ConstHolds(..) => true,
        Question::TraitDef(d) | Question::ImplHeader(d) | Question::Adt(d) | Question::FnSig(d) | Question::IsLocal(d) => is_inner_def(d),
        Question::Impls(d, shape) => is_inner_def(d) || shape_is_body_local(shape),
        Question::Specializes(a, b) => is_inner_def(a) || is_inner_def(b),
        // `assoc_def` hashes into all 128 bits without clearing byte 8's `INNER` bit; a body-local
        // trait's associated item also reads its `TraitDef` or `ImplHeader` above.
        Question::Assoc(_) | Question::Lang(_) => false,
    }
}

/// [`GoalTable::new`]'s options.
#[derive(Clone, Copy, Debug)]
pub struct GoalTableOpts {
    /// The request's sharing options: the audit, the planted bug (blaze-solve `ShareOpts`).
    pub share: ShareOpts,
    /// The most rows ([`MOST_GOALS`] outside tests).
    pub most: usize,
}

/// One shared goal, owned.
struct Row<A: Allocator> {
    words: Vec<u32, A>,
    ans: Option<(Certainty, Via)>,
    ans_words: Vec<u32, A>,
    required_depth: u32,
    overflow: bool,
    reads: Vec<Read, A>,
    reads_digest: u64,
}

/// The request's table (module docs), backed by one [`blaze_core::SharedMap`] in `A` (`Global`
/// or `SharedAlloc<'static>` in `engine_bodies.rs`) and dropped when `Bodies` ends (rule 70).
pub struct GoalTable<A: Allocator + Clone = Global> {
    opts: GoalTableOpts,
    len: AtomicUsize,
    rows: blaze_core::SharedMap<u128, Row<A>, A>,
    alloc: A,
}

impl GoalTable<Global> {
    pub fn new(opts: GoalTableOpts) -> GoalTable<Global> {
        GoalTable::new_in(opts, Global)
    }
}

impl<A: Allocator + Copy + Send + Sync + 'static> GoalTable<A> {
    pub fn new_in(opts: GoalTableOpts, alloc: A) -> GoalTable<A> {
        GoalTable {
            opts,
            len: AtomicUsize::new(0),
            rows: blaze_core::SharedMap::new_in("ladder.goals", alloc),
            alloc,
        }
    }

    /// The rows the table holds.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<A: Allocator + Copy + Send + Sync + 'static> GoalShare for GoalTable<A> {
    fn opts(&self) -> ShareOpts {
        self.opts.share
    }

    fn contains(&self, key: GoalKey<'_>) -> Result<bool, ShareRefused> {
        let rows = self.rows.pin();
        let Some(r) = rows.get(&key.digest) else { return Ok(false) };
        if r.words.as_slice() != key.words {
            return Err(ShareRefused::Collision { digest: key.digest });
        }
        Ok(true)
    }

    fn get(&self, key: GoalKey<'_>, see: &mut dyn FnMut(SharedGoal<'_>)) -> Result<bool, ShareRefused> {
        let rows = self.rows.pin();
        let Some(r) = rows.get(&key.digest) else { return Ok(false) };
        if r.words.as_slice() != key.words {
            return Err(ShareRefused::Collision { digest: key.digest });
        }
        let ans = r.ans.map(|(certainty, via)| SharedAns { certainty, via, words: r.ans_words.as_slice() });
        see(SharedGoal { ans, required_depth: r.required_depth, overflow: r.overflow, reads: r.reads.as_slice(), reads_digest: r.reads_digest });
        Ok(true)
    }

    fn put(&self, key: GoalKey<'_>, goal: SharedGoal<'_>) -> Result<bool, ShareRefused> {
        if goal.reads.iter().any(read_is_body_local) {
            return Ok(false);
        }
        let rows = self.rows.pin();
        if let Some(r) = rows.get(&key.digest) {
            return if r.words.as_slice() == key.words { Ok(false) } else { Err(ShareRefused::Collision { digest: key.digest }) };
        }
        let most = self.opts.most;
        if self.len.fetch_add(1, Ordering::Relaxed) >= most {
            self.len.fetch_sub(1, Ordering::Relaxed);
            return Err(ShareRefused::Full { most });
        }
        let mut words = Vec::with_capacity_in(key.words.len(), self.alloc);
        words.extend_from_slice(key.words);
        let mut ans_words = Vec::with_capacity_in(goal.ans.map_or(0, |a| a.words.len()), self.alloc);
        if let Some(a) = goal.ans {
            ans_words.extend_from_slice(a.words);
        }
        let mut reads = Vec::with_capacity_in(goal.reads.len(), self.alloc);
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
                if lost.current.words.as_slice() == key.words { Ok(false) } else { Err(ShareRefused::Collision { digest: key.digest }) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_solve::Def;
    use blaze_solve::facts::{Question as SQ, Read as SRead};
    use blaze_solve::share::{GoalKey, GoalShare, ReplayMode, ShareRefused, SharedGoal};
    use blaze_solve::term::Lang;

    use super::{GoalTable, GoalTableOpts, MOST_GOALS, share_opts_same_scope};

    #[test]
    fn goal_table_contains_same_scope_and_skips_inner() {
        let alloc = blaze_own::testing::heap().shared();
        let table = GoalTable::new_in(GoalTableOpts { share: share_opts_same_scope(), most: MOST_GOALS }, alloc);
        assert_eq!(table.opts(), share_opts_same_scope());
        assert_eq!(table.opts().replay, ReplayMode::SameScope);
        assert!(table.is_empty());

        let words_a = [1u32, 2, 3];
        let words_b = [1u32, 2, 4];
        let key_a = GoalKey { digest: 0xdead_beef, words: &words_a };
        let key_b = GoalKey { digest: 0xdead_beef, words: &words_b };
        assert_eq!(table.contains(key_a), Ok(false));

        let goal = SharedGoal {
            ans: None,
            required_depth: 1,
            overflow: false,
            reads: &[SRead { question: SQ::Lang(Lang::Sized), digest: 7 }],
            reads_digest: 99,
        };
        assert_eq!(table.put(key_a, goal), Ok(true));
        assert_eq!(table.contains(key_a), Ok(true));
        assert_eq!(table.contains(key_b), Err(ShareRefused::Collision { digest: 0xdead_beef }));
        assert_eq!(table.len(), 1);

        // A goal reading a body-local `INNER` item is never stored in the shared table.
        let inner_def = Def(0x0001_0000_0000_0000_8000_0000_0000_0003);
        let inner_key = GoalKey { digest: 0xcafe_babe, words: &[9u32] };
        let inner_goal = SharedGoal {
            ans: None,
            required_depth: 1,
            overflow: false,
            reads: &[SRead { question: SQ::TraitDef(inner_def), digest: 11 }],
            reads_digest: 101,
        };
        assert_eq!(table.put(inner_key, inner_goal), Ok(false));
        assert_eq!(table.contains(inner_key), Ok(false));
        assert_eq!(table.len(), 1);
    }
}
