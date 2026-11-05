//! Fold impls for the next-trait-solver.

use rustc_type_ir::{TypeFoldable, TypeFolder, TypeSuperFoldable, TypeVisitableExt};

use crate::next_solver::FxIndexMap;

use super::{Binder, BoundRegion, DbInterner, Region, RegionExt, SolverDefId, Ty};

pub use rustc_type_ir::BoundVarReplacerDelegate;

/// A simple delegate taking 3 mutable functions. The used functions must
/// always return the same result for each bound variable, no matter how
/// frequently they are called.
pub type FnMutDelegate<'db, 'a> = rustc_type_ir::FnMutDelegate<'a, DbInterner<'db>>;

/// Replaces the escaping bound vars (late bound regions or bound types) in a type.
pub type BoundVarReplacer<'db, D> = rustc_type_ir::BoundVarReplacer<DbInterner<'db>, D>;

pub fn fold_tys<'db, T: TypeFoldable<DbInterner<'db>>>(
    interner: DbInterner<'db>,
    t: T,
    callback: impl FnMut(Ty<'db>) -> Ty<'db>,
) -> T {
    struct Folder<'db, F> {
        interner: DbInterner<'db>,
        callback: F,
    }
    impl<'db, F: FnMut(Ty<'db>) -> Ty<'db>> TypeFolder<DbInterner<'db>> for Folder<'db, F> {
        fn cx(&self) -> DbInterner<'db> {
            self.interner
        }

        fn fold_ty(&mut self, t: Ty<'db>) -> Ty<'db> {
            let t = t.super_fold_with(self);
            (self.callback)(t)
        }
    }

    t.fold_with(&mut Folder { interner, callback })
}

impl<'db> DbInterner<'db> {
    /// Replaces all regions bound by the given `Binder` with the
    /// results returned by the closure; the closure is expected to
    /// return a free region (relative to this binder), and hence the
    /// binder is removed in the return type. The closure is invoked
    /// once for each unique `BoundRegionKind`; multiple references to the
    /// same `BoundRegionKind` will reuse the previous result. A map is
    /// returned at the end with each bound region and the free region
    /// that replaced it.
    ///
    /// # Panics
    ///
    /// This method only replaces late bound regions. Any types or
    /// constants bound by `value` will cause an ICE.
    pub fn instantiate_bound_regions<T, F>(
        self,
        value: Binder<'db, T>,
        mut fld_r: F,
    ) -> (T, FxIndexMap<BoundRegion<'db>, Region<'db>>)
    where
        F: FnMut(BoundRegion<'db>) -> Region<'db>,
        T: TypeFoldable<DbInterner<'db>>,
    {
        let mut region_map = FxIndexMap::default();
        let real_fld_r = |br: BoundRegion<'db>| *region_map.entry(br).or_insert_with(|| fld_r(br));
        let value = self.instantiate_bound_regions_uncached(value, real_fld_r);
        (value, region_map)
    }

    pub fn instantiate_bound_regions_uncached<T, F>(
        self,
        value: Binder<'db, T>,
        mut replace_regions: F,
    ) -> T
    where
        F: FnMut(BoundRegion<'db>) -> Region<'db>,
        T: TypeFoldable<DbInterner<'db>>,
    {
        let value = value.skip_binder();
        if !value.has_escaping_bound_vars() {
            value
        } else {
            let delegate = FnMutDelegate {
                regions: &mut replace_regions,
                types: &mut |b| panic!("unexpected bound ty in binder: {b:?}"),
                consts: &mut |b| panic!("unexpected bound ct in binder: {b:?}"),
            };
            let mut replacer = BoundVarReplacer::new(self, delegate);
            value.fold_with(&mut replacer)
        }
    }

    /// Replaces any late-bound regions bound in `value` with `'erased`. Useful in codegen but also
    /// method lookup and a few other places where precise region relationships are not required.
    pub fn instantiate_bound_regions_with_erased<T>(self, value: Binder<'db, T>) -> T
    where
        T: TypeFoldable<DbInterner<'db>>,
    {
        self.instantiate_bound_regions(value, |_| Region::new_erased(self)).0
    }

    /// Replaces any late-bound regions bound in `value` with
    /// free variants attached to `all_outlive_scope`.
    pub fn liberate_late_bound_regions<T>(
        self,
        all_outlive_scope: SolverDefId<'db>,
        value: Binder<'db, T>,
    ) -> T
    where
        T: TypeFoldable<DbInterner<'db>>,
    {
        self.instantiate_bound_regions_uncached(value, |br| {
            Region::new_late_param(self, all_outlive_scope, br)
        })
    }
}
