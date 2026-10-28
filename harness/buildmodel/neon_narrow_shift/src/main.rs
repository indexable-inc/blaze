//! zune-jpeg's YCbCr conversion calls `vqshrun_n_s32::<14>`. Its LLVM intrinsic
//! was missing. Call the intrinsics directly here so the differential harness cannot
//! accidentally use rustc's copy of a core::arch wrapper instead of testing the lowering.
//! The i128 oracle keeps rounding carries beyond a 64-bit source lane before saturation.
#![feature(link_llvm_intrinsics, simd_ffi)]
#![allow(internal_features)]

use std::{arch::aarch64::*, hint::black_box, mem::transmute};

fn inputs(bits: u32, shift: i32) -> Vec<i128> {
    let unit = 1i128 << shift;
    let half = unit / 2;
    let signed_limit = 1i128 << (bits / 2 - 1);
    let unsigned_limit = 1i128 << (bits / 2);
    let mut values = vec![
        -(1i128 << (bits - 1)),
        (1i128 << (bits - 1)) - 1,
        (1i128 << bits) - 1,
        -unit - 1,
        -unit,
        -half - 1,
        -half,
        -1,
        0,
        1,
        half - 1,
        half,
        half + 1,
        unit - 1,
        unit,
        unit + 1,
    ];
    // Both signed limits and the unsigned ceiling, just before and after the point
    // where rounding changes the result or saturation first becomes necessary.
    for boundary in
        [-signed_limit, signed_limit - 1, signed_limit, unsigned_limit - 1, unsigned_limit]
    {
        for offset in [-half - 1, -half, -1, 0, 1, half - 1, half] {
            values.push(boundary * unit + offset);
        }
    }
    values
}

fn expected(value: i128, shift: i32, bits: u32, signed: bool, rounded: bool) -> i128 {
    let rounding = if rounded { 1i128 << (shift - 1) } else { 0 };
    let shifted = (value + rounding) >> shift;
    let (min, max) = if signed {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    } else {
        (0, (1i128 << bits) - 1)
    };
    shifted.clamp(min, max)
}

macro_rules! check_width {
    ($name:ident, $bits:literal, $count:literal, $signed:ty, $unsigned:ty,
     $small_signed:ty, $small_unsigned:ty, $input_signed:ty, $input_unsigned:ty,
     $result_signed:ty, $result_unsigned:ty, $suffix:literal) => {
        #[inline(never)]
        fn $name<const N: i32>() {
            unsafe extern "llvm-intrinsic" {
                #[link_name = concat!("llvm.aarch64.neon.sqshrn.", $suffix)]
                fn sqshrn(a: $input_signed, n: i32) -> $result_signed;
                #[link_name = concat!("llvm.aarch64.neon.sqrshrn.", $suffix)]
                fn sqrshrn(a: $input_signed, n: i32) -> $result_signed;
                #[link_name = concat!("llvm.aarch64.neon.uqshrn.", $suffix)]
                fn uqshrn(a: $input_unsigned, n: i32) -> $result_unsigned;
                #[link_name = concat!("llvm.aarch64.neon.uqrshrn.", $suffix)]
                fn uqrshrn(a: $input_unsigned, n: i32) -> $result_unsigned;
                #[link_name = concat!("llvm.aarch64.neon.sqshrun.", $suffix)]
                fn sqshrun(a: $input_signed, n: i32) -> $result_unsigned;
                #[link_name = concat!("llvm.aarch64.neon.sqrshrun.", $suffix)]
                fn sqrshrun(a: $input_signed, n: i32) -> $result_unsigned;
            }
            for chunk in inputs($bits, N).chunks($count) {
                let mut s = [0; $count];
                let mut u = [0; $count];
                for (lane, &value) in chunk.iter().enumerate() {
                    s[lane] =
                        value.clamp(<$signed>::MIN as i128, <$signed>::MAX as i128) as $signed;
                    u[lane] = value.clamp(0, <$unsigned>::MAX as i128) as $unsigned;
                }
                let s = black_box(s);
                let u = black_box(u);
                let (signed, unsigned, mixed) = unsafe {
                    let a = transmute::<[$signed; $count], $input_signed>(s);
                    let b = transmute::<[$unsigned; $count], $input_unsigned>(u);
                    (
                        [
                            transmute::<_, [$small_signed; $count]>(sqshrn(a, N)),
                            transmute::<_, [$small_signed; $count]>(sqrshrn(a, N)),
                        ],
                        [
                            transmute::<_, [$small_unsigned; $count]>(uqshrn(b, N)),
                            transmute::<_, [$small_unsigned; $count]>(uqrshrn(b, N)),
                        ],
                        [
                            transmute::<_, [$small_unsigned; $count]>(sqshrun(a, N)),
                            transmute::<_, [$small_unsigned; $count]>(sqrshrun(a, N)),
                        ],
                    )
                };
                for rounded in 0..2 {
                    for lane in 0..$count {
                        assert_eq!(
                            signed[rounded][lane] as i128,
                            expected(s[lane] as i128, N, $bits / 2, true, rounded == 1)
                        );
                        assert_eq!(
                            unsigned[rounded][lane] as i128,
                            expected(u[lane] as i128, N, $bits / 2, false, rounded == 1)
                        );
                        assert_eq!(
                            mixed[rounded][lane] as i128,
                            expected(s[lane] as i128, N, $bits / 2, false, rounded == 1)
                        );
                    }
                }
            }
        }
    };
}

check_width!(check16, 16, 8, i16, u16, i8, u8, int16x8_t, uint16x8_t, int8x8_t, uint8x8_t, "v8i8");
check_width!(
    check32, 32, 4, i32, u32, i16, u16, int32x4_t, uint32x4_t, int16x4_t, uint16x4_t, "v4i16"
);
check_width!(
    check64, 64, 2, i64, u64, i32, u32, int64x2_t, uint64x2_t, int32x2_t, uint32x2_t, "v2i32"
);

#[inline(never)]
fn check_scalar<const N: i32>() {
    unsafe extern "llvm-intrinsic" {
        #[link_name = "llvm.aarch64.neon.sqshrn.i32"]
        fn signed(a: i64, n: i32) -> i32;
        #[link_name = "llvm.aarch64.neon.uqshrn.i32"]
        fn unsigned(a: u64, n: i32) -> u32;
    }
    for value in inputs(64, N) {
        let s = black_box(value.clamp(i64::MIN as i128, i64::MAX as i128) as i64);
        let u = black_box(value.clamp(0, u64::MAX as i128) as u64);
        assert_eq!(unsafe { signed(s, N) } as i128, expected(s as i128, N, 32, true, false));
        assert_eq!(unsafe { unsigned(u, N) } as i128, expected(u as i128, N, 32, false, false));
    }
}

fn main() {
    check16::<1>();
    check16::<4>();
    check16::<8>();
    check32::<1>();
    check32::<14>(); // zune-jpeg's shift
    check32::<16>();
    check64::<1>();
    check64::<17>();
    check64::<32>();
    check_scalar::<1>();
    check_scalar::<17>();
    check_scalar::<32>();

    let jpeg = black_box([i32::MIN, -1, 123 * (1 << 14) + (1 << 13), i32::MAX]);
    let got =
        unsafe { transmute::<_, [u16; 4]>(vqshrun_n_s32::<14>(transmute::<_, int32x4_t>(jpeg))) };
    assert_eq!(got, [0, 0, 123, u16::MAX]);
    println!("NEON narrowing shifts: ok");
}
