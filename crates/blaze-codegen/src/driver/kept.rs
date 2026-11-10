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

use blaze_core::lock::Lock;

use super::{
    delta::{Fresh, Last},
    patch::Walked,
};
use crate::{backend::Described, plan::Origin};

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
    /// Each program's last walk whole ([`Walked`]), by [`program`] of its inputs, which the
    /// walk itself holds and a build compares: taken by the build that patches it, so a
    /// locked map.
    walks: Lock<std::collections::HashMap<u64, Walked<I, S>>>,
    /// What each program's last build gave, by [`program`] of every input of the build: its
    /// ID and its entries, which the next build's [`super::Delta`] is made against.
    built: SharedMap<u64, Arc<Last>>,
}

impl<I: Eq + Hash, S> Default for Kept<I, S> {
    fn default() -> Self {
        Kept {
            rounds: std::array::from_fn(|_| SharedMap::default()),
            // alloc: none until a walk is kept
            walks: Lock::new("codegen.driver.kept_walks", std::collections::HashMap::new()),
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

    /// Takes the walk kept of `program`, if one is: a build that patches it owns it, and
    /// keeps the patched one in its place when it ends well.
    pub(super) fn take_walk(&self, program: u64) -> Option<Walked<I, S>> {
        self.walks.lock().remove(&program)
    }

    /// Keeps `walked` as the walk of `program`.
    pub(super) fn keep_walk(&self, program: u64, walked: Walked<I, S>) {
        self.walks.lock().insert(program, walked);
    }
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
    /// The functions of this program's last build that are not in this one's
    /// ([`super::Delta`]'s `removed`).
    pub dropped: usize,
    /// Why the walk ran whole, every function reached from the roots again, in place of
    /// patching the walk kept of the program; `None` for a patched walk.
    pub whole: Option<&'static str>,
    /// The functions and statics a patched walk touched: the changed ones and those that
    /// refer to them. 0 for a whole walk, which touches every one.
    pub visited: usize,
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
