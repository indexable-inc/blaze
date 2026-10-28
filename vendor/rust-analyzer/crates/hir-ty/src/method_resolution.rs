//! This module is concerned with finding methods that a given type provides.
//! For details about how this works in rustc, see the method lookup page in the
//! [rustc guide] and the corresponding code mostly in
//! [`rustc_hir_typeck/method/probe.rs`].
//!
//! [rustc guide]: https://rust-lang.github.io/rustc-guide/method-lookup.html
//! [`rustc_hir_typeck/method/probe.rs`]: https://github.com/rust-lang/rust/blob/5503df87342a73d0c29126a7e08dc9c1255c46ad/compiler/rustc_hir_typeck/src/method/probe.rs

mod confirm;
pub mod frozen;
mod probe;

use crate::next_solver::StoredGenericArgs;
use either::Either;
use hir_expand::name::Name;
use intern::Symbol;
use salsa::SalsaValue;
use span::Edition;
use tracing::{debug, instrument};

use base_db::Crate;
use hir_def::{
    AssocItemId, BlockIdLt, BuiltinDeriveImplId, ConstId, DefWithBodyId, FunctionId,
    GenericParamId, HasModule, ImplId, ItemContainerId, MacroId, ModuleDefId, ModuleId, TraitId,
    attrs::AttrFlags,
    builtin_derive::BuiltinDeriveImplMethod,
    expr_store::{Body, path::GenericArgs as HirGenericArgs},
    hir::{ExprId, generics::GenericParams},
    item_scope::ItemScope,
    lang_item::LangItems,
    nameres::{DefMap, block_def_map, crate_def_map},
    resolver::Resolver,
    signatures::{ConstSignature, FunctionSignature},
    unstable_features::UnstableFeatures,
};
use rustc_hash::{FxHashMap, FxHashSet};
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
    InferenceDiagnostic, Span, all_super_traits,
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

pub use self::probe::{
    Candidate, CandidateKind, CandidateStep, CandidateWithPrivate, Mode, Pick, PickKind,
};

pub struct MethodResolutionContext<'a, 'db> {
    pub infcx: &'a InferCtxt<'db>,
    pub resolver: &'a Resolver<'db>,
    pub param_env: ParamEnv<'db>,
    pub traits_in_scope: &'a FxHashSet<TraitId>,
    pub edition: Edition,
    pub features: &'a UnstableFeatures,
    pub call_span: Span,
    pub receiver_span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, salsa::SalsaValue)]
pub enum CandidateId {
    FunctionId(FunctionId),
    ConstId(ConstId),
}
impl_from!(FunctionId, ConstId for CandidateId);

impl CandidateId {
    fn container(self, db: &dyn HirDatabase) -> ItemContainerId {
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

impl<'db> InferenceContext<'db> {
    /// Performs method lookup. If lookup is successful, it will return the callee
    /// and store an appropriate adjustment for the self-expr. In some cases it may
    /// report an error (e.g., invoking the `drop` method).
    #[instrument(level = "debug", skip(self))]
    pub(crate) fn lookup_method_including_private(
        &mut self,
        self_ty: Ty<'db>,
        name: Name,
        generic_args: Option<&HirGenericArgs>,
        receiver: ExprId,
        call_expr: ExprId,
    ) -> Result<(MethodCallee<'db>, bool), MethodError<'db>> {
        let (pick, is_visible) = match self.lookup_probe(call_expr, receiver, name, self_ty) {
            Ok(it) => (it, true),
            Err(MethodError::PrivateMatch(it)) => {
                // FIXME: Report error.
                (it, false)
            }
            Err(err) => return Err(err),
        };

        let result = self.confirm_method(&pick, self_ty, call_expr, generic_args);
        debug!("result = {:?}", result);

        if result.illegal_sized_bound {
            self.push_diagnostic(InferenceDiagnostic::MethodCallIllegalSizedBound { call_expr });
        }

        self.write_expr_adj(receiver, result.adjustments);
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
        let traits_in_scope = self.get_traits_in_scope();
        let traits_in_scope = match &traits_in_scope {
            Either::Left(it) => it,
            Either::Right(it) => *it,
        };
        let ctx = MethodResolutionContext {
            infcx: &self.table.infer_ctxt,
            resolver: &self.resolver,
            param_env: self.table.param_env,
            traits_in_scope,
            edition: self.edition,
            features: self.features,
            call_span,
            receiver_span,
        };
        f(&ctx)
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

    lookup_impl_assoc_item_for_trait_ref(infcx, trait_ref, env, name)
        .and_then(|assoc| {
            if let (Either::Left(AssocItemId::ConstId(id)), s) = assoc {
                Some((id, s))
            } else {
                None
            }
        })
        .unwrap_or((const_id, subs))
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
pub(crate) type StoredImplMethod =
    (Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, StoredGenericArgs);

/// [`lookup_impl_method_query`], memoized: codegen asks the same trait method with the same
/// arguments many times per build and again on every edit, and each ask re-runs trait
/// selection in a fresh inference context.
#[salsa::tracked(cycle_result = lookup_impl_method_cycle_result, returns(clone))]
pub(crate) fn lookup_impl_method_stored(
    db: &dyn HirDatabase,
    env: crate::traits::StoredParamEnvAndCrate,
    func: FunctionId,
    fn_subst: StoredGenericArgs,
) -> StoredImplMethod {
    let (found, args) = lookup_impl_method_query(db, env.as_ref(db), func, fn_subst.as_ref());
    (found, args.store())
}

fn lookup_impl_method_cycle_result(
    _: &dyn HirDatabase,
    _: salsa::Id,
    _env: crate::traits::StoredParamEnvAndCrate,
    func: FunctionId,
    fn_subst: StoredGenericArgs,
) -> StoredImplMethod {
    (Either::Left(func), fn_subst)
}

/// Looks up the impl method that actually runs for the trait method `func`.
///
/// Returns `func` if it's not a method defined in a trait or the lookup failed.
pub(crate) fn lookup_impl_method_query<'db>(
    db: &'db dyn HirDatabase,
    env: ParamEnvAndCrate<'db>,
    func: FunctionId,
    fn_subst: GenericArgs<'db>,
) -> (Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>) {
    let interner = DbInterner::new_with(db, env.krate);
    let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);

    let ItemContainerId::TraitId(trait_id) = func.loc(db).container else {
        return (Either::Left(func), fn_subst);
    };
    let trait_params = GenericParams::of(db, trait_id.into()).len();
    let trait_ref = TraitRef::new_from_args(
        interner,
        trait_id.into(),
        GenericArgs::new_from_slice(&fn_subst[..trait_params]),
    );

    let name = &FunctionSignature::of(db, func).name;
    let Some((impl_fn, impl_subst)) =
        lookup_impl_assoc_item_for_trait_ref(&infcx, trait_ref, env.param_env, name).and_then(
            |(assoc, impl_args)| {
                let assoc = match assoc {
                    Either::Left(AssocItemId::FunctionId(id)) => Either::Left(id),
                    Either::Right(it) => Either::Right(it),
                    _ => return None,
                };
                Some((assoc, impl_args))
            },
        )
    else {
        return (Either::Left(func), fn_subst);
    };

    (
        impl_fn,
        GenericArgs::new_from_iter(impl_subst.iter().chain(fn_subst.iter().skip(trait_params))),
    )
}

fn lookup_impl_assoc_item_for_trait_ref<'db>(
    infcx: &InferCtxt<'db>,
    trait_ref: TraitRef<'db>,
    env: ParamEnv<'db>,
    name: &Name,
) -> Option<(Either<AssocItemId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>)>
{
    let (impl_id, impl_subst) = find_matching_impl(infcx, env, trait_ref)?;
    let impl_id = match impl_id {
        AnyImplId::ImplId(it) => it,
        AnyImplId::BuiltinDeriveImplId(impl_) => {
            return impl_
                .loc(infcx.interner.db)
                .trait_
                .get_method(name.symbol())
                .map(|method| (Either::Right((impl_, method)), impl_subst));
        }
    };
    let item_of = |impl_id: ImplId| {
        impl_id.impl_items(infcx.interner.db).items.iter().find_map(|(n, it)| match *it {
            AssocItemId::FunctionId(f) => (n == name).then_some(AssocItemId::FunctionId(f)),
            AssocItemId::ConstId(c) => (n == name).then_some(AssocItemId::ConstId(c)),
            AssocItemId::TypeAliasId(_) => None,
        })
    };
    if let Some(item) = item_of(impl_id) {
        return Some((Either::Left(item), impl_subst));
    }
    // A specializing impl takes the items it leaves out from the impl it specializes: the
    // closest one that has the item.
    let db = infcx.interner.db;
    let parent =
        crate::specialization::specialized_ancestor(db, impl_id, |it| item_of(it).is_some())?;
    let parent_args = infcx.fresh_args_for_item(Span::Dummy, parent.into());
    let parent_trait_ref =
        db.impl_trait(parent)?.instantiate(infcx.interner, parent_args).skip_norm_wip();
    let mut ocx = ObligationCtxt::new(infcx);
    ocx.eq(&ObligationCause::dummy(), env, trait_ref, parent_trait_ref).ok()?;
    let parent_args = infcx.resolve_vars_if_possible(parent_args);
    if parent_args.has_non_region_infer() {
        return None;
    }
    let parent_args = parent_args.fold_with(&mut ReplaceInferWithError::new(infcx.interner));
    Some((Either::Left(item_of(parent)?), parent_args))
}

pub(crate) fn find_matching_impl<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
    trait_ref: TraitRef<'db>,
) -> Option<(AnyImplId, GenericArgs<'db>)> {
    let trait_ref = infcx.at(&ObligationCause::dummy(), env).deeply_normalize(trait_ref).ok()?;

    let obligation = Obligation::new(infcx.interner, ObligationCause::dummy(), env, trait_ref);

    let selection = infcx.select(&obligation).ok()??;

    // Currently, we use a fulfillment context to completely resolve
    // all nested obligations. This is because they can inform the
    // inference of the impl's type parameters.
    let mut ocx = ObligationCtxt::new(infcx);
    let impl_source = selection.map(|obligation| ocx.register_obligation(obligation));

    let errors = ocx.evaluate_obligations_error_on_ambiguity();
    if !errors.is_empty() {
        return None;
    }

    let impl_source = infcx.resolve_vars_if_possible(impl_source);
    if impl_source.has_non_region_infer() {
        return None;
    }

    // Selection may leave region inference variables unresolved; replace them before they escape
    // this inference context.
    //
    // FIXME: decide whether inferred regions should be replaced with error or erased.
    match impl_source {
        ImplSource::UserDefined(impl_source) => Some((
            impl_source.impl_def_id,
            impl_source.args.fold_with(&mut ReplaceInferWithError::new(infcx.interner)),
        )),
        ImplSource::Param(_) | ImplSource::Builtin(..) => None,
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
#[salsa::tracked(returns(deref))]
pub fn loaded_crates(db: &dyn HirDatabase, krate: Crate) -> Box<[Crate]> {
    let _p = tracing::info_span!("loaded_crates").entered();
    let data = krate.data(db);
    let mut unnamed: Vec<_> = data
        .dependencies
        .iter()
        .filter(|dep| {
            let data = dep.crate_id.data(db);
            !(dep.is_sysroot() || data.origin.is_lang() || data.target_kind.is_proc_macro())
        })
        .collect();
    // Few files name a dependency, and a file's text tells which may without its tree. A file
    // `include!` puts among the items is one of the crate's as well: a root that is only
    // `include!("../../app/main.rs");` names its dependencies there.
    let included = hir_def::include_macro_invoc(db, krate);
    let def_map = crate_def_map(db, krate);
    let mut files = def_map
        .modules(db)
        .filter_map(|(_, module)| module.origin.file_id())
        .chain(included.iter().map(|&(_, file)| file));
    let named_in = |unnamed: &mut Vec<&base_db::BuiltDependency>, root: &syntax::SyntaxNode| {
        let root = root.to_flat();
        let (tree, at) = root.flat().expect("a flat tree");
        let identifiers: FxHashSet<&str> = (at..tree.end(at))
            .filter(|&it| tree.is_token(it) && tree.kind(it).0 == SyntaxKind::IDENT as u16)
            .map(|it| tree.token_text(it))
            .collect();
        unnamed.retain(|dep| !identifiers.contains(dep.name.symbol().as_str()));
    };
    while !unnamed.is_empty()
        && let Some(file) = files.next()
    {
        let text = db.file_text(file.file_id(db)).text(db);
        if !unnamed.iter().any(|dep| text.contains(dep.name.symbol().as_str())) {
            continue;
        }
        named_in(&mut unnamed, &file.parse(db).syntax_node());
    }
    // rustc loads the crate a path in a macro's output names too, as in the `impl
    // dep::Trait for ..` a derive of a proc macro writes. A `macro_rules!` of `krate` writes
    // nothing its files do not, nor does a built-in macro but `include!`, whose files are
    // above.
    let mut calls = def_map
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).all_macro_calls().collect::<Vec<_>>())
        .filter(|call| {
            let def = call.loc(db).def;
            def.krate != krate
                && matches!(
                    def.kind,
                    hir_expand::MacroDefKind::ProcMacro(..)
                        | hir_expand::MacroDefKind::Declarative(..)
                )
        });
    while !unnamed.is_empty()
        && let Some(call) = calls.next()
    {
        named_in(&mut unnamed, &call.parse_macro_expansion(db).value.0.syntax_node());
    }
    let mut loaded = FxHashSet::default();
    let mut ordered = vec![krate];
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
    ordered.into()
}

/// The items of `scope` whose bodies may hold impls: an impl is one of its crate wherever it
/// is written. Custom derives write theirs in a const, `const _: () = { ... };` or one with a
/// name, and those are looked into in every crate. A function can hold one as well, of a type
/// that is not its own (zerocopy's tests `impl KnownLayout for elain::Align<8>` in a test), and
/// a workspace crate's functions are all looked into. Lowering every body of every dependency
/// for this is too much, so a dependency's function is looked into when lexing its file finds
/// an impl in it (bincode's `deserialize_enum` holds its `EnumAccess` for `&mut Deserializer`).
fn impl_holders(db: &dyn HirDatabase, krate: Crate, scope: &ItemScope) -> Vec<DefWithBodyId> {
    let mut holders: Vec<DefWithBodyId> = scope.consts().map(DefWithBodyId::from).collect();
    let local = krate.data(db).origin.is_local();
    let holds = |it: FunctionId| local || holds_impl(db, krate, it);
    let declared = scope.declarations().filter_map(|def| match def {
        ModuleDefId::FunctionId(it) if holds(it) => Some(DefWithBodyId::from(it)),
        ModuleDefId::StaticId(it) if local => Some(DefWithBodyId::from(it)),
        _ => None,
    });
    holders.extend(declared);
    let in_impls =
        scope.impls().flat_map(|it| &it.impl_items(db).items).filter_map(|(_, item)| match *item {
            AssocItemId::FunctionId(it) if holds(it) => Some(DefWithBodyId::from(it)),
            AssocItemId::ConstId(it) if local => Some(DefWithBodyId::from(it)),
            _ => None,
        });
    holders.extend(in_impls);
    holders
}

/// Whether an impl of [`impls_in_bodies`] is in the text of `function`, or a call of one of
/// `krate`'s [`macros_writing_impls`]. A body containing both an import and a macro call must
/// also be lowered: the import may rename a macro, and body imports are not resolved yet.
/// The file is parsed for the function's range only when its lexing found a candidate.
fn holds_impl(db: &dyn HirDatabase, krate: Crate, function: FunctionId) -> bool {
    let ast_id = function.loc(db).id;
    let Some(file) = ast_id.file_id.file_id() else { return false };
    let at = impls_in_bodies(db, file);
    if at.is_empty() {
        return false;
    }
    let range = ast_id.file_id.ast_id_map(db).get(ast_id.value).text_range();
    let mut has_import = false;
    let mut has_macro = false;
    for (at, candidate) in at {
        if !range.contains(*at) {
            continue;
        }
        match candidate {
            BodyImplCandidate::Impl => return true,
            BodyImplCandidate::Import => has_import = true,
            BodyImplCandidate::Macro(name) => {
                if macros_writing_impls(db, krate).contains(name) {
                    return true;
                }
                has_macro = true;
            }
        }
    }
    has_import && has_macro
}

#[derive(Debug, PartialEq, Eq, SalsaValue)]
enum BodyImplCandidate {
    Impl,
    Import,
    Macro(Symbol),
}

/// Where `file` writes an impl in the body of a function, found by its tokens: an `impl` after
/// `{`, `}`, `;`, an attribute or `unsafe`, inside the braces that follow `fn name`. A macro's
/// `impl` is found as well, which costs a lowering and nothing else. A macro called in a body
/// is found with its name, for an impl the macro writes. Imports are recorded too, since
/// they can introduce aliases for macros that are only resolved when lowering the body.
#[salsa::tracked(returns(deref))]
fn impls_in_bodies(
    db: &dyn HirDatabase,
    file: base_db::EditionedFileId,
) -> Box<[(TextSize, BodyImplCandidate)]> {
    let text = db.file_text(file.file_id(db)).text(db);
    let lexed = syntax::LexedStr::new(file.edition(db), text);
    // For each open brace, whether it is a function's body.
    let mut braces = Vec::new();
    let mut bodies = 0usize;
    // The parentheses and brackets open: a `;` in `-> [u8; 4]` does not end the signature.
    let mut nested = 0usize;
    let mut signature = None;
    let mut previous = SyntaxKind::EOF;
    let mut previous_at = 0;
    let mut found = Vec::new();
    for i in 0..lexed.len() {
        let kind = lexed.kind(i);
        if kind.is_trivia() {
            continue;
        }
        match kind {
            SyntaxKind::IDENT if previous == T![fn] => signature = Some(nested),
            T!['('] | T!['['] => nested += 1,
            T![')'] | T![']'] => nested = nested.saturating_sub(1),
            T![;] if signature == Some(nested) => signature = None,
            T!['{'] => {
                let body = signature.take().is_some();
                bodies += usize::from(body);
                braces.push(body);
            }
            T!['}'] => bodies -= usize::from(braces.pop().unwrap_or(false)),
            T![impl]
                if bodies > 0
                    && matches!(previous, T!['{'] | T!['}'] | T![;] | T![']'] | T![unsafe]) =>
            {
                found.push((TextSize::from(lexed.text_start(i) as u32), BodyImplCandidate::Impl));
            }
            T![use] if bodies > 0 => {
                found.push((TextSize::from(lexed.text_start(i) as u32), BodyImplCandidate::Import));
            }
            T![!] if bodies > 0 && previous == SyntaxKind::IDENT => {
                let name = Symbol::intern(lexed.text(previous_at));
                found.push((
                    TextSize::from(lexed.text_start(previous_at) as u32),
                    BodyImplCandidate::Macro(name),
                ));
            }
            _ => {}
        }
        previous = kind;
        previous_at = i;
    }
    found.into()
}

/// The `macro_rules!` of `krate` and of its dependencies that write an impl, in their own text
/// or by calling another that does: dtoa's `dtoa!`, called in the body of `<f64 as
/// Sealed>::write`, calls `diyfp!`, which writes `impl DiyFp` there. Proc macros can write
/// impls as well; include their visible names, including imports and reexports, without
/// trying to infer their output from their Rust source. Known by name, so a macro of the
/// same name costs a lowering and nothing else.
#[salsa::tracked(returns(deref))]
fn macros_writing_impls(db: &dyn HirDatabase, krate: Crate) -> Box<[Symbol]> {
    let mut writing: Vec<Symbol> = krate
        .data(db)
        .dependencies
        .iter()
        .flat_map(|dep| macros_writing_impls(db, dep.crate_id).iter().cloned())
        .collect();
    let mut calling = Vec::new();
    let mut files = FxHashSet::default();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for (name, item) in module.scope(db).macros() {
            if matches!(item.def, MacroId::ProcMacroId(_)) {
                writing.push(name.symbol().clone());
            }
        }
        let Some(file) = module.origin.file_id() else { continue };
        if !files.insert(file) {
            continue;
        }
        for (name, writes, calls) in macro_definitions(db, file) {
            match writes {
                true => writing.push(name.clone()),
                false => calling.push((name, calls)),
            }
        }
    }
    loop {
        let before = writing.len();
        calling.retain(|&(name, calls)| {
            let writes = calls.iter().any(|it| writing.contains(it));
            if writes {
                writing.push(name.clone());
            }
            !writes
        });
        if writing.len() == before {
            break;
        }
    }
    writing.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    writing.dedup();
    writing.into()
}

/// The `macro_rules!` of `file`, found by its tokens: each one's name, whether its text writes
/// an impl (an `impl` after `{`, `}`, `;`, `(`, `*`, an attribute or `unsafe`) and the names of
/// the macros it calls.
#[salsa::tracked(returns(deref))]
fn macro_definitions(
    db: &dyn HirDatabase,
    file: base_db::EditionedFileId,
) -> Box<[(Symbol, bool, Box<[Symbol]>)]> {
    let text = db.file_text(file.file_id(db)).text(db);
    if !text.contains("macro_rules") {
        return Box::default();
    }
    let lexed = syntax::LexedStr::new(file.edition(db), text);
    let tokens: Vec<usize> = (0..lexed.len()).filter(|&i| !lexed.kind(i).is_trivia()).collect();
    let kind = |at: usize| tokens.get(at).map_or(SyntaxKind::EOF, |&i| lexed.kind(i));
    let mut found = Vec::new();
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
        let (mut writes, mut calls, mut depth) = (false, Vec::new(), 0usize);
        at += 3;
        while at < tokens.len() {
            match kind(at) {
                T!['{'] | T!['('] | T!['['] => depth += 1,
                T!['}'] | T![')'] | T![']'] => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                T![impl]
                    if matches!(
                        kind(at - 1),
                        T!['{'] | T!['}'] | T![;] | T!['('] | T![*] | T![']'] | T![unsafe]
                    ) =>
                {
                    writes = true
                }
                T![!] if kind(at - 1) == SyntaxKind::IDENT && kind(at - 2) != T![$] => {
                    calls.push(Symbol::intern(lexed.text(tokens[at - 1])))
                }
                _ => {}
            }
            at += 1;
        }
        found.push((name, writes, calls.into()));
        at += 1;
    }
    found.into()
}

/// What finding the impls of `krate` asks of each impl and of each body that may hold one.
/// Each is its own query, so [`ImplWork::run`] on many threads leaves the finding little to do.
pub fn impl_work(db: &dyn HirDatabase, krate: Crate) -> Vec<ImplWork> {
    let def_map = crate_def_map(db, krate);
    let mut work = Vec::new();
    for (_, module) in def_map.modules(db) {
        let scope = module.scope(db);
        work.extend(scope.impls().map(ImplWork::Impl));
        work.extend(impl_holders(db, krate, scope).into_iter().map(ImplWork::Holder));
    }
    work
}

#[derive(Clone, Copy)]
pub enum ImplWork {
    Impl(ImplId),
    Holder(DefWithBodyId),
}

impl ImplWork {
    pub fn run(self, db: &dyn HirDatabase) {
        match self {
            ImplWork::Impl(id) => {
                _ = db.impl_self_ty(id);
                _ = db.impl_trait(id);
            }
            ImplWork::Holder(owner) => _ = Body::of(db, owner),
        }
    }
}

#[salsa::tracked(returns(ref))]
fn crates_containing_incoherent_inherent_impls(db: &dyn HirDatabase, krate: Crate) -> Box<[Crate]> {
    let _p = tracing::info_span!("crates_containing_incoherent_inherent_impls").entered();
    // We assume that only sysroot crates contain `#[rustc_has_incoherent_inherent_impls]`
    // impls, since this is an internal feature and only std uses it.
    krate.transitive_deps(db).into_iter().filter(|krate| krate.data(db).origin.is_lang()).collect()
}

pub fn with_incoherent_inherent_impls<'db>(
    db: &'db dyn HirDatabase,
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
        let impls = InherentImpls::for_crate(db, krate);
        callback(impls.for_self_ty(self_ty));
    }
}

pub fn simplified_type_module(db: &dyn HirDatabase, ty: &SimplifiedType<'_>) -> Option<ModuleId> {
    match ty.def()? {
        SolverDefId::AdtId(id) => Some(id.module(db)),
        SolverDefId::TypeAliasId(id) => Some(id.module(db)),
        SolverDefId::TraitId(id) => Some(id.module(db)),
        _ => None,
    }
}

/// A `SimplifiedType` as the argument of a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SalsaValue)]
pub struct SelfTyKey<'db>(
    // SAFETY: as for the maps below, it only contains `SolverDefId<'db>`.
    #[salsa_value(unsafe(prove(SolverDefId<'db>: SalsaValue)))] pub SimplifiedType<'db>,
);

#[derive(Debug, PartialEq, Eq, SalsaValue)]
pub struct InherentImpls<'db> {
    // SAFETY: necessary due to `SimplifiedType<'db>`.
    // It's safe to retain, as it only contains `SolverDefId<'db>` (which is `SalsaValue`),
    // and no `&'db` references.
    #[salsa_value(unsafe(prove(SolverDefId<'db>: SalsaValue)))]
    map: FxHashMap<SimplifiedType<'db>, Box<[ImplId]>>,
}

#[salsa::tracked]
impl<'db> InherentImpls<'db> {
    #[salsa::tracked(returns(ref))]
    pub fn for_crate(db: &'db dyn HirDatabase, krate: Crate) -> InherentImpls<'db> {
        let _p = tracing::info_span!("inherent_impls_in_crate_query", ?krate).entered();
        if let Some(frozen) = frozen::inherent_impls(db, krate) {
            return frozen;
        }

        let crate_def_map = crate_def_map(db, krate);

        Self::collect_def_map(db, crate_def_map)
    }

    /// The inherent impls of `krate` for one type. An impl added for another type changes
    /// `for_crate()`, and leaves this as it was.
    #[salsa::tracked(returns(deref))]
    pub fn in_crate_for_self_ty(
        db: &'db dyn HirDatabase,
        krate: Crate,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ImplId]> {
        Self::for_crate(db, krate).for_self_ty(&self_ty.0).into()
    }

    #[salsa::tracked(returns(ref))]
    pub fn for_block(
        db: &'db dyn HirDatabase,
        block: BlockIdLt<'db>,
    ) -> Option<Box<InherentImpls<'db>>> {
        let _p = tracing::info_span!("inherent_impls_in_block_query").entered();

        let block_def_map = block_def_map(db, block);
        let result = Self::collect_def_map(db, block_def_map);
        if result.map.is_empty() { None } else { Some(Box::new(result)) }
    }
}

impl<'db> InherentImpls<'db> {
    fn collect_def_map(db: &'db dyn HirDatabase, def_map: &'db DefMap) -> Self {
        let mut map = FxHashMap::default();
        collect(db, def_map, &mut map);
        let mut map = map
            .into_iter()
            .map(|(self_ty, impls)| (self_ty, impls.into_boxed_slice()))
            .collect::<FxHashMap<_, _>>();
        map.shrink_to_fit();
        return Self { map };

        fn collect<'db>(
            db: &'db dyn HirDatabase,
            def_map: &DefMap,
            map: &mut FxHashMap<SimplifiedType<'db>, Vec<ImplId>>,
        ) {
            for (_module_id, module_data) in def_map.modules(db) {
                for impl_id in module_data.scope(db).inherent_impls() {
                    let interner = DbInterner::new_no_crate(db);
                    let self_ty = db.impl_self_ty(impl_id);
                    let self_ty = self_ty.instantiate_identity().skip_norm_wip();
                    if let Some(self_ty) =
                        simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer)
                    {
                        map.entry(self_ty).or_default().push(impl_id);
                    }
                }

                for owner in impl_holders(db, def_map.krate(), module_data.scope(db)) {
                    for (_, block_def_map) in Body::of(db, owner).blocks(db) {
                        collect(db, block_def_map, map);
                    }
                }
            }
        }
    }

    pub fn for_self_ty(&self, self_ty: &SimplifiedType<'db>) -> &[ImplId] {
        self.map.get(self_ty).map(|it| &**it).unwrap_or_default()
    }

    pub fn for_each_crate_and_block(
        db: &'db dyn HirDatabase,
        krate: Crate,
        block: Option<BlockIdLt<'db>>,
        for_each: &mut dyn FnMut(&InherentImpls<'db>),
    ) {
        let blocks = std::iter::successors(block, |block| block.module(db).block(db));
        blocks.filter_map(|block| Self::for_block(db, block).as_deref()).for_each(&mut *for_each);
        for_each(Self::for_crate(db, krate));
    }
}

#[derive(Debug, PartialEq, SalsaValue)]
struct OneTraitImpls<'db> {
    // SAFETY: necessary due to `SimplifiedType<'db>`.
    // It's safe to retain, as it only contains `SolverDefId<'db>` (which is `SalsaValue`),
    // and no `&'db` references.
    #[salsa_value(unsafe(prove(SolverDefId<'db>: SalsaValue)))]
    non_blanket_impls: FxHashMap<SimplifiedType<'db>, (Box<[ImplId]>, Box<[BuiltinDeriveImplId]>)>,
    blanket_impls: Box<[ImplId]>,
}

#[derive(Default)]
struct OneTraitImplsBuilder<'db> {
    non_blanket_impls: FxHashMap<SimplifiedType<'db>, (Vec<ImplId>, Vec<BuiltinDeriveImplId>)>,
    blanket_impls: Vec<ImplId>,
}

impl<'db> OneTraitImplsBuilder<'db> {
    fn finish(self) -> OneTraitImpls<'db> {
        let mut non_blanket_impls = self
            .non_blanket_impls
            .into_iter()
            .map(|(self_ty, (impls, builtin_derive_impls))| {
                (self_ty, (impls.into_boxed_slice(), builtin_derive_impls.into_boxed_slice()))
            })
            .collect::<FxHashMap<_, _>>();
        non_blanket_impls.shrink_to_fit();
        let blanket_impls = self.blanket_impls.into_boxed_slice();
        OneTraitImpls { non_blanket_impls, blanket_impls }
    }
}

#[derive(Debug, PartialEq, SalsaValue)]
pub struct TraitImpls<'db> {
    map: FxHashMap<TraitId, OneTraitImpls<'db>>,
}

#[salsa::tracked]
impl<'db> TraitImpls<'db> {
    #[salsa::tracked(returns(ref))]
    pub fn for_crate(db: &'db dyn HirDatabase, krate: Crate) -> Arc<TraitImpls<'db>> {
        let _p = tracing::info_span!("inherent_impls_in_crate_query", ?krate).entered();
        if let Some(frozen) = frozen::trait_impls(db, krate) {
            return Arc::new(frozen);
        }

        let crate_def_map = crate_def_map(db, krate);
        let result = Self::collect_def_map(db, crate_def_map);
        Arc::new(result)
    }

    #[salsa::tracked(returns(as_deref))]
    pub fn for_block(
        db: &'db dyn HirDatabase,
        block: BlockIdLt<'db>,
    ) -> Option<Box<TraitImpls<'db>>> {
        let _p = tracing::info_span!("inherent_impls_in_block_query").entered();

        let block_def_map = block_def_map(db, block);
        let result = Self::collect_def_map(db, block_def_map);
        if result.map.is_empty() { None } else { Some(Box::new(result)) }
    }

    #[salsa::tracked(returns(deref))]
    pub fn for_crate_and_deps(db: &'db dyn HirDatabase, krate: Crate) -> Box<[Arc<Self>]> {
        loaded_crates(db, krate).iter().map(|&dep| Self::for_crate(db, dep).clone()).collect()
    }

    /// The impls of one trait for one type in `krate` and its dependencies. An impl added for
    /// another trait or type changes `for_crate()`, and leaves this as it was.
    #[salsa::tracked(returns(deref))]
    pub fn in_crate_and_deps_for_trait_and_self_ty(
        db: &'db dyn HirDatabase,
        krate: Crate,
        trait_: TraitId,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[AnyImplId]> {
        let mut found = Vec::new();
        for impls in Self::for_crate_and_deps(db, krate) {
            let (impls, builtin_derive_impls) = impls.for_trait_and_self_ty(trait_, &self_ty.0);
            found.extend(impls.iter().map(|&it| AnyImplId::from(it)));
            found.extend(builtin_derive_impls.iter().map(|&it| AnyImplId::from(it)));
        }
        found.into()
    }

    /// The blanket impls of one trait in `krate` and its dependencies.
    #[salsa::tracked(returns(deref))]
    pub fn blanket_in_crate_and_deps(
        db: &'db dyn HirDatabase,
        krate: Crate,
        trait_: TraitId,
    ) -> Box<[ImplId]> {
        Self::for_crate_and_deps(db, krate)
            .iter()
            .flat_map(|impls| impls.blanket_impls(trait_))
            .copied()
            .collect()
    }
}

impl<'db> TraitImpls<'db> {
    fn collect_def_map(db: &'db dyn HirDatabase, def_map: &DefMap) -> Self {
        let lang_items = hir_def::lang_item::lang_items(db, def_map.krate());
        let mut map = FxHashMap::default();
        collect(db, def_map, lang_items, &mut map);
        let mut map = map
            .into_iter()
            .map(|(trait_id, trait_map)| (trait_id, trait_map.finish()))
            .collect::<FxHashMap<_, _>>();
        map.shrink_to_fit();
        return Self { map };

        fn collect<'db>(
            db: &'db dyn HirDatabase,
            def_map: &DefMap,
            lang_items: &LangItems,
            map: &mut FxHashMap<TraitId, OneTraitImplsBuilder<'db>>,
        ) {
            for (_module_id, module_data) in def_map.modules(db) {
                for impl_id in module_data.scope(db).trait_impls() {
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
                    if AttrFlags::query(db, impl_id.into())
                        .contains(AttrFlags::RUSTC_RESERVATION_IMPL)
                    {
                        continue;
                    }

                    let self_ty = trait_ref.self_ty();
                    if self_ty_has_error_constructor(self_ty) {
                        // If we see `impl Foo for NoSuchType`, just ignore it.
                        continue;
                    }

                    let interner = DbInterner::new_no_crate(db);
                    let entry = map.entry(trait_ref.def_id.0).or_default();
                    match simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer) {
                        Some(self_ty) => {
                            entry.non_blanket_impls.entry(self_ty).or_default().0.push(impl_id)
                        }
                        None => entry.blanket_impls.push(impl_id),
                    }
                }

                for impl_id in module_data.scope(db).builtin_derive_impls() {
                    let loc = impl_id.loc(db);
                    let Some(trait_id) = loc.trait_.get_id(lang_items) else { continue };
                    let entry = map.entry(trait_id).or_default();
                    let entry = entry
                        .non_blanket_impls
                        .entry(SimplifiedType::Adt(loc.adt.into()))
                        .or_default();
                    entry.1.push(impl_id);
                }

                for owner in impl_holders(db, def_map.krate(), module_data.scope(db)) {
                    for (_, block_def_map) in Body::of(db, owner).blocks(db) {
                        collect(db, block_def_map, lang_items, map);
                    }
                }
            }
        }
    }

    pub fn blanket_impls(&self, for_trait: TraitId) -> &[ImplId] {
        self.map.get(&for_trait).map(|it| &*it.blanket_impls).unwrap_or_default()
    }

    /// Queries whether `self_ty` has potentially applicable implementations of `trait_`.
    pub fn has_impls_for_trait_and_self_ty(
        &self,
        trait_: TraitId,
        self_ty: &SimplifiedType<'db>,
    ) -> bool {
        self.map.get(&trait_).is_some_and(|trait_impls| {
            trait_impls.non_blanket_impls.contains_key(self_ty)
                || !trait_impls.blanket_impls.is_empty()
        })
    }

    pub fn for_trait_and_self_ty(
        &'db self,
        trait_: TraitId,
        self_ty: &SimplifiedType<'db>,
    ) -> (&'db [ImplId], &'db [BuiltinDeriveImplId]) {
        self.map
            .get(&trait_)
            .and_then(|map| map.non_blanket_impls.get(self_ty))
            .map(|it| (&*it.0, &*it.1))
            .unwrap_or_default()
    }

    pub fn for_trait(
        &self,
        trait_: TraitId,
        mut callback: impl FnMut(Either<&[ImplId], &[BuiltinDeriveImplId]>),
    ) {
        if let Some(impls) = self.map.get(&trait_) {
            callback(Either::Left(&impls.blanket_impls));
            for impls in impls.non_blanket_impls.values() {
                callback(Either::Left(&impls.0));
                callback(Either::Right(&impls.1));
            }
        }
    }

    pub fn for_self_ty(
        &self,
        self_ty: &SimplifiedType<'db>,
        mut callback: impl FnMut(Either<&[ImplId], &[BuiltinDeriveImplId]>),
    ) {
        for for_trait in self.map.values() {
            if let Some(for_ty) = for_trait.non_blanket_impls.get(self_ty) {
                callback(Either::Left(&for_ty.0));
                callback(Either::Right(&for_ty.1));
            }
        }
    }

    pub fn for_each_crate_and_block<R: VisitorResult>(
        db: &'db dyn HirDatabase,
        krate: Crate,
        block: Option<BlockIdLt<'db>>,
        for_each: &mut dyn FnMut(&TraitImpls<'db>) -> R,
    ) -> R {
        try_visit!(Self::for_each_block(db, block, for_each));
        for impl_ in Self::for_crate_and_deps(db, krate) {
            try_visit!(for_each(impl_));
        }
        R::output()
    }

    /// The impls of `block` and of the blocks around it.
    pub fn for_each_block<R: VisitorResult>(
        db: &'db dyn HirDatabase,
        block: Option<BlockIdLt<'db>>,
        for_each: &mut dyn FnMut(&TraitImpls<'db>) -> R,
    ) -> R {
        let blocks = std::iter::successors(block, |block| block.module(db).block(db));
        for impl_ in blocks.filter_map(|block| Self::for_block(db, block)) {
            try_visit!(for_each(impl_));
        }
        R::output()
    }

    /// Like [`Self::for_each_crate_and_block()`], but takes in account two blocks, one for a trait and one for a self type.
    pub fn for_each_crate_and_block_trait_and_type<R: VisitorResult>(
        db: &'db dyn HirDatabase,
        krate: Crate,
        type_block: Option<BlockIdLt<'db>>,
        trait_block: Option<BlockIdLt<'db>>,
        for_each: &mut dyn FnMut(&TraitImpls<'db>) -> R,
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
        db: &'db dyn HirDatabase,
        type_block: Option<BlockIdLt<'db>>,
        trait_block: Option<BlockIdLt<'db>>,
        for_each: &mut dyn FnMut(&TraitImpls<'db>) -> R,
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
