//! High-throughput, zero-bounds-check 64-bit xxHash (`XXH64`) kernels backing `blaze-zstd`
//! (RFC 8878 content checksums) and `bstd::digest`.
//!
//! Reads four 64-bit lanes per 32-byte stripe directly via unaligned little-endian pointer loads
//! (`core::ptr::read_unaligned`) into four independent register accumulators, eliminating per-stripe
//! slice splits, bounds checks, and out-of-line helpers while remaining 100% bit-exact with the
//! canonical XXH64 specification.

use core::ptr;

use crate::Int;

const PRIME64_1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME64_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME64_3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME64_4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME64_5: u64 = 0x27D4_EB2F_1656_67C5;

#[inline(always)]
const fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(PRIME64_2))
        .rotate_left(31)
        .wrapping_mul(PRIME64_1)
}

#[inline(always)]
const fn merge_round(acc: u64, val: u64) -> u64 {
    let v = round(0, val);
    (acc ^ v).wrapping_mul(PRIME64_1).wrapping_add(PRIME64_4)
}

#[inline(always)]
const fn avalanche(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(PRIME64_2);
    h ^= h >> 29;
    h = h.wrapping_mul(PRIME64_3);
    h ^= h >> 32;
    h
}

#[inline(always)]
unsafe fn read_u64_le_at(p: *const u8, offset: usize) -> u64 {
    // SAFETY: Caller guarantees `p..p + offset + 8` lies within a single readable slice.
    let q = unsafe { p.add(offset) };
    // SAFETY: Caller guarantees `q..q + 8` is readable.
    u64::from_le(unsafe { ptr::read_unaligned(q.cast::<u64>()) })
}

#[inline(always)]
unsafe fn read_u32_le(p: *const u8) -> u32 {
    // SAFETY: Caller guarantees `p..p + 4` is readable.
    u32::from_le(unsafe { ptr::read_unaligned(p.cast::<u32>()) })
}

/// Process every complete 32-byte stripe in `data` into the four XXH64 streaming accumulators `v`
/// (`[v1, v2, v3, v4]`), returning the number of bytes consumed (`(data.len() / 32) * 32`).
///
/// Any trailing `< 32` bytes in `data[consumed..]` are left untouched for the caller's streaming
/// tail buffer.
#[inline]
pub fn xxhash64_stripes(v: &mut [u64; 4], data: &[u8]) -> usize {
    let consumed = data.len() & !31usize;
    if consumed == 0 {
        return 0;
    }

    let mut p = data.as_ptr();
    // SAFETY: `consumed <= data.len()`, so `p.add(consumed)` stays within or one-past-the-end of
    // `data`'s allocation.
    let limit = unsafe { p.add(consumed) };
    let [mut v1, mut v2, mut v3, mut v4] = *v;

    while p < limit {
        // SAFETY: `p < limit` and `(limit - p)` is a positive multiple of 32, so `p..p + 32` lies
        // strictly within `data[..consumed]`.
        let l0 = unsafe { read_u64_le_at(p, 0) };
        // SAFETY: `p..p + 32` lies strictly within `data[..consumed]`.
        let l1 = unsafe { read_u64_le_at(p, 8) };
        // SAFETY: `p..p + 32` lies strictly within `data[..consumed]`.
        let l2 = unsafe { read_u64_le_at(p, 16) };
        // SAFETY: `p..p + 32` lies strictly within `data[..consumed]`.
        let l3 = unsafe { read_u64_le_at(p, 24) };

        v1 = round(v1, l0);
        v2 = round(v2, l1);
        v3 = round(v3, l2);
        v4 = round(v4, l3);

        // SAFETY: `p < limit` and `(limit - p)` is a positive multiple of 32, so `p.add(32) <= limit`.
        p = unsafe { p.add(32) };
    }

    *v = [v1, v2, v3, v4];
    consumed
}

/// Compute the canonical one-shot 64-bit xxHash (`XXH64`) of `data` with `seed`.
#[inline]
#[must_use]
pub fn xxhash64_slice(data: &[u8], seed: u64) -> u64 {
    let len = data.len();
    let (mut h, consumed) = if len >= 32 {
        let mut v = [
            seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2),
            seed.wrapping_add(PRIME64_2),
            seed,
            seed.wrapping_sub(PRIME64_1),
        ];
        let consumed = xxhash64_stripes(&mut v, data);
        let [v1, v2, v3, v4] = v;
        let acc = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        let acc = merge_round(acc, v1);
        let acc = merge_round(acc, v2);
        let acc = merge_round(acc, v3);
        (merge_round(acc, v4), consumed)
    } else {
        (seed.wrapping_add(PRIME64_5), 0)
    };

    h = h.wrapping_add(len.widen_to::<u64>());

    let mut rem = len - consumed;
    // SAFETY: `consumed <= len`, so `data.as_ptr().add(consumed)` stays within or one-past-the-end
    // of `data`'s allocation.
    let mut p = unsafe { data.as_ptr().add(consumed) };

    while rem >= 8 {
        // SAFETY: `rem >= 8` guarantees `p..p + 8` lies within `data`.
        let k1 = round(0, unsafe { read_u64_le_at(p, 0) });
        h = (h ^ k1)
            .rotate_left(27)
            .wrapping_mul(PRIME64_1)
            .wrapping_add(PRIME64_4);
        // SAFETY: `rem >= 8` guarantees `p.add(8)` is at most one-past-the-end of `data`.
        p = unsafe { p.add(8) };
        rem -= 8;
    }

    if rem >= 4 {
        // SAFETY: `rem >= 4` guarantees `p..p + 4` lies within `data`.
        let k1 = u64::from(unsafe { read_u32_le(p) }).wrapping_mul(PRIME64_1);
        h = (h ^ k1)
            .rotate_left(23)
            .wrapping_mul(PRIME64_2)
            .wrapping_add(PRIME64_3);
        // SAFETY: `rem >= 4` guarantees `p.add(4)` is at most one-past-the-end of `data`.
        p = unsafe { p.add(4) };
        rem -= 4;
    }

    while rem > 0 {
        // SAFETY: `rem > 0` guarantees `p` points to a readable byte within `data`.
        let byte = unsafe { *p };
        h = (h ^ u64::from(byte).wrapping_mul(PRIME64_5))
            .rotate_left(11)
            .wrapping_mul(PRIME64_1);
        // SAFETY: `rem > 0` guarantees `p.add(1)` is at most one-past-the-end of `data`.
        p = unsafe { p.add(1) };
        rem -= 1;
    }

    avalanche(h)
}

#[cfg(test)]
mod tests {
    use super::{
        PRIME64_1, PRIME64_2, PRIME64_3, PRIME64_4, PRIME64_5, avalanche, merge_round, round,
        xxhash64_slice, xxhash64_stripes,
    };
    use crate::Int;

    fn fill_pattern(buf: &mut [u8]) {
        for (i, slot) in buf.iter_mut().enumerate() {
            *slot = (i.wrapping_mul(31).wrapping_add(7) & 0xFF).truncate_to::<u8>();
        }
    }

    fn stream_xxhash64(data: &[u8], seed: u64, chunk_size: usize) -> u64 {
        let mut v = [
            seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2),
            seed.wrapping_add(PRIME64_2),
            seed,
            seed.wrapping_sub(PRIME64_1),
        ];
        let mut buf = [0u8; 32];
        let mut buf_len = 0usize;
        let mut total_len = 0u64;

        let step = chunk_size.max(1);
        let mut pos = 0usize;
        while pos < data.len() {
            let end = pos.saturating_add(step).min(data.len());
            let mut chunk = data.get(pos..end).unwrap_or(&[]);
            pos = end;
            total_len = total_len.wrapping_add(chunk.len().widen_to::<u64>());

            if buf_len > 0 {
                let need = 32usize.saturating_sub(buf_len);
                if chunk.len() < need {
                    if let Some(dst) = buf.get_mut(buf_len..buf_len + chunk.len()) {
                        dst.copy_from_slice(chunk);
                    }
                    buf_len += chunk.len();
                    continue;
                }
                if let Some((head, tail)) = chunk.split_at_checked(need) {
                    if let Some(dst) = buf.get_mut(buf_len..32) {
                        dst.copy_from_slice(head);
                    }
                    let stripe = buf;
                    assert_eq!(xxhash64_stripes(&mut v, &stripe), 32);
                    buf_len = 0;
                    chunk = tail;
                }
            }

            let consumed = xxhash64_stripes(&mut v, chunk);
            assert_eq!(consumed, chunk.len() & !31usize);
            let rem = chunk.get(consumed..).unwrap_or(&[]);
            if !rem.is_empty() {
                if let Some(dst) = buf.get_mut(..rem.len()) {
                    dst.copy_from_slice(rem);
                }
                buf_len = rem.len();
            }
        }

        let mut h = if total_len >= 32 {
            let [v1, v2, v3, v4] = v;
            let acc = v1
                .rotate_left(1)
                .wrapping_add(v2.rotate_left(7))
                .wrapping_add(v3.rotate_left(12))
                .wrapping_add(v4.rotate_left(18));
            let acc = merge_round(acc, v1);
            let acc = merge_round(acc, v2);
            let acc = merge_round(acc, v3);
            merge_round(acc, v4)
        } else {
            seed.wrapping_add(PRIME64_5)
        };
        h = h.wrapping_add(total_len);

        let mut rem = buf.get(..buf_len).unwrap_or(&[]);
        while let Some((head, tail)) = rem.split_at_checked(8) {
            if let Ok(arr) = <[u8; 8]>::try_from(head) {
                let k1 = round(0, u64::from_le_bytes(arr));
                h = (h ^ k1)
                    .rotate_left(27)
                    .wrapping_mul(PRIME64_1)
                    .wrapping_add(PRIME64_4);
            }
            rem = tail;
        }
        if let Some((head, tail)) = rem.split_at_checked(4) {
            if let Ok(arr) = <[u8; 4]>::try_from(head) {
                h = (h ^ u64::from(u32::from_le_bytes(arr)).wrapping_mul(PRIME64_1))
                    .rotate_left(23)
                    .wrapping_mul(PRIME64_2)
                    .wrapping_add(PRIME64_3);
            }
            rem = tail;
        }
        for &b in rem {
            h = (h ^ u64::from(b).wrapping_mul(PRIME64_5))
                .rotate_left(11)
                .wrapping_mul(PRIME64_1);
        }
        avalanche(h)
    }

    #[test]
    fn xxhash64_standard_and_boundary_vectors() {
        assert_eq!(xxhash64_slice(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxhash64_slice(b"", 2_654_435_761), 0xAC75_FDA2_929B_17EF);
        assert_eq!(xxhash64_slice(b"a", 0), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(xxhash64_slice(b"hello", 0), 0x26C7_827D_889F_6DA3);
        assert_eq!(
            xxhash64_slice(b"hello", 0x1234_5678_9ABC_DEF0),
            0xF9D3_F097_0DAB_1E3E
        );

        // RFC 8878 / C zstd 1.5.7 low-32-bit content checksum cross-checks.
        assert_eq!(xxhash64_slice(b"hello zstd world!", 0) & 0xFFFF_FFFF, 0xB60C_1CA5);
        let rle_4k = [b'A'; 4_096];
        assert_eq!(xxhash64_slice(&rle_4k, 0) & 0xFFFF_FFFF, 0x6D2B_58C3);

        // Exact stripe boundary vectors: 31B, 32B, 33B, 64B, 1000B.
        let mut pat = [0u8; 1_000];
        fill_pattern(&mut pat);

        assert_eq!(xxhash64_slice(&pat[..31], 0), 0x4A74_F3A1_A39A_D4A1);
        assert_eq!(xxhash64_slice(&pat[..31], PRIME64_1), 0x659E_C2B1_252D_56F5);
        assert_eq!(xxhash64_slice(&pat[..32], 0), 0x8D57_D6A4_671C_C43D);
        assert_eq!(xxhash64_slice(&pat[..32], PRIME64_1), 0xF113_D245_7128_CF6D);
        assert_eq!(xxhash64_slice(&pat[..33], 0), 0x62C9_FD21_ED85_7664);
        assert_eq!(xxhash64_slice(&pat[..33], PRIME64_1), 0x8457_5B86_0242_FE41);
        assert_eq!(xxhash64_slice(&pat[..64], 0), 0x7BBA_BBC4_5729_D17E);
        assert_eq!(xxhash64_slice(&pat[..64], PRIME64_1), 0xEA2D_5E8A_98BE_C153);
        assert_eq!(xxhash64_slice(&pat, 0), 0x9959_4F48_2804_3D35);
        assert_eq!(xxhash64_slice(&pat, PRIME64_1), 0xD79C_60B9_4F81_CBF0);
    }

    #[test]
    fn xxhash64_streaming_and_unaligned_agreement() {
        let mut raw = [0u8; 1_008];
        fill_pattern(&mut raw);

        // Short inputs (< 32B) must leave accumulators unchanged and return 0 consumed.
        let init_v = [1u64, 2, 3, 4];
        for len in 0..32usize {
            let mut v = init_v;
            assert_eq!(xxhash64_stripes(&mut v, &raw[..len]), 0);
            assert_eq!(v, init_v);
        }

        let seeds = [0u64, PRIME64_1, 0xDEAD_BEEF_CAFE_BABE];
        let chunk_sizes = [1usize, 3, 7, 13, 31, 32, 33, 64, 97];
        let lengths = [0usize, 1, 3, 4, 7, 8, 15, 16, 31, 32, 33, 63, 64, 65, 127, 128, 129, 1_000];

        for &seed in &seeds {
            for &len in &lengths {
                // Test every misalignment offset 0..8 mod 8.
                for align in 0..8usize {
                    let slice = &raw[align..align + len];
                    let oneshot = xxhash64_slice(slice, seed);
                    for &chunk in &chunk_sizes {
                        assert_eq!(
                            stream_xxhash64(slice, seed, chunk),
                            oneshot,
                            "mismatch for seed={seed:#x} len={len} align={align} chunk={chunk}"
                        );
                    }
                }
            }
        }
    }
}
