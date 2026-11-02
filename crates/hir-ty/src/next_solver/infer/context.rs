//! Definition of `InferCtxtLike` from the librarified type layer.

use std::range::RangeInclusive;

use rustc_type_ir::{
    ConstVid, FloatVarValue, FloatVid, InferConst, InferTy, IntVarValue, IntVid, OpaqueAliasTy,
    RegionKind, RegionVid, TyVid, TypeFoldable, TypeFolder, TypeSuperFoldable, TypeVisitableExt,
    TypingMode, UniverseIndex, Variance,
    inherent::{IntoKind, Ty as _},
    region_constraint::{Assumptions, RegionConstraint},
    relate::combine::PredicateEmittingRelation,
    solve::{TyOrConstInferVar, VisibleForLeakCheck},
};

use crate::{
    Span,
    next_solver::{
        Binder, Const, ConstKind, DbInterner, ErrorGuaranteed, GenericArgs, OpaqueTypeKey, Region,
        SolverDefId, Ty, TyKind,
        infer::opaque_types::{OpaqueHiddenType, table::OpaqueTypeStorageEntries},
    },
};

use super::{
    BoundRegionConversionTime, InferCtxt, relate::GeneralizeCache, relate::RelateResult,
    type_variable::TypeVariableValue, unify_key::ConstVariableValue,
};

impl<'db> rustc_type_ir::InferCtxtLike for InferCtxt<'db> {
    type Interner = DbInterner<'db>;

    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn next_trait_solver(&self) -> bool {
        true
    }

    fn enable_next_solver_overflow_fcw(&self) -> bool {
        self.enable_next_solver_overflow_fcw()
    }

    fn disable_trait_solver_fast_paths(&self) -> bool {
        false
    }

    fn typing_mode_raw(&self) -> TypingMode<DbInterner<'db>> {
        self.typing_mode_raw()
    }

    fn universe(&self) -> UniverseIndex {
        self.universe()
    }

    fn create_next_universe(&self) -> UniverseIndex {
        self.create_next_universe()
    }

    fn insert_placeholder_assumptions(
        &self,
        u: UniverseIndex,
        assumptions: Assumptions<DbInterner<'db>>,
    ) {
        self.insert_placeholder_assumptions(u, assumptions);
    }

    fn get_placeholder_assumptions(&self, u: UniverseIndex) -> Assumptions<DbInterner<'db>> {
        self.get_placeholder_assumptions(u)
    }

    fn get_solver_region_constraint(&self) -> RegionConstraint<DbInterner<'db>> {
        self.get_solver_region_constraint().without_spans()
    }

    fn has_placeholder_assumptions(&self, range: RangeInclusive<UniverseIndex>) -> bool {
        self.has_placeholder_assumptions(range)
    }

    fn overwrite_solver_region_constraint(
        &self,
        constraint: RegionConstraint<DbInterner<'db>>,
        span: Span,
    ) {
        self.overwrite_solver_region_constraint(constraint.with_spans(span));
    }

    fn universe_of_ty(&self, vid: TyVid) -> Option<UniverseIndex> {
        self.try_resolve_ty_var(vid).err()
    }

    fn universe_of_region(&self, lt: RegionVid) -> Option<UniverseIndex> {
        self.inner.borrow_mut().unwrap_region_constraints().try_resolve_region_var(lt).err()
    }

    fn universe_of_const(&self, ct: ConstVid) -> Option<UniverseIndex> {
        self.try_resolve_const_var(ct).err()
    }

    fn root_ty_var(&self, var: TyVid) -> TyVid {
        self.root_var(var)
    }

    fn sub_unification_table_root_var(&self, var: TyVid) -> TyVid {
        self.sub_unification_table_root_var(var)
    }

    #[inline]
    fn is_sub_unification_table_root_var(&self, vid: TyVid) -> bool {
        self.inner.borrow().type_variable_storage.is_sub_unification_table_root_var(vid)
    }

    fn root_const_var(&self, var: ConstVid) -> ConstVid {
        self.root_const_var(var)
    }

    fn shallow_resolve_ty_var(&self, vid: TyVid) -> Ty<'db> {
        self.shallow_resolve_ty_var(vid)
    }

    fn shallow_resolve_int_var(&self, vid: IntVid) -> Ty<'db> {
        self.shallow_resolve_int_var(vid)
    }

    fn shallow_resolve_float_var(&self, vid: FloatVid) -> Ty<'db> {
        self.shallow_resolve_float_var(vid)
    }

    fn shallow_resolve_const_var(&self, vid: ConstVid) -> Const<'db> {
        self.shallow_resolve_const_var(vid)
    }

    fn shallow_resolve_region_var(&self, vid: RegionVid) -> Region<'db> {
        self.inner
            .borrow_mut()
            .unwrap_region_constraints()
            .shallow_resolve_region_var(self.interner, vid)
    }

    fn ty_or_const_infer_var_changed(&self, var: TyOrConstInferVar) -> bool {
        self.ty_or_const_infer_var_changed(var)
    }

    fn next_region_infer(&self) -> Region<'db> {
        self.next_region_var(Span::Dummy)
    }

    fn next_ty_infer(&self) -> Ty<'db> {
        self.next_ty_var(Span::Dummy)
    }

    fn next_const_infer(&self) -> Const<'db> {
        self.next_const_var(Span::Dummy)
    }

    fn fresh_args_for_item(&self, def_id: SolverDefId<'db>) -> GenericArgs<'db> {
        self.fresh_args_for_item(Span::Dummy, def_id)
    }

    fn instantiate_binder_with_infer<T: TypeFoldable<DbInterner<'db>> + Copy>(
        &self,
        value: Binder<'db, T>,
    ) -> T {
        self.instantiate_binder_with_fresh_vars(
            Span::Dummy,
            BoundRegionConversionTime::HigherRankedType,
            value,
        )
    }

    fn enter_forall_without_assumptions<T: TypeFoldable<DbInterner<'db>>, U>(
        &self,
        value: Binder<'db, T>,
        f: impl FnOnce(T) -> U,
    ) -> U {
        self.enter_forall(value, f)
    }

    fn enter_forall_with_empty_assumptions<T: TypeFoldable<DbInterner<'db>>, U>(
        &self,
        value: Binder<'db, T>,
        f: impl FnOnce(T) -> U,
    ) -> U {
        self.enter_forall(value, |value| {
            // Read only with assumptions on binders, and the vendored `Assumptions::empty`
            // cannot be built without them.
            if rustc_type_ir::Interner::assumptions_on_binders(self.interner) {
                let u = self.universe();
                self.placeholder_assumptions.borrow_mut().insert(u, Assumptions::empty());
            }
            f(value)
        })
    }

    fn equate_ty_vids_raw(&self, a: TyVid, b: TyVid) {
        self.inner.borrow_mut().type_variables().equate(a, b);
    }

    fn sub_unify_ty_vids_raw(&self, a: TyVid, b: TyVid) {
        self.sub_unify_ty_vids_raw(a, b);
    }

    fn equate_int_vids_raw(&self, a: IntVid, b: IntVid) {
        self.inner.borrow_mut().int_unification_table().union(a, b);
    }

    fn equate_float_vids_raw(&self, a: FloatVid, b: FloatVid) {
        self.inner.borrow_mut().float_unification_table().union(a, b);
    }

    fn equate_const_vids_raw(&self, a: ConstVid, b: ConstVid) {
        self.inner.borrow_mut().const_unification_table().union(a, b);
    }

    fn instantiate_ty_var_raw(&self, vid: TyVid, ty: Ty<'db>) {
        let universe = self
            .try_resolve_ty_var(vid)
            .expect_err("instantiating a type variable that is already known");
        let ty = lower_universe(self, universe, ty);

        self.inner.borrow_mut().type_variables().instantiate(vid, ty);
    }

    fn instantiate_const_var_raw(&self, vid: ConstVid, ct: Const<'db>) {
        let universe = self
            .try_resolve_const_var(vid)
            .expect_err("instantiating a const variable that is already known");
        let ct = lower_universe(self, universe, ct);

        self.inner
            .borrow_mut()
            .const_unification_table()
            .union_value(vid, ConstVariableValue::Known { value: ct });
    }

    fn instantiate_ty_var<R: PredicateEmittingRelation<Self>>(
        &self,
        relation: &mut R,
        target_is_expected: bool,
        target_vid: TyVid,
        instantiation_variance: Variance,
        source_ty: Ty<'db>,
    ) -> RelateResult<'db, ()> {
        self.instantiate_ty_var(
            relation,
            target_is_expected,
            target_vid,
            instantiation_variance,
            source_ty,
        )
    }

    fn instantiate_int_var_raw(&self, vid: IntVid, value: IntVarValue) {
        self.inner.borrow_mut().int_unification_table().union_value(vid, value);
    }

    fn instantiate_float_var_raw(&self, vid: FloatVid, value: FloatVarValue) {
        self.inner.borrow_mut().float_unification_table().union_value(vid, value);
    }

    fn instantiate_const_var<R: PredicateEmittingRelation<Self>>(
        &self,
        relation: &mut R,
        target_is_expected: bool,
        target_vid: ConstVid,
        source_ct: Const<'db>,
    ) -> RelateResult<'db, ()> {
        self.instantiate_const_var(relation, target_is_expected, target_vid, source_ct)
    }

    fn set_tainted_by_errors(&self, e: ErrorGuaranteed) {
        self.set_tainted_by_errors(e)
    }

    fn shallow_resolve(&self, ty: Ty<'db>) -> Ty<'db> {
        self.shallow_resolve(ty)
    }

    fn shallow_resolve_const(&self, ct: Const<'db>) -> Const<'db> {
        self.shallow_resolve_const(ct)
    }

    fn deeply_resolve_ignoring_regions<T>(&self, value: T) -> T
    where
        T: TypeFoldable<DbInterner<'db>>,
    {
        self.deeply_resolve_ignoring_regions(value)
    }

    fn probe<T>(&self, probe: impl FnOnce() -> T) -> T {
        self.probe(|_| probe())
    }

    fn commit_if_ok<T, E>(&self, f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
        self.commit_if_ok(|_| f())
    }

    fn sub_regions(&self, sub: Region<'db>, sup: Region<'db>, vis: VisibleForLeakCheck, _span: Span) {
        self.inner.borrow_mut().unwrap_region_constraints().make_subregion(sub, sup, vis);
    }

    fn equate_regions(&self, a: Region<'db>, b: Region<'db>, vis: VisibleForLeakCheck, _span: Span) {
        self.inner.borrow_mut().unwrap_region_constraints().make_eqregion(a, b, vis);
    }

    fn register_solver_region_constraint(
        &self,
        c: RegionConstraint<DbInterner<'db>>,
        span: Span,
    ) {
        self.register_solver_region_constraint(c.with_spans(span));
    }

    fn register_ty_outlives(&self, ty: Ty<'db>, r: Region<'db>, _span: Span) {
        if self.interner.regions() {
            self.register_type_outlives_constraint(ty, r);
        }
    }

    type OpaqueTypeStorageEntries = OpaqueTypeStorageEntries;

    #[inline]
    fn opaque_types_storage_num_entries(&self) -> OpaqueTypeStorageEntries {
        self.inner.borrow_mut().opaque_types().num_entries()
    }

    fn clone_opaque_types_lookup_table(&self) -> Vec<(OpaqueTypeKey<'db>, Ty<'db>)> {
        // alloc: `InferCtxtLike` returns an owned `Vec`; none without opaque types.
        self.inner.borrow_mut().opaque_types().iter_lookup_table().map(|(k, h)| (k, h.ty)).collect()
    }

    fn clone_duplicate_opaque_types(&self) -> Vec<(OpaqueTypeKey<'db>, Ty<'db>)> {
        self.inner
            .borrow_mut()
            .opaque_types()
            .iter_duplicate_entries()
            .map(|(k, h)| (k, h.ty))
            // alloc: `InferCtxtLike` returns an owned `Vec`; none without opaque types.
            .collect()
    }

    fn clone_opaque_types_added_since(
        &self,
        prev_entries: OpaqueTypeStorageEntries,
    ) -> Vec<(OpaqueTypeKey<'db>, Ty<'db>)> {
        // alloc: `InferCtxtLike` returns an owned `Vec`; none without opaque types.
        let mut added = Vec::new();
        self.clone_opaque_types_added_since_into(prev_entries, &mut added);
        added
    }

    fn clone_opaque_types_added_since_into(
        &self,
        prev_entries: OpaqueTypeStorageEntries,
        into: &mut Vec<(OpaqueTypeKey<'db>, Ty<'db>)>,
    ) {
        into.extend(
            self.inner
                .borrow_mut()
                .opaque_types()
                .opaque_types_added_since(prev_entries)
                .map(|(k, h)| (k, h.ty)),
        );
    }

    fn opaques_with_sub_unified_hidden_type(
        &self,
        ty_vid: TyVid,
    ) -> Vec<OpaqueAliasTy<DbInterner<'db>>> {
        let ty_sub_vid = self.sub_unification_table_root_var(ty_vid);
        let inner = &mut *self.inner.borrow_mut();
        let mut type_variables = inner.type_variable_storage.table();
        inner
            .opaque_type_storage
            .iter_opaque_types()
            .filter_map(|(key, hidden_ty)| {
                if let TyKind::Infer(InferTy::TyVar(hidden_vid)) = hidden_ty.ty.kind() {
                    let opaque_sub_vid = type_variables.sub_unification_table_root_var(hidden_vid);
                    if opaque_sub_vid == ty_sub_vid {
                        return Some(OpaqueAliasTy::new_opaque_from_args(
                            self.interner,
                            key.def_id,
                            key.args,
                        ));
                    }
                }

                None
            })
            // alloc: `InferCtxtLike` returns an owned `Vec`; none without opaque types.
            .collect()
    }

    fn register_hidden_type_in_storage(
        &self,
        opaque_type_key: OpaqueTypeKey<'db>,
        hidden_ty: Ty<'db>,
        _span: Span,
    ) -> Option<Ty<'db>> {
        self.register_hidden_type_in_storage(opaque_type_key, OpaqueHiddenType { ty: hidden_ty })
    }

    fn add_duplicate_opaque_type(
        &self,
        opaque_type_key: OpaqueTypeKey<'db>,
        hidden_ty: Ty<'db>,
        _span: Span,
    ) {
        self.inner
            .borrow_mut()
            .opaque_types()
            .add_duplicate(opaque_type_key, OpaqueHiddenType { ty: hidden_ty })
    }

    fn reset_opaque_types(&self) {
        let _ = self.take_opaque_types();
    }
}

fn lower_universe<'db, T: TypeFoldable<DbInterner<'db>> + Copy>(
    infcx: &InferCtxt<'db>,
    for_universe: UniverseIndex,
    value: T,
) -> T {
    let value = value.fold_with(&mut LowerUniverseFolder {
        infcx,
        for_universe,
        cache: Default::default(),
    });

    // Placeholders are not lowered by the folder, so a value naming a placeholder the variable's
    // universe can't name is a bug in the caller.
    #[cfg(debug_assertions)]
    {
        let value_universe = rustc_type_ir::max_universe(infcx, value);
        assert!(
            for_universe.can_name(value_universe),
            "variable in universe {for_universe:?} can't name value in universe {value_universe:?}",
        );
    }

    value
}

/// Canonicalizing inputs puts all inference variables and placeholders
/// into the root universe.
///
/// This means when instantiating the query response we need to pull
/// down the universe of returned `var_values` to the universe of
/// the inference variable in `orig_values`.
///
/// This folder is similar to the `Generalizer`, except that it simply
/// structurally folds non-rigid aliases as these should have already
/// been generalized in the query so we shouldn't try to do it again.
struct LowerUniverseFolder<'a, 'db> {
    infcx: &'a InferCtxt<'db>,
    for_universe: UniverseIndex,
    cache: GeneralizeCache<Ty<'db>, Ty<'db>>,
}

impl<'a, 'db> TypeFolder<DbInterner<'db>> for LowerUniverseFolder<'a, 'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.infcx.interner
    }

    fn fold_ty(&mut self, t: Ty<'db>) -> Ty<'db> {
        if !(t.has_free_regions() || t.has_infer()) {
            return t;
        }

        if let Some(answer) = self.cache.get(&t) {
            return answer;
        }

        let folded = match t.kind() {
            TyKind::Infer(InferTy::TyVar(vid)) => {
                let vid = self.infcx.root_var(vid);
                let probe = self.infcx.inner.borrow_mut().type_variables().probe(vid);
                match probe {
                    TypeVariableValue::Known { value: u } => u.super_fold_with(self),
                    TypeVariableValue::Unknown { universe } => {
                        if self.for_universe.can_name(universe) {
                            t
                        } else {
                            let mut inner = self.infcx.inner.borrow_mut();
                            let span = inner.type_variables().var_span(vid);
                            let new_var_id =
                                inner.type_variables().new_var(self.for_universe, span);
                            inner.type_variables().equate(vid, new_var_id);
                            Ty::new_var(self.infcx.interner, new_var_id)
                        }
                    }
                }
            }
            _ => t.super_fold_with(self),
        };

        self.cache.insert(t, folded);
        folded
    }

    fn fold_const(&mut self, c: Const<'db>) -> Const<'db> {
        if !(c.has_free_regions() || c.has_infer()) {
            return c;
        }

        match c.kind() {
            ConstKind::Infer(InferConst::Var(vid)) => {
                let vid = self.infcx.root_const_var(vid);
                let universe = match self.infcx.try_resolve_const_var(vid) {
                    Ok(value) => return value.fold_with(self),
                    Err(universe) => universe,
                };
                if self.for_universe.can_name(universe) {
                    c
                } else {
                    let span = self
                        .infcx
                        .const_var_span(vid)
                        .expect("an unresolved const variable has a span");
                    let new_var_id =
                        self.infcx.next_const_vid_in_universe(self.for_universe, span);
                    self.infcx.inner.borrow_mut().const_unification_table().union(vid, new_var_id);
                    Const::new_var(self.infcx.interner, new_var_id)
                }
            }
            _ => c.super_fold_with(self),
        }
    }

    fn fold_region(&mut self, r: Region<'db>) -> Region<'db> {
        match r.kind() {
            RegionKind::ReBound(..) | RegionKind::ReErased => r,
            _ => {
                let r_universe = self.infcx.universe_of_region(r);
                if self.for_universe.can_name(r_universe) {
                    r
                } else {
                    let new_region =
                        self.infcx.next_region_var_in_universe(self.for_universe, Span::Dummy);
                    self.infcx.inner.borrow_mut().unwrap_region_constraints().make_eqregion(
                        r,
                        new_region,
                        VisibleForLeakCheck::Yes,
                    );
                    new_region
                }
            }
        }
    }
}
