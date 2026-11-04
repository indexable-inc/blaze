//! blaze: compile-time reflection in the solver (`#![feature(blaze_comptime)]`,
//! docs/v2/plan/design/comptime.md, phase 2, and `rustc_type_ir::reflect`).
//!
//! - The builtin candidates of `Struct`, `Enum`, `Field<B>`, `Variant<B>` and `FnItem`
//!   ([`reflect_trait_holds`], used by the trait, projection and host-effect goal kinds).
//! - Proving a field binder, `for<F: Field<C>> P(F)`: expanded over the members of `C`
//!   ([`EvalCtxt::compute_reflected_forall_goal`]).
//! - Assuming one: never matched against a goal directly (an inference variable for `F` would
//!   stand for any type, not only a member); it reaches the environment through its members, as
//!   [`EvalCtxt::env_with_reflected_member`] instantiates it with each member that enters it.

use rustc_type_ir::inherent::*;
use rustc_type_ir::lang_items::SolverTraitLangItem;
use rustc_type_ir::reflect::{
    ReflectContainer, ReflectedDomain, first_reflected_var, instantiate_reflected_var,
};
use rustc_type_ir::{
    self as ty, Interner, TraitRef, TypeVisitableExt as _, Upcast as _, elaborate,
};

use crate::delegate::SolverDelegate;
use crate::solve::{
    Certainty, EvalCtxt, Goal, GoalSource, NoSolution, QueryResultOrRerunNonErased,
};

/// What a builtin reflection candidate answers for its goal's self type.
pub(super) enum ReflectAnswer<I: Interner> {
    Holds,
    /// Holds when the trait's `Base` argument is this container.
    HoldsFor(I::Ty),
    Ambiguous,
    Fails,
}

/// The domain the reflection trait `item` ranges a binder over, if any.
pub(super) fn domain_of(item: SolverTraitLangItem) -> Option<ReflectedDomain> {
    match item {
        SolverTraitLangItem::ReflectField => Some(ReflectedDomain::Fields),
        SolverTraitLangItem::ReflectVariant => Some(ReflectedDomain::Variants),
        _ => None,
    }
}

/// Whether the reflection trait `item` holds of `self_ty`, structurally (no environment):
/// `Struct` of a reflectable struct, a tuple or a variant view; `Enum` of an enum; `FnItem` of
/// a fn item or closure; `Field<B>` and `Variant<B>` of a member of `B`.
pub(super) fn reflect_trait_holds<I: Interner>(
    cx: I,
    item: SolverTraitLangItem,
    self_ty: I::Ty,
) -> ReflectAnswer<I> {
    let yes_if = |holds: bool| {
        if holds { ReflectAnswer::Holds } else { ReflectAnswer::Fails }
    };
    match item {
        SolverTraitLangItem::ReflectStruct => match self_ty.kind() {
            ty::Tuple(_) => ReflectAnswer::Holds,
            ty::Adt(..) => match cx.reflect_is_struct(self_ty) {
                Some(holds) => yes_if(holds),
                None => ReflectAnswer::Ambiguous,
            },
            _ => ReflectAnswer::Fails,
        },
        SolverTraitLangItem::ReflectEnum => yes_if(cx.reflect_is_enum(self_ty)),
        SolverTraitLangItem::ReflectFnItem => {
            yes_if(matches!(self_ty.kind(), ty::FnDef(..) | ty::Closure(..)))
        }
        SolverTraitLangItem::ReflectField | SolverTraitLangItem::ReflectVariant => {
            let domain = domain_of(item).expect("`Field` and `Variant` range over a domain");
            match cx.reflect_container_of(self_ty, domain) {
                ReflectContainer::Of(container) => ReflectAnswer::HoldsFor(container),
                ReflectContainer::Not => ReflectAnswer::Fails,
                ReflectContainer::Unknown => ReflectAnswer::Ambiguous,
            }
        }
        _ => unreachable!("{item:?} is not a reflection trait"),
    }
}

impl<D, I> EvalCtxt<'_, D>
where
    D: SolverDelegate<Interner = I>,
    I: Interner,
{
    /// A builtin candidate answering `answer` for a goal whose trait's `Base` argument is
    /// `base` (`Field<B>`, `Variant<B>`), for the trait and host-effect goal kinds.
    pub(super) fn reflect_candidate_response(
        &mut self,
        param_env: I::ParamEnv,
        answer: ReflectAnswer<I>,
        base: Option<I::Ty>,
    ) -> QueryResultOrRerunNonErased<I> {
        match answer {
            ReflectAnswer::Holds => {
                self.evaluate_added_goals_and_make_canonical_response(Certainty::Yes)
            }
            ReflectAnswer::HoldsFor(container) => {
                let base = base.expect("`Field` and `Variant` have a `Base` argument");
                self.eq(param_env, base, container)?;
                self.evaluate_added_goals_and_make_canonical_response(Certainty::Yes)
            }
            ReflectAnswer::Ambiguous => {
                self.evaluate_added_goals_and_make_canonical_response(Certainty::AMBIGUOUS)
            }
            ReflectAnswer::Fails => Err(NoSolution.into()),
        }
    }

    /// Proves `kind`, a predicate whose binder has the reflected variable `index` (its first):
    /// `for<F: Field<C>> P(F)` holds when `C` is a `Struct` and `P` holds of every field of `C`
    /// (`for<V: Variant<C>>`: `C` an `Enum`, every variant). The variable is instantiated with
    /// each member in declaration order, so the first failing nested goal is the first failing
    /// field, which is what an error names.
    ///
    /// A container that is a parameter, a placeholder or a rigid alias has no known members:
    /// `P` must hold of a member `!F` in a new universe, with `!F: Field<C>` and the
    /// environment's field binders over `C` instantiated with it ([`Self::env_with_reflected_member`]).
    /// A container still an inference variable is ambiguous.
    pub(super) fn compute_reflected_forall_goal(
        &mut self,
        param_env: I::ParamEnv,
        kind: ty::Binder<I, ty::PredicateKind<I>>,
        index: usize,
    ) -> QueryResultOrRerunNonErased<I> {
        let cx = self.cx();
        let vars = kind.bound_vars();
        let var = match vars.get(index) {
            Some(ty::BoundVariableKind::Reflected(var)) => var,
            other => unreachable!("variable {index} of {vars:?} is {other:?}, not reflected"),
        };
        // Lowering puts a binder's reflected variables first and refuses a container naming a
        // variable of its own binder (a lifetime of `for<'a, F: Field<&'a T>>`): no member of
        // such a container is known before the binder is entered.
        if var.container.has_escaping_bound_vars() {
            return Err(NoSolution.into());
        }
        let container = self.structurally_normalize_ty(param_env, var.container)?;
        let domain_trait = cx.require_trait_lang_item(match var.domain {
            ReflectedDomain::Fields => SolverTraitLangItem::ReflectStruct,
            ReflectedDomain::Variants => SolverTraitLangItem::ReflectEnum,
        });
        match container.kind() {
            ty::Infer(ty::TyVar(_)) => {
                return self.evaluate_added_goals_and_make_canonical_response(Certainty::AMBIGUOUS);
            }
            ty::Param(_) | ty::Placeholder(_) | ty::Alias(ty::IsRigid::Yes, _) => {
                let universe = self.create_next_universe();
                let member = I::Ty::new_placeholder(
                    cx,
                    ty::PlaceholderType::new(
                        universe,
                        ty::BoundTy { var: ty::BoundVar::from_usize(index), kind: var.kind },
                    ),
                );
                let env = self.env_with_reflected_member(param_env, member, var.domain, container);
                let rest = instantiate_reflected_var(cx, kind, index, member);
                self.add_goal(GoalSource::Misc, Goal::new(cx, env, rest))?;
            }
            _ => {
                // `C: Struct` (`C: Enum`): a container with no members to reflection is refused,
                // naming why, rather than every bound over it holding vacuously.
                self.add_goal(
                    GoalSource::Misc,
                    Goal::new(cx, param_env, TraitRef::new(cx, domain_trait, [container])),
                )?;
                for member in cx.reflect_members(container, var.domain).iter() {
                    let rest = instantiate_reflected_var(cx, kind, index, member);
                    self.add_goal(GoalSource::Misc, Goal::new(cx, param_env, rest))?;
                }
            }
        }
        self.evaluate_added_goals_and_make_canonical_response(Certainty::Yes)
    }

    /// `param_env` with `member` entering it as a member of `container` in `domain`:
    /// `member: Field<container>` (`Variant`) and what it elaborates to, and each field binder
    /// of the environment over `container` in `domain` instantiated with `member`
    /// (`for<F: Field<T>> F::Type: Hash` gives `<member as Field>::Type: Hash`). This is how a
    /// field binder in the environment is used: only ever of a member.
    pub(super) fn env_with_reflected_member(
        &mut self,
        param_env: I::ParamEnv,
        member: I::Ty,
        domain: ReflectedDomain,
        container: I::Ty,
    ) -> I::ParamEnv {
        let cx = self.cx();
        let item = match domain {
            ReflectedDomain::Fields => SolverTraitLangItem::ReflectField,
            ReflectedDomain::Variants => SolverTraitLangItem::ReflectVariant,
        };
        let membership: I::Clause =
            TraitRef::new(cx, cx.require_trait_lang_item(item), [member, container]).upcast(cx);
        let instantiated = param_env.caller_bounds().filter_map(|clause| {
            let kind = clause.kind();
            let (index, var) = first_reflected_var::<I>(kind.bound_vars())?;
            (var.domain == domain && var.container == container)
                .then(|| instantiate_reflected_var(cx, kind, index, member).upcast(cx))
        });
        let added = elaborate::elaborate(cx, [membership].into_iter().chain(instantiated));
        cx.param_env_with_clauses(param_env, added)
    }
}

/// Whether a goal's predicate is a field binder, and the index of its first reflected variable.
pub(super) fn reflected_binder<I: Interner>(
    kind: &ty::Binder<I, ty::PredicateKind<I>>,
) -> Option<usize> {
    first_reflected_var::<I>(kind.bound_vars()).map(|(index, _)| index)
}

/// Whether an assumption is a field binder, which no goal matches directly.
pub(super) fn is_field_binder<I: Interner>(clause: I::Clause) -> bool {
    first_reflected_var::<I>(clause.kind().bound_vars()).is_some()
}
