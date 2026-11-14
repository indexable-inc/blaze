//! `default_numeric_fallback_len`: `let n = 0;` (an integer literal with no suffix and no type
//! written) that nothing ties to a type, so inference falls back to `i32`, while the function
//! casts it `as usize` where it is used: a length, a count or an index kept in the wrong type.
//! It overflows at 2^31 where the `usize` it stands for would not, a negative value cast `as
//! usize` is a huge index, and each cast is a place to get it wrong again.
//!
//! The fix writes `: usize` on the binding and drops the `as usize` casts of it. It is
//! maybe-incorrect: arithmetic that went below zero in `i32` underflows in `usize` (which a
//! debug build reports), and a use that needs the `i32` (an argument of an `i32` parameter)
//! then does not compile. Outside test code.

use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{Fix, body_of, bound, in_test, parent_expr, report, strip, uses_of};
use crate::{blaze::Lint, check::Cx, diag::Applicability};

const LINT: Lint = Lint::DefaultNumericFallbackLen;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(stmt) = ast::LetStmt::cast(node) else { return };
    if stmt.ty().is_some() || stmt.let_else().is_some() {
        return;
    }
    let Some(ast::Expr::Literal(literal)) = stmt.initializer().map(strip) else { return };
    if !literal.syntax().text().chars().all(|it| it.is_ascii_digit() || it == '_') {
        return;
    }
    let Some(pat) = stmt.pat() else { return };
    let Some(local) = bound(cx, pat) else { return };
    let Some(ty) = cx.pat_ty(pat) else { return };
    if !ty.as_builtin().is_some_and(|it| it.name().as_str() == "i32") {
        return;
    }
    let Some(body) = body_of(node, true) else { return };
    // alloc: the casts of one local, a few
    let mut casts = Vec::new();
    for path in uses_of(cx, body, local) {
        let Some(cast) = parent_expr(path.syntax()).and_then(ast::CastExpr::cast) else { continue };
        let to_usize = cast
            .ty()
            .and_then(|it| cx.resolve_type(it))
            .and_then(|it| it.as_builtin())
            .is_some_and(|it| it.name().as_str() == "usize");
        if to_usize {
            casts.push((cast, path));
        }
    }
    if casts.is_empty() || in_test(cx, node) {
        return;
    }
    let Some(pat_range) = cx.range(pat.syntax()) else { return };
    // alloc: the fix's parts, one per cast
    let mut parts = vec![(TextRange::empty(pat_range.end()), ": usize".to_owned())];
    for (cast, path) in &casts {
        let (Some(at), Some(name)) = (cx.range(cast.syntax()), cx.range(path.syntax())) else {
            return;
        };
        parts.push((at, cx.snippet(name).to_owned()));
    }
    let fix = Fix { message: "write the type", parts, applicability: Applicability::MaybeIncorrect };
    let name = match pat {
        ast::Pat::IdentPat(it) => it.name().map_or_else(String::new, |it| it.text().to_owned()),
        _ => return,
    };
    report(
        cx,
        LINT,
        node,
        pat_range,
        format!(
            "`{name}` falls back to `i32` and is cast `as usize` where it is used ({} time{})",
            casts.len(),
            if casts.len() == 1 { "" } else { "s" }
        ),
        "a length, count or index is a `usize`: an `i32` overflows at 2^31, and a negative one cast `as usize` is a huge index",
        Some(fix),
    );
}
