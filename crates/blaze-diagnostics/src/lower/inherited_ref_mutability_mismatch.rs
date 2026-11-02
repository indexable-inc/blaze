//! E0308 "mismatched types" at a `&mut` pattern's `&mut` that would match an inherited shared
//! reference, noted "cannot match inherited `&` with `&mut` pattern" and suggested "replace
//! this `&mut` pattern with `&`" (machine applicable). Only under
//! `ref_pat_eat_one_layer_2024*`.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs`
//! (`error_inherited_ref_mutability_mismatch`) (MIT or Apache-2.0).

use either::Either;
use hir::InFile;
use syntax::view as ast;

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::InheritedRefMutabilityMismatch) -> Vec<Found> {
    let file = d.pat.file_id;
    let ptr = InFile::new(file, d.pat.value.syntax_node_ptr());
    let node = cx.node_view(d.pat);
    let prefix = match node.value {
        Either::Right(ast::Pat::RefPat(pat)) => pat.amp_token().zip(pat.mut_token()),
        _ => None,
    };
    let Some((amp, mut_)) = prefix else {
        let message = "blaze can't report this `&mut` pattern as rustc does: hir found it at no `&mut` pattern";
        let mut diag = Diag::error(None, message, cx.span_of(ptr));
        code_fact(cx, &mut diag, ptr);
        // alloc: the error's parts, for an error hir found
        return vec![Found::new(diag, Phase::Bodies)];
    };
    let at = cx.span(InFile::new(file, amp.text_range().cover(mut_.text_range())));
    // clone: a handle, on the error path
    let mut diag = Diag::error(Some("E0308"), "mismatched types", at.clone());
    diag.note("cannot match inherited `&` with `&mut` pattern");
    diag.suggestion_verbose(at, "replace this `&mut` pattern with `&`", "&", Applicability::MachineApplicable);
    code_fact(cx, &mut diag, ptr);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
