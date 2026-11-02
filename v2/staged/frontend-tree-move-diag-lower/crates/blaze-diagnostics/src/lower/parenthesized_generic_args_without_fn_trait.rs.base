//! E0214 "parenthesized type parameters may only be used with a `Fn` trait": `Vec(&str)`,
//! with the suggestion of angle brackets.
//!
//! Follows rustc's `compiler/rustc_ast_lowering/src/path.rs` (`lower_path_segment`,
//! `GenericArgsMode::Err`) and `compiler/rustc_ast_lowering/src/diagnostics.rs`
//! (`GenericTypeWithParentheses`, `UseAngleBrackets`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::{AstNode, TextRange, ast};

use crate::{
    cx::Cx,
    lower::{Found, Phase, incorrect_generics_len::site_fact},
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ParenthesizedGenericArgsWithoutFnTrait) -> Vec<Found> {
    let args = cx.node(d.args);
    let file = args.file_id;
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    let list = args.value;
    let segment = list.syntax().parent().and_then(ast::PathSegment::cast);
    // rustc's `ParenthesizedArgs::span`: from the segment's name to the end of its return type.
    let range = match &segment {
        Some(segment) => {
            let start = match segment.name_ref() {
                Some(name) => name.syntax().text_range().start(),
                None => list.syntax().text_range().start(),
            };
            let end = match segment.ret_type() {
                Some(ret) => ret.syntax().text_range().end(),
                None => list.syntax().text_range().end(),
            };
            TextRange::new(start, end.max(start))
        }
        None => list.syntax().text_range(),
    };
    let mut diag = Diag::error(
        Some("E0214"),
        "parenthesized type parameters may only be used with a `Fn` trait",
        span(range),
    );
    diag.span_label(span(range), "only `Fn` traits may use parentheses");
    // alloc: the error's parts, for an error hir found
    let inputs: Vec<ast::TypeArg> = list.type_args().collect();
    if let (Some(first), Some(last)) = (inputs.first(), inputs.last()) {
        let whole = list.syntax().text_range();
        let open = TextRange::new(whole.start(), first.syntax().text_range().start());
        let close = TextRange::new(last.syntax().text_range().end(), whole.end());
        diag.multipart_suggestion(
            "use angle brackets instead",
            // alloc: the error's text, for an error hir found
            vec![(span(open), "<".to_owned()), (span(close), ">".to_owned())],
            Applicability::MaybeIncorrect,
        );
    }
    site_fact(cx, &mut diag, InFile::new(file, list.syntax()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Resolve)]
}
