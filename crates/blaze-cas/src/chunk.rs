//! Content-defined chunking (FastCDC, normalized): cut points depend on the bytes around them,
//! not on their offset, so two outputs that differ in a few places share every chunk but the
//! few around the differences, whatever was inserted or removed before them.

use std::ops::Range;

use blaze_ids::rng;

/// The shortest chunk but a value's last.
pub const MIN: usize = 16 << 10;
pub(crate) const AVERAGE: usize = 64 << 10;
/// The longest chunk.
pub const MAX: usize = 256 << 10;

/// Before [`AVERAGE`] a cut needs more of the hash's top bits zero (18), after it fewer (14),
/// which keeps chunk sizes near the average.
const MASK_SMALL: u64 = !0 << (64 - 18);
const MASK_LARGE: u64 = !0 << (64 - 14);

/// The gear table: 256 fixed random words (SplitMix64 from a fixed seed). Part of the format:
/// changing it changes every chunk and so every chunk key.
const GEAR: [u64; 256] = gear();

const fn gear() -> [u64; 256] {
    let mut out = [0; 256];
    let mut state: u64 = 0x626c_617a_6520_7632;
    let mut at = 0;
    while at < 256 {
        state = state.wrapping_add(rng::GAMMA);
        out[at] = rng::mix(state);
        at += 1;
    }
    out
}

/// Where the chunk that starts `bytes` ends, on the fastest path this CPU has. Every path gives
/// the cut [`cut_scalar`] gives.
fn cut(bytes: &[u8]) -> usize {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        // SAFETY: `avx512f` was just detected.
        return unsafe { cut_avx512(bytes) };
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: `neon` was just detected.
        return unsafe { cut_neon(bytes) };
    }
    cut_scalar(bytes)
}

/// Every `cut` this CPU can run, the scalar one first, for the differential tests.
#[cfg(test)]
pub(crate) fn paths() -> Vec<(&'static str, fn(&[u8]) -> usize)> {
    let mut out: Vec<(&'static str, fn(&[u8]) -> usize)> = vec![("scalar", cut_scalar)];
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        fn avx512(bytes: &[u8]) -> usize {
            // SAFETY: listed only after `avx512f` was detected.
            unsafe { cut_avx512(bytes) }
        }
        out.push(("avx512f", avx512));
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        fn neon(bytes: &[u8]) -> usize {
            // SAFETY: listed only after `neon` was detected.
            unsafe { cut_neon(bytes) }
        }
        out.push(("neon", neon));
    }
    out
}

/// The scalar loop from `at` with the hash of the bytes before it, for what the vector paths
/// leave: the positions after the last full block of 8.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn cut_from(bytes: &[u8], mut at: usize, mut hash: u64, middle: usize, end: usize) -> usize {
    while at < end {
        hash = (hash << 1).wrapping_add(GEAR[bytes[at] as usize]);
        let mask = if at < middle { MASK_SMALL } else { MASK_LARGE };
        if hash & mask == 0 {
            return at + 1;
        }
        at += 1;
    }
    end
}

/// [`cut_scalar`], 8 positions at a time. Over a block `p..p+8` with `g[k] = GEAR[b[p+k]]`,
/// `h[p+k] = (h[p-1] << (k+1)) + s[k]` where `s[k] = sum over j <= k of g[j] << (k-j)`, which
/// a log-step scan computes.
///
/// # Safety
///
/// The CPU has AVX-512F: `cut` and `paths` call it only after
/// `is_x86_feature_detected!("avx512f")`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn cut_avx512(bytes: &[u8]) -> usize {
    use std::arch::x86_64::*;

    if bytes.len() <= MIN {
        return bytes.len();
    }
    let end = bytes.len().min(MAX);
    let middle = end.min(AVERAGE);
    let table: &'static [u64; 256] = &GEAR;
    let lanes = _mm512_set_epi64(7, 6, 5, 4, 3, 2, 1, 0);
    let shifts = _mm512_set_epi64(8, 7, 6, 5, 4, 3, 2, 1);
    let last = _mm512_set1_epi64(7);
    let middles = _mm512_set1_epi64(middle as i64);
    let small = _mm512_set1_epi64(MASK_SMALL as i64);
    let large = _mm512_set1_epi64(MASK_LARGE as i64);
    let zero = _mm512_setzero_si512();
    let mut previous = zero;
    let mut at = MIN;
    while at + 8 <= end {
        let word = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let indices = _mm512_cvtepu8_epi64(_mm_cvtsi64_si128(word as i64));
        // SAFETY: every index is a byte, so below 256, and `table` has 256 words.
        let gear = unsafe { _mm512_i64gather_epi64::<8>(indices, table.as_ptr().cast()) };
        let mut scan = gear;
        scan = _mm512_add_epi64(scan, _mm512_slli_epi64::<1>(_mm512_alignr_epi64::<7>(scan, zero)));
        scan = _mm512_add_epi64(scan, _mm512_slli_epi64::<2>(_mm512_alignr_epi64::<6>(scan, zero)));
        scan = _mm512_add_epi64(scan, _mm512_slli_epi64::<4>(_mm512_alignr_epi64::<4>(scan, zero)));
        let hash = _mm512_add_epi64(_mm512_sllv_epi64(previous, shifts), scan);
        let positions = _mm512_add_epi64(_mm512_set1_epi64(at as i64), lanes);
        let masks =
            _mm512_mask_blend_epi64(_mm512_cmplt_epu64_mask(positions, middles), large, small);
        let cuts = !_mm512_test_epi64_mask(hash, masks);
        if cuts != 0 {
            return at + cuts.trailing_zeros() as usize + 1;
        }
        previous = _mm512_permutexvar_epi64(last, hash);
        at += 8;
    }
    let hash = _mm_cvtsi128_si64(_mm512_castsi512_si128(previous)) as u64;
    cut_from(bytes, at, hash, middle, end)
}

/// The AVX-512 path in NEON: the 8 lanes of a block as 4 pairs, and the table words loaded one at
/// a time, as there is no gather.
///
/// # Safety
///
/// The CPU has NEON: `cut` and `paths` call it only after
/// `is_aarch64_feature_detected!("neon")`.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn cut_neon(bytes: &[u8]) -> usize {
    use std::arch::aarch64::*;

    if bytes.len() <= MIN {
        return bytes.len();
    }
    let end = bytes.len().min(MAX);
    let middle = end.min(AVERAGE);
    let zero = vdupq_n_u64(0);
    let shifts = [
        vcombine_s64(vcreate_s64(1), vcreate_s64(2)),
        vcombine_s64(vcreate_s64(3), vcreate_s64(4)),
        vcombine_s64(vcreate_s64(5), vcreate_s64(6)),
        vcombine_s64(vcreate_s64(7), vcreate_s64(8)),
    ];
    let lanes = vcombine_u64(vcreate_u64(0), vcreate_u64(1));
    let middles = vdupq_n_u64(middle as u64);
    let small = vdupq_n_u64(MASK_SMALL);
    let large = vdupq_n_u64(MASK_LARGE);
    let mut previous = zero;
    let mut at = MIN;
    while at + 8 <= end {
        let block = &bytes[at..at + 8];
        let pair = |k: usize| (GEAR[block[k] as usize], GEAR[block[k + 1] as usize]);
        let (g0, g1, g2, g3) = (pair(0), pair(2), pair(4), pair(6));
        let s0 = vcombine_u64(vcreate_u64(g0.0), vcreate_u64(g0.1));
        let s1 = vcombine_u64(vcreate_u64(g1.0), vcreate_u64(g1.1));
        let s2 = vcombine_u64(vcreate_u64(g2.0), vcreate_u64(g2.1));
        let s3 = vcombine_u64(vcreate_u64(g3.0), vcreate_u64(g3.1));

        let (s0, s1, s2, s3) = (
            vaddq_u64(s0, vshlq_n_u64::<1>(vextq_u64::<1>(zero, s0))),
            vaddq_u64(s1, vshlq_n_u64::<1>(vextq_u64::<1>(s0, s1))),
            vaddq_u64(s2, vshlq_n_u64::<1>(vextq_u64::<1>(s1, s2))),
            vaddq_u64(s3, vshlq_n_u64::<1>(vextq_u64::<1>(s2, s3))),
        );
        let (s1, s2, s3) = (
            vaddq_u64(s1, vshlq_n_u64::<2>(s0)),
            vaddq_u64(s2, vshlq_n_u64::<2>(s1)),
            vaddq_u64(s3, vshlq_n_u64::<2>(s2)),
        );
        let (s2, s3) = (vaddq_u64(s2, vshlq_n_u64::<4>(s0)), vaddq_u64(s3, vshlq_n_u64::<4>(s1)));

        let hashes = [
            vaddq_u64(vshlq_u64(previous, shifts[0]), s0),
            vaddq_u64(vshlq_u64(previous, shifts[1]), s1),
            vaddq_u64(vshlq_u64(previous, shifts[2]), s2),
            vaddq_u64(vshlq_u64(previous, shifts[3]), s3),
        ];
        let mut cuts = [zero; 4];
        for (n, hash) in hashes.into_iter().enumerate() {
            let positions = vaddq_u64(vdupq_n_u64((at + 2 * n) as u64), lanes);
            let masks = vbslq_u64(vcltq_u64(positions, middles), small, large);
            cuts[n] = vceqzq_u64(vandq_u64(hash, masks));
        }
        let any = vorrq_u64(vorrq_u64(cuts[0], cuts[1]), vorrq_u64(cuts[2], cuts[3]));
        if vmaxvq_u32(vreinterpretq_u32_u64(any)) != 0 {
            for (n, cut) in cuts.into_iter().enumerate() {
                if vgetq_lane_u64::<0>(cut) != 0 {
                    return at + 2 * n + 1;
                }
                if vgetq_lane_u64::<1>(cut) != 0 {
                    return at + 2 * n + 2;
                }
            }
        }
        previous = vdupq_laneq_u64::<1>(hashes[3]);
        at += 8;
    }
    cut_from(bytes, at, vgetq_lane_u64::<0>(previous), middle, end)
}

/// Where the chunk that starts `bytes` ends: the reference every other path matches.
fn cut_scalar(bytes: &[u8]) -> usize {
    if bytes.len() <= MIN {
        return bytes.len();
    }
    let end = bytes.len().min(MAX);
    let middle = end.min(AVERAGE);
    let mut hash: u64 = 0;
    let mut at = MIN;
    while at < middle {
        hash = (hash << 1).wrapping_add(GEAR[bytes[at] as usize]);
        if hash & MASK_SMALL == 0 {
            return at + 1;
        }
        at += 1;
    }
    while at < end {
        hash = (hash << 1).wrapping_add(GEAR[bytes[at] as usize]);
        if hash & MASK_LARGE == 0 {
            return at + 1;
        }
        at += 1;
    }
    end
}

/// The chunks of `bytes`, in order, covering all of it: each at most [`MAX`], each but the last
/// at least [`MIN`]. These are the pieces the store keeps an output as.
pub fn chunks(bytes: &[u8]) -> Vec<Range<usize>> {
    let mut out = Vec::with_capacity(bytes.len() / AVERAGE + 1);
    out.extend(cuts(bytes));
    out
}

/// [`chunks`], cut as they are walked: no list is made.
pub fn cuts(bytes: &[u8]) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut start = 0;
    std::iter::from_fn(move || {
        (start < bytes.len()).then(|| {
            let len = cut(&bytes[start..]);
            start += len;
            start - len..start
        })
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut rng = rng::SplitMix64::new(seed);
        (0..len).map(|_| rng.next_u64() as u8).collect()
    }

    #[test]
    fn the_gear_table_is_the_format() {
        assert_eq!(GEAR[..3], [0x2db9_8404_6906_f47e, 0x9400_e56a_1a4e_6d5d, 0x7873_de3b_2d33_0196]);
        let mut stream = rng::SplitMix64::new(0x626c_617a_6520_7632);
        assert!(GEAR.iter().all(|&word| word == stream.next_u64()));
    }

    #[test]
    fn chunks_cover_everything_within_bounds() {
        for (len, seed) in [(0, 1), (1, 2), (MIN, 3), (MIN + 1, 4), (3 << 20, 5), (MAX * 3 + 7, 6)] {
            let bytes = noise(len, seed);
            let chunks = chunks(&bytes);
            let mut at = 0;
            for (n, chunk) in chunks.iter().enumerate() {
                assert_eq!(chunk.start, at);
                assert!(chunk.len() <= MAX);
                assert!(chunk.len() >= MIN || n + 1 == chunks.len(), "{len}: chunk {n}");
                at = chunk.end;
            }
            assert_eq!(at, len);
        }
    }

    #[test]
    fn chunks_average_near_the_average() {
        let bytes = noise(16 << 20, 9);
        let count = chunks(&bytes).len();
        let average = bytes.len() / count;
        assert!((AVERAGE / 2..AVERAGE * 2).contains(&average), "average {average}");
    }

    /// An insertion near the start changes only the chunks around it.
    #[test]
    fn an_insertion_shares_the_chunks_after_it() {
        let bytes = noise(4 << 20, 11);
        let mut edited = bytes.clone();
        edited.splice(1000..1000, *b"an insertion").for_each(drop);
        let set = |bytes: &[u8]| -> HashSet<Vec<u8>> {
            chunks(bytes).into_iter().map(|it| bytes[it].to_vec()).collect()
        };
        let (before, after) = (set(&bytes), set(&edited));
        let shared = before.intersection(&after).count();
        assert!(shared + 3 >= before.len(), "{shared} of {} shared", before.len());
    }

    #[test]
    fn the_same_bytes_cut_the_same() {
        let bytes = noise(2 << 20, 13);
        assert_eq!(chunks(&bytes), chunks(&bytes.clone()));
    }

    /// Every path cuts `bytes` where the scalar `cut` does, at every chunk start.
    fn every_path_agrees(bytes: &[u8], what: &str) {
        let paths = paths();
        let mut start = 0;
        loop {
            let rest = &bytes[start..];
            let expected = cut_scalar(rest);
            for (name, path) in &paths {
                assert_eq!(path(rest), expected, "{name} on {what} at {start} of {}", bytes.len());
            }
            if rest.is_empty() {
                break;
            }
            start += expected;
        }
    }

    fn lengths() -> Vec<usize> {
        let mut out = vec![0, 1, 7, 8, 9, 1000, MIN - 1, MIN, MAX - 1, MAX, MAX + 1, 5 << 20];
        for around in [MIN, AVERAGE, MAX] {
            out.extend(around - 9..=around + 9);
        }
        out.extend([MIN + 8 * 100 + 3, AVERAGE + 8 * 100 + 5, 2 * MAX + 13]);
        out
    }

    #[test]
    fn every_path_cuts_random_data_like_the_scalar_one() {
        for len in lengths() {
            for seed in 0..4 {
                every_path_agrees(&noise(len, seed * 1000 + len as u64), "noise");
            }
        }
    }

    #[test]
    fn every_path_agrees_over_many_seeds() {
        for seed in 100..164 {
            let mut rng = rng::SplitMix64::new(seed);
            let len = MIN + rng.below((3 * MAX) as u64) as usize;
            every_path_agrees(&noise(len, seed), "noise");
        }
    }

    #[test]
    fn constant_bytes_never_cut_early() {
        for byte in [0x00, 0xFF] {
            for len in lengths() {
                let bytes = vec![byte; len];
                for (name, path) in paths() {
                    assert_eq!(path(&bytes), len.min(MAX), "{name} on {byte:#x} * {len}");
                }
                every_path_agrees(&bytes, "constant bytes");
            }
        }
    }

    /// Zeros of length `len` (which never cut) with the four bytes ending at `at` chosen so the
    /// first cut is the test at `at`.
    fn planted(len: usize, at: usize, seed: u64) -> Vec<u8> {
        let end = len.min(MAX);
        let middle = end.min(AVERAGE);
        assert!(at >= MIN + 64 + 4 && at < end, "{at} in {len}");
        let mask = |pos: usize| if pos < middle { MASK_SMALL } else { MASK_LARGE };
        let mut saturated = 0u64;
        for _ in 0..64 {
            saturated = (saturated << 1).wrapping_add(GEAR[0]);
        }
        let mut rng = rng::SplitMix64::new(seed);
        for _ in 0..1u64 << 26 {
            let chosen = rng.next_u64().to_le_bytes();
            let mut hash = saturated;
            let mut first = None;
            for (k, byte) in chosen[..4].iter().enumerate() {
                let pos = at - 3 + k;
                hash = (hash << 1).wrapping_add(GEAR[*byte as usize]);
                if hash & mask(pos) == 0 {
                    first = Some(pos);
                    break;
                }
            }
            if first == Some(at) {
                let mut bytes = vec![0; len];
                bytes[at - 3..=at].copy_from_slice(&chosen[..4]);
                assert_eq!(cut_scalar(&bytes), at + 1);
                return bytes;
            }
        }
        panic!("no four bytes make the first cut at {at}");
    }

    #[test]
    fn every_path_finds_a_cut_in_each_lane() {
        let len = 3 * MAX;
        for block in [MIN + 8 * 10, MIN + 8 * 1000, AVERAGE + 8 * 10, MAX - 8 * 10] {
            for lane in 0..8 {
                let at = block + lane;
                let bytes = planted(len, at, at as u64);
                for (name, path) in paths() {
                    assert_eq!(path(&bytes), at + 1, "{name}: lane {lane} of the block at {block}");
                }
                every_path_agrees(&bytes, "a planted cut");
            }
        }
    }

    #[test]
    fn every_path_finds_a_cut_at_the_edges() {
        let short = MIN + 8 * 100 + 5;
        for (len, at) in [
            (3 * MAX, AVERAGE - 1),
            (3 * MAX, AVERAGE),
            (3 * MAX, AVERAGE + 1),
            (3 * MAX, MAX - 2),
            (3 * MAX, MAX - 1),
            (MAX, MAX - 1),
            (short, short - 1),
            (short, short - 2),
            (short, short - 9),
        ] {
            let bytes = planted(len, at, (len + at) as u64);
            for (name, path) in paths() {
                assert_eq!(path(&bytes), at + 1, "{name}: a cut at {at} of {len}");
            }
            every_path_agrees(&bytes, "a planted cut");
        }
    }

    #[test]
    fn chunks_match_scalar_chunks() {
        let scalar = |bytes: &[u8]| -> Vec<Range<usize>> {
            let mut out = Vec::new();
            let mut start = 0;
            while start < bytes.len() {
                let len = cut_scalar(&bytes[start..]);
                out.push(start..start + len);
                start += len;
            }
            out
        };
        for (len, seed) in [(0, 1), (MIN + 3, 2), (5 << 20, 3), (16 << 20, 4)] {
            let bytes = noise(len, seed);
            assert_eq!(chunks(&bytes), scalar(&bytes), "{len}");
        }
        let zeros = vec![0; 3 * MAX + 5];
        assert_eq!(chunks(&zeros), scalar(&zeros));
    }
}
