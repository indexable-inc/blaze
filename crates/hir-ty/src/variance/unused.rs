//! rustc's `check_variances_for_type_defn` (`rustc_hir_analysis/src/check/wfcheck.rs`): the
//! parameters of an ADT its variance leaves bivariant and no where clause constrains
//! (`identify_constrained_generic_params`, `constrained_generic_params.rs`), each one E0392.

use std::ops::ControlFlow;

use base_db::Db;
use blaze_mem::Frame;
use hir_def::{AdtId, GenericDefId, GenericParamId, VariantId};
use rustc_type_ir::{
    AliasTyKind, TypeSuperVisitable, TypeVisitable, TypeVisitableExt, TypeVisitor, Variance,
    inherent::IntoKind,
};

use crate::{
    FrameSet, GenericPredicates,
    db::HirDatabase as _,
    frame_set,
    generics::generics,
    next_solver::{ClauseKind, Const, ConstKind, DbInterner, Region, RegionKind, Ty, TyKind},
};

/// The generic parameters of `adt` that its variance leaves bivariant and that no projection
/// of its where clauses constrains, in declaration order (lifetimes first, as rustc's
/// `own_params`). Empty when a field's type, or that of an ADT it names, has an error (rustc's
/// `HasErrorDeep`): that error is reported instead.
///
/// Called outside queries (hir's E0392 check), so it runs in a task of its own (R1).
pub fn adt_unused_params(db: &Db, adt: AdtId) -> Vec<GenericParamId> {
    let def = GenericDefId::AdtId(adt);
    let variances = super::variances_of(db, def);
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
        let mut params = Vec::new_in(frame);
        params.extend(generics(db, def).iter_id(false));
        let mut constrained = frame_set(frame);
        constrained.extend(
            params
                .iter()
                .zip(variances.as_slice())
                .filter(|&(_, &variance)| variance != Variance::Bivariant)
                .map(|(&param, _)| param),
        );
        if constrained.len() == params.len() {
            return Vec::new(); // alloc: empty, allocates nothing
        }

        let mut projections = Vec::new_in(frame);
        projections.extend(GenericPredicates::query_own_explicit(db, def).iter_identity().filter_map(
            |it| match it.skip_norm_wip().kind().skip_binder() {
                ClauseKind::Projection(projection) => Some(projection),
                _ => None,
            },
        ));
        let mut inputs = frame_set(frame);
        loop {
            let before = constrained.len();
            for projection in &projections {
                inputs.clear();
                parameters_for(projection.projection_term, true, &mut inputs);
                if inputs.is_subset(&constrained) {
                    parameters_for(projection.term, false, &mut constrained);
                }
            }
            if constrained.len() == before {
                break;
            }
        }

        let mut unused = Vec::new_in(frame);
        unused.extend(params.iter().copied().filter(|param| !constrained.contains(param)));
        if unused.is_empty() || has_error_deep(db, adt, frame) {
            return Vec::new(); // alloc: empty, allocates nothing
        }
        unused.to_vec() // alloc: the caller's list, exact size
    })
}

/// The parameters in `value`, as rustc's `parameters_for`: those in a projection, an inherent
/// associated type, an opaque type or an unevaluated constant only with
/// `include_nonconstraining`, as those are not injective.
fn parameters_for<'db>(
    value: impl TypeVisitable<DbInterner<'db>>,
    include_nonconstraining: bool,
    out: &mut FrameSet<'_, GenericParamId>,
) {
    struct Collect<'a, 'f> {
        include_nonconstraining: bool,
        out: &'a mut FrameSet<'f, GenericParamId>,
    }
    impl<'db> TypeVisitor<DbInterner<'db>> for Collect<'_, '_> {
        type Result = ();
        fn visit_ty(&mut self, ty: Ty<'db>) {
            match ty.kind() {
                TyKind::Alias(_, alias)
                    if !self.include_nonconstraining
                        && matches!(
                            alias.kind,
                            AliasTyKind::Projection { .. }
                                | AliasTyKind::Inherent { .. }
                                | AliasTyKind::Opaque { .. }
                        ) =>
                {
                    return;
                }
                TyKind::Param(param) => {
                    self.out.insert(GenericParamId::TypeParamId(param.id));
                }
                _ => (),
            }
            ty.super_visit_with(self)
        }
        fn visit_region(&mut self, region: Region<'db>) {
            if let RegionKind::ReEarlyParam(param) = region.kind() {
                self.out.insert(GenericParamId::LifetimeParamId(param.id));
            }
        }
        fn visit_const(&mut self, konst: Const<'db>) {
            match konst.kind() {
                ConstKind::Alias(..) if !self.include_nonconstraining => return,
                ConstKind::Param(param) => {
                    self.out.insert(GenericParamId::ConstParamId(param.id));
                }
                _ => (),
            }
            konst.super_visit_with(self)
        }
    }
    value.visit_with(&mut Collect { include_nonconstraining, out });
}

/// rustc's `HasErrorDeep` over `type_of(adt)`: whether the type of a field of `adt`, or of an
/// ADT a field names (each once), has an error.
fn has_error_deep(db: &Db, adt: AdtId, frame: &Frame) -> bool {
    struct HasErrorDeep<'db, 'f> {
        db: &'db Db,
        seen: FrameSet<'f, AdtId>,
    }
    impl<'db> HasErrorDeep<'db, '_> {
        fn fields(&mut self, adt: AdtId) -> ControlFlow<()> {
            if !self.seen.insert(adt) {
                return ControlFlow::Continue(());
            }
            let db = self.db;
            let mut variant = |variant: VariantId| {
                for (_, field) in db.field_types(variant).iter() {
                    field.ty().instantiate_identity().skip_norm_wip().visit_with(self)?;
                }
                ControlFlow::Continue(())
            };
            match adt {
                AdtId::StructId(it) => variant(VariantId::StructId(it)),
                AdtId::UnionId(it) => variant(VariantId::UnionId(it)),
                AdtId::EnumId(it) => {
                    for &(it, _) in it.enum_variants(db).variants.values() {
                        variant(VariantId::EnumVariantId(it))?;
                    }
                    ControlFlow::Continue(())
                }
            }
        }
    }
    impl<'db> TypeVisitor<DbInterner<'db>> for HasErrorDeep<'db, '_> {
        type Result = ControlFlow<()>;
        fn visit_ty(&mut self, ty: Ty<'db>) -> ControlFlow<()> {
            match ty.kind() {
                TyKind::Adt(..) => {
                    if let Some((adt, _)) = ty.as_adt() {
                        self.fields(adt)?;
                    }
                }
                TyKind::Error(_) => return ControlFlow::Break(()),
                _ => (),
            }
            ty.super_visit_with(self)
        }
        fn visit_region(&mut self, region: Region<'db>) -> ControlFlow<()> {
            match region.kind() {
                RegionKind::ReError(_) => ControlFlow::Break(()),
                _ => ControlFlow::Continue(()),
            }
        }
        fn visit_const(&mut self, konst: Const<'db>) -> ControlFlow<()> {
            match konst.references_error() {
                true => ControlFlow::Break(()),
                false => ControlFlow::Continue(()),
            }
        }
    }
    HasErrorDeep { db, seen: frame_set(frame) }.fields(adt).is_break()
}
