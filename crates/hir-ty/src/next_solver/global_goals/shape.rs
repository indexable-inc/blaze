//! The shape table: auto-trait, `Copy` and `Clone` goals proved by the self type's shape, in
//! the solver's fast path, without a canonical query.
//!
//! An auto-trait goal is proved the way the solver proves it, as a walk over the same steps,
//! that answers only when every step is certain:
//!
//! - a type with relevant impls of the trait goes through them: exactly one positive,
//!   non-default impl whose self type is the type's head over distinct impl parameters, with
//!   where clauses that are auto-trait goals (walked) or sizedness goals the sizedness fast
//!   path proves. Any other set of impls (a negative one, two, one that needs unification)
//!   gives no answer, as the solver's built-in candidate is then disqualified;
//! - a type with none is its constituents, `instantiate_constituent_tys_for_auto_trait`'s:
//!   none for scalars, function types and `!`, `[u8]` for `str`, the element of pointers,
//!   references, arrays, slices and pattern types, the elements of a tuple, the argument of
//!   `PhantomData`, the fields of any other ADT. Every other type gives no answer;
//! - a goal already on the walk's stack holds: auto traits are coinductive, and every step
//!   here is an impl or constituent step of an auto trait, so every cycle is coinductive.
//!
//! An ADT's verdict is a query of the ADT with its identity arguments, per (trait, asking
//! crate), so the impls seen are the solver's. A verdict that holds needs no fact about the
//! parameters, so it holds of every instance; the fast path asks it for any ADT instance and
//! walks only the built-in types around it. One that fails over a generic ADT's identity
//! arguments may have needed only a fact about them, so the fast path then walks the
//! instance's own impls or fields, as the query would have with those arguments.
//!
//! Every answer here is one the solver reaches through the search graph, so it costs the
//! search-graph depth the solver's evaluation would: one level per goal of the walk, a cycle
//! hit included, and none for a sizedness goal the sizedness fast path proves. The walk keeps
//! its depth (the verdict's included), answers only when it fits in the depth the goal is
//! evaluated with, and returns it, for the solver to charge to the parent goal as a global
//! cache hit of that depth would be. A cycle the solver closes through a goal further up its
//! stack, or a repeated goal its provisional cache answers, costs it less than the walk counts:
//! the walk then declines where the solver would still fit, and charges more, never less.
//!
//! `Copy` and `Clone` of an ADT without generic parameters hold when it has exactly one
//! positive, non-default, non-generic impl of the trait with no where clause: the solver's one
//! candidate, which adds no goal, so it costs the goal's own level.
//!
//! The fast path answers only for self types without inference variables, placeholders,
//! regions, parameters or errors, outside coherence and reflection (which takes no built-in
//! candidate). A where clause of the param env can then shadow a step of the walk only when it
//! is an auto-trait bound whose only local names are regions (`Foo<'a>: Send` against
//! `Foo<'static>`), which would add a region constraint; with
//! such a clause in the env no auto-trait goal is answered.

use base_db::{Crate, Db};
use hir_def::{AdtId, TraitId};
use rustc_type_ir::{
    ClausePolarity, ImplPolarity, Interner, TypeFlags, TypeVisitableExt, TypingMode,
    inherent::{AdtDef as _, GenericArgs as _, GenericsOf as _, IntoKind, Ty as _},
    lang_items::SolverTraitLangItem,
};

use super::switch;
use crate::next_solver::{
    AnyImplId, ClauseKind, ConstKind, DbInterner, GenericArg, GenericArgKind, GenericArgs, Goal,
    ParamEnv, Predicate, PredicateKind, RegionKind, SolverContext, TraitIdWrapper, TraitRef, Ty,
    TyKind, util::sizedness_fast_path,
};

/// Walk stack bound: a type whose proof is deeper is left to the solver.
const DEPTH: usize = 24;
/// Walk size bound: a type whose proof is larger is left to the solver.
const NODES: u32 = 512;
/// The most generic parameters of an impl the walk goes through.
const IMPL_PARAMS: usize = 8;

/// Self types the shape table answers for.
const NOT_SHAPE: TypeFlags = TypeFlags::HAS_INFER
    .union(TypeFlags::HAS_PLACEHOLDER)
    .union(TypeFlags::HAS_FREE_REGIONS)
    .union(TypeFlags::HAS_PARAM)
    .union(TypeFlags::HAS_ERROR);

/// Local names a where clause can have and still never unify with a goal of a type with none.
const NAMES_BUT_REGIONS: TypeFlags = TypeFlags::HAS_TY_PARAM
    .union(TypeFlags::HAS_CT_PARAM)
    .union(TypeFlags::HAS_TY_PLACEHOLDER)
    .union(TypeFlags::HAS_CT_PLACEHOLDER)
    .union(TypeFlags::HAS_TY_INFER)
    .union(TypeFlags::HAS_CT_INFER);

#[inline]
fn answers_for<'db>(cx: &SolverContext<'db>, self_ty: Ty<'db>) -> bool {
    !switch::off()
        && !self_ty.has_type_flags(NOT_SHAPE)
        && !self_ty.has_escaping_bound_vars()
        && !matches!(cx.0.typing_mode_raw(), TypingMode::Coherence | TypingMode::Reflection)
}

/// The search-graph depth below `goal` its proof costs, when the shape table proves it within
/// `available`, the depth the solver would evaluate `goal` with.
#[inline]
pub(crate) fn holds_within<'db>(
    cx: &SolverContext<'db>,
    goal: Goal<'db, Predicate<'db>>,
    available: usize,
) -> Option<usize> {
    let PredicateKind::Clause(ClauseKind::Trait(pred)) = goal.predicate.kind().skip_binder()
    else {
        return None;
    };
    if pred.polarity != ClausePolarity::Positive {
        return None;
    }
    match cx.0.interner.as_trait_lang_item(pred.def_id()) {
        Some(SolverTraitLangItem::Copy | SolverTraitLangItem::Clone) => {
            let self_ty = cx.deeply_resolve_ignoring_regions(pred.self_ty());
            copy_clone_holds(cx, pred.def_id(), self_ty).then_some(0)
        }
        _ => auto_trait_holds(
            cx,
            pred.def_id(),
            cx.shallow_resolve(pred.self_ty()),
            goal.param_env,
            available,
        ),
    }
}

/// Whether `self_ty: Copy` or `self_ty: Clone` (`trait_`) holds by its one impl.
#[inline]
pub(crate) fn copy_clone_holds<'db>(
    cx: &SolverContext<'db>,
    trait_: TraitIdWrapper,
    self_ty: Ty<'db>,
) -> bool {
    if !answers_for(cx, self_ty) {
        return false;
    }
    let TyKind::Adt(def, args) = self_ty.kind() else { return false };
    let interner = cx.0.interner;
    args.is_empty()
        && copy_clone_impl(interner.db, def.def_id(), trait_.0, interner.expect_crate())
}

/// The depth below the goal `self_ty: trait_` its proof by shape costs, when that is at most
/// `available`. `trait_` need not be an auto trait: for any other this is `None`.
#[inline]
pub(crate) fn auto_trait_holds<'db>(
    cx: &SolverContext<'db>,
    trait_: TraitIdWrapper,
    self_ty: Ty<'db>,
    param_env: ParamEnv<'db>,
    available: usize,
) -> Option<usize> {
    if !answers_for(cx, self_ty) {
        return None;
    }
    let interner = cx.0.interner;
    if !interner.trait_is_auto(trait_) || env_may_shadow(interner, param_env) {
        return None;
    }
    let mut walk = Walk::new(interner, trait_, self_ty, Inline::No, available.saturating_add(1));
    walk.holds(trait_, self_ty).then(|| walk.deepest - 1)
}

/// Whether a where clause could prove a step of the walk with a region constraint.
pub(super) fn env_may_shadow<'db>(interner: DbInterner<'db>, param_env: ParamEnv<'db>) -> bool {
    param_env.clauses.iter().any(|clause| {
        clause.has_type_flags(TypeFlags::HAS_FREE_LOCAL_REGIONS)
            && !clause.has_type_flags(NAMES_BUT_REGIONS)
            && match clause.kind().skip_binder() {
                ClauseKind::Trait(pred) => interner.trait_is_auto(pred.def_id()),
                _ => false,
            }
    })
}

/// The levels of the walk that proves `Adt<identity args>: trait_` with `krate`'s impls, the
/// goal's own included, or 0 when the walk does not prove it.
#[blaze_query::tracked(returns(copy), cycle_result = auto_trait_depth_cycle)]
pub(crate) fn auto_trait_depth(db: &Db, adt: AdtId, trait_: TraitId, krate: Crate) -> u8 {
    let interner = DbInterner::new_with(db, krate);
    let trait_ = TraitIdWrapper::from(trait_);
    let args = GenericArgs::identity_for_item(interner, adt.into());
    let ty = Ty::new_adt(interner, adt, args);
    let mut walk = Walk::new(interner, trait_, ty, Inline::Yes, DEPTH + 1);
    if walk.holds(trait_, ty) { walk.deepest as u8 } else { 0 }
}

/// A verdict that reaches itself through a query the walk reads (none does today) gives no
/// answer, so the solver decides.
fn auto_trait_depth_cycle(_: &Db, _: blaze_query::db::Id, _: AdtId, _: TraitId, _: Crate) -> u8 {
    0
}

/// Whether the ADT, which has no generic parameters, has exactly one positive, non-default,
/// non-generic impl of `trait_` with no where clause, among `krate`'s.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn copy_clone_impl(db: &Db, adt: AdtId, trait_: TraitId, krate: Crate) -> bool {
    let interner = DbInterner::new_with(db, krate);
    let ty = Ty::new_adt(interner, adt, GenericArgs::empty());
    let Impls::One(impl_) = impls(interner, trait_.into(), ty) else { return false };
    interner.impl_polarity(impl_) == ImplPolarity::Positive
        && !interner.impl_is_default(impl_)
        && interner.generics_of(impl_.into()).count() == 0
        && interner.impl_trait_ref(impl_).skip_binder().self_ty() == ty
        && interner.clauses_of(impl_.into()).skip_binder().into_iter().next().is_none()
}

enum Impls {
    None,
    One(AnyImplId),
    Many,
}

fn impls<'db>(interner: DbInterner<'db>, trait_: TraitIdWrapper, ty: Ty<'db>) -> Impls {
    let mut found = Impls::None;
    interner.for_each_relevant_impl(TraitRef::new(interner, trait_.into(), [ty]), |impl_| {
        found = match found {
            Impls::None => Impls::One(impl_),
            Impls::One(_) | Impls::Many => Impls::Many,
        }
    });
    found
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Inline {
    /// Walk an ADT's fields: inside its own verdict query.
    Yes,
    /// Ask an ADT's verdict query: in the fast path.
    No,
}

/// A walk's goal at stack length `len` is at level `len + 1`: the root goal is level 1, and the
/// solver evaluates a goal at level `l` with `l - 1` levels fewer available than the root.
struct Walk<'db> {
    interner: DbInterner<'db>,
    inline: Inline,
    stack: [(TraitIdWrapper, Ty<'db>); DEPTH],
    len: usize,
    /// The deepest level the walk reached.
    deepest: usize,
    /// The deepest level the walk may reach.
    limit: usize,
    nodes: u32,
}

impl<'db> Walk<'db> {
    fn new(
        interner: DbInterner<'db>,
        trait_: TraitIdWrapper,
        ty: Ty<'db>,
        inline: Inline,
        limit: usize,
    ) -> Self {
        Walk {
            interner,
            inline,
            stack: [(trait_, ty); DEPTH],
            len: 0,
            deepest: 0,
            limit,
            nodes: NODES,
        }
    }

    fn holds(&mut self, trait_: TraitIdWrapper, ty: Ty<'db>) -> bool {
        if self.len >= self.limit {
            return false;
        }
        if self.stack[..self.len].contains(&(trait_, ty)) {
            // The solver evaluates the goal and finds the cycle there: one level.
            self.deepest = self.deepest.max(self.len + 1);
            return true;
        }
        if self.len == DEPTH || self.nodes == 0 {
            return false;
        }
        self.nodes -= 1;
        self.stack[self.len] = (trait_, ty);
        self.len += 1;
        self.deepest = self.deepest.max(self.len);
        let holds = self.node(trait_, ty);
        self.len -= 1;
        holds
    }

    fn node(&mut self, trait_: TraitIdWrapper, ty: Ty<'db>) -> bool {
        let interner = self.interner;
        if self.inline == Inline::No
            && let TyKind::Adt(def, args) = ty.kind()
            && !def.is_phantom_data()
        {
            let depth =
                auto_trait_depth(interner.db, def.def_id(), trait_.0, interner.expect_crate());
            // A verdict that fails over the identity arguments may need only a fact about the
            // parameters (`Option<T>: Send` needs `T: Send`): an instance's own arguments are
            // walked below instead.
            if depth != 0 || args.is_empty() {
                let deepest = self.len - 1 + usize::from(depth);
                self.deepest = self.deepest.max(deepest);
                return depth != 0 && deepest <= self.limit;
            }
        }
        match impls(interner, trait_, ty) {
            Impls::None => {}
            Impls::One(impl_) => return self.through_impl(impl_, ty),
            Impls::Many => return false,
        }
        if interner.trait_is_unsafe(trait_) && ty.has_unsafe_fields() {
            return false;
        }
        match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Never
            | TyKind::FnDef(..)
            | TyKind::FnPtr(..) => true,
            TyKind::Str => self.holds(trait_, interner.default_types().types.u8_slice),
            TyKind::RawPtr(elem, _)
            | TyKind::Ref(_, elem, _)
            | TyKind::Array(elem, _)
            | TyKind::Slice(elem)
            | TyKind::Pat(elem, _) => self.holds(trait_, elem),
            TyKind::Tuple(tys) => tys.iter().all(|elem| self.holds(trait_, elem)),
            TyKind::Adt(def, args) if def.is_phantom_data() => {
                self.holds(trait_, args.type_at(0))
            }
            TyKind::Adt(def, args) => def
                .all_field_tys(interner)
                .iter_instantiated(interner, args.as_slice())
                .all(|field| self.holds(trait_, field.skip_norm_wip())),
            _ => false,
        }
    }

    /// The impl is the solver's one candidate for the goal on `ty` when its self type is `ty`'s
    /// head over distinct parameters; then its where clauses are the goal's nested goals.
    fn through_impl(&mut self, impl_: AnyImplId, ty: Ty<'db>) -> bool {
        let interner = self.interner;
        if interner.impl_polarity(impl_) != ImplPolarity::Positive
            || interner.impl_is_default(impl_)
        {
            return false;
        }
        let Some(args) = impl_args(interner, impl_, ty) else { return false };
        interner.clauses_of(impl_.into()).iter_instantiated(interner, args.as_slice()).all(
            |clause| {
                let clause = clause.skip_norm_wip();
                let Some(ClauseKind::Trait(pred)) = clause.kind().no_bound_vars() else {
                    return false;
                };
                if pred.polarity != ClausePolarity::Positive {
                    return false;
                }
                let def = pred.def_id();
                if interner.trait_is_auto(def) {
                    return self.holds(def, pred.self_ty());
                }
                matches!(
                    interner.as_trait_lang_item(def),
                    Some(SolverTraitLangItem::Sized | SolverTraitLangItem::MetaSized)
                ) && sizedness_fast_path(interner, clause.as_predicate(), ParamEnv::empty())
            },
        )
    }
}

/// The impl's arguments that make its self type `ty`, when that self type is `ty` with some
/// of its direct arguments replaced by distinct impl parameters: then instantiating the impl
/// is all the unification there is, and adds no constraint.
fn impl_args<'db>(
    interner: DbInterner<'db>,
    impl_: AnyImplId,
    ty: Ty<'db>,
) -> Option<GenericArgs<'db>> {
    let count = interner.generics_of(impl_.into()).count();
    if count > IMPL_PARAMS {
        return None;
    }
    let pattern = interner.impl_trait_ref(impl_).skip_binder().self_ty();
    let mut args: [Option<GenericArg<'db>>; IMPL_PARAMS] = [None; IMPL_PARAMS];
    let mut bind = |param: GenericArg<'db>, actual: GenericArg<'db>| {
        let index: u32 = match param.kind() {
            GenericArgKind::Type(ty) => match ty.kind() {
                TyKind::Param(param) => param.index,
                _ => return param == actual,
            },
            GenericArgKind::Lifetime(region) => match region.kind() {
                RegionKind::ReEarlyParam(param) => param.index,
                _ => return param == actual,
            },
            GenericArgKind::Const(ct) => match ct.kind() {
                ConstKind::Param(param) => param.index,
                _ => return param == actual,
            },
        };
        let index = index as usize;
        match args.get_mut(index) {
            Some(slot) if index < count && slot.is_none() => {
                *slot = Some(actual);
                true
            }
            _ => false,
        }
    };
    let matched = match (pattern.kind(), ty.kind()) {
        (TyKind::Adt(p, p_args), TyKind::Adt(a, a_args)) => {
            p == a && p_args.iter().zip(a_args.iter()).all(|(p, a)| bind(p, a))
        }
        (TyKind::Ref(p_r, p_t, p_m), TyKind::Ref(a_r, a_t, a_m)) => {
            p_m == a_m && bind(p_r.into(), a_r.into()) && bind(p_t.into(), a_t.into())
        }
        (TyKind::RawPtr(p_t, p_m), TyKind::RawPtr(a_t, a_m)) => {
            p_m == a_m && bind(p_t.into(), a_t.into())
        }
        (TyKind::Slice(p_t), TyKind::Slice(a_t)) => bind(p_t.into(), a_t.into()),
        (TyKind::Array(p_t, p_c), TyKind::Array(a_t, a_c)) => {
            bind(p_t.into(), a_t.into()) && bind(p_c.into(), a_c.into())
        }
        (TyKind::Tuple(p), TyKind::Tuple(a)) => {
            p.len() == a.len() && p.iter().zip(a.iter()).all(|(p, a)| bind(p.into(), a.into()))
        }
        _ => pattern == ty,
    };
    if !matched || args[..count].iter().any(Option::is_none) {
        return None;
    }
    let mut filled = [GenericArg::from(ty); IMPL_PARAMS];
    for (slot, arg) in filled.iter_mut().zip(&args[..count]) {
        *slot = arg.expect("checked above");
    }
    Some(GenericArgs::new_from_slice(&filled[..count]))
}
