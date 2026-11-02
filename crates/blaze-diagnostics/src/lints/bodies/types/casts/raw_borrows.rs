//! `RawBorrowsViaReferences`: `raw_borrows_via_references`, a reference to a place cast straight
//! to a raw pointer (`&x as *const T`), where `&raw const x` makes the pointer with no reference.
//!
//! Follows rustc's `compiler/rustc_lint/src/raw_borrows_via_references.rs`,
//! `compiler/rustc_lint/src/lints.rs` (`RawBorrowViaReference`,
//! `RawBorrowViaReferenceSuggestion`) and `compiler/rustc_hir/src/hir.rs`
//! (`Expr::is_syntactic_place_expr`) (MIT or Apache-2.0).

use hir_def::{
    hir::{
        Expr, ExprId, UnaryOp,
        type_ref::{Mutability, Rawness, TypeRef},
    },
    resolver::ValueNs,
};

use super::{ancestor_in_same_ctxt, can_be_used_for_suggestions, cast_ty_span, expr_span};
use crate::{
    lints::bodies::{
        BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set,
        types::util::path_value,
    },
    lower::{Found, Phase},
    model::{Applicability, Diag, SuggestionStyle},
};

const MESSAGE: &str =
    "creating an intermediate reference implies aliasing requirements even when immediately cast to a raw pointers";

#[derive(Default)]
pub(crate) struct RawBorrowsViaReferences;

impl LatePass for RawBorrowsViaReferences {
    const LINTS: LintSet = lint_set(["raw_borrows_via_references"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Cast]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let store = b.store();
        let Expr::Cast { expr: borrow, type_ref } = store.expr(expr) else { return };
        let Expr::Ref { expr: place, rawness: Rawness::Ref, mutability } = store.expr(borrow) else { return };
        let mutbl = if mutability == Mutability::Mut { "mut" } else { "const" };
        if !matches!(store.type_ref(type_ref), TypeRef::RawPtr(..)) || !is_syntactic_place(b, place) {
            return;
        }
        let (Some(at), Some(span)) = (b.expr_node(expr), expr_span(b, expr)) else { return };
        // clone: a lint's span, for a lint found
        let mut diag = Diag::lint("raw_borrows_via_references", MESSAGE, span.clone());
        // alloc: the lint's text, for a lint found
        let message = format!("consider using `&raw {mutbl}` for a safer and more explicit raw pointer");
        let place_span = expr_span(b, place).and_then(|it| ancestor_in_same_ctxt(&it, &span));
        let ty_span = cast_ty_span(b, expr).and_then(|it| ancestor_in_same_ctxt(&it, &span));
        match (place_span, ty_span) {
            (Some(place_span), Some(ty_span))
                if can_be_used_for_suggestions(&span)
                    && can_be_used_for_suggestions(&place_span)
                    && can_be_used_for_suggestions(&ty_span) =>
            {
                // rustc's `Span::until`: from the lower start to the higher one.
                let left = span.with_lo(span.lo.min(place_span.lo)).with_hi(span.lo.max(place_span.lo));
                let right = place_span.with_lo(place_span.hi.min(ty_span.hi)).with_hi(place_span.hi.max(ty_span.hi));
                diag.suggestion_with_style(
                    // alloc: the lint's text, for a lint found
                    vec![(left, format!("&raw {mutbl} ")), (right, String::new())],
                    message,
                    Applicability::MachineApplicable,
                    SuggestionStyle::ShowCode,
                );
            }
            _ => {
                diag.help(message);
            }
        }
        out.push(b.lint(diag, Phase::Late, at));
    }
}

/// rustc's `Expr::is_syntactic_place_expr`: a path to a local or a static, a dereference, or a
/// field or an index of anything.
fn is_syntactic_place(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> bool {
    match b.store().expr(expr) {
        Expr::Path(_) => matches!(path_value(b, expr), Some(ValueNs::LocalBinding(_) | ValueNs::StaticId(_))),
        Expr::UnaryOp { op: UnaryOp::Deref, .. } | Expr::Field { .. } | Expr::Index { .. } => true,
        _ => false,
    }
}
