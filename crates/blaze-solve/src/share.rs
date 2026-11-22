//! Trait goals shared by every body of a request (runtime.md design points 5 and 6, plan row
//! A4): a goal is solved once a request and every other body that asks it reads the answer.
//!
//! THE TABLE is the caller's ([`GoalShare`]): one request-wide concurrent map that this crate
//! never sees (it stays sans-IO and `no_std`); the solver is handed it through its fact source
//! ([`crate::facts::Source::goals`]) and the table hands the solver the request's options
//! ([`GoalShare::opts`]: the audit, and the planted bug of the controls).
//!
//! THE KEY ([`GoalKey`]) is the digest of words that are the goal's whole identity: the
//! solver's code id ([`SOLVER_CODE_ID`]), the recursion limit, the typing mode, the binder's
//! bound count, the canonical words of the goal's elaborated parameter environment (its
//! where-clauses in scope: a goal under `T: Clone` is another key than the goal without it)
//! and the goal's canonical predicate. The words go with the digest, so a table that meets two
//! goals under one digest refuses by type ([`ShareRefused::Collision`], rule 21).
//!
//! WHAT IS SHARED (the exact condition, eval.rs `share_refusal`): a goal finished with no cycle
//! head below it (the search graph's final results, the ones its own global cache keeps: a
//! non-root member of a cycle is provisional and never put), whose canonical input has NO
//! canonical variable (`vars == 0`: nothing of the body's own inference context is in it), no
//! placeholder (its universe numbers are the solver's state), no closure and no coroutine (the
//! body's own items, whose facts are the body's), no error and no canonical term; a nested
//! goal's overflow result (valid only at the depth it met) and a goal that read a `where const`
//! condition (its question names arena terms) are not put either. Each refusal is counted by
//! reason in `Stats`.
//!
//! THE VALUE ([`SharedGoal`]) is the goal's canonical answer (certainty, how a trait goal was
//! proven, the response's words; or no solution), the depth it required, whether it
//! overflowed, and its READ LIST: every fact the solve pulled (impl buckets by trait and shape,
//! trait definitions, headers, ..) with the digest each answered, and that list's digest.
//!
//! THE SCOPE. Which impls exist is part of an answer, and bodies of one request are checked in
//! different crates' scopes. The key does not name the scope; the READER checks the entry's
//! read list against its own facts before it takes the answer (eval.rs `share_replay`): every
//! question asked again of the body's facts, each digest compared. An entry whose list holds a
//! digest the body's facts answer otherwise is not taken (`shared_refused_scope`) and the goal
//! is solved in the body. So a shared answer is a function of its key and of the facts its
//! list names, which is what a record valid by its read list is (rule 77).
//!
//! A HIT IS ONE READ ([`GoalHit`]): the solver lists each goal it took with its key and its
//! read digest, for the body's result to name as one cell. TODAY (this piece) a hit ALSO
//! appends the goal's own reads to the body's list, as solving it in the body would, so the
//! engine's invalidation is unchanged; [`ShareOpts::one_read_only`] (tests only, until the
//! engine keeps goals as records with their own rows) records the one read alone.

use core::hash::{Hash, Hasher};

use blaze_own::{Alloc, Vec};

use crate::assembly::Via;
use crate::error::BadPred;
use crate::facts::Read;
use crate::term::Certainty;

/// The solver's code id in every shared key: bumped with any change to what a goal answers or
/// to the key's layout, so a table never serves an answer of other rules (rule 52).
pub const SOLVER_CODE_ID: u32 = 1;

/// The most words a shared key holds (rule 18): an environment and a goal of
/// `canon::MAX_WORDS` each.
pub const MOST_KEY_WORDS: usize = 2 * crate::canon::MAX_WORDS + 8;

/// The request's sharing options, the table's to give ([`GoalShare::opts`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShareOpts {
    /// THE AUDIT (runtime.md point 6): every hit is also solved afresh in the body and the two
    /// are compared (the answer's words, the entry's read digest, and the reads the hit would
    /// have recorded); a difference is [`ShareError::Audit`] naming the goal. Off by default.
    pub audit: bool,
    /// A hit records only its one goal read ([`GoalHit`]) and none of the goal's own reads: the
    /// form the engine takes once goals are records with their own rows. Off (and only for
    /// tests) while the engine invalidates a body by its own read list alone.
    pub one_read_only: bool,
    /// PLANTED BUG, tests only: a hit returns the answer and records NO read, neither the
    /// goal's nor its own. The audit and the read-set controls must catch it.
    #[doc(hidden)]
    pub plant_hit_without_read: bool,
}

/// A shared goal's key: the digest of its words, and the words (module docs, "THE KEY").
#[derive(Clone, Copy, Debug)]
pub struct GoalKey<'k> {
    pub digest: u128,
    pub words: &'k [u32],
}

/// A shared answer: a response's certainty, how a trait goal was proven, and its canonical
/// words (canon.rs `Responses`: `[nvals]` and the values, then the goals handed back).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedAns<'v> {
    pub certainty: Certainty,
    pub via: Via,
    pub words: &'v [u32],
}

/// A shared goal's entry (module docs, "THE VALUE"). `ans` is `None` for no solution.
#[derive(Clone, Copy, Debug)]
pub struct SharedGoal<'v> {
    pub ans: Option<SharedAns<'v>>,
    pub required_depth: u32,
    pub overflow: bool,
    /// The goal's reads, sorted and each once.
    pub reads: &'v [Read],
    /// [`reads_digest`] of `reads`.
    pub reads_digest: u64,
}

/// Why the table did not take or give an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareRefused {
    /// Two goals' words under one digest (rule 21): a refusal by type, never a wrong answer.
    Collision { digest: u128 },
    /// The table holds its most entries (rule 18): the goal stays the body's.
    Full { most: usize },
}

/// The request's table of shared goals (module docs). Implemented beside the body task
/// (blaze-ladder front2 tables/goals.rs); a test's table in this crate's tests.
pub trait GoalShare: Sync {
    /// The request's sharing options.
    fn opts(&self) -> ShareOpts;
    /// Calls `see` with the entry of `key` when the table holds one: `Ok(true)`. An entry of
    /// other words under `key.digest` is `Err(Collision)`.
    fn get(&self, key: GoalKey<'_>, see: &mut dyn FnMut(SharedGoal<'_>)) -> Result<bool, ShareRefused>;
    /// Puts `goal` under `key` unless an entry is there: `Ok(true)` when this put made it,
    /// `Ok(false)` when one was there (the first put stays: equal by the key, rule 25).
    fn put(&self, key: GoalKey<'_>, goal: SharedGoal<'_>) -> Result<bool, ShareRefused>;
}

/// A goal a body took from the table: the one read its result names (a cell named by the
/// goal's key, its digest the goal's read digest).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GoalHit {
    pub key: u128,
    pub reads_digest: u64,
}

/// Why a goal was not shared, each counted in `Stats` (`shared_refused_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refuse {
    /// Its canonical input has a canonical variable.
    Vars,
    /// A placeholder, a closure, a coroutine, an error or a canonical term in the goal.
    Local,
    /// It read a `where const` condition.
    ConstHolds,
    /// A nested goal's overflow result.
    Overflow,
    /// An entry's read answered otherwise in this body's facts.
    Scope,
    /// The table was full.
    Full,
}

/// What the audit found unequal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditMiss {
    /// The answers (certainty, how proven, or the response's words).
    Answer,
    /// The entry's read digest and the fresh solve's.
    EntryReads,
    /// The reads the hit recorded and the fresh solve's (the planted hit without its read).
    HitReads,
}

/// The sharing's own failures (`SolveError::Share`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareError {
    /// The audit solved a hit afresh and it answered otherwise (`what`): the goal, by its
    /// refusal's shape, and its key's digest.
    Audit { goal: BadPred, key: u128, what: AuditMiss },
    /// Two goals' words under one digest ([`ShareRefused::Collision`]).
    Collision { digest: u128 },
    /// An entry held a read that cannot be asked again (a `where const` condition, whose
    /// question names arena terms): the put refuses such goals, so a table that gives one is
    /// not this crate's.
    Unreplayable,
}

/// The digest of a key's words: two independent 64-bit FNV passes, each finished with a mix.
pub fn key_digest(words: &[u32]) -> u128 {
    let lo = crate::search::hash_words(words);
    let mut h: u64 = 0x6c62_272e_07bb_0142;
    for &x in words {
        h = (h ^ u64::from(x)).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(5);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    (u128::from(h) << 64) | u128::from(lo)
}

/// The digest of a read list, sorted and each once: its length, then each read as its derived
/// `Hash` writes it through [`Fnv`] (every integer little-endian at a fixed width, so the
/// digest holds nothing of the machine: rule 25).
pub fn reads_digest(reads: &[Read]) -> u64 {
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    h.write_u64(reads.len() as u64);
    for r in reads {
        r.hash(&mut h);
    }
    h.finish()
}

/// FNV-1a over bytes, each integer written little-endian at its own width (`usize` and `isize`
/// as 64 bits).
struct Fnv(u64);

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        let mut h = self.0;
        h ^= h >> 31;
        h = h.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        h ^ (h >> 29)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn write_u8(&mut self, v: u8) {
        self.write(&[v]);
    }

    fn write_u16(&mut self, v: u16) {
        self.write(&v.to_le_bytes());
    }

    fn write_u32(&mut self, v: u32) {
        self.write(&v.to_le_bytes());
    }

    fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }

    fn write_u128(&mut self, v: u128) {
        self.write(&v.to_le_bytes());
    }

    fn write_usize(&mut self, v: usize) {
        self.write(&(v as u64).to_le_bytes());
    }

    fn write_i8(&mut self, v: i8) {
        self.write(&v.to_le_bytes());
    }

    fn write_i16(&mut self, v: i16) {
        self.write(&v.to_le_bytes());
    }

    fn write_i32(&mut self, v: i32) {
        self.write(&v.to_le_bytes());
    }

    fn write_i64(&mut self, v: i64) {
        self.write(&v.to_le_bytes());
    }

    fn write_i128(&mut self, v: i128) {
        self.write(&v.to_le_bytes());
    }

    fn write_isize(&mut self, v: isize) {
        self.write(&(v as i64).to_le_bytes());
    }
}

/// An entry as the solver copies it out of the table ([`GoalShare::get`]'s `see`), in the
/// solver's allocator.
pub(crate) struct Taken<'a> {
    pub ans: Option<(Certainty, Via)>,
    pub words: Vec<u32, Alloc<'a>>,
    pub reads: Vec<Read, Alloc<'a>>,
    pub required_depth: u32,
    pub overflow: bool,
    pub reads_digest: u64,
}

impl<'a> Taken<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Taken<'a> {
        Taken { ans: None, words: Vec::new_in(alloc), reads: Vec::new_in(alloc), required_depth: 0, overflow: false, reads_digest: 0 }
    }

    pub fn clear(&mut self) {
        self.ans = None;
        self.words.clear();
        self.reads.clear();
        self.required_depth = 0;
        self.overflow = false;
        self.reads_digest = 0;
    }

    pub fn fill(&mut self, g: SharedGoal<'_>) {
        self.clear();
        if let Some(a) = g.ans {
            self.ans = Some((a.certainty, a.via));
            self.words.extend_from_slice(a.words);
        }
        self.reads.extend_from_slice(g.reads);
        self.required_depth = g.required_depth;
        self.overflow = g.overflow;
        self.reads_digest = g.reads_digest;
    }
}

/// What the audit keeps of a hit to compare with the fresh solve (eval.rs `share_after`).
pub(crate) struct Audit<'a> {
    pub key: u128,
    pub ans: Option<(Certainty, Via)>,
    pub words: Vec<u32, Alloc<'a>>,
    pub reads_digest: u64,
    /// [`reads_digest`] of what the hit would have recorded.
    pub hit_reads: u64,
}

/// A goal a root's solve finished with a final result, waiting to be put when the root ends
/// (eval.rs `finish` pushes, `share_after` puts).
#[derive(Clone, Copy, Debug)]
pub(crate) struct NewGoal {
    pub input: u32,
    pub entry: crate::search::Entry,
    pub overflow: bool,
    pub why: Option<Refuse>,
}
