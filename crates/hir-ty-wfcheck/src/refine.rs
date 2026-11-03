//! rustc's `refining_impl_trait` lint (`refining_impl_trait_reachable` and
//! `refining_impl_trait_internal`), its semantic half: whether a trait impl's method promises
//! more about what it returns than its trait's method does where the trait's returns an
//! `impl Trait` (RPITIT). It does when the impl's type for one of the trait's `impl Trait`s is
//! not an `impl Trait` of the impl's method (a concrete type, or another item's opaque type),
//! when an `impl Trait` of the impl's method has a bound the trait's lacks (compared
//! syntactically once both are normalized, so a projection the impl wrote normalized is no
//! refinement), or when it captures fewer generic parameters than the trait's does.
//!
//! Each type here is in the generics of the impl's item for the trait's `impl Trait`
//! ([`hir_ty::rpitit`]): the impl's parameters, then the impl method's own early-bound ones,
//! then the trait method's late-bound and left-out lifetimes, where rustc liberates the late-bound
//! ones onto the impl's method. The two sides name the same parameters either way.
//!
//! Which of the two lints, and the diagnostic, are the diagnostics crate's
//! (`checks/impls/refine.rs`). Ported from
//! `compiler/rustc_hir_analysis/src/check/compare_impl_item/refine.rs`
//! (`check_refining_return_position_impl_trait_in_trait`) (MIT or Apache-2.0).

use base_db::Db;
use hir_def::{
    CallableDefId, FunctionId, HasModule as _, ItemContainerId, Lookup as _,
    signatures::FunctionSignature,
};
use hir_ty::{
    ImplTraitId,
    db::HirDatabase as _,
    next_solver::{
        AliasTyKind, Binder, BoundRegion, BoundVariableKind, Clause, ClauseKind, DbInterner,
        GenericArg, GenericArgKind, GenericArgs, Region, RegionKind, SolverDefId, Ty, TyKind,
        TypingMode,
        infer::{DbInternerInferExt as _, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
    rpitit::{self, RpititId},
};
use rustc_type_ir::{
    TypeFoldable, TypeFolder, TypeSuperVisitable as _, TypeVisitable as _, TypeVisitableExt as _,
    TypeVisitor, Variance,
    inherent::{GenericArgs as _, IntoKind as _, Term as _},
};

/// How a trait impl's method refines the `impl Trait`s its trait's method returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplTraitRefinement<'db> {
    /// The trait's method the impl's implements.
    pub trait_method: FunctionId,
    pub kind: RefinementKind<'db>,
    /// The trait method's return type as the impl's method would write it to match the trait:
    /// the impl's trait arguments with `Self` for the self type, the trait's `impl Trait`s as
    /// projections (printed as `impl Bounds`), the late-bound lifetimes the impl method's. When
    /// both methods are `async fn`, the future's `Output` type, what each writes.
    pub trait_return: Ty<'db>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefinementKind<'db> {
    /// The impl's type for the trait's `impl Trait` number `rpitit` (as [`RpititId::idx`]
    /// counts) is `hidden`, which is not an `impl Trait` of the impl's method.
    NotImplTrait { rpitit: u32, hidden: Ty<'db> },
    /// The `impl Trait` number `opaque` of the impl method's return type has the bound `clause`
    /// (elaborated and normalized) the trait's `impl Trait` it stands for lacks. `written` is
    /// the bound as the impl's `impl Trait` has it, before elaborating, whose source it is.
    StrongerBound { opaque: u32, written: Clause<'db>, clause: Clause<'db> },
    /// Each `impl Trait` of the impl method's return type that captures fewer generic
    /// parameters than the trait's it stands for.
    FewerCaptures(Box<[Undercapture<'db>]>),
}

/// An `impl Trait` of the impl method's return type that does not capture each parameter the
/// trait's `impl Trait` it stands for does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undercapture<'db> {
    /// Its number among the `impl Trait`s of the impl method's return type.
    pub opaque: u32,
    /// What the trait's `impl Trait` captures, lifetimes first: what its `use<..>` would list.
    pub trait_captures: GenericArgs<'db>,
}

/// rustc's `check_refining_return_position_impl_trait_in_trait` for `impl_method`, a method of
/// a trait impl. `None` when it refines nothing, when it implements no trait method returning
/// an `impl Trait`, and when the impl's types for the trait's `impl Trait`s have an error
/// (`collect_return_position_impl_trait_in_trait_tys` failed, each reported of its own).
/// rustc checks a method only once `compare_impl_item` found no error; the caller does too.
pub fn impl_trait_refinement<'db>(
    db: &'db Db,
    impl_method: FunctionId,
) -> Option<ImplTraitRefinement<'db>> {
    let trait_method = rpitit::implemented_trait_method(db, impl_method)?;
    let ItemContainerId::ImplId(impl_) = impl_method.lookup(db).container else { return None };
    // rustc's `impl_method_has_trait_impl_trait_tys`, and its return on an error.
    let hidden_tys = rpitit::impl_rpitit_tys(db, impl_method);
    if hidden_tys.is_empty() {
        return None;
    }
    if hidden_tys.iter().any(|it| it.as_ref().is_err()) {
        return None;
    }
    let interner = DbInterner::new_with(db, impl_method.module(db).krate(db));
    let trait_ref = db.impl_trait(impl_)?.instantiate_identity().skip_norm_wip();
    if trait_ref.references_error() {
        return None;
    }
    let rpitits = || rpitit::rpitits_of(db, trait_method);
    // The generics of the impl's items for the trait's `impl Trait`s, all the same: the
    // trait's item's arguments are the impl's trait arguments, then the same own ones.
    let item_args = |rpitit: RpititId| {
        GenericArgs::identity_for_item(
            interner,
            SolverDefId::RpititId(RpititId { function: impl_method, idx: rpitit.idx }),
        )
    };
    let trait_args =
        |rpitit: RpititId| item_args(rpitit).rebase_onto(interner, impl_.into(), trait_ref.args);
    let hidden_of = |rpitit: RpititId| {
        let hidden = hidden_tys[rpitit.idx.into_raw().into_u32() as usize]
            .as_ref()
            .expect("no error, checked above");
        hidden.get().instantiate(interner, item_args(rpitit)).skip_norm_wip()
    };
    if rpitits().any(|it| hidden_of(it).references_error()) {
        return None;
    }
    let trait_return = trait_return(db, interner, impl_method, trait_method, trait_ref.args);
    let refined = |kind| Some(ImplTraitRefinement { trait_method, kind, trait_return });

    // Each trait `impl Trait`, nested ones first as rustc's collector reversed, with the
    // impl's `impl Trait` standing for it and the arguments of both.
    let mut pairs = Vec::new(); // alloc: per impl method returning `impl Trait`, one per `impl Trait`
    // The trait's bounds, and the impl's with the bound each is elaborated from.
    let mut trait_bounds = Vec::new(); // alloc: as `pairs`, a few bounds each
    let mut impl_bounds = Vec::new(); // alloc: as `pairs`, a few bounds each
    // alloc: per impl method returning `impl Trait`, the trait's `impl Trait`s, reversed
    let reversed: Vec<RpititId> = rpitits().collect();
    for &rpitit in reversed.iter().rev() {
        let hidden = hidden_of(rpitit);
        // The impl's type must be one of the impl method's own `impl Trait`s; another item's
        // opaque type (a type alias's) is a refinement too.
        let opaque = match hidden.kind() {
            TyKind::Alias(_, alias)
                if let AliasTyKind::Opaque { def_id } = alias.kind
                    && let ImplTraitId::ReturnTypeImplTrait(owner, index) = def_id.0.loc(db)
                    && owner == impl_method =>
            {
                (def_id.0, index, alias.args)
            }
            _ => {
                return refined(RefinementKind::NotImplTrait {
                    rpitit: rpitit.idx.into_raw().into_u32(),
                    hidden,
                });
            }
        };
        let args = trait_args(rpitit);
        trait_bounds.extend(
            rustc_type_ir::Interner::item_bounds(interner, SolverDefId::RpititId(rpitit))
                .iter_instantiated(interner, args.as_slice())
                .map(|it| it.skip_norm_wip()),
        );
        let (id, index, opaque_args) = opaque;
        for written in id.predicates(db).iter_instantiated_copied(interner, opaque_args.as_slice())
        {
            let written: Clause<'db> = written.skip_norm_wip();
            impl_bounds.extend(
                rustc_type_ir::elaborate::elaborate(interner, [written])
                    .map(|clause| (index.into_raw().into_u32(), written, clause)),
            );
        }
        pairs.push((rpitit, args, id, opaque_args));
    }

    // Normalize both: the trait's `impl Trait`s become the impl's, and any other projection
    // its value (rustc's `refine-normalize.rs`, `async-associated-types.rs`).
    // rustc's hybrid environment: the impl's where clauses and the trait method's own for the
    // impl, normalized (`check_refining_return_position_impl_trait_in_trait`).
    let trait_to_impl_args = GenericArgs::identity_for_item(interner, impl_method.into())
        .rebase_onto(interner, SolverDefId::ImplId(impl_), trait_ref.args);
    let param_env = hir_ty::hybrid_param_env(interner, impl_, trait_method, trait_to_impl_args);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let mut ocx = ObligationCtxt::new(&infcx);
    let cause = ObligationCause::dummy();
    let mut normalize = |clause: Clause<'db>| ocx.deeply_normalize(&cause, param_env, clause).ok();
    // alloc: per impl method returning `impl Trait`, as `trait_bounds`
    let trait_bounds: Option<Vec<Clause<'db>>> =
        trait_bounds.into_iter().map(&mut normalize).collect();
    // alloc: per impl method returning `impl Trait`, as `impl_bounds`
    let impl_bounds: Option<Vec<(u32, Clause<'db>, Clause<'db>)>> = impl_bounds
        .into_iter()
        .map(|(opaque, written, clause)| Some((opaque, written, normalize(clause)?)))
        .collect();
    // rustc's delayed bugs: what fails to normalize or to resolve is an error reported of its
    // own, so nothing is said of it here.
    let (Some(trait_bounds), Some(impl_bounds)) = (trait_bounds, impl_bounds) else { return None };
    if !ocx.evaluate_obligations_error_on_ambiguity().is_empty() {
        return None;
    }
    let resolve = |clause: Clause<'db>| {
        let clause = infcx.deeply_resolve_via_unification_table(clause);
        (!clause.has_infer() && !clause.references_error()).then(|| {
            // Compared as written: bound variables anonymous, as rustc's `Anonymize` (their
            // names are the places they are written), and aliases with no rigidness marked.
            let clause = clause.fold_with(&mut Anonymize { interner });
            rustc_type_ir::set_aliases_to_non_rigid(interner, clause).skip_normalization()
        })
    };
    // alloc: per impl method returning `impl Trait`, as `trait_bounds`
    let trait_keys: Option<Vec<Clause<'db>>> = trait_bounds.into_iter().map(resolve).collect();
    let trait_keys = trait_keys?;
    for (opaque, written, clause) in impl_bounds {
        let key = resolve(clause)?;
        if !trait_keys.contains(&key) {
            let clause = infcx.deeply_resolve_via_unification_table(clause);
            return refined(RefinementKind::StrongerBound { opaque, written, clause });
        }
    }

    // Each impl `impl Trait` captures at least the parameters its trait's does: one that
    // captures fewer promises its type outlives more.
    let mut undercaptures = Vec::new(); // alloc: empty unless the impl captures fewer
    for (rpitit, args, opaque, opaque_args) in pairs {
        let impl_variances = rustc_type_ir::Interner::variances_of(interner, opaque.into());
        let invariant = |args: GenericArgs<'db>,
                         variances: hir_ty::next_solver::VariancesOf<'db>| {
            args.iter()
                .zip(variances.iter())
                .filter(|(_, variance)| *variance == Variance::Invariant)
                .map(|(arg, _)| arg)
        };
        let trait_variances =
            rustc_type_ir::Interner::variances_of(interner, SolverDefId::RpititId(rpitit));
        let mut trait_captures = CollectParams { params: Vec::new() }; // alloc: as `undercaptures`, a few parameters
        for arg in invariant(args, trait_variances) {
            arg.visit_with(&mut trait_captures);
        }
        let mut trait_captures = trait_captures.params;
        if trait_captures
            .iter()
            .all(|arg| invariant(opaque_args, impl_variances).any(|it| it == *arg))
        {
            continue;
        }
        trait_captures.sort_by_key(|arg| !matches!(arg.kind(), GenericArgKind::Lifetime(_)));
        let ImplTraitId::ReturnTypeImplTrait(_, index) = opaque.loc(db) else {
            unreachable!("the impl's `impl Trait`s are of its method's return type")
        };
        undercaptures.push(Undercapture {
            opaque: index.into_raw().into_u32(),
            trait_captures: GenericArgs::new_from_slice(&trait_captures),
        });
    }
    if undercaptures.is_empty() {
        return None;
    }
    refined(RefinementKind::FewerCaptures(undercaptures.into_boxed_slice()))
}

/// The trait method's return type for the impl's method, for the suggestion: rustc's
/// `trait_m_sig_with_self_for_diag` with its `RemapLateParam`, and for two `async fn`s the
/// output of the future.
fn trait_return<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    impl_method: FunctionId,
    trait_method: FunctionId,
    impl_trait_args: GenericArgs<'db>,
) -> Ty<'db> {
    let ItemContainerId::ImplId(impl_) = impl_method.lookup(db).container else {
        unreachable!("a method implementing a trait's is an impl's")
    };
    let ItemContainerId::TraitId(trait_) = trait_method.lookup(db).container else {
        unreachable!("`implemented_trait_method` is a trait's method")
    };
    let self_param = GenericArgs::identity_for_item(interner, trait_.into()).type_at(0);
    let with_self = GenericArgs::new_from_iter(
        std::iter::once(GenericArg::from(self_param)).chain(impl_trait_args.iter().skip(1)),
    );
    let args = GenericArgs::identity_for_item(interner, impl_method.into()).rebase_onto(
        interner,
        impl_.into(),
        with_self,
    );
    let sig = db
        .callable_item_signature(CallableDefId::FunctionId(trait_method))
        .instantiate(interner, args)
        .skip_norm_wip();
    let output = interner.liberate_late_bound_regions(impl_method.into(), sig).output();
    // The trait's late-bound lifetimes as the impl method names them at the same places, in
    // the solver's signature of it (one late-bound lifetime for each its parameters leave out).
    let impl_vars =
        rustc_type_ir::Interner::fn_sig(interner, CallableDefId::FunctionId(impl_method).into())
            .instantiate_identity()
            .skip_norm_wip()
            .bound_vars();
    let output = rustc_type_ir::fold_regions(interner, output, |region, _| match region.kind() {
        RegionKind::ReLateParam(late)
            if late.scope == SolverDefId::FunctionId(impl_method)
                && let Some(BoundVariableKind::Region(kind)) =
                    impl_vars.iter().nth(late.kind.var.as_usize()) =>
        {
            Region::new_late_param(interner, late.scope, BoundRegion { var: late.kind.var, kind })
        }
        _ => region,
    });
    if !(FunctionSignature::of(db, impl_method).is_async()
        && FunctionSignature::of(db, trait_method).is_async())
    {
        return output;
    }
    // Both write the future's output type: rustc's `Future` projection bound of the AFIT.
    let Some((rpitit, args)) = rpitit::as_rpitit(output) else { return output };
    rustc_type_ir::Interner::item_bounds(interner, SolverDefId::RpititId(rpitit))
        .iter_instantiated(interner, args.as_slice())
        .find_map(|clause| match clause.skip_norm_wip().kind().no_bound_vars()? {
            ClauseKind::Projection(projection) => projection.term.as_type(),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "the future `{}` returns as an `async fn` of a trait has no `Output` bound",
                FunctionSignature::of(db, trait_method).name.as_str()
            )
        })
}

/// rustc's `Anonymize`: each binder's variables anonymous.
struct Anonymize<'db> {
    interner: DbInterner<'db>,
}

impl<'db> TypeFolder<DbInterner<'db>> for Anonymize<'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_binder<T: TypeFoldable<DbInterner<'db>>>(
        &mut self,
        binder: Binder<'db, T>,
    ) -> Binder<'db, T> {
        rustc_type_ir::Interner::anonymize_bound_vars(self.interner, binder)
    }
}

/// rustc's `CollectParams`: each type, lifetime and const parameter in what it visits, once.
struct CollectParams<'db> {
    params: Vec<GenericArg<'db>>,
}

impl<'db> CollectParams<'db> {
    fn insert(&mut self, arg: GenericArg<'db>) {
        if !self.params.contains(&arg) {
            self.params.push(arg);
        }
    }
}

impl<'db> TypeVisitor<DbInterner<'db>> for CollectParams<'db> {
    type Result = ();

    fn visit_ty(&mut self, ty: Ty<'db>) {
        match ty.kind() {
            TyKind::Param(_) => self.insert(ty.into()),
            _ => ty.super_visit_with(self),
        }
    }

    fn visit_region(&mut self, region: Region<'db>) {
        if let RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) = region.kind() {
            self.insert(region.into());
        }
    }

    fn visit_const(&mut self, ct: hir_ty::next_solver::Const<'db>) {
        match ct.kind() {
            rustc_type_ir::ConstKind::Param(_) => self.insert(ct.into()),
            _ => ct.super_visit_with(self),
        }
    }
}
