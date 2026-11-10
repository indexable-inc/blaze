//! What a walk keeps for the next walk over the same front end
//! (docs/v2/plan/design/incremental-walk.md): each function's describe, by the instance and
//! whose code asked for it, so the next walk asks the front end only whether it still holds
//! ([`crate::backend::Frontend::redescribe`]) and describes again what changed and what is new.
//!
//! Nothing here decides what a build holds. The walk reaches what it reaches from its roots
//! every time, through each describe's references, kept or new: a kept describe no root
//! reaches any more is never looked at, and one the front end says changed is never used. So
//! the walk over kept describes equals the walk over none whenever the front end's answer is
//! exact, which `driver/tests/kept.rs` holds it to.
//!
//! A kept describe names the front end's instances, which are ids of its database: it lives
//! in memory beside that database and is never written to the store. Whoever makes a [`Kept`]
//! keeps it for one database and one configuration of the front end (`hir-ty-codegen`'s
//! `HirFrontend::kept`); a restart begins with none and the first walk describes everything,
//! which [`Walk::fresh`] says ([`Fresh::Restart`]).

use std::{
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use blaze_hash::SharedMap;

use super::delta::{Fresh, Last};
use crate::{InstanceKey, backend::Described, plan::Origin};

/// The rounds of a walk whose describes are kept apart: the first, and the one over the facts
/// of the whole program the first closed ([`crate::backend::Frontend::closed`]). A body built
/// against those facts is another describe than the one before them, so one table would hold
/// whichever round ran last and each build would describe such a body twice.
const ROUNDS: usize = 2;

/// The describes of the walks over one database and configuration of a front end, of every
/// program walked over it: two programs that reach one instance share its describe. Shared by
/// the builds of a job, which walk at once: a describe is lent by one lookup and kept by one
/// insert, with nothing held between.
pub struct Kept<I, S> {
    /// Each round's describes, by the instance and whose code asked for it.
    rounds: [SharedMap<(I, Origin), Arc<Described<I, S>>>; ROUNDS],
    /// Each program's last walk, by [`program`]: the first word of each function's key,
    /// sorted. Read only to count what a walk dropped ([`Walk::dropped`]); nothing is built
    /// or left out by it, so two keys of one first word only miscount by one.
    programs: SharedMap<u64, Arc<[u64]>>,
    /// What each program's last build gave, by [`program`] of every input of the build: its
    /// ID and its entries, which the next build's [`super::Delta`] is made against.
    built: SharedMap<u64, Arc<Last>>,
}

impl<I: Eq + Hash, S> Default for Kept<I, S> {
    fn default() -> Self {
        Kept {
            rounds: std::array::from_fn(|_| SharedMap::default()),
            programs: SharedMap::default(),
            built: SharedMap::default(),
        }
    }
}

impl<I: Clone + Eq + Hash + Send + Sync, S: Send + Sync> Kept<I, S> {
    /// Nothing kept: what the first walk over a database begins with.
    pub fn new() -> Self {
        Kept::default()
    }

    /// How many describes are kept, over every round.
    pub fn len(&self) -> usize {
        self.rounds.iter().map(|it| it.pin().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The describe of `asked` the walk's round `round` (from 0) kept. The count is shared, so
    /// the table's guard is let go before the caller asks the front end anything.
    pub(super) fn function(
        &self,
        round: usize,
        asked: &(I, Origin),
    ) -> Option<Arc<Described<I, S>>> {
        // clone: a reference count, once per function a walk reaches
        self.rounds[round.min(ROUNDS - 1)].pin().get(asked).cloned()
    }

    /// Keeps `described` as the describe of `asked` in `round`, in place of any before.
    pub(super) fn keep(&self, round: usize, asked: &(I, Origin), described: Arc<Described<I, S>>) {
        // clone: the walk's handle of the function and who asks for it, once per describe
        self.rounds[round.min(ROUNDS - 1)].pin().insert(asked.clone(), described);
    }

    /// What `program`'s last build gave, if one was kept.
    pub(super) fn last(&self, program: u64) -> Option<Arc<Last>> {
        // clone: a reference count, once per build
        self.built.pin().get(&program).cloned()
    }

    /// Keeps `built` as what `program`'s last build gave.
    pub(super) fn built(&self, program: u64, built: Arc<Last>) {
        self.built.pin().insert(program, built);
    }

    /// Records `keys`, the functions of `program`'s walk in key order, as its last walk, and
    /// gives how many functions of the one before are not among them; `None` for a program
    /// never walked over this.
    pub(super) fn walked(&self, program: u64, keys: &[InstanceKey]) -> Option<usize> {
        // alloc: per build, eight bytes a function, kept until the program's next walk
        let now: Arc<[u64]> = keys.iter().map(first_word).collect();
        debug_assert!(now.is_sorted(), "a walk's keys are sorted, so their first words are");
        let programs = self.programs.pin();
        // clone: a reference count, once per build
        let before = programs.get(&program).cloned();
        programs.insert(program, now.clone()); // clone: a reference count, once per build
        before.map(|before| missing(&before, &now))
    }
}

/// The first eight bytes of a key, most significant first: keys in order give words in order.
fn first_word(key: &InstanceKey) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&key.0.0[..8]);
    u64::from_be_bytes(word)
}

/// How many of `before` are not in `now`, both sorted: one pass over the two.
fn missing(before: &[u64], now: &[u64]) -> usize {
    let (mut at, mut missing) = (0, 0);
    for word in before {
        while now.get(at).is_some_and(|it| it < word) {
            at += 1;
        }
        if now.get(at) != Some(word) {
            missing += 1;
        }
    }
    missing
}

/// Which program a walk is of, among those walked over one [`Kept`]: a hash of its roots as
/// the front end holds them, in order. Of one process and one database, never persisted.
pub(super) fn program<T: Hash>(roots: &T) -> u64 {
    use std::hash::BuildHasher as _;
    blaze_hash::FixedState.hash_one(roots)
}

/// What a walk did: the fields of the `codegen walk` span, and [`super::Built::walk`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Walk {
    /// Describes the front end was asked for: functions new to the kept walk, and kept ones
    /// the front end said changed. Over every round.
    pub described: usize,
    /// Kept describes the front end said still hold: each cost its answer, never a describe.
    pub verified: usize,
    /// The functions of the walk's result.
    pub reached: usize,
    /// The functions of this program's last walk that are not in this one's result.
    pub dropped: usize,
    /// Why every describe was asked for anew, when no kept one was looked at.
    pub fresh: Option<Fresh>,
}

/// A walk's counts while its levels describe on all cores.
#[derive(Default)]
pub(super) struct Counts {
    pub(super) described: AtomicUsize,
    pub(super) verified: AtomicUsize,
}

impl Counts {
    pub(super) fn described(&self) {
        self.described.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn verified(&self) {
        self.verified.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn get(&self) -> (usize, usize) {
        (self.described.load(Ordering::Relaxed), self.verified.load(Ordering::Relaxed))
    }
}
