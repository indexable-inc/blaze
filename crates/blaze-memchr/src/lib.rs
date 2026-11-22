//! Byte search: the first (or last) of one, two or three bytes, and the first of a byte string.
//!
//! One search, written once over a vector of byte lanes ([`Lanes`]), and its vectors:
//!
//! | vector | lanes | a lane's bits in the mask | used |
//! |---|---|---|---|
//! | [`Portable`]: `core::simd`'s `Simd<u8, N>`, `simd_eq`, `to_bitmask` | 16 or 32 | 1 | the search, on every target |
//! | [`Word`]: a `u64` (SWAR), the exact zero-byte test | 8 | 8 | the scalar twin the tests hold the vector to |
//!
//! The portable vector is the language's own (docs/v2/plan/design/std.md, "SIMD"): one source,
//! from which the compiler makes NEON on aarch64, SSE2 on x86-64 (where `to_bitmask` is
//! `movemask`) and scalar code on a target with no vectors; no `core::arch` path is left.
//!
//! What it costs (`examples/cost.rs`, release; 2026-11-15, one run, this machine: the
//! integration Mac, aarch64): bytes per nanosecond, the needle at the last byte.
//!
//! | search | 16 B | 256 B | 64 KiB |
//! |---|---|---|---|
//! | portable, 16 lanes (the search) | 12.05 | 57.30 | 79.60 |
//! | portable, 32 lanes | 3.24 | 49.56 | 89.49 |
//! | NEON intrinsics (the path this replaced, deleted) | 13.83 | 60.48 | 79.53 |
//! | word (SWAR) | 8.72 | 20.19 | 23.26 |
//! | naive loop | 3.17 | 3.68 | 4.24 |
//!
//! The portable 16 lanes equal the intrinsics at 64 KiB and are within 5% at 256 B and 13% at
//! 16 B, so the intrinsics went (one implementation). Two and three needles are the OR of
//! the comparisons; the first hit is the mask's trailing zeros, the last its leading zeros. A
//! search goes four vectors a step while it can, then one, then one load that ends at the
//! haystack's end and overlaps the bytes already searched (which held no hit, so the first hit
//! in it is the answer); a haystack shorter than one vector is searched a byte at a time.
//!
//! The byte string search ([`memmem`]) tests a needle's first and last bytes at every start a
//! vector at a time and compares the bytes between only where both hold (Muła's filter).
//!
//! The paths are those of the crates.io `memchr` this replaces (`memchr::memchr`,
//! `memchr::memchr_iter`, `memchr::memmem::find_iter`, `memchr::memmem::Finder`), so rustc's
//! lexer, copied unchanged into `crates/rustc_lexer`, names this crate through a renamed
//! dependency. The tests hold every vector, and the word on every target, to a byte-at-a-time
//! search by property.

#![no_std]
#![feature(portable_simd, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core, blaze::std_use, blaze::implicit_allocator, blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

use core::simd::{Mask, Simd, cmp::SimdPartialEq};

use blaze_int::Int;

/// A vector of byte lanes. `eq` gives a `Hit` (which lanes are equal), `or` and `and` combine
/// hits, and `mask` packs one into a `u64` in which lane `i` owns bits
/// `i * BITS .. (i + 1) * BITS`, nonzero exactly where the lane is a hit.
trait Lanes: Copy {
    type Hit: Copy;
    const WIDTH: usize;
    const BITS: u32;
    fn splat(byte: u8) -> Self;
    /// The first `WIDTH` bytes of `bytes`. Panics when it is shorter: every caller has checked.
    fn load(bytes: &[u8]) -> Self;
    fn eq(self, other: Self) -> Self::Hit;
    fn or(a: Self::Hit, b: Self::Hit) -> Self::Hit;
    fn and(a: Self::Hit, b: Self::Hit) -> Self::Hit;
    fn mask(hit: Self::Hit) -> u64;
}

/// `N` byte lanes of `core::simd`: the search's vector on every target.
#[derive(Clone, Copy)]
struct Portable<const N: usize>(Simd<u8, N>);

impl<const N: usize> Lanes for Portable<N> {
    type Hit = Mask<i8, N>;
    const WIDTH: usize = N;
    const BITS: u32 = 1;

    #[inline(always)]
    fn splat(byte: u8) -> Portable<N> {
        Portable(Simd::splat(byte))
    }

    #[inline(always)]
    fn load(bytes: &[u8]) -> Portable<N> {
        Portable(Simd::from_slice(bytes))
    }

    #[inline(always)]
    fn eq(self, other: Portable<N>) -> Mask<i8, N> {
        self.0.simd_eq(other.0)
    }

    #[inline(always)]
    fn or(a: Mask<i8, N>, b: Mask<i8, N>) -> Mask<i8, N> {
        a | b
    }

    #[inline(always)]
    fn and(a: Mask<i8, N>, b: Mask<i8, N>) -> Mask<i8, N> {
        a & b
    }

    #[inline(always)]
    fn mask(hit: Mask<i8, N>) -> u64 {
        hit.to_bitmask()
    }
}

/// Eight lanes in a word (SWAR): the scalar twin the tests hold the vector to.
#[derive(Clone, Copy)]
struct Word(u64);

const LOW_SEVEN: u64 = 0x7f7f_7f7f_7f7f_7f7f;

impl Lanes for Word {
    type Hit = u64;
    const WIDTH: usize = 8;
    const BITS: u32 = 8;

    #[inline(always)]
    fn splat(byte: u8) -> Word {
        Word(u64::from_ne_bytes([byte; 8]))
    }

    #[inline(always)]
    fn load(bytes: &[u8]) -> Word {
        let chunk = bytes.first_chunk::<8>().expect("a load of a word past the haystack's end");
        // Little-endian, so lane `i` is byte `i` and the lowest set bit is the first hit.
        Word(u64::from_le_bytes(*chunk))
    }

    /// 0x80 in each byte where the two are equal and 0 elsewhere, with no false hit: the sum
    /// `(x & 0x7f) + 0x7f` of a byte cannot carry into the next one.
    #[inline(always)]
    fn eq(self, other: Word) -> u64 {
        let x = self.0 ^ other.0;
        let low = (x & LOW_SEVEN) + LOW_SEVEN;
        !(low | x | LOW_SEVEN)
    }

    #[inline(always)]
    fn or(a: u64, b: u64) -> u64 {
        a | b
    }

    #[inline(always)]
    fn and(a: u64, b: u64) -> u64 {
        a & b
    }

    #[inline(always)]
    fn mask(hit: u64) -> u64 {
        hit
    }
}

/// The search's vector: 16 lanes, one NEON or SSE2 register. 32 lanes measured 3.7 times slower
/// at 16 bytes and 12% faster only at 64 KiB (the crate's table).
type Native = Portable<16>;

/// Each vector's one-needle search as a function of its own, for `examples/cost.rs` (and only
/// it): the portable vector at both widths, and the word.
#[doc(hidden)]
pub mod paths {
    use crate::{Portable, Word, find};

    #[inline(never)]
    pub fn portable_16(needle: u8, haystack: &[u8]) -> Option<usize> {
        find::<Portable<16>, 1>([needle], haystack)
    }

    #[inline(never)]
    pub fn portable_32(needle: u8, haystack: &[u8]) -> Option<usize> {
        find::<Portable<32>, 1>([needle], haystack)
    }

    #[inline(never)]
    pub fn word(needle: u8, haystack: &[u8]) -> Option<usize> {
        find::<Word, 1>([needle], haystack)
    }
}

/// The lane of a mask's lowest hit.
#[inline(always)]
fn first_lane<L: Lanes>(mask: u64) -> usize {
    (mask.trailing_zeros() / L::BITS).widen_to::<usize>()
}

/// The lane of a mask's highest hit.
#[inline(always)]
fn last_lane<L: Lanes>(mask: u64) -> usize {
    ((63 - mask.leading_zeros()) / L::BITS).widen_to::<usize>()
}

/// The lanes of `vector` equal to any of `needles`.
#[inline(always)]
fn hits<L: Lanes, const K: usize>(vector: L, needles: &[L; K]) -> L::Hit {
    let mut hit = vector.eq(needles[0]);
    for needle in &needles[1..] {
        hit = L::or(hit, vector.eq(*needle));
    }
    hit
}

/// The mask of the lanes of the vector at `bytes` equal to any of `needles`.
#[inline(always)]
fn mask_at<L: Lanes, const K: usize>(bytes: &[u8], needles: &[L; K]) -> u64 {
    L::mask(hits(L::load(bytes), needles))
}

/// The first byte of `haystack` that is one of `needles`.
#[inline(always)]
fn find<L: Lanes, const K: usize>(needles: [u8; K], haystack: &[u8]) -> Option<usize> {
    let width = L::WIDTH;
    if haystack.len() < width {
        return haystack.iter().position(|byte| needles.contains(byte));
    }
    let splats = needles.map(L::splat);
    let mut at = 0;
    while at + 4 * width <= haystack.len() {
        let a = hits(L::load(&haystack[at..]), &splats);
        let b = hits(L::load(&haystack[at + width..]), &splats);
        let c = hits(L::load(&haystack[at + 2 * width..]), &splats);
        let d = hits(L::load(&haystack[at + 3 * width..]), &splats);
        if L::mask(L::or(L::or(a, b), L::or(c, d))) != 0 {
            for (k, hit) in [a, b, c, d].into_iter().enumerate() {
                let mask = L::mask(hit);
                if mask != 0 {
                    return Some(at + k * width + first_lane::<L>(mask));
                }
            }
        }
        at += 4 * width;
    }
    while at + width <= haystack.len() {
        let mask = mask_at(&haystack[at..], &splats);
        if mask != 0 {
            return Some(at + first_lane::<L>(mask));
        }
        at += width;
    }
    if at < haystack.len() {
        // The last vector, overlapping bytes already searched, which held no hit.
        let last = haystack.len() - width;
        let mask = mask_at(&haystack[last..], &splats);
        if mask != 0 {
            return Some(last + first_lane::<L>(mask));
        }
    }
    None
}

/// The last byte of `haystack` that is one of `needles`.
#[inline(always)]
fn rfind<L: Lanes, const K: usize>(needles: [u8; K], haystack: &[u8]) -> Option<usize> {
    let width = L::WIDTH;
    if haystack.len() < width {
        return haystack.iter().rposition(|byte| needles.contains(byte));
    }
    let splats = needles.map(L::splat);
    let mut end = haystack.len();
    while end >= width {
        let start = end - width;
        let mask = mask_at(&haystack[start..], &splats);
        if mask != 0 {
            return Some(start + last_lane::<L>(mask));
        }
        end = start;
    }
    if end > 0 {
        // The first vector, overlapping bytes already searched, which held no hit.
        let mask = mask_at(haystack, &splats);
        if mask != 0 {
            return Some(last_lane::<L>(mask));
        }
    }
    None
}

/// The first start of `needle` in `haystack`; an empty needle is at 0.
#[inline(always)]
fn find_bytes<L: Lanes>(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    match needle {
        [] => return Some(0),
        [byte] => return find::<L, 1>([*byte], haystack),
        _ if needle.len() > haystack.len() => return None,
        _ => {}
    }
    let last = needle.len() - 1;
    // Every start in `0..starts` leaves room for the whole needle.
    let starts = haystack.len() - last;
    let inner = &needle[1..last];
    let first_byte = L::splat(needle[0]);
    let last_byte = L::splat(needle[last]);
    let lane_bits = (1u64 << L::BITS) - 1;
    let mut at = 0;
    // A vector of starts at `at` reads up to `at + last + WIDTH`, which `at + WIDTH <= starts`
    // keeps inside the haystack.
    while at + L::WIDTH <= starts {
        let firsts = L::load(&haystack[at..]).eq(first_byte);
        let lasts = L::load(&haystack[at + last..]).eq(last_byte);
        let mut mask = L::mask(L::and(firsts, lasts));
        while mask != 0 {
            let bit = mask.trailing_zeros();
            let start = at + (bit / L::BITS).widen_to::<usize>();
            if haystack[start + 1..start + last] == *inner {
                return Some(start);
            }
            mask &= !(lane_bits << (bit - bit % L::BITS));
        }
        at += L::WIDTH;
    }
    (at..starts).find(|&start| haystack[start..start + needle.len()] == *needle)
}

/// The first `needle` in `haystack`.
#[inline]
pub fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    find::<Native, 1>([needle], haystack)
}

/// The first byte of `haystack` that is `a` or `b`.
#[inline]
pub fn memchr2(a: u8, b: u8, haystack: &[u8]) -> Option<usize> {
    find::<Native, 2>([a, b], haystack)
}

/// The first byte of `haystack` that is `a`, `b` or `c`.
#[inline]
pub fn memchr3(a: u8, b: u8, c: u8, haystack: &[u8]) -> Option<usize> {
    find::<Native, 3>([a, b, c], haystack)
}

/// The last `needle` in `haystack`.
#[inline]
pub fn memrchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    rfind::<Native, 1>([needle], haystack)
}

/// The last byte of `haystack` that is `a` or `b`.
#[inline]
pub fn memrchr2(a: u8, b: u8, haystack: &[u8]) -> Option<usize> {
    rfind::<Native, 2>([a, b], haystack)
}

/// The last byte of `haystack` that is `a`, `b` or `c`.
#[inline]
pub fn memrchr3(a: u8, b: u8, c: u8, haystack: &[u8]) -> Option<usize> {
    rfind::<Native, 3>([a, b, c], haystack)
}

/// Every `needle` in `haystack`, by position, from either end.
#[inline]
pub fn memchr_iter(needle: u8, haystack: &[u8]) -> Memchr<'_> {
    Memchr { needle, haystack, start: 0, end: haystack.len() }
}

/// The iterator of [`memchr_iter`]: the positions of one byte in `start..end`, the part not yet
/// yielded from either end.
#[derive(Clone, Debug)]
pub struct Memchr<'h> {
    needle: u8,
    haystack: &'h [u8],
    start: usize,
    end: usize,
}

impl Iterator for Memchr<'_> {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        let at = self.start + memchr(self.needle, &self.haystack[self.start..self.end])?;
        self.start = at + 1;
        Some(at)
    }
}

impl DoubleEndedIterator for Memchr<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<usize> {
        let at = self.start + memrchr(self.needle, &self.haystack[self.start..self.end])?;
        self.end = at;
        Some(at)
    }
}

impl core::iter::FusedIterator for Memchr<'_> {}

/// Byte strings in byte strings.
pub mod memmem {
    use crate::{Native, find_bytes};

    /// The first start of `needle` in `haystack`; an empty needle is at 0.
    #[inline]
    pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        find_bytes::<Native>(haystack, needle)
    }

    /// The starts of `needle` in `haystack` that do not overlap, first to last; an empty needle
    /// is at every position, the end included.
    #[inline]
    pub fn find_iter<'h, 'n, N: ?Sized + AsRef<[u8]>>(
        haystack: &'h [u8],
        needle: &'n N,
    ) -> FindIter<'h, 'n> {
        FindIter { haystack, needle: needle.as_ref(), at: 0 }
    }

    /// The iterator of [`find_iter`]; `at` is where the next search starts.
    #[derive(Clone, Debug)]
    pub struct FindIter<'h, 'n> {
        haystack: &'h [u8],
        needle: &'n [u8],
        at: usize,
    }

    impl Iterator for FindIter<'_, '_> {
        type Item = usize;

        #[inline]
        fn next(&mut self) -> Option<usize> {
            let rest = self.haystack.get(self.at..)?;
            let found = self.at + find(rest, self.needle)?;
            self.at = found + self.needle.len().max(1);
            Some(found)
        }
    }

    impl core::iter::FusedIterator for FindIter<'_, '_> {}

    /// One needle, searched for in many haystacks.
    #[derive(Clone, Copy, Debug)]
    pub struct Finder<'n> {
        needle: &'n [u8],
    }

    impl<'n> Finder<'n> {
        #[inline]
        pub fn new<N: ?Sized + AsRef<[u8]>>(needle: &'n N) -> Finder<'n> {
            Finder { needle: needle.as_ref() }
        }

        #[inline]
        pub fn needle(&self) -> &'n [u8] {
            self.needle
        }

        /// The first start of the needle in `haystack`.
        #[inline]
        pub fn find(&self, haystack: &[u8]) -> Option<usize> {
            find(haystack, self.needle)
        }

        #[inline]
        pub fn find_iter<'h>(&self, haystack: &'h [u8]) -> FindIter<'h, 'n> {
            find_iter(haystack, self.needle)
        }
    }
}
