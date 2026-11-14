//! `dropped_error`: an error thrown away for a value, the quiet fallback AGENTS.md rule 3 bans.
//! One of, on a `core::result::Result` (resolved: the method is of `Result`'s own impl, the
//! pattern's variant is the lang item `Err`):
//!
//! - a `match` arm `Err(_)`, `Err(..)` or `Err(_name)` whose value is neither `()` nor `!` (an
//!   arm that returns, breaks or panics is no fallback);
//! - `.ok()`, `.unwrap_or(..)`, `.unwrap_or_default()`;
//! - `.unwrap_or_else(|_| ..)` and `.map_err(|_| ..)`: a closure whose one parameter is `_` or
//!   `_name`, which never reads the error.
//!
//! - `let _ = <Result>;`: the whole `Result` dropped, its error with it;
//! - `if let Ok(..) = <Result> { .. }` with no `else`: the `Err` case does nothing and says
//!   nothing (a `while let Ok(..)` loop ends on the error, which is its stop, and is not found);
//! - `.filter_map(Result::ok)` (`.filter_map(|it| it.ok())` is the `.ok()` above): every
//!   error of an iterator of `Result`s dropped;
//! - `.flatten()` of an iterator whose `Item` is a `Result` (resolved: `Iterator`'s own
//!   `flatten`, the item normalized): `Result`'s `IntoIterator` yields nothing for an `Err`, so
//!   each error is skipped as `filter_map(Result::ok)` skips it.
//!
//! A place a clippy port reports when it is on is left to it, so no place is reported twice:
//! `x.ok();` as a statement is `unused_result_ok`'s, `.map_err(|_| ..)` with a non-`move`
//! closure whose parameter is exactly `_` is `map_err_ignore`'s, and `let _ = <Result>;` is
//! `let_underscore_must_use`'s. An `Err` arm or an `.unwrap_or_else(|_| ..)` whose value is
//! another route to the result ([`takes_other_route`]) is `fallback`'s when that lint is on.

use blaze_hash::HashSet;
use hir::HirDisplay as _;
use hir_def::{
    DefWithBodyId, HasModule as _, ItemContainerId,
    expr_store::Body,
    hir::{CaptureBy, Expr, ExprId, MatchArm, Pat, PatId, Statement},
    lang_item::lang_items,
    signatures::FunctionSignature,
};
use hir_ty::{InferenceResult, next_solver::TyKind};
use rustc_type_ir::inherent::IntoKind as _;
use std::alloc::Global;

use super::{Finding, Lint, facts};
use crate::{check::Cx, lints::Implemented};

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(Lint::DroppedError) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// How the clippy ports that share places with this lint stand in the crate linted.
struct Shared {
    unused_result_ok: bool,
    map_err_ignore: bool,
    let_underscore_must_use: bool,
}

/// What the shapes that drop a whole `Result` say: rule no-fallback, AGENTS.md rule 3.
const NO_FALLBACK: &str = "rule no-fallback (AGENTS.md rule 3): a failure is loud; hand the error on with `?`, or say \
                           why dropping it is right with `#[expect(blaze::dropped_error, reason = \"..\")]`";

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let lang = lang_items(db, def.krate(db));
    let Some(err) = lang.ResultErr else { return found };
    let ok = lang.ResultOk;
    let Some(hir::ModuleDef::Adt(result)) = cx.std_def(&["core", "result", "Result"]) else {
        return found;
    };
    let shared = Shared {
        unused_result_ok: cx.on(Implemented::UnusedResultOk),
        map_err_ignore: cx.on(Implemented::MapErrIgnore),
        let_underscore_must_use: cx.on(Implemented::LetUnderscoreMustUse),
    };
    let body = analysis.body();
    let infer = analysis.infer();
    let is_result = |expr: ExprId| analysis.ty(infer.expr_ty(expr)).as_adt() == Some(result);
    let target = cx.krate().to_display_target(db);
    // An `Err` branch that answers the failure by another route is `fallback`'s when it is on:
    // the same place is not reported twice, and `fallback` says the sharper thing.
    let fallback = cx.on_blaze(Lint::Fallback);
    let statements = shared.unused_result_ok.then(|| expression_statements(body));
    let error_of = |scrutinee: ExprId| {
        let ty = analysis.ty(infer.expr_ty(scrutinee));
        let ty = ty.strip_references();
        match ty.type_arguments().nth(1) {
            Some(it) => format!("`{}`", it.display(db, target)),
            None => "error".to_owned(),
        }
    };
    for (id, expr) in body.exprs() {
        match expr {
            Expr::Match { expr: scrutinee, arms } => {
                for &MatchArm { pat, expr: value, .. } in arms {
                    if !drops_err(body, infer, err, pat) {
                        continue;
                    }
                    let value_ty = analysis.ty(infer.expr_ty(value));
                    if value_ty.is_never() || value_ty.is_unit() {
                        continue;
                    }
                    if fallback && takes_other_route(body, infer, value) {
                        continue;
                    }
                    let Some(range) = analysis.expr_range(value) else { continue };
                    let message = format!(
                        "this `Err` arm throws away the {} and makes a `{}` in its place",
                        error_of(scrutinee),
                        value_ty.display(db, target)
                    );
                    let note = "the value can't say what failed: hand the error on with `?` or keep it as the new \
                                error's source";
                    facts::push(
                        &mut found,
                        Lint::DroppedError,
                        range,
                        message,
                        Some(note.to_owned()),
                    );
                }
            }
            Expr::Block { statements, .. } => {
                if shared.let_underscore_must_use {
                    continue;
                }
                for statement in statements {
                    let Statement::Let { pat, initializer: Some(value), else_branch: None, .. } =
                        statement
                    else {
                        continue;
                    };
                    if !matches!(body.pat(pat), Pat::Wild) || !is_result(value) {
                        continue;
                    }
                    let Some(range) = analysis.expr_range(value) else { continue };
                    let message =
                        format!("`let _ =` throws away this `Result` and its {}", error_of(value));
                    facts::push(&mut found, Lint::DroppedError, range, message, Some(NO_FALLBACK.to_owned()));
                }
            }
            Expr::If { condition, else_branch: None, .. } => {
                let Expr::Let { pat, expr: scrutinee } = body.expr(condition) else { continue };
                if ok.is_none() || infer.variant_resolution_for_pat(pat) != ok.map(Into::into) {
                    continue;
                }
                if !is_result(scrutinee) {
                    continue;
                }
                let Some(range) = analysis.expr_range(condition) else { continue };
                let message = format!(
                    "this `if let Ok(..)` has no `else`: the {} of the `Result` is dropped and nothing says it failed",
                    error_of(scrutinee)
                );
                facts::push(&mut found, Lint::DroppedError, range, message, Some(NO_FALLBACK.to_owned()));
            }
            Expr::MethodCall { receiver, args, .. } => {
                let Some((f, _)) = infer.method_resolution(id) else { continue };
                if let [arg] = args
                    && FunctionSignature::of(db, f).name.as_str() == "filter_map"
                    && is_result_ok(db, infer, *arg, result)
                {
                    let Some(range) = analysis.expr_range(id) else { continue };
                    let message =
                        "`.filter_map(Result::ok)` throws away every error of the iterator's `Result`s"
                            .to_owned();
                    facts::push(&mut found, Lint::DroppedError, range, message, Some(NO_FALLBACK.to_owned()));
                    continue;
                }
                if args.is_empty()
                    && FunctionSignature::of(db, f).name.as_str() == "flatten"
                    && let Some(item) = iterator_item(cx, f, &analysis.ty(infer.expr_ty(receiver)))
                    && item.as_adt() == Some(result)
                {
                    let Some(range) = analysis.expr_range(id) else { continue };
                    let error = match item.type_arguments().nth(1) {
                        Some(it) => format!("`{}`", it.display(db, target)),
                        None => "error".to_owned(),
                    };
                    let message = format!(
                        "`.flatten()` throws away the {error} of each `Err` the iterator yields, as `Result`'s iterator yields nothing for one"
                    );
                    facts::push(&mut found, Lint::DroppedError, range, message, Some(NO_FALLBACK.to_owned()));
                    continue;
                }
                if !of_result(db, f, result) {
                    continue;
                }
                let name = FunctionSignature::of(db, f).name.as_str();
                let dropped = match (name, args) {
                    ("ok", []) => !statements.as_ref().is_some_and(|it| it.contains(&id)),
                    ("unwrap_or" | "unwrap_or_default", _) => true,
                    ("unwrap_or_else", [closure]) => {
                        ignores_param(body, *closure).is_some()
                            && !(fallback && closure_takes_other_route(body, infer, *closure))
                    }
                    ("map_err", [closure]) => match ignores_param(body, *closure) {
                        Some(Ignored { wild: true, by_move: false }) => !shared.map_err_ignore,
                        Some(_) => true,
                        None => false,
                    },
                    _ => false,
                };
                if !dropped {
                    continue;
                }
                let Some(range) = analysis.expr_range(id) else { continue };
                let message = format!(
                    "`.{name}(..)` throws away the {} of this `Result`",
                    error_of(receiver)
                );
                let note = match name {
                    "map_err" => {
                        "the new error can't say what failed: keep the original as its source"
                    }
                    _ => {
                        "the value in its place can't say what failed: hand the error on with `?`, or say why \
                          dropping it is right with `#[expect(blaze::dropped_error, reason = \"..\")]`"
                    }
                };
                facts::push(&mut found, Lint::DroppedError, range, message, Some(note.to_owned()));
            }
            _ => {}
        }
    }
    found
}

/// Whether `pat` is `Err(_)`, `Err(..)` or `Err(_name)`: the `Err` of a `Result` whose error is
/// never bound to a name that is read.
pub(super) fn drops_err(
    body: &Body,
    infer: &InferenceResult<'_>,
    err: hir_def::EnumVariantId,
    pat: PatId,
) -> bool {
    let Pat::TupleStruct { args, ellipsis, .. } = body.pat(pat) else { return false };
    if infer.variant_resolution_for_pat(pat) != Some(err.into()) {
        return false;
    }
    match args {
        [] => ellipsis.is_some(),
        [inner] => is_ignored(body, *inner),
        _ => false,
    }
}

/// Whether `pat` binds nothing that is meant to be read: `_`, `..` or `_name`.
fn is_ignored(body: &Body, pat: PatId) -> bool {
    match body.pat(pat) {
        Pat::Wild | Pat::Rest => true,
        Pat::Bind { id, subpat: None } => body[id].name.as_str().starts_with('_'),
        _ => false,
    }
}

/// How a closure ignores its one parameter.
pub(super) struct Ignored {
    /// The parameter is exactly `_`.
    wild: bool,
    by_move: bool,
}

/// Whether `value`, the value an `Err` branch gives in place of the `Ok`'s, is another route to
/// the result: a call of a function or a method (past a block that is only its tail) that is
/// not a default (`T::new()`, `Default::default()`, a call of no arguments by path) nor a copy
/// (`.clone()`, `.to_owned()`, `.to_string()`, `.into()`). The one home of the rule
/// `dropped_error` and `fallback` share: what this answers yes to is `fallback`'s, the rest of
/// a dropped error is this lint's.
pub(super) fn takes_other_route(
    body: &Body,
    infer: &InferenceResult<'_>,
    value: ExprId,
) -> bool {
    let mut at = value;
    for _ in 0..8 {
        match body.expr(at) {
            Expr::Block { statements, tail: Some(tail), .. } if statements.is_empty() => at = tail,
            Expr::Call { callee, args } => {
                let TyKind::FnDef(def, _) = infer.expr_ty(callee).kind() else { return false };
                let hir_def::CallableDefId::FunctionId(_) = def.0 else { return false };
                return !args.is_empty();
            }
            Expr::MethodCall { method_name, .. } => {
                return !matches!(
                    method_name.as_str(),
                    "clone" | "to_owned" | "to_string" | "into" | "cloned" | "copied"
                );
            }
            _ => return false,
        }
    }
    false
}

/// [`takes_other_route`] of the body of the closure `closure`.
pub(super) fn closure_takes_other_route(
    body: &Body,
    infer: &InferenceResult<'_>,
    closure: ExprId,
) -> bool {
    let Expr::Closure { body: inner, .. } = body.expr(closure) else { return false };
    takes_other_route(body, infer, inner)
}

/// How the closure `expr` ignores its one parameter, if it is a closure of one that it does.
pub(super) fn ignores_param(body: &Body, expr: ExprId) -> Option<Ignored> {
    let Expr::Closure { args: [param], capture_by, .. } = body.expr(expr) else { return None };
    is_ignored(body, *param).then(|| Ignored {
        wild: matches!(body.pat(*param), Pat::Wild),
        by_move: matches!(capture_by, CaptureBy::Value),
    })
}

/// What the iterator `receiver` yields, when `f` is `Iterator`'s own method (`flatten`), so a
/// `flatten` of an iterator of `Result`s is told from `Option::flatten` and the like.
fn iterator_item<'q>(
    cx: &Cx<'q, '_>,
    f: hir_def::FunctionId,
    receiver: &hir::Type<'q>,
) -> Option<hir::Type<'q>> {
    let db = cx.db();
    let iterator = cx.famous().core_iter_Iterator()?;
    let of = hir::AsAssocItem::as_assoc_item(hir::Function::from(f), db)?
        .container_or_implemented_trait(db)?;
    if of != iterator {
        return None;
    }
    cx.assoc_ty(receiver, iterator, "Item")
}

/// Whether `f` is a method of `core::result::Result`'s own impl.
pub(super) fn of_result(db: &base_db::Db, f: hir_def::FunctionId, result: hir::Adt) -> bool {
    let ItemContainerId::ImplId(it) = f.loc(db).container else { return false };
    let imp = hir::Impl::from(it);
    imp.trait_(db).is_none() && imp.self_ty(db).as_adt() == Some(result)
}

/// Whether `expr` names `Result::ok` as a value (`Result::ok`, `std::result::Result::ok`): the
/// function item of `Result`'s own impl named `ok`.
fn is_result_ok(
    db: &base_db::Db,
    infer: &InferenceResult<'_>,
    expr: ExprId,
    result: hir::Adt,
) -> bool {
    let TyKind::FnDef(def, _) = infer.expr_ty(expr).kind() else { return false };
    let hir_def::CallableDefId::FunctionId(f) = def.0 else { return false };
    of_result(db, f, result) && FunctionSignature::of(db, f).name.as_str() == "ok"
}

/// The expressions of `body` that are statements of their block (`x.ok();`).
fn expression_statements(body: &Body) -> HashSet<ExprId, Global> {
    // alloc: one set per body when `unused_result_ok` is on beside this lint
    let mut found = HashSet::new_in(Global);
    for (_, expr) in body.exprs() {
        let Expr::Block { statements, .. } = expr else { continue };
        for statement in statements {
            if let Statement::Expr { expr, .. } = statement {
                found.insert(expr);
            }
        }
    }
    found
}
