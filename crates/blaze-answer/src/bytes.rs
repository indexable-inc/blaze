//! Little-endian fixed-width fields, the bit-packed read lists and the dictionary's filter: the
//! only code that touches a byte of a segment by hand. Every read here indexes a slice whose
//! bounds the segment's open checked (segment.rs), so none of it fails on bytes that opened.

use core::alloc::Allocator;

use blaze_own::Vec;

pub(crate) fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(arr(b, at))
}

pub(crate) fn arr<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&b[at..at + N]);
    out
}

pub(crate) fn put_u16<A: Allocator>(out: &mut Vec<u8, A>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u32<A: Allocator>(out: &mut Vec<u8, A>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u64<A: Allocator>(out: &mut Vec<u8, A>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// The bits `x` needs: 0 for 0, 32 for `u32::MAX`.
pub(crate) fn bits(x: u32) -> u32 {
    32 - x.leading_zeros()
}

/// The bytes `count` values of `width` bits pack into.
pub(crate) fn packed_bytes(count: usize, width: u32) -> usize {
    (count * width as usize).div_ceil(8)
}

/// Appends `values`, each minus `base`, `width` bits apiece, low bits first: exactly
/// [`packed_bytes`] bytes. Every value is at least `base` and below `base + 2^width`.
pub(crate) fn pack<A: Allocator>(out: &mut Vec<u8, A>, values: impl Iterator<Item = u32>, base: u32, width: u32) {
    if width == 0 {
        return;
    }
    let mut acc: u64 = 0;
    let mut held: u32 = 0;
    for v in values {
        acc |= u64::from(v - base) << held;
        held += width;
        while held >= 8 {
            out.push((acc & 0xFF) as u8);
            acc >>= 8;
            held -= 8;
        }
    }
    if held > 0 {
        out.push((acc & 0xFF) as u8);
    }
}

/// The `i`th value [`pack`] wrote into `bytes` (its packed bytes alone), plus `base`.
pub(crate) fn unpack(bytes: &[u8], i: usize, base: u32, width: u32) -> u32 {
    if width == 0 {
        return base;
    }
    let bit = i * width as usize;
    let at = bit / 8;
    let shift = (bit % 8) as u32;
    let end = (at + 8).min(bytes.len());
    let mut word = [0u8; 8];
    word[..end - at].copy_from_slice(&bytes[at..end]);
    let mask = (1u64 << width) - 1;
    // Wrapping: a segment's writer keeps every value in range; bytes from elsewhere are held to
    // their digest, and an out-of-range id is then refused where it is looked up, never a panic.
    base.wrapping_add(((u64::from_le_bytes(word) >> shift) & mask) as u32)
}

/// The finaliser of SplitMix64: a key's bits spread over the word (keys of the simulation are
/// not digests, so the filter cannot take raw key bytes as uniform).
pub(crate) fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// One 64-bit hash of a key's bytes, in eight-byte chunks.
pub(crate) fn hash(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0x9E37_79B9_7F4A_7C15;
    for part in parts {
        for chunk in part.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            h = mix(h ^ u64::from_le_bytes(word));
        }
    }
    h
}

/// The words of a filter over `n` keys: ten bits a key, a power of two, at least one.
pub(crate) fn filter_words(n: usize) -> usize {
    (n * 10).div_ceil(64).max(1).next_power_of_two()
}

/// The three bits of key hash `h` in a filter of `words` words.
fn filter_bits(h: u64, words: usize) -> [usize; 3] {
    let m = (words * 64) as u64 - 1;
    let step = mix(h) | 1;
    [(h & m) as usize, (h.wrapping_add(step) & m) as usize, (h.wrapping_add(step.wrapping_mul(2)) & m) as usize]
}

pub(crate) fn filter_set(words: &mut [u64], h: u64) {
    for bit in filter_bits(h, words.len()) {
        words[bit / 64] |= 1 << (bit % 64);
    }
}

/// Whether the filter of `words` words at `at` in `b` may hold the key of hash `h`.
pub(crate) fn filter_has(b: &[u8], at: usize, words: usize, h: u64) -> bool {
    filter_bits(h, words).iter().all(|bit| (u64_at(b, at + (bit / 64) * 8) >> (bit % 64)) & 1 == 1)
}

/// The index of `needle` in the ascending `n` u32 column at `at`, or where it would go.
pub(crate) fn search_u32(b: &[u8], at: usize, n: usize, needle: u32) -> Result<usize, usize> {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match u32_at(b, at + mid * 4).cmp(&needle) {
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
            core::cmp::Ordering::Equal => return Ok(mid),
        }
    }
    Err(lo)
}
