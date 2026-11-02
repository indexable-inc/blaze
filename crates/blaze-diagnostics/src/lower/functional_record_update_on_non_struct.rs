//! E0436: `..base` in the struct expression of an enum's variant or a union, which only a
//! struct's may have.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`check_expr_struct_fields`) and
//! `compiler/rustc_hir_typeck/src/diagnostics.rs` (`FunctionalRecordUpdateOnNonStruct`)
//! (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::FunctionalRecordUpdateOnNonStruct) -> Vec<Found> {
    let span = cx.span_of_ptr(d.base_expr);
    let mut diag = Diag::error(Some("E0436"), "functional record update syntax requires a struct", span);
    let record = cx
        .node_view(d.base_expr)
        .value
        .left()
        .and_then(|base| base.syntax().ancestors().find_map(ast::RecordExpr::cast));
    if let Some(variant) = record.and_then(|it| cx.vsema.resolve_variant(it)) {
        let adt = variant.adt(cx.db);
        // alloc: a name for the error
        let name = adt.name(cx.db).display(cx.db, cx.edition).to_string();
        // alloc: the error's fact
        diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("what kind of type `{name}` is"));
    }
    code_fact(cx, &mut diag, d.base_expr.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)] // alloc: the error found
}
