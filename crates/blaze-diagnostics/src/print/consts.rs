//! Constants as rustc prints them in types and paths: parameters by name, `_` for one to be
//! inferred, an anonymous constant by its source, a named one by its path, and values by
//! rustc's `pretty_print_const_valtree`: byte strings, strings, arrays, tuples, ADTs in
//! construction syntax, integers as `ConstInt` prints them (`u8::MAX`), `{0x10 as *const T}`
//! for pointers and `{transmute(..): T}` for the rest. Ported from
//! `compiler/rustc_middle/src/ty/print/pretty.rs` and `compiler/rustc_middle/src/ty/consts/int.rs`
//! (MIT or Apache-2.0).

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

use super::{paths::Def, printer::Printer};

impl<'p, 'a, 'db: 't, 't> Printer<'p, 'a, 'db, 't> {
    /// rustc's `pretty_print_const`, without the type.
    pub(crate) fn print_const(&mut self, c: Const<'t>) {
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
                    let (_, map) = hir_def::expr_store::ExpressionStore::with_source_map(self.db, loc.owner);
                    let src = map.expr_syntax(loc.expr).expect("an anonymous constant is written");
                    let span = self.cx.span(src.map(|it| it.text_range()));
                    let snippet = self.cx.snippet(&span).expect("a span the context made is in its file");
                    self.write_str(snippet);
                }
            },
            ConstKind::Infer(_) => self.write_str("_"),
            ConstKind::Param(param) => {
                let params = GenericParams::of(self.db, param.id.parent());
                match &params[param.id.local_id()] {
                    TypeOrConstParamData::ConstParamData(data) => self.write_str(data.name.symbol().as_str()),
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
            (ValTreeKind::Branch(consts), TyKind::Adt(def, args)) => self.print_adt_value(def.def_id(), args, *consts, ty),
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
            (ValTreeKind::Branch(consts), _) if consts.as_slice().is_empty() => self.write_str("<ZST>"),
            (kind, _) => self.write_fmt(format_args!("{kind:?}")),
        }
    }

    /// An ADT value from rustc's `destructure_const`: an enum's first field is its variant's
    /// index, the variant's fields follow.
    fn print_adt_value(&mut self, adt: AdtId, args: GenericArgs<'t>, consts: Consts<'t>, ty: Ty<'t>) {
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
                        ValTreeKind::Branch(_) => unreachable!("an enum value starts with its variant's index"),
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
                self.out_push(escaped as char);
            }
        }
        self.write_str("\"");
    }

    /// rustc's `pretty_print_const_scalar_int`, without the type.
    fn print_scalar_int(&mut self, int: ScalarInt, ty: Ty<'t>) {
        match ty.kind() {
            TyKind::Bool if let Ok(b) = int.try_to_bool() => self.write_str(if b { "true" } else { "false" }),
            TyKind::Float(fty) => {
                let bits = int.to_bits_unchecked();
                match fty {
                    FloatTy::F16 => float(self, Half::from_bits(bits), "f16"),
                    FloatTy::F32 => float(self, Single::from_bits(bits), "f32"),
                    FloatTy::F64 => float(self, Double::from_bits(bits), "f64"),
                    FloatTy::F128 => float(self, Quad::from_bits(bits), "f128"),
                }
            }
            TyKind::Int(it) => self.write_str(&const_int(int, true, it == IntTy::Isize)),
            TyKind::Uint(it) => self.write_str(&const_int(int, false, it == UintTy::Usize)),
            TyKind::Char if let Ok(c) = char::try_from(int) => self.write_fmt(format_args!("{c:?}")),
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

    fn out_push(&mut self, c: char) {
        let mut buf = [0; 4];
        self.write_str(c.encode_utf8(&mut buf));
    }
}

/// `{value}{suffix}`, `_` between for one that is not finite: rustc's float printing.
fn float<'db: 't, 't, F: rustc_apfloat::Float + std::fmt::Display>(p: &mut Printer<'_, '_, 'db, 't>, value: F, suffix: &str) {
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

/// rustc's `ConstInt` `Debug`, without the suffix: `i8::MIN`, `usize::MAX`, else the number.
fn const_int(int: ScalarInt, signed: bool, ptr_sized: bool) -> String {
    let size = int.size().bytes();
    let raw = int.to_bits_unchecked();
    let bits = size * 8;
    let name = |prefix: char| {
        // alloc: an integer constant printed, the edge
        if ptr_sized { format!("{prefix}size") } else { format!("{prefix}{bits}") }
    };
    if signed {
        let min = 1u128 << (bits - 1);
        let max = min - 1;
        if raw == min {
            return format!("{}::MIN", name('i')); // alloc: an integer constant printed, the edge
        }
        if raw == max {
            return format!("{}::MAX", name('i')); // alloc: an integer constant printed, the edge
        }
        let shift = 128 - bits;
        (((raw << shift) as i128) >> shift).to_string() // alloc: an integer constant printed, the edge
    } else {
        let max = if bits == 128 { u128::MAX } else { (1u128 << bits) - 1 };
        if raw == max {
            return format!("{}::MAX", name('u')); // alloc: an integer constant printed, the edge
        }
        raw.to_string() // alloc: an integer constant printed, the edge
    }
}

#[cfg(test)]
mod tests {
    use hir_ty::next_solver::ScalarInt;
    use rustc_abi::Size;

    use super::const_int;

    #[test]
    fn integers_print_as_const_int_does() {
        let size = |n: u64| Size::from_bytes(n);
        assert_eq!(const_int(ScalarInt::try_from_uint(255u8, size(1)).unwrap(), false, false), "u8::MAX");
        assert_eq!(const_int(ScalarInt::try_from_uint(7u8, size(1)).unwrap(), false, false), "7");
        assert_eq!(const_int(ScalarInt::try_from_int(-128i8, size(1)).unwrap(), true, false), "i8::MIN");
        assert_eq!(const_int(ScalarInt::try_from_int(-3i64, size(8)).unwrap(), true, true), "-3");
        assert_eq!(const_int(ScalarInt::try_from_int(i64::MAX, size(8)).unwrap(), true, true), "isize::MAX");
        assert_eq!(const_int(ScalarInt::try_from_uint(u128::MAX, size(16)).unwrap(), false, false), "u128::MAX");
    }
}
