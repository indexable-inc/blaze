//! `#[blaze::bits]`: bit-packed structs, a blaze extension (docs/v2/plan/design/bits.md).
//!
//! A bits struct's fields sit at bit offsets of one backing integer, the first at bit 0, in
//! declaration order, each as wide as its type says ([`bit_width_of`]). The struct is laid out
//! as that integer: its size, alignment and ABI are the integer's, and it has no field offsets
//! (`FieldsShape::Primitive`). MIR keeps its fields as places until borrow checking is done;
//! `hir_ty_mir`'s `transform::bits` then turns every read into a shift and a mask and every
//! write into a read-modify-write of the backing integer. A struct without the attribute never
//! reaches this file.

use std::fmt::Write as _;

use base_db::{Crate, Db};
use hir_def::{
    AdtId, EnumId, EnumVariantId, GenericDefId, HasModule, LocalFieldId, StructId, VariantId,
    attrs::{AttrFlags, BitsBacking},
    hir::generics::GenericParams,
    signatures::{EnumSignature, StructFlags, StructSignature},
};
use intern::Symbol;
use rustc_abi::{Integer, IntegerType};
use rustc_type_ir::{IntTy, UintTy, inherent::IntoKind};

use crate::{
    ParamEnvAndCrate,
    consteval::discriminants::enum_discriminants,
    db::HirDatabase,
    display::{DisplayTarget, HirDisplay},
    layout::{Normalizer, enum_variant_ids, struct_flags, variant_field_ids},
    next_solver::{ParamEnv, StoredTy, Ty, TyKind},
    traits::StoredParamEnvAndCrate,
};

/// How many bits a value of a type takes as a field of a `#[blaze::bits]` struct, and how the
/// field's bits read back as the value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BitWidth {
    pub bits: u32,
    /// The bits are two's complement and a read sign-extends them (`iN`, an enum with a
    /// negative discriminant).
    pub signed: bool,
}

/// [`bit_width_of`]'s refusal: what the type is and why it has no bit width.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum BitWidthError {
    /// Neither `bool`, a fixed-width integer, a fieldless enum nor a `#[blaze::bits]` struct.
    NotPackable { ty: StoredTy },
    /// `usize` or `isize`, or an enum of `#[repr(usize)]` or `#[repr(isize)]`: its width is
    /// the target's, and a packed layout that changes with the target is refused.
    TargetWidth { ty: StoredTy },
    /// An enum with a variant that has fields.
    EnumWithFields { ty: StoredTy, variant: EnumVariantId },
    /// An enum with no variants: no value of it can be stored.
    EmptyEnum { ty: StoredTy },
    /// The discriminant of `variant` did not evaluate.
    Discriminant { ty: StoredTy, variant: EnumVariantId },
    /// The type's aliases do not normalize.
    Normalization { ty: StoredTy },
    /// A `#[blaze::bits]` struct that is refused itself.
    Nested { ty: StoredTy, error: Box<BitsError> },
}

/// One field of a `#[blaze::bits]` struct: where its bits are and what they hold.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BitsField {
    pub id: LocalFieldId,
    /// The bit its lowest bit is at: the sum of the widths of the fields before it.
    pub offset: u32,
    pub width: BitWidth,
    pub ty: StoredTy,
}

/// The layout of a `#[blaze::bits]` struct ([`bits_layout`]).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct BitsLayout {
    /// The unsigned integer the fields are packed into: the struct's size, alignment and ABI.
    pub backing: Integer,
    /// The bits the fields take, at most the backing integer's; the bits above are padding,
    /// zero in a struct literal and kept by a field write.
    pub used: u32,
    /// Every field, in declaration order.
    pub fields: Box<[BitsField]>,
}

impl BitsField {
    /// This field's value out of the backing integer's value `backing`, as a value of the
    /// field's type is stored (little-endian, low bytes first): its bits, sign-extended to
    /// 128 where the field is signed. What a constant's field reads as, for a pattern or a
    /// rendered value.
    pub fn read(&self, backing: u128) -> u128 {
        let width = self.width.bits;
        let raw = backing.checked_shr(self.offset).unwrap_or(0);
        let raw = if width >= 128 { raw } else { raw & ((1u128 << width) - 1) };
        let negative = self.width.signed && width > 0 && (raw >> (width - 1)) & 1 == 1;
        if negative && width < 128 { raw | (!0u128 << width) } else { raw }
    }
}

impl BitsLayout {
    /// The field `id`, by its position.
    pub fn field(&self, id: LocalFieldId) -> Option<&BitsField> {
        self.fields.iter().find(|it| it.id == id)
    }

    /// The backing integer's width in bits.
    pub fn backing_bits(&self) -> u32 {
        self.backing.size().bits() as u32
    }
}

/// Why a `#[blaze::bits]` struct is refused ([`bits_layout`]): each names the struct, and the
/// field and its type where there is one ([`BitsError::message`]).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum BitsError {
    /// A field's type has no bit width.
    Field { strukt: StructId, field: LocalFieldId, error: BitWidthError },
    /// The fields take more bits than the explicit backing integer has.
    DoesNotFit { strukt: StructId, bits: u32, backing: Integer },
    /// The fields take more than 128 bits, the widest backing integer.
    TooWide { strukt: StructId, bits: u32 },
    /// `#[blaze::bits(..)]` names no backing integer: the text it gives.
    MalformedBacking { strukt: StructId, text: Symbol },
    /// The struct has generic parameters: its layout would change with them.
    Generic { strukt: StructId },
    /// The struct has a `#[repr]`, which says another layout.
    Repr { strukt: StructId },
    /// A field's type runs a destructor: it would be dropped in place, through a reference
    /// to the field, which no field of a bits struct has.
    FieldNeedsDrop { strukt: StructId, field: LocalFieldId, ty: StoredTy },
    /// The struct holds itself, through its fields' types.
    Cycle { strukt: StructId },
}

/// Whether `id` is a `#[blaze::bits]` struct: one bit test of its flags.
pub fn is_bits_struct(db: &Db, id: StructId) -> bool {
    struct_flags(db, id).contains(StructFlags::IS_BLAZE_BITS)
}

/// The `#[blaze::bits]` struct `ty` is, if it is one.
pub fn bits_struct_of(db: &Db, ty: Ty<'_>) -> Option<StructId> {
    match ty.kind() {
        TyKind::Adt(def, _) => match def.def_id() {
            AdtId::StructId(id) if is_bits_struct(db, id) => Some(id),
            _ => None,
        },
        _ => None,
    }
}

/// The layout of the `#[blaze::bits]` struct `id`, or why it is refused.
pub fn bits_layout(db: &Db, id: StructId) -> Result<&BitsLayout, &BitsError> {
    bits_layout_query(db, id).as_ref()
}

#[blaze_query::tracked(cycle_result = bits_layout_cycle_result, returns(ref), mem_tag = Infer)]
fn bits_layout_query(db: &Db, id: StructId) -> Result<BitsLayout, BitsError> {
    let strukt = id;
    let explicit = match AttrFlags::blaze_bits(db, id.into()) {
        Some(BitsBacking::Smallest) | None => None,
        Some(BitsBacking::Explicit(integer)) => Some(integer),
        Some(BitsBacking::Malformed(text)) => {
            return Err(BitsError::MalformedBacking { strukt, text });
        }
    };
    if !GenericParams::of(db, GenericDefId::AdtId(id.into())).is_empty() {
        return Err(BitsError::Generic { strukt });
    }
    if AttrFlags::repr(db, id.into()).is_some() {
        return Err(BitsError::Repr { strukt });
    }
    let krate = AdtId::from(id).krate(db);
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store();
    // The struct has no generics, so every field type normalizes to a concrete type: the one
    // the drop check below and every reader of `BitsField::ty` see.
    let normalizer = Normalizer::new(db, krate, ParamEnv::empty());
    let field_types = db.field_types(id.into());
    let ids = variant_field_ids(db, id.into());
    let mut fields = Vec::with_capacity(ids.len()); // alloc: the query's value, kept in its memo
    let mut offset = 0u32;
    for &field in ids.iter() {
        let declared = field_types[field].ty().instantiate_identity().skip_norm_wip();
        let ty = normalizer.normalize(declared).map_err(|_| BitsError::Field {
            strukt,
            field,
            error: BitWidthError::Normalization { ty: declared.store() },
        })?;
        // clone: the env is two `u32` handles
        let width = bit_width_of(db, ty.store(), env.clone())
            .map_err(|error| BitsError::Field { strukt, field, error })?;
        // A fieldless enum or a nested bits struct can still `impl Drop`.
        if let TyKind::Adt(def, _) = ty.kind()
            && crate::drop::destructor(db, def.def_id()).is_some()
        {
            return Err(BitsError::FieldNeedsDrop { strukt, field, ty: ty.store() });
        }
        fields.push(BitsField { id: field, offset, width, ty: ty.store() });
        // At most 129 fields of up to 128 bits each before `TooWide` stops it: no overflow.
        offset = offset.saturating_add(width.bits);
        if offset > 128 {
            return Err(BitsError::TooWide { strukt, bits: offset });
        }
    }
    let used = offset;
    let backing = match explicit {
        Some(backing) if used > backing.size().bits() as u32 => {
            return Err(BitsError::DoesNotFit { strukt, bits: used, backing });
        }
        Some(backing) => backing,
        None => smallest_backing(used),
    };
    Ok(BitsLayout { backing, used, fields: fields.into_boxed_slice() })
}

fn bits_layout_cycle_result(
    _: &Db,
    _: blaze_query::db::Id,
    id: StructId,
) -> Result<BitsLayout, BitsError> {
    Err(BitsError::Cycle { strukt: id })
}

/// The least of `u8`, `u16`, `u32`, `u64` and `u128` with `bits` bits, for `bits <= 128`.
fn smallest_backing(bits: u32) -> Integer {
    match bits {
        0..=8 => Integer::I8,
        9..=16 => Integer::I16,
        17..=32 => Integer::I32,
        33..=64 => Integer::I64,
        _ => Integer::I128,
    }
}

/// How many bits a value of `ty` takes as a field of a `#[blaze::bits]` struct, and whether
/// they read back sign-extended. One arm per kind of type:
///
/// - `bool`: 1, unsigned.
/// - `u8..u128`, `i8..i128`: their width, signed for `iN`. `usize` and `isize` are refused
///   ([`BitWidthError::TargetWidth`]).
/// - an enum with no fields: an integer `#[repr]`'s width (Zig's `enum(u8)`), else the least
///   bits that hold every discriminant, so the default discriminants `0..n` take
///   `⌈log2 n⌉`; signed when one is negative.
/// - a `#[blaze::bits]` struct: the bits its fields take, not its padded backing integer.
///
/// Integers of the other widths (`u1..u127`, `i1..i127`, lane `uint`'s `UintTy::Bits` and
/// `IntTy::Bits`) answer their own width; a standard integer is used whole. Either way the
/// field's type holds a write to its range, so no write can spill into a neighbour.
#[blaze_query::tracked(cycle_result = bit_width_of_cycle_result, returns(clone), mem_tag = Infer)]
pub fn bit_width_of(
    db: &Db,
    ty: StoredTy,
    env: StoredParamEnvAndCrate,
) -> Result<BitWidth, BitWidthError> {
    let normalizer = Normalizer::new(db, env.krate, env.param_env(db));
    let ty = normalizer.normalize(ty.as_ref()).map_err(|_| BitWidthError::Normalization { ty })?;
    let stored = ty.store();
    let int = |bits: u32, signed: bool| Ok(BitWidth { bits, signed });
    match ty.kind() {
        TyKind::Bool => int(1, false),
        TyKind::Int(IntTy::Isize) | TyKind::Uint(UintTy::Usize) => {
            Err(BitWidthError::TargetWidth { ty: stored })
        }
        TyKind::Int(IntTy::I8) => int(8, true),
        TyKind::Int(IntTy::I16) => int(16, true),
        TyKind::Int(IntTy::I32) => int(32, true),
        TyKind::Int(IntTy::I64) => int(64, true),
        TyKind::Int(IntTy::I128) => int(128, true),
        TyKind::Uint(UintTy::U8) => int(8, false),
        TyKind::Uint(UintTy::U16) => int(16, false),
        TyKind::Uint(UintTy::U32) => int(32, false),
        TyKind::Uint(UintTy::U64) => int(64, false),
        TyKind::Uint(UintTy::U128) => int(128, false),
        // Lane `uint`'s `i1..i127` and `u1..u127` (`#![blaze::arbitrary_ints]`): their own
        // width, so a field of type `u3` takes 3 bits and its type keeps every write in range.
        TyKind::Int(IntTy::Bits(width)) => int(width.bits(), true),
        TyKind::Uint(UintTy::Bits(width)) => int(width.bits(), false),
        TyKind::Adt(def, _) => match def.def_id() {
            AdtId::StructId(id) if is_bits_struct(db, id) => match bits_layout(db, id) {
                Ok(layout) => int(layout.used, false),
                Err(error) => {
                    Err(BitWidthError::Nested { ty: stored, error: Box::new(error.clone()) }) // alloc: an error, once per refused field
                }
            },
            AdtId::EnumId(id) => enum_width(db, id, stored),
            AdtId::StructId(_) | AdtId::UnionId(_) => {
                Err(BitWidthError::NotPackable { ty: stored })
            }
        },
        _ => Err(BitWidthError::NotPackable { ty: stored }),
    }
}

fn bit_width_of_cycle_result(
    _: &Db,
    _: blaze_query::db::Id,
    ty: StoredTy,
    _: StoredParamEnvAndCrate,
) -> Result<BitWidth, BitWidthError> {
    Err(BitWidthError::NotPackable { ty })
}

/// [`bit_width_of`] of a fieldless enum.
fn enum_width(db: &Db, id: EnumId, ty: StoredTy) -> Result<BitWidth, BitWidthError> {
    let variants = enum_variant_ids(db, id);
    if variants.is_empty() {
        return Err(BitWidthError::EmptyEnum { ty });
    }
    if let Some(&variant) = variants.iter().find(|&&v| !variant_field_ids(db, v.into()).is_empty())
    {
        return Err(BitWidthError::EnumWithFields { ty, variant });
    }
    match AttrFlags::repr(db, id.into()).and_then(|it| it.int) {
        Some(IntegerType::Fixed(integer, signed)) => {
            return Ok(BitWidth { bits: integer.size().bits() as u32, signed });
        }
        Some(IntegerType::Pointer(_)) => return Err(BitWidthError::TargetWidth { ty }),
        None => {}
    }
    let discriminants = enum_discriminants(db, id);
    let mut values = Vec::with_capacity(variants.len()); // alloc: one enum's discriminants, freed on return
    for (index, value) in discriminants.iter().enumerate() {
        match value {
            Ok(value) => values.push(value),
            Err(_) => return Err(BitWidthError::Discriminant { ty, variant: variants[index] }),
        }
    }
    Ok(discriminant_width(&values))
}

/// The least bits that hold every one of `values`: unsigned when none is negative, else two's
/// complement.
fn discriminant_width(values: &[i128]) -> BitWidth {
    let signed = values.iter().any(|&it| it < 0);
    let bits = values
        .iter()
        .map(|&value| {
            if signed {
                // `!value` is `-value - 1`: the magnitude bits of a negative value.
                let magnitude = if value < 0 { !value } else { value } as u128;
                128 - magnitude.leading_zeros() + 1
            } else {
                128 - (value as u128).leading_zeros()
            }
        })
        .max()
        .unwrap_or(0);
    BitWidth { bits, signed }
}

impl BitsError {
    /// The struct the error is about.
    pub fn strukt(&self) -> StructId {
        match *self {
            BitsError::Field { strukt, .. }
            | BitsError::DoesNotFit { strukt, .. }
            | BitsError::TooWide { strukt, .. }
            | BitsError::MalformedBacking { strukt, .. }
            | BitsError::Generic { strukt }
            | BitsError::Repr { strukt }
            | BitsError::FieldNeedsDrop { strukt, .. }
            | BitsError::Cycle { strukt } => strukt,
        }
    }

    /// The field the error is about, if one.
    pub fn field(&self) -> Option<LocalFieldId> {
        match *self {
            BitsError::Field { field, .. } | BitsError::FieldNeedsDrop { field, .. } => Some(field),
            _ => None,
        }
    }

    /// The error as one sentence that names the struct, and the field and its type where
    /// there is one. Reads the database, so it is made while one is attached.
    pub fn message(&self, db: &Db, target: DisplayTarget) -> String {
        let mut out = String::new(); // alloc: the message, returned
        self.write(db, target, &mut out);
        out
    }

    fn write(&self, db: &Db, target: DisplayTarget, out: &mut String) {
        let strukt = self.strukt();
        let name = StructSignature::of(db, strukt).name.display(db, target.edition).to_string(); // alloc: the message's text
        let field = |field: LocalFieldId| {
            VariantId::from(strukt).fields(db).fields()[field]
                .name
                .display(db, target.edition)
                .to_string() // alloc: the message's text
        };
        let ty = |ty: StoredTy| ty.as_ref().display(db, target).to_string(); // alloc: the message's text
        let _ = match self {
            BitsError::Field { field: id, error, .. } => {
                let _ = write!(out, "field `{}` of `#[blaze::bits]` struct `{name}`: ", field(*id));
                error.write(db, target, out);
                Ok(())
            }
            BitsError::DoesNotFit { bits, backing, .. } => write!(
                out,
                "the fields of `#[blaze::bits]` struct `{name}` take {bits} bits, more than \
                 its backing integer `u{}` has",
                backing.size().bits()
            ),
            BitsError::TooWide { bits, .. } => write!(
                out,
                "the fields of `#[blaze::bits]` struct `{name}` take {bits} bits or more, \
                 more than `u128`, the widest backing integer, has"
            ),
            BitsError::MalformedBacking { text, .. } => write!(
                out,
                "`#[blaze::bits{}]` on struct `{name}` names no backing integer: write \
                 `#[blaze::bits]` or one of `u8`, `u16`, `u32`, `u64`, `u128` in it",
                text.as_str()
            ),
            BitsError::Generic { .. } => write!(
                out,
                "`#[blaze::bits]` struct `{name}` has generic parameters: a bit-packed \
                 layout is fixed, so its fields' types are too"
            ),
            BitsError::Repr { .. } => write!(
                out,
                "`#[blaze::bits]` struct `{name}` has a `#[repr]`: its layout is its backing \
                 integer's, so no `#[repr]` applies"
            ),
            BitsError::FieldNeedsDrop { field: id, ty: field_ty, .. } => write!(
                out,
                "field `{}` of `#[blaze::bits]` struct `{name}` has type `{}`, which \
                 implements `Drop`: a field of a bits struct has no address to drop it at",
                field(*id),
                ty(*field_ty)
            ),
            BitsError::Cycle { .. } => {
                write!(out, "`#[blaze::bits]` struct `{name}` holds itself through its fields")
            }
        };
    }
}

impl BitWidthError {
    /// The error as one sentence that names the type. Reads the database.
    pub fn message(&self, db: &Db, target: DisplayTarget) -> String {
        let mut out = String::new(); // alloc: the message, returned
        self.write(db, target, &mut out);
        out
    }

    fn write(&self, db: &Db, target: DisplayTarget, out: &mut String) {
        let ty = |ty: &StoredTy| ty.as_ref().display(db, target).to_string(); // alloc: the message's text
        let variant = |variant: EnumVariantId| {
            let loc = variant.loc(db);
            format!(
                "{}::{}",
                EnumSignature::of(db, loc.parent).name.display(db, target.edition),
                loc.name.display(db, target.edition)
            ) // alloc: the message's text
        };
        let _ = match self {
            BitWidthError::NotPackable { ty: t } => write!(
                out,
                "type `{}` cannot be bit-packed: a field of a bits struct is a `bool`, a \
                 fixed-width integer, an enum without fields or a `#[blaze::bits]` struct",
                ty(t)
            ),
            BitWidthError::TargetWidth { ty: t } => write!(
                out,
                "type `{}` is as wide as the target's pointers, and a bit-packed layout may not \
                 change with the target: use a fixed-width integer",
                ty(t)
            ),
            BitWidthError::EnumWithFields { ty: t, variant: v } => write!(
                out,
                "enum `{}` cannot be bit-packed: its variant `{}` has fields",
                ty(t),
                variant(*v)
            ),
            BitWidthError::EmptyEnum { ty: t } => {
                write!(out, "enum `{}` has no variants, so no value of it can be stored", ty(t))
            }
            BitWidthError::Discriminant { ty: t, variant: v } => write!(
                out,
                "the discriminant of `{}` in enum `{}` did not evaluate",
                variant(*v),
                ty(t)
            ),
            BitWidthError::Normalization { ty: t } => {
                write!(out, "type `{}` does not normalize", ty(t))
            }
            BitWidthError::Nested { error, .. } => {
                error.write(db, target, out);
                Ok(())
            }
        };
    }
}

/// The crate a bits struct's errors are displayed for.
pub fn bits_display_target(db: &Db, id: StructId) -> DisplayTarget {
    let krate: Crate = AdtId::from(id).krate(db);
    DisplayTarget::from_crate(db, krate)
}

#[cfg(test)]
mod tests {
    use super::{BitWidth, discriminant_width};

    #[test]
    fn default_discriminants_take_log2_of_the_variant_count() {
        let width = |n: i128| discriminant_width(&(0..n).collect::<Vec<_>>()).bits; // alloc: test
        assert_eq!([1, 2, 3, 4, 5, 8, 9, 256, 257].map(width), [0, 1, 2, 2, 3, 3, 4, 8, 9]);
    }

    #[test]
    fn a_negative_discriminant_makes_the_width_signed() {
        assert_eq!(discriminant_width(&[-1, 0]), BitWidth { bits: 1, signed: true });
        assert_eq!(discriminant_width(&[-2, 1]), BitWidth { bits: 2, signed: true });
        assert_eq!(discriminant_width(&[-1, 2]), BitWidth { bits: 3, signed: true });
        assert_eq!(discriminant_width(&[-128, 127]), BitWidth { bits: 8, signed: true });
    }
}
