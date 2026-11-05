//! rustc's `check_param_wf` for const parameters (`rustc_hir_analysis/src/check/wfcheck.rs`):
//! the type of a const parameter must be one a value tree can hold. Without
//! `adt_const_params` (or `min_adt_const_params`) that is an integer, `bool` or `char`; with
//! it, a type implementing the `ConstParamTy` lang item trait, proved in the item's own
//! environment, where an ambiguity is an error too.

use base_db::Db;
use hir_def::{
    ConstParamId, GenericDefId, HasModule, TypeOrConstParamId,
    hir::generics::{GenericParams, TypeOrConstParamData},
    unstable_features::UnstableFeatures,
};
use intern::Symbol;
use rustc_type_ir::{TypeVisitableExt as _, inherent::IntoKind as _};

use crate::{
    db::HirDatabase,
    next_solver::{
        DbInterner, StoredTy, TraitRef, Ty, TyKind, TypingMode,
        infer::{
            DbInternerInferExt,
            traits::{Obligation, ObligationCause},
        },
        obligation_ctxt::ObligationCtxt,
    },
};

/// Why a const parameter's type is refused, which picks rustc's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConstParamTyReason {
    /// Without the feature: a function pointer ("using function pointers as const generic
    /// parameters is forbidden").
    FnPtr,
    /// Without the feature: a raw pointer ("using raw pointers as const generic parameters is
    /// forbidden").
    RawPtr,
    /// Without the feature: any other type but an integer, `bool` or `char` ("`{ty}` is
    /// forbidden as the type of a const generic parameter").
    NotSupported,
    /// With the feature: the type does not implement `ConstParamTy` (E0741).
    NotConstParamTy,
    /// With the feature, in a crate with no `const_param_ty` lang item: rustc's
    /// `require_lang_item` error.
    MissingLangItem,
}

/// A const parameter whose type is refused.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConstParamTyError {
    pub param: ConstParamId,
    pub ty: StoredTy,
    pub reason: ConstParamTyReason,
}

/// The refused const parameters of `def`, its own parameters only, in their order. A type
/// with an error in it is passed over, as rustc passes over a type whose error is reported.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
pub fn const_param_ty_errors(db: &Db, def: GenericDefId) -> Box<[ConstParamTyError]> {
    let params = GenericParams::of(db, def);
    let mut consts = params.iter_type_or_consts().filter_map(|(local_id, data)| match data {
        TypeOrConstParamData::ConstParamData(_) => {
            Some(ConstParamId::from_unchecked(TypeOrConstParamId { parent: def, local_id }))
        }
        TypeOrConstParamData::TypeParamData(_) => None,
    });
    let Some(first) = consts.next() else {
        return Box::default();
    };
    let krate = def.module(db).krate(db);
    let features = UnstableFeatures::query(db, krate);
    let adt_const_params = features.is_enabled(&Symbol::intern("adt_const_params"))
        || features.is_enabled(&Symbol::intern("min_adt_const_params"));
    let types = crate::lower::const_param_types(db, def);
    let interner = DbInterner::new_with(db, krate);
    let mut errors = Vec::new(); // alloc: error path, empty when every type is allowed
    for param in std::iter::once(first).chain(consts) {
        let ty = types.get(param.local_id());
        if ty.references_error() {
            continue;
        }
        let reason = if adt_const_params {
            refused_without_const_param_ty(db, interner, def, ty)
        } else {
            refused_without_the_feature(ty)
        };
        if let Some(reason) = reason {
            errors.push(ConstParamTyError { param, ty: ty.store(), reason });
        }
    }
    errors.into_boxed_slice()
}

fn refused_without_the_feature(ty: Ty<'_>) -> Option<ConstParamTyReason> {
    match ty.kind() {
        TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) | TyKind::Error(_) => None,
        TyKind::FnPtr(..) => Some(ConstParamTyReason::FnPtr),
        TyKind::RawPtr(..) => Some(ConstParamTyReason::RawPtr),
        _ => Some(ConstParamTyReason::NotSupported),
    }
}

fn refused_without_const_param_ty<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    def: GenericDefId,
    ty: Ty<'db>,
) -> Option<ConstParamTyReason> {
    let Some(const_param_ty) = interner.lang_items().ConstParamTy else {
        return Some(ConstParamTyReason::MissingLangItem);
    };
    let param_env = db.trait_environment(def);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let mut ocx = ObligationCtxt::new(&infcx);
    ocx.register_obligation(Obligation::new(
        interner,
        ObligationCause::dummy(),
        param_env,
        TraitRef::new(interner, const_param_ty.into(), [ty]),
    ));
    let holds = ocx.evaluate_obligations_error_on_ambiguity().is_empty();
    (!holds).then_some(ConstParamTyReason::NotConstParamTy)
}
