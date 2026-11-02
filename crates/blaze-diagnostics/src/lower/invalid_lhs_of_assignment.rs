//! E0070 and E0067 "invalid left-hand side of assignment": an `=` or a compound assignment
//! whose left-hand side is not a place, on the operator, with the `let` for a `while` condition
//! that meant `while let`, the semicolon a multiplication across lines was missing, and the `*`
//! that assigns through a mutable reference.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`check_expr_assign`,
//! `check_lhs_assignable`, `comes_from_while_condition`) and `op.rs` (`check_expr_assign_op`)
//! (MIT or Apache-2.0).

use hir::{InFile, LangItem};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use super::binary_operator_cannot_be_applied::{
    code_fact, deref_once_mutably, lang_item, missing_semi,
};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::InvalidLhsOfAssignment) -> Vec<Found> {
    let node = cx.node_view(d.lhs);
    let file = node.file_id;
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let lhs_syntax = node.value.syntax();
    let lhs_range = lhs_syntax.text_range();
    let lhs_span = at(lhs_range);
    if lhs_syntax.descendants().any(|it| ast::LetExpr::can_cast(it.kind())) {
        // rustc's `contains_let_in_chain`: the let chain's own error says it.
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let assign = lhs_syntax.ancestors().filter_map(ast::BinExpr::cast).find(|it| {
        matches!(it.op_kind(), Some(ast::BinaryOp::Assignment { .. }))
            && it.lhs().is_some_and(|lhs| lhs.syntax().text_range().contains_range(lhs_range))
    });
    let op = assign.and_then(|it| it.op_kind());
    let compound = matches!(op, Some(ast::BinaryOp::Assignment { op: Some(_) }));
    let code = if compound { "E0067" } else { "E0070" };
    let op_span = assign
        .and_then(|it| it.op_token())
        // clone: on the error path, for an error hir found
        .map_or_else(|| lhs_span.clone(), |it| at(it.text_range()));

    let mut diag = Diag::error(Some(code), "invalid left-hand side of assignment", op_span);
    // clone: on the error path, for an error hir found
    diag.span_label(lhs_span.clone(), "cannot assign to this expression");

    if let Some(condition) = while_condition(lhs_syntax) {
        diag.suggestion_verbose(
            at(condition.syntax().text_range()).shrink_to_lo(),
            "you might have meant to use pattern destructuring",
            "let ",
            Applicability::MachineApplicable,
        );
    }
    if let Some(ast::Expr::BinExpr(bin)) = node.value.left() {
        missing_semi(cx, file, bin, &mut diag);
    }

    let lhs_ty = node.value.left().and_then(|it| cx.vsema.type_of_expr(it)).map(|it| it.original);
    let rhs_ty = assign
        .and_then(|it| it.rhs())
        .and_then(|it| cx.vsema.type_of_expr(it))
        .map(|it| it.original);
    if let (Some(lhs_ty), Some(rhs_ty)) = (&lhs_ty, &rhs_ty)
        && let Some(lhs_deref) = deref_once_mutably(cx, lhs_ty)
    {
        if compound {
            let trait_ = op.and_then(lang_item).and_then(|it| hir::Trait::lang(cx.db, cx.krate, it));
            if let Some(trait_) = trait_
                && lhs_deref.impls_trait(cx.db, trait_, std::slice::from_ref(rhs_ty))
            {
                if !lhs_ty.impls_trait(cx.db, trait_, std::slice::from_ref(rhs_ty)) {
                    // rustc's error on the operator already suggests the dereference.
                    // alloc: empty, allocates nothing
                    return Vec::new();
                }
                diag.suggestion_verbose(
                    lhs_span.shrink_to_lo(),
                    "consider dereferencing the left-hand side of this operation",
                    "*",
                    Applicability::MaybeIncorrect,
                );
            }
        } else {
            let sized = hir::Trait::lang(cx.db, cx.krate, LangItem::Sized)
                .is_some_and(|it| lhs_deref.impls_trait(cx.db, it, &[]));
            if sized && rhs_ty.could_coerce_to(cx.db, &lhs_deref) {
                diag.suggestion_verbose(
                    lhs_span.shrink_to_lo(),
                    "consider dereferencing here to assign to the mutably borrowed value",
                    "*",
                    Applicability::MachineApplicable,
                );
            }
        }
    }

    code_fact(cx, d.lhs.map(|it| it.syntax_node_ptr()), &mut diag);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// The condition of the `while` loop `lhs` is in the condition of, up to the enclosing item.
fn while_condition(lhs: ast::Node<'_>) -> Option<ast::Expr<'_>> {
    for ancestor in lhs.ancestors().skip(1) {
        if let Some(while_expr) = ast::WhileExpr::cast(ancestor) {
            let condition = while_expr.condition()?;
            return condition
                .syntax()
                .text_range()
                .contains_range(lhs.text_range())
                .then_some(condition);
        }
        if ast::Item::can_cast(ancestor.kind()) || ast::AssocItem::can_cast(ancestor.kind()) {
            return None;
        }
    }
    None
}
