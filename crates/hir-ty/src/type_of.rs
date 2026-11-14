//! `type_of!` (docs/v2/plan/design/type-of.md): the type a `core::reflect::TypeDesc` value
//! describes, as the trait solver asks it through `DbInterner`'s `reflect_described_desc` and
//! `reflect_type_of`: `<Described<'a, D, Args> as TypeOf>::Type`.
//!
//! The description is read from its value tree, so the type depends on the description's
//! value and on `Args`, nothing else: two equal descriptions are one type in every crate. What
//! is built is an ordinary type (a record is core's `Record` over a tuple, an `Option` is
//! core's), so layout, traits, display and identity see nothing new.
//!
//! A `TypeDesc` value tree, as rustc's `branches` makes it for an enum: a branch of the
//! variant's index as a `u32` leaf, then a tree per field; a reference is the tree of what it
//! points to; a slice is a branch of its elements; a `str` a branch of its bytes.

use std::alloc::Global;

use hir_def::{AdtId, StructId, VariantId};
use rustc_type_ir::{
    Interner as _,
    inherent::{GenericArgs as _, IntoKind as _, Ty as _},
    reflect::TypeOfAnswer,
};

use crate::{
    db::HirDatabase as _,
    next_solver::{
        Const, ConstKind, Consts, DbInterner, GenericArg, GenericArgs, Region, Ty, TyKind,
        ValTreeKind,
    },
};

/// How deep a description may nest before it is refused: the read recurses once per level,
/// and the value tree bounds its nodes but not its depth below that.
pub const MAX_DEPTH: u32 = 256;

/// Why a description gives no type yet, or none at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unread {
    /// An inference variable, a parameter or an unevaluated constant decides it.
    Unknown,
    /// It names no type.
    Refused,
}

/// `ty` as core's `Described<'a, D, Args>`: its arguments, lifetime first.
fn described<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Option<GenericArgs<'db>> {
    let TyKind::Adt(def, args) = ty.kind() else { return None };
    let AdtId::StructId(id) = def.def_id() else { return None };
    (Some(id) == interner.lang_items().ReflectDescribed).then_some(args)
}

/// `D` of `Described<'a, D, Args>`, for the solver to normalize. `None` for any other type.
pub(crate) fn described_desc<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Option<Const<'db>> {
    described(interner, ty).map(|args| args.const_at(1))
}

/// `<ty as TypeOf>::Type` of `ty`, `Described<'a, D, Args>`, with `desc` its `D` normalized.
pub(crate) fn type_of<'db>(
    interner: DbInterner<'db>,
    ty: Ty<'db>,
    desc: Const<'db>,
) -> TypeOfAnswer<DbInterner<'db>> {
    let Some(args) = described(interner, ty) else { return TypeOfAnswer::Refused };
    match desc.kind() {
        ConstKind::Value(_) => {}
        // The constant's own error (a panic of the const code that computed it, a SQL error)
        // is reported where it is evaluated; the type is the error type, not a second error.
        ConstKind::Error(_) => return TypeOfAnswer::Type(interner.default_types().types.error),
        _ => return TypeOfAnswer::Unknown,
    }
    let reader = Reader { interner, region: args.region_at(0), args: args.type_at(2) };
    match reader.desc(desc, Place::Sized, 0) {
        Ok(ty) => TypeOfAnswer::Type(ty),
        Err(Unread::Unknown) => TypeOfAnswer::Unknown,
        Err(Unread::Refused) => TypeOfAnswer::Refused,
    }
}

/// Where a described type is: an unsized one (`str`, a slice) only directly under a `Ref`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Sized,
    UnderRef,
}

struct Reader<'db> {
    interner: DbInterner<'db>,
    /// `'a` of `Described`, every `Ref`'s.
    region: Region<'db>,
    /// `Args` of `Described`, the tuple `Arg(i)` indexes.
    args: Ty<'db>,
}

impl<'db> Reader<'db> {
    /// The type the `TypeDesc` value `konst` describes.
    fn desc(&self, konst: Const<'db>, place: Place, depth: u32) -> Result<Ty<'db>, Unread> {
        if depth >= MAX_DEPTH {
            return Err(Unread::Refused);
        }
        let depth = depth + 1;
        let types = &self.interner.default_types().types;
        let (tag, parts) = variant(konst)?;
        let ty = match (tag, &parts.as_slice()[1..]) {
            (0, &[prim]) => {
                let (prim, _) = variant(prim)?;
                match prim {
                    0 => types.bool,
                    1 => types.char,
                    2 => types.i8,
                    3 => types.i16,
                    4 => types.i32,
                    5 => types.i64,
                    6 => types.i128,
                    7 => types.isize,
                    8 => types.u8,
                    9 => types.u16,
                    10 => types.u32,
                    11 => types.u64,
                    12 => types.u128,
                    13 => types.usize,
                    14 => types.f32,
                    15 => types.f64,
                    16 if place == Place::UnderRef => types.str,
                    _ => return Err(Unread::Refused),
                }
            }
            (1, &[index]) => {
                let index = leaf(index)?;
                match self.args.kind() {
                    TyKind::Tuple(tys) => tys
                        .as_slice()
                        .get(usize::try_from(index).map_err(|_| Unread::Refused)?)
                        .copied()
                        .ok_or(Unread::Refused)?,
                    _ => return Err(unknown_or_refused(self.args)),
                }
            }
            (2, &[of, index]) => {
                let of = self.desc(of, Place::Sized, depth)?;
                self.field_of(of, leaf(index)?)?
            }
            (3, &[of, index]) => {
                let of = self.desc(of, Place::Sized, depth)?;
                let TyKind::Adt(_, args) = of.kind() else { return Err(unknown_or_refused(of)) };
                let index = usize::try_from(leaf(index)?).map_err(|_| Unread::Refused)?;
                args.iter().filter_map(|arg| arg.ty()).nth(index).ok_or(Unread::Refused)?
            }
            (4, &[inner]) => {
                let inner = self.desc(inner, Place::Sized, depth)?;
                let option = self.interner.lang_items().Option.ok_or(Unread::Refused)?;
                let args = GenericArgs::new_from_slice(&[GenericArg::from(inner)]);
                Ty::new_adt(self.interner, AdtId::EnumId(option), args)
            }
            (5, &[inner]) => {
                let inner = self.desc(inner, Place::UnderRef, depth)?;
                Ty::new_imm_ref(self.interner, self.region, inner)
            }
            (6, &[elem]) if place == Place::UnderRef => {
                Ty::new_slice(self.interner, self.desc(elem, Place::Sized, depth)?)
            }
            (7, &[elems]) => {
                let elems = branch(elems)?;
                let mut tys = Vec::with_capacity_in(elems.as_slice().len(), Global);
                for &elem in elems.as_slice() {
                    tys.push(self.desc(elem, Place::Sized, depth)?);
                }
                Ty::new_tup(self.interner, &tys)
            }
            (8, &[fields]) => self.record(fields, depth)?,
            _ => return Err(Unread::Refused),
        };
        Ok(ty)
    }

    /// The type of field `index` (declaration order) of the struct or tuple `of`. A struct's
    /// fields are read only when reflection may read them (`core::reflect::Struct`), so a
    /// description sees no more of a type than `for const` does.
    fn field_of(&self, of: Ty<'db>, index: u128) -> Result<Ty<'db>, Unread> {
        let index = usize::try_from(index).map_err(|_| Unread::Refused)?;
        match of.kind() {
            TyKind::Tuple(tys) => tys.as_slice().get(index).copied().ok_or(Unread::Refused),
            TyKind::Adt(def, args) => {
                let AdtId::StructId(id) = def.def_id() else { return Err(Unread::Refused) };
                match self.interner.reflect_is_struct(of) {
                    Some(true) => {}
                    Some(false) => return Err(Unread::Refused),
                    None => return Err(Unread::Unknown),
                }
                self.struct_field(id, args, index)
            }
            _ => Err(unknown_or_refused(of)),
        }
    }

    fn struct_field(
        &self,
        id: StructId,
        args: GenericArgs<'db>,
        index: usize,
    ) -> Result<Ty<'db>, Unread> {
        let fields = self.interner.db.field_types(VariantId::StructId(id));
        let field = fields.values().nth(index).ok_or(Unread::Refused)?;
        Ok(field.ty().instantiate(self.interner, args).skip_norm_wip())
    }

    /// `Record(&[FieldDesc])`: core's `Record` of the names sorted by their bytes over the
    /// tuple of their types in that order. A name that is empty or has the separator, and two
    /// fields of one name, are refused, as a record type written out is.
    fn record(&self, fields: Const<'db>, depth: u32) -> Result<Ty<'db>, Unread> {
        let fields = branch(fields)?;
        let count = fields.as_slice().len();
        let mut names = Vec::with_capacity_in(count, Global);
        let mut tys = Vec::with_capacity_in(count, Global);
        for &field in fields.as_slice() {
            let parts = branch(field)?;
            let &[name, ty] = parts.as_slice() else { return Err(Unread::Refused) };
            let name = bytes(name)?;
            let valid = std::str::from_utf8(&name).is_ok_and(|it| {
                !it.is_empty() && !it.contains(hir_def::record::SEPARATOR)
            });
            if !valid {
                return Err(Unread::Refused);
            }
            names.push(name);
            tys.push(self.desc(ty, Place::Sized, depth)?);
        }
        let mut written = Vec::with_capacity_in(count, Global);
        for name in &names {
            written.push(std::str::from_utf8(name).map_err(|_| Unread::Refused)?);
        }
        let order = hir_def::record::canonical_order(&written).map_err(|_| Unread::Refused)?;
        let list = hir_def::record::joined(order.iter().map(|&at| written[at]));
        let mut canonical = Vec::with_capacity_in(count, Global);
        canonical.extend(order.iter().map(|&at| tys[at]));
        let record = self.interner.lang_items().Record.ok_or(Unread::Refused)?;
        Ok(crate::record::record_ty(self.interner, record, &list, &canonical))
    }
}

/// A type a description cannot look into yet (`Unknown`) or ever (`Refused`).
fn unknown_or_refused(ty: Ty<'_>) -> Unread {
    match ty.kind() {
        TyKind::Infer(_)
        | TyKind::Param(_)
        | TyKind::Placeholder(_)
        | TyKind::Bound(..)
        | TyKind::Alias(..) => Unread::Unknown,
        _ => Unread::Refused,
    }
}

/// The value tree of `konst` as a branch.
fn branch<'db>(konst: Const<'db>) -> Result<Consts<'db>, Unread> {
    let ConstKind::Value(value) = konst.kind() else { return Err(Unread::Unknown) };
    match value.value.inner() {
        ValTreeKind::Branch(consts) => Ok(*consts),
        ValTreeKind::Leaf(_) => Err(Unread::Refused),
    }
}

/// The value tree of `konst` as a leaf's bits.
fn leaf(konst: Const<'_>) -> Result<u128, Unread> {
    let ConstKind::Value(value) = konst.kind() else { return Err(Unread::Unknown) };
    match value.value.inner() {
        ValTreeKind::Leaf(leaf) => Ok(leaf.to_bits_unchecked()),
        ValTreeKind::Branch(_) => Err(Unread::Refused),
    }
}

/// An enum's value tree: its variant's index, and the whole branch, whose trees after the
/// first are its fields'.
fn variant<'db>(konst: Const<'db>) -> Result<(u128, Consts<'db>), Unread> {
    let parts = branch(konst)?;
    let &tag = parts.as_slice().first().ok_or(Unread::Refused)?;
    Ok((leaf(tag)?, parts))
}

/// A `str`'s value tree, a branch of its bytes.
fn bytes(konst: Const<'_>) -> Result<Vec<u8, Global>, Unread> {
    let elems = branch(konst)?;
    let mut bytes = Vec::with_capacity_in(elems.as_slice().len(), Global);
    for &byte in elems.as_slice() {
        bytes.push(u8::try_from(leaf(byte)?).map_err(|_| Unread::Refused)?);
    }
    Ok(bytes)
}
