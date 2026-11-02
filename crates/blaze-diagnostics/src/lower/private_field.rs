//! E0616: `x.f` where `f` is a field of `x`'s struct not visible where it is read, with the
//! method of the same name to call instead when there is one.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`check_expr_field`,
//! `ban_private_field_access`, `private_field_err`) and
//! `compiler/rustc_hir_typeck/src/method/mod.rs` (`suggest_method_call`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::{HasVisibility as _, InFile};
use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{
        Found, Phase,
        unresolved_field::{field_range, in_place, suggest_method_call},
    },
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::PrivateField) -> Vec<Found> {
    let node = cx.node_view(d.expr);
    // alloc: the error's parts, for an error hir found
    let Some(ast::Expr::FieldExpr(field_expr)) = node.value.left() else { return vec![] };
    // alloc: empty, allocates nothing
    let Some(range) = field_range(field_expr) else { return vec![] };
    let field_span = cx.span(InFile::new(node.file_id, range));
    // alloc: the error's text, for an error hir found
    let field = d.field.name(cx.db).display(cx.db, cx.edition).to_string();
    let adt = d.field.parent_def(cx.db).adt(cx.db);
    let kind = match adt {
        hir::Adt::Struct(_) => "struct",
        hir::Adt::Union(_) => "union",
        hir::Adt::Enum(_) => "enum",
    };
    let path = cx.def_path(hir::ModuleDef::Adt(adt));
    // alloc: the error's text, for an error hir found
    let message = format!("field `{field}` of {kind} `{path}` is private");
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some("E0616"), message, field_span.clone());
    diag.span_label(field_span, "private field");

    let receiver = field_expr.expr().and_then(|base| cx.vsema.type_of_expr(base)).map(|it| it.original);
    let scope = cx.vsema.scope(field_expr.syntax());
    let mut outer = ast::Expr::FieldExpr(field_expr);
    while let Some(paren) = outer.syntax().parent().and_then(ast::ParenExpr::cast) {
        outer = ast::Expr::ParenExpr(paren);
    }
    if let (Some(receiver), Some(scope)) = (&receiver, &scope)
        && !in_place(outer)
    {
        let name = d.field.name(cx.db);
        let params = receiver.iterate_method_candidates(cx.db, scope, Some(&name), |f| {
            f.is_visible_from(cx.db, scope.module()).then(|| f.params_without_self(cx.db).len())
        });
        if let Some(params) = params {
            // alloc: the error's text, for an error hir found
            let message = format!("a method `{field}` also exists, call it with parentheses");
            let span = cx.span(InFile::new(node.file_id, outer.syntax().text_range()));
            suggest_method_call(&mut diag, &message, params, span);
        }
    }

    // alloc: the error's text, for an error hir found
    let name = adt.name(cx.db).display(cx.db, cx.edition).to_string();
    diag.fact(
        Fact::Field { ty: cx.key(ItemId::Adt(adt.into())), field: d.field.index() as u32 },
        // alloc: the error's text, for an error hir found
        format!("the visibility of `{name}::{field}`"),
    );
    if let Some(receiver) = &receiver {
        // alloc: the error's text, for an error hir found
        diag.fact_opt(cx.method_fact(receiver, d.field.name(cx.db).as_str()), format!("what `{field}` is on `{}`", cx.ty(receiver)));
    }
    code_fact(cx, &mut diag, d.expr.map(|it| it.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
