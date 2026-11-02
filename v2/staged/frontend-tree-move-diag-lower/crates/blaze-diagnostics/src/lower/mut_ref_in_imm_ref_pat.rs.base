//! E0596 "cannot borrow as mutable inside an `&` pattern": a binding by mutable reference
//! (`ref mut x`, or by the default binding mode) under a `&` pattern that matched a `&mut`
//! (RFC 3627's rule 5, under `ref_pat_eat_one_layer_2024` or its `_structural` form), with
//! rustc's suggestion to make that `&` a `&mut`.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_pat_ident`, `MutblCap`,
//! `check_pat_ref`'s `pat_prefix_span`) (MIT or Apache-2.0). The `&` rustc's `MutblCap::WeaklyNot`
//! points to is the innermost `&` pattern around the binding: each `&` pattern replaces the
//! one before it, and a `&` that matched a shared reference ends the error.

use hir::InFile;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, TextRange,
    ast::{self, HasName as _},
};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MutRefInImmRefPat) -> Vec<Found> {
    let node = cx.node(d.pat);
    let range = match node.value.as_ref().right() {
        Some(ast::Pat::IdentPat(pat)) => match pat.name() {
            Some(name) => name.syntax().text_range(),
            None => pat.syntax().text_range(),
        },
        _ => node.value.syntax().text_range(),
    };
    let span = cx.span(InFile::new(node.file_id, range));
    let mut diag =
        Diag::error(Some("E0596"), "cannot borrow as mutable inside an `&` pattern", span);
    if let Some(and) = and_pat_prefix(cx, node.map(|it| it.syntax().clone())) {
        diag.suggestion(and, "replace this `&` with `&mut`", "&mut ", Applicability::MachineApplicable);
    }
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)]
}

/// The `&` (to where its pattern starts) of the innermost `&` pattern around `node`.
fn and_pat_prefix(cx: &Cx<'_, '_>, node: InFile<SyntaxNode>) -> Option<Span> {
    for ancestor in cx.sema.ancestors_with_macros_file(node).skip(1) {
        let it = &ancestor.value;
        match it.kind() {
            SyntaxKind::REF_PAT => {
                let pat = ast::RefPat::cast(it.clone())?;
                if pat.mut_token().is_some() {
                    continue;
                }
                let start = it.text_range().start();
                let end = pat.pat()?.syntax().text_range().start();
                return (end > start).then(|| cx.span(ancestor.with_value(TextRange::new(start, end))));
            }
            SyntaxKind::RECORD_PAT_FIELD
            | SyntaxKind::RECORD_PAT_FIELD_LIST
            | SyntaxKind::MACRO_CALL
            | SyntaxKind::TOKEN_TREE => {}
            kind if ast::Pat::can_cast(kind) => {}
            _ => return None,
        }
    }
    None
}
