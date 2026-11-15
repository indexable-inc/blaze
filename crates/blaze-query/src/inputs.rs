//! What a result read of its view, transitively: the files and directory listings under the
//! memo a key resolved to in a view (handoff-12 request 1). For a caller that keeps an answer
//! for as long as those leaves are equal, and so needs to know them: the daemon's plan
//! (`Jobs::planned`, which keeps `blaze_units::query::Plan`'s answer by its reads).
//!
//! Walked once per answer kept, never on a build's path: a plain walk over the recorded edges,
//! each key once.

use blaze_ids::ContentId;
use blaze_reclaim::Guard;
use blaze_vfs::ViewId;

use crate::{
    Database, Input, InputData, Pinned, Query, QueryKey, ViewState, absent_id, is_input, kinds,
    memo::MemoId,
};

/// The leaves of the view under one result: each file of the view whose bytes it read, and each
/// directory of the view whose listing it read, each with the content ID the view has for it,
/// which is the one the result saw (the result holds in that view). Sorted by path, each path
/// once.
///
/// - A file's ID is its bytes' (`blaze_vfs::FILE_DOMAIN`, as the tree names it); its mode is
///   in the edge's fingerprint, not in the ID. A directory's is the ID of its sorted names.
///   A read of a path the view has no file (or no directory) at is a leaf too, with the absent
///   ID of its kind: its appearing changes the result as much as an edit does.
/// - A read of a file or directory under another root ([`Input::FileAt`], [`Input::DirAt`]) is
///   no leaf of the view: such a root never changes, so the read is a pure function of
///   `(root, path)`, and the root itself came from something the result read of the view
///   (its configuration, or a file), which is the leaf.
/// - The view's configuration ([`Input::Config`]) is not listed: a caller keys what it keeps
///   by the configuration it opened the view with.
#[derive(PartialEq, Eq, Debug)]
pub struct Inputs {
    pub files: Vec<(Box<str>, ContentId)>,
    pub listings: Vec<(Box<str>, ContentId)>,
}

impl Pinned<'_> {
    /// The leaves under the memo that holds for `key` in `view` ([`Inputs`]); `None` when none
    /// does there: the view never resolved `key` (ask it with [`Pinned::get`] first), or the
    /// view is not open.
    ///
    /// # Panics
    ///
    /// When the walk meets an edge on a key with no memo in the view: a fact recorded with
    /// [`crate::Cx::read`] that no query answers. What such a fact stands for has no path the
    /// database knows, so the leaves under it can't be named; the panic names the fact and the
    /// query that read it.
    pub fn inputs_of<Q: Query>(&self, view: ViewId, key: &Q::Key) -> Option<Inputs> {
        let (db, task) = (self.db, &self.task);
        let state = db.view_state(task, view)?;
        let root = QueryKey { kind: Q::KIND, input: Q::input(key) };
        let memo = state.results.get(task, &root)?.memo;
        Some(db.leaves(task, state, root, memo))
    }
}

impl Database {
    /// [`Pinned::inputs_of`] of `root`, whose memo in the view is `memo`. Each key is walked
    /// once (a set of the keys met), so the walk ends after at most as many steps as the view
    /// has keys under `root`.
    fn leaves(&self, task: &Guard, state: &ViewState, root: QueryKey, memo: MemoId) -> Inputs {
        // alloc: the answer, owned by its caller: once per answer it keeps.
        let mut inputs = Inputs { files: Vec::new(), listings: Vec::new() };
        blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
            let mut met = hashbrown::HashSet::with_hasher_in(blaze_hash::FixedState, frame);
            let mut work: Vec<(QueryKey, MemoId), _> = Vec::new_in(frame);
            met.insert(root);
            work.push((root, memo));
            while let Some((by, memo)) = work.pop() {
                for edge in self.store.edges(task, memo) {
                    let read = edge.read();
                    if !met.insert(read) {
                        continue;
                    }
                    if is_input(read.kind) {
                        self.leaf(task, state, by, read, edge.saw(), &mut inputs);
                        continue;
                    }
                    let Some(resolved) = state.results.get(task, &read) else {
                        panic!(
                            "the inputs of {root:?} can't be named: {by:?} read {read:?}, a fact \
                             no query answers (recorded with `Cx::read`), so what it stands for \
                             has no path"
                        )
                    };
                    assert_eq!(
                        resolved.fingerprint(),
                        edge.saw(),
                        "{by:?} holds in view {:?}, but its edge on {read:?} saw another \
                         fingerprint than the view has",
                        state.id
                    );
                    #[cfg(test)]
                    if probe::SKIPS_VERIFIED.with(std::cell::Cell::get)
                        && state.reruns.get(task, &read).is_none()
                    {
                        // The planted bug: a dependency the view proved unchanged without
                        // running is taken to have read nothing that could change.
                        continue;
                    }
                    work.push((read, resolved.memo));
                }
            }
        });
        inputs.files.sort_unstable();
        inputs.listings.sort_unstable();
        inputs
    }

    /// Adds the input `read`, which `by` read seeing `saw`, to `inputs` when it is a leaf of the
    /// view (a file or a directory of it).
    fn leaf(
        &self,
        task: &Guard,
        state: &ViewState,
        by: QueryKey,
        read: QueryKey,
        saw: blaze_ids::Fingerprint,
        inputs: &mut Inputs,
    ) {
        if !matches!(read.kind, kinds::FILE | kinds::DIR) {
            return;
        }
        let Some(value) = state.inputs.get(task, &read) else {
            unreachable!(
                "{by:?} holds in view {:?}, but the view never read its input {read:?}",
                state.id
            )
        };
        assert_eq!(
            value.fingerprint, saw,
            "{by:?} holds in view {:?}, but its edge on input {read:?} saw another fingerprint \
             than the view has",
            state.id
        );
        let Some(input) = self.inputs.get(task, &read) else {
            unreachable!(
                "view {:?} read input {read:?}, which the database has no path of",
                state.id
            )
        };
        match (input, &value.data) {
            (Input::File(path), InputData::File(file)) => {
                let id = file.as_ref().map_or_else(|| absent_id("file"), |(id, ..)| *id);
                // alloc: a leaf's path in the answer, owned by its caller.
                inputs.files.push((Box::from(path.as_str()), id));
            }
            (Input::Dir(path), InputData::Dir { id, .. }) => {
                // alloc: a leaf's path in the answer, owned by its caller.
                inputs.listings.push((Box::from(path.as_str()), *id));
            }
            (input, _) => unreachable!(
                "input {read:?} of view {:?} is {input:?}, and its value is of another kind",
                state.id
            ),
        }
    }
}

#[cfg(test)]
pub(crate) mod probe {
    use std::cell::Cell;

    thread_local! {
        /// The planted bug: on this thread the walk leaves out every dependency the view
        /// proved unchanged (by early cutoff) instead of running, and what it read.
        pub(crate) static SKIPS_VERIFIED: Cell<bool> = const { Cell::new(false) };
    }
}
