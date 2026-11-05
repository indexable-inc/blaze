//! Constants as rustc prints them in types and paths: parameters by name, `_` for one to be
//! inferred, an anonymous constant by its source, a named one by its path, and values by
//! rustc's `pretty_print_const_valtree`: byte strings, strings, arrays, tuples, ADTs in
//! construction syntax, integers as `ConstInt` prints them (`u8::MAX`), `{0x10 as *const T}`
//! for pointers and `{transmute(..): T}` for the rest. `type_name` prints a `bool`, `char` or
//! integer value so, and every other spelling a constant as the number it evaluates to.
//! Ported from `compiler/rustc_middle/src/ty/print/pretty.rs` and
//! `compiler/rustc_middle/src/ty/consts/int.rs` (MIT or Apache-2.0).

use hir_def::{
    AdtId, VariantId,
    hir::generics::{GenericParams, TypeOrConstParamData},
    item_tree::FieldsShape,
};
use hir_ty::next_solver::{
    AliasConstKind, AnonConstIdWrapper, Const, ConstKind, Consts, FreeConstAliasId, GenericArgs,
    InherentAssocConstId, ScalarInt, TraitAssocConstId, Ty, TyKind, ValTreeKind, ValueConst,
};
use rustc_apfloat::{
    Float as _,
    ieee::{Double, Half, Quad, Single},
};
use rustc_type_ir::{FloatTy, IntTy, UintTy, inherent::IntoKind as _};

use crate::{
    def::Def,
    printer::{NOT_LOOKED_UP, Printer, Spelling},
};

impl<'h, 't> Printer<'h, 't> {
    /// rustc's `pretty_print_const`, without the type.
    pub fn print_const(&mut self, c: Const<'t>) {
        if self.refused.is_some() {
            return;
        }
        if self.spelling() != Spelling::Diagnostic {
            return self.print_spelled_const(c);
        }
        match c.kind() {
            ConstKind::Alias(_, alias) => match alias.kind {
                AliasConstKind::Projection { def_id: TraitAssocConstId(it) }
                | AliasConstKind::InherentSelf { def_id: InherentAssocConstId(it) }
                | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(it) }
                | AliasConstKind::Free { def_id: FreeConstAliasId(it) } => {
                    self.print_value_path(Def::Const(it), alias.args.as_slice())
                }
                AliasConstKind::Anon { def_id: AnonConstIdWrapper(it) } => {
                    let loc = it.loc(self.db);
                    self.host().anon_const_text(loc.owner, loc.expr, &mut *self.out);
                }
            },
            ConstKind::Infer(_) => self.write_str("_"),
            ConstKind::Param(param) => {
                let params = GenericParams::of(self.db, param.id.parent());
                match &params[param.id.local_id()] {
                    TypeOrConstParamData::ConstParamData(data) => {
                        self.write_str(data.name.symbol().as_str())
                    }
                    TypeOrConstParamData::TypeParamData(_) => {
                        unreachable!("a const parameter's data is a const parameter's")
                    }
                }
            }
            ConstKind::Value(value) => self.print_valtree(value),
            ConstKind::Bound(index, bound) => {
                let mut out = String::new(); // alloc: a bound constant printed, the edge
                // Writing into a `String` does not fail.
                let _ = rustc_type_ir::debug_bound_var(&mut out, index, bound.var);
                self.write_str(&out);
            }
            ConstKind::Placeholder(it) => self.write_fmt(format_args!("{it:?}")),
            ConstKind::Expr(_) => unreachable!("hir-ty lowers no constant expressions"),
            ConstKind::Error(_) => self.write_str("{const error}"),
        }
    }

    /// A constant argument outside a diagnostic: to `type_name` a `bool`, `char` or integer
    /// value as a diagnostic prints it (`true`, `'a'`, `i8::MIN`), else the number the
    /// constant evaluates to.
    fn print_spelled_const(&mut self, c: Const<'t>) {
        if self.spelling() == Spelling::TypeName
            && let ConstKind::Value(value) = c.kind()
            && let ValTreeKind::Leaf(leaf) = value.value.inner()
            && matches!(
                value.ty.kind(),
                TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_)
            )
        {
            return self.print_scalar_int(*leaf, value.ty);
        }
        match hir_ty::consteval::try_const_usize(self.db, c) {
            Some(value) => self.write_fmt(format_args!("{value}")),
            None => self.refuse(NOT_LOOKED_UP),
        }
    }

    /// rustc's `pretty_print_const_valtree`.
    fn print_valtree(&mut self, value: ValueConst<'t>) {
        let ty = value.ty;
        match (value.value.inner(), ty.kind()) {
            (ValTreeKind::Branch(consts), TyKind::Ref(_, inner, _)) => match inner.kind() {
                TyKind::Slice(elem) if matches!(elem.kind(), TyKind::Uint(UintTy::U8)) => {
                    self.print_byte_str(*consts)
                }
                TyKind::Str => {
                    // alloc: a string constant printed, the edge
                    let bytes: Vec<u8> = bytes(*consts).collect();
                    // alloc: none when the bytes are UTF-8; the constant printed, the edge
                    self.write_fmt(format_args!("{:?}", String::from_utf8_lossy(&bytes)));
                }
                _ => {
                    self.write_str("&");
                    self.print_valtree(ValueConst { ty: inner, value: value.value });
                }
            },
            (ValTreeKind::Branch(consts), TyKind::Array(elem, _))
                if matches!(elem.kind(), TyKind::Uint(UintTy::U8)) =>
            {
                self.write_str("*");
                self.print_byte_str(*consts);
            }
            (ValTreeKind::Branch(consts), TyKind::Array(..)) => {
                self.write_str("[");
                self.comma_sep_consts(consts.as_slice());
                self.write_str("]");
            }
            (ValTreeKind::Branch(consts), TyKind::Tuple(_)) => {
                self.write_str("(");
                self.comma_sep_consts(consts.as_slice());
                if consts.as_slice().len() == 1 {
                    self.write_str(",");
                }
                self.write_str(")");
            }
            (ValTreeKind::Branch(consts), TyKind::Adt(def, args)) => {
                self.print_adt_value(def.def_id(), args, *consts, ty)
            }
            (ValTreeKind::Leaf(leaf), TyKind::Ref(_, inner, _)) => {
                self.write_str("&");
                self.print_scalar_int(*leaf, inner);
            }
            (ValTreeKind::Leaf(leaf), _) => self.print_scalar_int(*leaf, ty),
            (_, TyKind::FnDef(def, args)) => {
                let def = match def.0 {
                    hir_def::CallableDefId::FunctionId(it) => Def::Function(it),
                    hir_def::CallableDefId::StructId(it) => Def::Adt(AdtId::StructId(it)),
                    hir_def::CallableDefId::EnumVariantId(it) => Def::Variant(it),
                };
                self.print_value_path(def, args.skip_binder().as_slice());
            }
            (ValTreeKind::Branch(consts), _) if consts.as_slice().is_empty() => {
                self.write_str("<ZST>")
            }
            (kind, _) => self.write_fmt(format_args!("{kind:?}")),
        }
    }

    /// An ADT value from rustc's `destructure_const`: an enum's first field is its variant's
    /// index, the variant's fields follow.
    fn print_adt_value(
        &mut self,
        adt: AdtId,
        args: GenericArgs<'t>,
        consts: Consts<'t>,
        ty: Ty<'t>,
    ) {
        let consts = consts.as_slice();
        let (variant, def, fields): (VariantId, Def, &[Const<'t>]) = match adt {
            AdtId::StructId(it) => (it.into(), Def::Adt(adt), consts),
            AdtId::UnionId(it) => (it.into(), Def::Adt(adt), consts),
            AdtId::EnumId(it) => {
                let variants = &it.enum_variants(self.db).variants;
                if variants.is_empty() {
                    self.write_str("{unreachable(): ");
                    let was = std::mem::replace(&mut self.in_value, false);
                    self.print_type(ty);
                    self.in_value = was;
                    return self.write_str("}");
                }
                let index = match consts.first().map(|it| it.kind()) {
                    Some(ConstKind::Value(v)) => match v.value.inner() {
                        ValTreeKind::Leaf(leaf) => leaf.to_u32() as usize,
                        ValTreeKind::Branch(_) => {
                            unreachable!("an enum value starts with its variant's index")
                        }
                    },
                    _ => unreachable!("an enum value starts with its variant's index"),
                };
                let variant = variants[index].0;
                (variant.into(), Def::Variant(variant), &consts[1..])
            }
        };
        self.print_value_path(def, args.as_slice());
        let data = variant.fields(self.db);
        match data.shape {
            FieldsShape::Unit => {}
            FieldsShape::Tuple => {
                self.write_str("(");
                self.comma_sep_consts(fields);
                self.write_str(")");
            }
            FieldsShape::Record => {
                self.write_str(" { ");
                for (i, ((_, field), value)) in data.fields().iter().zip(fields).enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.write_str(field.name.symbol().as_str());
                    self.write_str(": ");
                    self.print_const(*value);
                }
                self.write_str(" }");
            }
        }
    }

    fn comma_sep_consts(&mut self, consts: &[Const<'t>]) {
        for (i, c) in consts.iter().enumerate() {
            if i > 0 {
                self.write_str(", ");
            }
            self.print_const(*c);
        }
    }

    /// rustc's `pretty_print_byte_str`: `b"..."` with `ascii::escape_default`.
    fn print_byte_str(&mut self, consts: Consts<'t>) {
        self.write_str("b\"");
        for byte in bytes(consts) {
            for escaped in std::ascii::escape_default(byte) {
                self.out.push(escaped as char);
            }
        }
        self.write_str("\"");
    }

    /// rustc's `pretty_print_const_scalar_int`, without the type.
    fn print_scalar_int(&mut self, int: ScalarInt, ty: Ty<'t>) {
        match ty.kind() {
            TyKind::Bool if let Ok(b) = int.try_to_bool() => {
                self.write_str(if b { "true" } else { "false" })
            }
            TyKind::Float(fty) => {
                let bits = int.to_bits_unchecked();
                match fty {
                    FloatTy::F16 => float(self, Half::from_bits(bits), "f16"),
                    FloatTy::F32 => float(self, Single::from_bits(bits), "f32"),
                    FloatTy::F64 => float(self, Double::from_bits(bits), "f64"),
                    FloatTy::F128 => float(self, Quad::from_bits(bits), "f128"),
                }
            }
            TyKind::Int(it) => self.write_int(int, Int::Signed(it)),
            TyKind::Uint(it) => self.write_int(int, Int::Unsigned(it)),
            TyKind::Char if let Ok(c) = char::try_from(int) => {
                self.write_fmt(format_args!("{c:?}"))
            }
            TyKind::Ref(..) | TyKind::RawPtr(..) | TyKind::FnPtr(..) => {
                self.write_fmt(format_args!("{{0x{:x} as ", int.to_bits_unchecked()));
                let was = std::mem::replace(&mut self.in_value, false);
                self.print_type(ty);
                self.in_value = was;
                self.write_str("}");
            }
            _ => {
                if int.size().bytes() == 0 {
                    self.write_str("{transmute(()): ");
                } else {
                    self.write_fmt(format_args!("{{transmute(0x{int:x}): "));
                }
                let was = std::mem::replace(&mut self.in_value, false);
                self.print_type(ty);
                self.in_value = was;
                self.write_str("}");
            }
        }
    }

    fn write_int(&mut self, int: ScalarInt, ty: Int) {
        match const_int(int, ty) {
            ConstInt::Min(name) => self.write_fmt(format_args!("{name}::MIN")),
            ConstInt::Max(name) => self.write_fmt(format_args!("{name}::MAX")),
            ConstInt::Signed(value) => self.write_fmt(format_args!("{value}")),
            ConstInt::Unsigned(value) => self.write_fmt(format_args!("{value}")),
        }
    }
}

/// `{value}{suffix}`, `_` between for one that is not finite: rustc's float printing.
fn float<F: rustc_apfloat::Float + std::fmt::Display>(
    p: &mut Printer<'_, '_>,
    value: F,
    suffix: &str,
) {
    let sep = if value.is_finite() { "" } else { "_" };
    p.write_fmt(format_args!("{value}{sep}{suffix}"));
}

/// The bytes of a branch of `u8` leaves.
fn bytes(consts: Consts<'_>) -> impl Iterator<Item = u8> + '_ {
    consts.as_slice().iter().map(|c| match c.kind() {
        ConstKind::Value(v) => match v.value.inner() {
            ValTreeKind::Leaf(leaf) => leaf.to_u8(),
            ValTreeKind::Branch(_) => unreachable!("a byte string's bytes are leaves"),
        },
        _ => unreachable!("a byte string's bytes are values"),
    })
}

/// An integer type, signed or not.
#[derive(Clone, Copy)]
enum Int {
    Signed(IntTy),
    Unsigned(UintTy),
}

/// What rustc's `ConstInt` `Debug` prints, without the suffix.
#[derive(Debug, PartialEq, Eq)]
enum ConstInt {
    Min(&'static str),
    Max(&'static str),
    Signed(i128),
    Unsigned(u128),
}

/// rustc's `ConstInt` `Debug`: `i8::MIN`, `usize::MAX`, else the number. An arbitrary width's
/// value is sign-extended to its storage: `i3::MIN` is -4.
fn const_int(int: ScalarInt, ty: Int) -> ConstInt {
    let storage = int.size().bits();
    match ty {
        Int::Signed(it) => {
            let unused = 128 - it.arbitrary_width().map_or(storage, |it| u64::from(it.bits()));
            match int.to_int_unchecked() {
                v if v == i128::MIN >> unused => ConstInt::Min(it.name_str()),
                v if v == i128::MAX >> unused => ConstInt::Max(it.name_str()),
                v => ConstInt::Signed(v),
            }
        }
        Int::Unsigned(it) => {
            let unused = 128 - it.arbitrary_width().map_or(storage, |it| u64::from(it.bits()));
            match int.to_uint_unchecked() {
                v if v == u128::MAX >> unused => ConstInt::Max(it.name_str()),
                v => ConstInt::Unsigned(v),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hir_ty::next_solver::ScalarInt;
    use rustc_abi::Size;
    use rustc_type_ir::{IntTy, UintTy};

    use super::{ConstInt, Int, const_int};

    #[test]
    fn integers_print_as_const_int_does() {
        let size = |n: u64| Size::from_bytes(n);
        let u = |v: u128, n: u64, ty: UintTy| {
            const_int(ScalarInt::try_from_uint(v, size(n)).unwrap(), Int::Unsigned(ty))
        };
        let i = |v: i128, n: u64, ty: IntTy| {
            const_int(ScalarInt::try_from_int(v, size(n)).unwrap(), Int::Signed(ty))
        };
        assert_eq!(u(255, 1, UintTy::U8), ConstInt::Max("u8"));
        assert_eq!(u(7, 1, UintTy::U8), ConstInt::Unsigned(7));
        assert_eq!(i(-128, 1, IntTy::I8), ConstInt::Min("i8"));
        assert_eq!(i(-3, 8, IntTy::Isize), ConstInt::Signed(-3));
        assert_eq!(i(i64::MAX.into(), 8, IntTy::Isize), ConstInt::Max("isize"));
        assert_eq!(u(u128::MAX, 16, UintTy::U128), ConstInt::Max("u128"));
    }
}
