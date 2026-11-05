//! What the general speed lints (`unbuffered_io`, `stdout_lock_in_loop`, `front_vec_ops`,
//! `dyn_in_hot_field`) ask of an expression: the loop it runs once per iteration of.
//!
//! An expression runs once per iteration when it is inside a `loop`, `while` or `for` body, or in
//! a closure given to an iterator method ([`per_item`]'s rule: `map`, `for_each`, rayon's ..). A
//! closure given to anything else may run once, and ends the walk up. A `for` loop's iterable
//! is evaluated once, outside its loop, and does not count.

use hir_def::hir::{Expr, ExprId};

use super::{facts::Parents, per_item};
use crate::check::Cx;

/// How many expressions up an expression is followed to its loop.
const UP: usize = 64;

/// The innermost loop (or iterator closure) `expr` runs once per iteration of.
pub(super) fn enclosing_loop(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    parents: &Parents,
    expr: ExprId,
) -> Option<ExprId> {
    let body = analysis.body();
    for parent in parents.up(expr, UP) {
        match body.expr(parent) {
            Expr::Loop { .. } => return Some(parent),
            // No loop between `expr` and the closure: `per_item` finds the closure first, and
            // says whether it is an iterator method's.
            Expr::Closure { .. } => {
                return per_item::per_item(cx, analysis, parents, expr).map(per_item::Over::expr);
            }
            _ => {}
        }
    }
    None
}

/// Whether `id` is written in the file of the body's tree (not by a macro): its text is then the
/// source's.
pub(super) fn written(analysis: &hir::BodyAnalysis<'_>, id: ExprId) -> bool {
    let file = analysis.tree().1;
    analysis.source_map().expr_syntax(id).is_ok_and(|it| it.file_id == file)
}

/// Whether the literal `expr` is the integer `value`.
pub(super) fn is_int(body: &hir_def::expr_store::Body, expr: ExprId, value: u128) -> bool {
    match body.expr(expr) {
        Expr::Literal(hir_def::hir::Literal::Uint(it, _)) => *it == value,
        Expr::Literal(hir_def::hir::Literal::Int(it, _)) => {
            u128::try_from(*it).is_ok_and(|it| it == value)
        }
        _ => false,
    }
}
