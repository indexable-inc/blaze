//! Deflate (RFC 1951): LZ77 over a 32 KiB window with hash chains, each block sent as stored,
//! fixed or dynamic Huffman, whichever is fewest bits.
//!
//! **Levels.** [`Level::Store`] sends stored blocks only. [`Level::Fast`] matches greedily with
//! chains of 4 (zlib's level 1 shape). [`Level::Default`] defers a match by one byte when the
//! next is longer (zlib's lazy matching, its level 6 parameters: chains of 128, `nice` 128,
//! `lazy` 16, `good` 8).
//!
//! **Deterministic.** The output is a function of the whole input and the level alone: never of
//! how the input was split among [`Deflate::write`] calls, the machine or a clock. Input is
//! encoded in chunks whose ends are fixed by position (each chunk is encoded once its 262 bytes
//! of lookahead are in), blocks end by symbol count and span, and Huffman lengths come from the
//! counts with ties broken by symbol. The tests check a split never changes a byte.
//!
//! **Memory.** The buffer of input, the hash heads and chains and the block's symbols are in the
//! caller's allocator; the buffer slides, so it holds at most a few windows and one block's span.
//!
//! The Huffman lengths are the minimum-redundancy lengths of Moffat and Katajainen's in-place
//! method, held to 15 bits (7 for the code-length code) by miniz's adjustment of the counts per
//! length, as Relax's miniz port computes them.

// adapted from makepad/relax (MIT), libs/relax/base/src/inflate/miniz_deflate/core.rs: the
// minimum-redundancy lengths and the code-length limit (`calculate_minimum_redundancy`,
// `enforce_max_code_size`); the rest is written here.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt};

use blaze_own::{
    AllocFailure, Vec,
    planted::{NoPlant, Planted},
};

use crate::{
    plant::DeflatePlant,
    tables::{
        DIST_BASE, DIST_EXTRA, DIST_SYMBOLS, FIXED_DIST_LENS, FIXED_LIT_LENS, LEN_BASE, LEN_CODE,
        LEN_EXTRA, LIT_SYMBOLS, MAX_MATCH, MIN_MATCH, PRECODE_ORDER, WINDOW, dist_code, reverse,
    },
};

/// How hard a [`Deflate`] looks for matches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Store,
    Fast,
    Default,
}

/// Why a [`Deflate`] stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeflateError {
    /// A write or finish after [`Deflate::finish`].
    Finished,
    /// A buffer could not be had.
    Alloc(AllocFailure),
}

impl fmt::Display for DeflateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeflateError::Finished => f.write_str("blaze_deflate: a write to a stream already finished"),
            DeflateError::Alloc(failure) => write!(f, "blaze_deflate: {failure}"),
        }
    }
}

impl core::error::Error for DeflateError {}

const WMASK: usize = WINDOW - 1;
const HASH_BITS: u32 = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;
/// No position: an empty chain.
const NONE: u32 = u32::MAX;
/// The input encoded at a time, once its lookahead is in.
const CHUNK: usize = 1 << 16;
/// What a chunk's last position may read past it: a match, and the lazy look one byte on.
const LOOKAHEAD: usize = MAX_MATCH + MIN_MATCH + 1;
/// A block ends at this many symbols or this many bytes of input.
const BLOCK_SYMBOLS: usize = 16_383;
const BLOCK_SPAN: usize = 1 << 17;
/// The most bytes of one stored block.
const STORED_MOST: usize = 65_535;
/// A symbol that is a match: `MATCH | (length - 3) << 16 | (distance - 1)`; else a literal.
const MATCH: u32 = 1 << 31;

#[derive(Clone, Copy)]
struct Params {
    chain: u32,
    nice: usize,
    lazy: usize,
    good: usize,
}

const fn params(level: Level) -> Params {
    match level {
        Level::Store => Params { chain: 0, nice: 0, lazy: 0, good: 0 },
        Level::Fast => Params { chain: 4, nice: 16, lazy: 0, good: 4 },
        Level::Default => Params { chain: 128, nice: 128, lazy: 16, good: 8 },
    }
}

/// The bits not yet whole bytes, lowest first.
struct Bits {
    value: u64,
    n: u32,
}

impl Bits {
    /// `n` bits (at most 32) of `value`. The caller has reserved room in `out`.
    fn put<B: Allocator>(&mut self, out: &mut Vec<u8, B>, value: u32, n: u32) {
        self.value |= u64::from(value) << self.n;
        self.n += n;
        if self.n >= 32 {
            out.extend_from_slice(&(self.value as u32).to_le_bytes());
            self.value >>= 32;
            self.n -= 32;
        }
    }

    /// Zeros to the next byte, and every whole byte out.
    fn align<B: Allocator>(&mut self, out: &mut Vec<u8, B>) {
        let pad = (8 - self.n % 8) % 8;
        self.put(out, 0, pad);
        while self.n >= 8 {
            out.push(self.value as u8);
            self.value >>= 8;
            self.n -= 8;
        }
    }
}

/// Whether a deflater still takes input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stream {
    /// Writes are taken.
    Open,
    /// The final block is out: every write and finish is [`DeflateError::Finished`].
    Finished,
}

/// A streaming deflater: see the module. Ends with [`Deflate::finish`]. `P` is the plant seam
/// (D165): shipped code names [`Deflate`], this with [`NoPlant`].
pub struct PlantedDeflate<A: Allocator, P> {
    level: Level,
    params: Params,
    /// The input kept: the window behind `pos`, the block's span and what is not yet encoded.
    data: Vec<u8, A>,
    head: Vec<u32, A>,
    prev: Vec<u32, A>,
    /// The next position to encode.
    pos: usize,
    /// Where the block being gathered starts.
    block_start: usize,
    /// A match already found at `pos`, whose position is already hashed (lazy matching).
    carry: Option<(usize, usize)>,
    syms: Vec<u32, A>,
    lit_freq: [u32; LIT_SYMBOLS],
    dist_freq: [u32; DIST_SYMBOLS],
    bits: Bits,
    stream: Stream,
    plant: P,
}

/// The deflater as it ships: [`PlantedDeflate`] with no plant.
pub type Deflate<A> = PlantedDeflate<A, NoPlant>;

impl<A: Allocator + Clone> PlantedDeflate<A, NoPlant> {
    /// A deflater at `level`, its buffers in `alloc`.
    pub fn new_in(level: Level, alloc: A) -> Deflate<A> {
        PlantedDeflate::new_planted(level, alloc, NoPlant)
    }
}

impl<A: Allocator + Clone, P: Planted<DeflatePlant>> PlantedDeflate<A, P> {
    /// [`Deflate::new_in`] with its plant seam: `plant` is a control's [`DeflatePlant`].
    pub fn new_planted(level: Level, alloc: A, plant: P) -> PlantedDeflate<A, P> {
        PlantedDeflate {
            level,
            params: params(level),
            data: Vec::new_in(alloc.clone()),
            head: Vec::from_elem_in(NONE, HASH_SIZE, alloc.clone()),
            prev: Vec::from_elem_in(NONE, WINDOW, alloc.clone()),
            pos: 0,
            block_start: 0,
            carry: None,
            syms: Vec::with_capacity_in(BLOCK_SYMBOLS + 1, alloc),
            lit_freq: [0; LIT_SYMBOLS],
            dist_freq: [0; DIST_SYMBOLS],
            bits: Bits { value: 0, n: 0 },
            stream: Stream::Open,
            plant,
        }
    }
}

impl<A: Allocator, P: Planted<DeflatePlant>> PlantedDeflate<A, P> {
    /// `input` taken; whatever whole blocks it completes are appended to `out`.
    pub fn write<B: Allocator>(&mut self, input: &[u8], out: &mut Vec<u8, B>) -> Result<(), DeflateError> {
        if self.stream == Stream::Finished {
            return Err(DeflateError::Finished);
        }
        self.data.try_extend_from_slice(input).map_err(DeflateError::Alloc)?;
        while self.data.len() - self.pos >= CHUNK + LOOKAHEAD {
            let limit = self.pos + CHUNK;
            self.encode(limit, out)?;
            self.slide();
        }
        Ok(())
    }

    /// The rest encoded, the final block sent and the stream padded to a byte.
    pub fn finish<B: Allocator>(&mut self, out: &mut Vec<u8, B>) -> Result<(), DeflateError> {
        if self.stream == Stream::Finished {
            return Err(DeflateError::Finished);
        }
        let end = self.data.len();
        self.encode(end, out)?;
        self.emit_block(true, out)?;
        out.try_reserve(8).map_err(DeflateError::Alloc)?;
        self.bits.align(out);
        self.stream = Stream::Finished;
        self.data.clear();
        Ok(())
    }

    fn hash(&self, pos: usize) -> usize {
        let d = &self.data;
        let v = u32::from(d[pos]) | (u32::from(d[pos + 1]) << 8) | (u32::from(d[pos + 2]) << 16);
        (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    }

    /// `pos` added to its chain, if three bytes start there.
    fn insert(&mut self, pos: usize) {
        if pos + MIN_MATCH > self.data.len() {
            return;
        }
        let h = self.hash(pos);
        self.prev[pos & WMASK] = self.head[h];
        self.head[h] = pos as u32;
    }

    /// The longest match at `pos` longer than `prev_len`, as (length, distance), or (0, 0).
    /// Searched before `pos` is inserted, so every chain entry it follows is in the window.
    fn find(&self, pos: usize, prev_len: usize) -> (usize, usize) {
        let data = &self.data[..];
        let avail = data.len() - pos;
        if avail < MIN_MATCH {
            return (0, 0);
        }
        let most = avail.min(MAX_MATCH);
        let mut best = prev_len.max(MIN_MATCH - 1);
        if best >= most {
            return (0, 0);
        }
        let mut chain = self.params.chain;
        if prev_len >= self.params.good {
            chain >>= 2;
        }
        let mut cand = self.head[self.hash(pos)];
        let mut best_dist = 0;
        while cand != NONE && chain > 0 {
            let c = cand as usize;
            if c >= pos || pos - c > WINDOW {
                break;
            }
            if data[c + best] == data[pos + best] && data[c] == data[pos] && data[c + 1] == data[pos + 1] {
                let l = crate::simd::match_len(data, c, pos, 2, most);
                if l > best {
                    best = l;
                    best_dist = pos - c;
                    if l >= self.params.nice || l >= most {
                        break;
                    }
                }
            }
            cand = self.prev[c & WMASK];
            chain -= 1;
        }
        match best_dist {
            0 => (0, 0),
            _ => (best, best_dist),
        }
    }

    fn literal(&mut self, pos: usize) {
        let byte = self.data[pos];
        self.syms.push(u32::from(byte));
        self.lit_freq[usize::from(byte)] += 1;
    }

    fn matched(&mut self, length: usize, distance: usize) {
        let distance = match self.plant.has(DeflatePlant::DistanceOffByOne) && distance < WINDOW {
            true => distance + 1,
            false => distance,
        };
        self.syms.push(MATCH | (((length - MIN_MATCH) as u32) << 16) | (distance - 1) as u32);
        self.lit_freq[257 + usize::from(LEN_CODE[length - MIN_MATCH])] += 1;
        self.dist_freq[dist_code(distance)] += 1;
    }

    /// Positions up to `limit` encoded (a match may end past it).
    fn encode<B: Allocator>(&mut self, limit: usize, out: &mut Vec<u8, B>) -> Result<(), DeflateError> {
        if self.level == Level::Store {
            self.pos = self.pos.max(limit);
            if self.pos - self.block_start >= BLOCK_SPAN {
                self.emit_block(false, out)?;
            }
            return Ok(());
        }
        let lazy = self.params.lazy;
        while self.pos < limit {
            let pos = self.pos;
            let (len, dist) = match self.carry.take() {
                Some(found) => found,
                None => {
                    let found = self.find(pos, 0);
                    self.insert(pos);
                    found
                }
            };
            if len >= MIN_MATCH {
                let mut from = pos + 1;
                if len < lazy && pos + 1 < self.data.len() {
                    let next = self.find(pos + 1, len);
                    self.insert(pos + 1);
                    if next.0 > len {
                        self.literal(pos);
                        self.pos = pos + 1;
                        self.carry = Some(next);
                        self.check_block(out)?;
                        continue;
                    }
                    from = pos + 2;
                }
                self.matched(len, dist);
                for q in from..pos + len {
                    self.insert(q);
                }
                self.pos = pos + len;
            } else {
                self.literal(pos);
                self.pos = pos + 1;
            }
            self.check_block(out)?;
        }
        Ok(())
    }

    fn check_block<B: Allocator>(&mut self, out: &mut Vec<u8, B>) -> Result<(), DeflateError> {
        if self.syms.len() >= BLOCK_SYMBOLS || self.pos - self.block_start >= BLOCK_SPAN {
            self.emit_block(false, out)?;
        }
        Ok(())
    }

    fn emit_block<B: Allocator>(&mut self, last: bool, out: &mut Vec<u8, B>) -> Result<(), DeflateError> {
        let span = &self.data[self.block_start..self.pos];
        let bound = span.len() + 5 * (span.len() / STORED_MOST + 1) + 64;
        out.try_reserve(bound).map_err(DeflateError::Alloc)?;
        let block = Block { span, syms: &self.syms, lit_freq: &self.lit_freq, dist_freq: &self.dist_freq };
        block.write(&mut self.bits, out, self.level, last);
        self.syms.clear();
        self.lit_freq = [0; LIT_SYMBOLS];
        self.dist_freq = [0; DIST_SYMBOLS];
        self.block_start = self.pos;
        Ok(())
    }

    /// The buffer's front dropped, a multiple of the window at a time (so a chain's slot is
    /// unchanged), keeping the window behind `pos` and the block's span.
    fn slide(&mut self) {
        let keep = self.pos.saturating_sub(WINDOW).min(self.block_start) & !WMASK;
        if keep < 2 * WINDOW {
            return;
        }
        let len = self.data.len();
        self.data.copy_within(keep.., 0);
        self.data.truncate(len - keep);
        let k = keep as u32;
        for h in self.head.iter_mut().chain(self.prev.iter_mut()) {
            *h = match *h == NONE || *h < k {
                true => NONE,
                false => *h - k,
            };
        }
        self.pos -= keep;
        self.block_start -= keep;
    }
}

/// One block's input and symbols.
struct Block<'a> {
    span: &'a [u8],
    syms: &'a [u32],
    lit_freq: &'a [u32; LIT_SYMBOLS],
    dist_freq: &'a [u32; DIST_SYMBOLS],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Stored,
    Fixed,
    Dynamic,
}

impl Block<'_> {
    fn write<B: Allocator>(&self, w: &mut Bits, out: &mut Vec<u8, B>, level: Level, last: bool) {
        let mut lit = *self.lit_freq;
        lit[256] = 1;
        let stored = stored_bits(self.span.len(), w.n);
        if level == Level::Store {
            return self.write_stored(w, out, last);
        }
        // The dynamic code.
        let mut lit_lens = [0u8; LIT_SYMBOLS];
        code_lengths(&lit, 15, &mut lit_lens);
        let mut dist_lens = [0u8; DIST_SYMBOLS];
        code_lengths(self.dist_freq, 15, &mut dist_lens);
        let hlit = (lit_lens.iter().rposition(|&l| l != 0).unwrap_or(0) + 1).max(257);
        let hdist = (dist_lens.iter().rposition(|&l| l != 0).unwrap_or(0) + 1).max(1);
        let mut all = [0u8; LIT_SYMBOLS + DIST_SYMBOLS];
        all[..hlit].copy_from_slice(&lit_lens[..hlit]);
        all[hlit..hlit + hdist].copy_from_slice(&dist_lens[..hdist]);
        let mut items = [(0u8, 0u8); LIT_SYMBOLS + DIST_SYMBOLS];
        let n_items = run_lengths(&all[..hlit + hdist], &mut items);
        let items = &items[..n_items];
        let mut pre_freq = [0u32; 19];
        for &(symbol, _) in items {
            pre_freq[usize::from(symbol)] += 1;
        }
        let mut pre_lens = [0u8; 19];
        code_lengths(&pre_freq, 7, &mut pre_lens);
        let mut hclen = 19;
        while hclen > 4 && pre_lens[PRECODE_ORDER[hclen - 1]] == 0 {
            hclen -= 1;
        }
        let header: u64 = 14
            + 3 * hclen as u64
            + items.iter().map(|&(s, _)| u64::from(pre_lens[usize::from(s)]) + repeat_bits(s) as u64).sum::<u64>();
        let dynamic = 3 + header + data_bits(&lit, self.dist_freq, &lit_lens, &dist_lens);
        let fixed = 3 + data_bits(&lit, self.dist_freq, &FIXED_LIT_LENS[..LIT_SYMBOLS], &FIXED_DIST_LENS[..DIST_SYMBOLS]);
        let kind = match () {
            () if dynamic <= fixed && dynamic < stored => Kind::Dynamic,
            () if fixed < stored => Kind::Fixed,
            () => Kind::Stored,
        };
        match kind {
            Kind::Stored => self.write_stored(w, out, last),
            Kind::Fixed => {
                w.put(out, u32::from(last) | (1 << 1), 3);
                let mut lit_codes = [0u16; 288];
                canonical(&FIXED_LIT_LENS, &mut lit_codes);
                let mut dist_codes = [0u16; 32];
                canonical(&FIXED_DIST_LENS, &mut dist_codes);
                self.write_symbols(w, out, &FIXED_LIT_LENS, &lit_codes, &FIXED_DIST_LENS, &dist_codes);
            }
            Kind::Dynamic => {
                w.put(out, u32::from(last) | (2 << 1), 3);
                w.put(out, (hlit - 257) as u32, 5);
                w.put(out, (hdist - 1) as u32, 5);
                w.put(out, (hclen - 4) as u32, 4);
                for &symbol in &PRECODE_ORDER[..hclen] {
                    w.put(out, u32::from(pre_lens[symbol]), 3);
                }
                let mut pre_codes = [0u16; 19];
                canonical(&pre_lens, &mut pre_codes);
                for &(symbol, extra) in items {
                    let s = usize::from(symbol);
                    w.put(out, u32::from(pre_codes[s]), u32::from(pre_lens[s]));
                    w.put(out, u32::from(extra), repeat_bits(symbol));
                }
                let mut lit_codes = [0u16; LIT_SYMBOLS];
                canonical(&lit_lens, &mut lit_codes);
                let mut dist_codes = [0u16; DIST_SYMBOLS];
                canonical(&dist_lens, &mut dist_codes);
                self.write_symbols(w, out, &lit_lens, &lit_codes, &dist_lens, &dist_codes);
            }
        }
    }

    fn write_symbols<B: Allocator>(
        &self,
        w: &mut Bits,
        out: &mut Vec<u8, B>,
        lit_lens: &[u8],
        lit_codes: &[u16],
        dist_lens: &[u8],
        dist_codes: &[u16],
    ) {
        let lit = |w: &mut Bits, out: &mut Vec<u8, B>, s: usize| {
            w.put(out, u32::from(lit_codes[s]), u32::from(lit_lens[s]));
        };
        for &sym in self.syms {
            if sym & MATCH == 0 {
                lit(w, out, sym as usize);
                continue;
            }
            let length = ((sym >> 16) & 0xff) as usize + MIN_MATCH;
            let distance = (sym & 0xffff) as usize + 1;
            let lc = usize::from(LEN_CODE[length - MIN_MATCH]);
            lit(w, out, 257 + lc);
            w.put(out, (length - usize::from(LEN_BASE[lc])) as u32, u32::from(LEN_EXTRA[lc]));
            let dc = dist_code(distance);
            w.put(out, u32::from(dist_codes[dc]), u32::from(dist_lens[dc]));
            w.put(out, (distance - usize::from(DIST_BASE[dc])) as u32, u32::from(DIST_EXTRA[dc]));
        }
        lit(w, out, 256);
    }

    fn write_stored<B: Allocator>(&self, w: &mut Bits, out: &mut Vec<u8, B>, last: bool) {
        let count = self.span.len().div_ceil(STORED_MOST).max(1);
        for i in 0..count {
            let piece = &self.span[(i * STORED_MOST).min(self.span.len())..((i + 1) * STORED_MOST).min(self.span.len())];
            w.put(out, u32::from(last && i == count - 1), 3);
            w.align(out);
            let len = piece.len() as u32;
            w.put(out, len | ((!len & 0xffff) << 16), 32);
            out.extend_from_slice(piece);
        }
    }
}

/// The extra bits of a code-length symbol: 16 repeats with 2, 17 and 18 send zeros with 3 and 7.
const fn repeat_bits(symbol: u8) -> u32 {
    match symbol {
        16 => 2,
        17 => 3,
        18 => 7,
        _ => 0,
    }
}

/// The bits of a block's symbols and end under the given lengths.
fn data_bits(lit: &[u32; LIT_SYMBOLS], dist: &[u32; DIST_SYMBOLS], lit_lens: &[u8], dist_lens: &[u8]) -> u64 {
    let mut bits = 0u64;
    for (s, &f) in lit.iter().enumerate() {
        let extra = if s > 256 { u64::from(LEN_EXTRA[s - 257]) } else { 0 };
        bits += u64::from(f) * (u64::from(lit_lens[s]) + extra);
    }
    for (d, &f) in dist.iter().enumerate() {
        bits += u64::from(f) * (u64::from(dist_lens[d]) + u64::from(DIST_EXTRA[d]));
    }
    bits
}

/// The bits of `len` bytes sent stored with `n` bits already pending: each block's three header
/// bits, the padding to a byte, its lengths and its bytes.
fn stored_bits(len: usize, n: u32) -> u64 {
    let count = len.div_ceil(STORED_MOST).max(1);
    let (mut at, mut bits) = (u64::from(n), 0u64);
    for _ in 0..count {
        at += 3;
        let pad = (8 - at % 8) % 8;
        at += pad + 32;
        bits += 3 + pad + 32;
    }
    bits + 8 * len as u64
}

/// `lens` sent as the code-length code's symbols: runs of zeros as 17 and 18, a length repeated
/// as 16 after it is sent once. Returns how many items it wrote.
fn run_lengths(lens: &[u8], items: &mut [(u8, u8)]) -> usize {
    let (mut n, mut i) = (0, 0);
    let item = |items: &mut [(u8, u8)], n: &mut usize, it: (u8, u8)| {
        items[*n] = it;
        *n += 1;
    };
    while i < lens.len() {
        let value = lens[i];
        let mut run = 1;
        while i + run < lens.len() && lens[i + run] == value {
            run += 1;
        }
        i += run;
        if value == 0 {
            while run >= 11 {
                let take = run.min(138);
                item(items, &mut n, (18, (take - 11) as u8));
                run -= take;
            }
            if run >= 3 {
                item(items, &mut n, (17, (run - 3) as u8));
                run = 0;
            }
        } else {
            item(items, &mut n, (value, 0));
            run -= 1;
            while run >= 3 {
                let take = run.min(6);
                item(items, &mut n, (16, (take - 3) as u8));
                run -= take;
            }
        }
        for _ in 0..run {
            item(items, &mut n, (value, 0));
        }
    }
    n
}

/// Huffman lengths for `freq`, none over `limit`. At least two symbols get a code (zlib's
/// rule): one code of one bit is incomplete, which a strict reader refuses for the code-length
/// code. Ties are broken by symbol, so the lengths are a function of the counts.
fn code_lengths(freq: &[u32], limit: u32, lens: &mut [u8]) {
    let mut syms = [(0u32, 0u16); 288];
    let mut n = 0;
    for (s, &f) in freq.iter().enumerate() {
        if f != 0 {
            syms[n] = (f, s as u16);
            n += 1;
        }
    }
    let mut s = 0;
    while n < 2 {
        if freq[s] == 0 {
            syms[n] = (1, s as u16);
            n += 1;
        }
        s += 1;
    }
    lens.fill(0);
    let syms = &mut syms[..n];
    syms.sort_unstable();
    let mut a = [0u32; 288];
    for (slot, &(f, _)) in a.iter_mut().zip(syms.iter()) {
        *slot = f;
    }
    minimum_redundancy(&mut a[..n]);
    let mut count = [0u32; 290];
    for &depth in &a[..n] {
        count[depth as usize] += 1;
    }
    limit_lengths(&mut count, limit as usize);
    let mut j = n;
    for len in 1..=limit as usize {
        for _ in 0..count[len] {
            j -= 1;
            lens[usize::from(syms[j].1)] = len as u8;
        }
    }
}

/// Moffat and Katajainen's in-place minimum-redundancy lengths: `a` holds at least two weights
/// sorted ascending, and ends holding each one's code length (the first the longest).
fn minimum_redundancy(a: &mut [u32]) {
    let n = a.len();
    a[0] += a[1];
    let (mut root, mut leaf) = (0usize, 2usize);
    for next in 1..n - 1 {
        if leaf >= n || a[root] < a[leaf] {
            a[next] = a[root];
            a[root] = next as u32;
            root += 1;
        } else {
            a[next] = a[leaf];
            leaf += 1;
        }
        if leaf >= n || (root < next && a[root] < a[leaf]) {
            a[next] += a[root];
            a[root] = next as u32;
            root += 1;
        } else {
            a[next] += a[leaf];
            leaf += 1;
        }
    }
    a[n - 2] = 0;
    for next in (0..n.saturating_sub(2)).rev() {
        a[next] = a[a[next] as usize] + 1;
    }
    let (mut avail, mut used, mut depth) = (1i64, 0i64, 0u32);
    let (mut root, mut next) = (n as isize - 2, n as isize - 1);
    while avail > 0 {
        while root >= 0 && a[root as usize] == depth {
            used += 1;
            root -= 1;
        }
        while avail > used {
            a[next as usize] = depth;
            next -= 1;
            avail -= 1;
        }
        avail = 2 * used;
        depth += 1;
        used = 0;
    }
}

/// The counts per length held to `max`: every longer code moved to `max`, then codes moved down
/// a level until the code is complete again (miniz's `enforce_max_code_size`).
fn limit_lengths(count: &mut [u32; 290], max: usize) {
    for i in max + 1..count.len() {
        count[max] += count[i];
        count[i] = 0;
    }
    let mut total: u64 = 0;
    for (i, &c) in count.iter().enumerate().take(max + 1).skip(1) {
        total += u64::from(c) << (max - i);
    }
    while total != 1 << max {
        count[max] -= 1;
        for i in (1..max).rev() {
            if count[i] != 0 {
                count[i] -= 1;
                count[i + 1] += 2;
                break;
            }
        }
        total -= 1;
    }
}

/// Canonical codes of `lens`, each reversed for a bit buffer read from its lowest bit.
fn canonical(lens: &[u8], codes: &mut [u16]) {
    let mut count = [0u32; 16];
    for &l in lens {
        count[usize::from(l)] += 1;
    }
    count[0] = 0;
    let mut next = [0u32; 16];
    let mut code = 0u32;
    for len in 1..16 {
        code = (code + count[len - 1]) << 1;
        next[len] = code;
    }
    for (s, &l) in lens.iter().enumerate() {
        if l != 0 {
            let l = usize::from(l);
            codes[s] = reverse(next[l], l as u32) as u16;
            next[l] += 1;
        }
    }
}
