//! blaze: which impls a goal may use, for the impls core writes once over the markers of
//! `#![blaze::arbitrary_ints]` (docs/v2/plan/design/arbitrary-ints.md).
//!
//! `impl<T: ArbitraryInt> Add for T`, `impl<T: ArbitraryInt> Add<T> for &T` and
//! `impl<S, D: IntFrom<S>> From<S> for D` are blanket impls (or impls of a reference) to the
//! impl index, so the solver would list them for every goal of their trait, `String: Display`
//! and `u8: Add` among them, and refuse each with the marker's builtin. Instead
//! [`DbInterner::for_each_relevant_impl`] skips an impl whose marker-bounded parameters stand
//! only where the goal has types that can't be an arbitrary width: a type that is no integer, a
//! standard integer or an integer variable (none of which a marker ever holds of). What the
//! solver answers is unchanged; it never sees those impls for any other type.
//!
//! [`DbInterner::for_each_relevant_impl`]: crate::next_solver::DbInterner

use base_db::Db;
use hir_def::{HasModule, ImplId, lang_item::lang_items};
use rustc_type_ir::{
    InferTy,
    inherent::{GenericArgs as _, IntoKind as _},
};

use crate::{
    db::HirDatabase,
    lower::GenericPredicates,
    next_solver::{ClauseKind, GenericArg, TraitRef, Ty, TyKind},
};

/// The trait-reference argument positions an impl's marker bounds tie to arbitrary widths, a
/// bit per position of the first eight: each mask is one bound, which holds only if one of its
/// positions is an arbitrary width (`T: ArbitraryInt` has `T`'s positions, `D: IntFrom<S>`
/// those of `D` and of `S`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct IntGates {
    masks: [u8; 4],
    len: u8,
}

impl IntGates {
    /// No bound to check: the impl may apply to any goal.
    pub const NONE: IntGates = IntGates { masks: [0; 4], len: 0 };

    /// Whether every bound has a position among `maybe`, those the goal's arguments may be an
    /// arbitrary width at.
    pub fn admits(self, maybe: u8) -> bool {
        self.masks[..usize::from(self.len)].iter().all(|&mask| mask & maybe != 0)
    }

    fn push(&mut self, mask: u8) {
        // A bound past the fourth is not checked, so the impl is listed as without it.
        if let Some(slot) = self.masks.get_mut(usize::from(self.len)) {
            *slot = mask;
            self.len += 1;
        }
    }
}

/// [`IntGates`] of `impl_`: none unless a parameter bounded by `ArbitraryInt`,
/// `ArbitrarySigned`, `IntFrom` or `IntTryFrom` stands as one of the first eight arguments of
/// the impl's trait reference (through references). A bound whose parameter also stands past
/// the eighth, or nowhere, is not checked.
#[blaze_query::tracked(cycle_result = int_gates_cycle, returns(copy), mem_tag = Infer)]
pub fn int_gates(db: &Db, impl_: ImplId) -> IntGates {
    let Some(trait_ref) = db.impl_trait(impl_) else { return IntGates::NONE };
    let trait_ref = trait_ref.skip_binder();
    let stands_as_param = |arg: GenericArg<'_>| {
        arg.ty().is_some_and(|ty| matches!(peel_refs(ty).kind(), TyKind::Param(_)))
    };
    if !trait_ref.args.iter().any(stands_as_param) {
        return IntGates::NONE;
    }
    // The positions the parameter `param` stands at, if all are among the first eight.
    let positions = |param: Ty<'_>| -> Option<u8> {
        let TyKind::Param(param) = param.kind() else { return None };
        let mut mask = 0u8;
        for (at, arg) in trait_ref.args.iter().enumerate() {
            let Some(ty) = arg.ty() else { continue };
            if matches!(peel_refs(ty).kind(), TyKind::Param(it) if it.index == param.index) {
                if at >= 8 {
                    return None;
                }
                mask |= 1 << at;
            }
        }
        (mask != 0).then_some(mask)
    };
    let lang = lang_items(db, impl_.krate(db));
    let unary = [lang.ArbitraryInt, lang.ArbitrarySigned];
    let binary = [lang.IntFrom, lang.IntTryFrom];
    let mut gates = IntGates::NONE;
    for clause in GenericPredicates::query_explicit(db, impl_.into()).skip_binder() {
        let ClauseKind::Trait(predicate) = clause.kind().skip_binder() else { continue };
        let trait_ = Some(predicate.trait_ref.def_id.0);
        let mask = if unary.contains(&trait_) {
            positions(predicate.self_ty())
        } else if binary.contains(&trait_) {
            // `D: IntFrom<S>` holds when either side is an arbitrary width.
            match (positions(predicate.self_ty()), positions(predicate.trait_ref.args.type_at(1))) {
                (Some(target), Some(source)) => Some(target | source),
                _ => None,
            }
        } else {
            continue;
        };
        if let Some(mask) = mask {
            gates.push(mask);
        }
    }
    gates
}

fn int_gates_cycle(_db: &Db, _: blaze_query::db::Id, _impl: ImplId) -> IntGates {
    // A cycle through the impl's own predicates: list it, as the solver did before.
    IntGates::NONE
}

/// What a goal's trait reference is to [`IntGates`]: the positions of its first eight
/// arguments that may be an arbitrary width.
#[derive(Debug, Clone, Copy)]
pub struct GoalArgs {
    maybe: u8,
    /// Every type among the first eight may be one, so no impl is skipped.
    all: bool,
}

impl GoalArgs {
    pub fn of(trait_ref: TraitRef<'_>) -> GoalArgs {
        let (mut maybe, mut types) = (0u8, 0u8);
        for (at, arg) in trait_ref.args.iter().take(8).enumerate() {
            let Some(ty) = arg.ty() else { continue };
            types |= 1 << at;
            if may_be_arbitrary(ty) {
                maybe |= 1 << at;
            }
        }
        GoalArgs { maybe, all: maybe == types }
    }

    /// Whether the solver is to consider `impl_` for the goal.
    pub fn admits(self, db: &Db, impl_: ImplId) -> bool {
        self.all || int_gates(db, impl_).admits(self.maybe)
    }
}

/// Whether `ty`, its references peeled, is or may become an integer of an arbitrary width:
/// one, or a type not yet known (a parameter, a placeholder, an alias, a type variable).
fn may_be_arbitrary(ty: Ty<'_>) -> bool {
    match peel_refs(ty).kind() {
        TyKind::Int(int) => int.arbitrary_width().is_some(),
        TyKind::Uint(uint) => uint.arbitrary_width().is_some(),
        TyKind::Param(_)
        | TyKind::Placeholder(_)
        | TyKind::Alias(..)
        | TyKind::Bound(..)
        | TyKind::Infer(InferTy::TyVar(_))
        | TyKind::Error(_) => true,
        _ => false,
    }
}

fn peel_refs(mut ty: Ty<'_>) -> Ty<'_> {
    while let TyKind::Ref(_, inner, _) = ty.kind() {
        ty = inner;
    }
    ty
}
