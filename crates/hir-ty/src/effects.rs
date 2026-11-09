//! Const traits (`const_trait_impl`): which items are const or conditionally const, what a use of
//! one in a const context has to prove (its `[const]` bounds), and the calls of a const body that
//! are not const.
//!
//! Type checking does not prove effects: neither `trait_environment` nor `predicates_of` carry
//! `HostEffect` clauses. Only [`const_call_error`] (and so [`non_const_calls`]) does, in an
//! environment of its own, so non-const code pays nothing for this.

use base_db::{Crate, Db};
use either::Either;
use hir_def::{
    AdtId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, GenericDefId, HasModule, ImplId,
    ItemContainerId, Lookup, TraitId,
    expr_store::{Body, ExpressionStore},
    hir::{Expr, ExprId},
    signatures::{FnFlags, FunctionSignature, ImplSignature, TraitFlags, TraitSignature},
    unstable_features::UnstableFeatures,
};
use rustc_type_ir::{BoundConstness, ClauseKind, TypeVisitableExt, TypingMode, inherent::IntoKind};
use syntax::view as ast;

use crate::{
    InferBodyId, InferenceResult, ParamEnvAndCrate,
    db::{AnonConstId, HirDatabase, InternedClosure, InternedClosureId},
    next_solver::{
        AnyImplId, Binder, Clause, Clauses, DbInterner, EarlyBinder, GenericArgs, ParamEnv,
        StoredClauses, TraitRef, TyKind, Unnormalized,
        infer::{
            DbInternerInferExt,
            traits::{Obligation, ObligationCause},
        },
    },
};

pub fn trait_is_const(db: &Db, trait_: TraitId) -> bool {
    TraitSignature::of(db, trait_).flags.contains(TraitFlags::CONST)
}

/// Whether a trait impl is a `const impl` of a `const trait`, or an inherent impl is a
/// `const impl`.
pub fn impl_is_const(db: &Db, impl_: AnyImplId) -> bool {
    match impl_ {
        AnyImplId::ImplId(impl_) => {
            ImplSignature::of(db, impl_).is_const()
                && db
                    .impl_trait(impl_)
                    .is_none_or(|trait_ref| trait_is_const(db, trait_ref.skip_binder().def_id.0))
        }
        AnyImplId::BuiltinDeriveImplId(impl_) => {
            let loc = impl_.loc(db);
            let interner = DbInterner::new_with(db, loc.module(db).krate(db));
            loc.is_const(db)
                && loc.trait_.get_id(interner.lang_items()).is_some_and(|it| trait_is_const(db, it))
        }
    }
}

/// Whether `func` is (conditionally) const: a `const fn`, or a method of a `const trait` or of a
/// `const impl`.
pub fn fn_is_const(db: &Db, func: FunctionId) -> bool {
    match func.lookup(db).container {
        ItemContainerId::TraitId(trait_) => trait_is_const(db, trait_),
        ItemContainerId::ImplId(impl_) if db.impl_trait(impl_).is_some() => {
            impl_is_const(db, impl_.into())
        }
        ItemContainerId::ImplId(impl_) if ImplSignature::of(db, impl_).is_const() => true,
        _ => FunctionSignature::of(db, func).flags.contains(FnFlags::CONST),
    }
}

/// Whether the closure is a `const` closure.
pub fn closure_is_const(db: &Db, closure: InternedClosureId<'_>) -> bool {
    let InternedClosure { owner, expr, .. } = closure.loc(db);
    closure_expr_is_const(db, owner.expression_store_owner(db), expr)
}

fn closure_expr_is_const(db: &Db, owner: ExpressionStoreOwnerId, expr: ExprId) -> bool {
    let (_, source_map) = ExpressionStore::with_source_map(db, owner);
    let Ok(source) = source_map.expr_syntax(expr) else { return false };
    // The span of an expression in parentheses is the parentheses' (as rustc's).
    let Either::Left(mut syntax) = source.value.to_view(source.file_id.root(db)) else {
        return false;
    };
    while let ast::Expr::ParenExpr(paren) = &syntax {
        let Some(inner) = paren.expr() else { return false };
        syntax = inner;
    }
    matches!(syntax, ast::Expr::ClosureExpr(closure) if closure.const_token().is_some())
}

/// Whether the inline const `expr` of `owner` is a `const static { .. }` block
/// (`#![feature(blaze_comptime)]`, docs/v2/plan/design/comptime.md, "Phase 2b"): a constant of
/// its own, evaluated with no const checking. Read off the syntax, as a closure's `const` is, and
/// only in a crate with the feature, whose lowering is the only one that keeps such a block.
pub fn const_block_is_comptime(db: &Db, owner: ExpressionStoreOwnerId, expr: ExprId) -> bool {
    // `of_root`, not `query`: a const macro's evaluation may run while a def map is built.
    if !UnstableFeatures::of_root(db, owner.module(db).krate(db)).blaze_comptime {
        return false;
    }
    if !matches!(ExpressionStore::of(db, owner).expr(expr), Expr::Const(_)) {
        return false;
    }
    let (_, source_map) = ExpressionStore::with_source_map(db, owner);
    let Ok(source) = source_map.expr_syntax(expr) else { return false };
    let Either::Left(syntax) = source.value.to_view(source.file_id.root(db)) else {
        return false;
    };
    matches!(syntax.peel_parens(), ast::Expr::BlockExpr(block) if block.is_comptime())
}

/// Whether `konst` is a `const static { .. }` block ([`const_block_is_comptime`]): an inline
/// const of a body, or the whole of an array length or const argument written as one.
pub fn anon_const_is_comptime(db: &Db, konst: AnonConstId<'_>) -> bool {
    let loc = konst.loc(db);
    const_block_is_comptime(db, loc.owner, loc.expr)
}

/// Whether a call in a const context of `krate` through a function pointer or a `dyn` method
/// ([`is_dyn_dispatch`]) is checked when it is evaluated, where its target is known, rather
/// than refused: the crate has `#![feature(blaze_comptime)]`. The one rule of both checkers,
/// hir-ty-mir's `const_check` and [`non_const_calls`].
pub fn defers_dispatch_check(db: &Db, krate: Crate) -> bool {
    // `of_root`, not `query`: a const macro's evaluation may run while a def map is built.
    UnstableFeatures::of_root(db, krate).blaze_comptime
}

/// Whether the call of `callee` with `args` goes through a vtable as the evaluator decides it
/// (hir-ty-mir's `exec_fn_trait` and `get_mir_or_dyn_index`), so that every call deferred here
/// is one the evaluator checks: `Fn::call`, `FnMut::call_mut` or `FnOnce::call_once` whose
/// `Self`, behind references, is `dyn Trait` (the evaluator reads the vtable behind them), or
/// another trait's method whose `Self` is a `dyn` type with the trait among its principal's
/// supertraits ([`crate::method_resolution::is_dyn_method`]). A method of an impl for a `dyn`
/// type, or for a reference to one, is a direct call and is checked statically as any other.
pub fn is_dyn_dispatch<'db>(
    db: &'db Db,
    krate: Crate,
    callee: FunctionId,
    args: GenericArgs<'db>,
) -> bool {
    if !matches!(callee.lookup(db).container, ItemContainerId::TraitId(_)) {
        return false;
    }
    let interner = DbInterner::new_with(db, krate);
    let lang = interner.lang_items();
    if [lang.Fn_call, lang.FnMut_call_mut, lang.FnOnce_call_once].contains(&Some(callee)) {
        let Some(mut ty) = args.as_slice().first().and_then(|it| it.ty()) else { return false };
        while let TyKind::Ref(_, inner, _) = ty.kind() {
            ty = inner;
        }
        return matches!(ty.kind(), TyKind::Dynamic(..));
    }
    crate::method_resolution::is_dyn_method(interner, ParamEnv::empty(), callee, args).is_some()
}

/// Whether evaluating `owner`'s body checks the target of each call it makes through a
/// function pointer or a vtable: the body is a const context of a crate whose check of such a
/// call is deferred ([`defers_dispatch_check`]), so the static check let the call through.
/// The evaluator asks it only at compile time and outside a `const static` block.
pub fn dispatch_checked_at_eval(db: &Db, owner: InferBodyId<'_>) -> bool {
    if !defers_dispatch_check(db, owner.module(db).krate(db)) {
        return false;
    }
    match owner {
        InferBodyId::DefWithBodyId(def) => body_constness(db, def).is_some(),
        InferBodyId::AnonConstId(_) => true,
    }
}

/// rustc's `is_conditionally_const`: whether `def` can be used in a const context, given that its
/// [`crate::lower::const_conditions`] hold const.
pub(crate) fn is_conditionally_const(db: &Db, def: GenericDefId) -> bool {
    match def {
        GenericDefId::FunctionId(func) => fn_is_const(db, func),
        GenericDefId::ImplId(impl_) => impl_is_const(db, impl_.into()),
        GenericDefId::TraitId(trait_) => trait_is_const(db, trait_),
        GenericDefId::TypeAliasId(alias) => match alias.lookup(db).container {
            ItemContainerId::TraitId(trait_) => trait_is_const(db, trait_),
            ItemContainerId::ImplId(impl_) => impl_is_const(db, impl_.into()),
            _ => false,
        },
        _ => false,
    }
}

/// The trait refs of `HostEffect` clauses.
pub(crate) fn effect_trait_refs<'db>(
    clauses: impl IntoIterator<Item = Clause<'db>>,
) -> impl Iterator<Item = Binder<'db, TraitRef<'db>>> {
    clauses.into_iter().filter_map(|clause| {
        let kind = clause.kind();
        match kind.skip_binder() {
            ClauseKind::HostEffect(pred) => Some(kind.rebind(pred.trait_ref)),
            _ => None,
        }
    })
}

/// The `[const]` conditions of a `#[derive_const]` impl: `T: [const] Trait` for each of the
/// type's parameters, as rustc's derive writes them.
pub(crate) fn builtin_derive_const_conditions<'db>(
    db: &'db Db,
    impl_: hir_def::BuiltinDeriveImplId,
) -> Vec<Binder<'db, TraitRef<'db>>> {
    if !impl_is_const(db, impl_.into()) {
        return Vec::new(); // alloc: empty
    }
    let loc = impl_.loc(db);
    let interner = DbInterner::new_with(db, loc.module(db).krate(db));
    // alloc: empty
    let Some(trait_) = loc.trait_.get_id(interner.lang_items()) else { return Vec::new() };
    crate::builtin_derive::predicates(db, impl_)
        .all_predicates()
        .iter_identity()
        .map(Unnormalized::skip_norm_wip)
        .filter_map(|clause| {
            let kind = clause.kind();
            match kind.skip_binder() {
                ClauseKind::Trait(pred) if pred.trait_ref.def_id.0 == trait_ => {
                    Some(kind.rebind(pred.trait_ref))
                }
                _ => None,
            }
        })
        // alloc: only `#[derive_const]` impls, a nightly feature; the solver takes a `Vec`
        .collect()
}

/// How const a body is: `Const` for constants and statics, `Maybe` for const fns (const exactly
/// when their `[const]` bounds are), `None` when it is not const.
pub fn body_constness(db: &Db, def: DefWithBodyId) -> Option<BoundConstness> {
    match def {
        DefWithBodyId::FunctionId(func) => fn_is_const(db, func).then_some(BoundConstness::Maybe),
        DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_) => {
            Some(BoundConstness::Const)
        }
    }
}

/// The environment const calls in `def` are proven in: its where-clauses, and its `[const]`
/// bounds as `HostEffect(Maybe)` clauses.
pub fn const_param_env<'db>(db: &'db Db, def: GenericDefId) -> ParamEnvAndCrate<'db> {
    let krate = def.module(db).krate(db);
    let clauses = const_param_env_query(db, def).as_ref();
    ParamEnvAndCrate { param_env: ParamEnv { clauses }, krate }
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn const_param_env_query(db: &Db, def: GenericDefId) -> StoredClauses {
    let interner = DbInterner::new_with(db, def.module(db).krate(db));
    let env = crate::lower::trait_environment(db, def);
    let conditions = crate::lower::const_conditions(db, def).skip_binder();
    let clauses = rustc_type_ir::elaborate::elaborate(
        interner,
        env.clauses.iter().chain(conditions.iter().copied()),
    );
    Clauses::new_from_iter(clauses).store()
}

/// Why a call is not const.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NonConst<'db> {
    /// The callee is not a const fn, nor a method of a const trait or const impl.
    Fn(FunctionId),
    /// A call through a function pointer: the nightly refuses these in const. With
    /// `#![feature(blaze_comptime)]` it is checked at evaluation instead
    /// ([`defers_dispatch_check`]).
    FnPointer,
    /// The called closure is not a `const` closure.
    Closure,
    /// These `[const]` bounds of the callee do not hold const.
    Unsatisfied(FunctionId, Vec<Binder<'db, TraitRef<'db>>>),
}

/// Why calling `callee` with `args` in a body of `constness` with environment `env` (see
/// [`const_param_env`]) is not const, or `None` when it is.
pub fn const_call_error<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    constness: BoundConstness,
    callee: FunctionId,
    args: GenericArgs<'db>,
) -> Option<NonConst<'db>> {
    let interner = DbInterner::new_with(db, env.krate);
    let lang_items = interner.lang_items();
    let fn_trait = match callee.lookup(db).container {
        ItemContainerId::TraitId(it)
            if [lang_items.Fn, lang_items.FnMut, lang_items.FnOnce].contains(&Some(it)) =>
        {
            Some(it)
        }
        _ => None,
    };
    let fn_trait_condition;
    let conditions = match fn_trait {
        // `Fn`, `FnMut` and `FnOnce` are const traits in core (fixtures' minicore declares them
        // plain): calling through one is const when the callee implements it const.
        Some(fn_trait) => {
            fn_trait_condition = [Binder::dummy(TraitRef::identity(interner, fn_trait.into()))
                .to_host_effect_clause(interner, BoundConstness::Maybe)];
            EarlyBinder::bind_iter(&fn_trait_condition[..])
        }
        None if !fn_is_const(db, callee) => return Some(NonConst::Fn(callee)),
        None => crate::lower::const_conditions(db, callee.into()),
    };
    if conditions.skip_binder().is_empty() {
        return None;
    }
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let unsatisfied: Vec<_> = effect_trait_refs(
        conditions
            .iter_instantiated_copied(interner, args.as_slice())
            .map(Unnormalized::skip_norm_wip),
    )
    .filter(|trait_ref| !trait_ref.references_error())
    .filter(|trait_ref| {
        let clause = trait_ref.to_host_effect_clause(interner, constness);
        let obligation = Obligation::new(
            interner,
            ObligationCause::dummy(),
            env.param_env,
            clause.as_predicate(),
        );
        !infcx.predicate_must_hold_modulo_regions(&obligation)
    })
    .collect(); // alloc: error path, empty (no allocation) unless a bound does not hold
    (!unsatisfied.is_empty()).then_some(NonConst::Unsatisfied(callee, unsatisfied))
}

/// The calls in `def`'s body that are not const, when the body is const. Bodies of closures that
/// are not `const` are skipped (making such a closure is fine, calling it is not), and so are
/// `const static { .. }` blocks, which are not const checked. With `#![feature(blaze_comptime)]`
/// a call through a function pointer or a `dyn` method is not one: it is checked at evaluation.
pub fn non_const_calls<'db>(db: &'db Db, def: DefWithBodyId) -> Vec<(ExprId, NonConst<'db>)> {
    let Some(constness) = body_constness(db, def) else { return Vec::new() }; // alloc: empty
    let mut walk = Walk {
        db,
        owner: def.into(),
        body: Body::of(db, def),
        infer: InferenceResult::of(db, def),
        env: const_param_env(db, def.generic_def(db)),
        constness,
        defers_dispatch: defers_dispatch_check(db, def.module(db).krate(db)),
        found: Vec::new(), // alloc: error path, pushed only for a call that is not const
    };
    walk.expr(walk.body.root_expr());
    walk.found
}

struct Walk<'a, 'db> {
    db: &'db Db,
    owner: ExpressionStoreOwnerId,
    body: &'db Body,
    infer: &'a InferenceResult<'db>,
    env: ParamEnvAndCrate<'db>,
    constness: BoundConstness,
    /// [`defers_dispatch_check`] of the body's crate.
    defers_dispatch: bool,
    found: Vec<(ExprId, NonConst<'db>)>,
}

impl Walk<'_, '_> {
    fn expr(&mut self, expr: ExprId) {
        match self.body.expr(expr) {
            Expr::Closure { body, .. } => {
                if closure_expr_is_const(self.db, self.owner, expr) {
                    self.expr(body);
                }
                return;
            }
            Expr::Const(_) if const_block_is_comptime(self.db, self.owner, expr) => return,
            Expr::Call { callee, .. } => self.call(expr, Some(callee)),
            Expr::MethodCall { .. }
            | Expr::BinaryOp { .. }
            | Expr::UnaryOp { .. }
            | Expr::Index { .. } => self.call(expr, None),
            _ => {}
        }
        let body = self.body;
        body.walk_child_exprs_without_pats(expr, |child| self.expr(child));
    }

    fn call(&mut self, expr: ExprId, callee: Option<ExprId>) {
        let callee_ty = callee.map(|callee| self.infer.expr_ty(callee));
        let error = match callee_ty.map(|ty| ty.kind()) {
            Some(TyKind::FnPtr(..)) if self.defers_dispatch => None,
            Some(TyKind::FnPtr(..)) => Some(NonConst::FnPointer),
            Some(TyKind::Closure(closure, _)) if !closure_is_const(self.db, closure.0) => {
                Some(NonConst::Closure)
            }
            kind => match (self.infer.method_resolution(expr), kind) {
                (Some((func, args)), _)
                    if self.defers_dispatch
                        && is_dyn_dispatch(self.db, self.env.krate, func, args) =>
                {
                    None
                }
                (Some((func, args)), _) => {
                    const_call_error(self.db, self.env, self.constness, func, args)
                }
                (None, Some(TyKind::FnDef(def, args))) => match def.0 {
                    crate::CallableDefId::FunctionId(func) => {
                        let args = args.no_bound_vars().unwrap();
                        if self.defers_dispatch
                            && is_dyn_dispatch(self.db, self.env.krate, func, args)
                        {
                            None
                        } else {
                            const_call_error(self.db, self.env, self.constness, func, args)
                        }
                    }
                    crate::CallableDefId::StructId(_) | crate::CallableDefId::EnumVariantId(_) => {
                        None
                    }
                },
                (None, _) => None,
            },
        };
        self.found.extend(error.map(|error| (expr, error)));
    }
}

/// Whether an ADT's `Drop` impl is a `const impl`.
pub(crate) fn destructor_is_const(db: &Db, adt: AdtId) -> Option<bool> {
    crate::drop::destructor(db, adt).map(|impl_: ImplId| impl_is_const(db, impl_.into()))
}
