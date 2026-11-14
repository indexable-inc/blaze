//! `always_same_result`: a function whose every return gives the same answer: the same constant
//! at every return, a `Result` that is never `Err`, or an `Option` that is never `None`. The
//! signature promises a choice the body never makes: every caller handles an error or an absence
//! that cannot happen, or reads a value that never changes.
//!
//! Found from the body's tree: its results are the tail (through blocks, `if` and `match`
//! arms) and each `return`'s value outside closures, the diverging ones (`panic!`, `loop`)
//! left out. A `?` is a `return` of the residual, so a function that passes an error on is never
//! found. Not found: an exported function (an API may keep room for an error), a trait's function
//! or an impl's of one (the trait's signature is the choice), a function named as a value
//! (a callback's type is its caller's), one a serde impl calls (a `#[serde(default = "..")]`),
//! an `async` function, test code.
//!
//! No fix: the right form (a `const`, the plain value, the `Ok` type) changes every caller.

use hir_def::{
    CallableDefId, DefWithBodyId, FunctionId, Lookup as _,
    expr_store::Body,
    hir::{Expr, ExprId},
};
use hir_ty::{InferenceResult, next_solver::TyKind};
use rustc_type_ir::inherent::IntoKind as _;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{
    Lint, facts, fn_subject, judged_fn,
    rows::{self, Constant, Role, Way},
};
use crate::check::Cx;

const LINT: Lint = Lint::AlwaysSameResult;

/// The most expressions the walk of a body reads (rule 18).
const FUEL: usize = 1 << 14;

/// What one result of the function is.
#[derive(PartialEq, Eq)]
enum Result_ {
    Constant(Constant),
    /// `Ok(..)` or `Some(..)` of anything.
    Wrapped(&'static str),
    Other,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on_blaze(LINT) || cx.is_exported(f.syntax()) || f.async_token().is_some() {
        return;
    }
    let Some(func) = judged_fn(cx, f) else { return };
    let db = cx.db();
    if func.ret_type(db).is_unit() {
        return;
    }
    let Some(subject) = fn_subject(cx, func) else { return };
    let seen = facts(cx, subject);
    if seen.all(Way::Named) > 0 || seen.get(Role::Stored, Way::Call) > 0 {
        return;
    }
    let Ok(id) = FunctionId::try_from(func) else { return };
    let def = DefWithBodyId::FunctionId(id);
    let body = Body::of(db, def);
    let infer = InferenceResult::of(db, def);
    // alloc: the body's results and the walk's stack, once per function linted
    let mut results = Vec::new();
    tails(body, body.root_expr(), &mut results);
    let mut stack = vec![body.root_expr()];
    let mut fuel = FUEL;
    while let Some(at) = stack.pop() {
        let Some(left) = fuel.checked_sub(1) else { return };
        fuel = left;
        match body.expr(at) {
            Expr::Closure { .. } => continue,
            Expr::Return { expr: Some(value) } => tails(body, value, &mut results),
            Expr::Return { expr: None } => return,
            _ => {}
        }
        body.walk_child_exprs(at, |child| stack.push(child));
    }
    let famous = cx.famous();
    let result_enum = famous.core_result_Result();
    let option_enum = famous.core_option_Option();
    // alloc: one kind per result
    let mut kinds = Vec::new();
    for at in results {
        if let TyKind::Never = infer.expr_ty(at).kind() {
            continue;
        }
        let kind = if let Some(it) = rows::constant(db, infer, body, at) {
            Result_::Constant(it)
        } else if let Expr::Call { callee, .. } = body.expr(at)
            && let TyKind::FnDef(callable, _) = infer.expr_ty(callee).kind()
            && let CallableDefId::EnumVariantId(variant) = callable.0
        {
            let loc = variant.lookup(db);
            let parent = Some(hir::Enum::from(loc.parent));
            match loc.name.as_str() {
                "Ok" if parent == result_enum => Result_::Wrapped("Ok"),
                "Some" if parent == option_enum => Result_::Wrapped("Some"),
                _ => Result_::Other,
            }
        } else {
            Result_::Other
        };
        kinds.push(kind);
    }
    let Some(first) = kinds.first() else { return };
    if *first == Result_::Other || kinds.iter().any(|it| it != first) {
        return;
    }
    let name = func.name(db).as_str().to_owned();
    let (message, note) = match first {
        Result_::Wrapped("Ok") => (
            format!("`fn {name}` returns a `Result` that is never `Err`"),
            "every caller handles an error that cannot happen: return the value itself",
        ),
        Result_::Wrapped(_) => (
            format!("`fn {name}` returns an `Option` that is never `None`"),
            "every caller handles an absence that cannot happen: return the value itself",
        ),
        _ => (
            format!("`fn {name}` returns the same constant at every return"),
            "the function is a constant: make it a `const`, or use the value where it is called",
        ),
    };
    let Some(range) = f.name().and_then(|it| cx.range(it.syntax())) else { return };
    super::report(cx, LINT, f.syntax(), range, message, note, None);
}

/// The results of `expr`: itself, or through a block's tail, an `if`'s branches and a
/// `match`'s arms.
fn tails(body: &Body, expr: ExprId, out: &mut Vec<ExprId>) {
    match body.expr(expr) {
        Expr::Block { tail: Some(tail), .. } => tails(body, tail, out),
        Expr::If { then_branch, else_branch: Some(other), .. } => {
            tails(body, then_branch, out);
            tails(body, other, out);
        }
        Expr::Match { arms, .. } => {
            for arm in arms {
                tails(body, arm.expr, out);
            }
        }
        _ => out.push(expr),
    }
}
