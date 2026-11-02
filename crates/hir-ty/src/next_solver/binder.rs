use hir_def::{TraitId, TypeAliasId};
use macros::{TypeFoldable, TypeVisitable};
use rustc_type_ir::{
    FallibleTypeFolder, TypeFoldable as _, TypeFolder, TypeVisitable as _, TypeVisitableExt as _,
    TypeVisitor,
};

use crate::next_solver::{
    Binder, Clauses, DbInterner, EarlyBinder, FnSig, FnSigKind, GenericArg, PolyFnSig,
    StoredBoundVarKinds, StoredClauses, StoredGenericArg, StoredGenericArgs, StoredTy, StoredTys,
    TraitRef, Ty,
};

/// An [`EarlyBinder`] as kept in a query result.
///
/// # Invariant
///
/// The value has no rigid aliases: instantiating it needs normalizing again. [`Self::bind`]
/// makes every alias [`IsRigid::No`](rustc_type_ir::IsRigid::No), as rustc's
/// [`EarlyBinder::bind`] does; the getters check it in debug builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct StoredEarlyBinder<T>(T);

impl<T> StoredEarlyBinder<T> {
    /// Binds `value`, making its aliases non-rigid (a no-op, by its flags, for a lowered value).
    #[inline]
    pub fn bind<'db>(interner: DbInterner<'db>, value: T) -> Self
    where
        T: rustc_type_ir::TypeFoldable<DbInterner<'db>>,
    {
        Self(rustc_type_ir::set_aliases_to_non_rigid(interner, value).skip_normalization())
    }

    /// Binds `value`, which already has no rigid aliases (checked in debug builds), as rustc's
    /// [`EarlyBinder::bind_no_rigid_aliases`]: for where no interner is at hand.
    #[inline]
    pub fn bind_no_rigid_aliases<'db>(value: T) -> Self
    where
        T: rustc_type_ir::TypeVisitable<DbInterner<'db>>,
    {
        debug_assert!(!value.has_rigid_aliases());
        Self(value)
    }

    #[inline]
    pub fn skip_binder(self) -> T {
        self.0
    }

    #[inline]
    pub fn as_ref(&self) -> StoredEarlyBinder<&T> {
        StoredEarlyBinder(&self.0)
    }

    /// Binds what `f` reads out of the stored value; `f` must keep it free of rigid aliases.
    #[inline]
    pub fn get_with<'db, 'a, R>(&'a self, f: impl FnOnce(&'a T) -> R) -> EarlyBinder<'db, R> {
        EarlyBinder::bind_unchecked(f(&self.0))
    }
}

impl StoredEarlyBinder<(TypeAliasId, StoredGenericArgs)> {
    /// An associated type and its arguments; [`Self::bind`] for the arguments, as the id has
    /// nothing to fold.
    #[inline]
    pub fn bind_assoc_ty<'db>(
        interner: DbInterner<'db>,
        assoc_ty: TypeAliasId,
        args: StoredGenericArgs,
    ) -> Self {
        Self((assoc_ty, StoredEarlyBinder::bind(interner, args).0))
    }
}

impl StoredEarlyBinder<StoredTy> {
    #[inline]
    pub fn get<'db>(&self) -> EarlyBinder<'db, Ty<'db>> {
        EarlyBinder::bind_no_rigid_aliases(self.0.as_ref())
    }
}

impl StoredEarlyBinder<StoredGenericArg> {
    #[inline]
    pub fn get<'db>(&self) -> EarlyBinder<'db, GenericArg<'db>> {
        EarlyBinder::bind_no_rigid_aliases(self.0.as_ref())
    }
}

impl StoredEarlyBinder<StoredClauses> {
    #[inline]
    pub fn get<'db>(&self) -> EarlyBinder<'db, Clauses<'db>> {
        EarlyBinder::bind_no_rigid_aliases(self.0.as_ref())
    }
}

impl StoredEarlyBinder<StoredPolyFnSig> {
    #[inline]
    pub fn get<'db>(&'db self) -> EarlyBinder<'db, PolyFnSig<'db>> {
        EarlyBinder::bind_no_rigid_aliases(self.0.get())
    }
}

impl StoredEarlyBinder<StoredTraitRef> {
    #[inline]
    pub fn get<'db>(&'db self, interner: DbInterner<'db>) -> EarlyBinder<'db, TraitRef<'db>> {
        EarlyBinder::bind_no_rigid_aliases(self.0.get(interner))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct StoredPolyFnSig {
    bound_vars: StoredBoundVarKinds,
    sig: StoredFnSig,
}

impl StoredPolyFnSig {
    #[inline]
    pub fn new(sig: PolyFnSig<'_>) -> Self {
        let bound_vars = sig.bound_vars().store();
        Self { bound_vars, sig: StoredFnSig::new(sig.skip_binder()) }
    }

    #[inline]
    pub fn get<'db>(&self) -> PolyFnSig<'db> {
        Binder::bind_with_vars(self.sig.get(), self.bound_vars.as_ref())
    }
}

impl<'db> rustc_type_ir::TypeVisitable<DbInterner<'db>> for StoredPolyFnSig {
    fn visit_with<V: TypeVisitor<DbInterner<'db>>>(&self, visitor: &mut V) -> V::Result {
        self.get().visit_with(visitor)
    }
}

impl<'db> rustc_type_ir::TypeFoldable<DbInterner<'db>> for StoredPolyFnSig {
    fn try_fold_with<F: FallibleTypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Result<Self, F::Error> {
        Ok(StoredPolyFnSig::new(self.get().try_fold_with(folder)?))
    }

    fn fold_with<F: TypeFolder<DbInterner<'db>>>(self, folder: &mut F) -> Self {
        StoredPolyFnSig::new(self.get().fold_with(folder))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, TypeVisitable, TypeFoldable)]
pub struct StoredFnSig {
    inputs_and_output: StoredTys,
    #[type_visitable(ignore)]
    fn_sig_kind: FnSigKind<'static>,
}

impl StoredFnSig {
    #[inline]
    pub fn new(sig: FnSig<'_>) -> Self {
        let kind = sig.fn_sig_kind;
        let fn_sig_kind = FnSigKind::default()
            .set_abi(kind.abi())
            .set_safety(kind.safety())
            .set_c_variadic(kind.c_variadic())
            .set_splatted(kind.splatted(), sig.inputs().len())
            .expect("the splatted index was checked against these same inputs when `sig` was made");
        Self { inputs_and_output: sig.inputs_and_output.store(), fn_sig_kind }
    }

    #[inline]
    pub fn get<'db>(&self) -> FnSig<'db> {
        FnSig { inputs_and_output: self.inputs_and_output.as_ref(), fn_sig_kind: self.fn_sig_kind }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, TypeVisitable, TypeFoldable, blaze_query::Value)]
pub struct StoredTraitRef {
    #[type_visitable(ignore)]
    def_id: TraitId,
    args: StoredGenericArgs,
}

impl StoredTraitRef {
    #[inline]
    pub fn new(trait_ref: TraitRef<'_>) -> Self {
        Self { def_id: trait_ref.def_id.0, args: trait_ref.args.store() }
    }

    #[inline]
    pub fn get<'db>(&'db self, interner: DbInterner<'db>) -> TraitRef<'db> {
        TraitRef::new_from_args(interner, self.def_id.into(), self.args.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use rustc_type_ir::{TyKind, UintTy};

    use super::*;
    use crate::next_solver::Tys;

    #[test]
    fn a_stored_fn_sig_keeps_its_kind_and_splatted_arg() {
        let u8_ty = Ty::intern(TyKind::Uint(UintTy::U8));
        let inputs_and_output = Tys::new_from_slice(&[u8_ty, u8_ty]);
        let kinds = [
            FnSigKind::dummy(),
            FnSigKind::dummy().set_c_variadic(true),
            FnSigKind::dummy().set_splatted(Some(0), 1).expect("one input, so index 0 is in bounds"),
        ];
        for fn_sig_kind in kinds {
            let sig = FnSig { inputs_and_output, fn_sig_kind };
            let stored = StoredFnSig::new(sig);
            assert!(stored.get() == sig);
            assert!(stored.get().splatted() == fn_sig_kind.splatted());
        }
    }
}
