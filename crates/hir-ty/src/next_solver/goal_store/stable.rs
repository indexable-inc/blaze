//! A goal's stable form: `persist`'s words of its predicate and param env with each definition
//! as its own variant and its item key ([`GoalFacts::def_key`]) where `persist` writes an
//! ID, so the same goal has the same words in every database. Written either into a hash
//! ([`goal_key`], a lookup, on the stack) or into words ([`goal_words`], an offer, into the
//! thread's buffer).

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use hir_def::AdtId;

use super::{GoalFacts, GoalKey};
use crate::next_solver::{
    AdtDef, Clauses, Ctor, DbInterner, Predicate, SolverDefId, ensure_sufficient_stack,
    persist::{Code, Node, Sink},
};

pub(super) const GOAL_DOMAIN: &str = "blaze v2 goal 1";

/// The longest stable form kept: a goal longer than this is not stored, as the hash of a lookup
/// would cost more than a solve saves.
pub(super) const MAX_WORDS: usize = 1 << 16;

/// The words of a definition: its variant, then its item key.
pub(super) const DEF_WORDS: usize = 1 + 8;

/// A goal of the global-goal table, as its key names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Goal<'db> {
    pub(super) predicate: Predicate<'db>,
    pub(super) clauses: Clauses<'db>,
    /// The typing mode's tag, as [`GoalKey`] has it.
    pub(super) mode: u8,
}

impl<'db> Goal<'db> {
    #[inline]
    pub(super) fn of(key: GoalKey) -> Goal<'db> {
        Goal {
            predicate: key.predicate(),
            clauses: key.param_env().clauses(),
            mode: key.mode_tag(),
        }
    }
}

/// The key of `goal` for the crate of `interner`, `None` when its stable form is longer than
/// [`MAX_WORDS`]. Allocates nothing: the words go into a hash as they come.
pub(super) fn goal_key<'db>(
    interner: DbInterner<'db>,
    // dyn: lane items' item keys, one call per definition of a goal on a global-table miss
    facts: &dyn GoalFacts,
    version: &ContentId,
    crate_key: &ContentId,
    goal: Goal<'db>,
) -> Option<ContentId> {
    let mut sink = Stable { db: interner.db, facts, out: Hashed::new(version, crate_key), len: 0 };
    put_goal(&mut sink, interner, goal);
    let len = sink.len;
    (len <= MAX_WORDS).then(|| sink.out.finish(len))
}

/// The stable form of `goal` into `words`; `false` when it is longer than [`MAX_WORDS`].
pub(super) fn goal_words<'db>(
    interner: DbInterner<'db>,
    // dyn: lane items' item keys, one call per definition of a goal offered after a solve
    facts: &dyn GoalFacts,
    goal: Goal<'db>,
    words: &mut Vec<u32>,
) -> bool {
    words.clear();
    let mut sink = Stable { db: interner.db, facts, out: Words(words), len: 0 };
    put_goal(&mut sink, interner, goal);
    sink.len <= MAX_WORDS
}

/// The key of the goal whose stable form is `words`: [`goal_key`]'s.
pub(super) fn key_of(version: &ContentId, crate_key: &ContentId, words: &[u32]) -> ContentId {
    let mut out = Hashed::new(version, crate_key);
    for &word in words {
        out.push(word);
    }
    out.finish(words.len())
}

fn put_goal<'db, O: Out>(sink: &mut Stable<'_, 'db, O>, interner: DbInterner<'db>, goal: Goal<'db>) {
    sink.word(u32::from(goal.mode));
    sink.word(u32::from(interner.regions()));
    sink.word(u32::try_from(rustc_type_ir::Interner::recursion_limit(interner)).unwrap_or(u32::MAX));
    goal.predicate.put(sink);
    goal.clauses.put(sink);
}

/// Where a stable form's words go.
trait Out {
    fn push(&mut self, word: u32);
}

struct Words<'a>(&'a mut Vec<u32>);

impl Out for Words<'_> {
    #[inline]
    fn push(&mut self, word: u32) {
        // alloc: the thread's buffer, kept from offer to offer; grows to the longest goal
        self.0.push(word);
    }
}

/// Words into a hash, through a buffer on the stack.
struct Hashed {
    hasher: Hasher,
    buf: [u8; 1024],
    at: usize,
}

impl Hashed {
    fn new(version: &ContentId, crate_key: &ContentId) -> Hashed {
        let mut hasher = Hasher::new(GOAL_DOMAIN);
        hasher.id(version).id(crate_key);
        Hashed { hasher, buf: [0; 1024], at: 0 }
    }

    fn finish(mut self, len: usize) -> ContentId {
        self.hasher.update(&self.buf[..self.at]);
        self.hasher.update(&(len as u64).to_le_bytes());
        self.hasher.finish()
    }
}

impl Out for Hashed {
    #[inline]
    fn push(&mut self, word: u32) {
        if self.at == self.buf.len() {
            self.hasher.update(&self.buf);
            self.at = 0;
        }
        self.buf[self.at..self.at + 4].copy_from_slice(&word.to_le_bytes());
        self.at += 4;
    }
}

/// `persist`'s sink with children inline and definitions by item key. Once past
/// [`MAX_WORDS`] it only counts, and stops descending.
struct Stable<'a, 'db, O> {
    db: &'db Db,
    // dyn: lane items' item keys, one call per definition written
    facts: &'a dyn GoalFacts,
    out: O,
    len: usize,
}

impl<'db, O: Out> Sink<'db> for Stable<'_, 'db, O> {
    #[inline]
    fn word(&mut self, word: u32) {
        self.len += 1;
        if self.len <= MAX_WORDS {
            self.out.push(word);
        }
    }

    fn node<N: Node<'db>>(&mut self, node: N) {
        if self.len > MAX_WORDS {
            return;
        }
        ensure_sufficient_stack(|| node.put_node(self));
    }

    fn adt(&mut self, adt: AdtDef) {
        self.def(SolverDefId::AdtId(adt.def_id()));
        self.word(u32::from(adt.flag_bits()));
    }

    fn def(&mut self, def: SolverDefId<'db>) {
        self.word(def_variant(def));
        if self.len > MAX_WORDS {
            return;
        }
        let key = self.facts.def_key(self.db, def);
        for word in key.0.chunks_exact(4) {
            self.word(u32::from_le_bytes([word[0], word[1], word[2], word[3]]));
        }
    }
}

/// A definition's variant, as `persist` numbers them.
pub(super) fn def_variant(def: SolverDefId<'_>) -> u32 {
    match def {
        SolverDefId::AdtId(AdtId::StructId(_)) => 0,
        SolverDefId::AdtId(AdtId::UnionId(_)) => 1,
        SolverDefId::AdtId(AdtId::EnumId(_)) => 2,
        SolverDefId::ConstId(_) => 3,
        SolverDefId::FunctionId(_) => 4,
        SolverDefId::ImplId(_) => 5,
        SolverDefId::BuiltinDeriveImplId(_) => 6,
        SolverDefId::StaticId(_) => 7,
        SolverDefId::AnonConstId(_) => 8,
        SolverDefId::TraitId(_) => 9,
        SolverDefId::TypeAliasId(_) => 10,
        SolverDefId::InternedClosureId(_) => 11,
        SolverDefId::InternedCoroutineId(_) => 12,
        SolverDefId::InternedCoroutineClosureId(_) => 13,
        SolverDefId::InternedOpaqueTyId(_) => 14,
        SolverDefId::EnumVariantId(_) => 15,
        SolverDefId::Ctor(Ctor::Struct(_)) => 16,
        SolverDefId::Ctor(Ctor::Enum(_)) => 17,
    }
}
