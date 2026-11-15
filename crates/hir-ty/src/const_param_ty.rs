//! rustc's `check_param_wf` for const parameters (`rustc_hir_analysis/src/check/wfcheck.rs`):
//! the type of a const parameter must be one a value tree can hold. Without
//! `adt_const_params` (or `min_adt_const_params`) that is an integer, `bool` or `char`; with
//! it, a type implementing the `ConstParamTy` lang item trait, proved in the item's own
//! environment, where an ambiguity is an error too.
//!
//! A reference, `str` or a slice (`&'static str`, `&'static [u8]`) also needs
//! `unsized_const_params`: core's `ConstParamTy_` impls for `str`, `[T]` and `&T` are
//! `#[unstable_feature_bound(unsized_const_params)]`, which hir-ty does not lower into an
//! impl's predicates, so the bound is checked here, on the parameter's type, where rustc's
//! solver would refuse the impl. And whatever the features, the type may not name another
//! generic parameter (E0770): `const S: &'a str` is refused, not lowered to an error region
//! no one reports.

use std::alloc::Global;

use base_db::Db;
use hir_def::{
    ConstParamId, GenericDefId, HasModule, TypeOrConstParamId,
    expr_store::ExpressionStore,
    hir::generics::{GenericParams, TypeOrConstParamData},
    type_ref::{LifetimeRef, TypeRef, TypeRefId},
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
    /// With `adt_const_params` but not `unsized_const_params`: a reference, `str` or a slice
    /// (`&'static str`), whose `ConstParamTy_` impls core bounds on `unsized_const_params`.
    UnsizedConstParams,
    /// The type names another generic parameter, a lifetime (`&'a str`) or a type: E0770,
    /// "the type of const parameters must not depend on other generic parameters".
    DependsOnGenerics,
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
    let unsized_const_params = features.is_enabled(&Symbol::intern("unsized_const_params"));
    let types = crate::lower::const_param_types(db, def);
    let (_, store) = GenericParams::with_store(db, def);
    let interner = DbInterner::new_with(db, krate);
    let mut errors = Vec::new_in(Global);
    for param in std::iter::once(first).chain(consts) {
        let ty = types.get(param.local_id());
        // Lowering gives a parameter named here the error type or region and reports
        // nothing, so this is asked of what is written, before the error type is passed over.
        if let TypeOrConstParamData::ConstParamData(data) = &params[param.local_id()]
            && names_a_generic_param(params, store, data.ty)
        {
            errors.push(ConstParamTyError {
                param,
                ty: ty.store(),
                reason: ConstParamTyReason::DependsOnGenerics,
            });
            continue;
        }
        if ty.references_error() {
            continue;
        }
        let reason = if adt_const_params {
            match !unsized_const_params && needs_unsized_const_params(ty) {
                true => Some(ConstParamTyReason::UnsizedConstParams),
                false => refused_without_const_param_ty(db, interner, def, ty),
            }
        } else {
            refused_without_the_feature(ty)
        };
        if let Some(reason) = reason {
            errors.push(ConstParamTyError { param, ty: ty.store(), reason });
        }
    }
    errors.into_boxed_slice()
}

/// Whether the written type `ty` names a generic parameter of `params`: one of its lifetimes
/// in a reference (hir-def keeps a lifetime by its name; one no parameter declares is E0261's,
/// not this) or a type parameter, through references, pointers, arrays, slices and tuples.
fn names_a_generic_param(params: &GenericParams, store: &ExpressionStore, ty: TypeRefId) -> bool {
    let declared = |name: &hir_expand::name::Name| params.iter_lt().any(|(_, it)| it.name == *name);
    match store.type_ref(ty) {
        TypeRef::TypeParam(_) => true,
        TypeRef::Reference(it) => {
            it.lifetime.is_some_and(|it| match &store[it] {
                LifetimeRef::Param(_) => true,
                LifetimeRef::Named(name) => declared(name),
                LifetimeRef::Static | LifetimeRef::Placeholder | LifetimeRef::Error => false,
            }) || names_a_generic_param(params, store, it.ty)
        }
        TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => names_a_generic_param(params, store, it),
        TypeRef::Array(it) => names_a_generic_param(params, store, it.ty),
        TypeRef::Tuple(it) => it.iter().any(|&it| names_a_generic_param(params, store, it)),
        _ => false,
    }
}

/// Whether proving `ty: ConstParamTy_` would go through one of core's impls bounded on
/// `unsized_const_params`: a reference, `str` or a slice, at the top or inside an array or a
/// tuple. An ADT's own fields are its `ConstParamTy` impl's to check, not this.
fn needs_unsized_const_params(ty: Ty<'_>) -> bool {
    match ty.kind() {
        TyKind::Ref(..) | TyKind::Str | TyKind::Slice(_) => true,
        TyKind::Array(elem, _) => needs_unsized_const_params(elem),
        TyKind::Tuple(tys) => tys.iter().any(needs_unsized_const_params),
        _ => false,
    }
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
