use rustc_type_ir::{OutlivesClause, TypeVisitableExt};
use tracing::{debug, instrument};

use crate::next_solver::{
    ArgOutlivesClause, GenericArgKind, Region, RegionOutlivesClause, Ty,
    infer::{InferCtxt, TypeOutlivesConstraint},
};

impl<'db> InferCtxt<'db> {
    pub fn register_outlives_constraint(&self, OutlivesClause(arg, r2): ArgOutlivesClause<'db>) {
        match arg.kind() {
            GenericArgKind::Lifetime(r1) => {
                self.register_region_outlives_constraint(OutlivesClause(r1, r2));
            }
            GenericArgKind::Type(ty1) => {
                self.register_type_outlives_constraint(ty1, r2);
            }
            GenericArgKind::Const(_) => unreachable!(),
        }
    }

    pub fn register_region_outlives_constraint(
        &self,
        OutlivesClause(r_a, r_b): RegionOutlivesClause<'db>,
    ) {
        // `'a: 'b` ==> `'b <= 'a`
        self.sub_regions(r_b, r_a);
    }

    /// Registers that the given region obligation must be resolved
    /// from within the scope of `body_id`. These regions are enqueued
    /// and later processed by regionck, when full type information is
    /// available (see `region_obligations` field for more
    /// information).
    #[instrument(level = "debug", skip(self))]
    pub fn register_type_outlives_constraint_inner(&self, obligation: TypeOutlivesConstraint<'db>) {
        self.inner.borrow_mut().region_obligations.push(obligation);
    }

    pub fn register_type_outlives_constraint(&self, sup_type: Ty<'db>, sub_region: Region<'db>) {
        // `is_global` means the type has no params, infer, placeholder, or non-`'static`
        // free regions. If the type has none of these things, then we can skip registering
        // this outlives obligation since it has no components which affect lifetime
        // checking in an interesting way.
        if sup_type.is_global() {
            return;
        }

        debug!(?sup_type, ?sub_region);

        self.register_type_outlives_constraint_inner(TypeOutlivesConstraint {
            sup_type,
            sub_region,
        });
    }

    pub fn register_region_assumption(&self, assumption: ArgOutlivesClause<'db>) {
        self.inner.borrow_mut().region_assumptions.push(assumption);
    }
}
