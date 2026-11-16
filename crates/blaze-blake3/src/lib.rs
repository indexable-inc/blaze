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
//! hashes the batch's chunks side by side, a SIMD lane each ([`lanes::compress`], safe code that
//! LLVM vectorizes): 16 chunks in AVX-512 registers where the CPU has them, 8 in AVX2 ones, and
//! 8 at the target's baseline elsewhere (two NEON registers a word on AArch64). Parent nodes,
//! the last chunk and inputs of one chunk are compressed one block at a time. Which path runs is
//! dispatch on the CPU, not a fallback: every path computes the same bytes, held to the official
//! test vectors and to the `blake3` crate (`tests/`).

#![feature(portable_simd)]
#![deny(unsafe_code)]

pub mod lanes;

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

/// The most chaining values a hasher holds: one per level of a tree of 2^54 chunks, 2^64 bytes.
const MAX_DEPTH: usize = 54;

/// The most chunks hashed side by side (AVX-512's 16 lanes).
const MOST_LANES: usize = 16;

/// The hash of `input`, BLAKE3's plain mode.
pub fn hash(input: &[u8]) -> [u8; OUT_LEN] {
    one_shot(&IV, 0, input)
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
        let mut chunk = ChunkState::new(key, 0, flags);
        chunk.update(input);
        return first_32(&chunk.output().root_block(0));
    }
    let mut hasher = Hasher::with_key(*key, flags);
    hasher.update(input);
    hasher.finalize()
}

/// `bytes`' eight little-endian words.
fn words_of(bytes: &[u8; OUT_LEN]) -> [u32; 8] {
    std::array::from_fn(|word| {
        u32::from_le_bytes([
            bytes[4 * word],
            bytes[4 * word + 1],
            bytes[4 * word + 2],
            bytes[4 * word + 3],
        ])
    })
}

fn first_32(block: &[u8; BLOCK_LEN]) -> [u8; OUT_LEN] {
    std::array::from_fn(|byte| block[byte])
}

fn first_8(words: &[u32; 16]) -> [u32; 8] {
    std::array::from_fn(|word| words[word])
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
        counter as u32,
        (counter >> 32) as u32,
        block_len,
        flags,
    ];
    let mut m = *block;
    for round in 0..7 {
        g(&mut state, [0, 4, 8, 12], m[0], m[1]);
        g(&mut state, [1, 5, 9, 13], m[2], m[3]);
        g(&mut state, [2, 6, 10, 14], m[4], m[5]);
        g(&mut state, [3, 7, 11, 15], m[6], m[7]);
        g(&mut state, [0, 5, 10, 15], m[8], m[9]);
        g(&mut state, [1, 6, 11, 12], m[10], m[11]);
        g(&mut state, [2, 7, 8, 13], m[12], m[13]);
        g(&mut state, [3, 4, 9, 14], m[14], m[15]);
        if round < 6 {
            m = SCHEDULE.map(|word| m[word]);
        }
    }
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
    Output { input_cv: *key, block, counter: 0, block_len: BLOCK_LEN as u32, flags: PARENT | flags }
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
        BLOCK_LEN * self.blocks_compressed as usize + self.block_len as usize
    }

    fn start_flag(&self) -> u32 {
        if self.blocks_compressed == 0 { CHUNK_START } else { 0 }
    }

    /// Feeds `input`; the caller gives a chunk at most [`CHUNK_LEN`] bytes in all.
    fn update(&mut self, mut input: &[u8]) {
        while !input.is_empty() {
            if self.block_len as usize == BLOCK_LEN {
                let words = lanes::block_words(&self.block);
                let flags = self.flags | self.start_flag();
                let out = compress(&self.cv, &words, self.counter, BLOCK_LEN as u32, flags);
                self.cv = first_8(&out);
                self.blocks_compressed += 1;
                self.block = [0; BLOCK_LEN];
                self.block_len = 0;
            }
            let take = (BLOCK_LEN - self.block_len as usize).min(input.len());
            self.block[self.block_len as usize..][..take].copy_from_slice(&input[..take]);
            self.block_len += take as u8;
            input = &input[take..];
        }
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
        self.chunk.counter * CHUNK_LEN as u64 + self.chunk.len() as u64
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
            let left = self.stack[self.stack_len as usize];
            cv = parent_output(&left, &cv, &self.key, self.flags).chaining_value();
            total >>= 1;
        }
        self.stack[self.stack_len as usize] = cv;
        self.stack_len += 1;
    }

    /// The root node: the current chunk folded with every subtree left of it, right to left.
    fn root(&self) -> Output {
        let mut output = self.chunk.output();
        for left in self.stack[..self.stack_len as usize].iter().rev() {
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
            let block = self.root.root_block(self.position / BLOCK_LEN as u64);
            let offset = (self.position % BLOCK_LEN as u64) as usize;
            let take = (BLOCK_LEN - offset).min(out.len());
            let (head, rest) = std::mem::take(&mut out).split_at_mut(take);
            head.copy_from_slice(&block[offset..offset + take]);
            out = rest;
            self.position += take as u64;
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

/// Chunks a batch hashes side by side on this CPU: 16 where it has AVX-512F, else 8.
fn lanes_for_this_cpu() -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx512f") {
            return 16;
        }
    }
    8
}

/// The chaining values of `out.len()` whole chunks (`input`, `out.len() * CHUNK_LEN` bytes),
/// numbered from `counter`, side by side: the widths [`lanes_for_this_cpu`] gives.
fn hash_chunks(key: &[u32; 8], input: &[u8], counter: u64, flags: u32, out: &mut [[u32; 8]]) {
    #[cfg(target_arch = "x86_64")]
    {
        if out.len() == 16 {
            return x86::chunks_avx512(key, input, counter, flags, out);
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            return x86::chunks_avx2(key, input, counter, flags, out);
        }
    }
    chunks_of::<8>(key, input, counter, flags, out);
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
    let counter_low: [u32; N] = std::array::from_fn(|lane| (counter + lane as u64) as u32);
    let counter_high: [u32; N] =
        std::array::from_fn(|lane| ((counter + lane as u64) >> 32) as u32);
    let blocks = CHUNK_LEN / BLOCK_LEN;
    for block in 0..blocks {
        let rows: [[u32; 16]; N] = std::array::from_fn(|lane| {
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
            [BLOCK_LEN as u32; N],
            block_flags,
        );
    }
    for (lane, slot) in out.iter_mut().enumerate() {
        *slot = std::array::from_fn(|word| cv[word][lane]);
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
        assert!(
            std::arch::is_x86_feature_detected!("avx512f"),
            "AVX-512F lanes on a CPU without it"
        );
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
        assert!(std::arch::is_x86_feature_detected!("avx2"), "AVX2 lanes on a CPU without it");
        // SAFETY: a `target_feature` function may run only on a CPU with the feature, and this
        // one has AVX2, asserted just above.
        unsafe { chunks_narrow(key, input, counter, flags, out) }
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
