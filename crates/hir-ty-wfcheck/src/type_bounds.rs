//! rustc's `check_type_bounds` (`compare_impl_item.rs`): the type an impl gives an associated
//! type of its trait, and the type it gives each `impl Trait` a trait method returns (an
//! RPITIT, an associated type of the trait too), satisfies the bounds the trait declares of
//! it, in the impl's item's environment, outlives bounds included. `type A: 'static` given
//! `&'a u8` is E0477, `type A: Copy` given `String` E0277, and `-> impl Get + 'static` in the
//! trait given `-> impl Get` capturing `'a` in the impl is E0477: callers trust the trait's
//! bounds (`T::A: 'static`), so an impl that broke them would let a borrow outlive its scope.

#[cfg(test)]
mod tests;

use hir_def::{AssocItemId, FunctionId, ImplId, TypeAliasId};
use rustc_type_ir::{
    AliasTermKind, AliasTyKind, ClauseKind, IsRigid, OutlivesClause, TypeFoldable as _, TypeFolder,
    TypeSuperFoldable as _, TypeSuperVisitable as _, TypeVisitable as _, TypeVisitableExt as _,
    TypeVisitor,
    inherent::{GenericArgs as _, IntoKind as _, Ty as _},
};

use hir_ty::{
    ImplTraitId,
    db::HirDatabase as _,
    display::{DisplayTarget, HirDisplay as _},
    next_solver::{
        AssocTyId, Clause, DbInterner, EarlyBinder, GenericArgs, Predicate, SolverDefId,
        TraitAssocTyId, TraitRef, Ty, TyKind,
    },
    rpitit::{RpititId, as_rpitit, impl_rpitit_tys, implemented_trait_method, named_signature},
};

use crate::{
    Check, Site,
    mem::{self, FVec},
    outlives::{self, Known, Needs, Wfcx},
    push_new,
};

impl<'f, 'db> Check<'_, 'f, 'db> {
    /// The bounds of the trait's associated types, and of its methods' `impl Trait`s, hold of
    /// the types `impl_` gives them.
    pub(crate) fn type_bounds(&mut self, impl_: ImplId) {
        let db = self.db;
        let Some(trait_ref) = db.impl_trait(impl_) else { return };
        let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
        if trait_ref.references_error() {
            return;
        }
        let Some(header) = self.header(impl_) else { return };
        for &(ref name, item) in impl_.impl_items(db).items.iter() {
            match item {
                AssocItemId::TypeAliasId(alias) => {
                    let Some(in_trait) =
                        trait_ref.def_id.0.trait_items(db).associated_type_by_name(name)
                    else {
                        continue;
                    };
                    self.assoc_type_bounds(impl_, trait_ref, &header, alias, in_trait);
                }
                AssocItemId::FunctionId(function) => {
                    self.rpitit_bounds(impl_, trait_ref, &header, function)
                }
                _ => (),
            }
        }
    }

    /// The bounds of the trait's associated type `in_trait` hold of `alias`'s type, in
    /// `alias`'s environment, knowing what the impl's header implies.
    fn assoc_type_bounds(
        &mut self,
        impl_: ImplId,
        trait_ref: TraitRef<'db>,
        header: &[Ty<'db>],
        alias: TypeAliasId,
        in_trait: TypeAliasId,
    ) {
        let db = self.db;
        let interner = DbInterner::new_with(db, hir_def::HasModule::krate(&impl_, db));
        let value = db.ty(alias.into());
        if value.skip_binder().references_error() {
            return;
        }
        // rustc's `rebased_args`: the impl's trait arguments, then the item's own.
        let args = GenericArgs::identity_for_item(interner, alias.into()).rebase_onto(
            interner,
            impl_.into(),
            trait_ref.args,
        );
        if args.len() != GenericArgs::identity_for_item(interner, in_trait.into()).len() {
            // The two differ in their own generic parameters, E0049, reported where they are
            // compared.
            return;
        }
        let items = [(AssocTyId::TypeAliasId(in_trait), value)];
        let mut replace = Items {
            interner,
            trait_args: trait_ref.args,
            impl_args: GenericArgs::identity_for_item(interner, impl_.into()),
            items: &items,
        };
        let clauses = mem::collect(
            self.frame,
            rustc_type_ir::Interner::item_bounds(interner, SolverDefId::TypeAliasId(in_trait))
                .iter_instantiated(interner, args.as_slice())
                .map(|clause| clause.skip_norm_wip().fold_with(&mut replace)),
        );
        let wfcx = Wfcx::new(self.frame, db, alias.into());
        let assumed = normalized(self.frame, &wfcx, header.iter().copied());
        let known = self.known(alias.into(), &assumed);
        self.prove_bounds(&wfcx, &known, &clauses, alias.into(), Site::AssocType(alias));
    }

    /// The bounds of each `impl Trait` of the return type of the trait method `impl_fn`
    /// implements hold of the type `impl_fn` gives it, in the environment of the impl's items
    /// for them. They are checked in the trait method's signature instantiated for the impl
    /// (its lifetimes the trait method's), where the trait's bounds are written; the impl's
    /// types are in the generics of the impl's items, whose own arguments are those the
    /// trait's items have there. What it knows is what the impl's header and the trait
    /// method's parameters imply, as rustc's `assumed_wf_types` of an
    /// `ImplTraitInTraitData::Impl` item.
    fn rpitit_bounds(
        &mut self,
        impl_: ImplId,
        trait_ref: TraitRef<'db>,
        header: &[Ty<'db>],
        impl_fn: FunctionId,
    ) {
        let db = self.db;
        let values = impl_rpitit_tys(db, impl_fn);
        if values.is_empty() {
            return;
        }
        let Some(trait_fn) = implemented_trait_method(db, impl_fn) else { return };
        // A type that has no value is an error of its own (`RpititError`); the bounds of the
        // others are proven with every one of the method's known, as rustc installs them all.
        let mut items: FVec<'f, (AssocTyId, EarlyBinder<'db, Ty<'db>>)> =
            Vec::with_capacity_in(values.len(), self.frame);
        for (index, value) in values.iter().enumerate() {
            let Ok(value) = value else { return };
            let idx = impl_trait_idx(index);
            items.push((AssocTyId::RpititId(RpititId { function: trait_fn, idx }), value.get()));
        }
        let interner = DbInterner::new_with(db, hir_def::HasModule::krate(&impl_, db));
        let trait_to_impl_args = GenericArgs::identity_for_item(interner, impl_fn.into())
            .rebase_onto(interner, impl_.into(), trait_ref.args);
        let sig = named_signature(db, trait_fn, trait_to_impl_args, self.frame);
        let mut trait_args = None;
        let _ = sig.output().visit_with(&mut FindRpitit { trait_fn, found: &mut trait_args });
        let Some(trait_args) = trait_args else { return };

        // rustc's `param_env` of the impl's items for the method's `impl Trait`s: the hybrid
        // environment, as `impl_rpitit_tys` works their types out in.
        let env = hir_ty::hybrid_param_env(interner, impl_, trait_fn, trait_to_impl_args);
        let wfcx = Wfcx::with_env(self.frame, db, impl_fn.into(), env);
        let assumed = normalized(
            self.frame,
            &wfcx,
            sig.inputs().iter().copied().chain(header.iter().copied()),
        );
        let known = self.known_in(env.clauses().as_slice(), &assumed);

        let mut replace = Items {
            interner,
            trait_args: trait_ref.args,
            impl_args: GenericArgs::identity_for_item(interner, impl_.into()),
            items: &items,
        };
        for &(id, value) in items.iter() {
            let AssocTyId::RpititId(rpitit) = id else { continue };
            let value =
                value.instantiate(interner, replace.impl_args_for(trait_args)).skip_norm_wip();
            let site = match value.kind() {
                TyKind::Alias(_, alias)
                    if let AliasTyKind::Opaque { def_id } = alias.kind
                        && let ImplTraitId::ReturnTypeImplTrait(owner, index) =
                            def_id.0.loc(db)
                        && owner == impl_fn =>
                {
                    Site::Opaque(impl_fn, index.into_raw().into_u32())
                }
                _ => Site::Output(impl_fn),
            };
            let clauses = mem::collect(
                self.frame,
                rustc_type_ir::Interner::item_bounds(interner, SolverDefId::RpititId(rpitit))
                    .iter_instantiated(interner, trait_args.as_slice())
                    .map(|clause| clause.skip_norm_wip().fold_with(&mut replace)),
            );
            self.prove_bounds(&wfcx, &known, &clauses, impl_fn.into(), site);
        }
    }

    /// Proves `clauses`, the bounds of a trait's item with the impl's type in place of the
    /// item, given what the impl's item knows: a trait bound or projection that fails is
    /// E0277 or E0271 (and nothing else is reported of the item, as rustc reports region
    /// errors only once the trait bounds hold); what they and the proofs need of what
    /// outlives what must hold. An `impl Trait` outlives a region when a bound it declares
    /// does, else when each argument it captures does.
    fn prove_bounds(
        &mut self,
        wfcx: &Wfcx<'f, 'db>,
        known: &Known<'f, 'db>,
        clauses: &[Clause<'db>],
        item: hir_def::GenericDefId,
        site: Site,
    ) {
        let db = self.db;
        let mut needs = Vec::new_in(self.frame);
        let mut proven: FVec<'f, Clause<'db>> = Vec::new_in(self.frame);
        let mut predicates: FVec<'f, Predicate<'db>> = Vec::new_in(self.frame);
        for &clause in clauses {
            // A bound with an error, as a `'_` lowering could not name, is an error of its own.
            if clause.references_error() {
                continue;
            }
            match clause.kind().skip_binder() {
                ClauseKind::Trait(_) | ClauseKind::Projection(_) => {
                    proven.push(clause);
                    predicates.push(clause.as_predicate());
                }
                // An outlives bound that names a `for<'x>` lifetime is not reasoned about, as
                // rustc drops it (`no_bound_vars`). An RPITIT's bounds are bound by its method's
                // late-bound lifetimes even where they name none.
                ClauseKind::TypeOutlives(_) | ClauseKind::RegionOutlives(_) => {
                    match clause.kind().no_bound_vars() {
                        Some(ClauseKind::TypeOutlives(OutlivesClause(ty, region))) => {
                            let ty = wfcx.normalize(ty, &mut needs);
                            needs.push(Needs::Type(ty, region));
                        }
                        Some(ClauseKind::RegionOutlives(OutlivesClause(longer, shorter))) => {
                            needs.push(Needs::Region(longer, shorter));
                        }
                        _ => (),
                    }
                }
                _ => (),
            }
        }
        let failed = outlives::prove_each(wfcx, &predicates, &mut needs);
        if !failed.is_empty() {
            let krate = hir_def::HasModule::krate(&item, db);
            let target = DisplayTarget::from_crate(db, krate);
            // A bound another failing one implies is not reported, as rustc's `error_implies`:
            // the bounds are elaborated, so `Copy` failing gives its supertrait `Clone` too.
            let interner = wfcx.interner();
            let implied = |clause: Clause<'db>| {
                failed.iter().any(|&other| {
                    proven[other] != clause
                        && rustc_type_ir::elaborate::elaborate(interner, [proven[other]])
                            .any(|it| it == clause)
                })
            };
            for &index in failed.iter() {
                if implied(proven[index]) {
                    continue;
                }
                match proven[index].kind().skip_binder() {
                    ClauseKind::Trait(predicate) => push_new(
                        self.found,
                        "E0277",
                        format_args!(
                            "the trait bound `{}` is not satisfied",
                            predicate.display(db, target)
                        ),
                        site,
                    ),
                    ClauseKind::Projection(predicate)
                        if let AliasTermKind::ProjectionTy { .. } =
                            predicate.projection_term.kind =>
                    {
                        let alias = Ty::new_alias(
                            interner,
                            IsRigid::No,
                            predicate.projection_term.expect_ty(),
                        );
                        push_new(
                            self.found,
                            "E0271",
                            format_args!(
                                "type mismatch resolving `{} == {}`",
                                alias.display(db, target),
                                predicate.term.display(db, target)
                            ),
                            site,
                        );
                    }
                    _ => push_new(
                        self.found,
                        "E0271",
                        format_args!("type mismatch resolving a bound of the trait's item"),
                        site,
                    ),
                }
            }
            return;
        }
        // `Check::resolve` keeps quiet when a region must outlive one it is not known to (it
        // follows from another error of the item there); here it is the error.
        let region_fails = needs.iter().any(|&need| match need {
            Needs::Region(longer, shorter) => !known.region_outlives(longer, shorter),
            Needs::Type(..) => false,
        });
        if region_fails {
            push_new(self.found, "E0478", format_args!("lifetime bound not satisfied"), site);
            return;
        }
        let needs = mem::collect(
            self.frame,
            needs.into_iter().map(|need| {
                let need = match need {
                    Needs::Type(ty, region) => {
                        Needs::Type(known.opaques_as_captures(db, ty, region), region)
                    }
                    Needs::Region(..) => need,
                };
                (need, site)
            }),
        );
        self.resolve(known, needs);
    }
}

/// The `index`th `impl Trait` of a method's return type.
fn impl_trait_idx(index: usize) -> hir_ty::next_solver::ImplTraitIdx {
    hir_ty::next_solver::ImplTraitIdx::from_raw(la_arena::RawIdx::from_u32(index as u32))
}

/// `tys` normalized, those with an error left out: what an item assumes well-formed implies
/// as normalized, as rustc's implied bounds.
fn normalized<'f, 'db>(
    frame: &'f blaze_mem::frame::Frame,
    wfcx: &Wfcx<'f, 'db>,
    tys: impl Iterator<Item = Ty<'db>>,
) -> FVec<'f, Ty<'db>> {
    let mut dropped = Vec::new_in(frame);
    let mut out = Vec::new_in(frame);
    for ty in tys {
        dropped.clear();
        let ty = wfcx.normalize(ty, &mut dropped);
        if !ty.references_error() {
            out.push(ty);
        }
    }
    out
}

/// Replaces each projection of one of `items` (a trait's item, with the impl's trait
/// arguments) by the impl's type for it, as rustc's `param_env_with_gat_bounds` makes it
/// normalize: the impl's arguments, then the projection's own.
struct Items<'a, 'db> {
    interner: DbInterner<'db>,
    trait_args: GenericArgs<'db>,
    impl_args: GenericArgs<'db>,
    items: &'a [(AssocTyId, EarlyBinder<'db, Ty<'db>>)],
}

impl<'db> Items<'_, 'db> {
    /// The arguments of the impl's item for a projection of the trait's with `args`.
    fn impl_args_for(&self, args: GenericArgs<'db>) -> GenericArgs<'db> {
        GenericArgs::new_from_iter(
            self.impl_args.iter().chain(args.iter().skip(self.trait_args.len())),
        )
    }
}

impl<'db> TypeFolder<DbInterner<'db>> for Items<'_, 'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        if let TyKind::Alias(_, alias) = ty.kind()
            && let AliasTyKind::Projection { def_id: TraitAssocTyId(id) } = alias.kind
            && let Some(&(_, value)) = self.items.iter().find(|(it, _)| *it == id)
            && alias.args.as_slice().starts_with(self.trait_args.as_slice())
        {
            let args = self.impl_args_for(alias.args);
            return value.instantiate(self.interner, args).skip_norm_wip();
        }
        ty.super_fold_with(self)
    }
}

/// Finds the arguments of the first of `trait_fn`'s RPITITs in a type.
struct FindRpitit<'db, 'a> {
    trait_fn: FunctionId,
    found: &'a mut Option<GenericArgs<'db>>,
}

impl<'db> TypeVisitor<DbInterner<'db>> for FindRpitit<'db, '_> {
    type Result = std::ops::ControlFlow<()>;

    fn visit_ty(&mut self, ty: Ty<'db>) -> Self::Result {
        if let Some((rpitit, args)) = as_rpitit(ty)
            && rpitit.function == self.trait_fn
        {
            *self.found = Some(args);
            return std::ops::ControlFlow::Break(());
        }
        ty.super_visit_with(self)
    }
}
