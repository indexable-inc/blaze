//! E0695: an unlabeled `break` or `continue` whose innermost labeled block, loop or closure is
//! a labeled block, "unlabeled `break` inside of a labeled block".
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/loops.rs` (`require_label_in_labeled_block`)
//! and `compiler/rustc_hir_typeck/src/diagnostics.rs` (`UnlabeledInLabeledBlock`) (MIT or
//! Apache-2.0).

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnlabeledJumpInLabeledBlock) -> Vec<Found> {
    let at = d.expr.map(|it| it.syntax_node_ptr());
    let span = cx.span_of(at);
    let name = if d.is_break { "break" } else { "continue" };
    let mut diag =
        // alloc: clone: the error's parts, for an error hir found
        Diag::error(Some("E0695"), format!("unlabeled `{name}` inside of a labeled block"), span.clone());
    diag.span_label(
        span,
        // alloc: the error's text, for an error hir found
        format!("`{name}` statements that would diverge to or through a labeled block need to bear a label"),
    );
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
