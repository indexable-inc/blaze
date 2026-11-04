//! rustc's model of a trait method's return-position `impl Trait` (RPITIT): each `impl Trait` of
//! a trait method's return type is an associated type of the trait (rustc's synthesized
//! `ImplTraitInTraitData::Trait` item, `rustc_ty_utils/src/assoc.rs`), and each impl whose method
//! implements that method has one too (`ImplTraitInTraitData::Impl`), whose value is what the
//! impl's method returns there (`collect_return_position_impl_trait_in_trait_tys`,
//! `rustc_hir_analysis/src/check/compare_impl_item.rs`).
//!
//! The trait method's signature returns the projection `<Self as Trait>::{impl Trait#i}`, which
//! normalizes as any associated type does: to the impl's item, or, for a method with a default
//! body that the impl keeps, to the trait's value, the default body's own opaque type
//! (`ReturnTypeImplTrait(trait_fn, i)`), which the default body sees through a clause of its
//! environment (`rustc_ty_utils::ty::param_env`'s `ImplTraitInTraitFinder`).
//!
//! The generics of the trait's item are the opaque type's: the trait's, then the method's own
//! with its late-bound lifetimes, then one lifetime for each its parameters leave out
//! ([`crate::lower::elided_lifetimes`]). An impl's item has the impl's, then the impl method's
//! own early-bound ones (at the same places and of the same kinds as the trait method's, or the
//! impl is refused before this, E0049/E0195), then the trait method's late-bound and left-out
//! lifetimes, as rustc copies the trait item's own parameters onto the impl
//! (`associated_type_for_impl_trait_in_impl`).

use base_db::Db;
use blaze_mem::Frame;
use hir_def::{
    FunctionId, ImplId, ItemContainerId, Lookup, TraitId, signatures::FunctionSignature,
};
use rustc_type_ir::{
    BoundVar, Interner as _, IsRigid, TypeFoldable, TypeSuperFoldable, TypeVisitable as _,
    TypeVisitableExt, Variance,
    error::TypeError,
    inherent::{GenericArgs as _, IntoKind, Ty as _},
    relate::{self, Relate, TypeRelation, VarianceDiagInfo},
};

use crate::{
    GenericPredicates, ImplTraitId,
    db::{HirDatabase, InternedOpaqueTyId},
    lower::{ElidedOutput, ImplTraitIdx, SelfAndAssocBounds},
    next_solver::{
        AliasTy, AliasTyKind, AssocTyId, Binder, BoundRegion, BoundRegionKind, Clause, Clauses,
        DbInterner, EarlyBinder, FnSig, GenericArg, GenericArgKind, GenericArgs, Region,
        RegionKind, SolverDefId, StoredClauses, StoredEarlyBinder, StoredTy, TraitAssocTyId, Ty,
        TyKind, TypingMode,
        fulfill::NextSolverError,
        infer::{
            DbInternerInferExt as _,
            relate::RelateResult,
            traits::{Obligation, ObligationCause},
        },
        obligation_ctxt::ObligationCtxt,
        util::BottomUpFolder,
    },
};

/// A trait's or an impl's associated type for a trait method's return-position `impl Trait`.
/// Which of the two it is follows from `function`'s container ([`RpititId::side`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RpititId {
    /// The trait's method, for the trait's item; the impl's method implementing it, for an
    /// impl's.
    pub function: FunctionId,
    /// Which `impl Trait` of the trait method's return type, in the order lowering gives them
    /// (each before the ones in its bounds), the index of its opaque type
    /// `ReturnTypeImplTrait(trait_fn, idx)`.
    pub idx: ImplTraitIdx,
}

/// Where an [`RpititId`] is an associated type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RpititSide {
    /// The trait's item, of the trait's method `function`.
    Trait(TraitId),
    /// An impl's item: `function`, a method of `impl_`, implements `trait_fn`.
    Impl { impl_: ImplId, trait_fn: FunctionId },
}

impl RpititId {
    /// Of a trait's method, the trait's item for its `impl Trait` `idx`.
    pub fn new_in_trait(db: &Db, trait_fn: FunctionId, idx: ImplTraitIdx) -> RpititId {
        assert!(
            is_trait_method(db, trait_fn),
            "an RPITIT of the trait is made of a trait's method, not {}",
            FunctionSignature::of(db, trait_fn).name.as_str()
        );
        RpititId { function: trait_fn, idx }
    }

    pub fn side(self, db: &Db) -> RpititSide {
        match self.function.lookup(db).container {
            ItemContainerId::TraitId(trait_) => RpititSide::Trait(trait_),
            ItemContainerId::ImplId(impl_) => {
                let trait_fn = implemented_trait_method(db, self.function).unwrap_or_else(|| {
                    panic!(
                        "the RPITIT `impl Trait` #{} of `{}` is an impl's, but the method \
                         implements no trait method",
                        self.idx.into_raw().into_u32(),
                        FunctionSignature::of(db, self.function).name.as_str()
                    )
                });
                RpititSide::Impl { impl_, trait_fn }
            }
            ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => panic!(
                "an RPITIT is a trait's or an impl's, not of the free function `{}`",
                FunctionSignature::of(db, self.function).name.as_str()
            ),
        }
    }

    /// The trait's method whose return type has this `impl Trait`.
    pub fn trait_fn(self, db: &Db) -> FunctionId {
        match self.side(db) {
            RpititSide::Trait(_) => self.function,
            RpititSide::Impl { trait_fn, .. } => trait_fn,
        }
    }

    /// The trait's item this is or implements.
    pub fn in_trait(self, db: &Db) -> RpititId {
        RpititId { function: self.trait_fn(db), idx: self.idx }
    }

    /// The trait of the method.
    pub fn trait_id(self, db: &Db) -> TraitId {
        match self.trait_fn(db).lookup(db).container {
            ItemContainerId::TraitId(trait_) => trait_,
            _ => unreachable!("`RpititId::trait_fn` is a trait's method"),
        }
    }

    /// The opaque type the trait's method's `impl Trait` is in its own default body, whose
    /// bounds are the item's bounds: `ReturnTypeImplTrait(trait_fn, idx)`.
    pub fn opaque<'db>(self, db: &'db Db) -> InternedOpaqueTyId<'db> {
        InternedOpaqueTyId::new(db, ImplTraitId::ReturnTypeImplTrait(self.trait_fn(db), self.idx))
    }

    /// Whether the trait's item has a value: its method has a default body, which defines the
    /// `impl Trait` (rustc's `defaultness(..).has_value()` of the synthesized item). An impl's
    /// always has one.
    pub fn has_value(self, db: &Db) -> bool {
        match self.side(db) {
            RpititSide::Trait(_) => FunctionSignature::of(db, self.function).has_body(),
            RpititSide::Impl { .. } => true,
        }
    }

    /// The impl's item for this trait item, when `impl_` has a method implementing it.
    pub fn in_impl(self, db: &Db, impl_: ImplId) -> Option<RpititId> {
        let name = &FunctionSignature::of(db, self.trait_fn(db)).name;
        let impl_fn = impl_.impl_items(db).items.iter().find_map(|(item, id)| match *id {
            hir_def::AssocItemId::FunctionId(it) if item == name => Some(it),
            _ => None,
        })?;
        Some(RpititId { function: impl_fn, idx: self.idx })
    }

    /// `{impl Trait#idx of method}`, how a diagnostic names the item.
    pub fn display(self, db: &Db) -> String {
        // alloc: a diagnostic's or a debug print's text
        format!(
            "{{impl Trait#{} of `{}`}}",
            self.idx.into_raw().into_u32(),
            FunctionSignature::of(db, self.trait_fn(db)).name.as_str()
        )
    }
}

/// Whether `function` is a method of a trait (not of an impl, not free).
pub fn is_trait_method(db: &Db, function: FunctionId) -> bool {
    matches!(function.lookup(db).container, ItemContainerId::TraitId(_))
}

/// The trait method `impl_fn`, a method of a trait impl, implements: the trait's method of its
/// name. `None` for a method of an inherent impl, or one the trait lacks (E0407).
pub fn implemented_trait_method(db: &Db, impl_fn: FunctionId) -> Option<FunctionId> {
    let ItemContainerId::ImplId(impl_) = impl_fn.lookup(db).container else { return None };
    let trait_ = db.impl_trait(impl_)?.skip_binder().def_id.0;
    trait_.trait_items(db).method_by_name(&FunctionSignature::of(db, impl_fn).name)
}

/// The trait's items for each `impl Trait` of the return type of `trait_fn`, a trait's method,
/// nested ones included. None for a method of an impl or a free function, whose `impl Trait`
/// are opaque types.
pub fn rpitits_of(db: &Db, function: FunctionId) -> impl Iterator<Item = RpititId> {
    let count = if is_trait_method(db, function) {
        SelfAndAssocBounds::return_type_impl_traits(db, function).len()
    } else {
        0
    };
    (0..count as u32).map(move |idx| RpititId {
        function,
        idx: ImplTraitIdx::from_raw(la_arena::RawIdx::from_u32(idx)),
    })
}

/// The projection `<Self as Trait>::{impl Trait#idx}<args>` the trait's method `trait_fn`
/// returns for its `impl Trait` `idx`, the arguments those of its opaque type.
pub fn rpitit_projection<'db>(
    interner: DbInterner<'db>,
    rpitit: RpititId,
    args: GenericArgs<'db>,
) -> Ty<'db> {
    Ty::new_alias(
        interner,
        IsRigid::No,
        AliasTy::new_from_args(
            interner,
            AliasTyKind::Projection { def_id: TraitAssocTyId(AssocTyId::RpititId(rpitit)) },
            args,
        ),
    )
}

/// The RPITIT `ty` is, when it is the projection of one.
pub fn as_rpitit(ty: Ty<'_>) -> Option<(RpititId, GenericArgs<'_>)> {
    match ty.kind() {
        TyKind::Alias(_, alias) => match alias.kind {
            AliasTyKind::Projection { def_id: TraitAssocTyId(AssocTyId::RpititId(id)) } => {
                Some((id, alias.args))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The function whose return-position `impl Trait` `alias` is: an RPIT's opaque type, or a
/// trait method's RPITIT projection.
pub fn returned_impl_trait_owner(db: &Db, alias: AliasTy<'_>) -> Option<FunctionId> {
    match alias.kind {
        AliasTyKind::Opaque { def_id } => match def_id.0.loc(db) {
            ImplTraitId::ReturnTypeImplTrait(owner, _) => Some(owner),
            ImplTraitId::TypeAliasImplTrait(..) => None,
        },
        AliasTyKind::Projection { def_id: TraitAssocTyId(AssocTyId::RpititId(rpitit)) } => {
            Some(rpitit.trait_fn(db))
        }
        _ => None,
    }
}

/// `value` with each opaque type of `trait_fn`'s own `impl Trait` replaced by the trait's item
/// for it, the same arguments: how the bounds written on the opaque type become the item's.
pub fn opaques_to_rpitits<'db, T: TypeFoldable<DbInterner<'db>>>(
    interner: DbInterner<'db>,
    trait_fn: FunctionId,
    value: T,
) -> T {
    let db = interner.db;
    value.fold_with(&mut BottomUpFolder {
        interner,
        ty_op: |ty: Ty<'db>| match ty.kind() {
            TyKind::Alias(_, alias)
                if let AliasTyKind::Opaque { def_id } = alias.kind
                    && let ImplTraitId::ReturnTypeImplTrait(owner, idx) = def_id.0.loc(db)
                    && owner == trait_fn =>
            {
                rpitit_projection(interner, RpititId { function: trait_fn, idx }, alias.args)
            }
            _ => ty,
        },
        lt_op: |region| region,
        ct_op: |ct| ct,
    })
}

/// `ty` with each of `trait_fn`'s RPITITs unknown, an error type: the trait's return type as
/// rustc's `ImplTraitInTraitCollector` leaves it for an error, each item a type variable
/// (printed `_`), for E0053 "incompatible return type".
pub fn rpitits_unknown<'db>(
    interner: DbInterner<'db>,
    trait_fn: FunctionId,
    ty: Ty<'db>,
) -> Ty<'db> {
    ty.fold_with(&mut BottomUpFolder {
        interner,
        ty_op: |ty: Ty<'db>| match as_rpitit(ty) {
            Some((rpitit, _)) if rpitit.function == trait_fn => {
                Ty::new_error(interner, crate::next_solver::ErrorGuaranteed)
            }
            _ => ty,
        },
        lt_op: |region| region,
        ct_op: |ct| ct,
    })
}

/// The inverse of [`opaques_to_rpitits`]: `value` with each of `trait_fn`'s own RPITIT
/// projections replaced by its opaque type, the same arguments. That is what the projection
/// normalizes to in `trait_fn`'s default body, whose environment says so
/// (`default_body_clauses`): the body's own signature, as the body sees it.
pub fn rpitits_to_opaques<'db, T: TypeFoldable<DbInterner<'db>>>(
    interner: DbInterner<'db>,
    trait_fn: FunctionId,
    value: T,
) -> T {
    let db = interner.db;
    value.fold_with(&mut BottomUpFolder {
        interner,
        ty_op: |ty: Ty<'db>| match as_rpitit(ty) {
            Some((rpitit, args)) if rpitit.function == trait_fn => {
                Ty::new_opaque(interner, IsRigid::No, rpitit.opaque(db), args)
            }
            _ => ty,
        },
        lt_op: |region| region,
        ct_op: |ct| ct,
    })
}

/// `type_of` of an RPITIT. The trait's item has a value only when its method has a default
/// body: that body's opaque type, with the item's own arguments, which are the opaque's.
pub fn type_of_rpitit<'db>(
    interner: DbInterner<'db>,
    rpitit: RpititId,
) -> EarlyBinder<'db, Ty<'db>> {
    let db = interner.db;
    match rpitit.side(db) {
        RpititSide::Trait(_) => {
            assert!(
                rpitit.has_value(db),
                "the trait's {} has no value: its method has no default body",
                rpitit.display(db)
            );
            let opaque = rpitit.opaque(db);
            let args = GenericArgs::identity_for_item(interner, opaque.into());
            EarlyBinder::bind(
                interner,
                Ty::new_alias(
                    interner,
                    IsRigid::No,
                    AliasTy::new_from_args(
                        interner,
                        AliasTyKind::Opaque { def_id: opaque.into() },
                        args,
                    ),
                ),
            )
        }
        RpititSide::Impl { .. } => match impl_rpitit_tys(db, rpitit.function)
            .get(rpitit.idx.into_raw().into_u32() as usize)
        {
            Some(Ok(ty)) => ty.get(),
            // Reported where the impl's method is checked, and by a use that normalizes to it
            // ([`rpitit_value_error`]).
            Some(Err(_)) | None => EarlyBinder::bind(
                interner,
                Ty::new_error(interner, crate::next_solver::ErrorGuaranteed),
            ),
        },
    }
}

/// Why the RPITIT projection `ty` normalizes to an error type in `env`: the impl it selects
/// gives its item no value, [`RpititError::message`] of it. `None` for any other type, and for
/// a projection whose impl gives a value or isn't known.
pub fn rpitit_value_error<'db>(
    infcx: &crate::next_solver::infer::InferCtxt<'db>,
    env: crate::next_solver::ParamEnv<'db>,
    ty: Ty<'db>,
) -> Option<String> {
    let TyKind::Alias(_, alias) = ty.kind() else { return None };
    let (rpitit, _) = as_rpitit(ty)?;
    let db = infcx.interner.db;
    let trait_ref = alias.trait_ref(infcx.interner);
    let (crate::next_solver::AnyImplId::ImplId(impl_), _) =
        crate::method_resolution::find_matching_impl(infcx, env, trait_ref).ok()?
    else {
        return None;
    };
    let item = rpitit.in_impl(db, impl_)?;
    match impl_rpitit_tys(db, item.function).get(item.idx.into_raw().into_u32() as usize)? {
        Err(error) => Some(error.message(db, item)),
        Ok(_) => None,
    }
}

/// Why an impl's item for a trait method's `impl Trait` has no value. Each names the impl's
/// method and the trait item where it is reported (`RpititError::message`).
#[derive(Clone, PartialEq, Eq, Hash, Debug, blaze_query::Value)]
pub enum RpititError {
    /// The impl's method and the trait's differ in their own early-bound generic parameters
    /// (E0049, E0195), already an error of its own.
    GenericsDiffer,
    /// The impl method's return type has no type where the trait method's has this
    /// `impl Trait`, as its return type differs from the trait's (E0053).
    NotFound,
    /// The impl's type names a lifetime the trait's `impl Trait` does not capture: rustc's
    /// "return type captures more lifetimes than trait definition". `lifetime` is its name as
    /// written; `hidden` is the impl's type, in the impl method's generics with its late-bound
    /// lifetimes liberated.
    UncapturedLifetime { lifetime: Box<str>, hidden: StoredTy },
    /// The type of an `impl Trait` in the bounds of another could not be worked out from the
    /// outer one's: proving those bounds failed or stayed ambiguous.
    Unsolved,
    /// The impl method's signature or the trait method's, as the impl sees it, names an
    /// associated type that does not normalize in the impl method's environment: rustc's
    /// E0277 of that signature, which fails the comparison.
    Unnormalized,
    /// The impl's types do not meet these bounds of the trait's `impl Trait`s (rustc's E0277,
    /// "required by a bound in `Trait::method::{anon_assoc#i}`"), each with the index of the
    /// `impl Trait` it bounds, in `of`. The bounds have the impl's types for the trait's items,
    /// in the impl method's generics with its late-bound lifetimes liberated; one another
    /// failing bound implies is left out. rustc fails every item of the method then.
    Unsatisfied { bounds: StoredClauses, of: Box<[u32]> },
}

impl RpititError {
    pub fn message(&self, db: &Db, rpitit: RpititId) -> String {
        let impl_fn = FunctionSignature::of(db, rpitit.function).name.as_str().to_owned();
        let item = rpitit.in_trait(db).display(db);
        // alloc: a diagnostic's text
        match self {
            RpititError::GenericsDiffer => format!(
                "the method `{impl_fn}` has other generic parameters than the trait's, so it \
                 gives no type for {item}"
            ),
            RpititError::NotFound => format!(
                "the return type of the method `{impl_fn}` has no type where the trait's has \
                 {item}"
            ),
            RpititError::UncapturedLifetime { lifetime, .. } => format!(
                "the return type of the method `{impl_fn}` captures more lifetimes than the \
                 trait's definition: its type for {item} names `{lifetime}`, which the trait's \
                 `impl Trait` does not capture"
            ),
            RpititError::Unsolved => format!(
                "the type the method `{impl_fn}` gives {item} could not be worked out from the \
                 bounds of the `impl Trait` it is in"
            ),
            RpititError::Unnormalized => format!(
                "the signature of the method `{impl_fn}` or of the trait's names an associated \
                 type that cannot be normalized there, so it gives no type for {item}"
            ),
            RpititError::Unsatisfied { .. } => format!(
                "the types the method `{impl_fn}` gives the trait's `impl Trait`s do not meet \
                 their bounds, so it gives no type for {item}"
            ),
        }
    }
}

/// The value of each impl's item for an `impl Trait` of the return type of the trait method
/// `impl_fn` implements, by the trait's index, in the generics of the impl's item: rustc's
/// `collect_return_position_impl_trait_in_trait_tys`. Empty for a method that implements no
/// trait method with an `impl Trait`.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn impl_rpitit_tys(
    db: &Db,
    impl_fn: FunctionId,
) -> Box<[Result<StoredEarlyBinder<StoredTy>, RpititError>]> {
    blaze_mem::frame::current(|frame| impl_rpitit_tys_in(db, impl_fn, frame))
}

fn impl_rpitit_tys_in<'db>(
    db: &'db Db,
    impl_fn: FunctionId,
    frame: &Frame,
) -> Box<[Result<StoredEarlyBinder<StoredTy>, RpititError>]> {
    let Some(trait_fn) = implemented_trait_method(db, impl_fn) else { return Box::default() };
    let count = rpitits_of(db, trait_fn).count();
    if count == 0 {
        return Box::default();
    }
    let ItemContainerId::ImplId(impl_) = impl_fn.lookup(db).container else {
        unreachable!("`implemented_trait_method` is of an impl's method")
    };
    let interner = DbInterner::new_with(db, hir_def::HasModule::krate(&impl_fn, db));
    let all = |error: RpititError| {
        // alloc: the memo, one entry per `impl Trait`
        (0..count).map(|_| Err(error.clone())).collect()
    };
    let Some(trait_ref) = db.impl_trait(impl_) else { return Box::default() };
    let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();

    // The own early-bound parameters of both, of the same kinds at the same places, so the
    // impl method's stand for the trait method's (compare.rs checks it and reports E0049).
    let own = |def: FunctionId, parent: SolverDefId<'_>| {
        let all = GenericArgs::identity_for_item(interner, SolverDefId::FunctionId(def));
        let parent = GenericArgs::identity_for_item(interner, parent).len();
        all.iter().skip(parent).map(|arg| arg_kind(arg.kind())).collect::<Vec<_>>() // alloc: per impl method, a few params
    };
    if own(trait_fn, SolverDefId::TraitId(trait_ref.def_id.0))
        != own(impl_fn, SolverDefId::ImplId(impl_))
    {
        return all(RpititError::GenericsDiffer);
    }

    // rustc's `trait_to_impl_args`: the impl's trait arguments and the impl method's own.
    let trait_to_impl_args = GenericArgs::identity_for_item(interner, impl_fn.into()).rebase_onto(
        interner,
        impl_.into(),
        trait_ref.args,
    );
    // The impl's where clauses and the trait method's own: a bound of an `impl Trait` may need
    // the method's own where clauses (`fn items<T: Clone>(..) -> impl Iterator<Item = impl Clone>`).
    let param_env = crate::lower::hybrid_param_env(interner, impl_, trait_fn, trait_to_impl_args);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let mut ocx = ObligationCtxt::new(&infcx);
    let cause = ObligationCause::dummy();

    let impl_sig = named_signature(
        db,
        impl_fn,
        GenericArgs::identity_for_item(interner, impl_fn.into()),
        frame,
    );
    // The trait's signature with each of its RPITITs an inference variable whose bounds are to
    // be proven (rustc's `ImplTraitInTraitCollector`), so normalizing it does not normalize them.
    let mut collector = Collector {
        interner,
        infcx: &infcx,
        trait_fn,
        vars: vec![None; count], // alloc: per impl method, one slot per `impl Trait`
        bounds: Vec::new_in(frame),
    };
    let trait_sig =
        named_signature(db, trait_fn, trait_to_impl_args, frame).fold_with(&mut collector);
    let Collector { vars, bounds, .. } = collector;
    if trait_sig.inputs().len() != impl_sig.inputs().len() {
        // E0050, reported where the signatures are compared.
        return all(RpititError::NotFound);
    }
    // rustc normalizes both before relating them: an impl may write its return type through an
    // associated type (`fn f(&self) -> Self::Out`). One that does not normalize fails the
    // comparison, as in rustc.
    let (Ok(impl_sig), Ok(trait_sig)) = (
        ocx.deeply_normalize(&cause, param_env, impl_sig),
        ocx.deeply_normalize(&cause, param_env, trait_sig),
    ) else {
        return all(RpititError::Unnormalized);
    };

    // Each RPITIT's variable is the impl's type at its place.
    if ocx.eq(&cause, param_env, trait_sig.output(), impl_sig.output()).is_err() {
        // E0053, reported where the signatures are compared.
        return all(RpititError::NotFound);
    }
    // The bounds of each give the ones nested in them their types.
    for &(clause, _) in &bounds {
        // clone: an obligation's cause, once per bound of an `impl Trait`
        ocx.register_obligation(Obligation::new(interner, cause.clone(), param_env, clause));
    }
    let errors = ocx.evaluate_obligations_error_on_ambiguity();
    if let Some(error) = unsatisfied(&infcx, &errors, &bounds, frame) {
        return all(error);
    }
    let unproven = !errors.is_empty();

    // The lifetimes of the impl's signature, each with the trait's at the same place, the
    // RPITITs' places left out.
    let mut relation = Pair { interner, regions: Vec::new_in(frame) };
    for (t, i) in trait_sig.inputs_and_output.iter().zip(impl_sig.inputs_and_output.iter()) {
        relation.relate(t, i).expect("`Pair` relates as far as both have the same shape");
    }
    let Pair { regions, .. } = relation;

    // Where an RPITIT sits in the trait's return type, the arguments it has there: the same
    // for every one of `trait_fn`'s, which share the method's generics.
    let trait_args = vars.iter().flatten().map(|&(_, args)| args).next();
    // alloc: per impl method, one slot per `impl Trait`
    let values: Vec<Option<Result<Ty<'db>, RpititError>>> = vars
        .iter()
        .map(|var| {
            let (var, _) = (*var)?;
            let ty = infcx.deeply_resolve_via_unification_table(var);
            Some(match (ty.has_non_region_infer(), unproven) {
                (false, _) => Ok(ty),
                // Its bounds failed or stayed ambiguous (rustc's E0277 of the impl's type).
                (true, true) => Err(RpititError::Unsolved),
                (true, false) => Err(RpititError::NotFound),
            })
        })
        .collect();

    // The identity arguments of the impl's item: what each of the trait's own arguments of
    // the RPITIT (a lifetime late-bound in the trait's method, or one its parameters leave out)
    // becomes in the impl's item.
    let identity = GenericArgs::identity_for_item(
        interner,
        SolverDefId::RpititId(RpititId {
            function: impl_fn,
            idx: ImplTraitIdx::from_raw(la_arena::RawIdx::from_u32(0)),
        }),
    );
    let mut remap = Remap {
        interner,
        impl_fn,
        trait_fn,
        regions: &regions,
        trait_args: trait_args.map_or(&[][..], |it| it.as_slice()),
        identity: identity.as_slice(),
        own_len: trait_args.map_or(0, |it| it.len() - trait_ref.args.len()),
        captures: &[],
        uncaptured: None,
    };
    // alloc: the memo, one entry per `impl Trait`
    values
        .into_iter()
        .enumerate()
        .map(|(idx, value)| match value {
            None => Err(RpititError::NotFound),
            Some(Err(error)) => Err(error),
            Some(Ok(hidden)) => {
                let item = RpititId {
                    function: trait_fn,
                    idx: ImplTraitIdx::from_raw(la_arena::RawIdx::from_u32(idx as u32)),
                };
                remap.captures = interner.variances_of(SolverDefId::RpititId(item)).as_slice();
                assert_eq!(
                    remap.captures.len(),
                    remap.trait_args.len(),
                    "the variances of {} are not one per argument of it",
                    item.display(db),
                );
                remap.uncaptured = None;
                let ty = hidden.fold_with(&mut remap);
                match remap.uncaptured.take() {
                    Some(lifetime) => {
                        Err(RpititError::UncapturedLifetime { lifetime, hidden: hidden.store() })
                    }
                    None => {
                        let ty = rustc_type_ir::set_aliases_to_non_rigid(interner, ty)
                            .skip_normalization();
                        Ok(StoredEarlyBinder::bind(interner, ty.store()))
                    }
                }
            }
        })
        .collect()
}

/// rustc's E0277 of the bounds of the trait's `impl Trait`s the impl's types do not meet
/// (`ImplTraitInTraitCollector`'s obligations, `report_fulfillment_errors`): each failing bound,
/// its variables resolved (each RPITIT's is the impl's type), with the index of the `impl Trait`
/// it bounds. A bound another failing one implies is left out, as rustc's `error_implies`
/// does: the item bounds are elaborated, so `impl Copy` failing gives `Copy` and its supertrait
/// `Clone`, and only `Copy` is the error. `None` when nothing failed but ambiguities;
/// [`RpititError::NotFound`] when a goal of relating the return types failed.
fn unsatisfied<'db>(
    infcx: &crate::next_solver::infer::InferCtxt<'db>,
    errors: &[NextSolverError<'db>],
    bounds: &[(Clause<'db>, u32)],
    frame: &Frame,
) -> Option<RpititError> {
    let interner = infcx.interner;
    let mut failed: Vec<(Clause<'db>, u32), _> = Vec::new_in(frame);
    for error in errors {
        let NextSolverError::TrueError(obligation) = error else { continue };
        // The errors are of the root obligations, the bounds registered, their variables
        // resolved as far as solving went.
        let predicate = infcx.deeply_resolve_via_unification_table(obligation.predicate);
        let Some(&(clause, index)) = bounds.iter().find(|(clause, _)| {
            infcx.deeply_resolve_via_unification_table(clause.as_predicate()) == predicate
        }) else {
            // A goal of relating the return types (`ocx.eq` registers the nested ones in the
            // same context) failed: the return types differ, rustc's E0053.
            return Some(RpititError::NotFound);
        };
        let clause = infcx.deeply_resolve_via_unification_table(clause);
        if !failed.contains(&(clause, index)) {
            failed.push((clause, index));
        }
    }
    if failed.is_empty() {
        return None;
    }
    let implied = |clause: Clause<'db>| {
        failed.iter().any(|&(other, _)| {
            other != clause
                && rustc_type_ir::elaborate::elaborate(interner, [other]).any(|it| it == clause)
        })
    };
    let mut reported: Vec<(Clause<'db>, u32), _> = Vec::new_in(frame);
    reported.extend(failed.iter().copied().filter(|&(clause, _)| !implied(clause)));
    Some(RpititError::Unsatisfied {
        bounds: Clauses::new_from_iter(reported.iter().map(|(clause, _)| *clause)).store(),
        // alloc: an error, once per impl method whose types fail the trait's bounds
        of: reported.iter().map(|(_, index)| *index).collect(),
    })
}

fn arg_kind(arg: GenericArgKind<'_>) -> u8 {
    match arg {
        GenericArgKind::Lifetime(_) => 0,
        GenericArgKind::Type(_) => 1,
        GenericArgKind::Const(_) => 2,
    }
}

/// The signature of `function` with `args`, its late-bound lifetimes liberated onto `function`
/// and each lifetime its parameters leave out one of them too
/// ([`crate::lower::bind_elided_lifetimes`]), in its parameters, in the arguments of its own
/// `impl Trait` (opaque types, or RPITIT projections for a trait's method) for them
/// ([`crate::lower::with_elided_regions`]), and where its return type leaves one out. A
/// lifetime this cannot tell stays an error region, as lowering left it. hir-ty-wfcheck checks
/// an impl's types for the trait's `impl Trait`s against their bounds in this signature.
pub fn named_signature<'db>(
    db: &'db Db,
    function: FunctionId,
    args: GenericArgs<'db>,
    frame: &Frame,
) -> FnSig<'db> {
    let interner = DbInterner::new_no_crate(db);
    let raw =
        db.callable_item_signature(function.into()).instantiate(interner, args).skip_norm_wip();
    let named = raw.bound_vars().len();
    let bound = crate::lower::bind_elided_lifetimes(db, function, raw, frame);
    let scope = SolverDefId::FunctionId(function);
    let late = |var: usize| {
        Region::new_late_param(
            interner,
            scope,
            BoundRegion { var: BoundVar::from_usize(var), kind: BoundRegionKind::Anon },
        )
    };
    let inputs = interner.liberate_late_bound_regions(scope, bound).inputs();
    let output = interner.liberate_late_bound_regions(scope, raw).output();
    // Where the parameters' lifetimes could not be bound, no left-out one is known.
    let bound_all = bound.bound_vars().len() > named || !raw.skip_binder().references_error();
    let output = if bound_all {
        let elided = crate::lower::elided_lifetimes(db, function);
        let mut regions = Vec::new_in(frame);
        regions.extend((0..elided.len() as usize).map(|k| Some(late(named + k))));
        let output = crate::lower::with_elided_regions(db, output, function, &regions);
        match elided.output {
            Some(ElidedOutput::Elided(index)) => {
                rustc_type_ir::fold_regions(interner, output, |region, _| {
                    if region.is_error() { late(named + index as usize) } else { region }
                })
            }
            Some(ElidedOutput::Written(_)) | None => output,
        }
    } else {
        output
    };
    let mut tys = Vec::with_capacity_in(inputs.len() + 1, frame);
    tys.extend(inputs.iter().copied());
    tys.push(output);
    FnSig { inputs_and_output: crate::next_solver::Tys::new_from_slice(&tys), ..raw.skip_binder() }
}

/// Relates the trait method's signature, instantiated for the impl and normalized, with the impl
/// method's, as far as both have the same shape: each lifetime of the impl's at a place the
/// trait's has one is recorded with it. Where the trait's has one of its RPITITs (an inference
/// variable, [`Collector`]) or the two differ (another type, an error, an alias on one side) it
/// stops there and records nothing below; the signatures' agreement is checked where they are
/// compared (E0053).
struct Pair<'db, 'f> {
    interner: DbInterner<'db>,
    /// The impl's lifetime and the trait's at the same place, in the order met.
    regions: Vec<(Region<'db>, Region<'db>), &'f Frame>,
}

impl<'db> TypeRelation<DbInterner<'db>> for Pair<'db, '_> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn relate_ty_args(
        &mut self,
        a_ty: Ty<'db>,
        _: Ty<'db>,
        _: SolverDefId<'db>,
        a_args: GenericArgs<'db>,
        b_args: GenericArgs<'db>,
        _: impl FnOnce(GenericArgs<'db>) -> Ty<'db>,
    ) -> RelateResult<'db, Ty<'db>> {
        relate::relate_args_invariantly(self, a_args, b_args)?;
        Ok(a_ty)
    }

    fn relate_with_variance<T: Relate<DbInterner<'db>>>(
        &mut self,
        _: rustc_type_ir::Variance,
        _: VarianceDiagInfo<DbInterner<'db>>,
        a: T,
        b: T,
    ) -> RelateResult<'db, T> {
        self.relate(a, b)
    }

    fn tys(&mut self, a: Ty<'db>, b: Ty<'db>) -> RelateResult<'db, Ty<'db>> {
        if matches!(a.kind(), TyKind::Infer(_))
            || matches!(b.kind(), TyKind::Infer(_))
            || matches!(a.kind(), TyKind::Alias(..)) != matches!(b.kind(), TyKind::Alias(..))
        {
            return Ok(a);
        }
        // A difference below is the signatures' disagreement, reported where they are compared.
        // Two rigid aliases of one item relate by their arguments.
        let _ = relate::structurally_relate_tys(self, a, b);
        Ok(a)
    }

    fn regions(&mut self, a: Region<'db>, b: Region<'db>) -> RelateResult<'db, Region<'db>> {
        if !matches!(a.kind(), RegionKind::ReBound(..)) {
            self.regions.push((b, a));
        }
        Ok(a)
    }

    fn consts(
        &mut self,
        a: crate::next_solver::Const<'db>,
        b: crate::next_solver::Const<'db>,
    ) -> RelateResult<'db, crate::next_solver::Const<'db>> {
        let _ = relate::structurally_relate_consts(self, a, b);
        Ok(a)
    }

    fn binders<T: Relate<DbInterner<'db>>>(
        &mut self,
        a: Binder<'db, T>,
        b: Binder<'db, T>,
    ) -> RelateResult<'db, Binder<'db, T>> {
        if a.bound_vars().len() != b.bound_vars().len() {
            return Err(TypeError::Mismatch);
        }
        self.relate(a.skip_binder(), b.skip_binder())?;
        Ok(a)
    }
}

/// rustc's `ImplTraitInTraitCollector`: each of `trait_fn`'s RPITITs in the trait's signature
/// becomes an inference variable, the same one each time, with its item bounds, themselves so
/// folded (an `impl Trait` nested in another's bounds gets its own), to be proven of it.
struct Collector<'db, 'a, 'f> {
    interner: DbInterner<'db>,
    infcx: &'a crate::next_solver::infer::InferCtxt<'db>,
    trait_fn: FunctionId,
    /// By the RPITIT's index: its variable and the arguments it has in the signature.
    vars: Vec<Option<(Ty<'db>, GenericArgs<'db>)>>,
    /// Each bound to prove, with the index of the RPITIT it bounds.
    bounds: Vec<(Clause<'db>, u32), &'f Frame>,
}

impl<'db> rustc_type_ir::TypeFolder<DbInterner<'db>> for Collector<'db, '_, '_> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        let Some((rpitit, args)) = as_rpitit(ty) else { return ty.super_fold_with(self) };
        // One bound inside the projection can't be named outside it (rustc's "FIXME(RPITIT):
        // error here"): it stays, and its item gets no value.
        if rpitit.function != self.trait_fn || args.has_escaping_bound_vars() {
            return ty.super_fold_with(self);
        }
        let slot = rpitit.idx.into_raw().into_u32() as usize;
        if let Some((var, _)) = self.vars[slot] {
            return var;
        }
        let var = self.infcx.next_ty_var(crate::Span::Dummy);
        self.vars[slot] = Some((var, args));
        let bounds = rpitit_item_bounds(self.interner.db, rpitit.function, rpitit.idx);
        for clause in bounds.all_bounds().iter_instantiated(self.interner, args) {
            let clause: Clause<'db> = clause.skip_norm_wip().fold_with(self);
            self.bounds.push((clause, slot as u32));
        }
        var
    }
}

/// Writes an impl's type for an RPITIT in the generics of the impl's item: each lifetime of
/// the impl method's own (liberated) becomes the trait's at its place in the signatures, and
/// each of the trait method's that the RPITIT takes as an argument becomes the impl item's own
/// parameter at that place (rustc's `RemapHiddenTyRegions`).
struct Remap<'db, 'a> {
    interner: DbInterner<'db>,
    impl_fn: FunctionId,
    trait_fn: FunctionId,
    regions: &'a [(Region<'db>, Region<'db>)],
    /// The arguments of the trait's RPITITs in the trait's signature as instantiated.
    trait_args: &'a [GenericArg<'db>],
    /// The identity arguments of the impl's item: the impl's, then the RPITIT's own.
    identity: &'a [GenericArg<'db>],
    /// How many of the arguments of the trait's and of the impl's item are the RPITIT's own.
    own_len: usize,
    /// The variances of the trait's item being remapped, one per argument of `trait_args`: a
    /// lifetime it does not capture (`use<..>` leaves it out) is bivariant. rustc's item has
    /// only the lifetimes it captures as its own arguments.
    captures: &'a [Variance],
    /// The name of the first lifetime met that the trait's `impl Trait` does not capture.
    uncaptured: Option<Box<str>>,
}

impl<'db> Remap<'db, '_> {
    /// The impl item's parameter for a lifetime of the trait's signature, by its place among
    /// the trait RPITIT's own arguments.
    fn of_trait(&self, region: Region<'db>) -> Option<Region<'db>> {
        let own_start = self.trait_args.len().checked_sub(self.own_len)?;
        let at = self.trait_args[own_start..].iter().enumerate().position(|(at, arg)| {
            arg.region() == Some(region) && self.captures[own_start + at] != Variance::Bivariant
        })?;
        self.identity.get(self.identity.len().checked_sub(self.own_len)? + at)?.region()
    }

    fn uncaptured(&mut self, region: Region<'db>) -> Region<'db> {
        if self.uncaptured.is_none() {
            self.uncaptured = Some(lifetime_name(self.interner.db, region));
        }
        region
    }
}

/// The name of `region` as written, `'a`, for an error's text: a lifetime parameter's (early-
/// or late-bound) or a `for<'a>` one's, else `'_` (one left out, or inferred).
fn lifetime_name(db: &Db, region: Region<'_>) -> Box<str> {
    let named = match region.kind() {
        RegionKind::ReStatic => return "'static".into(),
        RegionKind::ReEarlyParam(param) => {
            let params = hir_def::hir::generics::GenericParams::of(db, param.id.parent);
            // One past them is an `impl Trait`'s for a lifetime the parameters leave out.
            ((param.id.local_id.into_raw().into_u32() as usize) < params.len_lifetimes()
                && !params[param.id.local_id].is_fresh())
            .then(|| params[param.id.local_id].name.as_str().into())
        }
        RegionKind::ReLateParam(late) => match late.kind.kind {
            // The function's `var`th late-bound parameter.
            BoundRegionKind::Named(SolverDefId::FunctionId(function)) => {
                match crate::generics::generics(db, function.into())
                    .iter_self_late_bound()
                    .nth(late.kind.var.as_usize())
                {
                    Some((
                        _,
                        hir_def::hir::generics::GenericParamDataRef::LifetimeParamData(it),
                    )) if !it.is_fresh() => Some(it.name.as_str().into()),
                    _ => None,
                }
            }
            BoundRegionKind::NamedForPrinting(name) => Some(name.0.as_str().into()),
            _ => None,
        },
        _ => None,
    };
    // alloc: an error's text
    named.unwrap_or_else(|| "'_".into())
}

impl<'db> rustc_type_ir::TypeFolder<DbInterner<'db>> for Remap<'db, '_> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        if !ty.has_free_regions() {
            return ty;
        }
        // The lifetimes an opaque type does not capture are not in its hidden type, so they
        // need no place in the trait's item (rustc's `RemapHiddenTyRegions` skips them). Here
        // an opaque's arguments hold its function's late-bound lifetimes too, which the item
        // cannot name: those become `'static`, as rustc's `ReverseMapper` makes a region it
        // cannot map in an opaque's arguments; a bivariant argument does not change the type.
        if let TyKind::Alias(_, alias) = ty.kind()
            && let AliasTyKind::Opaque { def_id } = alias.kind
        {
            let variances =
                self.interner.variances_of(SolverDefId::InternedOpaqueTyId(def_id.0)).as_slice();
            let args = GenericArgs::new_from_iter(alias.args.iter().zip(variances).map(
                |(arg, variance)| match (arg.region(), variance) {
                    (Some(region), Variance::Bivariant) => match region.kind() {
                        RegionKind::ReLateParam(_) => Region::new_static(self.interner).into(),
                        _ => arg,
                    },
                    _ => arg.fold_with(self),
                },
            ));
            return Ty::new_alias(
                self.interner,
                IsRigid::No,
                AliasTy::new_from_args(self.interner, alias.kind, args),
            );
        }
        ty.super_fold_with(self)
    }

    fn fold_region(&mut self, region: Region<'db>) -> Region<'db> {
        match region.kind() {
            RegionKind::ReBound(..)
            | RegionKind::ReStatic
            | RegionKind::ReError(_)
            | RegionKind::ReErased
            | RegionKind::ReEarlyParam(_) => region,
            RegionKind::ReLateParam(late)
                if late.scope == SolverDefId::FunctionId(self.trait_fn) =>
            {
                self.of_trait(region).unwrap_or_else(|| self.uncaptured(region))
            }
            RegionKind::ReLateParam(late)
                if late.scope == SolverDefId::FunctionId(self.impl_fn) =>
            {
                // The trait's lifetime at the same place in the signatures; one the impl's
                // takes at two places is the trait's only if both are the same one.
                let mut found = self.regions.iter().filter(|(i, _)| *i == region).map(|(_, t)| *t);
                let Some(first) = found.next() else { return self.uncaptured(region) };
                if found.any(|other| other != first) {
                    return self.uncaptured(region);
                }
                match first.kind() {
                    RegionKind::ReLateParam(_) => {
                        self.of_trait(first).unwrap_or_else(|| self.uncaptured(region))
                    }
                    _ => first,
                }
            }
            RegionKind::ReLateParam(_) | RegionKind::ReVar(_) | RegionKind::RePlaceholder(_) => {
                self.uncaptured(region)
            }
        }
    }
}

/// The bounds of an RPITIT: for the trait's, the bounds of its opaque type with that type (and
/// each other of its method's) the trait's item; an impl's has none of its own.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub(crate) fn rpitit_item_bounds(
    db: &Db,
    function: FunctionId,
    idx: ImplTraitIdx,
) -> crate::next_solver::util::ItemBounds {
    crate::next_solver::util::ItemBounds::of_rpitit(db, RpititId { function, idx })
}

/// The RPITIT a read of [`rpitit_item_bounds`] with the key `key` asks the bounds of, or `None`
/// when `key` is no key of it: for the item and goal stores, which take the read for its trait
/// method's signature.
pub fn rpitit_item_bounds_of_key(db: &Db, key: blaze_query::db::Id) -> Option<RpititId> {
    rpitit_item_bounds::arguments(db, key).map(|(function, idx)| RpititId { function, idx })
}

/// rustc's `predicates_of` an impl's RPITIT item (`gather_explicit_predicates_of`'s
/// `ImplTraitInTraitData::Impl` arm): the trait item's predicates, which are its method's,
/// rebased onto the impl: the impl's, then the trait method's own instantiated for the impl
/// (`trait_to_impl_args`). The solver reads it for [`RpititId`]s of an impl. These are the
/// clauses of the hybrid environment ([`crate::lower::hybrid_param_env`]) of `impl_fn`, kept in
/// a `GenericPredicates`' order; that one elaborates and normalizes them.
///
/// The impl method's own where clauses are not in it: E0276 keeps them no stronger than the
/// trait's, and they are what the comparison checks.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn impl_method_hybrid_predicates(db: &Db, impl_fn: FunctionId) -> GenericPredicates {
    let ItemContainerId::ImplId(impl_) = impl_fn.lookup(db).container else {
        panic!(
            "the hybrid environment is of a trait impl's method, not of `{}`",
            FunctionSignature::of(db, impl_fn).name.as_str()
        )
    };
    let (Some(trait_fn), Some(trait_ref)) =
        (implemented_trait_method(db, impl_fn), db.impl_trait(impl_))
    else {
        panic!(
            "the hybrid environment is of a method implementing a trait's, not of `{}`",
            FunctionSignature::of(db, impl_fn).name.as_str()
        )
    };
    let interner = DbInterner::new_with(db, hir_def::HasModule::krate(&impl_fn, db));
    let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
    let args = GenericArgs::identity_for_item(interner, impl_fn.into()).rebase_onto(
        interner,
        impl_.into(),
        trait_ref.args,
    );
    let impl_predicates = GenericPredicates::query(db, impl_.into());
    let trait_predicates = GenericPredicates::query(db, trait_fn.into());
    let own = |range: std::ops::Range<u32>| {
        trait_predicates
            .predicates
            .get()
            .map_bound(move |it| {
                it.as_slice()[range.start as usize..range.end as usize].iter().copied()
            })
            .iter_instantiated(interner, args.as_slice())
            .map(|it| it.skip_norm_wip())
    };
    let trait_len = trait_predicates.predicates.get().skip_binder().len() as u32;
    // rustc's order of a `GenericPredicates`: the parent's (the impl's own, which has none),
    // its associated types' bounds, then the item's own and its associated types' bounds.
    let parent: &[Clause<'_>] = impl_predicates.predicates.get().skip_binder().as_slice();
    let own_non_assoc =
        own(trait_predicates.own_predicates_start..trait_predicates.own_assoc_ty_bounds_start);
    let own_assoc = own(trait_predicates.own_assoc_ty_bounds_start..trait_len);
    let mut clauses = Vec::new(); // alloc: the memo's clauses, interned below
    clauses.extend(parent.iter().copied());
    let own_predicates_start = clauses.len() as u32;
    clauses.extend(own_non_assoc);
    let own_assoc_ty_bounds_start = clauses.len() as u32;
    clauses.extend(own_assoc);
    GenericPredicates {
        predicates: StoredEarlyBinder::bind(interner, Clauses::new_from_iter(clauses).store()),
        has_trait_implied_predicate: impl_predicates.has_trait_implied_predicate,
        // The impl's own predicates, both kinds, are the parent's here; its associated type
        // bounds begin where they did in the impl's.
        parent_assoc_ty_bounds_start: impl_predicates.own_assoc_ty_bounds_start,
        own_predicates_start,
        own_assoc_ty_bounds_start,
    }
}

/// rustc's `ImplTraitInTraitFinder` (`rustc_ty_utils::ty::param_env`): for a trait method with
/// a default body, the clause `<Self as Trait>::{impl Trait#i}<..> == Opaque(trait_fn, i)<..>`
/// for each `impl Trait` of its return type, nested ones in another's bounds too, bound by the
/// signature's late-bound lifetimes. In that environment the default body's return type
/// normalizes to its own opaque type, which the body defines. Empty for any other function.
pub(crate) fn default_body_clauses<'db, 'f>(
    interner: DbInterner<'db>,
    function: FunctionId,
    frame: &'f Frame,
) -> std::vec::Vec<Clause<'db>, &'f Frame> {
    let db = interner.db;
    let mut finder = DefaultBodyFinder {
        interner,
        function,
        depth: rustc_type_ir::DebruijnIndex::ZERO,
        seen: std::vec::Vec::new_in(frame),
        clauses: std::vec::Vec::new_in(frame),
        bound_vars: None,
    };
    if !is_trait_method(db, function) || !FunctionSignature::of(db, function).has_body() {
        return finder.clauses;
    }
    let sig = db.callable_item_signature(function.into()).instantiate_identity().skip_norm_wip();
    finder.bound_vars = Some(sig.bound_vars());
    // The signature's binder is accounted for: its bound regions are the clauses'.
    sig.skip_binder().output().visit_with(&mut finder);
    finder.clauses
}

struct DefaultBodyFinder<'db, 'f> {
    interner: DbInterner<'db>,
    function: FunctionId,
    depth: rustc_type_ir::DebruijnIndex,
    seen: std::vec::Vec<RpititId, &'f Frame>,
    clauses: std::vec::Vec<Clause<'db>, &'f Frame>,
    bound_vars: Option<crate::next_solver::BoundVarKinds<'db>>,
}

impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for DefaultBodyFinder<'db, '_> {
    type Result = ();

    fn visit_binder<T: rustc_type_ir::TypeVisitable<DbInterner<'db>>>(
        &mut self,
        binder: &Binder<'db, T>,
    ) {
        self.depth.shift_in(1);
        rustc_type_ir::TypeSuperVisitable::super_visit_with(binder, self);
        self.depth.shift_out(1);
    }

    fn visit_ty(&mut self, ty: Ty<'db>) {
        let interner = self.interner;
        if let Some((rpitit, args)) = as_rpitit(ty)
            && rpitit.function == self.function
            && !self.seen.contains(&rpitit)
        {
            self.seen.push(rpitit);
            // Out of the binders walked into to reach it; a region bound inside the projection
            // itself cannot be named outside it (rustc's "we don't support this").
            let depth = self.depth;
            let shifted =
                rustc_type_ir::fold_regions(interner, args, |region, inner| match region.kind() {
                    RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(index), bound)
                        if inner == rustc_type_ir::INNERMOST =>
                    {
                        Region::new_bound(interner, index.shifted_out_to_binder(depth), bound)
                    }
                    RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(_), _) => {
                        <Region<'db> as crate::next_solver::RegionExt<'db>>::error(interner)
                    }
                    _ => region,
                });
            let projection = crate::next_solver::ProjectionClause {
                projection_term: rustc_type_ir::AliasTerm::new_from_args(
                    interner,
                    crate::next_solver::AliasTermKind::ProjectionTy {
                        def_id: TraitAssocTyId(AssocTyId::RpititId(rpitit)),
                    },
                    shifted,
                ),
                term: Ty::new_opaque(interner, IsRigid::No, rpitit.opaque(interner.db), shifted)
                    .into(),
            };
            let bound_vars = self.bound_vars.expect("set before the signature is walked");
            self.clauses.push(rustc_type_ir::Upcast::upcast(
                Binder::bind_with_vars(projection, bound_vars),
                interner,
            ));
            let bounds = rpitit_item_bounds(interner.db, rpitit.function, rpitit.idx);
            for clause in bounds.all_bounds().iter_instantiated(interner, args) {
                clause.skip_norm_wip().visit_with(self);
            }
        }
        rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self);
    }
}
