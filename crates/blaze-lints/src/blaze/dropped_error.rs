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
//! A place a clippy port reports when it is on is left to it, so no place is reported twice:
//! `x.ok();` as a statement is `unused_result_ok`'s, and `.map_err(|_| ..)` with a non-`move`
//! closure whose parameter is exactly `_` is `map_err_ignore`'s.

use blaze_hash::HashSet;
use hir::HirDisplay as _;
use hir_def::{
    DefWithBodyId, HasModule as _, ItemContainerId,
    expr_store::Body,
    hir::{CaptureBy, Expr, ExprId, MatchArm, Pat, PatId, Statement},
    lang_item::lang_items,
    signatures::FunctionSignature,
};
use hir_ty::InferenceResult;

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
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let lang = lang_items(db, def.krate(db));
    let Some(err) = lang.ResultErr else { return found };
    let Some(hir::ModuleDef::Adt(result)) = cx.std_def(&["core", "result", "Result"]) else { return found };
    let shared =
        Shared { unused_result_ok: cx.on(Implemented::UnusedResultOk), map_err_ignore: cx.on(Implemented::MapErrIgnore) };
    let body = analysis.body();
    let infer = analysis.infer();
    let target = cx.krate().to_display_target(db);
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
                    let Some(range) = analysis.expr_range(value) else { continue };
                    let message = format!(
                        "this `Err` arm throws away the {} and makes a `{}` in its place",
                        error_of(scrutinee),
                        value_ty.display(db, target)
                    );
                    let note = "the value can't say what failed: hand the error on with `?` or keep it as the new \
                                error's source";
                    facts::push(&mut found, Lint::DroppedError, range, message, Some(note.to_owned()));
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                let Some((f, _)) = infer.method_resolution(id) else { continue };
                if !of_result(db, f, result) {
                    continue;
                }
                let name = FunctionSignature::of(db, f).name.as_str();
                let dropped = match (name, args) {
                    ("ok", []) => !statements.as_ref().is_some_and(|it| it.contains(&id)),
                    ("unwrap_or" | "unwrap_or_default", _) => true,
                    ("unwrap_or_else", [closure]) => ignores_param(body, *closure).is_some(),
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
                let message = format!("`.{name}(..)` throws away the {} of this `Result`", error_of(receiver));
                let note = match name {
                    "map_err" => "the new error can't say what failed: keep the original as its source",
                    _ => "the value in its place can't say what failed: hand the error on with `?`, or say why \
                          dropping it is right with `#[expect(blaze::dropped_error, reason = \"..\")]`",
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
fn drops_err(body: &Body, infer: &InferenceResult<'_>, err: hir_def::EnumVariantId, pat: PatId) -> bool {
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
struct Ignored {
    /// The parameter is exactly `_`.
    wild: bool,
    by_move: bool,
}

/// How the closure `expr` ignores its one parameter, if it is a closure of one that it does.
fn ignores_param(body: &Body, expr: ExprId) -> Option<Ignored> {
    let Expr::Closure { args: [param], capture_by, .. } = body.expr(expr) else { return None };
    is_ignored(body, *param).then(|| Ignored {
        wild: matches!(body.pat(*param), Pat::Wild),
        by_move: matches!(capture_by, CaptureBy::Value),
    })
}

/// Whether `f` is a method of `core::result::Result`'s own impl.
fn of_result(db: &base_db::Db, f: hir_def::FunctionId, result: hir::Adt) -> bool {
    let ItemContainerId::ImplId(it) = f.loc(db).container else { return false };
    let imp = hir::Impl::from(it);
    imp.trait_(db).is_none() && imp.self_ty(db).as_adt() == Some(result)
}

/// The expressions of `body` that are statements of their block (`x.ok();`).
fn expression_statements(body: &Body) -> HashSet<ExprId> {
    // alloc: one set per body when `unused_result_ok` is on beside this lint
    let mut found = HashSet::default();
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
