//! `useless_borrows_in_formatting`: a format macro's argument borrowed when what it borrows
//! formats the same.
//!
//! Follows clippy's `clippy_lints/src/format_args.rs` (`check_useless_borrows_in_formatting`)
//! at rust-lang/rust 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0).

use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use super::format::{self, Arg, Trait};
use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::UselessBorrowsInFormatting;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, call: ast::MacroCall<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(format) = format::of(cx, call) else { return };
    // Only a borrow written as an argument is peeled: one without asks for no type.
    let borrowed = (0..format.arg_count).any(|index| {
        format.arg(index).is_some_and(|it| matches!(it.peel_parens(), ast::Expr::RefExpr(_)))
    });
    if !borrowed {
        return;
    }
    let (Some(display), Some(debug), Some(sized)) = (
        cx.std_trait(&["core", "fmt", "Display"]),
        cx.std_trait(&["core", "fmt", "Debug"]),
        cx.lang_trait(hir::LangItem::Sized),
    ) else {
        return;
    };
    // The same argument taken twice is one diagnostic, as rustc emits a diagnostic once. Few
    // are found in one call, so the ones reported are kept in a list.
    let mut seen: Vec<TextRange> = Vec::new();
    for placeholder in format.placeholders() {
        let fmt_trait = match placeholder.format_trait {
            Trait::Display => display,
            Trait::Debug => debug,
            Trait::Other => continue,
        };
        for arg in [Some(placeholder.arg), placeholder.width, placeholder.precision] {
            let Some(Arg::Index(index)) = arg else { continue };
            let Some(expr) = format.arg(index) else { continue };
            let Some((range, peeled, message)) = peel(cx, expr, fmt_trait, sized) else { continue };
            if seen.contains(&range) {
                continue;
            }
            seen.push(range);
            let span = cx.span(range);
            cx.lint(
                LINT,
                call.syntax(),
                span.clone(),
                format!("redundant reference in `{}!` argument", format.name),
            )
            .suggestion(span, message, peeled, Applicability::MachineApplicable);
        }
    }
}

/// Where `arg` is written, what it is with the borrows that change nothing peeled, and the
/// help saying which.
fn peel<'t>(
    cx: &Cx<'_, 't>,
    arg: ast::Expr<'t>,
    fmt_trait: hir::Trait,
    sized: hir::Trait,
) -> Option<(TextRange, &'t str, &'static str)> {
    if !matches!(arg.peel_parens(), ast::Expr::RefExpr(_)) {
        return None;
    }
    let range = cx.original_range(arg.syntax())?;
    let db = cx.db();
    let mut only_mutable = true;
    let mut has_mutable = false;
    let mut peeled = arg;
    // rustc's HIR has no parentheses: `&(x)` is a borrow of `x` written `(x)`.
    while let ast::Expr::RefExpr(borrow) = peeled.peel_parens()
        && borrow.raw_token().is_none()
        && let Some(inner) = borrow.expr()
        && let Some(ty) = cx.ty(inner.peel_parens())
        && ty.impls_trait(db, sized, &[])
        && ty.impls_trait(db, fmt_trait, &[])
    {
        let mutable = borrow.mut_token().is_some();
        only_mutable &= mutable;
        has_mutable |= mutable;
        peeled = inner;
    }
    if peeled.syntax() == arg.syntax() {
        return None;
    }
    let peeled = cx.snippet(cx.original_range(peeled.syntax())?);
    let message = if only_mutable {
        "remove the redundant `&mut`"
    } else if has_mutable {
        "remove the redundant `&`/`&mut`"
    } else {
        "remove the redundant `&`"
    };
    Some((range, peeled, message))
}

