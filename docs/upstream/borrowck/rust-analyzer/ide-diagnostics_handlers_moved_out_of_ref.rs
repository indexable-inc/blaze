use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use hir::HirDisplay;

// Diagnostic: moved-out-of-ref
//
// This diagnostic is triggered on moving non copy things out of references.
pub(crate) fn moved_out_of_ref(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::MovedOutOfRef<'_>,
) -> Diagnostic {
    Diagnostic::new_with_syntax_node_ptr(
        ctx,
        DiagnosticCode::RustcHardError("E0507"),
        format!("cannot move `{}` out of reference", d.ty.display(ctx.sema.db, ctx.display_target)),
        d.span,
    )
    // spans are broken, and I'm not sure how precise we can detect copy types
}

#[cfg(test)]
