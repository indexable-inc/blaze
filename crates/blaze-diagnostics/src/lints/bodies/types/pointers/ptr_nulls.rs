//! `useless_ptr_null_checks`: a function pointer or reference cast to a raw pointer, or the
//! pointer a function marked `#[rustc_never_returns_null_ptr]` returns, checked for null
//! (`.is_null()`, `<*const _>::is_null(..)`, `== ptr::null()`, `== 0 as *const _`).
//! `invalid_null_arguments` (deny): a null pointer passed where std's pointer and slice
//! functions require one that is not (`ptr::read`, `ptr::write`, `slice::from_raw_parts`, ..).
//!
//! Follows rustc's `compiler/rustc_lint/src/ptr_nulls.rs`, `compiler/rustc_lint/src/utils.rs`
//! (`peel_casts`) and `compiler/rustc_lint/src/lints.rs` (`UselessPtrNullChecksDiag`,
//! `InvalidNullArgumentsDiag`) (MIT or Apache-2.0).

use blaze_hash::HashMap;
use hir_def::{
    hir::{BinaryOp, BindingId, CmpOp, Expr, ExprId, type_ref::TypeRef},
    signatures::FunctionSignature,
};
use hir_ty::next_solver::TyKind;
use intern::Symbol;
use rustc_type_ir::inherent::IntoKind as _;

use super::{
    Inits, callee_fn, callee_item, has_attr, is_zero, layout, method_item, peel_casts, span,
    ty_text,
};
use crate::{
    lints::bodies::{BodyCx, ExprKind, ExprKinds, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::{Diag, Span},
};

const NULL_ARGUMENTS_HELP: &str = "for more information, visit <https://doc.rust-lang.org/std/ptr/index.html> and <https://doc.rust-lang.org/reference/behavior-considered-undefined.html>";

#[derive(Default)]
pub(crate) struct PtrNullChecks {
    inits: Inits,
}

/// What a useless null check checks: rustc's `UselessPtrNullChecksDiag`.
enum Useless {
    FnPtr { orig_ty: String, label: Span },
    Ref { orig_ty: String, label: Span },
    FnRet { fn_name: String },
}

impl LatePass for PtrNullChecks {
    const LINTS: LintSet = lint_set(["useless_ptr_null_checks", "invalid_null_arguments"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Call, ExprKind::MethodCall, ExprKind::BinaryOp]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let store = b.store();
        match store.expr(expr) {
            Expr::Call { callee, args } => {
                // Every function of rustc's tables takes a raw pointer first.
                let Some(&first) = args.first() else { return };
                if !matches!(util::ty_adjusted(b, first).kind(), TyKind::RawPtr(..)) {
                    return;
                }
                let Some(name) = callee_item(b, callee) else { return };
                let items = util::items();
                if (name == items.ptr_const_is_null || name == items.ptr_is_null)
                    && let [arg] = args
                    && let Some(useless) = useless_check(b, *arg)
                {
                    emit_useless(b, expr, useless, out);
                    return;
                }
                let first: [Symbol; 6] = [
                    items.ptr_read,
                    items.ptr_read_unaligned,
                    items.ptr_replace,
                    items.ptr_write,
                    items.ptr_write_bytes,
                    items.ptr_write_unaligned,
                ];
                let both = [items.ptr_copy, items.ptr_copy_nonoverlapping, items.ptr_swap, items.ptr_swap_nonoverlapping];
                let (arg_indices, are_zsts_allowed): (&[usize], bool) = if first.contains(&name) {
                    (&[0], true)
                } else if name == items.slice_from_raw_parts || name == items.slice_from_raw_parts_mut {
                    (&[0], false)
                } else if both.contains(&name) {
                    (&[0, 1], true)
                } else {
                    return;
                };
                let inits = self.inits.of(b);
                for &index in arg_indices {
                    let Some(&arg) = args.get(index) else { continue };
                    let Some(null_span) = is_null_ptr(b, inits, arg) else { continue };
                    let TyKind::RawPtr(pointee, _) = util::ty(b, arg).kind() else { continue };
                    if are_zsts_allowed && layout(b, pointee).is_some_and(|it| it.is_1zst) {
                        break;
                    }
                    let (Some(at), Some(call_span), Some(arg_span)) = (b.expr_node(expr), span(b, expr), span(b, arg))
                    else {
                        continue;
                    };
                    let mut diag = Diag::lint(
                        "invalid_null_arguments",
                        "calling this function with a null pointer is undefined behavior, even if the result of the function is unused",
                        call_span,
                    );
                    let inline =
                        arg_span.file == null_span.file && arg_span.lo <= null_span.lo && null_span.hi <= arg_span.hi;
                    diag.help(NULL_ARGUMENTS_HELP);
                    if inline {
                        diag.span_label(null_span, "null pointer originates from here");
                    } else {
                        diag.span_note(null_span, "null pointer originates from here");
                    }
                    out.push(b.lint(diag, Phase::Late, at));
                }
            }
            Expr::MethodCall { receiver, args: &[], .. } => {
                if matches!(util::ty_adjusted(b, receiver).kind(), TyKind::RawPtr(..))
                    && method_item(b, expr).is_some_and(|it| it == util::items().ptr_const_is_null || it == util::items().ptr_is_null)
                    && let Some(useless) = useless_check(b, receiver)
                {
                    emit_useless(b, expr, useless, out);
                }
            }
            Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::CmpOp(CmpOp::Eq { negated: false })) } => {
                let (to_check, useless) = match useless_check(b, lhs) {
                    Some(useless) => (rhs, useless),
                    None => match useless_check(b, rhs) {
                        Some(useless) => (lhs, useless),
                        None => return,
                    },
                };
                let is_null = match store.expr(to_check) {
                    Expr::Cast { expr: inner, .. } => is_zero(b, inner),
                    Expr::Call { callee, args } => {
                        args.is_empty() && callee_item(b, callee).is_some_and(|it| it == util::items().ptr_null || it == util::items().ptr_null_mut)
                    }
                    _ => false,
                };
                if is_null {
                    emit_useless(b, expr, useless, out);
                }
            }
            _ => {}
        }
    }
}

fn emit_useless(b: &BodyCx<'_, '_, '_>, expr: ExprId, useless: Useless, out: &mut Vec<Found>) {
    let (Some(at), Some(span)) = (b.expr_node(expr), span(b, expr)) else { return };
    let diag = match useless {
        Useless::FnPtr { orig_ty, label } => {
            let mut diag = Diag::lint(
                "useless_ptr_null_checks",
                "function pointers are not nullable, so checking them for null will always return false",
                span,
            );
            diag.help("wrap the function pointer inside an `Option` and use `Option::is_none` to check for null pointer value");
            // alloc: the lint's text, for a lint found
            diag.span_label(label, format!("expression has type `{orig_ty}`"));
            diag
        }
        Useless::Ref { orig_ty, label } => {
            let mut diag = Diag::lint(
                "useless_ptr_null_checks",
                "references are not nullable, so checking them for null will always return false",
                span,
            );
            // alloc: the lint's text, for a lint found
            diag.span_label(label, format!("expression has type `{orig_ty}`"));
            diag
        }
        Useless::FnRet { fn_name } => Diag::lint(
            "useless_ptr_null_checks",
            // alloc: the lint's text, for a lint found
            format!("returned pointer of `{fn_name}` call is never null, so checking it for null will always return false"),
            span,
        ),
    };
    out.push(b.lint(diag, Phase::Late, at));
}

/// rustc's `useless_check`: `e` is, through casts to raw pointers and `.cast()`/`.cast_mut()`,
/// a function pointer or a reference (after at least one cast), or a call of a function
/// marked `#[rustc_never_returns_null_ptr]`.
fn useless_check(b: &BodyCx<'_, '_, '_>, mut e: ExprId) -> Option<Useless> {
    let store = b.store();
    let mut had_at_least_one_cast = false;
    loop {
        e = util::peel_blocks(b, e);
        let never_null = match store.expr(e) {
            Expr::MethodCall { args, .. } if args.is_empty() => util::method_fn(b, e),
            Expr::Call { callee, .. } => callee_fn(b, callee),
            _ => None,
        };
        if let Some(def) = never_null
            && has_attr(b.db(), def, "rustc_never_returns_null_ptr")
        {
            // alloc: the lint's text, for a lint found
            let fn_name = FunctionSignature::of(b.db(), def).name.as_str().to_owned();
            return Some(Useless::FnRet { fn_name });
        }
        e = match store.expr(e) {
            Expr::Cast { expr, type_ref } if matches!(store.type_ref(type_ref), TypeRef::RawPtr(..)) => {
                had_at_least_one_cast = true;
                expr
            }
            Expr::MethodCall { receiver, args, .. }
                if args.is_empty() && method_item(b, e).is_some_and(|it| it == util::items().ptr_cast || it == util::items().ptr_cast_mut) =>
            {
                had_at_least_one_cast = true;
                receiver
            }
            _ if had_at_least_one_cast => {
                let orig = util::ty(b, e);
                return if matches!(orig.kind(), TyKind::FnDef(..) | TyKind::FnPtr(..)) {
                    Some(Useless::FnPtr { orig_ty: ty_text(b, e)?, label: span(b, e)? })
                } else if orig.is_ref() {
                    Some(Useless::Ref { orig_ty: ty_text(b, e)?, label: span(b, e)? })
                } else {
                    None
                };
            }
            _ => return None,
        };
    }
}

/// rustc's `is_null_ptr` of this pass: through `peel_casts`, a call of `ptr::null()` or
/// `ptr::null_mut()`, or the literal `0`, with where it is.
fn is_null_ptr(b: &BodyCx<'_, '_, '_>, inits: &HashMap<BindingId, ExprId>, expr: ExprId) -> Option<Span> {
    let expr = peel_casts(b, inits, expr);
    let is_null = match b.store().expr(expr) {
        Expr::Call { callee, args } if args.is_empty() => {
            callee_item(b, callee).is_some_and(|it| it == util::items().ptr_null || it == util::items().ptr_null_mut)
        }
        Expr::Call { .. } => false,
        _ => is_zero(b, expr),
    };
    if is_null { span(b, expr) } else { None }
}
