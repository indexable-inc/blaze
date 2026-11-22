//! The wide tree: an input of two or more chunks hashed as the largest power-of-two subtrees,
//! straight from the caller's bytes, with the chunks of a subtree side by side in SIMD lanes and
//! then each level's parents side by side too, up to two chaining values (the BLAKE3 paper,
//! section 5.1.2 "Multithreading", and the official crate's `compress_subtree_wide`, whose
//! shape this follows in our own form). The two are the children of the subtree's top node,
//! which is left uncompressed because it may be the root: [`subtree_to_parent`] returns them
//! for the hasher's stack or for the one-shot root.
//!
//! Lanes a batch takes: the narrowest power of two that holds it, at most the engine's
//! [`Degree`] (three chunks at four lanes, the fourth lane hashing a copy of the first and its
//! result dropped); one alone is compressed by the engine's [`crate::Single`]. Parents go the
//! same way, a pair a lane. Recursion depth is the tree's height over the degree, at most 54
//! levels for a 2^64 byte input, each frame two degrees of chaining values (1 KiB at sixteen
//! lanes).

use core::simd::Simd;

use blaze_int::{Cast, Int};

use crate::engine::{Degree, Engine, Isa, Pair};
use crate::{
    BLOCK_LEN, CHUNK_END, CHUNK_LEN, CHUNK_START, ChunkState, MOST_LANES, PARENT, first_8, lanes, pair, single,
    single_in_place,
};

/// The most chaining values a frame holds: two subtrees' worth at the widest degree.
const MOST_CVS: usize = 2 * MOST_LANES;

/// The length of the left subtree of an input of `len` bytes (more than a chunk): the largest
/// power-of-two number of whole chunks that leaves at least one byte to its right (the BLAKE3
/// paper, section 2.1: the left subtree is complete and as large as it can be).
pub(crate) fn left_len(len: usize) -> usize {
    debug_assert!(len > CHUNK_LEN, "a subtree of more than one chunk");
    let whole = (len - 1) / CHUNK_LEN;
    largest_power_of_two_at_most(whole) * CHUNK_LEN
}

/// The largest power of two at most `n` (at least 1).
pub(crate) fn largest_power_of_two_at_most(n: usize) -> usize {
    (n / 2 + 1).next_power_of_two()
}

/// The two children of the top node of the subtree `input` (more than a chunk), its chunks
/// numbered from `counter`: every chunk and every parent below them compressed, side by side
/// as the engine says.
pub(crate) fn subtree_to_parent(e: Engine, key: &[u32; 8], flags: u32, input: &[u8], counter: u64) -> [[u32; 8]; 2] {
    match e.degree {
        Degree::One => to_parent::<1>(e, key, flags, input, counter),
        Degree::Two => to_parent::<2>(e, key, flags, input, counter),
        Degree::Four => to_parent::<4>(e, key, flags, input, counter),
        Degree::Eight => to_parent::<8>(e, key, flags, input, counter),
        Degree::Sixteen => to_parent::<16>(e, key, flags, input, counter),
    }
}

fn to_parent<const N: usize>(e: Engine, key: &[u32; 8], flags: u32, input: &[u8], counter: u64) -> [[u32; 8]; 2] {
    assert!(input.len() > CHUNK_LEN, "a subtree of {} bytes, not more than a chunk", input.len());
    let mut cvs = [[0; 8]; MOST_CVS];
    let mut count = subtree::<N>(e, key, flags, input, counter, &mut cvs);
    debug_assert!(count >= 2, "a subtree of more than a chunk gives two chaining values or more");
    // More than two: whole levels of parents side by side until two are left.
    while count > 2 {
        let mut next = [[0; 8]; MOST_CVS];
        count = parents::<N>(e, key, flags, &cvs[..count], &mut next);
        cvs[..count].copy_from_slice(&next[..count]);
    }
    [cvs[0], cvs[1]]
}

/// The chaining values of `input`'s subtree, its chunks numbered from `counter`, written to
/// `out` (room for `max(N, 2)`): `N` of them where the input has `N` chunks or more (fewer for
/// a short one), and at least two where it has more than one chunk, so the subtree's top node
/// is never compressed here.
fn subtree<const N: usize>(e: Engine, key: &[u32; 8], flags: u32, input: &[u8], counter: u64, out: &mut [[u32; 8]]) -> usize {
    if input.len() <= N * CHUNK_LEN {
        return chunks::<N>(e, key, flags, input, counter, out);
    }
    let split = left_len(input.len());
    let (left, right) = input.split_at(split);
    let right_counter = counter + (split / CHUNK_LEN).widen_to::<u64>();
    let mut children = [[0; 8]; MOST_CVS];
    // One a side at one lane and the leaves (a left of one chunk), so two come back up.
    let width = if left.len() == CHUNK_LEN { 1 } else { N.max(2) };
    let (left_out, right_out) = children.split_at_mut(width);
    let left_count = subtree::<N>(e, key, flags, left, counter, left_out);
    let right_count = subtree::<N>(e, key, flags, right, right_counter, right_out);
    debug_assert_eq!(left_count, width, "a complete left subtree fills its width");
    if left_count == 1 {
        out[..2].copy_from_slice(&children[..2]);
        return 2;
    }
    parents::<N>(e, key, flags, &children[..left_count + right_count], out)
}

/// The chaining values of `input`'s chunks (one to `N`, the last maybe partial), numbered from
/// `counter`, into `out`: the whole ones side by side, a partial last one alone. Never the
/// root: the caller has more than one chunk in all.
fn chunks<const N: usize>(e: Engine, key: &[u32; 8], flags: u32, input: &[u8], counter: u64, out: &mut [[u32; 8]]) -> usize {
    debug_assert!(!input.is_empty(), "no empty chunk below the root");
    let (whole, rest) = input.as_chunks::<CHUNK_LEN>();
    side_by_side_chunks::<N>(e, key, flags, whole, counter, &mut out[..whole.len()]);
    if rest.is_empty() {
        return whole.len();
    }
    let mut chunk = ChunkState::new(key, counter + whole.len().widen_to::<u64>(), flags);
    chunk.update(e, rest);
    out[whole.len()] = chunk.output(e).chaining_value();
    whole.len() + 1
}

/// The chaining values of whole chunks numbered from `counter`, into `out` (one a chunk).
pub(crate) fn side_by_side_chunks<const N: usize>(
    e: Engine,
    key: &[u32; 8],
    flags: u32,
    whole: &[[u8; CHUNK_LEN]],
    counter: u64,
    out: &mut [[u32; 8]],
) {
    assert_eq!(whole.len(), out.len(), "a chaining value for each chunk");
    assert!(whole.len() <= N.max(1), "{} chunks side by side at {N} lanes", whole.len());
    if N == 1 || whole.len() <= 1 {
        for (at, (chunk, slot)) in whole.iter().zip(out.iter_mut()).enumerate() {
            *slot = chunk_cv(e, key, flags, chunk, counter + at.widen_to::<u64>());
        }
        return;
    }
    match whole.len().next_power_of_two().min(N) {
        2 => match e.pair {
            Pair::Lanes => chunks_lanes::<2>(e.isa, key, flags, whole, counter, out),
            Pair::Interleaved => chunks_pair(key, flags, whole, counter, out),
        },
        4 => chunks_lanes::<4>(e.isa, key, flags, whole, counter, out),
        8 => chunks_lanes::<8>(e.isa, key, flags, whole, counter, out),
        16 => chunks_lanes::<16>(e.isa, key, flags, whole, counter, out),
        lanes => panic!("blaze_blake3::tree: {lanes} lanes for {} chunks at degree {N}", whole.len()),
    }
}

/// One whole chunk's chaining value, its sixteen blocks compressed where they lie by
/// the engine's [`crate::Single`].
pub(crate) fn chunk_cv(e: Engine, key: &[u32; 8], flags: u32, chunk: &[u8; CHUNK_LEN], counter: u64) -> [u32; 8] {
    let (blocks, _) = chunk.as_chunks::<BLOCK_LEN>();
    let mut cv = *key;
    for (at, block) in blocks.iter().enumerate() {
        let mut block_flags = flags;
        if at == 0 {
            block_flags |= CHUNK_START;
        }
        if at == blocks.len() - 1 {
            block_flags |= CHUNK_END;
        }
        single_in_place(e, &mut cv, block, counter, block_flags);
    }
    cv
}

/// The parents of `children` taken in pairs, side by side, into `out`; an odd last child is
/// carried up as it is. Gives how many were written.
fn parents<const N: usize>(e: Engine, key: &[u32; 8], flags: u32, children: &[[u32; 8]], out: &mut [[u32; 8]]) -> usize {
    debug_assert!(children.len() >= 2, "a parent needs two children");
    let (pairs, odd) = children.as_chunks::<2>();
    let count = pairs.len();
    // At one lane a frame holds two children a side (the leaves give two), so two pairs.
    assert!(count <= N.max(2), "{count} parents side by side at {N} lanes");
    if N == 1 || count == 1 {
        let full = const { Cast::<usize, u32>::narrow_or_stop(BLOCK_LEN) };
        for (pair, slot) in pairs.iter().zip(out.iter_mut()) {
            *slot = first_8(&single(e, key, &parent_block(pair), 0, full, flags | PARENT));
        }
    } else {
        match count.next_power_of_two().min(N) {
            2 => match e.pair {
                Pair::Lanes => parents_lanes::<2>(e.isa, key, flags, pairs, &mut out[..count]),
                Pair::Interleaved => parents_pair(key, flags, pairs, &mut out[..count]),
            },
            4 => parents_lanes::<4>(e.isa, key, flags, pairs, &mut out[..count]),
            8 => parents_lanes::<8>(e.isa, key, flags, pairs, &mut out[..count]),
            16 => parents_lanes::<16>(e.isa, key, flags, pairs, &mut out[..count]),
            lanes => panic!("blaze_blake3::tree: {lanes} lanes for {count} parents at degree {N}"),
        }
    }
    if let [last] = odd {
        out[count] = *last;
        return count + 1;
    }
    count
}

/// A parent's block: its left child's eight words, then its right child's.
#[inline(always)]
pub(crate) fn parent_block([left, right]: &[[u32; 8]; 2]) -> [u32; 16] {
    core::array::from_fn(|word| if word < 8 { left[word] } else { right[word - 8] })
}

/// [`chunks_of`] at `L` lanes on the instructions `isa` proves.
fn chunks_lanes<const L: usize>(isa: Isa, key: &[u32; 8], flags: u32, whole: &[[u8; CHUNK_LEN]], counter: u64, out: &mut [[u32; 8]]) {
    #[cfg(target_arch = "x86_64")]
    {
        crate::x86::chunks::<L>(isa, key, flags, whole, counter, out)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _baseline: Isa = isa;
        chunks_of::<L>(key, flags, whole, counter, out)
    }
}

/// [`parents_of`] at `L` lanes on the instructions `isa` proves.
fn parents_lanes<const L: usize>(isa: Isa, key: &[u32; 8], flags: u32, pairs: &[[[u32; 8]; 2]], out: &mut [[u32; 8]]) {
    #[cfg(target_arch = "x86_64")]
    {
        crate::x86::parents::<L>(isa, key, flags, pairs, out)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _baseline: Isa = isa;
        parents_of::<L>(key, flags, pairs, out)
    }
}

/// The chaining values of one to `L` whole chunks at `L` lanes, a chunk a lane: a lane past
/// the chunks hashes a copy of the first and nothing reads its result. Always inlined, so it is
/// vectorized for the features of the function it is inlined into.
#[inline(always)]
pub(crate) fn chunks_of<const L: usize>(key: &[u32; 8], flags: u32, whole: &[[u8; CHUNK_LEN]], counter: u64, out: &mut [[u32; 8]]) {
    let real = whole.len();
    assert!((1..=L).contains(&real) && out.len() == real, "{real} chunks at {L} lanes into {} slots", out.len());
    let full = Simd::splat(const { Cast::<usize, u32>::narrow_or_stop(BLOCK_LEN) });
    let chunk = move |lane: usize| &whole[if lane < real { lane } else { 0 }];
    // The chaining value, counters and block length as vectors for the whole chunk: nothing
    // goes through memory between blocks (`lanes::compress_in_registers`).
    let mut cv: [Simd<u32, L>; 8] = key.map(Simd::splat);
    // Each lane's counter, split into its low and high words: dropping the other half is the
    // point. A lane past the chunks counts on; its result is dropped.
    let counter_low = Simd::from_array(core::array::from_fn(|lane| (counter + lane.widen_to::<u64>()).truncate_to::<u32>()));
    let counter_high =
        Simd::from_array(core::array::from_fn(|lane| ((counter + lane.widen_to::<u64>()) >> 32).truncate_to::<u32>()));
    let blocks = CHUNK_LEN / BLOCK_LEN;
    for block in 0..blocks {
        let message = lanes::message_columns::<L>(&core::array::from_fn(|lane| {
            let (all, _) = chunk(lane).as_chunks::<BLOCK_LEN>();
            &all[block]
        }));
        let mut block_flags = flags;
        if block == 0 {
            block_flags |= CHUNK_START;
        }
        if block == blocks - 1 {
            block_flags |= CHUNK_END;
        }
        cv = lanes::compress_in_registers(cv, &message, counter_low, counter_high, full, Simd::splat(block_flags));
    }
    let rows = lanes::rows_of(&cv.map(Simd::to_array));
    out.copy_from_slice(&rows[..real]);
}

/// The chaining values of two whole chunks numbered from `counter`, by [`pair::compress`]: the
/// two chunks' blocks compressed step for step in general registers ([`Pair::Interleaved`]).
fn chunks_pair(key: &[u32; 8], flags: u32, whole: &[[u8; CHUNK_LEN]], counter: u64, out: &mut [[u32; 8]]) {
    let [left, right] = whole else {
        panic!("blaze_blake3::tree: {} chunks as a pair", whole.len());
    };
    assert_eq!(out.len(), 2, "a chaining value for each of the pair");
    let full = const { Cast::<usize, u32>::narrow_or_stop(BLOCK_LEN) };
    let (left, _) = left.as_chunks::<BLOCK_LEN>();
    let (right, _) = right.as_chunks::<BLOCK_LEN>();
    let mut cv = [*key, *key];
    for (at, (l, r)) in left.iter().zip(right).enumerate() {
        let mut block_flags = flags;
        if at == 0 {
            block_flags |= CHUNK_START;
        }
        if at == left.len() - 1 {
            block_flags |= CHUNK_END;
        }
        let message = [lanes::block_words(l), lanes::block_words(r)];
        cv = pair::compress([&cv[0], &cv[1]], [&message[0], &message[1]], [counter, counter + 1], full, block_flags);
    }
    out.copy_from_slice(&cv);
}

/// The chaining values of two parents, by [`pair::compress`] ([`Pair::Interleaved`]).
fn parents_pair(key: &[u32; 8], flags: u32, pairs: &[[[u32; 8]; 2]], out: &mut [[u32; 8]]) {
    let [left, right] = pairs else {
        panic!("blaze_blake3::tree: {} parents as a pair", pairs.len());
    };
    assert_eq!(out.len(), 2, "a chaining value for each of the pair");
    let full = const { Cast::<usize, u32>::narrow_or_stop(BLOCK_LEN) };
    let message = [parent_block(left), parent_block(right)];
    // Parents always count 0 (the BLAKE3 paper, section 2.5).
    let cv = pair::compress([key, key], [&message[0], &message[1]], [0, 0], full, flags | PARENT);
    out.copy_from_slice(&cv);
}

/// The chaining values of one to `L` parents at `L` lanes, a pair of children a lane: a lane
/// past the parents compresses a copy of the first and nothing reads its result. Always
/// inlined, as [`chunks_of`].
#[inline(always)]
pub(crate) fn parents_of<const L: usize>(key: &[u32; 8], flags: u32, pairs: &[[[u32; 8]; 2]], out: &mut [[u32; 8]]) {
    let real = pairs.len();
    assert!((1..=L).contains(&real) && out.len() == real, "{real} parents at {L} lanes into {} slots", out.len());
    let full = const { Cast::<usize, u32>::narrow_or_stop(BLOCK_LEN) };
    let rows: [[u32; 16]; L] = core::array::from_fn(|lane| parent_block(&pairs[if lane < real { lane } else { 0 }]));
    let message = lanes::columns(&rows).map(Simd::from_array);
    // Parents always count 0 (the BLAKE3 paper, section 2.5). Splats, not `[x; L]` arrays,
    // which LLVM built by a `memset_pattern16` call (`lanes::compress_in_registers`).
    let zero = Simd::splat(0);
    let cv = lanes::compress_in_registers(
        key.map(Simd::splat),
        &message,
        zero,
        zero,
        Simd::splat(full),
        Simd::splat(flags | PARENT),
    );
    let rows = lanes::rows_of(&cv.map(Simd::to_array));
    out.copy_from_slice(&rows[..real]);
}
