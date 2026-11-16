//! BLAKE3, written here (the reference algorithm of the BLAKE3 paper and its reference
//! implementation, github.com/BLAKE3-team/BLAKE3, `reference_impl`): every mode blaze uses
//! (the plain hash, the keyed hash, key derivation by context string and by a context's key)
//! and the extended output, bit for bit the function official BLAKE3 1.x computes, since every
//! content ID, store key and record blaze has written is BLAKE3 of its content.
//!
//! No dependencies, so the bottom of the tree (`blaze-ids`, `blaze-id`) stands on nothing from
//! outside the workspace (docs/v2/plan/design/bottom.md, "Also decided on 2026-11-15").
//!
//! How it is fast without assembly of its own: a hasher fed more than a batch of whole chunks
//! hashes the batch's chunks side by side, a SIMD lane each ([`lanes::compress`], safe
//! `core::simd` code): 16 chunks in AVX-512 registers where the CPU has them, 8 in AVX2 ones,
//! and 8 at the target's baseline elsewhere (two NEON registers a word on AArch64). Many
//! independent short inputs go the same way through [`hash_many`] (and [`keyed_hash_many`],
//! [`derive_key_many`]), an input a lane. Parent nodes, the last chunk and one input of one
//! chunk are compressed one block at a time by the scalar [`compress`], where they lie, with no
//! copy. Which path runs is dispatch on the CPU, not a fallback: every path computes the same
//! bytes, held to the official test vectors, to the `blake3` crate at every length to 4,097
//! bytes and every alignment, and lane by lane to the scalar compression (`tests/`). The CPU's
//! features are [`cpu`]'s, found on `core` alone: the crate is `no_std` and allocates nothing.
//!
//! # Cost, and the floor (rule 67)
//!
//! `examples/cost.rs`, 2026-11-15, the owner's M5 Max (one P core at 4.25 GHz, measured by a
//! chain of dependent adds), best of seven runs, hot in cache:
//!
//! | input | before | now | floor | now / floor |
//! |---|---|---|---|---|
//! | 64 B, one call | 63.2 ns | 46.6 ns | 46 ns | 1.01 |
//! | 1 KiB, one call | 1,010 ns | 784 ns | 738 ns | 1.06 |
//! | 16 MiB, one call | 756 MB/s | 2,958 MB/s | 3,760 MB/s | 1.27 |
//! | 64 B each, `hash_many` | 14.6 M/s | 41.6 M/s | 62.5 M/s | 1.50 |
//! | 1 KiB each, `hash_many` | 0.93 M/s | 3.10 M/s | 3.91 M/s | 1.26 |
//!
//! The floor of one input is the latency of its chain of compressions: each block's seven
//! rounds are fourteen half-rounds of fourteen dependent one-cycle operations, 196 cycles, 46
//! ns a block, and a chunk's blocks run one after another. Only independent inputs (lanes) beat
//! it. The floor of lanes is the compression alone at 8 lanes with nothing around it, 16.0 ns a
//! block a lane (blaze-int-1112-notes/nightly-probes/blake3_lanes.rs), plus one parent block a
//! chunk for a long input. What `hash_many` spends above it is gathering each lane's block,
//! the transpose into columns and the lanes' bookkeeping.

#![no_std]
#![feature(portable_simd)]
#![deny(unsafe_code)]

pub mod cpu;
pub mod lanes;

use blaze_int::{Cast, Int};

/// Bytes of a hash, and of a key.
pub const OUT_LEN: usize = 32;
/// Bytes of a block, the unit of one compression.
pub const BLOCK_LEN: usize = 64;
/// Bytes of a chunk, a leaf of the hash tree.
pub const CHUNK_LEN: usize = 1024;

/// The block flags (the BLAKE3 paper, table 3).
pub const CHUNK_START: u32 = 1 << 0;
pub const CHUNK_END: u32 = 1 << 1;
pub const PARENT: u32 = 1 << 2;
pub const ROOT: u32 = 1 << 3;
pub const KEYED_HASH: u32 = 1 << 4;
pub const DERIVE_KEY_CONTEXT: u32 = 1 << 5;
pub const DERIVE_KEY_MATERIAL: u32 = 1 << 6;

/// BLAKE3's IV: SHA-256's.
pub const IV: [u32; 8] = [
    0x6A09_E667,
    0xBB67_AE85,
    0x3C6E_F372,
    0xA54F_F53A,
    0x510E_527F,
    0x9B05_688C,
    0x1F83_D9AB,
    0x5BE0_CD19,
];

/// The message permutation applied between rounds: word `i` of the next round's message is
/// word `SCHEDULE[i]` of this round's.
pub const SCHEDULE: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];

/// Which word of the block each of a round's sixteen message slots reads: [`SCHEDULE`] composed
/// with itself once a round, so no round moves the message.
pub(crate) const MESSAGE: [[usize; 16]; 7] = {
    let mut table = [[0; 16]; 7];
    let mut word = 0;
    while word < 16 {
        table[0][word] = word;
        word += 1;
    }
    let mut round = 1;
    while round < 7 {
        let mut word = 0;
        while word < 16 {
            table[round][word] = table[round - 1][SCHEDULE[word]];
            word += 1;
        }
        round += 1;
    }
    table
};

/// The most chaining values a hasher holds: one per level of a tree of 2^54 chunks, 2^64 bytes.
const MAX_DEPTH: usize = 54;

/// The most chunks hashed side by side (AVX-512's 16 lanes).
const MOST_LANES: usize = 16;

/// The hash of `input`, BLAKE3's plain mode.
pub fn hash(input: &[u8]) -> [u8; OUT_LEN] {
    one_shot(&IV, 0, input)
}

/// [`hash`] of each of `inputs`, into the same place of `out`: inputs of at most a chunk side
/// by side, one a SIMD lane (see [`many`]); a longer one by [`hash`] where it comes.
pub fn hash_many(inputs: &[&[u8]], out: &mut [[u8; OUT_LEN]]) {
    many(&IV, 0, inputs, out);
}

/// [`keyed_hash`] under `key` of each of `inputs`, into the same place of `out`, as
/// [`hash_many`] runs them.
pub fn keyed_hash_many(key: &[u8; OUT_LEN], inputs: &[&[u8]], out: &mut [[u8; OUT_LEN]]) {
    many(&words_of(key), KEYED_HASH, inputs, out);
}

/// [`derive_key_with`] under the context key `key` of each of `inputs`, into the same place of
/// `out`, as [`hash_many`] runs them.
pub fn derive_key_many(key: &[u8; OUT_LEN], inputs: &[&[u8]], out: &mut [[u8; OUT_LEN]]) {
    many(&words_of(key), DERIVE_KEY_MATERIAL, inputs, out);
}

/// The hash of `input` under `key`, BLAKE3's keyed mode.
pub fn keyed_hash(key: &[u8; OUT_LEN], input: &[u8]) -> [u8; OUT_LEN] {
    one_shot(&words_of(key), KEYED_HASH, input)
}

/// The key `material` derives under `context`, BLAKE3's key derivation mode.
pub fn derive_key(context: &str, material: &[u8]) -> [u8; OUT_LEN] {
    derive_key_with(&context_key(context), material)
}

/// `context`'s key: the hash of the context string in the context mode, the key every input
/// under that context is hashed with (official BLAKE3's `hazmat::hash_derive_key_context`).
pub fn context_key(context: &str) -> [u8; OUT_LEN] {
    one_shot(&IV, DERIVE_KEY_CONTEXT, context.as_bytes())
}

/// The key `material` derives under the context whose [`context_key`] is `key`: what
/// [`derive_key`] computes after the context's key, with the key already made.
pub fn derive_key_with(key: &[u8; OUT_LEN], material: &[u8]) -> [u8; OUT_LEN] {
    one_shot(&words_of(key), DERIVE_KEY_MATERIAL, material)
}

/// An input of at most a chunk is its own root, compressed with no hasher (and no chaining
/// value stack to set up); a longer one goes through [`Hasher`].
fn one_shot(key: &[u32; 8], flags: u32, input: &[u8]) -> [u8; OUT_LEN] {
    if input.len() <= CHUNK_LEN {
        return one_chunk(key, flags, input);
    }
    let mut hasher = Hasher::with_key(*key, flags);
    hasher.update(input);
    hasher.finalize()
}

/// The root of an input of at most a chunk: its blocks compressed where they lie, the last one
/// read by [`lanes::padded_words`], with no buffer and no copy.
fn one_chunk(key: &[u32; 8], flags: u32, input: &[u8]) -> [u8; OUT_LEN] {
    let mut cv = *key;
    let mut start = CHUNK_START;
    let mut rest = input;
    while let Some((block, after)) = rest.split_first_chunk::<BLOCK_LEN>()
        && !after.is_empty()
    {
        let full = const { Cast::<usize, u32>::narrow_or_stop(BLOCK_LEN) };
        cv = first_8(&compress(&cv, &lanes::block_words(block), 0, full, flags | start));
        start = 0;
        rest = after;
    }
    // At most `BLOCK_LEN` (64) bytes are left: the length fits a `u32`.
    let len = rest.len().narrow_or_stop::<u32>();
    let flags = flags | start | CHUNK_END | ROOT;
    bytes_of(&first_8(&compress(&cv, &lanes::padded_words(rest), 0, len, flags)))
}

/// Eight words as their 32 little-endian bytes.
fn bytes_of(words: &[u32; 8]) -> [u8; OUT_LEN] {
    let mut out = [0; OUT_LEN];
    for (bytes, word) in out.chunks_exact_mut(4).zip(words) {
        bytes.copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// `bytes`' eight little-endian words.
fn words_of(bytes: &[u8; OUT_LEN]) -> [u32; 8] {
    core::array::from_fn(|word| {
        u32::from_le_bytes([
            bytes[4 * word],
            bytes[4 * word + 1],
            bytes[4 * word + 2],
            bytes[4 * word + 3],
        ])
    })
}

fn first_32(block: &[u8; BLOCK_LEN]) -> [u8; OUT_LEN] {
    core::array::from_fn(|byte| block[byte])
}

fn first_8(words: &[u32; 16]) -> [u32; 8] {
    core::array::from_fn(|word| words[word])
}

#[inline(always)]
fn g(state: &mut [u32; 16], [a, b, c, d]: [usize; 4], x: u32, y: u32) {
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(x);
    state[d] = (state[d] ^ state[a]).rotate_right(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(12);
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(y);
    state[d] = (state[d] ^ state[a]).rotate_right(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(7);
}

#[inline(always)]
fn round(state: &mut [u32; 16], m: &[u32; 16], t: &[usize; 16]) {
    g(state, [0, 4, 8, 12], m[t[0]], m[t[1]]);
    g(state, [1, 5, 9, 13], m[t[2]], m[t[3]]);
    g(state, [2, 6, 10, 14], m[t[4]], m[t[5]]);
    g(state, [3, 7, 11, 15], m[t[6]], m[t[7]]);
    g(state, [0, 5, 10, 15], m[t[8]], m[t[9]]);
    g(state, [1, 6, 11, 12], m[t[10]], m[t[11]]);
    g(state, [2, 7, 8, 13], m[t[12]], m[t[13]]);
    g(state, [3, 4, 9, 14], m[t[14]], m[t[15]]);
}

/// BLAKE3's compression function, all sixteen words of its output: the first eight are the
/// next chaining value, all sixteen a root's 64 bytes of output.
pub fn compress(
    cv: &[u32; 8],
    block: &[u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
) -> [u32; 16] {
    let mut state = [
        cv[0],
        cv[1],
        cv[2],
        cv[3],
        cv[4],
        cv[5],
        cv[6],
        cv[7],
        IV[0],
        IV[1],
        IV[2],
        IV[3],
        // The counter's low word, then its high word: dropping the other half is the point.
        counter.truncate_to::<u32>(),
        (counter >> 32).truncate_to::<u32>(),
        block_len,
        flags,
    ];
    // Seven rounds written out, each reading the block where `MESSAGE` says: every index a
    // constant, the state in registers, no round moving the message (49.4 ns a compression on
    // the owner's M5 Max, from 63.6 ns with the message permuted by a map each round:
    // blaze-int-1112-notes/nightly-probes/blake3_compress.rs).
    round(&mut state, block, &MESSAGE[0]);
    round(&mut state, block, &MESSAGE[1]);
    round(&mut state, block, &MESSAGE[2]);
    round(&mut state, block, &MESSAGE[3]);
    round(&mut state, block, &MESSAGE[4]);
    round(&mut state, block, &MESSAGE[5]);
    round(&mut state, block, &MESSAGE[6]);
    for word in 0..8 {
        state[word] ^= state[word + 8];
        state[word + 8] ^= cv[word];
    }
    state
}

/// A node not yet compressed: the last block of a chunk, or a parent's two children. Its
/// chaining value is what its parent takes; as the root, it gives the output.
#[derive(Clone, Copy)]
struct Output {
    input_cv: [u32; 8],
    block: [u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
}

impl Output {
    fn chaining_value(&self) -> [u32; 8] {
        first_8(&compress(&self.input_cv, &self.block, self.counter, self.block_len, self.flags))
    }

    /// Output block `at` of the root: bytes `64 * at..64 * (at + 1)` of the extended output.
    fn root_block(&self, at: u64) -> [u8; BLOCK_LEN] {
        let words = compress(&self.input_cv, &self.block, at, self.block_len, self.flags | ROOT);
        let mut out = [0; BLOCK_LEN];
        for (bytes, word) in out.chunks_exact_mut(4).zip(words) {
            bytes.copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

/// A parent node of `left` and `right`.
fn parent_output(left: &[u32; 8], right: &[u32; 8], key: &[u32; 8], flags: u32) -> Output {
    let mut block = [0; 16];
    block[..8].copy_from_slice(left);
    block[8..].copy_from_slice(right);
    Output {
        input_cv: *key,
        block,
        counter: 0,
        block_len: const { Cast::<usize, u32>::narrow_or_stop(BLOCK_LEN) },
        flags: PARENT | flags,
    }
}

/// The chunk being fed: its chaining value after its compressed blocks, and the block after
/// them, held back until more input shows it is not the chunk's last.
#[derive(Clone)]
struct ChunkState {
    cv: [u32; 8],
    counter: u64,
    block: [u8; BLOCK_LEN],
    block_len: u8,
    blocks_compressed: u8,
    flags: u32,
}

impl ChunkState {
    fn new(key: &[u32; 8], counter: u64, flags: u32) -> ChunkState {
        ChunkState {
            cv: *key,
            counter,
            block: [0; BLOCK_LEN],
            block_len: 0,
            blocks_compressed: 0,
            flags,
        }
    }

    fn len(&self) -> usize {
        BLOCK_LEN * self.blocks_compressed.widen_to::<usize>() + self.block_len.widen_to::<usize>()
    }

    fn start_flag(&self) -> u32 {
        if self.blocks_compressed == 0 { CHUNK_START } else { 0 }
    }

    /// Compresses `block` as one of the chunk's blocks before its last.
    fn compress_block(&mut self, block: &[u8; BLOCK_LEN]) {
        let words = lanes::block_words(block);
        let flags = self.flags | self.start_flag();
        let block_len = const { Cast::<usize, u32>::narrow_or_stop(BLOCK_LEN) };
        self.cv = first_8(&compress(&self.cv, &words, self.counter, block_len, flags));
        self.blocks_compressed += 1;
    }

    /// Feeds `input`; the caller gives a chunk at most [`CHUNK_LEN`] bytes in all. The held
    /// block is topped up first; after it, every block with input after it is compressed where
    /// it lies, and only the last (at most a block) is copied, held back as maybe the chunk's
    /// last.
    fn update(&mut self, mut input: &[u8]) {
        if input.is_empty() {
            return;
        }
        if self.block_len > 0 {
            let held = self.block_len.widen_to::<usize>();
            let take = (BLOCK_LEN - held).min(input.len());
            self.block[held..][..take].copy_from_slice(&input[..take]);
            // `take` is at most `BLOCK_LEN` (64) less what the block holds, so it fits a `u8`.
            self.block_len += take.narrow_or_stop::<u8>();
            input = &input[take..];
            if input.is_empty() {
                return;
            }
            let block = self.block;
            self.compress_block(&block);
            self.block_len = 0;
        }
        while let Some((block, after)) = input.split_first_chunk::<BLOCK_LEN>()
            && !after.is_empty()
        {
            self.compress_block(block);
            input = after;
        }
        self.block = [0; BLOCK_LEN];
        self.block[..input.len()].copy_from_slice(input);
        // At most `BLOCK_LEN` (64) bytes are left: the length fits a `u8`.
        self.block_len = input.len().narrow_or_stop::<u8>();
    }

    fn output(&self) -> Output {
        Output {
            input_cv: self.cv,
            block: lanes::block_words(&self.block),
            counter: self.counter,
            block_len: u32::from(self.block_len),
            flags: self.flags | self.start_flag() | CHUNK_END,
        }
    }
}

/// A message fed in pieces: [`Hasher::new`] (the plain hash), [`Hasher::new_keyed`],
/// [`Hasher::new_derive_key`] and [`Hasher::new_from_context_key`], then [`Hasher::update`]s
/// and [`Hasher::finalize`] (or [`Hasher::finalize_xof`] for more than 32 bytes). Finalizing
/// leaves the hasher as it was, so it can be fed more and finalized again.
#[derive(Clone)]
pub struct Hasher {
    key: [u32; 8],
    flags: u32,
    chunk: ChunkState,
    /// The chaining values of the complete subtrees left of `chunk`, largest first.
    stack: [[u32; 8]; MAX_DEPTH],
    stack_len: u8,
}

impl Hasher {
    fn with_key(key: [u32; 8], flags: u32) -> Hasher {
        Hasher {
            key,
            flags,
            chunk: ChunkState::new(&key, 0, flags),
            stack: [[0; 8]; MAX_DEPTH],
            stack_len: 0,
        }
    }

    /// A hasher of BLAKE3's plain mode.
    pub fn new() -> Hasher {
        Hasher::with_key(IV, 0)
    }

    /// A hasher of the keyed mode under `key`.
    pub fn new_keyed(key: &[u8; OUT_LEN]) -> Hasher {
        Hasher::with_key(words_of(key), KEYED_HASH)
    }

    /// A hasher deriving a key from what it is fed under `context`.
    pub fn new_derive_key(context: &str) -> Hasher {
        Hasher::new_from_context_key(&context_key(context))
    }

    /// [`Hasher::new_derive_key`] for the context whose [`context_key`] is `key`.
    pub fn new_from_context_key(key: &[u8; OUT_LEN]) -> Hasher {
        Hasher::with_key(words_of(key), DERIVE_KEY_MATERIAL)
    }

    /// Bytes fed so far.
    pub fn count(&self) -> u64 {
        self.chunk.counter * CHUNK_LEN.widen_to::<u64>() + self.chunk.len().widen_to::<u64>()
    }

    /// Feeds `input`.
    pub fn update(&mut self, mut input: &[u8]) -> &mut Hasher {
        // The current chunk first. It is closed only when input follows it, since the last
        // chunk is finalized differently (as the root when it is the only one).
        if self.chunk.len() > 0 {
            let take = (CHUNK_LEN - self.chunk.len()).min(input.len());
            self.chunk.update(&input[..take]);
            input = &input[take..];
            if input.is_empty() {
                return self;
            }
            let cv = self.chunk.output().chaining_value();
            let total = self.chunk.counter + 1;
            self.push_chunk(cv, total);
            self.chunk = ChunkState::new(&self.key, total, self.flags);
        }
        // The chunk is empty here. Whole chunks with input after them, a batch side by side.
        let width = lanes_for_this_cpu();
        let batch = width * CHUNK_LEN;
        let mut counter = self.chunk.counter;
        while input.len() > batch {
            let mut cvs = [[0; 8]; MOST_LANES];
            hash_chunks(&self.key, &input[..batch], counter, self.flags, &mut cvs[..width]);
            for cv in &cvs[..width] {
                counter += 1;
                self.push_chunk(*cv, counter);
            }
            input = &input[batch..];
        }
        while input.len() > CHUNK_LEN {
            let mut chunk = ChunkState::new(&self.key, counter, self.flags);
            chunk.update(&input[..CHUNK_LEN]);
            counter += 1;
            self.push_chunk(chunk.output().chaining_value(), counter);
            input = &input[CHUNK_LEN..];
        }
        if counter != self.chunk.counter {
            self.chunk = ChunkState::new(&self.key, counter, self.flags);
        }
        self.chunk.update(input);
        self
    }

    /// Adds the chaining value of the chunk that makes `total` chunks, merging every subtree it
    /// completes (one per trailing zero bit of `total`): eager merging, sound because more input
    /// is known to follow.
    fn push_chunk(&mut self, mut cv: [u32; 8], mut total: u64) {
        while total & 1 == 0 {
            self.stack_len -= 1;
            let left = self.stack[self.stack_len.widen_to::<usize>()];
            cv = parent_output(&left, &cv, &self.key, self.flags).chaining_value();
            total >>= 1;
        }
        self.stack[self.stack_len.widen_to::<usize>()] = cv;
        self.stack_len += 1;
    }

    /// The root node: the current chunk folded with every subtree left of it, right to left.
    fn root(&self) -> Output {
        let mut output = self.chunk.output();
        for left in self.stack[..self.stack_len.widen_to::<usize>()].iter().rev() {
            output = parent_output(left, &output.chaining_value(), &self.key, self.flags);
        }
        output
    }

    /// The hash of what was fed so far.
    pub fn finalize(&self) -> [u8; OUT_LEN] {
        first_32(&self.root().root_block(0))
    }

    /// The extended output of what was fed so far, read from its start.
    pub fn finalize_xof(&self) -> OutputReader {
        OutputReader { root: self.root(), position: 0 }
    }
}

impl Default for Hasher {
    fn default() -> Hasher {
        Hasher::new()
    }
}

/// The extended output of a [`Hasher`]: any number of bytes, read from any position.
#[derive(Clone)]
pub struct OutputReader {
    root: Output,
    position: u64,
}

impl OutputReader {
    /// Fills `out` with the output from the position on, and moves the position past them.
    pub fn fill(&mut self, mut out: &mut [u8]) {
        while !out.is_empty() {
            let block = self.root.root_block(self.position / BLOCK_LEN.widen_to::<u64>());
            // A remainder of `BLOCK_LEN` (64): it fits a `usize`.
            let offset = (self.position % BLOCK_LEN.widen_to::<u64>()).narrow_or_stop::<usize>();
            let take = (BLOCK_LEN - offset).min(out.len());
            let (head, rest) = core::mem::take(&mut out).split_at_mut(take);
            head.copy_from_slice(&block[offset..offset + take]);
            out = rest;
            self.position += take.widen_to::<u64>();
        }
    }

    /// The position of the next byte [`OutputReader::fill`] reads.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Moves the position to `position`.
    pub fn set_position(&mut self, position: u64) {
        self.position = position;
    }
}

/// Chunks a batch hashes side by side at the target's baseline: 8, two NEON registers a word of
/// the state (one AVX2 register, two SSE2 ones). On the owner's M5 Max a block a lane took
/// 25.1 ns at 4 lanes, 16.0 at 8 and 14.3 at 16 (blaze-int-1112-notes/nightly-probes/
/// blake3_lanes.rs): 8 is within 12% of 16 at half the batch, so inputs of 8 to 16 KiB get it.
const CHUNK_LANES: usize = 8;

/// Chunks a batch hashes side by side on this CPU: 16 where it has AVX-512F, else
/// [`CHUNK_LANES`].
fn lanes_for_this_cpu() -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        if cpu::has(cpu::Feature::Avx512f) {
            return 16;
        }
    }
    CHUNK_LANES
}

/// The chaining values of `out.len()` whole chunks (`input`, `out.len() * CHUNK_LEN` bytes),
/// numbered from `counter`, side by side: the widths [`lanes_for_this_cpu`] gives.
fn hash_chunks(key: &[u32; 8], input: &[u8], counter: u64, flags: u32, out: &mut [[u32; 8]]) {
    #[cfg(target_arch = "x86_64")]
    {
        if out.len() == 16 {
            return x86::chunks_avx512(key, input, counter, flags, out);
        }
        if cpu::has(cpu::Feature::Avx2) {
            return x86::chunks_avx2(key, input, counter, flags, out);
        }
    }
    chunks_of::<CHUNK_LANES>(key, input, counter, flags, out);
}

/// [`hash_chunks`] at `N` lanes. Always inlined, so it is vectorized for the features of the
/// function it is inlined into.
#[inline(always)]
fn chunks_of<const N: usize>(
    key: &[u32; 8],
    input: &[u8],
    counter: u64,
    flags: u32,
    out: &mut [[u32; 8]],
) {
    assert_eq!(out.len(), N, "a batch of {N} chunks");
    assert_eq!(input.len(), N * CHUNK_LEN, "a batch of {N} whole chunks");
    let mut cv: [[u32; N]; 8] = key.map(|word| [word; N]);
    // Each lane's counter, split into its low and high words: dropping the other half is the
    // point.
    let counter_low: [u32; N] =
        core::array::from_fn(|lane| (counter + lane.widen_to::<u64>()).truncate_to::<u32>());
    let counter_high: [u32; N] =
        core::array::from_fn(|lane| ((counter + lane.widen_to::<u64>()) >> 32).truncate_to::<u32>());
    let blocks = CHUNK_LEN / BLOCK_LEN;
    for block in 0..blocks {
        let rows: [[u32; 16]; N] = core::array::from_fn(|lane| {
            let at = lane * CHUNK_LEN + block * BLOCK_LEN;
            lanes::block_words(input[at..at + BLOCK_LEN].try_into().expect("a whole block"))
        });
        let mut block_flags = flags;
        if block == 0 {
            block_flags |= CHUNK_START;
        }
        if block == blocks - 1 {
            block_flags |= CHUNK_END;
        }
        cv = lanes::compress(
            &cv,
            &lanes::columns(&rows),
            counter_low,
            counter_high,
            [const { Cast::<usize, u32>::narrow_or_stop(BLOCK_LEN) }; N],
            block_flags,
        );
    }
    for (lane, slot) in out.iter_mut().enumerate() {
        *slot = core::array::from_fn(|word| cv[word][lane]);
    }
}

/// Short inputs [`many`] hashes side by side at the target's baseline: 8, as [`CHUNK_LANES`].
const MANY_LANES: usize = 8;

/// The roots of `inputs` under `key` and `flags`, into `out`: 16 lanes in AVX-512 registers
/// and 8 in AVX2 ones where the CPU has them, else [`MANY_LANES`].
fn many(key: &[u32; 8], flags: u32, inputs: &[&[u8]], out: &mut [[u8; OUT_LEN]]) {
    assert_eq!(inputs.len(), out.len(), "a digest for each input");
    #[cfg(target_arch = "x86_64")]
    {
        if cpu::has(cpu::Feature::Avx512f) {
            return x86::many_avx512(key, flags, inputs, out);
        }
        if cpu::has(cpu::Feature::Avx2) {
            return x86::many_avx2(key, flags, inputs, out);
        }
    }
    many_of::<MANY_LANES>(key, flags, inputs, out);
}

/// No input in a lane of [`many_of`].
const IDLE: usize = usize::MAX;

/// [`many`] at `N` lanes. Each lane hashes one input of at most a chunk, a block a step, and
/// takes the next input the step after its last block, so lanes of unequal inputs never wait
/// for each other and no input is sorted or grouped: a tape of mixed lengths costs its blocks
/// divided by `N` steps, plus the last few steps with lanes empty. That needs no buffer, so the
/// crate stays without allocation. An input longer than a chunk is hashed one shot where it
/// comes. Always inlined, so it is vectorized for the features of the function it is inlined
/// into.
#[inline(always)]
fn many_of<const N: usize>(key: &[u32; 8], flags: u32, inputs: &[&[u8]], out: &mut [[u8; OUT_LEN]]) {
    let full = const { Cast::<usize, u32>::narrow_or_stop(BLOCK_LEN) };
    let mut cv: [[u32; N]; 8] = key.map(|word| [word; N]);
    // Each lane's input (its place in `inputs`, or `IDLE`), the bytes of it not yet compressed,
    // and `CHUNK_START` until its first block is.
    let mut owner = [IDLE; N];
    let mut rest: [&[u8]; N] = [&[]; N];
    let mut start = [0; N];
    let mut next = 0;
    loop {
        let mut live = false;
        for lane in 0..N {
            if owner[lane] == IDLE {
                while let Some(input) = inputs.get(next)
                    && input.len() > CHUNK_LEN
                {
                    out[next] = one_shot(key, flags, input);
                    next += 1;
                }
                if let Some(input) = inputs.get(next) {
                    owner[lane] = next;
                    rest[lane] = input;
                    start[lane] = CHUNK_START;
                    next += 1;
                    for (word, value) in key.iter().enumerate() {
                        cv[word][lane] = *value;
                    }
                }
            }
            live |= owner[lane] != IDLE;
        }
        if !live {
            return;
        }
        let mut rows = [[0; 16]; N];
        let mut block_len = [0; N];
        let mut lane_flags = [0; N];
        let mut last = [false; N];
        for lane in 0..N {
            if let Some((block, after)) = rest[lane].split_first_chunk::<BLOCK_LEN>()
                && !after.is_empty()
            {
                rows[lane] = lanes::block_words(block);
                block_len[lane] = full;
                lane_flags[lane] = flags | start[lane];
                rest[lane] = after;
            } else {
                // An idle lane hashes an empty block, and nothing reads its result.
                rows[lane] = lanes::padded_words(rest[lane]);
                // At most `BLOCK_LEN` (64) bytes: the length fits a `u32`.
                block_len[lane] = rest[lane].len().narrow_or_stop::<u32>();
                lane_flags[lane] = flags | start[lane] | CHUNK_END | ROOT;
                last[lane] = owner[lane] != IDLE;
            }
            start[lane] = 0;
        }
        cv = lanes::compress_each(
            &cv,
            &lanes::columns(&rows),
            [0; N],
            [0; N],
            block_len,
            lane_flags,
        );
        for lane in 0..N {
            if last[lane] {
                out[owner[lane]] = bytes_of(&core::array::from_fn(|word| cv[word][lane]));
                owner[lane] = IDLE;
            }
        }
    }
}

/// [`chunks_of`] compiled for AVX-512F at 16 lanes (each word of the transposed state one
/// 512-bit register, the rotates `vprord`) and for AVX2 at 8 (one 256-bit register a word),
/// where the baseline would give them SSE2's two 128-bit registers a word and three
/// instructions a rotate.
#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod x86 {
    pub(super) fn chunks_avx512(
        key: &[u32; 8],
        input: &[u8],
        counter: u64,
        flags: u32,
        out: &mut [[u32; 8]],
    ) {
        assert!(super::cpu::has(super::cpu::Feature::Avx512f), "AVX-512F lanes on a CPU without it");
        // SAFETY: a `target_feature` function may run only on a CPU with the feature, and this
        // one has AVX-512F, asserted just above.
        unsafe { chunks_wide(key, input, counter, flags, out) }
    }

    pub(super) fn chunks_avx2(
        key: &[u32; 8],
        input: &[u8],
        counter: u64,
        flags: u32,
        out: &mut [[u32; 8]],
    ) {
        assert!(super::cpu::has(super::cpu::Feature::Avx2), "AVX2 lanes on a CPU without it");
        // SAFETY: a `target_feature` function may run only on a CPU with the feature, and this
        // one has AVX2, asserted just above.
        unsafe { chunks_narrow(key, input, counter, flags, out) }
    }

    pub(super) fn many_avx512(
        key: &[u32; 8],
        flags: u32,
        inputs: &[&[u8]],
        out: &mut [[u8; super::OUT_LEN]],
    ) {
        assert!(super::cpu::has(super::cpu::Feature::Avx512f), "AVX-512F lanes on a CPU without it");
        // SAFETY: a `target_feature` function may run only on a CPU with the feature, and this
        // one has AVX-512F, asserted just above.
        unsafe { many_wide(key, flags, inputs, out) }
    }

    pub(super) fn many_avx2(
        key: &[u32; 8],
        flags: u32,
        inputs: &[&[u8]],
        out: &mut [[u8; super::OUT_LEN]],
    ) {
        assert!(super::cpu::has(super::cpu::Feature::Avx2), "AVX2 lanes on a CPU without it");
        // SAFETY: a `target_feature` function may run only on a CPU with the feature, and this
        // one has AVX2, asserted just above.
        unsafe { many_narrow(key, flags, inputs, out) }
    }

    #[target_feature(enable = "avx512f")]
    fn many_wide(key: &[u32; 8], flags: u32, inputs: &[&[u8]], out: &mut [[u8; super::OUT_LEN]]) {
        super::many_of::<16>(key, flags, inputs, out);
    }

    #[target_feature(enable = "avx2")]
    fn many_narrow(key: &[u32; 8], flags: u32, inputs: &[&[u8]], out: &mut [[u8; super::OUT_LEN]]) {
        super::many_of::<8>(key, flags, inputs, out);
    }

    #[target_feature(enable = "avx512f")]
    fn chunks_wide(
        key: &[u32; 8],
        input: &[u8],
        counter: u64,
        flags: u32,
        out: &mut [[u32; 8]],
    ) {
        super::chunks_of::<16>(key, input, counter, flags, out);
    }

    #[target_feature(enable = "avx2")]
    fn chunks_narrow(
        key: &[u32; 8],
        input: &[u8],
        counter: u64,
        flags: u32,
        out: &mut [[u32; 8]],
    ) {
        super::chunks_of::<8>(key, input, counter, flags, out);
    }
}
