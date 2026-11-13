//! blaze's error unions (`#![blaze::error_unions]`) once lowered: what the type `A | B` is to
//! the passes that read it (pattern checking, match checking, MIR lowering), in one place.
//! [`crate::lower`]'s `error_union` makes the type; core's `error_union` module is its
//! library (docs/v2/plan/design/error-sets.md).
//!
//! The union of the members `(M0, M1, ..)`, in canonical order, is core's
//! `ErrorUnion<(M0, M1, ..)>`: a struct of one field, `<(M0, M1, ..) as Members>::Repr`, which
//! is core's enum `OfN<M0, M1, ..>`, whose variant `i` holds one field, the member `i`.

use hir_def::{AdtId, EnumVariantId, LocalFieldId, VariantId};
use rustc_type_ir::{TyKind, inherent::IntoKind as _};

use crate::{
    db::HirDatabase as _,
    next_solver::{DbInterner, Ty, Tys},
};

/// The members of `ty`, in canonical order, when it is an error union. `interner` is of a
/// crate (`DbInterner::new_with`): the union's struct is a lang item.
pub fn members<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Option<Tys<'db>> {
    let TyKind::Adt(def, args) = ty.kind() else { return None };
    let union = interner.lang_items().ErrorUnion?;
    if def.def_id() != AdtId::StructId(union) {
        return None;
    }
    match args.types().next()?.kind() {
        TyKind::Tuple(members) => Some(members),
        _ => None,
    }
}

/// Which of `members` a pattern or value of type `ty` is of: the index, in canonical order,
/// of the one member that is `ty`'s struct, enum or union. It is also the index of the
/// variant of the union's `Repr` enum that holds it. `None` when `ty` is no nominal type, no
/// member is of it, or more than one is (`Wrap<A> | Wrap<B>`: the item alone does not say
/// which).
pub fn member_index<'db>(members: Tys<'db>, ty: Ty<'db>) -> Option<usize> {
    let TyKind::Adt(def, _) = ty.kind() else { return None };
    member_index_of(members, def.def_id())
}

/// [`member_index`] of a type whose struct, enum or union is `adt`.
pub fn member_index_of(members: Tys<'_>, adt: AdtId) -> Option<usize> {
    let mut found = None;
    for (at, member) in members.iter().enumerate() {
        let TyKind::Adt(of_member, _) = member.kind() else { continue };
        if of_member.def_id() != adt {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(at);
    }
    found
}

/// The type of the one field of the error union `ty` as core writes it,
/// `<(M0, M1, ..) as Members>::Repr`: a reader normalizes it its own way, to core's enum
/// `OfN<M0, M1, ..>` ([`repr_variant`]).
pub fn repr_field<'db>(
    db: &'db base_db::Db,
    interner: DbInterner<'db>,
    ty: Ty<'db>,
) -> Option<Ty<'db>> {
    let TyKind::Adt(def, args) = ty.kind() else { return None };
    let union = interner.lang_items().ErrorUnion?;
    if def.def_id() != AdtId::StructId(union) {
        return None;
    }
    let field = LocalFieldId::from_raw(0u32.into());
    let fields = db.field_types(VariantId::StructId(union));
    Some(fields.get(field)?.ty().instantiate(interner, args).skip_norm_wip())
}

/// The variant of `repr`, a union's normalized [`repr_field`], that holds the member `at`:
/// core's `OfN` has one variant per member, in the members' order, each of one field.
pub fn repr_variant<'db>(
    db: &'db base_db::Db,
    repr: Ty<'db>,
    at: usize,
) -> Option<EnumVariantId> {
    let TyKind::Adt(def, _) = repr.kind() else { return None };
    let AdtId::EnumId(repr) = def.def_id() else { return None };
    repr.enum_variants(db).variants.get_index(at).map(|(_, &(variant, _))| variant)
}
