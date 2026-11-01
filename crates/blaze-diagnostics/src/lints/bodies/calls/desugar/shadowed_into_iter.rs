//! rustc's `ShadowedIntoIter`: `.into_iter()` on an array before 2021 (`array_into_iter`) or
//! on a boxed slice before 2024 (`boxed_slice_into_iter`), which method resolution takes as
//! `(&x).into_iter()` for compatibility (`#[rustc_skip_during_method_dispatch(array,
//! boxed_slice)]` on `IntoIterator`) and the next edition by value, with what to write
//! instead: `.iter()`, `IntoIterator::into_iter(x)`, or in a `for` loop no call at all.
//!
//! Follows rustc's `compiler/rustc_lint/src/shadowed_into_iter.rs`, the diagnostics of
//! `compiler/rustc_lint/src/lints.rs` (`ShadowedIntoIterDiag`, `ShadowedIntoIterDiagSub`) and
//! `compiler/rustc_span/src/lib.rs` (`Span::find_ancestor_in_same_ctxt`) (MIT or Apache-2.0).

use blaze_hash::HashSet;
use blaze_items::{Fact, ItemId};
use hir_def::{
    FunctionId, HasModule as _, ModuleDefId,
    hir::{Expr, ExprId},
    lang_item::lang_items,
};
use hir_ty::next_solver::{Ty, TyKind};
use rustc_type_ir::inherent::IntoKind as _;
use syntax::view::{self as ast, AstNode};

use super::for_loop;
use crate::{
    lints::bodies::{BodyCx, ExprKind, ExprKinds, Hooks, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

#[derive(Default)]
pub(crate) struct ShadowedIntoIter {
    /// `IntoIterator::into_iter`, as the body's crate sees it.
    into_iter: Option<FunctionId>,
    /// The iterated expressions of the body's `for` loops visited so far: the argument of the
    /// `IntoIterator::into_iter` call each is desugared to (rustc's parent of the parent of a
    /// method call that is one).
    for_heads: HashSet<ExprId>,
}

impl LatePass for ShadowedIntoIter {
    const LINTS: LintSet = lint_set(["array_into_iter", "boxed_slice_into_iter"]);
    const HOOKS: Hooks = Hooks::BODY;
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Match, ExprKind::MethodCall]);

    fn check_body(&mut self, b: &BodyCx<'_, '_, '_>, _out: &mut Vec<Found>) {
        self.for_heads.clear();
        let db = b.db();
        self.into_iter = lang_items(db, b.owner.krate(db)).IntoIterIntoIter;
    }

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Some(into_iter) = self.into_iter else { return };
        if let Some(found) = for_loop(b, expr)
            && util::called_fn(b, found.into_iter).is_some_and(|(def, _)| def == into_iter)
        {
            self.for_heads.insert(found.head);
            return;
        }
        let Expr::MethodCall { receiver, .. } = b.store().expr(expr) else { return };
        if util::method_fn(b, expr) != Some(into_iter) {
            return;
        }

        let receiver_ty = util::ty(b, receiver);
        let adjustments = b.infer.expr_adjustment(receiver).unwrap_or_default();
        let adjusted = || std::iter::once(receiver_ty).chain(adjustments.iter().map(|it| it.target.as_ref()));
        let last = adjustments.last().map_or(receiver_ty, |it| it.target.as_ref());
        let array_at = || adjusted().take_while(|it| !is_ref_to_array(*it)).position(|it| it.is_array());
        let boxed_slice_at =
            || adjusted().take_while(|it| !is_ref_to_boxed_slice(*it)).position(|it| is_boxed_slice(it));
        let (lint, target, edition, can_suggest_ufcs) = if is_ref_to_array(last)
            && let Some(index) = array_at()
        {
            ("array_into_iter", "[T; N]", "2021", index == 0)
        } else if is_ref_to_boxed_slice(last)
            && let Some(index) = boxed_slice_at()
        {
            ("boxed_slice_into_iter", "Box<[T]>", "2024", index == 0)
        } else {
            return;
        };

        let (Some(at), Some(expr_span), Some(name_span)) = (b.expr_node(expr), util::span(b, expr), method_name(b, expr))
        else {
            return;
        };
        let span = util::span(b, receiver).and_then(|it| ancestor_in_same_ctxt(&it, &expr_span));
        // alloc: the lint's diagnostic
        let message = format!(
            "this method call resolves to `<&{target} as IntoIterator>::into_iter` (due to backwards compatibility), but will resolve to `<{target} as IntoIterator>::into_iter` in Rust {edition}"
        );
        // clone: a node or span, for the lint's diagnostic
        let mut diag = Diag::lint(lint, message, name_span.clone());
        diag.suggestion(
            name_span,
            "use `.iter()` instead of `.into_iter()` to avoid ambiguity",
            "iter",
            Applicability::MachineApplicable,
        );
        match span {
            Some(span) if self.for_heads.contains(&expr) => {
                diag.suggestion_with_style(
                    // alloc: the lint's diagnostic
                    vec![(span.shrink_to_hi().to(&expr_span.shrink_to_hi()), String::new())],
                    "or remove `.into_iter()` to iterate by value",
                    Applicability::MaybeIncorrect,
                    SuggestionStyle::ShowCode,
                );
            }
            Some(span) if can_suggest_ufcs => {
                diag.suggestion_with_style(
                    // alloc: the lint's diagnostic
                    vec![
                        // alloc: the lint's diagnostic
                        (expr_span.shrink_to_lo(), "IntoIterator::into_iter(".to_owned()),
                        // alloc: the lint's diagnostic
                        (span.shrink_to_hi().to(&expr_span.shrink_to_hi()), ")".to_owned()),
                    ],
                    "or use `IntoIterator::into_iter(..)` instead of `.into_iter()` to explicitly iterate by value",
                    Applicability::MaybeIncorrect,
                    SuggestionStyle::ShowCode,
                );
            }
            _ => {}
        }
        let called = b.cx.def_path(ModuleDefId::FunctionId(into_iter).into());
        // alloc: the lint's diagnostic
        diag.fact(Fact::Signature(b.cx.key(ItemId::Function(into_iter))), format!("the signature of `{called}`"));
        out.push(b.lint(diag, Phase::Late, at));
    }
}

fn is_ref_to_array(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Ref(_, pointee, _) if pointee.is_array())
}

fn is_boxed_slice(ty: Ty<'_>) -> bool {
    ty.boxed_ty().is_some_and(Ty::is_slice)
}

fn is_ref_to_boxed_slice(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Ref(_, pointee, _) if is_boxed_slice(pointee))
}

/// The span of the name of the method `expr` calls (rustc's `segment.ident.span`).
fn method_name(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<Span> {
    let node = util::syntax(b, expr)?;
    // clone: a node or span, for the lint's diagnostic
    let call = ast::MethodCallExpr::cast(node.value)?;
    let name = call.name_ref()?;
    Some(b.cx.span(node.with_value(name.syntax().text_range())))
}

/// rustc's `Span::find_ancestor_in_same_ctxt`: `span`, or the call of the macro it came
/// from, and of the macro that call came from, .., the first in the expansion `other` is in.
fn ancestor_in_same_ctxt(span: &Span, other: &Span) -> Option<Span> {
    // clone: a node or span, for the lint's diagnostic
    let mut at = span.clone();
    loop {
        if at.expansion == other.expansion {
            return Some(at);
        }
        at = at.expansion?.call_site;
    }
}
