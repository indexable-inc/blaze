//! `print_witness_pat` (`rustc_pattern_analysis/src/rustc.rs` and `rustc/print.rs`): a witness
//! as rustc writes it in E0004 and E0005, with constants as rustc's `ConstInt` prints them.

use std::fmt::Write;

use hir_def::{
    AdtId, Lookup, VariantId,
    item_tree::FieldsShape,
    signatures::{EnumSignature, StructSignature, UnionSignature},
};
use rustc_pattern_analysis::constructor::{
    Constructor, IntRange, MaybeInfiniteInt, RangeEnd, SliceKind,
};
use rustc_type_ir::{Mutability, inherent::IntoKind};

use hir_ty::next_solver::{Ty, TyKind};

use crate::cx::{Cx, IntInfo, Witness};

/// A bound of a range as rustc's `PatRangeBoundary` holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bound {
    NegInfinity,
    /// The value's bits, truncated to its type's size.
    Finite(u128),
    PosInfinity,
}

/// `hoist_pat_range_bdy`: `isize/usize::MAX + 1`, which no value of the type is, is
/// `PosInfinity`.
pub(crate) fn hoist(info: IntInfo, x: MaybeInfiniteInt) -> Bound {
    match x {
        MaybeInfiniteInt::NegInfinity => Bound::NegInfinity,
        MaybeInfiniteInt::PosInfinity => Bound::PosInfinity,
        MaybeInfiniteInt::Finite { .. } => {
            let bits = if info.signed {
                x.as_finite_int(info.bits).unwrap_or(0)
            } else {
                x.as_finite_uint().unwrap_or(0)
            };
            if info.bits < 128 && bits >> info.bits != 0 {
                Bound::PosInfinity
            } else {
                Bound::Finite(bits)
            }
        }
    }
}

fn int_name(ty: Ty<'_>) -> &'static str {
    match ty.kind() {
        TyKind::Int(int) => int.name_str(),
        TyKind::Uint(uint) => uint.name_str(),
        _ => "",
    }
}

/// A value of an integer or `char` type as rustc's pretty printer writes it with its type:
/// `ConstInt`'s alternate `Debug` (`i32::MIN`, `u8::MAX`, `-1_i32`, `0_usize`), a `char`'s
/// `Debug`.
pub(crate) fn write_scalar(out: &mut String, info: IntInfo, ty: Ty<'_>, bits: u128) {
    if let TyKind::Char = ty.kind() {
        match char::from_u32(bits as u32) {
            Some(c) => _ = write!(out, "{c:?}"),
            None => _ = write!(out, "{bits}"),
        }
        return;
    }
    let name = int_name(ty);
    let size = info.bits;
    if info.signed {
        let min = 1u128 << (size - 1);
        let max = min - 1;
        if bits == min {
            _ = write!(out, "{name}::MIN");
        } else if bits == max {
            _ = write!(out, "{name}::MAX");
        } else {
            let shift = 128 - size as u32;
            let value = ((bits << shift) as i128) >> shift;
            _ = write!(out, "{value}_{name}");
        }
    } else {
        let max = u128::MAX >> (128 - size as u32);
        if bits == max {
            _ = write!(out, "{name}::MAX");
        } else {
            _ = write!(out, "{bits}_{name}");
        }
    }
}

fn would_print_as_wildcard(p: &Witness<'_, '_>, never_patterns: bool) -> bool {
    match p.ctor() {
        Constructor::IntRange(IntRange {
            lo: MaybeInfiniteInt::NegInfinity,
            hi: MaybeInfiniteInt::PosInfinity,
        })
        | Constructor::Wildcard
        | Constructor::NonExhaustive
        | Constructor::Hidden
        | Constructor::PrivateUninhabited => true,
        Constructor::Never => !never_patterns,
        _ => false,
    }
}

impl<'a, 'db> Cx<'a, 'db> {
    /// `print_pat_range`.
    pub(crate) fn print_range(&self, out: &mut String, range: &IntRange, ty: Ty<'db>) {
        let Some(info) = self.int_info(ty) else { return };
        if matches!((range.lo, range.hi), (MaybeInfiniteInt::NegInfinity, MaybeInfiniteInt::PosInfinity))
        {
            out.push('_');
            return;
        }
        if range.is_singleton() {
            if let Bound::Finite(bits) = hoist(info, range.lo) {
                write_scalar(out, info, ty, bits);
            }
            return;
        }
        let mut end = RangeEnd::Included;
        let mut lo = hoist(info, range.lo);
        if lo == Bound::PosInfinity {
            // `{u,i}size::MAX + 1`: the values after the type's max, shown as `usize::MAX..`.
            let max = if info.signed {
                (1u128 << (info.bits - 1)) - 1
            } else {
                u128::MAX >> (128 - info.bits as u32)
            };
            lo = Bound::Finite(max);
        }
        let hi = match range.hi.minus_one() {
            Some(hi) => hi,
            None => {
                // `..ty::MIN`, which no inclusive range writes.
                end = RangeEnd::Excluded;
                range.hi
            }
        };
        if let Bound::Finite(bits) = lo {
            write_scalar(out, info, ty, bits);
        }
        match hoist(info, hi) {
            Bound::Finite(bits) => {
                _ = write!(out, "{end}");
                write_scalar(out, info, ty, bits);
            }
            Bound::NegInfinity | Bound::PosInfinity => out.push_str(".."),
        }
    }

    /// `print_pat_range` as text.
    pub(crate) fn range_text(&self, range: &IntRange, ty: Ty<'db>) -> Box<str> {
        // alloc: the text of a lint that fired.
        let mut out = String::new();
        self.print_range(&mut out, range, ty);
        out.into_boxed_str()
    }

    /// The exclusive range pattern `lo..hi` (`range`) made inclusive, `lo..=hi`, as rustc's
    /// `PatRange` prints it; `has_lo` is false for `..hi`, whose `lo` is `NegInfinity`.
    pub(crate) fn inclusive_text(&self, range: &IntRange, ty: Ty<'db>, has_lo: bool) -> Box<str> {
        // alloc: the text of a lint that fired.
        let mut out = String::new();
        let Some(info) = self.int_info(ty) else { return out.into_boxed_str() };
        if has_lo && let Bound::Finite(bits) = hoist(info, range.lo) {
            write_scalar(&mut out, info, ty, bits);
        }
        match hoist(info, range.hi) {
            Bound::Finite(bits) => {
                out.push_str("..=");
                write_scalar(&mut out, info, ty, bits);
            }
            Bound::NegInfinity | Bound::PosInfinity => out.push_str(".."),
        }
        out.into_boxed_str()
    }

    pub(crate) fn print_witness(&self, pat: &Witness<'a, 'db>) -> Box<str> {
        // alloc: a witness is printed only for an error that fired.
        let mut out = String::new();
        self.write_witness(&mut out, pat);
        out.into_boxed_str()
    }

    fn write_witness(&self, out: &mut String, pat: &Witness<'a, 'db>) {
        let ty = *pat.ty();
        match pat.ctor() {
            Constructor::Bool(b) => _ = write!(out, "{b}"),
            Constructor::Str(s) => _ = write!(out, "{:?}", s.as_str()),
            Constructor::IntRange(range) => self.print_range(out, range, ty),
            Constructor::Struct | Constructor::Variant(_) | Constructor::UnionField => {
                self.write_struct_like(out, pat)
            }
            Constructor::Ref => {
                out.push('&');
                if let TyKind::Ref(_, _, Mutability::Mut) = ty.kind() {
                    out.push_str("mut ");
                }
                if let Some(inner) = pat.iter_fields().next() {
                    self.write_witness(out, inner);
                }
            }
            Constructor::DerefPattern(_) => {
                // `box _` is how stable diagnostics write a deref of a box without
                // `deref_patterns`.
                if ty.is_box() && !self.deref_patterns {
                    out.push_str("box ");
                    if let Some(inner) = pat.iter_fields().next() {
                        self.write_witness(out, inner);
                    }
                } else {
                    out.push_str("deref!(");
                    if let Some(inner) = pat.iter_fields().next() {
                        self.write_witness(out, inner);
                    }
                    out.push(')');
                }
            }
            Constructor::Slice(slice) => {
                let (prefix_len, has_dot_dot) = match slice.kind() {
                    SliceKind::FixedLen(len) => (len, false),
                    SliceKind::VarLen(prefix_len, _) => (prefix_len, true),
                };
                // alloc: printing a witness of an error that fired.
                let fields: Vec<&Witness<'a, 'db>> = pat.iter_fields().collect();
                let split = prefix_len.min(fields.len());
                let (mut prefix, mut suffix) = fields.split_at(split);
                // Applied to an array, `..` takes in the wildcards next to it:
                // `[x, _, .., _, y]` is `[x, .., y]`.
                if has_dot_dot && slice.array_len().is_some() {
                    while let [rest @ .., last] = prefix
                        && would_print_as_wildcard(last, self.never_patterns)
                    {
                        prefix = rest;
                    }
                    while let [first, rest @ ..] = suffix
                        && would_print_as_wildcard(first, self.never_patterns)
                    {
                        suffix = rest;
                    }
                }
                out.push('[');
                let mut first = true;
                let mut comma = |out: &mut String| {
                    if !std::mem::take(&mut first) {
                        out.push_str(", ");
                    }
                };
                for p in prefix {
                    comma(out);
                    self.write_witness(out, p);
                }
                if has_dot_dot {
                    comma(out);
                    out.push_str("..");
                }
                for p in suffix {
                    comma(out);
                    self.write_witness(out, p);
                }
                out.push(']');
            }
            Constructor::Never if self.never_patterns => out.push('!'),
            Constructor::Never
            | Constructor::Wildcard
            | Constructor::NonExhaustive
            | Constructor::Hidden
            | Constructor::PrivateUninhabited => out.push('_'),
            // Never in a witness: `rustc_pattern_analysis` builds witnesses of none of these.
            Constructor::Missing
            | Constructor::F16Range(..)
            | Constructor::F32Range(..)
            | Constructor::F64Range(..)
            | Constructor::F128Range(..)
            | Constructor::Opaque(..)
            | Constructor::Or => out.push('_'),
        }
    }

    /// `write_struct_like`.
    fn write_struct_like(&self, out: &mut String, pat: &Witness<'a, 'db>) {
        let db = self.db;
        let ty = *pat.ty();
        let lang = self.infcx.interner.lang_items();
        // The variant and the name it is written with: `Some`, `Ok` without their enum.
        let named = match ty.kind() {
            TyKind::Adt(def, _) => {
                let adt = def.def_id();
                self.variant_of(pat.ctor(), adt).map(|variant| {
                    let name = match (adt, variant) {
                        (AdtId::EnumId(enum_), VariantId::EnumVariantId(v)) => {
                            let loc = v.lookup(db);
                            let is_option_or_result = lang.Option == Some(enum_)
                                || lang.ResultOk.is_some_and(|ok| ok.lookup(db).parent == enum_);
                            // alloc: printing a witness of an error that fired.
                            if is_option_or_result {
                                loc.name.as_str().to_owned() // alloc: as above.
                            } else {
                                // alloc: as above.
                                format!(
                                    "{}::{}",
                                    EnumSignature::of(db, enum_).name.as_str(),
                                    loc.name.as_str()
                                )
                            }
                        }
                        (AdtId::StructId(s), _) => {
                            // alloc: as above.
                            StructSignature::of(db, s).name.as_str().to_owned()
                        }
                        // alloc: as above.
                        (AdtId::UnionId(u), _) => UnionSignature::of(db, u).name.as_str().to_owned(),
                        (AdtId::EnumId(_), _) => String::new(), // alloc: empty.
                    };
                    (variant, name)
                })
            }
            _ => None,
        };
        let is_union = matches!(ty.kind(), TyKind::Adt(def, _) if matches!(def.def_id(), AdtId::UnionId(_)));
        let mut first = true;
        let mut comma = |out: &mut String| {
            if !std::mem::take(&mut first) {
                out.push_str(", ");
            }
        };
        if let Some((variant, name)) = &named {
            out.push_str(name);
            let fields = variant.fields(db);
            if fields.shape == FieldsShape::Record {
                out.push_str(" { ");
                let mut printed = 0;
                let names = fields.fields();
                for (i, field) in pat.iter_fields().enumerate() {
                    if would_print_as_wildcard(field, self.never_patterns) {
                        continue;
                    }
                    let Some((_, data)) = names.iter().nth(i) else { continue };
                    comma(out);
                    _ = write!(out, "{}: ", data.name.as_str());
                    self.write_witness(out, field);
                    printed += 1;
                }
                if printed < fields.len() && (!is_union || printed == 0) {
                    comma(out);
                    out.push_str("..");
                }
                out.push_str(" }");
                return;
            }
        }
        let num_fields = match &named {
            Some((variant, _)) => variant.fields(db).len(),
            None => pat.iter_fields().count(),
        };
        if num_fields != 0 || named.is_none() {
            out.push('(');
            for field in pat.iter_fields() {
                comma(out);
                self.write_witness(out, field);
            }
            if matches!(ty.kind(), TyKind::Tuple(..)) && num_fields == 1 {
                out.push(',');
            }
            out.push(')');
        }
    }
}

#[cfg(test)]
mod tests {
    use rustc_type_ir::{IntTy, UintTy};

    use super::*;

    fn info(bits: u64, signed: bool) -> IntInfo {
        IntInfo { bits, signed, ptr_sized: false }
    }

    #[test]
    fn hoisting_a_bound_past_the_type_is_pos_infinity() {
        let usize64 = IntInfo { bits: 64, signed: false, ptr_sized: true };
        let past_max = MaybeInfiniteInt::new_finite_uint(1u128 << 64);
        assert_eq!(hoist(usize64, past_max), Bound::PosInfinity);
        assert_eq!(hoist(usize64, MaybeInfiniteInt::new_finite_uint(7)), Bound::Finite(7));
        let i8_min = MaybeInfiniteInt::new_finite_int(0x80, 8);
        assert_eq!(hoist(info(8, true), i8_min), Bound::Finite(0x80));
    }

    #[test]
    fn unused_int_names_are_rustcs() {
        assert_eq!(IntTy::I32.name_str(), "i32");
        assert_eq!(UintTy::Usize.name_str(), "usize");
    }
}
