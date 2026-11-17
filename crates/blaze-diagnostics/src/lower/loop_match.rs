//! What rustc refuses of a `#[loop_match]` loop or a `#[const_continue]` break.
//!
//! Follows rustc's `compiler/rustc_mir_build/src/errors.rs` (`LoopMatch*`, `ConstContinue*`),
//! `compiler/rustc_mir_build/src/builder/expr/into.rs` (the state type, fatal) and
//! `compiler/rustc_hir_typeck/src/errors.rs` (`ConstContinueBadLabel`) (MIT or Apache-2.0).
//! The checks and the nodes each refusal is at are `hir_ty_mir::mir::loop_match`'s.

use hir::{InFile, LoopMatchErrorKind, LoopMatchNotEarly};
use hir_def::expr_store::ExprOrPatPtr;
use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Diag, Span},
    print,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::LoopMatchRefused) -> Vec<Found> {
    use LoopMatchErrorKind::*;
    let message = d.kind.message();
    let (mut diag, at) = match &d.kind {
        MissingAssignment { at } | BadRhs { at } | MissingLabelOrValue { at } => {
            (Diag::error(None, message, span(cx, *at)), *at)
        }
        BadStatements { first, last } => {
            let span = span(cx, *first).to(&span(cx, *last));
            (Diag::error(None, message, span), *first)
        }
        InvalidMatch { scrutinee } => {
            let mut diag = Diag::error(None, message, span(cx, *scrutinee));
            diag.note("a local variable must be the scrutinee within a `#[loop_match]`");
            (diag, *scrutinee)
        }
        InvalidUpdate { lhs, scrutinee } => {
            let mut diag = Diag::error(None, message, span(cx, *lhs));
            diag.span_label(span(cx, *scrutinee), "the assignment must update this variable");
            (diag, *lhs)
        }
        UnsupportedType { scrutinee, ty } => {
            let ty = print::solver_ty(cx, ty.as_ref());
            // alloc: the error's text, for a refused `#[loop_match]`
            let message = format!("this `#[loop_match]` state value has type `{ty}`, which is not supported");
            let mut diag = Diag::error(None, message, span(cx, *scrutinee));
            diag.note("only integers, floats, bool, char, and enums without fields are supported");
            (diag, *scrutinee)
        }
        Guard { guard } => (Diag::error(None, message, span(cx, *guard)), *guard),
        BadLabel { at, label } => {
            let whole = span(cx, *at);
            let span = match label {
                true => label_span(cx, *at).unwrap_or(whole),
                false => whole,
            };
            (Diag::error(None, message, span), *at)
        }
        NotEarly { value, why } => {
            let span = span(cx, *value);
            // clone: a handle, on the error path, for a refused `#[const_continue]`
            let mut diag = Diag::error(None, message, span.clone());
            diag.span_label(
                span,
                match why {
                    LoopMatchNotEarly::ConstParam => {
                        "constant parameters may use generics, and are not evaluated early enough"
                    }
                    LoopMatchNotEarly::ConstBlock => {
                        "`const` blocks may use generics, and are not evaluated early enough"
                    }
                    LoopMatchNotEarly::Other => "this value must be a literal or a monomorphic const",
                },
            );
            diag.help("try extracting the expression into a `const` item");
            (diag, *value)
        }
        TooGeneric { value } => {
            let span = span(cx, *value);
            // clone: a handle, on the error path, for a refused `#[const_continue]`
            let mut diag = Diag::error(None, message, span.clone());
            diag.span_label(span, "this value is too generic");
            diag.help("try extracting the expression into a `const` item");
            (diag, *value)
        }
        UnknownJumpTarget { at } => (Diag::error(None, message, span(cx, *at)), *at),
        Unevaluated { at, what } => {
            let mut diag = Diag::error(None, message, span(cx, *at));
            // alloc: the note's text, for a `#[const_continue]` blaze cannot decide
            diag.note(format!("blaze does not evaluate {what} here; rustc's const evaluation would"));
            (diag, *at)
        }
    };
    // rustc's `emit_fatal`, which every refusal but `NotEarly` is: it stops at this error.
    if d.kind.is_fatal() {
        diag.stop();
    }
    code_fact(cx, &mut diag, at.map(|it| it.syntax_node_ptr()));
    // `ConstContinueBadLabel` is type checking's (`rustc_hir_typeck/src/loops.rs`), made in
    // a body with a type error too; the rest are THIR's and MIR building's.
    let phase = match d.kind {
        BadLabel { .. } => Phase::Bodies,
        _ => Phase::Mir,
    };
    // alloc: the error found
    vec![Found::new(diag, phase)]
}

fn span(cx: &Cx<'_, '_>, node: InFile<ExprOrPatPtr>) -> Span {
    let at = cx.syntax_view(node.map(|it| it.syntax_node_ptr()));
    cx.span(at.map(|it| it.text_range()))
}

/// The label `'blk` of the `break 'blk` at `node`: where rustc's `ConstContinueBadLabel` is.
fn label_span(cx: &Cx<'_, '_>, node: InFile<ExprOrPatPtr>) -> Option<Span> {
    let at = cx.syntax_view(node.map(|it| it.syntax_node_ptr()));
    let lifetime = ast::BreakExpr::cast(at.value)?.lifetime()?;
    Some(cx.span(at.with_value(lifetime.syntax().text_range())))
}
