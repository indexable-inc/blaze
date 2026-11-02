//! Type inhabitedness logic.
use std::ops::ControlFlow::{self, Break, Continue};

use base_db::{Crate, Db};
use blaze_mem::Frame;
use hir_def::{
    AdtId, EnumVariantId, HasModule, ModuleId, VariantId, attrs::AttrFlags,
    signatures::VariantFields, visibility::Visibility,
};
use rustc_type_ir::{TypeSuperVisitable, TypeVisitable, TypeVisitor, inherent::IntoKind};

use crate::{
    FrameSet,
    consteval::try_const_usize,
    frame_set,
    db::HirDatabase,
    next_solver::{
        DbInterner, EarlyBinder, GenericArgs, ParamEnv, Ty, TyKind,
        infer::{InferCtxt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
};

// FIXME: Turn this into a query, it can be quite slow
/// Checks whether a type is visibly uninhabited from a particular module.
pub fn is_ty_uninhabited_from<'db>(
    infcx: &InferCtxt<'db>,
    ty: Ty<'db>,
    target_mod: ModuleId,
    env: ParamEnv<'db>,
) -> bool {
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
        is_ty_uninhabited_from_in(frame, infcx, ty, target_mod, env)
    })
}

/// [`is_ty_uninhabited_from`], its set of the types being visited in the caller's `frame`.
pub fn is_ty_uninhabited_from_in<'db>(
    frame: &Frame,
    infcx: &InferCtxt<'db>,
    ty: Ty<'db>,
    target_mod: ModuleId,
    env: ParamEnv<'db>,
) -> bool {
    let _p = tracing::info_span!("is_ty_uninhabited_from", ?ty).entered();
    let local = target_mod.krate(infcx.interner.db);
    let mut uninhabited_from = UninhabitedFrom::new(frame, infcx, Some(target_mod), local, env);
    let inhabitedness = ty.visit_with(&mut uninhabited_from);
    inhabitedness == BREAK_VISIBLY_UNINHABITED
}

// FIXME: Turn this into a query, it can be quite slow
/// Checks whether a variant is visibly uninhabited from a particular module.
pub fn is_enum_variant_uninhabited_from<'db>(
    infcx: &InferCtxt<'db>,
    variant: EnumVariantId,
    subst: GenericArgs<'db>,
    target_mod: ModuleId,
    env: ParamEnv<'db>,
) -> bool {
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
        is_enum_variant_uninhabited_from_in(frame, infcx, variant, subst, target_mod, env)
    })
}

/// [`is_enum_variant_uninhabited_from`], its set of the types being visited in the caller's
/// `frame`.
pub fn is_enum_variant_uninhabited_from_in<'db>(
    frame: &Frame,
    infcx: &InferCtxt<'db>,
    variant: EnumVariantId,
    subst: GenericArgs<'db>,
    target_mod: ModuleId,
    env: ParamEnv<'db>,
) -> bool {
    let _p = tracing::info_span!("is_enum_variant_uninhabited_from").entered();

    let local = target_mod.krate(infcx.interner.db);
    let mut uninhabited_from = UninhabitedFrom::new(frame, infcx, Some(target_mod), local, env);
    let inhabitedness = uninhabited_from.visit_variant(variant.into(), subst);
    inhabitedness == BREAK_VISIBLY_UNINHABITED
}

/// Whether a variant is uninhabited with every field taken as visible: rustc's
/// `inhabited_predicate(..).apply_ignore_module`, negated, as `witness_1_is_privately_uninhabited`
/// reads it. `local` is the crate being checked, whose own `#[non_exhaustive]` enums count.
pub fn is_enum_variant_privately_uninhabited<'db>(
    infcx: &InferCtxt<'db>,
    variant: EnumVariantId,
    subst: GenericArgs<'db>,
    local: Crate,
    env: ParamEnv<'db>,
) -> bool {
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
        let mut uninhabited = UninhabitedFrom::new(frame, infcx, None, local, env);
        uninhabited.visit_variant(variant.into(), subst) == BREAK_VISIBLY_UNINHABITED
    })
}

struct UninhabitedFrom<'a, 'db> {
    /// The module the fields must be visible from; `None` takes every field as visible.
    target_mod: Option<ModuleId>,
    /// The crate being checked: a `#[non_exhaustive]` enum of another is inhabited.
    local: Crate,
    recursive_ty: FrameSet<'a, Ty<'db>>,
    // guard for preventing stack overflow in non trivial non terminating types
    max_depth: usize,
    infcx: &'a InferCtxt<'db>,
    env: ParamEnv<'db>,
}

const CONTINUE_OPAQUELY_INHABITED: ControlFlow<VisiblyUninhabited> = Continue(());
const BREAK_VISIBLY_UNINHABITED: ControlFlow<VisiblyUninhabited> = Break(VisiblyUninhabited);
#[derive(PartialEq, Eq)]
struct VisiblyUninhabited;

impl<'db> TypeVisitor<DbInterner<'db>> for UninhabitedFrom<'_, 'db> {
    type Result = ControlFlow<VisiblyUninhabited>;

    fn visit_ty(&mut self, mut ty: Ty<'db>) -> ControlFlow<VisiblyUninhabited> {
        if self.recursive_ty.contains(&ty) || self.max_depth == 0 {
            // rustc considers recursive types always inhabited. I think it is valid to consider
            // recursive types as always uninhabited, but we should do what rustc is doing.
            return CONTINUE_OPAQUELY_INHABITED;
        }
        self.recursive_ty.insert(ty);
        self.max_depth -= 1;

        if matches!(ty.kind(), TyKind::Alias(..)) {
            let mut ocx = ObligationCtxt::new(self.infcx);
            match ocx.structurally_normalize_ty(&ObligationCause::dummy(), self.env, ty) {
                Ok(it) => ty = it,
                Err(_) => return CONTINUE_OPAQUELY_INHABITED,
            }
        }

        let r = match ty.kind() {
            TyKind::Adt(adt, subst) => self.visit_adt(adt.def_id(), subst),
            TyKind::Never => BREAK_VISIBLY_UNINHABITED,
            TyKind::Tuple(..) => ty.super_visit_with(self),
            TyKind::Array(item_ty, len) => match try_const_usize(self.infcx.interner.db, len) {
                Some(0) | None => CONTINUE_OPAQUELY_INHABITED,
                Some(1..) => item_ty.visit_with(self),
            },
            _ => CONTINUE_OPAQUELY_INHABITED,
        };
        self.recursive_ty.remove(&ty);
        self.max_depth += 1;
        r
    }
}

impl<'a, 'db> UninhabitedFrom<'a, 'db> {
    fn new(
        frame: &'a Frame,
        infcx: &'a InferCtxt<'db>,
        target_mod: Option<ModuleId>,
        local: Crate,
        env: ParamEnv<'db>,
    ) -> Self {
        Self { target_mod, local, recursive_ty: frame_set(frame), max_depth: 500, infcx, env }
    }

    #[inline]
    fn interner(&self) -> DbInterner<'db> {
        self.infcx.interner
    }

    #[inline]
    fn db(&self) -> &'db Db {
        self.interner().db
    }

    fn visit_adt(
        &mut self,
        adt: AdtId,
        subst: GenericArgs<'db>,
    ) -> ControlFlow<VisiblyUninhabited> {
        // An ADT is uninhabited iff all its variants uninhabited.
        match adt {
            // rustc: For now, `union`s are never considered uninhabited.
            AdtId::UnionId(_) => CONTINUE_OPAQUELY_INHABITED,
            AdtId::StructId(s) => self.visit_variant(s.into(), subst),
            // rustc's `variant_list_has_applicable_non_exhaustive`: an enum another crate may
            // add a variant to. A `#[non_exhaustive]` struct or variant is walked all the same.
            AdtId::EnumId(e)
                if e.krate(self.db()) != self.local
                    && AttrFlags::query(self.db(), e.into()).contains(AttrFlags::NON_EXHAUSTIVE) =>
            {
                CONTINUE_OPAQUELY_INHABITED
            }
            AdtId::EnumId(e) => {
                let enum_data = e.enum_variants(self.db());

                for &(variant, _) in enum_data.variants.values() {
                    let variant_inhabitedness = self.visit_variant(variant.into(), subst);
                    match variant_inhabitedness {
                        Break(VisiblyUninhabited) => (),
                        Continue(()) => return CONTINUE_OPAQUELY_INHABITED,
                    }
                }
                BREAK_VISIBLY_UNINHABITED
            }
        }
    }

    fn visit_variant(
        &mut self,
        variant: VariantId,
        subst: GenericArgs<'db>,
    ) -> ControlFlow<VisiblyUninhabited> {
        let variant_data = variant.fields(self.db());
        let fields = variant_data.fields();
        if fields.is_empty() {
            return CONTINUE_OPAQUELY_INHABITED;
        }

        let is_enum = matches!(variant, VariantId::EnumVariantId(..));
        let field_tys = self.db().field_types(variant);
        let field_vis = if is_enum {
            None
        } else {
            Some(VariantFields::field_visibilities(self.db(), variant))
        };

        for (fid, _) in fields.iter() {
            self.visit_field(field_vis.as_ref().map(|it| it[fid]), &field_tys[fid].ty(), subst)?;
        }
        CONTINUE_OPAQUELY_INHABITED
    }

    fn visit_field(
        &mut self,
        vis: Option<Visibility>,
        ty: &EarlyBinder<'db, Ty<'db>>,
        subst: GenericArgs<'db>,
    ) -> ControlFlow<VisiblyUninhabited> {
        let visible = |vis: Visibility| {
            self.target_mod.is_none_or(|module| vis.is_visible_from(self.db(), module))
        };
        if vis.is_none_or(visible) {
            let ty = ty.instantiate(self.interner(), subst).skip_norm_wip();
            ty.visit_with(self)
        } else {
            CONTINUE_OPAQUELY_INHABITED
        }
    }
}
