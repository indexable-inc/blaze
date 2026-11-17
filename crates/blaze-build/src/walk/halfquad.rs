//! `f16` and `f128` in the walk: their LIR scalars, a literal's bits at its type, and the
//! conversion between two float widths.
//!
//! The pinned library's `core/src/primitive_docs.rs`: `f16` is "A 16-bit floating-point type
//! (specifically, the "binary16" type defined in IEEE 754-2008)", and `f128` "A 128-bit
//! floating-point type (specifically, the "binary128" type defined in IEEE 754-2008)". LIR has
//! both as scalars (`Ty::F16`, `Ty::F128`; `Builder::fconst` takes 16 and 128 bits; the
//! verifier takes an `fconst` at `f16` and an `fconst_wide` at `f128`), so the walk builds
//! them as it builds `f32` and `f64`: the same `fadd`, `fcmp`, `fpext`, `fptosi` and kin at
//! the wider or narrower type. What a tier cannot do on them is LIR's legalize's to rewrite
//! (INTRINSICS.md P3: `f16` arithmetic through `f32`, every `f128` operation a call of the
//! program's compiler_builtins routine), never the walk's.
//!
//! "Type cast expressions": "Casting from an f32 to an f64 is perfect and lossless"; "from an
//! f64 to an f32 will produce the closest possible f32". The same holds between any two of
//! the four IEEE formats, each wider one holding every value of a narrower one: a cast to a
//! wider float is `fpext`, to a narrower one `fptrunc` (round to nearest, ties to even), to
//! the same one nothing ([`resize_op`]).

use core::alloc::Allocator;

use blaze_lir::{Builder, Op, Sink, Ty, Value};
use blaze_typeck::FloatTy;

/// The LIR scalar a float type is held in.
pub(crate) fn float_ty(f: FloatTy) -> Ty {
    match f {
        FloatTy::F16 => Ty::F16,
        FloatTy::F32 => Ty::F32,
        FloatTy::F64 => Ty::F64,
        FloatTy::F128 => Ty::F128,
    }
}

/// The float type LIR scalar `ty` holds; `None` for a type that is no float scalar.
pub(crate) fn float_of(ty: Ty) -> Option<FloatTy> {
    if ty == Ty::F16 {
        Some(FloatTy::F16)
    } else if ty == Ty::F32 {
        Some(FloatTy::F32)
    } else if ty == Ty::F64 {
        Some(FloatTy::F64)
    } else if ty == Ty::F128 {
        Some(FloatTy::F128)
    } else {
        None
    }
}

/// The width of float type `f` in bits: its IEEE 754 format's (binary16, 32, 64, 128).
pub(crate) fn width(f: FloatTy) -> u32 {
    match f {
        FloatTy::F16 => 16,
        FloatTy::F32 => 32,
        FloatTy::F64 => 64,
        FloatTy::F128 => 128,
    }
}

/// Whether `bits` are a value of float type `f`: no bit set past its width. A literal's bits
/// come from the front end (`Adts::float_lit`); bits past the width are a malformed seam,
/// which the caller refuses by name before [`float_lit`] (which keeps only the width's bits,
/// as `Builder::fconst` does).
pub(crate) fn lit_fits(f: FloatTy, bits: u128) -> bool {
    let w = width(f);
    w == 128 || bits >> w == 0
}

/// A float constant of type `ty` with IEEE bits `bits` (checked by [`lit_fits`] first): an
/// `fconst` at `f16` and `f32`, an `fconst_wide` at `f64` and `f128` (`Builder::fconst`).
pub(crate) fn float_lit<LA: Allocator + Clone, S: Sink>(b: &mut Builder<LA, S>, ty: FloatTy, bits: u128) -> Value {
    b.fconst(float_ty(ty), bits)
}

/// The conversion of a float of type `from` to type `to` ("Type cast expressions", above):
/// `fpext` to a wider type, `fptrunc` to a narrower one, `None` for the same type.
pub(crate) fn resize_op(from: FloatTy, to: FloatTy) -> Option<Op> {
    match width(from).cmp(&width(to)) {
        core::cmp::Ordering::Less => Some(Op::Fpext),
        core::cmp::Ordering::Greater => Some(Op::Fptrunc),
        core::cmp::Ordering::Equal => None,
    }
}

/// Whether LIR scalar `ty` is a float scalar of any of the four widths: a value with no drop
/// (walk/unwind.rs asks it of the value stack).
pub(crate) fn is_float_scalar(ty: Ty) -> bool {
    float_of(ty).is_some()
}

#[cfg(test)]
mod tests {
    use blaze_lir::{Op, Ty};
    use blaze_typeck::FloatTy;

    use super::{float_of, float_ty, is_float_scalar, lit_fits, resize_op};

    /// "Type cast expressions": "Casting from an f32 to an f64 is perfect and lossless"; "from
    /// an f64 to an f32 will produce the closest possible f32": to a wider format `fpext`, to
    /// a narrower `fptrunc`, at all four widths (the old walk chose `fpext` only when the
    /// target was `f64`, which makes `f32 as f128` an `fptrunc`: this test fails on it).
    /// Expected line: `test walk::halfquad::tests::widths_order_the_casts ... ok`.
    #[test]
    fn widths_order_the_casts() {
        let all = [FloatTy::F16, FloatTy::F32, FloatTy::F64, FloatTy::F128];
        for (i, &a) in all.iter().enumerate() {
            assert_eq!(float_of(float_ty(a)), Some(a));
            assert!(is_float_scalar(float_ty(a)));
            for (j, &b) in all.iter().enumerate() {
                let want = match i.cmp(&j) {
                    core::cmp::Ordering::Less => Some(Op::Fpext),
                    core::cmp::Ordering::Greater => Some(Op::Fptrunc),
                    core::cmp::Ordering::Equal => None,
                };
                assert_eq!(resize_op(a, b), want, "{a:?} as {b:?}");
            }
        }
        assert_eq!(float_of(Ty::I64), None);
        assert!(!is_float_scalar(Ty::I16));
        // binary16's 1.0 is 0x3c00; a 17-bit value is no f16; every u128 is an f128's bits.
        assert!(lit_fits(FloatTy::F16, 0x3c00) && !lit_fits(FloatTy::F16, 0x1_0000));
        assert!(lit_fits(FloatTy::F128, u128::MAX));
    }
}
