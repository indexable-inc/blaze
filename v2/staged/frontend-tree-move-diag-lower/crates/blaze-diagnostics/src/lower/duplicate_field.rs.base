//! A field named twice: E0062 in a struct expression (`S { x: 0, x: 0 }`), E0025 in a pattern
//! (`S { a: x, a: y }`, an expression on the left of `=` included).
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`check_expr_struct_fields`),
//! `compiler/rustc_hir_typeck/src/pat.rs` (`check_struct_pat_fields`,
//! `error_field_already_bound`) and `compiler/rustc_hir_typeck/src/diagnostics.rs`
//! (`FieldMultiplySpecifiedInInitializer`) (MIT or Apache-2.0).

use either::Either;
use hir::InFile;
use syntax::{AstNode, SyntaxNodePtr};

use crate::{
    cx::{Cx, item_range},
    lints::code_fact,
    lower::{Found, Phase, no_such_field::Record},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::DuplicateField) -> Vec<Found> {
    let node = cx.node(d.field);
    let Some(record) = Record::of_field(node.file_id, &node.value) else { return vec![] };
    let range = match &node.value {
        Either::Left(it) => item_range(it.syntax()),
        Either::Right(it) => item_range(it.syntax()),
    };
    let Some(at) = record.fields.iter().position(|it| it.range == range) else { return vec![] };
    let field = &record.fields[at];
    let Some(first) = record.fields[..at].iter().find(|it| it.name == field.name) else { return vec![] };
    let name = &field.name;
    let first_span = record.span(cx, first.range);
    let mut diag = match record.is_pat {
        false => {
            let ident = record.span(cx, field.ident);
            let mut diag = Diag::error(Some("E0062"), format!("field `{name}` specified more than once"), ident.clone());
            diag.span_label(ident, "used more than once");
            diag
        }
        true => {
            let span = record.span(cx, field.range);
            let message = format!("field `{name}` bound multiple times in the pattern");
            let mut diag = Diag::error(Some("E0025"), message, span.clone());
            diag.span_label(span, format!("multiple uses of `{name}` in pattern"));
            diag
        }
    };
    diag.span_label(first_span, format!("first use of `{name}`"));
    code_fact(cx, &mut diag, InFile::new(record.file, SyntaxNodePtr::new(&record.node)));
    vec![Found::new(diag, Phase::Bodies)]
}
