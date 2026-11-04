//! blaze's compile-time reflection (`#![feature(blaze_comptime)]`, docs/v2/plan/design/comptime.md,
//! phase 2) as the trait solver asks it, through `DbInterner`'s `reflect_*` methods: which
//! types are `core::reflect::Struct`, and the members a field binder ranges over.
//!
//! Which items are reflectable is `hir_ty_comptime`'s (the domain, shared with the expansion of
//! `for const`); the types (field representing types, variant representing types, variant
//! views) are built here.

use hir_def::{AdtId, EnumId, StructId, VariantId};
use rustc_type_ir::{
    inherent::{GenericArgs as _, IntoKind},
    reflect::{ReflectContainer, ReflectedDomain},
};

use crate::next_solver::{
    Const, ConstKind, DbInterner, GenericArg, GenericArgs, ScalarInt, Ty, TyKind, Tys, ValTree,
    ValTreeKind,
};

/// A `u32` index argument (`VARIANT`, `FIELD`) as reflection reads it.
enum Index {
    Value(u32),
    /// An inference variable: not known yet.
    Infer,
    /// A parameter, a placeholder or an unevaluated constant: no member is known by it.
    Other,
}

fn index(konst: Const<'_>) -> Index {
    match konst.kind() {
        ConstKind::Value(value) => match value.try_to_leaf() {
            Some(leaf) => Index::Value(leaf.to_u32()),
            None => Index::Other,
        },
        ConstKind::Infer(_) => Index::Infer,
        _ => Index::Other,
    }
}

/// The value of a representing type's `u32` index argument (`VARIANT`, `FIELD`): `None` when
/// it is no value yet (a parameter, a placeholder, an inference variable, an unevaluated
/// constant).
pub fn index_value(konst: Const<'_>) -> Option<u32> {
    match index(konst) {
        Index::Value(it) => Some(it),
        Index::Infer | Index::Other => None,
    }
}

fn u32_const<'db>(interner: DbInterner<'db>, value: u32) -> Const<'db> {
    let leaf = ValTree::new(ValTreeKind::Leaf(ScalarInt::from(value)));
    Const::new_value(interner, leaf, interner.default_types().types.u32)
}

/// `Adt<base, a, b>` for the lang struct `adt` with a type and two `u32` arguments, the shape
/// of `FieldRepresentingType<T, VARIANT, FIELD>`; `b` absent for
/// `VariantRepresentingType<T, VARIANT>` and `VariantOf<T, VARIANT>`.
fn indexed<'db>(
    interner: DbInterner<'db>,
    adt: StructId,
    base: Ty<'db>,
    a: u32,
    b: Option<u32>,
) -> Ty<'db> {
    let a: GenericArg<'db> = u32_const(interner, a).into();
    let args = match b {
        Some(b) => GenericArgs::new_from_slice(&[base.into(), a, u32_const(interner, b).into()]),
        None => GenericArgs::new_from_slice(&[base.into(), a]),
    };
    Ty::new_adt(interner, AdtId::StructId(adt), args)
}

/// The variant `v` of `VariantOf<E, v>` (`view`), when `E` is an enum and `v` one of its variants.
fn variant_of_view<'db>(
    interner: DbInterner<'db>,
    view: Ty<'db>,
) -> Option<Result<(Ty<'db>, EnumId, u32), ()>> {
    let TyKind::Adt(def, args) = view.kind() else {
        return None;
    };
    let AdtId::StructId(id) = def.def_id() else {
        return None;
    };
    if Some(id) != interner.lang_items().ReflectVariantOf {
        return None;
    }
    let base = args.type_at(0);
    Some(match (base.kind(), index(args.const_at(1))) {
        (TyKind::Adt(base_def, _), Index::Value(v)) => match base_def.def_id() {
            AdtId::EnumId(e) if v < hir_ty_comptime::enum_variants(interner.db, e) => {
                Ok((base, e, v))
            }
            _ => Err(()),
        },
        (TyKind::Infer(_), _) | (_, Index::Infer) => Err(()),
        _ => Err(()),
    })
}

/// `Interner::reflect_members`: [`reflect_members_checked`], empty for a container with none.
pub(crate) fn reflect_members<'db>(
    interner: DbInterner<'db>,
    container: Ty<'db>,
    domain: ReflectedDomain,
) -> Tys<'db> {
    reflect_members_checked(interner, container, domain).unwrap_or_else(Tys::empty)
}

/// The members `domain` ranges over in the rigid `container`, in declaration order: for
/// `Fields`, `FieldRepresentingType<C, 0, i>` of a reflectable struct or a tuple `C`, and
/// `FieldRepresentingType<E, v, i>` of a variant view `VariantOf<E, v>`; for `Variants`,
/// `VariantRepresentingType<E, v>` of an enum `E`. `None` when `container` is no reflected
/// container: a struct that is not `Struct` (a private field without `#[blaze::reflect]`,
/// `repr(packed)`), a union, a `VariantOf` of no variant, a non-enum for `Variants`, any other
/// type, or `core` without the lang items. The one list the solver's field binders and the
/// `for const` expansion both use.
pub fn reflect_members_checked<'db>(
    interner: DbInterner<'db>,
    container: Ty<'db>,
    domain: ReflectedDomain,
) -> Option<Tys<'db>> {
    let lang = interner.lang_items();
    let db = interner.db;
    match domain {
        ReflectedDomain::Fields => {
            let frt = lang.FieldRepresentingType?;
            if let Some(view) = variant_of_view(interner, container) {
                let (base, e, v) = view.ok()?;
                let variant = hir_ty_comptime::variant_at(db, e, v)?;
                let count = hir_ty_comptime::variant_fields(db, variant);
                return Some(Tys::new_from_iter(
                    (0..count).map(|i| indexed(interner, frt, base, v, Some(i))),
                ));
            }
            let count = match container.kind() {
                TyKind::Tuple(tys) => tys.len() as u32,
                TyKind::Adt(def, _) => match def.def_id() {
                    AdtId::StructId(id) => hir_ty_comptime::struct_domain(db, id).ok()?,
                    AdtId::UnionId(_) | AdtId::EnumId(_) => return None,
                },
                _ => return None,
            };
            Some(Tys::new_from_iter(
                (0..count).map(|i| indexed(interner, frt, container, 0, Some(i))),
            ))
        }
        ReflectedDomain::Variants => {
            let vrt = lang.ReflectVariantRepresentingType?;
            let TyKind::Adt(def, _) = container.kind() else { return None };
            let AdtId::EnumId(e) = def.def_id() else { return None };
            let count = hir_ty_comptime::enum_variants(db, e);
            Some(Tys::new_from_iter((0..count).map(|v| indexed(interner, vrt, container, v, None))))
        }
    }
}

/// `Interner::reflect_container_of`.
pub(crate) fn reflect_container_of<'db>(
    interner: DbInterner<'db>,
    member: Ty<'db>,
    domain: ReflectedDomain,
) -> ReflectContainer<DbInterner<'db>> {
    let lang = interner.lang_items();
    let db = interner.db;
    let TyKind::Adt(def, args) = member.kind() else {
        return ReflectContainer::Not;
    };
    let AdtId::StructId(id) = def.def_id() else {
        return ReflectContainer::Not;
    };
    let base = args.type_at(0);
    match domain {
        ReflectedDomain::Fields => {
            if Some(id) != lang.FieldRepresentingType {
                return ReflectContainer::Not;
            }
            let (v, i) = match (index(args.const_at(1)), index(args.const_at(2))) {
                (Index::Value(v), Index::Value(i)) => (v, i),
                (Index::Infer, _) | (_, Index::Infer) => return ReflectContainer::Unknown,
                _ => return ReflectContainer::Not,
            };
            let (container, count) = match base.kind() {
                TyKind::Infer(_) => return ReflectContainer::Unknown,
                TyKind::Tuple(tys) if v == 0 => (base, tys.len() as u32),
                TyKind::Adt(base_def, _) => match base_def.def_id() {
                    // `VariantOf`'s own field `value` is no field to reflection.
                    AdtId::StructId(s) if Some(s) == lang.ReflectVariantOf => {
                        return ReflectContainer::Not;
                    }
                    AdtId::StructId(s) if v == 0 => match hir_ty_comptime::struct_domain(db, s) {
                        Ok(count) => (base, count),
                        Err(_) => return ReflectContainer::Not,
                    },
                    AdtId::EnumId(e) => {
                        let (Some(variant_of), Some(variant)) =
                            (lang.ReflectVariantOf, hir_ty_comptime::variant_at(db, e, v))
                        else {
                            return ReflectContainer::Not;
                        };
                        let view = indexed(interner, variant_of, base, v, None);
                        (view, hir_ty_comptime::variant_fields(db, variant))
                    }
                    _ => return ReflectContainer::Not,
                },
                _ => return ReflectContainer::Not,
            };
            if i < count { ReflectContainer::Of(container) } else { ReflectContainer::Not }
        }
        ReflectedDomain::Variants => {
            if Some(id) != lang.ReflectVariantRepresentingType {
                return ReflectContainer::Not;
            }
            match (base.kind(), index(args.const_at(1))) {
                (TyKind::Infer(_), _) | (_, Index::Infer) => ReflectContainer::Unknown,
                (TyKind::Adt(base_def, _), Index::Value(v)) => match base_def.def_id() {
                    AdtId::EnumId(e) if v < hir_ty_comptime::enum_variants(db, e) => {
                        ReflectContainer::Of(base)
                    }
                    _ => ReflectContainer::Not,
                },
                _ => ReflectContainer::Not,
            }
        }
    }
}

/// `Interner::reflect_is_struct`.
pub(crate) fn reflect_is_struct<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Option<bool> {
    if let Some(view) = variant_of_view(interner, ty) {
        return match view {
            Ok(_) => Some(true),
            Err(()) => {
                let TyKind::Adt(_, args) = ty.kind() else {
                    return Some(false);
                };
                let unknown = matches!(args.type_at(0).kind(), TyKind::Infer(_))
                    || matches!(index(args.const_at(1)), Index::Infer);
                if unknown { None } else { Some(false) }
            }
        };
    }
    let TyKind::Adt(def, _) = ty.kind() else {
        return Some(false);
    };
    Some(match def.def_id() {
        AdtId::StructId(id) => hir_ty_comptime::struct_domain(interner.db, id).is_ok(),
        AdtId::UnionId(_) | AdtId::EnumId(_) => false,
    })
}

/// `Interner::reflect_variant_view`.
pub(crate) fn reflect_variant_view<'db>(
    interner: DbInterner<'db>,
    member: Ty<'db>,
) -> Option<Ty<'db>> {
    let ReflectContainer::Of(base) =
        reflect_container_of(interner, member, ReflectedDomain::Variants)
    else {
        return None;
    };
    let TyKind::Adt(_, args) = member.kind() else {
        return None;
    };
    let Index::Value(v) = index(args.const_at(1)) else {
        return None;
    };
    let variant_of = interner.lang_items().ReflectVariantOf?;
    Some(indexed(interner, variant_of, base, v, None))
}

/// What a field binder's member names, for an error that names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReflectedMember {
    /// A field of a struct or of an enum's variant.
    Field(hir_def::FieldId),
    /// The field `.i` of a tuple.
    TupleField(u32),
    /// A variant of an enum.
    Variant(hir_def::EnumVariantId),
}

/// What the member `member` of a field binder names (`FieldRepresentingType<C, v, i>`: a field
/// of `C`'s variant `v`; `VariantRepresentingType<E, v>`: a variant of `E`), with a field's type
/// in its container's arguments. `None` for a type that is no member, or names none.
pub fn reflected_member<'db>(
    interner: DbInterner<'db>,
    member: Ty<'db>,
) -> Option<(ReflectedMember, Option<Ty<'db>>)> {
    let lang = interner.lang_items();
    let db = interner.db;
    let TyKind::Adt(def, args) = member.kind() else {
        return None;
    };
    let AdtId::StructId(id) = def.def_id() else {
        return None;
    };
    let base = args.type_at(0);
    if Some(id) == lang.ReflectVariantRepresentingType {
        let (TyKind::Adt(base_def, _), Index::Value(v)) = (base.kind(), index(args.const_at(1)))
        else {
            return None;
        };
        let AdtId::EnumId(e) = base_def.def_id() else {
            return None;
        };
        return Some((ReflectedMember::Variant(hir_ty_comptime::variant_at(db, e, v)?), None));
    }
    if Some(id) != lang.FieldRepresentingType {
        return None;
    }
    let (Index::Value(v), Index::Value(i)) = (index(args.const_at(1)), index(args.const_at(2)))
    else {
        return None;
    };
    match base.kind() {
        TyKind::Tuple(tys) if v == 0 => {
            let ty = tys.iter().nth(i as usize)?;
            Some((ReflectedMember::TupleField(i), Some(ty)))
        }
        TyKind::Adt(base_def, base_args) => {
            let variant: VariantId = match base_def.def_id() {
                AdtId::StructId(s) if v == 0 => s.into(),
                AdtId::EnumId(e) => hir_ty_comptime::variant_at(db, e, v)?.into(),
                _ => return None,
            };
            let field = hir_ty_comptime::field_at(db, variant, i)?;
            let ty = crate::db::HirDatabase::field_types(db, variant)[field.local_id]
                .ty()
                .instantiate(interner, base_args)
                .skip_norm_wip();
            Some((ReflectedMember::Field(field), Some(ty)))
        }
        _ => None,
    }
}

/// A field binder's variable for lowering: `F` of `for<F: Field<container>>` (`Fields`) or `V`
/// of `for<V: Variant<container>>` (`Variants`), named `kind` (`BoundTyKind::Param` of the
/// binder parameter, for printing). `container` is written inside the binder: it may name the
/// binder's earlier variables at `INNERMOST`, never its own or a later one.
pub fn reflected_var<'db>(
    kind: crate::next_solver::BoundTyKind<'db>,
    domain: ReflectedDomain,
    container: Ty<'db>,
) -> crate::next_solver::BoundVariableKind<'db> {
    crate::next_solver::BoundVariableKind::Reflected(rustc_type_ir::reflect::ReflectedVar {
        kind,
        domain,
        container,
    })
}

/// The clause `for<vars> clause` of a bounded `for<..>` where predicate: `vars` in binder order
/// (reflected ones from [`reflected_var`], then any lifetimes), `clause` written with the
/// binder's variables as `Ty::new_bound(interner, INNERMOST, BoundTy { var, kind })`, the kind
/// the same as its entry in `vars`. `for<F: Field<T>> F::Type: Hash` is
/// `field_binder_clause(interner, &[reflected_var(f, Fields, T)], <^0 as Field>::Type: Hash)`.
///
/// Only `F: Field<C>` and `V: Variant<C>` bind a reflected variable; a filter
/// (`+ Marked<M>`, `+ Unmarked<M>`) or any other bound on a binder parameter has no clause yet
/// and lowering refuses it with a typed error naming the bound.
pub fn field_binder_clause<'db>(
    interner: DbInterner<'db>,
    vars: &[crate::next_solver::BoundVariableKind<'db>],
    clause: crate::next_solver::ClauseKind<'db>,
) -> crate::next_solver::Clause<'db> {
    use rustc_type_ir::Upcast;
    let vars = crate::next_solver::BoundVarKinds::new_from_slice(vars);
    rustc_type_ir::Binder::bind_with_vars(clause, vars).upcast(interner)
}

/// What a loop's binder, or any member `member` of `container` in `domain` entering the
/// environment `env`, assumes of it, elaborated: `member: reflect_trait<container>` and its
/// host effect (`Field` and `Variant` are const traits, so `const { f.index() }` holds), and
/// each field binder of `env` over the same container and domain instantiated with `member`,
/// as the solver's `env_with_reflected_member` enters a member (rule 1): under
/// `for<F: Field<T>> F::Type: Hash`, the body of `for const f in fields_of::<T>()` has
/// `<F as Field>::Type: Hash`.
pub(crate) fn member_assumptions<'db>(
    interner: DbInterner<'db>,
    env: crate::next_solver::ParamEnv<'db>,
    member: Ty<'db>,
    domain: ReflectedDomain,
    container: Ty<'db>,
    reflect_trait: hir_def::TraitId,
) -> crate::next_solver::Clauses<'db> {
    use rustc_type_ir::{BoundConstness, Upcast, reflect::first_reflected_var};

    use crate::next_solver::{Binder, Clause, Clauses, TraitRef};

    let membership = TraitRef::new(interner, reflect_trait.into(), [member, container]);
    let host = Binder::dummy(membership).to_host_effect_clause(interner, BoundConstness::Const);
    let instantiated = env.clauses().iter().filter_map(|clause| {
        let kind = clause.kind();
        let (index, var) = first_reflected_var::<DbInterner<'db>>(kind.bound_vars())?;
        (var.domain == domain && var.container == container).then(|| -> Clause<'db> {
            rustc_type_ir::reflect::instantiate_reflected_var(interner, kind, index, member)
                .upcast(interner)
        })
    });
    let added = [membership.upcast(interner), host].into_iter().chain(instantiated);
    Clauses::new_from_iter(rustc_type_ir::elaborate::elaborate(interner, added))
}

/// The field binder `clause` (`for<F: Field<C>> P(F)`) normalized in `at`'s environment as the
/// solver enters one (rule 2): its variables become placeholders, each reflected one, in
/// binder order, a member of its container (normalized with the ones before it entered, so a
/// dependent `G: Field<F::Type>` matches the environment's binders over `<!F as Field>::Type`)
/// with what [`member_assumptions`] says of it, `P` and the containers are normalized in that
/// environment, and the placeholders are bound again. Normalizing `P` with
/// `F` a bare placeholder asks `<!F as core::field::Field>::Type` of an environment that does
/// not know `!F` is a field, which has no solution. `None` when the clause does not normalize
/// there (a goal fails or stays ambiguous), as an alias of any other clause that does not.
pub(crate) fn normalize_field_binder_clause<'db>(
    infcx: &crate::next_solver::infer::InferCtxt<'db>,
    env: crate::next_solver::ParamEnv<'db>,
    clause: crate::next_solver::Clause<'db>,
) -> Option<crate::next_solver::Clause<'db>> {
    use rustc_next_trait_solver::placeholder::{BoundVarReplacer, PlaceholderReplacer};
    use rustc_type_ir::{TypeVisitableExt, Upcast};

    use crate::next_solver::{
        Binder, BoundVarKinds, BoundVariableKind, Clauses, infer::traits::ObligationCause,
        obligation_ctxt::ObligationCtxt,
    };

    let interner = infcx.interner;
    let kind = clause.kind();
    let vars = kind.bound_vars();
    let containers = Tys::new_from_iter(vars.iter().filter_map(|var| match var {
        BoundVariableKind::Reflected(var) => Some(var.container),
        _ => None,
    }));
    // alloc: one entry, the clause's own binder
    let mut universes = vec![None];
    let ((value, containers), regions, types, consts) = BoundVarReplacer::replace_bound_vars(
        infcx,
        &mut universes,
        (kind.skip_binder(), containers),
    );
    let lang = interner.lang_items();
    let cause = ObligationCause::dummy();
    // Each reflected variable enters in binder order, as the solver enters a binder
    // (`compute_reflected_forall_goal`): its container normalized in the environment with the
    // variables before it entered (`G: Field<F::Type>` ranges over `<!F as Field>::Type`), so
    // the environment's binders over that container are instantiated with it.
    let mut env = env;
    for ((index, var), container) in vars
        .iter()
        .enumerate()
        .filter_map(|(index, var)| match var {
            BoundVariableKind::Reflected(var) => Some((index, var)),
            _ => None,
        })
        .zip(containers.iter())
    {
        // A variable neither `P` nor a later container names is a member nothing is asked of.
        let Some((&placeholder, _)) = types.iter().find(|(_, bound)| bound.var.as_usize() == index)
        else {
            continue;
        };
        let reflect_trait = match var.domain {
            ReflectedDomain::Fields => lang.ReflectField,
            ReflectedDomain::Variants => lang.ReflectVariant,
        }
        .unwrap_or_else(|| {
            // Lowering makes a field binder only when core has both reflection traits.
            panic!(
                "the field binder {clause:?} without the reflection lang item of {:?}",
                var.domain
            )
        });
        let container = if container.has_aliases() {
            let (container, obligations) =
                crate::next_solver::normalize::normalize(infcx.at(&cause, env), container);
            let mut ocx = ObligationCtxt::new(infcx);
            ocx.register_obligations(obligations);
            if !ocx.evaluate_obligations_error_on_ambiguity().is_empty() {
                return None;
            }
            let container = infcx.deeply_resolve_via_unification_table(container);
            if container.has_infer() {
                return None;
            }
            container
        } else {
            container
        };
        let member = Ty::new_placeholder(interner, placeholder);
        let assumptions =
            member_assumptions(interner, env, member, var.domain, container, reflect_trait);
        env = crate::next_solver::ParamEnv::new(Clauses::new_from_iter(
            env.clauses().iter().chain(assumptions.iter()),
        ));
    }
    let (normalized, obligations) =
        crate::next_solver::normalize::normalize(infcx.at(&cause, env), (value, containers));
    let mut ocx = ObligationCtxt::new(infcx);
    ocx.register_obligations(obligations);
    if !ocx.evaluate_obligations_error_on_ambiguity().is_empty() {
        return None;
    }
    let normalized = infcx.deeply_resolve_via_unification_table(normalized);
    if normalized.has_infer() {
        return None;
    }
    let (value, containers) = PlaceholderReplacer::replace_placeholders(
        infcx, regions, types, consts, &universes, normalized,
    );
    let mut containers = containers.iter();
    let vars = BoundVarKinds::new_from_iter(vars.iter().map(|var| match var {
        BoundVariableKind::Reflected(var) => {
            BoundVariableKind::Reflected(rustc_type_ir::reflect::ReflectedVar {
                container: containers.next().expect("one container per reflected variable"),
                ..var
            })
        }
        var => var,
    }));
    Some(Binder::bind_with_vars(value, vars).upcast(interner))
}
