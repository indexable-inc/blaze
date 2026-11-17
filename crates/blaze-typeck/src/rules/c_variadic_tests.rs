//! C's default argument promotions as Rust refuses them at a variadic call (rules/c_variadic.rs).
//!
//! The rule, quoted from the pinned library (library/core/src/ffi/va_list.rs, `VaArgSafe`):
//! "When C passes variable arguments, signed integers smaller than [`c_int`] are promoted to
//! [`c_int`], unsigned integers smaller than [`c_uint`] are promoted to [`c_uint`], and
//! [`c_float`] is promoted to [`c_double`]." and from the error index, E0617: "Certain Rust
//! types must be cast before passing them to a variadic function, because of arcane ABI rules
//! dictated by the C standard" ("cannot pass an `f32` to variadic function, cast to
//! `c_double`").
//!
//! So `snprintf(buf, n, fmt, 42i32, 2.5f64)` passes (an `int` and a `double`, already the
//! promoted types), and each of `1.0f32`, `1i8`, `1i16`, `true`, `1u8`, `1u16` is refused with
//! the cast it needs; `i64`, `u32`, `usize`, `f64` and pointers pass. A wrong rule fails here:
//! one that refuses `i32` or `f64` (the promoted types), or lets `f32` or `bool` through.
//!
//! Run: `lanecheck.sh blaze-typeck test c_variadic`; prints "test blaze-typeck: exit 0; 1
//! passed, 0 failed".

use crate::ids::{FloatTy, IntTy, UintTy};
use crate::rules::c_variadic::{Promote, promotion_refusal};
use crate::ty::TyView;

#[test]
fn c_variadic_promotion_rule_refuses_what_c_promotes() {
    let refused = [
        (TyView::Float(FloatTy::F32), Promote::ToCDouble),
        (TyView::Int(IntTy::I8), Promote::ToCInt),
        (TyView::Int(IntTy::I16), Promote::ToCInt),
        (TyView::Bool, Promote::ToCInt),
        (TyView::Uint(UintTy::U8), Promote::ToCUint),
        (TyView::Uint(UintTy::U16), Promote::ToCUint),
    ];
    for (view, cast) in refused {
        assert_eq!(promotion_refusal(view), Some(cast), "{view:?} must be cast to {cast:?} before `...`");
    }
    let passed = [
        TyView::Int(IntTy::I32),
        TyView::Int(IntTy::I64),
        TyView::Int(IntTy::Isize),
        TyView::Uint(UintTy::U32),
        TyView::Uint(UintTy::U64),
        TyView::Uint(UintTy::Usize),
        TyView::Float(FloatTy::F64),
        TyView::Char,
    ];
    for view in passed {
        assert_eq!(promotion_refusal(view), None, "{view:?} is passed to `...` as it is");
    }
}
