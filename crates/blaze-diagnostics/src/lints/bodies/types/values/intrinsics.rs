//! rustc's `EnumIntrinsicsNonEnums` (`enum_intrinsics_non_enums`): `mem::discriminant` and
//! `mem::variant_count` of a type that is not an enum.
//!
//! Follows rustc's `compiler/rustc_lint/src/enum_intrinsics_non_enums.rs` and
//! `compiler/rustc_lint/src/lints.rs` (`EnumIntrinsicsMemDiscriminate`,
//! `EnumIntrinsicsMemVariant`) (MIT or Apache-2.0).

use hir_def::hir::{Expr, ExprId};
use hir_ty::next_solver::{Ty, TyKind};
use rustc_type_ir::{
    TypeVisitableExt as _,
    inherent::{GenericArgs as _, IntoKind as _},
};

use crate::{
    lints::bodies::{BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::Diag,
};

#[derive(Default)]
pub(crate) struct EnumIntrinsicsNonEnums;

impl LatePass for EnumIntrinsicsNonEnums {
    const LINTS: LintSet = lint_set(["enum_intrinsics_non_enums"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Call]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let store = b.store();
        let Expr::Call { callee, args } = store.expr(expr) else { return };
        // `mem::discriminant` takes one argument, `mem::variant_count` none.
        if args.len() > 1 {
            return;
        }
        let Expr::Path(_) = store.expr(callee) else { return };
        let Some(name) = util::path_item(b, callee) else { return };
        let items = util::items();
        let discriminant = if name == items.mem_discriminant {
            true
        } else if name == items.mem_variant_count {
            false
        } else {
            return;
        };
        let TyKind::FnDef(_, fn_args) = util::ty(b, callee).kind() else { return };
        let ty_param = fn_args.skip_binder().type_at(0);
        if !is_non_enum(ty_param) {
            return;
        }
        let (Some(span), Some(node), Some(ty)) = (util::span(b, expr), b.expr_node(expr), super::print(b, expr, ty_param))
        else {
            return;
        };
        let diag = if discriminant {
            let Some(arg_span) = args.first().and_then(|&it| util::span(b, it)) else { return };
            let mut diag = Diag::lint(
                "enum_intrinsics_non_enums",
                "the return value of `mem::discriminant` is unspecified when called with a non-enum type",
                span,
            );
            diag.span_note(
                arg_span,
                // alloc: the lint's text, for a lint found
                format!(
                    "the argument to `discriminant` should be a reference to an enum, but it was passed a reference to a `{ty}`, which is not an enum"
                ),
            );
            diag
        } else {
            let mut diag = Diag::lint(
                "enum_intrinsics_non_enums",
                "the return value of `mem::variant_count` is unspecified when called with a non-enum type",
                span,
            );
            // alloc: the lint's text, for a lint found
            diag.note(format!(
                "the type parameter of `variant_count` should be an enum, but it was instantiated with the type `{ty}`, which is not an enum"
            ));
            diag
        };
        out.push(b.lint(diag, Phase::Late, node));
    }
}

/// rustc's `is_non_enum`: not an enum, nor generic. The regions of rustc's type checking
/// results are erased, so a lifetime parameter does not count.
fn is_non_enum(ty: Ty<'_>) -> bool {
    let is_enum = matches!(ty.kind(), TyKind::Adt(def, _) if def.is_enum());
    !is_enum && !ty.has_non_region_param()
}
