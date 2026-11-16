//! Comparing: [`eq`] and [`cmp`] of slices, by bytes where `==` is byte equality
//! ([`PlainKey`]); [`is_sorted`], [`dedup`], [`min`] and [`max`] by the images of
//! [`RadixKey`]s, branch-free over blocks so the compiler vectorizes them.

#![blaze::sans_io]

use core::cmp::Ordering;
use core::simd::{Simd, cmp::SimdPartialEq};

use blaze_hash::PlainKey;

use crate::key::RadixKey;
use crate::plant::{self, Plant};
use crate::radix::{Word, image};

/// Whether `a` and `b` are equal value by value: core's `==`. For a [`PlainKey`] one compare of
/// the two blocks of bytes (`memcmp`), as `==` is byte equality there.
pub fn eq<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    <T as Compare>::eq(a, b)
}

/// The lexicographic order of `a` and `b`: core's `Ord` for slices. For an `Ord` [`PlainKey`]
/// the first differing element is found by a vector compare of their bytes, and only that
/// element is compared by `Ord` (equal elements are equal bytes, and `Ord` agrees with `Eq`).
pub fn cmp<T: Ord>(a: &[T], b: &[T]) -> Ordering {
    <T as CompareOrd>::cmp(a, b)
}

trait Compare: PartialEq + Sized {
    fn eq(a: &[Self], b: &[Self]) -> bool;
}

impl<T: PartialEq> Compare for T {
    #[inline]
    default fn eq(a: &[T], b: &[T]) -> bool {
        a == b
    }
}

/// The bytes of a slice of plain keys.
#[inline(always)]
fn bytes<T: PlainKey>(v: &[T]) -> &[u8] {
    // SAFETY: the slice's `size_of_val(v)` bytes are readable for its borrow, and a
    // `PlainKey` has no padding or uninitialized byte (its first promise); `u8` has
    // alignment 1.
    unsafe { core::slice::from_raw_parts(v.as_ptr().cast::<u8>(), size_of_val(v)) }
}

// Sound under `min_specialization`: the one added bound is `PlainKey`, a specialization trait,
// whose third promise is that `==` is byte equality.
impl<T: PlainKey> Compare for T {
    #[inline]
    fn eq(a: &[T], b: &[T]) -> bool {
        a.len() == b.len() && bytes(a) == bytes(b)
    }
}

trait CompareOrd: Ord + Sized {
    fn cmp(a: &[Self], b: &[Self]) -> Ordering;
}

impl<T: Ord> CompareOrd for T {
    #[inline]
    default fn cmp(a: &[T], b: &[T]) -> Ordering {
        a.cmp(b)
    }
}

// Sound under `min_specialization`: as `Compare`'s. The element at the first differing byte is
// the first unequal element, so the first whose `cmp` is not `Equal` (`Ord` agrees with `Eq`).
impl<T: Ord + PlainKey> CompareOrd for T {
    #[inline]
    fn cmp(a: &[T], b: &[T]) -> Ordering {
        if size_of::<T>() == 0 {
            return a.len().cmp(&b.len());
        }
        let shared = a.len().min(b.len());
        match mismatch(bytes(&a[..shared]), bytes(&b[..shared])) {
            Some(byte) => {
                let at = byte / size_of::<T>();
                a[at].cmp(&b[at])
            }
            None => a.len().cmp(&b.len()),
        }
    }
}

/// The first index at which `a` and `b` (of one length) differ: a vector compare of 16 bytes
/// four at a time, then one at a time, then the rest byte by byte; [`scalar::mismatch`] is
/// its twin.
pub(crate) fn mismatch(a: &[u8], b: &[u8]) -> Option<usize> {
    type V = Simd<u8, 16>;
    debug_assert_eq!(a.len(), b.len());
    let (blocks_a, rest_a) = a.as_chunks::<64>();
    let (blocks_b, rest_b) = b.as_chunks::<64>();
    for (k, (x, y)) in blocks_a.iter().zip(blocks_b).enumerate() {
        let (xs, _) = x.as_chunks::<16>();
        let (ys, _) = y.as_chunks::<16>();
        let differ = [
            V::from_array(xs[0]).simd_ne(V::from_array(ys[0])),
            V::from_array(xs[1]).simd_ne(V::from_array(ys[1])),
            V::from_array(xs[2]).simd_ne(V::from_array(ys[2])),
            V::from_array(xs[3]).simd_ne(V::from_array(ys[3])),
        ];
        let first = usize::from(plant::on(Plant::MismatchSkipsVector));
        if (differ[first] | differ[1] | differ[2] | differ[3]).any() {
            for (v, mask) in differ.iter().enumerate().skip(first) {
                if let Some(lane) = mask.first_set() {
                    return Some(k * 64 + v * 16 + lane);
                }
            }
        }
    }
    let done = blocks_a.len() * 64;
    let (vectors_a, tail_a) = rest_a.as_chunks::<16>();
    let (vectors_b, tail_b) = rest_b.as_chunks::<16>();
    for (k, (x, y)) in vectors_a.iter().zip(vectors_b).enumerate() {
        if let Some(lane) = V::from_array(*x).simd_ne(V::from_array(*y)).first_set() {
            return Some(done + k * 16 + lane);
        }
    }
    let done = done + vectors_a.len() * 16;
    tail_a.iter().zip(tail_b).position(|(x, y)| x != y).map(|at| done + at)
}

/// The scalar twins of the vector compares.
#[cfg(test)]
pub(crate) mod scalar {
    pub(crate) fn mismatch(a: &[u8], b: &[u8]) -> Option<usize> {
        a.iter().zip(b).position(|(x, y)| x != y)
    }
}

/// Keys a block of the branch-free loops: the loop over a block has no exit, so the compiler
/// vectorizes it, and the check between blocks costs one branch in 64 keys.
const BLOCK: usize = 64;

/// Whether `v` is in its keys' order (non-decreasing): for `Ord` types core's `is_sorted`, for
/// floats `is_sorted_by(|a, b| a.total_cmp(b).is_le())`.
pub fn is_sorted<T: RadixKey>(v: &[T]) -> bool {
    if const { T::BYTES <= 4 } {
        is_sorted_word::<T, u32>(v)
    } else if const { T::BYTES <= 8 } {
        is_sorted_word::<T, u64>(v)
    } else if const { T::BYTES <= 16 } {
        is_sorted_word::<T, u128>(v)
    } else {
        v.is_sorted_by(|a, b| a.cmp_image(b).is_le())
    }
}

fn is_sorted_word<T: RadixKey, W: Word>(v: &[T]) -> bool {
    if v.len() < 2 {
        return true;
    }
    let (left, right) = (&v[..v.len() - 1], &v[1..]);
    for (a, b) in left.chunks(BLOCK).zip(right.chunks(BLOCK)) {
        let mut out_of_order = false;
        for (x, y) in a.iter().zip(b) {
            out_of_order |= image::<T, W>(x) > image::<T, W>(y);
        }
        if out_of_order {
            return false;
        }
    }
    true
}

/// Removes consecutive repeats from `v` (sorted or not), keeping the first of each run: the
/// kept values move to the front, in order, and their count is returned; what lies past it is
/// unspecified. For `Ord` types the values `Vec::dedup` keeps; for floats, repeats are equal
/// bits (`total_cmp` equal). Branch-free: each value is written and the count advanced by
/// whether it differs from the last kept, which, a value being its image, is unobservable
/// against keeping the first.
pub fn dedup<T: RadixKey>(v: &mut [T]) -> usize {
    if const { T::BYTES <= 4 } {
        dedup_word::<T, u32>(v)
    } else if const { T::BYTES <= 8 } {
        dedup_word::<T, u64>(v)
    } else if const { T::BYTES <= 16 } {
        dedup_word::<T, u128>(v)
    } else {
        let mut kept = usize::from(!v.is_empty());
        for at in 1..v.len() {
            if v[at].cmp_image(&v[kept - 1]).is_ne() {
                v[kept] = v[at];
                kept += 1;
            }
        }
        kept
    }
}

fn dedup_word<T: RadixKey, W: Word>(v: &mut [T]) -> usize {
    let n = v.len();
    if n == 0 {
        return 0;
    }
    let mut kept = 1;
    let mut last = image::<T, W>(&v[0]);
    for at in 1..n {
        // SAFETY: `at < n`.
        let x = unsafe { *v.get_unchecked(at) };
        let k = image::<T, W>(&x);
        // SAFETY: `kept <= at < n`: both start at 1 and `kept` grows by at most one a step,
        // as `at` does.
        unsafe { *v.get_unchecked_mut(kept) = x };
        kept += usize::from(k != last);
        last = k;
    }
    kept
}

/// The least key of `v` by the keys' order: for `Ord` types core's `iter().min()`, for floats
/// `min_by(total_cmp)`. A branch-free minimum of each block's images, then the key with the
/// least image found in its block; equal images are one value, so which is not observable.
pub fn min<T: RadixKey>(v: &[T]) -> Option<T> {
    extreme(v, Ordering::Less)
}

/// The greatest key of `v`, as [`min`] is the least.
pub fn max<T: RadixKey>(v: &[T]) -> Option<T> {
    extreme(v, Ordering::Greater)
}

fn extreme<T: RadixKey>(v: &[T], want: Ordering) -> Option<T> {
    if const { T::BYTES <= 4 } {
        extreme_word::<T, u32>(v, want)
    } else if const { T::BYTES <= 8 } {
        extreme_word::<T, u64>(v, want)
    } else if const { T::BYTES <= 16 } {
        extreme_word::<T, u128>(v, want)
    } else {
        let mut best = *v.first()?;
        for x in &v[1..] {
            if x.cmp_image(&best) == want {
                best = *x;
            }
        }
        Some(best)
    }
}

/// The extreme image of each block of [`BLOCK`], then the key found in the winning block. Two
/// small-slice forms were measured and lost to this (best of 5, ns a key at 256 u32: blocks
/// 0.07, a fold of every image then a find 0.11, `min_by_key` 0.45; core's `iter().min()`
/// 0.03, which has no image to compute and is still ahead below 4096 unsigned keys).
fn extreme_word<T: RadixKey, W: Word>(v: &[T], want: Ordering) -> Option<T> {
    let first = image::<T, W>(v.first()?);
    let (mut best, mut block) = (first, 0);
    for (at, chunk) in v.chunks(BLOCK).enumerate() {
        let mut here = first;
        if want == Ordering::Less {
            for x in chunk {
                here = here.min(image::<T, W>(x));
            }
        } else {
            for x in chunk {
                here = here.max(image::<T, W>(x));
            }
        }
        if here.cmp(&best) == want {
            (best, block) = (here, at);
        }
    }
    let start = block * BLOCK;
    v[start..].iter().find(|x| image::<T, W>(*x) == best).copied()
}
