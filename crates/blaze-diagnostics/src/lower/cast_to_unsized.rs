//! E0620 "cast to unsized type: `&[usize; 2]` as `[usize]`": an `as` to a type that is not
//! `Sized`, with the reference or `Box` to cast to instead.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/cast.rs` (`report_cast_to_unsized_type`)
//! (MIT or Apache-2.0).

use hir::InFile;
use syntax::view::{self as ast, AstNode as _};

use super::binary_operator_cannot_be_applied::code_fact;
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::CastToUnsized<'db>) -> Vec<Found> {
    let node = cx.node_view(d.expr);
    let file = node.file_id;
    let span = cx.span_of_ptr(d.expr);
    let cast = match node.value.left() {
        Some(ast::Expr::CastExpr(it)) => Some(it),
        _ => None,
    };
    let operand = cast.and_then(|it| it.expr());
    let expr_ty = operand.and_then(|it| cx.vsema.type_of_expr(it)).map(|it| it.original);
    let expr_span = operand.map(|it| cx.span(InFile::new(file, it.syntax().text_range())));
    let cast_span = cast
        .and_then(|it| it.ty())
        .map(|it| cx.span(InFile::new(file, it.syntax().text_range())));

    // alloc: a type printed for the error's message
    let expr_str = expr_ty.as_ref().map_or_else(|| "_".to_owned(), |it| cx.ty(it));
    let mut diag = Diag::error(
        Some("E0620"),
        // alloc: the error's message
        format!("cast to unsized type: `{expr_str}` as `{}`", cx.ty(&d.cast_ty)),
        span,
    );
    match (&expr_ty, &cast_span) {
        (Some(ty), Some(cast_span)) if ty.as_reference().is_some() => {
            let prefix = match ty.as_reference().map(|(_, it)| it) {
                Some(hir::Mutability::Mut) => "&mut ",
                _ => "&",
            };
            diag.suggestion_verbose(
                cast_span.shrink_to_lo(),
                "consider casting to a reference instead",
                prefix,
                Applicability::MachineApplicable,
            );
        }
        (Some(ty), Some(cast_span)) if is_box(cx, ty) => {
            diag.multipart_suggestion(
                "you can cast to a `Box` instead",
                // alloc: the suggestion's parts
                vec![
                    (cast_span.shrink_to_lo(), "Box<".to_owned()), // alloc: the suggestion's text
                    (cast_span.shrink_to_hi(), ">".to_owned()),    // alloc: the suggestion's text
                ],
                Applicability::MachineApplicable,
            );
        }
        _ => {
            if let Some(expr_span) = expr_span {
                diag.span_help(expr_span, "consider using a box or reference as appropriate");
            }
        }
    }
    code_fact(cx, d.expr.map(|it| it.syntax_node_ptr()), &mut diag);
    vec![Found::new(diag, Phase::Bodies)] // alloc: the error found
}

/// Whether `ty` is the lang item `owned_box`, rustc's `AdtDef::is_box`.
pub(crate) fn is_box(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> bool {
    let owned_box = hir_def::lang_item::lang_items(cx.db, cx.krate.base()).OwnedBox;
    matches!(ty.as_adt(), Some(hir::Adt::Struct(it)) if owned_box.is_some_and(|box_| hir_def::StructId::from(it) == box_))
}
