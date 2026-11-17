//! Float `%`, 128-bit division and remainder, and casts between 128-bit integers and floats:
//! built by the walk (walk/numbers.rs, walk.rs `arith`), emitted as calls of the runtime's
//! routines (blaze-emit pass/runtime.rs), run natively with each routine bound to this test
//! binary's own (`routine_at`, which tests.rs `run_native_vt` asks for a `RelocTo::Routine`).

use super::*;

#[cfg(target_arch = "aarch64")]
#[cfg_attr(target_os = "linux", link(name = "m"))]
unsafe extern "C" {
    fn fmod(a: f64, b: f64) -> f64;
    fn fmodf(a: f32, b: f32) -> f32;
    fn __udivti3(n: u128, d: u128) -> u128;
    fn __umodti3(n: u128, d: u128) -> u128;
    fn __divti3(n: i128, d: i128) -> i128;
    fn __modti3(n: i128, d: i128) -> i128;
    fn __floattidf(i: i128) -> f64;
    fn __floatuntidf(i: u128) -> f64;
    fn __floattisf(i: i128) -> f32;
    fn __floatuntisf(i: u128) -> f32;
    fn __fixdfti(f: f64) -> i128;
    fn __fixunsdfti(f: f64) -> u128;
    fn __fixsfti(f: f32) -> i128;
    fn __fixunssfti(f: f32) -> u128;
}

/// The address the native run binds a system routine to: the C library's (`memcpy` and kin,
/// `fmod`, `fmodf`) and the compiler runtime's that this binary links.
#[cfg(target_arch = "aarch64")]
pub(super) fn routine_at(routine: blaze_emit::Routine) -> u64 {
    use blaze_emit::Routine as R;
    unsafe extern "C" {
        fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8;
        fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8;
        fn memset(dst: *mut u8, byte: i32, n: usize) -> *mut u8;
    }
    match routine {
        R::Memcpy => memcpy as usize as u64,
        R::Memmove => memmove as usize as u64,
        R::Memset => memset as usize as u64,
        R::Fmod => fmod as usize as u64,
        R::Fmodf => fmodf as usize as u64,
        R::Udivti3 => __udivti3 as usize as u64,
        R::Umodti3 => __umodti3 as usize as u64,
        R::Divti3 => __divti3 as usize as u64,
        R::Modti3 => __modti3 as usize as u64,
        R::Floattidf => __floattidf as usize as u64,
        R::Floatuntidf => __floatuntidf as usize as u64,
        R::Floattisf => __floattisf as usize as u64,
        R::Floatuntisf => __floatuntisf as usize as u64,
        R::Fixdfti => __fixdfti as usize as u64,
        R::Fixunsdfti => __fixunsdfti as usize as u64,
        R::Fixsfti => __fixsfti as usize as u64,
        R::Fixunssfti => __fixunssfti as usize as u64,
        R::UnwindResume => panic!("no wide-integer test calls `_Unwind_Resume`"),
        R::Memcmp => panic!("no wide-integer test calls `memcmp`"),
    }
}

const F64: PNode = PNode::Float(blaze_typeck::FloatTy::F64);

fn f64_lit(b: &mut BodyBuf, x: f64) -> ExprId {
    let e = b.e(Expr::Lit(Lit::Float(Some(blaze_typeck::FloatTy::F64))));
    b.lits.push((e, u128::from(x.to_bits())));
    e
}

fn neg(b: &mut BodyBuf, x: ExprId) -> ExprId {
    b.e(Expr::Unary { op: blaze_typeck::UnOp::Neg, operand: x })
}

/// Each check is one bit of the result; 255 is all eight. Every expected value by hand:
/// ```text
/// fn main() -> u64 {
///     (7.5f64 % 2.0 == 1.5) as u64                                     // 1
///     // The Reference, "Arithmetic and Logical Binary Operators": float `%` is the
///     // remainder of a truncating division, its sign the dividend's: 7.5 = 3 * 2.0 + 1.5,
///     // -7.5 = -3 * 2.0 - 1.5.
///   + (-7.5f64 % 2.0 == -1.5) as u64 * 2                               // 2
///     // 2^100 - 1 is 100 one bits, 25 hex `F`s; / 3 is 25 hex `5`s; 2^100 = 3q + 1 since
///     // 4^50 = 1 (mod 3), so 2^100 / 3 truncates to the same q.
///   + ((1u128 << 100) / 3 == 0x5_5555_5555_5555_5555_5555_5555) as u64 * 4
///     // Integer `%` truncates too: -7 = -2 * 3 - 1.
///   + (-7i128 % 3 == -1) as u64 * 8
///     // "Type cast expressions": "Casting from an integer to float will produce the closest
///     // possible float": 2^128 - 1 is 2^-128 relatively from 2^128, far inside half an f64
///     // ulp (2^75 at 2^128), so the f64 is 2^128 exactly (bits (1023 + 128) << 52).
///   + (u128::MAX as f64 == 2^128) as u64 * 16
///     // "values larger than the maximum integer value ... will saturate": 1e40 > 2^128
///     // (about 3.4e38).
///   + (1e40f64 as u128 == u128::MAX) as u64 * 32
///     // "`NaN` will return `0`".
///   + ((0.0f64 / 0.0) as i128 == 0) as u64 * 64
///     // "will round the float towards zero": -3.75 to -3 (the runtime's `__fixdfti`).
///   + (-3.75f64 as i128 == -3) as u64 * 128
/// }                                                                    // 255
/// ```
/// Expected line (lanecheck test blaze-build "wide"): `test ...wide::float_rem_wide_div_and_wide_casts_run ... ok`.
/// A wrong fix fails it: `fmod` swapped for a floored remainder makes check 2 `0.5` (bit 2
/// lost: 253); a missing NaN case makes check 7 depend on the system's `__fixdfti` (compiler-rt
/// gives `i128::MAX`: 191); `__divti3` for `%` makes check 4 `-2` (247).
#[test]
fn float_rem_wide_div_and_wide_casts_run() {
    let mut w = TestWorld::new();
    let u64_ = PNode::Uint(UintTy::U64);
    w.func(1, "main", &[], u64_);
    let mut b = BodyBuf::new();
    let u64_ty = b.types.intern(u64_).expect("a type interns");
    let f64_ty = b.types.intern(F64).expect("a type interns");
    let u128_ty = b.types.intern(PNode::Uint(UintTy::U128)).expect("a type interns");
    let i128_ty = b.types.intern(PNode::Int(IntTy::I128)).expect("a type interns");
    let i128_ = Some(IntSuffix::Int(IntTy::I128));
    let u128_ = Some(IntSuffix::Uint(UintTy::U128));
    let cast = |b: &mut BodyBuf, x: ExprId, ty: PTy| b.e(Expr::Cast { operand: x, ty });
    // `(lhs == rhs) as u64 * weight`.
    let bit = |b: &mut BodyBuf, lhs: ExprId, rhs: ExprId, weight: u128| {
        let eq = b.bin(BinOp::Eq, lhs, rhs);
        let eq = b.e(Expr::Cast { operand: eq, ty: u64_ty });
        let k = b.int(weight, None);
        b.bin(BinOp::Mul, eq, k)
    };
    // 1: 7.5 % 2.0 == 1.5
    let (x, y) = (f64_lit(&mut b, 7.5), f64_lit(&mut b, 2.0));
    let r = b.bin(BinOp::Rem, x, y);
    let want = f64_lit(&mut b, 1.5);
    let c1 = bit(&mut b, r, want, 1);
    // 2: -7.5 % 2.0 == -1.5
    let x = f64_lit(&mut b, 7.5);
    let x = neg(&mut b, x);
    let y = f64_lit(&mut b, 2.0);
    let r = b.bin(BinOp::Rem, x, y);
    let want = f64_lit(&mut b, 1.5);
    let want = neg(&mut b, want);
    let c2 = bit(&mut b, r, want, 2);
    // 4: (1u128 << 100) / 3 == 0x5_5555_5555_5555_5555_5555_5555
    let one = b.int(1, u128_);
    let k = b.int(100, None);
    let p = b.bin(BinOp::Shl, one, k);
    let three = b.int(3, None);
    let q = b.bin(BinOp::Div, p, three);
    let want = b.int(0x5_5555_5555_5555_5555_5555_5555, u128_);
    let c3 = bit(&mut b, q, want, 4);
    // 8: -7i128 % 3 == -1
    let seven = b.int(7, i128_);
    let seven = neg(&mut b, seven);
    let three = b.int(3, None);
    let r = b.bin(BinOp::Rem, seven, three);
    let one = b.int(1, None);
    let want = neg(&mut b, one);
    let c4 = bit(&mut b, r, want, 8);
    // 16: u128::MAX as f64 == 2^128
    let max = b.int(u128::MAX, u128_);
    let f = cast(&mut b, max, f64_ty);
    let want = f64_lit(&mut b, f64::from_bits((1023 + 128) << 52));
    let c5 = bit(&mut b, f, want, 16);
    // 32: 1e40 as u128 == u128::MAX
    let big = f64_lit(&mut b, 1e40);
    let n = cast(&mut b, big, u128_ty);
    let want = b.int(u128::MAX, u128_);
    let c6 = bit(&mut b, n, want, 32);
    // 64: (0.0 / 0.0) as i128 == 0
    let (z1, z2) = (f64_lit(&mut b, 0.0), f64_lit(&mut b, 0.0));
    let nan = b.bin(BinOp::Div, z1, z2);
    let n = cast(&mut b, nan, i128_ty);
    let want = b.int(0, None);
    let c7 = bit(&mut b, n, want, 64);
    // 128: -3.75 as i128 == -3
    let x = f64_lit(&mut b, 3.75);
    let x = neg(&mut b, x);
    let n = cast(&mut b, x, i128_ty);
    let three = b.int(3, None);
    let want = neg(&mut b, three);
    let c8 = bit(&mut b, n, want, 128);
    let mut sum = c1;
    for c in [c2, c3, c4, c5, c6, c7, c8] {
        sum = b.bin(BinOp::Add, sum, c);
    }
    let root = b.block(&[], Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    // The walk makes the operations the routines serve (`sitofp` from `i128` is not checked).
    let text = blaze_lir::print(built.func.view());
    for op in ["frem", "udiv", "srem", "uitofp", "fptoui", "fptosi"] {
        assert!(text.contains(op), "no `{op}` in the built function:\n{text}");
    }
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native_with(&[(PANIC_CONST, host_panic_const as usize as u64)], &[(MAIN, None, &built)]), 255);
    let _ = built;
}
