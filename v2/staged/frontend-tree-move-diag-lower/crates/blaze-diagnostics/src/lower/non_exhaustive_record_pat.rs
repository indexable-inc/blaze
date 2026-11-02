//! E0638: a struct pattern without `..` of a struct or variant another crate marked
//! `#[non_exhaustive]`, with rustc's suggestion to add the `..`.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_struct_pat_fields`,
//! `error_foreign_non_exhaustive_spat`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use either::Either;
use syntax::view as ast;

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::NonExhaustiveRecordPat) -> Vec<Found> {
    let span = cx.span_of_ptr(d.pat);
    let descr = match d.variant {
        hir::Variant::Struct(_) => "struct",
        hir::Variant::Union(_) => "union",
        hir::Variant::EnumVariant(_) => "variant",
    };
    let no_fields = match cx.node_view(d.pat).value {
        Either::Right(ast::Pat::RecordPat(it)) => {
            it.record_pat_field_list().is_none_or(|it| it.fields().next().is_none())
        }
        Either::Left(ast::Expr::RecordExpr(it)) => {
            it.record_expr_field_list().is_none_or(|it| it.fields().next().is_none())
        }
        _ => false,
    };
    let mut diag = Diag::error(
        Some("E0638"),
        // alloc: the error's text, for an error hir found
        format!("`..` required with {descr} marked as non-exhaustive"),
        // clone: on the error path, for an error hir found
        span.clone(),
    );
    if span.hi > span.lo {
        let brace = span.with_lo(span.hi - 1);
        let code = if no_fields { ".. }" } else { ", .. }" };
        diag.suggestion_verbose(
            brace,
            "add `..` at the end of the field list to ignore all other fields",
            code,
            Applicability::MachineApplicable,
        );
    }
    let adt = d.variant.adt(cx.db);
    let name = d.variant.name(cx.db);
    diag.fact(
        Fact::Signature(cx.key(ItemId::Adt(adt.into()))),
        // alloc: the error's text, for an error hir found
        format!("whether `{}` is `#[non_exhaustive]`", name.display(cx.db, cx.edition)),
    );
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
