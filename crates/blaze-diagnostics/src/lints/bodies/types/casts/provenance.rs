//! `ImplicitProvenanceCasts`: `implicit_provenance_casts`, an `as` cast from a raw pointer to
//! an integer (which exposes the pointer's provenance) or from an integer to a raw pointer
//! (which takes exposed provenance).
//!
//! Follows rustc's `compiler/rustc_lint/src/implicit_provenance_casts.rs`,
//! `compiler/rustc_lint/src/lints.rs` (`ImplicitProvenanceCastsPtr2Int`,
//! `ImplicitProvenanceCastsInt2Ptr`, `Ptr2IntSuggestion`, `Int2PtrSuggestion`),
//! `compiler/rustc_lint/src/context.rs` (`LateContext::precedence`) and
//! `compiler/rustc_hir/src/hir.rs` (`Expr::precedence`) (MIT or Apache-2.0).

use hir_def::hir::{Expr, ExprId};
use hir_ty::next_solver::{Ty, TyKind};
use rustc_type_ir::{InferTy, inherent::IntoKind as _};
use syntax::SyntaxKind;

use super::{can_be_used_for_suggestions, cast_ty_span, expr_span, ty_text};
use crate::{
    lints::bodies::{BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set},
    lower::{Found, Phase},
    model::{Applicability, Diag, SuggestionStyle},
};

const NOTE: &str = "for more information, visit <https://doc.rust-lang.org/std/ptr/index.html#provenance>";

#[derive(Default)]
pub(crate) struct ImplicitProvenanceCasts;

impl LatePass for ImplicitProvenanceCasts {
    const LINTS: LintSet = lint_set(["implicit_provenance_casts"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Cast]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Expr::Cast { expr: from, .. } = b.store().expr(expr) else { return };
        let from_ty = b.infer.expr_ty(from);
        if from_ty.is_raw_ptr() {
            let to_ty = b.infer.expr_ty(expr);
            if is_integral(to_ty) {
                ptr2int(b, expr, from, to_ty, out);
            }
        } else if is_integral(from_ty) && b.infer.expr_ty(expr).is_raw_ptr() {
            int2ptr(b, expr, from, out);
        }
    }
}

/// rustc's `Ty::is_integral`.
fn is_integral(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_) | TyKind::Infer(InferTy::IntVar(_)))
}

/// rustc's `lint_ptr2int`.
fn ptr2int(b: &BodyCx<'_, '_, '_>, expr: ExprId, from: ExprId, to_ty: Ty<'_>, out: &mut Vec<Found>) {
    let (Some(at), Some(span)) = (b.expr_node(expr), expr_span(b, expr)) else { return };
    let (Some(from_text), Some(to_text)) = (ty_text(b, from), ty_text(b, expr)) else { return };
    // alloc: the lint's text, for a lint found
    let message = format!("cast from `{from_text}` to `{to_text}` implicitly exposes pointer provenance");
    // clone: a lint's span, for a lint found
    let mut diag = Diag::lint("implicit_provenance_casts", message, span.clone());
    diag.help("if conforming to strict provenance is not possible, use `.expose_provenance()`");
    diag.note(NOTE);
    if can_be_used_for_suggestions(&span)
        && let (Some(from_span), Some(ty_span)) = (expr_span(b, from), cast_ty_span(b, expr))
    {
        let message = "use `.addr()` to obtain the address of a pointer";
        let cast_span = from_span.shrink_to_hi().to(&ty_span);
        let lo = from_span.shrink_to_lo();
        let applicability = Applicability::MaybeIncorrect;
        match (!unambiguous(b, from), !to_ty.is_usize()) {
            (true, true) => {
                // alloc: the lint's text, for a lint found
                let parts = vec![(lo, "(".to_owned()), (cast_span, format!(").addr() as {to_text}"))];
                diag.suggestion_with_style(parts, message, applicability, SuggestionStyle::ShowCode)
            }
            (true, false) => {
                // alloc: the lint's text, for a lint found
                let parts = vec![(lo, "(".to_owned()), (cast_span, ").addr()".to_owned())];
                diag.suggestion_with_style(parts, message, applicability, SuggestionStyle::ShowCode)
            }
            // alloc: the lint's text, for a lint found
            (false, true) => diag.suggestion(cast_span, message, format!(".addr() as {to_text}"), applicability),
            (false, false) => diag.suggestion(cast_span, message, ".addr()", applicability),
        };
    }
    out.push(b.lint(diag, Phase::Late, at));
}

/// rustc's `lint_int2ptr`.
fn int2ptr(b: &BodyCx<'_, '_, '_>, expr: ExprId, from: ExprId, out: &mut Vec<Found>) {
    let (Some(at), Some(span)) = (b.expr_node(expr), expr_span(b, expr)) else { return };
    let (Some(from_text), Some(to_text)) = (ty_text(b, from), ty_text(b, expr)) else { return };
    // alloc: the lint's text, for a lint found
    let message = format!("cast from `{from_text}` to `{to_text}` implicitly relies on exposed provenance");
    // clone: a lint's span, for a lint found
    let mut diag = Diag::lint("implicit_provenance_casts", message, span.clone());
    diag.help("if conforming to strict provenance is not possible, use `std::ptr::with_exposed_provenance()`");
    diag.note(NOTE);
    if can_be_used_for_suggestions(&span)
        && let (Some(from_span), Some(ty_span)) = (expr_span(b, from), cast_ty_span(b, expr))
    {
        // alloc: the lint's text, for a lint found
        let parts = vec![
            // alloc: the lint's text, for a lint found
            (from_span.shrink_to_lo(), "(...).with_addr(".to_owned()),
            // alloc: the lint's text, for a lint found
            (from_span.shrink_to_hi().to(&ty_span), ")".to_owned()),
        ];
        diag.suggestion_with_style(
            parts,
            "use `.with_addr()` to adjust the address of a valid pointer in the same allocation",
            Applicability::HasPlaceholders,
            SuggestionStyle::ShowCode,
        );
    }
    out.push(b.lint(diag, Phase::Late, at));
}

/// Whether rustc's `LateContext::precedence` of the HIR expression `expr` is at least
/// `ExprPrecedence::Unambiguous`: it is not a jump, a binary operation, a cast, an assignment
/// or a prefix operation, and no attribute is written on it. A range is a struct expression
/// in rustc's HIR, and `?` and `.await` are matches, so they are unambiguous.
fn unambiguous(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> bool {
    match b.store().expr(expr) {
        Expr::Closure { ret_type: None, .. }
        | Expr::Break { .. }
        | Expr::Return { .. }
        | Expr::Yield { .. }
        | Expr::Yeet { .. }
        | Expr::Become { .. }
        | Expr::BinaryOp { .. }
        | Expr::Cast { .. }
        | Expr::Assignment { .. }
        | Expr::Ref { .. }
        | Expr::Let { .. }
        | Expr::UnaryOp { .. } => false,
        _ => match b.expr_node(expr) {
            Some(node) => !b.cx.syntax_view(node).value.children().any(|it| it.kind() == SyntaxKind::ATTR),
            None => true,
        },
    }
}
