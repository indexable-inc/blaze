//! hir's `VerifyRefused`: a `blaze_verify` specification clause (`requires`, `ensures`,
//! `invariant`, ..) or named return type `-> (r: T)` that body lowering refused
//! (docs/v2/plan/design/verus.md), named by the function it is on and the clause's keyword.

use hir::InFile;
use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::VerifyRefused) -> Vec<Found> {
    let node = cx.syntax_view(d.node);
    let at = cx.span(InFile::new(node.file_id, node.value.text_range()));
    let what = match ast::SpecClause::cast(node.value) {
        Some(clause) => match clause.syntax().first_token() {
            // alloc: the clause's keyword, for an error hir found
            Some(keyword) => format!("`{}` clause", keyword.text()),
            // alloc: as above
            None => "clause".to_owned(),
        },
        // alloc: as above
        None => "named return type".to_owned(),
    };
    let function = crate::lints::body_item(cx, d.node).map(|(_, name)| name);
    // alloc: the message, for an error hir found
    let message = match function {
        Some(function) => format!("{}: the {what} of `{function}`", d.why.message()),
        None => format!("{}: this {what}", d.why.message()),
    };
    let diag = Diag::error(None, message, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Lower)]
}
