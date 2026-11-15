//! What a program's walk keeps for the next build of the same program over the same front end
//! (docs/v2/plan/design/incremental-walk.md): its last walk ([`Walked`]), to be patched by
//! what the front end names as changed, and its last `Built` by row or by its entries, for
//! the next build's delta.
//!
//! No describe is kept here apart from a kept walk's own nodes. Until 2026-11-13 every
//! describe of every walk was kept a second time in a table a round (`rounds`), by the
//! instance and who asked, for a whole walk to verify in place of a describe: a describe was
//! then held three times (that table, the kept walk's nodes, the build's own), and an entry
//! no root reached any more was never removed (lane F6 of the memory wave, read from code).
//! A function a whole walk reaches is now the store's record of it or the front end's
//! describe ([`super::records`], `Build::found_or_described`): the record is its one holder
//! between builds (docs/v2/plan/design/end-state/memory.md 4.4).
//!
//! Nothing here decides what a build holds. The walk reaches what it reaches from its roots
//! every time, patched or whole, so the walk over a kept walk equals the walk over none
//! whenever the front end's changed set is exact, which `driver/tests/kept.rs` holds it to.
//!
//! A kept walk names the front end's instances, which are ids of its database: it lives in
//! memory beside that database, owned by whoever owns the database (`hir-ty-codegen`'s
//! `Walks`, dropped with it), and is never written to the store. A restart begins with none
//! and the first walk is whole, which [`Walk::fresh`] says ([`Fresh::Restart`]).

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
    lists::Lists,
    patch::Walked,
};

/// What the builds of the programs walked over one database and configuration of a front end
/// keep for their next builds: each program's last walk and its last `Built`. Shared by the
/// builds of a job, which walk at once: a walk or lists are taken by the one build that
/// patches them and kept again as it ends.
pub struct Kept<I, S> {
    /// Each program's last walk whole ([`Walked`]), by [`program`] of its inputs, which the
    /// walk itself holds and a build compares: taken by the build that patches it, so a
    /// locked map.
    walks: Lock<std::collections::HashMap<u64, Walked<I, S>>>,
    /// What each program's last build gave, by [`program`] of every input of the build: its
    /// ID and its entries, which the next build's [`super::Delta`] is made against. Of a
    /// build that is not kept by row (`lists`): a lazy one, one with a folded or release
    /// function.
    built: SharedMap<u64, Arc<Last>>,
    /// Each program's last `Built` by row ([`Lists`]), by the [`program`] its walk is kept
    /// under: taken by the build that patches it, as its walk is.
    lists: Lock<std::collections::HashMap<u64, Lists>>,
}

impl<I: Eq + Hash, S> Default for Kept<I, S> {
    fn default() -> Self {
        Kept {
            // alloc: none until a walk is kept
            walks: Lock::new("codegen.driver.kept_walks", std::collections::HashMap::new()),
            built: SharedMap::default(),
            // alloc: none until a build's lists are kept
            lists: Lock::new("codegen.driver.kept_lists", std::collections::HashMap::new()),
        }
    }
}

impl<I: Clone + Eq + Hash + Send + Sync, S: Send + Sync> Kept<I, S> {
    /// Nothing kept: what the first walk over a database begins with.
    pub fn new() -> Self {
        Kept::default()
    }

    /// How many programs have something kept: a walk, lists or what a build gave.
    pub fn len(&self) -> usize {
        let walks = self.walks.lock().len();
        let lists = self.lists.lock().len();
        walks.max(lists).max(self.built.pin().len())
    }

    /// Nothing kept of any program: what [`Fresh::Restart`] says of a walk over it.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many functions the kept walks hold, each walk's own: what a [`Kept`] keeps of the
    /// front end's describes, and nothing beside them.
    pub fn functions(&self) -> usize {
        self.walks.lock().values().map(Walked::functions).sum()
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

    /// Lets go of what `program`'s last build gave: its lists are kept by row from now on.
    pub(super) fn forget_built(&self, program: u64) {
        self.built.pin().remove(&program);
    }

    /// Takes the lists kept of `program`'s last `Built`, if they are: the build that patches
    /// them owns them, and keeps its own in their place when it ends well. A build that is
    /// refused keeps none, so the build after it makes its lists whole and its delta has no
    /// base.
    pub(super) fn take_lists(&self, program: u64) -> Option<Lists> {
        self.lists.lock().remove(&program)
    }

    /// Keeps `lists` as `program`'s last `Built`.
    pub(super) fn keep_lists(&self, program: u64, lists: Lists) {
        self.lists.lock().insert(program, lists);
    }

    /// Gives back `built`, a `Built` a build over this gave, once its reader is done with
    /// it: the next build of its program writes the rows it changed into these lists in
    /// place of making every list again from its rows, so what an edit costs here is the
    /// edit's rows and never the program's ([`Work::copied`]). Kept only where it is the
    /// last `Built` of a program kept by row (its ID is the rows'), and let go otherwise:
    /// one given late, after its program was built again, is of no use to anyone. A reader
    /// that never gives one back costs the next build a copy a row, and nothing else: which
    /// `Built` the next build gives does not depend on it.
    pub fn give_back(&self, built: super::Built) {
        let unused = {
            let mut lists = self.lists.lock();
            match lists.values_mut().find(|it| it.gave(&built)) {
                Some(of) => {
                    of.take_back(built);
                    None
                }
                None => Some(built),
            }
        };
        // Freed outside the lock.
        drop(unused);
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
    /// Describes the front end was asked for: every function of a whole walk no record
    /// holds, and of a patched walk the functions new to it and those the front end said
    /// changed. Over every round.
    pub described: usize,
    /// Functions of a patched walk the front end said still hold: each cost its answer, never
    /// a describe. 0 for a whole walk, which takes no describe from memory.
    pub verified: usize,
    /// Describes read from the store's records ([`super::records`]): each cost a lookup and
    /// its record's decode, never a describe. Over every round.
    pub found: usize,
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

/// What a build's walk and the passes after it did, counted: each a bound on the work of
/// one pass, so a test holds each to the edit and a span records each
/// (docs/v2/plan/design/incremental-walk.md, "Cost"). All 0 for a build whose walk ran
/// whole, which counts nothing: its work is the program's.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Work {
    /// Functions and statics the front end was asked of again: those asked each build and
    /// those the front end named that the walk holds, each once; every one the walk holds
    /// when the front end named no changed set.
    pub asked: usize,
    /// Functions, counted instances and statics the front end named as changed
    /// ([`Frontend::changed_since`]): 0 when it named no set. A regression of the front
    /// end's changed set shows here: growing with the program where the edit did not.
    ///
    /// [`Frontend::changed_since`]: crate::backend::Frontend::changed_since
    pub named: usize,
    /// Functions and statics of the walk that those names are of, each once.
    pub held: usize,
    /// Functions and statics asked each build ([`Frontend::each_build`]).
    ///
    /// [`Frontend::each_build`]: crate::backend::Frontend::each_build
    pub each_build: usize,
    /// Describes taken anew: of kept functions and statics that changed, and of new ones.
    pub described: usize,
    /// Items the reachability graph looked at: entered, dropped or brought back.
    pub graph: usize,
    /// Claims of the recursion limit's check: 0 where no body has more instances than the
    /// least limit admits, else every item of the walk.
    pub claims: usize,
    /// Nodes whose slot was decided again.
    pub classified: usize,
    /// Nodes and statics whose places were made again.
    pub resolved: usize,
    /// Nodes whose built-in closure was made again.
    pub built_in: usize,
    /// Nodes and statics the owners' check looked at again.
    pub owners: usize,
    /// Store lookups: one a function or static made again.
    pub lookups: usize,
    /// Functions whose version IDs were made.
    pub versions: usize,
    /// Functions dedupe hashed: those made again, or every one when a class changed.
    pub deduped: usize,
    /// Nodes of [`super::Reach`] whose edges were looked up again.
    pub reach: usize,
    /// Rows of the kept lists replaced, added or removed.
    pub replaced: usize,
    /// Bytes hashed: the code of the functions made again and the paths of the keyed sets.
    pub hashed: u64,
    /// Rows written into the `Built`'s contiguous lists: those the build changed, where the
    /// last `Built` was given back to be written into ([`Kept::give_back`]); every one,
    /// where it was not, or where it was a whole build's (once, for its template entries).
    pub copied: usize,
    /// Whether the `Built` was made by patching the lists kept of the last one.
    pub patched: bool,
}

/// A walk's counts while its describes run on all cores.
#[derive(Default)]
pub(super) struct Counts {
    pub(super) described: AtomicUsize,
    pub(super) found: AtomicUsize,
}

impl Counts {
    pub(super) fn described(&self) {
        self.described.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn found(&self) {
        self.found.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn found_so_far(&self) -> usize {
        self.found.load(Ordering::Relaxed)
    }

    pub(super) fn described_so_far(&self) -> usize {
        self.described.load(Ordering::Relaxed)
    }
}
