//! `FunctionCastsAsInteger`: `function_casts_as_integer`, a function item (a function, a tuple
//! struct's or variant's constructor) cast straight to an integer.
//!
//! Follows rustc's `compiler/rustc_lint/src/function_cast_as_integer.rs` and
//! `compiler/rustc_lint/src/lints.rs` (`FunctionCastsAsIntegerDiag`,
//! `FunctionCastsAsIntegerSugg`) (MIT or Apache-2.0).

use hir_def::hir::{Expr, ExprId};
use hir_ty::next_solver::TyKind;
use rustc_type_ir::inherent::IntoKind as _;

use super::{cast_ty_span, expr_span};
use crate::{
    lints::bodies::{BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set},
    lower::{Found, Phase},
    model::{Applicability, Diag, SuggestionStyle},
};

#[derive(Default)]
pub(crate) struct FunctionCastsAsInteger;

impl LatePass for FunctionCastsAsInteger {
    const LINTS: LintSet = lint_set(["function_casts_as_integer"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Cast]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Expr::Cast { expr: from, .. } = b.store().expr(expr) else { return };
        if matches!(b.infer.expr_ty(expr).kind(), TyKind::FnDef(..) | TyKind::FnPtr(..) | TyKind::RawPtr(..)) {
            return;
        }
        if !matches!(b.infer.expr_ty(from).kind(), TyKind::FnDef(..)) {
            return;
        }
        let (Some(at), Some(from_span), Some(ty_span)) = (b.expr_node(expr), expr_span(b, from), cast_ty_span(b, expr))
        else {
            return;
        };
        let span = ty_span.with_lo(from_span.hi + 1);
        let mut diag = Diag::lint("function_casts_as_integer", "direct cast of function item into an integer", span);
        diag.suggestion_with_style(
            // alloc: the lint's suggestion, for a lint found
            vec![(from_span.shrink_to_hi(), " as *const ()".to_owned())],
            "first cast to a pointer `as *const ()`",
            Applicability::MachineApplicable,
            SuggestionStyle::ShowAlways,
        );
        out.push(b.lint(diag, Phase::Late, at));
    }
}
