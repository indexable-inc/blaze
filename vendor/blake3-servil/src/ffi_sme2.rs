//! AArch64 SME2 implementation, 512-bit streaming vector length (Apple M4
//! and later). The kernels live in c/blake3_sme2_aarch64.S.
//!
//! The assembly handles exactly one shape: groups of sixteen whole inputs,
//! either 1024-byte chunks (sixteen blocks each, counter incrementing) or
//! 64-byte parent blocks (one block each, counter fixed). This wrapper feeds
//! it every full group and hands anything else to the NEON backend: a
//! remainder of fewer than sixteen inputs, chunk hashing with
//! `IncrementCounter::No`, or parent hashing with `IncrementCounter::Yes`.
//!
//! The kernels return the streaming vector length in 32-bit lanes and do
//! work only when it is 16. `Platform::detect()` checks that once and
//! selects NEON otherwise, so a return value other than 16 here is a
//! contract violation and stops the program.
//!
//! # The SME unit's slow state
//!
//! The SME unit runs SME2 kernels at full speed back to back. About 0.2 µs
//! of other work on the core between them puts it in a slow state for the
//! next ones (M4 Max P-core, 8 MiB in 64 KiB pieces: 0.152 ns/B back to back
//! and with 0.1 µs of scalar work between pieces, 0.179 at 0.25 µs,
//! 0.195-0.20 from 0.5 µs, about 2.5 µs a piece; the VM alike); streaming
//! sessions back to back cost nothing. The core's cycles per ns show the
//! state (3.93 fast, 3.20-3.30 slow: the difference is time the core waits
//! on the unit), which classifies a measurement better than comparing wall
//! times across runs. So the SME2 paths keep NEON and other work from
//! falling between their kernels: the flat walk, the padded groups, the
//! remainders' placement. Three placements put the unit there as well:
//! 32-byte chaining-value stores off a 32-byte boundary; a deep stack frame,
//! whose page probes land among the buffers the unit is about to use (so
//! the flat walk's scratch is the thread's, not the stack's); and buffers at
//! one address mod 4 KiB (so the scratch sits on a 4 KiB boundary, its
//! buffers at 0, 1 and 3 KiB). The unit writing lines the core has just
//! written costs too (a zeroed scratch output 46 ns a message against 38
//! written in place), so the kernels store only the lanes asked for, and
//! scratch stays uninitialised. Streaming mode also lowers a P-core's
//! clock: the same integer loop ran 13% longer inside it (3.93 cycles per
//! ns against 4.51).
//!
//! What SME2 buys over the NEON hybrids, nonstop on an M4 Max (probe/sme2-worth):
//! one thread 1.5-1.7x (hash of 64 KiB-64 MiB 0.25 -> 0.15-0.17 ns/B),
//! batches 1.9x (18.2 -> 9.4 ns a message), outboards 1.5x, a Hasher fed
//! 64 KiB pieces 1.2x; `hash_multithreaded` on every core level. One
//! thread's energy, the chip's power measured: about 690 pJ a byte at 6.2
//! GB/s against NEON's 1990 at 3.9. Batches lent after other work run about
//! a quarter slower than back to back (the SME unit's slow state, whatever
//! the work); cleaning the copied lines first, or larger tables a call,
//! did not help. Tried and
//! set aside: a padded SME2 group for a batch's remainder (80-290% slower
//! at 24 messages, every variant), and a lone 16 KiB group on the NEON
//! hybrids in place of SME2 (no change).

use crate::{BLOCK_LEN, CHUNK_LEN, CVWords, IncrementCounter, OUT_LEN};

/// Lanes per kernel group: the hardware's real SIMD width.
pub const GROUP: usize = 16;

/// The degree `Platform::SME2` reports. Every `hash_many` call carries up to
/// this many inputs, so the kernel hashes `DEGREE / GROUP` groups per entry
/// into streaming mode. Entering and leaving streaming mode costs about half
/// a microsecond on Apple M4, more than a whole group of parent compressions
/// (~170 ns); eight groups per call amortize it. Measured single-thread
/// `blake3::hash` on a 64 MiB input: degree 16 → 226 ps/B, 64 → 182, 128 → 167
/// (NEON: 424). 128 also keeps the walk's (`tree::wide`) stack arrays at 8 KiB.
pub const DEGREE: usize = 128;

/// Fewest one-block inputs left over after the SME2 groups that go to
/// one more, overlapping, SME2 group rather than the NEON kernels: from 13
/// (p9 + p4 and up), NEON runs past the SME unit's idle threshold. Below it
/// the NEON kernels are cheaper than a second streaming session (M4 Max,
/// probe/neon-cold job 129: 15 left over, overlap -19 to -28% on P-cores;
/// 1 to 12 left over, +2 to +69% where the SME unit stayed fast).
const OVERLAP_MIN: usize = 13;

/// Chunk-group and parent-group entry points share one signature.
type Kernel = unsafe extern "C" fn(*const u8, *const u32, u64, u32, *mut u8, u64) -> u64;

// Unsafe because this may only be called on CPUs that report SME2 with a
// 512-bit streaming vector length, and on which NEON is also available.
pub unsafe fn hash_many<const N: usize>(
    inputs: &[&[u8; N]],
    key: &CVWords,
    counter: u64,
    increment_counter: IncrementCounter,
    flags: u8,
    flags_start: u8,
    flags_end: u8,
    out: &mut [u8],
) {
    assert!(out.len() >= inputs.len() * OUT_LEN);

    let full_groups = inputs.len() / GROUP;
    // The chunk kernel takes flags, flags_start, and flags_end packed into
    // one word; the parent kernel applies flags | flags_start | flags_end to
    // its single block, which is what hash_many means for one-block inputs.
    let (kernel, packed_flags): (Option<Kernel>, u32) = if N == CHUNK_LEN && increment_counter.yes() {
        (Some(ffi::blake3_servil_sme2_hash16_chunks_512), flags as u32 | (flags_start as u32) << 8 | (flags_end as u32) << 16)
    } else if N == BLOCK_LEN && !increment_counter.yes() {
        (Some(ffi::blake3_servil_sme2_hash16_parents_512), (flags | flags_start | flags_end) as u32)
    } else {
        (None, 0)
    };

    let mut done = 0;
    // Separate messages of 2 to 16 whole blocks, every one at counter zero:
    // the message kernel, sixteen per group (the chunk kernel with a block
    // count and one counter for every lane).
    let blocks = N / BLOCK_LEN;
    if kernel.is_none() && N % BLOCK_LEN == 0 && (2..=16).contains(&blocks) && !increment_counter.yes() && full_groups > 0 {
        let lanes = unsafe {
            crate::ran_kernel(ffi::blake3_servil_sme2_hash16_messages_512 as *const ());
            ffi::blake3_servil_sme2_hash16_messages_512(
                inputs.as_ptr() as *const *const u8,
                key.as_ptr(),
                counter,
                flags as u64 | (flags_start as u64) << 8 | (flags_end as u64) << 16,
                out.as_mut_ptr(),
                full_groups as u64,
                blocks as u64,
            )
        };
        assert_eq!(lanes, 16, "SME2 streaming vector length changed under us");
        done = full_groups * GROUP;
    }
    if let (Some(kernel), true) = (kernel, full_groups > 0) {
        crate::ran_kernel(kernel as *const ());
        let count = full_groups * GROUP;
        let lanes = if N == BLOCK_LEN {
            // The parent kernel reads its sixteen 64-byte blocks from one
            // contiguous run. Every caller in this crate already stores
            // parent inputs that way; anyone else gets a copy.
            let base = inputs[0].as_ptr();
            let contiguous = inputs[..count].iter().enumerate().all(|(i, input)| input.as_ptr() == unsafe { base.add(i * BLOCK_LEN) });
            if contiguous {
                unsafe { kernel(base, key.as_ptr(), counter, packed_flags, out.as_mut_ptr(), full_groups as u64) }
            } else {
                // Gather up to GATHER groups per kernel call, so one entry
                // into streaming mode covers them all.
                const GATHER: usize = DEGREE / GROUP;
                let mut buf = [0u8; GATHER * GROUP * BLOCK_LEN];
                let mut group = 0;
                while group < full_groups {
                    let groups = (full_groups - group).min(GATHER);
                    for (i, input) in inputs[group * GROUP..][..groups * GROUP].iter().enumerate() {
                        buf[i * BLOCK_LEN..][..BLOCK_LEN].copy_from_slice(&input[..]);
                    }
                    let lanes =
                        unsafe { kernel(buf.as_ptr(), key.as_ptr(), counter, packed_flags, out[group * GROUP * OUT_LEN..].as_mut_ptr(), groups as u64) };
                    assert_eq!(lanes, 16, "SME2 streaming vector length changed under us");
                    group += groups;
                }
                16
            }
        } else {
            unsafe { kernel(inputs.as_ptr() as *const u8, key.as_ptr(), counter, packed_flags, out.as_mut_ptr(), full_groups as u64) }
        };
        assert_eq!(lanes, 16, "SME2 streaming vector length changed under us");
        done = count;
    }

    // One-block inputs after SME2 groups, with OVERLAP_MIN or more left over:
    // the last sixteen as one more group, overlapping the one before, and
    // only the remainder's values kept. The NEON kernels for that many run
    // past the idle time (about a quarter of a microsecond) after which the
    // SME unit drops to a state about 20% slower that lasts tens of
    // microseconds, so the next SME2 work, this call's or the next one's,
    // runs slow (the slow state, in this module's documentation).
    let rest = inputs.len() - done;
    if N == BLOCK_LEN && !increment_counter.yes() && done > 0 && rest >= OVERLAP_MIN {
        let mut last = [0u8; GROUP * OUT_LEN];
        unsafe {
            hash_many(&inputs[inputs.len() - GROUP..], key, counter, increment_counter, flags, flags_start, flags_end, &mut last);
        }
        out[done * OUT_LEN..inputs.len() * OUT_LEN].copy_from_slice(&last[(GROUP - rest) * OUT_LEN..]);
        return;
    }

    if done < inputs.len() {
        let rest_counter = if increment_counter.yes() { counter + done as u64 } else { counter };
        // The remainder is fewer than sixteen inputs: the NEON kernels
        // (integer + vector hybrids, see neon_hybrid.rs) are the fastest
        // path there.
        unsafe {
            crate::neon_hybrid::hash_many(&inputs[done..], key, rest_counter, increment_counter, flags, flags_start, flags_end, &mut out[done * OUT_LEN..]);
        }
    }
}

/// Messages in slots of N bytes (1 to 16 whole blocks), every one at
/// counter zero and hashed alone (flags_start on its first block,
/// flags_end on its last), its last block `last_len` bytes long (1 to 64;
/// the slot's bytes past it zero): `lanes` holds whole groups of sixteen,
/// of which the first `out.len() / OUT_LEN` are messages whose digests go
/// to `out` and the rest (fewer than sixteen, all in the last group) spare
/// lanes whose values the kernel computes and never stores. One kernel
/// call, so one entry into streaming mode. Unsafe because the CPU must
/// have SME2 with 512-bit streaming vectors.
pub unsafe fn hash_messages<const N: usize>(lanes: &[&[u8; N]], key: &CVWords, flags: u8, flags_start: u8, flags_end: u8, last_len: usize, out: &mut [u8]) {
    let blocks = N / BLOCK_LEN;
    assert!(N % BLOCK_LEN == 0 && (1..=16).contains(&blocks), "slots of 1 to 16 whole blocks");
    assert!((1..=BLOCK_LEN).contains(&last_len), "a last block of 1 to 64 bytes");
    let count = out.len() / OUT_LEN;
    assert!(
        out.len() % OUT_LEN == 0 && lanes.len() % GROUP == 0 && count <= lanes.len() && lanes.len() - count < GROUP,
        "whole groups of lanes, fewer than sixteen of them spare, all in the last group"
    );
    if count == 0 {
        return;
    }
    let stored = (count % GROUP) as u64;
    let lanes_returned = unsafe {
        crate::ran_kernel(ffi::blake3_servil_sme2_hash16_messages_512 as *const ());
        ffi::blake3_servil_sme2_hash16_messages_512(
            lanes.as_ptr() as *const *const u8,
            key.as_ptr(),
            0,
            flags as u64 | (flags_start as u64) << 8 | (flags_end as u64) << 16 | stored << 24 | ((last_len % BLOCK_LEN) as u64) << 40,
            out.as_mut_ptr(),
            (lanes.len() / GROUP) as u64,
            blocks as u64,
        )
    };
    assert_eq!(lanes_returned, 16, "SME2 streaming vector length changed under us");
}

#[cfg(feature = "std")]
/// Groups of sixteen whole chunks, each at its own first counter: group g
/// is `chunks[16 g..16 g + 16]`, chunk i of it at `counters[g] + i`, its
/// sixteen chaining values to `out[512 g..]`. One kernel call, so one
/// entry into streaming mode. Unsafe because the CPU must have SME2 with
/// 512-bit streaming vectors.
pub unsafe fn hash_groups_at(chunks: &[&[u8; CHUNK_LEN]], counters: &[u64], key: &CVWords, flags: u8, flags_start: u8, flags_end: u8, out: &mut [u8]) {
    assert!(chunks.len() == GROUP * counters.len() && out.len() == chunks.len() * OUT_LEN, "sixteen chunks and their values for each counter");
    if counters.is_empty() {
        return;
    }
    let lanes = unsafe {
        crate::ran_kernel(ffi::blake3_servil_sme2_hash16_chunks_at_512 as *const ());
        ffi::blake3_servil_sme2_hash16_chunks_at_512(
            chunks.as_ptr() as *const *const u8,
            key.as_ptr(),
            counters.as_ptr(),
            flags as u32 | (flags_start as u32) << 8 | (flags_end as u32) << 16,
            out.as_mut_ptr(),
            counters.len() as u64,
        )
    };
    assert_eq!(lanes, 16, "SME2 streaming vector length changed under us");
}

/// Chunks per message that [`hash_chunked_messages`] takes: 2 to 15. From
/// sixteen, one message fills an SME2 group by itself.
pub const MESSAGE_CHUNKS: core::ops::RangeInclusive<usize> = 2..=15;

/// Sixteen messages of `len` bytes each (2 to 15 chunks), hashed side by
/// side: `lanes[i]` points at message i (spare lanes repeat one), which
/// reaches `len` rounded up to whole blocks, its bytes past `len` zero;
/// the first `out.len() / OUT_LEN` lanes' digests go to `out`.
/// Chunk k of every lane goes to the message kernel at counter k (the
/// whole chunks in one call, the last chunk in a second); then each parent
/// level across the lanes, pairs gathered into sixteen contiguous blocks
/// per group, until the root, which takes ROOT. `key` and `flags` give
/// the mode (IV and 0 for plain hashing). Unsafe because the CPU
/// must have SME2 with 512-bit streaming vectors and every pointer must
/// reach `len` rounded up to whole blocks.
pub unsafe fn hash_chunked_messages(lanes: &[*const u8; GROUP], len: usize, key: &CVWords, flags: u8, out: &mut [u8]) {
    use crate::{CHUNK_END, CHUNK_START, PARENT, ROOT};
    let chunks = len.div_ceil(CHUNK_LEN);
    assert!(MESSAGE_CHUNKS.contains(&chunks), "messages of 2 to 15 chunks");
    let count = out.len() / OUT_LEN;
    assert!(out.len() % OUT_LEN == 0 && (1..=GROUP).contains(&count), "1 to 16 digests");
    let last_chunk = len - (chunks - 1) * CHUNK_LEN;
    let last_blocks = last_chunk.div_ceil(BLOCK_LEN);
    let last_len = last_chunk - (last_blocks - 1) * BLOCK_LEN;
    // Uninitialised scratch, written by the kernels before any read:
    // zeroing it cost more than the kernels' own stores (the SME unit
    // writing lines the core has just written).
    let mut scratch = core::mem::MaybeUninit::<ChunkedScratch>::uninit();
    let base = scratch.as_mut_ptr();
    // Sound: raw pointers into the scratch, no references to its bytes.
    let (cvs, pairs, table) = unsafe {
        (
            core::ptr::addr_of_mut!((*base).cvs) as *mut u8,
            core::ptr::addr_of_mut!((*base).pairs) as *mut u8,
            core::ptr::addr_of_mut!((*base).table) as *mut *const u8,
        )
    };
    // cvs holds node k of every lane at cvs + (16 k + lane) * 32, as the
    // kernels write sixteen lanes contiguous per call and group.
    let cv = |k: usize, lane: usize| unsafe { cvs.add((GROUP * k + lane) * OUT_LEN) };
    let chunk_flags = flags as u64 | (CHUNK_START as u64) << 8 | (CHUNK_END as u64) << 16;
    unsafe {
        for k in 0..chunks {
            for (i, lane) in lanes.iter().enumerate() {
                table.add(k * GROUP + i).write(lane.add(k * CHUNK_LEN));
            }
        }
        // The whole chunks, one group per chunk index, the counter stepping
        // by one from group to group; then the last chunk.
        crate::ran_kernel(ffi::blake3_servil_sme2_hash16_messages_512 as *const ());
        let n = ffi::blake3_servil_sme2_hash16_messages_512(table, key.as_ptr(), 0, chunk_flags | 1 << 32, cvs, (chunks - 1) as u64, 16);
        assert_eq!(n, 16, "SME2 streaming vector length changed under us");
        let last_flags = chunk_flags | ((last_len % BLOCK_LEN) as u64) << 40;
        crate::ran_kernel(ffi::blake3_servil_sme2_hash16_messages_512 as *const ());
        let n = ffi::blake3_servil_sme2_hash16_messages_512(
            table.add((chunks - 1) * GROUP),
            key.as_ptr(),
            (chunks - 1) as u64,
            last_flags,
            cv(chunks - 1, 0),
            1,
            last_blocks as u64,
        );
        assert_eq!(n, 16, "SME2 streaming vector length changed under us");
        // Parent levels: node j of the next level joins nodes 2j and 2j + 1
        // (gathered into one 64-byte block per lane, sixteen per group); an
        // odd last node carries up. Down to two nodes, then the root.
        let mut nodes = chunks;
        loop {
            let parents = nodes / 2;
            for j in 0..parents {
                for i in 0..GROUP {
                    let pair = pairs.add((GROUP * j + i) * BLOCK_LEN);
                    core::ptr::copy_nonoverlapping(cv(2 * j, i), pair, OUT_LEN);
                    core::ptr::copy_nonoverlapping(cv(2 * j + 1, i), pair.add(OUT_LEN), OUT_LEN);
                }
            }
            if nodes == 2 {
                break;
            }
            if nodes % 2 == 1 {
                core::ptr::copy_nonoverlapping(cv(nodes - 1, 0), cv(parents, 0), GROUP * OUT_LEN);
            }
            crate::ran_kernel(ffi::blake3_servil_sme2_hash16_parents_512 as *const ());
            let n = ffi::blake3_servil_sme2_hash16_parents_512(pairs, key.as_ptr(), 0, (flags | PARENT) as u32, cvs, parents as u64);
            assert_eq!(n, 16, "SME2 streaming vector length changed under us");
            nodes = parents + nodes % 2;
        }
        crate::ran_kernel(ffi::blake3_servil_sme2_hash16_parents_512 as *const ());
        let n = ffi::blake3_servil_sme2_hash16_parents_512(pairs, key.as_ptr(), 0, (flags | PARENT | ROOT) as u32, cvs, 1);
        assert_eq!(n, 16, "SME2 streaming vector length changed under us");
        core::ptr::copy_nonoverlapping(cvs, out.as_mut_ptr(), count * OUT_LEN);
    }
}

/// [`hash_chunked_messages`]'s scratch: the chaining values of up to 15
/// nodes per lane, the gathered pairs of up to 7 parents, and the chunk
/// pointer table, on 128-byte lines (Apple's).
#[repr(C, align(128))]
struct ChunkedScratch {
    cvs: [[[u8; OUT_LEN]; GROUP]; 15],
    pairs: [[[u8; BLOCK_LEN]; GROUP]; 7],
    table: [*const u8; GROUP * 15],
}

/// Whole subtrees from this many chunks to FLAT_MAX_CHUNKS, powers of two,
/// take the flat walk ([`compress_subtree_flat`]): 32 chunks are the
/// fewest whose first parent level fills a group of 16.
pub const FLAT_MIN_CHUNKS: usize = 32;
/// The fewest chunks, a power of two, that split exactly into groups of 18
/// (the integer lane's kernel) and 16: 256 = 8 x 18 + 7 x 16. Smaller
/// subtrees take groups of 16 alone. The split's arithmetic (8 lane groups
/// per 144 chunks) makes this boundary; the dispatch test checks it.
#[cfg(all(test, feature = "std"))]
pub const LANE_MIN_CHUNKS: usize = 256;
/// The most chunks the flat walk takes; its chaining values sit in 48 KiB
/// of stack. Larger subtrees are split by the tree walk above it.
pub const FLAT_MAX_CHUNKS: usize = 1024;

/// Whether [`compress_subtree_flat`] takes an input of `len` bytes.
#[inline]
pub fn flat_takes(len: usize) -> bool {
    len.is_power_of_two() && (FLAT_MIN_CHUNKS * CHUNK_LEN..=FLAT_MAX_CHUNKS * CHUNK_LEN).contains(&len)
}

/*
 * A whole subtree of 32 to 1024 chunks, bottom up, on SME2 alone: what
 * the walk (`tree::wide`) returns for it, its DEGREE chaining values
 * (each covering n / DEGREE chunks). The chunks go to the kernel with an
 * integer lane in groups of 18 (8 of them per 144 chunks), the rest to the
 * plain kernel in groups of 16, with nothing left over (n is a multiple of
 * 16); then each parent level from n / 2 down to DEGREE values goes to the
 * parent kernel, every level a multiple of 16. No NEON work falls between
 * SME2 kernels, so the SME unit never idles on our account (the slow
 * state, in this module's documentation). Out of line: its 56 KiB of stack arrays belong to
 * large inputs alone. Unsafe because the CPU must have SME2 with 512-bit
 * streaming vectors; `flat_takes(input.len())` must hold, and `out` must
 * have room for DEGREE values.
 */
#[inline(never)]
pub unsafe fn compress_subtree_flat(input: &[u8], ahead: usize, key: &CVWords, chunk_counter: u64, flags: u8, out: &mut [u8]) -> usize {
    unsafe { flat_walk(input, ahead, key, chunk_counter, flags, DEGREE, out) }
}

/// The flat walk down to the subtree's two children: what
/// `compress_subtree_to_parent_node` returns for it. The levels below
/// sixteen parents run as one padded SME2 group each (lanes past the
/// level's parents compress bytes nobody reads), so no NEON or scalar work
/// falls between the SME2 kernels: 0.25 µs of other work between them puts
/// the SME unit in its slow state for the next ones (the module's
/// documentation). Unsafe for the reasons of [`compress_subtree_flat`].
#[inline(never)]
pub unsafe fn compress_subtree_flat_to_parent(input: &[u8], ahead: usize, key: &CVWords, chunk_counter: u64, flags: u8) -> [u8; crate::BLOCK_LEN] {
    let mut out = [0u8; crate::BLOCK_LEN];
    unsafe { flat_walk(input, ahead, key, chunk_counter, flags, 2, &mut out) };
    out
}

/// The flat walk down to the subtree's own chaining value, for a subtree
/// that cannot be the root (input comes before it). Unsafe for the
/// reasons of [`compress_subtree_flat`].
#[inline(never)]
pub unsafe fn compress_subtree_flat_to_cv(input: &[u8], ahead: usize, key: &CVWords, chunk_counter: u64, flags: u8) -> crate::CVBytes {
    let mut out = [0u8; OUT_LEN];
    unsafe { flat_walk(input, ahead, key, chunk_counter, flags, 1, &mut out) };
    out
}

/// The flat walk until `keep` chaining values remain (DEGREE, 2, or 1),
/// which it copies into `out`. `ahead` bytes of the caller's input follow
/// `input` in memory: the walk first prefetches as many of them as it
/// hashes, into L2 (the next subtree's), so they arrive while it hashes.
/// The SME unit's loads leave DRAM-resident input to come one stall after
/// another: hash() of 128 MiB not in the caches, VM, 0.188 -> 0.162 ns/B;
/// the same in the caches +0.6% (1 MiB 0.153 -> 0.154). Bytes past the
/// caller's input are never prefetched (a lone 1 MiB from DRAM ran 10%
/// slower so).
#[inline(always)]
unsafe fn flat_walk(input: &[u8], ahead: usize, key: &CVWords, chunk_counter: u64, flags: u8, keep: usize, out: &mut [u8]) -> usize {
    assert!(flat_takes(input.len()), "a whole subtree of 32 to 1024 chunks");
    assert!(keep == DEGREE || keep == 2 || keep == 1, "the flat walk keeps DEGREE, 2, or 1 chaining values");
    assert!(out.len() >= keep * OUT_LEN, "room for the chaining values kept");
    assert!(keep < input.len() / CHUNK_LEN, "the flat walk keeps fewer chaining values than its subtree has chunks");
    let n = input.len() / CHUNK_LEN;
    let mut line = input.as_ptr() as usize + input.len();
    let end = line + ahead.min(input.len());
    while line < end {
        // Sound: a prefetch never faults, whatever the address.
        unsafe { core::arch::asm!("prfm pldl2keep, [{0}]", in(reg) line, options(nostack, readonly, preserves_flags)) };
        line += 128;
    }
    #[cfg(feature = "std")]
    if let Ok(base) = SCRATCH_BLOCK.try_with(|block| block.get() as *mut u8) {
        // Sound: this thread's block outlives the call, and the walk calls
        // nothing that re-enters it, so the block has one user at a time.
        return unsafe { walk_in(input, key, chunk_counter, flags, keep, out, n, base) };
    }
    // Without std, or in a thread's teardown after its block is gone.
    unsafe { walk_on_stack(input, key, chunk_counter, flags, keep, out, n) }
}

/// [`walk_in`] with the scratch on the stack, out of line so the usual
/// path keeps a small frame.
#[inline(never)]
unsafe fn walk_on_stack(input: &[u8], key: &CVWords, chunk_counter: u64, flags: u8, keep: usize, out: &mut [u8], n: usize) -> usize {
    let mut block = Scratch([core::mem::MaybeUninit::<u8>::uninit(); SCRATCH]);
    unsafe { walk_in(input, key, chunk_counter, flags, keep, out, n, block.0.as_mut_ptr() as *mut u8) }
}

/// The flat walk's scratch, on a 4 KiB boundary, so the residues mod 4 KiB
/// that [`scratch_layout`] gives its buffers are their addresses' residues.
/// On 128-byte lines alone, malloc chose the residues: VM, hash(32 KiB)
/// 0.18 ns/B with the block at a multiple of 1 KiB mod 4 KiB and 0.22 at
/// 0x680, 0x700, 0xe80, 0xf00, so a process's first spawned thread (its
/// block at 0xd00) ran slow, and its main thread by chance (September 26,
/// 2026).
#[repr(C, align(4096))]
struct Scratch([core::mem::MaybeUninit<u8>; SCRATCH]);

// Each thread's scratch for the flat walk, allocated on its first walk.
// Off the stack: a 64 KiB stack frame is probed with a store into each
// page on every call, and those stores, landing among the buffers the SME
// unit is about to use, put it in its slow state at some stack depths
// (M4 Max, hash(32 KiB) 0.178-0.245 ns/B by depth with the buffers on the
// stack, 0.177 at every depth off it; probe/stack-map, jobs 169-171).
#[cfg(feature = "std")]
std::thread_local! {
    static SCRATCH_BLOCK: Box<core::cell::UnsafeCell<Scratch>> =
        Box::new(core::cell::UnsafeCell::new(Scratch([core::mem::MaybeUninit::uninit(); SCRATCH])));
}

/// The flat walk in `block`, SCRATCH bytes on a 4 KiB boundary, which
/// holds the pointer table at its start and the chaining values where
/// [`scratch_layout`] puts them: 1 KiB and 3 KiB mod 4 KiB, so the three
/// buffers never share an address mod 4 KiB. The kernels' 32-byte stores
/// also want 32-byte boundaries (VM, up to 21% slower off them).
#[allow(clippy::too_many_arguments)]
#[inline(always)]
unsafe fn walk_in(input: &[u8], key: &CVWords, chunk_counter: u64, flags: u8, keep: usize, out: &mut [u8], n: usize, base: *mut u8) -> usize {
    let (cvs_at, half_at) = scratch_layout(n);
    let table = base as *mut *const u8;
    for i in 0..n {
        unsafe { table.add(i).write(input.as_ptr().add(i * CHUNK_LEN)) };
    }
    let table = table as *const *const u8;
    let (mut src, mut dst) = unsafe { (base.add(cvs_at), base.add(half_at)) };

    let chunk_flags = flags as u32 | (crate::CHUNK_START as u32) << 8 | (crate::CHUNK_END as u32) << 16;
    let lane_groups = 8 * (n / 144);
    let plain_groups = (n - HYBRID_GROUP * lane_groups) / GROUP;
    debug_assert_eq!(HYBRID_GROUP * lane_groups + GROUP * plain_groups, n);
    unsafe {
        if lane_groups > 0 {
            crate::ran_kernel(ffi::blake3_servil_sme2x2_hash_chunks_512 as *const ());
            let lanes = ffi::blake3_servil_sme2x2_hash_chunks_512(table, key.as_ptr(), chunk_counter, chunk_flags, src, lane_groups as u64);
            assert_eq!(lanes, 16, "SME2 streaming vector length changed under us");
        }
        let done = HYBRID_GROUP * lane_groups;
        crate::ran_kernel(ffi::blake3_servil_sme2_hash16_chunks_512 as *const ());
        let lanes = ffi::blake3_servil_sme2_hash16_chunks_512(
            table.add(done) as *const u8,
            key.as_ptr(),
            chunk_counter + done as u64,
            chunk_flags,
            src.add(done * OUT_LEN),
            plain_groups as u64,
        );
        assert_eq!(lanes, 16, "SME2 streaming vector length changed under us");
        // Parent levels: pairs of adjacent chaining values, contiguous, from
        // `src` into `dst`, until `keep` values remain. A level of fewer than
        // sixteen parents reads 32 values from `src` and writes 16 into
        // `dst`, within the room scratch_layout leaves.
        let mut count = n;
        while count > keep {
            let parents = count / 2;
            let groups = parents.div_ceil(GROUP);
            crate::ran_kernel(ffi::blake3_servil_sme2_hash16_parents_512 as *const ());
            let lanes = ffi::blake3_servil_sme2_hash16_parents_512(src, key.as_ptr(), 0, (flags | crate::PARENT) as u32, dst, groups as u64);
            assert_eq!(lanes, 16, "SME2 streaming vector length changed under us");
            count = parents;
            core::mem::swap(&mut src, &mut dst);
        }
        core::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), keep * OUT_LEN);
    }
    keep
}

/// Bytes of the flat walk's scratch: the pointer table, the chaining
/// values, and the level above them for FLAT_MAX_CHUNKS, as
/// [`scratch_layout`] places them.
const SCRATCH: usize = 64 * 1024;

/// Where the flat walk of `n` chunks keeps its chaining values and the
/// level above them, in bytes from the scratch's start (the pointer table
/// sits at 0): at 1 KiB and 3 KiB mod 4 KiB, past the table and the
/// values before them, with room for a padded group of 16 parents' inputs
/// (32 values) at either.
fn scratch_layout(n: usize) -> (usize, usize) {
    let cvs = (8 * n).next_multiple_of(4096) + 1024;
    let half = cvs + (32 * n.max(32)).saturating_sub(2048).next_multiple_of(4096) + 2048;
    debug_assert!(half + 16 * n.max(64) <= SCRATCH, "the flat walk's scratch holds its buffers");
    (cvs, half)
}

/// Chunks per group of the kernel with an integer lane: sixteen on SME2,
/// two on the integer units (c/blake3_sme2_hybrid_aarch64.S).
pub const HYBRID_GROUP: usize = 18;

#[cfg(feature = "std")]
/// Prefetch the code of c/blake3_sme2_aarch64.S (its chunk, message, and
/// parent kernels, 11 KB), which a call of 16 chunks to 64 KiB runs.
pub fn prefetch_code() {
    crate::platform::prefetch_code((ffi::blake3_servil_sme2_hash16_chunks_512 as *const () as usize, &raw const ffi::blake3_servil_sme2_text_end as usize));
}

/// Fill `out`'s whole groups of sixteen 64-byte blocks with extended
/// output: block i the compression of `block` under `cv` at `counter + i`
/// with `flags` (ROOT among them) and `block_len`, both halves of the
/// state. Returns the bytes filled (a multiple of 1024; the rest is the
/// caller's). Unsafe because the CPU must have SME2 with 512-bit streaming
/// vectors (the platform's detect()).
pub unsafe fn xof_many(cv: &CVWords, block: &[u8; crate::BLOCK_LEN], block_len: u8, counter: u64, flags: u8, out: &mut [u8]) -> usize {
    let groups = out.len() / (GROUP * crate::BLOCK_LEN);
    if groups == 0 {
        return 0;
    }
    // Sound: `out` holds `groups` groups of sixteen blocks, and the caller
    // found SME2.
    crate::ran_kernel(ffi::blake3_servil_sme2_xof16_512 as *const ());
    let lanes = unsafe {
        ffi::blake3_servil_sme2_xof16_512(cv.as_ptr(), block.as_ptr(), counter, flags as u64 | (block_len as u64) << 8, out.as_mut_ptr(), groups as u64)
    };
    assert_eq!(lanes, 16, "SME2 streaming vector length changed under us");
    groups * GROUP * crate::BLOCK_LEN
}

/// Every SME2 assembly entry point, named (for the self-test's test).
#[cfg(all(test, feature = "std"))]
pub(crate) fn entries() -> std::vec::Vec<(&'static str, *const ())> {
    std::vec![
        ("hash16_chunks", ffi::blake3_servil_sme2_hash16_chunks_512 as *const ()),
        ("hash16_chunks_at", ffi::blake3_servil_sme2_hash16_chunks_at_512 as *const ()),
        ("hash16_messages", ffi::blake3_servil_sme2_hash16_messages_512 as *const ()),
        ("hash16_parents", ffi::blake3_servil_sme2_hash16_parents_512 as *const ()),
        ("xof16", ffi::blake3_servil_sme2_xof16_512 as *const ()),
        ("sme2x2_chunks", ffi::blake3_servil_sme2x2_hash_chunks_512 as *const ()),
    ]
}

pub mod ffi {
    unsafe extern "C" {
        #[cfg(feature = "std")]
        /// `blake3_servil_sme2_hash16_chunks_512` with group g's first counter
        /// `counters[g]`: separate messages' groups of sixteen chunks.
        pub fn blake3_servil_sme2_hash16_chunks_at_512(
            inputs: *const *const u8,
            key: *const u32,
            counters: *const u64,
            flags: u32,
            out: *mut u8,
            groups: u64,
        ) -> u64;
        /// Sixteen extended-output blocks per group (see xof_many).
        pub fn blake3_servil_sme2_xof16_512(cv: *const u32, block: *const u8, counter: u64, flags_len: u64, out: *mut u8, groups: u64) -> u64;
        /// Where c/blake3_sme2_aarch64.S's code ends.
        #[cfg(feature = "std")]
        pub static blake3_servil_sme2_text_end: u8;
        /// Eighteen whole chunks per group, sixteen on the SME2 lanes and
        /// two on the integer units beside them (generated by
        /// tools/gen_sme2_hybrid.py). `inputs` is a table of `18 * groups`
        /// chunk pointers; otherwise as `blake3_servil_sme2_hash16_chunks_512`.
        pub fn blake3_servil_sme2x2_hash_chunks_512(inputs: *const *const u8, key: *const u32, counter: u64, flags: u32, out: *mut u8, groups: u64) -> u64;

        /// Sixteen separate messages of `blocks` whole blocks (1 to 16) per
        /// group, every one at `counter`. `inputs` is a table of
        /// `16 * groups` message pointers; `flags` packs as for
        /// `blake3_servil_sme2_hash16_chunks_512`, flags_end on each message's last
        /// block. Writes `16 * groups` 32-byte chaining values to `out`,
        /// or, when bits 24 to 31 of `flags` hold k (1 to 15), the last
        /// group's first k alone. Bits 32 to 39 of `flags` step the counter
        /// from one group to the next (0: every group at `counter`); bits
        /// 40 to 47 give the last block's length, 1 to 63 (0: 64), its
        /// bytes past that length zero.
        /// Returns the streaming vector length in 32-bit lanes.
        pub fn blake3_servil_sme2_hash16_messages_512(
            inputs: *const *const u8,
            key: *const u32,
            counter: u64,
            flags: u64,
            out: *mut u8,
            groups: u64,
            blocks: u64,
        ) -> u64;

        /// Sixteen whole 1024-byte chunks per group. `inputs` is a table of
        /// `16 * groups` chunk pointers; `counter` applies to the first chunk
        /// and increments per chunk; `flags` packs `flags | flags_start << 8
        /// | flags_end << 16`. Writes `16 * groups` 32-byte chaining values
        /// to `out`. Returns the streaming vector length in 32-bit lanes.
        pub fn blake3_servil_sme2_hash16_chunks_512(inputs: *const u8, key: *const u32, counter: u64, flags: u32, out: *mut u8, groups: u64) -> u64;

        /// Sixteen parent compressions per group. `pairs` points at
        /// `16 * groups` adjacent 64-byte parent blocks; `counter` is used
        /// unchanged for every block; `flags` are the parent flags. Writes
        /// `16 * groups` 32-byte chaining values to `out`. Returns the
        /// streaming vector length in 32-bit lanes.
        pub fn blake3_servil_sme2_hash16_parents_512(pairs: *const u8, key: *const u32, counter: u64, flags: u32, out: *mut u8, groups: u64) -> u64;
    }
}
