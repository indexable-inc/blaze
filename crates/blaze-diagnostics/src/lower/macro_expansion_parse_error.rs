//! A macro's expansion that does not parse as the fragment its call stands for. rustc's parser
//! stops at the first error of a fragment (`rustc_expand/src/expand.rs`'s `parse_ast_fragment`,
//! MIT or Apache-2.0), so only the first is given, at its place in the expansion, in the words
//! `lower::parse` gives a file's parse errors in.

use hir::InFile;
use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::Cx,
    lower::{Found, Phase, parse},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MacroExpansionParseError) -> Vec<Found> {
    // alloc: empty, allocates nothing
    let Some(error) = d.errors.first() else { return Vec::new() };
    let root = cx.root(d.range.file_id);
    let call = root.covering_element(d.range.value).ancestors().find_map(ast::MacroCall::cast);
    let expansion = call.and_then(|call| cx.vsema.expand_macro_call(call));
    let (Some(call), Some(expansion)) = (call, expansion) else {
        // alloc: the error's text, for an error hir found
        let message = format!("the expansion of this macro does not parse: {error}");
        // alloc: the error's parts, for an error hir found
        return vec![Found::new(Diag::error(None, message, cx.span(d.range)), Phase::Expand)];
    };
    let site = cx.span(InFile::new(d.range.file_id, call.syntax().text_range()));
    let diag = parse::expansion_error(
        cx,
        expansion.file_id,
        expansion.value,
        site,
        // alloc: the error's text, for an error hir found
        &error.to_string(),
        error.range().start(),
    );
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Expand)]
}
