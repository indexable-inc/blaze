//! E0600 "cannot apply unary operator `-` to type `u32`": a `!` or `-` on a type that does not
//! implement `Not` or `Neg`, with "unsigned values cannot be negated" and the `MAX`
//! suggestion for `-1`, the bound to add to a type parameter, the note on a type that might be
//! missing the impl, and the parentheses after a block rustc's parser ended a statement at.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/op.rs` (`check_user_unop`) and
//! `method/suggest.rs` (`note_unmet_impls_on_type`) (MIT or Apache-2.0).

use hir::{InFile, LangItem};
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use super::binary_operator_cannot_be_applied::{
    code_fact, has_param, note_unmet_impl, parentheses_needed, restrict_param,
};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower<'db>(
    cx: &Cx<'_, 'db>,
    d: &hir::UnaryOperatorCannotBeApplied<'db>,
) -> Vec<Found> {
    let (op, lang) = match d.op {
        ast::UnaryOp::Not => ("!", Some(LangItem::Not)),
        ast::UnaryOp::Neg => ("-", Some(LangItem::Neg)),
        ast::UnaryOp::Deref => ("*", None),
    };
    let span = cx.span_of_ptr(d.expr);
    // clone: the type as rustc prints it, for an error hir found
    let ty = d.at_error.clone().unwrap_or_else(|| cx.ty(&d.found));
    let mut diag = Diag::error(
        Some("E0600"),
        // alloc: the error's text, for an error hir found
        format!("cannot apply unary operator `{op}` to type `{ty}`"),
        // clone: on the error path, for an error hir found
        span.clone(),
    );
    // alloc: the error's text, for an error hir found
    diag.span_label(span.clone(), format!("cannot apply unary operator `{op}`"));
    let trait_ = lang.and_then(|it| hir::Trait::lang(cx.db, cx.krate, it));

    if let Some(trait_) = trait_
        && has_param(cx, &d.found)
    {
        restrict_param(cx, &mut diag, &d.found, trait_, "");
    }

    if !parentheses_needed(cx, d.expr, &mut diag) {
        let unsigned = d.found.as_builtin().is_some_and(|it| it.is_uint());
        if unsigned && d.op == ast::UnaryOp::Neg {
            diag.note("unsigned values cannot be negated");
            max_suggestion(cx, d, &ty, &mut diag);
        } else if let Some(trait_) = trait_ {
            note_unmet_impl(cx, &mut diag, &d.found, trait_, None, true);
        }
    }

    if let Some(trait_) = trait_ {
        let name = cx.def_path(hir::ModuleDef::Trait(trait_));
        // alloc: the error's text, for an error hir found
        diag.fact_opt(cx.implements_fact(trait_, &d.found), format!("whether `{ty}` implements `{name}`"));
    }
    code_fact(cx, d.expr.map(|it| it.syntax_node_ptr()), &mut diag);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// `-1` of an unsigned type, or `-1 as usize`: rustc's "you may have meant the maximum value".
fn max_suggestion(
    cx: &Cx<'_, '_>,
    d: &hir::UnaryOperatorCannotBeApplied<'_>,
    ty: &str,
    diag: &mut Diag,
) {
    let node = cx.node_view(d.expr);
    let Some(ast::Expr::PrefixExpr(prefix)) = node.value.left() else { return };
    let Some(ast::Expr::Literal(literal)) = prefix.expr() else { return };
    let ast::LiteralKind::IntNumber(number) = literal.kind() else { return };
    if number.value().ok() != Some(1) {
        return;
    }
    let mut parent = prefix.syntax().parent();
    while let Some(it) = parent
        && it.kind() == SyntaxKind::PAREN_EXPR
    {
        parent = it.parent();
    }
    let range = match parent.and_then(ast::CastExpr::cast) {
        Some(cast) => cast.syntax().text_range(),
        None => prefix.syntax().text_range(),
    };
    diag.suggestion_verbose(
        cx.span(InFile::new(node.file_id, range)),
        // alloc: the error's text, for an error hir found
        format!("you may have meant the maximum value of `{ty}`"),
        // alloc: the error's text, for an error hir found
        format!("{ty}::MAX"),
        Applicability::MaybeIncorrect,
    );
}
