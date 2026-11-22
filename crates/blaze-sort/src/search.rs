//! Searching: [`lower_bound`] and [`binary_search`] on a sorted slice of [`RadixKey`]s by
//! their images, with no unpredictable branch; [`position`] and [`contains`], by vector compare
//! for plain keys of 1, 2, 4 and 8 bytes.

#![blaze::sans_io]

use core::hint::select_unpredictable;
use core::simd::{Simd, cmp::SimdPartialEq};

use blaze_hash::PlainKey;

use crate::key::RadixKey;
use crate::plant::{self, Plant};
use crate::radix::{Word, image};

/// The first index of the sorted `v` whose key is not below `probe` (`v.len()` when none):
/// core's `partition_point(|x| x < probe)` by the keys' order. A halving that always runs
/// `ceil(log2 n)` steps, each one load and one conditional select (`select_unpredictable`),
/// so a search takes no branch on the data.
pub fn lower_bound<T: RadixKey>(v: &[T], probe: &T) -> usize {
    if const { T::BYTES <= 4 } {
        lower_bound_word::<T, u32>(v, image(probe))
    } else if const { T::BYTES <= 8 } {
        lower_bound_word::<T, u64>(v, image(probe))
    } else if const { T::BYTES <= 16 } {
        lower_bound_word::<T, u128>(v, image(probe))
    } else {
        v.partition_point(|x| x.cmp_image(probe).is_lt())
    }
}

#[inline]
fn lower_bound_word<T: RadixKey, W: Word>(v: &[T], probe: W) -> usize {
    let mut size = v.len();
    if size == 0 {
        return 0;
    }
    let mut base = 0usize;
    let short = plant::on(Plant::LowerBoundShort) && size > 8;
    while size > 1 {
        let half = size / 2;
        let mid = base + half;
        // SAFETY: `mid = base + half < base + size <= v.len()`: the range `[base, base +
        // size)` stays inside `v`, and `half < size`.
        let below = image::<T, W>(unsafe { v.get_unchecked(mid) }) < probe;
        base = select_unpredictable(below, mid, base);
        size -= half;
        if short && size <= 2 {
            break;
        }
    }
    // SAFETY: `base < v.len()`: it is 0 or a `mid` above.
    let below = image::<T, W>(unsafe { v.get_unchecked(base) }) < probe;
    base + usize::from(below)
}

/// `Ok` of the first index of the sorted `v` holding `probe`, or `Err` of where it would go:
/// one of the answers core's `binary_search` may give (it may give any index of a run of equal
/// keys; this gives the first, which for a `RadixKey`, whose equal keys are one value, holds
/// the same value).
pub fn binary_search<T: RadixKey>(v: &[T], probe: &T) -> Result<usize, usize> {
    let at = lower_bound(v, probe);
    match v.get(at) {
        Some(found) if found.cmp_image(probe).is_eq() => Ok(at),
        Some(_) | None => Err(at),
    }
}

/// The first index of `v` holding a value equal to `needle`. For a [`PlainKey`] of 1, 2, 4 or
/// 8 bytes, aligned to its size (integers, `char`, `bool`, arrays of bytes), a vector compare
/// of 64 bytes a step; for any other type `==` value by value, as core's `position` does.
pub fn position<T: PartialEq>(v: &[T], needle: &T) -> Option<usize> {
    <T as Find>::find(v, needle)
}

/// Whether `v` holds a value equal to `needle`: core's `contains`, which the compiler already
/// turns into a vector loop with no early exit. The lane search of [`position`] only tied it
/// for plain keys and lost below a few vectors and for other types (best of 5, ns a key: u32
/// 0.12 against core's 0.05 at 16, 0.03 against 0.03 at 4096; f64 0.26 against 0.07 at 4096).
pub fn contains<T: PartialEq>(v: &[T], needle: &T) -> bool {
    v.contains(needle)
}

trait Find: PartialEq + Sized {
    fn find(v: &[Self], needle: &Self) -> Option<usize>;
}

impl<T: PartialEq> Find for T {
    #[inline]
    default fn find(v: &[T], needle: &T) -> Option<usize> {
        v.iter().position(|x| x == needle)
    }
}

// Sound under `min_specialization`: the one added bound is `PlainKey`, a specialization trait.
// Its third promise, `==` exactly when the bytes are equal, makes the lane compare `==`.
impl<T: PlainKey> Find for T {
    #[inline]
    fn find(v: &[T], needle: &T) -> Option<usize> {
        macro_rules! lanes {
            ($u:ty, $find:ident) => {{
                // SAFETY: `T` is `size_of::<$u>()` bytes aligned to as many (the branch's
                // condition), so the slice's memory is a slice of `$u` of the same length;
                // a `PlainKey`'s bytes are all initialized (no padding), so each is a `$u`;
                // and two `T`s are equal exactly when their bytes are, so when the `$u`s are.
                let lanes = unsafe { core::slice::from_raw_parts(v.as_ptr().cast::<$u>(), v.len()) };
                // SAFETY: the same, for the needle.
                let needle = unsafe { *(needle as *const T).cast::<$u>() };
                $find(lanes, needle)
            }};
        }
        let (size, align) = (size_of::<T>(), align_of::<T>());
        if size == 1 {
            lanes!(u8, find_u8)
        } else if size == 2 && align == 2 {
            lanes!(u16, find_u16)
        } else if size == 4 && align == 4 {
            lanes!(u32, find_u32)
        } else if size == 8 && align == 8 {
            lanes!(u64, find_u64)
        } else {
            v.iter().position(|x| x == needle)
        }
    }
}

#[inline]
fn find_u8(hay: &[u8], needle: u8) -> Option<usize> {
    blaze_memchr::memchr(needle, hay)
}

/// The vector search over lanes of one width: four vectors (64 bytes) a step while they last,
/// then one at a time, then the rest one by one. Each has a scalar twin, [`scalar`], which
/// the tests hold it to at every length and alignment.
macro_rules! find_lanes {
    ($name:ident, $t:ty, $lanes:literal) => {
        pub(crate) fn $name(hay: &[$t], needle: $t) -> Option<usize> {
            type V = Simd<$t, $lanes>;
            let splat = V::splat(needle);
            let (blocks, rest) = hay.as_chunks::<{ 4 * $lanes }>();
            for (b, block) in blocks.iter().enumerate() {
                let (vectors, _) = block.as_chunks::<$lanes>();
                let hits = [
                    V::from_array(vectors[0]).simd_eq(splat),
                    V::from_array(vectors[1]).simd_eq(splat),
                    V::from_array(vectors[2]).simd_eq(splat),
                    V::from_array(vectors[3]).simd_eq(splat),
                ];
                if (hits[0] | hits[1] | hits[2] | hits[3]).any() {
                    let order: [usize; 4] =
                        if plant::on(Plant::FindSecondVectorFirst) { [1, 0, 2, 3] } else { [0, 1, 2, 3] };
                    for k in order {
                        if let Some(lane) = hits[k].first_set() {
                            return Some(b * 4 * $lanes + k * $lanes + lane);
                        }
                    }
                }
            }
            let done = blocks.len() * 4 * $lanes;
            let (vectors, tail) = rest.as_chunks::<$lanes>();
            for (k, vector) in vectors.iter().enumerate() {
                if let Some(lane) = V::from_array(*vector).simd_eq(splat).first_set() {
                    return Some(done + k * $lanes + lane);
                }
            }
            let done = done + vectors.len() * $lanes;
            tail.iter().position(|&x| x == needle).map(|at| done + at)
        }
    };
}

find_lanes!(find_u16, u16, 8);
find_lanes!(find_u32, u32, 4);
find_lanes!(find_u64, u64, 2);

/// The scalar twins of the vector searches.
#[cfg(test)]
pub(crate) mod scalar {
    pub(crate) fn find<T: PartialEq + Copy>(hay: &[T], needle: T) -> Option<usize> {
        hay.iter().position(|&x| x == needle)
    }
}
