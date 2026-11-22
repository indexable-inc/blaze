//! Textual and numeric forms (walk/strs.rs): `char`, C string literals, `offset_of!`; each
//! built from a body the checker types, then run natively and its result compared.

use super::*;

/// The fixture's `core::ffi::CStr` (lang "CStr").
pub(super) const CSTR: ItemKey = ItemKey(70);

const CHAR: PNode = PNode::Char;

/// The Reference, "Textual types": "`char` ... has the same size and alignment as `u32`", a
/// Unicode scalar value; "Operator expressions", "Type cast expressions": "`u8` -> `char`" casts
/// the byte to the scalar of that value, "`char` -> integer" gives the scalar's value.
/// ```text
/// fn main() -> u32 {
///     let c = 'é';            // U+00E9
///     let a = 65u8 as char;
///     (c as u32) + (a as u32) * 1000 + ((c == 'é') as u32) * 100_000 + ((a < c) as u32) * 1_000_000
/// }
/// ```
/// 233 + 65_000 + 100_000 + 1_000_000.
#[test]
fn a_char_is_its_scalar_value_compared_and_cast() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    let mut b = BodyBuf::new();
    let u32_ty = b.types.intern(U32).expect("a type interns");
    let char_ty = b.types.intern(CHAR).expect("a type interns");
    let lit = |b: &mut BodyBuf, v: u128| {
        let e = b.e(Expr::Lit(Lit::Char));
        b.lits.push((e, v));
        e
    };
    let (c, a) = (b.bind("c"), b.bind("a"));
    let c_init = lit(&mut b, 0xE9);
    let byte = b.int(65, Some(IntSuffix::Uint(UintTy::U8)));
    let a_init = b.e(Expr::Cast { operand: byte, ty: char_ty });
    let cast = |b: &mut BodyBuf, x: ExprId| b.e(Expr::Cast { operand: x, ty: u32_ty });
    let c_e = b.local(c);
    let c32 = cast(&mut b, c_e);
    let a_e = b.local(a);
    let a32 = cast(&mut b, a_e);
    let k = b.int(1000, None);
    let a_k = b.bin(BinOp::Mul, a32, k);
    let c_e = b.local(c);
    let e9 = lit(&mut b, 0xE9);
    let eq = b.bin(BinOp::Eq, c_e, e9);
    let eq = cast(&mut b, eq);
    let k = b.int(100_000, None);
    let eq_k = b.bin(BinOp::Mul, eq, k);
    let (a_e, c_e) = (b.local(a), b.local(c));
    let lt = b.bin(BinOp::Lt, a_e, c_e);
    let lt = cast(&mut b, lt);
    let k = b.int(1_000_000, None);
    let lt_k = b.bin(BinOp::Mul, lt, k);
    let sum = b.bin(BinOp::Add, c32, a_k);
    let sum = b.bin(BinOp::Add, sum, eq_k);
    let sum = b.bin(BinOp::Add, sum, lt_k);
    let root = b.block(
        &[
            Stmt::Let { pat: c, ty: None, init: Some(c_init), else_: None },
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]), 1_165_233);
    let _ = built;
}

/// The Reference, "C string literal expressions": type `&'static core::ffi::CStr`, the
/// literal's bytes "followed by a nul byte". The toolchain's library/core/src/ffi/c_str.rs:
/// `#[repr(transparent)] pub struct CStr { inner: [c_char] }`: the wide pointer's length counts
/// the nul.
/// ```text
/// fn main() -> usize {
///     let s = c"ab";
///     let t: (*const u8, usize) = transmute(s);
///     t.1 * 1000 + *t.0 as usize      // 3 * 1000 + b'a'
/// }
/// ```
#[test]
fn a_c_string_literal_is_its_bytes_with_a_nul() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], USIZE);
    let u8_ = w.ty(U8);
    let slice = w.ty(PNode::Slice(u8_));
    w.strukt(CSTR.0, "CStr", &[("inner", PNode::Slice(u8_))]);
    let _ = slice;
    w.generic_func(6, "transmute", 2, &[PNode::Param(0)], PNode::Param(1));
    w.intrinsics.push((ItemKey(6), Intrinsic::Table(crate::intrinsics::Which::Transmute)));
    let mut b = BodyBuf::new();
    let u8_ty = b.types.intern(U8).expect("a type interns");
    let usize_ty = b.types.intern(USIZE).expect("a type interns");
    let p = b.types.intern(PNode::Ptr(Mutbl::Not, u8_ty)).expect("a type interns");
    let l = b.types.intern_list(&[p, usize_ty]).expect("a list interns");
    let pair = b.types.intern(PNode::Tuple(l)).expect("a type interns");
    let (s, t) = (b.bind("s"), b.bind("t"));
    let lit = b.e(Expr::Lit(Lit::CStr));
    b.strs.push((lit, "ab"));
    let s_e = b.local(s);
    let t_init = b.call("transmute", &[s_e]);
    let t_e = b.local(t);
    let len = b.tfield(t_e, 1);
    let k = b.int(1000, None);
    let len_k = b.bin(BinOp::Mul, len, k);
    let t_e = b.local(t);
    let addr = b.tfield(t_e, 0);
    let first = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: addr });
    let first = b.e(Expr::Cast { operand: first, ty: usize_ty });
    let sum = b.bin(BinOp::Add, len_k, first);
    let root = b.block(
        &[
            Stmt::Let { pat: s, ty: None, init: Some(lit), else_: None },
            Stmt::Let { pat: t, ty: Some(pair), init: Some(t_init), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("bytes \"616200\""), "{text}");
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]), 3097);
}

/// `offset_of!` (core::mem::offset_of, "Expands to the offset in bytes of a field from the
/// beginning of the given type"), over `repr(C)` structs whose offsets the Reference's "The C
/// Representation" fixes: `struct S { a: u8, b: u32, c: P }`, `struct P { x: u8, y: u64 }`:
/// `b` at 4, `c` at 8, `c.y` at 8 + 8.
/// ```text
/// fn main() -> usize { offset_of!(S, b) * 100 + offset_of!(S, c.y) }   // 416
/// ```
#[test]
fn offset_of_adds_each_steps_offset() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], USIZE);
    let u64_ = PNode::Uint(UintTy::U64);
    w.strukt(71, "P", &[("x", U8), ("y", u64_)]);
    let p = w.ty(PNode::Adt(ItemKey(71), List::EMPTY));
    let _ = p;
    w.strukt(72, "S", &[("a", U8), ("b", U32), ("c", PNode::Adt(ItemKey(71), List::EMPTY))]);
    for k in [71, 72] {
        w.reprs.push((ItemKey(k), ReprAttrs { c: true, ..ReprAttrs::RUST }));
    }
    let mut b = BodyBuf::new();
    let s_ty = b.types.intern(PNode::Adt(ItemKey(72), List::EMPTY)).expect("a type interns");
    let path = |b: &mut BodyBuf, names: &[&str]| {
        let start = b.segments.len() as u32;
        for n in names {
            b.segments.push(Segment { name: Name::of(n), generic_args: List::EMPTY, has_args: false });
        }
        b.e(Expr::OffsetOf { ty: s_ty, fields: List { start, len: names.len() as u32 } })
    };
    let ob = path(&mut b, &["b"]);
    let k = b.int(100, None);
    let ob = b.bin(BinOp::Mul, ob, k);
    let ocy = path(&mut b, &["c", "y"]);
    let sum = b.bin(BinOp::Add, ob, ocy);
    let root = b.block(&[], Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]), 416);
    let _ = built;
}

const F64: PNode = PNode::Float(blaze_typeck::FloatTy::F64);

fn f64_lit(b: &mut BodyBuf, x: f64) -> ExprId {
    let e = b.e(Expr::Lit(Lit::Float(Some(blaze_typeck::FloatTy::F64))));
    b.lits.push((e, u128::from(x.to_bits())));
    e
}

/// walk/numbers.rs: the Reference's "Comparison operators" (`NaN` is unordered: `==` false,
/// `!=` true) and "Type cast expressions" (float to integer rounds toward zero and saturates).
/// ```text
/// fn main() -> i32 {
///     let a = 1.5f64;
///     let b = a + 2.25;
///     let n = 0.0f64 / 0.0;
///     (b > a) as i32 + (n == n) as i32 * 10 + (n != n) as i32 * 100
///         + (b * 1000.0) as i32 * 1000 + (-1e300f64 as i8) as i32      // 1 + 0 + 100 + 3_750_000 - 128
/// }
/// ```
#[test]
fn f64_arithmetic_comparison_and_saturating_casts_run() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], I32);
    let mut b = BodyBuf::new();
    let i32_ty = b.types.intern(I32).expect("a type interns");
    let i8_ty = b.types.intern(PNode::Int(IntTy::I8)).expect("a type interns");
    let (a, bb, n) = (b.bind("a"), b.bind("b"), b.bind("n"));
    let a_init = f64_lit(&mut b, 1.5);
    let a_e = b.local(a);
    let two = f64_lit(&mut b, 2.25);
    let b_init = b.bin(BinOp::Add, a_e, two);
    let (z1, z2) = (f64_lit(&mut b, 0.0), f64_lit(&mut b, 0.0));
    let n_init = b.bin(BinOp::Div, z1, z2);
    let cast = |b: &mut BodyBuf, x: ExprId, ty: PTy| b.e(Expr::Cast { operand: x, ty });
    let times = |b: &mut BodyBuf, x: ExprId, k: u128| {
        let k = b.int(k, None);
        b.bin(BinOp::Mul, x, k)
    };
    let (b_e, a_e) = (b.local(bb), b.local(a));
    let gt = b.bin(BinOp::Gt, b_e, a_e);
    let gt = cast(&mut b, gt, i32_ty);
    let (n1, n2) = (b.local(n), b.local(n));
    let eq = b.bin(BinOp::Eq, n1, n2);
    let eq = cast(&mut b, eq, i32_ty);
    let eq = times(&mut b, eq, 10);
    let (n1, n2) = (b.local(n), b.local(n));
    let ne = b.bin(BinOp::Ne, n1, n2);
    let ne = cast(&mut b, ne, i32_ty);
    let ne = times(&mut b, ne, 100);
    let b_e = b.local(bb);
    let k = f64_lit(&mut b, 1000.0);
    let scaled = b.bin(BinOp::Mul, b_e, k);
    let scaled = cast(&mut b, scaled, i32_ty);
    let scaled = times(&mut b, scaled, 1000);
    let huge = f64_lit(&mut b, 1e300);
    let neg = b.e(Expr::Unary { op: blaze_typeck::UnOp::Neg, operand: huge });
    let sat = cast(&mut b, neg, i8_ty);
    let sat = cast(&mut b, sat, i32_ty);
    let sum = b.bin(BinOp::Add, gt, eq);
    let sum = b.bin(BinOp::Add, sum, ne);
    let sum = b.bin(BinOp::Add, sum, scaled);
    let sum = b.bin(BinOp::Add, sum, sat);
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
            Stmt::Let { pat: bb, ty: None, init: Some(b_init), else_: None },
            Stmt::Let { pat: n, ty: None, init: Some(n_init), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]), 3_749_973);
    let _ = (built, F64);
}

/// walk/numbers.rs: `u128` is an integer of the walk (the Reference, "Integer types": 16 bytes),
/// computed in register pairs by the emitter (pass/wide.rs).
/// ```text
/// fn main() -> u64 {
///     let a = 0xFFFF_FFFF_FFFF_FFFFu128;
///     let b = a * a;                         // 2^128 - 2^65 + 1
///     (b >> 64) as u64 - b as u64 + (b > a) as u64       // 0xFFFF_FFFF_FFFF_FFFE
/// }
/// ```
#[test]
fn u128_multiply_shift_compare_and_cast_run() {
    let mut w = TestWorld::new();
    let u64_ = PNode::Uint(UintTy::U64);
    w.func(1, "main", &[], u64_);
    let mut b = BodyBuf::new();
    let u64_ty = b.types.intern(u64_).expect("a type interns");
    let (a, p) = (b.bind("a"), b.bind("b"));
    let a_init = b.int(u128::from(u64::MAX), Some(IntSuffix::Uint(UintTy::U128)));
    let (a1, a2) = (b.local(a), b.local(a));
    let p_init = b.bin(BinOp::Mul, a1, a2);
    let p_e = b.local(p);
    let k = b.int(64, None);
    let hi = b.bin(BinOp::Shr, p_e, k);
    let hi = b.e(Expr::Cast { operand: hi, ty: u64_ty });
    let p_e = b.local(p);
    let lo = b.e(Expr::Cast { operand: p_e, ty: u64_ty });
    let (p_e, a_e) = (b.local(p), b.local(a));
    let gt = b.bin(BinOp::Gt, p_e, a_e);
    let gt = b.e(Expr::Cast { operand: gt, ty: u64_ty });
    let d = b.bin(BinOp::Sub, hi, lo);
    let sum = b.bin(BinOp::Add, d, gt);
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
            Stmt::Let { pat: p, ty: None, init: Some(p_init), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]), 0xFFFF_FFFF_FFFF_FFFE);
    let _ = built;
}
