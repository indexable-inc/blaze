//! Byte search: the first (or last) of one, two or three bytes, and the first of a byte string.
//!
//! One search, written once over a vector of byte lanes ([`Lanes`]), and three vectors:
//!
//! | target | vector | a lane's bits in the mask |
//! |---|---|---|
//! | aarch64 | NEON, 16 lanes; the mask by `shrn` (a nibble per lane) | 4 |
//! | x86-64 | SSE2, 16 lanes; the mask by `movemask` | 1 |
//! | every other | a `u64` word of 8 lanes (SWAR), the exact zero-byte test | 8 |
//!
//! NEON and SSE2 are in the baseline of every aarch64 and x86-64 target, so nothing is detected
//! at run time and each target compiles one path. A search goes four vectors a step while it
//! can, then one, then one load that ends at the haystack's end and overlaps the bytes already
//! searched (which held no hit, so the first hit in it is the answer); a haystack shorter than
//! one vector is searched a byte at a time.
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
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::trust(
    unsafe,
    reason = "the NEON and SSE2 intrinsics: each load reads exactly one vector from a `&[u8; 16]`, and every other intrinsic is a `#[target_feature]` function of a feature in the baseline of the only target that compiles it"
)]

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

use blaze_int::Int;

/// A vector of byte lanes. `eq` and its `and`/`or` give a hit vector; `mask` packs it into a
/// `u64` in which lane `i` owns bits `i * BITS .. (i + 1) * BITS`, nonzero exactly where the lane
/// is a hit.
trait Lanes: Copy {
    const WIDTH: usize;
    const BITS: u32;
    fn splat(byte: u8) -> Self;
    /// The first `WIDTH` bytes of `bytes`. Panics when it is shorter: every caller has checked.
    fn load(bytes: &[u8]) -> Self;
    fn eq(self, other: Self) -> Self;
    fn or(self, other: Self) -> Self;
    fn and(self, other: Self) -> Self;
    fn mask(self) -> u64;
}

/// Eight lanes in a word (SWAR): the portable vector, and the one of the tests on every target.
#[derive(Clone, Copy)]
struct Word(u64);

const LOW_SEVEN: u64 = 0x7f7f_7f7f_7f7f_7f7f;

impl Lanes for Word {
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
    fn eq(self, other: Word) -> Word {
        let x = self.0 ^ other.0;
        let low = (x & LOW_SEVEN) + LOW_SEVEN;
        Word(!(low | x | LOW_SEVEN))
    }

    #[inline(always)]
    fn or(self, other: Word) -> Word {
        Word(self.0 | other.0)
    }

    #[inline(always)]
    fn and(self, other: Word) -> Word {
        Word(self.0 & other.0)
    }

    #[inline(always)]
    fn mask(self) -> u64 {
        self.0
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use core::arch::aarch64::{
        uint8x16_t, vandq_u8, vceqq_u8, vdupq_n_u8, vget_lane_u64, vld1q_u8, vorrq_u8,
        vreinterpret_u64_u8, vreinterpretq_u16_u8, vshrn_n_u16,
    };

    use crate::Lanes;

    #[derive(Clone, Copy)]
    pub(crate) struct Neon(uint8x16_t);

    // Every block below calls a `#[target_feature(enable = "neon")]` intrinsic, which is unsafe
    // only for the feature: NEON is in the baseline of every aarch64 target.
    impl Lanes for Neon {
        const WIDTH: usize = 16;
        const BITS: u32 = 4;

        #[inline(always)]
        fn splat(byte: u8) -> Neon {
            // SAFETY: NEON is in every aarch64 target's baseline.
            Neon(unsafe { vdupq_n_u8(byte) })
        }

        #[inline(always)]
        fn load(bytes: &[u8]) -> Neon {
            let chunk = bytes.first_chunk::<16>().expect("a load of a vector past the haystack's end");
            // SAFETY: `chunk` is 16 readable bytes, and `vld1q_u8` reads 16 with no alignment.
            Neon(unsafe { vld1q_u8(chunk.as_ptr()) })
        }

        #[inline(always)]
        fn eq(self, other: Neon) -> Neon {
            // SAFETY: NEON is in every aarch64 target's baseline.
            Neon(unsafe { vceqq_u8(self.0, other.0) })
        }

        #[inline(always)]
        fn or(self, other: Neon) -> Neon {
            // SAFETY: NEON is in every aarch64 target's baseline.
            Neon(unsafe { vorrq_u8(self.0, other.0) })
        }

        #[inline(always)]
        fn and(self, other: Neon) -> Neon {
            // SAFETY: NEON is in every aarch64 target's baseline.
            Neon(unsafe { vandq_u8(self.0, other.0) })
        }

        /// Each 16-bit pair of lanes shifted right by 4 and narrowed to a byte: lane `i` (0x00
        /// or 0xff) becomes the nibble at bits `4 * i ..`.
        #[inline(always)]
        fn mask(self) -> u64 {
            // SAFETY: NEON is in every aarch64 target's baseline.
            let pairs = unsafe { vreinterpretq_u16_u8(self.0) };
            // SAFETY: NEON is in every aarch64 target's baseline.
            let nibbles = unsafe { vshrn_n_u16::<4>(pairs) };
            // SAFETY: NEON is in every aarch64 target's baseline.
            let word = unsafe { vreinterpret_u64_u8(nibbles) };
            // SAFETY: NEON is in every aarch64 target's baseline.
            unsafe { vget_lane_u64::<0>(word) }
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod sse2 {
    use core::arch::x86_64::{
        __m128i, _mm_and_si128, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_or_si128,
        _mm_set1_epi8,
    };

    use crate::Lanes;

    #[derive(Clone, Copy)]
    pub(crate) struct Sse2(__m128i);

    // Every block below calls a `#[target_feature(enable = "sse2")]` intrinsic, which is unsafe
    // only for the feature: SSE2 is in the baseline of every x86-64 target.
    impl Lanes for Sse2 {
        const WIDTH: usize = 16;
        const BITS: u32 = 1;

        #[inline(always)]
        fn splat(byte: u8) -> Sse2 {
            // SAFETY: SSE2 is in every x86-64 target's baseline.
            Sse2(unsafe { _mm_set1_epi8(byte.cast_signed()) })
        }

        #[inline(always)]
        fn load(bytes: &[u8]) -> Sse2 {
            let chunk = bytes.first_chunk::<16>().expect("a load of a vector past the haystack's end");
            // SAFETY: `chunk` is 16 readable bytes, and `_mm_loadu_si128` reads 16 with no
            // alignment.
            Sse2(unsafe { _mm_loadu_si128(chunk.as_ptr().cast::<__m128i>()) })
        }

        #[inline(always)]
        fn eq(self, other: Sse2) -> Sse2 {
            // SAFETY: SSE2 is in every x86-64 target's baseline.
            Sse2(unsafe { _mm_cmpeq_epi8(self.0, other.0) })
        }

        #[inline(always)]
        fn or(self, other: Sse2) -> Sse2 {
            // SAFETY: SSE2 is in every x86-64 target's baseline.
            Sse2(unsafe { _mm_or_si128(self.0, other.0) })
        }

        #[inline(always)]
        fn and(self, other: Sse2) -> Sse2 {
            // SAFETY: SSE2 is in every x86-64 target's baseline.
            Sse2(unsafe { _mm_and_si128(self.0, other.0) })
        }

        #[inline(always)]
        fn mask(self) -> u64 {
            // SAFETY: SSE2 is in every x86-64 target's baseline.
            let bits = unsafe { _mm_movemask_epi8(self.0) };
            u64::from(bits.cast_unsigned())
        }
    }
}

#[cfg(target_arch = "aarch64")]
type Native = neon::Neon;
#[cfg(target_arch = "x86_64")]
type Native = sse2::Sse2;
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
type Native = Word;

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
fn hits<L: Lanes, const K: usize>(vector: L, needles: &[L; K]) -> L {
    let mut hit = vector.eq(needles[0]);
    for needle in &needles[1..] {
        hit = hit.or(vector.eq(*needle));
    }
    hit
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
        if a.or(b).or(c.or(d)).mask() != 0 {
            for (k, vector) in [a, b, c, d].into_iter().enumerate() {
                let mask = vector.mask();
                if mask != 0 {
                    return Some(at + k * width + first_lane::<L>(mask));
                }
            }
        }
        at += 4 * width;
    }
    while at + width <= haystack.len() {
        let mask = hits(L::load(&haystack[at..]), &splats).mask();
        if mask != 0 {
            return Some(at + first_lane::<L>(mask));
        }
        at += width;
    }
    if at < haystack.len() {
        // The last vector, overlapping bytes already searched, which held no hit.
        let last = haystack.len() - width;
        let mask = hits(L::load(&haystack[last..]), &splats).mask();
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
        let mask = hits(L::load(&haystack[start..]), &splats).mask();
        if mask != 0 {
            return Some(start + last_lane::<L>(mask));
        }
        end = start;
    }
    if end > 0 {
        // The first vector, overlapping bytes already searched, which held no hit.
        let mask = hits(L::load(haystack), &splats).mask();
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
        let mut mask = firsts.and(lasts).mask();
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
