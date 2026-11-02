//! Impl specialization related things

use base_db::Db;
use hir_def::{
    HasModule, ImplId, LifetimeParamId, TypeOrConstParamId, hir::generics::GenericParams,
    signatures::ImplSignature, unstable_features::UnstableFeatures,
};
use blaze_mem::Frame;
use rustc_type_ir::{
    AliasTyKind, ConstKind, RegionKind, TypeSuperVisitable, TypeVisitable, TypeVisitableExt as _,
    TypeVisitor,
    fast_reject::{DeepRejectCtxt, TreatParams, simplify_type},
    inherent::IntoKind as _,
};
use smallvec::SmallVec;
use tracing::debug;

use crate::{
    FrameSet, Span,
    db::HirDatabase,
    frame_set,
    lower::GenericPredicates,
    method_resolution::{RelevantImplsKey, SelfTyKey, relevant_impls},
    next_solver::{
        AnyImplId, ClauseKind, Const, DbInterner, Region, Ty, TyKind, TypingMode, Unnormalized,
        infer::{DbInternerInferExt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
        util::clauses_as_obligations,
    },
};

/// The key of [`specializes_query`]: whether `specializing` specializes `parent`.
#[blaze_query::interned(debug)]
pub struct ImplPair {
    #[returns(copy)]
    pub specializing: ImplId,
    #[returns(copy)]
    pub parent: ImplId,
}

// rustc does not have a cycle handling for the `specializes` query, meaning a cycle is a bug,
// and indeed I was unable to cause cycles even with erroneous code. However, in r-a we can
// create a cycle if there is an error in the impl's where clauses. I believe well formed code
// cannot create a cycle, but a cycle handler is required nevertheless.
fn specializes_query_cycle(_db: &Db, _: blaze_query::db::Id, _pair: ImplPair) -> bool {
    false
}

/// Whether the crate of `impl_id` enables `specialization` or `min_specialization`: only
/// then may it specialize another impl.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub(crate) fn may_specialize(db: &Db, impl_id: ImplId) -> bool {
    let features = UnstableFeatures::query(db, impl_id.krate(db));
    features.specialization || features.min_specialization
}

/// Is `specializing_impl_def_id` a specialization of `parent_impl_def_id`?
///
/// For every type that could apply to `specializing_impl_def_id`, we prove that
/// the `parent_impl_def_id` also applies (i.e. it has a valid impl header and
/// its where-clauses hold).
///
/// For the purposes of const traits, we also check that the specializing
/// impl is not more restrictive than the parent impl. That is, if the
/// `parent_impl_def_id` is a const impl (conditionally based off of some `[const]`
/// bounds), then `specializing_impl_def_id` must also be const for the same
/// set of types.
#[blaze_query::tracked(cycle_result = specializes_query_cycle, returns(copy), mem_tag = Infer)]
fn specializes_query<'db>(db: &'db Db, pair: ImplPair<'db>) -> bool {
    let (specializing_impl_def_id, parent_impl_def_id) = (pair.specializing(db), pair.parent(db));
    let trait_env = db.trait_environment(specializing_impl_def_id.into());
    let interner = DbInterner::new_with(db, specializing_impl_def_id.krate(db));

    let specializing_impl_signature = ImplSignature::of(db, specializing_impl_def_id);
    let parent_impl_signature = ImplSignature::of(db, parent_impl_def_id);

    // We determine whether there's a subset relationship by:
    //
    // - replacing bound vars with placeholders in impl1,
    // - assuming the where clauses for impl1,
    // - instantiating impl2 with fresh inference variables,
    // - unifying,
    // - attempting to prove the where clauses for impl2
    //
    // The last three steps are encapsulated in `fulfill_implication`.
    //
    // See RFC 1210 for more details and justification.

    // Currently we do not allow e.g., a negative impl to specialize a positive one
    if specializing_impl_signature.is_negative() != parent_impl_signature.is_negative() {
        return false;
    }

    // create a parameter environment corresponding to an identity instantiation of the specializing impl,
    // i.e. the most generic instantiation of the specializing impl.
    let param_env = trait_env;

    // Create an infcx, taking the predicates of the specializing impl as assumptions:
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());

    let specializing_impl_trait_ref =
        db.impl_trait(specializing_impl_def_id).unwrap().instantiate_identity().skip_norm_wip();
    let cause = &ObligationCause::dummy();
    debug!(
        "fulfill_implication({:?}, trait_ref={:?} |- {:?} applies)",
        param_env, specializing_impl_trait_ref, parent_impl_def_id
    );

    // Attempt to prove that the parent impl applies, given all of the above.

    let mut ocx = ObligationCtxt::new(&infcx);

    let parent_args = infcx.fresh_args_for_item(Span::Dummy, parent_impl_def_id.into());
    let parent_impl_trait_ref = db
        .impl_trait(parent_impl_def_id)
        .expect("expected source impl to be a trait impl")
        .instantiate(interner, parent_args)
        .skip_norm_wip();

    // do the impls unify? If not, no specialization.
    let Ok(()) = ocx.eq(cause, param_env, specializing_impl_trait_ref, parent_impl_trait_ref)
    else {
        return false;
    };

    // Now check that the source trait ref satisfies all the where clauses of the target impl.
    // This is not just for correctness; we also need this to constrain any params that may
    // only be referenced via projection predicates.
    ocx.register_obligations(clauses_as_obligations(
        GenericPredicates::query_all(db, parent_impl_def_id.into())
            .iter_instantiated(interner, parent_args.as_slice())
            .map(Unnormalized::skip_norm_wip),
        *cause,
        param_env,
    ));

    let errors = ocx.evaluate_obligations_error_on_ambiguity();
    if !errors.is_empty() {
        // no dice!
        debug!(
            "fulfill_implication: for impls on {:?} and {:?}, \
                 could not fulfill: {:?} given {:?}",
            specializing_impl_trait_ref, parent_impl_trait_ref, errors, param_env
        );
        return false;
    }

    // FIXME: Check impl constness (when we implement const impls).

    debug!(
        "fulfill_implication: an impl for {:?} specializes {:?}",
        specializing_impl_trait_ref, parent_impl_trait_ref
    );

    true
}

// This function is used to avoid creating the query for crates that does not define `#![feature(specialization)]`,
// as the solver is calling this a lot, and creating the query consumes a lot of memory.
pub(crate) fn specializes(
    db: &Db,
    specializing_impl_def_id: ImplId,
    parent_impl_def_id: ImplId,
) -> bool {
    // We check that the specializing impl comes from a crate that has specialization enabled.
    //
    // We don't really care if the specialized impl (the parent) is in a crate that has
    // specialization enabled, since it's not being specialized.
    //
    // rustc also checks whether the specializing impls comes from a macro marked
    // `#[allow_internal_unstable(specialization)]`, but `#[allow_internal_unstable]`
    // is an internal feature, std is not using it for specialization nor is likely to
    // ever use it, and we don't have the span information necessary to replicate that.
    may_specialize(db, specializing_impl_def_id)
        && specializes_query(db, ImplPair::new(db, specializing_impl_def_id, parent_impl_def_id))
}

/// The impl that `impl_id` takes an item it leaves out from: the closest of the impls it
/// specializes for which `has_item` holds.
pub(crate) fn specialized_ancestor(
    db: &Db,
    impl_id: ImplId,
    has_item: impl Fn(ImplId) -> bool,
) -> Option<ImplId> {
    if !may_specialize(db, impl_id) {
        return None;
    }
    let trait_ref = db.impl_trait(impl_id)?.instantiate_identity().skip_norm_wip();
    // As rustc's `overlapping_impls` rejects a pair before relating it, with the parameters as
    // `specializes_query` has them: the specializing impl's rigid, the parent's inference
    // variables. A pair rejected here is one whose trait refs that query fails to equate.
    let interner = DbInterner::new_no_crate(db);
    let reject = DeepRejectCtxt::relate_rigid_infer(interner);
    let headers_may_unify = |ancestor: ImplId| {
        db.impl_trait(ancestor).is_some_and(|parent| {
            let parent = parent.instantiate_identity().skip_norm_wip();
            reject.args_may_unify(trait_ref.args, parent.args)
        })
    };
    // An impl `impl_id` specializes is at least as general: a blanket impl, or one for the
    // same simplified self type. The closest is the one that specializes every other, so the
    // order these come in (blanket impls first) picks the same one.
    let (krate, trait_id) = (impl_id.krate(db), trait_ref.def_id.0);
    let self_ty = simplify_type(interner, trait_ref.self_ty(), TreatParams::InstantiateWithInfer);
    let key = RelevantImplsKey::new(db, krate, trait_id, self_ty.map(SelfTyKey));
    let relevant = relevant_impls(db, key);
    fn impls(impls: &[AnyImplId]) -> impl Iterator<Item = ImplId> + '_ {
        impls.iter().filter_map(|&it| match it {
            AnyImplId::ImplId(it) => Some(it),
            AnyImplId::BuiltinDeriveImplId(_) => None,
        })
    }
    let ancestors: SmallVec<[ImplId; 4]> = impls(relevant.blanket())
        .chain(impls(relevant.for_self_ty()))
        .filter(|&ancestor| {
            ancestor != impl_id
                && has_item(ancestor)
                && headers_may_unify(ancestor)
                && specializes(db, impl_id, ancestor)
        })
        // alloc: inline up to 4 ancestors; an impl specializes one or two.
        .collect();
    ancestors.iter().copied().find(|&ancestor| {
        ancestors.iter().all(|&other| other == ancestor || specializes(db, ancestor, other))
    })
}

/// A parameter of an impl, as rustc's `constrained_generic_params::Parameter`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Parameter {
    TypeOrConst(TypeOrConstParamId),
    Lifetime(LifetimeParamId),
}

/// The parameters in `value`, as rustc's `parameters_for`: those in an associated type or an
/// unevaluated constant only with `include_nonconstraining`, as they do not constrain one.
fn parameters_for<'db>(
    value: impl TypeVisitable<DbInterner<'db>>,
    include_nonconstraining: bool,
    out: &mut FrameSet<'_, Parameter>,
) {
    struct Collect<'a, 'f> {
        include_nonconstraining: bool,
        out: &'a mut FrameSet<'f, Parameter>,
    }
    impl<'db> TypeVisitor<DbInterner<'db>> for Collect<'_, '_> {
        type Result = ();
        fn visit_ty(&mut self, ty: Ty<'db>) {
            match ty.kind() {
                TyKind::Alias(_, alias)
                    if !self.include_nonconstraining
                        && matches!(
                            alias.kind,
                            AliasTyKind::Projection { .. } | AliasTyKind::Inherent { .. }
                        ) =>
                {
                    return;
                }
                TyKind::Param(param) => {
                    self.out.insert(Parameter::TypeOrConst(param.id.into()));
                }
                _ => (),
            }
            ty.super_visit_with(self)
        }
        fn visit_region(&mut self, region: Region<'db>) {
            if let RegionKind::ReEarlyParam(param) = region.kind() {
                self.out.insert(Parameter::Lifetime(param.id));
            }
        }
        fn visit_const(&mut self, konst: Const<'db>) {
            match konst.kind() {
                ConstKind::Alias(..) if !self.include_nonconstraining => return,
                ConstKind::Param(param) => {
                    self.out.insert(Parameter::TypeOrConst(param.id.into()));
                }
                _ => (),
            }
            konst.super_visit_with(self)
        }
    }
    value.visit_with(&mut Collect { include_nonconstraining, out });
}

/// Whether a type or const parameter of `impl_id` is constrained neither by its header nor
/// by the associated types its where clauses equate with what is (rustc's E0207,
/// `enforce_impl_non_lifetime_params_are_constrained`). rustc's `assoc_def` refuses to
/// project an item such an impl defines, which would leave the parameter an inference
/// variable nothing constrains, and gives an error instead.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub(crate) fn impl_has_unconstrained_params(db: &Db, impl_id: ImplId) -> bool {
    blaze_mem::frame::current(|frame| {
        let Some(constrained) = constrained_params(db, frame, impl_id) else { return false };
        type_or_const_params(db, impl_id)
            .any(|param| !constrained.contains(&Parameter::TypeOrConst(param)))
    })
}

/// The type and const parameters of `impl_id` that are constrained neither by its header nor
/// by the associated types its where clauses equate with what is, in declaration order: each
/// is one E0207. None when the header has an error type.
pub fn impl_unconstrained_params(db: &Db, impl_id: ImplId) -> Vec<TypeOrConstParamId> {
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
        // alloc: an empty `Vec` holds no block
        let Some(constrained) = constrained_params(db, frame, impl_id) else { return Vec::new() };
        let mut unconstrained = Vec::new_in(frame);
        unconstrained.extend(
            type_or_const_params(db, impl_id)
                .filter(|&param| !constrained.contains(&Parameter::TypeOrConst(param))),
        );
        // alloc: the caller's result, one block at its size
        unconstrained.iter().copied().collect()
    })
}

fn type_or_const_params(
    db: &Db,
    impl_id: ImplId,
) -> impl Iterator<Item = TypeOrConstParamId> + '_ {
    GenericParams::of(db, impl_id.into())
        .iter_type_or_consts()
        .map(move |(local_id, _)| TypeOrConstParamId { parent: impl_id.into(), local_id })
}

/// The parameters of `impl_id` its header and its where clauses' projections constrain, or
/// `None` when the header has an error type.
fn constrained_params<'f>(
    db: &Db,
    frame: &'f Frame,
    impl_id: ImplId,
) -> Option<FrameSet<'f, Parameter>> {
    let interner = DbInterner::new_no_crate(db);
    let self_ty = db.impl_self_ty(impl_id).instantiate_identity().skip_norm_wip();
    let trait_ref = db.impl_trait(impl_id).map(|it| it.instantiate_identity().skip_norm_wip());
    if self_ty.references_error() || trait_ref.is_some_and(|it| it.references_error()) {
        return None;
    }
    let mut constrained = frame_set(frame);
    match trait_ref {
        Some(trait_ref) => parameters_for(trait_ref, false, &mut constrained),
        None => parameters_for(self_ty, false, &mut constrained),
    }
    // rustc's `setup_constraining_predicates`: `<T as Trait>::Assoc == U` constrains `U` once
    // what the projection names is. A projection of the impl's own trait ref never does.
    let mut projections = Vec::new_in(frame);
    let predicates = GenericPredicates::query_own_explicit(db, impl_id.into());
    projections.extend(predicates.iter_identity().filter_map(|it| {
        match it.skip_norm_wip().kind().skip_binder() {
            ClauseKind::Projection(projection)
                if Some(projection.projection_term.trait_ref(interner)) != trait_ref =>
            {
                Some(projection)
            }
            _ => None,
        }
    }));
    let mut inputs = frame_set(frame);
    loop {
        let before = constrained.len();
        for projection in &projections {
            inputs.clear();
            parameters_for(projection.projection_term, true, &mut inputs);
            if inputs.is_subset(&constrained) {
                parameters_for(projection.term, false, &mut constrained);
            }
        }
        if constrained.len() == before {
            break;
        }
    }
    Some(constrained)
}
