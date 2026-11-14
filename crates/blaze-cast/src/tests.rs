//! Each function is the `as` it names: `f(x) == (x as T)` for every `x` the tape draws and for
//! every boundary value of the source type, a float compared by its bits so NaN and the sign
//! of zero count. And each composition `blaze::as_cast`'s fix writes from std's own operations
//! (a width change in the source's sign, then the sign) is the `as` it replaces.
//!
//! The invariant: for every function `f: F -> T` here and every `x: F`, `f(x)` and `x as T`
//! are the same bits; for every composition the lint writes for `x as T`, the same.

use blaze_sim::Config;

use super::*;

/// The values of a type where an `as` from it changes how it behaves: the ends of every
/// integer range, the float powers of two where precision runs out, the halves where rounding
/// ties, NaN, the infinities and both zeros.
trait Boundaries: Sized + Copy {
    fn boundaries() -> Vec<Self>;
}

macro_rules! integer_boundaries {
    ($($t:ty),*) => {$(
        impl Boundaries for $t {
            fn boundaries() -> Vec<Self> {
                let mut out = vec![<$t>::MIN, <$t>::MAX, 0, 1, <$t>::MIN.wrapping_add(1), <$t>::MAX - 1];
                for bit in 0..<$t>::BITS {
                    let one: $t = 1;
                    let power = one.wrapping_shl(bit);
                    for v in [power, power.wrapping_sub(1), power.wrapping_add(1), power.wrapping_neg()] {
                        out.push(v);
                    }
                }
                out
            }
        }
    )*};
}
integer_boundaries!(u8, i8, u16, i16, u32, i32, u64, i64, u128, i128, usize, isize);

macro_rules! float_boundaries {
    ($($t:ty),*) => {$(
        impl Boundaries for $t {
            fn boundaries() -> Vec<Self> {
                let mut out = vec![
                    <$t>::NAN, -<$t>::NAN, <$t>::INFINITY, <$t>::NEG_INFINITY, 0.0, -0.0,
                    <$t>::MAX, <$t>::MIN, <$t>::MIN_POSITIVE, -<$t>::MIN_POSITIVE, <$t>::EPSILON,
                    0.5, -0.5, 0.999_999, -0.999_999, 1.0, -1.0, 1.5, -1.5,
                    <$t>::from_bits(1), -<$t>::from_bits(1),
                ];
                let mut power: $t = 1.0;
                for _ in 0..130 {
                    for v in [power, -power, power - 1.0, power + 1.0, power * 1.5, -power - 1.0] {
                        out.push(v);
                    }
                    power *= 2.0;
                }
                out
            }
        }
    )*};
}
float_boundaries!(f32, f64);

/// Bits to compare a result by: the value of an integer, the bit pattern of a float.
trait Same: Copy + core::fmt::Debug {
    fn bits(self) -> u128;
}

macro_rules! same_integer {
    ($($t:ty),*) => {$(
        impl Same for $t {
            fn bits(self) -> u128 {
                self as u128
            }
        }
    )*};
}
same_integer!(u8, i8, u16, i16, u32, i32, u64, i64, u128, i128, usize, isize);

impl Same for f32 {
    fn bits(self) -> u128 {
        u128::from(self.to_bits())
    }
}

impl Same for f64 {
    fn bits(self) -> u128 {
        u128::from(self.to_bits())
    }
}

impl Same for char {
    fn bits(self) -> u128 {
        u128::from(u32::from(self))
    }
}

/// `f(x)` and `x as T` for each named function, on the tape's draws and on the boundaries.
/// A source is drawn by its own rule (`Tape::any`): any integer of its type, any float with NaN
/// and the infinities among them.
macro_rules! is_the_cast {
    ($($test:ident: $f:ident($from:ty) -> $to:ty;)*) => {$(
        #[test]
        fn $test() {
            blaze_sim::check(Config::DEFAULT, |tape| {
                let x: $from = tape.any::<$from>("x");
                assert_eq!($f(x).bits(), (x as $to).bits(), "{}({x:?})", stringify!($f));
            });
        }
    )*
        #[test]
        fn every_function_is_its_cast_on_the_boundaries() {
            $(for x in <$from as Boundaries>::boundaries() {
                assert_eq!($f(x).bits(), (x as $to).bits(), "{}({x:?})", stringify!($f));
            })*
        }
    };
}

is_the_cast! {
    usize_from_u32_is_its_cast: usize_from_u32(u32) -> usize;
    isize_from_i32_is_its_cast: isize_from_i32(i32) -> isize;
    isize_from_u16_is_its_cast: isize_from_u16(u16) -> isize;
    u64_from_usize_is_its_cast: u64_from_usize(usize) -> u64;
    u128_from_usize_is_its_cast: u128_from_usize(usize) -> u128;
    i128_from_usize_is_its_cast: i128_from_usize(usize) -> i128;
    i64_from_isize_is_its_cast: i64_from_isize(isize) -> i64;
    i128_from_isize_is_its_cast: i128_from_isize(isize) -> i128;

    u32_truncated_from_usize_is_its_cast: u32_truncated_from_usize(usize) -> u32;
    i32_truncated_from_isize_is_its_cast: i32_truncated_from_isize(isize) -> i32;
    usize_truncated_from_u64_is_its_cast: usize_truncated_from_u64(u64) -> usize;
    isize_truncated_from_i64_is_its_cast: isize_truncated_from_i64(i64) -> isize;
    usize_truncated_from_u128_is_its_cast: usize_truncated_from_u128(u128) -> usize;
    isize_truncated_from_i128_is_its_cast: isize_truncated_from_i128(i128) -> isize;

    f64_from_f32_is_its_cast: f64_from_f32(f32) -> f64;
    f32_from_u8_is_its_cast: f32_from_u8(u8) -> f32;
    f32_from_i8_is_its_cast: f32_from_i8(i8) -> f32;
    f32_from_u16_is_its_cast: f32_from_u16(u16) -> f32;
    f32_from_i16_is_its_cast: f32_from_i16(i16) -> f32;
    f64_from_u8_is_its_cast: f64_from_u8(u8) -> f64;
    f64_from_i8_is_its_cast: f64_from_i8(i8) -> f64;
    f64_from_u16_is_its_cast: f64_from_u16(u16) -> f64;
    f64_from_i16_is_its_cast: f64_from_i16(i16) -> f64;
    f64_from_u32_is_its_cast: f64_from_u32(u32) -> f64;
    f64_from_i32_is_its_cast: f64_from_i32(i32) -> f64;

    f32_rounded_from_u32_is_its_cast: f32_rounded_from_u32(u32) -> f32;
    f32_rounded_from_i32_is_its_cast: f32_rounded_from_i32(i32) -> f32;
    f32_rounded_from_u64_is_its_cast: f32_rounded_from_u64(u64) -> f32;
    f32_rounded_from_i64_is_its_cast: f32_rounded_from_i64(i64) -> f32;
    f32_rounded_from_u128_is_its_cast: f32_rounded_from_u128(u128) -> f32;
    f32_rounded_from_i128_is_its_cast: f32_rounded_from_i128(i128) -> f32;
    f32_rounded_from_usize_is_its_cast: f32_rounded_from_usize(usize) -> f32;
    f32_rounded_from_isize_is_its_cast: f32_rounded_from_isize(isize) -> f32;
    f64_rounded_from_u64_is_its_cast: f64_rounded_from_u64(u64) -> f64;
    f64_rounded_from_i64_is_its_cast: f64_rounded_from_i64(i64) -> f64;
    f64_rounded_from_u128_is_its_cast: f64_rounded_from_u128(u128) -> f64;
    f64_rounded_from_i128_is_its_cast: f64_rounded_from_i128(i128) -> f64;
    f64_rounded_from_usize_is_its_cast: f64_rounded_from_usize(usize) -> f64;
    f64_rounded_from_isize_is_its_cast: f64_rounded_from_isize(isize) -> f64;
    f32_rounded_from_f64_is_its_cast: f32_rounded_from_f64(f64) -> f32;

    u8_saturated_from_f32_is_its_cast: u8_saturated_from_f32(f32) -> u8;
    u16_saturated_from_f32_is_its_cast: u16_saturated_from_f32(f32) -> u16;
    u32_saturated_from_f32_is_its_cast: u32_saturated_from_f32(f32) -> u32;
    u64_saturated_from_f32_is_its_cast: u64_saturated_from_f32(f32) -> u64;
    u128_saturated_from_f32_is_its_cast: u128_saturated_from_f32(f32) -> u128;
    usize_saturated_from_f32_is_its_cast: usize_saturated_from_f32(f32) -> usize;
    i8_saturated_from_f32_is_its_cast: i8_saturated_from_f32(f32) -> i8;
    i16_saturated_from_f32_is_its_cast: i16_saturated_from_f32(f32) -> i16;
    i32_saturated_from_f32_is_its_cast: i32_saturated_from_f32(f32) -> i32;
    i64_saturated_from_f32_is_its_cast: i64_saturated_from_f32(f32) -> i64;
    i128_saturated_from_f32_is_its_cast: i128_saturated_from_f32(f32) -> i128;
    isize_saturated_from_f32_is_its_cast: isize_saturated_from_f32(f32) -> isize;
    u8_saturated_from_f64_is_its_cast: u8_saturated_from_f64(f64) -> u8;
    u16_saturated_from_f64_is_its_cast: u16_saturated_from_f64(f64) -> u16;
    u32_saturated_from_f64_is_its_cast: u32_saturated_from_f64(f64) -> u32;
    u64_saturated_from_f64_is_its_cast: u64_saturated_from_f64(f64) -> u64;
    u128_saturated_from_f64_is_its_cast: u128_saturated_from_f64(f64) -> u128;
    usize_saturated_from_f64_is_its_cast: usize_saturated_from_f64(f64) -> usize;
    i8_saturated_from_f64_is_its_cast: i8_saturated_from_f64(f64) -> i8;
    i16_saturated_from_f64_is_its_cast: i16_saturated_from_f64(f64) -> i16;
    i32_saturated_from_f64_is_its_cast: i32_saturated_from_f64(f64) -> i32;
    i64_saturated_from_f64_is_its_cast: i64_saturated_from_f64(f64) -> i64;
    i128_saturated_from_f64_is_its_cast: i128_saturated_from_f64(f64) -> i128;
    isize_saturated_from_f64_is_its_cast: isize_saturated_from_f64(f64) -> isize;
}

/// The functions whose source has values a generator does not draw.
#[test]
fn the_bool_and_char_functions_are_their_casts() {
    for b in [false, true] {
        assert_eq!(u8_from_bool(b), b as u8);
    }
    for c in ['\0', 'a', '\u{7f}', '\u{80}', '\u{ff}', '\u{100}', '\u{d7ff}', '\u{e000}', '\u{10ffff}'] {
        assert_eq!(u32_from_char(c), c as u32);
    }
    for x in u8::MIN..=u8::MAX {
        assert_eq!(char_from_u8(x), x as char);
    }
}

/// The compositions `blaze::as_cast` writes for a cast that changes width and sign, or
/// narrows: each is the `as` it replaces, on the tape's draws (any value of the source type)
/// and on the boundaries.
macro_rules! composes {
    ($($test:ident: |$x:ident: $from:ty| $written:expr => $to:ty;)*) => {$(
        #[test]
        fn $test() {
            blaze_sim::check(Config::DEFAULT, |tape| {
                let $x: $from = tape.any::<$from>("x");
                assert_eq!(($written).bits(), ($x as $to).bits(), "{} at {:?}", stringify!($written), $x);
            });
        }
    )*
        #[test]
        fn every_composition_is_its_cast_on_the_boundaries() {
            $(for $x in <$from as Boundaries>::boundaries() {
                assert_eq!(($written).bits(), ($x as $to).bits(), "{} at {:?}", stringify!($written), $x);
            })*
        }
    };
}

composes! {
    // A sign change alone.
    i32_from_u32_is_cast_signed: |x: u32| x.cast_signed() => i32;
    usize_from_isize_is_cast_unsigned: |x: isize| x.cast_unsigned() => usize;
    // A narrowing between fixed widths.
    u32_from_u64_is_truncate: |x: u64| x.truncate::<u32>() => u32;
    i8_from_i64_is_truncate: |x: i64| x.truncate::<i8>() => i8;
    u16_from_usize_is_truncate: |x: usize| x.truncate::<u16>() => u16;
    // A widening in the source's sign, then the sign: a negative source sign-extends.
    u64_from_i32_is_from_then_cast_unsigned: |x: i32| i64::from(x).cast_unsigned() => u64;
    i64_from_u32_is_from_then_cast_signed: |x: u32| u64::from(x).cast_signed() => i64;
    usize_from_i32_is_widened_then_cast_unsigned: |x: i32| isize_from_i32(x).cast_unsigned() => usize;
    u64_from_i8_in_a_const_is_widen: |x: i8| x.widen::<i64>().cast_unsigned() => u64;
    usize_from_u8_in_a_const_is_widen: |x: u8| x.widen::<usize>() => usize;
    // A narrowing in the source's sign, then the sign.
    i32_from_u64_is_truncate_then_cast_signed: |x: u64| x.truncate::<u32>().cast_signed() => i32;
    u8_from_i32_is_truncate_then_cast_unsigned: |x: i32| x.truncate::<i8>().cast_unsigned() => u8;
    i32_from_usize_is_truncated_then_cast_signed: |x: usize| u32_truncated_from_usize(x).cast_signed() => i32;
    // Through the pointer width.
    u32_from_isize_is_truncated_then_cast_unsigned: |x: isize| i32_truncated_from_isize(x).cast_unsigned() => u32;
    // A float through an integer the lint names first.
    f64_from_i8_through_i32: |x: i8| f64::from(i32::from(x)) => f64;
}
