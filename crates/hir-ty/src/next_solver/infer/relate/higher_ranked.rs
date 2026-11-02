//! Helper routines for higher-ranked things. See the `doc` module at
//! the end of the file for details.

use rustc_type_ir::{TypeFoldable, TypeVisitableExt, UniverseIndex};
use tracing::{debug, instrument};

use crate::next_solver::fold::BoundVarReplacerDelegate;
use crate::next_solver::infer::InferCtxt;
use crate::next_solver::infer::relate::RelateResult;
use crate::next_solver::infer::snapshot::CombinedSnapshot;
use crate::next_solver::{
    Binder, BoundConst, BoundRegion, BoundTy, Const, DbInterner, PlaceholderConst,
    PlaceholderRegion, PlaceholderType, Region, Ty,
};

impl<'db> InferCtxt<'db> {
    /// Replaces all bound variables (lifetimes, types, and constants) bound by
    /// `binder` with placeholder variables in a new universe. This means that the
    /// new placeholders can only be named by inference variables created after
    /// this method has been called.
    ///
    /// This is the first step of checking subtyping when higher-ranked things are involved.
    /// For more details visit the relevant sections of the [rustc dev guide].
    ///
    /// `fn enter_forall` should be preferred over this method.
    ///
    /// [rustc dev guide]: https://rustc-dev-guide.rust-lang.org/traits/hrtb.html
    #[instrument(level = "debug", skip(self), ret)]
    pub fn enter_forall_and_leak_universe<T>(&self, binder: Binder<'db, T>) -> T
    where
        T: TypeFoldable<DbInterner<'db>>,
    {
        // Inlined `no_bound_vars`.
        if !binder.as_ref().skip_binder().has_escaping_bound_vars() {
            return binder.skip_binder();
        }

        let next_universe = self.create_next_universe();

        let delegate = ToPlaceholders { interner: self.interner, universe: next_universe };

        debug!(?next_universe);
        self.interner.replace_bound_vars_uncached(binder, delegate)
    }

    /// Replaces all bound variables (lifetimes, types, and constants) bound by
    /// `binder` with placeholder variables in a new universe and then calls the
    /// closure `f` with the instantiated value. The new placeholders can only be
    /// named by inference variables created inside of the closure `f` or afterwards.
    ///
    /// This is the first step of checking subtyping when higher-ranked things are involved.
    /// For more details visit the relevant sections of the [rustc dev guide].
    ///
    /// This method should be preferred over `fn enter_forall_and_leak_universe`.
    ///
    /// [rustc dev guide]: https://rustc-dev-guide.rust-lang.org/traits/hrtb.html
    #[instrument(level = "debug", skip(self, f))]
    pub fn enter_forall<T, U>(&self, forall: Binder<'db, T>, f: impl FnOnce(T) -> U) -> U
    where
        T: TypeFoldable<DbInterner<'db>>,
    {
        // FIXME: currently we do nothing to prevent placeholders with the new universe being
        // used after exiting `f`. For example region subtyping can result in outlives constraints
        // that name placeholders created in this function. Nested goals from type relations can
        // also contain placeholders created by this function.
        let value = self.enter_forall_and_leak_universe(forall);
        debug!(?value);
        f(value)
    }

    /// See [`RegionConstraintCollector::leak_check`]. We only check placeholder leaking into
    /// `outer_universe`, i.e. placeholders which cannot be named by that universe.
    ///
    /// [`RegionConstraintCollector::leak_check`]: crate::next_solver::infer::region_constraints::RegionConstraintCollector::leak_check
    pub fn leak_check(
        &self,
        outer_universe: UniverseIndex,
        only_consider_snapshot: Option<&CombinedSnapshot>,
    ) -> RelateResult<'db, ()> {
        let max_universe = self.universe();
        self.inner.borrow_mut().unwrap_region_constraints().leak_check(
            self.interner,
            outer_universe,
            max_universe,
            only_consider_snapshot,
        )
    }
}

/// Replaces each bound variable with a placeholder in `universe`, by static dispatch.
struct ToPlaceholders<'db> {
    interner: DbInterner<'db>,
    universe: UniverseIndex,
}

impl<'db> BoundVarReplacerDelegate<DbInterner<'db>> for ToPlaceholders<'db> {
    #[inline]
    fn replace_region(&mut self, br: BoundRegion<'db>) -> Region<'db> {
        Region::new_placeholder(self.interner, PlaceholderRegion::new(self.universe, br))
    }

    #[inline]
    fn replace_ty(&mut self, bound_ty: BoundTy<'db>) -> Ty<'db> {
        Ty::new_placeholder(self.interner, PlaceholderType::new(self.universe, bound_ty))
    }

    #[inline]
    fn replace_const(&mut self, bound: BoundConst<'db>) -> Const<'db> {
        Const::new_placeholder(self.interner, PlaceholderConst::new(self.universe, bound))
    }
}
