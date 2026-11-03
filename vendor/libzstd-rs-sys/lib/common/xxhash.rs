enum Align {
    Aligned,
    Unaligned,
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub(crate) struct XXH64_state_t {
    total_len: u64,
    v: [u64; 4],
    mem64: [u64; 4],
    memsize: u32,
    reserved32: u32,
    reserved64: u64,
}

impl XXH64_state_t {
    #[cfg(test)]
    fn new(seed: u64) -> Self {
        let mut state = XXH64_state_t::default();
        state.reset(seed);
        state
    }

    fn reset(&mut self, seed: u64) {
        // Zero out all fields.
        *self = XXH64_state_t::default();

        self.v[0] = seed.wrapping_add(XXH_PRIME64_1).wrapping_add(XXH_PRIME64_2);
        self.v[1] = seed.wrapping_add(XXH_PRIME64_2);
        self.v[2] = seed.wrapping_add(0);
        self.v[3] = seed.wrapping_sub(XXH_PRIME64_1);
    }

    fn mem64_as_bytes_ref(&self) -> &[u8; 32] {
        // SAFETY: casting an array of u64 to u8 is valid.
        unsafe { core::mem::transmute::<&[u64; 4], &[u8; 8 * 4]>(&self.mem64) }
    }

    fn mem64_as_bytes_mut(&mut self) -> &mut [u8; 32] {
        // SAFETY: casting an array of u64 to u8 is valid.
        unsafe { core::mem::transmute::<&mut [u64; 4], &mut [u8; 8 * 4]>(&mut self.mem64) }
    }
}

const XXH_PRIME64_1: u64 = 0x9e3779b185ebca87;
const XXH_PRIME64_2: u64 = 0xc2b2ae3d27d4eb4f;
const XXH_PRIME64_3: u64 = 0x165667b19e3779f9;
const XXH_PRIME64_4: u64 = 0x85ebca77c2b2ae63;
const XXH_PRIME64_5: u64 = 0x27d4eb2f165667c5;

#[inline(always)]
const fn XXH64_round(acc: u64, input: u64) -> u64 {
    input
        .wrapping_mul(XXH_PRIME64_2)
        .wrapping_add(acc)
        .rotate_left(31)
        .wrapping_mul(XXH_PRIME64_1)
}

#[inline(always)]
const fn XXH64_mergeRound(acc: u64, val: u64) -> u64 {
    (acc ^ XXH64_round(0, val))
        .wrapping_mul(XXH_PRIME64_1)
        .wrapping_add(XXH_PRIME64_4)
}

#[inline(always)]
const fn XXH64_avalanche(mut hash: u64) -> u64 {
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(XXH_PRIME64_2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(XXH_PRIME64_3);
    hash ^= hash >> 32;
    hash
}

fn XXH64_finalize(mut hash: u64, slice: &[u8], _align: Align) -> u64 {
    let (chunks, slice) = slice.as_chunks::<8>();
    for chunk in chunks {
        let k1 = XXH64_round(0, u64::from_le_bytes(*chunk));
        hash ^= k1;
        hash = (hash.rotate_left(27))
            .wrapping_mul(XXH_PRIME64_1)
            .wrapping_add(XXH_PRIME64_4);
    }

    let (chunks, slice) = slice.as_chunks::<4>();
    for chunk in chunks {
        hash ^= (u64::from(u32::from_le_bytes(*chunk))).wrapping_mul(XXH_PRIME64_1);
        hash = (hash.rotate_left(23))
            .wrapping_mul(XXH_PRIME64_2)
            .wrapping_add(XXH_PRIME64_3);
    }

    for byte in slice {
        hash ^= u64::from(*byte).wrapping_mul(XXH_PRIME64_5);
        hash = hash.rotate_left(11).wrapping_mul(XXH_PRIME64_1);
    }

    XXH64_avalanche(hash)
}

#[inline(always)]
fn XXH64_endian_align(input: &[u8], seed: u64, align: Align) -> u64 {
    let mut h64: u64;

    let (chunks, remainder) = input.as_chunks::<32>();

    if !chunks.is_empty() {
        let mut v1 = seed.wrapping_add(XXH_PRIME64_1).wrapping_add(XXH_PRIME64_2);
        let mut v2 = seed.wrapping_add(XXH_PRIME64_2);
        let mut v3 = seed.wrapping_add(0);
        let mut v4 = seed.wrapping_sub(XXH_PRIME64_1);

        for chunk in chunks {
            let ([n1, n2, n3, n4], &[]) = chunk.as_chunks() else {
                unreachable!()
            };

            v1 = XXH64_round(v1, u64::from_le_bytes(*n1));
            v2 = XXH64_round(v2, u64::from_le_bytes(*n2));
            v3 = XXH64_round(v3, u64::from_le_bytes(*n3));
            v4 = XXH64_round(v4, u64::from_le_bytes(*n4));
        }

        h64 = (v1.rotate_left(1))
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));

        h64 = XXH64_mergeRound(h64, v1);
        h64 = XXH64_mergeRound(h64, v2);
        h64 = XXH64_mergeRound(h64, v3);
        h64 = XXH64_mergeRound(h64, v4);
    } else {
        h64 = seed.wrapping_add(XXH_PRIME64_5);
    }
    h64 = h64.wrapping_add(input.len() as u64);
    XXH64_finalize(h64, remainder, align)
}

pub fn ZSTD_XXH64_slice(input: &[u8], seed: u64) -> u64 {
    XXH64_endian_align(input, seed, Align::Unaligned)
}

pub unsafe fn ZSTD_XXH64(input: *const core::ffi::c_void, len: usize, seed: u64) -> u64 {
    let slice = if input.is_null() {
        assert_eq!(len, 0);
        &[]
    } else {
        core::slice::from_raw_parts(input.cast::<u8>(), len)
    };
    XXH64_endian_align(slice, seed, Align::Unaligned)
}

pub(crate) fn ZSTD_XXH64_reset(state: &mut XXH64_state_t, seed: u64) {
    state.reset(seed)
}

pub(crate) fn ZSTD_XXH64_update_slice(state: &mut XXH64_state_t, mut slice: &[u8]) {
    state.total_len = state.total_len.wrapping_add(slice.len() as u64);

    if (state.memsize as usize).wrapping_add(slice.len()) < 32 {
        let in_use = state.memsize as usize;
        state.mem64_as_bytes_mut()[in_use..][..slice.len()].copy_from_slice(slice);
        state.memsize = state.memsize.wrapping_add(slice.len() as u32);
        return;
    }

    if state.memsize != 0 {
        let in_use = state.memsize as usize;
        let remainder = &mut state.mem64_as_bytes_mut()[in_use..];
        let (left, right) = slice.split_at(remainder.len());
        remainder.copy_from_slice(left);
        slice = right;

        state.v[0] = XXH64_round(state.v[0], state.mem64[0].to_le());
        state.v[1] = XXH64_round(state.v[1], state.mem64[1].to_le());
        state.v[2] = XXH64_round(state.v[2], state.mem64[2].to_le());
        state.v[3] = XXH64_round(state.v[3], state.mem64[3].to_le());

        state.memsize = 0;
    }

    let (chunks, remainder) = slice.as_chunks::<32>();
    for chunk in chunks {
        let ([n0, n1, n2, n3], &[]) = chunk.as_chunks() else {
            unreachable!()
        };

        state.v[0] = XXH64_round(state.v[0], u64::from_le_bytes(*n0));
        state.v[1] = XXH64_round(state.v[1], u64::from_le_bytes(*n1));
        state.v[2] = XXH64_round(state.v[2], u64::from_le_bytes(*n2));
        state.v[3] = XXH64_round(state.v[3], u64::from_le_bytes(*n3));
    }

    if !remainder.is_empty() {
        state.mem64_as_bytes_mut()[..remainder.len()].copy_from_slice(remainder);
        state.memsize = remainder.len() as u32;
    }
}
pub(crate) fn ZSTD_XXH64_digest(state: &mut XXH64_state_t) -> u64 {
    let mut h64;
    if state.total_len >= 32 {
        h64 = (state.v[0].rotate_left(1))
            .wrapping_add(state.v[1].rotate_left(7))
            .wrapping_add(state.v[2].rotate_left(12))
            .wrapping_add(state.v[3].rotate_left(18));

        h64 = XXH64_mergeRound(h64, state.v[0]);
        h64 = XXH64_mergeRound(h64, state.v[1]);
        h64 = XXH64_mergeRound(h64, state.v[2]);
        h64 = XXH64_mergeRound(h64, state.v[3]);
    } else {
        h64 = state.v[2].wrapping_add(XXH_PRIME64_5);
    }

    h64 = h64.wrapping_add(state.total_len);

    XXH64_finalize(
        h64,
        &state.mem64_as_bytes_ref()[..state.total_len as usize % 32],
        Align::Aligned,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use quickcheck::quickcheck;

    fn helper_u64(input: &[u8], seed: u64) -> u64 {
        unsafe { ZSTD_XXH64(input.as_ptr().cast(), input.len(), seed) }
    }

    quickcheck! {
        fn prop_xxh64_matches(input: Vec<u8>, seed: u64) -> bool {
            let expected = xxhash_rust::xxh64::xxh64(&input, seed);
            let actual = helper_u64(&input, seed);
            assert_eq!(expected, actual);
            expected == actual
        }
    }

    fn helper_state_u64(input: &[u8], seed: u64) -> u64 {
        let mut state = XXH64_state_t::default();
        ZSTD_XXH64_reset(&mut state, seed);

        ZSTD_XXH64_update_slice(&mut state, input);

        ZSTD_XXH64_digest(&mut state)
    }

    quickcheck! {
        fn prop_xxh64_state_matches(input: Vec<u8>, seed: u64) -> bool {
            let expected = xxhash_rust::xxh64::xxh64(&input, seed);
            let actual = helper_state_u64(&input, seed);
            assert_eq!(expected, actual);
            expected == actual
        }
    }

    #[test]
    fn test_xxh64_state_matches_chunked() {
        let seed = 0;

        let mut expected = xxhash_rust::xxh64::Xxh64::new(seed);
        let mut actual = XXH64_state_t::new(seed);

        expected.reset(seed);
        actual.reset(seed);
        let input = vec![vec![0], vec![1]];
        for chunk in input {
            expected.update(&chunk);
            ZSTD_XXH64_update_slice(&mut actual, &chunk);
        }
        assert_eq!(expected.digest(), ZSTD_XXH64_digest(&mut actual));

        expected.reset(seed);
        actual.reset(seed);
        let input = vec![[0u8; 32]];
        for chunk in input {
            expected.update(&chunk);
            ZSTD_XXH64_update_slice(&mut actual, &chunk);
        }
        assert_eq!(expected.digest(), ZSTD_XXH64_digest(&mut actual));

        expected.reset(seed);
        actual.reset(seed);
        let input = vec![
            vec![0],
            vec![
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 1,
            ],
        ];
        for chunk in input {
            expected.update(&chunk);
            ZSTD_XXH64_update_slice(&mut actual, &chunk);
        }
        assert_eq!(
            expected.digest().to_le_bytes(),
            ZSTD_XXH64_digest(&mut actual).to_le_bytes()
        );
    }

    quickcheck! {
        #[cfg(not(miri))]
        fn prop_xxh64_state_matches_chunked(input: Vec<Vec<u8>>, seed: u64) -> bool {
            let mut expected = xxhash_rust::xxh64::Xxh64::new( seed);
            let mut actual = {
                let mut state = XXH64_state_t::default();
                ZSTD_XXH64_reset(&mut state, seed);
                state
            };

            for chunk in input {
                expected.update(&chunk);
                ZSTD_XXH64_update_slice(&mut actual, &chunk);
            }

            assert_eq!(expected.digest(), ZSTD_XXH64_digest(&mut actual));
            expected.digest() == ZSTD_XXH64_digest(&mut actual)
        }
    }

    #[cfg(feature = "nightly")]
    extern crate test;

    #[cfg(feature = "nightly")]
    #[bench]
    fn xxh64_reference(b: &mut test::Bencher) {
        b.iter(|| {
            xxhash_rust::xxh64::xxh64(test::black_box(&[b'a'; 1024]), test::black_box(123));
        });
    }

    #[cfg(feature = "nightly")]
    #[bench]
    fn xxh64_ours(b: &mut test::Bencher) {
        b.iter(|| {
            helper_u64(test::black_box(&[b'a'; 1024]), test::black_box(123));
        });
    }
}
