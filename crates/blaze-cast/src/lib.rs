//! blaze's named casts: the target of `blaze::as_cast`'s fix (docs/lints.md#as_cast) for each
//! `as` between numbers that std has no named operation for. Each is a `const fn`, inlined
//! always, whose name says what it does to a value, `<to>_<how>_from_<from>`:
//!
//! - no word: lossless on every target blaze supports (`usize_from_u32`), or the const twin of
//!   a `From` that a const context can't call (`f64_from_f32`, `u32_from_char`);
//! - `truncated`: keeps the low bits, as `as` does, where the source is wider on some target
//!   (`u32_truncated_from_usize` is lossless on 32-bit targets and keeps the low half on
//!   64-bit ones);
//! - `rounded`: the nearest value, ties to even, as `as` does from an integer to a float or
//!   from `f64` to `f32` (out of `f32`'s range is infinity);
//! - `saturated`: from a float to an integer, toward zero, clamped to the integer's range, NaN
//!   to 0, as `as` does.
//!
//! What std names is not here, and the fix writes std's: `T::from(x)` for a lossless widening
//! outside a const context, `x.widen::<T>()` inside one, `x.truncate::<T>()` for a narrowing
//! between fixed widths, `cast_signed` and `cast_unsigned` for a sign change, and the pointer
//! methods (`cast`, `cast_mut`, `expose_provenance`, `ptr::with_exposed_provenance`). A cast
//! from one integer to another of the other sign and another width is two of these: the width
//! in the source's sign, then the sign.
//!
//! The `truncated` names, with std's `truncate`, `cast_signed` and `cast_unsigned`, are where a
//! value can change: `blaze::truncating_cast` lists their calls, the places to decide between
//! wrapping and a checked conversion with a typed error.
//!
//! blaze runs on targets whose pointers are 32 or 64 bits wide (the wasm32 seed and the 64-bit
//! hosts); the assertion below refuses any other at compile time, so every lossless name here
//! is lossless wherever this crate builds.
//!
//! A leaf crate with no dependencies, so every crate of the workspace can name it, blaze-core
//! and the crates under it (blaze-mem, blaze-hash) included. bstd re-exports it as `bstd::cast`.

#![cfg_attr(not(test), no_std)]
#![feature(register_tool)]
#![register_tool(blaze)]
// The tests check the compositions `blaze::as_cast` writes with std's `widen` and `truncate`.
#![cfg_attr(test, feature(integer_widen_truncate))]
#![expect(
    blaze::as_cast,
    reason = "the one place a numeric `as` is written: each function is the cast it names"
)]

/// Every target blaze supports has 32- or 64-bit pointers: what makes the lossless names here
/// lossless.
const _: () = assert!(
    usize::BITS >= 32 && usize::BITS <= 64,
    "blaze-cast's lossless casts assume a pointer of 32 or 64 bits"
);

// Lossless between a pointer-sized integer and a fixed one, on 32- and 64-bit targets.

/// A `u32` as a `usize`: lossless, a pointer being at least 32 bits.
#[inline(always)]
pub const fn usize_from_u32(x: u32) -> usize {
    x as usize
}

/// An `i32` as an `isize`: lossless, a pointer being at least 32 bits.
#[inline(always)]
pub const fn isize_from_i32(x: i32) -> isize {
    x as isize
}

/// A `u16` as an `isize`: lossless, a pointer being at least 32 bits.
#[inline(always)]
pub const fn isize_from_u16(x: u16) -> isize {
    x as isize
}

/// A `usize` as a `u64`: lossless, a pointer being at most 64 bits.
#[inline(always)]
pub const fn u64_from_usize(x: usize) -> u64 {
    x as u64
}

/// A `usize` as a `u128`: lossless.
#[inline(always)]
pub const fn u128_from_usize(x: usize) -> u128 {
    x as u128
}

/// A `usize` as an `i128`: lossless, a pointer being at most 64 bits.
#[inline(always)]
pub const fn i128_from_usize(x: usize) -> i128 {
    x as i128
}

/// An `isize` as an `i64`: lossless, a pointer being at most 64 bits.
#[inline(always)]
pub const fn i64_from_isize(x: isize) -> i64 {
    x as i64
}

/// An `isize` as an `i128`: lossless.
#[inline(always)]
pub const fn i128_from_isize(x: isize) -> i128 {
    x as i128
}

// Truncating between a pointer-sized integer and a fixed one: lossless on the targets where
// the two are as wide, the low bits on the others.

/// The low 32 bits of a `usize`: all of it on a 32-bit target.
#[inline(always)]
pub const fn u32_truncated_from_usize(x: usize) -> u32 {
    x as u32
}

/// The low 32 bits of an `isize`, as an `i32`: all of it on a 32-bit target.
#[inline(always)]
pub const fn i32_truncated_from_isize(x: isize) -> i32 {
    x as i32
}

/// The low bits of a `u64` that a `usize` holds: all of it on a 64-bit target.
#[inline(always)]
pub const fn usize_truncated_from_u64(x: u64) -> usize {
    x as usize
}

/// The low bits of an `i64` that an `isize` holds: all of it on a 64-bit target.
#[inline(always)]
pub const fn isize_truncated_from_i64(x: i64) -> isize {
    x as isize
}

/// The low bits of a `u128` that a `usize` holds.
#[inline(always)]
pub const fn usize_truncated_from_u128(x: u128) -> usize {
    x as usize
}

/// The low bits of an `i128` that an `isize` holds.
#[inline(always)]
pub const fn isize_truncated_from_i128(x: i128) -> isize {
    x as isize
}

// The const twins of a `From` std has: a const context can't call a trait's function under
// the pinned nightly without `const_trait_impl`, so the fix writes these there.

/// `u8::from(b)` in a const context: 1 for `true`, 0 for `false`.
#[inline(always)]
pub const fn u8_from_bool(b: bool) -> u8 {
    b as u8
}

/// `u32::from(c)` in a const context: the code point.
#[inline(always)]
pub const fn u32_from_char(c: char) -> u32 {
    c as u32
}

/// `char::from(x)` in a const context: the code point `x`, U+0000 to U+00FF.
#[inline(always)]
pub const fn char_from_u8(x: u8) -> char {
    x as char
}

/// `f64::from(x)` in a const context: exact.
#[inline(always)]
pub const fn f64_from_f32(x: f32) -> f64 {
    x as f64
}

/// `f32::from(x)` in a const context: exact.
#[inline(always)]
pub const fn f32_from_u8(x: u8) -> f32 {
    x as f32
}

/// `f32::from(x)` in a const context: exact.
#[inline(always)]
pub const fn f32_from_i8(x: i8) -> f32 {
    x as f32
}

/// `f32::from(x)` in a const context: exact.
#[inline(always)]
pub const fn f32_from_u16(x: u16) -> f32 {
    x as f32
}

/// `f32::from(x)` in a const context: exact.
#[inline(always)]
pub const fn f32_from_i16(x: i16) -> f32 {
    x as f32
}

/// `f64::from(x)` in a const context: exact.
#[inline(always)]
pub const fn f64_from_u8(x: u8) -> f64 {
    x as f64
}

/// `f64::from(x)` in a const context: exact.
#[inline(always)]
pub const fn f64_from_i8(x: i8) -> f64 {
    x as f64
}

/// `f64::from(x)` in a const context: exact.
#[inline(always)]
pub const fn f64_from_u16(x: u16) -> f64 {
    x as f64
}

/// `f64::from(x)` in a const context: exact.
#[inline(always)]
pub const fn f64_from_i16(x: i16) -> f64 {
    x as f64
}

/// `f64::from(x)` in a const context: exact.
#[inline(always)]
pub const fn f64_from_u32(x: u32) -> f64 {
    x as f64
}

/// `f64::from(x)` in a const context: exact.
#[inline(always)]
pub const fn f64_from_i32(x: i32) -> f64 {
    x as f64
}

// An integer to a float that can't hold every value of it: the nearest, ties to even.

/// The `f32` nearest a `u32`.
#[inline(always)]
pub const fn f32_rounded_from_u32(x: u32) -> f32 {
    x as f32
}

/// The `f32` nearest an `i32`.
#[inline(always)]
pub const fn f32_rounded_from_i32(x: i32) -> f32 {
    x as f32
}

/// The `f32` nearest a `u64`.
#[inline(always)]
pub const fn f32_rounded_from_u64(x: u64) -> f32 {
    x as f32
}

/// The `f32` nearest an `i64`.
#[inline(always)]
pub const fn f32_rounded_from_i64(x: i64) -> f32 {
    x as f32
}

/// The `f32` nearest a `u128`; infinity past `f32::MAX`.
#[inline(always)]
pub const fn f32_rounded_from_u128(x: u128) -> f32 {
    x as f32
}

/// The `f32` nearest an `i128`.
#[inline(always)]
pub const fn f32_rounded_from_i128(x: i128) -> f32 {
    x as f32
}

/// The `f32` nearest a `usize`.
#[inline(always)]
pub const fn f32_rounded_from_usize(x: usize) -> f32 {
    x as f32
}

/// The `f32` nearest an `isize`.
#[inline(always)]
pub const fn f32_rounded_from_isize(x: isize) -> f32 {
    x as f32
}

/// The `f64` nearest a `u64`.
#[inline(always)]
pub const fn f64_rounded_from_u64(x: u64) -> f64 {
    x as f64
}

/// The `f64` nearest an `i64`.
#[inline(always)]
pub const fn f64_rounded_from_i64(x: i64) -> f64 {
    x as f64
}

/// The `f64` nearest a `u128`.
#[inline(always)]
pub const fn f64_rounded_from_u128(x: u128) -> f64 {
    x as f64
}

/// The `f64` nearest an `i128`.
#[inline(always)]
pub const fn f64_rounded_from_i128(x: i128) -> f64 {
    x as f64
}

/// The `f64` nearest a `usize`: exact on a 32-bit target.
#[inline(always)]
pub const fn f64_rounded_from_usize(x: usize) -> f64 {
    x as f64
}

/// The `f64` nearest an `isize`: exact on a 32-bit target.
#[inline(always)]
pub const fn f64_rounded_from_isize(x: isize) -> f64 {
    x as f64
}

/// The `f32` nearest an `f64`, ties to even: infinity beyond `f32::MAX`, NaN for NaN.
#[inline(always)]
pub const fn f32_rounded_from_f64(x: f64) -> f32 {
    x as f32
}

// A float to an integer: toward zero, clamped to the integer's range, NaN to 0.

/// An `f32` toward zero as a `u8`, clamped to `0..=u8::MAX`, NaN to 0.
#[inline(always)]
pub const fn u8_saturated_from_f32(x: f32) -> u8 {
    x as u8
}

/// An `f32` toward zero as a `u16`, clamped to `0..=u16::MAX`, NaN to 0.
#[inline(always)]
pub const fn u16_saturated_from_f32(x: f32) -> u16 {
    x as u16
}

/// An `f32` toward zero as a `u32`, clamped to `0..=u32::MAX`, NaN to 0.
#[inline(always)]
pub const fn u32_saturated_from_f32(x: f32) -> u32 {
    x as u32
}

/// An `f32` toward zero as a `u64`, clamped to `0..=u64::MAX`, NaN to 0.
#[inline(always)]
pub const fn u64_saturated_from_f32(x: f32) -> u64 {
    x as u64
}

/// An `f32` toward zero as a `u128`, clamped at 0, NaN to 0.
#[inline(always)]
pub const fn u128_saturated_from_f32(x: f32) -> u128 {
    x as u128
}

/// An `f32` toward zero as a `usize`, clamped to `0..=usize::MAX`, NaN to 0.
#[inline(always)]
pub const fn usize_saturated_from_f32(x: f32) -> usize {
    x as usize
}

/// An `f32` toward zero as an `i8`, clamped to `i8::MIN..=i8::MAX`, NaN to 0.
#[inline(always)]
pub const fn i8_saturated_from_f32(x: f32) -> i8 {
    x as i8
}

/// An `f32` toward zero as an `i16`, clamped to `i16::MIN..=i16::MAX`, NaN to 0.
#[inline(always)]
pub const fn i16_saturated_from_f32(x: f32) -> i16 {
    x as i16
}

/// An `f32` toward zero as an `i32`, clamped to `i32::MIN..=i32::MAX`, NaN to 0.
#[inline(always)]
pub const fn i32_saturated_from_f32(x: f32) -> i32 {
    x as i32
}

/// An `f32` toward zero as an `i64`, clamped to `i64::MIN..=i64::MAX`, NaN to 0.
#[inline(always)]
pub const fn i64_saturated_from_f32(x: f32) -> i64 {
    x as i64
}

/// An `f32` toward zero as an `i128`, clamped to `i128`'s range, NaN to 0.
#[inline(always)]
pub const fn i128_saturated_from_f32(x: f32) -> i128 {
    x as i128
}

/// An `f32` toward zero as an `isize`, clamped to `isize::MIN..=isize::MAX`, NaN to 0.
#[inline(always)]
pub const fn isize_saturated_from_f32(x: f32) -> isize {
    x as isize
}

/// An `f64` toward zero as a `u8`, clamped to `0..=u8::MAX`, NaN to 0.
#[inline(always)]
pub const fn u8_saturated_from_f64(x: f64) -> u8 {
    x as u8
}

/// An `f64` toward zero as a `u16`, clamped to `0..=u16::MAX`, NaN to 0.
#[inline(always)]
pub const fn u16_saturated_from_f64(x: f64) -> u16 {
    x as u16
}

/// An `f64` toward zero as a `u32`, clamped to `0..=u32::MAX`, NaN to 0.
#[inline(always)]
pub const fn u32_saturated_from_f64(x: f64) -> u32 {
    x as u32
}

/// An `f64` toward zero as a `u64`, clamped to `0..=u64::MAX`, NaN to 0.
#[inline(always)]
pub const fn u64_saturated_from_f64(x: f64) -> u64 {
    x as u64
}

/// An `f64` toward zero as a `u128`, clamped to `0..=u128::MAX`, NaN to 0.
#[inline(always)]
pub const fn u128_saturated_from_f64(x: f64) -> u128 {
    x as u128
}

/// An `f64` toward zero as a `usize`, clamped to `0..=usize::MAX`, NaN to 0.
#[inline(always)]
pub const fn usize_saturated_from_f64(x: f64) -> usize {
    x as usize
}

/// An `f64` toward zero as an `i8`, clamped to `i8::MIN..=i8::MAX`, NaN to 0.
#[inline(always)]
pub const fn i8_saturated_from_f64(x: f64) -> i8 {
    x as i8
}

/// An `f64` toward zero as an `i16`, clamped to `i16::MIN..=i16::MAX`, NaN to 0.
#[inline(always)]
pub const fn i16_saturated_from_f64(x: f64) -> i16 {
    x as i16
}

/// An `f64` toward zero as an `i32`, clamped to `i32::MIN..=i32::MAX`, NaN to 0.
#[inline(always)]
pub const fn i32_saturated_from_f64(x: f64) -> i32 {
    x as i32
}

/// An `f64` toward zero as an `i64`, clamped to `i64::MIN..=i64::MAX`, NaN to 0.
#[inline(always)]
pub const fn i64_saturated_from_f64(x: f64) -> i64 {
    x as i64
}

/// An `f64` toward zero as an `i128`, clamped to `i128::MIN..=i128::MAX`, NaN to 0.
#[inline(always)]
pub const fn i128_saturated_from_f64(x: f64) -> i128 {
    x as i128
}

/// An `f64` toward zero as an `isize`, clamped to `isize::MIN..=isize::MAX`, NaN to 0.
#[inline(always)]
pub const fn isize_saturated_from_f64(x: f64) -> isize {
    x as isize
}

#[cfg(test)]
mod tests;
