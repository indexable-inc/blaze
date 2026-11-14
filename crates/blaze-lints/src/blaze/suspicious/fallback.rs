//! `fallback`: a `Result`'s failure answered by computing the same result another way, the error
//! never looked at. The route that failed says nothing; the second route runs on every failure
//! of the first, the one it was meant for and every other (a permission error, a corrupt file, a
//! bug), and the output is whatever the second route makes. AGENTS.md rule 3 (no-fallback) bans
//! it: a failure is loud. One of, on a `core::result::Result` (resolved: the method is of
//! `Result`'s own impl, the pattern's variant is the lang item `Err`):
//!
//! - a `match` arm `Err(_)`, `Err(..)` or `Err(_name)` whose value is another route to the
//!   result: a call of a function with arguments or of a method that is not a copy
//!   (`dropped_error::takes_other_route`, the one home of the rule);
//! - `.unwrap_or_else(|_| route(..))`, the same in a closure that never reads the error;
//! - `.or_else(|_| ..)`: a second `Result` in place of the first, whatever its body;
//! - `if let Err(_) = r { .. }`, an `Err(_)` arm whose value is `()`, `if r.is_err() { .. }` and
//!   `if !r.is_ok() { .. }`, whose branch assigns a place a call's value: the result
//!   recomputed into the variable the first route's went to (`v = slow_path(..)`).
//!
//! An arm that returns, breaks or panics is no fallback; neither is an `Err(e)` that names the
//! error it reads, `.or_else(|e| ..)` that reads `e`, nor a default (`unwrap_or_default`, an
//! `Err(_) => Vec::new()`), which stays `dropped_error`'s. The first two shapes were
//! `dropped_error`'s too: when this lint is on, that lint leaves them to it, so each place is
//! said once. Test code is left alone: a test's fallback is its own.
//!
//! The right form: hand the error on with `?`; or match the one failure the second route is for
//! and hand on every other (`Err(e) if e.kind() == NotFound => route(..)`, `Err(e) => return
//! Err(e)`), so the fallback is a decision the code states, not a net under everything.

use hir_def::{
    DefWithBodyId, HasModule as _,
    expr_store::Body,
    hir::{Expr, ExprId, MatchArm, UnaryOp},
    lang_item::lang_items,
    signatures::FunctionSignature,
};
use hir_ty::{InferenceResult, next_solver::TyKind};
use rustc_type_ir::inherent::IntoKind as _;
use syntax::view as ast;

use super::super::{
    Finding, Lint,
    dropped_error::{
        closure_takes_other_route, drops_err, ignores_param, of_result, takes_other_route,
    },
    facts,
};
use crate::check::Cx;

const LINT: Lint = Lint::Fallback;

const NOTE: &str = "rule no-fallback (AGENTS.md rule 3): the second route runs on every failure of the first, \
                    the one it is for and every other; hand the error on with `?`, or match the one failure \
                    the second route is for (`Err(e) if e.kind() == ..`) and hand on the rest";

/// Once per body, at its root: the shapes are read from the body's HIR.
pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) || node != cx.root() || !cx.in_file() {
        return;
    }
    if cx.is_in_test(node) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// What a body's shapes need: its HIR, its types and `Result`'s pieces.
struct Shapes<'a, 'i> {
    db: &'a base_db::Db,
    body: &'a Body,
    infer: &'a InferenceResult<'i>,
    err: hir_def::EnumVariantId,
    result: hir::Adt,
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let lang = lang_items(db, def.krate(db));
    let Some(err) = lang.ResultErr else { return found };
    let Some(hir::ModuleDef::Adt(result)) = cx.std_def(&["core", "result", "Result"]) else {
        return found;
    };
    let body = analysis.body();
    let infer = analysis.infer();
    let at = Shapes { db, body, infer, err, result };
    let is_result = |expr: ExprId| analysis.ty(infer.expr_ty(expr)).as_adt() == Some(result);
    for (id, expr) in body.exprs() {
        match expr {
            // The arm's pattern is `Result`'s `Err` by resolution, as `dropped_error` reads it,
            // so the places that lint leaves to this one are exactly the ones found here.
            Expr::Match { arms, .. } => {
                for &MatchArm { pat, expr: value, .. } in arms {
                    if !drops_err(body, infer, err, pat) {
                        continue;
                    }
                    let value_ty = analysis.ty(infer.expr_ty(value));
                    if value_ty.is_never() {
                        continue;
                    }
                    let route = match value_ty.is_unit() {
                        false if takes_other_route(body, infer, value) => Some(value),
                        false => None,
                        true => at.assigned_route(value),
                    };
                    let Some(route) = route else { continue };
                    let Some(range) = analysis.expr_range(value) else { continue };
                    let message = format!(
                        "this `Err` arm answers the failure by another route, {}, and never looks at the error",
                        at.route_name(route)
                    );
                    facts::push(&mut found, LINT, range, message, Some(NOTE.to_owned()));
                }
            }
            Expr::If { condition, then_branch, .. } => {
                let Some(what) = at.failed(condition, &is_result) else { continue };
                let Some(route) = at.assigned_route(then_branch) else { continue };
                let Some(range) = analysis.expr_range(condition) else { continue };
                let message = format!(
                    "when {what} fails this branch computes the result again by another route, {}, and never looks at the error",
                    at.route_name(route)
                );
                facts::push(&mut found, LINT, range, message, Some(NOTE.to_owned()));
            }
            Expr::MethodCall { args: [closure], .. } => {
                let Some((f, _)) = infer.method_resolution(id) else { continue };
                if !of_result(db, f, result) || ignores_param(body, *closure).is_none() {
                    continue;
                }
                let name = FunctionSignature::of(db, f).name.as_str();
                let message = match name {
                    "or_else" => "`.or_else(|_| ..)` answers every error of this `Result` with a second `Result` and never looks at the first error".to_owned(),
                    "unwrap_or_else" if closure_takes_other_route(body, infer, *closure) => {
                        let Expr::Closure { body: inner, .. } = body.expr(*closure) else { continue };
                        format!(
                            "`.unwrap_or_else(|_| ..)` answers every error of this `Result` by another route, {}, and never looks at the error",
                            at.route_name(inner)
                        )
                    }
                    _ => continue,
                };
                let Some(range) = analysis.expr_range(id) else { continue };
                facts::push(&mut found, LINT, range, message, Some(NOTE.to_owned()));
            }
            _ => {}
        }
    }
    found
}

impl Shapes<'_, '_> {
    /// What `condition` tests the failure of, when it is one that never reads the error:
    /// `let Err(_) = r`, `r.is_err()`, `!r.is_ok()`.
    fn failed(&self, condition: ExprId, is_result: &dyn Fn(ExprId) -> bool) -> Option<&'static str> {
        match self.body.expr(condition) {
            Expr::Let { pat, expr } => {
                (is_result(expr) && drops_err(self.body, self.infer, self.err, pat)).then_some("the `Result`")
            }
            Expr::MethodCall { args: [], .. } => {
                self.result_method(condition, "is_err").then_some("the `Result`")
            }
            Expr::UnaryOp { expr, op: UnaryOp::Not } => {
                self.result_method(expr, "is_ok").then_some("the `Result`")
            }
            _ => None,
        }
    }

    /// Whether `call` is `Result`'s own method `name`.
    fn result_method(&self, call: ExprId, name: &str) -> bool {
        let Some((f, _)) = self.infer.method_resolution(call) else { return false };
        of_result(self.db, f, self.result) && FunctionSignature::of(self.db, f).name.as_str() == name
    }

    /// The call `branch` assigns to a place, when it does (`v = route(..)`), at any depth of the
    /// branch but in a closure, which runs at another time.
    fn assigned_route(&self, branch: ExprId) -> Option<ExprId> {
        let mut found = None;
        let mut stack = vec![branch]; // alloc: a branch's few nested expressions, walked once
        let mut budget = 256_u32;
        while let Some(at) = stack.pop() {
            budget = budget.checked_sub(1)?;
            match self.body.expr(at) {
                Expr::Closure { .. } => continue,
                Expr::Assignment { value, .. } if takes_other_route(self.body, self.infer, value) => {
                    found = Some(value);
                    break;
                }
                _ => {}
            }
            self.body.walk_child_exprs(at, |it| stack.push(it));
        }
        found
    }

    /// The route `value` is, as the message names it: `` `name(..)` `` or `` `.name(..)` ``.
    fn route_name(&self, value: ExprId) -> String {
        let mut at = value;
        for _ in 0..8 {
            match self.body.expr(at) {
                Expr::Block { tail: Some(tail), .. } => at = tail,
                Expr::MethodCall { method_name, .. } => return format!("`.{}(..)`", method_name.as_str()),
                Expr::Call { callee, .. } => {
                    if let TyKind::FnDef(def, _) = self.infer.expr_ty(callee).kind()
                        && let hir_def::CallableDefId::FunctionId(f) = def.0
                    {
                        return format!("`{}(..)`", FunctionSignature::of(self.db, f).name.as_str());
                    }
                    return "a call".to_owned();
                }
                _ => break,
            }
        }
        "a call".to_owned()
    }
}
