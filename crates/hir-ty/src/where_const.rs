//! `where const { COND }` (`#![feature(blaze_comptime)]`, docs/v2/plan/design/ownership.md): a
//! where predicate that is a `bool` constant over the generic parameters.
//!
//! It lowers to `ClauseKind::ConstEvaluatable(c)`, `c` the anonymous constant of the block with
//! the item's own generic arguments (nothing else in blaze lowers to that clause). The clause
//! holds when `c`, with the arguments it is asked with, evaluates to `true`:
//!
//! - `true`: holds;
//! - `false`, or an evaluation that fails: does not hold. A panic's message is the reason
//!   ([`Failure::Refused`]), which the error of the bound shows: a condition explains itself by
//!   panicking with the path it found (`bstd::own::none`);
//! - arguments that still name a generic parameter: does not hold, unless the environment has
//!   the same condition (a caller's own `where const`), by identity or by its normal form
//!   ([`crate::const_exprs`]). Generic code states the conditions it relies on, as it states
//!   bounds;
//! - arguments with an inference variable: not decided yet.
//!
//! Evaluation is [`crate::consteval::anon_const_eval`], keyed by the constant and its
//! arguments, so an instance is evaluated once. A condition's own evaluation may ask trait
//! goals (reflection's `type_id_vtable`); one that asks the bound it is the condition of is a
//! query cycle, which is an evaluation that fails.

use base_db::Db;
use hir_def::{ImplId, hir::generics::GenericParams};
use rustc_type_ir::{
    ClauseKind, TypeVisitableExt, TypingMode,
    inherent::{GenericArgs as _, IntoKind},
};

use crate::{
    Span,
    consteval::ConstEvalError,
    db::HirDatabase,
    method_resolution::TraitImpls,
    mir::MirEvalError,
    next_solver::{
        AliasConst, AliasConstKind, Const, ConstKind, DbInterner, FreeConstAliasId,
        InherentAssocConstId, ParamEnv, TraitAssocConstId, TraitRef, default_types,
        infer::{DbInternerInferExt, traits::ObligationCause},
    },
};

/// Whether a condition holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    Holds,
    Fails(Failure),
    /// It has an inference variable: asked again once that is known.
    Undecided,
}

/// Why a condition does not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// It evaluated to `false`.
    False,
    /// Its evaluation panicked: the message, which is the condition's own reason.
    Refused(String),
    /// Its evaluation failed some other way, or it is an error already reported.
    Errored,
    /// It names a generic parameter and the environment does not have it.
    Generic,
}

impl Failure {
    /// The reason, as a note says it.
    pub fn text(&self) -> String {
        // alloc: error path, each arm
        match self {
            Failure::False => "its `where const` condition is `false`".to_owned(),
            Failure::Refused(reason) => reason.clone(), // clone: error path
            Failure::Errored => "its `where const` condition could not be evaluated".to_owned(),
            Failure::Generic => "its `where const` condition depends on a generic parameter; \
                                 state the same condition on this item"
                .to_owned(),
        }
    }
}

/// Whether the condition `ct` holds in `param_env`.
pub fn condition<'db>(
    interner: DbInterner<'db>,
    param_env: ParamEnv<'db>,
    ct: Const<'db>,
) -> Condition {
    let db = interner.db;
    match ct.kind() {
        ConstKind::Infer(_) => Condition::Undecided,
        ConstKind::Value(value) => {
            if value.ty != default_types().types.bool {
                return Condition::Fails(Failure::Errored);
            }
            match value.value.inner().to_leaf().to_uint_unchecked() {
                0 => Condition::Fails(Failure::False),
                _ => Condition::Holds,
            }
        }
        ConstKind::Alias(_, alias) => {
            if alias.args.has_non_region_infer() {
                return Condition::Undecided;
            }
            if assumed(db, param_env, ct) {
                return Condition::Holds;
            }
            if alias.args.has_non_region_param() || alias.args.has_non_region_placeholders() {
                return Condition::Fails(Failure::Generic);
            }
            match evaluate(db, alias) {
                Ok(true) => Condition::Holds,
                Ok(false) => Condition::Fails(Failure::False),
                Err(error) => Condition::Fails(match panic_of(&error) {
                    Some(reason) => Failure::Refused(reason.to_owned()), // alloc: error path
                    None => Failure::Errored,
                }),
            }
        }
        ConstKind::Param(_) | ConstKind::Placeholder(_) => match assumed(db, param_env, ct) {
            true => Condition::Holds,
            false => Condition::Fails(Failure::Generic),
        },
        // An error constant is a condition whose evaluation failed where it was normalized, or
        // one that did not lower: refused, never taken to hold.
        ConstKind::Error(_) => Condition::Fails(Failure::Errored),
        ConstKind::Bound(..) | ConstKind::Expr(_) => Condition::Fails(Failure::Generic),
    }
}

/// Whether `param_env` has the condition `ct`: the same constant, or one with the same normal
/// form.
fn assumed<'db>(db: &'db Db, param_env: ParamEnv<'db>, ct: Const<'db>) -> bool {
    param_env.clauses.iter().any(|clause| match clause.kind().skip_binder() {
        ClauseKind::ConstEvaluatable(assumed) => {
            assumed == ct || crate::const_exprs::same_expression(db, assumed, ct)
        }
        _ => false,
    })
}

/// The `bool` the constant `alias`, whose arguments are concrete, evaluates to.
fn evaluate<'db>(db: &'db Db, alias: AliasConst<'db>) -> Result<bool, ConstEvalError<'db>> {
    let allocation = match alias.kind {
        AliasConstKind::Projection { def_id: TraitAssocConstId(id) }
        | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) }
        | AliasConstKind::Free { def_id: FreeConstAliasId(id) } => {
            db.const_eval(id, alias.args, None)?
        }
        AliasConstKind::Anon { def_id } => db.anon_const_eval(def_id.0, alias.args, None)?,
        AliasConstKind::InherentSelf { .. } => return Err(ConstEvalError::MirEvalError(
            MirEvalError::NotSupported(
                "a `where const` condition that is an inherent constant in `Self` form".to_owned(), // alloc: error path
            ),
        )),
    };
    Ok(allocation.memory.first().is_some_and(|&byte| byte != 0))
}

/// The message of the panic an evaluation stopped with, through the functions and constants it
/// was in.
pub fn panic_of<'a>(error: &'a ConstEvalError<'_>) -> Option<&'a str> {
    let ConstEvalError::MirEvalError(first) = error else { return None };
    let mut inner = first;
    loop {
        match inner {
            MirEvalError::InFunction(it, _) => inner = &**it,
            MirEvalError::ConstEvalError(_, it) => return panic_of(it),
            MirEvalError::Panic(message) => return Some(message),
            _ => return None,
        }
    }
}

/// Why the bound `trait_ref` does not hold through an impl with a `where const` condition:
/// the first such impl of the trait whose header matches, its conditions asked with the
/// arguments the match gives, and the first that fails. `None` when no impl with a condition
/// matches or every condition of one holds. For the error of the bound only.
pub fn bound_refusal<'db>(interner: DbInterner<'db>, trait_ref: TraitRef<'db>) -> Option<Failure> {
    let db = interner.db;
    let trait_ = trait_ref.def_id.0;
    let impls = TraitImpls::for_crate(db, hir_def::HasModule::krate(&trait_, db));
    impls.blanket_impls(trait_).iter().find_map(|&impl_| impl_refusal(interner, impl_, trait_ref))
}

fn impl_refusal<'db>(
    interner: DbInterner<'db>,
    impl_: ImplId,
    trait_ref: TraitRef<'db>,
) -> Option<Failure> {
    let db = interner.db;
    if GenericParams::of(db, impl_.into()).where_consts().is_empty() {
        return None;
    }
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let args = infcx.fresh_args_for_item(Span::Dummy, impl_.into());
    let header = db.impl_trait(impl_)?.instantiate(interner, args).skip_norm_wip();
    infcx.at(&ObligationCause::dummy(), ParamEnv::empty()).eq(header, trait_ref).ok()?;
    let args = infcx.deeply_resolve_ignoring_regions(args);
    crate::lower::GenericPredicates::query_all(db, impl_.into())
        .iter_instantiated(interner, args.as_slice())
        .map(crate::next_solver::Unnormalized::skip_norm_wip)
        .find_map(|clause| match clause.kind().skip_binder() {
            ClauseKind::ConstEvaluatable(ct) => match condition(interner, ParamEnv::empty(), ct) {
                Condition::Fails(failure) => Some(failure),
                Condition::Holds | Condition::Undecided => None,
            },
            _ => None,
        })
}

/// Why the condition `ct` does not hold, for the error of a goal that is one: `None` when it
/// holds or is not decided.
pub fn refusal<'db>(interner: DbInterner<'db>, ct: Const<'db>) -> Option<Failure> {
    match condition(interner, ParamEnv::empty(), ct) {
        Condition::Fails(failure) => Some(failure),
        Condition::Holds | Condition::Undecided => None,
    }
}
