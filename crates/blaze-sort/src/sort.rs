//! The sorts' front door: radix sorts for [`RadixKey`]s, a stable radix sort by key for any
//! type, and the comparison sorts for the rest, the generic ones taking the radix path when the
//! type is a `RadixKey` (`Spec`).

#![blaze::sans_io]

use core::ptr;

use blaze_own::{Alloc, AllocFailure, RawBuf};

use crate::key::RadixKey;
use crate::merge;
use crate::plant::{self, Plant};
use crate::radix::{self, Run, Word, image};

/// Up to this many keys, an insertion sort on the images. Measured (`cost threshold`, best of
/// 5, ns a key): at 4 keys insertion 0.55 against core's 0.59 (u32) and 0.43 against 0.46
/// (u64); at 8, 0.98 against 0.82 (u32), so core's own small sort wins from there.
pub const TINY: usize = 4;

/// The fewest keys for which [`sort_in`] sorts by digits through scratch, at one varying digit;
/// at `d` varying digits the bound is `LSD_FROM * d * d` ([`lsd_from`]), since each digit is a
/// pass over every key while core's cost grows with `log n`. Measured (ns a key, LSD against
/// core): u32 (4 digits) 2.99 against 3.41 at 768, 3.34 against 3.23 at 512, so 1024; u64 (8)
/// 4.55 against 4.42 at 4096, (u32,u32) (8) 5.44 against 6.78 at 4096, so 4096.
pub const LSD_FROM: usize = 64;

/// [`LSD_FROM`] scaled to `digits` varying digits.
#[must_use]
pub const fn lsd_from(digits: usize) -> usize {
    LSD_FROM.saturating_mul(digits).saturating_mul(digits)
}

/// Sorts `v` in place by its keys' order, with no allocation: for `Ord` types `Ord`'s, for
/// floats `total_cmp`'s. Not stable, which for a `RadixKey` no code can tell (equal keys are
/// one value).
///
/// Up to [`TINY`] keys an insertion sort, above it core's unstable sort on the images (one
/// integer compare a step for tuples, floats and signed keys). No radix sort without scratch:
/// the in-place most-significant-digit sort (`kernels::msd`) lost to core at every size
/// measured (best of 5, ns a key: u32 4.49 against 4.41 at 4096, 10.21 against 6.82 at 1e5,
/// 15.22 against 10.48 at 1e7), so [`sort_in`] is the fast path. Keys wider than 16 bytes are
/// sorted by core's unstable sort on their images.
pub fn sort<T: RadixKey>(v: &mut [T]) {
    if const { T::BYTES <= 4 } {
        sort_word::<T, u32>(v);
    } else if const { T::BYTES <= 8 } {
        sort_word::<T, u64>(v);
    } else if const { T::BYTES <= 16 } {
        sort_word::<T, u128>(v);
    } else {
        v.sort_unstable_by(T::cmp_image);
    }
}

fn sort_word<T: RadixKey, W: Word>(v: &mut [T]) {
    if v.len() <= TINY {
        radix::insertion(v, image::<T, W>);
    } else {
        v.sort_unstable_by_key(image::<T, W>);
    }
}

/// [`sort`] with scratch of `v.len()` keys from `alloc` (asked only from [`lsd_from`] of the
/// varying digits, on input not in order): a least-significant-digit radix sort over the digits on which the
/// keys differ, one read and one write of every key a digit. Keys wider than 16 bytes are read
/// a byte at a time, a digit every key shares skipped.
pub fn sort_in<T: RadixKey>(alloc: Alloc<'_>, v: &mut [T]) -> Result<(), AllocFailure> {
    if const { T::BYTES <= 4 } {
        sort_in_word::<T, u32>(alloc, v)
    } else if const { T::BYTES <= 8 } {
        sort_in_word::<T, u64>(alloc, v)
    } else if const { T::BYTES <= 16 } {
        sort_in_word::<T, u128>(alloc, v)
    } else {
        if v.len() < LSD_FROM {
            v.sort_unstable_by(T::cmp_image);
            return Ok(());
        }
        let scratch = RawBuf::<T, Alloc<'_>>::try_with_capacity_in(v.len(), alloc)?;
        // SAFETY: the scratch has room for `v.len()` keys and is its own allocation; a
        // `RadixKey`'s `byte` is pure (its first promise).
        unsafe { radix::lsd_bytes(v, scratch.ptr(), T::BYTES, T::byte) };
        Ok(())
    }
}

fn sort_in_word<T: RadixKey, W: Word>(alloc: Alloc<'_>, v: &mut [T]) -> Result<(), AllocFailure> {
    let n = v.len();
    if n <= TINY {
        radix::insertion(v, image::<T, W>);
        return Ok(());
    }
    if n < LSD_FROM {
        v.sort_unstable_by_key(image::<T, W>);
        return Ok(());
    }
    match radix::run(v, image::<T, W>) {
        Run::Ascending => return Ok(()),
        Run::Descending => {
            v.reverse();
            return Ok(());
        }
        Run::Neither => {}
    }
    let digits = radix::varying(v, image::<T, W>);
    if digits.as_slice().is_empty() {
        return Ok(());
    }
    if n < lsd_from(digits.as_slice().len()) {
        v.sort_unstable_by_key(image::<T, W>);
        return Ok(());
    }
    let scratch = RawBuf::<T, Alloc<'_>>::try_with_capacity_in(n, alloc)?;
    // SAFETY: the scratch has room for `n` keys and is its own allocation; a `RadixKey`'s
    // image is pure (its first promise).
    unsafe { radix::lsd(v, scratch.ptr(), digits.as_slice(), image::<T, W>) };
    Ok(())
}

/// The kernels without the thresholds in front of them, for `examples/cost.rs` to find where
/// each wins: what the thresholds above were set from.
#[doc(hidden)]
pub mod kernels {
    use blaze_own::{Alloc, AllocFailure, RawBuf};

    use crate::key::RadixKey;
    use crate::radix::{self, image};

    /// The insertion sort on the images (keys of at most 8 bytes).
    pub fn insertion<T: RadixKey>(v: &mut [T]) {
        assert!(T::BYTES <= 8, "blaze_sort::kernels: a key wider than a word");
        radix::insertion(v, image::<T, u64>);
    }

    /// core's unstable sort on the images (keys of at most 8 bytes).
    pub fn core_on_images<T: RadixKey>(v: &mut [T]) {
        assert!(T::BYTES <= 8, "blaze_sort::kernels: a key wider than a word");
        v.sort_unstable_by_key(image::<T, u64>);
    }

    /// The in-place radix sort at any length (keys of at most 8 bytes).
    pub fn msd<T: RadixKey>(v: &mut [T]) {
        assert!(T::BYTES <= 8, "blaze_sort::kernels: a key wider than a word");
        if v.len() < 2 {
            return;
        }
        let digits = radix::varying(v, image::<T, u64>);
        radix::msd::<T, u64, 8>(v, digits.as_slice(), image::<T, u64>);
    }

    /// The radix sort through scratch at any length (keys of at most 8 bytes).
    pub fn lsd<T: RadixKey>(alloc: Alloc<'_>, v: &mut [T]) -> Result<(), AllocFailure> {
        assert!(T::BYTES <= 8, "blaze_sort::kernels: a key wider than a word");
        let digits = radix::varying(v, image::<T, u64>);
        let scratch = RawBuf::<T, Alloc<'_>>::try_with_capacity_in(v.len(), alloc)?;
        // SAFETY: room for `v.len()` keys in an allocation of its own; a `RadixKey`'s image
        // is pure.
        unsafe { radix::lsd(v, scratch.ptr(), digits.as_slice(), image::<T, u64>) };
        Ok(())
    }
}

/// An index into the slice a sort by key permutes: `u32` when the slice is that short, so a
/// (key, index) pair of a `u32` key is 8 bytes.
trait Index: Copy {
    fn of(at: usize) -> Self;
    fn at(self) -> usize;
}

impl Index for u32 {
    #[inline(always)]
    fn of(at: usize) -> u32 {
        at as u32
    }
    #[inline(always)]
    fn at(self) -> usize {
        self as usize
    }
}

impl Index for usize {
    #[inline(always)]
    fn of(at: usize) -> usize {
        at
    }
    #[inline(always)]
    fn at(self) -> usize {
        self
    }
}

/// Sorts `v` stably by `key`, for any `T`, in time linear in `v.len()` and with no comparison:
/// `key` is called once a value, in order, and its image paired with the value's index; the
/// pairs are sorted by a least-significant-digit radix sort (stable by construction, the
/// indices starting in order), over the digits on which the keys differ; then `v` is permuted
/// in place by cycles, each value moved once and nothing of `T` called.
///
/// Scratch from `alloc`: two buffers of `v.len()` pairs, a pair being the key's image (4, 8
/// or 16 bytes, or the key itself when wider) and a `u32` index (`usize` past `u32::MAX`
/// values), as aligned: 16 bytes a value for a `u64` key, 8 for a `u32` one. A panic in `key`
/// leaves `v` as it was.
pub fn sort_by_key_in<T, K: RadixKey>(
    alloc: Alloc<'_>,
    v: &mut [T],
    key: impl Fn(&T) -> K,
) -> Result<(), AllocFailure> {
    if v.len() < 2 {
        return Ok(());
    }
    if u32::try_from(v.len()).is_ok() {
        by_key::<T, K, u32>(alloc, v, key)
    } else {
        by_key::<T, K, usize>(alloc, v, key)
    }
}

fn by_key<T, K: RadixKey, I: Index>(
    alloc: Alloc<'_>,
    v: &mut [T],
    key: impl Fn(&T) -> K,
) -> Result<(), AllocFailure> {
    if const { K::BYTES <= 4 } {
        by_key_word::<T, K, u32, I>(alloc, v, key)
    } else if const { K::BYTES <= 8 } {
        by_key_word::<T, K, u64, I>(alloc, v, key)
    } else if const { K::BYTES <= 16 } {
        by_key_word::<T, K, u128, I>(alloc, v, key)
    } else {
        let n = v.len();
        let mut pairs = RawBuf::<(K, I), Alloc<'_>>::try_with_capacity_in(pair_room(n)?, alloc)?;
        let (pairs, scratch) = fill_pairs(&mut pairs, v, |value| key(value));
        // SAFETY: the scratch is the buffer's second half, `n` pairs, apart from the first;
        // a `RadixKey`'s `byte` is pure.
        unsafe { radix::lsd_bytes(pairs, scratch, K::BYTES, |pair, at| pair.0.byte(at)) };
        permute(alloc, v, pairs, scratch)
    }
}

fn by_key_word<T, K: RadixKey, W: Word, I: Index>(
    alloc: Alloc<'_>,
    v: &mut [T],
    key: impl Fn(&T) -> K,
) -> Result<(), AllocFailure> {
    let n = v.len();
    let mut pairs = RawBuf::<(W, I), Alloc<'_>>::try_with_capacity_in(pair_room(n)?, alloc)?;
    let (pairs, scratch) = fill_pairs(&mut pairs, v, |value| image::<K, W>(&key(value)));
    if radix::run(pairs, |pair| pair.0) == Run::Ascending {
        // Already in order: a stable sort moves nothing.
        return Ok(());
    }
    if n <= TINY {
        radix::insertion(pairs, |pair| pair.0);
    } else {
        let digits = radix::varying(pairs, |pair| pair.0);
        if n < lsd_from(digits.as_slice().len()) {
            // Each pair holds its value's index, so no two pairs are equal and the unstable
            // sort of (image, index) is the stable order.
            pairs.sort_unstable_by_key(|pair| (pair.0, pair.1.at()));
        } else if plant::on(Plant::ByKeyUnstable) {
            radix::msd::<(W, I), W, 16>(pairs, digits.as_slice(), |pair| pair.0);
        } else {
            // SAFETY: the scratch is the buffer's second half, `n` pairs, apart from the
            // first; the key of a pair is a field, pure.
            unsafe { radix::lsd(pairs, scratch, digits.as_slice(), |pair| pair.0) };
        }
    }
    permute(alloc, v, pairs, scratch)
}

/// The pairs a sort by key of `n` values needs: `2 * n`, which overflows only for a slice of
/// a zero-sized type longer than half the address space.
fn pair_room(n: usize) -> Result<usize, AllocFailure> {
    n.checked_mul(2).ok_or(AllocFailure::CapacityOverflow { what: "blaze_sort::sort_by_key_in's pairs", len: n, additional: n })
}

/// Writes `(make(v[i]), i)` into the first `v.len()` slots of `buf` (room for twice that) and
/// gives them, and the rest of the buffer as scratch. `make` is called once a value, in order;
/// a panic in it leaves `v` untouched and the buffer's pairs `Copy`, nothing to drop.
fn fill_pairs<'b, T, P: Copy, I: Index>(
    buf: &'b mut RawBuf<(P, I), Alloc<'_>>,
    v: &[T],
    make: impl Fn(&T) -> P,
) -> (&'b mut [(P, I)], *mut (P, I)) {
    let n = v.len();
    let base = buf.ptr();
    for (at, value) in v.iter().enumerate() {
        // SAFETY: `at < n`, and the buffer has room for `2 * n` pairs.
        unsafe { base.add(at).write((make(value), I::of(at))) };
    }
    // SAFETY: the first `n` slots were written just now; the next `n` are the buffer's, apart
    // from them, and the borrow of `buf` keeps the buffer alive for `'b`.
    unsafe { (core::slice::from_raw_parts_mut(base, n), base.add(n)) }
}

/// Moves `v`'s values so that position `i` holds the value that was at `order[i]`'s index: a
/// gather into `spare` (the pair buffer's free half, `n` pairs) when a `T` fits a pair's slot,
/// else into `n` values of scratch from `alloc`, then one block copy back. A gather's reads are
/// independent, so the memory system overlaps their misses; following the permutation's
/// cycles in place makes each read wait on the last (measured: 112.88 ns a key at 1e7 u32
/// values, against core's 14.76). Calls nothing of `T`, so nothing panics midway; an
/// allocation failure leaves `v` as it was.
fn permute<T, P: Copy, I: Index>(
    alloc: Alloc<'_>,
    v: &mut [T],
    order: &[(P, I)],
    spare: *mut (P, I),
) -> Result<(), AllocFailure> {
    let n = v.len();
    assert_eq!(order.len(), n, "blaze_sort: a permutation of {} positions for {n} values", order.len());
    if const { size_of::<T>() <= size_of::<(P, I)>() && align_of::<T>() <= align_of::<(P, I)>() } {
        // SAFETY: `spare` has room for `n` pairs apart from `order` and `v`, and a `T` is no
        // larger nor more aligned than a pair, so it has room for `n` values at their
        // alignment.
        unsafe { gather(v, order, spare.cast::<T>()) };
    } else {
        let out = RawBuf::<T, Alloc<'_>>::try_with_capacity_in(n, alloc)?;
        // SAFETY: the buffer has room for `n` values and is its own allocation.
        unsafe { gather(v, order, out.ptr()) };
    }
    Ok(())
}

/// # Safety
///
/// `out` has room for `v.len()` values and overlaps neither `v` nor `order`; `order` holds
/// each index of `0..v.len()` once (the radix sort only moves pairs), with `order.len() ==
/// v.len()`.
unsafe fn gather<T, P: Copy, I: Index>(v: &mut [T], order: &[(P, I)], out: *mut T) {
    let base = v.as_mut_ptr();
    // SAFETY: each index read is below `n` (the caller's permutation), so each source is in
    // `v`; every value is copied out once and the block copied back whole, so each is in `v`
    // exactly once at the end, and nothing between can unwind.
    unsafe {
        for (at, pair) in order.iter().enumerate() {
            ptr::copy_nonoverlapping(base.add(pair.1.at()), out.add(at), 1);
        }
        ptr::copy_nonoverlapping(out, base, v.len());
    }
}

/// Stable sort of `v` by `Ord`, scratch from `alloc`. For a [`RadixKey`] this is [`sort_in`]:
/// equal keys are one value, so stability is not observable and the radix sort's bytes are
/// the same. For any other type a bottom-up merge sort with `v.len() / 2` values of scratch.
pub fn sort_stable_in<T: Ord>(alloc: Alloc<'_>, v: &mut [T]) -> Result<(), AllocFailure> {
    <T as Spec>::stable_in(alloc, v)
}

/// Stable sort of `v` by `is_less`, scratch of `v.len() / 2` values from `alloc` (none for 20
/// values or fewer).
pub fn sort_stable_by_in<T>(
    alloc: Alloc<'_>,
    v: &mut [T],
    mut is_less: impl FnMut(&T, &T) -> bool,
) -> Result<(), AllocFailure> {
    merge::sort(alloc, v, &mut is_less)
}

/// Unstable sort of `v` by `Ord`, in place. For a [`RadixKey`] this is [`sort`], else core's.
pub fn sort_unstable<T: Ord>(v: &mut [T]) {
    <T as Spec>::unstable(v);
}

/// Unstable sort of `v` by `compare`, in place: core's.
pub fn sort_unstable_by<T>(v: &mut [T], compare: impl FnMut(&T, &T) -> core::cmp::Ordering) {
    v.sort_unstable_by(compare);
}

/// The generic sorts' choice by type.
trait Spec: Ord + Sized {
    fn stable_in(alloc: Alloc<'_>, v: &mut [Self]) -> Result<(), AllocFailure>;
    fn unstable(v: &mut [Self]);
}

impl<T: Ord> Spec for T {
    #[inline]
    default fn stable_in(alloc: Alloc<'_>, v: &mut [T]) -> Result<(), AllocFailure> {
        merge::sort(alloc, v, &mut |a: &T, b: &T| a < b)
    }

    #[inline]
    default fn unstable(v: &mut [T]) {
        v.sort_unstable();
    }
}

// Sound under `min_specialization`: the one added bound is `RadixKey`, a specialization trait
// whose impls are all always applicable (`key`'s notes). Its third promise makes `Ord` the
// images' order, so the radix sorts give what `Ord` would.
impl<T: Ord + RadixKey> Spec for T {
    #[inline]
    fn stable_in(alloc: Alloc<'_>, v: &mut [T]) -> Result<(), AllocFailure> {
        sort_in(alloc, v)
    }

    #[inline]
    fn unstable(v: &mut [T]) {
        sort(v);
    }
}
