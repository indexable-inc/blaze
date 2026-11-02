//! Compute the dyn-compatibility of a trait

use std::ops::ControlFlow;

use base_db::Db;
use hir_def::{
    AssocItemId, ConstId, FunctionId, GenericDefId, HasModule, ItemContainerId, Lookup, TraitId,
    TypeAliasId, TypeOrConstParamId, TypeParamId,
    hir::generics::{GenericParams, LocalTypeOrConstParamId},
    signatures::{FunctionSignature, TraitFlags, TraitSignature},
    unstable_features::UnstableFeatures,
};
use rustc_type_ir::{
    AliasTyKind, ClausePolarity, ClauseKind, TypeSuperVisitable as _, TypeVisitable as _,
    Upcast, elaborate, inherent::IntoKind,
};

use crate::{
    ImplTraitId, PredicateSource,
    db::HirDatabase,
    lower::{GenericPredicates, ImplTraitIdx},
    next_solver::{
        AliasTy, Binder, Clause, Clauses, DbInterner, EarlyBinder, GenericArgs, ParamEnv, ParamTy,
        SolverDefId, TraitClause, TraitRef, Ty, TypingMode, Unnormalized,
        infer::{
            DbInternerInferExt,
            traits::{Obligation, ObligationCause},
        },
        mk_param,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DynCompatibilityViolation {
    SizedSelf,
    SelfReferential,
    Method(FunctionId, MethodViolationCode),
    AssocConst(ConstId),
    GAT(TypeAliasId),
    // This doesn't exist in rustc, but added for better visualization
    HasNonCompatibleSuperTrait(TraitId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MethodViolationCode {
    StaticMethod,
    ReferencesSelfInput,
    ReferencesSelfOutput,
    ReferencesImplTraitInTrait,
    AsyncFn,
    WhereClauseReferencesSelf,
    Generic,
    UndispatchableReceiver,
}

pub fn dyn_compatibility(
    db: &Db,
    trait_: TraitId,
) -> Option<DynCompatibilityViolation> {
    for &super_trait in crate::utils::all_super_traits(db, trait_) {
        if let Some(v) = db.dyn_compatibility_of_trait(super_trait) {
            return if super_trait == trait_ {
                Some(v)
            } else {
                Some(DynCompatibilityViolation::HasNonCompatibleSuperTrait(super_trait))
            };
        }
    }

    None
}

pub fn dyn_compatibility_with_callback<F>(
    db: &Db,
    trait_: TraitId,
    cb: &mut F,
) -> ControlFlow<()>
where
    F: FnMut(DynCompatibilityViolation) -> ControlFlow<()>,
{
    for &super_trait in &crate::utils::all_super_traits(db, trait_)[1..] {
        if db.dyn_compatibility_of_trait(super_trait).is_some() {
            cb(DynCompatibilityViolation::HasNonCompatibleSuperTrait(super_trait))?;
        }
    }

    dyn_compatibility_of_trait_with_callback(db, trait_, cb)
}

pub fn dyn_compatibility_of_trait_with_callback<F>(
    db: &Db,
    trait_: TraitId,
    cb: &mut F,
) -> ControlFlow<()>
where
    F: FnMut(DynCompatibilityViolation) -> ControlFlow<()>,
{
    // Check whether this has a `Sized` bound
    if generics_require_sized_self(db, trait_.into()) {
        cb(DynCompatibilityViolation::SizedSelf)?;
    }

    // Check if there exist bounds that referencing self
    if predicates_reference_self(db, trait_) {
        cb(DynCompatibilityViolation::SelfReferential)?;
    }
    if bounds_reference_self(db, trait_) {
        cb(DynCompatibilityViolation::SelfReferential)?;
    }

    // rustc checks for non-lifetime binders here, but we don't support HRTB yet

    let trait_data = trait_.trait_items(db);
    let mut features = None;
    for (_, assoc_item) in &trait_data.items {
        dyn_compatibility_violation_for_assoc_item(db, &mut features, trait_, *assoc_item, cb)?;
    }

    ControlFlow::Continue(())
}

/// Where a violation is written, as rustc's `DynCompatibilityViolation::spans()` has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationSite {
    /// The `index`th predicate of `def` in `GenericPredicates::query_all`'s order, the index
    /// [`crate::predicate_sources`] takes. Always a written one.
    Predicate { def: GenericDefId, index: u32 },
    /// The `index`th item bound of an associated type (`type_alias_bounds`), its own and those
    /// the trait's where clauses put on it.
    AssocTypeBound { alias: TypeAliasId, index: u32 },
    /// The type of the function's `index`th parameter, `self` counted (rustc's
    /// `decl.inputs[index]`).
    Param { function: FunctionId, index: u32 },
    /// The function's return type.
    Output(FunctionId),
    /// The function's return-position `impl Trait` (`ImplTraitId::ReturnTypeImplTrait`).
    Opaque { function: FunctionId, index: ImplTraitIdx },
    /// The item's name.
    Name(AssocItemId),
}

/// rustc's `dyn_compatibility_violations` of `trait_` with each violation's `spans()`, in the
/// order [`dyn_compatibility_with_callback`] gives them. A violation with no site is one rustc
/// gives no span: a `Sized` that `Self` gets only by elaboration, and a supertrait's, whose
/// own violations have the sites. rustc's two `SelfReferential` violations stay apart: the
/// first from the trait's predicates, the second from its associated types' bounds.
pub fn dyn_compatibility_violation_sites(
    db: &Db,
    trait_: TraitId,
) -> Vec<(DynCompatibilityViolation, Box<[ViolationSite]>)> {
    // alloc: error path, read only to report a dyn-incompatible trait
    let mut out = Vec::new();
    let mut predicates_given = false;
    _ = dyn_compatibility_with_callback(db, trait_, &mut |violation| {
        let sites: Box<[ViolationSite]> = match violation {
            DynCompatibilityViolation::SizedSelf => sized_self_sites(db, trait_),
            DynCompatibilityViolation::SelfReferential
                if !predicates_given && predicates_reference_self(db, trait_) =>
            {
                predicates_given = true;
                self_referential_predicate_sites(db, trait_)
            }
            DynCompatibilityViolation::SelfReferential => self_referential_bound_sites(db, trait_),
            DynCompatibilityViolation::Method(function, code) => {
                // alloc: error path, one site
                Box::new([method_site(db, trait_, function, code)])
            }
            DynCompatibilityViolation::AssocConst(id) => {
                // alloc: error path, one site
                Box::new([ViolationSite::Name(id.into())])
            }
            DynCompatibilityViolation::GAT(id) => {
                // alloc: error path, one site
                Box::new([ViolationSite::Name(id.into())])
            }
            DynCompatibilityViolation::HasNonCompatibleSuperTrait(_) => Box::default(),
        };
        out.push((violation, sites));
        ControlFlow::Continue(())
    });
    out
}

/// rustc's `get_sized_bounds`: each bound written on the trait, on `Self` or not, whose trait
/// requires `Self: Sized`.
fn sized_self_sites(db: &Db, trait_: TraitId) -> Box<[ViolationSite]> {
    let interner = DbInterner::new_with(db, trait_.krate(db));
    let sized = interner.lang_items().Sized;
    written_predicate_sites(db, trait_, |clause| match clause.kind().skip_binder() {
        ClauseKind::Trait(pred) => {
            let bound = pred.def_id().0;
            Some(bound) == sized || generics_require_sized_self(db, bound.into())
        }
        _ => false,
    })
}

/// rustc's `predicates_reference_self`: each predicate of the trait with `Self` in an argument.
fn self_referential_predicate_sites(db: &Db, trait_: TraitId) -> Box<[ViolationSite]> {
    written_predicate_sites(db, trait_, |clause| {
        predicate_references_self(db, trait_, clause, AllowSelfProjections::No)
    })
}

fn written_predicate_sites<'db>(
    db: &'db Db,
    trait_: TraitId,
    mut keep: impl FnMut(Clause<'db>) -> bool,
) -> Box<[ViolationSite]> {
    let def = GenericDefId::from(trait_);
    let sources = crate::predicate_sources(db, def);
    GenericPredicates::query_all(db, def)
        .iter_identity()
        .map(Unnormalized::skip_norm_wip)
        .zip(&sources)
        .enumerate()
        .filter(|&(_, (clause, source))| {
            matches!(source, PredicateSource::Written { def: written, .. } if *written == def)
                && keep(clause)
        })
        .map(|(index, _)| ViolationSite::Predicate { def, index: index as u32 })
        .collect() // alloc: error path
}

/// rustc's `bounds_reference_self`: each item bound with `Self` in it of an associated type
/// that does not require `Self: Sized`.
fn self_referential_bound_sites(db: &Db, trait_: TraitId) -> Box<[ViolationSite]> {
    trait_
        .trait_items(db)
        .items
        .iter()
        .filter_map(|(_, it)| match *it {
            AssocItemId::TypeAliasId(id) => Some(id),
            _ => None,
        })
        .filter(|&alias| !generics_require_sized_self(db, alias.into()))
        .flat_map(|alias| {
            db.type_alias_bounds(alias)
                .iter_identity_copied()
                .map(Unnormalized::skip_norm_wip)
                .enumerate()
                .filter(move |&(_, clause)| {
                    predicate_references_self(db, trait_, clause, AllowSelfProjections::Yes)
                })
                .map(move |(index, _)| ViolationSite::AssocTypeBound { alias, index: index as u32 })
        })
        .collect() // alloc: error path
}

/// The span rustc's `dyn_compatibility_violations_for_assoc_item` gives a method's violation.
fn method_site(
    db: &Db,
    trait_: TraitId,
    function: FunctionId,
    code: MethodViolationCode,
) -> ViolationSite {
    let sig = db.callable_item_signature(function.into());
    match code {
        MethodViolationCode::ReferencesSelfInput => sig
            .skip_binder()
            .inputs()
            .iter()
            .skip(1)
            .position(|ty| {
                contains_illegal_self_type_reference(
                    db,
                    trait_,
                    ty.skip_binder(),
                    AllowSelfProjections::Yes,
                )
            })
            .map_or(ViolationSite::Name(function.into()), |arg| ViolationSite::Param {
                function,
                index: arg as u32 + 1,
            }),
        MethodViolationCode::ReferencesSelfOutput => ViolationSite::Output(function),
        MethodViolationCode::ReferencesImplTraitInTrait => return_impl_trait(db, &sig)
            .map_or(ViolationSite::Name(function.into()), |index| ViolationSite::Opaque {
                function,
                index,
            }),
        MethodViolationCode::UndispatchableReceiver => ViolationSite::Param { function, index: 0 },
        MethodViolationCode::StaticMethod
        | MethodViolationCode::AsyncFn
        | MethodViolationCode::WhereClauseReferencesSelf
        | MethodViolationCode::Generic => ViolationSite::Name(function.into()),
    }
}

#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn dyn_compatibility_of_trait_query(
    db: &Db,
    trait_: TraitId,
) -> Option<DynCompatibilityViolation> {
    let mut res = None;
    _ = dyn_compatibility_of_trait_with_callback(db, trait_, &mut |osv| {
        res = Some(osv);
        ControlFlow::Break(())
    });

    res
}

#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn generics_require_sized_self(db: &Db, def_id: GenericDefId) -> bool {
    let krate = def_id.krate(db);
    let interner = DbInterner::new_with(db, krate);
    let Some(sized_def_id) = interner.lang_items().Sized else {
        return false; /* No Sized trait, can't require it! */
    };

    // Search for a clause like `Self: Sized` amongst the trait bounds.
    let clauses = GenericPredicates::query_own_explicit(db, def_id)
        .iter_identity()
        .map(Unnormalized::skip_norm_wip);
    elaborate::elaborate(interner, clauses).any(|clause| match clause.kind().skip_binder() {
        ClauseKind::Trait(ref trait_pred) => {
            trait_pred.def_id().0 == sized_def_id && trait_pred.self_ty().is_param(0)
        }
        ClauseKind::RegionOutlives(_)
        | ClauseKind::TypeOutlives(_)
        | ClauseKind::Projection(_)
        | ClauseKind::ConstArgHasType(_, _)
        | ClauseKind::WellFormed(_)
        | ClauseKind::ConstEvaluatable(_)
        | ClauseKind::UnstableFeature(_)
        | ClauseKind::HostEffect(..) => false,
    })
}

// rustc gathers all the spans that references `Self` for error rendering,
// but we don't have good way to render such locations.
// So, just return single boolean value for existence of such `Self` reference
fn predicates_reference_self(db: &Db, trait_: TraitId) -> bool {
    GenericPredicates::query_explicit(db, trait_.into()).iter_identity().any(|pred| {
        predicate_references_self(db, trait_, pred.skip_norm_wip(), AllowSelfProjections::No)
    })
}

// Same as the above, `predicates_reference_self`
fn bounds_reference_self(db: &Db, trait_: TraitId) -> bool {
    let trait_data = trait_.trait_items(db);
    trait_data
        .items
        .iter()
        .filter_map(|(_, it)| match *it {
            AssocItemId::TypeAliasId(id) => Some(id),
            _ => None,
        })
        .filter(|&assoc_type| !generics_require_sized_self(db, assoc_type.into()))
        .flat_map(|assoc_type| {
            db.type_alias_bounds(assoc_type).iter_identity_copied().map(Unnormalized::skip_norm_wip)
        })
        .any(|clause| {
            // Item bounds *can* have self projections, since they never get
            // their self type erased.
            predicate_references_self(db, trait_, clause, AllowSelfProjections::Yes)
        })
}

#[derive(Clone, Copy)]
enum AllowSelfProjections {
    Yes,
    No,
}

fn predicate_references_self<'db>(
    db: &'db Db,
    trait_: TraitId,
    predicate: Clause<'db>,
    allow_self_projection: AllowSelfProjections,
) -> bool {
    match predicate.kind().skip_binder() {
        ClauseKind::Trait(trait_pred) => trait_pred.trait_ref.args.iter().skip(1).any(|arg| {
            contains_illegal_self_type_reference(db, trait_, &arg, allow_self_projection)
        }),
        ClauseKind::Projection(proj_pred) => {
            proj_pred.projection_term.args.iter().skip(1).any(|arg| {
                contains_illegal_self_type_reference(db, trait_, &arg, allow_self_projection)
            })
        }
        _ => false,
    }
}

fn contains_illegal_self_type_reference<'db, T: rustc_type_ir::TypeVisitable<DbInterner<'db>>>(
    db: &'db Db,
    trait_: TraitId,
    t: &T,
    allow_self_projection: AllowSelfProjections,
) -> bool {
    struct IllegalSelfTypeVisitor<'db> {
        db: &'db Db,
        trait_: TraitId,
        allow_self_projection: AllowSelfProjections,
    }
    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for IllegalSelfTypeVisitor<'db> {
        type Result = ControlFlow<()>;

        fn visit_ty(
            &mut self,
            ty: <DbInterner<'db> as rustc_type_ir::Interner>::Ty,
        ) -> Self::Result {
            let interner = DbInterner::new_no_crate(self.db);
            match ty.kind() {
                rustc_type_ir::TyKind::Param(param) if param.index == 0 => ControlFlow::Break(()),
                rustc_type_ir::TyKind::Param(_) => ControlFlow::Continue(()),
                rustc_type_ir::TyKind::Alias(
                    _,
                    proj @ AliasTy { kind: AliasTyKind::Projection { .. }, .. },
                ) => match self.allow_self_projection {
                    AllowSelfProjections::Yes => {
                        let trait_ = proj.trait_def_id(interner).0;
                        if crate::utils::all_super_traits(self.db, self.trait_).contains(&trait_) {
                            ControlFlow::Continue(())
                        } else {
                            ty.super_visit_with(self)
                        }
                    }
                    AllowSelfProjections::No => ty.super_visit_with(self),
                },
                // rustc's `is_impl_trait_in_trait` skip: a trait's own `impl Trait` is
                // `ReferencesImplTraitInTrait`, denied in its own pass.
                rustc_type_ir::TyKind::Alias(
                    _,
                    AliasTy { kind: AliasTyKind::Opaque { def_id }, .. },
                ) if is_impl_trait_in_trait(self.db, def_id.0.loc(self.db)) =>
                {
                    ControlFlow::Continue(())
                }
                _ => ty.super_visit_with(self),
            }
        }
    }

    let mut visitor = IllegalSelfTypeVisitor { db, trait_, allow_self_projection };
    t.visit_with(&mut visitor).is_break()
}

fn is_impl_trait_in_trait(db: &Db, id: ImplTraitId) -> bool {
    match id {
        ImplTraitId::ReturnTypeImplTrait(function, _) => {
            matches!(function.lookup(db).container, ItemContainerId::TraitId(_))
        }
        ImplTraitId::TypeAliasImplTrait(..) => false,
    }
}

fn dyn_compatibility_violation_for_assoc_item<'db, F>(
    db: &'db Db,
    features: &mut Option<&'db UnstableFeatures>,
    trait_: TraitId,
    item: AssocItemId,
    cb: &mut F,
) -> ControlFlow<()>
where
    F: FnMut(DynCompatibilityViolation) -> ControlFlow<()>,
{
    // Any item that has a `Self : Sized` requisite is otherwise
    // exempt from the regulations.
    if generics_require_sized_self(db, item.into()) {
        return ControlFlow::Continue(());
    }

    match item {
        AssocItemId::ConstId(it) => cb(DynCompatibilityViolation::AssocConst(it)),
        AssocItemId::FunctionId(it) => {
            virtual_call_violations_for_method(db, trait_, it, &mut |mvc| {
                cb(DynCompatibilityViolation::Method(it, mvc))
            })
        }
        AssocItemId::TypeAliasId(it) => {
            if features
                .get_or_insert_with(|| UnstableFeatures::query(db, trait_.krate(db)))
                .generic_associated_type_extended
            {
                ControlFlow::Continue(())
            } else {
                let generic_params = GenericParams::of(db, item.into());
                if !generic_params.is_empty() {
                    cb(DynCompatibilityViolation::GAT(it))
                } else {
                    ControlFlow::Continue(())
                }
            }
        }
    }
}

fn virtual_call_violations_for_method<F>(
    db: &Db,
    trait_: TraitId,
    func: FunctionId,
    cb: &mut F,
) -> ControlFlow<()>
where
    F: FnMut(MethodViolationCode) -> ControlFlow<()>,
{
    let func_data = FunctionSignature::of(db, func);
    if !func_data.has_self_param() {
        cb(MethodViolationCode::StaticMethod)?;
    }

    if func_data.is_async() {
        cb(MethodViolationCode::AsyncFn)?;
    }

    let sig = db.callable_item_signature(func.into());
    if sig.skip_binder().inputs().iter().skip(1).any(|ty| {
        contains_illegal_self_type_reference(
            db,
            trait_,
            ty.skip_binder(),
            AllowSelfProjections::Yes,
        )
    }) {
        cb(MethodViolationCode::ReferencesSelfInput)?;
    }

    if contains_illegal_self_type_reference(
        db,
        trait_,
        &sig.skip_binder().output(),
        AllowSelfProjections::Yes,
    ) {
        cb(MethodViolationCode::ReferencesSelfOutput)?;
    }

    if !func_data.is_async()
        && let Some(mvc) = contains_illegal_impl_trait_in_trait(db, &sig)
    {
        cb(mvc)?;
    }

    let generic_params = GenericParams::of(db, func.into());
    if generic_params.len_type_or_consts() > 0 {
        cb(MethodViolationCode::Generic)?;
    }

    if func_data.has_self_param() && !receiver_is_dispatchable(db, trait_, func, &sig) {
        cb(MethodViolationCode::UndispatchableReceiver)?;
    }

    let predicates = GenericPredicates::query_own_explicit(db, func.into());
    for pred in predicates.iter_identity() {
        let pred = pred.kind().skip_binder();

        if matches!(pred, ClauseKind::TypeOutlives(_)) {
            continue;
        }

        // Allow `impl AutoTrait` predicates
        if let ClauseKind::Trait(TraitClause {
            trait_ref: pred_trait_ref,
            polarity: ClausePolarity::Positive,
        }) = pred
            && let trait_data = TraitSignature::of(db, pred_trait_ref.def_id.0)
            && trait_data.flags.contains(TraitFlags::AUTO)
            && let rustc_type_ir::TyKind::Param(ParamTy { index: 0, .. }) =
                pred_trait_ref.self_ty().kind()
        {
            continue;
        }

        if contains_illegal_self_type_reference(db, trait_, &pred, AllowSelfProjections::Yes) {
            cb(MethodViolationCode::WhereClauseReferencesSelf)?;
            break;
        }
    }

    ControlFlow::Continue(())
}

fn receiver_is_dispatchable<'db>(
    db: &'db Db,
    trait_: TraitId,
    func: FunctionId,
    sig: &EarlyBinder<'db, Binder<'db, rustc_type_ir::FnSig<DbInterner<'db>>>>,
) -> bool {
    let sig = sig.instantiate_identity().skip_norm_wip();

    let module = trait_.module(db);
    let interner = DbInterner::new_with(db, module.krate(db));
    let self_param_id = TypeParamId::from_unchecked(TypeOrConstParamId {
        parent: trait_.into(),
        local_id: LocalTypeOrConstParamId::from_raw(la_arena::RawIdx::from_u32(0)),
    });
    let self_param_ty =
        Ty::new(interner, rustc_type_ir::TyKind::Param(ParamTy { index: 0, id: self_param_id }));

    // `self: Self` can't be dispatched on, but this is already considered dyn-compatible
    // See rustc's comment on https://github.com/rust-lang/rust/blob/3f121b9461cce02a703a0e7e450568849dfaa074/compiler/rustc_trait_selection/src/traits/object_safety.rs#L433-L437
    if sig.inputs().iter().next().is_some_and(|p| *p.skip_binder() == self_param_ty) {
        return true;
    }

    let receiver_ty = interner.liberate_late_bound_regions(func.into(), sig.input(0));

    let lang_items = interner.lang_items();
    let traits = (lang_items.Unsize, lang_items.DispatchFromDyn);
    let (Some(unsize_did), Some(dispatch_from_dyn_did)) = traits else {
        return false;
    };

    let Some(meta_sized_did) = lang_items.MetaSized else {
        return false;
    };

    // Type `U`
    // FIXME: That seems problematic to fake a generic param like that?
    let unsized_self_ty = Ty::new_param(interner, self_param_id, u32::MAX);
    // `Receiver[Self => U]`
    let unsized_receiver_ty = receiver_for_self_ty(interner, func, receiver_ty, unsized_self_ty);

    let param_env = {
        let generic_predicates = GenericPredicates::query_all(db, func.into());

        // Self: Unsize<U>
        let unsize_predicate =
            TraitRef::new(interner, unsize_did.into(), [self_param_ty, unsized_self_ty]);

        // U: Trait<Arg1, ..., ArgN>
        let args = GenericArgs::for_item(interner, trait_.into(), |index, kind, _, _| {
            if index == 0 { unsized_self_ty.into() } else { mk_param(interner, index, kind) }
        });
        let trait_predicate = TraitRef::new_from_args(interner, trait_.into(), args);

        let meta_sized_predicate =
            TraitRef::new(interner, meta_sized_did.into(), [unsized_self_ty]).upcast(interner);

        ParamEnv {
            clauses: Clauses::new_from_iter(
                generic_predicates
                    .iter_identity()
                    .map(Unnormalized::skip_norm_wip)
                    .chain([unsize_predicate.upcast(interner), trait_predicate.upcast(interner)])
                    .chain(std::iter::once(meta_sized_predicate)),
            ),
        }
    };

    // Receiver: DispatchFromDyn<Receiver[Self => U]>
    let predicate =
        TraitRef::new(interner, dispatch_from_dyn_did.into(), [receiver_ty, unsized_receiver_ty]);
    let obligation = Obligation::new(interner, ObligationCause::dummy(), param_env, predicate);

    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    // the receiver is dispatchable iff the obligation holds
    infcx.predicate_must_hold_modulo_regions(&obligation)
}

fn receiver_for_self_ty<'db>(
    interner: DbInterner<'db>,
    func: FunctionId,
    receiver_ty: Ty<'db>,
    self_ty: Ty<'db>,
) -> Ty<'db> {
    let args =
        GenericArgs::for_item(interner, SolverDefId::FunctionId(func), |index, kind, _, _| {
            if index == 0 { self_ty.into() } else { mk_param(interner, index, kind) }
        });

    EarlyBinder::bind(interner, receiver_ty).instantiate(interner, args).skip_norm_wip()
}

fn contains_illegal_impl_trait_in_trait<'db>(
    db: &'db Db,
    sig: &EarlyBinder<'db, Binder<'db, rustc_type_ir::FnSig<DbInterner<'db>>>>,
) -> Option<MethodViolationCode> {
    return_impl_trait(db, sig).map(|_| MethodViolationCode::ReferencesImplTraitInTrait)
}

/// The first return-position `impl Trait` of `sig`'s return type, where rustc's
/// `contains_illegal_impl_trait_in_trait` points.
fn return_impl_trait<'db>(
    db: &'db Db,
    sig: &EarlyBinder<'db, Binder<'db, rustc_type_ir::FnSig<DbInterner<'db>>>>,
) -> Option<ImplTraitIdx> {
    struct ReturnTypeImplTrait<'db>(&'db Db);

    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for ReturnTypeImplTrait<'db> {
        type Result = ControlFlow<ImplTraitIdx>;

        fn visit_ty(
            &mut self,
            ty: <DbInterner<'db> as rustc_type_ir::Interner>::Ty,
        ) -> Self::Result {
            if let rustc_type_ir::TyKind::Alias(
                _,
                AliasTy { kind: AliasTyKind::Opaque { def_id }, .. },
            ) = ty.kind()
                && let ImplTraitId::ReturnTypeImplTrait(_, index) = def_id.0.loc(self.0)
            {
                return ControlFlow::Break(index);
            }
            ty.super_visit_with(self)
        }
    }

    // Since we haven't implemented RPITIT in proper way like rustc yet,
    // just check whether `ret` contains RPIT for now
    let ret = sig.skip_binder().output();
    ret.visit_with(&mut ReturnTypeImplTrait(db)).break_value()
}
