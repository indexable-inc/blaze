//! BLAKE3, written here (the reference algorithm of the BLAKE3 paper and its reference
//! implementation, github.com/BLAKE3-team/BLAKE3, `reference_impl`): every mode blaze uses
//! (the plain hash, the keyed hash, key derivation by context string and by a context's key)
//! and the extended output, bit for bit the function official BLAKE3 1.x computes, since every
//! content ID, store key and record blaze has written is BLAKE3 of its content.
//!
//! No dependencies, so the bottom of the tree (`blaze-ids`, `blaze-id`) stands on nothing from
//! outside the workspace (docs/v2/plan/design/bottom.md, "Also decided on 2026-11-15").
//!
//! How it is fast without assembly of its own:
//!
//! - An input of two chunks or more, one shot or streamed, is hashed as the largest
//!   power-of-two subtrees that fit, straight from the caller's bytes (`src/tree.rs`): a subtree's
//!   whole chunks side by side, a SIMD lane each ([`lanes::compress`], safe `core::simd` code),
//!   then each level's parents side by side, up to the two children of the subtree's top node,
//!   which waits on a lazily merged stack because it may be the root. So 2 KiB takes two lanes,
//!   4 KiB four, 8 KiB eight, and no chunk of a whole-chunk input is hashed alone.
//! - Many independent short inputs go side by side through [`hash_many`] (and
//!   [`keyed_hash_many`], [`derive_key_many`]), an input a lane, each lane taking the next input
//!   the step after its last block.
//! - A lone block (an input of at most a chunk, a partial last chunk, a lone parent, the root)
//!   is compressed where it lies by [`compress`], with no hasher and no copy.
//!
//! Which strategies run is one value, an [`Engine`] ([`engine`]): the lone-block form
//! ([`Single`]), the lanes ([`Degree`]) and the instructions ([`Isa`]), chosen by the caller and
//! passed to the `*_on` entries; the entries without one run [`Engine::detected`], this CPU's
//! defaults. Every engine computes the same bytes, held to the official test vectors, to the
//! `blake3` crate at every length to 4,097 bytes and every alignment, at every length to five
//! chunks and a byte under every engine one shot and streamed in drawn pieces, and lane by lane
//! to the scalar compression (`tests/`). The crate is `no_std` and allocates nothing.
//!
//! # Cost, and the floor (rule 67)
//!
//! `examples/cost.rs`, 2026-11-15, the owner's M5 Max (one P core at 4.25 GHz, measured by a
//! chain of dependent adds), best of seven runs, hot in cache, before the wide tree:
//!
//! | input | before | then | floor | then / floor |
//! |---|---|---|---|---|
//! | 64 B, one call | 63.2 ns | 46.6 ns | 46 ns | 1.01 |
//! | 1 KiB, one call | 1,010 ns | 784 ns | 738 ns | 1.06 |
//! | 16 MiB, one call | 756 MB/s | 2,958 MB/s | 3,760 MB/s | 1.27 |
//! | 64 B each, `hash_many` | 14.6 M/s | 41.6 M/s | 62.5 M/s | 1.50 |
//! | 1 KiB each, `hash_many` | 0.93 M/s | 3.10 M/s | 3.91 M/s | 1.26 |
//!
//! The floor of one input of a chunk or less is the latency of its chain of compressions: each
//! block's seven rounds are fourteen half-rounds of fourteen dependent one-cycle operations,
//! 196 cycles, 46 ns a block, and a chunk's blocks run one after another. Only independent
//! chunks or inputs (lanes) beat it. The floor of lanes is the compression alone at 8 lanes
//! with nothing around it, 16.0 ns a block a lane (blaze-int-1112-notes/nightly-probes/
//! blake3_lanes.rs), plus a parent block a chunk for a long input, those too at lanes' cost
//! now. Between one and sixteen chunks the old hasher ran every chunk but the batches of eight
//! alone (4 KiB at half the official crate's speed); the wide tree runs them all side by side.

#![no_std]
#![feature(portable_simd, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
// A core but for one site: [`cpu`]'s process-wide cache of the CPU's features (`FOUND`), which
// D170 moves to blaze-sys as a value the driver reads once and hands in; blaze-sys has no CPU
// call yet (wave-1124-sansio, W1 group a's report). The `*_on` entries take it handed in.
#![blaze::sans_io]
#![deny(blaze::sans_io_core, blaze::std_use, blaze::implicit_allocator, blaze::process_static)]
#![deny(unsafe_code)]

pub mod cpu;
pub mod engine;
pub mod lanes;
mod many;
mod pair;
mod rows;
mod tree;

pub use engine::{Degree, Engine, EngineOpts, Isa, Pair, Single};
pub use many::{CountMismatch, MANY_DEFAULT, Many, Mode, many_checked, many_checked_on};

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

/// The most chaining values a hasher's stack holds: one per level of a tree of 2^54 chunks,
/// 2^64 bytes, and one more, the subtree's right child that lazy merging leaves on top.
const MAX_DEPTH: usize = 54;

/// The most lanes of any degree ([`Degree::Sixteen`]).
pub(crate) const MOST_LANES: usize = 16;

/// The hash of `input`, BLAKE3's plain mode.
pub fn hash(input: &[u8]) -> [u8; OUT_LEN] {
    by_default(&IV, 0, input)
}

/// [`one_shot`] by the engine the entries without one run: an input of at most a chunk by
/// [`Single::Scalar`], [`Engine::detected`]'s lone-block form, without reading the CPU's
/// features at all (on x86-64 a load or two a call, which a 64-byte hash of 50 ns would feel);
/// a longer one by [`Engine::detected`].
#[inline(always)]
fn by_default(key: &[u32; 8], flags: u32, input: &[u8]) -> [u8; OUT_LEN] {
    if input.len() <= CHUNK_LEN {
        return one_chunk(Engine::SCALAR, key, flags, input);
    }
    one_shot(Engine::detected(), key, flags, input)
}

/// The digest of `input` in `mode`, by `engine`: [`hash`], [`keyed_hash`] or
/// [`derive_key_with`], with the strategies handed in.
pub fn digest_on(engine: Engine, mode: Mode, input: &[u8]) -> [u8; OUT_LEN] {
    let (key, flags) = mode.key_and_flags();
    one_shot(engine, &key, flags, input)
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
    by_default(&words_of(key), KEYED_HASH, input)
}

/// The key `material` derives under `context`, BLAKE3's key derivation mode.
pub fn derive_key(context: &str, material: &[u8]) -> [u8; OUT_LEN] {
    derive_key_with(&context_key(context), material)
}

/// `context`'s key: the hash of the context string in the context mode, the key every input
/// under that context is hashed with (official BLAKE3's `hazmat::hash_derive_key_context`).
pub fn context_key(context: &str) -> [u8; OUT_LEN] {
    by_default(&IV, DERIVE_KEY_CONTEXT, context.as_bytes())
}

/// The key `material` derives under the context whose [`context_key`] is `key`: what
/// [`derive_key`] computes after the context's key, with the key already made.
pub fn derive_key_with(key: &[u8; OUT_LEN], material: &[u8]) -> [u8; OUT_LEN] {
    by_default(&words_of(key), DERIVE_KEY_MATERIAL, material)
}

/// An input of at most a chunk is its own root, compressed with no hasher (and no chaining
/// value stack to set up); a longer one is the parent of its tree's top two children, which
/// the wide tree gives straight from `input` (the official crate's `hash_all_at_once`).
fn one_shot(e: Engine, key: &[u32; 8], flags: u32, input: &[u8]) -> [u8; OUT_LEN] {
    if input.len() <= CHUNK_LEN {
        return one_chunk(e, key, flags, input);
    }
    let [left, right] = tree::subtree_to_parent(e, key, flags, input, 0);
    first_32(&parent_output(&left, &right, key, flags, e).root_block(0))
}

/// The root of an input of at most a chunk: its blocks compressed where they lie, the last one
/// read by [`lanes::padded_words`], with no buffer and no copy. For an input of at most a block
/// that is one compression, its words read by overlapping loads, and the 32 bytes written.
fn one_chunk(e: Engine, key: &[u32; 8], flags: u32, input: &[u8]) -> [u8; OUT_LEN] {
    let mut cv = *key;
    let mut start = CHUNK_START;
    let mut rest = input;
    while let Some((block, after)) = rest.split_first_chunk::<BLOCK_LEN>()
        && !after.is_empty()
    {
        single_in_place(e, &mut cv, block, 0, flags | start);
        start = 0;
        rest = after;
    }
    // At most `BLOCK_LEN` (64) bytes are left: the length fits a `u32`.
    let len = rest.len().narrow_or_stop::<u32>();
    let flags = flags | start | CHUNK_END | ROOT;
    bytes_of(&first_8(&single(e, &cv, &lanes::padded_words(rest), 0, len, flags)))
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

pub(crate) fn first_8(words: &[u32; 16]) -> [u32; 8] {
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
/// next chaining value, all sixteen a root's 64 bytes of output. The scalar form,
/// [`Single::Scalar`].
pub fn compress(
    cv: &[u32; 8],
    block: &[u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
) -> [u32; 16] {
    let mut state = rounds(cv, block, counter, block_len, flags);
    for word in 0..8 {
        state[word] ^= state[word + 8];
        state[word + 8] ^= cv[word];
    }
    state
}

/// [`compress`]'s next chaining value written over `cv`, the block read from its bytes: what a
/// chunk's blocks before its last take, as the official crate's `compress_in_place`. Through
/// `compress`, a block's words went to a stack array and all sixteen output words came back
/// through memory for eight to be read. The twin is the first eight words of [`compress`] of
/// [`lanes::block_words`] (`tests::in_place_is_the_first_eight_words`).
pub(crate) fn compress_in_place(cv: &mut [u32; 8], block: &[u8; BLOCK_LEN], counter: u64, block_len: u32, flags: u32) {
    let state = rounds(cv, &lanes::block_words(block), counter, block_len, flags);
    for word in 0..8 {
        cv[word] = state[word] ^ state[word + 8];
    }
}

/// [`compress`]'s state after its seven rounds, before the feed-forward.
#[inline(always)]
fn rounds(cv: &[u32; 8], block: &[u32; 16], counter: u64, block_len: u32, flags: u32) -> [u32; 16] {
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
    state
}

/// [`compress`] by `engine`'s [`Single`] form on its [`Isa`]: every form gives the same sixteen
/// words for every input (`tests/engines.rs`, `single_forms_are_the_scalar_one`).
pub fn compress_by(
    engine: Engine,
    cv: &[u32; 8],
    block: &[u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
) -> [u32; 16] {
    single(engine, cv, block, counter, block_len, flags)
}

/// The one place a lone block's compression is chosen: the engine's [`Single`].
#[inline(always)]
pub(crate) fn single(e: Engine, cv: &[u32; 8], block: &[u32; 16], counter: u64, block_len: u32, flags: u32) -> [u32; 16] {
    match e.single {
        Single::Scalar => compress(cv, block, counter, block_len, flags),
        Single::Rows => {
            #[cfg(target_arch = "x86_64")]
            {
                x86::rows(e.isa, cv, block, counter, block_len, flags)
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                rows::compress(cv, block, counter, block_len, flags)
            }
        }
    }
}

/// [`single`]'s next chaining value written over `cv`, for a block of a chunk before its last:
/// the scalar form by [`compress_in_place`], the row form through its sixteen words.
#[inline(always)]
pub(crate) fn single_in_place(e: Engine, cv: &mut [u32; 8], block: &[u8; BLOCK_LEN], counter: u64, flags: u32) {
    let full = const { Cast::<usize, u32>::narrow_or_stop(BLOCK_LEN) };
    match e.single {
        Single::Scalar => compress_in_place(cv, block, counter, full, flags),
        Single::Rows => *cv = first_8(&single(e, cv, &lanes::block_words(block), counter, full, flags)),
    }
}

/// A node not yet compressed: the last block of a chunk, or a parent's two children. Its
/// chaining value is what its parent takes; as the root, it gives the output.
#[derive(Clone, Copy)]
pub(crate) struct Output {
    engine: Engine,
    input_cv: [u32; 8],
    block: [u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
}

impl Output {
    pub(crate) fn chaining_value(&self) -> [u32; 8] {
        first_8(&single(self.engine, &self.input_cv, &self.block, self.counter, self.block_len, self.flags))
    }

    /// Output block `at` of the root: bytes `64 * at..64 * (at + 1)` of the extended output.
    fn root_block(&self, at: u64) -> [u8; BLOCK_LEN] {
        let words = single(self.engine, &self.input_cv, &self.block, at, self.block_len, self.flags | ROOT);
        let mut out = [0; BLOCK_LEN];
        for (bytes, word) in out.chunks_exact_mut(4).zip(words) {
            bytes.copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

/// A parent node of `left` and `right`.
fn parent_output(left: &[u32; 8], right: &[u32; 8], key: &[u32; 8], flags: u32, engine: Engine) -> Output {
    Output {
        engine,
        input_cv: *key,
        block: tree::parent_block(&[*left, *right]),
        counter: 0,
        block_len: const { Cast::<usize, u32>::narrow_or_stop(BLOCK_LEN) },
        flags: PARENT | flags,
    }
}

/// The chunk being fed: its chaining value after its compressed blocks, and the block after
/// them, held back until more input shows it is not the chunk's last. The bytes of `block`
/// past `block_len` are zero (every write below keeps it so), which is the last block's
/// padding.
#[derive(Clone)]
pub(crate) struct ChunkState {
    cv: [u32; 8],
    counter: u64,
    block: [u8; BLOCK_LEN],
    block_len: u8,
    blocks_compressed: u8,
    flags: u32,
}

impl ChunkState {
    pub(crate) fn new(key: &[u32; 8], counter: u64, flags: u32) -> ChunkState {
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
        BLOCK_LEN * usize::from(self.blocks_compressed) + usize::from(self.block_len)
    }

    fn start_flag(&self) -> u32 {
        if self.blocks_compressed == 0 { CHUNK_START } else { 0 }
    }

    /// Appends `input` to the held block when it fits there, compressing nothing: the common
    /// case of a hasher fed a few bytes at a time, one bounds check and one copy. False, and
    /// nothing changed, when it does not fit. Sound because the held block is compressed only
    /// once input follows it, and a block of at most 64 bytes needs nothing after it yet; a
    /// held block of 0 bytes is the chunk's first (`block_len` is 0 only before a chunk's
    /// first byte), so the bytes go where the first block's do.
    #[inline(always)]
    fn append(&mut self, input: &[u8]) -> bool {
        let held = usize::from(self.block_len);
        let Some(room) = self.block.get_mut(held..held + input.len()) else {
            return false;
        };
        room.copy_from_slice(input);
        // `held + input.len()` is at most `BLOCK_LEN` (64), the room's end: it fits a `u8`.
        self.block_len = (held + input.len()).narrow_or_stop::<u8>();
        true
    }

    /// Compresses `block` as one of the chunk's blocks before its last.
    fn compress_block(&mut self, e: Engine, block: &[u8; BLOCK_LEN]) {
        let flags = self.flags | self.start_flag();
        single_in_place(e, &mut self.cv, block, self.counter, flags);
        self.blocks_compressed += 1;
    }

    /// Feeds `input`; the caller gives a chunk at most [`CHUNK_LEN`] bytes in all. The held
    /// block is topped up first; after it, every block with input after it is compressed where
    /// it lies, and only the last (at most a block) is copied, held back as maybe the chunk's
    /// last.
    pub(crate) fn update(&mut self, e: Engine, mut input: &[u8]) {
        if self.append(input) {
            return;
        }
        if self.block_len > 0 {
            let held = usize::from(self.block_len);
            let take = BLOCK_LEN - held;
            self.block[held..].copy_from_slice(&input[..take]);
            input = &input[take..];
            // `append` refused, so more than `take` bytes came: the held block is not the last.
            let block = self.block;
            self.compress_block(e, &block);
            self.block_len = 0;
        }
        while let Some((block, after)) = input.split_first_chunk::<BLOCK_LEN>()
            && !after.is_empty()
        {
            self.compress_block(e, block);
            input = after;
        }
        self.block = [0; BLOCK_LEN];
        self.block[..input.len()].copy_from_slice(input);
        // At most `BLOCK_LEN` (64) bytes are left: the length fits a `u8`.
        self.block_len = input.len().narrow_or_stop::<u8>();
    }

    pub(crate) fn output(&self, e: Engine) -> Output {
        Output {
            engine: e,
            input_cv: self.cv,
            block: lanes::block_words(&self.block),
            counter: self.counter,
            block_len: u32::from(self.block_len),
            flags: self.flags | self.start_flag() | CHUNK_END,
        }
    }
}

/// A message fed in pieces: [`Hasher::new`] (the plain hash), [`Hasher::new_keyed`],
/// [`Hasher::new_derive_key`] and [`Hasher::new_from_context_key`] (or [`Hasher::on`], any mode
/// by a handed engine), then [`Hasher::update`]s and [`Hasher::finalize`] (or
/// [`Hasher::finalize_xof`] for more than 32 bytes). Finalizing leaves the hasher as it was, so
/// it can be fed more and finalized again.
#[derive(Clone)]
pub struct Hasher {
    engine: Engine,
    key: [u32; 8],
    flags: u32,
    chunk: ChunkState,
    /// The chaining values of the complete subtrees left of `chunk`, largest first, merged
    /// lazily: the top one waits for what comes after it, since it may be a child of the root.
    stack: [[u32; 8]; MAX_DEPTH + 1],
    stack_len: u8,
}

impl Hasher {
    fn with_key(engine: Engine, key: [u32; 8], flags: u32) -> Hasher {
        Hasher {
            engine,
            key,
            flags,
            chunk: ChunkState::new(&key, 0, flags),
            stack: [[0; 8]; MAX_DEPTH + 1],
            stack_len: 0,
        }
    }

    /// A hasher of BLAKE3's plain mode.
    pub fn new() -> Hasher {
        Hasher::with_key(Engine::detected(), IV, 0)
    }

    /// A hasher of `mode` that runs by `engine`.
    pub fn on(engine: Engine, mode: Mode) -> Hasher {
        let (key, flags) = mode.key_and_flags();
        Hasher::with_key(engine, key, flags)
    }

    /// A hasher of the keyed mode under `key`.
    pub fn new_keyed(key: &[u8; OUT_LEN]) -> Hasher {
        Hasher::with_key(Engine::detected(), words_of(key), KEYED_HASH)
    }

    /// A hasher deriving a key from what it is fed under `context`.
    pub fn new_derive_key(context: &str) -> Hasher {
        Hasher::new_from_context_key(&context_key(context))
    }

    /// [`Hasher::new_derive_key`] for the context whose [`context_key`] is `key`.
    pub fn new_from_context_key(key: &[u8; OUT_LEN]) -> Hasher {
        Hasher::with_key(Engine::detected(), words_of(key), DERIVE_KEY_MATERIAL)
    }

    /// Bytes fed so far.
    pub fn count(&self) -> u64 {
        self.chunk.counter * CHUNK_LEN.widen_to::<u64>() + self.chunk.len().widen_to::<u64>()
    }

    /// Feeds `input`.
    pub fn update(&mut self, mut input: &[u8]) -> &mut Hasher {
        // A few bytes that fit the held block: one bounds check and a copy.
        if self.chunk.append(input) {
            return self;
        }
        let e = self.engine;
        // The current chunk first. It is closed only when input follows it, since the last
        // chunk is finalized differently (as the root when it is the only one).
        if self.chunk.len() > 0 {
            let take = (CHUNK_LEN - self.chunk.len()).min(input.len());
            self.chunk.update(e, &input[..take]);
            input = &input[take..];
            if input.is_empty() {
                return self;
            }
            let counter = self.chunk.counter;
            let cv = self.chunk.output(e).chaining_value();
            self.push_cv(cv, counter);
            self.chunk = ChunkState::new(&self.key, counter + 1, self.flags);
        }
        // The chunk is empty here. While more than a chunk is left (so none of it is the last
        // chunk), the largest power-of-two subtree that fits and that the chunks so far leave
        // room for (a whole number of it so far), hashed wide straight from `input`: so a
        // caller who feeds powers of two gets each as one subtree.
        while input.len() > CHUNK_LEN {
            let counter = self.chunk.counter;
            let so_far = counter * CHUNK_LEN.widen_to::<u64>();
            let mut subtree = tree::largest_power_of_two_at_most(input.len());
            while (subtree - 1).widen_to::<u64>() & so_far != 0 {
                subtree /= 2;
            }
            let chunks = (subtree / CHUNK_LEN).widen_to::<u64>();
            if subtree <= CHUNK_LEN {
                let (chunk, _) = input.split_first_chunk::<CHUNK_LEN>().expect("more than a chunk is left");
                self.push_cv(tree::chunk_cv(e, &self.key, self.flags, chunk, counter), counter);
            } else {
                let [left, right] = tree::subtree_to_parent(e, &self.key, self.flags, &input[..subtree], counter);
                // Pushed one at a time, so the right one is never merged before what follows
                // it is known: it may be a child of the root.
                self.push_cv(left, counter);
                self.push_cv(right, counter + chunks / 2);
            }
            self.chunk = ChunkState::new(&self.key, counter + chunks, self.flags);
            input = &input[subtree..];
        }
        self.chunk.update(e, input);
        self
    }

    /// Pushes the chaining value of the subtree whose first chunk is `counter`, after merging
    /// what the chunks before it complete: each subtree that stays is a one bit of `counter`
    /// (the official crate's "count the 1-bits" form of the paper's merge rule, which holds for
    /// subtrees of any power-of-two size). The new value is not merged: it may be the root's
    /// child, which only what follows it can say.
    fn push_cv(&mut self, cv: [u32; 8], counter: u64) {
        let keep = counter.count_ones();
        while u32::from(self.stack_len) > keep {
            let top = usize::from(self.stack_len);
            let parent = parent_output(&self.stack[top - 2], &self.stack[top - 1], &self.key, self.flags, self.engine);
            self.stack[top - 2] = parent.chaining_value();
            self.stack_len -= 1;
        }
        self.stack[usize::from(self.stack_len)] = cv;
        self.stack_len += 1;
    }

    /// The root node: the current chunk (or, if it is empty, the parent of the top two
    /// subtrees) folded with every subtree left of it, right to left.
    fn root(&self) -> Output {
        let e = self.engine;
        let depth = usize::from(self.stack_len);
        if depth == 0 {
            return self.chunk.output(e);
        }
        let (mut output, left) = if self.chunk.len() > 0 {
            // `update` merges the stack lazily: bytes fed into a chunk after a pushed subtree
            // (an update's tail, or `append` into the empty chunk an exactly fed subtree leaves)
            // merge nothing, so the stack may hold more subtrees than the chunk's counter has one
            // bits. The surplus at its top is one complete subtree left of the chunk and is
            // merged first, as the official crate's `merge_cv_stack` does before it finalizes;
            // folding each into the chunk one by one gives another tree, and another hash.
            let keep = self.chunk.counter.count_ones();
            let mut top = self.stack_len;
            let mut merged = self.stack[depth - 1];
            while u32::from(top) > keep {
                assert!(top >= 2, "blaze_blake3::Hasher: a stack deeper than its chunk counter allows (a bug in `update`)");
                let below = usize::from(top) - 2;
                merged = parent_output(&self.stack[below], &merged, &self.key, self.flags, e).chaining_value();
                top -= 1;
            }
            let chunk = self.chunk.output(e).chaining_value();
            (parent_output(&merged, &chunk, &self.key, self.flags, e), usize::from(top) - 1)
        } else {
            // An empty chunk after input follows only a subtree's two children (`update`), so
            // the stack holds two at least.
            assert!(depth >= 2, "blaze_blake3::Hasher: an empty chunk over one subtree (a bug in `update`)");
            (parent_output(&self.stack[depth - 2], &self.stack[depth - 1], &self.key, self.flags, e), depth - 2)
        };
        for cv in self.stack[..left].iter().rev() {
            output = parent_output(cv, &output.chaining_value(), &self.key, self.flags, e);
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

/// The roots of `inputs` under `key` and `flags`, into `out`, by this CPU's engine.
fn many(key: &[u32; 8], flags: u32, inputs: &[&[u8]], out: &mut [[u8; OUT_LEN]]) {
    many_on(Engine::detected(), key, flags, inputs, out);
}

/// The roots of `inputs` under `key` and `flags`, into `out`, at the engine's degree: one at a
/// time at [`Degree::One`], else side by side ([`many_of`]).
pub(crate) fn many_on(e: Engine, key: &[u32; 8], flags: u32, inputs: &[&[u8]], out: &mut [[u8; OUT_LEN]]) {
    assert_eq!(inputs.len(), out.len(), "a digest for each input");
    match e.degree {
        Degree::One => {
            for (input, slot) in inputs.iter().zip(out.iter_mut()) {
                *slot = one_shot(e, key, flags, input);
            }
        }
        Degree::Two => many_lanes::<2>(e, key, flags, inputs, out),
        Degree::Four => many_lanes::<4>(e, key, flags, inputs, out),
        Degree::Eight => many_lanes::<8>(e, key, flags, inputs, out),
        Degree::Sixteen => many_lanes::<16>(e, key, flags, inputs, out),
    }
}

/// [`many_of`] at `L` lanes on the instructions the engine's [`Isa`] proves.
fn many_lanes<const L: usize>(e: Engine, key: &[u32; 8], flags: u32, inputs: &[&[u8]], out: &mut [[u8; OUT_LEN]]) {
    #[cfg(target_arch = "x86_64")]
    {
        x86::many::<L>(e, key, flags, inputs, out)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        many_of::<L>(e, key, flags, inputs, out)
    }
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
fn many_of<const N: usize>(e: Engine, key: &[u32; 8], flags: u32, inputs: &[&[u8]], out: &mut [[u8; OUT_LEN]]) {
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
                    out[next] = one_shot(e, key, flags, input);
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

/// The wide code compiled for AVX-512F (each word of the transposed state one 512-bit register
/// at 16 lanes, the rotates `vprord`) and for AVX2 (one 256-bit register a word at 8), where
/// the baseline would give them SSE2's 128-bit registers and three instructions a rotate; and
/// the row-vector single compression with AVX2's `vpshufb` for its byte rotations.
///
/// Every `unsafe` call below rests on one assumption, the [`Isa`] contract: an `Isa` of kind
/// `Avx2` is made only by [`Isa::avx2_unchecked`], whose caller promised the CPU has AVX2, or
/// by [`Isa::detected`] after [`crate::cpu::has`] said so; one of kind `Avx512` likewise for AVX-512F
/// and AVX2 both. A `#[target_feature]` function may run on such a CPU and on no other.
#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod x86 {
    use crate::engine::{Engine, Isa, IsaKind};
    use crate::{CHUNK_LEN, OUT_LEN, rows, tree};

    pub(crate) fn chunks<const L: usize>(
        isa: Isa,
        key: &[u32; 8],
        flags: u32,
        whole: &[[u8; CHUNK_LEN]],
        counter: u64,
        out: &mut [[u32; 8]],
    ) {
        match isa.kind {
            IsaKind::Baseline => tree::chunks_of::<L>(key, flags, whole, counter, out),
            // SAFETY: the `Isa` contract above: this CPU has AVX2.
            IsaKind::Avx2 => unsafe { chunks_avx2::<L>(key, flags, whole, counter, out) },
            // SAFETY: the `Isa` contract above: this CPU has AVX-512F.
            IsaKind::Avx512 => unsafe { chunks_avx512::<L>(key, flags, whole, counter, out) },
        }
    }

    pub(crate) fn parents<const L: usize>(
        isa: Isa,
        key: &[u32; 8],
        flags: u32,
        pairs: &[[[u32; 8]; 2]],
        out: &mut [[u32; 8]],
    ) {
        match isa.kind {
            IsaKind::Baseline => tree::parents_of::<L>(key, flags, pairs, out),
            // SAFETY: the `Isa` contract above: this CPU has AVX2.
            IsaKind::Avx2 => unsafe { parents_avx2::<L>(key, flags, pairs, out) },
            // SAFETY: the `Isa` contract above: this CPU has AVX-512F.
            IsaKind::Avx512 => unsafe { parents_avx512::<L>(key, flags, pairs, out) },
        }
    }

    pub(crate) fn many<const L: usize>(
        e: Engine,
        key: &[u32; 8],
        flags: u32,
        inputs: &[&[u8]],
        out: &mut [[u8; OUT_LEN]],
    ) {
        match e.isa.kind {
            IsaKind::Baseline => super::many_of::<L>(e, key, flags, inputs, out),
            // SAFETY: the `Isa` contract above: this CPU has AVX2.
            IsaKind::Avx2 => unsafe { many_avx2::<L>(e, key, flags, inputs, out) },
            // SAFETY: the `Isa` contract above: this CPU has AVX-512F.
            IsaKind::Avx512 => unsafe { many_avx512::<L>(e, key, flags, inputs, out) },
        }
    }

    pub(crate) fn rows(
        isa: Isa,
        cv: &[u32; 8],
        block: &[u32; 16],
        counter: u64,
        block_len: u32,
        flags: u32,
    ) -> [u32; 16] {
        match isa.kind {
            IsaKind::Baseline => rows::compress(cv, block, counter, block_len, flags),
            // SAFETY: the `Isa` contract above: this CPU has AVX2 (an `Avx512` one has it too).
            IsaKind::Avx2 | IsaKind::Avx512 => unsafe { rows_avx2(cv, block, counter, block_len, flags) },
        }
    }

    #[target_feature(enable = "avx2")]
    fn chunks_avx2<const L: usize>(key: &[u32; 8], flags: u32, whole: &[[u8; CHUNK_LEN]], counter: u64, out: &mut [[u32; 8]]) {
        tree::chunks_of::<L>(key, flags, whole, counter, out);
    }

    #[target_feature(enable = "avx512f")]
    fn chunks_avx512<const L: usize>(key: &[u32; 8], flags: u32, whole: &[[u8; CHUNK_LEN]], counter: u64, out: &mut [[u32; 8]]) {
        tree::chunks_of::<L>(key, flags, whole, counter, out);
    }

    #[target_feature(enable = "avx2")]
    fn parents_avx2<const L: usize>(key: &[u32; 8], flags: u32, pairs: &[[[u32; 8]; 2]], out: &mut [[u32; 8]]) {
        tree::parents_of::<L>(key, flags, pairs, out);
    }

    #[target_feature(enable = "avx512f")]
    fn parents_avx512<const L: usize>(key: &[u32; 8], flags: u32, pairs: &[[[u32; 8]; 2]], out: &mut [[u32; 8]]) {
        tree::parents_of::<L>(key, flags, pairs, out);
    }

    #[target_feature(enable = "avx2")]
    fn many_avx2<const L: usize>(e: Engine, key: &[u32; 8], flags: u32, inputs: &[&[u8]], out: &mut [[u8; OUT_LEN]]) {
        super::many_of::<L>(e, key, flags, inputs, out);
    }

    #[target_feature(enable = "avx512f")]
    fn many_avx512<const L: usize>(e: Engine, key: &[u32; 8], flags: u32, inputs: &[&[u8]], out: &mut [[u8; OUT_LEN]]) {
        super::many_of::<L>(e, key, flags, inputs, out);
    }

    #[target_feature(enable = "avx2")]
    fn rows_avx2(cv: &[u32; 8], block: &[u32; 16], counter: u64, block_len: u32, flags: u32) -> [u32; 16] {
        rows::compress(cv, block, counter, block_len, flags)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use blaze_sim::{Config, Tape, check};

    use super::{BLOCK_LEN, compress, compress_in_place, first_8, lanes};

    /// The in-place form is the first eight words of [`compress`] of the block's words: drawn
    /// chaining values, blocks, counters, lengths and flags.
    #[test]
    fn in_place_is_the_first_eight_words() {
        check(Config::cases(2_000), |tape: &Tape| {
            let cv: [u32; 8] = core::array::from_fn(|_| tape.next_u32("cv"));
            let mut block = [0; BLOCK_LEN];
            tape.fill(&mut block, "block");
            let counter = tape.next_u64("counter");
            let block_len = u32::try_from(tape.range(0..65, "block length")).expect("at most 64");
            let flags = u32::try_from(tape.below(128, "flags")).expect("under 128");
            let want = first_8(&compress(&cv, &lanes::block_words(&block), counter, block_len, flags));
            let mut got = cv;
            compress_in_place(&mut got, &block, counter, block_len, flags);
            assert_eq!(got, want);
        });
    }
}
