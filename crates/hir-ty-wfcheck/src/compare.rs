//! The type comparison of rustc's `compare_method_clause_entailment`
//! (`rustc_hir_analysis/src/check/compare_impl_item.rs`): the signature of a trait's method,
//! instantiated with a trait impl's arguments and normalized in the impl method's
//! environment, must be a supertype of the impl method's (E0053). Also the trait method's
//! signature as an impl's method would have it, for rustc's suggestion for `_` in an impl
//! method (`suggest_trait_fn_ty_for_impl_fn_infer`, E0121), and a proc macro function's
//! signature compared with `fn(TokenStream) -> TokenStream` (`check_proc_macro`).

use base_db::Db;
use hir_def::{
    CallableDefId, FunctionId, HasModule as _, ImplId, ItemContainerId, Lookup as _, TraitId,
    hir::generics::{GenericParams, TypeOrConstParamData, TypeParamProvenance},
    signatures::FunctionSignature,
};
use rustc_type_ir::{
    error::{ExpectedFound, TypeError},
    inherent::{GenericArgs as _, IntoKind as _},
};
use hir_ty::{
    GenericPredicates, Span,
    db::HirDatabase as _,
    next_solver::{
        Clauses, DbInterner, FnSig, GenericArgKind, GenericArgs, ParamEnv, Region,
        RegionExt as _, RegionKind, SolverDefId, Ty, TypingMode, references_non_lt_error,
        infer::{BoundRegionConversionTime, DbInternerInferExt as _, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
};

/// Where the signatures of an impl's method and its trait's differ: rustc's `TypeError` of
/// relating them (`extract_spans_for_error_reporting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureMismatchAt {
    /// The type of the parameter of this index, `self` included (`ArgumentSorts`).
    Param(u32),
    /// The mutability of the parameter of this index (`ArgumentMutability`): "types differ in
    /// mutability".
    ParamMutability(u32),
    /// The return type (`ArgumentSorts` of the last of `inputs_and_output`).
    Return,
    /// The mutability of the return type.
    ReturnMutability,
    /// Another number of parameters (`ArgCount`). Only [`compare_proc_macro_signature`] gives
    /// it: an impl method with another number is E0050, checked before.
    ParamCount,
    /// Another difference, of `unsafe`, the ABI or `...`: rustc reports it at the impl
    /// method's span.
    Signature,
}

impl SignatureMismatchAt {
    /// Where `error`, of relating two signatures with `inputs` parameters, is.
    fn of<'db>(error: &TypeError<DbInterner<'db>>, inputs: usize) -> SignatureMismatchAt {
        match *error {
            TypeError::ArgumentSorts(_, i) if i == inputs => SignatureMismatchAt::Return,
            TypeError::ArgumentSorts(_, i) => SignatureMismatchAt::Param(i as u32),
            TypeError::ArgumentMutability(i) if i == inputs => {
                SignatureMismatchAt::ReturnMutability
            }
            TypeError::ArgumentMutability(i) => SignatureMismatchAt::ParamMutability(i as u32),
            TypeError::ArgCount => SignatureMismatchAt::ParamCount,
            _ => SignatureMismatchAt::Signature,
        }
    }
}

/// The first difference between a trait impl's method's signature and its trait's, as
/// rustc's E0053 reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureMismatch<'db> {
    pub trait_method: FunctionId,
    pub at: SignatureMismatchAt,
    /// For [`SignatureMismatchAt::Param`] and [`SignatureMismatchAt::Return`], the trait's type
    /// there (`expected`) and the impl's (`found`).
    pub types: Option<ExpectedFound<Ty<'db>>>,
    /// The trait's signature, instantiated and normalized: rustc's "expected signature".
    pub expected: FnSig<'db>,
    /// The impl's signature, normalized: rustc's "found signature". The lifetimes its
    /// `for<'a>` binds are erased.
    pub found: FnSig<'db>,
}

/// The method of the trait `impl_method` implements, with the trait and the impl, when
/// `impl_method` is a method of a trait impl that the trait has a method of its name for.
fn trait_method_of(
    db: &Db,
    impl_method: FunctionId,
) -> Option<(ImplId, TraitId, FunctionId)> {
    let ItemContainerId::ImplId(impl_) = impl_method.lookup(db).container else { return None };
    let trait_ref = db.impl_trait(impl_)?;
    let trait_ = trait_ref.skip_binder().def_id.0;
    let name = &FunctionSignature::of(db, impl_method).name;
    let trait_method = trait_.trait_items(db).method_by_name(name)?;
    Some((impl_, trait_, trait_method))
}

/// The signature of the trait method `impl_method` implements, instantiated with the impl's
/// trait arguments and with the trait method's own parameters, its `for<'a>` lifetimes
/// liberated onto `impl_method`: rustc's `suggest_trait_fn_ty_for_impl_fn_infer`, the types
/// it suggests for a `_` in an impl's method. `None` when `impl_method` implements no trait
/// method.
pub fn trait_signature_in_impl<'db>(
    db: &'db Db,
    impl_method: FunctionId,
) -> Option<(FunctionId, FnSig<'db>)> {
    let (impl_, trait_, trait_method) = trait_method_of(db, impl_method)?;
    let interner = DbInterner::new_no_crate(db);
    let trait_ref = db.impl_trait(impl_)?.instantiate_identity().skip_norm_wip();
    let args = GenericArgs::identity_for_item(interner, SolverDefId::FunctionId(trait_method))
        .rebase_onto(interner, SolverDefId::TraitId(trait_), trait_ref.args);
    let sig = db
        .callable_item_signature(CallableDefId::FunctionId(trait_method))
        .instantiate(interner, args)
        .skip_norm_wip();
    Some((trait_method, interner.liberate_late_bound_regions(impl_method.into(), sig)))
}

/// rustc's E0053: the first place the signature of `impl_method`, a method of a trait impl,
/// differs from its trait method's, both normalized in the environment of the impl and the
/// trait method's own where clauses (rustc's hybrid environment). `None` when they agree,
/// when one has an error, and when the structural checks rustc makes first
/// (`check_method_is_structurally_compatible`: `self`, the number and kinds of generic
/// parameters, the number of parameters) fail, as each is an error of its own.
pub fn compare_impl_method_signature<'db>(
    db: &'db Db,
    impl_method: FunctionId,
) -> Option<SignatureMismatch<'db>> {
    // A method of an inherent impl, or one its trait lacks (E0407), has nothing to compare.
    let (impl_, _, trait_method) = trait_method_of(db, impl_method)?;
    let interner = DbInterner::new_with(db, impl_method.module(db).krate(db));
    let trait_ref = db.impl_trait(impl_)?.instantiate_identity().skip_norm_wip();
    // rustc's `compare_impl_item` stops at `trait_ref.error_reported()`, a type error already
    // reported. An error region is none: lowering leaves some lifetimes as error regions.
    if references_non_lt_error(&trait_ref) {
        return None;
    }

    // The own generic parameters of both, at the same places and of the same kinds, so the
    // impl method's stand for the trait method's.
    let own = |def: FunctionId, parent: SolverDefId<'db>| {
        let all = GenericArgs::identity_for_item(interner, SolverDefId::FunctionId(def));
        let parent = GenericArgs::identity_for_item(interner, parent).len();
        all.iter().skip(parent)
    };
    let kind = |arg: GenericArgKind<'db>| match arg {
        GenericArgKind::Lifetime(_) => 0u8,
        GenericArgKind::Type(_) => 1,
        GenericArgKind::Const(_) => 2,
    };
    // `compare_number_of_generics` and `compare_generic_param_kinds` (E0049, E0053 of a
    // const parameter's kind): each is its own error, and rustc compares no types after it.
    if !own(trait_method, SolverDefId::TraitId(trait_ref.def_id.0))
        .map(|it| kind(it.kind()))
        .eq(own(impl_method, SolverDefId::ImplId(impl_)).map(|it| kind(it.kind())))
    {
        return None;
    }
    // `compare_self_type` (E0185, E0186) and `compare_synthetic_generics` (E0643).
    if FunctionSignature::of(db, impl_method).has_self_param()
        != FunctionSignature::of(db, trait_method).has_self_param()
    {
        return None;
    }
    let synthetic = |def: FunctionId| {
        GenericParams::of(db, def.into())
            .iter_type_or_consts()
            .map(|(_, data)| {
                matches!(
                    data,
                    TypeOrConstParamData::TypeParamData(it)
                        if it.provenance == TypeParamProvenance::ArgumentImplTrait
                )
            })
    };
    if !synthetic(impl_method).eq(synthetic(trait_method)) {
        return None;
    }
    let trait_to_impl_args =
        GenericArgs::identity_for_item(interner, SolverDefId::FunctionId(impl_method))
            .rebase_onto(interner, SolverDefId::ImplId(impl_), trait_ref.args);

    let impl_poly_sig = db
        .callable_item_signature(CallableDefId::FunctionId(impl_method))
        .instantiate_identity()
        .skip_norm_wip();
    let trait_sig = db
        .callable_item_signature(CallableDefId::FunctionId(trait_method))
        .instantiate(interner, trait_to_impl_args)
        .skip_norm_wip();
    let trait_sig = interner.liberate_late_bound_regions(impl_method.into(), trait_sig);
    // `compare_number_of_method_arguments` (E0050), and a signature with a type error already
    // reported (rustc's `references_error` check before relating). An error region is none.
    if impl_poly_sig.skip_binder().inputs().len() != trait_sig.inputs().len()
        || references_non_lt_error(&impl_poly_sig)
        || references_non_lt_error(&trait_sig)
    {
        return None;
    }

    // The impl's where clauses, and the trait method's own instantiated for the impl's.
    let hybrid = GenericPredicates::query_all(db, impl_.into())
        .iter_identity()
        .map(|it| it.skip_norm_wip())
        .chain(
            GenericPredicates::query_own_explicit(db, trait_method.into())
                .iter_instantiated(interner, trait_to_impl_args.as_slice())
                .map(|it| it.skip_norm_wip()),
        );
    let param_env = ParamEnv::new(Clauses::new_from_iter(rustc_type_ir::elaborate::elaborate(
        interner, hybrid,
    )));

    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let mut ocx = ObligationCtxt::new(&infcx);
    let cause = ObligationCause::dummy();
    let impl_sig = infcx.instantiate_binder_with_fresh_vars(
        Span::Dummy,
        BoundRegionConversionTime::HigherRankedType,
        impl_poly_sig,
    );
    // rustc's solver normalizes eagerly: a projection that normalizes to the other's type
    // agrees with it. One that can't be normalized is compared as written.
    let impl_sig = ocx.deeply_normalize(&cause, param_env, impl_sig).unwrap_or(impl_sig);
    let trait_sig = ocx.deeply_normalize(&cause, param_env, trait_sig).unwrap_or(trait_sig);

    let error = ocx.sup(&cause, param_env, trait_sig, impl_sig).err()?;
    let at = SignatureMismatchAt::of(&error, trait_sig.inputs().len());
    let types = match error {
        TypeError::ArgumentSorts(types, _) => Some(types),
        _ => None,
    };
    // The impl's signature has no type variables, only the region variables of its `for<'a>`
    // lifetimes, which mean nothing outside this inference context.
    let erase = |region: Region<'db>, _: rustc_type_ir::DebruijnIndex| match region.kind() {
        RegionKind::ReVar(_) => Region::new_erased(interner),
        _ => region,
    };
    let found = rustc_type_ir::fold_regions(interner, impl_sig, erase);
    let types = types.map(|ExpectedFound { expected, found }| ExpectedFound {
        expected,
        found: rustc_type_ir::fold_regions(interner, found, erase),
    });
    Some(SignatureMismatch { trait_method, at, types, expected: trait_sig, found })
}

/// The first difference between a proc macro function's signature and the one rustc requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcMacroSignatureMismatch<'db> {
    pub at: SignatureMismatchAt,
    /// Safe Rust `fn(TokenStream) -> TokenStream`, with two `TokenStream`s for an attribute
    /// macro: rustc's "expected signature".
    pub expected: FnSig<'db>,
    /// The function's signature with its own generic parameters, normalized: rustc's "found
    /// signature".
    pub found: FnSig<'db>,
}

/// rustc's `check_proc_macro` (`rustc_passes/src/check_attr.rs`, `ProcMacroBadSig`): the
/// signature of `function`, a `#[proc_macro]` or `#[proc_macro_derive]` function (`inputs` 1)
/// or a `#[proc_macro_attribute]` one (`inputs` 2), its generic parameters fresh inference
/// variables and normalized, equated with safe Rust `fn(TokenStream, ..) -> TokenStream`.
/// `None` when they agree, when `function`'s crate sees no diagnostic item `TokenStream`, and
/// when proving the signature well-formed fails, as rustc's.
pub fn compare_proc_macro_signature<'db>(
    db: &'db Db,
    function: FunctionId,
    inputs: usize,
) -> Option<ProcMacroSignatureMismatch<'db>> {
    let krate = function.module(db).krate(db);
    // rustc's `check_proc_macro` returns where no `TokenStream` diagnostic item is in scope.
    let token_stream = match hir_def::diagnostic_item::diagnostic_items(db, krate)
        .get(&intern::Symbol::intern("TokenStream"))?
    {
        hir_def::AttrDefId::AdtId(adt) => adt,
        _ => return None,
    };
    let token_stream = db.ty(token_stream.into()).instantiate_identity().skip_norm_wip();

    let interner = DbInterner::new_with(db, krate);
    let param_env = ParamEnv::empty();
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let mut ocx = ObligationCtxt::new(&infcx);
    let cause = ObligationCause::dummy();
    let poly_sig = db.callable_item_signature(CallableDefId::FunctionId(function));
    let fresh_args = infcx.fresh_args_for_item(Span::Dummy, SolverDefId::FunctionId(function));
    let sig = interner.liberate_late_bound_regions(
        function.into(),
        poly_sig.instantiate(interner, fresh_args).skip_norm_wip(),
    );
    // A type error already reported, as rustc's `references_error` check; an error region is
    // none.
    if references_non_lt_error(&sig) {
        return None;
    }
    // rustc's: a signature that does not normalize or is not well-formed is an error of its own.
    let sig = ocx.deeply_normalize(&cause, param_env, sig).ok()?;
    if !ocx.try_evaluate_obligations().is_empty() {
        return None;
    }

    let expected = interner.mk_fn_sig_safe_rust_abi(
        std::iter::repeat_n(token_stream, inputs),
        token_stream,
    );
    let error = ocx.eq(&cause, param_env, expected, sig).err()?;
    let at = SignatureMismatchAt::of(&error, sig.inputs().len());
    let identity = interner.liberate_late_bound_regions(
        function.into(),
        poly_sig.instantiate_identity().skip_norm_wip(),
    );
    let found = ocx.deeply_normalize(&cause, param_env, identity).unwrap_or(identity);
    Some(ProcMacroSignatureMismatch { at, expected, found })
}
