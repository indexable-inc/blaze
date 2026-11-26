//! Sans-IO parallelism seam for the plain store's pure CPU-bound passes (segment sealing,
//! segment digest verification, and cache-line blocked Bloom filter construction).
//!
//! Nothing here spawns OS threads, reads clocks, or performs IO: callers at the driver edge
//! (e.g. `blaze-cli`) supply a concrete [`Par`] backed by the command's worker pool, while
//! sans-IO cores default to [`Serial`] (zero overhead, inline execution) and tests run
//! [`ShuffledPar`] (deterministic permuted chunk execution) to prove Rule 25 + Rule 43
//! order-independence.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(unsafe_code)]

use core::alloc::Allocator;

use blaze_own::Vec;

/// Pure data-parallel execution seam for sans-IO plain-store algorithms (Rule 14, Rule 78).
///
/// Every implementation MUST preserve deterministic output independent of chunk execution order
/// (Rule 25): [`Par::map_range`] returns results strictly ordered from `0` to `count`, and
/// [`Par::for_each_mut_chunks`] visits disjoint mutable sub-slices covering `items` once each.
pub trait Par {
    /// Maximum worker parallelism available to this executor (`1` for serial execution).
    fn workers(&self) -> usize;

    /// Partitions `0..count` into contiguous ranges `[lo..hi)` of size `grain.max(1)` (the last
    /// range may be shorter), evaluates `f(lo, hi)` on each range, and returns the results
    /// strictly ordered by `lo` in `0..count`.
    fn map_range<R: Send, A: Allocator + Copy>(
        &self,
        count: usize,
        grain: usize,
        alloc: A,
        f: impl Fn(usize, usize) -> R + Sync,
    ) -> Vec<R, A>;

    /// Splits `items` into disjoint mutable chunks of at most `grain.max(1)` elements and invokes
    /// `f(start, chunk)` for each chunk, where `start` is the element offset of `chunk` in `items`
    /// (`items[start..start + chunk.len()]`).
    fn for_each_mut_chunks<T: Send>(
        &self,
        items: &mut [T],
        grain: usize,
        f: impl Fn(usize, &mut [T]) + Sync,
    );
}

/// Zero-overhead inline serial executor (`workers() == 1`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Serial;

impl Par for Serial {
    #[inline]
    fn workers(&self) -> usize {
        1
    }

    fn map_range<R: Send, A: Allocator + Copy>(
        &self,
        count: usize,
        grain: usize,
        alloc: A,
        f: impl Fn(usize, usize) -> R + Sync,
    ) -> Vec<R, A> {
        if count == 0 {
            return Vec::new_in(alloc);
        }
        let step = grain.max(1);
        let chunks = count.div_ceil(step);
        let mut out = Vec::with_capacity_in(chunks, alloc);
        let mut lo = 0usize;
        while lo < count {
            let hi = lo.saturating_add(step).min(count);
            out.push(f(lo, hi));
            lo = hi;
        }
        out
    }

    fn for_each_mut_chunks<T: Send>(
        &self,
        items: &mut [T],
        grain: usize,
        f: impl Fn(usize, &mut [T]) + Sync,
    ) {
        if items.is_empty() {
            return;
        }
        let step = grain.max(1);
        for (idx, chunk) in items.chunks_mut(step).enumerate() {
            f(idx.saturating_mul(step), chunk);
        }
    }
}

/// Deterministic shuffled executor for tests (Rule 25 + Rule 43): reports `workers` > 1 so
/// parallel branches are taken, and executes chunks in a deterministically permuted order drawn
/// from a seed or a [`blaze_sim::Tape`] to prove that outputs are completely order-independent.
#[cfg(test)]
pub struct ShuffledPar<'a> {
    workers: usize,
    source: ShuffleSource<'a>,
}

#[cfg(test)]
enum ShuffleSource<'a> {
    Seed(core::cell::Cell<u64>),
    Tape(&'a blaze_sim::Tape),
}

#[cfg(test)]
const SHUFFLE_TEST_ALLOC: blaze_mem::Kept = blaze_mem::Kept(blaze_mem::Tag::Store);

#[cfg(test)]
impl<'a> ShuffledPar<'a> {
    /// Creates a deterministic shuffled executor backed by a splitmix64 PRNG seeded with `seed`.
    pub fn from_seed(workers: usize, seed: u64) -> ShuffledPar<'static> {
        ShuffledPar {
            workers: workers.max(2),
            source: ShuffleSource::Seed(core::cell::Cell::new(seed.wrapping_add(1))),
        }
    }

    /// Creates a deterministic shuffled executor drawing its chunk permutation from `tape`.
    pub fn from_tape(workers: usize, tape: &'a blaze_sim::Tape) -> ShuffledPar<'a> {
        ShuffledPar { workers: workers.max(2), source: ShuffleSource::Tape(tape) }
    }

    fn next_below(&self, bound: usize) -> usize {
        if bound <= 1 {
            return 0;
        }
        let bound_u64 = u64::try_from(bound).unwrap_or(u64::MAX);
        let draw = match &self.source {
            ShuffleSource::Seed(state) => {
                let mut z = state.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
                state.set(z);
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                (z ^ (z >> 31)) % bound_u64
            }
            ShuffleSource::Tape(tape) => tape.below(bound_u64, "par_shuffle"),
        };
        usize::try_from(draw).unwrap_or(0)
    }

    fn shuffle<T>(&self, slice: &mut [T]) {
        let mut i = slice.len();
        while i > 1 {
            i -= 1;
            let j = self.next_below(i + 1);
            slice.swap(i, j);
        }
    }
}

#[cfg(test)]
impl Par for ShuffledPar<'_> {
    fn workers(&self) -> usize {
        self.workers
    }

    fn map_range<R: Send, A: Allocator + Copy>(
        &self,
        count: usize,
        grain: usize,
        alloc: A,
        f: impl Fn(usize, usize) -> R + Sync,
    ) -> Vec<R, A> {
        if count == 0 {
            return Vec::new_in(alloc);
        }
        let step = grain.max(1);
        let chunks = count.div_ceil(step);
        let mut order: Vec<usize, A> = Vec::with_capacity_in(chunks, alloc);
        for idx in 0..chunks {
            order.push(idx);
        }
        self.shuffle(order.as_mut_slice());
        let mut slots: Vec<Option<R>, A> = Vec::with_capacity_in(chunks, alloc);
        for _ in 0..chunks {
            slots.push(None);
        }
        for idx in order.into_iter() {
            let lo = idx.saturating_mul(step);
            let hi = lo.saturating_add(step).min(count);
            let value = f(lo, hi);
            if let Some(slot) = slots.get_mut(idx) {
                *slot = Some(value);
            }
        }
        let mut out: Vec<R, A> = Vec::with_capacity_in(chunks, alloc);
        for slot in slots.into_iter() {
            if let Some(value) = slot {
                out.push(value);
            }
        }
        out
    }

    fn for_each_mut_chunks<T: Send>(
        &self,
        items: &mut [T],
        grain: usize,
        f: impl Fn(usize, &mut [T]) + Sync,
    ) {
        if items.is_empty() {
            return;
        }
        let step = grain.max(1);
        let chunks = items.len().div_ceil(step);
        let mut list: Vec<(usize, &mut [T]), blaze_mem::Kept> = Vec::with_capacity_in(chunks, SHUFFLE_TEST_ALLOC);
        for (idx, chunk) in items.chunks_mut(step).enumerate() {
            list.push((idx.saturating_mul(step), chunk));
        }
        self.shuffle(list.as_mut_slice());
        for (start, chunk) in list.into_iter() {
            f(start, chunk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffled_par_matches_serial_order_and_mutates_disjoint_chunks() {
        let frame = crate::plain::test_root();
        let par = ShuffledPar::from_seed(16, 0x1234_5678);

        let got = par.map_range(103, 7, &frame, |lo, hi| (lo, hi));
        let want = Serial.map_range(103, 7, &frame, |lo, hi| (lo, hi));
        assert_eq!(got.as_slice(), want.as_slice());

        let mut a: Vec<usize, _> = Vec::with_capacity_in(103, &frame);
        let mut b: Vec<usize, _> = Vec::with_capacity_in(103, &frame);
        for _ in 0..103 {
            a.push(0);
            b.push(0);
        }
        par.for_each_mut_chunks(a.as_mut_slice(), 8, |start, chunk| {
            for (i, slot) in chunk.iter_mut().enumerate() {
                *slot = start + i + 1;
            }
        });
        Serial.for_each_mut_chunks(b.as_mut_slice(), 8, |start, chunk| {
            for (i, slot) in chunk.iter_mut().enumerate() {
                *slot = start + i + 1;
            }
        });
        assert_eq!(a.as_slice(), b.as_slice());
    }
}
