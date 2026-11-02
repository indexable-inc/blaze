//! Const traits (`const_trait_impl`): which items are const or conditionally const, what a use of
//! one in a const context has to prove (its `[const]` bounds), and the calls of a const body that
//! are not const.
//!
//! Type checking does not prove effects: neither `trait_environment` nor `predicates_of` carry
//! `HostEffect` clauses. Only [`const_call_error`] (and so [`non_const_calls`]) does, in an
//! environment of its own, so non-const code pays nothing for this.

use base_db::Db;
use either::Either;
use hir_def::{
    AdtId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, GenericDefId, HasModule, ImplId,
    ItemContainerId, Lookup, TraitId,
    expr_store::{Body, ExpressionStore},
    hir::{Expr, ExprId},
    signatures::{FnFlags, FunctionSignature, ImplSignature, TraitFlags, TraitSignature},
};
use rustc_type_ir::{BoundConstness, ClauseKind, TypeVisitableExt, TypingMode, inherent::IntoKind};
use syntax::view as ast;

use crate::{
    InferenceResult, ParamEnvAndCrate,
    db::{HirDatabase, InternedClosure, InternedClosureId},
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

fn closure_expr_is_const(
    db: &Db,
    owner: ExpressionStoreOwnerId,
    expr: ExprId,
) -> bool {
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
    /// A call through a function pointer: the nightly refuses these in const.
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
/// are not `const` are skipped (making such a closure is fine, calling it is not).
pub fn non_const_calls<'db>(
    db: &'db Db,
    def: DefWithBodyId,
) -> Vec<(ExprId, NonConst<'db>)> {
    let Some(constness) = body_constness(db, def) else { return Vec::new() }; // alloc: empty
    let mut walk = Walk {
        db,
        owner: def.into(),
        body: Body::of(db, def),
        infer: InferenceResult::of(db, def),
        env: const_param_env(db, def.generic_def(db)),
        constness,
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
            Some(TyKind::FnPtr(..)) => Some(NonConst::FnPointer),
            Some(TyKind::Closure(closure, _)) if !closure_is_const(self.db, closure.0) => {
                Some(NonConst::Closure)
            }
            kind => match (self.infer.method_resolution(expr), kind) {
                (Some((func, args)), _) => {
                    const_call_error(self.db, self.env, self.constness, func, args)
                }
                (None, Some(TyKind::FnDef(def, args))) => match def.0 {
                    crate::CallableDefId::FunctionId(func) => {
                        let args = args.no_bound_vars().unwrap();
                        const_call_error(self.db, self.env, self.constness, func, args)
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
