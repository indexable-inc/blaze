use rustc_type_ir::{Interner, inherent::Term as _};

use crate::next_solver::{
    Const, ProjectionClause, Term, Ty,
    fulfill::{FulfillmentCtxt, NextSolverError},
    infer::{at::At, traits::Obligation},
};

impl<'db> At<'_, 'db> {
    pub(crate) fn structurally_normalize_ty(
        &self,
        ty: Ty<'db>,
        fulfill_cx: &mut FulfillmentCtxt<'db>,
    ) -> Result<Ty<'db>, Vec<NextSolverError<'db>>> {
        self.structurally_normalize_term(ty.into(), fulfill_cx).map(|term| term.expect_type())
    }

    pub(crate) fn structurally_normalize_const(
        &self,
        ct: Const<'db>,
        fulfill_cx: &mut FulfillmentCtxt<'db>,
    ) -> Result<Const<'db>, Vec<NextSolverError<'db>>> {
        self.structurally_normalize_term(ct.into(), fulfill_cx).map(|term| term.expect_const())
    }

    pub(crate) fn structurally_normalize_term(
        &self,
        term: Term<'db>,
        fulfill_cx: &mut FulfillmentCtxt<'db>,
    ) -> Result<Term<'db>, Vec<NextSolverError<'db>>> {
        assert!(!term.is_infer(), "should have resolved vars before calling");

        let interner = self.infcx.interner;
        if !interner.renormalize_rigid_aliases() && !term.is_non_rigid_alias() {
            return Ok(term);
        }

        let Some(alias) = term.to_alias_term() else {
            return Ok(term);
        };

        let new_infer = self.infcx.next_term_var_of_alias_kind(alias, self.cause.span());

        // We simply emit a `Projection` goal here, since that will take care of
        // normalizing the LHS of the projection until it is a rigid projection
        // (or a not-yet-defined opaque in scope).
        let obligation = Obligation::new(
            interner,
            *self.cause,
            self.param_env,
            ProjectionClause { projection_term: alias, term: new_infer },
        );

        fulfill_cx.register_predicate_obligation(self.infcx, obligation);
        let errors = fulfill_cx.try_evaluate_obligations(self.infcx);
        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(self.infcx.deeply_resolve_ignoring_regions(new_infer))
    }
}
