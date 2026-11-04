//! What rustc's `check_tail_calls` refuses of a `become`, and the lint
//! `tail_call_track_caller` of a tail call of a `#[track_caller]` function.
//!
//! Follows rustc's `compiler/rustc_mir_build/src/check_tail_calls.rs` (the `report_*`
//! functions) and `compiler/rustc_codegen_ssa/src/mir/block.rs` (the lint) (MIT or
//! Apache-2.0). A HIR expression's span in rustc covers the parentheses written around it, so
//! each span here is widened to them.

use hir::{InFile, TailCallErrorKind, TailCallSite};
use hir_def::expr_store::ExprOrPatPtr;
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lints::{code_fact, registry::LintId},
    lower::{Found, Phase, break_outside_of_loop::up},
    model::{Applicability, Diag, Span},
    print,
};

const TRACK_CALLER: LintId = LintId::named("tail_call_track_caller");

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TailCallRefused) -> Vec<Found> {
    use TailCallErrorKind::*;
    let at = d.become_expr.map(|it| it.syntax_node_ptr());
    let become_span = expr_span(cx, d.become_expr);
    let message = d.kind.message();
    let mut diag = match &d.kind {
        InClosure | Intrinsic | TrackCallerCaller | CVariadicCaller | CVariadicCallee => {
            Diag::error(None, message, become_span)
        }
        BuiltinOp { value } => {
            let value = expr_span(cx, *value);
            // clone: a handle, on the error path, for a refused tail call
            let mut diag = Diag::error(None, message, value.clone());
            diag.note("using `become` on a builtin operator is not useful");
            diag.suggestion(
                become_span.with_hi(value.lo),
                "try using `return` instead",
                "return ",
                Applicability::MachineApplicable,
            );
            diag
        }
        NonCall { value } => {
            let value = expr_span(cx, *value);
            // clone: a handle, on the error path, for a refused tail call
            let mut diag = Diag::error(None, message, value.clone());
            // clone: a handle, on the error path, for a refused tail call
            diag.span_note(value.clone(), "not a function call");
            diag.suggestion(
                become_span.with_hi(value.lo),
                "try using `return` instead",
                "return ",
                Applicability::MaybeIncorrect,
            );
            diag
        }
        Op { site, method, lhs, rhs } => {
            let (fn_span, _) = site_spans(cx, *site, site.call);
            // clone: a handle, on the error path, for a refused tail call
            let mut diag = Diag::error(None, message, fn_span.clone());
            if let Some(method) = method {
                let lhs = expr_span(cx, *lhs);
                let parts = match rhs {
                    None => vec![
                        // alloc: the suggestion's parts, for a refused tail call
                        (fn_span.with_hi(lhs.lo), "(".to_owned()),
                        // alloc: the suggestion's text, for a refused tail call
                        (lhs.shrink_to_hi(), format!(").{method}()")),
                    ],
                    Some(rhs) => {
                        let rhs = expr_span(cx, *rhs);
                        vec![
                            // alloc: the suggestion's parts, for a refused tail call
                            (lhs.shrink_to_lo(), "(".to_owned()),
                            // alloc: the suggestion's text, for a refused tail call
                            (lhs.with_lo(lhs.hi).with_hi(rhs.lo), format!(").{method}(")),
                            // alloc: the suggestion's text, for a refused tail call
                            (rhs.with_lo(rhs.hi).with_hi(become_span.hi), ")".to_owned()),
                        ]
                    }
                };
                diag.multipart_suggestion(
                    "try using the method directly",
                    parts,
                    Applicability::MaybeIncorrect,
                );
            }
            diag
        }
        CallingClosure { callee, args } => {
            let fun = expr_span(cx, *callee);
            let underscored = match args {
                Some(0) => String::new(),
                // alloc: the suggestion's text, for a refused tail call
                Some(n) => vec!["_"; *n as usize].join(", "),
                // alloc: the suggestion's text, for a refused tail call
                None => "_".to_owned(),
            };
            let mut diag = Diag::error(None, message, become_span);
            diag.multipart_suggestion(
                "try casting the closure to a function pointer type",
                vec![
                    // alloc: the suggestion's parts, for a refused tail call
                    (fun.shrink_to_lo(), "(".to_owned()),
                    // alloc: the suggestion's text, for a refused tail call
                    (fun.shrink_to_hi(), format!(" as fn({underscored}) -> _)")),
                ],
                Applicability::MaybeIncorrect,
            );
            diag
        }
        NonFnCallee { site, callee, ty, deref } => {
            let (fn_span, fun) = site_spans(cx, *site, *callee);
            let mut diag = Diag::error(None, message, fn_span);
            // alloc: the note's text, for a refused tail call
            diag.note(format!("callee has type `{}`", print::solver_ty(cx, ty.as_ref())));
            if let Some(deref) = deref {
                let thing = if deref.pointer { "pointer" } else { "definition" };
                // alloc: the suggestion's text, for a refused tail call
                let derefs = format!("({}", "*".repeat(deref.count as usize));
                diag.multipart_suggestion(
                    // alloc: the suggestion's text, for a refused tail call
                    format!("consider dereferencing the expression to get a function {thing}"),
                    // alloc: the suggestion's parts, for a refused tail call
                    vec![(fun.shrink_to_lo(), derefs), (fun.shrink_to_hi(), ")".to_owned())],
                    Applicability::MachineApplicable,
                );
            }
            diag
        }
        AbiMismatch { caller, callee } => {
            let mut diag = Diag::error(None, message, become_span);
            diag.note("`become` requires caller and callee to have the same ABI");
            // alloc: the note's text, for a refused tail call
            diag.note(format!("caller ABI is `{caller}`, while callee ABI is `{callee}`"));
            diag
        }
        UnsupportedAbi(abi) => {
            let mut diag = Diag::error(None, message, become_span);
            // alloc: the note's text, for a refused tail call
            diag.note(format!("`become` is not supported for `extern {abi}` functions"));
            diag
        }
        SignatureMismatch { caller, callee } => {
            let mut diag = Diag::error(None, message, become_span);
            diag.note("`become` requires caller and callee to have matching signatures");
            // alloc: the note's text, for a refused tail call
            diag.note(format!("caller signature: `{}`", print::solver_ty(cx, caller.as_ref())));
            // alloc: the note's text, for a refused tail call
            diag.note(format!("callee signature: `{}`", print::solver_ty(cx, callee.as_ref())));
            diag
        }
        UnsizedArgument(ty) => {
            let mut diag = Diag::error(None, message, become_span);
            // alloc: the note's text, for a refused tail call
            diag.note(format!("unsized argument of type `{}`", print::solver_ty(cx, ty.as_ref())));
            diag
        }
        NotNormalized(ty) => {
            let ty = print::solver_ty(cx, ty.as_ref());
            // alloc: the error's text, for a refused tail call
            let message =
                format!("blaze could not normalize `{ty}` in the signatures of this tail call");
            let mut diag = Diag::error(None, message, become_span);
            diag.note("rustc's `check_tail_calls` normalizes both signatures, which a body with no type error allows");
            diag
        }
    };
    code_fact(cx, &mut diag, at);
    // alloc: the error found
    vec![Found::new(diag, Phase::Mir)]
}

pub(crate) fn lower_track_caller(cx: &Cx<'_, '_>, d: &hir::TailCallTrackCaller) -> Vec<Found> {
    if !cx.lint_may_fire(TRACK_CALLER) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let (fn_span, _) = site_spans(cx, d.site, d.site.call);
    let mut diag = Diag::lint(
        "tail_call_track_caller",
        "tail calling a function marked with `#[track_caller]` has no special effect",
        fn_span,
    );
    let at = d.site.call.map(|it| it.syntax_node_ptr());
    code_fact(cx, &mut diag, at);
    // The lint's level is of the call: rustc's lint is at the terminator's `HirId`.
    let node = cx.syntax_view(at);
    let lint_at = up(cx, node)
        .find(|it| ast::Expr::can_cast(it.value.kind()) || ast::Item::can_cast(it.value.kind()))
        .unwrap_or(node)
        .map(ast::ptr);
    // rustc gives it from codegen, which no crate with an error reaches.
    // alloc: the lint found
    vec![Found::lint(diag, Phase::Late, lint_at)]
}

/// The span of expression `node` as rustc's HIR has it: with the parentheses around it.
fn expr_span(cx: &Cx<'_, '_>, node: InFile<ExprOrPatPtr>) -> Span {
    let mut at = cx.syntax_view(node.map(|it| it.syntax_node_ptr()));
    while let Some(parent) = at.value.parent().filter(|it| it.kind() == SyntaxKind::PAREN_EXPR) {
        at = at.with_value(parent);
    }
    cx.span(at.map(|it| it.text_range()))
}

/// THIR's `fn_span` and `fun`'s span of a call: the call and `callee`, or of a method call
/// from the method's name to the call's end, and the name.
fn site_spans(
    cx: &Cx<'_, '_>,
    site: TailCallSite<InFile<ExprOrPatPtr>>,
    callee: InFile<ExprOrPatPtr>,
) -> (Span, Span) {
    let node = cx.syntax_view(site.call.map(|it| it.syntax_node_ptr()));
    if site.method
        && let Some(method) = ast::MethodCallExpr::cast(node.value)
        && let Some(name) = method.name_ref()
    {
        let name = cx.span(node.with_value(name.syntax().text_range()));
        let end = cx.span(node.with_value(method.syntax().text_range()));
        return (name.with_hi(end.hi), name);
    }
    (expr_span(cx, site.call), expr_span(cx, callee))
}
