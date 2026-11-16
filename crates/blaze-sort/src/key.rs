//! [`RadixKey`]: a type whose values map one-to-one, and in order, onto fixed-width unsigned
//! integers, so it sorts by counting digits instead of comparing, and elements that compare
//! equal are the same value, so a stable sort and an unstable one give the same bytes.
//!
//! | type | image | order |
//! |---|---|---|
//! | `u8` .. `u128`, `usize` | itself | the integer's |
//! | `i8` .. `i128`, `isize` | its bits with the sign bit flipped | the integer's |
//! | `bool` | 0, 1 | `false < true` |
//! | `char` | its scalar value | `char`'s |
//! | `f32`, `f64` | its bits, all flipped when negative, else the sign bit set | `total_cmp`'s: `-NaN < -inf < .. < -0.0 < +0.0 < .. < +inf < +NaN` |
//! | `NonZero<_>` of the above | the integer's image | the integer's |
//! | `[K; N]`, `(A, ..)` up to four | the images end to end, the first most significant | lexicographic, as derived `Ord` is |
//! | an id over a `u32` | [`radix_key!`] | the id's own |
//!
//! # Why a marker trait, not reflection or plain data
//!
//! "Plain old data" is not the property: a `(key, payload)` struct with a hand-written `Ord`
//! on the key is plain data, and a stable sort of it is observably not an unstable one.
//! Reflection (`core::mem::type_info`) sees fields, not whether `Ord` is derived or written
//! by hand, so it cannot tell that struct from one whose order is all its fields; and it
//! panics the pinned compiler on tuples. The property is the type's promise, so it is a
//! trait: implemented here for the types whose order is known, and by a type's own crate,
//! with [`radix_key!`], where the author knows it.
//!
//! The trait is a `#[rustc_specialization_trait]`, as blaze-hash's `PlainKey` is, so the
//! generic front doors ([`sort_stable_in`](crate::sort_stable_in),
//! [`sort_unstable`](crate::sort_unstable)) take the radix path for these types without the
//! caller asking. Every impl is always applicable: no lifetime, no type parameter twice, bounds
//! only on other `RadixKey`s.

use core::cmp::Ordering;
use core::num::NonZero;

use crate::plant::{self, Plant};

/// A type whose values have a fixed-width unsigned image that is one-to-one and preserves the
/// type's order, so it sorts by the image's digits.
///
/// The image is `BYTES` bytes wide. [`image`](RadixKey::image) gives its low 128 bits, all of
/// it when `BYTES <= 16`; [`byte`](RadixKey::byte) gives any byte of it (0 the least
/// significant) and [`cmp_image`](RadixKey::cmp_image) compares two whole images: the wider
/// keys override both.
///
/// The order a sort here gives is the images' order. For a type that is `Ord` it is `Ord`'s;
/// for `f32` and `f64`, which are not, it is `total_cmp`'s.
///
/// # Safety
///
/// An implementor promises, for every value of the type:
///
/// - the image is a pure function of the value: two calls on one value, or on two copies of
///   it, give one image. The sorts count each key's digits in one pass and place it by them in
///   another, writing without a bounds check, so an image that changed between the two would
///   write outside the slice;
/// - two values have one image exactly when they are the same value, nothing about them told
///   apart by any code (so a stable and an unstable sort give the same bytes);
/// - where the type is `Ord`, `a.cmp(b)` is the images' order (so the front doors may sort an
///   `Ord` type by its images and call no `cmp`);
/// - `byte` and `cmp_image` agree with the image as their documentation says.
#[rustc_specialization_trait]
pub unsafe trait RadixKey: Copy {
    /// The image's width in bytes.
    const BYTES: usize;

    /// The image's low 128 bits; the whole image when `BYTES <= 16`.
    fn image(&self) -> u128;

    /// Byte `at` of the image, 0 the least significant; `at < BYTES`.
    #[inline(always)]
    fn byte(&self, at: usize) -> u8 {
        (self.image() >> (8 * at)) as u8
    }

    /// The order of the two whole images.
    #[inline(always)]
    fn cmp_image(&self, other: &Self) -> Ordering {
        self.image().cmp(&other.image())
    }
}

macro_rules! unsigned {
    ($($t:ty),*) => {$(
        // SAFETY: an unsigned integer is its own image: pure, one-to-one and in its order.
        unsafe impl RadixKey for $t {
            const BYTES: usize = size_of::<$t>();
            #[inline(always)]
            fn image(&self) -> u128 {
                *self as u128
            }
        }
    )*};
}

unsigned!(u8, u16, u32, u64, u128, usize);

macro_rules! signed {
    ($($t:ty => $u:ty),*) => {$(
        // SAFETY: flipping the sign bit maps `MIN..=MAX` onto `0..=<$u>::MAX` in order, one to
        // one: two's complement with the sign bit flipped is offset binary.
        unsafe impl RadixKey for $t {
            const BYTES: usize = size_of::<$t>();
            #[inline(always)]
            fn image(&self) -> u128 {
                let bits = self.cast_unsigned();
                if plant::on(Plant::SignNotFlipped) {
                    return bits as u128;
                }
                (bits ^ (1 << (<$u>::BITS - 1))) as u128
            }
        }
    )*};
}

signed!(i8 => u8, i16 => u16, i32 => u32, i64 => u64, i128 => u128, isize => usize);

// SAFETY: `false` is 0 and `true` is 1, in `bool`'s order.
unsafe impl RadixKey for bool {
    const BYTES: usize = 1;
    #[inline(always)]
    fn image(&self) -> u128 {
        u128::from(*self)
    }
}

// SAFETY: a `char` is its scalar value, and `char`'s order is that value's.
unsafe impl RadixKey for char {
    const BYTES: usize = 4;
    #[inline(always)]
    fn image(&self) -> u128 {
        u128::from(u32::from(*self))
    }
}

macro_rules! float {
    ($($t:ty => $u:ty),*) => {$(
        // SAFETY: `total_cmp`'s own mapping: a negative float's bits all flipped, a positive
        // one's sign bit set. Negatives then run from `-NaN` up to `-0.0` below every
        // positive, from `+0.0` up to `+NaN`; each bit pattern is its own image, so one to one
        // (the two zeros and every NaN payload differ, as they do under `total_cmp`).
        unsafe impl RadixKey for $t {
            const BYTES: usize = size_of::<$t>();
            #[inline(always)]
            fn image(&self) -> u128 {
                let mut bits = self.to_bits();
                if plant::on(Plant::ZerosMerged) && *self == 0.0 {
                    bits = 0;
                }
                // All ones when the sign is set, else zero; or the sign bit.
                let flip = (bits.cast_signed() >> (<$u>::BITS - 1)).cast_unsigned() | (1 << (<$u>::BITS - 1));
                (bits ^ flip) as u128
            }
        }
    )*};
}

float!(f32 => u32, f64 => u64);

macro_rules! nonzero {
    ($($t:ty),*) => {$(
        // SAFETY: a `NonZero` is its integer, ordered as it is: the integer's image.
        unsafe impl RadixKey for NonZero<$t> {
            const BYTES: usize = size_of::<$t>();
            #[inline(always)]
            fn image(&self) -> u128 {
                self.get().image()
            }
        }
    )*};
}

nonzero!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize);

/// `high`'s image above `low_bytes` bytes of `low`'s: the low 128 bits of the two end to end.
#[inline(always)]
fn join(high: u128, low_bytes: usize, low: u128) -> u128 {
    high.unbounded_shl((8 * low_bytes) as u32) | low
}

// SAFETY: the images end to end, each element's of `T::BYTES`, the first most significant:
// pure as each is; one to one, as each part is and the parts have fixed widths; and in the
// lexicographic order, which is the order of the concatenation, as `[T; N]`'s `Ord` is.
// `byte` reads the element that holds byte `at`, `cmp_image` compares element by element.
unsafe impl<T: RadixKey, const N: usize> RadixKey for [T; N] {
    const BYTES: usize = T::BYTES * N;

    #[inline(always)]
    fn image(&self) -> u128 {
        let mut image = 0u128;
        for item in self {
            image = join(image, T::BYTES, item.image());
        }
        image
    }

    #[inline(always)]
    fn byte(&self, at: usize) -> u8 {
        self[N - 1 - at / T::BYTES].byte(at % T::BYTES)
    }

    #[inline]
    fn cmp_image(&self, other: &Self) -> Ordering {
        for (a, b) in self.iter().zip(other) {
            match a.cmp_image(b) {
                Ordering::Equal => {}
                unequal @ (Ordering::Less | Ordering::Greater) => return unequal,
            }
        }
        Ordering::Equal
    }
}

/// `RadixKey` for a tuple: the parts' images end to end, the first most significant.
macro_rules! tuple {
    ($($name:ident . $at:tt),+) => {
        // SAFETY: as the array's: the parts' images end to end at fixed widths, in the
        // lexicographic order derived `Ord` gives a tuple.
        unsafe impl<$($name: RadixKey),+> RadixKey for ($($name,)+) {
            const BYTES: usize = 0 $(+ $name::BYTES)+;

            #[inline(always)]
            fn image(&self) -> u128 {
                let mut image = 0u128;
                $(image = join(image, $name::BYTES, self.$at.image());)+
                image
            }

            #[inline(always)]
            fn byte(&self, at: usize) -> u8 {
                // The parts from the last (least significant) to the first.
                let mut at = at;
                tuple!(@byte self, at, [] $($name . $at),+)
            }

            #[inline]
            fn cmp_image(&self, other: &Self) -> Ordering {
                Ordering::Equal $(.then_with(|| self.$at.cmp_image(&other.$at)))+
            }
        }
    };
    // Reverse the parts, then read the byte from the last part first.
    (@byte $self:ident, $at_var:ident, [$($rev:ident . $rat:tt),*] $name:ident . $at:tt $(, $rest:ident . $rrest:tt)*) => {
        tuple!(@byte $self, $at_var, [$name . $at $(, $rev . $rat)*] $($rest . $rrest),*)
    };
    (@byte $self:ident, $at_var:ident, [$($rev:ident . $rat:tt),*]) => {{
        $(
            if $at_var < $rev::BYTES {
                return $self.$rat.byte($at_var);
            }
            $at_var -= $rev::BYTES;
        )*
        let _ = $at_var;
        unreachable!("blaze_sort: a tuple key's byte past its width")
    }};
}

tuple!(A.0);
tuple!(A.0, B.1);
tuple!(A.0, B.1, C.2);
tuple!(A.0, B.1, C.2, D.3);

/// `RadixKey` for a type that is one integer key in order: a typed id over a `u32`, an index,
/// a timestamp. The `unsafe` is written at the call, as the promise is the caller's: the
/// expression is a pure function of the value, one-to-one, and the type's `Ord` (if any) is
/// the order of what it gives.
///
/// ```ignore
/// #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
/// struct FileId(u32);
/// blaze_sort::radix_key!(unsafe FileId => u32, |id| id.0);
/// ```
///
/// The crate that writes it turns on `#![feature(min_specialization)]`, as an impl of any
/// `#[rustc_specialization_trait]` asks.
#[macro_export]
macro_rules! radix_key {
    (unsafe $ty:ty => $inner:ty, |$value:ident| $image:expr) => {
        // SAFETY: the caller's promise, written `unsafe` at the macro: `$image` is pure and
        // one-to-one, and `$ty`'s order is `$inner`'s order of it.
        unsafe impl $crate::RadixKey for $ty {
            const BYTES: usize = <$inner as $crate::RadixKey>::BYTES;
            #[inline(always)]
            fn image(&self) -> u128 {
                let $value: &$ty = self;
                let inner: $inner = $image;
                <$inner as $crate::RadixKey>::image(&inner)
            }
        }
    };
}
