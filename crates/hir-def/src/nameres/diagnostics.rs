//! The diagnostics of collecting a scope's items. Collection is blaze-scope's now, and its
//! findings are records keyed like its check (rule 81); none are made here. The type stays so
//! the readers of `(items, diagnostics)` pairs keep their shape.

/// The diagnostics a collection made: none here (see the module's doc).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct DefDiagnostics(());

impl DefDiagnostics {
    pub fn none() -> DefDiagnostics {
        DefDiagnostics(())
    }

    pub fn is_empty(&self) -> bool {
        true
    }
}
