//! The domain of `#![feature(blaze_comptime)]` reflection (docs/v2/plan/design/comptime.md,
//! phase 2, "Which fields: visibility"): which types are `Struct` and `Enum`, and how many
//! fields and variants they range over, in declaration order. The solver's builtin candidates
//! (`Struct`, `Enum`, `reflect::Field<B>`, `Variant<B>`, answered through `hir_ty`'s interner)
//! and the expansion of `for const` in monomorphization both read it here, so the two cannot
//! disagree.
//!
//! Items only, no types: a tuple's fields, the field types and the field representing types
//! are `hir_ty`'s. Declaration order is the arena order of a variant's fields and the index
//! order of an enum's variants, never layout or hash order (deterministic-output).

#![forbid(unsafe_code)]

use base_db::{Crate, Db};
use hir_def::{
    EnumId, EnumVariantId, FieldId, StructId, TraitId, VariantId, attrs::AttrFlags,
    item_tree::RawVisibility, lang_item::lang_items, signatures::StructSignature,
    signatures::VariantFields,
};

/// The lang items of `core::reflect` (`library/core/src/reflect.rs`) reflection reads: the
/// traits the solver answers, `Struct`, `Enum`, `Field<Base>`, `Variant<Base>` and `FnItem`,
/// and the types its members and views are, `VariantRepresentingType<E, v>` (a variant) and
/// `VariantOf<E, v>` (its view); a field is nightly's `FieldRepresentingType<T, v, i>`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ReflectItems {
    pub struct_: Option<TraitId>,
    pub enum_: Option<TraitId>,
    pub field: Option<TraitId>,
    pub variant: Option<TraitId>,
    pub fn_item: Option<TraitId>,
    pub variant_representing_type: Option<StructId>,
    pub variant_of: Option<StructId>,
    pub field_representing_type: Option<StructId>,
}

/// The reflection lang items `krate` sees, found as every lang item is, through
/// [`lang_items`] of `krate` and its dependencies. Only blaze's `core` declares them, so a
/// crate graph whose `core` is not blaze's answers none, and nothing is a reflection trait.
pub fn reflect_items(db: &Db, krate: Crate) -> ReflectItems {
    let lang = lang_items(db, krate);
    ReflectItems {
        struct_: lang.ReflectStruct,
        enum_: lang.ReflectEnum,
        field: lang.ReflectField,
        variant: lang.ReflectVariant,
        fn_item: lang.ReflectFnItem,
        variant_representing_type: lang.ReflectVariantRepresentingType,
        variant_of: lang.ReflectVariantOf,
        field_representing_type: lang.FieldRepresentingType,
    }
}

/// Why a struct is not `core::reflect::Struct`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NotStruct {
    /// `repr(packed)`: a borrow of `x.[f]` could be unaligned, which rustc refuses for a named
    /// field too.
    Packed,
    /// The first field, in declaration order, that is not `pub`, of a struct not written
    /// `#[blaze::reflect]`.
    PrivateField(FieldId),
}

/// Whether the struct `id` is `core::reflect::Struct`, and with how many fields: every field
/// `pub`, or the struct written `#[blaze::reflect]` (the author's consent, as a derive's), and
/// not `repr(packed)`. A union is never one (no field of it can be read safely); a tuple always
/// is, and an enum's variant (`VariantOf<E, v>`) always is, its fields being as visible as the
/// enum: those are `hir_ty`'s, which has their types.
pub fn struct_domain(db: &Db, id: StructId) -> Result<u32, NotStruct> {
    if StructSignature::of(db, id).repr(db, id).is_some_and(|repr| repr.packed()) {
        return Err(NotStruct::Packed);
    }
    let fields = VariantFields::of(db, id.into()).fields();
    if !AttrFlags::query(db, id.into()).contains(AttrFlags::BLAZE_REFLECT)
        && let Some((local_id, _)) =
            fields.iter().find(|(_, field)| !matches!(field.visibility, RawVisibility::Public))
    {
        return Err(NotStruct::PrivateField(FieldId { parent: id.into(), local_id }));
    }
    Ok(count(fields.len()))
}

/// How many variants the enum `id` has: what `for<V: Variant<E>>` ranges over. Every enum is
/// `core::reflect::Enum`.
pub fn enum_variants(db: &Db, id: EnumId) -> u32 {
    count(id.enum_variants(db).variants.len())
}

/// The variant of `id` at `index`, in declaration order.
pub fn variant_at(db: &Db, id: EnumId, index: u32) -> Option<EnumVariantId> {
    id.enum_variants(db).variants.get_index(index as usize).map(|(_, &(variant, _))| variant)
}

/// How many fields the enum variant `id` has: what `for<F: Field<VariantOf<E, v>>>` ranges
/// over.
pub fn variant_fields(db: &Db, id: EnumVariantId) -> u32 {
    count(VariantFields::of(db, id.into()).fields().len())
}

/// The field of `variant` at `index`, in declaration order: the field a failing field-binder
/// bound names.
pub fn field_at(db: &Db, variant: VariantId, index: u32) -> Option<FieldId> {
    VariantFields::of(db, variant)
        .fields()
        .iter()
        .nth(index as usize)
        .map(|(local_id, _)| FieldId { parent: variant, local_id })
}

/// A count of fields or variants, which the field representing type's `u32` indices bound.
fn count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or_else(|_| panic!("{len} fields or variants overflow a `u32` index"))
}
