//! E0624: a method, an associated function or an associated constant not visible where it is
//! called or named (`x.f()`, `S::f()`, `S::C`).
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/method/suggest.rs` (`report_method_error`,
//! `MethodError::PrivateMatch`) and `compiler/rustc_middle/src/hir/map.rs` (`opt_span`)
//! (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::InFile;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use crate::{
    cx::{Cx, def_range_view as def_range},
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::PrivateAssocItem) -> Vec<Found> {
    let node = cx.node_view(d.expr_or_pat);
    // alloc: empty, allocates nothing
    let Some(range) = name_range(node.value) else { return vec![] };
    let name_span = cx.span(InFile::new(node.file_id, range));
    // alloc: empty, allocates nothing
    let Some(name) = d.item.name(cx.db) else { return vec![] };
    // alloc: the error's text, for an error hir found
    let name = name.display(cx.db, cx.edition).to_string();
    let kind = match d.item {
        hir::AssocItem::Function(f) if f.has_self_param(cx.db) => "method",
        hir::AssocItem::Function(_) => "associated function",
        hir::AssocItem::Const(_) => "associated constant",
        hir::AssocItem::TypeAlias(_) => "associated type",
    };
    // alloc: the error's text, for an error hir found
    let mut diag = Diag::error(Some("E0624"), format!("{kind} `{name}` is private"), name_span.clone());
    // alloc: the error's text, for an error hir found
    diag.span_label(name_span, format!("private {kind}"));
    let defined = match d.item {
        hir::AssocItem::Function(f) => cx.vsema.source(f).map(|it| it.map(|it| def_range(it.syntax()))),
        hir::AssocItem::Const(c) => cx.vsema.source(c).map(|it| it.map(|it| def_range(it.syntax()))),
        hir::AssocItem::TypeAlias(t) => cx.vsema.source(t).map(|it| it.map(|it| def_range(it.syntax()))),
    };
    if let Some(defined) = defined {
        // alloc: the error's text, for an error hir found
        diag.span_label(cx.span(defined), format!("private {kind} defined here"));
    }

    let item = match d.item {
        hir::AssocItem::Function(f) => hir_def::FunctionId::try_from(f).ok().map(ItemId::Function),
        hir::AssocItem::Const(c) => Some(ItemId::Const(c.into())),
        hir::AssocItem::TypeAlias(t) => Some(ItemId::Alias(t.into())),
    };
    if let Some(item) = item {
        // alloc: the error's text, for an error hir found
        diag.fact(Fact::Signature(cx.key(item)), format!("the visibility of `{name}`"));
    }
    if let either::Either::Left(ast::Expr::MethodCallExpr(call)) = node.value
        && let Some(receiver) = call.receiver().and_then(|it| cx.vsema.type_of_expr(it))
    {
        let receiver = receiver.original;
        // alloc: the error's text, for an error hir found
        diag.fact_opt(cx.method_fact(&receiver, &name), format!("what `{name}` is on `{}`", cx.ty(&receiver)));
    }
    code_fact(cx, &mut diag, d.expr_or_pat.map(|it| it.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// The range of the name a method call calls, or of the last segment of a path.
fn name_range(node: either::Either<ast::Expr<'_>, ast::Pat<'_>>) -> Option<TextRange> {
    let path = match node {
        either::Either::Left(ast::Expr::MethodCallExpr(call)) => {
            return Some(call.name_ref()?.syntax().text_range());
        }
        either::Either::Left(ast::Expr::PathExpr(it)) => it.path()?,
        either::Either::Right(ast::Pat::PathPat(it)) => it.path()?,
        either::Either::Right(ast::Pat::IdentPat(it)) => return Some(it.name()?.syntax().text_range()),
        either::Either::Right(ast::Pat::TupleStructPat(it)) => it.path()?,
        either::Either::Right(ast::Pat::RecordPat(it)) => it.path()?,
        either::Either::Left(ast::Expr::RecordExpr(it)) => it.path()?,
        _ => return None,
    };
    Some(path.segment()?.name_ref()?.syntax().text_range())
}
