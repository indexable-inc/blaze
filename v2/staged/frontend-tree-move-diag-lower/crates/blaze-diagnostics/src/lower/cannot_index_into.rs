//! E0608 "cannot index into a value of type `u8`": an index into a type that implements no
//! `Index`, on the brackets, with the help for tuples and the suggestion for raw pointers.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`check_expr_index`)
//! (MIT or Apache-2.0).

use hir::{InFile, LangItem};
use rustc_type_ir::inherent::Ty as _;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use super::binary_operator_cannot_be_applied::code_fact;
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::CannotIndexInto<'db>) -> Vec<Found> {
    let node = cx.node_view(d.expr);
    let file = node.file_id;
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let index_expr = match node.value.left() {
        Some(ast::Expr::IndexExpr(it)) => Some(it),
        _ => None,
    };
    let brackets = index_expr.and_then(|it| {
        let open = it.l_brack_token()?.text_range().start();
        let close = it.r_brack_token()?.text_range().end();
        Some(at(TextRange::new(open, close)))
    });
    let brackets = brackets.unwrap_or_else(|| cx.span_of_ptr(d.expr));
    let ty = cx.ty(&d.found);
    let mut diag =
        Diag::error(Some("E0608"), format!("cannot index into a value of type `{ty}`"), brackets.clone());

    let base = index_expr.and_then(|it| it.base());
    let index = index_expr.and_then(|it| it.index());
    if d.found.is_tuple() {
        diag.help("tuples are indexed with a dot and a literal index: `tuple.0`, `tuple.1`, etc.");
        if let Some(ast::Expr::Literal(literal)) = index
            && let ast::LiteralKind::IntNumber(number) = literal.kind()
            && number.split_into_parts().2.is_empty()
            && let Ok(i) = number.value()
            && i < d.found.tuple_fields(cx.db).len() as u128
        {
            diag.suggestion(
                brackets.clone(),
                format!("to access tuple element `{i}`, use"),
                format!(".{i}"),
                Applicability::MachineApplicable,
            );
        }
    }

    if d.found.is_raw_ptr()
        && let (Some(base), Some(index), Some(index_expr)) = (base, index, index_expr)
        && cx.vsema.type_of_expr(index).is_some_and(|it| it.original.to_next_solver().is_integral())
    {
        let base_end = base.syntax().text_range().end();
        let index_range = index.syntax().text_range();
        diag.multipart_suggestion(
            "consider using `wrapping_add` or `add` for indexing into raw pointer",
            vec![
                (at(TextRange::new(base_end, index_range.start())), ".wrapping_add(".to_owned()),
                (
                    at(TextRange::new(index_range.end(), index_expr.syntax().text_range().end())),
                    ")".to_owned(),
                ),
            ],
            Applicability::MaybeIncorrect,
        );
    }

    if let Some(index_trait) = hir::Trait::lang(cx.db, cx.krate, LangItem::Index) {
        diag.fact_opt(cx.implements_fact(index_trait, &d.found), format!("whether `{ty}` implements `Index`"));
    }
    code_fact(cx, d.expr.map(|it| it.syntax_node_ptr()), &mut diag);
    vec![Found::new(diag, Phase::Bodies)]
}
