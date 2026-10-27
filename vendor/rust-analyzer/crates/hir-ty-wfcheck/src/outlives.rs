//! What outlives what: what a type's well-formedness needs (rustc's `wf.rs`), what an ADT's
//! fields let rustc infer of its parameters (`rustc_hir_analysis::outlives`, RFC 2093) and
//! what an item knows (its where clauses and the types it may assume well-formed).

use hir_def::{AdtId, ItemContainerId, Lookup as _, VariantId};
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::{
    ClauseKind, EarlyBinder, ExistentialPredicate, OutlivesPredicate, RegionKind, TypeFoldable,
    TypeFolder, TypeSuperFoldable, TypeSuperVisitable, TypeVisitable, TypeVisitableExt as _,
    TypeVisitor,
    inherent::{GenericArgs as _, IntoKind, Term as _, Ty as _},
};

use hir_ty::{
    GenericPredicates,
    db::HirDatabase,
    next_solver::{
        AliasTy, Clause, DbInterner, GenericArg, GenericArgKind, GenericArgs, Region, SolverDefId,
        Ty, TyKind,
    },
};

/// A type that must outlive a region, or a region that must outlive another.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Needs<'db> {
    Type(Ty<'db>, Region<'db>),
    Region(Region<'db>, Region<'db>),
}

/// rustc's outlives components of a type: the regions and the type parameters and associated
/// types in it, those under a binder aside, all of which must outlive a region for it to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Component<'db> {
    Region(Region<'db>),
    Param(Ty<'db>),
    Alias(Ty<'db>),
}

pub(crate) fn components<'db>(ty: Ty<'db>) -> Vec<Component<'db>> {
    struct Find<'db>(Vec<Component<'db>>);
    impl<'db> TypeVisitor<DbInterner<'db>> for Find<'db> {
        type Result = ();
        fn visit_ty(&mut self, ty: Ty<'db>) {
            match ty.kind() {
                TyKind::Param(_) => self.0.push(Component::Param(ty)),
                TyKind::Alias(_) if !ty.has_escaping_bound_vars() => {
                    self.0.push(Component::Alias(ty))
                }
                TyKind::Bound(..) | TyKind::Placeholder(_) | TyKind::Infer(_) => (),
                _ => ty.super_visit_with(self),
            }
        }
        fn visit_region(&mut self, region: Region<'db>) {
            if !matches!(region.kind(), RegionKind::ReBound(..)) {
                self.0.push(Component::Region(region));
            }
        }
    }
    let mut find = Find(Vec::new());
    ty.visit_with(&mut find);
    find.0
}

/// The `impl Trait`s in `ty`, outermost first.
pub(crate) fn opaques<'db>(ty: Ty<'db>, out: &mut Vec<AliasTy<'db>>) {
    struct Find<'a, 'db>(&'a mut Vec<AliasTy<'db>>);
    impl<'db> TypeVisitor<DbInterner<'db>> for Find<'_, 'db> {
        type Result = ();
        fn visit_ty(&mut self, ty: Ty<'db>) {
            if let TyKind::Alias(alias) = ty.kind()
                && let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind
            {
                self.0.push(alias);
            }
            ty.super_visit_with(self)
        }
    }
    ty.visit_with(&mut Find(out));
}

/// `ty` with what outlives a region as it does in its place, as rustc's outlives components
/// see it: a closure or a coroutine its captures, an `impl Trait` with no outlives bound its
/// arguments.
pub(crate) fn outlived_as<'db>(db: &'db dyn HirDatabase, ty: Ty<'db>) -> Ty<'db> {
    struct Fold<'db> {
        db: &'db dyn HirDatabase,
        depth: usize,
    }
    impl<'db> TypeFolder<DbInterner<'db>> for Fold<'db> {
        fn cx(&self) -> DbInterner<'db> {
            DbInterner::new_no_crate(self.db)
        }
        fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
            if self.depth > 16 {
                return ty;
            }
            self.depth += 1;
            let interner = self.cx();
            let folded = match ty.kind() {
                TyKind::Closure(_, args) => args.as_closure().tupled_upvars_ty().fold_with(self),
                TyKind::Coroutine(_, args) => {
                    args.as_coroutine().tupled_upvars_ty().fold_with(self)
                }
                TyKind::CoroutineClosure(_, args) => {
                    args.as_coroutine_closure().tupled_upvars_ty().fold_with(self)
                }
                TyKind::Alias(alias)
                    if let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind
                        && !rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id())
                            .iter_instantiated(interner, alias.args.as_slice())
                            .any(|clause| {
                                matches!(
                                    clause.kind().skip_binder(),
                                    ClauseKind::TypeOutlives(OutlivesPredicate(bounded, _))
                                        if bounded == ty
                                )
                            }) =>
                {
                    Ty::new_tup_from_iter(interner, alias.args.types()).fold_with(self)
                }
                _ => ty.super_fold_with(self),
            };
            self.depth -= 1;
            folded
        }
    }
    ty.fold_with(&mut Fold { db, depth: 0 })
}

/// `ty` without the regions of a function body, which only the borrow checker reasons
/// about: a type with such a region of its own, as `&'r T`, is left out, what it needs
/// following from the borrow checker's `'r`, and any other such region is erased.
pub(crate) fn without_body_regions<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Ty<'db> {
    fn is_body(region: Region<'_>) -> bool {
        !matches!(
            region.kind(),
            RegionKind::ReEarlyParam(_)
                | RegionKind::ReLateParam(_)
                | RegionKind::ReBound(..)
                | RegionKind::ReStatic
        )
    }
    struct Fold<'db>(DbInterner<'db>);
    impl<'db> TypeFolder<DbInterner<'db>> for Fold<'db> {
        fn cx(&self) -> DbInterner<'db> {
            self.0
        }
        fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
            let own_body_region = match ty.kind() {
                TyKind::Ref(region, ..) => is_body(region),
                TyKind::Adt(_, args) => args.regions().any(is_body),
                TyKind::Dynamic(_, region) => is_body(region),
                _ => false,
            };
            if own_body_region { Ty::new_unit(self.0) } else { ty.super_fold_with(self) }
        }
        fn fold_region(&mut self, region: Region<'db>) -> Region<'db> {
            if is_body(region) { Region::new_erased(self.0) } else { region }
        }
    }
    ty.fold_with(&mut Fold(interner))
}

/// A region rustc calls free: a lifetime parameter of the item, which inference may name.
fn is_free(region: Region<'_>) -> bool {
    matches!(region.kind(), RegionKind::ReEarlyParam(_))
}

/// The outlives bounds of an ADT, with the arguments `args`: those it declares and those its
/// fields let rustc infer.
fn adt_needs<'db>(
    db: &'db dyn HirDatabase,
    inferred: &mut Inferred<'db>,
    adt: AdtId,
    args: GenericArgs<'db>,
    out: &mut Vec<Needs<'db>>,
) {
    let interner = DbInterner::new_no_crate(db);
    let clauses: Vec<Clause<'db>> = GenericPredicates::query_all(db, adt.into())
        .iter_instantiated(interner, args.as_slice())
        .map(|it| it.skip_norm_wip())
        .collect();
    clause_needs(&clauses, out);
    for predicate in inferred.of(db, adt) {
        let OutlivesPredicate(arg, region) =
            EarlyBinder::bind(predicate).instantiate(interner, args).skip_norm_wip();
        out.push(match arg.kind() {
            GenericArgKind::Type(ty) => Needs::Type(ty, region),
            GenericArgKind::Lifetime(longer) => Needs::Region(longer, region),
            GenericArgKind::Const(_) => continue,
        });
    }
}

/// The outlives bounds among `clauses`.
pub(crate) fn clause_needs<'db>(clauses: &[Clause<'db>], out: &mut Vec<Needs<'db>>) {
    for clause in clauses {
        match clause.kind().skip_binder() {
            ClauseKind::TypeOutlives(OutlivesPredicate(ty, region)) => {
                out.push(Needs::Type(ty, region))
            }
            ClauseKind::RegionOutlives(OutlivesPredicate(longer, shorter)) => {
                out.push(Needs::Region(longer, shorter))
            }
            _ => (),
        }
    }
}

/// What the well-formedness of `ty` needs of what outlives what: rustc's `wf::obligations`,
/// its outlives obligations. What names a region bound inside `ty` is left out, as rustc
/// leaves it out.
pub(crate) fn wf<'db>(
    db: &'db dyn HirDatabase,
    inferred: &mut Inferred<'db>,
    ty: Ty<'db>,
    out: &mut Vec<Needs<'db>>,
) {
    let mut found = Vec::new();
    wf_into(db, inferred, ty, &mut found, 0);
    out.extend(found.into_iter().filter(|it| match *it {
        Needs::Type(ty, region) => {
            !ty.has_escaping_bound_vars() && !region.has_escaping_bound_vars()
        }
        Needs::Region(longer, shorter) => {
            !longer.has_escaping_bound_vars() && !shorter.has_escaping_bound_vars()
        }
    }));
}

fn wf_into<'db>(
    db: &'db dyn HirDatabase,
    inferred: &mut Inferred<'db>,
    ty: Ty<'db>,
    out: &mut Vec<Needs<'db>>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    let args = |inferred: &mut Inferred<'db>, args: GenericArgs<'db>, out: &mut Vec<Needs<'db>>| {
        for arg in args.iter() {
            if let GenericArgKind::Type(ty) = arg.kind() {
                wf_into(db, inferred, ty, out, depth + 1);
            }
        }
    };
    match ty.kind() {
        TyKind::Ref(region, inner, _) => {
            out.push(Needs::Type(inner, region));
            wf_into(db, inferred, inner, out, depth + 1);
        }
        TyKind::Adt(def, adt_args) => {
            adt_needs(db, inferred, def.def_id(), adt_args, out);
            args(inferred, adt_args, out);
        }
        TyKind::FnPtr(sig, _) => {
            for it in sig.skip_binder().inputs_and_output.iter() {
                wf_into(db, inferred, it, out, depth + 1);
            }
        }
        TyKind::Dynamic(predicates, _) => {
            for predicate in predicates.iter() {
                match predicate.skip_binder() {
                    ExistentialPredicate::Trait(it) => args(inferred, it.args, out),
                    ExistentialPredicate::Projection(it) => {
                        args(inferred, it.args, out);
                        if let Some(term) = it.term.as_type() {
                            wf_into(db, inferred, term, out, depth + 1);
                        }
                    }
                    ExistentialPredicate::AutoTrait(_) => (),
                }
            }
        }
        TyKind::Tuple(tys) => {
            for it in tys.iter() {
                wf_into(db, inferred, it, out, depth + 1);
            }
        }
        TyKind::Array(inner, _) | TyKind::Slice(inner) | TyKind::RawPtr(inner, _) => {
            wf_into(db, inferred, inner, out, depth + 1)
        }
        TyKind::Alias(alias) => args(inferred, alias.args, out),
        _ => (),
    }
}

/// What the impls of the associated types in `ty` need of what outlives what, given the where
/// clauses of `def`: rustc's well-formedness asks that the trait of each be implemented, and
/// its inference of an ADT's outlives bounds (RFC 2093) does not infer these, so a lifetime
/// parameter can fail them too. `<T as Dummy<'a>>::Out` with only
/// `impl<'a, T: 'a> Dummy<'a> for T` needs `T: 'a`.
pub(crate) fn projection_needs<'db>(
    db: &'db dyn HirDatabase,
    def: hir_def::GenericDefId,
    ty: Ty<'db>,
    out: &mut Vec<Needs<'db>>,
) {
    let interner = DbInterner::new_no_crate(db);
    let mut aliases = Vec::new();
    walk(ty, &mut aliases);
    let predicates = aliases
        .into_iter()
        .filter_map(|it| match it.kind() {
            TyKind::Alias(alias)
                if matches!(alias.kind, rustc_type_ir::AliasTyKind::Projection { .. })
                    && !it.has_escaping_bound_vars()
                    && !it.references_error() =>
            {
                Some(trait_predicate(interner, alias))
            }
            _ => None,
        })
        .collect();
    prove_needs(db, def, predicates, out);
}

/// What proving the trait bounds the well-formedness of `ty` asks needs of what outlives what,
/// given the where clauses of `def`: of each associated type in it, that its trait is
/// implemented, and of each struct or enum, its where clauses. `ServiceChainBuilder<
/// ServiceChain<P, S>, NS>` of `struct ServiceChainBuilder<P: Service, ..>` with only
/// `impl<P: Service, S: Service> Service for ServiceChain<P, S> where P::Error: 'static`
/// needs `P::Error: 'static`.
pub(crate) fn trait_needs<'db>(
    db: &'db dyn HirDatabase,
    def: hir_def::GenericDefId,
    ty: Ty<'db>,
    out: &mut Vec<Needs<'db>>,
) {
    use rustc_type_ir::inherent::Clause as _;
    let interner = DbInterner::new_no_crate(db);
    let mut tys = Vec::new();
    walk(ty, &mut tys);
    let mut predicates = Vec::new();
    for it in tys {
        if it.has_escaping_bound_vars() || it.references_error() {
            continue;
        }
        match it.kind() {
            TyKind::Alias(alias)
                if matches!(alias.kind, rustc_type_ir::AliasTyKind::Projection { .. }) =>
            {
                predicates.push(trait_predicate(interner, alias));
            }
            TyKind::Adt(adt, args) => {
                predicates.extend(
                    GenericPredicates::query_all(db, adt.def_id().into())
                        .iter_instantiated(interner, args.as_slice())
                        .map(|it| it.skip_norm_wip())
                        .filter(|it| {
                            matches!(
                                it.kind().skip_binder(),
                                ClauseKind::Trait(_) | ClauseKind::Projection(_)
                            ) && !it.references_error()
                        })
                        .map(|it| it.as_predicate()),
                );
            }
            _ => (),
        }
    }
    prove_needs(db, def, predicates, out);
}

fn trait_predicate<'db>(
    interner: DbInterner<'db>,
    alias: AliasTy<'db>,
) -> hir_ty::next_solver::Predicate<'db> {
    use rustc_type_ir::Upcast as _;
    rustc_type_ir::TraitPredicate {
        trait_ref: alias.trait_ref(interner),
        polarity: rustc_type_ir::PredicatePolarity::Positive,
    }
    .upcast(interner)
}

/// What proving `predicates` with the trait solver needs of what outlives what, given the
/// where clauses of `def`: the where clauses of the impls that prove them. Nothing when one
/// is not proven.
fn prove_needs<'db>(
    db: &'db dyn HirDatabase,
    def: hir_def::GenericDefId,
    predicates: Vec<hir_ty::next_solver::Predicate<'db>>,
    out: &mut Vec<Needs<'db>>,
) {
    use hir_def::HasModule as _;
    use hir_ty::next_solver::{
        TypingMode,
        infer::{
            DbInternerInferExt as _,
            region_constraints::Constraint,
            traits::{Obligation, ObligationCause},
        },
        obligation_ctxt::ObligationCtxt,
    };
    if predicates.is_empty() {
        return;
    }
    let interner = DbInterner::new_with(db, def.module(db).krate(db)).with_regions();
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let env = db.trait_environment(def);
    for predicate in predicates {
        let mut ocx = ObligationCtxt::new(&infcx);
        ocx.register_obligation(Obligation::new(
            interner,
            ObligationCause::dummy(),
            env,
            predicate,
        ));
        if !ocx.evaluate_obligations_error_on_ambiguity().is_empty() {
            return;
        }
    }
    let mut inner = infcx.inner.borrow_mut();
    let obligations: Vec<_> =
        inner.region_obligations().iter().map(|it| (it.sup_type, it.sub_region)).collect();
    let constraints =
        inner.unwrap_region_constraints().region_constraint_data().constraints.clone();
    drop(inner);
    // The impl's lifetimes are inference variables the solver equates with the item's.
    let mut equal: FxHashMap<rustc_type_ir::RegionVid, Region<'db>> = FxHashMap::default();
    for constraint in &constraints {
        if let Constraint::VarSubReg(var, region) | Constraint::RegSubVar(region, var) = *constraint
            && matches!(
                region.kind(),
                RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) | RegionKind::ReStatic
            )
        {
            equal.entry(var).or_insert(region);
        }
    }
    let resolve = |region: Region<'db>| match region.kind() {
        RegionKind::ReVar(var) => equal.get(&var).copied(),
        RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) | RegionKind::ReStatic => {
            Some(region)
        }
        _ => None,
    };
    for (ty, region) in obligations {
        let mut unresolved = false;
        let ty = rustc_type_ir::fold_regions(interner, ty, |it, _| {
            resolve(it).unwrap_or_else(|| {
                unresolved |= matches!(it.kind(), RegionKind::ReVar(_));
                it
            })
        });
        if let Some(region) = resolve(region)
            && !unresolved
        {
            out.push(Needs::Type(ty, region));
        }
    }
    for constraint in constraints {
        let (shorter, longer) = match constraint {
            Constraint::VarSubVar(a, b) => {
                (Region::new_var(interner, a), Region::new_var(interner, b))
            }
            Constraint::RegSubVar(a, b) => (a, Region::new_var(interner, b)),
            Constraint::VarSubReg(a, b) => (Region::new_var(interner, a), b),
            Constraint::RegSubReg(a, b) => (a, b),
        };
        if let (Some(shorter), Some(longer)) = (resolve(shorter), resolve(longer))
            && shorter != longer
        {
            out.push(Needs::Region(longer, shorter));
        }
    }
}

/// rustc's `inferred_outlives_of` the ADTs a check comes across: what each one's fields need
/// of its own parameters, `T: 'a` and `'b: 'a` with `'a` a lifetime parameter, never
/// `'static`. Worked out to a fixed point, as ADTs can contain each other.
#[derive(Default)]
pub struct Inferred<'db> {
    memo: FxHashMap<AdtId, Vec<OutlivesPredicate<DbInterner<'db>, GenericArg<'db>>>>,
    in_progress: FxHashSet<AdtId>,
    cyclic: bool,
    fixing: bool,
}

impl<'db> Inferred<'db> {
    pub fn of(
        &mut self,
        db: &'db dyn HirDatabase,
        adt: AdtId,
    ) -> Vec<OutlivesPredicate<DbInterner<'db>, GenericArg<'db>>> {
        if let Some(found) = self.memo.get(&adt) {
            if self.in_progress.contains(&adt) {
                self.cyclic = true;
            }
            return found.clone();
        }
        self.memo.insert(adt, Vec::new());
        self.in_progress.insert(adt);
        let found = self.compute(db, adt);
        self.in_progress.remove(&adt);
        self.memo.insert(adt, found);
        if self.in_progress.is_empty() && self.cyclic && !self.fixing {
            self.cyclic = false;
            self.fixing = true;
            for _ in 0..8 {
                let mut changed = false;
                let adts: Vec<_> = self.memo.keys().copied().collect();
                for adt in adts {
                    let found = self.compute(db, adt);
                    if self.memo.get(&adt) != Some(&found) {
                        self.memo.insert(adt, found);
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
            self.fixing = false;
        }
        self.memo[&adt].clone()
    }

    fn compute(
        &mut self,
        db: &'db dyn HirDatabase,
        adt: AdtId,
    ) -> Vec<OutlivesPredicate<DbInterner<'db>, GenericArg<'db>>> {
        let mut found = Vec::new();
        for variant in variants(db, adt) {
            for (_, field) in db.field_types(variant).iter() {
                let ty = field.ty().instantiate_identity().skip_norm_wip();
                let mut subtypes = Vec::new();
                walk(ty, &mut subtypes);
                for ty in subtypes {
                    self.required(db, ty, &mut found);
                }
            }
        }
        let mut seen = FxHashSet::default();
        found.retain(|it| seen.insert(*it));
        found
    }

    /// rustc's `insert_required_predicates_to_be_wf`, for one type in a field's.
    fn required(
        &mut self,
        db: &'db dyn HirDatabase,
        ty: Ty<'db>,
        found: &mut Vec<OutlivesPredicate<DbInterner<'db>, GenericArg<'db>>>,
    ) {
        let interner = DbInterner::new_no_crate(db);
        let mut needs = Vec::new();
        match ty.kind() {
            TyKind::Ref(region, inner, _) => needs.push(Needs::Type(inner, region)),
            TyKind::Adt(def, args) => {
                let clauses: Vec<Clause<'db>> =
                    GenericPredicates::query_all(db, def.def_id().into())
                        .iter_instantiated(interner, args.as_slice())
                        .map(|it| it.skip_norm_wip())
                        .collect();
                clause_needs(&clauses, &mut needs);
                for predicate in self.of(db, def.def_id()) {
                    let OutlivesPredicate(arg, region) =
                        EarlyBinder::bind(predicate).instantiate(interner, args).skip_norm_wip();
                    match arg.kind() {
                        GenericArgKind::Type(ty) => needs.push(Needs::Type(ty, region)),
                        GenericArgKind::Lifetime(longer) => {
                            needs.push(Needs::Region(longer, region))
                        }
                        GenericArgKind::Const(_) => (),
                    }
                }
            }
            TyKind::Alias(alias)
                if let rustc_type_ir::AliasTyKind::Projection { .. } = alias.kind
                    && let SolverDefId::TypeAliasId(item) = alias.kind.def_id()
                    && let ItemContainerId::TraitId(trait_) = item.lookup(db).container =>
            {
                let clauses: Vec<Clause<'db>> = GenericPredicates::query_all(db, trait_.into())
                    .iter_instantiated(interner, alias.args.as_slice())
                    .map(|it| it.skip_norm_wip())
                    .collect();
                clause_needs(&clauses, &mut needs);
            }
            _ => (),
        }
        for need in needs {
            match need {
                Needs::Type(ty, region) if is_free(region) => {
                    for component in components(ty) {
                        let arg: GenericArg<'db> = match component {
                            Component::Region(longer) if is_free(longer) => longer.into(),
                            Component::Param(ty) | Component::Alias(ty) => ty.into(),
                            Component::Region(_) => continue,
                        };
                        found.push(OutlivesPredicate(arg, region));
                    }
                }
                Needs::Region(longer, shorter) if is_free(longer) && is_free(shorter) => {
                    found.push(OutlivesPredicate(longer.into(), shorter));
                }
                _ => (),
            }
        }
    }
}

/// Every type in `ty`, `ty` too, as rustc's `walk`.
fn walk<'db>(ty: Ty<'db>, out: &mut Vec<Ty<'db>>) {
    struct Walk<'a, 'db>(&'a mut Vec<Ty<'db>>);
    impl<'db> TypeVisitor<DbInterner<'db>> for Walk<'_, 'db> {
        type Result = ();
        fn visit_ty(&mut self, ty: Ty<'db>) {
            self.0.push(ty);
            ty.super_visit_with(self)
        }
    }
    ty.visit_with(&mut Walk(out));
}

pub(crate) fn variants(db: &dyn HirDatabase, adt: AdtId) -> Vec<VariantId> {
    match adt {
        AdtId::StructId(it) => vec![it.into()],
        AdtId::UnionId(it) => vec![it.into()],
        AdtId::EnumId(it) => {
            it.enum_variants(db).variants.values().map(|&(it, _)| it.into()).collect()
        }
    }
}

/// What an item knows of what outlives what: its where clauses, elaborated, and what the
/// types it assumes well-formed imply.
pub(crate) struct Known<'db> {
    regions: Vec<(Region<'db>, Region<'db>)>,
    types: Vec<(Ty<'db>, Region<'db>)>,
}

impl<'db> Known<'db> {
    pub(crate) fn new(clauses: &[Clause<'db>]) -> Self {
        let mut known = Known { regions: Vec::new(), types: Vec::new() };
        let mut needs = Vec::new();
        clause_needs(clauses, &mut needs);
        known.add(needs);
        known
    }

    /// Knows what `needs` says, as rustc's implied bounds: a type outlives a region when
    /// each of its components does.
    pub(crate) fn add(&mut self, needs: Vec<Needs<'db>>) {
        for need in needs {
            match need {
                Needs::Region(longer, shorter) => self.regions.push((longer, shorter)),
                Needs::Type(ty, region) => {
                    self.types.push((ty, region));
                    for component in components(ty) {
                        match component {
                            Component::Region(longer) => self.regions.push((longer, region)),
                            Component::Param(ty) | Component::Alias(ty) => {
                                self.types.push((ty, region))
                            }
                        }
                    }
                }
            }
        }
    }

    /// Whether `longer` is known to outlive `shorter`. A region this does not reason about,
    /// as an elided one rust-analyzer gives an error region, counts as outliving.
    pub(crate) fn region_outlives(&self, longer: Region<'db>, shorter: Region<'db>) -> bool {
        let named = |region: Region<'db>| {
            matches!(
                region.kind(),
                RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) | RegionKind::ReStatic
            )
        };
        if longer == shorter
            || matches!(longer.kind(), RegionKind::ReStatic)
            || !named(shorter)
            || !matches!(
                longer.kind(),
                RegionKind::ReEarlyParam(_) | RegionKind::ReBound(..) | RegionKind::ReLateParam(_)
            )
        {
            return true;
        }
        let mut seen = vec![longer];
        let mut stack = vec![longer];
        while let Some(region) = stack.pop() {
            for &(from, to) in &self.regions {
                if from != region || seen.contains(&to) {
                    continue;
                }
                if to == shorter || matches!(to.kind(), RegionKind::ReStatic) {
                    return true;
                }
                seen.push(to);
                stack.push(to);
            }
        }
        false
    }

    /// The associated types in `ty` not known to outlive `region`, as rustc's
    /// `alias_bound`s: one does when the where clauses say so, when the trait declares it
    /// does (`type Error: 'static`), or when each of its arguments does.
    pub(crate) fn failing_aliases(
        &self,
        db: &'db dyn HirDatabase,
        ty: Ty<'db>,
        region: Region<'db>,
    ) -> Vec<Ty<'db>> {
        let mut failing = Vec::new();
        if !matches!(
            region.kind(),
            RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) | RegionKind::ReStatic
        ) {
            return failing;
        }
        for component in components(ty) {
            if let Component::Alias(alias) = component
                && !failing.contains(&alias)
                && !self.alias_outlives(db, alias, region, 0)
            {
                failing.push(alias);
            }
        }
        failing
    }

    fn alias_outlives(
        &self,
        db: &'db dyn HirDatabase,
        ty: Ty<'db>,
        region: Region<'db>,
        depth: usize,
    ) -> bool {
        let TyKind::Alias(alias) = ty.kind() else { return true };
        if depth > 8
            || !matches!(alias.kind, rustc_type_ir::AliasTyKind::Projection { .. })
            || ty.has_non_region_infer()
            || ty.references_error()
            || self
                .types
                .iter()
                .any(|&(known, bound)| known == ty && self.region_outlives(bound, region))
        {
            return true;
        }
        let interner = DbInterner::new_no_crate(db);
        let declared = rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id())
            .iter_instantiated(interner, alias.args.as_slice())
            .any(|clause| match clause.kind().skip_binder() {
                ClauseKind::TypeOutlives(OutlivesPredicate(bounded, bound)) => {
                    bounded == ty
                        && !bound.has_escaping_bound_vars()
                        && self.region_outlives(bound, region)
                }
                _ => false,
            });
        if declared {
            return true;
        }
        alias.args.iter().all(|arg| match arg.kind() {
            GenericArgKind::Lifetime(longer) => self.region_outlives(longer, region),
            GenericArgKind::Type(arg) => components(arg).into_iter().all(|it| match it {
                Component::Region(longer) => self.region_outlives(longer, region),
                Component::Param(param) => self
                    .types
                    .iter()
                    .any(|&(known, bound)| known == param && self.region_outlives(bound, region)),
                Component::Alias(inner) => self.alias_outlives(db, inner, region, depth + 1),
            }),
            GenericArgKind::Const(_) => true,
        })
    }

    /// The type parameters in `ty` not known to outlive `region`. Associated types count as
    /// outliving it: what proves that of them (their bounds, the trait's) is not reasoned
    /// about here.
    pub(crate) fn failing_params(&self, ty: Ty<'db>, region: Region<'db>) -> Vec<Ty<'db>> {
        let mut failing = Vec::new();
        if !matches!(
            region.kind(),
            RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) | RegionKind::ReStatic
        ) {
            return failing;
        }
        for component in components(ty) {
            if let Component::Param(param) = component
                && !failing.contains(&param)
                && !self
                    .types
                    .iter()
                    .any(|&(known, bound)| known == param && self.region_outlives(bound, region))
            {
                failing.push(param);
            }
        }
        failing
    }
}
