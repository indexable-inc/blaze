//! A projection met while a constant or a static is evaluated, normalized by the checker's
//! own solver step (`blaze_typeck::normalize_projection`), in a front2 world over the records
//! as they are now: the one normalization the checker and the run path's layout use
//! (program.rs `FrontEnd::normalize`), never a second normalizer.
//!
//! Why it is needed. The Reference, "Static items": "The static initializer is a constant
//! expression evaluated at compile time"; "Constant evaluation": a value is laid out by its
//! type. A field's type is grounded by blaze-build's `ground` (every argument substituted, then
//! every projection normalized through `LayoutFacts::normalize`). The pinned core writes every
//! atomic through one: `pub struct Atomic<T: AtomicPrimitive> { v: UnsafeCell<T::Storage> }`
//! (library/core/src/sync/atomic.rs), so `ArcInner<[u8; 1]>`'s `strong: Atomic<usize>` (alloc's
//! `static STATIC_INNER_SLICE`, library/alloc/src/rcs/arc.rs) holds
//! `<usize as AtomicPrimitive>::Storage`. Answered by the trait's default
//! (`BuildError::Projection`), every such field had "no ground form".
//!
//! The world asks over ground types only: no written path is resolved, so the scope is the
//! trait's own (its library key as the module, no parameters, no `Self`, the synthetic
//! spelling [`Spelling::fixed`]). What the solver needs and the store has not made (the impls
//! of the trait at the type's shape, a header) is noted as the check loop's fills and made by
//! the caller before it asks again ([`answer`]), exactly as a body's check makes them
//! (check.rs `make_all`). A projection the solver does not answer stays refused, by a typed
//! [`ProjectionRefused`] naming the trait, the associated type, `Self` and the solver's error.

use core::cell::{Cell, RefCell};
use std::alloc::Global;
use std::vec::Vec;

use blaze_build::BuildError;
use blaze_own::Alloc;
use blaze_typeck as tc;
use blaze_typeck::{ItemKey, PTy, PortableTys};

use super::block_traits::BlockTraits;
use super::buckets::Buckets;
use super::check::{CheckFail, Records, make_all};
use super::consts::ConstFail;
use super::heads::Store;
use super::index::Index;
use super::keys::Keys;
use super::source::NamesSource;
use super::spell::Spelling;
use super::types::SelfTy;
use super::world_items::{BodyWorld, Fill, Gap};

/// Why a projection reached by a constant's types was not normalized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionWhy {
    /// The solver refused (its error), or gave no type (`None`).
    Solver(Option<tc::ResolveError>),
    /// The trait is not an item of a library (a trait declared in a body): no world is made
    /// for it here.
    NoLibraryTrait,
    /// The solver's answer still holds this node (a projection, a hole, a parameter, an
    /// error): no ground form, so it is not taken.
    NotGround(tc::PNode),
}

/// The projection `<self_ty as trait_>::name` that has no ground form, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectionRefused {
    pub trait_: ItemKey,
    pub name: tc::Name,
    /// `Self`'s node at the asked arguments (`None`: no arguments).
    pub self_ty: Option<tc::PNode>,
    pub why: ProjectionWhy,
}

impl core::fmt::Display for ProjectionRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let ProjectionRefused { trait_, name, self_ty, why } = self;
        write!(f, "the projection `<{self_ty:?} as {trait_:?}>::{name:?}` has no ground form: ")?;
        match why {
            ProjectionWhy::Solver(Some(e)) => write!(f, "{e}"),
            ProjectionWhy::Solver(None) => write!(f, "the solver's step gave no type"),
            ProjectionWhy::NoLibraryTrait => write!(f, "its trait is declared in a body, which this world does not read"),
            ProjectionWhy::NotGround(n) => write!(f, "the solver's answer still holds {n:?}"),
        }
    }
}

/// What one evaluation step's projections asked of the records: kept by the step's world
/// (`ConstsWorld::project`), read once after the step by [`ProjectAsk::into_asked`].
pub(super) struct ProjectAsk<'w> {
    buckets: Option<&'w Buckets>,
    fills: RefCell<Vec<Fill, Global>>,
    gaps: RefCell<Vec<Gap, Global>>,
    refused: Cell<Option<ProjectionRefused>>,
}

/// What a step's projections left for the caller, in the order it is handled: records to
/// make, then questions this front end cannot answer, then the solver's refusal.
pub(super) enum Asked {
    Nothing,
    Fills(Vec<Fill, Global>),
    Gaps(Vec<Gap, Global>),
    Refused(ProjectionRefused),
}

impl<'w> ProjectAsk<'w> {
    /// Nothing asked yet; `buckets`: the records' impl buckets (`Records::buckets`), `None`
    /// while none is made (every impls question is then a fill).
    pub(super) fn new(buckets: Option<&'w Buckets>) -> ProjectAsk<'w> {
        ProjectAsk { buckets, fills: RefCell::new(Vec::new_in(Global)), gaps: RefCell::new(Vec::new_in(Global)), refused: Cell::new(None) }
    }

    /// The records' impl buckets this step's world reads (const_select.rs `select_once`, a
    /// trait's constant selected inside the step, reads the same).
    pub(super) fn buckets(&self) -> Option<&'w Buckets> {
        self.buckets
    }

    /// The fills and gaps a selection inside the step noted (consts.rs `ConstsWorld::resolve`
    /// through const_select.rs): kept with the projections' own, made or refused by [`answer`].
    pub(super) fn note(&self, fills: Vec<Fill, Global>, gaps: Vec<Gap, Global>) {
        self.fills.borrow_mut().extend(fills);
        self.gaps.borrow_mut().extend(gaps);
    }

    /// `<args[0] as trait_<args[1..]>>::name` at ground `args` (types of `ground`), normalized
    /// by the checker's solver over the store; the answer is interned in `ground`. Not
    /// answered: the fills and gaps the world noted, and the refusal, are kept for the caller,
    /// and the build's own refusal is returned (`BuildError::Projection`, naming it).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn normalize<'n, S: NamesSource<'n>>(
        &self,
        names: S,
        keys: &Keys,
        index: &Index<'n>,
        store: &Store<'n>,
        trait_: ItemKey,
        name: tc::Name,
        args: &[PTy],
        ground: &mut PortableTys<'_>,
    ) -> Result<PTy, BuildError> {
        let self_ty = args.first().and_then(|&a| ground.view().node(a).ok());
        let refuse = |why: ProjectionWhy| {
            self.refused.set(Some(ProjectionRefused { trait_, name, self_ty, why }));
            Err(BuildError::Projection { trait_, name })
        };
        let Some(module) = keys.library_in(names, trait_) else { return refuse(ProjectionWhy::NoLibraryTrait) };
        let spell = Spelling::fixed();
        let alloc = store.tys.alloc();
        let blocks = BlockTraits::module(alloc);
        let self_ = SelfTy::None;
        let world = BodyWorld {
            names,
            keys,
            index,
            store,
            buckets: self.buckets,
            // No path is resolved in a projection's step: the scope is the trait's own.
            owner: trait_,
            module,
            params: &[],
            self_: &self_,
            spell: &spell,
            traits: &[],
            blocks: &blocks,
            path_marks: &[],
            path_ns: &[],
            fills: RefCell::new(Vec::new_in(Global)),
            gaps: RefCell::new(Vec::new_in(Global)),
            // A constant's evaluation is after analysis (rustc's CTFE runs post-analysis):
            // every opaque reached is its hidden type.
            reveal: super::opaque_auto::Reveal::All,
        };
        let used_before = store.provisional_used.borrow().len();
        let out = tc::normalize_projection(&world, trait_, name, args, ground, alloc);
        let pending_hit = {
            let mut used = store.provisional_used.borrow_mut();
            let hit = used.get(used_before..).is_some_and(|slice| {
                slice.iter().any(|&ck| {
                    store.provisional_impls.get(&ck).is_some_and(|p| {
                        p.head.trait_ == Some(trait_) && p.pending_assoc.iter().any(|&n| n == name)
                    })
                })
            });
            used.truncate(used_before);
            hit
        };
        if pending_hit {
            return refuse(ProjectionWhy::Solver(None));
        }
        let fills = world.fills.into_inner();
        let gaps = world.gaps.into_inner();
        // Taken only when the world noted nothing to make or answer AND the answer is ground: while
        // a question is unanswered the solver leaves the goal pending and the term exports as
        // the projection itself, which `normalize_ground` would ask again until its round bound.
        let unground = match out {
            Ok(Some(t)) => unground_node(ground, t),
            _ => None,
        };
        match out {
            Ok(Some(t)) if fills.is_empty() && gaps.is_empty() && unground.is_none() => Ok(t),
            out => {
                self.fills.borrow_mut().extend(fills);
                self.gaps.borrow_mut().extend(gaps);
                refuse(match (out, unground) {
                    (Err(e), _) => ProjectionWhy::Solver(Some(e)),
                    (Ok(_), Some(n)) => ProjectionWhy::NotGround(n),
                    (Ok(_), None) => ProjectionWhy::Solver(None),
                })
            }
        }
    }

    /// What the step asked: fills first (made, the step is asked again), then gaps, then the
    /// last refusal.
    pub(super) fn into_asked(self) -> Asked {
        let fills = self.fills.into_inner();
        let gaps = self.gaps.into_inner();
        match (fills.is_empty(), gaps.is_empty(), self.refused.get()) {
            (false, _, _) => Asked::Fills(fills),
            (true, false, _) => Asked::Gaps(gaps),
            (true, true, Some(p)) => Asked::Refused(p),
            (true, true, None) => Asked::Nothing,
        }
    }
}

/// The first node of `t` (in `ground`) with no ground form: a projection, an opaque type, a
/// parameter, a hole or an error; `None` when `t` is ground. A work list over the node's
/// children (blaze-build `instance::each_child`, the one child order), no recursion; a type
/// past the table is reported as `Error`.
fn unground_node(ground: &PortableTys<'_>, t: PTy) -> Option<tc::PNode> {
    let view = ground.view();
    // blaze-own's `Vec`: the type `each_child` fills.
    let mut work: blaze_own::Vec<PTy, Alloc<'_>> = blaze_own::Vec::new_in(ground.alloc());
    let mut children: blaze_own::Vec<PTy, Alloc<'_>> = blaze_own::Vec::new_in(ground.alloc());
    work.push(t);
    while let Some(at) = work.pop() {
        let Ok(node) = view.node(at) else { return Some(tc::PNode::Error) };
        if matches!(node, tc::PNode::Alias { .. } | tc::PNode::Opaque(..) | tc::PNode::Param(_) | tc::PNode::ConstParam(_) | tc::PNode::Infer | tc::PNode::Error) {
            return Some(node);
        }
        children.clear();
        if blaze_build::instance::each_child(view, node, &mut children).is_err() {
            return Some(tc::PNode::Error);
        }
        work.extend_from_slice(&children);
    }
    None
}

/// The caller's answer to what a refused step `item` asked: `None` when its projections asked
/// nothing (the step's own refusal stands); `Some(Ok(()))` when the fills were made and the
/// same need is to be evaluated again (each round counts against the caller's step bound);
/// else the constant's typed refusal.
pub(super) fn answer<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, item: ItemKey, asked: Asked) -> Option<Result<(), ConstFail>> {
    match asked {
        Asked::Nothing => None,
        Asked::Fills(fills) => Some(make_all(r, fills, item).map_err(|error| ConstFail::Check { item, error })),
        Asked::Gaps(gaps) => Some(Err(ConstFail::Check { item, error: CheckFail::Gap { owner: item, gaps } })),
        Asked::Refused(refused) => Some(Err(ConstFail::Projection { item, refused })),
    }
}
