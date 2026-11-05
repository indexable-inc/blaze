//! Autoderef helpers for inference.

use std::iter;

use blaze_mem::Frame;
use rustc_ast_ir::Mutability;

use crate::{
    Adjust, Adjustment, OverloadedDeref, Span,
    autoderef::{Autoderef, AutoderefCtx, AutoderefKind, GeneralAutoderef},
    infer::unify::InferenceTable,
    next_solver::{
        Ty,
        infer::{InferOk, traits::PredicateObligations},
    },
};

impl<'db> InferenceTable<'db> {
    pub(crate) fn autoderef(&self, base_ty: Ty<'db>, span: Span) -> Autoderef<'_, 'db, usize> {
        Autoderef::new(&self.infer_ctxt, self.param_env, base_ty, span)
            .reporting_to(&self.recursion_limit_errors)
    }

    pub(crate) fn autoderef_with_tracking(
        &self,
        base_ty: Ty<'db>,
        span: Span,
    ) -> Autoderef<'_, 'db> {
        Autoderef::new_with_tracking(&self.infer_ctxt, self.param_env, base_ty, span)
            .reporting_to(&self.recursion_limit_errors)
    }
}

impl<'db, Ctx: AutoderefCtx<'db>> GeneralAutoderef<'db, Ctx> {
    pub(crate) fn adjust_steps_as_infer_ok<'f>(
        &mut self,
        frame: &'f Frame,
    ) -> InferOk<'db, Vec<Adjustment, &'f Frame>> {
        let steps = self.steps();
        let mut adjustments = Vec::with_capacity_in(steps.len(), frame);
        if steps.is_empty() {
            return InferOk { obligations: PredicateObligations::new(), value: adjustments };
        }

        let targets = steps.iter().skip(1).map(|&(ty, _)| ty).chain(iter::once(self.final_ty()));
        adjustments.extend(
            steps
                .iter()
                .map(|&(_source, kind)| {
                    if let AutoderefKind::Overloaded = kind {
                        Some(OverloadedDeref(Mutability::Not))
                    } else {
                        None
                    }
                })
                .zip(targets)
                .map(|(autoderef, target)| Adjustment {
                    kind: Adjust::Deref(autoderef),
                    target: target.store(),
                }),
        );

        InferOk { obligations: self.take_obligations(), value: adjustments }
    }
}
