//! `needless_bool`: an `if` whose branches are `true` and `false`, or an `if` guard returning
//! a bool followed by the other bool, which the condition says alone.
//!
//! Follows clippy's `clippy_lints/src/needless_bool.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0). Code handed to a macro is
//! not linted here.

use hir::PathResolution;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasArgList as _},
};

use super::utils;
use crate::{check::Cx, diag::Applicability, sugg::Sugg};

const LINT: super::Implemented = super::Implemented::NeedlessBool;

/// What a branch gives: a bool, or a `return` of one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Expression {
    Bool(bool),
    RetBool(bool),
}

pub(super) fn check_if<'t>(cx: &mut Cx<'_, 't>, expr: ast::IfExpr<'t>) {
    if !cx.on(LINT) || !cx.in_file() {
        return;
    }
    let (Some(cond), Some(then), Some(ast::ElseBranch::Block(else_block))) =
        (expr.condition(), expr.then_branch(), expr.else_branch())
    else {
        return;
    };
    let (Some(a), Some(b)) = (
        fetch_bool_block(ast::Expr::BlockExpr(then)),
        fetch_bool_block(ast::Expr::BlockExpr(else_block)),
    ) else {
        return;
    };
    if matches!(cond, ast::Expr::LetExpr(_))
        || cond.syntax().descendants().any(|it| ast::LetExpr::can_cast(it.kind()))
    {
        return;
    }
    let range = expr.syntax().text_range();
    if utils::contains_comment(expr.syntax(), range) {
        return;
    }
    let span = cx.span(range);
    let (ret, not) = match (a, b) {
        (Expression::RetBool(true), Expression::RetBool(true))
        | (Expression::Bool(true), Expression::Bool(true)) => {
            cx.lint(
                LINT,
                expr.syntax(),
                span,
                "this if-then-else expression will always return true",
            );
            return;
        }
        (Expression::RetBool(false), Expression::RetBool(false))
        | (Expression::Bool(false), Expression::Bool(false)) => {
            cx.lint(
                LINT,
                expr.syntax(),
                span,
                "this if-then-else expression will always return false",
            );
            return;
        }
        (Expression::RetBool(true), Expression::RetBool(false)) => (true, false),
        (Expression::Bool(true), Expression::Bool(false)) => (false, false),
        (Expression::RetBool(false), Expression::RetBool(true)) => (true, true),
        (Expression::Bool(false), Expression::Bool(true)) => (false, true),
        _ => return,
    };
    let mut snip = utils::sugg(cx, cond);
    if not {
        snip = !snip;
    }
    if ret {
        snip = snip.make_return();
    }
    let parent = expr.syntax().parent();
    if parent.and_then(ast::IfExpr::cast).is_some_and(|it| {
        matches!(it.else_branch(), Some(ast::ElseBranch::IfExpr(inner)) if inner.syntax() == expr.syntax())
    }) {
        snip = snip.blockify();
    }
    let parent_expr = parent.and_then(ast::Expr::cast);
    let is_receiver = matches!(parent_expr, Some(ast::Expr::MethodCallExpr(call)) if call.receiver().is_some_and(|it| it.syntax() == expr.syntax()));
    let is_operand = match parent_expr {
        Some(ast::Expr::CastExpr(_) | ast::Expr::PrefixExpr(_)) => true,
        Some(ast::Expr::BinExpr(bin)) => {
            !matches!(bin.op_kind(), Some(ast::BinaryOp::Assignment { .. }))
        }
        _ => false,
    };
    let is_stmt = parent.is_some_and(|it| ast::ExprStmt::can_cast(it.kind()));
    if (condition_needs_parentheses(cond) && is_stmt) || is_receiver || is_operand {
        snip = snip.maybe_paren();
    }
    cx.lint(
        LINT,
        expr.syntax(),
        span.clone(),
        "this if-then-else expression returns a bool literal",
    )
    .suggestion(
        span,
        "you can reduce it to",
        snip.to_string(),
        Applicability::MachineApplicable,
    );
}

/// The early-return form: `if c { return Ok(true); } Ok(false)`, the last statement of a
/// block and its tail, which is `Ok(c)`.
pub(super) fn check_block<'t>(cx: &mut Cx<'_, 't>, block: ast::StmtList<'t>) {
    if !cx.on(LINT) || !cx.in_file() {
        return;
    }
    let Some(tail) = block.tail_expr() else { return };
    let (mut previous, mut last) = (None, None);
    for stmt in block.statements() {
        previous = last.replace(stmt);
    }
    let Some(ast::Stmt::ExprStmt(last)) = last else { return };
    let Some(ast::Expr::IfExpr(if_expr)) = last.expr() else { return };
    let Some(then_val) = guard_return(if_expr) else { return };
    let Some(cond) = if_expr.condition() else { return };
    if cond.syntax().descendants().any(|it| ast::LetExpr::can_cast(it.kind())) {
        return;
    }
    let Some((then_func, then_val)) = fetch_bool_and_wrapper(then_val) else { return };
    let Some((tail_func, tail_val)) = fetch_bool_and_wrapper(tail) else { return };
    if then_val == tail_val || !wrapper_ctors_match(cx, then_func, tail_func) {
        return;
    }
    // Another guard before this one: replacing the last alone makes the block no simpler.
    if let Some(ast::Stmt::ExprStmt(previous)) = previous
        && let Some(ast::Expr::IfExpr(previous)) = previous.expr()
        && guard_return(previous).is_some()
    {
        return;
    }
    let range =
        TextRange::new(if_expr.syntax().text_range().start(), tail.syntax().text_range().end());
    if utils::contains_comment(block.syntax(), range) {
        return;
    }
    let mut snip: Sugg = utils::sugg(cx, cond);
    if !then_val {
        snip = !snip;
    }
    let sugg = match tail_func {
        Some(func) => format!("{}({snip})", cx.source_of(func.syntax())),
        None => snip.to_string(),
    };
    let span = cx.span(range);
    cx.lint(
        LINT,
        block.syntax(),
        span.clone(),
        "this `if` guard returns a bool literal and is followed by another",
    )
    .suggestion(span, "you can reduce it to", sugg, Applicability::MachineApplicable);
}

/// What `if_expr`, an `if` without `else` whose block only returns, returns.
fn guard_return(if_expr: ast::IfExpr<'_>) -> Option<ast::Expr<'_>> {
    if if_expr.else_branch().is_some() {
        return None;
    }
    let then = if_expr.then_branch()?;
    let ast::Expr::ReturnExpr(ret) = utils::peel_blocks_with_stmt(ast::Expr::BlockExpr(then))
    else {
        return None;
    };
    ret.expr()
}

fn fetch_bool_block(expr: ast::Expr<'_>) -> Option<Expression> {
    match utils::peel_blocks_with_stmt(expr) {
        ast::Expr::ReturnExpr(ret) => Some(Expression::RetBool(fetch_bool_expr(ret.expr()?)?)),
        _ => Some(Expression::Bool(fetch_bool_expr(expr)?)),
    }
}

fn fetch_bool_expr(expr: ast::Expr<'_>) -> Option<bool> {
    let ast::Expr::Literal(literal) = utils::peel_blocks(expr) else { return None };
    match literal.kind() {
        ast::LiteralKind::Bool(it) => Some(it),
        _ => None,
    }
}

/// The constructor around a bool literal, `Ok` of `Ok(true)`, if any, and the bool.
fn fetch_bool_and_wrapper(expr: ast::Expr<'_>) -> Option<(Option<ast::Expr<'_>>, bool)> {
    let expr = utils::peel_blocks(expr);
    if let Some(value) = fetch_bool_expr(expr) {
        return Some((None, value));
    }
    let ast::Expr::CallExpr(call) = expr else { return None };
    let mut args = call.arg_list()?.args();
    let arg = args.next()?;
    if args.next().is_some() {
        return None;
    }
    Some((Some(call.expr()?), fetch_bool_expr(arg)?))
}

fn wrapper_ctors_match<'t>(
    cx: &Cx<'_, 't>,
    a: Option<ast::Expr<'t>>,
    b: Option<ast::Expr<'t>>,
) -> bool {
    let ctor = |func: ast::Expr<'t>| -> Option<hir::ModuleDef> {
        let ast::Expr::PathExpr(path) = func else { return None };
        match cx.path(path.path()?)? {
            PathResolution::Def(def @ hir::ModuleDef::EnumVariant(_)) => Some(def),
            PathResolution::Def(def @ hir::ModuleDef::Adt(hir::Adt::Struct(_))) => Some(def),
            _ => None,
        }
    };
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => ctor(a).is_some_and(|it| Some(it) == ctor(b)),
        _ => false,
    }
}

/// Whether the condition, put first in a statement, would be read as a block.
fn condition_needs_parentheses(mut inner: ast::Expr<'_>) -> bool {
    loop {
        let next = match inner {
            ast::Expr::BinExpr(it) => it.lhs(),
            ast::Expr::CallExpr(it) => it.expr(),
            ast::Expr::CastExpr(it) => it.expr(),
            ast::Expr::IndexExpr(it) => it.base(),
            _ => return false,
        };
        let Some(next) = next else { return false };
        if matches!(
            next,
            ast::Expr::BlockExpr(_)
                | ast::Expr::IfExpr(_)
                | ast::Expr::MatchExpr(_)
                | ast::Expr::LoopExpr(_)
                | ast::Expr::WhileExpr(_)
                | ast::Expr::ForExpr(_)
        ) {
            return true;
        }
        inner = next;
    }
}
