//! The goals left ambiguous when inference is done: rustc's `report_ambiguity_errors`.

use crate::{infer::InferenceContext, solver_errors::ambiguity_diagnostics};

impl<'db, 'f> InferenceContext<'db, 'f> {
    /// E0282, E0283 and E0284 for every goal the solver could neither prove nor refute, as
    /// `TypeMustBeKnown` with the goal. rustc reports the pending goals only of a body with no
    /// error yet; those found while normalizing it reported where they were found, so of a body
    /// with an error they give only what `maybe_report_ambiguity` gives then. The goals stalled
    /// on a coroutine of the body are kept, not reported (rustc's `coroutine_stalled_predicates`).
    pub(super) fn report_ambiguity_errors(&mut self) {
        let (mut errors, pending, stalled) = self.table.take_ambiguity_errors();
        self.coroutine_stalled.extend(
            stalled.into_iter().map(|it| (it.cause.span(), it.predicate.store())),
        );
        let tainted = self.tainted_by_errors();
        if !tainted {
            errors.extend(pending);
        }
        if errors.is_empty() {
            return;
        }
        for diagnostic in ambiguity_diagnostics(&self.table.infer_ctxt, errors, tainted) {
            self.push_diagnostic(diagnostic);
        }
    }
}
