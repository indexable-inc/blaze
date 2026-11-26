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

/// A streaming hasher for one aligned subtree of a BLAKE3 Merkle tree, starting at chunk counter
/// `start_chunk`. Feeds arbitrary slices through the same wide SIMD subtree compressor as
/// [`Hasher`] and finishes either as a non-root 8-word chaining value
/// ([`SubtreeHasher::finalize_non_root`]) for parallel parent folding via [`finalize_subtrees`],
/// or as a root digest when `start_chunk == 0` and this subtree covers the entire message
/// ([`SubtreeHasher::finalize_root`]).
#[derive(Clone)]
pub struct SubtreeHasher {
    start_chunk: u64,
    chunk: ChunkState,
    stack: [[u32; 8]; MAX_DEPTH + 1],
    key: [u32; 8],
    flags: u32,
    engine: Engine,
    stack_len: u8,
}

impl SubtreeHasher {
    /// A subtree hasher initialized with explicit `key` words, `flags`, and starting chunk counter
    /// `start_chunk`, running on this CPU's detected [`Engine`].
    pub fn new(key: &[u32; 8], flags: u32, start_chunk: u64) -> SubtreeHasher {
        SubtreeHasher::with_engine(Engine::detected(), *key, flags, start_chunk)
    }

    /// A subtree hasher in BLAKE3's plain mode (`IV`, flags `0`) starting at `start_chunk`.
    pub fn new_subtree(start_chunk: u64) -> SubtreeHasher {
        SubtreeHasher::with_engine(Engine::detected(), IV, 0, start_chunk)
    }

    /// A subtree hasher in BLAKE3's keyed mode under `key` starting at `start_chunk`.
    pub fn new_keyed(key: &[u8; OUT_LEN], start_chunk: u64) -> SubtreeHasher {
        SubtreeHasher::with_engine(Engine::detected(), words_of(key), KEYED_HASH, start_chunk)
    }

    /// A subtree hasher in BLAKE3's key-derivation material mode under context key `key` starting
    /// at `start_chunk`.
    pub fn new_from_context_key(key: &[u8; OUT_LEN], start_chunk: u64) -> SubtreeHasher {
        SubtreeHasher::with_engine(Engine::detected(), words_of(key), DERIVE_KEY_MATERIAL, start_chunk)
    }

    /// A subtree hasher of `mode` starting at `start_chunk` on a handed `engine`.
    pub fn on(engine: Engine, mode: Mode, start_chunk: u64) -> SubtreeHasher {
        let (key, flags) = mode.key_and_flags();
        SubtreeHasher::with_engine(engine, key, flags, start_chunk)
    }

    fn with_engine(engine: Engine, key: [u32; 8], flags: u32, start_chunk: u64) -> SubtreeHasher {
        SubtreeHasher {
            start_chunk,
            chunk: ChunkState::new(&key, start_chunk, flags),
            stack: [[0; 8]; MAX_DEPTH + 1],
            key,
            flags,
            engine,
            stack_len: 0,
        }
    }

    /// Feeds `input` into this subtree.
    pub fn update(&mut self, mut input: &[u8]) -> &mut SubtreeHasher {
        if self.chunk.append(input) {
            return self;
        }
        let e = self.engine;
        if self.chunk.len() > 0 {
            let take = (CHUNK_LEN - self.chunk.len()).min(input.len());
            let Some(head) = input.get(..take) else {
                return self;
            };
            let Some(tail) = input.get(take..) else {
                return self;
            };
            self.chunk.update(e, head);
            input = tail;
            if input.is_empty() {
                return self;
            }
            let global_counter = self.chunk.counter;
            let local_counter = global_counter.wrapping_sub(self.start_chunk);
            let cv = self.chunk.output(e).chaining_value();
            self.push_cv(cv, local_counter);
            self.chunk = ChunkState::new(&self.key, global_counter.wrapping_add(1), self.flags);
        }
        while input.len() > CHUNK_LEN {
            let global_counter = self.chunk.counter;
            let local_counter = global_counter.wrapping_sub(self.start_chunk);
            let so_far = local_counter.wrapping_mul(CHUNK_LEN.widen_to::<u64>());
            let mut subtree = tree::largest_power_of_two_at_most(input.len());
            while subtree > CHUNK_LEN && (subtree - 1).widen_to::<u64>() & so_far != 0 {
                subtree /= 2;
            }
            let chunks = (subtree / CHUNK_LEN).widen_to::<u64>();
            if subtree <= CHUNK_LEN {
                let Some((chunk, rest)) = input.split_first_chunk::<CHUNK_LEN>() else {
                    break;
                };
                self.push_cv(tree::chunk_cv(e, &self.key, self.flags, chunk, global_counter), local_counter);
                self.chunk = ChunkState::new(&self.key, global_counter.wrapping_add(1), self.flags);
                input = rest;
            } else {
                let Some(head) = input.get(..subtree) else {
                    break;
                };
                let Some(rest) = input.get(subtree..) else {
                    break;
                };
                let [left, right] = tree::subtree_to_parent(e, &self.key, self.flags, head, global_counter);
                self.push_cv(left, local_counter);
                self.push_cv(right, local_counter.wrapping_add(chunks / 2));
                self.chunk = ChunkState::new(&self.key, global_counter.wrapping_add(chunks), self.flags);
                input = rest;
            }
        }
        self.chunk.update(e, input);
        self
    }

    fn push_cv(&mut self, cv: [u32; 8], local_counter: u64) {
        let keep = local_counter.count_ones();
        while u32::from(self.stack_len) > keep {
            let top = usize::from(self.stack_len);
            let Some(below) = top.checked_sub(2) else {
                break;
            };
            let (Some(left), Some(right)) = (self.stack.get(below), self.stack.get(top - 1)) else {
                break;
            };
            let merged = parent_output(left, right, &self.key, self.flags, self.engine).chaining_value();
            if let Some(slot) = self.stack.get_mut(below) {
                *slot = merged;
            }
            self.stack_len = self.stack_len.saturating_sub(1);
        }
        if let Some(slot) = self.stack.get_mut(usize::from(self.stack_len)) {
            *slot = cv;
            self.stack_len = self.stack_len.saturating_add(1);
        }
    }

    fn root(&self) -> Output {
        let e = self.engine;
        let depth = usize::from(self.stack_len);
        if depth == 0 {
            return self.chunk.output(e);
        }
        let (mut output, left) = if self.chunk.len() > 0 {
            let local_counter = self.chunk.counter.wrapping_sub(self.start_chunk);
            let keep = local_counter.count_ones();
            let mut top = self.stack_len;
            let mut merged = self.stack.get(depth.saturating_sub(1)).copied().unwrap_or(self.key);
            while u32::from(top) > keep {
                let Some(below) = usize::from(top).checked_sub(2) else {
                    break;
                };
                let Some(left_cv) = self.stack.get(below) else {
                    break;
                };
                merged = parent_output(left_cv, &merged, &self.key, self.flags, e).chaining_value();
                top = top.saturating_sub(1);
            }
            let chunk = self.chunk.output(e).chaining_value();
            (parent_output(&merged, &chunk, &self.key, self.flags, e), usize::from(top).saturating_sub(1))
        } else {
            let (Some(left_cv), Some(right_cv)) = (
                depth.checked_sub(2).and_then(|i| self.stack.get(i)),
                depth.checked_sub(1).and_then(|i| self.stack.get(i)),
            ) else {
                return self.chunk.output(e);
            };
            (parent_output(left_cv, right_cv, &self.key, self.flags, e), depth.saturating_sub(2))
        };
        if let Some(rest) = self.stack.get(..left) {
            for cv in rest.iter().rev() {
                output = parent_output(cv, &output.chaining_value(), &self.key, self.flags, e);
            }
        }
        output
    }

    /// Compresses this subtree's top node WITHOUT the `ROOT` flag, yielding its 8-word chaining
    /// value for parent-level folding via [`finalize_subtrees`].
    pub fn finalize_non_root(&self) -> [u32; 8] {
        self.root().chaining_value()
    }

    /// Compresses this subtree's top node WITH the `ROOT` flag (valid when `start_chunk == 0` and
    /// this subtree covers the entire message).
    pub fn finalize_root(&self) -> [u8; OUT_LEN] {
        first_32(&self.root().root_block(0))
    }
}

/// Folds the chaining values of aligned power-of-two subtrees (`stripe_chunks` chunks each, plus
/// an optional shorter final tail subtree) into the final 32-byte BLAKE3 digest in plain mode.
///
/// Requires `stripe_chunks.is_power_of_two()` and either zero subtrees (empty input) or at least
/// two subtrees (`full_cvs.len() + usize::from(tail_cv.is_some()) >= 2`); for a message that fits
/// inside a single stripe, use [`Hasher`] or [`SubtreeHasher::finalize_root`].
pub fn finalize_subtrees(stripe_chunks: u64, full_cvs: &[[u32; 8]], tail_cv: Option<[u32; 8]>) -> [u8; OUT_LEN] {
    finalize_subtrees_with(Engine::detected(), &IV, 0, stripe_chunks, full_cvs, tail_cv)
}

/// [`finalize_subtrees`] in keyed mode under `key`.
pub fn finalize_subtrees_keyed(
    key: &[u8; OUT_LEN],
    stripe_chunks: u64,
    full_cvs: &[[u32; 8]],
    tail_cv: Option<[u32; 8]>,
) -> [u8; OUT_LEN] {
    finalize_subtrees_with(Engine::detected(), &words_of(key), KEYED_HASH, stripe_chunks, full_cvs, tail_cv)
}

/// [`finalize_subtrees`] in key-derivation material mode under context key `key`.
pub fn finalize_subtrees_from_context_key(
    key: &[u8; OUT_LEN],
    stripe_chunks: u64,
    full_cvs: &[[u32; 8]],
    tail_cv: Option<[u32; 8]>,
) -> [u8; OUT_LEN] {
    finalize_subtrees_with(Engine::detected(), &words_of(key), DERIVE_KEY_MATERIAL, stripe_chunks, full_cvs, tail_cv)
}

/// [`finalize_subtrees`] for `mode` on a handed `engine`.
pub fn finalize_subtrees_on(
    engine: Engine,
    mode: Mode,
    stripe_chunks: u64,
    full_cvs: &[[u32; 8]],
    tail_cv: Option<[u32; 8]>,
) -> [u8; OUT_LEN] {
    let (key, flags) = mode.key_and_flags();
    finalize_subtrees_with(engine, &key, flags, stripe_chunks, full_cvs, tail_cv)
}

/// [`finalize_subtrees`] with explicit `engine`, `key` words, and `flags`.
pub fn finalize_subtrees_with(
    engine: Engine,
    key: &[u32; 8],
    flags: u32,
    stripe_chunks: u64,
    full_cvs: &[[u32; 8]],
    tail_cv: Option<[u32; 8]>,
) -> [u8; OUT_LEN] {
    let mut hasher = Hasher::with_key(engine, *key, flags);
    if full_cvs.is_empty() && tail_cv.is_none() {
        return first_32(&hasher.root().root_block(0));
    }
    let step = if stripe_chunks.is_power_of_two() { stripe_chunks } else { 1 };
    for (idx, &cv) in full_cvs.iter().enumerate() {
        hasher.push_cv(cv, idx.widen_to::<u64>().wrapping_mul(step));
    }
    if let Some(tail) = tail_cv {
        hasher.push_cv(tail, full_cvs.len().widen_to::<u64>().wrapping_mul(step));
    }
    if hasher.stack_len < 2 {
        let lone = hasher.stack.first().copied().unwrap_or(*key);
        return bytes_of(&lone);
    }
    first_32(&hasher.root().root_block(0))
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

    use super::{
        BLOCK_LEN, CHUNK_LEN, Hasher, SubtreeHasher, compress, compress_in_place, context_key, first_8,
        finalize_subtrees, finalize_subtrees_from_context_key, finalize_subtrees_keyed, lanes,
    };

    /// Deterministic byte value at stream offset `pos`, avoiding any heap buffer for multi-MB tests.
    fn pattern_byte(pos: usize) -> u8 {
        let mut x = (pos as u64).wrapping_add(1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
        ((x ^ (x >> 31)) >> 56) as u8
    }

    /// Fills `dst` with [`pattern_byte`] starting at logical stream offset `start`.
    fn fill_pattern(start: usize, dst: &mut [u8]) {
        for (i, byte) in dst.iter_mut().enumerate() {
            *byte = pattern_byte(start + i);
        }
    }

    /// Streams logical bytes `start..end` into `feed` in pieces cycling through `splits`
    /// (or 8 KiB chunks when `splits` is empty), using only a stack buffer.
    fn stream_pattern(start: usize, end: usize, splits: &[usize], mut feed: impl FnMut(&[u8])) {
        let mut buf = [0u8; 8192];
        let mut at = start;
        let mut step = 0usize;
        while at < end {
            let want = if splits.is_empty() {
                buf.len()
            } else {
                splits[step % splits.len()].max(1).min(buf.len())
            };
            let take = want.min(end - at);
            fill_pattern(at, &mut buf[..take]);
            feed(&buf[..take]);
            at += take;
            step += 1;
        }
    }

    fn parallel_hash_pattern_plain(len: usize, stripe_chunks: u64, splits: &[usize]) -> [u8; 32] {
        let stripe_bytes = usize::try_from(stripe_chunks).expect("stripe fits") * CHUNK_LEN;
        if len == 0 {
            return finalize_subtrees(stripe_chunks, &[], None);
        }
        if len <= stripe_bytes {
            let mut h = SubtreeHasher::new_subtree(0);
            stream_pattern(0, len, splits, |chunk| {
                h.update(chunk);
            });
            return h.finalize_root();
        }
        let full_count = len / stripe_bytes;
        let mut full_cvs = [[0u32; 8]; 2048];
        assert!(full_count <= full_cvs.len(), "test stack cap");
        for (i, slot) in full_cvs[..full_count].iter_mut().enumerate() {
            let lo = i * stripe_bytes;
            let hi = lo + stripe_bytes;
            let mut h = SubtreeHasher::new_subtree(u64::try_from(i).expect("i fits") * stripe_chunks);
            stream_pattern(lo, hi, splits, |chunk| {
                h.update(chunk);
            });
            *slot = h.finalize_non_root();
        }
        let rem_lo = full_count * stripe_bytes;
        let tail_cv = if rem_lo < len {
            let mut h = SubtreeHasher::new_subtree(u64::try_from(full_count).expect("n fits") * stripe_chunks);
            stream_pattern(rem_lo, len, splits, |chunk| {
                h.update(chunk);
            });
            Some(h.finalize_non_root())
        } else {
            None
        };
        finalize_subtrees(stripe_chunks, &full_cvs[..full_count], tail_cv)
    }

    fn feed_slice_split(hasher: &mut SubtreeHasher, data: &[u8], splits: &[usize]) {
        if splits.is_empty() {
            hasher.update(data);
            return;
        }
        let mut at = 0usize;
        let mut step = 0usize;
        while at < data.len() {
            let take = splits[step % splits.len()].max(1).min(data.len() - at);
            hasher.update(&data[at..at + take]);
            at += take;
            step += 1;
        }
    }

    fn parallel_hash_slice_plain(data: &[u8], stripe_chunks: u64, splits: &[usize]) -> [u8; 32] {
        let stripe_bytes = usize::try_from(stripe_chunks).expect("stripe fits") * CHUNK_LEN;
        if data.is_empty() {
            return finalize_subtrees(stripe_chunks, &[], None);
        }
        if data.len() <= stripe_bytes {
            let mut h = SubtreeHasher::new_subtree(0);
            feed_slice_split(&mut h, data, splits);
            return h.finalize_root();
        }
        let full_count = data.len() / stripe_bytes;
        let mut full_cvs = [[0u32; 8]; 64];
        assert!(full_count <= full_cvs.len(), "slice stack cap");
        for (i, slot) in full_cvs[..full_count].iter_mut().enumerate() {
            let lo = i * stripe_bytes;
            let hi = lo + stripe_bytes;
            let mut h = SubtreeHasher::new_subtree(u64::try_from(i).expect("i fits") * stripe_chunks);
            feed_slice_split(&mut h, &data[lo..hi], splits);
            *slot = h.finalize_non_root();
        }
        let rem_lo = full_count * stripe_bytes;
        let tail_cv = if rem_lo < data.len() {
            let mut h = SubtreeHasher::new_subtree(u64::try_from(full_count).expect("n fits") * stripe_chunks);
            feed_slice_split(&mut h, &data[rem_lo..], splits);
            Some(h.finalize_non_root())
        } else {
            None
        };
        finalize_subtrees(stripe_chunks, &full_cvs[..full_count], tail_cv)
    }

    /// `SubtreeHasher` + `finalize_subtrees` matches both `blaze_blake3::Hasher` and the
    /// official `blake3` crate bit-for-bit across critical lengths, power-of-two stripe widths,
    /// and arbitrary odd streaming splits.
    #[test]
    fn subtree_hasher_matches_reference_across_lengths_stripes_and_splits() {
        const LENGTHS: &[usize] = &[
            0, 1, 1023, 1024, 1025, 65_535, 65_536, 65_537, 131_072, 196_608, 500_000, 1_500_000,
        ];
        const STRIPES: &[u64] = &[1, 2, 4, 8, 16, 64];
        const ODD_SPLITS: &[&[usize]] = &[&[], &[1, 7, 63, 64, 65, 333, 1023, 1025, 4099]];

        for &len in LENGTHS {
            let mut serial = Hasher::new();
            let mut reference = blake3::Hasher::new();
            stream_pattern(0, len, &[], |chunk| {
                serial.update(chunk);
                reference.update(chunk);
            });
            let want_serial = serial.finalize();
            let want_ref = *reference.finalize().as_bytes();
            assert_eq!(want_serial, want_ref, "serial mismatch at len={len}");

            for &stripe_chunks in STRIPES {
                for &splits in ODD_SPLITS {
                    if len > 196_608 && !splits.is_empty() && stripe_chunks < 64 {
                        continue;
                    }
                    let got = parallel_hash_pattern_plain(len, stripe_chunks, splits);
                    assert_eq!(
                        got, want_ref,
                        "subtree plain mismatch at len={len}, stripe_chunks={stripe_chunks}, splits={splits:?}"
                    );
                }
            }
        }
    }

    /// `SubtreeHasher` + `finalize_subtrees_from_context_key` and `finalize_subtrees_keyed` match
    /// official `blake3` derive-key and keyed modes bit-for-bit.
    #[test]
    fn subtree_hasher_matches_derive_key_and_keyed_modes() {
        let domain = "blaze plain store segment 1";
        let ctx_key = context_key(domain);
        let mac_key: [u8; 32] = core::array::from_fn(|i| (i as u8).wrapping_mul(13).wrapping_add(7));
        let splits = [17usize, 64, 511, 1024, 2049, 8193];

        for &len in &[0usize, 1, 1024, 1025, 65_536, 131_072, 196_609, 300_000] {
            let mut ref_dk = blake3::Hasher::new_derive_key(domain);
            let mut ref_mac = blake3::Hasher::new_keyed(&mac_key);
            stream_pattern(0, len, &[], |chunk| {
                ref_dk.update(chunk);
                ref_mac.update(chunk);
            });
            let want_dk = *ref_dk.finalize().as_bytes();
            let want_mac = *ref_mac.finalize().as_bytes();

            for &stripe_chunks in &[1u64, 4, 16, 64] {
                let stripe_bytes = usize::try_from(stripe_chunks).expect("fit") * CHUNK_LEN;
                let (got_dk, got_mac) = if len == 0 {
                    (
                        finalize_subtrees_from_context_key(&ctx_key, stripe_chunks, &[], None),
                        finalize_subtrees_keyed(&mac_key, stripe_chunks, &[], None),
                    )
                } else if len <= stripe_bytes {
                    let mut h_dk = SubtreeHasher::new_from_context_key(&ctx_key, 0);
                    let mut h_mac = SubtreeHasher::new_keyed(&mac_key, 0);
                    stream_pattern(0, len, &splits, |chunk| {
                        h_dk.update(chunk);
                        h_mac.update(chunk);
                    });
                    (h_dk.finalize_root(), h_mac.finalize_root())
                } else {
                    let full = len / stripe_bytes;
                    let mut dk_cvs = [[0u32; 8]; 512];
                    let mut mac_cvs = [[0u32; 8]; 512];
                    assert!(full <= dk_cvs.len());
                    for i in 0..full {
                        let lo = i * stripe_bytes;
                        let hi = lo + stripe_bytes;
                        let sc = u64::try_from(i).expect("fit") * stripe_chunks;
                        let mut h_dk = SubtreeHasher::new_from_context_key(&ctx_key, sc);
                        let mut h_mac = SubtreeHasher::new_keyed(&mac_key, sc);
                        stream_pattern(lo, hi, &splits, |chunk| {
                            h_dk.update(chunk);
                            h_mac.update(chunk);
                        });
                        dk_cvs[i] = h_dk.finalize_non_root();
                        mac_cvs[i] = h_mac.finalize_non_root();
                    }
                    let rem = full * stripe_bytes;
                    let (dk_tail, mac_tail) = if rem < len {
                        let sc = u64::try_from(full).expect("fit") * stripe_chunks;
                        let mut h_dk = SubtreeHasher::new_from_context_key(&ctx_key, sc);
                        let mut h_mac = SubtreeHasher::new_keyed(&mac_key, sc);
                        stream_pattern(rem, len, &splits, |chunk| {
                            h_dk.update(chunk);
                            h_mac.update(chunk);
                        });
                        (Some(h_dk.finalize_non_root()), Some(h_mac.finalize_non_root()))
                    } else {
                        (None, None)
                    };
                    (
                        finalize_subtrees_from_context_key(&ctx_key, stripe_chunks, &dk_cvs[..full], dk_tail),
                        finalize_subtrees_keyed(&mac_key, stripe_chunks, &mac_cvs[..full], mac_tail),
                    )
                };
                assert_eq!(got_dk, want_dk, "derive-key mismatch len={len} stripe={stripe_chunks}");
                assert_eq!(got_mac, want_mac, "keyed mismatch len={len} stripe={stripe_chunks}");
            }
        }
    }

    /// Property test: arbitrary drawn lengths, power-of-two stripe widths, and random streaming
    /// chunk splits match official `blake3::hash` bit-for-bit.
    #[test]
    fn subtree_hasher_property_matches_blake3() {
        check(Config::cases(200), |tape: &Tape| {
            let len = usize::try_from(tape.range(0..20_000, "len")).expect("len fits");
            let mut data = [0u8; 20_000];
            tape.fill(&mut data[..len], "data");
            let shift = tape.range(0..5, "stripe_shift");
            let stripe_chunks = 1u64 << shift;
            let n_splits = usize::try_from(tape.range(1..6, "n_splits")).expect("n_splits fits");
            let mut splits = [1usize; 6];
            for slot in &mut splits[..n_splits] {
                *slot = usize::try_from(tape.range(1..2049, "split")).expect("split fits");
            }
            let got = parallel_hash_slice_plain(&data[..len], stripe_chunks, &splits[..n_splits]);
            let want = *blake3::hash(&data[..len]).as_bytes();
            assert_eq!(got, want, "len={len} stripe_chunks={stripe_chunks} splits={:?}", &splits[..n_splits]);
        });
    }

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
