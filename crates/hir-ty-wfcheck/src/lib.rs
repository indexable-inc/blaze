//! Item well-formedness: the part of rustc's `wfcheck` (`rustc_hir_analysis::check::wfcheck`)
//! about what outlives what, which reports E0309, E0310 and E0311 outside function bodies.
//!
//! An ADT's fields must be well-formed given its where clauses and what rustc infers from
//! the fields themselves (RFC 2093), which is every `T: 'a` and never `T: 'static`. A trait
//! impl's associated types must be well-formed given the impl's where clauses and what its
//! header's types imply, and the impl must satisfy the outlives bounds of the trait, as
//! `trait Foo: 'static`. The where clauses of an ADT, impl, trait or function must be
//! well-formed given its bounds and what the types it assumes well-formed imply (a
//! function's signature, an impl's header). Each item is a query of its own, as the borrow checker's bodies are;
//! `docs/borrowck.md` in the repository describes what is checked.

#![forbid(unsafe_code)]
#![feature(allocator_api, allocator_ext)]

extern crate rustc_type_ir;

mod compare;
#[doc(hidden)]
pub mod mem;
mod outlives;
mod persist;

#[cfg(test)]
mod budget;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::sync::LazyLock;

use crate::mem::FVec;
use base_db::Db;
use blaze_mem::frame::Frame;
use hir_def::{
    AdtId, AssocItemId, CallableDefId, FunctionId, GenericDefId, ImplId, ItemContainerId,
    LocalFieldId, Lookup as _, TraitId, TypeAliasId, VariantId,
    expr_store::ExpressionStore,
    hir::{
        generics::{GenericParams, WherePredicate},
        type_ref::{TypeBound, TypeRef},
    },
    signatures::{FunctionSignature, TraitSignature},
};
use rustc_type_ir::{
    AliasTyKind, BoundRegionKind, BoundVar, ClauseKind, RegionKind, TypeVisitableExt as _,
    inherent::{IntoKind, Term as _},
};

use hir_ty::{
    GenericPredicates, ImplTraitId, InferBodyId, InferenceResult,
    db::HirDatabase,
    next_solver::{
        BoundRegion, Clause, DbInterner, GenericArgKind, OpaqueTyIdWrapper, Region, TermId, Ty,
        TyKind,
    },
};

pub use compare::{
    ProcMacroSignatureMismatch, SignatureMismatch, SignatureMismatchAt,
    compare_impl_method_signature, compare_proc_macro_signature, trait_signature_in_impl,
};
pub use outlives::Inferred;
pub use persist::{WfDiagnosticRef, WfResultRef, WfRow};
use outlives::{Known, Needs, Wfcx};

/// Where an error is: rustc's span of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub enum Site {
    /// The type of a field.
    Field(VariantId, LocalFieldId),
    /// The type an impl gives an associated type.
    AssocType(TypeAliasId),
    /// The trait an impl implements.
    ImplTrait(ImplId),
    /// The bound of a where clause of an item, by its index among the item's where
    /// predicates (a bound written on a generic parameter is one too).
    Bound(GenericDefId, u32),
    /// An `impl Trait` a function returns, by its index among those of its return type.
    Opaque(FunctionId, u32),
    /// The type of a function's parameter, by its index, `self` included.
    Param(FunctionId, u32),
    /// The type a function returns.
    Output(FunctionId),
    /// The type an impl is for.
    SelfTy(ImplId),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct WfDiagnostic {
    /// rustc's error code, as `E0310`.
    pub code: &'static str,
    pub message: String,
    pub site: Site,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, blaze_query::Value)]
pub struct WfResult {
    pub diagnostics: Vec<WfDiagnostic>,
}

/// The well-formedness errors of the item `def`: an ADT, an impl, a trait or a function.
#[blaze_query::tracked(returns(ref))]
pub fn wfcheck(db: &Db, def: GenericDefId) -> WfResult {
    check_item(db, def)
}

/// [`wfcheck`] of `def`, not cached: what it reads is asked through the database as before.
fn check_item(db: &Db, def: GenericDefId) -> WfResult {
    if off() {
        return WfResult::default();
    }
    blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
        let mut found = Vec::new_in(frame);
        let mut check = Check { db, frame, inferred: Inferred::default(), found: &mut found };
        match def {
            GenericDefId::AdtId(adt) => check.adt(adt),
            GenericDefId::ImplId(impl_) => check.impl_(impl_),
            GenericDefId::TraitId(trait_) => check.trait_(trait_),
            GenericDefId::FunctionId(function) => check.function(function),
            _ => (),
        }
        WfResult { diagnostics: mem::owned(found.drain(..)) }
    })
}

/// `BLAZE_BORROWCK=off`, read once for the process as the borrow checker reads it, so a
/// result does not depend on when its query ran.
fn off() -> bool {
    static OFF: LazyLock<bool> =
        LazyLock::new(|| matches!(std::env::var("BLAZE_BORROWCK").as_deref(), Ok("off" | "0")));
    *OFF
}

struct Check<'a, 'f, 'db> {
    db: &'db Db,
    frame: &'f Frame,
    inferred: Inferred<'db>,
    found: &'a mut FVec<'f, WfDiagnostic>,
}

impl<'f, 'db> Check<'_, 'f, 'db> {
    fn interner(&self) -> DbInterner<'db> {
        DbInterner::new_no_crate(self.db)
    }

    fn clauses(&self, def: GenericDefId) -> &'db [Clause<'db>] {
        self.db.trait_environment(def).clauses().as_slice()
    }

    /// Every field must be well-formed. What it needs of a lifetime parameter rustc infers,
    /// so only what it needs of `'static` can fail. Its where clauses must be well-formed
    /// given its bounds, those rustc infers included.
    fn adt(&mut self, adt: AdtId) {
        let mut known = Known::new(self.frame, self.clauses(adt.into()));
        known.add(self.inferred.of(self.db, adt).filter_map(|it| {
            let rustc_type_ir::OutlivesClause(arg, region) = it;
            match arg.kind() {
                GenericArgKind::Type(ty) => Some(Needs::Type(ty, region)),
                GenericArgKind::Lifetime(longer) => Some(Needs::Region(longer, region)),
                GenericArgKind::Const(_) => None,
            }
        }));
        let mut needs = Vec::new_in(self.frame);
        // One inference context for the whole item, as rustc's `enter_wf_checking_ctxt`, and
        // one buffer for what each field needs.
        let mut wfcx = None;
        let mut found = Vec::new_in(self.frame);
        for variant in adt.variants(self.db) {
            for (field, ty) in self.db.field_types(variant).iter() {
                let ty = ty.ty().instantiate_identity().skip_norm_wip();
                if ty.references_error() {
                    continue;
                }
                let site = Site::Field(variant, field);
                found.clear();
                outlives::wf(self.db, &mut self.inferred, ty, &mut found);
                needs.extend(
                    found
                        .drain(..)
                        .filter(|it| match *it {
                            Needs::Type(_, region) | Needs::Region(_, region) => {
                                matches!(region.kind(), RegionKind::ReStatic)
                            }
                        })
                        .map(|it| (it, site)),
                );
                if !ty.has_aliases() {
                    continue;
                }
                let wfcx = wfcx.get_or_insert_with(|| Wfcx::new(self.frame, self.db, adt.into()));
                outlives::projection_needs(self.db, wfcx, ty, &mut found);
                needs.extend(found.drain(..).map(|it| (it, site)));
            }
        }
        needs.extend(self.where_needs(adt.into()));
        self.resolve(&known, needs);
    }

    /// The self type and trait arguments of an impl, which it and its items assume
    /// well-formed. `None` when they have an error.
    fn header(&self, impl_: ImplId) -> Option<FVec<'f, Ty<'db>>> {
        let db = self.db;
        let self_ty = db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
        let mut header = Vec::new_in(self.frame);
        header.push(self_ty);
        if let Some(trait_ref) = db.impl_trait(impl_) {
            let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
            header.extend(trait_ref.args.iter().skip(1).filter_map(|arg| match arg.kind() {
                GenericArgKind::Type(ty) => Some(ty),
                _ => None,
            }));
        }
        if header.iter().any(|ty| ty.references_error()) {
            return None;
        }
        Some(header)
    }

    /// The impl's header, when it has no associated type: what normalizing it would imply
    /// is not reasoned about for the impl itself and its associated types.
    fn plain_header(&self, impl_: ImplId) -> Option<FVec<'f, Ty<'db>>> {
        self.header(impl_).filter(|header| !header.iter().any(|ty| ty.has_aliases()))
    }

    /// What an item knows: its where clauses and what the types it assumes well-formed imply.
    fn known(&mut self, def: GenericDefId, assumed: &[Ty<'db>]) -> Known<'f, 'db> {
        let mut known = Known::new(self.frame, self.clauses(def));
        let mut implied = Vec::new_in(self.frame);
        for &ty in assumed {
            outlives::wf(self.db, &mut self.inferred, ty, &mut implied);
        }
        known.add(implied);
        known
    }

    fn impl_(&mut self, impl_: ImplId) {
        let db = self.db;
        let Some(header) = self.plain_header(impl_) else { return };
        let known = self.known(impl_.into(), &header);
        let mut needs = self.where_needs(impl_.into());
        // The header is well-formed: the trait bounds that asks hold.
        let wfcx = Wfcx::new(self.frame, db, impl_.into());
        let mut found = Vec::new_in(self.frame);
        for (index, &ty) in header.iter().enumerate() {
            let site = match index {
                0 => Site::SelfTy(impl_),
                _ => Site::ImplTrait(impl_),
            };
            outlives::trait_needs(db, &wfcx, ty, &mut found);
            needs.extend(found.drain(..).map(|it| (it, site)));
        }

        // The trait's outlives bounds hold of the impl's arguments.
        if let Some(trait_ref) = db.impl_trait(impl_) {
            let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
            let mut found = Vec::new_in(self.frame);
            outlives::predicate_needs(
                db,
                trait_ref.def_id.0.into(),
                trait_ref.args.as_slice(),
                &mut found,
            );
            found.retain(|it| match *it {
                Needs::Type(ty, region) => {
                    !ty.has_escaping_bound_vars() && !region.has_escaping_bound_vars()
                }
                Needs::Region(longer, shorter) => {
                    !longer.has_escaping_bound_vars() && !shorter.has_escaping_bound_vars()
                }
            });
            needs.extend(found.into_iter().map(|it| (it, Site::ImplTrait(impl_))));
        }
        self.resolve(&known, needs);

        // Each associated type is well-formed, given what the impl knows and, for a generic
        // one, its own where clauses (not the trait's).
        for &(_, item) in impl_.impl_items(db).items.iter() {
            let AssocItemId::TypeAliasId(alias) = item else { continue };
            let ty = db.ty(alias.into()).instantiate_identity().skip_norm_wip();
            if ty.references_error() {
                continue;
            }
            let known = self.known(alias.into(), &header);
            let mut found = Vec::new_in(self.frame);
            outlives::wf(db, &mut self.inferred, ty, &mut found);
            let mut needs =
                mem::collect(self.frame, found.into_iter().map(|it| (it, Site::AssocType(alias))));
            needs.extend(self.where_needs(alias.into()));
            self.resolve(&known, needs);
        }
    }

    fn trait_(&mut self, trait_: TraitId) {
        let known = self.known(trait_.into(), &[]);
        let needs = self.where_needs(trait_.into());
        self.resolve(&known, needs);
    }

    /// A function's where clauses must be well-formed given its bounds and what the types
    /// of its signature, and of its impl's header, imply.
    fn function(&mut self, function: FunctionId) {
        let db = self.db;
        let sig = self.interner().liberate_late_bound_regions(
            function.into(),
            db.callable_item_signature(CallableDefId::FunctionId(function))
                .instantiate_identity()
                .skip_norm_wip(),
        );
        // rust-analyzer gives a lifetime an argument elides an error region, where rustc makes
        // each a lifetime parameter of its own.
        let interner = self.interner();
        let mut anonymous = 0;
        let mut errors = mem::collect(
            self.frame,
            std::iter::repeat_n(Vec::new_in(self.frame), sig.inputs().len()),
        );
        let inputs = mem::collect(
            self.frame,
            sig.inputs().iter().zip(&mut errors).map(|(&ty, errors)| {
                rustc_type_ir::fold_regions(interner, ty, |region, _| {
                    if !region.is_error() {
                        return region;
                    }
                    anonymous += 1;
                    let var = BoundVar::from_u32(u32::MAX / 2 + anonymous);
                    let bound_region = BoundRegion { var, kind: BoundRegionKind::Anon };
                    let region = Region::new_late_param(interner, function.into(), bound_region);
                    errors.push(region);
                    region
                })
            }),
        );
        if inputs.iter().any(|ty| ty.references_error()) {
            return;
        }
        let wfcx = Wfcx::new(self.frame, db, function.into());
        // What it assumes well-formed is normalized for what that implies apart from the
        // normalizing of its signature for its well-formedness, as rustc's implied bounds.
        let mut dropped = Vec::new_in(self.frame);
        let mut assumed = mem::collect(
            self.frame,
            inputs.iter().map(|&ty| {
                dropped.clear();
                wfcx.normalize(ty, &mut dropped)
            }),
        );
        if let ItemContainerId::ImplId(impl_) = function.lookup(db).container {
            let Some(header) = self.header(impl_) else { return };
            assumed.extend(header.into_iter().map(|ty| {
                dropped.clear();
                wfcx.normalize(ty, &mut dropped)
            }));
        }
        assumed.retain(|ty| !ty.references_error());
        // An `impl Trait` it returns has those lifetimes too.
        let mut regions = Vec::new_in(self.frame);
        hir_ty::elided_lifetimes(db, function).regions_into(&errors, &mut regions);
        let output = hir_ty::with_elided_regions(db, sig.output(), function, &regions);
        if output.references_error() {
            return;
        }
        if let ItemContainerId::TraitId(_) = function.lookup(db).container
            && output.has_opaque_types()
        {
            // An `impl Trait` a trait's method returns is an associated type of the trait,
            // whose bounds must be well-formed given only what the method's parameters imply:
            // the return type, its projection, implies nothing.
            let known = self.known(function.into(), &assumed);
            self.trait_opaques(function, output, &known);
        }
        if !output.has_opaque_types() {
            assumed.extend(
                Some({
                    dropped.clear();
                    wfcx.normalize(output, &mut dropped)
                })
                .filter(|ty| !ty.references_error()),
            );
            let known = self.known(function.into(), &assumed);
            let mut needs = self.where_needs(function.into());
            // Its signature, normalized, is well-formed: what that needs of what outlives
            // what holds, and so do the trait bounds it asks. Without an associated type
            // what a type needs is what it implies.
            let sites = (0..inputs.len() as u32)
                .map(|it| Site::Param(function, it))
                .chain([Site::Output(function)]);
            let mut found = Vec::new_in(self.frame);
            for (&ty, site) in inputs.iter().chain([&output]).zip(sites) {
                found.clear();
                let normalized = wfcx.normalize(ty, &mut found);
                if !normalized.references_error() {
                    if ty.has_aliases() {
                        outlives::wf(db, &mut self.inferred, normalized, &mut found);
                    }
                    outlives::trait_needs(db, &wfcx, normalized, &mut found);
                }
                needs.extend(found.drain(..).map(|it| (it, site)));
            }
            self.resolve(&known, needs);
        } else if !FunctionSignature::of(db, function).is_async() {
            let known = self.known(function.into(), &assumed);
            let elided = elided_output(self.frame, db, function, &inputs);
            self.opaques(function, output, &known, elided);
        }
    }

    /// rustc's `check_opaque_meets_bounds`: the type each `impl Trait` the function returns
    /// hides is well-formed and outlives what the `impl Trait`'s bounds say it does, given
    /// what the function knows; `-> impl Sized + 'static` hiding a `T` needs `T: 'static`.
    /// rust-analyzer gives a lifetime the return type elides an error region; `elided` is
    /// the one lifetime elision makes it.
    fn opaques(
        &mut self,
        function: FunctionId,
        output: Ty<'db>,
        known: &Known<'f, 'db>,
        elided: Option<Region<'db>>,
    ) {
        let db = self.db;
        let interner = self.interner();
        let infer = InferenceResult::of(db, InferBodyId::from(function));
        let hidden = mem::collect(self.frame, infer.return_position_impl_trait_types(db));
        let mut opaques = Vec::new_in(self.frame);
        outlives::opaques(output, &mut opaques);
        let mut found = Vec::new_in(self.frame);
        for alias in opaques {
            let AliasTyKind::Opaque { def_id: OpaqueTyIdWrapper(opaque) } = alias.kind else {
                continue;
            };
            let ImplTraitId::ReturnTypeImplTrait(owner, index) = opaque.loc(db) else { continue };
            if owner != function {
                continue;
            }
            let Some(&(_, ty)) = hidden.iter().find(|(it, _)| *it == index) else { continue };
            if ty.references_error() || ty.has_non_region_infer() {
                continue;
            }
            let ty = outlives::without_body_regions(interner, outlives::outlived_as(db, ty));
            let site = Site::Opaque(function, index.into_raw().into_u32());
            found.clear();
            outlives::wf(db, &mut self.inferred, ty, &mut found);
            let bounds = rustc_type_ir::Interner::item_bounds(interner, opaque.into());
            for clause in bounds.iter_instantiated(interner, alias.args.as_slice()) {
                if let ClauseKind::TypeOutlives(rustc_type_ir::OutlivesClause(bounded, region)) =
                    clause.kind().skip_binder()
                    && let TyKind::Alias(_, bounded) = bounded.kind()
                    && bounded.kind == alias.kind
                    && !region.has_escaping_bound_vars()
                {
                    let region = match region.kind() {
                        RegionKind::ReError(_) => match elided {
                            Some(elided) => elided,
                            None => continue,
                        },
                        _ => region,
                    };
                    found.push(Needs::Type(ty, region));
                }
            }
            let needs = mem::collect(self.frame, found.drain(..).map(|it| (it, site)));
            self.resolve(known, needs);
        }
    }

    /// rustc's `check_associated_type_bounds` of the associated type an `impl Trait` a
    /// trait's method returns is (an `async fn`'s future too): the types its bounds name are
    /// well-formed given what the method knows, so `async fn foo(&self) -> &(T, U)` in
    /// `trait MyTrait<T, U>` needs `T` and `U` to outlive the lifetime of `&self` (E0311), as
    /// only `Self` is known to. An `impl Trait` with no outlives bound outlives as its
    /// arguments do: `&'static impl T` needs `Self: 'static` (E0310).
    fn trait_opaques(&mut self, function: FunctionId, output: Ty<'db>, known: &Known<'f, 'db>) {
        let db = self.db;
        let interner = self.interner();
        let mut opaques = Vec::new_in(self.frame);
        outlives::opaques(output, &mut opaques);
        let mut found = Vec::new_in(self.frame);
        for alias in opaques {
            let AliasTyKind::Opaque { def_id: OpaqueTyIdWrapper(opaque) } = alias.kind else {
                continue;
            };
            let ImplTraitId::ReturnTypeImplTrait(owner, index) = opaque.loc(db) else { continue };
            if owner != function {
                continue;
            }
            found.clear();
            let bounds = rustc_type_ir::Interner::item_bounds(interner, opaque.into());
            for clause in bounds.iter_instantiated(interner, alias.args.as_slice()) {
                let (args, term) = match clause.kind().skip_binder() {
                    ClauseKind::Trait(predicate) => (predicate.trait_ref.args, None),
                    ClauseKind::Projection(predicate) => {
                        (predicate.projection_term.args, predicate.term.as_type())
                    }
                    _ => continue,
                };
                let tys = args
                    .iter()
                    .skip(1)
                    .filter_map(|arg| match arg.kind() {
                        GenericArgKind::Type(ty) => Some(ty),
                        _ => None,
                    })
                    .chain(term);
                for ty in tys {
                    if !ty.references_error() {
                        outlives::wf(db, &mut self.inferred, ty, &mut found);
                    }
                }
            }
            let site = Site::Opaque(function, index.into_raw().into_u32());
            let needs = mem::collect(
                self.frame,
                found
                    .drain(..)
                    .map(|it| match it {
                        Needs::Type(ty, region) => {
                            Needs::Type(outlives::outlived_as(db, ty), region)
                        }
                        Needs::Region(..) => it,
                    })
                    .map(|it| (it, site)),
            );
            self.resolve(known, needs);
        }
    }

    /// What the well-formedness of `def`'s own where clauses needs: of each type in them,
    /// and of a trait bound the trait's outlives bounds.
    fn where_needs(&mut self, def: GenericDefId) -> FVec<'f, (Needs<'db>, Site)> {
        let db = self.db;
        let clauses = GenericPredicates::query_own_explicit(db, def)
            .iter_identity()
            .map(|it| it.skip_norm_wip());
        let mut needs = Vec::new_in(self.frame);
        let mut found = Vec::new_in(self.frame);
        let mut tys = Vec::new_in(self.frame);
        for clause in clauses {
            found.clear();
            tys.clear();
            let mut self_ty = None;
            let mut trait_ = None;
            match clause.kind().skip_binder() {
                ClauseKind::Trait(predicate) => {
                    let trait_ref = predicate.trait_ref;
                    self_ty = Some(trait_ref.self_ty());
                    trait_ = Some(trait_ref.def_id.0);
                    tys.extend(trait_ref.args.iter().filter_map(|arg| match arg.kind() {
                        GenericArgKind::Type(ty) => Some(ty),
                        _ => None,
                    }));
                    outlives::predicate_needs(
                        db,
                        trait_ref.def_id.0.into(),
                        trait_ref.args.as_slice(),
                        &mut found,
                    );
                }
                ClauseKind::Projection(predicate) => {
                    let TermId::TypeAliasId(item) = predicate.def_id().0 else { continue };
                    let ItemContainerId::TraitId(it) = item.lookup(db).container else {
                        continue;
                    };
                    trait_ = Some(it);
                    self_ty = predicate.projection_term.args.iter().next().and_then(|it| match it
                        .kind()
                    {
                        GenericArgKind::Type(ty) => Some(ty),
                        _ => None,
                    });
                    tys.extend(predicate.projection_term.args.iter().filter_map(|arg| {
                        match arg.kind() {
                            GenericArgKind::Type(ty) => Some(ty),
                            _ => None,
                        }
                    }));
                    tys.extend(predicate.term.as_type());
                }
                ClauseKind::TypeOutlives(rustc_type_ir::OutlivesClause(ty, _)) => {
                    self_ty = Some(ty);
                    tys.push(ty);
                }
                _ => continue,
            }
            if tys.iter().any(|ty| ty.references_error()) {
                continue;
            }
            for &ty in &tys {
                outlives::wf(db, &mut self.inferred, ty, &mut found);
            }
            found.retain(|it| match *it {
                Needs::Type(ty, region) => {
                    !ty.has_escaping_bound_vars() && !region.has_escaping_bound_vars()
                }
                Needs::Region(longer, shorter) => {
                    !longer.has_escaping_bound_vars() && !shorter.has_escaping_bound_vars()
                }
            });
            if found.is_empty() {
                continue;
            }
            let Some(site) = self.bound_site(def, self_ty, trait_) else { continue };
            needs.extend(found.drain(..).map(|it| (it, site)));
        }
        needs
    }

    /// The where predicate of `def` a clause comes from: the one on `self_ty` naming the
    /// trait `trait_`, or with no trait an outlives bound on `self_ty`.
    fn bound_site(
        &self,
        def: GenericDefId,
        self_ty: Option<Ty<'db>>,
        trait_: Option<TraitId>,
    ) -> Option<Site> {
        let db = self.db;
        let params = GenericParams::of(db, def);
        let store = ExpressionStore::of(db, def.into());
        let trait_name = trait_.map(|it| TraitSignature::of(db, it).name.clone()); // clone: a `Name` is an interned `Symbol`, a reference count
        let param = match self_ty.map(|it| it.kind()) {
            Some(TyKind::Param(param)) => Some(param.id),
            _ => None,
        };
        let found = params.where_predicates().iter().position(|predicate| {
            let WherePredicate::TypeBound { target, bound, .. } = predicate else {
                return false;
            };
            let on_target = match (store.type_ref(*target), param) {
                (TypeRef::TypeParam(id), Some(param)) => id == param,
                (TypeRef::TypeParam(_), None) | (_, Some(_)) => false,
                (_, None) => true,
            };
            let names = match (bound, &trait_name) {
                (TypeBound::Path(path, _) | TypeBound::ForLifetime(_, path), Some(name)) => {
                    store.path(*path).segments().last().is_some_and(|it| it.name == name)
                }
                (TypeBound::Lifetime(_), None) => true,
                _ => false,
            };
            on_target && names
        })?;
        Some(Site::Bound(def, found as u32))
    }

    /// E0309, E0310 or E0311 (an anonymous lifetime) for each type parameter not known to
    /// outlive a region it must, unless a region must outlive one it is not known to: rustc's
    /// `process_errors` keeps only that error of one check, as the others follow from it.
    fn resolve(&mut self, known: &Known<'f, 'db>, needs: FVec<'f, (Needs<'db>, Site)>) {
        let region_fails = needs.iter().any(|&(need, _)| match need {
            Needs::Region(longer, shorter) => !known.region_outlives(longer, shorter),
            Needs::Type(..) => false,
        });
        if region_fails {
            return;
        }
        // A type that outlives a region only if a lifetime in it does, which it is not known
        // to: rustc's `ConcreteFailure` of a `RelateParamBound`, E0477, which also keeps the
        // errors of type parameters from being reported.
        let mut concrete: FVec<'_, Site> = Vec::new_in(self.frame);
        for &(need, site) in &needs {
            let Needs::Type(ty, region) = need else { continue };
            let fails = outlives::components(ty).into_iter().any(|it| {
                matches!(it, outlives::Component::Region(longer)
                    if !known.region_outlives(longer, region))
            });
            if !fails {
                continue;
            }
            if !concrete.contains(&site) {
                concrete.push(site);
            }
        }
        if !concrete.is_empty() {
            for &site in concrete.iter() {
                push_new(
                    self.found,
                    "E0477",
                    format_args!("the type does not fulfill the required lifetime"),
                    site,
                );
            }
            return;
        }
        for (need, site) in needs {
            let Needs::Type(ty, region) = need else { continue };
            let code = match region.kind() {
                RegionKind::ReStatic => "E0310",
                RegionKind::ReLateParam(late)
                    if !matches!(late.kind.kind, BoundRegionKind::Named(_)) =>
                {
                    "E0311"
                }
                _ => "E0309",
            };
            for _ in known.failing_aliases(self.db, ty, region) {
                push_new(
                    self.found,
                    code,
                    format_args!("the associated type may not live long enough"),
                    site,
                );
            }
            for param in known.failing_params(ty, region) {
                let TyKind::Param(param) = param.kind() else { continue };
                let params = GenericParams::of(self.db, param.id.parent());
                let name = params[param.id.local_id()].name().map_or("_", |it| it.as_str());
                push_new(
                    self.found,
                    code,
                    format_args!("the parameter type `{name}` may not live long enough"),
                    site,
                );
            }
        }
    }
}

/// Adds the diagnostic unless `found` has it, making its message only then.
fn push_new(
    found: &mut FVec<'_, WfDiagnostic>,
    code: &'static str,
    message: std::fmt::Arguments<'_>,
    site: Site,
) {
    if found
        .iter()
        .any(|it| it.code == code && it.site == site && formats_to(&it.message, message))
    {
        return;
    }
    // alloc: the result's own text, once per diagnostic reported
    found.push(WfDiagnostic { code, message: message.to_string(), site });
}

/// Whether `message` formats to `text`, compared piece by piece as it formats.
fn formats_to(text: &str, message: std::fmt::Arguments<'_>) -> bool {
    struct Rest<'t>(&'t str);
    impl std::fmt::Write for Rest<'_> {
        fn write_str(&mut self, piece: &str) -> std::fmt::Result {
            self.0 = self.0.strip_prefix(piece).ok_or(std::fmt::Error)?;
            Ok(())
        }
    }
    let mut rest = Rest(text);
    std::fmt::write(&mut rest, message).is_ok() && rest.0.is_empty()
}

/// The lifetime elision gives a lifetime the return type elides: `&self`'s, else the one
/// lifetime of the arguments.
fn elided_output<'db>(
    frame: &Frame,
    db: &'db Db,
    function: FunctionId,
    inputs: &[Ty<'db>],
) -> Option<Region<'db>> {
    if FunctionSignature::of(db, function).has_self_param()
        && let Some(TyKind::Ref(region, ..)) = inputs.first().map(|it| it.kind())
    {
        return Some(region);
    }
    let mut regions: FVec<'_, Region<'db>> = Vec::new_in(frame);
    for &input in inputs {
        rustc_type_ir::fold_regions(DbInterner::new_no_crate(db), input, |region, _| {
            if !matches!(region.kind(), RegionKind::ReBound(..)) && !regions.contains(&region) {
                regions.push(region);
            }
            region
        });
    }
    match regions.as_slice() {
        &[only] => Some(only),
        _ => None,
    }
}
