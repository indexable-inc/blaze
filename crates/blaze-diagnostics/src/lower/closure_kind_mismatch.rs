//! E0525: a closure that implements fewer of `Fn`, `FnMut` and `FnOnce` than the bound it is
//! given to asks for, labelled at the closure's head, at the use that made it `FnOnce` or
//! `FnMut`, and at the argument the requirement comes from, with the obligation cause's notes
//! (`obligation_cause.rs`: "required by a bound in `f`").
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/traits/
//! fulfillment_errors.rs` (`report_closure_error`), `compiler/rustc_trait_selection/src/
//! errors.rs` (`ClosureKindMismatch`, `ClosureFnOnceLabel`, `ClosureFnMutLabel`) and
//! `compiler/rustc_hir_typeck/src/upvar.rs` (`closure_kind_origins`) (MIT or Apache-2.0).

use hir::FnTrait;
use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::{Cx, closure_decl_range_view as closure_decl_range},
    lints::code_fact,
    lower::{Found, Phase, obligation_cause, unimplemented_trait::Cause},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ClosureKindMismatch) -> Vec<Found> {
    let (expected, found) = (d.expected, d.found);
    let at = cx.syntax_view(d.span.map(|it| it.syntax_node_ptr()));
    let cause = Cause::of(cx, at, |ty| ty.as_closure().is_some_and(|it| it.fn_trait(cx.db) == found));
    let closure = d.closure.map(|it| cx.node_view(it));
    let head = closure.map(|it| cx.span(it.with_value(closure_decl_range(it.value.syntax()))));
    let mut diag = Diag::error(
        Some("E0525"),
        // alloc: the error's message
        format!(
            "expected a closure that implements the `{expected}` trait, but this closure only \
             implements `{found}`"
        ),
        head.clone().unwrap_or_else(|| cause.span.clone()), // clone: the error's span
    );
    if let Some(head) = head {
        // alloc: the error's label
        diag.span_label(head, format!("this closure implements `{found}`, not `{expected}`"));
    }
    if let Some(closure) = closure
        && let Some(ty) = cx.vsema.type_of_expr(ast::Expr::ClosureExpr(closure.value))
        && let Some(origin) = ty.original.as_closure()
        && let Some((node, _)) = origin.kind_origin(cx.db)
        && let Some(place) = origin.kind_origin_place(cx.db)
    {
        let span = cx.span_of(node.map(|it| it.syntax_node_ptr()));
        let label = match found {
            FnTrait::FnOnce | FnTrait::AsyncFnOnce => {
                // alloc: the error's label
                Some(format!("closure is `{found}` because it moves the variable `{place}` out of its environment"))
            }
            FnTrait::FnMut | FnTrait::AsyncFnMut => {
                // alloc: the error's label
                Some(format!("closure is `{found}` because it mutates the variable `{place}` here"))
            }
            FnTrait::Fn | FnTrait::AsyncFn => None,
        };
        if let Some(label) = label {
            diag.span_label(span, label);
        }
    }
    // alloc: the error's label
    let label = format!("the requirement to implement `{expected}` derives from here");
    diag.span_label(cause.span.clone(), label); // clone: the label's span
    cause.label_call(&mut diag);
    let trait_ = hir::Trait::lang(cx.db, cx.krate, expected.lang_item());
    obligation_cause::note(cx, &mut diag, d.cause, trait_);
    code_fact(cx, &mut diag, d.span.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)] // alloc: the error found
}
