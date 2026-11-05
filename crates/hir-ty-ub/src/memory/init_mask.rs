//! Which bytes of an allocation are initialized: a port of Miri's `InitMask`
//! (`compiler/rustc_middle/src/mir/interpret/allocation/init_mask.rs`), one bit per byte in
//! `u64` blocks, kept lazy (a single state for every byte) until a write mixes states.

use std::ops::Range;

type Block = u64;

const BLOCK_SIZE: u64 = 64;

// Miri: init_mask.rs, `InitMask`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct InitMask {
    blocks: Blocks,
    len: u64,
}

// Miri: init_mask.rs, `InitMaskBlocks`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Blocks {
    Lazy { state: bool },
    Materialized(Vec<Block>),
}

impl InitMask {
    // Miri: init_mask.rs, `InitMask::new`.
    pub(super) fn new(size: u64, state: bool) -> InitMask {
        InitMask { blocks: Blocks::Lazy { state }, len: size }
    }

    pub(super) fn len(&self) -> u64 {
        self.len
    }

    /// `Some(state)` when every byte is in `state` without looking at a bit.
    pub(super) fn lazy_state(&self) -> Option<bool> {
        match self.blocks {
            Blocks::Lazy { state } => Some(state),
            Blocks::Materialized(_) => None,
        }
    }

    /// Ok, or the first contiguous uninitialized part of `range`.
    // Miri: init_mask.rs, `InitMask::is_range_initialized`.
    #[inline]
    pub(super) fn is_range_initialized(&self, range: Range<u64>) -> Result<(), Range<u64>> {
        let end = range.end;
        if end > self.len {
            return Err(self.len..end);
        }
        match &self.blocks {
            Blocks::Lazy { state } => {
                if *state || range.start >= end {
                    Ok(())
                } else {
                    Err(range)
                }
            }
            Blocks::Materialized(blocks) => match find_bit(blocks, range.start, end, false) {
                Some(uninit_start) => {
                    let uninit_end = find_bit(blocks, uninit_start, end, true).unwrap_or(end);
                    Err(uninit_start..uninit_end)
                }
                None => Ok(()),
            },
        }
    }

    /// Sets `range` to `new_state`, growing the mask when it ends past it.
    // Miri: init_mask.rs, `InitMask::set_range`. Empty ranges are a no-op, as Miri's callers
    // (`Allocation::mark_init`) make them.
    pub(super) fn set_range(&mut self, range: Range<u64>, new_state: bool) {
        let start = range.start;
        let end = range.end;
        if start >= end {
            return;
        }
        let is_full_overwrite = start == 0 && end >= self.len;
        match &mut self.blocks {
            Blocks::Lazy { state } if is_full_overwrite => {
                *state = new_state;
                self.len = end;
            }
            Blocks::Materialized(_) if is_full_overwrite => {
                self.blocks = Blocks::Lazy { state: new_state };
                self.len = end;
            }
            Blocks::Lazy { state } if *state == new_state => {
                if end > self.len {
                    self.len = end;
                }
            }
            _ => {
                let len = self.len;
                let blocks = self.materialize_blocks();
                if end <= len {
                    set_range_inbounds(blocks, start, end, new_state);
                } else {
                    if start < len {
                        set_range_inbounds(blocks, start, len, new_state);
                    }
                    grow(blocks, len, end - len, new_state);
                    self.len = end;
                }
            }
        }
    }

    // Miri: init_mask.rs, `InitMask::materialize_blocks`.
    #[inline]
    fn materialize_blocks(&mut self) -> &mut Vec<Block> {
        if let Blocks::Lazy { state } = self.blocks {
            let mut blocks = Vec::new();
            grow(&mut blocks, 0, self.len, state);
            self.blocks = Blocks::Materialized(blocks);
        }
        match &mut self.blocks {
            Blocks::Materialized(blocks) => blocks,
            Blocks::Lazy { .. } => unreachable!("init mask blocks were just materialized"),
        }
    }

    /// The state of in-bounds byte `idx`.
    // Miri: init_mask.rs, `InitMask::get`.
    #[inline]
    pub(super) fn get(&self, idx: u64) -> bool {
        match &self.blocks {
            Blocks::Lazy { state } => *state,
            Blocks::Materialized(blocks) => get(blocks, idx),
        }
    }

    /// The runs of initialized and uninitialized bytes of `range`, alternating, nonempty, and
    /// covering exactly `range`.
    // Miri: init_mask.rs, `InitMask::range_as_init_chunks`.
    #[inline]
    pub(super) fn chunks(&self, range: Range<u64>) -> Chunks<'_> {
        assert!(range.end <= self.len);
        let is_init = if range.start < range.end { self.get(range.start) } else { false };
        Chunks { mask: self, is_init, start: range.start, end: range.end }
    }

    /// The run-length encoding of `range` into `ranges` (cleared first), returning the state of
    /// the first run: what [`InitMask::apply_copy`] writes elsewhere.
    // Miri: init_mask.rs, `InitMask::prepare_copy`. The runs go into a buffer the caller keeps,
    // not a fresh `InitCopy`, so a copy allocates nothing once the buffer is warm.
    pub(super) fn prepare_copy(&self, range: Range<u64>, ranges: &mut Vec<u64>) -> bool {
        ranges.clear();
        let mut chunks = self.chunks(range).peekable();
        let initial = chunks.peek().expect("range should be nonempty").is_init();
        for chunk in chunks {
            let r = chunk.range();
            ranges.push(r.end - r.start);
        }
        initial
    }

    /// `repeat` copies of the runs `prepare_copy` made, each of `size` bytes, from `start`.
    // Miri: init_mask.rs, `InitMask::apply_copy`.
    pub(super) fn apply_copy(
        &mut self,
        initial: bool,
        ranges: &[u64],
        start: u64,
        size: u64,
        repeat: u64,
    ) {
        if ranges.len() <= 1 {
            self.set_range(start..start + size * repeat, initial);
            return;
        }
        let blocks = self.materialize_blocks();
        for j in 0..repeat {
            let mut j = j * size + start;
            let mut cur = initial;
            for range in ranges {
                let old_j = j;
                j += range;
                set_range_inbounds(blocks, old_j, j, cur);
                cur = !cur;
            }
        }
    }

    /// Writes the states of `range` into `out` as bits from bit 0 (byte `range.start`), bits
    /// past the range cleared: the init part of [`super::Bytes`].
    pub(super) fn bits(&self, range: Range<u64>, out: &mut [u64]) {
        let len = range.end - range.start;
        debug_assert!(out.len() as u64 == len.div_ceil(BLOCK_SIZE));
        match &self.blocks {
            Blocks::Lazy { state } => {
                out.fill(if *state { u64::MAX } else { 0 });
            }
            Blocks::Materialized(blocks) => extract(blocks, range.start, out),
        }
        if let Some(last) = out.last_mut() {
            let tail = len % BLOCK_SIZE;
            if tail != 0 {
                *last &= u64::MAX >> (64 - tail);
            }
        }
    }

    /// Sets bytes `at..at + len` from `bits` (bit 0 is byte `at`), in bounds of the mask.
    pub(super) fn set_bits(&mut self, at: u64, bits: &[u64], len: u64) {
        debug_assert!(at + len <= self.len);
        let mut word = 0;
        let mut pos = at;
        let end = at + len;
        let blocks = self.materialize_blocks();
        while pos < end {
            let n = (end - pos).min(BLOCK_SIZE);
            blit(blocks, pos, bits[word], n);
            word += 1;
            pos += n;
        }
    }

    #[cfg(test)]
    pub(super) fn materialized_block_count(&self) -> usize {
        match &self.blocks {
            Blocks::Lazy { .. } => 0,
            Blocks::Materialized(blocks) => blocks.len(),
        }
    }
}

/// Fills `out` with the bits of `blocks` from bit `start` on, a word at a time; the caller
/// masks what is past the end.
pub(super) fn extract(blocks: &[Block], start: u64, out: &mut [u64]) {
    let shift = (start % BLOCK_SIZE) as u32;
    let first = (start / BLOCK_SIZE) as usize;
    for (w, word) in out.iter_mut().enumerate() {
        let lo = blocks.get(first + w).map_or(0, |b| b >> shift);
        let hi = match blocks.get(first + w + 1) {
            Some(next) if shift != 0 => next << (64 - shift),
            _ => 0,
        };
        *word = lo | hi;
    }
}

/// Writes the low `n` bits of `value` (`n <= 64`) at bit `pos` of `blocks`.
pub(super) fn blit(blocks: &mut [Block], pos: u64, value: u64, n: u64) {
    let mask = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
    let value = value & mask;
    let block = (pos / BLOCK_SIZE) as usize;
    let off = pos % BLOCK_SIZE;
    blocks[block] = (blocks[block] & !(mask << off)) | (value << off);
    if off + n > BLOCK_SIZE {
        let rest = off + n - BLOCK_SIZE;
        let rest_mask = (1u64 << rest) - 1;
        blocks[block + 1] = (blocks[block + 1] & !rest_mask) | (value >> (BLOCK_SIZE - off));
    }
}

// Miri: init_mask.rs, `InitMaskMaterialized::bit_index`.
#[inline]
fn bit_index(bits: u64) -> (usize, usize) {
    ((bits / BLOCK_SIZE) as usize, (bits % BLOCK_SIZE) as usize)
}

// Miri: init_mask.rs, `InitMaskMaterialized::get`.
#[inline]
fn get(blocks: &[Block], i: u64) -> bool {
    let (block, bit) = bit_index(i);
    (blocks[block] & (1 << bit)) != 0
}

// Miri: init_mask.rs, `InitMaskMaterialized::set_range_inbounds`.
fn set_range_inbounds(blocks: &mut [Block], start: u64, end: u64, new_state: bool) {
    if start >= end {
        return;
    }
    let (block_a, bit_a) = bit_index(start);
    let (block_b, bit_b) = bit_index(end);
    if block_a == block_b {
        let range = if bit_b == 0 {
            u64::MAX << bit_a
        } else {
            (u64::MAX << bit_a) & (u64::MAX >> (64 - bit_b))
        };
        if new_state {
            blocks[block_a] |= range;
        } else {
            blocks[block_a] &= !range;
        }
        return;
    }
    if new_state {
        blocks[block_a] |= u64::MAX << bit_a;
        if bit_b != 0 {
            blocks[block_b] |= u64::MAX >> (64 - bit_b);
        }
        for block in &mut blocks[block_a + 1..block_b] {
            *block = u64::MAX;
        }
    } else {
        blocks[block_a] &= !(u64::MAX << bit_a);
        if bit_b != 0 {
            blocks[block_b] &= !(u64::MAX >> (64 - bit_b));
        }
        for block in &mut blocks[block_a + 1..block_b] {
            *block = 0;
        }
    }
}

// Miri: init_mask.rs, `InitMaskMaterialized::grow`.
fn grow(blocks: &mut Vec<Block>, len: u64, amount: u64, new_state: bool) {
    if amount == 0 {
        return;
    }
    let unused_trailing_bits = blocks.len() as u64 * BLOCK_SIZE - len;
    if amount > unused_trailing_bits {
        let additional_blocks = amount / BLOCK_SIZE + 1;
        let block = if new_state { u64::MAX } else { 0 };
        blocks.extend(std::iter::repeat_n(block, additional_blocks as usize));
    }
    if unused_trailing_bits > 0 {
        set_range_inbounds(blocks, len, len + unused_trailing_bits, new_state);
    }
}

/// The first bit of `start..end` equal to `is_init`.
// Miri: init_mask.rs, `InitMaskMaterialized::find_bit`, with its fast path checked against the
// slow one in debug builds as Miri does.
fn find_bit(blocks: &[Block], start: u64, end: u64, is_init: bool) -> Option<u64> {
    let result = find_bit_fast(blocks, start, end, is_init);
    debug_assert_eq!(result, find_bit_slow(blocks, start, end, is_init));
    result
}

// Miri: init_mask.rs, `find_bit_fast`.
fn find_bit_fast(blocks: &[Block], start: u64, end: u64, is_init: bool) -> Option<u64> {
    fn search_block(bits: Block, block: usize, start_bit: usize, is_init: bool) -> Option<u64> {
        let bits = if is_init { bits } else { !bits };
        let bits = bits & (!0 << start_bit);
        if bits == 0 {
            None
        } else {
            Some(block as u64 * BLOCK_SIZE + u64::from(bits.trailing_zeros()))
        }
    }

    if start >= end {
        return None;
    }
    let (start_block, start_bit) = bit_index(start);
    let (end_block_inclusive, _) = bit_index(end - 1);
    if let Some(i) = search_block(blocks[start_block], start_block, start_bit, is_init) {
        return if i < end { Some(i) } else { None };
    }
    if start_block < end_block_inclusive {
        for (&bits, block) in
            blocks[start_block + 1..end_block_inclusive + 1].iter().zip(start_block + 1..)
        {
            if let Some(i) = search_block(bits, block, 0, is_init) {
                return if i < end { Some(i) } else { None };
            }
        }
    }
    None
}

// Miri: init_mask.rs, `find_bit_slow`.
fn find_bit_slow(blocks: &[Block], start: u64, end: u64, is_init: bool) -> Option<u64> {
    (start..end).find(|&i| get(blocks, i) == is_init)
}

/// A run of initialized or uninitialized bytes.
// Miri: init_mask.rs, `InitChunk`.
pub(super) enum Chunk {
    Init(Range<u64>),
    Uninit(Range<u64>),
}

impl Chunk {
    #[inline]
    pub(super) fn is_init(&self) -> bool {
        matches!(self, Chunk::Init(_))
    }

    #[inline]
    pub(super) fn range(&self) -> Range<u64> {
        match self {
            Chunk::Init(r) | Chunk::Uninit(r) => r.clone(),
        }
    }
}

// Miri: init_mask.rs, `InitChunkIter`.
#[derive(Clone)]
pub(super) struct Chunks<'a> {
    mask: &'a InitMask,
    is_init: bool,
    start: u64,
    end: u64,
}

impl Iterator for Chunks<'_> {
    type Item = Chunk;

    #[inline]
    fn next(&mut self) -> Option<Chunk> {
        if self.start >= self.end {
            return None;
        }
        let end_of_chunk = match &self.mask.blocks {
            Blocks::Lazy { .. } => self.end,
            Blocks::Materialized(blocks) => {
                find_bit(blocks, self.start, self.end, !self.is_init).unwrap_or(self.end)
            }
        };
        let range = self.start..end_of_chunk;
        let chunk = if self.is_init { Chunk::Init(range) } else { Chunk::Uninit(range) };
        self.is_init = !self.is_init;
        self.start = end_of_chunk;
        Some(chunk)
    }
}
