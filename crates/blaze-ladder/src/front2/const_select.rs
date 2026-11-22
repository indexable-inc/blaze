//! A trait's associated constant at ground arguments, selected by the checker's own solver:
//! `blaze_typeck::resolve_instance`, the one home of impl selection (the same call a body's
//! check makes for a trait's method, check.rs `callee_target`, and the run path's instances,
//! program.rs). No second matcher: every impl of the trait in the self type's bucket and the
//! blanket bucket is instantiated with fresh variables, its trait reference related to the
//! goal's and its where-clauses proved (blaze-typeck instance.rs `select_in`/`try_impl`), so a
//! generic impl (`impl<T: U> U for S<T>`, typenum's `impl<U: Unsigned, B: Bit> Unsigned for
//! UInt<U, B>`) is selected exactly as a body's goal selects it.
//!
//! The rule. The Reference, "Associated Items", "Associated Constants": "Associated constants
//! are constants associated with a type", and "An associated constant definition defines a
//! constant associated with a type"; its example "Using default values" (`trait
//! ConstantIdDefault { const ID: i32 = 1; }` with `impl ConstantIdDefault for Struct {}` gives
//! `assert_eq!(1, <Struct as ConstantIdDefault>::ID)`). So `<T as Tr>::C` is the definition in
//! the impl of `Tr` that holds for `T`, at that impl's arguments, or, where that impl writes
//! none, the trait's default at the asked arguments. By coherence (the Reference, "Trait
//! implementation coherence": overlapping implementation instances are an error) at most one
//! impl holds; two is the solver's typed `Overlap`.
//!
//! What the solver asks and the records have not made (the impls of the trait at the type's
//! shape, a header, the trait's members) is noted by the world as fills: [`select_made`] makes
//! them and asks again, at most `MOST_ROUNDS` times; inside an evaluation step, where the
//! records are not mutable, [`select_once`] hands them to the caller, which makes them and
//! evaluates the step again (consts.rs `value_key`, through const_project.rs `answer`). A
//! question this front end cannot answer (a gap), no impl, or any solver error is a typed
//! refusal naming the constant, the trait and `Self`: never a guessed impl, never the trait's
//! default taken in place of an impl that was not found.

use core::cell::RefCell;
use std::alloc::Global;
use std::vec::Vec;

use blaze_own::Alloc;
use blaze_typeck as tc;
use blaze_typeck::{ItemKey, List, PortableTys};

use super::block_traits::BlockTraits;
use super::buckets::Buckets;
use super::check::{CheckFail, MOST_ROUNDS, Records, make_all};
use super::consts::ConstFail;
use super::heads::Store;
use super::index::Index;
use super::keys::Keys;
use super::source::NamesSource;
use super::spell::Spelling;
use super::types::SelfTy;
use super::world_items::{BodyWorld, Fill, Gap};

/// Why a trait's constant was not selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectWhy {
    /// No impl of the trait holds for `Self` at these arguments (the solver's `NoImpl`).
    NoImpl,
    /// The solver refused with this error (an overlap, a question the world has no answer
    /// to, an argument not ground, a malformed record).
    Solver(tc::ResolveError),
    /// The trait is declared in a body: no world of its scope is made here.
    NoLibraryTrait,
}

/// The trait's constant or fn `constant` of `trait_` (consts.rs `ConstsWorld::resolve`), asked
/// at `Self` = `self_ty` (`None`: the arguments hold no `Self`), and why no definition was
/// selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConstSelectRefused {
    pub constant: ItemKey,
    pub trait_: ItemKey,
    pub self_ty: Option<tc::PNode>,
    pub why: SelectWhy,
}

impl core::fmt::Display for ConstSelectRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let ConstSelectRefused { constant, trait_, self_ty, why } = self;
        write!(f, "the associated item {constant:?} of trait {trait_:?} at `Self` = {self_ty:?} selects no definition (rule: the impl the solver selects for the trait reference, or the trait's default where that impl writes none): ")?;
        match why {
            SelectWhy::NoImpl => write!(f, "no impl of the trait holds for that type"),
            SelectWhy::Solver(e) => write!(f, "{e}"),
            SelectWhy::NoLibraryTrait => write!(f, "its trait is declared in a body, which this world does not read"),
        }
    }
}

impl ConstSelectRefused {
    /// The evaluator's refusal: no impl at a known `Self` is [`ConstFail::NoImplConst`] (the
    /// census's name for it); no `Self` at all is [`ConstFail::TraitConstNoSelf`]; every
    /// other refusal is [`ConstFail::SelectConst`], carrying this whole.
    pub(super) fn into_fail(self) -> ConstFail {
        match (self.why, self.self_ty) {
            (SelectWhy::NoImpl, Some(self_ty)) => ConstFail::NoImplConst { trait_: self.trait_, self_ty, constant: self.constant },
            (SelectWhy::NoImpl, None) | (SelectWhy::Solver(tc::ResolveError::NotGround { .. }), None) => {
                ConstFail::TraitConstNoSelf { item: self.constant, constant: self.constant }
            }
            _ => ConstFail::SelectConst { refused: self },
        }
    }
}

/// One ask of the solver ([`select_once`]).
pub(super) enum Selection {
    /// The definition the trait reference selects, and its arguments (in the caller's
    /// `ground`): the impl's constant at the impl's arguments, or the trait's own constant
    /// (its default) at the asked arguments.
    Selected { item: ItemKey, args: List },
    /// The world noted records to make (`fills`) or questions it cannot answer (`gaps`): the
    /// answer is not taken; the caller makes the fills and asks again, or refuses the gaps.
    Asked { fills: Vec<Fill, Global>, gaps: Vec<Gap, Global> },
    /// The solver selected nothing, by name.
    Refused(ConstSelectRefused),
}

/// The trait's constant `constant` (declared by `trait_`) at the ground `args` (`[Self, the
/// trait's arguments..]`, types of `ground`), selected once by `blaze_typeck::resolve_instance`
/// in a world over the records as they are now. The world's scope is the trait's own (its
/// library key as the module, no parameters, no `Self`, the fixed spelling), as
/// const_project.rs `ProjectAsk::normalize` makes it: the arguments are ground, so no written
/// path is resolved. An answer is taken only when the world noted nothing: while a question
/// is unanswered the solver's selection is over the impls made so far and may differ from
/// the one the whole set gives.
#[allow(clippy::too_many_arguments)]
pub(super) fn select_once<'n, S: NamesSource<'n>>(
    names: S,
    keys: &Keys,
    index: &Index<'n>,
    store: &Store<'n>,
    buckets: Option<&Buckets>,
    constant: ItemKey,
    trait_: ItemKey,
    args: List,
    ground: &mut PortableTys<'_>,
    alloc: Alloc<'_>,
) -> Selection {
    let self_ty = {
        let g = ground.view();
        g.list(args).ok().and_then(|l| l.first().copied()).and_then(|t| g.node(t).ok())
    };
    let refuse = |why: SelectWhy| Selection::Refused(ConstSelectRefused { constant, trait_, self_ty, why });
    let Some(module) = keys.library_in(names, trait_) else { return refuse(SelectWhy::NoLibraryTrait) };
    let spell = Spelling::fixed();
    // An empty block list (no block's `use` is in a trait's own scope); `BlockTraits::module`
    // takes an `Alloc<'static>`, which the caller's `alloc` is not: the store's is.
    let blocks = BlockTraits::module(store.tys.alloc());
    let self_ = SelfTy::None;
    let world = BodyWorld {
        names,
        keys,
        index,
        store,
        buckets,
        // No path is resolved in a selection at ground arguments: the scope is the trait's own.
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
        // A constant is evaluated after analysis (rustc's CTFE runs post-analysis): every
        // opaque in the arguments or the impls' where-clauses is its hidden type.
        reveal: super::opaque_auto::Reveal::All,
    };
    let out = tc::resolve_instance(&world, constant, args, ground, alloc);
    let fills = world.fills.into_inner();
    let gaps = world.gaps.into_inner();
    if !fills.is_empty() || !gaps.is_empty() {
        return Selection::Asked { fills, gaps };
    }
    match out {
        Ok(r) => Selection::Selected { item: r.item, args: r.args },
        Err(tc::ResolveError::NoImpl { .. }) => refuse(SelectWhy::NoImpl),
        Err(e) => refuse(SelectWhy::Solver(e)),
    }
}

/// [`select_once`] with the records' fill loop: the fills a selection notes are made
/// (check.rs `make_all`, the body check's own) and it is asked again, at most `MOST_ROUNDS`
/// times; a gap is the check's typed `Gap` refusal; a refusal is the evaluator's
/// ([`ConstSelectRefused::into_fail`]). A loop, not recursion.
pub(super) fn select_made<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    constant: ItemKey,
    trait_: ItemKey,
    args: List,
    ground: &mut PortableTys<'_>,
    alloc: Alloc<'_>,
) -> Result<(ItemKey, List), ConstFail> {
    // For a refusal past the last round (check.rs `rounds_fail`).
    let mut made = 0u64;
    for _ in 0..MOST_ROUNDS {
        let step = select_once(r.names, &*r.keys, r.index, &*r.store, r.buckets.as_ref(), constant, trait_, args, ground, alloc);
        match step {
            Selection::Selected { item, args } => return Ok((item, args)),
            Selection::Refused(refused) => return Err(refused.into_fail()),
            Selection::Asked { fills, gaps } if fills.is_empty() => {
                return Err(ConstFail::Check { item: constant, error: CheckFail::Gap { owner: constant, gaps } });
            }
            Selection::Asked { fills, .. } => {
                made += fills.len() as u64;
                make_all(r, fills, constant).map_err(|error| ConstFail::Check { item: constant, error })?
            }
        }
    }
    Err(ConstFail::Check { item: constant, error: super::check::rounds_fail(r, constant, "selecting a trait's constant", made, Vec::new_in(Global)) })
}
