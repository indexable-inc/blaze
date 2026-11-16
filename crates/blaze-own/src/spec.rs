//! The fast paths: where a generic method would go element by element and one block copy, one
//! byte search or one reservation does the same, the collections take it without the caller
//! asking.
//!
//! | path | generic | fast, when | how |
//! |---|---|---|---|
//! | clone a slice's values into room (`extend_from_slice`, `from_slice_in`, `insert_many`, `Clone` of `Vec` and `VecDeque`) | one `clone` and one write each | one `copy_nonoverlapping`, for `T: TrivialClone` | [`CloneSlice`], specialized |
//! | `Soa` clone and `clone_in` | one row at a time, a clone per field | one `copy_nonoverlapping` per column, when every field is `TrivialClone` | [`CloneSlice::trivial`], read by `CloneColumns::clone_rows` |
//! | copy into another allocator (`clone_in` of `Vec`, `[T]`, `VecDeque`, so of `SortedMap`, `SortedSet`, `Packed`, `Box<[T]>`) | one `try_clone_in` and one write each | one `copy_nonoverlapping`, for `T: Copy` | `CloneIn::try_clone_slice_in`, overridden by the `Copy` blanket impl: no specialization |
//! | `Vec::extend`, so `from_iter_in` and `collect_in` | a capacity check per value, growing by the lower bound | reserved once, written with no check, for a `TrustedLen` iterator | `vec::SpecExtend`, specialized |
//! | `Vec::extend` from another vector's `into_iter` | as above | one `copy_nonoverlapping` | `vec::SpecExtend`, specialized |
//! | `Vec::extend` from `&[T]`'s `iter()` (`Extend<&T>`) | a copy per value | one `copy_nonoverlapping` | `vec::SpecExtendRef`, specialized |
//! | `Vec::contains`, `Vec::position` | `==` per value | blaze-memchr's vector search, for `u8`, `i8`, `bool` | [`Find`], specialized |
//! | `VecDeque::append` | (was) one copy per value | two to four `copy_nonoverlapping`, for every `T`: a move is bitwise anyway | `deque`: no specialization |
//!
//! Each path is tested equal to the generic one on random input, the generic one reached
//! through a wrapper type that hides the marker (`tests`, `specialized_*`), and timed in
//! `examples/alloc_cost.rs`.
//!
//! # Within `min_specialization`
//!
//! Each specializing impl is in `min_specialization`'s sound subset, as std's own are: it is
//! "always applicable", so choosing it never depends on lifetimes (the unsound part of full
//! `specialization`, where an impl could hold for `'static` and not for `'a`, which codegen,
//! erasing lifetimes, cannot tell): no `'static`, no type parameter standing twice for the
//! base impl's parameters, and no bound beyond the base impl's but a specialization trait.
//! The ones used, and why each is sound here:
//!
//! - `TrivialClone` (core, `#[rustc_specialization_trait]`, feature `trivial_clone`): an
//!   `unsafe` promise that `clone` is a bitwise copy, which `#[derive(Clone, Copy)]` and core's
//!   `Copy` types make. Core marks it sound to specialize on although an impl may have lifetime
//!   bounds: a `clone` cannot differ by lifetime, so where it holds for one it holds for all.
//!   `Copy` is no specialization marker on this nightly (rustc: "cannot specialize on trait
//!   `Copy`"; std moved to `TrivialClone`), and a marker of our own would cover only the types
//!   given it by hand, where this one comes with every derive. Tuples have no impl, so a `Vec` of
//!   tuples clones each value; a `Soa` asks per field.
//! - `TrustedLen` (core, `#[rustc_specialization_trait]`): an `unsafe` promise that `size_hint`
//!   is exact. Implemented here only for iterators whose `size_hint` is a count they keep
//!   (`vec::IntoIter`, `vec::Drain`, the ring's, the tree's and `Soa`'s), each with no
//!   condition on lifetimes.
//! - None for [`Find`]: its fast impls are for the concrete types `u8`, `i8` and `bool`, always
//!   applicable.
//!
//! The copy into another allocator needs no marker: [`CloneIn`](crate::CloneIn)'s hidden
//! provided method `try_clone_slice_in` copies one value at a time, and the blanket impl for
//! `T: Copy`, where `Self::In` is `T` by its own definition, overrides it with one block copy.
//! Coherence makes that impl the only one for any `Copy` type, so nothing is assumed.
//!
//! # Already fast, so not specialized here
//!
//! - `==` and `cmp` of vectors and strings: `Vec` compares through its slices and `String`
//!   through `str`, and core's slice impls compare integers of one width (`BytewiseEq`) by
//!   `memcmp` for `==`, and `u8` by `memcmp` for `cmp`. Wider integers order by value, which on
//!   a little-endian machine is not their bytes' order, so `memcmp` would be wrong for them.
//! - Dropping a vector of values with no drop glue: `drop_in_place` of a slice of them is no
//!   code at all (`needs_drop` is false at compile time), so nothing walks the values.
//! - `dedup`: it compares neighbours, not a needle, so no byte search fits; for `Copy` values
//!   its loop is one load, one compare and one store per value already.
//! - `contains` of wider integers: blaze-memchr searches bytes; core's loop compares a value per
//!   step.

use core::{clone::TrivialClone, ptr};

/// Clones of a slice's values, written into room the caller has: the one place a collection
/// clones values into its own buffer, a block copy when they are `TrivialClone`.
pub(crate) trait CloneSlice: Clone {
    /// Writes clones of `values` at `dst.add(*count)`, `dst.add(*count + 1)`, .., adding one to
    /// `*count` as each lands, so a clone that panics leaves `*count` saying how many did.
    ///
    /// # Safety
    ///
    /// The `values.len()` slots from `dst.add(*count)` are in one allocation, hold no value and
    /// do not overlap `values`.
    unsafe fn clone_to(values: &[Self], dst: *mut Self, count: &mut usize);

    /// Whether a clone is a bitwise copy (`TrivialClone`), so a caller may copy bytes itself.
    fn trivial() -> bool;
}

impl<T: Clone> CloneSlice for T {
    #[inline]
    default unsafe fn clone_to(values: &[T], dst: *mut T, count: &mut usize) {
        for value in values {
            let copy = value.clone();
            // SAFETY: slot `*count` is one of the empty slots the caller gave.
            unsafe { dst.add(*count).write(copy) };
            *count += 1;
        }
    }

    #[inline]
    default fn trivial() -> bool {
        false
    }
}

// Sound under `min_specialization`: the one added bound is `TrivialClone`, a specialization
// trait (the module's notes).
impl<T: TrivialClone> CloneSlice for T {
    #[inline]
    unsafe fn clone_to(values: &[T], dst: *mut T, count: &mut usize) {
        // SAFETY: the caller's slots: in one allocation, empty, apart from `values`. A
        // `TrivialClone` value's clone is its bitwise copy (the trait's contract).
        unsafe { ptr::copy_nonoverlapping(values.as_ptr(), dst.add(*count), values.len()) };
        *count += values.len();
    }

    #[inline]
    fn trivial() -> bool {
        true
    }
}

/// The first value equal to a needle: blaze-memchr's vector search for bytes, `==` per value
/// for the rest.
pub(crate) trait Find: PartialEq + Sized {
    fn find(values: &[Self], needle: &Self) -> Option<usize>;
}

impl<T: PartialEq> Find for T {
    #[inline]
    default fn find(values: &[T], needle: &T) -> Option<usize> {
        values.iter().position(|value| value == needle)
    }
}

impl Find for u8 {
    #[inline]
    fn find(values: &[u8], needle: &u8) -> Option<usize> {
        blaze_memchr::memchr(*needle, values)
    }
}

impl Find for i8 {
    #[inline]
    fn find(values: &[i8], needle: &i8) -> Option<usize> {
        // SAFETY: `i8` and `u8` have one size and alignment and every bit pattern of either is a
        // value of both; two `i8`s are equal exactly when their bytes are.
        let bytes = unsafe { core::slice::from_raw_parts(values.as_ptr().cast::<u8>(), values.len()) };
        blaze_memchr::memchr(needle.cast_unsigned(), bytes)
    }
}

impl Find for bool {
    #[inline]
    fn find(values: &[bool], needle: &bool) -> Option<usize> {
        // SAFETY: a `bool` is one initialized byte, 0 or 1, so a slice of them reads as bytes;
        // two are equal exactly when their bytes are.
        let bytes = unsafe { core::slice::from_raw_parts(values.as_ptr().cast::<u8>(), values.len()) };
        blaze_memchr::memchr(u8::from(*needle), bytes)
    }
}
