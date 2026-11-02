//! This module is concerned with finding methods that a given type provides.
//! For details about how this works in rustc, see the method lookup page in the
//! [rustc guide] and the corresponding code mostly in
//! [`rustc_hir_typeck/method/probe.rs`].
//!
//! [rustc guide]: https://rust-lang.github.io/rustc-guide/method-lookup.html
//! [`rustc_hir_typeck/method/probe.rs`]: https://github.com/rust-lang/rust/blob/5503df87342a73d0c29126a7e08dc9c1255c46ad/compiler/rustc_hir_typeck/src/method/probe.rs

mod confirm;
pub mod delta;
pub mod frozen;
#[cfg(test)]
mod naming_tests;
mod pick_cache;
mod probe;
#[cfg(test)]
mod relevant_tests;
pub mod scope_sets;

use std::{hash::BuildHasher, ops::Range};

use crate::next_solver::StoredGenericArgs;
use blaze_ids::bits::{BitMatrix, Ones, words_for};
use blaze_mem::Frame;
use blaze_query::db::AsId;
use either::Either;
use hir_expand::name::Name;
use intern::{Symbol, sym};
use span::Edition;
use tracing::{debug, instrument};

use base_db::{Crate, Db};
use blaze_hash::FixedState;
use hir_def::{
    AssocItemId, BlockIdLt, BuiltinDeriveImplId, ConstId, DefWithBodyId, FunctionId,
    GenericParamId, HasModule, ImplId, ItemContainerId, MacroId, ModuleDefId, ModuleId, TraitId,
    attrs::AttrFlags,
    builtin_derive::BuiltinDeriveImplMethod,
    expr_store::{Body, path::GenericArgs as HirGenericArgs},
    hir::{ExprId, generics::GenericParams},
    item_scope::ItemScope,
    lang_item::LangItems,
    nameres::{DefMap, block_def_map, crate_def_map, module_impls},
    resolver::Resolver,
    signatures::{ConstSignature, FunctionSignature},
    unstable_features::UnstableFeatures,
};
use rustc_type_ir::{
    TypeFoldable, TypeVisitableExt, VisitorResult,
    fast_reject::{TreatParams, simplify_type},
    inherent::{BoundExistentialPredicates, IntoKind},
    try_visit,
};
use stdx::impl_from;
use syntax::{SyntaxKind, T, TextSize};
use triomphe::Arc;

use crate::{
    Span, all_super_traits,
    db::HirDatabase,
    infer::{InferenceContext, unify::InferenceTable},
    lower::GenericPredicates,
    next_solver::{
        AnyImplId, Binder, ClauseKind, DbInterner, FnSig, GenericArgs, ParamEnv, PredicateKind,
        SimplifiedType, SolverDefId, TraitRef, Ty, TyKind, TypingMode, Unnormalized,
        infer::{
            BoundRegionConversionTime, DbInternerInferExt, InferCtxt, InferOk,
            resolve::ReplaceInferWithError,
            select::ImplSource,
            traits::{Obligation, ObligationCause, PredicateObligations},
        },
        obligation_ctxt::ObligationCtxt,
        util::clauses_as_obligations,
    },
    traits::ParamEnvAndCrate,
};

pub use self::{
    probe::{
        AutorefOrPtrAdjustment, Candidate, CandidateKind, CandidateStep, CandidateWithPrivate,
        Mode, Pick, PickKind,
    },
    scope_sets::{TraitSet, TraitsInScope},
};
pub use crate::specialization::ImplPair;
pub(crate) use crate::{FrameMap, FrameSet};

pub struct MethodResolutionContext<'a, 'db> {
    pub infcx: &'a InferCtxt<'db>,
    pub resolver: &'a Resolver<'db>,
    pub param_env: ParamEnv<'db>,
    pub traits_in_scope: TraitsInScope,
    pub edition: Edition,
    pub features: &'a UnstableFeatures,
    pub call_span: Span,
    pub receiver_span: Span,
    /// The frame the probe's temporaries live in: the inference run's, or the task a caller
    /// outside inference opened.
    pub frame: &'a Frame,
    /// Where the probe puts the errors rustc reports as it probes, for inference to report;
    /// `None` silences them, as rustc's `is_suggestion` probes are.
    pub errors: Option<&'a ProbeErrors<'db>>,
}

/// The errors a method probe finds that rustc reports from the probe itself.
#[derive(Default)]
pub struct ProbeErrors<'db> {
    /// E0055: the receiver's autoderef went past the crate's recursion limit at this type
    /// (resolved, its variables made errors).
    pub(crate) recursion_limit: std::cell::Cell<Option<Ty<'db>>>,
    /// E0282: the receiver's autoderef ended at this type variable, with whether it went
    /// through a raw pointer to reach it.
    pub(crate) unknown_receiver: std::cell::Cell<Option<(Ty<'db>, bool)>>,
    /// `tyvar_behind_raw_pointer`: as `unknown_receiver` through a raw pointer, in edition 2015,
    /// where rustc lints and goes on.
    pub(crate) tyvar_behind_raw_pointer: std::cell::Cell<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub enum CandidateId {
    FunctionId(FunctionId),
    ConstId(ConstId),
}
impl_from!(FunctionId, ConstId for CandidateId);

impl CandidateId {
    fn container(self, db: &Db) -> ItemContainerId {
        match self {
            CandidateId::FunctionId(id) => id.loc(db).container,
            CandidateId::ConstId(id) => id.loc(db).container,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MethodCallee<'db> {
    /// Impl method ID, for inherent methods, or trait method ID, otherwise.
    pub def_id: FunctionId,
    pub args: GenericArgs<'db>,

    /// Instantiated method signature, i.e., it has been
    /// instantiated, normalized, and has had late-bound
    /// lifetimes replaced with inference variables.
    pub sig: FnSig<'db>,
}

#[derive(Debug)]
pub enum MethodError<'db> {
    /// Did not find an applicable method.
    NoMatch,

    /// Multiple methods might apply.
    Ambiguity(Vec<CandidateSource>),

    /// Found an applicable method, but it is not visible.
    PrivateMatch(Pick<'db>),

    /// Found a `Self: Sized` bound where `Self` is a trait object.
    IllegalSizedBound { candidates: Vec<FunctionId>, needs_mut: bool },

    /// Error has already been emitted, no need to emit another one.
    ErrorReported,
}

// A pared down enum describing just the places from which a method
// candidate can arise. Used for error reporting only.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CandidateSource {
    Impl(AnyImplId),
    Trait(TraitId),
}

/// What a method is looked up for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProbeFor {
    /// A method call: its pick marks the trait's import used and may give the edition lint.
    Call,
    /// A field access that names no field, asking whether a method of the name exists
    /// (rustc's `method_exists_for_diagnostic`): neither, as no call is made.
    Diagnostic,
}

impl<'db, 'f> InferenceContext<'db, 'f> {
    /// Performs method lookup. If lookup is successful, it will return the callee
    /// and store an appropriate adjustment for the self-expr. In some cases it may
    /// report an error (e.g., invoking the `drop` method).
    #[instrument(level = "debug", skip(self))]
    pub(crate) fn lookup_method_including_private(
        &mut self,
        self_ty: Ty<'db>,
        name: Name,
        generic_args: Option<HirGenericArgs<'_>>,
        receiver: ExprId,
        call_expr: ExprId,
    ) -> Result<(MethodCallee<'db>, bool), MethodError<'db>> {
        self.lookup_method_as(ProbeFor::Call, self_ty, name, generic_args, receiver, call_expr)
    }

    /// [`Self::lookup_method_including_private`] for a field access `field_expr` that names no
    /// field: the same pick, adjustments, unstable candidates and resolution, but the pick's
    /// trait import is not marked used and no edition lint is given for it, as rustc's
    /// `method_exists_for_diagnostic` does neither.
    #[instrument(level = "debug", skip(self))]
    pub(crate) fn lookup_method_for_diagnostic(
        &mut self,
        self_ty: Ty<'db>,
        name: Name,
        receiver: ExprId,
        field_expr: ExprId,
    ) -> Result<(MethodCallee<'db>, bool), MethodError<'db>> {
        self.lookup_method_as(ProbeFor::Diagnostic, self_ty, name, None, receiver, field_expr)
    }

    fn lookup_method_as(
        &mut self,
        probe_for: ProbeFor,
        self_ty: Ty<'db>,
        name: Name,
        generic_args: Option<HirGenericArgs<'_>>,
        receiver: ExprId,
        call_expr: ExprId,
    ) -> Result<(MethodCallee<'db>, bool), MethodError<'db>> {
        let (pick, is_visible) = match self.lookup_probe(call_expr, receiver, name, self_ty) {
            Ok(it) => {
                self.write_unstable_candidates(call_expr.into(), &it);
                if probe_for == ProbeFor::Call {
                    self.record_trait_pick(call_expr.into(), &it);
                    self.lint_edition_dependent_dot_call(self_ty, call_expr, receiver, &it);
                }
                (it, true)
            }
            Err(MethodError::PrivateMatch(it)) => {
                // FIXME: Report error.
                (it, false)
            }
            Err(err) => return Err(err),
        };

        let result = self.confirm_method(&pick, self_ty, call_expr, generic_args);
        debug!("result = {:?}", result);

        if let Some(bound) = result.illegal_sized_bound {
            let method = result.callee.def_id;
            self.push_illegal_sized_bound(&pick, self_ty, call_expr, receiver, method, bound);
        }

        self.write_expr_adj(receiver, &result.adjustments);
        self.write_method_resolution(call_expr, result.callee.def_id, result.callee.args);

        Ok((result.callee, is_visible))
    }

    #[instrument(level = "debug", skip(self))]
    pub(crate) fn lookup_probe(
        &self,
        call_expr: ExprId,
        receiver: ExprId,
        method_name: Name,
        self_ty: Ty<'db>,
    ) -> probe::PickResult<'db> {
        self.with_method_resolution(call_expr.into(), receiver.into(), |ctx| {
            let pick = ctx.probe_for_name(probe::Mode::MethodCall, method_name, self_ty)?;
            Ok(pick)
        })
    }

    pub(crate) fn with_method_resolution<R>(
        &self,
        call_span: Span,
        receiver_span: Span,
        f: impl FnOnce(&MethodResolutionContext<'_, 'db>) -> R,
    ) -> R {
        let errors = ProbeErrors::default();
        let ctx = MethodResolutionContext {
            infcx: &self.table.infer_ctxt,
            resolver: &self.resolver,
            param_env: self.table.param_env,
            traits_in_scope: TraitsInScope::of_resolver(self.db, &self.resolver),
            edition: self.edition,
            features: self.features,
            call_span,
            receiver_span,
            frame: self.frame,
            errors: Some(&errors),
        };
        let result = f(&ctx);
        if let Some(ty) = errors.recursion_limit.take() {
            self.table.recursion_limit_errors.report(&self.table.infer_ctxt, call_span, ty);
        }
        if let Some((ty, raw_pointer)) = errors.unknown_receiver.take() {
            self.report_unknown_receiver(call_span, ty, raw_pointer);
        }
        if errors.tyvar_behind_raw_pointer.take() {
            self.lint_tyvar_behind_raw_pointer(call_span);
        }
        result
    }
}

/// Used by `FnCtxt::lookup_method_for_operator` with `-Znext-solver`.
///
/// With `AsRigid` we error on `impl Opaque: NotInItemBounds` while
/// `AsInfer` just treats it as ambiguous and succeeds. This is necessary
/// as we want `FnCtxt::check_expr_call` to treat not-yet-defined opaque
/// types as rigid to support `impl Deref<Target = impl FnOnce()>` and
/// `Box<impl FnOnce()>`.
///
/// We only want to treat opaque types as rigid if we need to eagerly choose
/// between multiple candidates. We otherwise treat them as ordinary inference
/// variable to avoid rejecting otherwise correct code.
#[derive(Debug)]
pub(super) enum TreatNotYetDefinedOpaques {
    AsInfer,
    AsRigid,
}

impl<'db> InferenceTable<'db> {
    /// `lookup_method_in_trait` is used for overloaded operators.
    /// It does a very narrow slice of what the normal probe/confirm path does.
    /// In particular, it doesn't really do any probing: it simply constructs
    /// an obligation for a particular trait with the given self type and checks
    /// whether that trait is implemented.
    #[instrument(level = "debug", skip(self))]
    pub(super) fn lookup_method_for_operator(
        &self,
        cause: ObligationCause,
        trait_def_id: TraitId,
        method_item: FunctionId,
        self_ty: Ty<'db>,
        opt_rhs_ty: Option<Ty<'db>>,
        treat_opaques: TreatNotYetDefinedOpaques,
    ) -> Option<InferOk<'db, MethodCallee<'db>>> {
        // Construct a trait-reference `self_ty : Trait<input_tys>`
        let args = GenericArgs::for_item(
            self.interner(),
            trait_def_id.into(),
            |param_idx, param_id, _, _| match param_id {
                GenericParamId::LifetimeParamId(_) | GenericParamId::ConstParamId(_) => {
                    unreachable!("did not expect operator trait to have lifetime/const")
                }
                GenericParamId::TypeParamId(_) => {
                    if param_idx == 0 {
                        self_ty.into()
                    } else if let Some(rhs_ty) = opt_rhs_ty {
                        assert_eq!(param_idx, 1, "did not expect >1 param on operator trait");
                        rhs_ty.into()
                    } else {
                        // FIXME: We should stop passing `None` for the failure case
                        // when probing for call exprs. I.e. `opt_rhs_ty` should always
                        // be set when it needs to be.
                        self.var_for_def(param_id, cause.span())
                    }
                }
            },
        );

        let obligation = Obligation::new(
            self.interner(),
            cause,
            self.param_env,
            TraitRef::new_from_args(self.interner(), trait_def_id.into(), args),
        );

        // Now we want to know if this can be matched
        let matches_trait = match treat_opaques {
            TreatNotYetDefinedOpaques::AsInfer => self.infer_ctxt.predicate_may_hold(&obligation),
            TreatNotYetDefinedOpaques::AsRigid => {
                self.infer_ctxt.predicate_may_hold_opaque_types_jank(&obligation)
            }
        };

        if !matches_trait {
            debug!("--> Cannot match obligation");
            // Cannot be matched, no such method resolution is possible.
            return None;
        }

        // Trait must have a method named `m_name` and it should not have
        // type parameters or early-bound regions.
        let interner = self.interner();

        let def_id = method_item;

        debug!("lookup_in_trait_adjusted: method_item={:?}", method_item);
        let mut obligations = PredicateObligations::new();

        // Instantiate late-bound regions and instantiate the trait
        // parameters into the method type to get the actual method type.
        //
        // N.B., instantiate late-bound regions before normalizing the
        // function signature so that normalization does not need to deal
        // with bound regions.
        let fn_sig = self
            .db
            .callable_item_signature(method_item.into())
            .instantiate(interner, args)
            .skip_norm_wip();
        let fn_sig = self.infer_ctxt.instantiate_binder_with_fresh_vars(
            cause.span(),
            BoundRegionConversionTime::FnCall,
            fn_sig,
        );
        // An operator's output is an associated type of its trait (`Index::Output`): left an
        // alias, `a[i][j] += 1` sees no integer on its left and checks `AddAssign` on it.
        let (fn_sig, normalizing) =
            crate::next_solver::normalize::normalize(self.at(&obligation.cause), fn_sig);
        obligations.extend(normalizing);

        // Register obligations for the parameters. This will include the
        // `Self` parameter, which in turn has a bound of the main trait,
        // so this also effectively registers `obligation` as well. (We
        // used to register `obligation` explicitly, but that resulted in
        // double error messages being reported.)
        //
        // Note that as the method comes from a trait, it should not have
        // any late-bound regions appearing in its bounds.
        let bounds = GenericPredicates::query_all(self.db, method_item.into());
        let bounds = clauses_as_obligations(
            bounds.iter_instantiated(interner, args.as_slice()).map(Unnormalized::skip_norm_wip),
            cause,
            self.param_env,
        );

        obligations.extend(bounds);

        // Also add an obligation for the method type being well-formed.
        debug!(
            "lookup_method_in_trait: matched method fn_sig={:?} obligation={:?}",
            fn_sig, obligation
        );
        for ty in fn_sig.inputs_and_output {
            obligations.push(Obligation::new(
                interner,
                obligation.cause,
                self.param_env,
                Binder::dummy(PredicateKind::Clause(ClauseKind::WellFormed(ty.into()))),
            ));
        }

        let callee = MethodCallee { def_id, args, sig: fn_sig };
        debug!("callee = {:?}", callee);

        Some(InferOk { obligations, value: callee })
    }
}

pub fn lookup_impl_const<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
    const_id: ConstId,
    subs: GenericArgs<'db>,
) -> (ConstId, GenericArgs<'db>) {
    let interner = infcx.interner;
    let db = interner.db;

    let trait_id = match const_id.loc(db).container {
        ItemContainerId::TraitId(id) => id,
        _ => return (const_id, subs),
    };
    let trait_ref = TraitRef::new_from_args(interner, trait_id.into(), subs);

    let const_signature = ConstSignature::of(db, const_id);
    let name = match const_signature.name.as_ref() {
        Some(name) => name,
        None => return (const_id, subs),
    };

    match lookup_impl_assoc_item_for_trait_ref(infcx, trait_ref, env, name) {
        Ok((Either::Left(AssocItemId::ConstId(id)), s)) => (id, s),
        _ => (const_id, subs),
    }
}

/// Checks if the self parameter of `Trait` method is the `dyn Trait` and we should
/// call the method using the vtable.
pub fn is_dyn_method<'db>(
    interner: DbInterner<'db>,
    _env: ParamEnv<'db>,
    func: FunctionId,
    fn_subst: GenericArgs<'db>,
) -> Option<usize> {
    let db = interner.db;

    let ItemContainerId::TraitId(trait_id) = func.loc(db).container else {
        return None;
    };
    let trait_params = GenericParams::of(db, trait_id.into()).len();
    let fn_params = fn_subst.len() - trait_params;
    let trait_ref = TraitRef::new_from_args(
        interner,
        trait_id.into(),
        GenericArgs::new_from_slice(&fn_subst[..trait_params]),
    );
    let self_ty = trait_ref.self_ty();
    if let TyKind::Dynamic(d, _) = self_ty.kind() {
        // rustc doesn't accept `impl Foo<2> for dyn Foo<5>`, so if the trait id is equal, no matter
        // what the generics are, we are sure that the method is come from the vtable.
        let is_my_trait_in_bounds = d
            .principal_def_id()
            .is_some_and(|trait_| all_super_traits(db, trait_.0).contains(&trait_id));
        if is_my_trait_in_bounds {
            return Some(fn_params);
        }
    }
    None
}

/// What [`lookup_impl_method_query`] returns, in a form a query can keep.
pub(crate) type StoredImplMethod = Result<
    (Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, StoredGenericArgs),
    NoImplMethod,
>;

/// [`lookup_impl_method_query`], memoized: codegen asks the same trait method with the same
/// arguments many times per build and again on every edit, and each ask re-runs trait
/// selection in a fresh inference context.
#[blaze_query::tracked(cycle_result = lookup_impl_method_cycle_result, returns(clone), mem_tag = Infer)]
pub(crate) fn lookup_impl_method_stored(
    db: &Db,
    env: crate::traits::StoredParamEnvAndCrate,
    func: FunctionId,
    fn_subst: StoredGenericArgs,
) -> StoredImplMethod {
    lookup_impl_method_query(db, env.as_ref(db), func, fn_subst.as_ref())
        .map(|(found, args)| (found, args.store()))
}

fn lookup_impl_method_cycle_result(
    _: &Db,
    _: blaze_query::db::Id,
    _env: crate::traits::StoredParamEnvAndCrate,
    _func: FunctionId,
    _fn_subst: StoredGenericArgs,
) -> StoredImplMethod {
    Err(NoImplMethod::Cycle)
}

/// Looks up the impl method that actually runs for the trait method `func`.
///
/// Returns `func` if it's not a method defined in a trait, and why not where the lookup fails.
pub(crate) fn lookup_impl_method_query<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    func: FunctionId,
    fn_subst: GenericArgs<'db>,
) -> Result<
    (Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>),
    NoImplMethod,
> {
    let interner = DbInterner::new_with(db, env.krate);
    let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);

    let ItemContainerId::TraitId(trait_id) = func.loc(db).container else {
        return Ok((Either::Left(func), fn_subst));
    };
    let trait_params = GenericParams::of(db, trait_id.into()).len();
    let trait_ref = TraitRef::new_from_args(
        interner,
        trait_id.into(),
        GenericArgs::new_from_slice(&fn_subst[..trait_params]),
    );

    let name = &FunctionSignature::of(db, func).name;
    let (assoc, impl_subst) =
        lookup_impl_assoc_item_for_trait_ref(&infcx, trait_ref, env.param_env, name)?;
    let impl_fn = match assoc {
        Either::Left(AssocItemId::FunctionId(id)) => Either::Left(id),
        Either::Right(it) => Either::Right(it),
        Either::Left(_) => return Err(NoImplMethod::NoItem),
    };

    Ok((
        impl_fn,
        GenericArgs::new_from_iter(impl_subst.iter().chain(fn_subst.iter().skip(trait_params))),
    ))
}

fn lookup_impl_assoc_item_for_trait_ref<'db>(
    infcx: &InferCtxt<'db>,
    trait_ref: TraitRef<'db>,
    env: ParamEnv<'db>,
    name: &Name,
) -> Result<
    (Either<AssocItemId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>),
    NoImplMethod,
> {
    let (impl_id, impl_subst) = find_matching_impl(infcx, env, trait_ref)?;
    let impl_id = match impl_id {
        AnyImplId::ImplId(it) => it,
        AnyImplId::BuiltinDeriveImplId(impl_) => {
            // A derive writes the methods it must; `PartialEq::ne` and the like are the trait's.
            return match impl_.loc(infcx.interner.db).trait_.get_method(name.symbol()) {
                Some(method) => Ok((Either::Right((impl_, method)), impl_subst)),
                None => provided(infcx.interner.db, trait_ref, name),
            };
        }
    };
    let db = infcx.interner.db;
    let item_of = |impl_id: ImplId| {
        // clone: the tracked query's key; a `Name` is an interned symbol, so a count bump
        impl_value_item(db, impl_id, name.clone()).map(|it| match it {
            CandidateId::FunctionId(f) => AssocItemId::FunctionId(f),
            CandidateId::ConstId(c) => AssocItemId::ConstId(c),
        })
    };
    if let Some(item) = item_of(impl_id) {
        return Ok((Either::Left(item), impl_subst));
    }
    // A specializing impl takes the items it leaves out from the impl it specializes: the
    // closest one that has the item. One that none has is the trait's default, as rustc's
    // `leaf_def` finds it.
    let Some(parent) =
        crate::specialization::specialized_ancestor(db, impl_id, |it| item_of(it).is_some())
    else {
        return provided(db, trait_ref, name);
    };
    let parent_args = infcx.fresh_args_for_item(Span::Dummy, parent.into());
    let parent_trait_ref = db
        .impl_trait(parent)
        .ok_or(NoImplMethod::NoItem)?
        .instantiate(infcx.interner, parent_args)
        .skip_norm_wip();
    let mut ocx = ObligationCtxt::new(infcx);
    ocx.eq(&ObligationCause::dummy(), env, trait_ref, parent_trait_ref)
        .map_err(|_| NoImplMethod::Unimplemented)?;
    let parent_args = infcx.deeply_resolve_ignoring_regions(parent_args);
    if parent_args.has_non_region_infer() {
        return Err(NoImplMethod::Ambiguous);
    }
    let parent_args = parent_args.fold_with(&mut ReplaceInferWithError::new(infcx.interner));
    Ok((Either::Left(item_of(parent).ok_or(NoImplMethod::NoItem)?), parent_args))
}

/// The trait's own item named `name`, with the trait ref's arguments, where it has a default:
/// a provided method's body or an associated constant's value. What an impl that leaves the
/// item out runs, `Write::write_fmt` of `Stderr`.
fn provided<'db>(
    db: &'db Db,
    trait_ref: TraitRef<'db>,
    name: &Name,
) -> Result<
    (Either<AssocItemId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>),
    NoImplMethod,
> {
    // clone: the tracked query's key; a `Name` is an interned symbol, so a count bump
    let item = match trait_value_item(db, trait_ref.def_id.0, name.clone()) {
        Some(CandidateId::FunctionId(f)) if FunctionSignature::of(db, f).has_body() => {
            AssocItemId::FunctionId(f)
        }
        Some(CandidateId::ConstId(c)) if ConstSignature::of(db, c).has_body() => {
            AssocItemId::ConstId(c)
        }
        _ => return Err(NoImplMethod::NoItem),
    };
    Ok((Either::Left(item), trait_ref.args))
}

/// The function or constant of `impl_` named `name`, the first if there are more: all that a
/// lookup of `name` reads of the impl, so an item of another name added to it leaves the
/// lookup as it was.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn impl_value_item(db: &Db, impl_: ImplId, name: Name) -> Option<CandidateId> {
    value_item_named(&impl_.impl_items(db).items, &name)
}

/// As [`impl_value_item`], of a trait.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn trait_value_item(
    db: &Db,
    trait_: TraitId,
    name: Name,
) -> Option<CandidateId> {
    value_item_named(&trait_.trait_items(db).items, &name)
}

/// For every trait declared in a module of a frozen `krate` (not in a block), what
/// [`trait_value_item`] answers for each name its items have, sorted by name and then trait.
/// A maintained crate's are its candidate slices ([`delta::candidate_traits`]).
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
fn trait_value_items(db: &Db, krate: Crate) -> Box<[(Name, TraitId, CandidateId)]> {
    blaze_mem::frame::current(|frame| {
        // Each with where it was found: of a trait's items of one name the first stays first,
        // as `value_item_named` takes it.
        let mut items: Vec<(Name, TraitId, usize, CandidateId), &Frame> = Vec::new_in(frame);
        for (_, module) in crate_def_map(db, krate).modules(db) {
            for def in module.scope(db).declarations() {
                let ModuleDefId::TraitId(trait_) = def else { continue };
                for &(name, item) in trait_.trait_items(db).items.iter() {
                    let item = match item {
                        AssocItemId::FunctionId(it) => CandidateId::from(it),
                        AssocItemId::ConstId(it) => CandidateId::from(it),
                        AssocItemId::TypeAliasId(_) => continue,
                    };
                    let at = items.len();
                    items.push((name, trait_, at, item));
                }
            }
        }
        items.sort_unstable_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
        items.dedup_by(|later, first| (later.0, later.1) == (first.0, first.1));
        // alloc: the memo's result, one exact block
        items.iter().map(|&(name, trait_, _, item)| (name, trait_, item)).collect()
    })
}

/// What [`trait_value_item`] answers for `name` for each trait declared in a module of `krate`
/// (not in a block), sorted by trait: the traits of the name's candidate slice, so an item of
/// another name added to a trait of `krate` leaves it as it was. A frozen crate's are the run
/// of [`trait_value_items`] named `name`.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
pub fn in_crate_trait_value_items(
    db: &Db,
    krate: Crate,
    name: Name,
) -> Box<[(TraitId, CandidateId)]> {
    if !frozen::served(db, krate) {
        let traits = delta::candidate_traits(db, krate, name);
        return traits
            .iter()
            .filter_map(|&trait_| Some((trait_, trait_value_item(db, trait_, name)?)))
            // alloc: the memo's result, grown to at most the slice's length (none when empty)
            .collect();
    }
    let items = trait_value_items(db, krate);
    let start = items.partition_point(|&(it, _, _)| it < name);
    let end = items.partition_point(|&(it, _, _)| it <= name);
    // alloc: the memo's result, one exact block (none when empty)
    items[start..end].iter().map(|&(_, t, i)| (t, i)).collect()
}

/// [`trait_value_item`] for each trait a probe asks of, through one
/// [`in_crate_trait_value_items`] per crate and a binary search per trait. A trait in a block is
/// asked of alone.
#[derive(Debug)]
pub struct TraitValueItems<'db, 'f> {
    runs: Vec<(Crate, &'db [(TraitId, CandidateId)]), &'f Frame>,
}

impl<'db, 'f> TraitValueItems<'db, 'f> {
    pub fn new_in(frame: &'f Frame) -> Self {
        TraitValueItems { runs: Vec::new_in(frame) }
    }

    pub fn get(
        &mut self,
        db: &'db Db,
        trait_: TraitId,
        name: Name,
    ) -> Option<CandidateId> {
        let module = trait_.module(db);
        if module.block(db).is_some() {
            return trait_value_item(db, trait_, name);
        }
        let krate = module.krate(db);
        let run = match self.runs.iter().find(|&&(it, _)| it == krate) {
            Some(&(_, run)) => run,
            None => {
                let run = in_crate_trait_value_items(db, krate, name);
                self.runs.push((krate, run));
                run
            }
        };
        run.binary_search_by_key(&trait_, |&(it, _)| it).ok().map(|i| run[i].1)
    }
}

fn value_item_named(items: &[(Name, AssocItemId)], name: &Name) -> Option<CandidateId> {
    items.iter().find_map(|(item_name, item)| match *item {
        AssocItemId::FunctionId(it) if item_name == name => Some(it.into()),
        AssocItemId::ConstId(it) if item_name == name => Some(it.into()),
        _ => None,
    })
}

/// Why a trait method's call names no impl's method ([`HirDatabase::lookup_impl_method_or_why`]):
/// it then calls the trait's own, which is right where the impl is chosen later or is the
/// compiler's, and an error where the call is concrete and no impl holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub enum NoImplMethod {
    /// The trait's arguments hold a type or const parameter: the impl is chosen once they are
    /// known.
    Generic,
    /// A where clause of the environment proves the trait ref.
    Param,
    /// The compiler's own impl proves it: a closure's `Fn*`, `dyn Trait`'s vtable, `Sized`,
    /// `Copy` of a tuple and the like.
    Builtin,
    /// Trait selection proves that no impl holds, or the nested obligations of the one it
    /// picks fail.
    Unimplemented,
    /// More than one impl may hold, or the one picked leaves its arguments open.
    Ambiguous,
    /// The trait ref's projections do not normalize.
    Unnormalized,
    /// The impl found has no method of the name, nor does an impl it specializes, and the
    /// trait's has no default body.
    NoItem,
    /// The lookup asks itself again.
    Cycle,
}

impl NoImplMethod {
    /// Whether the call names the trait's own method rightly: its impl is chosen later or is
    /// the compiler's.
    pub fn is_expected(self) -> bool {
        matches!(self, NoImplMethod::Generic | NoImplMethod::Param | NoImplMethod::Builtin)
    }

    pub fn describe(self) -> &'static str {
        match self {
            NoImplMethod::Generic => "its arguments are still generic",
            NoImplMethod::Param => "a where clause proves it",
            NoImplMethod::Builtin => "the compiler's built-in impl proves it",
            NoImplMethod::Unimplemented => "no impl holds",
            NoImplMethod::Ambiguous => "more than one impl may hold",
            NoImplMethod::Unnormalized => "its projections do not normalize",
            NoImplMethod::NoItem => {
                "the impl found has no method of that name and the trait's has no default body"
            }
            NoImplMethod::Cycle => "the lookup depends on itself",
        }
    }
}

pub(crate) fn find_matching_impl<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
    trait_ref: TraitRef<'db>,
) -> Result<(AnyImplId, GenericArgs<'db>), NoImplMethod> {
    let generic = trait_ref.has_non_region_param();
    let open = |why| if generic { NoImplMethod::Generic } else { why };
    let trait_ref = infcx
        .at(&ObligationCause::dummy(), env)
        .deeply_normalize(trait_ref)
        .map_err(|_| open(NoImplMethod::Unnormalized))?;

    let obligation = Obligation::new(infcx.interner, ObligationCause::dummy(), env, trait_ref);

    let selection = infcx
        .select(&obligation)
        .map_err(|_| open(NoImplMethod::Unimplemented))?
        .ok_or_else(|| open(NoImplMethod::Ambiguous))?;

    // Currently, we use a fulfillment context to completely resolve
    // all nested obligations. This is because they can inform the
    // inference of the impl's type parameters.
    let mut ocx = ObligationCtxt::new(infcx);
    let impl_source = selection.map(|obligation| ocx.register_obligation(obligation));

    let errors = ocx.evaluate_obligations_error_on_ambiguity();
    if !errors.is_empty() {
        return Err(open(NoImplMethod::Unimplemented));
    }

    let impl_source = infcx.deeply_resolve_ignoring_regions(impl_source);
    if impl_source.has_non_region_infer() {
        return Err(open(NoImplMethod::Ambiguous));
    }

    // Selection may leave region inference variables unresolved; replace them before they escape
    // this inference context.
    //
    // FIXME: decide whether inferred regions should be replaced with error or erased.
    match impl_source {
        ImplSource::UserDefined(impl_source) => Ok((
            impl_source.impl_def_id,
            impl_source.args.fold_with(&mut ReplaceInferWithError::new(infcx.interner)),
        )),
        ImplSource::Param(_) => Err(NoImplMethod::Param),
        ImplSource::Builtin(..) => Err(NoImplMethod::Builtin),
    }
}

/// The crates rustc loads to compile `krate`, and so the ones whose impls it knows of: `krate`,
/// the dependencies it names and what those load in turn. A dependency that is never named
/// is not loaded, and with it go the impls that leave a type open: `assert_eq!(1.5, s.parse()
/// .unwrap())` parses an `f64` until serde_json's `impl PartialEq<Value> for f64` is seen, and
/// a test of a crate whose benches use criterion depends on serde_json.
///
/// Named is by the identifiers of `krate`'s files and of what the macros of other crates
/// expand to among its items, which is more than rustc resolves (a local of the same name
/// does) and less where such a macro in a body alone names the dependency.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
pub fn loaded_crates(db: &Db, krate: Crate) -> Box<[Crate]> {
    let _p = tracing::info_span!("loaded_crates").entered();
    blaze_mem::frame::current(|frame| {
        let data = krate.data(db);
        let mut unnamed = Vec::new_in(frame);
        unnamed.extend(data.dependencies.iter().filter(|dep| {
            let data = dep.crate_id.data(db);
            !(dep.is_sysroot() || data.origin.is_lang() || data.target_kind.is_proc_macro())
        }));
        // Few files name a dependency, and a file's text tells which may without its tree. A
        // file `include!` puts among the items is one of the crate's as well: a root that is
        // only `include!("../../app/main.rs");` names its dependencies there.
        let included = hir_def::include_macro_invoc(db, krate);
        let def_map = crate_def_map(db, krate);
        let mut files = def_map
            .modules(db)
            .filter_map(|(_, module)| module.origin.file_id())
            .chain(included.iter().map(|&(_, file)| file));
        // Few dependencies are left unnamed: each identifier is compared with them, and the walk
        // ends when none is left.
        fn named_by<'a>(
            unnamed: &mut Vec<&base_db::BuiltDependency, &Frame>,
            mut identifiers: impl Iterator<Item = &'a str>,
        ) {
            while !unnamed.is_empty()
                && let Some(text) = identifiers.next()
            {
                unnamed.retain(|dep| dep.name.symbol().as_str() != text);
            }
        }
        let named_in = |unnamed: &mut Vec<&base_db::BuiltDependency, &Frame>,
                        root: syntax::view::Node<'_>| {
            let (tree, at) = root.flat();
            named_by(
                unnamed,
                (at..tree.end(at))
                    .filter(|&it| tree.is_token(it) && tree.kind(it).0 == SyntaxKind::IDENT as u16)
                    .map(|it| tree.token_text(it)),
            );
        };
        // A file is lexed, not walked: the tree of a library file is an outline, whose `fn`
        // bodies are not in it until walked into, and a dependency named in a body alone is
        // loaded all the same (a library's `memchr::memchr(..)` in a function).
        while !unnamed.is_empty()
            && let Some(file) = files.next()
        {
            let text = db.file_text(file.file_id(db)).text(db);
            if !unnamed.iter().any(|dep| text.contains(dep.name.symbol().as_str())) {
                continue;
            }
            let lexed = syntax::LexedStr::new(file.edition(db), text);
            named_by(
                &mut unnamed,
                (0..lexed.len())
                    .filter(|&i| lexed.kind(i) == SyntaxKind::IDENT)
                    .map(|i| lexed.text(i)),
            );
        }
        // rustc loads the crate a path in a macro's output names too, as in the `impl
        // dep::Trait for ..` a derive of a proc macro writes. A `macro_rules!` of `krate`
        // writes nothing its files do not, nor does a built-in macro but `include!`, whose
        // files are above.
        'modules: for (_, module) in def_map.modules(db) {
            for call in module.scope(db).all_macro_calls() {
                if unnamed.is_empty() {
                    break 'modules;
                }
                let def = call.loc(db).def;
                if def.krate == krate
                    || !matches!(
                        def.kind,
                        hir_expand::MacroDefKind::ProcMacro(..)
                            | hir_expand::MacroDefKind::Declarative(..)
                    )
                {
                    continue;
                }
                named_in(&mut unnamed, call.parse_macro_expansion(db).value.0.root());
            }
        }
        let mut loaded = crate::frame_set(frame);
        let mut ordered = Vec::new_in(frame);
        ordered.push(krate);
        loaded.insert(krate);
        for dep in &data.dependencies {
            if unnamed.iter().any(|it| std::ptr::eq(*it, dep)) {
                continue;
            }
            // The same of the dependency: the crates of its tests and benches are among its own.
            for &dep in loaded_crates(db, dep.crate_id) {
                if loaded.insert(dep) {
                    ordered.push(dep);
                }
            }
        }
        // alloc: the memo's result, one exact block
        ordered.iter().copied().collect()
    })
}

/// For each crate `krate`'s [`loaded_crates`] depend on, which of those can name its items: a
/// crate that is it or depends on it, directly or not. An impl names its trait and the type it
/// is for, so only a crate that can name the crates of both holds one: the walks over the
/// loaded crates for one trait ask those alone, one AND of two rows, where each crate asked is
/// a memo read and, cold, a table built.
#[derive(Debug, PartialEq, Eq, blaze_query::Value)]
pub struct CratesThatName {
    /// The loaded crates and what they depend on, sorted: row `r` is `crates[r]`'s.
    crates: Box<[Crate]>,
    row_words: u32,
    /// Row `r`: a bit per place in [`loaded_crates`], set where the crate there can name
    /// `crates[r]`.
    rows: Box<[u64]>,
}

impl CratesThatName {
    fn row(&self, krate: Crate) -> Option<&[u64]> {
        let r = self.crates.binary_search(&krate).ok()?;
        let words = self.row_words as usize;
        Some(&self.rows[r * words..(r + 1) * words])
    }

    /// The places in [`loaded_crates`], ascending, of the crates that can name both
    /// `trait_crate` and, given, `self_crate`.
    fn holders<'f>(&self, frame: &'f Frame, trait_crate: Crate, self_crate: Option<Crate>) -> Ones<'f> {
        let Some(row) = self.row(trait_crate) else { return Ones::new(&[]) };
        let holders = frame.alloc_slice_copy(row);
        if let Some(self_crate) = self_crate {
            match self.row(self_crate) {
                Some(row) => holders.iter_mut().zip(row).for_each(|(it, row)| *it &= row),
                None => holders.fill(0),
            }
        }
        Ones::new(holders)
    }
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn crates_that_name(db: &Db, krate: Crate) -> CratesThatName {
    let _p = tracing::info_span!("crates_that_name").entered();
    blaze_mem::frame::current(|frame| {
        let loaded = loaded_crates(db, krate);
        // Every crate after the ones it depends on, the graph being acyclic: a walk from each
        // loaded crate, placing a crate when its last dependency is placed.
        let mut place = crate::frame_map::<Crate, u32>(frame);
        let mut seen = crate::frame_set(frame);
        let mut order = Vec::new_in(frame);
        let mut stack = Vec::new_in(frame);
        for &root in loaded {
            if !seen.insert(root) {
                continue;
            }
            stack.push((root, 0u32));
            while let Some(top) = stack.len().checked_sub(1) {
                let (at, next) = stack[top];
                match at.data(db).dependencies.get(next as usize) {
                    Some(dep) => {
                        stack[top].1 += 1;
                        if seen.insert(dep.crate_id) {
                            stack.push((dep.crate_id, 0));
                        }
                    }
                    None => {
                        stack.pop();
                        place.insert(at, order.len() as u32);
                        order.push(at);
                    }
                }
            }
        }
        let n = order.len();
        // Row `p`: `order[p]` and what it depends on, by place in `order`.
        let mut reach: BitMatrix<usize, &mut [u64]> =
            BitMatrix::from_words(n, n, frame.alloc_slice_fill_with(n * words_for(n), |_| 0u64));
        for (p, &at) in order.iter().enumerate() {
            reach.insert(p, p);
            for dep in &at.data(db).dependencies {
                reach.union_rows(p, place[&dep.crate_id] as usize);
            }
        }
        let mut sorted = Vec::with_capacity_in(n, frame);
        sorted.extend(order.iter().enumerate().map(|(p, &it)| (it, p as u32)));
        sorted.sort_unstable();
        let rank = frame.alloc_slice_fill_with(n, |_| 0u32);
        for (r, &(_, p)) in sorted.iter().enumerate() {
            rank[p as usize] = r as u32;
        }
        let row_words = words_for(loaded.len());
        // alloc: the memo's rows, once at their size
        let mut rows = vec![0u64; n * row_words].into_boxed_slice();
        for (at, dep) in loaded.iter().enumerate() {
            for p in reach.iter_row(place[dep] as usize) {
                rows[rank[p] as usize * row_words + at / 64] |= 1 << (at % 64);
            }
        }
        CratesThatName {
            // alloc: the memo's crates, once at their size
            crates: sorted.iter().map(|&(it, _)| it).collect(),
            row_words: row_words as u32,
            rows,
        }
    })
}

/// The items of `scope` whose bodies may hold impls: an impl is one of its crate wherever it
/// is written. Custom derives write theirs in a const, `const _: () = { ... };` or one with a
/// name, and those are looked into in every crate. A function can hold one as well, of a type
/// that is not its own (zerocopy's tests `impl KnownLayout for elain::Align<8>` in a test).
/// Lowering every body for this is too much, and puts every body's lowering before the first
/// trait query, so a function, static or associated constant is looked into unless lexing its
/// text proves it holds none ([`may_hold`]): bincode's `deserialize_enum` holds its
/// `EnumAccess` for `&mut Deserializer`, and a `#[derive]` on a struct in a body or a
/// procedural macro called there writes one with no `impl` in the text. A module's constants
/// are all looked into: lexing finds no initializer after `const _`.
fn impl_holders<'f>(
    db: &Db,
    frame: &'f Frame,
    krate: Crate,
    scope: &ItemScope,
) -> Vec<DefWithBodyId, &'f Frame> {
    let mut holders = Vec::new_in(frame);
    holders.extend(scope.consts().map(DefWithBodyId::from));
    let declared = scope.declarations().filter_map(|def| match def {
        ModuleDefId::FunctionId(it) if may_hold(db, krate, it.loc(db).id, true) => {
            Some(DefWithBodyId::from(it))
        }
        ModuleDefId::StaticId(it) if may_hold(db, krate, it.loc(db).id, true) => {
            Some(DefWithBodyId::from(it))
        }
        _ => None,
    });
    holders.extend(declared);
    let item = |item: &AssocItemId| match *item {
        AssocItemId::FunctionId(it) if may_hold(db, krate, it.loc(db).id, true) => {
            Some(DefWithBodyId::from(it))
        }
        AssocItemId::ConstId(it) if may_hold(db, krate, it.loc(db).id, true) => {
            Some(DefWithBodyId::from(it))
        }
        _ => None,
    };
    let in_impls = scope.impls().flat_map(|it| &it.impl_items(db).items);
    holders.extend(in_impls.filter_map(|(_, it)| item(it)));
    // An impl in a trait's default method or constant is as global as any other
    // (macro_rules_attribute's `impl<T: IntoIterator> CollectVec for T {}` in `CollectVec::vec`).
    let traits = scope.declarations().filter_map(|def| match def {
        ModuleDefId::TraitId(it) => Some(it),
        _ => None,
    });
    let in_traits = traits.flat_map(|it| &it.trait_items(db).items);
    holders.extend(in_traits.filter_map(|(_, it)| item(it)));
    holders
}

/// Built-in attributes that write no item, so one in a body leaves it without an impl. Any
/// other (`derive`, `cfg_attr`, a tool's, a procedural macro's) may write one.
const INERT_ATTRIBUTES: &[&str] = &[
    "allow",
    "warn",
    "deny",
    "forbid",
    "expect",
    "doc",
    "inline",
    "cold",
    "must_use",
    "track_caller",
    "cfg",
];

blaze_phf::phf_table!(static INERT_ATTRIBUTE = INERT_ATTRIBUTES;);

fn attribute_is_inert(name: &str) -> bool {
    INERT_ATTRIBUTE.get(name).is_some()
}

/// What lexing a body found that may write an impl there.
#[derive(Debug, Clone, PartialEq, Eq, blaze_query::Value)]
enum BodyMark {
    /// An `impl` token.
    Impl,
    /// A macro call, by the name before its `!`.
    Call(Symbol),
    /// An attribute, `#[..]` or `#![..]`, by its first identifier (empty when it has none).
    Attribute(Symbol),
}

/// The `macro_rules!` of a file ([`macro_definitions`]): each one's name, whether it may write
/// an impl, and the end of its run in `calls`, the names of the other macros it calls.
#[derive(Debug, Default, PartialEq, Eq, blaze_query::Value)]
struct MacroDefinitions {
    defs: Box<[(Symbol, bool, u32)]>,
    calls: Box<[Symbol]>,
}

impl MacroDefinitions {
    fn iter(&self) -> impl Iterator<Item = (&Symbol, bool, &[Symbol])> + '_ {
        self.defs.iter().scan(0, |start, (name, writes, end)| {
            let calls = &self.calls[*start as usize..*end as usize];
            *start = *end;
            Some((name, *writes, calls))
        })
    }
}

/// Whether the body of `id` may hold an impl (or, without `impls`, an item a macro or an
/// attribute writes): its text has an `impl`, a call of a macro not proven to write none
/// ([`macro_is_inert`]) or an attribute that is not inert. An item a macro writes has no text
/// of its own and may. The file is parsed for the item's range only when lexing it found one.
pub(crate) fn may_hold<N: syntax::AstNode>(
    db: &Db,
    krate: Crate,
    id: hir_expand::AstId<N>,
    impls: bool,
) -> bool {
    let Some(file) = id.file_id.file_id() else { return true };
    let marks = impls_in_bodies(db, file);
    if marks.is_empty() {
        return false;
    }
    let range = id.file_id.ast_id_map(db).get(id.value).text_range();
    marks.iter().any(|(at, mark)| {
        range.contains(*at)
            && match mark {
                BodyMark::Impl => impls,
                BodyMark::Call(name) => !macro_is_inert(db, krate, name),
                BodyMark::Attribute(name) => !attribute_is_inert(name.as_str()),
            }
    })
}

/// Whether a macro called `name` in a body of `krate` writes no impl: some `macro_rules!` of
/// `krate` or its dependencies has the name, none of that name may write one, and no
/// procedural macro of a dependency has it ([`macros_writing_impls`]). `macro_rules!` itself
/// defines and writes nothing; `include!` writes what its file has.
fn macro_is_inert(db: &Db, krate: Crate, name: &Symbol) -> bool {
    if *name == sym::macro_rules {
        return true;
    }
    if *name == sym::include {
        return false;
    }
    let known = macros_writing_impls(db, krate);
    matches!(known.binary_search_by(|(it, _)| it.cmp_order(name)), Ok(i) if !known[i].1)
}

/// Whether `BLAZE_TRACE_HOLDERS=1` asks for [`trace_holders`]; read once per process.
pub(crate) fn tracing_holders() -> bool {
    static TRACE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var_os("BLAZE_TRACE_HOLDERS").is_some_and(|it| it == "1")
    });
    *TRACE
}

/// Writes to stderr, one line per holder of `module` ([`impl_holders`]), why it makes the
/// maintainer lower the module's bodies: `blaze-holders`, the crate, the holder's file and
/// offset, its kind (`const`, `fn`, `static`) and the reason, tab-separated. The reason is
/// `impl`, `attr NAME`, `call NAME RESOLVED` (see [`macro_resolution`]), `macro-written` (the
/// item has no text of its own) or `unconditional` (a constant whose text has no mark).
pub(crate) fn trace_holders(db: &Db, module: ModuleId, holders: &[DefWithBodyId]) {
    use std::io::Write as _;
    let krate = module.krate(db);
    let crate_name = crate_name(db, krate);
    let mut out = String::new();
    for holder in holders {
        let (kind, (place, reason)) = match *holder {
            DefWithBodyId::FunctionId(it) => ("fn", why_held(db, krate, it.loc(db).id)),
            DefWithBodyId::StaticId(it) => ("static", why_held(db, krate, it.loc(db).id)),
            DefWithBodyId::ConstId(it) => ("const", why_held(db, krate, it.loc(db).id)),
            // alloc: a trace line, only under BLAZE_TRACE_HOLDERS
            DefWithBodyId::VariantId(it) => ("variant", (format!("{it:?}"), String::new())),
        };
        // alloc: as above
        out.push_str(&format!("blaze-holders\t{crate_name}\t{place}\t{kind}\t{reason}\n"));
    }
    // A failed write to stderr loses a trace line and nothing else.
    _ = std::io::stderr().lock().write_all(out.as_bytes());
}

fn crate_name(db: &Db, krate: Crate) -> String {
    let name = krate.extra_data(db).display_name.as_ref();
    // alloc: a trace line's crate, only under BLAZE_TRACE_HOLDERS
    name.map_or_else(|| format!("{krate:?}"), |it| it.to_string())
}

/// Where the item of `id` is (`path@offset`) and the first mark in its text that made
/// [`may_hold`] count it, as [`trace_holders`] writes them.
fn why_held<N: syntax::AstNode>(
    db: &Db,
    krate: Crate,
    id: hir_expand::AstId<N>,
) -> (String, String) {
    let Some(file) = id.file_id.file_id() else {
        // alloc: a trace line's place and reason, only under BLAZE_TRACE_HOLDERS
        return (format!("{:?}", id.file_id), "macro-written".to_owned());
    };
    let range = id.file_id.ast_id_map(db).get(id.value).text_range();
    let path = db.path_for_file(file.file_id(db));
    // alloc: as above
    let path = path.map_or_else(|| format!("{file:?}"), |it| it.to_string());
    // alloc: as above
    let place = format!("{path}@{}", u32::from(range.start()));
    let reason = impls_in_bodies(db, file).iter().filter(|(at, _)| range.contains(*at)).find_map(
        |(_, mark)| match mark {
            // alloc: as above
            BodyMark::Impl => Some("impl".to_owned()),
            BodyMark::Call(name) if !macro_is_inert(db, krate, name) => {
                // alloc: as above
                Some(format!("call {} {}", name.as_str(), macro_resolution(db, krate, name)))
            }
            BodyMark::Attribute(name) if !attribute_is_inert(name.as_str()) => {
                // alloc: as above
                Some(format!("attr {}", name.as_str()))
            }
            _ => None,
        },
    );
    // alloc: as above
    (place, reason.unwrap_or_else(|| "unconditional".to_owned()))
}

/// Why [`macro_is_inert`] says a macro called `name` in `krate` may write an impl: `include`,
/// `unknown` (no `macro_rules!` or procedural macro of `krate` or its dependencies has the
/// name), or `writes:` and every definition of the name that may write one, comma-separated:
/// `CRATE:macro_rules` (its transcribers do), `CRATE:macro_rules(calls)` (a macro it calls
/// may) or `CRATE:proc_macro`.
fn macro_resolution(db: &Db, krate: Crate, name: &Symbol) -> String {
    // Every allocation below is a trace line's, only under BLAZE_TRACE_HOLDERS.
    if *name == sym::include {
        // alloc: as above
        return "include".to_owned();
    }
    let known = macros_writing_impls(db, krate);
    if known.binary_search_by(|(it, _)| it.cmp_order(name)).is_err() {
        // alloc: as above
        return "unknown".to_owned();
    }
    let mut found = Vec::new();
    let mut seen = blaze_hash::HashSet::default();
    // alloc: as above
    let mut stack = vec![krate];
    while let Some(at) = stack.pop() {
        // alloc: as above
        if !seen.insert(at) {
            continue;
        }
        // alloc: as above
        stack.extend(at.data(db).dependencies.iter().map(|dep| dep.crate_id));
        let def_map = crate_def_map(db, at);
        if at.data(db).target_kind.is_proc_macro() {
            for (_, module) in def_map.modules(db) {
                for (macro_name, item) in module.scope(db).macros() {
                    if macro_name.symbol() == name && matches!(item.def, MacroId::ProcMacroId(_)) {
                        // alloc: as above
                        found.push(format!("{}:proc_macro", crate_name(db, at)));
                    }
                }
            }
        }
        let writes = macros_writing_impls(db, at);
        let Ok(i) = writes.binary_search_by(|(it, _)| it.cmp_order(name)) else { continue };
        if !writes[i].1 {
            continue;
        }
        let mut files = blaze_hash::HashSet::default();
        for (_, module) in def_map.modules(db) {
            let Some(file) = module.origin.file_id() else { continue };
            // alloc: as above
            if !files.insert(file) {
                continue;
            }
            for (defined, direct, _) in macro_definitions(db, file).iter() {
                if defined == name {
                    let how = if direct { "" } else { "(calls)" };
                    // alloc: as above
                    found.push(format!("{}:macro_rules{how}", crate_name(db, at)));
                }
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    // alloc: as above
    format!("writes:{}", found.join(","))
}

/// Where `file` may write an impl in a body, found by its tokens: each `impl`, macro call and
/// attribute inside the braces that follow `fn name`, or between the `=` and the `;` of a
/// `const` or `static`. An `impl Trait` type is found as well, which costs a lowering and
/// nothing else.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
fn impls_in_bodies(
    db: &Db,
    file: base_db::EditionedFileId,
) -> Box<[(TextSize, BodyMark)]> {
    blaze_mem::frame::current(|frame| {
        let text = db.file_text(file.file_id(db)).text(db);
        let lexed = syntax::LexedStr::new(file.edition(db), text);
        let mut tokens: Vec<usize, &Frame> = Vec::with_capacity_in(lexed.len(), frame);
        tokens.extend((0..lexed.len()).filter(|&i| !lexed.kind(i).is_trivia()));
        let kind = |at: usize| tokens.get(at).map_or(SyntaxKind::EOF, |&i| lexed.kind(i));
        let start = |at: usize| TextSize::from(lexed.text_start(tokens[at]) as u32);
        // For each open brace, whether it is a function's body.
        let mut braces = Vec::new_in(frame);
        let mut bodies = 0usize;
        // The parentheses and brackets open: a `;` in `-> [u8; 4]` does not end the signature.
        let mut nested = 0usize;
        let mut signature = None;
        // Every delimiter open, and the depths at which a `const` or `static` initializer is.
        let mut depth = 0usize;
        let mut constant = None;
        let mut initializers: Vec<usize, &Frame> = Vec::new_in(frame);
        let mut found = Vec::new_in(frame);
        for at in 0..tokens.len() {
            let previous = if at == 0 { SyntaxKind::EOF } else { kind(at - 1) };
            let in_body = bodies > 0 || !initializers.is_empty();
            match kind(at) {
                SyntaxKind::IDENT if previous == T![fn] => signature = Some(nested),
                SyntaxKind::IDENT if matches!(previous, T![const] | T![static] | T![mut]) => {
                    constant = Some(depth)
                }
                T![=] if constant == Some(depth) => {
                    constant = None;
                    initializers.push(depth);
                }
                T!['('] | T!['['] => {
                    nested += 1;
                    depth += 1;
                }
                T![')'] | T![']'] => {
                    nested = nested.saturating_sub(1);
                    depth = depth.saturating_sub(1);
                    initializers.retain(|&it| it <= depth);
                }
                T![;] => {
                    if signature == Some(nested) {
                        signature = None;
                    }
                    if constant == Some(depth) {
                        constant = None;
                    }
                    if initializers.last() == Some(&depth) {
                        initializers.pop();
                    }
                }
                T!['{'] => {
                    // A block inside brackets (`-> [u8; { N }]`) is no body. One after `<`,
                    // `,` or `=` may be a const argument (`-> impl Tr<{ N }>`): counted as a
                    // body, it leaves the brace after it one as well.
                    let body = signature == Some(nested);
                    if body && !matches!(previous, T![<] | T![,] | T![=]) {
                        signature = None;
                    }
                    bodies += usize::from(body);
                    braces.push(body);
                    depth += 1;
                }
                T!['}'] => {
                    bodies -= usize::from(braces.pop().unwrap_or(false));
                    depth = depth.saturating_sub(1);
                    initializers.retain(|&it| it <= depth);
                }
                _ => {}
            }
            if !in_body {
                continue;
            }
            match kind(at) {
                T![impl] => found.push((start(at), BodyMark::Impl)),
                T![!] if previous == SyntaxKind::IDENT => {
                    let name = Symbol::intern(lexed.text(tokens[at - 1]));
                    found.push((start(at - 1), BodyMark::Call(name)));
                }
                T![#] => {
                    let open = if kind(at + 1) == T![!] { at + 2 } else { at + 1 };
                    if kind(open) == T!['['] {
                        let name = match kind(open + 1) {
                            SyntaxKind::IDENT => lexed.text(tokens[open + 1]),
                            _ => "",
                        };
                        found.push((start(at), BodyMark::Attribute(Symbol::intern(name))));
                    }
                }
                _ => {}
            }
        }
        // alloc: the memo's result, one exact block (none when empty)
        found.drain(..).collect()
    })
}

/// Every `macro_rules!` name of `krate` and of its dependencies, and every procedural macro
/// of a dependency, with whether a macro of that name may write an impl, sorted by name. A
/// `macro_rules!` may when its transcribers have an `impl`, an attribute that is not inert or
/// one written from a fragment (`#[$meta]`), or a call of a macro that may: one passed in
/// (`$name!`), `include!`, a procedural one, one of no known definition, or one that may in
/// turn (dtoa's `dtoa!`, called in the body of `<f64 as Sealed>::write`, calls `diyfp!`, which
/// writes `impl DiyFp` there). Known by name, so a name that may costs a lowering and nothing
/// else.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
fn macros_writing_impls(db: &Db, krate: Crate) -> Box<[(Symbol, bool)]> {
    fn add<'a>(known: &mut FrameMap<'_, &'a Symbol, bool>, name: &'a Symbol, writes: bool) {
        *known.entry(name).or_default() |= writes;
    }
    blaze_mem::frame::current(|frame| {
        // Keyed by the names the queries below hold: a name is cloned once, into the result.
        let mut known = crate::frame_map(frame);
        for dep in &krate.data(db).dependencies {
            for (name, writes) in macros_writing_impls(db, dep.crate_id) {
                add(&mut known, name, *writes);
            }
            if dep.crate_id.data(db).target_kind.is_proc_macro() {
                for (_, module) in crate_def_map(db, dep.crate_id).modules(db) {
                    for (name, item) in module.scope(db).macros() {
                        if matches!(item.def, MacroId::ProcMacroId(_)) {
                            add(&mut known, name.symbol(), true);
                        }
                    }
                }
            }
        }
        let mut calling = Vec::new_in(frame);
        let mut files = crate::frame_set(frame);
        for (_, module) in crate_def_map(db, krate).modules(db) {
            let Some(file) = module.origin.file_id() else { continue };
            if !files.insert(file) {
                continue;
            }
            for (name, writes, calls) in macro_definitions(db, file).iter() {
                add(&mut known, name, writes);
                if !writes {
                    calling.push((name, calls));
                }
            }
        }
        let may_write = |known: &FrameMap<'_, &Symbol, bool>, call: &Symbol| {
            if *call == sym::macro_rules {
                false
            } else if *call == sym::include {
                true
            } else {
                known.get(call).copied().unwrap_or(true)
            }
        };
        loop {
            let before = calling.len();
            calling.retain(|&(name, calls)| {
                let writes = calls.iter().any(|it| may_write(&known, it));
                if writes {
                    known.insert(name, true);
                }
                !writes
            });
            if calling.len() == before {
                break;
            }
        }
        let mut sorted = Vec::with_capacity_in(known.len(), frame);
        sorted.extend(known.iter().map(|(&name, &writes)| (name, writes)));
        sorted.sort_unstable_by(|a: &(&Symbol, bool), b| a.0.cmp_order(b.0));
        // alloc: clone: the memo's result, one exact block holding each name once
        sorted.iter().map(|&(name, writes)| (name.clone(), writes)).collect()
    })
}

/// The `macro_rules!` of `file`, found by its tokens: each one's name, whether its
/// transcribers write an impl (an `impl` after `{`, `}`, `;`, `(`, `*`, an attribute or
/// `unsafe`), an attribute that is not inert or one from a fragment, or call a macro passed in
/// as `$name!`, and the names of the other macros they call.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn macro_definitions(db: &Db, file: base_db::EditionedFileId) -> MacroDefinitions {
    let text = db.file_text(file.file_id(db)).text(db);
    if !text.contains("macro_rules") {
        return MacroDefinitions::default();
    }
    blaze_mem::frame::current(|frame| {
        let lexed = syntax::LexedStr::new(file.edition(db), text);
        let mut tokens: Vec<usize, &Frame> = Vec::with_capacity_in(lexed.len(), frame);
        tokens.extend((0..lexed.len()).filter(|&i| !lexed.kind(i).is_trivia()));
        let kind = |at: usize| tokens.get(at).map_or(SyntaxKind::EOF, |&i| lexed.kind(i));
        let mut found = Vec::new_in(frame);
        let mut calls = Vec::new_in(frame);
        let mut at = 0;
        while at < tokens.len() {
            let definition = kind(at) == SyntaxKind::IDENT
                && lexed.text(tokens[at]) == "macro_rules"
                && kind(at + 1) == T![!]
                && kind(at + 2) == SyntaxKind::IDENT
                && matches!(kind(at + 3), T!['{'] | T!['('] | T!['[']);
            if !definition {
                at += 1;
                continue;
            }
            let name = Symbol::intern(lexed.text(tokens[at + 2]));
            // The rules alternate, a matcher then a transcriber, each a group at depth 1.
            let (mut writes, mut depth, mut groups) = (false, 0usize, 0usize);
            at += 3;
            while at < tokens.len() {
                let transcriber = depth >= 2 && groups % 2 == 0;
                match kind(at) {
                    T!['{'] | T!['('] | T!['['] => {
                        if depth == 1 {
                            groups += 1;
                        }
                        depth += 1;
                    }
                    T!['}'] | T![')'] | T![']'] => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    T![impl]
                        if transcriber
                            && matches!(
                                kind(at - 1),
                                T!['{'] | T!['}'] | T![;] | T!['('] | T![*] | T![']'] | T![unsafe]
                            ) =>
                    {
                        writes = true
                    }
                    T![#] if transcriber => {
                        let open = if kind(at + 1) == T![!] { at + 2 } else { at + 1 };
                        let inert = kind(open + 1) == SyntaxKind::IDENT
                            && attribute_is_inert(lexed.text(tokens[open + 1]));
                        if kind(open) == T!['['] && !inert {
                            writes = true;
                        }
                    }
                    T![!] if transcriber && kind(at - 1) == SyntaxKind::IDENT => {
                        if kind(at - 2) == T![$] {
                            writes = true;
                        } else {
                            calls.push(Symbol::intern(lexed.text(tokens[at - 1])));
                        }
                    }
                    _ => {}
                }
                at += 1;
            }
            let end = u32::try_from(calls.len()).expect("more than 2^32 macro calls in a file");
            found.push((name, writes, end));
            at += 1;
        }
        // alloc: the memo's result, one exact block per column
        MacroDefinitions { defs: found.drain(..).collect(), calls: calls.drain(..).collect() }
    })
}

/// What finding the impls of `krate` asks of each impl and of each body that may hold one.
/// Each is its own query, so [`ImplWork::run`] on many threads leaves the finding little to do.
pub fn impl_work(db: &Db, krate: Crate) -> Vec<ImplWork> {
    blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
        let def_map = crate_def_map(db, krate);
        let mut work = Vec::new_in(frame);
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            work.extend(scope.impls().map(ImplWork::Impl));
            work.extend(impl_holders(db, frame, krate, scope).into_iter().map(ImplWork::Holder));
        }
        // alloc: the caller's list, one exact block per crate
        work.iter().copied().collect()
    })
}

#[derive(Clone, Copy)]
pub enum ImplWork {
    Impl(ImplId),
    Holder(DefWithBodyId),
}

impl ImplWork {
    pub fn run(self, db: &Db) {
        match self {
            ImplWork::Impl(id) => {
                _ = db.impl_self_ty(id);
                _ = db.impl_trait(id);
            }
            ImplWork::Holder(owner) => _ = Body::of(db, owner),
        }
    }
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn crates_containing_incoherent_inherent_impls(db: &Db, krate: Crate) -> Box<[Crate]> {
    blaze_mem::frame::current(|frame| {
        let _p = tracing::info_span!("crates_containing_incoherent_inherent_impls").entered();
        // We assume that only sysroot crates contain `#[rustc_has_incoherent_inherent_impls]`
        // impls, since this is an internal feature and only std uses it.
        let mut lang = Vec::new_in(frame);
        lang.extend(
            krate.transitive_deps(db).iter().copied().filter(|krate| krate.data(db).origin.is_lang()),
        );
        // alloc: the memo's value, once at its length.
        lang.iter().copied().collect()
    })
}

pub fn with_incoherent_inherent_impls<'db>(
    db: &'db Db,
    krate: Crate,
    self_ty: &SimplifiedType<'db>,
    mut callback: impl FnMut(&[ImplId]),
) {
    let has_incoherent_impls = match self_ty.def() {
        Some(def_id) => match def_id.try_into() {
            Ok(def_id) => AttrFlags::query(db, def_id)
                .contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS),
            Err(()) => true,
        },
        _ => true,
    };
    if !has_incoherent_impls {
        return;
    }
    let _p = tracing::info_span!("incoherent_inherent_impls").entered();
    let crates = crates_containing_incoherent_inherent_impls(db, krate);
    for &krate in crates {
        callback(InherentImpls::in_crate_for_self_ty(db, krate, SelfTyKey(*self_ty)));
    }
}

pub fn simplified_type_module(db: &Db, ty: &SimplifiedType<'_>) -> Option<ModuleId> {
    match ty.def()? {
        SolverDefId::AdtId(id) => Some(id.module(db)),
        SolverDefId::TypeAliasId(id) => Some(id.module(db)),
        SolverDefId::TraitId(id) => Some(id.module(db)),
        _ => None,
    }
}

/// A `SimplifiedType` as the argument of a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct SelfTyKey<'db>(
    // SAFETY: the query database may keep this value past `'db` only if nothing in it borrows
    // from the database. `SimplifiedType<'db>` is `fast_reject::SimplifiedType<SolverDefId<'db>>`
    // (`next_solver/ty.rs`), whose variants (`vendor/rustc_type_ir/src/fast_reject.rs`) hold
    // only plain data and `SolverDefId<'db>`, which is `blaze_query::db::Value`; no `&'db`.
    #[value(unsafe(prove(SolverDefId<'db>: blaze_query::db::Value)))] pub SimplifiedType<'db>,
);

const _: () = assert!(size_of::<ImplId>() <= 8);
const _: () = assert!(size_of::<BuiltinDeriveImplId>() <= 8);
const _: () = assert!(size_of::<TraitId>() <= 8);
const _: () = assert!(size_of::<SimplifiedType<'static>>() <= 24);

/// Runs of one flat array, as compressed sparse rows: run `i` is
/// `values[starts[i]..starts[i + 1]]`.
#[derive(Debug, PartialEq, Eq, blaze_query::Value)]
struct Runs<V> {
    starts: Box<[u32]>,
    values: Box<[V]>,
}

impl<V: Copy> Runs<V> {
    /// The runs of `values`, each `(group, value)`: run `rank[group]` holds a group's values in
    /// the order they come. Counted first, so each column is allocated once at its size.
    fn from_groups(frame: &Frame, rank: &[u32], values: &[(u32, V)]) -> Self {
        u32::try_from(values.len()).expect("more than 2^32 impls in a crate");
        let ends = frame.alloc_slice_fill_with(rank.len() + 1, |_| 0u32);
        for &(group, _) in values {
            ends[rank[group as usize] as usize + 1] += 1;
        }
        for run in 1..ends.len() {
            ends[run] += ends[run - 1];
        }
        let starts = Box::<[u32]>::from(&*ends);
        let Some(&(_, first)) = values.first() else {
            return Runs { starts, values: Box::default() };
        };
        // `ends[run]` is now where run `run` starts, and is moved past each value placed.
        let placed = frame.alloc_slice_fill_with(values.len(), |_| first);
        for &(group, value) in values {
            let next = &mut ends[rank[group as usize] as usize];
            placed[*next as usize] = value;
            *next += 1;
        }
        Runs { starts, values: Box::from(&*placed) }
    }

    fn run(&self, i: usize) -> &[V] {
        &self.values[self.starts[i] as usize..self.starts[i + 1] as usize]
    }
}

/// Where each group goes: `rank[order[i]] == i`.
fn ranks<'f>(frame: &'f Frame, order: &[u32]) -> &'f mut [u32] {
    let rank = frame.alloc_slice_fill_with(order.len(), |_| 0u32);
    for (at, &group) in order.iter().enumerate() {
        rank[group as usize] = at as u32;
    }
    rank
}

/// The number of `trait_`, numbered as first seen: a new one is pushed on `trait_ids`.
fn trait_number(
    traits: &mut FrameMap<'_, TraitId, u32>,
    trait_ids: &mut Vec<TraitId, &Frame>,
    trait_: TraitId,
) -> u32 {
    *traits.entry(trait_).or_insert_with(|| {
        trait_ids.push(trait_);
        u32::try_from(trait_ids.len() - 1).expect("more than 2^32 traits in a crate")
    })
}

/// The traits `trait_ids` in the order [`TraitImpls::traits`] holds them, as indices.
fn order_by_trait_id<'f>(frame: &'f Frame, trait_ids: &[TraitId]) -> &'f mut [u32] {
    let order = frame.alloc_slice_fill_with(trait_ids.len(), |t| t as u32);
    order.sort_unstable_by_key(|&t| trait_ids[t as usize].as_id().as_u32());
    order
}

fn hash_self_ty(self_ty: &SimplifiedType<'_>) -> u64 {
    FixedState.hash_one(self_ty)
}

/// Simplified self types as two columns: the hashes, searched, and the types, compared only
/// where a hash matches. They are sorted by hash within each range a table searches.
#[derive(Debug, PartialEq, Eq)]
struct SelfTys<'db> {
    hashes: Box<[u64]>,
    self_tys: Box<[SimplifiedType<'db>]>,
}

impl<'db> SelfTys<'db> {
    /// The self types of `groups` (each with its hash) in the order `order` gives.
    fn in_order(groups: &[(u64, SimplifiedType<'db>)], order: &[u32]) -> Self {
        SelfTys {
            // alloc: the memo's two columns, each once at its length.
            hashes: order.iter().map(|&group| groups[group as usize].0).collect(),
            // alloc: as above.
            self_tys: order.iter().map(|&group| groups[group as usize].1).collect(),
        }
    }

    fn len(&self) -> usize {
        self.self_tys.len()
    }

    fn is_empty(&self) -> bool {
        self.self_tys.is_empty()
    }

    /// The index of `self_ty` in `range`, which is sorted by hash.
    fn find(&self, range: Range<usize>, self_ty: &SimplifiedType<'db>) -> Option<usize> {
        let hash = hash_self_ty(self_ty);
        // clone: a `Range<usize>`, two words.
        let first = range.start + self.hashes[range.clone()].partition_point(|&it| it < hash);
        (first..range.end)
            .take_while(|&i| self.hashes[i] == hash)
            .find(|&i| self.self_tys[i] == *self_ty)
    }
}

/// The group of `self_ty`, numbered as first seen: a new one is pushed on `self_tys` with its
/// hash.
fn group_of<'db>(
    groups: &mut FrameMap<'_, SimplifiedType<'db>, u32>,
    self_tys: &mut Vec<(u64, SimplifiedType<'db>), &Frame>,
    self_ty: SimplifiedType<'db>,
) -> u32 {
    *groups.entry(self_ty).or_insert_with(|| {
        self_tys.push((hash_self_ty(&self_ty), self_ty));
        u32::try_from(self_tys.len() - 1).expect("more than 2^32 self types in a crate")
    })
}

/// The groups sorted by the hash of their type; types of equal hash stay in the order they
/// were first seen.
fn order_by_hash<'f>(frame: &'f Frame, self_tys: &[(u64, SimplifiedType<'_>)]) -> &'f mut [u32] {
    let order = frame.alloc_slice_fill_with(self_tys.len(), |group| group as u32);
    order.sort_unstable_by_key(|&group| (self_tys[group as usize].0, group));
    order
}

/// Values grouped by simplified self type while they are found or read, every temporary in
/// the query's frame: the self types sorted by hash, each with its run of values in the order
/// they came.
struct SelfTyRunsBuilder<'db, 'f, V> {
    frame: &'f Frame,
    groups: FrameMap<'f, SimplifiedType<'db>, u32>,
    /// Each self type with its hash, in the order first seen.
    self_tys: Vec<(u64, SimplifiedType<'db>), &'f Frame>,
    /// Each value with its group, in the order found.
    values: Vec<(u32, V), &'f Frame>,
}

/// The inherent impls of a crate, a module or a block while they are found or read.
type InherentImplsBuilder<'db, 'f> = SelfTyRunsBuilder<'db, 'f, ImplId>;

impl<'db, 'f, V: Copy> SelfTyRunsBuilder<'db, 'f, V> {
    fn new(frame: &'f Frame) -> Self {
        SelfTyRunsBuilder {
            frame,
            groups: crate::frame_map(frame),
            self_tys: Vec::new_in(frame),
            values: Vec::new_in(frame),
        }
    }

    fn group(&mut self, self_ty: SimplifiedType<'db>) -> u32 {
        group_of(&mut self.groups, &mut self.self_tys, self_ty)
    }

    fn push(&mut self, group: u32, value: V) {
        self.values.push((group, value));
    }

    fn columns(self) -> (SelfTys<'db>, Runs<V>) {
        let order = order_by_hash(self.frame, &self.self_tys);
        let rank = ranks(self.frame, order);
        (SelfTys::in_order(&self.self_tys, order), Runs::from_groups(self.frame, rank, &self.values))
    }
}

impl<'db> InherentImplsBuilder<'db, '_> {
    fn finish(self) -> InherentImpls<'db> {
        let (self_tys, impls) = self.columns();
        InherentImpls { self_tys, impls }
    }
}

/// The inherent impls of a crate or block, as columns: the self types sorted by hash, each
/// with its run of impls in the order they were found.
#[derive(Debug, PartialEq, Eq, blaze_query::Value)]
pub struct InherentImpls<'db> {
    // SAFETY: the query database may keep this column past `'db` only if nothing in it borrows
    // from the database. `SelfTys<'db>` holds `u64`s and `SimplifiedType<'db>`s, which are
    // `fast_reject::SimplifiedType<SolverDefId<'db>>` (`next_solver/ty.rs`), whose variants
    // (`vendor/rustc_type_ir/src/fast_reject.rs`) hold only plain data and `SolverDefId<'db>`,
    // which is `blaze_query::db::Value`; no `&'db`.
    #[value(unsafe(prove(SolverDefId<'db>: blaze_query::db::Value)))]
    self_tys: SelfTys<'db>,
    impls: Runs<ImplId>,
}

#[blaze_query::tracked]
impl<'db> InherentImpls<'db> {
    #[blaze_query::tracked(returns(ref), mem_tag = Infer)]
    pub fn for_crate(db: &'db Db, krate: Crate) -> InherentImpls<'db> {
        blaze_mem::frame::current(|frame| {
            let _p = tracing::info_span!("inherent_impls_in_crate_query", ?krate).entered();
            if let Some(frozen) = frozen::inherent_impls(db, frame, krate) {
                return frozen;
            }
            delta::read::inherent_impls_of_crate(db, frame, krate)
        })
    }

    /// The inherent impls one module of a crate or block def map gives its table: its own
    /// ([`module_impls`]) and those of the blocks in the bodies it holds, in the order
    /// [`Self::for_crate`] holds them. Reruns when the module's scope changes, and is equal
    /// unless one of those impls is added, removed or changes its self type.
    #[blaze_query::tracked(returns(ref), mem_tag = Infer)]
    pub fn for_module(db: &'db Db, module: ModuleId) -> InherentImpls<'db> {
        blaze_mem::frame::current(|frame| {
            let mut builder = InherentImplsBuilder::new(frame);
            Self::collect_module(db, module, &mut builder);
            builder.finish()
        })
    }

    /// The inherent impls of `krate` for one type, in the order [`Self::for_crate`] holds
    /// them: the type's inherent slice and the bodies of the modules whose bodies hold impls
    /// for it, so an impl added for another type, or anything else added anywhere, leaves this
    /// as it was. A frozen crate's are read from its frozen table.
    #[blaze_query::tracked(returns(deref), mem_tag = Infer)]
    pub fn in_crate_for_self_ty(
        db: &'db Db,
        krate: Crate,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ImplId]> {
        if frozen::served(db, krate) {
            return Self::for_crate(db, krate).for_self_ty(&self_ty.0).into();
        }
        delta::read::inherent_impls_for(db, krate, self_ty)
    }

    #[blaze_query::tracked(returns(ref), mem_tag = Infer)]
    pub fn for_block(
        db: &'db Db,
        block: BlockIdLt<'db>,
    ) -> Option<Box<InherentImpls<'db>>> {
        blaze_mem::frame::current(|frame| {
            let _p = tracing::info_span!("inherent_impls_in_block_query").entered();

            let block_def_map = block_def_map(db, block);
            let result = Self::collect_def_map(db, frame, block_def_map);
            // alloc: the memo's handle, one per block with impls.
            if result.self_tys.is_empty() { None } else { Some(Box::new(result)) }
        })
    }

    /// The inherent impls of `block` for one type, as [`Self::in_crate_for_self_ty`].
    #[blaze_query::tracked(returns(deref), mem_tag = Infer)]
    pub fn in_block_for_self_ty(
        db: &'db Db,
        block: BlockIdLt<'db>,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ImplId]> {
        match Self::for_block(db, block) {
            Some(impls) => impls.for_self_ty(&self_ty.0).into(),
            None => Box::default(),
        }
    }
}

impl<'db> InherentImpls<'db> {
    fn collect_def_map(db: &'db Db, frame: &Frame, def_map: &'db DefMap) -> Self {
        let mut builder = InherentImplsBuilder::new(frame);
        for (module, _) in def_map.modules(db) {
            Self::collect_module(db, module, &mut builder);
        }
        builder.finish()
    }

    /// Adds the inherent impls of `module` and of the blocks in the bodies it holds to
    /// `builder`.
    fn collect_module(db: &'db Db, module: ModuleId, builder: &mut InherentImplsBuilder<'db, '_>) {
        Self::collect_own(db, module, builder);
        Self::collect_bodies(db, module, builder);
    }

    /// Adds the inherent impls of `module`'s own scope ([`module_impls`]) to `builder`.
    fn collect_own(db: &'db Db, module: ModuleId, builder: &mut InherentImplsBuilder<'db, '_>) {
        let interner = DbInterner::new_no_crate(db);
        for &impl_id in module_impls(db, module).inherent_impls.iter() {
            let self_ty = db.impl_self_ty(impl_id);
            let self_ty = self_ty.instantiate_identity().skip_norm_wip();
            if let Some(self_ty) =
                simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer)
            {
                let group = builder.group(self_ty);
                builder.push(group, impl_id);
            }
        }
    }

    /// Adds the inherent impls of the blocks in the bodies `module` holds to `builder`.
    fn collect_bodies(db: &'db Db, module: ModuleId, builder: &mut InherentImplsBuilder<'db, '_>) {
        let scope = module.def_map(db).module(db, module).scope(db);
        for owner in impl_holders(db, builder.frame, module.krate(db), scope) {
            for (_, block_def_map) in Body::of(db, owner).blocks(db) {
                for (block_module, _) in block_def_map.modules(db) {
                    Self::collect_module(db, block_module, builder);
                }
            }
        }
    }

    /// Adds every impl of this table to `builder`, each type's after those already there.
    fn merge_into(&self, builder: &mut InherentImplsBuilder<'db, '_>) {
        for (self_ty, impls) in self.iter() {
            let group = builder.group(*self_ty);
            for &impl_id in impls {
                builder.push(group, impl_id);
            }
        }
    }

    pub fn for_self_ty(&self, self_ty: &SimplifiedType<'db>) -> &[ImplId] {
        match self.self_tys.find(0..self.self_tys.len(), self_ty) {
            Some(i) => self.impls.run(i),
            None => &[],
        }
    }

    /// Every simplified self type with its inherent impls, in the order of their hashes.
    pub fn iter(&self) -> impl Iterator<Item = (&SimplifiedType<'db>, &[ImplId])> + '_ {
        self.self_tys.self_tys.iter().enumerate().map(|(i, self_ty)| (self_ty, self.impls.run(i)))
    }

    pub fn for_each_crate_and_block(
        db: &'db Db,
        krate: Crate,
        block: Option<BlockIdLt<'db>>,
        for_each: &mut impl FnMut(&InherentImpls<'db>),
    ) {
        let blocks = std::iter::successors(block, |block| block.module(db).block(db));
        blocks.filter_map(|block| Self::for_block(db, block).as_deref()).for_each(&mut *for_each);
        for_each(Self::for_crate(db, krate));
    }
}

/// The trait impls of a crate, a module or a block while they are found or read, every
/// temporary in the query's frame. A trait and a row (trait, self type) are numbered as first
/// seen.
struct TraitImplsBuilder<'db, 'f> {
    frame: &'f Frame,
    traits: FrameMap<'f, TraitId, u32>,
    trait_ids: Vec<TraitId, &'f Frame>,
    rows: FrameMap<'f, (u32, SimplifiedType<'db>), u32>,
    /// Each row's trait, and its self type with its hash, in the order first seen.
    row_keys: Vec<(u32, (u64, SimplifiedType<'db>)), &'f Frame>,
    impls: Vec<(u32, ImplId), &'f Frame>,
    derives: Vec<(u32, BuiltinDeriveImplId), &'f Frame>,
    /// Each blanket impl with its trait.
    blanket: Vec<(u32, ImplId), &'f Frame>,
}

impl<'db, 'f> TraitImplsBuilder<'db, 'f> {
    fn new(frame: &'f Frame) -> Self {
        TraitImplsBuilder {
            frame,
            traits: crate::frame_map(frame),
            trait_ids: Vec::new_in(frame),
            rows: crate::frame_map(frame),
            row_keys: Vec::new_in(frame),
            impls: Vec::new_in(frame),
            derives: Vec::new_in(frame),
            blanket: Vec::new_in(frame),
        }
    }

    fn trait_(&mut self, trait_: TraitId) -> u32 {
        trait_number(&mut self.traits, &mut self.trait_ids, trait_)
    }

    fn row(&mut self, t: u32, self_ty: SimplifiedType<'db>) -> u32 {
        let row_keys = &mut self.row_keys;
        *self.rows.entry((t, self_ty)).or_insert_with(|| {
            row_keys.push((t, (hash_self_ty(&self_ty), self_ty)));
            u32::try_from(row_keys.len() - 1).expect("more than 2^32 impls in a crate")
        })
    }

    fn push_impl(&mut self, row: u32, impl_id: ImplId) {
        self.impls.push((row, impl_id));
    }

    fn push_derive(&mut self, row: u32, impl_id: BuiltinDeriveImplId) {
        self.derives.push((row, impl_id));
    }

    fn push_blanket(&mut self, t: u32, impl_id: ImplId) {
        self.blanket.push((t, impl_id));
    }

    fn finish(self) -> TraitImpls<'db> {
        let frame = self.frame;
        let trait_ids = &self.trait_ids;
        let trait_order = order_by_trait_id(frame, trait_ids);
        let trait_rank = ranks(frame, trait_order);
        let row_keys = &self.row_keys;
        let row_order = frame.alloc_slice_fill_with(row_keys.len(), |r| r as u32);
        row_order.sort_unstable_by_key(|&r| {
            let (t, (hash, _)) = row_keys[r as usize];
            (trait_rank[t as usize], hash, r)
        });
        let row_rank = ranks(frame, row_order);
        let trait_rows = frame.alloc_slice_fill_with(trait_ids.len() + 1, |_| 0u32);
        for &(t, _) in row_keys.iter() {
            trait_rows[trait_rank[t as usize] as usize + 1] += 1;
        }
        for t in 1..trait_rows.len() {
            trait_rows[t] += trait_rows[t - 1];
        }
        TraitImpls {
            // alloc: the memo's columns, each once at its length.
            traits: trait_order.iter().map(|&t| trait_ids[t as usize]).collect(),
            trait_rows: Box::from(&*trait_rows),
            blanket: Runs::from_groups(frame, trait_rank, &self.blanket),
            self_tys: SelfTys {
                // alloc: as above.
                hashes: row_order.iter().map(|&r| row_keys[r as usize].1.0).collect(),
                // alloc: as above.
                self_tys: row_order.iter().map(|&r| row_keys[r as usize].1.1).collect(),
            },
            impls: Runs::from_groups(frame, row_rank, &self.impls),
            derives: Runs::from_groups(frame, row_rank, &self.derives),
        }
    }
}

/// The trait impls of a crate or block, as columns. The traits are sorted; trait `t` has the
/// run `blanket[t]` of blanket impls and the rows `trait_rows[t]..trait_rows[t + 1]`, one per
/// self type, sorted by hash. Row `r` has the runs `impls[r]` and `derives[r]`. Every run is in
/// the order its impls were found.
#[derive(Debug, PartialEq, blaze_query::Value)]
pub struct TraitImpls<'db> {
    traits: Box<[TraitId]>,
    trait_rows: Box<[u32]>,
    blanket: Runs<ImplId>,
    // SAFETY: the query database may keep this column past `'db` only if nothing in it borrows
    // from the database. `SelfTys<'db>` holds `u64`s and `SimplifiedType<'db>`s, which are
    // `fast_reject::SimplifiedType<SolverDefId<'db>>` (`next_solver/ty.rs`), whose variants
    // (`vendor/rustc_type_ir/src/fast_reject.rs`) hold only plain data and `SolverDefId<'db>`,
    // which is `blaze_query::db::Value`; no `&'db`.
    #[value(unsafe(prove(SolverDefId<'db>: blaze_query::db::Value)))]
    self_tys: SelfTys<'db>,
    impls: Runs<ImplId>,
    derives: Runs<BuiltinDeriveImplId>,
}

impl<'db> TraitImpls<'db> {
    fn trait_index(&self, trait_: TraitId) -> Option<usize> {
        self.traits.binary_search(&trait_).ok()
    }

    fn rows(&self, t: usize) -> Range<usize> {
        self.trait_rows[t] as usize..self.trait_rows[t + 1] as usize
    }

    fn row(&self, trait_: TraitId, self_ty: &SimplifiedType<'db>) -> Option<usize> {
        self.trait_index(trait_).and_then(|t| self.self_tys.find(self.rows(t), self_ty))
    }

    /// Each trait with its index, for [`frozen`].
    fn trait_indices(&self) -> impl Iterator<Item = (TraitId, usize)> + '_ {
        self.traits.iter().copied().zip(0..)
    }

    /// The impls of the trait at index `t` per self type, for [`frozen`].
    fn rows_at(
        &self,
        t: usize,
    ) -> impl Iterator<Item = (&SimplifiedType<'db>, (&[ImplId], &[BuiltinDeriveImplId]))> + '_
    {
        self.rows(t).map(|r| {
            (&self.self_tys.self_tys[r], (self.impls.run(r), self.derives.run(r)))
        })
    }
}

#[blaze_query::tracked]
impl<'db> TraitImpls<'db> {
    #[blaze_query::tracked(returns(ref), mem_tag = Infer)]
    pub fn for_crate(db: &'db Db, krate: Crate) -> Arc<TraitImpls<'db>> {
        blaze_mem::frame::current(|frame| {
            let _p = tracing::info_span!("inherent_impls_in_crate_query", ?krate).entered();
            if let Some(frozen) = frozen::trait_impls(db, frame, krate) {
                // alloc: the memo's handle, shared by `for_crate_and_deps`.
                return Arc::new(frozen);
            }
            // alloc: the memo's handle, shared by `for_crate_and_deps`.
            Arc::new(delta::read::trait_impls_of_crate(db, frame, krate))
        })
    }

    /// The trait impls one module of a crate or block def map gives its tables: its own
    /// ([`module_impls`]) and those of the blocks in the bodies it holds, in the order
    /// [`Self::for_crate`] holds them. Reruns when the module's scope changes, and is equal
    /// unless one of those impls is added, removed or changes its trait or self type.
    #[blaze_query::tracked(returns(ref), mem_tag = Infer)]
    pub fn for_module(db: &'db Db, module: ModuleId) -> TraitImpls<'db> {
        blaze_mem::frame::current(|frame| {
            let lang_items = hir_def::lang_item::lang_items(db, module.krate(db));
            let mut builder = TraitImplsBuilder::new(frame);
            Self::collect_module(db, module, lang_items, &mut builder);
            builder.finish()
        })
    }

    /// The impls of `trait_` in `krate`, as a table of that trait alone, in the order
    /// [`Self::for_crate`] holds them: the bucket every narrow memo of one crate reads. Reads
    /// the trait's slice and the bodies of the modules whose bodies hold impls of it, so an
    /// impl of another trait, or anything else added anywhere, leaves this as it was. A frozen
    /// crate's is read from its frozen table.
    #[blaze_query::tracked(returns(ref), mem_tag = Infer)]
    pub fn for_crate_and_trait(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
    ) -> TraitImpls<'db> {
        blaze_mem::frame::current(|frame| {
            let mut builder = TraitImplsBuilder::new(frame);
            if frozen::served(db, krate) {
                Self::for_crate(db, krate).merge_trait_into(trait_, &mut builder);
            } else {
                delta::read::trait_impls_into(db, krate, trait_, &mut builder);
            }
            builder.finish()
        })
    }

    #[blaze_query::tracked(returns(as_deref), mem_tag = Infer)]
    pub fn for_block(
        db: &'db Db,
        block: BlockIdLt<'db>,
    ) -> Option<Box<TraitImpls<'db>>> {
        blaze_mem::frame::current(|frame| {
            let _p = tracing::info_span!("inherent_impls_in_block_query").entered();

            let block_def_map = block_def_map(db, block);
            let result = Self::collect_def_map(db, frame, block_def_map);
            // alloc: the memo's handle, one per block with impls.
            if result.traits.is_empty() { None } else { Some(Box::new(result)) }
        })
    }

    #[blaze_query::tracked(returns(deref), mem_tag = Infer)]
    pub fn for_crate_and_deps(db: &'db Db, krate: Crate) -> Box<[Arc<Self>]> {
        // clone: an `Arc` count per crate into the memo's value, made once at its length.
        loaded_crates(db, krate).iter().map(|&dep| Self::for_crate(db, dep).clone()).collect()
    }

    /// The impls of one trait for one type in `krate` and its dependencies, in the order of
    /// [`loaded_crates`]. Reads [`Self::for_crate_and_trait`] of each crate that can name both
    /// ([`crates_that_name`]), so an impl of another trait anywhere leaves this
    /// unvalidated-equal, and one of `trait_` for another type reruns it equal.
    #[blaze_query::tracked(returns(deref), mem_tag = Infer)]
    pub fn in_crate_and_deps_for_trait_and_self_ty(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[AnyImplId]> {
        blaze_mem::frame::current(|frame| {
            let mut found = Vec::new_in(frame);
            let loaded = loaded_crates(db, krate);
            let self_crate = simplified_type_module(db, &self_ty.0).map(|it| it.krate(db));
            for at in crates_that_name(db, krate).holders(frame, trait_.krate(db), self_crate) {
                let impls = Self::for_crate_and_trait(db, loaded[at], trait_);
                let (impls, builtin_derive_impls) =
                    impls.for_trait_and_self_ty(trait_, &self_ty.0);
                found.extend(impls.iter().map(|&it| AnyImplId::from(it)));
                found.extend(builtin_derive_impls.iter().map(|&it| AnyImplId::from(it)));
            }
            // alloc: the memo's value, once at its length.
            found.iter().copied().collect()
        })
    }

    /// The blanket impls of one trait in `krate` and its dependencies.
    #[blaze_query::tracked(returns(deref), mem_tag = Infer)]
    pub fn blanket_in_crate_and_deps(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
    ) -> Box<[ImplId]> {
        blaze_mem::frame::current(|frame| {
            let mut found = Vec::new_in(frame);
            let loaded = loaded_crates(db, krate);
            for at in crates_that_name(db, krate).holders(frame, trait_.krate(db), None) {
                found.extend_from_slice(
                    Self::for_crate_and_trait(db, loaded[at], trait_).blanket_impls(trait_),
                );
            }
            // alloc: the memo's value, once at its length.
            found.iter().copied().collect()
        })
    }

    /// The impls of one trait for one type in `krate` alone, without the built-in derives.
    #[blaze_query::tracked(returns(deref), mem_tag = Infer)]
    pub fn in_crate_for_trait_and_self_ty(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ImplId]> {
        Self::for_crate_and_trait(db, krate, trait_)
            .for_trait_and_self_ty(trait_, &self_ty.0)
            .0
            .into()
    }

    /// The impls of one trait for one type in `block` alone, without the built-in derives.
    #[blaze_query::tracked(returns(deref), mem_tag = Infer)]
    pub fn in_block_for_trait_and_self_ty(
        db: &'db Db,
        block: BlockIdLt<'db>,
        trait_: TraitId,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ImplId]> {
        match Self::for_block(db, block) {
            Some(impls) => impls.for_trait_and_self_ty(trait_, &self_ty.0).0.into(),
            None => Box::default(),
        }
    }

    /// The impls of one trait in `krate` alone, without the built-in derives, in the order
    /// [`Self::for_trait`] gives them: the blanket impls first.
    #[blaze_query::tracked(returns(deref), mem_tag = Infer)]
    pub fn in_crate_for_trait(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
    ) -> Box<[ImplId]> {
        blaze_mem::frame::current(|frame| {
            let mut found = Vec::new_in(frame);
            Self::for_crate_and_trait(db, krate, trait_).for_trait(trait_, |impls| {
                if let Either::Left(impls) = impls {
                    found.extend_from_slice(impls);
                }
            });
            // alloc: the memo's value, once at its length.
            found.iter().copied().collect()
        })
    }

    /// The impls of one trait in `krate` and its dependencies, each crate's as
    /// [`Self::in_crate_for_trait`]. An impl of another trait leaves every crate's
    /// [`Self::for_crate_and_trait`] for this one as it was, so this does not rerun.
    #[blaze_query::tracked(returns(deref), mem_tag = Infer)]
    pub fn in_crate_and_deps_for_trait(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
    ) -> Box<[ImplId]> {
        blaze_mem::frame::current(|frame| {
            let mut found = Vec::new_in(frame);
            let loaded = loaded_crates(db, krate);
            for at in crates_that_name(db, krate).holders(frame, trait_.krate(db), None) {
                found.extend_from_slice(Self::in_crate_for_trait(db, loaded[at], trait_));
            }
            // alloc: the memo's value, once at its length.
            found.iter().copied().collect()
        })
    }
}

impl<'db> TraitImpls<'db> {
    fn collect_def_map(db: &'db Db, frame: &Frame, def_map: &DefMap) -> Self {
        let lang_items = hir_def::lang_item::lang_items(db, def_map.krate());
        let mut builder = TraitImplsBuilder::new(frame);
        for (module, _) in def_map.modules(db) {
            Self::collect_module(db, module, lang_items, &mut builder);
        }
        builder.finish()
    }

    /// Adds the trait impls of `module` and of the blocks in the bodies it holds to
    /// `builder`.
    fn collect_module(
        db: &'db Db,
        module: ModuleId,
        lang_items: &LangItems,
        builder: &mut TraitImplsBuilder<'db, '_>,
    ) {
        Self::collect_own(db, module, lang_items, builder);
        Self::collect_bodies(db, module, lang_items, builder);
    }

    /// Adds the trait impls and built-in derive impls of `module`'s own scope
    /// ([`module_impls`]) to `builder`.
    fn collect_own(
        db: &'db Db,
        module: ModuleId,
        lang_items: &LangItems,
        builder: &mut TraitImplsBuilder<'db, '_>,
    ) {
        let impls = module_impls(db, module);
        for &impl_id in impls.trait_impls.iter() {
            let trait_ref = match db.impl_trait(impl_id) {
                Some(tr) => tr.instantiate_identity().skip_norm_wip(),
                None => continue,
            };
            // Reservation impls should be ignored during trait resolution, so we never need
            // them during type analysis. See rust-lang/rust#64631 for details.
            //
            // FIXME: Reservation impls should be considered during coherence checks. If we are
            // (ever) to implement coherence checks, this filtering should be done by the trait
            // solver.
            if AttrFlags::query(db, impl_id.into()).contains(AttrFlags::RUSTC_RESERVATION_IMPL) {
                continue;
            }

            let self_ty = trait_ref.self_ty();
            if self_ty_has_error_constructor(self_ty) {
                // If we see `impl Foo for NoSuchType`, just ignore it.
                continue;
            }

            let interner = DbInterner::new_no_crate(db);
            let t = builder.trait_(trait_ref.def_id.0);
            match simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer) {
                Some(self_ty) => {
                    let row = builder.row(t, self_ty);
                    builder.push_impl(row, impl_id);
                }
                None => builder.push_blanket(t, impl_id),
            }
        }

        for &impl_id in impls.builtin_derive_impls.iter() {
            let loc = impl_id.loc(db);
            let Some(trait_id) = loc.trait_.get_id(lang_items) else { continue };
            let t = builder.trait_(trait_id);
            let row = builder.row(t, SimplifiedType::Adt(loc.adt.into()));
            builder.push_derive(row, impl_id);
        }
    }

    /// Adds the trait impls of the blocks in the bodies `module` holds to `builder`.
    fn collect_bodies(
        db: &'db Db,
        module: ModuleId,
        lang_items: &LangItems,
        builder: &mut TraitImplsBuilder<'db, '_>,
    ) {
        let scope = module.def_map(db).module(db, module).scope(db);
        for owner in impl_holders(db, builder.frame, module.krate(db), scope) {
            for (_, block_def_map) in Body::of(db, owner).blocks(db) {
                for (block_module, _) in block_def_map.modules(db) {
                    Self::collect_module(db, block_module, lang_items, builder);
                }
            }
        }
    }

    /// Adds every impl of this table to `builder`, each run after the runs already there.
    fn merge_into(&self, builder: &mut TraitImplsBuilder<'db, '_>) {
        for (trait_, t) in self.trait_indices() {
            self.merge_at(t, builder.trait_(trait_), builder);
        }
    }

    /// Adds this table's impls of `trait_` to `builder`, as [`Self::merge_into`] does.
    fn merge_trait_into(&self, trait_: TraitId, builder: &mut TraitImplsBuilder<'db, '_>) {
        if let Some(t) = self.trait_index(trait_) {
            self.merge_at(t, builder.trait_(trait_), builder);
        }
    }

    /// Adds the impls of the trait at index `t` here to trait `into` of `builder`.
    fn merge_at(&self, t: usize, into: u32, builder: &mut TraitImplsBuilder<'db, '_>) {
        for &impl_id in self.blanket.run(t) {
            builder.push_blanket(into, impl_id);
        }
        for (self_ty, (impls, derives)) in self.rows_at(t) {
            let row = builder.row(into, *self_ty);
            for &impl_id in impls {
                builder.push_impl(row, impl_id);
            }
            for &impl_id in derives {
                builder.push_derive(row, impl_id);
            }
        }
    }

    pub fn blanket_impls(&self, for_trait: TraitId) -> &[ImplId] {
        match self.trait_index(for_trait) {
            Some(t) => self.blanket.run(t),
            None => &[],
        }
    }

    /// Queries whether `self_ty` has potentially applicable implementations of `trait_`.
    pub fn has_impls_for_trait_and_self_ty(
        &self,
        trait_: TraitId,
        self_ty: &SimplifiedType<'db>,
    ) -> bool {
        self.trait_index(trait_).is_some_and(|t| {
            self.self_tys.find(self.rows(t), self_ty).is_some() || !self.blanket.run(t).is_empty()
        })
    }

    pub fn for_trait_and_self_ty(
        &'db self,
        trait_: TraitId,
        self_ty: &SimplifiedType<'db>,
    ) -> (&'db [ImplId], &'db [BuiltinDeriveImplId]) {
        match self.row(trait_, self_ty) {
            Some(r) => (self.impls.run(r), self.derives.run(r)),
            None => (&[], &[]),
        }
    }

    pub fn for_trait(
        &self,
        trait_: TraitId,
        mut callback: impl FnMut(Either<&[ImplId], &[BuiltinDeriveImplId]>),
    ) {
        if let Some(t) = self.trait_index(trait_) {
            callback(Either::Left(self.blanket.run(t)));
            for r in self.rows(t) {
                callback(Either::Left(self.impls.run(r)));
                callback(Either::Right(self.derives.run(r)));
            }
        }
    }

    /// [`Self::for_trait`] without the blanket impls: rustc's `non_blanket_impls`.
    pub fn for_trait_non_blanket(
        &self,
        trait_: TraitId,
        mut callback: impl FnMut(Either<&[ImplId], &[BuiltinDeriveImplId]>),
    ) {
        if let Some(t) = self.trait_index(trait_) {
            for r in self.rows(t) {
                callback(Either::Left(self.impls.run(r)));
                callback(Either::Right(self.derives.run(r)));
            }
        }
    }

    pub fn for_self_ty(
        &self,
        self_ty: &SimplifiedType<'db>,
        mut callback: impl FnMut(Either<&[ImplId], &[BuiltinDeriveImplId]>),
    ) {
        for t in 0..self.traits.len() {
            if let Some(r) = self.self_tys.find(self.rows(t), self_ty) {
                callback(Either::Left(self.impls.run(r)));
                callback(Either::Right(self.derives.run(r)));
            }
        }
    }

    pub fn for_each_crate_and_block<R: VisitorResult>(
        db: &'db Db,
        krate: Crate,
        block: Option<BlockIdLt<'db>>,
        for_each: &mut impl FnMut(&TraitImpls<'db>) -> R,
    ) -> R {
        try_visit!(Self::for_each_block(db, block, &mut *for_each));
        for impl_ in Self::for_crate_and_deps(db, krate) {
            try_visit!(for_each(impl_));
        }
        R::output()
    }

    /// The impls of `block` and of the blocks around it.
    pub fn for_each_block<R: VisitorResult>(
        db: &'db Db,
        block: Option<BlockIdLt<'db>>,
        for_each: &mut impl FnMut(&TraitImpls<'db>) -> R,
    ) -> R {
        let blocks = std::iter::successors(block, |block| block.module(db).block(db));
        for impl_ in blocks.filter_map(|block| Self::for_block(db, block)) {
            try_visit!(for_each(impl_));
        }
        R::output()
    }

    /// Like [`Self::for_each_crate_and_block()`], but takes in account two blocks, one for a trait and one for a self type.
    pub fn for_each_crate_and_block_trait_and_type<R: VisitorResult>(
        db: &'db Db,
        krate: Crate,
        type_block: Option<BlockIdLt<'db>>,
        trait_block: Option<BlockIdLt<'db>>,
        for_each: &mut impl FnMut(&TraitImpls<'db>) -> R,
    ) -> R {
        let in_self_and_deps = TraitImpls::for_crate_and_deps(db, krate);
        for impl_ in in_self_and_deps {
            try_visit!(for_each(impl_));
        }
        Self::for_each_block_trait_and_type(db, type_block, trait_block, for_each)
    }

    /// The impls of two blocks and of the blocks around them, one for a trait and one for a
    /// self type.
    pub fn for_each_block_trait_and_type<R: VisitorResult>(
        db: &'db Db,
        type_block: Option<BlockIdLt<'db>>,
        trait_block: Option<BlockIdLt<'db>>,
        for_each: &mut impl FnMut(&TraitImpls<'db>) -> R,
    ) -> R {
        // We must not provide duplicate impls to the solver. Therefore we work with the following strategy:
        // start from each block, and walk ancestors until you meet the other block. If they never meet,
        // that means there can't be duplicate impls; if they meet, we stop the search of the deeper block.
        // This breaks when they are equal (both will stop immediately), therefore we handle this case
        // specifically.
        let blocks_iter = |block: Option<BlockIdLt<'db>>| {
            std::iter::successors(block, |block| block.module(db).block(db))
        };
        let for_each_block = |current_block: Option<BlockIdLt<'db>>,
                              other_block: Option<BlockIdLt<'db>>| {
            blocks_iter(current_block)
                .take_while(move |&block| {
                    other_block.is_none_or(|other_block| other_block != block)
                })
                .filter_map(move |block| TraitImpls::for_block(db, block))
        };
        if trait_block == type_block {
            for impl_ in
                blocks_iter(trait_block).filter_map(|block| TraitImpls::for_block(db, block))
            {
                try_visit!(for_each(impl_));
            }
        } else {
            for impl_ in for_each_block(trait_block, type_block) {
                try_visit!(for_each(impl_));
            }
            for impl_ in for_each_block(type_block, trait_block) {
                try_visit!(for_each(impl_));
            }
        }
        R::output()
    }
}

/// The key of [`relevant_impls`]: a goal of `trait_` solved in `krate` whose self type
/// simplifies to `self_ty`, or to nothing (a parameter, an alias, a placeholder) when `None`.
#[blaze_query::interned(debug)]
pub struct RelevantImplsKey<'db> {
    #[returns(copy)]
    pub krate: Crate,
    #[returns(copy)]
    pub trait_: TraitId,
    #[returns(copy)]
    pub self_ty: Option<SelfTyKey<'db>>,
}

/// What [`relevant_impls`] gives: the impls for the self type, then the blanket ones.
#[derive(Debug, PartialEq, Eq)]
pub struct RelevantImpls {
    impls: Box<[AnyImplId]>,
    blanket: u32,
}

impl RelevantImpls {
    /// Every impl, in the order the solver considers them.
    pub fn all(&self) -> &[AnyImplId] {
        &self.impls
    }

    pub fn for_self_ty(&self) -> &[AnyImplId] {
        &self.impls[..self.blanket as usize]
    }

    pub fn blanket(&self) -> &[AnyImplId] {
        &self.impls[self.blanket as usize..]
    }
}

/// The impls the trait solver considers for a goal of `key.trait_` in `key.krate`, in the
/// order it considers them (`DbInterner::for_each_relevant_impl`): with a self type, those for
/// it in the crate and the crates it loads, then those of the blocks around the trait and the
/// type not among them; then the blanket impls of the blocks around the trait, then those of
/// the crates not among them. With no self type, the blanket impls alone. One read names one
/// trait and one simplified self type, as a goal's `Implements` fact does; the whole tables
/// name neither.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn relevant_impls<'db>(db: &'db Db, key: RelevantImplsKey<'db>) -> RelevantImpls {
    blaze_mem::frame::current(|frame| {
        let (krate, trait_) = (key.krate(db), key.trait_(db));
        let trait_block = trait_.loc(db).container.block(db);
        let mut found = Vec::new_in(frame);
        let blanket = match key.self_ty(db) {
            Some(SelfTyKey(self_ty)) => {
                found.extend_from_slice(TraitImpls::in_crate_and_deps_for_trait_and_self_ty(
                    db,
                    krate,
                    trait_,
                    SelfTyKey(self_ty),
                ));
                let type_block = simplified_type_module(db, &self_ty).and_then(|it| it.block(db));
                // The impls of a block are among those of its crate as well as its own, and
                // the solver takes a second copy for an ambiguity.
                TraitImpls::for_each_block_trait_and_type(
                    db,
                    type_block,
                    trait_block,
                    &mut |impls| {
                        let (regular, derives) = impls.for_trait_and_self_ty(trait_, &self_ty);
                        let in_block = regular
                            .iter()
                            .map(|&it| AnyImplId::from(it))
                            .chain(derives.iter().map(|&it| AnyImplId::from(it)));
                        for impl_ in in_block {
                            if !found.contains(&impl_) {
                                found.push(impl_);
                            }
                        }
                    },
                );
                let blanket = found.len();
                let none = RelevantImplsKey::new(db, krate, trait_, None);
                found.extend_from_slice(relevant_impls(db, none).blanket());
                blanket
            }
            None => {
                TraitImpls::for_each_block(db, trait_block, &mut |impls| {
                    found.extend(impls.blanket_impls(trait_).iter().map(|&it| AnyImplId::from(it)));
                });
                let in_blocks = found.len();
                for &impl_ in TraitImpls::blanket_in_crate_and_deps(db, krate, trait_) {
                    let impl_ = AnyImplId::from(impl_);
                    if !found[..in_blocks].contains(&impl_) {
                        found.push(impl_);
                    }
                }
                0
            }
        };
        RelevantImpls {
            // alloc: the memo's value, once at its length.
            impls: found.iter().copied().collect(),
            blanket: u32::try_from(blanket).expect("fewer than 2^32 impls of a trait"),
        }
    })
}

fn self_ty_has_error_constructor<'db>(mut self_ty: Ty<'db>) -> bool {
    if !self_ty.references_non_lt_error() {
        return false;
    }

    loop {
        self_ty = match self_ty.kind() {
            TyKind::Error(_) => return true,
            TyKind::Ref(_, inner, _)
            | TyKind::RawPtr(inner, _)
            | TyKind::Array(inner, _)
            | TyKind::Slice(inner)
            | TyKind::Pat(inner, _) => inner,
            TyKind::UnsafeBinder(inner) => inner.skip_binder(),
            _ => return false,
        };
    }
}

#[cfg(test)]
mod inert_attribute_tests {
    use intern::Symbol;

    use super::{INERT_ATTRIBUTES, MacroDefinitions, attribute_is_inert};

    #[test]
    fn inert_attributes_as_a_linear_search() {
        let mut names: Vec<String> = Vec::new();
        for name in INERT_ATTRIBUTES {
            names.push((*name).to_owned());
            names.push(format!("{name}_"));
            names.push(format!("r#{name}"));
            names.push(name.to_uppercase());
            names.push(name[..name.len() - 1].to_owned());
        }
        names.extend(
            ["", "derive", "cfg_attr", "test", "macro_export", "rustfmt", "clippy", "allowed"]
                .map(str::to_owned),
        );
        for name in &names {
            let by_search = INERT_ATTRIBUTES.contains(&name.as_str());
            assert_eq!(attribute_is_inert(name), by_search, "{name}");
        }
    }

    #[test]
    fn macro_definitions_iterate_their_runs() {
        let s = Symbol::intern;
        let defs = MacroDefinitions {
            defs: Box::new([(s("a"), false, 2), (s("b"), true, 2), (s("c"), false, 3)]),
            calls: Box::new([s("x"), s("y"), s("z")]),
        };
        let runs: Vec<(Symbol, bool, Vec<Symbol>)> =
            defs.iter().map(|(n, w, c)| (n.clone(), w, c.to_vec())).collect();
        assert_eq!(runs, [
            (s("a"), false, vec![s("x"), s("y")]),
            (s("b"), true, vec![]),
            (s("c"), false, vec![s("z")]),
        ]);
    }
}

#[cfg(test)]
mod columns_tests {
    use blaze_mem::{Tag, frame};
    use rustc_type_ir::{IntTy, UintTy};

    use super::{Runs, SimplifiedType, hash_self_ty, order_by_hash, ranks};

    /// The old `Runs::new`: each run copied after the one before.
    fn reference(runs: &[Vec<u32>]) -> (Vec<u32>, Vec<u32>) {
        let mut starts = vec![0];
        let mut values = Vec::new();
        for run in runs {
            values.extend_from_slice(run);
            starts.push(values.len() as u32);
        }
        (starts, values)
    }

    #[test]
    fn runs_from_groups_equal_runs_copied_in_order() {
        frame::task(Tag::Infer, |frame| {
            // Values of 4 groups, interleaved; groups go in the order 2, 0, 3, 1.
            let values: [(u32, u32); 6] = [(0, 10), (2, 20), (0, 11), (3, 30), (2, 21), (0, 12)];
            let order = [2u32, 0, 3, 1];
            let rank = ranks(frame, &order);
            let runs = Runs::from_groups(frame, rank, &values);
            let by_group = |g: u32| -> Vec<u32> {
                values.iter().filter(|it| it.0 == g).map(|it| it.1).collect()
            };
            let (starts, flat) = reference(&order.map(by_group));
            assert_eq!(&*runs.starts, &starts[..]);
            assert_eq!(&*runs.values, &flat[..]);
            assert_eq!(runs.run(3), &[] as &[u32]);
        });
    }

    #[test]
    fn runs_of_nothing_have_one_start() {
        frame::task(Tag::Infer, |frame| {
            let runs = Runs::<u32>::from_groups(frame, &[], &[]);
            assert_eq!(&*runs.starts, &[0]);
            assert!(runs.values.is_empty());
        });
    }

    #[test]
    fn order_by_hash_is_the_stable_sort_by_hash() {
        frame::task(Tag::Infer, |frame| {
            let tys: [SimplifiedType<'static>; 6] = [
                SimplifiedType::Bool,
                SimplifiedType::Int(IntTy::I32),
                SimplifiedType::Uint(UintTy::U8),
                SimplifiedType::Char,
                SimplifiedType::Str,
                SimplifiedType::Never,
            ];
            // Six groups, three hashes: some share one, and ties keep first-seen order.
            let groups: Vec<(u64, SimplifiedType<'static>)> =
                tys.iter().map(|&ty| (hash_self_ty(&ty) % 3, ty)).collect();
            let order = order_by_hash(frame, &groups);
            let mut expected: Vec<u32> = (0..groups.len() as u32).collect();
            expected.sort_by_key(|&g| groups[g as usize].0);
            assert_eq!(&*order, &expected[..]);
        });
    }
}
