//! `needless_return`: `return x` as the last thing a function or closure does, where `x`
//! alone would do.
//!
//! Follows clippy's `clippy_lints/src/returns/needless_return.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0). Not ported: clippy's check
//! that the returned value leaks no temporary with a significant drop.

use std::ops::Range;

use syntax::{
    TextRange, TextSize,
    view::{self as ast, AstNode as _, HasAttrs as _},
};

use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::NeedlessReturn;

/// clippy's `RetReplacement`: what the `return` gives way to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Replacement<'t> {
    Empty,
    Block,
    Unit,
    NeedsPar(&'t str),
    Expr(&'t str),
}

impl Replacement<'_> {
    fn help(self) -> &'static str {
        match self {
            Self::Empty | Self::Expr(_) => "remove `return`",
            Self::Block => "replace `return` with an empty block",
            Self::Unit => "replace `return` with a unit value",
            Self::NeedsPar(_) => "remove `return` and wrap the sequence with parentheses",
        }
    }

    fn text(self) -> String {
        match self {
            Self::Empty => String::new(),
            Self::Block => "{}".to_owned(),
            Self::Unit => "()".to_owned(),
            Self::NeedsPar(inner) => format!("({inner})"),
            Self::Expr(inner) => inner.to_owned(),
        }
    }
}

/// What a needless `return` found is reported with.
struct Found<'t> {
    ret: ast::ReturnExpr<'t>,
    ret_span: TextRange,
    /// Its semicolons, in [`Walk::saved`].
    semis: Range<usize>,
    replacement: Replacement<'t>,
}

/// What a walk over one body's last expressions has found, and the semicolons it is under.
#[derive(Default)]
struct Walk<'t> {
    found: Vec<Found<'t>>,
    /// The semicolons of the blocks the walk is in, outermost first.
    semis: Vec<TextRange>,
    /// The semicolons of each found, one after another.
    saved: Vec<TextRange>,
}

/// clippy's `check_fn` for a function or a method with a body.
pub(super) fn check_fn<'t>(cx: &mut Cx<'_, 't>, function: ast::Fn<'t>) {
    if !cx.on(LINT) || cx.range(function.syntax()).is_none() {
        return;
    }
    let Some(body) = function.body() else { return };
    let mut walk = Walk::default();
    check_block(cx, ast::Expr::BlockExpr(body), &mut walk);
    report(cx, function.syntax(), walk);
}

/// clippy's `check_fn` for a closure.
pub(super) fn check_closure<'t>(cx: &mut Cx<'_, 't>, closure: ast::ClosureExpr<'t>) {
    if !cx.on(LINT) || cx.range(closure.syntax()).is_none() {
        return;
    }
    let Some(body) = closure.body() else { return };
    let replacement = match body {
        ast::Expr::ReturnExpr(it) if it.expr().is_none() => Replacement::Block,
        _ => Replacement::Empty,
    };
    let mut walk = Walk::default();
    check_final(cx, body, replacement, None, &mut walk);
    report(cx, closure.syntax(), walk);
}

fn report<'t>(cx: &mut Cx<'_, 't>, when: ast::Node<'t>, walk: Walk<'t>) {
    for Found { ret, ret_span, semis, replacement } in walk.found {
        let Some(range) = cx.range(ret.syntax()) else { continue };
        let mut parts = Vec::with_capacity(1 + semis.len());
        parts.push((cx.span(ret_span), replacement.text()));
        parts.extend(walk.saved[semis].iter().map(|&it| (cx.span(it), String::new())));
        let span = cx.span(range);
        cx.lint_when(LINT, ret.syntax(), when, span, "unneeded `return` statement")
            .multipart_suggestion(replacement.help(), parts, Applicability::MachineApplicable);
    }
}

/// clippy's `check_block_return`: the last expression of `expr`, if it is a block.
fn check_block<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>, walk: &mut Walk<'t>) {
    let ast::Expr::BlockExpr(block) = expr else { return };
    if !matches!(
        block.modifier(),
        None | Some(ast::BlockModifier::Unsafe(_) | ast::BlockModifier::Label(_))
    ) {
        return;
    }
    let Some(list) = block.stmt_list() else { return };
    if let Some(tail) = list.tail_expr() {
        check_final(cx, tail, Replacement::Empty, None, walk);
        return;
    }
    let Some(ast::Stmt::ExprStmt(stmt)) = list.statements().last() else { return };
    let (Some(stmt_range), Some(list_range)) = (cx.range(stmt.syntax()), cx.range(list.syntax()))
    else {
        return;
    };
    if super::collapsible_if::contains_cfg(
        cx.snippet(TextRange::new(stmt_range.end(), list_range.end())),
    ) {
        return;
    }
    let Some(inner) = stmt.expr() else { return };
    let depth = walk.semis.len();
    if stmt.semicolon_token().is_some()
        && let Some(inner_range) = cx.range(inner.syntax())
    {
        // The semicolon and the spaces before it, which without `return` would make the
        // value a statement.
        let start = inner_range.end();
        let text = cx.snippet(TextRange::new(start, list_range.end()));
        let len = text.bytes().take_while(|it| *it == b' ' || *it == b';').count();
        walk.semis.push(TextRange::at(start, TextSize::try_from(len).unwrap_or_default()));
    }
    check_final(cx, inner, Replacement::Empty, None, walk);
    walk.semis.truncate(depth);
}

/// clippy's `check_final_expr`. `in_match` is the `match` whose arm `expr` ends, whose type
/// says what a `return` with no value gives way to.
fn check_final<'t>(
    cx: &Cx<'_, 't>,
    expr: ast::Expr<'t>,
    replacement: Replacement<'t>,
    in_match: Option<ast::MatchExpr<'t>>,
    walk: &mut Walk<'t>,
) {
    match expr {
        ast::Expr::ReturnExpr(ret) => {
            // An attribute on the `return` keeps it an expression of its own.
            if ret.attrs().next().is_some() {
                return;
            }
            let Some(range) = cx.range(ret.syntax()) else { return };
            let inner = ret.expr();
            let ret_span = if inner.is_none() && replacement == Replacement::Empty {
                extend_to_previous_non_ws(cx, range)
            } else {
                range
            };
            let replacement = match inner {
                Some(inner) => {
                    let Some(at) = cx.range(inner.syntax()) else { return };
                    let snippet = cx.snippet(at);
                    if needs_parentheses(inner) {
                        Replacement::NeedsPar(snippet)
                    } else {
                        Replacement::Expr(snippet)
                    }
                }
                None => match in_match.and_then(|it| cx.ty(ast::Expr::MatchExpr(it))) {
                    Some(ty) if ty.is_unit() || ty.is_tuple() => Replacement::Unit,
                    Some(_) => return,
                    None => replacement,
                },
            };
            let start = walk.saved.len();
            walk.saved.extend_from_slice(&walk.semis);
            let semis = start..walk.saved.len();
            walk.found.push(Found { ret, ret_span, semis, replacement });
        }
        ast::Expr::IfExpr(if_expr) => {
            if let Some(then) = if_expr.then_branch() {
                check_block(cx, ast::Expr::BlockExpr(then), walk);
            }
            match if_expr.else_branch() {
                Some(ast::ElseBranch::Block(block)) => {
                    check_final(
                        cx,
                        ast::Expr::BlockExpr(block),
                        Replacement::Empty,
                        in_match,
                        walk,
                    );
                }
                Some(ast::ElseBranch::IfExpr(inner)) => {
                    check_final(cx, ast::Expr::IfExpr(inner), Replacement::Empty, in_match, walk);
                }
                None => {}
            }
        }
        ast::Expr::MatchExpr(match_expr) => {
            for arm in match_expr.match_arm_list().into_iter().flat_map(|it| it.arms()) {
                if let Some(body) = arm.expr() {
                    check_final(cx, body, Replacement::Unit, Some(match_expr), walk);
                }
            }
        }
        _ => check_block(cx, expr, walk),
    }
}

/// clippy's `binary_expr_needs_parentheses`: an operator whose left operand starts with a
/// block, which as a statement would end there.
fn needs_parentheses(expr: ast::Expr<'_>) -> bool {
    fn contains_block(expr: ast::Expr<'_>, is_operand: bool) -> bool {
        match expr {
            ast::Expr::BinExpr(bin) => bin.lhs().is_some_and(|it| contains_block(it, true)),
            ast::Expr::CastExpr(cast) => cast.expr().is_some_and(|it| contains_block(it, true)),
            _ if is_block_like(expr) => is_operand,
            _ => false,
        }
    }
    contains_block(expr, false)
}

fn is_block_like(expr: ast::Expr<'_>) -> bool {
    matches!(
        expr,
        ast::Expr::BlockExpr(_)
            | ast::Expr::IfExpr(_)
            | ast::Expr::LoopExpr(_)
            | ast::Expr::MatchExpr(_)
            | ast::Expr::WhileExpr(_)
            | ast::Expr::ForExpr(_)
    )
}

/// clippy's `extend_span_to_previous_non_ws`.
fn extend_to_previous_non_ws(cx: &Cx<'_, '_>, range: TextRange) -> TextRange {
    let before = &cx.text()[..usize::from(range.start())];
    match before.bytes().rposition(|c| !b" \t\n".contains(&c)) {
        Some(at) => TextRange::new(TextSize::try_from(at + 1).unwrap_or_default(), range.end()),
        None => range,
    }
}
