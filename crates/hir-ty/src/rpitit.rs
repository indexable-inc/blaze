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
    FunctionId, ImplId, ItemContainerId, Lookup, TraitId,
    signatures::FunctionSignature,
};
use rustc_type_ir::{
    BoundVar, IsRigid, TypeFoldable, TypeSuperFoldable, TypeVisitableExt,
    error::TypeError,
    inherent::{GenericArgs as _, IntoKind, Ty as _},
    relate::{self, Relate, TypeRelation, VarianceDiagInfo},
};

use crate::{
    ImplTraitId,
    db::{HirDatabase, InternedOpaqueTyId},
    lower::{ElidedOutput, ImplTraitIdx, SelfAndAssocBounds},
    next_solver::{
        AliasTy, AliasTyKind, AssocTyId, Binder, BoundRegion, BoundRegionKind, Clause,
        DbInterner, EarlyBinder, FnSig, GenericArg, GenericArgKind, GenericArgs, Region,
        RegionKind, SolverDefId, StoredEarlyBinder, StoredTy, TraitAssocTyId, Ty, TyKind,
        TypingMode,
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

/// `type_of` of an RPITIT. The trait's item has a value only when its method has a default
/// body: that body's opaque type, with the item's own arguments, which are the opaque's.
pub fn type_of_rpitit<'db>(interner: DbInterner<'db>, rpitit: RpititId) -> EarlyBinder<'db, Ty<'db>> {
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
            EarlyBinder::bind(interner, Ty::new_alias(
                interner,
                IsRigid::No,
                AliasTy::new_from_args(
                    interner,
                    AliasTyKind::Opaque { def_id: opaque.into() },
                    args,
                ),
            ))
        }
        RpititSide::Impl { .. } => match impl_rpitit_tys(db, rpitit.function)
            .get(rpitit.idx.into_raw().into_u32() as usize)
        {
            Some(Ok(ty)) => ty.get(),
            // Reported where the impl's method is checked (`RpititError`).
            Some(Err(_)) | None => {
                EarlyBinder::bind(
                    interner,
                    Ty::new_error(interner, crate::next_solver::ErrorGuaranteed),
                )
            }
        },
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
    /// "return type captures more lifetimes than trait definition".
    UncapturedLifetime { lifetime: Box<str> },
    /// The type of an `impl Trait` in the bounds of another could not be worked out from the
    /// outer one's: proving those bounds failed or stayed ambiguous.
    Unsolved,
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
            RpititError::UncapturedLifetime { lifetime } => format!(
                "the return type of the method `{impl_fn}` captures more lifetimes than the \
                 trait's definition: its type for {item} names `{lifetime}`, which the trait's \
                 `impl Trait` does not capture"
            ),
            RpititError::Unsolved => format!(
                "the type the method `{impl_fn}` gives {item} could not be worked out from the \
                 bounds of the `impl Trait` it is in"
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

fn impl_rpitit_tys_in(
    db: &Db,
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
    let trait_to_impl_args = GenericArgs::identity_for_item(interner, impl_fn.into())
        .rebase_onto(interner, impl_.into(), trait_ref.args);
    let trait_sig = named_signature(db, trait_fn, trait_to_impl_args, frame);
    let impl_sig = named_signature(
        db,
        impl_fn,
        GenericArgs::identity_for_item(interner, impl_fn.into()),
        frame,
    );
    if trait_sig.inputs().len() != impl_sig.inputs().len() {
        // E0050, reported where the signatures are compared.
        return all(RpititError::NotFound);
    }

    let mut relation = Pair {
        interner,
        trait_fn,
        regions: Vec::new_in(frame),
        values: vec![None; count], // alloc: per impl method, one slot per `impl Trait`
    };
    for (&t, &i) in trait_sig.inputs().iter().zip(impl_sig.inputs()) {
        relation.relate(t, i).expect("`Pair` relates as far as both have the same shape");
    }
    relation
        .relate(trait_sig.output(), impl_sig.output())
        .expect("`Pair` relates as far as both have the same shape");
    let Pair { regions, mut values, .. } = relation;

    // Where an RPITIT sits in the trait's return type, the arguments it has there: the same
    // for every one of `trait_fn`'s, which share the method's generics.
    let mut trait_args = None;
    let _ = trait_sig.output().visit_with(&mut FindRpitit { trait_fn, found: &mut trait_args });

    // An `impl Trait` in the bounds of another has the type those bounds give it once the
    // outer one's is known (rustc's `ImplTraitInTraitCollector` registers the bounds of each it
    // replaces). Each round finds at least one, or stops.
    if let Some(args) = trait_args {
        for _round in 0..count {
            if values.iter().all(Option::is_some) {
                break;
            }
            match solve_nested(interner, impl_, trait_fn, args, &values) {
                Ok(found) if found.iter().zip(&values).any(|(new, old)| new.is_some() && old.is_none()) => {
                    for (slot, new) in values.iter_mut().zip(found) {
                        if slot.is_none() {
                            *slot = new;
                        }
                    }
                }
                Ok(_) => break,
                Err(()) => {
                    for slot in values.iter_mut().filter(|it| it.is_none()) {
                        *slot = Some(Err(RpititError::Unsolved));
                    }
                    break;
                }
            }
        }
    }

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
        error: None,
    };
    // alloc: the memo, one entry per `impl Trait`
    values
        .into_iter()
        .map(|value| match value {
            None => Err(RpititError::NotFound),
            Some(Err(error)) => Err(error),
            Some(Ok(ty)) => {
                remap.error = None;
                let ty = ty.fold_with(&mut remap);
                match remap.error.take() {
                    Some(error) => Err(error),
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
/// lifetime this cannot tell stays an error region, as lowering left it.
fn named_signature<'db>(
    db: &'db Db,
    function: FunctionId,
    args: GenericArgs<'db>,
    frame: &Frame,
) -> FnSig<'db> {
    let interner = DbInterner::new_no_crate(db);
    let raw = db
        .callable_item_signature(function.into())
        .instantiate(interner, args)
        .skip_norm_wip();
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
    FnSig {
        inputs_and_output: crate::next_solver::Tys::new_from_slice(&tys),
        ..raw.skip_binder()
    }
}

/// Relates the trait method's signature, instantiated for the impl, with the impl method's, as
/// far as both have the same shape: each lifetime of the impl's at a place the trait's has one
/// is recorded with it, and the impl's type where the trait's has one of `trait_fn`'s RPITITs
/// is that RPITIT's value. Where the two differ (a projection the impl wrote normalized,
/// another type, an error) it stops there and records nothing below; the signatures' agreement
/// is checked where they are compared (E0053).
struct Pair<'db, 'f> {
    interner: DbInterner<'db>,
    trait_fn: FunctionId,
    /// The impl's lifetime and the trait's at the same place, in the order met.
    regions: Vec<(Region<'db>, Region<'db>), &'f Frame>,
    values: Vec<Option<Result<Ty<'db>, RpititError>>>,
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
        if let Some((rpitit, _)) = as_rpitit(a)
            && rpitit.function == self.trait_fn
        {
            let slot = &mut self.values[rpitit.idx.into_raw().into_u32() as usize];
            match slot {
                None => *slot = Some(Ok(b)),
                // One `impl Trait` appears once in its method's return type.
                Some(_) => {}
            }
            return Ok(a);
        }
        if matches!(a.kind(), TyKind::Alias(..)) || matches!(b.kind(), TyKind::Alias(..)) {
            return Ok(a);
        }
        // A difference below is the signatures' disagreement, reported where they are compared.
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

/// Finds the arguments of the first of `trait_fn`'s RPITITs in a type.
struct FindRpitit<'db, 'a> {
    trait_fn: FunctionId,
    found: &'a mut Option<GenericArgs<'db>>,
}

impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for FindRpitit<'db, '_> {
    type Result = std::ops::ControlFlow<()>;

    fn visit_ty(&mut self, ty: Ty<'db>) -> Self::Result {
        if let Some((rpitit, args)) = as_rpitit(ty)
            && rpitit.function == self.trait_fn
        {
            *self.found = Some(args);
            return std::ops::ControlFlow::Break(());
        }
        rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
    }
}

use rustc_type_ir::TypeVisitable as _;

/// The types the bounds of each RPITIT whose type is known give the RPITITs written in them:
/// `impl Iterator<Item = impl Display>` given `vec::IntoIter<u8>` gives the inner one `u8`.
/// `Err` when proving the bounds fails.
fn solve_nested<'db>(
    interner: DbInterner<'db>,
    impl_: ImplId,
    trait_fn: FunctionId,
    trait_args: GenericArgs<'db>,
    values: &[Option<Result<Ty<'db>, RpititError>>],
) -> Result<Vec<Option<Result<Ty<'db>, RpititError>>>, ()> {
    let db = interner.db;
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let param_env = db.trait_environment(impl_.into());
    // alloc: per impl method with nested `impl Trait`, one per `impl Trait`
    let vars: Vec<Ty<'db>> = values
        .iter()
        .map(|value| match value {
            Some(Ok(ty)) => *ty,
            _ => infcx.next_ty_var(crate::Span::Dummy),
        })
        .collect();
    let mut replace = BottomUpFolder {
        interner,
        ty_op: |ty: Ty<'db>| match as_rpitit(ty) {
            Some((rpitit, _)) if rpitit.function == trait_fn => {
                vars[rpitit.idx.into_raw().into_u32() as usize]
            }
            _ => ty,
        },
        lt_op: |region| region,
        ct_op: |ct| ct,
    };
    let mut ocx = ObligationCtxt::new(&infcx);
    let cause = ObligationCause::dummy();
    for (index, value) in values.iter().enumerate() {
        let Some(Ok(_)) = value else { continue };
        let rpitit = RpititId {
            function: trait_fn,
            idx: ImplTraitIdx::from_raw(la_arena::RawIdx::from_u32(index as u32)),
        };
        let bounds = rpitit_item_bounds(db, rpitit.function, rpitit.idx);
        for clause in bounds.all_bounds().iter_instantiated(interner, trait_args) {
            let clause: Clause<'db> = clause.skip_norm_wip().fold_with(&mut replace);
            ocx.register_obligation(Obligation::new(
                interner,
                // clone: an obligation's cause, once per bound of a nested `impl Trait`
                cause.clone(),
                param_env,
                clause,
            ));
        }
    }
    if !ocx.evaluate_obligations_error_on_ambiguity().is_empty() {
        return Err(());
    }
    // alloc: per impl method with nested `impl Trait`, one per `impl Trait`
    Ok(vars
        .iter()
        .zip(values)
        .map(|(&var, value)| match value {
            Some(_) => None,
            None => {
                let ty = infcx.deeply_resolve_via_unification_table(var);
                (!ty.has_infer_types()).then_some(Ok(ty))
            }
        })
        .collect())
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
    error: Option<RpititError>,
}

impl<'db> Remap<'db, '_> {
    /// The impl item's parameter for a lifetime of the trait's signature, by its place among
    /// the trait RPITIT's own arguments.
    fn of_trait(&self, region: Region<'db>) -> Option<Region<'db>> {
        let own_start = self.trait_args.len().checked_sub(self.own_len)?;
        let at =
            self.trait_args[own_start..].iter().position(|arg| arg.region() == Some(region))?;
        self.identity.get(self.identity.len().checked_sub(self.own_len)? + at)?.region()
    }

    fn uncaptured(&mut self, region: Region<'db>) -> Region<'db> {
        if self.error.is_none() {
            self.error = Some(RpititError::UncapturedLifetime {
                // alloc: an error's text
                lifetime: format!("{region:?}").into(),
            });
        }
        region
    }
}

impl<'db> rustc_type_ir::TypeFolder<DbInterner<'db>> for Remap<'db, '_> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        if !ty.has_free_regions() {
            return ty;
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
            RegionKind::ReLateParam(late) if late.scope == SolverDefId::FunctionId(self.trait_fn) => {
                self.of_trait(region).unwrap_or_else(|| self.uncaptured(region))
            }
            RegionKind::ReLateParam(late) if late.scope == SolverDefId::FunctionId(self.impl_fn) => {
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
            RegionKind::ReLateParam(_)
            | RegionKind::ReVar(_)
            | RegionKind::RePlaceholder(_) => self.uncaptured(region),
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
    let sig = db
        .callable_item_signature(function.into())
        .instantiate_identity()
        .skip_norm_wip();
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
            let shifted = rustc_type_ir::fold_regions(interner, args, |region, inner| {
                match region.kind() {
                    RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(index), bound)
                        if inner == rustc_type_ir::INNERMOST =>
                    {
                        Region::new_bound(interner, index.shifted_out_to_binder(depth), bound)
                    }
                    RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(_), _) => {
                        <Region<'db> as crate::next_solver::RegionExt<'db>>::error(interner)
                    }
                    _ => region,
                }
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
