//! E0040 "explicit use of destructor method": a call of `Drop::drop`, as a method (`x.drop()`)
//! or a path (`Drop::drop(&mut x)`), with the `drop` function to call instead.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/callee.rs`
//! (`check_legal_trait_for_method_call`) and `diagnostics.rs` (`ExplicitDestructorCall`)
//! (MIT or Apache-2.0).

use either::Either;
use hir::InFile;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasArgList as _},
};

use super::binary_operator_cannot_be_applied::code_fact;
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ExplicitDropMethodUse) -> Vec<Found> {
    let message = "explicit use of destructor method";
    let label = "explicit destructor calls not allowed";
    let sugg = "consider using `drop` function";
    let (mut diag, at) = match &d.expr_or_path {
        Either::Left(call) => {
            let node = cx.node_view(*call);
            let file = node.file_id;
            let at = |range: TextRange| cx.span(InFile::new(file, range));
            let expr_range = node.value.syntax().text_range();
            let span = node
                .value
                .name_ref()
                .map_or_else(|| at(expr_range), |it| at(it.syntax().text_range()));
            let mut diag = Diag::error(Some("E0040"), message, span.clone()); // clone: its span
            diag.span_label(span.clone(), label); // clone: the label's span
            match node.value.receiver().map(|it| it.syntax().text_range()) {
                Some(receiver) if !receiver.is_empty() => {
                    diag.suggestion_with_style(
                        // alloc: the suggestion's parts
                        vec![
                            // alloc: the suggestion's text
                            (at(TextRange::new(expr_range.start(), receiver.start())), "drop(".to_owned()),
                            // alloc: the suggestion's text
                            (at(TextRange::new(receiver.end(), expr_range.end())), ")".to_owned()),
                        ],
                        sugg,
                        Applicability::Unspecified,
                        SuggestionStyle::HideCodeInline,
                    );
                }
                _ => {
                    diag.suggestion(span, sugg, "drop", Applicability::MaybeIncorrect);
                }
            }
            (diag, call.map(|it| it.syntax_node_ptr()))
        }
        Either::Right(path) => {
            let span = cx.span_of_ptr(*path);
            let node = cx.node_view(*path);
            let file = node.file_id;
            let at = |range: TextRange| cx.span(InFile::new(file, range));
            let mut diag = Diag::error(Some("E0040"), message, span.clone()); // clone: its span
            diag.span_label(span.clone(), label); // clone: the label's span
            match first_argument(node.value) {
                Some((call, arg)) if !arg.is_empty() => {
                    diag.suggestion_with_style(
                        // alloc: the suggestion's parts
                        vec![
                            // alloc: the suggestion's text
                            (at(TextRange::new(call.start(), arg.start())), "drop(".to_owned()),
                            // alloc: the suggestion's text
                            (at(TextRange::new(arg.end(), call.end())), ")".to_owned()),
                        ],
                        sugg,
                        Applicability::Unspecified,
                        SuggestionStyle::HideCodeInline,
                    );
                }
                _ => {
                    diag.suggestion(span, sugg, "drop", Applicability::MaybeIncorrect);
                }
            }
            (diag, path.map(|it| it.syntax_node_ptr()))
        }
    };
    code_fact(cx, at, &mut diag);
    vec![Found::new(diag, Phase::Bodies)] // alloc: the error found
}

/// The call a path is the callee of and its first argument without the borrows before it
/// (rustc's `arg_span` in `instantiate_value_path`).
fn first_argument(path: ast::Path<'_>) -> Option<(TextRange, TextRange)> {
    let callee = ast::PathExpr::cast(path.syntax().parent()?)?;
    let call = ast::CallExpr::cast(callee.syntax().parent()?)?;
    let mut arg = call.arg_list()?.args().next()?;
    while let ast::Expr::RefExpr(it) = arg {
        arg = it.expr()?;
    }
    Some((call.syntax().text_range(), arg.syntax().text_range()))
}
