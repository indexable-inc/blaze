//! What outlives what: what a type's well-formedness needs (rustc's `wf.rs`), what an ADT's
//! fields let rustc infer of its parameters (`rustc_hir_analysis::outlives`, RFC 2093) and
//! what an item knows (its where clauses and the types it may assume well-formed).

#[cfg(test)]
pub(crate) mod tests;

use base_db::Db;
use hir_def::{AdtId, GenericDefId, ItemContainerId, Lookup as _};
use std::alloc::Allocator;

use crate::mem::{self, FMap, FVec};
use blaze_mem::frame::Frame;
use rustc_type_ir::{
    ClauseKind, EarlyBinder, ExistentialPredicate, OutlivesClause, RegionKind, TypeFlags,
    TypeFoldable, TypeFolder, TypeSuperFoldable, TypeSuperVisitable, TypeVisitable,
    TypeVisitableExt as _, TypeVisitor,
    inherent::{GenericArgs as _, IntoKind, Term as _, Ty as _},
};
use smallvec::SmallVec;

use hir_ty::{
    GenericPredicates,
    db::HirDatabase,
    next_solver::{
        AliasTy, Clause, DbInterner, GenericArg, GenericArgKind, GenericArgs, ParamEnv, Region,
        RegionExt as _, StoredRegion, StoredTy, TermId, TraitAssocTyId, Ty, TyKind,
        infer::InferCtxt,
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

/// At most four on the stack, which a type with more spills to the heap.
pub(crate) type Components<'db> = SmallVec<[Component<'db>; 4]>;

const _: () = assert!(size_of::<Component<'static>>() <= 16);

pub(crate) fn components<'db>(ty: Ty<'db>) -> Components<'db> {
    struct Find<'db>(Components<'db>);
    impl<'db> TypeVisitor<DbInterner<'db>> for Find<'db> {
        type Result = ();
        fn visit_ty(&mut self, ty: Ty<'db>) {
            match ty.kind() {
                TyKind::Param(_) => self.0.push(Component::Param(ty)),
                TyKind::Alias(_, _) if !ty.has_escaping_bound_vars() => {
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
    let mut find = Find(SmallVec::new());
    ty.visit_with(&mut find);
    find.0
}

/// The `impl Trait`s in `ty`, outermost first.
pub(crate) fn opaques<'db, A: Allocator>(ty: Ty<'db>, out: &mut Vec<AliasTy<'db>, A>) {
    struct Find<'a, 'db, A: Allocator>(&'a mut Vec<AliasTy<'db>, A>);
    impl<'db, A: Allocator> TypeVisitor<DbInterner<'db>> for Find<'_, 'db, A> {
        type Result = ();
        fn visit_ty(&mut self, ty: Ty<'db>) {
            if let TyKind::Alias(_, alias) = ty.kind()
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
pub(crate) fn outlived_as<'db>(db: &'db Db, ty: Ty<'db>) -> Ty<'db> {
    struct Fold<'db> {
        db: &'db Db,
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
                TyKind::Alias(_, alias)
                    if let rustc_type_ir::AliasTyKind::Opaque { def_id } = alias.kind
                        && !rustc_type_ir::Interner::item_bounds(interner, def_id.into())
                            .iter_instantiated(interner, alias.args.as_slice())
                            .any(|clause| {
                                matches!(
                                    clause.kind().skip_binder(),
                                    ClauseKind::TypeOutlives(OutlivesClause(bounded, _))
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

/// `ty` with every alias in it marked not rigid. Whether the solver has found an alias rigid
/// (it could not normalize it further) says nothing of which type it is, and the bounds a type
/// is checked against come from both sides: `&Self::Output` of
/// `impl<I: SliceIndex<[Value]>> Index<I> for Array` normalizes, for its implied bound, to a
/// rigid `<I as SliceIndex<[Value]>>::Output`, while the callee's signature it is tested
/// against names the same projection as lowered, not rigid (sonic-rs's `Array::index`, E0311);
/// `type Data = T::Data` of wasmtime's `impl<T: AsContext> AsContext for &T`, normalized to
/// prove the trait's `type Data: 'static`, is rigid where the where clauses and the trait's
/// bounds name it as lowered. Types compared as types, to find what bounds them, are compared
/// in this form, as rustc compares `AliasTy`s, which carry no rigidity.
pub fn unrigid<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Ty<'db> {
    struct Unrigid<'db>(DbInterner<'db>);
    impl<'db> TypeFolder<DbInterner<'db>> for Unrigid<'db> {
        fn cx(&self) -> DbInterner<'db> {
            self.0
        }

        fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
            if !ty.has_aliases() {
                return ty;
            }
            let ty = ty.super_fold_with(self);
            match ty.kind() {
                TyKind::Alias(rustc_type_ir::IsRigid::Yes, alias) => {
                    Ty::new_alias(self.0, rustc_type_ir::IsRigid::No, alias)
                }
                _ => ty,
            }
        }
    }
    if !ty.has_rigid_aliases() {
        return ty;
    }
    ty.fold_with(&mut Unrigid(interner))
}

/// A region of a function body, which only the borrow checker reasons about: inference
/// writes its region variables back as error regions, and anything that is not a
/// parameter's, a binder's or `'static` is one.
fn is_body(region: Region<'_>) -> bool {
    !matches!(
        region.kind(),
        RegionKind::ReEarlyParam(_)
            | RegionKind::ReLateParam(_)
            | RegionKind::ReBound(..)
            | RegionKind::ReStatic
    )
}

/// `ty` without the regions of a function body, which only the borrow checker reasons
/// about: a type with such a region of its own, as `&'r T`, is left out, what it needs
/// following from the borrow checker's `'r`, and any other such region is erased.
pub(crate) fn without_body_regions<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Ty<'db> {
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
fn adt_needs<'db, A: Allocator>(
    db: &'db Db,
    inferred: &mut Inferred<'db>,
    adt: AdtId,
    args: GenericArgs<'db>,
    out: &mut Vec<Needs<'db>, A>,
) {
    let interner = DbInterner::new_no_crate(db);
    predicate_needs(db, adt.into(), args.as_slice(), out);
    for predicate in inferred.of(db, adt) {
        let OutlivesClause(arg, region) =
            EarlyBinder::bind(interner, predicate).instantiate(interner, args).skip_norm_wip();
        out.push(match arg.kind() {
            GenericArgKind::Type(ty) => Needs::Type(ty, region),
            GenericArgKind::Lifetime(longer) => Needs::Region(longer, region),
            GenericArgKind::Const(_) => continue,
        });
    }
}

fn is_outlives(clause: &Clause<'_>) -> bool {
    matches!(
        clause.kind().skip_binder(),
        ClauseKind::TypeOutlives(_) | ClauseKind::RegionOutlives(_)
    )
}

/// The outlives bounds among the predicates of `def`, with the arguments `args`. Only those are
/// instantiated: the others, most of them (`T: Sized`, `A: Allocator`), are never folded.
pub(crate) fn predicate_needs<'db, A: Allocator>(
    db: &'db Db,
    def: GenericDefId,
    args: &[GenericArg<'db>],
    out: &mut Vec<Needs<'db>, A>,
) {
    let interner = DbInterner::new_no_crate(db);
    let clauses = GenericPredicates::query_all(db, def)
        .map_bound(|it| it.filter(is_outlives))
        .iter_instantiated(interner, args)
        .map(|it| it.skip_norm_wip());
    clause_needs(clauses, out);
}

/// The outlives bounds among `clauses`.
pub(crate) fn clause_needs<'db, A: Allocator>(
    clauses: impl IntoIterator<Item = Clause<'db>>,
    out: &mut Vec<Needs<'db>, A>,
) {
    for clause in clauses {
        match clause.kind().skip_binder() {
            ClauseKind::TypeOutlives(OutlivesClause(ty, region)) => {
                out.push(Needs::Type(ty, region))
            }
            ClauseKind::RegionOutlives(OutlivesClause(longer, shorter)) => {
                out.push(Needs::Region(longer, shorter))
            }
            _ => (),
        }
    }
}

/// What the well-formedness of `ty` needs of what outlives what: rustc's `wf::obligations`,
/// its outlives obligations. What names a region bound inside `ty` is left out, as rustc
/// leaves it out.
pub(crate) fn wf<'db, A: Allocator>(
    db: &'db Db,
    inferred: &mut Inferred<'db>,
    ty: Ty<'db>,
    out: &mut Vec<Needs<'db>, A>,
) {
    let start = out.len();
    wf_into(db, inferred, ty, out, 0);
    let mut kept = start;
    for at in start..out.len() {
        let need = out[at];
        let escapes = match need {
            Needs::Type(ty, region) => {
                ty.has_escaping_bound_vars() || region.has_escaping_bound_vars()
            }
            Needs::Region(longer, shorter) => {
                longer.has_escaping_bound_vars() || shorter.has_escaping_bound_vars()
            }
        };
        if !escapes {
            out[kept] = need;
            kept += 1;
        }
    }
    out.truncate(kept);
}

/// What a type needs one of for its well-formedness to need anything that can fail, or that
/// tells an item something: a type without a parameter, an alias, an inference variable, a
/// placeholder, an error or a region other than `'static`, a bound one and an erased one
/// (`u8`, `Vec<String>`, `&'static str`) only needs `'static` to outlive what it does.
const MAY_NEED: TypeFlags = TypeFlags::HAS_PARAM
    .union(TypeFlags::HAS_ALIAS)
    .union(TypeFlags::HAS_INFER)
    .union(TypeFlags::HAS_PLACEHOLDER)
    .union(TypeFlags::HAS_ERROR)
    .union(TypeFlags::HAS_FREE_LOCAL_REGIONS);

fn wf_into<'db, A: Allocator>(
    db: &'db Db,
    inferred: &mut Inferred<'db>,
    ty: Ty<'db>,
    out: &mut Vec<Needs<'db>, A>,
    depth: usize,
) {
    if depth > 32 || !ty.has_type_flags(MAY_NEED) {
        return;
    }
    let args =
        |inferred: &mut Inferred<'db>, args: GenericArgs<'db>, out: &mut Vec<Needs<'db>, A>| {
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
        TyKind::Alias(_, alias) => args(inferred, alias.args, out),
        _ => (),
    }
}

/// What the impls of the associated types in `ty` need of what outlives what, given the where
/// clauses of the item `wfcx` checks: rustc's well-formedness asks that the trait of each be
/// implemented, and
/// its inference of an ADT's outlives bounds (RFC 2093) does not infer these, so a lifetime
/// parameter can fail them too. `<T as Dummy<'a>>::Out` with only
/// `impl<'a, T: 'a> Dummy<'a> for T` needs `T: 'a`.
pub(crate) fn projection_needs<'db, A: Allocator>(
    db: &'db Db,
    wfcx: &Wfcx<'_, 'db>,
    ty: Ty<'db>,
    out: &mut Vec<Needs<'db>, A>,
) {
    let interner = DbInterner::new_no_crate(db);
    let mut aliases = Vec::new_in(wfcx.frame);
    walk(ty, &mut aliases);
    let predicates = mem::collect(
        wfcx.frame,
        aliases.into_iter().filter_map(|it| match it.kind() {
            TyKind::Alias(_, alias)
                if matches!(alias.kind, rustc_type_ir::AliasTyKind::Projection { .. })
                    && !it.has_escaping_bound_vars()
                    && !it.references_error() =>
            {
                Some(trait_predicate(interner, alias))
            }
            _ => None,
        }),
    );
    prove_needs(wfcx, &predicates, out);
}

/// What proving the trait bounds the well-formedness of `ty` asks needs of what outlives what,
/// given the where clauses of the item `wfcx` checks: of each associated type in it, that its trait is
/// implemented, and of each struct or enum, its where clauses. `ServiceChainBuilder<
/// ServiceChain<P, S>, NS>` of `struct ServiceChainBuilder<P: Service, ..>` with only
/// `impl<P: Service, S: Service> Service for ServiceChain<P, S> where P::Error: 'static`
/// needs `P::Error: 'static`.
pub(crate) fn trait_needs<'db, A: Allocator>(
    db: &'db Db,
    wfcx: &Wfcx<'_, 'db>,
    ty: Ty<'db>,
    out: &mut Vec<Needs<'db>, A>,
) {
    let mut predicates = Vec::new_in(wfcx.frame);
    trait_predicates(db, wfcx.frame, ty, &mut predicates);
    prove_needs(wfcx, &predicates, out);
}

/// The predicates [`trait_needs`] proves: the trait of each projection in `ty`, and the trait
/// and projection predicates of each struct or enum, instantiated.
pub(crate) fn trait_predicates<'db, A: Allocator>(
    db: &'db Db,
    frame: &Frame,
    ty: Ty<'db>,
    predicates: &mut Vec<hir_ty::next_solver::Predicate<'db>, A>,
) {
    use rustc_type_ir::inherent::Clause as _;
    let interner = DbInterner::new_no_crate(db);
    let mut tys = Vec::new_in(frame);
    walk(ty, &mut tys);
    for it in tys {
        if it.has_escaping_bound_vars() || it.references_error() {
            continue;
        }
        match it.kind() {
            TyKind::Alias(_, alias)
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
}

fn trait_predicate<'db>(
    interner: DbInterner<'db>,
    alias: AliasTy<'db>,
) -> hir_ty::next_solver::Predicate<'db> {
    use rustc_type_ir::Upcast as _;
    rustc_type_ir::TraitClause {
        trait_ref: alias.trait_ref(interner),
        polarity: rustc_type_ir::ClausePolarity::Positive,
    }
    .upcast(interner)
}

/// rustc's `WfCheckingCtxt`: an inference context in the environment of the item checked,
/// which the solver's proofs and normalizations of one check share.
pub(crate) struct Wfcx<'f, 'db> {
    frame: &'f Frame,
    db: &'db Db,
    def: hir_def::GenericDefId,
    /// The environment, when it is not `def`'s own (`trait_environment`): that of an item with
    /// no `GenericDefId`, as an impl's item for a trait method's `impl Trait`.
    env: Option<ParamEnv<'db>>,
    interner: DbInterner<'db>,
    /// Built by the first normalization or proof: an item whose types have no alias and no
    /// trait predicate to prove never needs one.
    solver: std::cell::OnceCell<(InferCtxt<'db>, ParamEnv<'db>)>,
}

impl<'f, 'db> Wfcx<'f, 'db> {
    pub(crate) fn new(frame: &'f Frame, db: &'db Db, def: hir_def::GenericDefId) -> Self {
        use hir_def::HasModule as _;
        let interner = DbInterner::new_with(db, def.module(db).krate(db)).with_regions();
        Wfcx { frame, db, def, env: None, interner, solver: std::cell::OnceCell::new() }
    }

    /// A context in the environment `env`, in the crate of `def`.
    pub(crate) fn with_env(
        frame: &'f Frame,
        db: &'db Db,
        def: hir_def::GenericDefId,
        env: ParamEnv<'db>,
    ) -> Self {
        Wfcx { env: Some(env), ..Wfcx::new(frame, db, def) }
    }

    pub(crate) fn interner(&self) -> DbInterner<'db> {
        self.interner
    }

    fn infcx(&self) -> &InferCtxt<'db> {
        &self.solver().0
    }

    fn env(&self) -> ParamEnv<'db> {
        self.solver().1
    }

    fn solver(&self) -> &(InferCtxt<'db>, ParamEnv<'db>) {
        use hir_ty::next_solver::{TypingMode, infer::DbInternerInferExt as _};
        self.solver.get_or_init(|| {
            let infcx = self.interner.infer_ctxt().build(TypingMode::non_body_analysis());
            (infcx, self.env.unwrap_or_else(|| self.db.trait_environment(self.def)))
        })
    }

    /// How many region obligations and constraints the solver has made so far.
    fn mark(&self) -> (usize, usize) {
        let mut inner = self.infcx().inner.borrow_mut();
        let obligations = inner.region_obligations().len();
        (obligations, inner.unwrap_region_constraints().region_constraint_data().constraints.len())
    }

    /// What the solver has needed of what outlives what since `mark`: the type outlives
    /// obligations and region constraints of the item's own regions.
    fn needs_since<A: Allocator>(&self, mark: (usize, usize), out: &mut Vec<Needs<'db>, A>) {
        use hir_ty::next_solver::infer::region_constraints::Constraint;
        let (interner, infcx) = (self.interner, self.infcx());
        let mut inner = infcx.inner.borrow_mut();
        let obligations = mem::collect(
            self.frame,
            inner.region_obligations()[mark.0..].iter().map(|it| (it.sup_type, it.sub_region)),
        );
        let constraints: FVec<'_, Constraint<'db>> = mem::collect(
            self.frame,
            inner
                .unwrap_region_constraints()
                .region_constraint_data()
                .constraints
                .iter_from(mark.1),
        );
        drop(inner);
        // The impl's lifetimes are inference variables the solver equates with the item's.
        let mut equal: FMap<'_, rustc_type_ir::RegionVid, Region<'db>> = mem::map(self.frame);
        for constraint in &constraints {
            if let Constraint::VarSubReg(var, region) | Constraint::RegSubVar(region, var) =
                *constraint
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

    /// `ty` normalized, as rustc's `WfCheckingCtxt::normalize`. A lifetime normalizing makes,
    /// as one of a `for<'f>` where clause it uses, is a region variable of its own each time:
    /// rustc normalizes what a signature implies apart from its well-formedness, so
    /// `<Q as QueryBase>::Db` with `Q: for<'f> AsyncQueryFunction<'f>` (a supertrait
    /// `QueryBase<Db = <Self as AsyncQueryFunction<'f>>::SendDb>`) is a `SendDb` of a
    /// different lifetime in each. What normalizing needs of what outlives what goes to
    /// `out`: the where clauses of the impls it goes through, as `T: 'b` of `impl<'b, T, U>
    /// AsRef2 for Foo<T> where T: AsRef2<Output<'b> = &'b [U]>` with `type Output<'a> where
    /// Self: 'a`, whose own where clause normalizing `<T as AsRef2>::Output<'b>` asks.
    pub(crate) fn normalize<A: Allocator>(
        &self,
        ty: Ty<'db>,
        out: &mut Vec<Needs<'db>, A>,
    ) -> Ty<'db> {
        use hir_ty::next_solver::{
            infer::traits::ObligationCause, obligation_ctxt::ObligationCtxt,
        };
        if !ty.has_aliases() || ty.has_escaping_bound_vars() {
            return ty;
        }
        let mark = self.mark();
        let ocx = ObligationCtxt::new(self.infcx());
        let Ok(normalized) = ocx.deeply_normalize(&ObligationCause::dummy(), self.env(), ty) else {
            return ty;
        };
        self.needs_since(mark, out);
        let mut inner = self.infcx().inner.borrow_mut();
        let mut constraints = inner.unwrap_region_constraints();
        rustc_type_ir::fold_regions(self.interner, normalized, |region, _| match region.kind() {
            RegionKind::ReVar(var) => constraints.shallow_resolve_region_var(self.interner, var),
            _ => region,
        })
    }
}

/// What proving `predicates` with the trait solver needs of what outlives what, given the
/// where clauses of the item `wfcx` checks: the where clauses of the impls that prove them.
/// Nothing when one is not proven.
fn prove_needs<'db, A: Allocator>(
    wfcx: &Wfcx<'_, 'db>,
    predicates: &[hir_ty::next_solver::Predicate<'db>],
    out: &mut Vec<Needs<'db>, A>,
) {
    use hir_ty::next_solver::{
        infer::traits::{Obligation, ObligationCause},
        obligation_ctxt::ObligationCtxt,
    };
    if predicates.is_empty() {
        return;
    }
    let (interner, infcx, env) = (wfcx.interner, wfcx.infcx(), wfcx.env());
    let (obligations_before, constraints_before) = wfcx.mark();
    for &predicate in predicates {
        let obligation = Obligation::new(interner, ObligationCause::dummy(), env, predicate);
        // A global predicate names none of the item's parameters or regions, so all its proof
        // can need is that a global type or `'static` outlives `'static`, which holds: only
        // whether it holds matters, which the solver's evaluation (its global cache) answers
        // without a fulfillment context.
        if predicate.is_global() {
            if !infcx.predicate_must_hold_modulo_regions(&obligation) {
                return;
            }
            continue;
        }
        let mut ocx = ObligationCtxt::new(infcx);
        ocx.register_obligation(obligation);
        if !ocx.evaluate_obligations_error_on_ambiguity().is_empty() {
            return;
        }
    }
    wfcx.needs_since((obligations_before, constraints_before), out);
}

/// Proves each of `predicates` with the trait solver, given the where clauses of the item
/// `wfcx` checks, each in a fulfillment context of its own, as rustc proves an item's bounds
/// (`check_type_bounds`): the index of each that fails or stays ambiguous, or, when all hold,
/// none, and what their proofs need of what outlives what goes to `out`.
pub(crate) fn prove_each<'db, 'f, A: Allocator>(
    wfcx: &Wfcx<'f, 'db>,
    predicates: &[hir_ty::next_solver::Predicate<'db>],
    out: &mut Vec<Needs<'db>, A>,
) -> FVec<'f, usize> {
    use hir_ty::next_solver::{
        infer::traits::{Obligation, ObligationCause},
        obligation_ctxt::ObligationCtxt,
    };
    let mut failed = Vec::new_in(wfcx.frame);
    if predicates.is_empty() {
        return failed;
    }
    let (interner, infcx, env) = (wfcx.interner, wfcx.infcx(), wfcx.env());
    let mark = wfcx.mark();
    for (index, &predicate) in predicates.iter().enumerate() {
        let mut ocx = ObligationCtxt::new(infcx);
        let cause = ObligationCause::dummy();
        ocx.register_obligation(Obligation::new(interner, cause, env, predicate));
        if !ocx.evaluate_obligations_error_on_ambiguity().is_empty() {
            failed.push(index);
        }
    }
    if failed.is_empty() {
        wfcx.needs_since(mark, out);
    }
    failed
}

/// [`prove_needs`] as it was: every predicate proven in a fulfillment context of its own.
#[cfg(test)]
pub(crate) fn prove_needs_reference<'db, A: Allocator>(
    wfcx: &Wfcx<'_, 'db>,
    predicates: &[hir_ty::next_solver::Predicate<'db>],
    out: &mut Vec<Needs<'db>, A>,
) {
    use hir_ty::next_solver::{
        infer::traits::{Obligation, ObligationCause},
        obligation_ctxt::ObligationCtxt,
    };
    if predicates.is_empty() {
        return;
    }
    let (interner, infcx, env) = (wfcx.interner, wfcx.infcx(), wfcx.env());
    let mark = wfcx.mark();
    for &predicate in predicates {
        let mut ocx = ObligationCtxt::new(infcx);
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
    wfcx.needs_since(mark, out);
}

/// rustc's `inferred_outlives_of` the ADTs a check comes across: what each one's fields need
/// of its own parameters, `T: 'a` and `'b: 'a` with `'a` a lifetime parameter, never
/// `'static`. Read in place from the query's memo: a lookup allocates nothing.
#[derive(Default)]
pub struct Inferred<'db> {
    _db: std::marker::PhantomData<&'db ()>,
}

impl<'db> Inferred<'db> {
    pub fn of(
        &self,
        db: &'db Db,
        adt: AdtId,
    ) -> impl Iterator<Item = OutlivesClause<DbInterner<'db>, GenericArg<'db>>> + use<'db> {
        inferred_outlives(db, adt).0.iter().map(|it| {
            let arg: GenericArg<'db> = match &it.arg {
                InferredArg::Type(ty) => ty.as_ref().into(),
                InferredArg::Region(region) => region.as_ref().into(),
            };
            OutlivesClause(arg, it.region.as_ref())
        })
    }
}

/// One of an ADT's inferred outlives predicates as the query database keeps it: what outlives, a type or a
/// region, and the region.
#[derive(Debug, Clone, PartialEq, Eq, blaze_query::Value)]
enum InferredArg {
    Type(StoredTy),
    Region(StoredRegion),
}

#[derive(Debug, Clone, PartialEq, Eq, blaze_query::Value)]
struct InferredPredicate {
    arg: InferredArg,
    region: StoredRegion,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, blaze_query::Value)]
struct InferredOutlives(Vec<InferredPredicate>);

/// A query, so an edit checks again only the ADTs it changes. Worked out to a fixed point, as
/// ADTs can contain each other.
#[blaze_query::tracked(
    returns(ref),
    cycle_fn = inferred_outlives_cycle_fn,
    cycle_initial = inferred_outlives_cycle_initial,
)]
fn inferred_outlives(db: &Db, adt: AdtId) -> InferredOutlives {
    blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
        let mut found = Vec::new_in(frame);
        let mut subtypes = Vec::new_in(frame);
        for variant in adt.variants(db) {
            for (_, field) in db.field_types(variant).iter() {
                let ty = field.ty().instantiate_identity().skip_norm_wip();
                subtypes.clear();
                walk_with(ty, TypeFlags::HAS_RE_PARAM, &mut subtypes);
                for &ty in &subtypes {
                    required(db, ty, &mut found);
                }
            }
        }
        let mut seen = mem::set(frame);
        found.retain(|it| seen.insert(*it));
        let kept = mem::collect(
            frame,
            found.into_iter().filter_map(|OutlivesClause(arg, region)| {
                let arg = match arg.kind() {
                    GenericArgKind::Type(ty) => InferredArg::Type(ty.store()),
                    GenericArgKind::Lifetime(region) => InferredArg::Region(region.store()),
                    GenericArgKind::Const(_) => return None,
                };
                Some(InferredPredicate { arg, region: region.store() })
            }),
        );
        InferredOutlives(mem::owned(kept.into_iter()))
    })
}

fn inferred_outlives_cycle_initial(
    _db: &Db,
    _: blaze_query::db::Id,
    _adt: AdtId,
) -> InferredOutlives {
    InferredOutlives::default()
}

fn inferred_outlives_cycle_fn(
    _db: &Db,
    _: &blaze_query::db::Cycle<'_>,
    _last_provisional_value: &InferredOutlives,
    value: InferredOutlives,
    _adt: AdtId,
) -> InferredOutlives {
    value
}

/// rustc's `insert_required_predicates_to_be_wf`, for one type in a field's. What it finds
/// outlives a lifetime parameter, which comes from `ty`'s arguments: a type without one
/// finds nothing.
fn required<'db, A: Allocator + Copy>(
    db: &'db Db,
    ty: Ty<'db>,
    found: &mut Vec<OutlivesClause<DbInterner<'db>, GenericArg<'db>>, A>,
) {
    if !ty.has_type_flags(TypeFlags::HAS_RE_PARAM) {
        return;
    }
    let interner = DbInterner::new_no_crate(db);
    let mut needs = Vec::new_in(*found.allocator());
    match ty.kind() {
        TyKind::Ref(region, inner, _) => needs.push(Needs::Type(inner, region)),
        TyKind::Adt(def, args) => {
            predicate_needs(db, def.def_id().into(), args.as_slice(), &mut needs);
            for predicate in Inferred::default().of(db, def.def_id()) {
                let OutlivesClause(arg, region) = EarlyBinder::bind(interner, predicate)
                    .instantiate(interner, args)
                    .skip_norm_wip();
                match arg.kind() {
                    GenericArgKind::Type(ty) => needs.push(Needs::Type(ty, region)),
                    GenericArgKind::Lifetime(longer) => needs.push(Needs::Region(longer, region)),
                    GenericArgKind::Const(_) => (),
                }
            }
        }
        TyKind::Alias(_, alias)
            if let rustc_type_ir::AliasTyKind::Projection { def_id: TraitAssocTyId(item) } =
                alias.kind
                && let ItemContainerId::TraitId(trait_) = TermId::from(item).container(db) =>
        {
            predicate_needs(db, trait_.into(), alias.args.as_slice(), &mut needs);
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
                    found.push(OutlivesClause(arg, region));
                }
            }
            Needs::Region(longer, shorter) if is_free(longer) && is_free(shorter) => {
                found.push(OutlivesClause(longer.into(), shorter));
            }
            _ => (),
        }
    }
}

/// Every type in `ty`, `ty` too, as rustc's `walk`.
fn walk<'db, A: Allocator>(ty: Ty<'db>, out: &mut Vec<Ty<'db>, A>) {
    struct Walk<'a, 'db, A: Allocator>(&'a mut Vec<Ty<'db>, A>);
    impl<'db, A: Allocator> TypeVisitor<DbInterner<'db>> for Walk<'_, 'db, A> {
        type Result = ();
        fn visit_ty(&mut self, ty: Ty<'db>) {
            self.0.push(ty);
            ty.super_visit_with(self)
        }
    }
    ty.visit_with(&mut Walk(out));
}

/// The types in `ty`, `ty` too, that have one of `flags`, in `walk`'s order: one without has
/// none in it either, so it is not entered.
fn walk_with<'db, A: Allocator>(ty: Ty<'db>, flags: TypeFlags, out: &mut Vec<Ty<'db>, A>) {
    struct Walk<'a, 'db, A: Allocator>(&'a mut Vec<Ty<'db>, A>, TypeFlags);
    impl<'db, A: Allocator> TypeVisitor<DbInterner<'db>> for Walk<'_, 'db, A> {
        type Result = ();
        fn visit_ty(&mut self, ty: Ty<'db>) {
            if !ty.has_type_flags(self.1) {
                return;
            }
            self.0.push(ty);
            ty.super_visit_with(self)
        }
    }
    ty.visit_with(&mut Walk(out, flags));
}

/// What an item knows of what outlives what: its where clauses, elaborated, and what the
/// types it assumes well-formed imply.
pub(crate) struct Known<'f, 'db> {
    regions: FVec<'f, (Region<'db>, Region<'db>)>,
    types: FVec<'f, (Ty<'db>, Region<'db>)>,
}

impl<'f, 'db> Known<'f, 'db> {
    pub(crate) fn new(frame: &'f Frame, clauses: &[Clause<'db>]) -> Self {
        let mut known = Known { regions: Vec::new_in(frame), types: Vec::new_in(frame) };
        let mut needs = Vec::new_in(frame);
        clause_needs(clauses.iter().copied(), &mut needs);
        known.add(needs);
        known
    }

    /// Knows what `needs` says, as rustc's implied bounds: a type outlives a region when
    /// each of its components does.
    pub(crate) fn add(&mut self, needs: impl IntoIterator<Item = Needs<'db>>) {
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
        let mut seen = Vec::new_in(*self.regions.allocator());
        seen.push(longer);
        let mut next = 0;
        while let Some(&region) = seen.get(next) {
            next += 1;
            for &(from, to) in self.regions.iter() {
                if from != region || seen.contains(&to) {
                    continue;
                }
                if to == shorter || matches!(to.kind(), RegionKind::ReStatic) {
                    return true;
                }
                seen.push(to);
            }
        }
        false
    }

    /// The associated types in `ty` not known to outlive `region`, as rustc's
    /// `alias_bound`s: one does when the where clauses say so, when the trait declares it
    /// does (`type Error: 'static`), or when each of its arguments does. Whether the solver
    /// found one rigid does not matter ([`unrigid`]).
    pub(crate) fn failing_aliases(
        &self,
        db: &'db Db,
        ty: Ty<'db>,
        region: Region<'db>,
    ) -> FVec<'f, Ty<'db>> {
        let mut failing = Vec::new_in(*self.regions.allocator());
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

    fn alias_outlives(&self, db: &'db Db, ty: Ty<'db>, region: Region<'db>, depth: usize) -> bool {
        let TyKind::Alias(_, alias) = ty.kind() else { return true };
        let rustc_type_ir::AliasTyKind::Projection { .. } = alias.kind else { return true };
        if depth > 8 || ty.has_non_region_infer() || ty.references_error() {
            return true;
        }
        let interner = DbInterner::new_no_crate(db);
        // The alias as lowered: one the solver normalized to, rigid, is the one the where
        // clauses and the trait's bounds name (rustc's `alias_bound` compares `AliasTy`s).
        let ty = unrigid(interner, ty);
        let TyKind::Alias(_, alias) = ty.kind() else { return true };
        if self.types.iter().any(|&(known, bound)| {
            unrigid(interner, known) == ty && self.region_outlives(bound, region)
        }) {
            return true;
        }
        // rustc's `declared_bounds_from_definition`: the outlives bounds the trait declares of
        // the associated type itself (`item_self_bounds`), with the alias's arguments.
        let declared = mem::collect(
            *self.regions.allocator(),
            rustc_type_ir::outlives::declared_bounds_from_definition(interner, alias),
        );
        if declared.iter().any(|&bound| self.region_outlives(bound, region)) {
            return true;
        }
        // rustc's `alias_ty_must_outlive`: with a region variable normalizing made in it and
        // no bound from the where clauses or the trait, even one of other lifetimes, its
        // arguments must outlive the region, which a variable can be made to. With one, that
        // is verified instead, and a variable outlives nothing: `<Q as Trait<'?2>>::Out: 'a`
        // known only of `<Q as Trait<'?1>>::Out` does not hold.
        let erased = |ty: Ty<'db>| {
            rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_erased(interner))
        };
        let verified = ty.has_infer_regions()
            && (!declared.is_empty()
                || self
                    .types
                    .iter()
                    .any(|&(known, _)| erased(unrigid(interner, known)) == erased(ty)));
        let outlives = |longer: Region<'db>| {
            !(verified && matches!(longer.kind(), RegionKind::ReVar(_)))
                && self.region_outlives(longer, region)
        };
        alias.args.iter().all(|arg| match arg.kind() {
            GenericArgKind::Lifetime(longer) => outlives(longer),
            GenericArgKind::Type(arg) => components(arg).into_iter().all(|it| match it {
                Component::Region(longer) => outlives(longer),
                Component::Param(param) => self
                    .types
                    .iter()
                    .any(|&(known, bound)| known == param && self.region_outlives(bound, region)),
                Component::Alias(inner) => self.alias_outlives(db, inner, region, depth + 1),
            }),
            GenericArgKind::Const(_) => true,
        })
    }

    /// `ty` with each `impl Trait` in it replaced by what must outlive `region` for it to,
    /// as rustc's `alias_ty_must_outlive` of an opaque type: nothing when an outlives bound it
    /// declares is known to outlive `region`; otherwise each argument it captures (its
    /// variances leave out the others, `use<..>`), a lifetime as `&'r ()`, so a lifetime that
    /// fails is the type's own (E0477) and a type parameter that fails is named (E0309,
    /// E0310). `impl Get` of `fn mks<'a>(&self, x: &'a u8)` captures `'a` and the lifetime of
    /// `&self`, so it outlives `'static` only if they do.
    pub(crate) fn opaques_as_captures(
        &self,
        db: &'db Db,
        ty: Ty<'db>,
        region: Region<'db>,
    ) -> Ty<'db> {
        self.opaques_as_captures_with(db, ty, region, false)
    }

    /// [`Self::opaques_as_captures`] of `ty`, the type a returned `impl Trait` hides, where
    /// an `impl Trait` in it whose outlives bound is a region of the body counts as
    /// outliving `region`: rustc's `alias_ty_must_outlive` relates a unique declared bound
    /// alone (`'?b: 'r`) and never its captured types, and that edge is the borrow checker's
    /// (`hidden_aliases_outlive`), as the body region is. `inner_t(x, t)` of `-> impl Plain +
    /// 'a`, `inner_t<'b, U>` returning `impl Plain + 'b`, needs nothing of `T` here. A bound
    /// that is itself an error region, a `+ '_` lowering could not name, still proves nothing.
    pub(crate) fn hidden_opaques_as_captures(
        &self,
        db: &'db Db,
        ty: Ty<'db>,
        region: Region<'db>,
    ) -> Ty<'db> {
        self.opaques_as_captures_with(db, ty, region, true)
    }

    fn opaques_as_captures_with(
        &self,
        db: &'db Db,
        ty: Ty<'db>,
        region: Region<'db>,
        body_bounds_outlive: bool,
    ) -> Ty<'db> {
        struct Fold<'k, 'f, 'db> {
            known: &'k Known<'f, 'db>,
            interner: DbInterner<'db>,
            region: Region<'db>,
            body_bounds_outlive: bool,
            depth: usize,
        }
        impl<'db> TypeFolder<DbInterner<'db>> for Fold<'_, '_, 'db> {
            fn cx(&self) -> DbInterner<'db> {
                self.interner
            }
            fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
                use rustc_type_ir::Interner as _;
                let TyKind::Alias(_, alias) = ty.kind() else { return ty.super_fold_with(self) };
                let rustc_type_ir::AliasTyKind::Opaque { def_id } = alias.kind else {
                    return ty.super_fold_with(self);
                };
                if self.depth > 16 {
                    return ty;
                }
                let interner = self.interner;
                // The bounds as declared, beside the same bounds instantiated, in one order.
                let instantiated = interner
                    .item_bounds(def_id.into())
                    .iter_instantiated(interner, alias.args.as_slice());
                let declared = interner
                    .item_bounds(def_id.into())
                    .skip_binder()
                    .into_iter()
                    .zip(instantiated)
                    .any(|(as_declared, clause)| {
                        let ClauseKind::TypeOutlives(OutlivesClause(bounded, bound)) =
                            clause.kind().skip_binder()
                        else {
                            return false;
                        };
                        // A `+ '_` lowering could not name proves nothing.
                        let unnamed = match as_declared.kind().skip_binder() {
                            ClauseKind::TypeOutlives(OutlivesClause(_, it)) => it.is_error(),
                            _ => true,
                        };
                        // The opaque type itself, rigid or not.
                        let own = matches!(bounded.kind(), TyKind::Alias(_, it)
                            if it.kind == alias.kind && it.args == alias.args)
                            && !bound.has_escaping_bound_vars()
                            && !unnamed;
                        own && ((self.body_bounds_outlive && is_body(bound))
                            || (!bound.is_error()
                                && self.known.region_outlives(bound, self.region)))
                    });
                if declared {
                    return Ty::new_unit(interner);
                }
                let variances = interner.variances_of(def_id.into());
                let captured = |at: usize| {
                    let variance = variances.as_slice().get(at);
                    variance.is_none_or(|it| *it != rustc_type_ir::Variance::Bivariant)
                };
                self.depth += 1;
                let mut parts = SmallVec::<[Ty<'db>; 8]>::new();
                for (at, arg) in alias.args.iter().enumerate() {
                    if !captured(at) {
                        continue;
                    }
                    match arg.kind() {
                        GenericArgKind::Type(it) => parts.push(it.fold_with(self)),
                        GenericArgKind::Lifetime(it) => parts.push(Ty::new_ref(
                            interner,
                            it,
                            Ty::new_unit(interner),
                            rustc_type_ir::Mutability::Not,
                        )),
                        GenericArgKind::Const(_) => (),
                    }
                }
                self.depth -= 1;
                Ty::new_tup(interner, &parts)
            }
        }
        if !ty.has_opaque_types() {
            return ty;
        }
        let interner = DbInterner::new_no_crate(db);
        ty.fold_with(&mut Fold { known: self, interner, region, body_bounds_outlive, depth: 0 })
    }

    /// The type parameters in `ty` not known to outlive `region`. Associated types count as
    /// outliving it: what proves that of them (their bounds, the trait's) is not reasoned
    /// about here.
    pub(crate) fn failing_params(&self, ty: Ty<'db>, region: Region<'db>) -> FVec<'f, Ty<'db>> {
        let mut failing = Vec::new_in(*self.regions.allocator());
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
