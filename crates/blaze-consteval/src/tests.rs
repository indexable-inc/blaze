//! The evaluator's suite: each case is Rust text lowered, checked and evaluated through the
//! driver's own pipeline (`fixture`), and states the rule of the Reference it holds the
//! evaluator to. Refused cases name their refusal. Then the property (the evaluator equals
//! direct computation) and three planted-bug controls (rule 23).

use std::string::String;
use std::vec::Vec as StdVec;

use blaze_sim::{Broke, Config, Tape, control};
use blaze_typeck::{ItemKey, PNode, PortableTys};

use crate::fixture::{World, int_of};
use crate::machine::{Answer, Codes, Plant, discriminant_with_plant, int_binop, mask, sext, value_of_planted};
use crate::record::DataTarget;
use crate::{ConstError, Intrinsic, Layouts, Limits, Op, PanicPart, Refusal, eval_discriminant};

mod byte_intrinsics;
mod closure_fn_ptr;
mod dst_tail;
mod fn_fact;
mod needs_drop;
mod records_unions;
mod transmute_unchecked;
mod type_id;
mod ub_checks;
mod wide_floats;

/// A world with the intrinsics and the panic entry core's code reaches.
fn world() -> World {
    let mut w = World::new();
    let fx = &mut w.fx;
    let p0 = fx.param(0);
    let p1 = fx.param(1);
    let usize_ = fx.usize();
    let b = fx.ty(PNode::Bool);
    let pair = fx.tuple(&[p0, b]);
    w.intrinsic("size_of", 1, &[], usize_, Intrinsic::SizeOf);
    w.intrinsic("align_of", 1, &[], usize_, Intrinsic::AlignOf);
    w.intrinsic("transmute", 2, &[p0], p1, Intrinsic::Transmute);
    w.intrinsic("wrapping_add", 1, &[p0, p0], p0, Intrinsic::WrappingAdd);
    w.intrinsic("wrapping_sub", 1, &[p0, p0], p0, Intrinsic::WrappingSub);
    w.intrinsic("wrapping_mul", 1, &[p0, p0], p0, Intrinsic::WrappingMul);
    w.intrinsic("add_with_overflow", 1, &[p0, p0], pair, Intrinsic::AddWithOverflow);
    w.intrinsic("rotate_left", 1, &[p0, p0], p0, Intrinsic::RotateLeft);
    w.intrinsic("ctpop", 1, &[p0], p0, Intrinsic::Ctpop);
    w.intrinsic("ctlz", 1, &[p0], p0, Intrinsic::Ctlz);
    w.intrinsic("ctlz_nonzero", 1, &[p0], p0, Intrinsic::CtlzNonzero);
    w.intrinsic("cttz_nonzero", 1, &[p0], p0, Intrinsic::CttzNonzero);
    w.intrinsic("bswap", 1, &[p0], p0, Intrinsic::Bswap);
    w.intrinsic("saturating_sub", 1, &[p0, p0], p0, Intrinsic::SaturatingSub);
    w.panic_fn("panic_display", "&str");
    w
}

/// `const C: TY = BODY;` alone in the prelude world: its value as an integer, or the refusal.
fn int(ty: &str, body: &str) -> Result<u128, String> {
    let mut w = world();
    w.konst("C", ty, body);
    w.build();
    eval_int(&mut w, "C")
}

fn eval_int(w: &mut World, name: &str) -> Result<u128, String> {
    w.eval(name).map(int_of).map_err(|e| std::format!("{:?}", e.refusal))
}

fn refusal(w: &mut World, name: &str) -> String {
    match w.eval(name) {
        Ok(r) => panic!("`{name}` evaluated to {:?}, a refusal was expected", r),
        Err(e) => std::format!("{:?}", e.refusal),
    }
}

fn refused(ty: &str, body: &str) -> String {
    match int(ty, body) {
        Ok(v) => panic!("`{body}` evaluated to {v:#x}, a refusal was expected"),
        Err(e) => e,
    }
}

const INTS: [(&str, u32, bool); 12] = [
    ("i8", 8, true),
    ("i16", 16, true),
    ("i32", 32, true),
    ("i64", 64, true),
    ("i128", 128, true),
    ("isize", 64, true),
    ("u8", 8, false),
    ("u16", 16, false),
    ("u32", 32, false),
    ("u64", 64, false),
    ("u128", 128, false),
    ("usize", 64, false),
];

fn max_text(bits: u32, signed: bool) -> String {
    if signed { std::format!("{}", (1u128 << (bits - 1)) - 1) } else { std::format!("{}", mask(u128::MAX, bits)) }
}

/// The Reference, "Arithmetic and Logical Binary Operators" and "Overflow": integer
/// arithmetic at every width gives the mathematical result when it fits, and in a constant
/// context overflow is an error (rustc's `arithmetic_overflow`, deny-by-default, is a hard
/// error in const evaluation).
#[test]
fn arithmetic_at_every_width() {
    for (ty, bits, signed) in INTS {
        let max = max_text(bits, signed);
        assert_eq!(int(ty, &std::format!("{max} - 1 + 1")), Ok(mask(u128::MAX, if signed { bits - 1 } else { bits })), "{ty}: MAX - 1 + 1");
        assert_eq!(int(ty, "7 * 6 - 2 / 2 % 5"), Ok(41), "{ty}: precedence");
        assert_eq!(refused(ty, &std::format!("{max} + 1")), std::format!("{:?}", Refusal::Overflow { op: Op::Add }), "{ty}: MAX + 1");
        assert_eq!(refused(ty, &std::format!("{max} * 2")), std::format!("{:?}", Refusal::Overflow { op: Op::Mul }), "{ty}: MAX * 2");
        if signed {
            let min = std::format!("(-{max} - 1)");
            let want = if bits == 128 { i128::MIN } else { -(1i128 << (bits - 1)) };
            assert_eq!(int(ty, &min).map(|v| sext(v, bits)), Ok(want), "{ty}: MIN");
            assert_eq!(refused(ty, &std::format!("{min} - 1")), std::format!("{:?}", Refusal::Overflow { op: Op::Sub }), "{ty}: MIN - 1");
            assert_eq!(refused(ty, &std::format!("{min} / -1")), std::format!("{:?}", Refusal::Overflow { op: Op::Div }), "{ty}: MIN / -1");
            assert_eq!(refused(ty, &std::format!("{min} % -1")), std::format!("{:?}", Refusal::Overflow { op: Op::Rem }), "{ty}: MIN % -1");
            assert_eq!(int(ty, "-7 / 2").map(|v| sext(v, bits)), Ok(-3), "{ty}: division truncates toward zero");
            assert_eq!(int(ty, "-7 % 2").map(|v| sext(v, bits)), Ok(-1), "{ty}: the remainder has the dividend's sign");
        } else {
            assert_eq!(refused(ty, "0 - 1"), std::format!("{:?}", Refusal::Overflow { op: Op::Sub }), "{ty}: 0 - 1");
        }
    }
}

/// The Reference, "Literal expressions": `-128i8` is the negation of a literal that does not
/// fit `i8` by itself, accepted (the `overflowing_literals` lint looks through the minus);
/// negating the minimum held in a constant overflows (`Overflow { op: Neg }`).
#[test]
fn negation() {
    assert_eq!(int("i8", "-128").map(|v| sext(v, 8)), Ok(-128));
    let mut w = world();
    w.konst("M", "i8", "-128");
    w.konst("C", "i8", "-M");
    w.build();
    assert_eq!(refusal(&mut w, "C"), std::format!("{:?}", Refusal::Overflow { op: Op::Neg }));
}

/// core's `wrapping_*`, `*_with_overflow`, `rotate_left`, `ctpop`, `bswap` and
/// `saturating_sub` intrinsics: the wrapping operations are defined modulo 2^N, never an error.
#[test]
fn wrapping_and_bit_operations() {
    assert_eq!(int("u8", "wrapping_add(250u8, 10u8)"), Ok(4));
    assert_eq!(int("i8", "wrapping_sub(-128i8, 1i8)").map(|v| sext(v, 8)), Ok(127));
    assert_eq!(int("u16", "wrapping_mul(300u16, 300u16)"), Ok((300u32 * 300 % 65536) as u128));
    assert_eq!(int("u32", "rotate_left(0x8000_0001u32, 4u32)"), Ok(0x18));
    assert_eq!(int("u64", "ctpop(0xF0F0u64)"), Ok(8));
    assert_eq!(int("u32", "bswap(0x1122_3344u32)"), Ok(0x4433_2211));
    assert_eq!(int("u8", "saturating_sub(3u8, 10u8)"), Ok(0));
    assert_eq!(int("bool", "add_with_overflow(255u8, 1u8).1"), Ok(1));
    assert_eq!(int("u8", "add_with_overflow(255u8, 2u8).0"), Ok(1));
    assert_eq!(int("u32", "0xF0u32 & 0x3C | 0x01 ^ 0x03"), Ok(0x32));
}

/// The pinned core (intrinsics/mod.rs): `ctlz_nonzero` and `cttz_nonzero` count as `ctlz`
/// and `cttz` do (its examples: `ctlz_nonzero(0b0001_1100_u8)` is 3, `cttz_nonzero(0b0011_1000_u8)`
/// is 3), and on 0 are undefined, which a constant refuses ("`{name}` called on 0", rustc's
/// interpret/intrinsics.rs). The control: `ctlz(0)` is defined, the width. Before (tempo
/// t1935d, ruint's `LowerHex`): "ctlz_nonzero is not evaluated yet".
#[test]
fn nonzero_bit_counts() {
    assert_eq!(int("u8", "ctlz_nonzero(0b0001_1100u8)"), Ok(3));
    assert_eq!(int("u8", "cttz_nonzero(0b0011_1000u8)"), Ok(3));
    assert_eq!(int("u64", "ctlz_nonzero(1u64)"), Ok(63));
    assert_eq!(int("u128", "cttz_nonzero(1u128 << 100)"), Ok(100));
    assert_eq!(refused("u32", "ctlz_nonzero(0u32)"), std::format!("{:?}", Refusal::NonzeroOnZero { name: "ctlz_nonzero" }));
    assert_eq!(refused("u16", "cttz_nonzero(0u16)"), std::format!("{:?}", Refusal::NonzeroOnZero { name: "cttz_nonzero" }));
    assert_eq!(int("u32", "ctlz(0u32)"), Ok(32));
}

/// The Reference, "Arithmetic and Logical Binary Operators": a shift by the width or more,
/// or by a negative amount, overflows; division and remainder by zero are errors.
#[test]
fn shifts_and_division_by_zero() {
    assert_eq!(int("u32", "1u32 << 31"), Ok(1 << 31));
    assert_eq!(refused("u32", "1u32 << 32"), std::format!("{:?}", Refusal::ShiftTooFar { amount: 32, bits: 32 }));
    assert_eq!(refused("i64", "1i64 >> -1i32"), std::format!("{:?}", Refusal::ShiftTooFar { amount: -1, bits: 64 }));
    assert_eq!(int("i16", "-16i16 >> 2").map(|v| sext(v, 16)), Ok(-4), "`>>` of a signed integer is arithmetic");
    assert_eq!(refused("u8", "1u8 / 0"), std::format!("{:?}", Refusal::DivByZero));
    assert_eq!(refused("i32", "5i32 % 0"), std::format!("{:?}", Refusal::RemByZero));
}

/// The pinned core's `HALF_USIZE` (library/core/src/num/niche_types.rs:113 "const HALF_USIZE:
/// usize = usize::MAX >> 1;", the range end of :129 "pub struct UsizeNoHighBit(usize is
/// 0..=HALF_USIZE);") over `usize::MAX` (library/core/src/num/uint_macros.rs, in `impl usize`:
/// "pub const MAX: Self = !0;"). The derivation on a 64-bit target: `0` at `usize` is 64 zero
/// bits; `!` on an integer is bitwise NOT (the Reference, "Negation operators"), so `!0` is 64
/// one bits = 2^64 - 1 = 18446744073709551615; `>>` on an unsigned integer is a logical shift
/// (the Reference, "Arithmetic and Logical Binary Operators"), so `>> 1` is 2^63 - 1 =
/// 9223372036854775807; `HALF_USIZE - 1` (niche_types.rs:114) is 9223372036854775806. Each
/// constant READS the one before it (a constant's value asked of the world, not its text).
/// The fixture world has no impl blocks, so core's associated `usize::MAX` is the free `MAX`
/// here with core's own body; the path through `impl usize` is blaze-ladder's
/// tests/front2_items.rs `a_pattern_type_range_end_is_a_constant_items_value`. Overflow stays an
/// error in a constant (the Reference, "Constant evaluation", with "Arithmetic and Logical
/// Binary Operators", "Overflow": "these operations panic when overflow occurs" and a panic
/// during constant evaluation is a compile error): `MAX + 1` is refused, never wrapped to 0.
/// Expected: "test tests::half_usize_is_usize_max_shifted_right_once ... ok".
#[test]
fn half_usize_is_usize_max_shifted_right_once() {
    let mut w = world();
    w.konst("MAX", "usize", "!0");
    w.konst("HALF_USIZE", "usize", "MAX >> 1");
    w.konst("HALF_USIZE_MINUS_ONE", "usize", "HALF_USIZE - 1");
    w.konst("PAST", "usize", "MAX + 1");
    w.konst("I8_MIN", "i8", "!0 << 7");
    w.build();
    assert_eq!(eval_int(&mut w, "MAX"), Ok(18_446_744_073_709_551_615));
    assert_eq!(eval_int(&mut w, "HALF_USIZE"), Ok(9_223_372_036_854_775_807));
    assert_eq!(eval_int(&mut w, "HALF_USIZE_MINUS_ONE"), Ok(9_223_372_036_854_775_806));
    assert_eq!(refusal(&mut w, "PAST"), std::format!("{:?}", Refusal::Overflow { op: Op::Add }));
    // At a signed width: `!0` at `i8` is -1 (eight one bits); `<< 7` keeps the low eight bits
    // of the shifted value: 0x80, which is `i8::MIN` (-128).
    assert_eq!(eval_int(&mut w, "I8_MIN").map(|v| sext(v, 8)), Ok(-128));
}

/// The Reference, "Type cast expressions": integer to integer truncates or extends by the
/// source's sign; float to integer rounds toward zero and saturates, NaN to 0; integer to
/// float rounds to nearest; `u8` to `char`; `char` and `bool` to integers.
#[test]
fn casts() {
    assert_eq!(int("u8", "-1i8 as u8"), Ok(255));
    assert_eq!(int("u8", "300i32 as u8"), Ok(44));
    assert_eq!(int("i64", "-1i32 as i64").map(|v| sext(v, 64)), Ok(-1));
    assert_eq!(int("u64", "-1i32 as u32 as u64"), Ok(0xFFFF_FFFF));
    assert_eq!(int("u8", "3.9f64 as u8"), Ok(3));
    assert_eq!(int("u8", "-1.0f64 as u8"), Ok(0));
    assert_eq!(int("i32", "1e10f64 as i32"), Ok(i32::MAX as u128));
    assert_eq!(int("i32", "(0.0f64 / 0.0) as i32"), Ok(0));
    assert_eq!(int("char", "97u8 as char"), Ok(97));
    assert_eq!(int("u32", "'\\u{1F600}' as u32"), Ok(0x1F600));
    assert_eq!(int("u8", "true as u8"), Ok(1));
    assert_eq!(int("f32", "255u8 as f32"), Ok(u128::from(255f32.to_bits())));
    assert_eq!(int("f64", "-1i32 as f64"), Ok(u128::from((-1f64).to_bits())));
    assert_eq!(int("f32", "0.1f64 as f32"), Ok(u128::from(0.1f32.to_bits())));
    assert_eq!(int("f64", "0.5 + 0.25 * 2.0"), Ok(u128::from(1.0f64.to_bits())));
}

/// The Reference, "Struct expressions", "Tuple expressions", "Array expressions", "Field
/// access", "Array and slice indexing" and "Match expressions": aggregates are built and read
/// by field, index and pattern; an index past the length is an error.
#[test]
fn aggregates_built_and_read() {
    let mut w = world();
    w.strukt("P", &[("x", "i32"), ("y", "i32")]);
    w.enum_("Shape", None, &[("Circle", &["u32"], None), ("Rect", &["u32", "u32"], None), ("Empty", &[], None)]);
    w.konst("S", "i32", "{ let p = P { x: 3, y: 4 }; p.x * 10 + p.y }");
    w.konst("T", "u16", "(1u8, 300u16).1");
    w.konst("A", "u8", "[1u8, 2, 3][2]");
    w.konst("R", "u32", "[7u32; 5][4]");
    w.konst(
        "M",
        "u32",
        "match Shape::Rect(2, 3) { Shape::Circle(r) => r, Shape::Rect(a, b) => a * b, Shape::Empty => 0 }",
    );
    w.konst("E", "u32", "match Shape::Empty { Shape::Circle(r) => r, Shape::Rect(_, _) => 1, _ => 9 }");
    w.konst("G", "u32", "match (3u32, 4u32) { (a, b) if a > b => a, (_, b) => b }");
    w.konst("F", "u8", "{ let mut p = P { x: 1, y: 2 }; p.y = 40; let q = P { x: 2, ..p }; q.y as u8 + q.x as u8 }");
    w.konst("O", "u8", "{ let a = [1u8, 2]; let i = 2usize; a[i] }");
    w.konst("PS", "P", "P { x: -1, y: 2 }");
    w.build();
    assert_eq!(eval_int(&mut w, "S"), Ok(34));
    assert_eq!(eval_int(&mut w, "T"), Ok(300));
    assert_eq!(eval_int(&mut w, "A"), Ok(3));
    assert_eq!(eval_int(&mut w, "R"), Ok(7));
    assert_eq!(eval_int(&mut w, "M"), Ok(6));
    assert_eq!(eval_int(&mut w, "E"), Ok(9));
    assert_eq!(eval_int(&mut w, "G"), Ok(4));
    assert_eq!(eval_int(&mut w, "F"), Ok(42));
    assert_eq!(refusal(&mut w, "O"), std::format!("{:?}", Refusal::IndexOutOfBounds { index: 2, len: 2 }));
    let ps = w.eval("PS").expect("PS");
    assert_eq!(ps.data.allocs[0].bytes.as_slice(), &[0xFF, 0xFF, 0xFF, 0xFF, 2, 0, 0, 0], "a struct's record is its bytes in layout order");
}

/// The Reference, "Constant items": a constant's value is copied where it is used; a
/// reference to a constant's value points into its record ("constant promotion"), and a read
/// through it gives the value.
#[test]
fn references_into_constants() {
    let mut w = world();
    w.konst("A", "[u8; 3]", "[5, 6, 7]");
    w.konst("R", "&[u8; 3]", "&A");
    w.konst("C", "u8", "R[1]");
    w.konst("S", "&u8", "&A[2]");
    w.build();
    assert_eq!(eval_int(&mut w, "C"), Ok(6));
    let r = w.eval("R").expect("R");
    assert_eq!(r.data.allocs.len(), 2, "the reference's target travels with it");
    assert_eq!(r.data.allocs[0].relocs[0].target, DataTarget::Local(1));
    assert_eq!(r.data.allocs[1].bytes.as_slice(), &[5, 6, 7]);
    let s = w.eval("S").expect("S");
    assert_eq!(s.data.allocs[0].bytes.as_slice(), &[2, 0, 0, 0, 0, 0, 0, 0], "the addend is the offset into the target");
}

/// The Reference, "Literal expressions", "C string literal expressions": a C string literal is
/// a `&'static CStr` over its bytes "followed by a nul byte"; "Tokens", "C string and raw C
/// string literals": `\xNN` is that byte (past ASCII too, unlike a `str`), a raw one keeps its
/// `\`, and "it is an error for a C string literal to contain a NUL", which is refused naming
/// the NUL's offset in the literal at the literal's range. Before the fix `lit::bytes` refused
/// every C string (`LitError::CStr`).
#[test]
fn c_string_literals_end_in_one_nul() {
    let mut w = world();
    let cstr = w.strukt("CStr", &[("inner", "[i8]")]);
    w.fx.lang(blaze_typeck::LangItem::CStr, cstr);
    w.konst("C", "&CStr", "c\"hi\"");
    w.konst("X", "&CStr", "c\"\\xff\"");
    w.konst("R", "&CStr", "cr#\"a\\b\"#");
    w.konst("N", "&CStr", "c\"a\\0b\"");
    w.build();
    for (name, want) in [("C", &b"hi\0"[..]), ("X", &b"\xff\0"[..]), ("R", &b"a\\b\0"[..])] {
        let r = w.eval(name).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(r.data.allocs.len(), 2, "{name}: the pointer and its bytes");
        assert_eq!(r.data.allocs[1].bytes.as_slice(), want, "{name}");
        let len = u64::from_le_bytes(r.data.allocs[0].bytes[8..16].try_into().expect("a wide pointer"));
        assert_eq!(len, want.len() as u64, "{name}: the metadata counts the NUL");
    }
    let e = w.eval("N").expect_err("an interior NUL");
    assert_eq!(e.refusal, Refusal::Literal(crate::LitError::InteriorNul { at: 3 }));
    let at = e.at.expect("the literal's range");
    assert_eq!(at.range.end - at.range.start, "c\"a\\0b\"".len() as u32, "{at:?}");
}

/// The Reference, "Constant evaluation": calls to `const fn`s are evaluated, recursion
/// included, within the evaluator's bounds; past the call-depth bound the constant is refused
/// naming the bound, never a stack overflow of blaze.
#[test]
fn const_fn_recursion_up_to_the_bound() {
    let mut w = world();
    w.const_fn("fact", &[("n", "u64")], "u64", "{ if n == 0 { 1 } else { n * fact(n - 1) } }");
    w.const_fn("down", &[("n", "u32")], "u32", "{ if n == 0 { 0 } else { down(n - 1) } }");
    w.const_fn("spin", &[], "u32", "{ loop {} }");
    w.const_fn("fib", &[("n", "u32")], "u64", "{ let mut a = 0u64; let mut b = 1u64; let mut i = 0; while i < n { let t = a + b; a = b; b = t; i += 1; } return a; }");
    w.konst("F20", "u64", "fact(20)");
    w.konst("F21", "u64", "fact(21)");
    w.konst("D500", "u32", "down(500)");
    w.konst("D600", "u32", "down(600)");
    w.konst("SPIN", "u32", "spin()");
    w.konst("FIB", "u64", "fib(90)");
    w.limits.steps = 200_000;
    w.build();
    assert_eq!(eval_int(&mut w, "F20"), Ok(2_432_902_008_176_640_000));
    assert_eq!(eval_int(&mut w, "F21"), Err(std::format!("{:?}", Refusal::Overflow { op: Op::Mul })), "21! overflows u64");
    assert_eq!(eval_int(&mut w, "D500"), Ok(0));
    assert_eq!(eval_int(&mut w, "D600"), Err(std::format!("{:?}", Refusal::CallDepth { limit: 512 })));
    assert_eq!(eval_int(&mut w, "SPIN"), Err(std::format!("{:?}", Refusal::StepLimit { limit: 200_000 })));
    assert_eq!(eval_int(&mut w, "FIB"), Ok(2_880_067_194_370_816_120));
    let e = w.eval("SPIN").expect_err("spin");
    assert_eq!(e.constant, w.key("SPIN"), "the refusal names the constant");
}

/// A const generic parameter is a value in a constant of the item it belongs to (the
/// Reference, "Const generics": "const parameters may be used ... as a standalone
/// expression"; "Associated constants": evaluated after monomorphization): `CAP` in
/// `const LAP: usize = CAP + 1;` of `impl<T, const CAP: usize> SegQueue<T, CAP>` is the
/// instance's argument, so each instance has its own value. A version that refuses the path
/// (`a path with no definition`) or reads one instance's argument for another fails here.
/// Prints `test tests::const_param_is_the_instance_argument ... ok`.
#[test]
fn const_param_is_the_instance_argument() {
    let mut w = world();
    w.generic_konst("LAP", "usize", ("CAP", "usize"), "CAP + 1");
    w.generic_konst("WIDE", "usize", ("CAP", "usize"), "if CAP > 8 { CAP * 2 } else { 0 }");
    w.build();
    let at = |w: &mut World, name: &str, cap: u64| w.eval_at(name, &[cap]).map(int_of).map_err(|e| std::format!("{:?}", e.refusal));
    assert_eq!(at(&mut w, "LAP", 31), Ok(32));
    assert_eq!(at(&mut w, "LAP", 7), Ok(8), "another instance, another value");
    assert_eq!(at(&mut w, "WIDE", 31), Ok(62));
    assert_eq!(at(&mut w, "WIDE", 3), Ok(0));
}

/// Pointers in constants (the Reference, "Constant evaluation": comparison operators only on
/// integers, floats, `bool` and `char`; `ptr_guaranteed_cmp`'s contract: 1 guaranteed equal,
/// 0 guaranteed unequal, 2 unknown, never wrong). `==` is refused by name; the intrinsic
/// answers exactly when every run of the program agrees, and core's `is_null` (written as core
/// writes it) is false for a pointer made from a reference.
#[test]
fn pointers_compare_only_when_certain() {
    let mut w = world();
    let p = w.fx.ty(PNode::Param(0));
    let ptr = w.fx.ty(PNode::Ptr(blaze_typeck::Mutbl::Not, p));
    let u8_ = w.ty("u8");
    w.intrinsic("ptr_guaranteed_cmp", 1, &[ptr, ptr], u8_, Intrinsic::PtrGuaranteedCmp);
    w.panic_fn("panic_null_unknown", "&str");
    w.const_fn(
        "is_null",
        &[("p", "*const u32")],
        "bool",
        "{ match ptr_guaranteed_cmp(p, 0 as *const u32) { 0 => false, 1 => true, _ => panic_null_unknown(\"nullness not known\") } }",
    );
    w.konst("K", "[u32; 2]", "[1, 2]");
    w.stat("S", "[u32; 2]", "[3, 4]");
    w.stat("T", "u32", "5");
    let cases: [(&str, &str, u128); 9] = [
        ("SAME", "ptr_guaranteed_cmp(&S[0] as *const u32, &S[0] as *const u32)", 1),
        ("WITHIN", "ptr_guaranteed_cmp(&S[0] as *const u32, &S[1] as *const u32)", 0),
        ("STATICS", "ptr_guaranteed_cmp(&S[1] as *const u32, &T as *const u32)", 0),
        ("ONE_CONST", "{ let r = &K; ptr_guaranteed_cmp(&r[0] as *const u32, &r[1] as *const u32) }", 0),
        // Two reads of a constant are two temporaries, which the program may merge.
        ("TWO_TEMPS", "ptr_guaranteed_cmp(&K[0] as *const u32, &K[0] as *const u32)", 2),
        ("LOCALS", "{ let a = 1u32; let b = 2u32; ptr_guaranteed_cmp(&a as *const u32, &b as *const u32) }", 0),
        ("NOT_NULL", "ptr_guaranteed_cmp(&T as *const u32, 0 as *const u32)", 0),
        ("NULLS", "ptr_guaranteed_cmp(0 as *const u32, 0 as *const u32)", 1),
        ("IS_NULL", "is_null(&K[1] as *const u32) as u8", 0),
    ];
    for (name, body, _) in cases {
        w.konst(name, "u8", body);
    }
    w.konst("EQ", "bool", "(&T as *const u32) == (&T as *const u32)");
    w.build();
    for (name, _, want) in cases {
        assert_eq!(eval_int(&mut w, name), Ok(want), "{name}");
    }
    assert_eq!(refusal(&mut w, "EQ"), std::format!("{:?}", Refusal::PointerCompare));
}

/// Rule 18: each hostile constant stops by its own bound, under the default limits but the
/// steps (lowered so the test is quick), and the refusal names the constant it was asked for.
#[test]
fn hostile_constants_stop_by_their_bound() {
    let mut w = world();
    w.const_fn("forever", &[("n", "u64")], "u64", "{ forever(n + 1) }");
    w.konst("INF_REC", "u64", "forever(0)");
    w.konst("INF_LOOP", "u32", "{ let mut i = 0u32; loop { i ^= 1; } }");
    // 1 GiB: refused by the memory bound before a byte of it is allocated.
    w.konst("HUGE", "[u8; 1073741824]", "[0u8; 1073741824]");
    // Reads itself through a reference: a cycle, not a hang.
    w.konst("SELF_REF", "&u32", "&(*SELF_REF + 1)");
    w.limits.steps = 1_000_000;
    w.build();
    let cases = [
        ("INF_REC", std::format!("{:?}", Refusal::CallDepth { limit: Limits::DEFAULT.depth })),
        ("INF_LOOP", std::format!("{:?}", Refusal::StepLimit { limit: 1_000_000 })),
        ("HUGE", std::format!("{:?}", Refusal::MemoryLimit { limit: Limits::DEFAULT.memory })),
    ];
    for (name, want) in cases {
        let e = w.eval(name).expect_err(name);
        assert_eq!(std::format!("{:?}", e.refusal), want, "{name}");
        assert_eq!(e.constant, w.key(name), "{name}: the refusal names the constant");
    }
    let me = w.key("SELF_REF");
    let e = w.eval("SELF_REF").expect_err("SELF_REF");
    assert_eq!(std::format!("{:?}", e.refusal), std::format!("{:?}", Refusal::Cycle { chain: { let mut v = blaze_own::Vec::new_in(blaze_own::testing::heap().alloc()); v.extend_from_slice(&[me, me]); v } }));
    assert_eq!(e.constant, me);
}

/// A static's initialiser is a constant context (the Reference, "Static items"): a table
/// built by a loop, as real libraries build CRC and lookup tables.
#[test]
fn static_table_built_by_a_loop() {
    let mut w = world();
    w.stat(
        "CRC",
        "[u32; 256]",
        "{ let mut t = [0u32; 256]; let mut i = 0; while i < 256 { let mut c = i as u32; let mut k = 0; while k < 8 { c = if c & 1 == 1 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 }; k += 1; } t[i] = c; i += 1; } t }",
    );
    w.konst("AT", "u32", "CRC[255]");
    w.build();
    let rec = w.eval("CRC").expect("CRC");
    let bytes = rec.data.allocs[0].bytes.as_slice();
    let mut want = [0u32; 256];
    for (i, slot) in want.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 == 1 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *slot = c;
    }
    for (i, w4) in bytes.chunks(4).enumerate() {
        assert_eq!(u32::from_le_bytes(w4.try_into().expect("4")), want[i], "entry {i}");
    }
    assert_eq!(eval_int(&mut w, "AT"), Ok(u128::from(want[255])), "a constant reads a static's record");
}

/// The Reference, "Static items": a static "may contain interior mutability", so the
/// allocations of its record that are written in place are exactly those whose type is not
/// `Freeze` (an `UnsafeCell` held by value, through fields, tuples, arrays and enum payloads),
/// and each allocation it points to is decided by its pointee type (`&AtomicU8::new(0)` in a
/// static is its own written allocation). A plain value, a slice of plain bytes and a constant
/// (copied from, never written) stay read-only. Removing the cause (the cell marked as a cell)
/// makes every mark false: the last assertion.
#[test]
fn a_static_is_written_in_place_exactly_where_its_type_is_not_freeze() {
    let build = |cells: bool| {
        let mut w = world();
        let cell = w.strukt("UnsafeCell", &[("value", "u8")]);
        let wide = w.strukt("UnsafeCellUsize", &[("value", "usize")]);
        if cells {
            w.unsafe_cell_is(cell);
            w.unsafe_cell_is(wide);
        }
        w.strukt("AtomicU8", &[("v", "UnsafeCell")]);
        w.strukt("AtomicUsize", &[("v", "UnsafeCellUsize")]);
        w.strukt("Cell", &[("value", "UnsafeCell")]);
        w.enum_("Slot", None, &[("Empty", &[], None), ("Full", &["UnsafeCell"], None)]);
        w.stat("A", "AtomicU8", "AtomicU8 { v: UnsafeCell { value: 0 } }");
        w.stat("B", "(u8, Cell)", "(1, Cell { value: UnsafeCell { value: 2 } })");
        w.stat("C", "[AtomicUsize; 2]", "[AtomicUsize { v: UnsafeCellUsize { value: 3 } }, AtomicUsize { v: UnsafeCellUsize { value: 4 } }]");
        w.stat("S", "Slot", "Slot::Full(UnsafeCell { value: 5 })");
        w.stat("D", "&AtomicU8", "&AtomicU8 { v: UnsafeCell { value: 6 } }");
        w.stat("E", "u32", "7");
        w.stat("F", "&[u8]", "&[1, 2]");
        w.stat("G", "&(u8, &AtomicU8)", "&(8, &AtomicU8 { v: UnsafeCell { value: 9 } })");
        w.konst("K", "AtomicU8", "AtomicU8 { v: UnsafeCell { value: 0 } }");
        w.build();
        w
    };
    let marks = |w: &mut World, name: &str| -> StdVec<bool> { w.eval(name).expect(name).data.allocs.iter().map(|a| a.mutable).collect() };
    let mut w = build(true);
    let want: [(&str, &[bool]); 9] = [
        ("A", &[true]),
        ("B", &[true]),
        ("C", &[true]),
        ("S", &[true]),
        ("D", &[false, true]),
        ("E", &[false]),
        ("F", &[false, false]),
        ("G", &[false, false, true]),
        ("K", &[false]),
    ];
    for (name, marked) in want {
        assert_eq!(marks(&mut w, name), marked, "{name}");
    }
    let mut plain = build(false);
    for (name, marked) in want {
        assert!(marks(&mut plain, name).iter().all(|m| !m), "{name}: no cell, nothing written in place (was {marked:?})");
    }
}

/// The Reference, "Enumerations", "Discriminants": an explicit discriminant is its constant's
/// value; an implicit one is the previous variant's plus one, the first's 0; in the `repr`
/// type, `isize` without one; past the type's range is an error. `as` reads it.
#[test]
fn discriminants() {
    let mut w = world();
    let e = w.enum_("E", Some("i8"), &[("A", &[], Some("-2")), ("B", &[], None), ("C", &[], Some("10")), ("D", &[], None)]);
    let plain = w.enum_("Plain", None, &[("X", &[], None), ("Y", &[], None), ("Z", &[], None)]);
    let over = w.enum_("Over", Some("u8"), &[("A", &[], Some("255")), ("B", &[], None)]);
    w.konst("CB", "i32", "E::B as i32");
    w.konst("CD", "u8", "E::D as u8");
    w.konst("PZ", "i64", "Plain::Z as i64");
    w.build();
    // The explicit discriminants' records first: the seam returns `Need` until they exist.
    let alloc = blaze_own::testing::heap().alloc();
    assert!(matches!(eval_discriminant(&w, e, 0, alloc), Ok(Answer::Need(_))), "an explicit discriminant not evaluated yet is a need");
    w.eval("E_A_DISC").expect("A");
    w.eval("E_C_DISC").expect("C");
    w.eval("Over_A_DISC").expect("Over A");
    let d = |w: &World, adt, v| match eval_discriminant(w, adt, v, alloc) {
        Ok(Answer::Done(d)) => Ok(d),
        Ok(Answer::Need(n)) => panic!("need {n:?}"),
        Err(e) => Err(std::format!("{:?}", e.refusal)),
    };
    assert_eq!([d(&w, e, 0), d(&w, e, 1), d(&w, e, 2), d(&w, e, 3)], [Ok(-2), Ok(-1), Ok(10), Ok(11)]);
    assert_eq!([d(&w, plain, 0), d(&w, plain, 1), d(&w, plain, 2)], [Ok(0), Ok(1), Ok(2)]);
    assert_eq!(d(&w, over, 1), Err(std::format!("{:?}", Refusal::DiscriminantOverflow { variant: 1 })));
    assert_eq!(eval_int(&mut w, "CB").map(|v| sext(v, 32)), Ok(-1));
    assert_eq!(eval_int(&mut w, "CD"), Ok(11));
    assert_eq!(eval_int(&mut w, "PZ"), Ok(2));
}

/// The Reference, "Slice types" and "Slice patterns": a reference to a constant array
/// coerces to a slice (a pointer and a length); indexing it and matching it read the array.
#[test]
fn slice_of_a_constant_array() {
    let mut w = world();
    w.konst("A", "[u16; 4]", "[1, 2, 3, 4]");
    w.konst("S", "&[u16]", "&A");
    w.konst("C", "u16", "S[3]");
    w.konst("M", "u16", "match S { [a, _, _, d] => *a + *d, _ => 0 }");
    w.konst("STR", "&str", "\"h\\u{e9}llo\"");
    w.build();
    let s = w.eval("S").expect("S");
    assert_eq!(&s.data.allocs[0].bytes[8..], &[4, 0, 0, 0, 0, 0, 0, 0], "the length after the pointer");
    assert_eq!(s.data.allocs[1].bytes.as_slice(), &[1, 0, 2, 0, 3, 0, 4, 0]);
    assert_eq!(eval_int(&mut w, "C"), Ok(4));
    assert_eq!(eval_int(&mut w, "M"), Ok(5));
    let st = w.eval("STR").expect("STR");
    assert_eq!(st.data.allocs[1].bytes.as_slice(), "h\u{e9}llo".as_bytes(), "a string literal's bytes, escapes decoded");
}

/// The Reference, "Slice patterns": with a `..`, the elements before it are at their index,
/// those after it are counted from the end, the `..` (bound by `rest @ ..`) is the part between
/// (`[T]` over a slice, `[T; n - k]` over an array), and a slice matches when it is at least as
/// long as the other elements. A version that indexed the elements after `..` from the start,
/// or tested the length for equality, gives other numbers here.
#[test]
fn slice_patterns_with_rest() {
    let mut w = world();
    w.konst("A", "[u16; 4]", "[1, 2, 3, 4]");
    w.konst("S", "&[u16]", "&A");
    w.konst("FIRST", "u16", "match S { [first, ..] => *first, _ => 0 }");
    w.konst("LAST", "u16", "match S { [.., last] => *last, _ => 0 }");
    w.konst("TAIL2", "u16", "match S { [.., y, z] => *y * 10 + *z, _ => 0 }");
    w.konst("MID", "u16", "match S { [_, rest @ .., _] => rest[0] * 10 + rest[1], _ => 0 }");
    w.konst("SHORT", "u16", "match S { [_, _, _, _, _, ..] => 1, [_, _, _, _, ..] => 2, _ => 3 }");
    w.konst("ARR", "u16", "match A { [a, rest @ .., z] => a * 100 + rest[1] * 10 + z }");
    w.build();
    assert_eq!(eval_int(&mut w, "FIRST"), Ok(1), "[first, ..]");
    assert_eq!(eval_int(&mut w, "LAST"), Ok(4), "[.., last]: the last element, not the first");
    assert_eq!(eval_int(&mut w, "TAIL2"), Ok(34), "[.., y, z]: counted from the end");
    assert_eq!(eval_int(&mut w, "MID"), Ok(23), "[_, rest @ .., _]: the subslice between");
    assert_eq!(eval_int(&mut w, "SHORT"), Ok(2), "five fixed elements do not fit four; four with `..` do");
    assert_eq!(eval_int(&mut w, "ARR"), Ok(134), "[a, rest @ .., z] over [u16; 4]: rest is [u16; 2]");
}

/// The Reference, "Constant evaluation": a panic during evaluation is a compile error whose
/// message is the panic's; here its parts.
#[test]
fn panic_in_a_constant() {
    // The Reference ("Constant evaluation"): a panic where the value must be known at compile
    // time is a compile error. The forms a constant reaches are core's const panic entries:
    // `panic!("lit")`, `assert!`, `unreachable!` and kin (`panicking::panic(&str)`) and
    // `panic!("{}", s)` of a `&str` (`panic_display`). The finding is the message, the constant
    // asked for and the call's item-relative range (rule 71), in the record that refuses.
    let body = "if 1 + 1 == 2 { panic_display(\"bad value\") } else { 1 }";
    let mut w = world();
    w.konst("C", "u8", body);
    w.panic_fn("panic", "&str");
    w.const_fn("checked", &[("n", "u8")], "u8", "{ if n > 9 { panic(\"n is more than 9\") } n }");
    w.konst("D", "u8", "checked(10)");
    w.build();
    let e = w.eval("C").expect_err("panics");
    let Refusal::Panic { parts } = &e.refusal else { panic!("{:?}", e.refusal) };
    assert!(matches!(parts.as_slice(), [PanicPart::Str(s)] if s.as_slice() == b"bad value"), "{parts:?}");
    assert_eq!(e.constant, w.key("C"));
    let at = e.at.expect("the panic's place");
    let text = std::format!("const C: u8 = {body};");
    assert_eq!(&text[at.range.start as usize..at.range.end as usize], "panic_display(\"bad value\")", "item-relative");
    // A panic in a const fn: the finding names the constant asked for, and the place is in
    // the fn's own item.
    let e = w.eval("D").expect_err("panics");
    let Refusal::Panic { parts } = &e.refusal else { panic!("{:?}", e.refusal) };
    assert!(matches!(parts.as_slice(), [PanicPart::Str(s)] if s.as_slice() == b"n is more than 9"), "{parts:?}");
    assert_eq!(e.constant, w.key("D"));
    assert_eq!(e.at.expect("the panic's place").body, w.key("checked"));
}

/// What the evaluator refuses, each by its name: what the language makes an error in a
/// constant, and what is not built.
#[test]
fn refused_by_name() {
    let mut w = world();
    w.plain_fn("not_const", &[], "u8", "{ 1 }");
    w.strukt("Pad", &[("a", "u8"), ("b", "u16")]);
    w.const_fn("dangle", &[], "&u8", "{ let x = 5u8; &x }");
    w.konst("NC", "u8", "not_const()");
    w.stat_mut("SM", "u8", "1");
    w.konst("RSM", "u8", "unsafe { SM }");
    w.konst("A", "u8", "1");
    w.konst("B", "u8", "2");
    w.konst("PC", "bool", "&A as *const u8 == &B as *const u8");
    w.konst("PI", "usize", "&A as *const u8 as usize");
    w.konst("UN", "u32", "transmute::<Pad, u32>(Pad { a: 1, b: 2 })");
    w.konst("DG", "u8", "*dangle()");
    w.konst("IP", "u8", "*transmute::<usize, &u8>(8)");
    w.konst("TS", "u16", "transmute::<u8, u16>(1)");
    w.konst("IB", "bool", "transmute::<u8, bool>(2)");
    w.konst("IC", "char", "transmute::<u32, char>(0xD800)");
    w.konst("CY1", "u32", "CY2 + 1");
    w.konst("CY2", "u32", "CY1 + 1");
    w.konst("MEM", "[u64; 1000000]", "[0u64; 1000000]");
    w.build();
    let nc = w.key("not_const");
    assert_eq!(refusal(&mut w, "NC"), std::format!("{:?}", Refusal::NotConstFn { item: nc }));
    let sm = w.key("SM");
    assert_eq!(refusal(&mut w, "RSM"), std::format!("{:?}", Refusal::MutableStatic { item: sm }));
    assert_eq!(refusal(&mut w, "PC"), std::format!("{:?}", Refusal::PointerCompare));
    assert_eq!(refusal(&mut w, "PI"), std::format!("{:?}", Refusal::PointerAsInt));
    // repr(Rust) as blaze-build lays it out: `b: u16` at 0, `a: u8` at 2, padding at 3.
    assert_eq!(refusal(&mut w, "UN"), std::format!("{:?}", Refusal::Uninit { offset: 3 }));
    assert_eq!(refusal(&mut w, "DG"), std::format!("{:?}", Refusal::Dangling));
    assert_eq!(refusal(&mut w, "IP"), std::format!("{:?}", Refusal::IntAsPointer));
    assert_eq!(refusal(&mut w, "TS"), std::format!("{:?}", Refusal::TransmuteSize { from: 1, to: 2 }));
    assert_eq!(refusal(&mut w, "IB"), std::format!("{:?}", Refusal::InvalidBool { value: 2 }));
    assert_eq!(refusal(&mut w, "IC"), std::format!("{:?}", Refusal::InvalidChar { value: 0xD800 }));
    let (c1, c2) = (w.key("CY1"), w.key("CY2"));
    assert_eq!(refusal(&mut w, "CY1"), std::format!("{:?}", Refusal::Cycle { chain: { let mut v = blaze_own::Vec::new_in(blaze_own::testing::heap().alloc()); v.extend_from_slice(&[c1, c2, c1]); v } }));
    w.limits.memory = 1 << 20;
    assert_eq!(refusal(&mut w, "MEM"), std::format!("{:?}", Refusal::MemoryLimit { limit: 1 << 20 }));
}

/// `size_of` and `align_of` answer from the layout; `transmute` of plain data reinterprets
/// the bytes.
#[test]
fn size_align_and_transmute() {
    let mut w = world();
    w.strukt("Pad", &[("a", "u8"), ("b", "u16")]);
    w.konst("SZ", "usize", "size_of::<Pad>() * 100 + align_of::<u64>()");
    w.konst("BITS", "u32", "transmute::<f32, u32>(1.0)");
    w.konst("ARR", "u32", "transmute::<[u8; 4], u32>([1, 2, 3, 4])");
    w.build();
    assert_eq!(eval_int(&mut w, "SZ"), Ok(408));
    assert_eq!(eval_int(&mut w, "BITS"), Ok(u128::from(1f32.to_bits())));
    assert_eq!(eval_int(&mut w, "ARR"), Ok(0x0403_0201));
}

/// The constants the hosted hello's 67 functions reach (the ladder's
/// `port27/hosted-hello-functions.txt`, program side), each with its body as `bstd` and core
/// write it. The fixture world has no methods, impl selection or generics, so a method is a
/// free `const fn`, an associated constant a free one, and a generic call its instance; what
/// each computes is the source's. Not here: `Duration::MAX` (`MAIN_UNBOUNDED`,
/// `Limits::USUAL`) and `NonZero::<usize>::MIN`, whose types hold a pattern type that no
/// checker type can name yet (consteval.md, "The hosted hello's constants").
#[test]
fn hosted_hello_constants() {
    let mut w = world();
    let fx = &mut w.fx;
    let usize_ = fx.usize();
    let unit = fx.tuple(&[]);
    let bytes_ptr = w.ty("*const [u8]");
    let thin = w.ty("*const ()");
    let byte_ptr = w.ty("*const u8");
    // core's `ptr::metadata` and `ptr::from_raw_parts`, at the instances `<[u8]>::len` and
    // `ptr::null::<u8>` call.
    w.intrinsic("ptr_metadata", 0, &[bytes_ptr], usize_, Intrinsic::PtrMetadata);
    w.intrinsic("aggregate_raw_ptr", 0, &[thin, unit], byte_ptr, Intrinsic::AggregateRawPtr);
    // bstd::io::Offers (five of its capabilities), `Offers::NONE`, bstd::guest::OFFERS.
    w.strukt("Offers", &[("out", "bool"), ("err", "bool"), ("clock", "bool"), ("args", "bool"), ("rng", "bool"), ("fs", "bool")]);
    w.konst("NONE", "Offers", "Offers { out: false, err: false, clock: false, args: false, rng: false, fs: false }");
    w.konst(
        "OFFERS",
        "Offers",
        "{ let mut offers = NONE; offers.out = true; offers.err = true; offers.clock = true; offers.args = true; offers.rng = true; offers }",
    );
    w.konst("OFFERS_FS", "bool", "OFFERS.fs");
    w.konst("OFFERS_RNG", "bool", "OFFERS.rng");
    // bstd::grant::Kind, `Kind::name`, `<Io as Grant>::KIND`.
    w.enum_(
        "Kind",
        None,
        &[
            ("Args", &[], None),
            ("Out", &[], None),
            ("Clock", &[], None),
            ("Mem", &[], None),
            ("Spec", &[], None),
            ("Dir", &["bool"], None),
            ("Net", &[], None),
            ("Env", &[], None),
            ("Everything", &[], None),
            ("Whole", &["&str"], None),
        ],
    );
    w.const_fn(
        "kind_name",
        &[("k", "Kind")],
        "&str",
        "{ match k { Kind::Args => \"args\", Kind::Out => \"out\", Kind::Clock => \"clock\", Kind::Mem => \"mem\", Kind::Spec => \"spec\", Kind::Dir(_) => \"dir\", Kind::Net => \"net\", Kind::Env => \"env\", Kind::Everything => \"everything\", Kind::Whole(_) => \"whole\" } }",
    );
    w.konst("IO_KIND", "Kind", "Kind::Everything");
    w.konst("IO_KIND_NAME", "&str", "kind_name(IO_KIND)");
    // core's `str::as_bytes`, `<[u8]>::len`, `u8::is_ascii_digit`, `u8::is_ascii_alphanumeric`;
    // bstd::spec::is_name, bstd::grant::same, `Param::new`, the `static MANIFEST` that
    // `#[bstd::main]` makes for `main(cx, io: Io)`.
    w.const_fn("as_bytes", &[("s", "&str")], "&[u8]", "{ transmute::<&str, &[u8]>(s) }");
    w.const_fn("len", &[("s", "&[u8]")], "usize", "{ ptr_metadata(s as *const [u8]) }");
    w.const_fn("is_ascii_digit", &[("b", "u8")], "bool", "{ match b { b'0'..=b'9' => true, _ => false } }");
    w.const_fn(
        "is_ascii_alphanumeric",
        &[("b", "u8")],
        "bool",
        "{ (match b { b'0'..=b'9' => true, _ => false }) | (match b { b'A'..=b'Z' => true, _ => false }) | (match b { b'a'..=b'z' => true, _ => false }) }",
    );
    w.const_fn(
        "is_name",
        &[("raw", "&str")],
        "bool",
        "{ let bytes = as_bytes(raw); if len(bytes) == 0 || len(bytes) > 64 || is_ascii_digit(bytes[0]) { return false; } let mut at = 0; while at < len(bytes) { if !(is_ascii_alphanumeric(bytes[at]) || bytes[at] == b'_') { return false; } at += 1; } true }",
    );
    w.const_fn(
        "same",
        &[("a", "&str"), ("b", "&str")],
        "bool",
        "{ let (a, b) = (as_bytes(a), as_bytes(b)); if len(a) != len(b) { return false; } let mut at = 0; while at < len(a) { if a[at] != b[at] { return false; } at += 1; } true }",
    );
    w.strukt("Param", &[("name", "&str"), ("kind", "Kind")]);
    w.const_fn(
        "param_new",
        &[("name", "&str"), ("kind", "Kind")],
        "Param",
        "{ if (match kind { Kind::Out => true, _ => false }) && !same(name, \"out\") && !same(name, \"err\") { panic_display(\"an `Out` parameter of `main` is named `out` (standard output) or `err` (standard error)\"); } if !is_name(name) { panic_display(\"a parameter of `main` is named by ASCII letters, digits and `_`, at most 64 bytes\"); } Param { name, kind } }",
    );
    w.strukt("Manifest", &[("params", "&[Param]")]);
    w.stat("MANIFEST", "Manifest", "Manifest { params: &[param_new(\"io\", IO_KIND)] }");
    w.konst("BAD_PARAM", "Param", "param_new(\"1o\", Kind::Everything)");
    w.konst("BAD_OUT", "Param", "param_new(\"stdout\", Kind::Out)");
    // bstd::wire::HostTable (`#[repr(C)]`): its six `extern "C" fn` entries stand here as
    // `*const u8`, of the same size and alignment; `checked` compares against its size.
    w.strukt(
        "HostTable",
        &[
            ("version", "u32"),
            ("size", "u32"),
            ("host", "*const u8"),
            ("exchange", "*const u8"),
            ("exchange_len", "usize"),
            ("write", "*const u8"),
            ("clock", "*const u8"),
            ("arg", "*const u8"),
            ("grant", "*const u8"),
            ("effect", "*const u8"),
            ("exit", "*const u8"),
        ],
    );
    w.konst("TABLE_SIZE", "usize", "size_of::<HostTable>()");
    // bstd::out::HELD and the `Held` buffer's `[0; HELD]`.
    w.konst("HELD", "usize", "1024");
    // The fixture's converter takes literal lengths only (a length naming `HELD` is an
    // anonymous constant, `eval_len`'s, in the driver).
    w.konst("HELD_BYTES", "[u8; 1024]", "[0; 1024]");
    w.konst("TAKE", "usize", "{ let len = 1000usize; if len == HELD { 0 } else { HELD - len } }");
    // core's `ptr::null::<u8>()` (`from_raw_parts(without_provenance::<()>(0), ())`), and
    // bstd::guest's `static CURRENT: Cell<*const Run> = Cell::new(ptr::null())`.
    w.const_fn("without_provenance", &[("addr", "usize")], "*const ()", "{ transmute::<usize, *const ()>(addr) }");
    w.const_fn("null", &[], "*const u8", "{ aggregate_raw_ptr(without_provenance(0), ()) }");
    w.strukt("Cell", &[("value", "*const u8")]);
    w.const_fn("cell_new", &[("value", "*const u8")], "Cell", "{ Cell { value } }");
    w.stat("CURRENT", "Cell", "cell_new(null())");
    w.build();

    assert_eq!(eval_int(&mut w, "OFFERS_FS"), Ok(0));
    assert_eq!(eval_int(&mut w, "OFFERS_RNG"), Ok(1));
    let name = w.eval("IO_KIND_NAME").expect("IO_KIND_NAME");
    assert_eq!(name.data.allocs[1].bytes.as_slice(), b"everything", "`<Io as Grant>::KIND.name()`");
    let manifest = w.eval("MANIFEST").expect("MANIFEST");
    assert_eq!(&manifest.data.allocs[0].bytes[8..], &[1, 0, 0, 0, 0, 0, 0, 0], "one parameter");
    let message = |w: &mut World, name: &str| -> StdVec<u8> {
        let e = w.eval(name).expect_err("panics");
        let Refusal::Panic { parts } = &e.refusal else { panic!("{:?}", e.refusal) };
        let [PanicPart::Str(s)] = parts.as_slice() else { panic!("{parts:?}") };
        s.as_slice().to_vec()
    };
    assert!(message(&mut w, "BAD_PARAM").starts_with(b"a parameter of `main` is named by ASCII"), "a name starting with a digit: `Param::new`'s message");
    assert!(message(&mut w, "BAD_OUT").starts_with(b"an `Out` parameter"), "an `Out` named other than `out` or `err`");
    assert_eq!(eval_int(&mut w, "TABLE_SIZE"), Ok(80));
    let held = w.eval("HELD_BYTES").expect("HELD_BYTES");
    assert_eq!(held.data.allocs[0].bytes.len(), 1024);
    assert_eq!(eval_int(&mut w, "TAKE"), Ok(24), "`Held::take`'s room left");
    let current = w.eval("CURRENT").expect("CURRENT");
    assert_eq!(current.data.allocs[0].bytes.as_slice(), &[0; 8], "`ptr::null()` is address 0");
    assert!(current.data.allocs[0].relocs.is_empty(), "and points into nothing");
}

/// Layout cannot drift between compile time and run time: a constant's record, placed in
/// memory, is read back by code the builder emits (blaze-build's `read_back`: one emitted
/// function loading every scalar leaf through the builder's own offsets), and each leaf is the
/// value the source wrote. Three shapes: a struct repr(Rust) may reorder, an array of a
/// `repr(u8)` enum with explicit and implicit discriminants (the tag's raw bits), and a
/// reference to a tuple holding a slice (two relocations).
#[test]
fn emitted_code_reads_constants_back() {
    let mut w = world();
    w.strukt("Mixed", &[("a", "u8"), ("b", "u64"), ("c", "u16"), ("s", "&str")]);
    w.enum_("E", Some("u8"), &[("A", &[], Some("3")), ("B", &[], None), ("C", &[], Some("10"))]);
    w.konst("M", "Mixed", "Mixed { a: 7, b: 0x1122_3344_5566_7788, c: 0xBEEF, s: \"hi\" }");
    w.konst("ES", "[E; 4]", "[E::A, E::C, E::B, E::A]");
    w.konst("R", "&(u16, &[u8])", "&(5, &[1, 2, 3])");
    w.build();
    let cases: [(&str, &str, StdVec<u64>); 3] = [
        ("M", "Mixed", std::vec![7, 0x1122_3344_5566_7788, 0xBEEF, 2, u64::from(b'h'), u64::from(b'i')]),
        ("ES", "[E; 4]", std::vec![3, 10, 4, 3]),
        ("R", "&(u16, &[u8])", std::vec![5, 3, 1, 2, 3]),
    ];
    for (name, ty, want) in cases {
        let t = w.ty(ty);
        let got = w.read_back(name, t).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(got, want, "{name}: the emitted code reads what the evaluator wrote");
    }
}

/// Rule 25: two evaluations of one key write equal bytes, and the record's key and reads are
/// a function of the text.
#[test]
fn records_are_deterministic() {
    let build = || {
        let mut w = world();
        w.konst("A", "[u8; 3]", "[5, 6, 7]");
        w.konst("R", "&[u8]", "&A");
        w.build();
        let r = w.eval("R").expect("R");
        (r.key, std::format!("{r:?}"), r.reads.len())
    };
    let (a, b) = (build(), build());
    assert_eq!(a, b);
    assert_eq!(a.2, 2, "R's body and A's record (A's body is A's record's read, not R's): {a:?}");
}

// ---- The property and the controls ----

/// One drawn integer case: a type, an operator and two operands as written.
struct Case {
    ty: &'static str,
    bits: u32,
    signed: bool,
    op: &'static str,
    a: u128,
    b: u128,
}

const OPS: [&str; 10] = ["+", "-", "*", "/", "%", "<<", ">>", "&", "|", "^"];

fn draw(tape: &Tape) -> Case {
    let &(ty, bits, signed) = tape.pick(&INTS, "type").expect("types");
    let op = *tape.pick(&OPS, "op").expect("ops");
    let operand = |label| {
        let v = match tape.below(4, label) {
            0 => tape.below(8, label) as u128,
            1 => mask(u128::MAX, bits) - tape.below(4, label) as u128,
            2 => mask((1u128 << (bits - 1)).wrapping_add(tape.below(3, label) as u128).wrapping_sub(1), bits),
            _ => mask(u128::from(tape.next_u64(label)) | u128::from(tape.next_u64(label)) << 64, bits),
        };
        if op == "<<" || op == ">>" { v % u128::from(bits + 2) } else { v }
    };
    Case { ty, bits, signed, op, a: operand("a"), b: operand("b") }
}

fn written(v: u128, bits: u32, signed: bool) -> String {
    if signed && sext(v, bits) < 0 {
        // The minimum has no positive literal of its type: written as `-MAX - 1`.
        let m = sext(v, bits);
        if m == if bits >= 128 { i128::MIN } else { -(1i128 << (bits - 1)) } { std::format!("({} - 1)", m + 1) } else { std::format!("({m})") }
    } else {
        std::format!("{v}")
    }
}

/// The direct computation: the host's checked arithmetic on the type.
fn direct(c: &Case) -> Option<u128> {
    macro_rules! host {
        ($t:ty) => {{
            let (a, b) = (c.a as $t, c.b as $t);
            let r: Option<$t> = match c.op {
                "+" => a.checked_add(b),
                "-" => a.checked_sub(b),
                "*" => a.checked_mul(b),
                "/" => a.checked_div(b),
                "%" => a.checked_rem(b),
                "<<" => a.checked_shl(c.b as u32).filter(|_| c.b < u128::from(c.bits)),
                ">>" => a.checked_shr(c.b as u32).filter(|_| c.b < u128::from(c.bits)),
                "&" => Some(a & b),
                "|" => Some(a | b),
                _ => Some(a ^ b),
            };
            r.map(|r| mask(r as u128, c.bits))
        }};
    }
    match c.ty {
        "i8" => host!(i8),
        "i16" => host!(i16),
        "i32" => host!(i32),
        "i64" | "isize" => host!(i64),
        "i128" => host!(i128),
        "u8" => host!(u8),
        "u16" => host!(u16),
        "u32" => host!(u32),
        "u64" | "usize" => host!(u64),
        _ => host!(u128),
    }
}

fn case_text(c: &Case) -> String {
    let rhs_ty = if c.op == "<<" || c.op == ">>" { "u32" } else { c.ty };
    let b = if rhs_ty == "u32" { std::format!("{}", c.b) } else { written(c.b, c.bits, c.signed) };
    std::format!("{{ let a: {} = {}; let b: {rhs_ty} = {b}; a {} b }}", c.ty, written(c.a, c.bits, c.signed), c.op)
}

fn eval_case(c: &Case, plant: Plant) -> Result<u128, String> {
    let mut w = world();
    let k = w.konst("C", c.ty, &case_text(c));
    w.build();
    let alloc = blaze_own::testing::heap().alloc();
    let mut ground = PortableTys::new_in(alloc);
    let mut layout = Layouts::new_in(alloc);
    let mut codes = Codes::new_in(alloc);
    match value_of_planted(&w, &mut layout, &mut codes, k, &mut ground, plant, alloc) {
        Ok(Answer::Done(r)) => Ok(int_of(&r)),
        Ok(Answer::Need(n)) => Err(std::format!("need {n:?}")),
        Err(ConstError { refusal, .. }) => Err(std::format!("{refusal:?}")),
    }
}

fn arithmetic_oracle(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let c = draw(tape);
    let got = eval_case(&c, plant);
    let want = direct(&c);
    tape.note(format_args!("{} = {got:?}, direct {want:?}", case_text(&c)));
    match (got, want) {
        (Ok(g), Some(w)) if g == w => Ok(()),
        (Err(_), None) => Ok(()),
        (Ok(g), None) => Err(Broke::Oracle(std::format!("an overflow answered {g:#x} where the language makes it an error: {}", case_text(&c)))),
        (got, want) => Err(Broke::Oracle(std::format!("the evaluator differs from direct computation: {got:?} against {want:?}: {}", case_text(&c)))),
    }
}

/// The property: every drawn integer expression evaluates to what the host's checked
/// arithmetic computes, and is refused exactly where the host's is `None`.
#[test]
fn evaluator_equals_direct_computation() {
    blaze_sim::check(Config::cases(300), |tape: &Tape| arithmetic_oracle(tape, Plant::None));
}

/// Control: arithmetic that wraps where it must error is caught by the property's oracle.
#[test]
fn control_overflow_that_wraps_is_caught() {
    control(Config::cases(300), Plant::None, Plant::WrapOnOverflow, "where the language makes it an error", arithmetic_oracle);
}

/// Control: comparing pointers of two allocations by their offsets, instead of refusing, is
/// caught (the Reference, "Constant evaluation": pointer comparison is not a constant
/// operation; rustc refuses it).
#[test]
fn control_pointer_comparison_answered_is_caught() {
    control(Config::cases(40), Plant::None, Plant::ComparePointers, "pointer comparison answered", |tape, plant| {
        let mut w = world();
        let n = tape.below(3, "statics") as usize + 2;
        for i in 0..n {
            w.konst(&std::format!("K{i}"), "u8", &std::format!("{i}"));
        }
        let (i, j) = (tape.below(n as u64, "i"), tape.below(n as u64, "j"));
        let op = *tape.pick(&["==", "!=", "<"], "op").expect("op");
        let k = w.konst("C", "bool", &std::format!("(&K{i} as *const u8) {op} (&K{j} as *const u8)"));
        w.build();
        let alloc = blaze_own::testing::heap().alloc();
        let mut ground = PortableTys::new_in(alloc);
        let mut layout = Layouts::new_in(alloc);
        let mut codes = Codes::new_in(alloc);
        // The two constants' records first.
        for i in 0..n {
            w.eval(&std::format!("K{i}")).map_err(|e| Broke::Harness(std::format!("{e}")))?;
        }
        match value_of_planted(&w, &mut layout, &mut codes, k, &mut ground, plant, alloc) {
            Err(ConstError { refusal: Refusal::PointerCompare, .. }) => Ok(()),
            Ok(Answer::Done(r)) => Err(Broke::Oracle(std::format!("pointer comparison answered {}", int_of(&r)))),
            other => Err(Broke::Harness(std::format!("{other:?}"))),
        }
    });
}

/// Control: a discriminant taken from the wrong variant is caught by comparing every
/// variant's discriminant with the rule worked by hand.
#[test]
fn control_discriminant_of_the_wrong_variant_is_caught() {
    control(Config::cases(60), Plant::None, Plant::DiscriminantOffByOne, "wrong discriminant", |tape, plant| {
        let mut w = world();
        let n = tape.below(5, "variants") as usize + 1;
        let mut model = StdVec::new();
        let mut texts: StdVec<(String, Option<String>)> = StdVec::new();
        let mut next: i128 = 0;
        for v in 0..n {
            let explicit = tape.coin("explicit").then(|| tape.below(41, "value") as i128 - 20);
            let d = explicit.unwrap_or(next);
            model.push(d);
            next = d + 1;
            texts.push((std::format!("V{v}"), explicit.map(|e| std::format!("{e}"))));
        }
        let shaped: StdVec<(&str, &[&str], Option<&str>)> = texts.iter().map(|(n, d)| (n.as_str(), &[] as &[&str], d.as_deref())).collect();
        let e = w.enum_("E", Some("i32"), &shaped);
        w.build();
        for (v, (_, d)) in texts.iter().enumerate() {
            if d.is_some() {
                w.eval(&std::format!("E_V{v}_DISC")).map_err(|e| Broke::Harness(std::format!("{e}")))?;
            }
        }
        for (v, &want) in model.iter().enumerate() {
            match discriminant_with_plant(&w, e, v as u32, plant, blaze_own::testing::heap().alloc()) {
                Ok(Answer::Done(got)) if got == want => {}
                Ok(Answer::Done(got)) => return Err(Broke::Oracle(std::format!("wrong discriminant for variant {v}: {got}, the rule gives {want}"))),
                other => return Err(Broke::Harness(std::format!("{other:?}"))),
            }
        }
        Ok(())
    });
}

/// `int_binop` itself refuses what the language refuses, without the pipeline: the oracle's
/// own unit, for a failure that names the operator.
#[test]
fn int_binop_names_the_operator() {
    let r = int_binop(blaze_typeck::BinOp::Add, 255, 1, 8, false, (8, false), Plant::None);
    assert_eq!(r, Err(Refusal::Overflow { op: Op::Add }));
    let _: ItemKey = ItemKey(0);
}

/// Overlapping `Memory::copy` (`to > from && to < from + size`) must preserve the exact `init`
/// bitmask and relocations without clobbering source bits before reading them, and recycled
/// regions after `kill` must still report `Uninit` on unwritten bytes (zero-memset safety).
#[test]
fn overlapping_memory_copy_preserves_init_and_relocs() {
    use crate::mem::{Memory, Origin, Pointer, Scalar};
    let alloc = blaze_own::testing::heap().alloc();
    let mut mem = Memory::new_in(4096, alloc);

    // 1. Overlapping forward copy (to > from && to < from + size) with init/uninit holes.
    // Layout of 16 bytes: init at offsets 0, 2, 4; uninit at 1, 3, 5..16.
    // Copy [0..8] -> [2..10]: expected init at 2, 4, 6; uninit at 3, 5, 7, 8, 9.
    let id = mem.allocate(16, 3, true, Origin::Local).unwrap();
    let base = Pointer { alloc: id, offset: 0 };
    mem.write(base.at(0), 1, Scalar::Int(0x11)).unwrap();
    mem.write(base.at(2), 1, Scalar::Int(0x22)).unwrap();
    mem.write(base.at(4), 1, Scalar::Int(0x33)).unwrap();
    mem.copy(base.at(0), base.at(2), 8).unwrap();
    assert_eq!(mem.read(base.at(2), 1), Ok(Scalar::Int(0x11)));
    assert_eq!(mem.read(base.at(3), 1), Err(Refusal::Uninit { offset: 3 }));
    assert_eq!(mem.read(base.at(4), 1), Ok(Scalar::Int(0x22)));
    assert_eq!(mem.read(base.at(5), 1), Err(Refusal::Uninit { offset: 5 }));
    assert_eq!(mem.read(base.at(6), 1), Ok(Scalar::Int(0x33)));
    assert_eq!(mem.read(base.at(7), 1), Err(Refusal::Uninit { offset: 7 }));
    assert_eq!(mem.read(base.at(8), 1), Err(Refusal::Uninit { offset: 8 }));
    assert_eq!(mem.read(base.at(9), 1), Err(Refusal::Uninit { offset: 9 }));

    // 2. Overlapping copy with relocations + sorted reloc lookup.
    let target = mem.allocate(8, 3, false, Origin::Data).unwrap();
    let buf = mem.allocate(32, 3, true, Origin::Local).unwrap();
    let bp = Pointer { alloc: buf, offset: 0 };
    let ptr_val = Scalar::Ptr(Pointer { alloc: target, offset: 4 });
    mem.write(bp.at(0), 8, ptr_val).unwrap();
    mem.write(bp.at(16), 8, Scalar::Ptr(Pointer { alloc: target, offset: 0 })).unwrap();
    // Forward overlap: [0..16] -> [8..24] (shifts reloc from 0 -> 8, overwrites old reloc at 16).
    mem.copy(bp.at(0), bp.at(8), 16).unwrap();
    assert_eq!(mem.read(bp.at(8), 8), Ok(ptr_val));
    assert_eq!(mem.read(bp.at(16), 8), Err(Refusal::Uninit { offset: 16 }));
    assert_eq!(mem.relocs(buf).unwrap(), &[(0, target), (8, target)]);

    // 3. Zero-memset recycle: killing `buf` and reallocating reuses heap bytes, but `init`
    //    guards all reads and `zero_unwritten` clears stale bytes.
    let dirty = mem.allocate(16, 0, true, Origin::Local).unwrap();
    let dp = Pointer { alloc: dirty, offset: 0 };
    mem.write(dp, 8, Scalar::Int(0xDEAD_BEEF_CAFE_BABE)).unwrap();
    mem.kill(dirty);
    let fresh = mem.allocate(16, 0, true, Origin::Local).unwrap();
    let fp = Pointer { alloc: fresh, offset: 0 };
    assert_eq!(mem.read(fp, 1), Err(Refusal::Uninit { offset: 0 }));
    assert_eq!(mem.read(fp.at(1), 2), Err(Refusal::Uninit { offset: 1 }));
    mem.write(fp.at(2), 2, Scalar::Int(0x1234)).unwrap();
    assert_eq!(mem.read(fp.at(2), 2), Ok(Scalar::Int(0x1234)));
    let mut out = [0xFFu8; 16];
    out.copy_from_slice(mem.bytes(fresh).unwrap());
    mem.zero_unwritten(fresh, &mut out);
    assert_eq!(&out[0..4], &[0, 0, 0x34, 0x12]);
}

/// String literals inside a loop must reuse their immutable `Origin::Data` allocation rather
/// than allocating fresh bytes every iteration and tripping `MemoryLimit`.
#[test]
fn str_literal_in_loop_does_not_leak_memory() {
    let mut w = world();
    let s_lit = std::format!("\"{}\"", "a".repeat(256));
    let body = std::format!(
        "{{ let mut i = 0u32; let mut acc = 0usize; while i < 500 {{ let s: &str = {s_lit}; acc += 1; i += 1; }} acc }}"
    );
    w.konst("LOOP_STR", "usize", &body);
    w.limits.memory = 4096;
    w.build();
    assert_eq!(eval_int(&mut w, "LOOP_STR"), Ok(500));
}

/// `Intrinsic::Offset` must reject overflowing `count * size` and byte offsets exceeding
/// `isize::MAX` with `Refusal::OutOfBounds` instead of panicking.
#[test]
fn offset_intrinsic_overflow_is_refused() {
    let mut w = world();
    let p0 = w.fx.param(0);
    let ptr = w.fx.ty(PNode::Ptr(blaze_typeck::Mutbl::Not, p0));
    let isize_ = w.ty("isize");
    w.intrinsic("offset", 1, &[ptr, isize_], ptr, Intrinsic::Offset);
    w.konst("OVER_MUL", "*const u64", "{ let x = 0u64; offset(&x as *const u64, 9223372036854775807isize) }");
    w.konst("OVER_MIN", "*const u8", "{ let x = 0u8; offset(&x as *const u8, -9223372036854775807isize - 1isize) }");
    w.build();
    assert!(refusal(&mut w, "OVER_MUL").contains("OutOfBounds"));
    assert!(refusal(&mut w, "OVER_MIN").contains("OutOfBounds"));
}

/// The Reference, "Raw borrow operators" (`&raw const`, `&raw mut`): forms a thin or wide raw
/// pointer to a place (locals, struct/tuple fields, array elements, slices/`str`/DST tails,
/// and `static` / `static mut` items) without reading the pointee or requiring a safe
/// reference.
#[test]
fn raw_borrows_of_locals_fields_elements_and_wide_places() {
    let mut w = world();
    let usize_ = w.fx.usize();
    let bytes_ptr = w.ty("*const [u8]");
    let str_ptr = w.ty("*const str");
    w.intrinsic("ptr_metadata_slice", 0, &[bytes_ptr], usize_, Intrinsic::PtrMetadata);
    w.intrinsic("ptr_metadata_str", 0, &[str_ptr], usize_, Intrinsic::PtrMetadata);
    w.strukt("Pair", &[("a", "u32"), ("b", "u32")]);
    w.strukt("Dst", &[("head", "u16"), ("tail", "[u8]")]);
    // Scalar local (restarts from register home to memory slot) & mutable local.
    w.konst("LOCAL_CONST", "u32", "{ let x = 42u32; let p = &raw const x; unsafe { *p } }");
    w.konst("LOCAL_MUT", "u32", "{ let mut x = 10u32; let p = &raw mut x; unsafe { *p = 99; } x }");
    // Uninitialized scalar local initialized through `&raw mut`.
    w.konst("UNINIT_LOCAL", "u32", "{ let mut x: u32; let p = &raw mut x; unsafe { *p = 77; } x }");
    // Struct field, tuple field, and array element raw borrows.
    w.konst(
        "STRUCT_FIELD",
        "u32",
        "{ let mut s = Pair { a: 3, b: 4 }; let pb = &raw mut s.b; unsafe { *pb = 40; } let pa = &raw const s.a; unsafe { *pa + s.b } }",
    );
    w.konst(
        "TUPLE_FIELD",
        "u16",
        "{ let mut t = (1u8, 200u16); let p = &raw mut t.1; unsafe { *p = 300; *(&raw const t.1) } }",
    );
    w.konst(
        "ARRAY_ELEM",
        "u8",
        "{ let mut a = [10u8, 20, 30]; let p = &raw mut a[1]; unsafe { *p = 25; } let q = &raw const a[2]; unsafe { a[1] + *q } }",
    );
    // Wide raw pointers: `[u8]` and `str` keep their length metadata.
    w.konst("BYTES", "[u8; 4]", "[5, 6, 7, 8]");
    w.konst(
        "WIDE_SLICE_LEN",
        "usize",
        "{ let s: &[u8] = &BYTES; let p: *const [u8] = &raw const *s; ptr_metadata_slice(p) }",
    );
    w.konst(
        "WIDE_SLICE_ELEM",
        "u8",
        "{ let mut a = [1u8, 2, 3]; let s: &mut [u8] = &mut a; let p: *mut [u8] = &raw mut *s; unsafe { (*p)[2] = 9; (*(&raw const *p))[2] } }",
    );
    w.konst(
        "WIDE_STR_LEN",
        "usize",
        "{ let s: &str = \"hello\"; let p: *const str = &raw const *s; ptr_metadata_str(p) }",
    );
    // DST struct: `&raw const (*p).tail` keeps length, `&raw const (*p).head` is thin.
    w.konst(
        "DST_TAIL_LEN",
        "usize",
        "{ let s: &[u8] = &BYTES; let d: *const Dst = s as *const [u8] as *const Dst; ptr_metadata_slice(unsafe { &raw const (*d).tail }) }",
    );
    w.konst(
        "DST_HEAD",
        "u16",
        "{ let a = [0x34u8, 0x12, 0x78, 0x56]; let s: &[u8] = &a; let d: *const Dst = s as *const [u8] as *const Dst; unsafe { *(&raw const (*d).head) } }",
    );
    w.build();

    assert_eq!(eval_int(&mut w, "LOCAL_CONST"), Ok(42));
    assert_eq!(eval_int(&mut w, "LOCAL_MUT"), Ok(99));
    assert_eq!(eval_int(&mut w, "UNINIT_LOCAL"), Ok(77));
    assert_eq!(eval_int(&mut w, "STRUCT_FIELD"), Ok(43));
    assert_eq!(eval_int(&mut w, "TUPLE_FIELD"), Ok(300));
    assert_eq!(eval_int(&mut w, "ARRAY_ELEM"), Ok(55));
    assert_eq!(eval_int(&mut w, "WIDE_SLICE_LEN"), Ok(4));
    assert_eq!(eval_int(&mut w, "WIDE_SLICE_ELEM"), Ok(9));
    assert_eq!(eval_int(&mut w, "WIDE_STR_LEN"), Ok(5));
    assert_eq!(eval_int(&mut w, "DST_TAIL_LEN"), Ok(4));
    assert_eq!(eval_int(&mut w, "DST_HEAD"), Ok(0x1234));
}

/// `&raw const (*p).field` / `&raw mut (*p).field` forms the field's address without reading
/// the pointee: uninitialized structs, zero-offset null pointers, and transient dangling raw
/// borrows succeed, while non-zero null offsets, out-of-bounds field projections, escaping
/// dangling pointers, and invalid safe references are refused by name.
#[test]
fn raw_borrow_deref_projection_without_loading_pointee() {
    let mut w = world();
    w.strukt("Pair", &[("a", "u32"), ("b", "u32")]);
    w.const_fn("dangle", &[], "*const u8", "{ let x = 5u8; &raw const x }");
    // Partial initialization through `&raw mut (*p).b`: `s.a` stays uninitialized, never read.
    w.konst(
        "UNINIT_DEREF_FIELD",
        "u32",
        "{ let mut s: Pair; let p = &raw mut s; let pb = unsafe { &raw mut (*p).b }; unsafe { *pb = 123; *pb } }",
    );
    // Null / zero-offset deref raw borrows succeed without reading.
    w.konst("NULL_RAW", "*const u8", "&raw const *(0usize as *const u8)");
    w.konst("NULL_FIELD0", "*const u32", "unsafe { &raw const (*(0usize as *const Pair)).a }");
    // Non-zero offset on a provenance-free pointer is refused.
    w.konst("NULL_FIELD4", "*const u32", "unsafe { &raw const (*(0usize as *const Pair)).b }");
    // Out-of-bounds field projection past a live allocation is refused; one-past-end succeeds.
    w.konst(
        "ONE_PAST_END",
        "bool",
        "{ let x = 1u32; let p = &raw const x as *const Pair; let _pb = unsafe { &raw const (*p).b }; true }",
    );
    w.konst(
        "PAST_END",
        "bool",
        "{ let x = 1u8; let p = &raw const x as *const Pair; let _pb = unsafe { &raw const (*p).b }; true }",
    );
    // Transient dangling raw reborrow succeeds; escaping dangling raw pointer is refused at intern.
    w.konst("TRANSIENT_DANGLE", "u32", "{ let p = dangle(); let _q = &raw const *p; 7 }");
    w.konst("ESCAPING_DANGLE", "*const u8", "{ let p = dangle(); &raw const *p }");
    // Safe references still reject null and dangling pointers immediately.
    w.konst("SAFE_NULL_REF", "u32", "{ let _r: &u8 = unsafe { &*(0usize as *const u8) }; 1 }");
    w.konst("SAFE_DANGLE_REF", "u32", "{ let p = dangle(); let _r: &u8 = unsafe { &*p }; 1 }");
    w.build();

    assert_eq!(eval_int(&mut w, "UNINIT_DEREF_FIELD"), Ok(123));
    let null_raw = w.eval("NULL_RAW").expect("NULL_RAW");
    assert_eq!(null_raw.data.allocs[0].bytes.as_slice(), &[0; 8]);
    assert!(null_raw.data.allocs[0].relocs.is_empty());
    let null_f0 = w.eval("NULL_FIELD0").expect("NULL_FIELD0");
    assert_eq!(null_f0.data.allocs[0].bytes.as_slice(), &[0; 8]);
    assert!(null_f0.data.allocs[0].relocs.is_empty());
    assert_eq!(w.eval("NULL_FIELD4").expect_err("NULL_FIELD4").refusal, Refusal::IntAsPointer);
    assert_eq!(eval_int(&mut w, "ONE_PAST_END"), Ok(1));
    assert_eq!(
        w.eval("PAST_END").expect_err("PAST_END").refusal,
        Refusal::OutOfBounds { offset: 4, size: 0, len: 1 }
    );
    assert_eq!(eval_int(&mut w, "TRANSIENT_DANGLE"), Ok(7));
    assert_eq!(w.eval("ESCAPING_DANGLE").expect_err("ESCAPING_DANGLE").refusal, Refusal::Dangling);
    assert_eq!(w.eval("SAFE_NULL_REF").expect_err("SAFE_NULL_REF").refusal, Refusal::IntAsPointer);
    assert_eq!(w.eval("SAFE_DANGLE_REF").expect_err("SAFE_DANGLE_REF").refusal, Refusal::Dangling);
}

/// Raw borrows of `static` and `static mut` items in `static` initializers (the shape of
/// `ANTITHESIS_GUIDANCE_CATALOG`) produce `DataTarget::Static` relocations with the field
/// offset without reading the target static's bytes; reading or writing a `static mut` through
/// the raw pointer is still refused with `Refusal::MutableStatic`.
#[test]
fn raw_borrows_of_statics_in_static_initializers() {
    let mut w = world();
    w.strukt("Pair", &[("a", "u32"), ("b", "u32")]);
    w.strukt("Catalog", &[("id", "u32"), ("ro", "*const u32"), ("rw", "*mut u32"), ("slice", "*const [u8]")]);
    w.stat("S", "Pair", "Pair { a: 1, b: 2 }");
    w.stat_mut("SM", "Pair", "Pair { a: 3, b: 4 }");
    w.stat("ARR", "[u8; 3]", "[9, 8, 7]");
    w.stat(
        "ANTITHESIS_GUIDANCE_CATALOG",
        "Catalog",
        "Catalog { id: 1, ro: &raw const S.b, rw: unsafe { &raw mut SM.b }, slice: &raw const ARR }",
    );
    w.stat("LOCAL_MUT_PTR", "*mut u32", "&raw mut (Pair { a: 5, b: 6 }).b");
    w.konst("READ_SM_RAW", "u32", "unsafe { *(&raw const SM.b) }");
    w.konst("WRITE_SM_RAW", "()", "unsafe { *(&raw mut SM.a) = 99; }");
    w.build();

    let s_key = w.key("S");
    let sm_key = w.key("SM");
    let arr_key = w.key("ARR");
    let cat = w.eval("ANTITHESIS_GUIDANCE_CATALOG").expect("ANTITHESIS_GUIDANCE_CATALOG");
    assert_eq!(cat.data.allocs.len(), 1, "only static relocations, no extra local allocs");
    let relocs = &cat.data.allocs[0].relocs;
    assert_eq!(relocs.len(), 3, "ro, rw, and slice pointers");
    assert!(relocs.iter().any(|r| r.target == DataTarget::Static(s_key)));
    assert!(relocs.iter().any(|r| r.target == DataTarget::Static(sm_key)));
    assert!(relocs.iter().any(|r| r.target == DataTarget::Static(arr_key)));
    // Each `.b` pointer relocation carries field offset 4 as its little-endian addend.
    for r in relocs.iter().filter(|r| r.target == DataTarget::Static(s_key) || r.target == DataTarget::Static(sm_key)) {
        let off = usize::try_from(r.offset).expect("offset fits usize");
        let addend = u64::from_le_bytes(cat.data.allocs[0].bytes[off..off + 8].try_into().expect("8-byte word"));
        assert_eq!(addend, 4, "field `b` offset");
    }
    // The slice raw pointer carries length 3 in the word right after its data pointer.
    let slice_reloc = relocs.iter().find(|r| r.target == DataTarget::Static(arr_key)).expect("ARR reloc");
    let slice_off = usize::try_from(slice_reloc.offset).expect("offset fits usize");
    let slice_len = u64::from_le_bytes(cat.data.allocs[0].bytes[slice_off + 8..slice_off + 16].try_into().expect("8-byte len"));
    assert_eq!(slice_len, 3);

    // `&raw mut` to a local/promoted allocation inside a static marks the target allocation mutable.
    let local_mut = w.eval("LOCAL_MUT_PTR").expect("LOCAL_MUT_PTR");
    assert_eq!(local_mut.data.allocs.len(), 2);
    assert_eq!(local_mut.data.allocs[0].relocs[0].target, DataTarget::Local(1));
    assert!(local_mut.data.allocs[1].mutable, "target of `*mut` in a static is writable");

    // Reading or writing a `static mut` through a raw pointer is still refused.
    assert_eq!(w.eval("READ_SM_RAW").expect_err("READ_SM_RAW").refusal, Refusal::MutableStatic { item: sm_key });
    assert_eq!(w.eval("WRITE_SM_RAW").expect_err("WRITE_SM_RAW").refusal, Refusal::MutableStatic { item: sm_key });
}
