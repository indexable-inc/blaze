//! What each drop of a built body may drop, for values a caller picks: the move paths and
//! the maybe-init and maybe-uninit analyses drop elaboration runs ([`super::move_paths`]),
//! rooted at the locals whose type the caller's predicate picks instead of those with drop
//! glue, read at each `Drop` and `DropAndReplace` of the body as lowering built it.
//!
//! `hir-ty-linear` asks it for the locals that hold a `#[blaze::linear]` value
//! (docs/v2/plan/design/linear.md). It reads the body and changes nothing: drop elaboration
//! runs its own analysis, rooted as before, so no body's code depends on it.

use blaze_mem::{Tag, frame};

use base_db::Db;

use crate::{
    mir::{
        BasicBlockId, MirBody, MirSpan, PlaceTy, StoredPlace, TerminatorKind,
        framed::{bits_in, block_index},
    },
    next_solver::Ty,
};

use super::{
    Ctx,
    move_paths::{Effects, InitAnalysis, InitState, Lookup, MoveData, live_blocks},
};

/// A drop of a built body, with what may be held there of the paths below the place it drops.
#[derive(Debug, Clone)]
pub struct DropState<'db> {
    pub block: BasicBlockId,
    /// The place the `Drop` or `DropAndReplace` names.
    pub place: StoredPlace,
    /// The type of `place`.
    pub ty: PlaceTy<'db>,
    /// The terminator's span: where lowering says the drop is written.
    pub span: MirSpan,
    /// Where the drop goes when it does not unwind.
    pub target: BasicBlockId,
    /// A `DropAndReplace`.
    pub replace: bool,
    /// The drop is in a cleanup block: on the way out of a panic that unwinds.
    pub cleanup: bool,
    /// The move paths of `place` and every path below it, breadth first, `place`'s own first.
    /// `None` when `place` has no path of its own (it is behind a reference or a raw pointer,
    /// a part of a value whose type implements `Drop`, an element at an index only known when
    /// the body runs) or its local was not picked: nothing says what it holds, so it may hold
    /// all of its value.
    pub paths: Option<Vec<HeldPath<'db>>>,
}

/// A move path below a dropped place and what it may hold where the drop is.
#[derive(Debug, Clone)]
pub struct HeldPath<'db> {
    pub place: StoredPlace,
    pub ty: PlaceTy<'db>,
    /// The index in [`DropState::paths`] of the path just above, `None` for the dropped
    /// place's own path. A path's last projection says how it is reached from that one.
    pub parent: Option<u32>,
    /// On some way to the drop, the path holds a value: assigned last, or never moved out of.
    pub maybe_init: bool,
    /// On some way to the drop, the path holds none.
    pub maybe_uninit: bool,
}

/// The drops of `body`, a [`crate::mir::MirPhase::Built`] body, in the order of their blocks,
/// with what the paths of the locals `root` picks may hold at each. A drop in a block no way
/// leads to is left out.
pub fn drop_states<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    root: impl FnMut(Ty<'db>) -> bool,
) -> Vec<DropState<'db>> {
    frame::task(Tag::Mir, |frame| {
        let ctx = Ctx::new(db, body.owner, frame);
        let blocks = body.basic_blocks.len();
        let mut unreached = bits_in(blocks, frame);
        for &it in &body.unreached {
            unreached.insert(block_index(it));
        }
        let live = live_blocks(body, &unreached, frame);
        let data = MoveData::gather_rooted(&ctx, body, &live, frame, root);
        let mut effects = Effects::new_in(blocks, frame);
        let none = bits_in(blocks, frame);
        let analysis =
            InitAnalysis::run(&ctx, &data, body, &live, &unreached, &none, &mut effects, frame);
        let mut state = InitState::bottom(data.paths.len(), frame);
        let mut found = Vec::new(); // alloc: the answer, owned by the caller
        for (id, block) in body.basic_blocks.iter() {
            let Some(terminator) = &block.terminator else { continue };
            let (place, target, replace) = match &terminator.kind {
                TerminatorKind::Drop { place, target, .. } => (place, *target, false),
                TerminatorKind::DropAndReplace { place, target, .. } => (place, *target, true),
                _ => continue,
            };
            let Some(entry) = &analysis.entry[block_index(id)] else { continue };
            state.copy_from(entry);
            for at in 0..block.statements.len() {
                for effect in effects.at(&ctx, &data, body, id, at) {
                    state.apply(data.subtree(effect.path), effect.change);
                }
            }
            let paths = match data.lookup(&ctx, place) {
                Lookup::Exact(path) => {
                    let subtree = data.subtree(path);
                    let mut held = Vec::with_capacity(subtree.len()); // alloc: the answer's
                    for &it in subtree {
                        let parent = (it != path).then(|| {
                            let parent = subtree
                                .iter()
                                .position(|&above| {
                                    data.paths[above].children.iter().any(|&(_, c)| c == it)
                                })
                                .expect(
                                    "a path below the dropped one has its parent in the subtree",
                                );
                            parent as u32
                        });
                        let (maybe_init, maybe_uninit) = state.of(it);
                        held.push(HeldPath {
                            place: data.paths[it].place.clone(), // clone: an interned place
                            ty: data.paths[it].ty,
                            parent,
                            maybe_init,
                            maybe_uninit,
                        });
                    }
                    Some(held)
                }
                Lookup::Untracked => None,
            };
            found.push(DropState {
                block: id,
                place: place.clone(), // clone: an interned place
                ty: place.as_ref().ty(body, &ctx.infcx, ctx.env),
                span: terminator.span,
                target,
                replace,
                cleanup: block.is_cleanup,
                paths,
            });
        }
        found
    })
}
