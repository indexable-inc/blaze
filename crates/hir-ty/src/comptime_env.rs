//! The environment the goals of a `for const` body hold in, for every pass after inference
//! (`#![feature(blaze_comptime)]`, docs/v2/plan/design/comptime.md, "Type checking once").
//!
//! Inference checks a loop's body once, its binder `F` a placeholder of a universe made for
//! that loop, with the body's environment extended by what the loop assumes of `F`
//! ([`InferenceResult::comptime_param_env`]). A pass that solves goals over the body's types
//! again (borrowck's `Copy`, drop glue, the optimizer's normalization, unsafeck, lints) gets the
//! same answer only in that environment and with an inference context that can name `F`: one
//! whose universe is at least the binder's. Ask in another and `F: Copy` is false,
//! `<F as Field>::Type` does not normalize to itself (an inference variable of the root universe
//! cannot name `F`), and the pass reports an unsatisfied bound or a use after move inference
//! never saw.
//!
//! Each such pass builds its environment with [`body_env`] (it has the body's inference result)
//! or [`owner_env`] (it has the body's owner), and each inference context that solves in an
//! environment it was handed calls [`enter_universes`]. Without the feature no body has a loop,
//! no environment holds a placeholder, and each of these returns its input with the context
//! untouched: one flags load or one table length.

use std::ops::ControlFlow;

use rustc_type_ir::{
    TypeFlags, TypeSuperVisitable, TypeVisitable, TypeVisitableExt, TypeVisitor, UniverseIndex,
    inherent::IntoKind,
};

use base_db::Db;

use crate::{
    InferBodyId, InferenceResult,
    next_solver::{DbInterner, ParamEnv, Ty, TyKind, infer::InferCtxt},
};

/// The largest universe of a placeholder type in `env`, `ROOT` when it names none. A where
/// clause names parameters, never placeholders, so only an environment
/// [`InferenceResult::comptime_param_env`] extended has one: what its loops assume of their
/// binders.
pub fn placeholder_universe(env: ParamEnv<'_>) -> UniverseIndex {
    let clauses = env.clauses();
    if !clauses.has_type_flags(TypeFlags::HAS_TY_PLACEHOLDER) {
        return UniverseIndex::ROOT;
    }
    let mut max = MaxPlaceholderUniverse(UniverseIndex::ROOT);
    let _ = clauses.visit_with(&mut max);
    max.0
}

/// Gives `infcx` the universes up to every placeholder of `env`, so that its inference
/// variables can name a `for const` binder `env` assumes things of, as inference's could.
/// Called before the context makes a universe of its own (an `enter_forall`), so its own
/// placeholders never share a universe with a binder. Nothing for an `env` with no
/// placeholder.
pub fn enter_universes<'db>(infcx: &InferCtxt<'db>, env: ParamEnv<'db>) {
    raise(infcx, placeholder_universe(env));
}

/// `env` extended by what the `for const` loops of the body `infer` is of assume of their
/// binders, and `infcx` given their universes: the environment a pass over that body solves
/// in, which is the one inference solved in. `env` itself, `infcx` untouched, for a body with
/// no loop.
pub fn body_env<'db>(
    infcx: &InferCtxt<'db>,
    infer: &InferenceResult<'db>,
    env: ParamEnv<'db>,
) -> ParamEnv<'db> {
    if infer.comptime_loops().len() == 0 {
        return env;
    }
    raise(infcx, infer.comptime_max_universe());
    infer.comptime_param_env(env)
}

/// [`body_env`] for the body of `owner` (a closure's MIR is its parent's `owner`, which inferred
/// it; an inline `const { .. }` was inferred with its body), for a pass that has only the
/// owner.
pub fn owner_env<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
) -> ParamEnv<'db> {
    body_env(infcx, InferenceResult::of(db, owner), env)
}

/// [`owner_env`] for a pass that builds no inference context of its own and hands the
/// environment to queries ([`crate::db::HirDatabase::lookup_impl_method`], layout), each of
/// which calls [`enter_universes`] on the context it builds.
pub fn owner_param_env<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    env: ParamEnv<'db>,
) -> ParamEnv<'db> {
    InferenceResult::of(db, owner).comptime_param_env(env)
}

/// Whether `ty` names a `for const` binder: a placeholder type, which only a binder is outside
/// the solver. A pass that cannot be given the body's environment refuses such a type.
pub fn names_binder(ty: Ty<'_>) -> bool {
    ty.has_type_flags(TypeFlags::HAS_TY_PLACEHOLDER)
}

fn raise(infcx: &InferCtxt<'_>, max: UniverseIndex) {
    // Bounded by `max`: each call makes the next universe.
    while infcx.universe() < max {
        infcx.create_next_universe();
    }
}

struct MaxPlaceholderUniverse(UniverseIndex);

impl<'db> TypeVisitor<DbInterner<'db>> for MaxPlaceholderUniverse {
    type Result = ControlFlow<()>;

    fn visit_ty(&mut self, ty: Ty<'db>) -> Self::Result {
        if !ty.has_type_flags(TypeFlags::HAS_TY_PLACEHOLDER) {
            return ControlFlow::Continue(());
        }
        if let TyKind::Placeholder(placeholder) = ty.kind() {
            self.0 = self.0.max(placeholder.universe);
            return ControlFlow::Continue(());
        }
        ty.super_visit_with(self)
    }
}
