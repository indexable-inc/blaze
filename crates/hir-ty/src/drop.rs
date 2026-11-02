//! Utilities for computing drop info about types.

use base_db::Db;
use blaze_mem::Frame;
use hir_def::{AdtId, ImplId, signatures::StructFlags};
use rustc_type_ir::inherent::{AdtDef, GenericArgs as _, IntoKind};

use crate::{
    FrameSet, consteval, frame_set,
    db::HirDatabase,
    layout::{enum_variant_ids, struct_flags},
    method_resolution::{SelfTyKey, TraitImpls},
    next_solver::{
        DbInterner, ParamEnv, SimplifiedType, Ty, TyKind,
        infer::{InferCtxt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
};

#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn destructor(db: &Db, adt: AdtId) -> Option<ImplId> {
    let module = match adt {
        AdtId::EnumId(id) => id.loc(db).container,
        AdtId::StructId(id) => id.loc(db).container,
        AdtId::UnionId(id) => id.loc(db).container,
    };
    let interner = DbInterner::new_with(db, module.krate(db));
    let drop_trait = interner.lang_items().Drop?;
    let self_ty = SelfTyKey(SimplifiedType::Adt(adt.into()));
    let impls = match module.block(db) {
        Some(block) => TraitImpls::in_block_for_trait_and_self_ty(db, block, drop_trait, self_ty),
        None => {
            TraitImpls::in_crate_for_trait_and_self_ty(db, module.krate(db), drop_trait, self_ty)
        }
    };
    impls.first().copied()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DropGlue {
    // Order of variants is important.
    None,
    /// May have a drop glue if some type parameter has it.
    ///
    /// For the compiler this is considered as a positive result, IDE distinguishes this from "yes".
    DependOnParams,
    HasDropGlue,
}

/// Whether no part of a value of `ty` can change behind a shared reference: no `UnsafeCell`,
/// no union, no type parameter. rustc promotes `&expr` only of such a type.
pub fn is_freeze<'db>(infcx: &InferCtxt<'db>, ty: Ty<'db>, env: ParamEnv<'db>) -> bool {
    let mut ocx = ObligationCtxt::new(infcx);
    // A field of an associated type, `Cow<str>`'s `<str as ToOwned>::Owned`, is what it
    // normalizes to; one the solver can't decide stays the alias, which is not `Freeze`.
    let ty = match ocx.structurally_normalize_ty(&ObligationCause::dummy(), env, ty) {
        Ok(normalized) if !matches!(normalized.kind(), TyKind::Infer(_)) => normalized,
        _ => ty,
    };
    let interner = infcx.interner;
    let db = interner.db;
    match ty.kind() {
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Str
        | TyKind::Never
        | TyKind::Ref(..)
        | TyKind::RawPtr(..)
        | TyKind::FnPtr(..)
        | TyKind::FnDef(..) => true,
        TyKind::Array(element, _) | TyKind::Slice(element) | TyKind::Pat(element, _) => {
            is_freeze(infcx, element, env)
        }
        TyKind::Tuple(fields) => fields.iter().all(|field| is_freeze(infcx, field, env)),
        TyKind::Closure(_, args) => is_freeze(infcx, args.as_closure().tupled_upvars_ty(), env),
        TyKind::Adt(def, args) => {
            let fields_freeze = |variant: hir_def::VariantId| {
                db.field_types(variant).iter().all(|(_, field)| {
                    let ty = field.ty().instantiate(interner, args).skip_norm_wip();
                    is_freeze(infcx, ty, env)
                })
            };
            match def.def_id() {
                AdtId::StructId(id) => {
                    Some(id) != interner.lang_items().UnsafeCell && fields_freeze(id.into())
                }
                AdtId::EnumId(id) => {
                    enum_variant_ids(db, id).iter().all(|&variant| fields_freeze(variant.into()))
                }
                AdtId::UnionId(_) => false,
            }
        }
        _ => false,
    }
}

/// [`has_drop_glue_in`] in a task of its own, for a caller with no frame at hand.
pub fn has_drop_glue<'db>(infcx: &InferCtxt<'db>, ty: Ty<'db>, env: ParamEnv<'db>) -> DropGlue {
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| has_drop_glue_in(frame, infcx, ty, env))
}

/// Whether dropping a `ty` runs code, its visited set in the caller's `frame`.
pub fn has_drop_glue_in<'db>(
    frame: &Frame,
    infcx: &InferCtxt<'db>,
    ty: Ty<'db>,
    env: ParamEnv<'db>,
) -> DropGlue {
    has_drop_glue_impl(infcx, ty, env, &mut frame_set(frame))
}

fn has_drop_glue_impl<'db>(
    infcx: &InferCtxt<'db>,
    ty: Ty<'db>,
    env: ParamEnv<'db>,
    visited: &mut FrameSet<'_, Ty<'db>>,
) -> DropGlue {
    let mut ocx = ObligationCtxt::new(infcx);
    // An alias the solver can't decide under `env` comes back as the variable it was to equal,
    // which is no type: it stays the alias, which may have drop glue.
    let ty = match ocx.structurally_normalize_ty(&ObligationCause::dummy(), env, ty) {
        Ok(normalized) if !matches!(normalized.kind(), TyKind::Infer(_)) => normalized,
        _ => ty,
    };

    if !visited.insert(ty) {
        // Recursive type.
        return DropGlue::None;
    }

    let db = infcx.interner.db;
    match ty.kind() {
        TyKind::Adt(adt_def, subst) => {
            let adt_id = adt_def.def_id();
            if adt_def.destructor(infcx.interner).is_some() {
                return DropGlue::HasDropGlue;
            }
            match adt_id {
                AdtId::StructId(id) => {
                    if struct_flags(db, id)
                        .intersects(StructFlags::IS_MANUALLY_DROP | StructFlags::IS_PHANTOM_DATA)
                    {
                        return DropGlue::None;
                    }
                    most_glue(db.field_types(id.into()).iter().map(|(_, field)| {
                        has_drop_glue_impl(
                            infcx,
                            field.ty().instantiate(infcx.interner, subst).skip_norm_wip(),
                            env,
                            visited,
                        )
                    }))
                }
                // Unions cannot have fields with destructors.
                AdtId::UnionId(_) => DropGlue::None,
                AdtId::EnumId(id) => {
                    let mut most = DropGlue::None;
                    for &variant in enum_variant_ids(db, id).iter() {
                        most = most.max(most_glue(db.field_types(variant.into()).iter().map(
                            |(_, field)| {
                                has_drop_glue_impl(
                                    infcx,
                                    field.ty().instantiate(infcx.interner, subst).skip_norm_wip(),
                                    env,
                                    visited,
                                )
                            },
                        )));
                        if most == DropGlue::HasDropGlue {
                            break;
                        }
                    }
                    most
                }
            }
        }
        TyKind::Tuple(tys) => {
            most_glue(tys.iter().map(|ty| has_drop_glue_impl(infcx, ty, env, visited)))
        }
        TyKind::Array(ty, len) => {
            if consteval::try_const_usize(db, len) == Some(0) {
                // Arrays of size 0 don't have drop glue.
                return DropGlue::None;
            }
            has_drop_glue_impl(infcx, ty, env, visited)
        }
        TyKind::Slice(ty) => has_drop_glue_impl(infcx, ty, env, visited),
        TyKind::Closure(_, args) => {
            has_drop_glue_impl(infcx, args.as_closure().tupled_upvars_ty(), env, visited)
        }
        // What is suspended inside it is not visible from here.
        TyKind::Coroutine(..) => DropGlue::HasDropGlue,
        TyKind::CoroutineClosure(_, args) => {
            has_drop_glue_impl(infcx, args.as_coroutine_closure().tupled_upvars_ty(), env, visited)
        }
        // FIXME: Coroutine witness.
        TyKind::CoroutineWitness(..) => DropGlue::None,
        TyKind::Ref(..)
        | TyKind::RawPtr(..)
        | TyKind::FnDef(..)
        | TyKind::Str
        | TyKind::Never
        | TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::FnPtr(..)
        | TyKind::Foreign(_)
        | TyKind::Error(_)
        | TyKind::Bound(..)
        | TyKind::Placeholder(..) => DropGlue::None,
        TyKind::Dynamic(..) => DropGlue::HasDropGlue,
        TyKind::Alias(..) => {
            if infcx.type_is_copy_modulo_regions(env, ty) {
                DropGlue::None
            } else {
                DropGlue::HasDropGlue
            }
        }
        TyKind::Param(_) => {
            if infcx.type_is_copy_modulo_regions(env, ty) {
                DropGlue::None
            } else {
                DropGlue::DependOnParams
            }
        }
        TyKind::Infer(..) => unreachable!("inference vars shouldn't exist out of inference"),
        TyKind::Pat(ty, _) => has_drop_glue_impl(infcx, ty, env, visited),
        TyKind::UnsafeBinder(ty) => has_drop_glue_impl(infcx, ty.skip_binder(), env, visited),
    }
}

/// The most glue of `parts`, which stops at the first part with drop glue: no part after it
/// can have more, so the rest are not walked.
fn most_glue(parts: impl Iterator<Item = DropGlue>) -> DropGlue {
    let mut most = DropGlue::None;
    for glue in parts {
        if glue == DropGlue::HasDropGlue {
            return glue;
        }
        most = most.max(glue);
    }
    most
}

#[cfg(test)]
mod tests {
    use super::{DropGlue, most_glue};

    #[test]
    fn the_most_glue_is_the_max_of_every_part() {
        let all = [DropGlue::None, DropGlue::DependOnParams, DropGlue::HasDropGlue];
        for len in 0..5u32 {
            for mut code in 0..3u32.pow(len) {
                let parts: Vec<DropGlue> = (0..len)
                    .map(|_| {
                        let it = all[(code % 3) as usize];
                        code /= 3;
                        it
                    })
                    .collect();
                let max = parts.iter().copied().max().unwrap_or(DropGlue::None);
                assert_eq!(most_glue(parts.iter().copied()), max, "{parts:?}");
                let walked = std::cell::Cell::new(0);
                most_glue(parts.iter().inspect(|_| walked.set(walked.get() + 1)).copied());
                let stop = parts.iter().position(|&it| it == DropGlue::HasDropGlue);
                assert_eq!(walked.get(), stop.map_or(parts.len(), |at| at + 1), "{parts:?}");
            }
        }
    }
}
