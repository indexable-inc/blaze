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

extern crate ra_ap_rustc_type_ir as rustc_type_ir;

mod outlives;

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
    BoundRegionKind, BoundVar, ClauseKind, RegionKind, TypeVisitableExt as _,
    inherent::{IntoKind, Term as _},
};
use salsa::SalsaValue;

use hir_ty::{
    GenericPredicates, ImplTraitId, InferBodyId, InferenceResult,
    db::HirDatabase,
    next_solver::{
        BoundRegion, Clause, DbInterner, GenericArgKind, Region, SolverDefId, TermId, Ty, TyKind,
    },
};

pub use outlives::Inferred;
use outlives::{Known, Needs};

/// Where an error is: rustc's span of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SalsaValue)]
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

#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
pub struct WfDiagnostic {
    /// rustc's error code, as `E0310`.
    pub code: &'static str,
    pub message: String,
    pub site: Site,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, SalsaValue)]
pub struct WfResult {
    pub diagnostics: Vec<WfDiagnostic>,
}

/// The well-formedness errors of the item `def`: an ADT, an impl, a trait or a function.
#[salsa::tracked(returns(ref))]
pub fn wfcheck(db: &dyn HirDatabase, def: GenericDefId) -> WfResult {
    let mut result = WfResult::default();
    if matches!(std::env::var("RA_JIT_BORROWCK").as_deref(), Ok("off" | "0")) {
        return result;
    }
    let mut check = Check { db, inferred: Inferred::default(), found: &mut result.diagnostics };
    match def {
        GenericDefId::AdtId(adt) => check.adt(adt),
        GenericDefId::ImplId(impl_) => check.impl_(impl_),
        GenericDefId::TraitId(trait_) => check.trait_(trait_),
        GenericDefId::FunctionId(function) => check.function(function),
        _ => (),
    }
    result
}

struct Check<'a, 'db> {
    db: &'db dyn HirDatabase,
    inferred: Inferred<'db>,
    found: &'a mut Vec<WfDiagnostic>,
}

impl<'db> Check<'_, 'db> {
    fn interner(&self) -> DbInterner<'db> {
        DbInterner::new_no_crate(self.db)
    }

    fn clauses(&self, def: GenericDefId) -> Vec<Clause<'db>> {
        self.db.trait_environment(def).clauses().iter().collect()
    }

    /// Every field must be well-formed. What it needs of a lifetime parameter rustc infers,
    /// so only what it needs of `'static` can fail. Its where clauses must be well-formed
    /// given its bounds, those rustc infers included.
    fn adt(&mut self, adt: AdtId) {
        let mut known = Known::new(&self.clauses(adt.into()));
        let inferred = self
            .inferred
            .of(self.db, adt)
            .into_iter()
            .filter_map(|it| {
                let rustc_type_ir::OutlivesPredicate(arg, region) = it;
                match arg.kind() {
                    GenericArgKind::Type(ty) => Some(Needs::Type(ty, region)),
                    GenericArgKind::Lifetime(longer) => Some(Needs::Region(longer, region)),
                    GenericArgKind::Const(_) => None,
                }
            })
            .collect();
        known.add(inferred);
        let mut needs = Vec::new();
        for variant in outlives::variants(self.db, adt) {
            for (field, ty) in self.db.field_types(variant).iter() {
                let ty = ty.ty().instantiate_identity().skip_norm_wip();
                if ty.references_error() {
                    continue;
                }
                let mut found = Vec::new();
                outlives::wf(self.db, &mut self.inferred, ty, &mut found);
                needs.extend(
                    found
                        .into_iter()
                        .filter(|it| match *it {
                            Needs::Type(_, region) | Needs::Region(_, region) => {
                                matches!(region.kind(), RegionKind::ReStatic)
                            }
                        })
                        .map(|it| (it, Site::Field(variant, field))),
                );
                let mut found = Vec::new();
                outlives::projection_needs(self.db, adt.into(), ty, &mut found);
                needs.extend(found.into_iter().map(|it| (it, Site::Field(variant, field))));
            }
        }
        needs.extend(self.where_needs(adt.into()));
        self.resolve(&known, needs);
    }

    /// The self type and trait arguments of an impl, which it and its items assume
    /// well-formed. `None` when they have an error or an associated type: what normalizing
    /// them would imply is not known here.
    fn header(&self, impl_: ImplId) -> Option<Vec<Ty<'db>>> {
        let db = self.db;
        let self_ty = db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
        let mut header = vec![self_ty];
        if let Some(trait_ref) = db.impl_trait(impl_) {
            let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
            header.extend(trait_ref.args.iter().skip(1).filter_map(|arg| match arg.kind() {
                GenericArgKind::Type(ty) => Some(ty),
                _ => None,
            }));
        }
        if header.iter().any(|ty| ty.references_error() || ty.has_aliases()) {
            return None;
        }
        Some(header)
    }

    /// What an item knows: its where clauses and what the types it assumes well-formed imply.
    fn known(&mut self, def: GenericDefId, assumed: &[Ty<'db>]) -> Known<'db> {
        let mut known = Known::new(&self.clauses(def));
        let mut implied = Vec::new();
        for &ty in assumed {
            outlives::wf(self.db, &mut self.inferred, ty, &mut implied);
        }
        known.add(implied);
        known
    }

    fn impl_(&mut self, impl_: ImplId) {
        let db = self.db;
        let Some(header) = self.header(impl_) else { return };
        let known = self.known(impl_.into(), &header);
        let mut needs = self.where_needs(impl_.into());
        // The header is well-formed: the trait bounds that asks hold.
        for (index, &ty) in header.iter().enumerate() {
            let site = match index {
                0 => Site::SelfTy(impl_),
                _ => Site::ImplTrait(impl_),
            };
            let mut found = Vec::new();
            outlives::trait_needs(db, impl_.into(), ty, &mut found);
            needs.extend(found.into_iter().map(|it| (it, site)));
        }

        // The trait's outlives bounds hold of the impl's arguments.
        if let Some(trait_ref) = db.impl_trait(impl_) {
            let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
            let trait_clauses: Vec<Clause<'db>> =
                GenericPredicates::query_all(db, trait_ref.def_id.0.into())
                    .iter_instantiated(self.interner(), trait_ref.args.as_slice())
                    .map(|it| it.skip_norm_wip())
                    .collect();
            let mut found = Vec::new();
            outlives::clause_needs(&trait_clauses, &mut found);
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
            let mut found = Vec::new();
            outlives::wf(db, &mut self.inferred, ty, &mut found);
            let mut needs: Vec<_> =
                found.into_iter().map(|it| (it, Site::AssocType(alias))).collect();
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
        let mut errors = vec![Vec::new(); sig.inputs().len()];
        let inputs: Vec<Ty<'db>> = sig
            .inputs()
            .iter()
            .zip(&mut errors)
            .map(|(&ty, errors)| {
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
            })
            .collect();
        let mut assumed = inputs.clone();
        if assumed.iter().any(|ty| ty.references_error() || ty.has_aliases()) {
            return;
        }
        if let ItemContainerId::ImplId(impl_) = function.lookup(db).container {
            let Some(header) = self.header(impl_) else { return };
            assumed.extend(header);
        }
        // An `impl Trait` it returns has those lifetimes too.
        let regions = hir_ty::elided_lifetimes(db, function).regions(&errors);
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
        if !output.has_aliases() {
            assumed.push(output);
            let known = self.known(function.into(), &assumed);
            let mut needs = self.where_needs(function.into());
            // Its signature is well-formed: the trait bounds that asks hold.
            let sites = (0..inputs.len() as u32)
                .map(|it| Site::Param(function, it))
                .chain([Site::Output(function)]);
            for (&ty, site) in inputs.iter().chain([&output]).zip(sites) {
                let mut found = Vec::new();
                outlives::trait_needs(db, function.into(), ty, &mut found);
                needs.extend(found.into_iter().map(|it| (it, site)));
            }
            self.resolve(&known, needs);
        } else if !FunctionSignature::of(db, function).is_async() {
            let known = self.known(function.into(), &assumed);
            let elided = elided_output(db, function, &inputs);
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
        known: &Known<'db>,
        elided: Option<Region<'db>>,
    ) {
        let db = self.db;
        let interner = self.interner();
        let infer = InferenceResult::of(db, InferBodyId::from(function));
        let hidden: Vec<_> = infer.return_position_impl_trait_types(db).collect();
        let mut opaques = Vec::new();
        outlives::opaques(output, &mut opaques);
        for alias in opaques {
            let SolverDefId::InternedOpaqueTyId(opaque) = alias.kind.def_id() else { continue };
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
            let mut needs = Vec::new();
            let mut found = Vec::new();
            outlives::wf(db, &mut self.inferred, ty, &mut found);
            let bounds = rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id());
            for clause in bounds.iter_instantiated(interner, alias.args.as_slice()) {
                if let ClauseKind::TypeOutlives(rustc_type_ir::OutlivesPredicate(bounded, region)) =
                    clause.kind().skip_binder()
                    && let TyKind::Alias(bounded) = bounded.kind()
                    && bounded.kind.def_id() == alias.kind.def_id()
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
            needs.extend(found.into_iter().map(|it| (it, site)));
            self.resolve(known, needs);
        }
    }

    /// rustc's `check_associated_type_bounds` of the associated type an `impl Trait` a
    /// trait's method returns is (an `async fn`'s future too): the types its bounds name are
    /// well-formed given what the method knows, so `async fn foo(&self) -> &(T, U)` in
    /// `trait MyTrait<T, U>` needs `T` and `U` to outlive the lifetime of `&self` (E0311), as
    /// only `Self` is known to. An `impl Trait` with no outlives bound outlives as its
    /// arguments do: `&'static impl T` needs `Self: 'static` (E0310).
    fn trait_opaques(&mut self, function: FunctionId, output: Ty<'db>, known: &Known<'db>) {
        let db = self.db;
        let interner = self.interner();
        let mut opaques = Vec::new();
        outlives::opaques(output, &mut opaques);
        for alias in opaques {
            let SolverDefId::InternedOpaqueTyId(opaque) = alias.kind.def_id() else { continue };
            let ImplTraitId::ReturnTypeImplTrait(owner, index) = opaque.loc(db) else { continue };
            if owner != function {
                continue;
            }
            let mut found = Vec::new();
            let bounds = rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id());
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
            let needs = found
                .into_iter()
                .map(|it| match it {
                    Needs::Type(ty, region) => Needs::Type(outlives::outlived_as(db, ty), region),
                    Needs::Region(..) => it,
                })
                .map(|it| (it, site))
                .collect();
            self.resolve(known, needs);
        }
    }

    /// What the well-formedness of `def`'s own where clauses needs: of each type in them,
    /// and of a trait bound the trait's outlives bounds.
    fn where_needs(&mut self, def: GenericDefId) -> Vec<(Needs<'db>, Site)> {
        let db = self.db;
        let interner = self.interner();
        let clauses: Vec<Clause<'db>> = GenericPredicates::query_own_explicit(db, def)
            .iter_identity()
            .map(|it| it.skip_norm_wip())
            .collect();
        let mut needs = Vec::new();
        for clause in clauses {
            let mut found = Vec::new();
            let mut tys = Vec::new();
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
                    let trait_clauses: Vec<Clause<'db>> =
                        GenericPredicates::query_all(db, trait_ref.def_id.0.into())
                            .iter_instantiated(interner, trait_ref.args.as_slice())
                            .map(|it| it.skip_norm_wip())
                            .collect();
                    outlives::clause_needs(&trait_clauses, &mut found);
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
                ClauseKind::TypeOutlives(rustc_type_ir::OutlivesPredicate(ty, _)) => {
                    self_ty = Some(ty);
                    tys.push(ty);
                }
                _ => continue,
            }
            if tys.iter().any(|ty| ty.references_error()) {
                continue;
            }
            for ty in tys {
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
            needs.extend(found.into_iter().map(|it| (it, site)));
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
        let trait_name = trait_.map(|it| TraitSignature::of(db, it).name.clone());
        let param = match self_ty.map(|it| it.kind()) {
            Some(TyKind::Param(param)) => Some(param.id),
            _ => None,
        };
        let found = params.where_predicates().iter().position(|predicate| {
            let WherePredicate::TypeBound { target, bound, .. } = predicate else {
                return false;
            };
            let on_target = match (&store[*target], param) {
                (TypeRef::TypeParam(id), Some(param)) => *id == param,
                (TypeRef::TypeParam(_), None) | (_, Some(_)) => false,
                (_, None) => true,
            };
            let names = match (bound, &trait_name) {
                (TypeBound::Path(path, _) | TypeBound::ForLifetime(_, path), Some(name)) => {
                    store[*path].segments().last().is_some_and(|it| it.name == name)
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
    fn resolve(&mut self, known: &Known<'db>, needs: Vec<(Needs<'db>, Site)>) {
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
        let mut concrete = Vec::new();
        for &(need, site) in &needs {
            let Needs::Type(ty, region) = need else { continue };
            let fails = outlives::components(ty).into_iter().any(|it| {
                matches!(it, outlives::Component::Region(longer)
                    if !known.region_outlives(longer, region))
            });
            let message = "the type does not fulfill the required lifetime".to_owned();
            let diagnostic = WfDiagnostic { code: "E0477", message, site };
            if fails && !concrete.contains(&diagnostic) {
                concrete.push(diagnostic);
            }
        }
        if !concrete.is_empty() {
            for diagnostic in concrete {
                if !self.found.contains(&diagnostic) {
                    self.found.push(diagnostic);
                }
            }
            return;
        }
        for (need, site) in needs {
            let Needs::Type(ty, region) = need else { continue };
            let code = match region.kind() {
                RegionKind::ReStatic => "E0310",
                RegionKind::ReLateParam(late)
                    if !matches!(late.bound_region.kind, BoundRegionKind::Named(_)) =>
                {
                    "E0311"
                }
                _ => "E0309",
            };
            for _ in known.failing_aliases(self.db, ty, region) {
                let message = "the associated type may not live long enough".to_owned();
                let diagnostic = WfDiagnostic { code, message, site };
                if !self.found.contains(&diagnostic) {
                    self.found.push(diagnostic);
                }
            }
            for param in known.failing_params(ty, region) {
                let TyKind::Param(param) = param.kind() else { continue };
                let params = GenericParams::of(self.db, param.id.parent());
                let name = params[param.id.local_id()].name().map_or("_", |it| it.as_str());
                let message = format!("the parameter type `{name}` may not live long enough");
                let diagnostic = WfDiagnostic { code, message, site };
                if !self.found.contains(&diagnostic) {
                    self.found.push(diagnostic);
                }
            }
        }
    }
}

/// The lifetime elision gives a lifetime the return type elides: `&self`'s, else the one
/// lifetime of the arguments.
fn elided_output<'db>(
    db: &'db dyn HirDatabase,
    function: FunctionId,
    inputs: &[Ty<'db>],
) -> Option<Region<'db>> {
    if FunctionSignature::of(db, function).has_self_param()
        && let Some(TyKind::Ref(region, ..)) = inputs.first().map(|it| it.kind())
    {
        return Some(region);
    }
    let mut regions = Vec::new();
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
