//! The radix kernels: a key's image as a machine word ([`Word`]), the one pass that finds
//! input already in order ([`run`]), the one that finds the digits on which keys differ
//! ([`varying`]), the stable least-significant-digit sort into scratch ([`lsd`]) and the
//! in-place most-significant-digit sort ([`msd`]), with their forms for keys wider than 16
//! bytes, read a byte at a time ([`lsd_bytes`]).
//!
//! Digits are 8 bits. A key moves whole (`E: Copy`); its image is computed from it again in
//! each pass, which for every key here is a few register instructions.

#![blaze::sans_io]

use core::ops::{BitAnd, BitOr, BitXor};
use core::ptr;

use crate::key::RadixKey;
use crate::plant::{self, Plant};

/// An unsigned machine word a key's image fits in: `u32`, `u64` or `u128`, chosen by the
/// key's width so a `u32` key is sorted in 32-bit registers.
pub(crate) trait Word:
    Copy + Ord + BitAnd<Output = Self> + BitOr<Output = Self> + BitXor<Output = Self>
{
    /// The word's width in bytes: its most digits.
    const BYTES: usize;
    const ZERO: Self;
    const ONES: Self;
    /// The low bits of `image`, which hold all of it for a key of at most `BYTES` bytes.
    fn of(image: u128) -> Self;
    /// Digit `at` (byte `at`, 0 the least significant).
    fn digit(self, at: usize) -> usize;
}

macro_rules! word {
    ($($t:ty),*) => {$(
        impl Word for $t {
            const BYTES: usize = size_of::<$t>();
            const ZERO: $t = 0;
            const ONES: $t = <$t>::MAX;
            #[inline(always)]
            fn of(image: u128) -> $t {
                image as $t
            }
            #[inline(always)]
            fn digit(self, at: usize) -> usize {
                usize::from((self >> (8 * at)) as u8)
            }
        }
    )*};
}

word!(u32, u64, u128);

/// A key's image in the word `W`.
#[inline(always)]
pub(crate) fn image<T: RadixKey, W: Word>(key: &T) -> W {
    W::of(key.image())
}

/// The digit positions on which some two keys differ, least significant first.
#[derive(Clone, Copy)]
pub(crate) struct Digits {
    at: [u8; 16],
    len: usize,
}

impl Digits {
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.at[..self.len]
    }
}

/// The digits on which some two of `v`'s keys differ: the bits set in some key and clear in
/// another are the OR of all keys XOR their AND, read in one pass the compiler vectorizes.
/// A digit every key shares moves nothing, so a sort skips it: ids below `2^20` in a `u64`
/// take three passes, not eight.
#[inline]
pub(crate) fn varying<E, W: Word>(v: &[E], key: impl Fn(&E) -> W) -> Digits {
    let (mut or, mut and) = (W::ZERO, W::ONES);
    for e in v {
        let k = key(e);
        or = or | k;
        and = and & k;
    }
    let differ = or ^ and;
    let mut digits = Digits { at: [0; 16], len: 0 };
    for at in 0..W::BYTES {
        if differ.digit(at) != 0 {
            digits.at[digits.len] = at as u8;
            digits.len += 1;
        }
    }
    if plant::on(Plant::SkipVaryingDigit) && digits.len > 0 {
        digits.at.copy_within(1..digits.len, 0);
        digits.len -= 1;
    }
    digits
}

/// Whether a slice is in order already, either way.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Run {
    /// Non-decreasing: sorted.
    Ascending,
    /// Non-increasing and not all equal: sorted once reversed. Equal keys are one value, so
    /// reversing a run of them changes nothing.
    Descending,
    Neither,
}

/// One pass over `v` (at least two keys) that stops at the first key out of order both ways:
/// on random input after a few keys.
#[inline]
pub(crate) fn run<E, W: Word>(v: &[E], key: impl Fn(&E) -> W) -> Run {
    let (mut up, mut down) = (true, true);
    let mut last = key(&v[0]);
    for e in &v[1..] {
        let k = key(e);
        up &= last <= k;
        down &= last >= k;
        if !(up | down) {
            return Run::Neither;
        }
        last = k;
    }
    if up { Run::Ascending } else { Run::Descending }
}

/// Insertion sort by `key`, for a few keys: stable, no allocation, moves by copy.
#[inline]
pub(crate) fn insertion<E: Copy, W: Word>(v: &mut [E], key: impl Fn(&E) -> W) {
    for i in 1..v.len() {
        let x = v[i];
        let k = key(&x);
        let mut j = i;
        while j > 0 && key(&v[j - 1]) > k {
            v[j] = v[j - 1];
            j -= 1;
        }
        v[j] = x;
    }
}

/// Exclusive prefix sums of `counts`: where each digit's run starts.
#[inline(always)]
fn starts(counts: &[usize; 256]) -> [usize; 256] {
    let mut starts = [0usize; 256];
    let mut sum = 0;
    for (start, &count) in starts.iter_mut().zip(counts) {
        *start = sum;
        sum += count;
    }
    starts
}

/// Stable least-significant-digit radix sort of `v` by `key`, one scatter pass per digit of
/// `digits` (least significant first), between `v` and `scratch`; the result ends in `v`.
/// The count of each pass's digit is made during the pass before, so a sort of `k` digits
/// reads the keys `k + 1` times and writes them `k` times (`k + 1` when `k` is odd: the
/// result is copied back).
///
/// # Safety
///
/// `scratch` has room for `v.len()` values of `E`, is valid for writes and does not overlap
/// `v`. `key` is pure (`RadixKey`'s first promise), so each pass places exactly as many keys
/// in a digit's run as it counted: every write is inside `scratch` or `v`.
pub(crate) unsafe fn lsd<E: Copy, W: Word>(
    v: &mut [E],
    scratch: *mut E,
    digits: &[u8],
    key: impl Fn(&E) -> W,
) {
    let n = v.len();
    let Some(&first) = digits.first() else { return };
    let mut counts = [0usize; 256];
    for e in v.iter() {
        counts[key(e).digit(usize::from(first))] += 1;
    }
    let home = v.as_mut_ptr();
    let (mut src, mut dst) = (home, scratch);
    for (pass, &at) in digits.iter().enumerate() {
        let at = usize::from(at);
        let mut next = starts(&counts);
        let mut following = [0usize; 256];
        match digits.get(pass + 1) {
            Some(&then) => {
                let then = usize::from(then);
                for i in 0..n {
                    // SAFETY: `i < n`, and `src` holds `n` initialized values (`v`, or
                    // `scratch` after a full pass).
                    let e = unsafe { src.add(i).read() };
                    let k = key(&e);
                    let d = k.digit(at);
                    // SAFETY: `next[d]` is below the end of digit `d`'s run, inside `dst`'s
                    // `n` slots: the run's length is the count of the keys with digit `d`,
                    // and `key` is pure (the function's contract).
                    unsafe { dst.add(next[d]).write(e) };
                    next[d] += 1;
                    following[k.digit(then)] += 1;
                }
            }
            None => {
                for i in 0..n {
                    // SAFETY: as above.
                    let e = unsafe { src.add(i).read() };
                    let d = key(&e).digit(at);
                    // SAFETY: as above.
                    unsafe { dst.add(next[d]).write(e) };
                    next[d] += 1;
                }
            }
        }
        counts = following;
        (src, dst) = (dst, src);
    }
    if src != home {
        // SAFETY: `src` is `scratch`, holding the `n` sorted values; `home` is `v`'s `n`
        // slots; they do not overlap (the contract).
        unsafe { ptr::copy_nonoverlapping(src, home, n) };
    }
}

/// [`lsd`] for keys wider than a word, read a byte at a time: each of the `bytes` digits is
/// counted in a pass of its own and skipped when every key shares it.
///
/// # Safety
///
/// As [`lsd`]'s.
pub(crate) unsafe fn lsd_bytes<E: Copy>(
    v: &mut [E],
    scratch: *mut E,
    bytes: usize,
    byte: impl Fn(&E, usize) -> u8,
) {
    let n = v.len();
    let home = v.as_mut_ptr();
    let (mut src, mut dst) = (home, scratch);
    let mut skipped_one = false;
    for at in 0..bytes {
        let mut counts = [0usize; 256];
        for i in 0..n {
            // SAFETY: `i < n`, and `src` holds `n` initialized values.
            counts[usize::from(byte(unsafe { &*src.add(i) }, at))] += 1;
        }
        if counts.contains(&n) {
            continue;
        }
        if plant::on(Plant::SkipVaryingDigit) && !skipped_one {
            skipped_one = true;
            continue;
        }
        let mut next = starts(&counts);
        for i in 0..n {
            // SAFETY: as in `lsd`.
            let e = unsafe { src.add(i).read() };
            let d = usize::from(byte(&e, at));
            // SAFETY: as in `lsd`: `byte` is pure, so each run holds what it counted.
            unsafe { dst.add(next[d]).write(e) };
            next[d] += 1;
        }
        (src, dst) = (dst, src);
    }
    if src != home {
        // SAFETY: as in `lsd`.
        unsafe { ptr::copy_nonoverlapping(src, home, n) };
    }
}

/// Buckets of the in-place sort at most this long are sorted by core's unstable sort on their
/// images. Measured (`examples/cost.rs`, `threshold`): see the crate's notes.
pub(crate) const MSD_SMALL: usize = 64;

/// One open level of the in-place sort: a range cut into its 256 buckets, and the next bucket
/// to descend into.
struct Frame {
    /// Where the range starts in the whole slice.
    start: usize,
    /// Where each bucket ends, relative to `start`.
    ends: [usize; 256],
    /// The next bucket to visit.
    next: usize,
    /// The index into the most-significant-first digits this range was cut by.
    level: usize,
}

impl Frame {
    const EMPTY: Frame = Frame { start: 0, ends: [0; 256], next: 256, level: 0 };
}

/// Cuts `v` by the first digit from `level` on (of `msb`, most significant first) on which
/// its keys differ: counts it, then moves each key to its bucket by cycles (American flag
/// sort). Returns that level and the buckets' ends, or `None` when the keys agree on every
/// digit left (all one value).
fn partition<T: Copy, W: Word>(
    v: &mut [T],
    msb: &[u8],
    mut level: usize,
    key: &impl Fn(&T) -> W,
) -> Option<(usize, [usize; 256])> {
    let n = v.len();
    loop {
        let at = usize::from(*msb.get(level)?);
        let mut counts = [0usize; 256];
        for e in v.iter() {
            counts[key(e).digit(at)] += 1;
        }
        if counts.contains(&n) {
            level += 1;
            continue;
        }
        let mut heads = starts(&counts);
        let mut ends = [0usize; 256];
        for (end, (&head, &count)) in ends.iter_mut().zip(heads.iter().zip(&counts)) {
            *end = head + count;
        }
        for b in 0..256 {
            while heads[b] < ends[b] {
                // SAFETY: `heads[b] < ends[b] <= n`.
                let mut x = unsafe { *v.get_unchecked(heads[b]) };
                let mut d = key(&x).digit(at);
                while d != b {
                    // SAFETY: bucket `d` still has an unplaced slot, so `heads[d] < ends[d]
                    // <= n`: its count is the number of keys with digit `d`, of which fewer
                    // than all are placed while `x` (one of them) is not, and `key` is pure.
                    let slot = unsafe { v.get_unchecked_mut(heads[d]) };
                    heads[d] += 1;
                    core::mem::swap(&mut x, slot);
                    d = key(&x).digit(at);
                }
                // SAFETY: `heads[b] < ends[b] <= n`, checked by the loop.
                unsafe { *v.get_unchecked_mut(heads[b]) = x };
                heads[b] += 1;
            }
        }
        return Some((level, ends));
    }
}

/// In-place most-significant-digit radix sort of `v` by `key`, over the digits `digits`
/// (least significant first, as [`varying`] gives them). Unstable: for a `RadixKey`, where
/// equal keys are one value, that is not observable. No allocation: an explicit stack of at
/// most `LEVELS` frames (2 KiB each) on the call's stack, `LEVELS` the word's bytes.
pub(crate) fn msd<T: Copy, W: Word, const LEVELS: usize>(
    v: &mut [T],
    digits: &[u8],
    key: impl Fn(&T) -> W,
) {
    let mut msb = [0u8; 16];
    let levels = digits.len();
    for (to, &at) in msb.iter_mut().zip(digits.iter().rev()) {
        *to = at;
    }
    let msb = &msb[..levels];
    let mut stack = [const { Frame::EMPTY }; LEVELS];
    let mut depth = 0;
    if let Some((level, ends)) = partition(v, msb, 0, &key) {
        stack[0] = Frame { start: 0, ends, next: 0, level };
        depth = 1;
    }
    while depth > 0 {
        let top = &mut stack[depth - 1];
        if top.next == 256 {
            depth -= 1;
            continue;
        }
        let b = top.next;
        top.next += 1;
        let lo = top.start + if b == 0 { 0 } else { top.ends[b - 1] };
        let hi = top.start + top.ends[b];
        let level = top.level + 1;
        if hi - lo < 2 || level == levels {
            continue;
        }
        let bucket = &mut v[lo..hi];
        if bucket.len() <= MSD_SMALL {
            bucket.sort_unstable_by_key(&key);
            continue;
        }
        // A child's level is above its parent's, so the frame at depth `d` has a level of at
        // least `d`, below `levels <= LEVELS`.
        if let Some((level, ends)) = partition(bucket, msb, level, &key) {
            stack[depth] = Frame { start: lo, ends, next: 0, level };
            depth += 1;
        }
    }
}
