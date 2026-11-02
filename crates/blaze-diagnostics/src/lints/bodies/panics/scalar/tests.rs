//! The scalar arithmetic against Rust's own on every pair of 8-bit values: what rustc's
//! interpreter computes is what the operation computes on the machine.

use super::*;

const I8: Kind = Kind::Int { signed: true, pointer_sized: false };
const U8: Kind = Kind::Int { signed: false, pointer_sized: false };

fn i8s(value: i8) -> Scalar {
    Scalar::new(value as u8 as u128, 1, I8)
}

fn u8s(value: u8) -> Scalar {
    Scalar::new(u128::from(value), 1, U8)
}

fn one(output: Option<Output>) -> Option<u128> {
    match output? {
        Output::One(it) => Some(it.bits),
        Output::Pair(..) => panic!("a pair from an operation without overflow"),
    }
}

fn pair(output: Option<Output>) -> (u128, bool) {
    match output {
        Some(Output::Pair(it, overflowed)) => (it.bits, overflowed),
        _ => panic!("no pair from an operation with overflow"),
    }
}

#[test]
fn signed_operations_are_rusts_on_every_pair() {
    for a in i8::MIN..=i8::MAX {
        for b in i8::MIN..=i8::MAX {
            let (l, r) = (i8s(a), i8s(b));
            let bits = |(value, overflowed): (i8, bool)| (value as u8 as u128, overflowed);
            assert_eq!(pair(binary_op(&BinOp::AddWithOverflow, l, r)), bits(a.overflowing_add(b)), "{a} + {b}");
            assert_eq!(pair(binary_op(&BinOp::SubWithOverflow, l, r)), bits(a.overflowing_sub(b)), "{a} - {b}");
            assert_eq!(pair(binary_op(&BinOp::MulWithOverflow, l, r)), bits(a.overflowing_mul(b)), "{a} * {b}");
            assert_eq!(one(binary_op(&BinOp::Add, l, r)), Some(a.wrapping_add(b) as u8 as u128));
            assert_eq!(one(binary_op(&BinOp::Div, l, r)), a.checked_div(b).map(|it| it as u8 as u128), "{a} / {b}");
            assert_eq!(one(binary_op(&BinOp::Rem, l, r)), a.checked_rem(b).map(|it| it as u8 as u128), "{a} % {b}");
            assert_eq!(one(binary_op(&BinOp::Lt, l, r)), Some(u128::from(a < b)));
            assert_eq!(one(binary_op(&BinOp::BitXor, l, r)), Some((a ^ b) as u8 as u128));
            let by = b as u32;
            assert_eq!(one(binary_op(&BinOp::Shl, l, r)), Some(a.wrapping_shl(by) as u8 as u128), "{a} << {b}");
            assert_eq!(one(binary_op(&BinOp::Shr, l, r)), Some(a.wrapping_shr(by) as u8 as u128), "{a} >> {b}");
        }
        let negated = a.overflowing_neg();
        assert_eq!(unary_op(UnOp::Neg, i8s(a)).map(|it| it.bits), Some(negated.0 as u8 as u128));
        assert_eq!(overflows(&BinOp::SubWithOverflow, i8s(0), i8s(a)), Some(negated.1));
    }
}

#[test]
fn unsigned_operations_are_rusts_on_every_pair() {
    for a in 0..=u8::MAX {
        for b in 0..=u8::MAX {
            let (l, r) = (u8s(a), u8s(b));
            let bits = |(value, overflowed): (u8, bool)| (u128::from(value), overflowed);
            assert_eq!(pair(binary_op(&BinOp::AddWithOverflow, l, r)), bits(a.overflowing_add(b)), "{a} + {b}");
            assert_eq!(pair(binary_op(&BinOp::SubWithOverflow, l, r)), bits(a.overflowing_sub(b)), "{a} - {b}");
            assert_eq!(pair(binary_op(&BinOp::MulWithOverflow, l, r)), bits(a.overflowing_mul(b)), "{a} * {b}");
            assert_eq!(one(binary_op(&BinOp::Div, l, r)), a.checked_div(b).map(u128::from), "{a} / {b}");
            assert_eq!(one(binary_op(&BinOp::Rem, l, r)), a.checked_rem(b).map(u128::from), "{a} % {b}");
            assert_eq!(one(binary_op(&BinOp::Ge, l, r)), Some(u128::from(a >= b)));
            let by = u32::from(b);
            assert_eq!(one(binary_op(&BinOp::Shl, l, r)), Some(u128::from(a.wrapping_shl(by))), "{a} << {b}");
            assert_eq!(one(binary_op(&BinOp::Shr, l, r)), Some(u128::from(a.wrapping_shr(by))), "{a} >> {b}");
        }
    }
}

#[test]
fn casts_are_rusts() {
    for a in i8::MIN..=i8::MAX {
        assert_eq!(int_cast(i8s(a), U8, 1).map(|it| it.bits), Some(u128::from(a as u8)));
        assert_eq!(int_cast(i8s(a), Kind::Int { signed: true, pointer_sized: false }, 4).map(|it| it.bits),
            Some(u128::from(a as i32 as u32)));
        let float = int_cast(i8s(a), Kind::Float(FloatTy::F64), 8).unwrap();
        assert_eq!(float.bits, u128::from((a as f64).to_bits()));
        assert_eq!(float_cast(float, I8, 1).map(|it| it.bits), Some(u128::from(a as u8)));
    }
    for value in [f64::NAN, f64::INFINITY, -1e300, -0.5, 1.5, 255.9, 1e300] {
        let float = Scalar::new(u128::from(value.to_bits()), 8, Kind::Float(FloatTy::F64));
        assert_eq!(float_cast(float, U8, 1).map(|it| it.bits), Some(u128::from(value as u8)), "{value}");
        assert_eq!(float_cast(float, I8, 1).map(|it| it.bits), Some(u128::from(value as i8 as u8)), "{value}");
    }
}

#[test]
fn integers_print_as_rustc_prints_them() {
    let i32s = |value: i32| Scalar::new(value as u32 as u128, 4, Kind::Int { signed: true, pointer_sized: false });
    assert_eq!(const_int(i32s(i32::MIN), true), "i32::MIN");
    assert_eq!(const_int(i32s(i32::MAX), false), "i32::MAX");
    assert_eq!(const_int(i32s(-5), true), "-5_i32");
    assert_eq!(const_int(i32s(1), false), "1");
    assert_eq!(const_int(u8s(u8::MAX), true), "u8::MAX");
    assert_eq!(const_int(Scalar::new(7, 8, Kind::Int { signed: false, pointer_sized: true }), true), "7_usize");
}
