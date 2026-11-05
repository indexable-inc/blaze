//! Things related to opaques in the next-trait-solver.

use std::{marker::PhantomData, num::NonZeroU32};

use rustc_ast_ir::try_visit;
use rustc_type_ir::inherent::SliceLike;

use crate::next_solver::{impl_foldable_for_interned_slice, interned_slice, interner::table};

use super::{DbInterner, SolverDefId, Ty};

pub type OpaqueTypeKey<'db> = rustc_type_ir::OpaqueTypeKey<DbInterner<'db>>;

type PredefinedOpaque<'db> = (OpaqueTypeKey<'db>, Ty<'db>);
interned_slice!(
    PredefinedOpaquesStorage,
    PredefinedOpaques,
    StoredPredefinedOpaques,
    predefined_opaques,
    PredefinedOpaque<'db>,
    PredefinedOpaque<'static>,
);
impl_foldable_for_interned_slice!(PredefinedOpaques);

pub type ExternalConstraintsData<'db> =
    rustc_type_ir::solve::ExternalConstraintsData<DbInterner<'db>>;

interned_slice!(
    SolverDefIdsStorage,
    SolverDefIds,
    StoredSolverDefIds,
    def_ids,
    SolverDefId<'db>,
    SolverDefId<'static>,
);
impl_foldable_for_interned_slice!(SolverDefIds);

/// Every interned [`ExternalConstraints`], kept whole: it derefs to its data.
static EXTERNAL_CONSTRAINTS: table::Values<ExternalConstraintsData<'static>> =
    table::Values::new("external constraints");
table::erase!(ExternalConstraintsData);

/// Solver constraints: their row in [`EXTERNAL_CONSTRAINTS`], which is never freed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct ExternalConstraints<'db> {
    index: NonZeroU32,
    _db: PhantomData<&'db ()>,
}

const _: () = assert!(size_of::<Option<ExternalConstraints<'static>>>() == 4);

impl<'db> ExternalConstraints<'db> {
    #[inline]
    pub fn new(_interner: DbInterner<'db>, data: ExternalConstraintsData<'db>) -> Self {
        ExternalConstraints { index: EXTERNAL_CONSTRAINTS.intern(data), _db: PhantomData }
    }

    #[inline]
    pub fn inner(&self) -> &ExternalConstraintsData<'db> {
        EXTERNAL_CONSTRAINTS.get(self.index)
    }

    /// Whether there are no constraints, which every fold maps to the same value: most
    /// responses.
    #[inline]
    fn is_empty(&self) -> bool {
        let data = self.inner();
        data.region_constraints.is_empty()
            && data.opaque_types.is_empty()
            && data.normalization_nested_goals.is_empty()
    }
}

impl<'db> std::ops::Deref for ExternalConstraints<'db> {
    type Target = ExternalConstraintsData<'db>;

    fn deref(&self) -> &Self::Target {
        self.inner()
    }
}

impl std::fmt::Debug for ExternalConstraints<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner().fmt(f)
    }
}

impl<'db> rustc_type_ir::TypeVisitable<DbInterner<'db>> for ExternalConstraints<'db> {
    fn visit_with<V: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
        &self,
        visitor: &mut V,
    ) -> V::Result {
        try_visit!(self.region_constraints.visit_with(visitor));
        try_visit!(self.opaque_types.visit_with(visitor));
        self.normalization_nested_goals.visit_with(visitor)
    }
}

impl<'db> rustc_type_ir::TypeFoldable<DbInterner<'db>> for ExternalConstraints<'db> {
    fn try_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Result<Self, F::Error> {
        if self.is_empty() {
            return Ok(self);
        }
        Ok(ExternalConstraints::new(
            folder.cx(),
            ExternalConstraintsData {
                // clone: folding takes the fields owned; only a response with constraints gets here.
                region_constraints: self.region_constraints.clone().try_fold_with(folder)?,
                opaque_types: self
                    .opaque_types
                    .iter()
                    .cloned()
                    .map(|opaque| opaque.try_fold_with(folder))
                    // alloc: the folded copy of a non-empty list, interned below.
                    .collect::<Result<_, F::Error>>()?,
                normalization_nested_goals: self
                    .normalization_nested_goals
                    // clone: as above.
                    .clone()
                    .try_fold_with(folder)?,
            },
        ))
    }
    fn fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(self, folder: &mut F) -> Self {
        if self.is_empty() {
            return self;
        }
        ExternalConstraints::new(
            folder.cx(),
            ExternalConstraintsData {
                // clone: folding takes the fields owned; only a response with constraints gets here.
                region_constraints: self.region_constraints.clone().fold_with(folder),
                opaque_types: self
                    .opaque_types
                    .iter()
                    .cloned()
                    .map(|opaque| opaque.fold_with(folder))
                    // alloc: the folded copy of a non-empty list, interned below.
                    .collect(),
                normalization_nested_goals: self
                    .normalization_nested_goals
                    // clone: as above.
                    .clone()
                    .fold_with(folder),
            },
        )
    }
}
