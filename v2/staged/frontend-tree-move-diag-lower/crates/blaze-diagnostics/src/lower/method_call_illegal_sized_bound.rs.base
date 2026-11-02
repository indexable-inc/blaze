//! "the `f` method cannot be invoked on a trait object": a method call on a trait object of a
//! method with a `Self: Sized` bound, on the method's name, with "this has a `Sized`
//! requirement" on the bound.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/method/mod.rs` (`lookup_method`),
//! `method/confirm.rs` (`predicates_require_illegal_sized_bound`) and `method/suggest.rs`
//! (`report_method_error`, `MethodError::IllegalSizedBound`) (MIT or Apache-2.0).

use hir::{HasSource as _, InFile, LangItem, PathResolution};
use syntax::{
    AstNode, AstPtr,
    ast::{self, HasGenericParams as _, HasTypeBounds as _},
};

use super::binary_operator_cannot_be_applied::code_fact;
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Diag, Span},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MethodCallIllegalSizedBound) -> Vec<Found> {
    let node = cx.node(d.call_expr);
    let call = match node.value.clone().left() {
        Some(ast::Expr::MethodCallExpr(it)) => Some(it),
        _ => None,
    };
    let name = call.as_ref().and_then(|it| it.name_ref());
    let span = match &name {
        Some(name) => cx.span(InFile::new(node.file_id, name.syntax().text_range())),
        None => cx.span_of_ptr(d.call_expr),
    };
    let method = name.as_ref().map_or_else(String::new, |it| it.text().to_string());
    let mut diag =
        Diag::error(None, format!("the `{method}` method cannot be invoked on a trait object"), span);
    let function = call.as_ref().and_then(|it| cx.sema.resolve_method_call(it));
    if let Some(bound) = function.and_then(|it| sized_bound(cx, it)) {
        diag.span_label(bound, "this has a `Sized` requirement");
    }
    code_fact(cx, d.call_expr.map(|it| it.syntax_node_ptr()), &mut diag);
    vec![Found::new(diag, Phase::Bodies)]
}

/// The bound of `function`'s where clause on `Self` that implies `Sized`.
fn sized_bound(cx: &Cx<'_, '_>, function: hir::Function) -> Option<Span> {
    let sized = hir::Trait::lang(cx.db, cx.krate, LangItem::Sized)?;
    let source = function.source(cx.db)?;
    let file = source.file_id;
    let source = cx.node(source.map(|it| AstPtr::new(&it))).value;
    let where_clause = source.where_clause()?;
    for pred in where_clause.predicates() {
        let is_self = matches!(
            pred.ty(),
            Some(ast::Type::PathType(it)) if it.path().is_some_and(|it| it.syntax().text() == "Self")
        );
        if !is_self {
            continue;
        }
        for bound in pred.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
            if bound.question_mark_token().is_some() {
                continue;
            }
            let Some(ast::Type::PathType(ty)) = bound.ty() else { continue };
            let Some(PathResolution::Def(hir::ModuleDef::Trait(trait_))) =
                ty.path().and_then(|it| cx.sema.resolve_path(&it))
            else {
                continue;
            };
            if trait_ == sized || trait_.all_supertraits(cx.db).contains(&sized) {
                return Some(cx.span(InFile::new(file, ty.syntax().text_range())));
            }
        }
    }
    None
}
