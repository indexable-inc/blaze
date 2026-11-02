//! The Hasher's stack of chaining values, its loop over the input, a
//! chunk's state, and a node's output, in safe Rust over their kernels.

use crate::{BLOCK_LEN, CHUNK_END, CHUNK_LEN, CHUNK_START, PARENT, ROOT};
use crate::{CVBytes as Cv, MAX_DEPTH};
const DEPTH: usize = MAX_DEPTH + 1;

/// The parent of two chaining values, the stack's one use of the kernels.
pub trait Parent {
    fn parent(&self, left: &Cv, right: &Cv) -> Cv;
}

/// Chaining values bottom first: an array made at the first push (a message
/// of one chunk pushes none) and its length.
pub struct CvStack {
    cvs: Option<[Cv; DEPTH]>,
    len: usize,
}

impl Clone for CvStack {
    fn clone(&self) -> CvStack {
        CvStack { cvs: self.cvs, len: self.len }
    }
}

impl CvStack {
    pub fn new() -> CvStack {
        CvStack { cvs: None, len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Value `i`, below the length.
    pub fn get(&self, i: usize) -> Cv {
        assert!(i < self.len, "a value below the stack's length");
        match &self.cvs {
            Some(cvs) => cvs[i],
            None => panic!("a stack with values has its array"),
        }
    }

    /// `cv` on top; the stack holds fewer than DEPTH values.
    pub fn push(&mut self, cv: Cv) {
        if self.cvs.is_none() {
            self.cvs = Some([[0; 32]; DEPTH]);
        }
        if let Some(cvs) = &mut self.cvs {
            cvs[self.len] = cv;
        }
        self.len += 1;
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// While the stack holds more than `target` values, its top two become
    /// their parent (merge_cv_stack). Two are there to merge whenever
    /// `target` is at least 1, as the Hasher's are.
    /// A loop of a count fixed at its start (no condition a change could
    /// keep true forever: a mutation test's endless loop, which only a clock
    /// stops, becomes a wrong value a test catches).
    pub fn merge<P: Parent>(&mut self, p: &P, target: usize) {
        for _ in target..self.len {
            assert!(self.len >= 2, "two values to merge");
            if let Some(cvs) = &mut self.cvs {
                let parent = p.parent(&cvs[self.len - 2], &cvs[self.len - 1]);
                cvs[self.len - 2] = parent;
            }
            self.len -= 1;
        }
    }
}

/// A chunk's state: its number and the bytes it has so far (at most
/// CHUNK_LEN).
pub trait Chunk {
    fn counter(&self) -> u64;
    fn len(&self) -> usize;
    /// The next bytes, at most CHUNK_LEN - len() of them.
    fn update(&mut self, input: &[u8]);
    /// An empty chunk's number, advanced by `chunks`.
    fn advance(&mut self, chunks: u64);
}

/// The kernels the Hasher's loop calls, beyond the stack's `Parent`.
pub trait Feed<C: Chunk>: Parent {
    /// An empty chunk, numbered `counter`.
    fn new_chunk(&self, counter: u64) -> C;
    /// A full chunk's chaining value.
    fn chunk_cv(&self, chunk: &C) -> Cv;
    /// The chaining value of `chunk` (CHUNK_LEN bytes), numbered `counter`.
    fn whole_chunk_cv(&self, chunk: &[u8], counter: u64) -> Cv;
    /// The chaining value of the subtree over `input` (more than one chunk),
    /// its first chunk numbered `counter`, computed another way where the
    /// kernels have one (None: they have none for it). `ahead` bytes follow.
    fn subtree_cv(&self, input: &[u8], ahead: usize, counter: u64) -> Option<Cv>;
    /// The chaining values of the two children of the subtree over `input`
    /// (more than one chunk), its first chunk numbered `counter`.
    fn children(&self, input: &[u8], ahead: usize, counter: u64) -> [Cv; 2];
}

/// The nodes a Hasher's output is computed from.
pub trait Nodes<C: Chunk, N> {
    /// The node of `chunk`, the last.
    fn chunk_node(&self, chunk: &C) -> N;
    /// The parent node of two chaining values.
    fn parent_node(&self, left: &Cv, right: &Cv) -> N;
    /// A node's chaining value.
    fn node_cv(&self, node: &N) -> Cv;
}

/// The number of 1-bits of `x` (its popcount, which the loop this replaced
/// compiled to anyway; with no loop, no change to it can run forever).
pub fn ones(x: u64) -> usize {
    x.count_ones() as usize
}

/// A whole subtree's length to hash next, from chunk `counter` with
/// `remaining` (more than CHUNK_LEN) bytes to go: the largest power of two
/// at most `remaining` that divides the bytes so far, CHUNK_LEN times a
/// power of two dividing `counter`.
/// A power of two divides `so_far` exactly when it is at most its lowest
/// 1-bit, so the length is the smaller of the two, with no loop (one that
/// halved until it divided could, changed, run forever).
pub fn subtree_len(counter: u64, remaining: usize) -> usize {
    let len = 1usize << (usize::BITS - 1 - remaining.leading_zeros());
    let so_far = counter * CHUNK_LEN as u64;
    if so_far == 0 {
        len
    } else {
        // The lowest 1-bit as a mask, compared in u64: no shift to bound,
        // and the result is at most `len`, so it fits a usize.
        (len as u64).min(so_far & so_far.wrapping_neg()) as usize
    }
}

/// `cv`, the subtree from chunk `counter`, onto the stack: the stack's
/// values merged to the subtrees before it first (lazily: the top value
/// may still merge with the next).
pub fn push<C: Chunk, K: Feed<C>>(k: &K, stack: &mut CvStack, initial: u64, cv: Cv, counter: u64) {
    stack.merge(k, ones(counter - initial));
    stack.push(cv);
}

/// The partial chunk filled from `input`; when more input follows, the full
/// chunk goes onto the stack and a new chunk begins. Returns the rest.
pub fn fill<'a, C: Chunk, K: Feed<C>>(k: &K, chunk: &mut C, stack: &mut CvStack, initial: u64, input: &'a [u8]) -> &'a [u8] {
    if chunk.len() == 0 {
        return input;
    }
    let take = if CHUNK_LEN - chunk.len() < input.len() { CHUNK_LEN - chunk.len() } else { input.len() };
    chunk.update(&input[..take]);
    let rest = &input[take..];
    if rest.is_empty() {
        return rest;
    }
    let counter = chunk.counter();
    push(k, stack, initial, k.chunk_cv(chunk), counter);
    *chunk = k.new_chunk(counter + 1);
    rest
}

/// `input` after an empty chunk: whole subtrees onto the stack, then at most
/// a chunk into the chunk, and the stack merged behind it.
pub fn feed<C: Chunk, K: Feed<C>>(k: &K, chunk: &mut C, stack: &mut CvStack, initial: u64, input: &[u8]) {
    let mut input = input;
    while input.len() > CHUNK_LEN {
        let counter = chunk.counter();
        let len = subtree_len(counter, input.len());
        let chunks = (len / CHUNK_LEN) as u64;
        let ahead = input.len() - len;
        let sub = &input[..len];
        let flat = if counter > initial { k.subtree_cv(sub, ahead, counter) } else { None };
        match flat {
            Some(cv) => push(k, stack, initial, cv, counter),
            None => {
                if len <= CHUNK_LEN {
                    push(k, stack, initial, k.whole_chunk_cv(sub, counter), counter);
                } else {
                    let [left, right] = k.children(sub, ahead, counter);
                    push(k, stack, initial, left, counter);
                    push(k, stack, initial, right, counter + chunks / 2);
                }
            }
        }
        chunk.advance(chunks);
        input = &input[len..];
    }
    if !input.is_empty() {
        chunk.update(input);
        stack.merge(k, ones(chunk.counter() - initial));
    }
}

/// The root's node: the last chunk's (or the top two values' parent),
/// folded into the stack's values from the top.
pub fn final_node<C: Chunk, N, K: Nodes<C, N>>(k: &K, chunk: &C, stack: &CvStack) -> N {
    if stack.len() == 0 {
        return k.chunk_node(chunk);
    }
    let mut n = stack.len();
    let mut out = if chunk.len() > 0 {
        k.chunk_node(chunk)
    } else {
        n -= 2;
        k.parent_node(&stack.get(n), &stack.get(n + 1))
    };
    while n > 0 {
        n -= 1;
        out = k.parent_node(&stack.get(n), &k.node_cv(&out));
    }
    out
}

/// What the Hasher's kernels take from the library beyond the compression
/// function: Feed's methods of those names, as Feed describes them.
pub trait Hooks {
    fn subtree_cv(&self, input: &[u8], ahead: usize, counter: u64) -> Option<Cv>;
    fn children(&self, input: &[u8], ahead: usize, counter: u64) -> [Cv; 2];
}

/// The Hasher's kernels: chunks, nodes, and parents on `key`, `flags`, and
/// `platform`, and the rest from `hooks`.
pub struct Kernels<'a, P, H> {
    pub key: &'a [u32; 8],
    pub flags: u8,
    pub platform: P,
    pub hooks: H,
}

impl<P: Blocks + Copy, H> Parent for Kernels<'_, P, H> {
    #[inline(always)]
    fn parent(&self, left: &Cv, right: &Cv) -> Cv {
        parent_output(left, right, self.key, self.flags, self.platform).chaining_value()
    }
}

impl<P: Blocks + Copy, H: Hooks> Feed<ChunkState<P>> for Kernels<'_, P, H> {
    #[inline(always)]
    fn new_chunk(&self, counter: u64) -> ChunkState<P> {
        ChunkState::new(self.key, counter, self.flags, self.platform)
    }

    #[inline(always)]
    fn chunk_cv(&self, chunk: &ChunkState<P>) -> Cv {
        chunk.output().chaining_value()
    }

    #[inline(always)]
    fn whole_chunk_cv(&self, chunk: &[u8], counter: u64) -> Cv {
        let mut state = ChunkState::new(self.key, counter, self.flags, self.platform);
        state.absorb(chunk);
        state.output().chaining_value()
    }

    #[inline(always)]
    fn subtree_cv(&self, input: &[u8], ahead: usize, counter: u64) -> Option<Cv> {
        self.hooks.subtree_cv(input, ahead, counter)
    }

    #[inline(always)]
    fn children(&self, input: &[u8], ahead: usize, counter: u64) -> [Cv; 2] {
        self.hooks.children(input, ahead, counter)
    }
}

impl<P: Blocks + Copy, H> Nodes<ChunkState<P>, Output<P>> for Kernels<'_, P, H> {
    #[inline(always)]
    fn chunk_node(&self, chunk: &ChunkState<P>) -> Output<P> {
        chunk.output()
    }

    #[inline(always)]
    fn parent_node(&self, left: &Cv, right: &Cv) -> Output<P> {
        parent_output(left, right, self.key, self.flags, self.platform)
    }

    #[inline(always)]
    fn node_cv(&self, node: &Output<P>) -> Cv {
        node.chaining_value()
    }
}

/// The compression kernels a chunk's state calls.
pub trait Blocks {
    /// `cv` compressed with `block`, its first `block_len` bytes the
    /// message's and the rest zero.
    fn compress_in_place(&self, cv: &mut [u32; 8], block: &[u8; BLOCK_LEN], block_len: u8, counter: u64, flags: u8);
    /// `cv` compressed with the `count` (1 to 16) whole blocks at the start
    /// of `blocks` in turn: the first with `flags | flags_start`, the
    /// others with `flags`.
    fn compress_blocks(&self, cv: &mut [u32; 8], blocks: &[u8], count: usize, counter: u64, flags: u8, flags_start: u8);
}

/// A chunk's state: the chaining value after its blocks so far but the
/// last, which waits in `buf` (zero past `buf_len`) for the output. `buf`
/// comes first and the state is 64-byte aligned, as Output's block (below)
/// explains.
#[repr(C, align(64))]
pub struct ChunkState<P> {
    pub buf: [u8; BLOCK_LEN],
    pub cv: [u32; 8],
    pub chunk_counter: u64,
    pub buf_len: u8,
    pub blocks_compressed: u8,
    pub flags: u8,
    pub platform: P,
}

impl<P: Blocks> ChunkState<P> {
    pub fn new(key: &[u32; 8], chunk_counter: u64, flags: u8, platform: P) -> Self {
        ChunkState { buf: [0; BLOCK_LEN], cv: *key, chunk_counter, buf_len: 0, blocks_compressed: 0, flags, platform }
    }

    /// The bytes so far.
    pub fn count(&self) -> usize {
        BLOCK_LEN * self.blocks_compressed as usize + self.buf_len as usize
    }

    /// As much of `input` as `buf` has room for, into it; returns how much.
    fn fill_buf(&mut self, input: &[u8]) -> usize {
        let want = BLOCK_LEN - self.buf_len as usize;
        let take = if want < input.len() { want } else { input.len() };
        let start = self.buf_len as usize;
        self.buf[start..start + take].copy_from_slice(&input[..take]);
        self.buf_len += take as u8;
        take
    }

    pub fn start_flag(&self) -> u8 {
        if self.blocks_compressed == 0 { CHUNK_START } else { 0 }
    }

    /// The next bytes, `count() + input.len()` at most CHUNK_LEN. Whole
    /// blocks compress straight from `input`; the last block always waits.
    pub fn absorb(&mut self, input: &[u8]) {
        let mut input = input;
        if self.buf_len > 0 {
            let take = self.fill_buf(input);
            input = &input[take..];
            if !input.is_empty() {
                let block_flags = self.flags | self.start_flag();
                self.platform.compress_in_place(&mut self.cv, &self.buf, BLOCK_LEN as u8, self.chunk_counter, block_flags);
                self.buf_len = 0;
                self.buf = [0; BLOCK_LEN];
                self.blocks_compressed += 1;
            }
        }
        if input.len() > BLOCK_LEN {
            let count = (input.len() - 1) / BLOCK_LEN;
            let start_flag = self.start_flag();
            self.platform.compress_blocks(&mut self.cv, input, count, self.chunk_counter, self.flags, start_flag);
            self.blocks_compressed += count as u8;
            input = &input[count * BLOCK_LEN..];
        }
        self.fill_buf(input);
    }
}

/// A node: the inputs of its last compression. `block` comes first and the
/// node is 64-byte aligned: glibc's AVX-512 memcpy writes a 64-byte copy
/// with a pair of overlapping unaligned 64-byte vector stores, and the
/// immediate reload of the destination then fails store-to-load forwarding
/// whenever the stack frame leaves it misaligned (address & 31 != 0). Stack
/// layout decides which case each process gets, so the same binary hashed
/// 64-byte inputs 35-45% slower in some processes than in others (Zen 5).
/// Aligning the block pins every process to the forwarding-friendly case
/// (https://github.com/zooko/bench-hashes/issues/2,
/// https://github.com/BLAKE3-team/BLAKE3/pull/582).
#[repr(C, align(64))]
pub struct Output<P> {
    pub block: [u8; BLOCK_LEN],
    pub input_chaining_value: [u32; 8],
    pub block_len: u8,
    pub counter: u64,
    pub flags: u8,
    pub platform: P,
}

/// The bytes of `words`, least significant first.
pub fn le_bytes(words: &[u32; 8]) -> [u8; 32] {
    // Each word into its four bytes' slot: no index or count to get wrong
    // (a counted loop, changed, could run forever).
    let mut out = [0u8; 32];
    for (bytes, word) in out.chunks_exact_mut(4).zip(words) {
        bytes.copy_from_slice(&word.to_le_bytes());
    }
    out
}

impl<P: Blocks> Output<P> {
    /// The node's chaining value.
    pub fn chaining_value(&self) -> [u8; 32] {
        let mut cv = self.input_chaining_value;
        self.platform.compress_in_place(&mut cv, &self.block, self.block_len, self.counter, self.flags);
        le_bytes(&cv)
    }

    /// The first 32 bytes of the root's output (the node the root, its
    /// counter 0).
    #[inline]
    pub fn root_bytes(&self) -> [u8; 32] {
        let mut cv = self.input_chaining_value;
        self.platform.compress_in_place(&mut cv, &self.block, self.block_len, 0, self.flags | ROOT);
        le_bytes(&cv)
    }
}

impl<P: Blocks + Copy> ChunkState<P> {
    /// The chunk's node.
    pub fn output(&self) -> Output<P> {
        Output {
            block: self.buf,
            input_chaining_value: self.cv,
            block_len: self.buf_len,
            counter: self.chunk_counter,
            flags: self.flags | self.start_flag() | CHUNK_END,
            platform: self.platform,
        }
    }
}

/// The parent node of two chaining values.
pub fn parent_output<P>(left: &[u8; 32], right: &[u8; 32], key: &[u32; 8], flags: u8, platform: P) -> Output<P> {
    let mut block = [0u8; BLOCK_LEN];
    block[..32].copy_from_slice(left);
    block[32..].copy_from_slice(right);
    Output { block, input_chaining_value: *key, block_len: BLOCK_LEN as u8, counter: 0, flags: flags | PARENT, platform }
}

/// The chunk state the Hasher's loop (above) works with.
impl<P: Blocks> Chunk for ChunkState<P> {
    #[inline(always)]
    fn counter(&self) -> u64 {
        self.chunk_counter
    }

    #[inline(always)]
    fn len(&self) -> usize {
        self.count()
    }

    #[inline(always)]
    fn update(&mut self, input: &[u8]) {
        self.absorb(input)
    }

    #[inline(always)]
    fn advance(&mut self, chunks: u64) {
        self.chunk_counter += chunks
    }
}

#[cfg(all(test, feature = "zeroize"))]
impl CvStack {
    /// The array, for the zeroize test.
    pub fn array(&self) -> Option<&[Cv; DEPTH]> {
        self.cvs.as_ref()
    }
}

#[cfg(feature = "zeroize")]
impl zeroize::Zeroize for CvStack {
    fn zeroize(&mut self) {
        if let Some(cvs) = &mut self.cvs {
            cvs.zeroize();
        }
        self.len = 0;
    }
}
