//! A whole chunk and a partial one, side by side (x86-64, AVX2 or AVX-512).
//!
//! An input of 1089 to 2047 bytes is one whole chunk and a partial
//! chunk of 2 to 16 blocks. One chunk is one dependency chain, about 200
//! cycles a block, and the two chains are independent; the AVX2 and AVX-512
//! kernels hash two inputs side by side, in one register's two halves, for
//! about the price of one. So the first blocks of both chunks go to that
//! path together (all but the partial chunk's last, which alone carries
//! CHUNK_END), and each chunk then finishes alone. At 1536 bytes the chain
//! falls from 25 blocks to 18 (llvm-mca: a paired block costs 0.92-1.09
//! times a single one on Skylake to Zen 4). SSE4.1 hashes two inputs one
//! after the other, and the NEON and SME2 platforms have their own kernels
//! for a partial chunk (q1), so [`takes`] admits AVX2 and AVX-512 alone.

// Without the AVX2 and AVX-512 assembly, takes() is false and the rest unused.
#![cfg_attr(not(any(blake3_avx2_ffi, blake3_avx512_ffi)), allow(unused_variables, dead_code, unreachable_code))]

use crate::platform::{Platform, words_from_le_bytes_32};
use crate::{BLOCK_LEN, CHUNK_END, CHUNK_LEN, CHUNK_START, CVWords, OUT_LEN};

/// Whether [`parent`] hashes an input of `len` bytes on `platform`: a whole
/// chunk and a partial one of two blocks or more, on AVX2 or AVX-512 (whose
/// assembly is in this build).
#[inline]
pub(crate) fn takes(platform: Platform, len: usize) -> bool {
    takes_platform(platform) && (CHUNK_LEN + BLOCK_LEN + 1..2 * CHUNK_LEN).contains(&len)
}

/// Whether [`hash_many_blocks`] runs on `platform`: AVX2 or AVX-512, whose
/// assembly is in this build.
#[inline]
pub(crate) fn takes_platform(platform: Platform) -> bool {
    match platform {
        #[cfg(blake3_avx2_ffi)]
        Platform::AVX2 => true,
        #[cfg(blake3_avx512_ffi)]
        Platform::AVX512 => true,
        _ => false,
    }
}

/// The chaining values of `input`'s two chunks, at `chunk_counter` and the
/// one after, as `compress_chunks_parallel` returns them. `takes(platform, input.len())` must hold.
pub(crate) fn parent(input: &[u8], key: &CVWords, chunk_counter: u64, flags: u8, platform: Platform) -> [u8; BLOCK_LEN] {
    assert!(takes(platform, input.len()), "a whole chunk and a partial one of two blocks or more, on AVX2 or AVX-512");
    let (whole, partial) = input.split_at(CHUNK_LEN);
    let paired = partial.len().div_ceil(BLOCK_LEN) - 1;
    let mut out = [0u8; BLOCK_LEN];
    // Sound: each pointer reaches `blocks` whole blocks (the whole chunk's
    // 16; the partial chunk's `paired`, all but its last), `out` has room
    // for the values, and `takes` admitted only a platform the CPU has.
    unsafe {
        // The first `paired` blocks of both chunks, side by side.
        let mut halfway = [0u8; 2 * OUT_LEN];
        hash_many_blocks(platform, &[whole.as_ptr(), partial.as_ptr()], paired, key, chunk_counter, true, flags, CHUNK_START, 0, &mut halfway);
        // The whole chunk's other blocks, from its chaining value so far.
        let cv = words_from_le_bytes_32(halfway[..OUT_LEN].try_into().unwrap());
        let rest = whole[paired * BLOCK_LEN..].as_ptr();
        hash_many_blocks(platform, &[rest], CHUNK_LEN / BLOCK_LEN - paired, &cv, chunk_counter, false, flags, 0, CHUNK_END, &mut out[..OUT_LEN]);
        // The partial chunk's last block, zero-padded.
        let mut cv = words_from_le_bytes_32(halfway[OUT_LEN..].try_into().unwrap());
        let last = &partial[paired * BLOCK_LEN..];
        let mut block = [0u8; BLOCK_LEN];
        block[..last.len()].copy_from_slice(last);
        platform.compress_in_place(&mut cv, &block, last.len() as u8, chunk_counter + 1, flags | CHUNK_END);
        out[OUT_LEN..].copy_from_slice(&crate::hasher::le_bytes(&cv));
    }
    out
}

/// The platform's assembly `hash_many`, with the block count at run time
/// (AVX2 and AVX-512; elsewhere each input in turn). Unsafe: each of `inputs` reaches `blocks`
/// blocks, and the CPU has the platform.
#[allow(clippy::too_many_arguments)]
// Out of line: inlined into Platform::hash_many once its portable arm went,
// batches of one to three 64 B messages after other work ran 5-12% slower on
// the i7, and level with it out of line (24 pairs each; data
// 2026-10-27/no-portable-i7). Warm, both forms run the same instructions.
#[inline(never)]
pub(crate) unsafe fn hash_many_blocks(
    platform: Platform,
    inputs: &[*const u8],
    blocks: usize,
    key: &CVWords,
    counter: u64,
    increment: bool,
    flags: u8,
    flags_start: u8,
    flags_end: u8,
    out: &mut [u8],
) {
    assert!(out.len() >= inputs.len() * OUT_LEN && (1..=CHUNK_LEN / BLOCK_LEN).contains(&blocks));
    if !takes_platform(platform) {
        // Elsewhere one input after another on the platform's compression
        // (for the tests of hash_many's batches on every platform).
        for (i, &p) in inputs.iter().enumerate() {
            let mut cv = *key;
            for b in 0..blocks {
                // Sound: `p` reaches `blocks` blocks.
                let block: &[u8; BLOCK_LEN] = unsafe { &*(p.add(b * BLOCK_LEN) as *const [u8; BLOCK_LEN]) };
                let f = flags | if b == 0 { flags_start } else { 0 } | if b + 1 == blocks { flags_end } else { 0 };
                platform.compress_in_place(&mut cv, block, BLOCK_LEN as u8, counter + if increment { i as u64 } else { 0 }, f);
            }
            out[i * OUT_LEN..][..OUT_LEN].copy_from_slice(&crate::hasher::le_bytes(&cv));
        }
        return;
    }
    let (n, input, key, out) = (inputs.len(), inputs.as_ptr(), key.as_ptr(), out.as_mut_ptr());
    match platform {
        #[cfg(blake3_avx2_ffi)]
        Platform::AVX2 => unsafe {
            crate::avx2::ffi::blake3_servil_hash_many_avx2(input, n, blocks, key, counter, increment, flags, flags_start, flags_end, out)
        },
        #[cfg(blake3_avx512_ffi)]
        Platform::AVX512 => unsafe {
            crate::avx512::ffi::blake3_servil_hash_many_avx512(input, n, blocks, key, counter, increment, flags, flags_start, flags_end, out)
        },
        _ => unreachable!("takes_platform admitted AVX2 or AVX-512"),
    }
}
