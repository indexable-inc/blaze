//! `redundant_static_lifetimes`: `'static` written on a reference in the type of a constant or
//! a static, which have it already.
//!
//! Follows clippy's `clippy_lints/src/redundant_static_lifetimes.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0).

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use crate::{check::Cx, conf::Version, diag::Applicability};

const LINT: super::Implemented = super::Implemented::RedundantStaticLifetimes;

/// clippy's `msrvs::STATIC_IN_CONST`.
const STATIC_IN_CONST: Version = Version(1, 17, 0);

pub(super) fn check_const<'t>(cx: &mut Cx<'_, 't>, item: ast::Const<'t>) {
    // An associated constant cannot leave `'static` out.
    let associated =
        item.syntax().parent().is_some_and(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST);
    if associated || !cx.on(LINT) || !cx.conf().meets(STATIC_IN_CONST) {
        return;
    }
    if let Some(ty) = item.ty() {
        visit(cx, item.syntax(), ty, "constants have by default a `'static` lifetime");
    }
}

pub(super) fn check_static<'t>(cx: &mut Cx<'_, 't>, item: ast::Static<'t>) {
    let foreign =
        item.syntax().parent().is_some_and(|it| it.kind() == SyntaxKind::EXTERN_ITEM_LIST);
    if foreign || !cx.on(LINT) || !cx.conf().meets(STATIC_IN_CONST) {
        return;
    }
    if let Some(ty) = item.ty() {
        visit(cx, item.syntax(), ty, "statics have by default a `'static` lifetime");
    }
}

fn visit<'t>(cx: &mut Cx<'_, 't>, item: ast::Node<'t>, ty: ast::Type<'t>, reason: &'static str) {
    match ty {
        ast::Type::ArrayType(it) => {
            if let Some(ty) = it.ty() {
                visit(cx, item, ty, reason);
            }
        }
        ast::Type::SliceType(it) => {
            if let Some(ty) = it.ty() {
                visit(cx, item, ty, reason);
            }
        }
        ast::Type::TupleType(it) => {
            for ty in it.fields() {
                visit(cx, item, ty, reason);
            }
        }
        ast::Type::RefType(reference) => {
            let Some(borrowed) = reference.ty() else { return };
            if let Some(lifetime) = reference.lifetime()
                && lifetime.text() == "'static"
                && matches!(
                    borrowed,
                    ast::Type::PathType(_)
                        | ast::Type::SliceType(_)
                        | ast::Type::ArrayType(_)
                        | ast::Type::TupleType(_)
                )
            {
                let (Some(at), Some(whole), Some(borrowed_at)) = (
                    cx.range(lifetime.syntax()),
                    cx.range(reference.syntax()),
                    cx.range(borrowed.syntax()),
                ) else {
                    return;
                };
                let mutability = if reference.mut_token().is_some() { "mut " } else { "" };
                let sugg = format!("&{mutability}{}", cx.snippet(borrowed_at));
                let span = cx.span(at);
                let whole = cx.span(whole);
                cx.lint(LINT, item, span, reason).suggestion(
                    whole,
                    "consider removing `'static`",
                    sugg,
                    Applicability::MachineApplicable,
                );
            }
            visit(cx, item, borrowed, reason);
        }
        _ => {}
    }
}
