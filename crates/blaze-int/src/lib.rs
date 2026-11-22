//! Integer conversions that say what they do, in place of `as` (after Zig, where `@intCast` is
//! checked, `@truncate` drops bits on purpose and wrapping and saturating arithmetic are
//! operators of their own). `x as u32` means one of five things, and the reader has to work out
//! which; each has a name here:
//!
//! | method | what happens | exists for |
//! |---|---|---|
//! | [`widen_to`](Int::widen_to) | the same value in a wider type | only pairs where no value of the source is lost on any supported target: a compile error otherwise, because the impl does not exist |
//! | [`narrow`](Int::narrow) | the same value, or a [`Narrowed`] naming the value and both types | every other pair |
//! | [`narrow_or_stop`](Int::narrow_or_stop) | the same value, or a panic naming it and both types, at the caller (`#[track_caller]`) | the pairs of `narrow`, where a value that does not fit is a bug: say the invariant in a comment beside it |
//! | [`truncate_to`](Int::truncate_to) | the low bits, kept on purpose (hash mixing, codecs) | pairs whose target has no more bits than the source on every supported target |
//! | [`saturate`](Int::saturate) | the value, clamped to the target's range | the pairs of `narrow` |
//! | `core`'s `cast_signed`, `cast_unsigned` (stable, `const`) | the same bits read with the other sign | pairs of equal width |
//!
//! `use blaze_int::Int` brings the methods; `x.widen_to::<u64>()` names the target where
//! inference does not. A conversion that `as` does in one step and that is two things, such as
//! `i32 as u64` (sign-extend, then reinterpret), is written as its two steps here:
//! `x.widen_to::<i64>().cast_unsigned()`, or `x.cast_unsigned().widen_to::<u64>()` to
//! zero-extend.
//!
//! Why `widen_to` and `truncate_to`, not `widen` and `truncate`: the pinned nightly's `core` has
//! unstable inherent `widen` and `truncate` (`integer_widen_truncate`, rust-lang/rust#154330)
//! for same-sign pairs only and with `usize` taken as possibly 16 bits (no `u32 -> usize`, no
//! `usize -> u64`, the two the tree needs most). A trait method of the same name draws
//! `unstable_name_collisions` at every call today, and once they are stable the inherent method
//! wins and every pair it lacks stops compiling. `core`'s `cast_signed`/`cast_unsigned` are
//! stable and are this crate's reinterpretations: one home, so none is written here.
//!
//! Every conversion is also a `const fn` of [`Cast`], the one home of the code:
//! `Cast::<u64, u32>::narrow(x)`, `Cast::<u32, usize>::widen(x)`; the traits call it. Trait
//! methods cannot be `const` on stable Rust, so a `const` item or `const fn` uses `Cast`. The
//! one difference: `Cast::narrow_or_stop`'s panic names the two types but not the value (a
//! `const fn` cannot format one); the trait method's names all three.
//!
//! `usize` and `isize` are 32 or 64 bits: a pair with one of them is a widening only where it
//! loses nothing at both widths, so `u32 -> usize` and `usize -> u64` widen, while `u64 ->
//! usize` and `usize -> u32` narrow (and truncate). The widenings that need a pointer of at
//! least 32 bits (`u32 -> usize`, `u16 -> isize`, `i32 -> isize`, and [`idx`]) are left out
//! on a 16-bit target, which blaze does not build for.
//!
//! Ids in the tree are 32-bit and lengths are `usize`: [`idx`] makes an index of an id, and
//! [`len32`] an id or a stored length of a length.
//!
//! Overflow in arithmetic is not this crate's: `wrapping_*`, `checked_*`, `saturating_*` and
//! `overflowing_*` of `core` already name what is meant, and the profiles' `overflow-checks`
//! decide what a plain `+` does.
//!
//! The crate also holds time as a number ([`time`]): [`Nanos`], [`Deadline`] and the [`Clock`]
//! trait a core is handed, since this is the one crate every other may depend on.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core, blaze::std_use, blaze::implicit_allocator, blaze::process_static)]
#![forbid(unsafe_code)]

// The tests' `format!` and `catch_unwind`: `core` has neither.
#[cfg(test)]
extern crate std;

use core::{fmt, marker::PhantomData};

/// The conversions from `S` to `T` as `const fn`s, one inherent impl per pair the table has:
/// `Cast::<u64, u32>::narrow(x)`. Never made: it only names the pair.
pub struct Cast<S, T>(PhantomData<fn(S) -> T>);

/// A value that did not fit, as wide as any integer: unsigned when its type is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Value {
    Unsigned(u128),
    Signed(i128),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Unsigned(it) => write!(f, "{it}"),
            Value::Signed(it) => write!(f, "{it}"),
        }
    }
}

/// A narrowing that failed: the value, its type and the type it does not fit in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Narrowed {
    pub value: Value,
    /// The source type's name (`"u64"`).
    pub from: &'static str,
    /// The target type's name (`"u32"`).
    pub to: &'static str,
}

impl Narrowed {
    /// Panics with this failure, at the caller: what [`Int::narrow_or_stop`] does when the
    /// value does not fit.
    #[cold]
    #[track_caller]
    pub fn stop(self) -> ! {
        panic!("blaze_int: narrow_or_stop: {self} (an invariant the caller states was broken)")
    }
}

impl fmt::Display for Narrowed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the {} {} does not fit in {}", self.from, self.value, self.to)
    }
}

impl core::error::Error for Narrowed {}

/// `Self` to `T` with no value lost. Implemented only for such pairs.
pub trait Widen<T>: Copy {
    fn widened(self) -> T;
}

/// `Self` to `T`, checked. Implemented for every pair that is not a [`Widen`] (and not `T`
/// to itself).
pub trait Narrow<T>: Copy {
    fn narrowed(self) -> Result<T, Narrowed>;
}

/// `Self`'s low bits as a `T`. Implemented where `T` has no more bits than `Self`.
pub trait Truncate<T>: Copy {
    fn truncated(self) -> T;
}

/// `Self` clamped to `T`'s range. Implemented for the pairs of [`Narrow`].
pub trait Saturate<T>: Copy {
    fn saturated(self) -> T;
}

/// The conversions as methods with the target as a type parameter: `x.widen_to::<u64>()`,
/// `x.narrow::<u32>()?`. Implemented for every primitive integer.
pub trait Int: Copy {
    /// The same value in a wider type; the pair must lose nothing.
    #[inline(always)]
    fn widen_to<T>(self) -> T
    where
        Self: Widen<T>,
    {
        self.widened()
    }

    /// The same value, or a [`Narrowed`] naming it and both types.
    #[inline(always)]
    fn narrow<T>(self) -> Result<T, Narrowed>
    where
        Self: Narrow<T>,
    {
        self.narrowed()
    }

    /// The same value; one that does not fit is a bug, and panics at the caller naming the
    /// value and both types. Say beside the call which invariant makes it fit.
    #[inline(always)]
    #[track_caller]
    fn narrow_or_stop<T>(self) -> T
    where
        Self: Narrow<T>,
    {
        match self.narrowed() {
            Ok(it) => it,
            Err(e) => e.stop(),
        }
    }

    /// The low bits, on purpose.
    #[inline(always)]
    fn truncate_to<T>(self) -> T
    where
        Self: Truncate<T>,
    {
        self.truncated()
    }

    /// The value clamped to `T`'s range.
    #[inline(always)]
    fn saturate<T>(self) -> T
    where
        Self: Saturate<T>,
    {
        self.saturated()
    }
}

/// An id as an index: `u32` to `usize`, which loses nothing where `usize` has 32 bits or more.
#[cfg(any(target_pointer_width = "32", target_pointer_width = "64"))]
#[inline(always)]
pub const fn idx(id: u32) -> usize {
    Cast::<u32, usize>::widen(id)
}

/// A length as a 32-bit id or stored length, or a [`Narrowed`] when it has more than
/// `u32::MAX` elements.
#[inline(always)]
pub const fn len32(len: usize) -> Result<u32, Narrowed> {
    Cast::<usize, u32>::narrow(len)
}

/// The table: for each primitive integer, its sign and the targets of each conversion. Calls
/// `$then!` with it, so the impls and the tests are made from the same rows. A source's
/// `widen`, `widen_ptr` and `narrow` together name each other integer type once; `truncate` is
/// the part of `narrow` whose target has no more bits on a 32-bit or a 64-bit target.
/// `widen_ptr` are the widenings that need `usize`/`isize` of at least 32 bits.
macro_rules! table {
    ($then:ident) => {
        $then! {
            u8 unsigned: widen [u16, u32, u64, u128, usize, i16, i32, i64, i128, isize] widen_ptr []
                narrow [i8] truncate [];
            u16 unsigned: widen [u32, u64, u128, usize, i32, i64, i128] widen_ptr [isize]
                narrow [u8, i8, i16] truncate [u8, i8];
            u32 unsigned: widen [u64, u128, i64, i128] widen_ptr [usize]
                narrow [u8, u16, i8, i16, i32, isize] truncate [u8, u16, i8, i16];
            u64 unsigned: widen [u128, i128] widen_ptr []
                narrow [u8, u16, u32, usize, i8, i16, i32, i64, isize]
                truncate [u8, u16, u32, usize, i8, i16, i32, isize];
            u128 unsigned: widen [] widen_ptr []
                narrow [u8, u16, u32, u64, usize, i8, i16, i32, i64, i128, isize]
                truncate [u8, u16, u32, u64, usize, i8, i16, i32, i64, isize];
            usize unsigned: widen [u64, u128, i128] widen_ptr []
                narrow [u8, u16, u32, i8, i16, i32, i64, isize] truncate [u8, u16, u32, i8, i16, i32];
            i8 signed: widen [i16, i32, i64, i128, isize] widen_ptr []
                narrow [u8, u16, u32, u64, u128, usize] truncate [];
            i16 signed: widen [i32, i64, i128, isize] widen_ptr []
                narrow [u8, u16, u32, u64, u128, usize, i8] truncate [u8, i8];
            i32 signed: widen [i64, i128] widen_ptr [isize]
                narrow [u8, u16, u32, u64, u128, usize, i8, i16] truncate [u8, u16, i8, i16];
            i64 signed: widen [i128] widen_ptr []
                narrow [u8, u16, u32, u64, u128, usize, i8, i16, i32, isize]
                truncate [u8, u16, u32, usize, i8, i16, i32, isize];
            i128 signed: widen [] widen_ptr []
                narrow [u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, isize]
                truncate [u8, u16, u32, u64, usize, i8, i16, i32, i64, isize];
            isize signed: widen [i64, i128] widen_ptr []
                narrow [u8, u16, u32, u64, u128, usize, i8, i16, i32] truncate [u8, u16, u32, i8, i16, i32];
        }
    };
}

macro_rules! impls {
    ($(
        $s:ident $sign:ident: widen [$($w:ident),*] widen_ptr [$($p:ident),*]
            narrow [$($n:ident),*] truncate [$($t:ident),*];
    )*) => {
        $(
            impl Int for $s {}
            $( widen! { $s => $w } )*
            $(
                #[cfg(any(target_pointer_width = "32", target_pointer_width = "64"))]
                widen! { $s => $p }
            )*
            $( narrow! { $s $sign => $n } )*
            $( truncate! { $s => $t } )*
        )*
    };
}

macro_rules! widen {
    ($s:ident => $t:ident) => {
        impl Cast<$s, $t> {
            /// The same value: every value of the source fits.
            #[inline(always)]
            pub const fn widen(x: $s) -> $t {
                x as $t
            }
        }

        impl Widen<$t> for $s {
            #[inline(always)]
            fn widened(self) -> $t {
                Cast::<$s, $t>::widen(self)
            }
        }
    };
}

macro_rules! narrow {
    ($s:ident unsigned => $t:ident) => {
        // `x as u128` of a `u128` source is the value itself: the table's one body for all.
        #[allow(clippy::unnecessary_cast)]
        impl Cast<$s, $t> {
            /// The same value, or a [`Narrowed`] naming it.
            #[inline(always)]
            pub const fn narrow(x: $s) -> Result<$t, Narrowed> {
                // An unsigned value is never under the target's minimum; the target's maximum
                // is positive, so its `u128` is exact.
                if x as u128 <= <$t>::MAX as u128 {
                    Ok(x as $t)
                } else {
                    Err(Narrowed { value: Value::Unsigned(x as u128), from: stringify!($s), to: stringify!($t) })
                }
            }

            /// The value clamped to the target's range.
            #[inline(always)]
            pub const fn saturate(x: $s) -> $t {
                match Self::narrow(x) {
                    Ok(it) => it,
                    Err(_) => <$t>::MAX,
                }
            }
        }
        narrow! { @common $s => $t }
    };
    ($s:ident signed => $t:ident) => {
        // `x as i128` of an `i128` source is the value itself: the table's one body for all.
        #[allow(clippy::unnecessary_cast)]
        impl Cast<$s, $t> {
            /// The same value, or a [`Narrowed`] naming it.
            #[inline(always)]
            pub const fn narrow(x: $s) -> Result<$t, Narrowed> {
                let wide = x as i128;
                // The target's minimum is 0 or negative and its maximum positive, so both are
                // exact in `i128` and `u128`.
                let fits = if wide < 0 { wide >= <$t>::MIN as i128 } else { wide as u128 <= <$t>::MAX as u128 };
                if fits {
                    Ok(x as $t)
                } else {
                    Err(Narrowed { value: Value::Signed(wide), from: stringify!($s), to: stringify!($t) })
                }
            }

            /// The value clamped to the target's range.
            #[inline(always)]
            pub const fn saturate(x: $s) -> $t {
                match Self::narrow(x) {
                    Ok(it) => it,
                    Err(_) if x < 0 => <$t>::MIN,
                    Err(_) => <$t>::MAX,
                }
            }
        }
        narrow! { @common $s => $t }
    };
    (@common $s:ident => $t:ident) => {
        impl Cast<$s, $t> {
            /// The same value; one that does not fit is a bug. The panic names both types (a
            /// `const fn` cannot format the value; [`Int::narrow_or_stop`] names it too).
            #[inline(always)]
            #[track_caller]
            pub const fn narrow_or_stop(x: $s) -> $t {
                match Self::narrow(x) {
                    Ok(it) => it,
                    Err(_) => panic!(concat!(
                        "blaze_int: narrow_or_stop: a ",
                        stringify!($s),
                        " value does not fit in ",
                        stringify!($t),
                        " (an invariant the caller states was broken)"
                    )),
                }
            }
        }

        impl Narrow<$t> for $s {
            #[inline(always)]
            fn narrowed(self) -> Result<$t, Narrowed> {
                Cast::<$s, $t>::narrow(self)
            }
        }

        impl Saturate<$t> for $s {
            #[inline(always)]
            fn saturated(self) -> $t {
                Cast::<$s, $t>::saturate(self)
            }
        }
    };
}

macro_rules! truncate {
    ($s:ident => $t:ident) => {
        impl Cast<$s, $t> {
            /// The low bits of the source, on purpose.
            #[inline(always)]
            pub const fn truncate(x: $s) -> $t {
                x as $t
            }
        }

        impl Truncate<$t> for $s {
            #[inline(always)]
            fn truncated(self) -> $t {
                Cast::<$s, $t>::truncate(self)
            }
        }
    };
}

table!(impls);

// Time as a number: `Nanos`, `Deadline` and the handed-in `Clock` (the module's doc).
pub mod time;

pub use time::{Clock, Deadline, Nanos};

#[cfg(test)]
mod tests;
