//! `match_like_matches_macro`: a `match`, or an `if let .. else`, whose arms give `true` and
//! `false`, which `matches!` says alone.
//!
//! Follows clippy's `clippy_lints/src/matches/match_like_matches.rs` and the checks before it
//! in `clippy_lints/src/matches/mod.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0). A `Some(_)` arm is left to
//! `redundant_pattern_matching` as clippy leaves it when that lint is not allowed, which is
//! taken to be always.

use syntax::view::{self as ast, AstNode as _, HasAttrs as _};

use super::utils;
use crate::{
    check::Cx,
    diag::{Applicability, SuggestionStyle},
    msrvs,
};

const LINT: super::Implemented = super::Implemented::MatchLikeMatchesMacro;

pub(super) fn check_match<'t>(cx: &mut Cx<'_, 't>, expr: ast::MatchExpr<'t>) {
    if !cx.on(LINT) || !cx.conf().meets(msrvs::MATCHES_MACRO) {
        return;
    }
    let (Some(range), Some(scrutinee), Some(list)) =
        (cx.range(expr.syntax()), expr.expr(), expr.match_arm_list())
    else {
        return;
    };
    let arms = list.arms();
    // clippy's `walk_intra_arm_text` turns away a match with an arm under `#[cfg]`.
    if arms
        .clone()
        .any(|arm| arm.attrs().any(|it| it.path().is_some_and(|it| it.syntax().text() == "cfg")))
    {
        return;
    }
    let count = arms.clone().count();
    if count < 2 {
        return;
    }
    let without_last = arms.clone().take(count - 1);
    let (Some(first), Some(last)) = (arms.clone().next(), arms.clone().last()) else { return };
    let (Some(b0), Some(b1)) = (
        first.expr().and_then(find_bool_lit),
        last.expr().and_then(find_bool_lit),
    ) else {
        return;
    };
    if b0 == b1 {
        return;
    }
    let first_guard_ok = first.guard().is_none_or(|guard| {
        !guard.syntax().descendants().any(|it| ast::LetExpr::can_cast(it.kind()))
    });
    let shape = (count == 2 && first_guard_ok)
        || without_last.clone().all(|arm| {
            arm.attrs().next().is_none()
                && arm.guard().is_none()
                && arm.expr().and_then(find_bool_lit) == Some(b0)
        });
    if !shape || !matches!(last.pat(), Some(ast::Pat::WildcardPat(_))) {
        return;
    }
    if without_last.clone().any(|arm| arm.pat().is_some_and(is_some_wild)) {
        return;
    }
    if utils::contains_comment(expr.syntax(), expr.syntax().text_range())
        || !is_bool(cx, ast::Expr::MatchExpr(expr))
    {
        return;
    }
    let mut pat_and_guard = String::new();
    for arm in without_last {
        let Some(pat) = arm.pat().and_then(|it| cx.range(it.syntax())) else { return };
        if !pat_and_guard.is_empty() {
            pat_and_guard.push_str(" | ");
        }
        pat_and_guard.push_str(cx.snippet(pat));
    }
    if let Some(guard) = first.guard() {
        let Some(condition) = guard.condition().and_then(|it| cx.range(it.syntax())) else {
            return;
        };
        pat_and_guard.push_str(" if ");
        pat_and_guard.push_str(cx.snippet(condition));
    }
    let Some(snippet) = scrutinee_snippet(cx, scrutinee) else { return };
    // An arm under a `cfg` that is off is gone before clippy sees the match.
    let applicability = Applicability::MaybeIncorrect;
    let span = cx.span(range);
    let code = format!("{}matches!({snippet}, {pat_and_guard})", if b0 { "" } else { "!" });
    cx.lint(LINT, expr.syntax(), span.clone(), "match expression looks like `matches!` macro")
        .suggestion_with_style(
            vec![(span, code)],
            "use `matches!` directly",
            applicability,
            SuggestionStyle::ShowAlways,
        );
}

pub(super) fn check_if_let<'t>(cx: &mut Cx<'_, 't>, expr: ast::IfExpr<'t>) {
    if !cx.on(LINT) || !cx.conf().meets(msrvs::MATCHES_MACRO) {
        return;
    }
    let (
        Some(ast::Expr::LetExpr(let_expr)),
        Some(then),
        Some(ast::ElseBranch::Block(otherwise)),
    ) = (expr.condition(), expr.then_branch(), expr.else_branch())
    else {
        return;
    };
    let Some(range) = cx.range(expr.syntax()) else { return };
    let (Some(pat), Some(scrutinee)) = (let_expr.pat(), let_expr.expr()) else { return };
    let (Some(b0), Some(b1)) = (
        find_bool_lit(ast::Expr::BlockExpr(then)),
        find_bool_lit(ast::Expr::BlockExpr(otherwise)),
    ) else {
        return;
    };
    if b0 == b1 || is_some_wild(pat) {
        return;
    }
    if utils::contains_comment(expr.syntax(), expr.syntax().text_range())
        || !is_bool(cx, ast::Expr::IfExpr(expr))
    {
        return;
    }
    let applicability = Applicability::MaybeIncorrect;
    let Some(pat) = cx.range(pat.syntax()) else { return };
    let pat = cx.snippet(pat);
    let Some(snippet) = scrutinee_snippet(cx, scrutinee) else { return };
    let span = cx.span(range);
    let code = format!("{}matches!({snippet}, {pat})", if b0 { "" } else { "!" });
    cx.lint(
        LINT,
        expr.syntax(),
        span.clone(),
        "`if let .. else` expression looks like `matches!` macro",
    )
    .suggestion_with_style(
        vec![(span, code)],
        "use `matches!` directly",
        applicability,
        SuggestionStyle::ShowAlways,
    );
}

fn is_bool<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> bool {
    cx.ty(expr).is_some_and(|it| it.is_bool())
}

/// The scrutinee as written, a borrow of a reference taken off (clippy's #6503).
fn scrutinee_snippet<'t>(cx: &Cx<'_, 't>, scrutinee: ast::Expr<'t>) -> Option<&'t str> {
    let mut expr = scrutinee;
    if let ast::Expr::RefExpr(borrow) = scrutinee
        && borrow.raw_token().is_none()
        && let Some(inner) = borrow.expr()
        && cx.ty(inner).is_some_and(|it| it.is_reference())
    {
        expr = inner;
    }
    Some(cx.snippet(cx.range(expr.syntax())?))
}

/// clippy's `find_bool_lit`: a `bool` literal, or a block of nothing else.
fn find_bool_lit(expr: ast::Expr<'_>) -> Option<bool> {
    let expr = match expr {
        ast::Expr::BlockExpr(block) if block.modifier().is_none() => {
            let list = block.stmt_list()?;
            if list.statements().next().is_some() {
                return None;
            }
            list.tail_expr()?
        }
        _ => expr,
    };
    let ast::Expr::Literal(literal) = expr else { return None };
    match literal.kind() {
        ast::LiteralKind::Bool(it) => Some(it),
        _ => None,
    }
}

/// clippy's `is_some_wild`: `Some(_, ..)`.
fn is_some_wild(pat: ast::Pat<'_>) -> bool {
    let ast::Pat::TupleStructPat(pat) = pat else { return false };
    let first_wild = matches!(pat.fields().next(), Some(ast::Pat::WildcardPat(_)));
    let name = pat.path().and_then(|it| it.first_segment()).and_then(|it| it.name_ref());
    first_wild && name.is_some_and(|it| it.text() == "Some")
}
