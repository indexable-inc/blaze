//! Streaming inflate (RFC 1951): a sans-IO state machine fed the input in any pieces, every block
//! type (stored, fixed and dynamic Huffman), its output appended to the caller's vector as it
//! comes.
//!
//! **Bounds.** A hostile stream is bounded before a byte past its bound is written: the output
//! may not pass [`Limits::most_out`], nor the input read so far times [`Limits::most_ratio`] (plus
//! [`RATIO_SLACK`] for a stream's first symbols). Either is a typed refusal ([`Why::OutputOver`],
//! [`Why::RatioOver`]), never a partial answer said whole. DEFLATE's own largest ratio is about
//! 1032 to 1 (a one-bit literal/length code for 258 and a one-bit distance), so a `most_ratio` of
//! 1032 or more refuses no valid stream for its ratio, and less refuses the most repetitive ones.
//!
//! **Memory.** One block of the caller's allocator holds the 32 KiB window and the three decode
//! tables; nothing grows with the input. Output goes to the caller's vector, grown with
//! `try_reserve`, so memory running out is [`Why::Alloc`].
//!
//! **Decoding.** A code is looked up in a 10-bit table, and one longer than 10 bits (rare in
//! practice) is read bit by bit against the code's counts (the canonical walk of Mark Adler's
//! `puff`). A literal/length symbol, its extra bits, its distance and their extra bits are taken
//! as one step or not at all, so a piece's end inside a symbol leaves the state as it was; the bit
//! buffer holds 57 bits or more whenever input remains, and one step needs at most 48.
//!
//! **Where a stream ends.** The bit buffer reads up to eight bytes ahead. When the final block
//! ends, the whole bytes it read past it are kept in [`Inflate::tail`]: they are the next thing's
//! (a zlib or gzip trailer, the next object of a git pack), and the framings feed them on.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_own::{Box, Vec};

use crate::{
    error::{Error, Format, Tree, Why},
    plant::{self, Plant},
    tables::{
        DIST_BASE, DIST_EXTRA, FIXED_DIST_LENS, FIXED_LIT_LENS, LEN_BASE, LEN_EXTRA, MAX_MATCH,
        PRECODE_ORDER, WINDOW, reverse,
    },
};

const WMASK: usize = WINDOW - 1;

/// The bits of the first-level decode table.
const FAST_BITS: u32 = 10;
const FAST_SIZE: usize = 1 << FAST_BITS;

/// Output a stream may have before its ratio is held to [`Limits::most_ratio`]: its first
/// symbols say more than any steady ratio does.
pub const RATIO_SLACK: u64 = 1 << 12;

/// How much a stream may say: every field is the caller's choice, so there is no default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most bytes of output.
    pub most_out: u64,
    /// The most output per byte of input: a decompression bomb is refused at this ratio.
    pub most_ratio: u64,
    /// The most bytes of a gzip header (its extra field, name and comment); unused by deflate
    /// and zlib, whose headers are two bytes.
    pub most_header: u32,
}

/// What one [`Inflate::feed`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fed {
    /// Bytes of this call's input taken (into the output or the bit buffer).
    pub consumed: usize,
    /// Whether the stream has ended.
    pub done: bool,
}

/// A canonical Huffman decoder: counts per length, symbols sorted by code, and the first-level
/// table (`symbol | length << 16`, 0 for a code longer than [`FAST_BITS`] or for no code).
struct Huffman {
    counts: [u16; 16],
    symbols: [u16; 288],
    fast: [u32; FAST_SIZE],
}

/// What a decoder found in the bits at hand.
enum Decoded {
    /// A symbol and the bits its code took.
    Symbol(u16, u32),
    /// The bits at hand end inside a code.
    More,
    /// The bits are no code.
    Bad,
}

impl Huffman {
    const EMPTY: Huffman = Huffman { counts: [0; 16], symbols: [0; 288], fast: [0; FAST_SIZE] };

    /// The code of `lens` (one length per symbol, 0 for none). An oversubscribed code is refused,
    /// and an incomplete one too but for zlib's exceptions: one code of length 1 for literals or
    /// distances, and no code at all for distances (a block of literals only).
    fn build(&mut self, lens: &[u8], tree: Tree) -> Result<(), Why> {
        self.counts = [0; 16];
        for &len in lens {
            self.counts[usize::from(len)] += 1;
        }
        self.counts[0] = 0;
        let mut left: i32 = 1;
        for len in 1..16 {
            left = (left << 1) - i32::from(self.counts[len]);
            if left < 0 {
                return Err(Why::Oversubscribed(tree));
            }
        }
        let used: u32 = self.counts.iter().map(|&c| u32::from(c)).sum();
        if left > 0 {
            let one_short = tree != Tree::Precode && used == 1 && self.counts[1] == 1;
            let none = tree == Tree::Distances && used == 0;
            if !(one_short || none) {
                return Err(Why::Incomplete(tree));
            }
        }
        let mut offsets = [0u16; 16];
        for len in 1..15 {
            offsets[len + 1] = offsets[len] + self.counts[len];
        }
        for (symbol, &len) in lens.iter().enumerate() {
            if len != 0 {
                let slot = &mut offsets[usize::from(len)];
                // A symbol index below 288 (`lens` is at most 288 long).
                self.symbols[usize::from(*slot)] = symbol as u16;
                *slot += 1;
            }
        }
        self.fast = [0; FAST_SIZE];
        let (mut code, mut index) = (0u32, 0usize);
        for len in 1..=FAST_BITS {
            for _ in 0..self.counts[len as usize] {
                let entry = u32::from(self.symbols[index]) | (len << 16);
                let mut slot = reverse(code, len) as usize;
                while slot < FAST_SIZE {
                    self.fast[slot] = entry;
                    slot += 1 << len;
                }
                index += 1;
                code += 1;
            }
            code <<= 1;
        }
        Ok(())
    }

    /// The symbol at the bottom of `bits`, of which `have` are real (the rest are zero).
    fn decode(&self, bits: u64, have: u32) -> Decoded {
        let entry = self.fast[(bits as usize) & (FAST_SIZE - 1)];
        let len = entry >> 16;
        if len != 0 {
            return match len <= have {
                true => Decoded::Symbol((entry & 0xffff) as u16, len),
                false => Decoded::More,
            };
        }
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=15u32 {
            if len > have {
                return Decoded::More;
            }
            code |= ((bits >> (len - 1)) & 1) as i32;
            let count = i32::from(self.counts[len as usize]);
            if code - count < first {
                return Decoded::Symbol(self.symbols[(index + code - first) as usize], len);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Decoded::Bad
    }
}

/// The tables of a block, kept in the caller's allocator.
struct Tables {
    lit: Huffman,
    dist: Huffman,
    pre: Huffman,
    /// The lengths read so far: the precode's 19, then the two codes' together.
    pre_lens: [u8; 19],
    lens: [u8; 286 + 32],
    /// Whether `lit` and `dist` hold the fixed code now (so a fixed block builds nothing).
    fixed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Header,
    StoredHeader,
    Stored { left: u32 },
    TableHeader,
    Precode { have: u16 },
    Lengths { have: u16 },
    Codes,
    Done,
}

/// A streaming inflater: see the module.
pub struct Inflate<A: Allocator> {
    tables: Box<Tables, A>,
    window: Vec<u8, A>,
    /// Bytes written: the output's length so far.
    written: u64,
    /// Bytes of the window already appended to the caller's output.
    flushed: u64,
    bits: u64,
    nbits: u32,
    state: State,
    last_block: bool,
    hlit: u16,
    hdist: u16,
    hclen: u16,
    /// Bytes of input taken.
    taken: u64,
    limits: Limits,
    /// Whole bytes the bit buffer read past the final block.
    tail: [u8; 8],
    tail_len: u8,
}

impl<A: Allocator + Clone> Inflate<A> {
    /// An inflater held to `limits`, its window and tables in `alloc`.
    pub fn new_in(limits: Limits, alloc: A) -> Inflate<A> {
        let tables = Tables {
            lit: Huffman::EMPTY,
            dist: Huffman::EMPTY,
            pre: Huffman::EMPTY,
            pre_lens: [0; 19],
            lens: [0; 286 + 32],
            fixed: false,
        };
        Inflate {
            tables: Box::new_in(tables, alloc.clone()),
            window: Vec::from_elem_in(0u8, WINDOW, alloc),
            written: 0,
            flushed: 0,
            bits: 0,
            nbits: 0,
            state: State::Header,
            last_block: false,
            hlit: 0,
            hdist: 0,
            hclen: 0,
            taken: 0,
            limits,
            tail: [0; 8],
            tail_len: 0,
        }
    }
}

impl<A: Allocator> Inflate<A> {
    /// Takes what it can of `input`, appending the output it makes to `out`. Once the stream has
    /// ended nothing more is taken. On an error the stream is refused for good: `out` holds what
    /// was made before it, which the caller must not take as the stream's output.
    pub fn feed<B: Allocator>(&mut self, input: &[u8], out: &mut Vec<u8, B>) -> Result<Fed, Error> {
        let mut at = 0usize;
        let ran = self.run(input, &mut at, out);
        let flushed = self.flush(out);
        ran.and(flushed).map_err(|why| Error::new(Format::Deflate, self.taken, why))?;
        Ok(Fed { consumed: at, done: self.state == State::Done })
    }

    /// Whether the final block has ended.
    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// The input ended: an error unless the stream did.
    pub fn finish(&self) -> Result<(), Error> {
        match self.state {
            State::Done => Ok(()),
            _ => Err(Error::new(Format::Deflate, self.taken, Why::Truncated)),
        }
    }

    /// The whole bytes read past the final block: the next thing's (see the module).
    pub fn tail(&self) -> &[u8] {
        &self.tail[..usize::from(self.tail_len)]
    }

    /// Bytes of output so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Bytes of input taken so far, the bit buffer's included.
    pub fn taken(&self) -> u64 {
        self.taken
    }

    fn refill(&mut self, input: &[u8], at: &mut usize) {
        while self.nbits <= 56 && *at < input.len() {
            self.bits |= u64::from(input[*at]) << self.nbits;
            self.nbits += 8;
            *at += 1;
            self.taken += 1;
        }
    }

    /// The low `n` bits (at most 32), taken.
    fn take(&mut self, n: u32) -> u32 {
        let value = (self.bits & ((1u64 << n) - 1)) as u32;
        self.bits >>= n;
        self.nbits -= n;
        value
    }

    fn end_block(&mut self) {
        if !self.last_block {
            self.state = State::Header;
            return;
        }
        self.state = State::Done;
        let partial = self.nbits % 8;
        self.take(partial);
        let whole = self.nbits / 8;
        for i in 0..whole {
            self.tail[i as usize] = (self.bits >> (8 * i)) as u8;
        }
        self.tail_len = whole as u8;
        self.bits = 0;
        self.nbits = 0;
    }

    /// Whether `more` bytes of output are within the limits.
    fn room(&self, more: u64) -> Result<(), Why> {
        let output = self.written + more;
        if output > self.limits.most_out {
            return Err(Why::OutputOver { most: self.limits.most_out });
        }
        let most = self.taken.saturating_mul(self.limits.most_ratio).saturating_add(RATIO_SLACK);
        if output > most && !plant::on(Plant::RatioUnchecked) {
            return Err(Why::RatioOver { input: self.taken, output, most_ratio: self.limits.most_ratio });
        }
        Ok(())
    }

    /// The window's bytes not yet in `out`, appended.
    fn flush<B: Allocator>(&mut self, out: &mut Vec<u8, B>) -> Result<(), Why> {
        let pending = (self.written - self.flushed) as usize;
        if pending == 0 {
            return Ok(());
        }
        let start = (self.flushed as usize) & WMASK;
        out.try_reserve(pending).map_err(Why::Alloc)?;
        if start + pending <= WINDOW {
            out.extend_from_slice(&self.window[start..start + pending]);
        } else {
            out.extend_from_slice(&self.window[start..]);
            out.extend_from_slice(&self.window[..start + pending - WINDOW]);
        }
        self.flushed = self.written;
        Ok(())
    }

    /// Flushes when fewer than one match's bytes of the window are free.
    fn make_room<B: Allocator>(&mut self, out: &mut Vec<u8, B>) -> Result<(), Why> {
        if (self.written - self.flushed) as usize > WINDOW - MAX_MATCH {
            self.flush(out)?;
        }
        Ok(())
    }

    fn put_byte(&mut self, byte: u8) {
        self.window[(self.written as usize) & WMASK] = byte;
        self.written += 1;
    }

    fn put_match(&mut self, distance: usize, length: usize) {
        let dst = (self.written as usize) & WMASK;
        let src = dst.wrapping_sub(distance) & WMASK;
        if distance >= length && src + length <= WINDOW && dst + length <= WINDOW {
            self.window.copy_within(src..src + length, dst);
        } else {
            for i in 0..length {
                self.window[(dst + i) & WMASK] = self.window[(src + i) & WMASK];
            }
        }
        self.written += length as u64;
    }

    fn run<B: Allocator>(&mut self, input: &[u8], at: &mut usize, out: &mut Vec<u8, B>) -> Result<(), Why> {
        loop {
            match self.state {
                State::Done => return Ok(()),
                State::Header => {
                    self.refill(input, at);
                    if self.nbits < 3 {
                        return Ok(());
                    }
                    let header = self.take(3);
                    self.last_block = header & 1 != 0;
                    self.state = match header >> 1 {
                        0 => State::StoredHeader,
                        1 => {
                            let t = &mut *self.tables;
                            if !t.fixed {
                                t.lit.build(&FIXED_LIT_LENS, Tree::Literals)?;
                                t.dist.build(&FIXED_DIST_LENS, Tree::Distances)?;
                                t.fixed = true;
                            }
                            State::Codes
                        }
                        2 => State::TableHeader,
                        _ => return Err(Why::BlockType),
                    };
                }
                State::StoredHeader => {
                    let partial = self.nbits % 8;
                    self.take(partial);
                    self.refill(input, at);
                    if self.nbits < 32 {
                        return Ok(());
                    }
                    let len = self.take(16) as u16;
                    let nlen = self.take(16) as u16;
                    if len != !nlen {
                        return Err(Why::StoredLength { len, nlen });
                    }
                    self.state = State::Stored { left: u32::from(len) };
                }
                State::Stored { left } => {
                    if left == 0 {
                        self.end_block();
                        continue;
                    }
                    self.make_room(out)?;
                    let free = WINDOW - (self.written - self.flushed) as usize;
                    if self.nbits >= 8 {
                        // The bytes the bit buffer read ahead come first, one by one.
                        self.room(1)?;
                        let byte = self.take(8) as u8;
                        self.put_byte(byte);
                        self.state = State::Stored { left: left - 1 };
                        continue;
                    }
                    let n = (left as usize).min(input.len() - *at).min(free);
                    if n == 0 {
                        return Ok(());
                    }
                    self.taken += n as u64;
                    self.room(n as u64)?;
                    let dst = (self.written as usize) & WMASK;
                    let first = n.min(WINDOW - dst);
                    self.window[dst..dst + first].copy_from_slice(&input[*at..*at + first]);
                    self.window[..n - first].copy_from_slice(&input[*at + first..*at + n]);
                    self.written += n as u64;
                    *at += n;
                    self.state = State::Stored { left: left - n as u32 };
                }
                State::TableHeader => {
                    self.refill(input, at);
                    if self.nbits < 14 {
                        return Ok(());
                    }
                    self.hlit = self.take(5) as u16 + 257;
                    self.hdist = self.take(5) as u16 + 1;
                    self.hclen = self.take(4) as u16 + 4;
                    if self.hlit > 286 || self.hdist > 30 {
                        return Err(Why::TooManyCodes { literals: self.hlit, distances: self.hdist });
                    }
                    self.tables.pre_lens = [0; 19];
                    self.state = State::Precode { have: 0 };
                }
                State::Precode { mut have } => {
                    while have < self.hclen {
                        self.refill(input, at);
                        if self.nbits < 3 {
                            self.state = State::Precode { have };
                            return Ok(());
                        }
                        self.tables.pre_lens[PRECODE_ORDER[usize::from(have)]] = self.take(3) as u8;
                        have += 1;
                    }
                    let t = &mut *self.tables;
                    t.pre.build(&t.pre_lens, Tree::Precode)?;
                    self.state = State::Lengths { have: 0 };
                }
                State::Lengths { mut have } => {
                    let total = self.hlit + self.hdist;
                    while have < total {
                        self.refill(input, at);
                        let (symbol, len) = match self.tables.pre.decode(self.bits, self.nbits) {
                            Decoded::Symbol(symbol, len) => (symbol, len),
                            Decoded::More => {
                                self.state = State::Lengths { have };
                                return Ok(());
                            }
                            Decoded::Bad => return Err(Why::BadCode(Tree::Precode)),
                        };
                        if symbol < 16 {
                            self.take(len);
                            self.tables.lens[usize::from(have)] = symbol as u8;
                            have += 1;
                            continue;
                        }
                        let (extra, least) = match symbol {
                            16 => (2, 3),
                            17 => (3, 3),
                            _ => (7, 11),
                        };
                        if self.nbits < len + extra {
                            self.state = State::Lengths { have };
                            return Ok(());
                        }
                        if symbol == 16 && have == 0 {
                            return Err(Why::RepeatFirst);
                        }
                        self.take(len);
                        let count = least + self.take(extra) as u16;
                        if have + count > total {
                            return Err(Why::RepeatPastEnd);
                        }
                        let value = match symbol {
                            16 => self.tables.lens[usize::from(have) - 1],
                            _ => 0,
                        };
                        let from = usize::from(have);
                        self.tables.lens[from..from + usize::from(count)].fill(value);
                        have += count;
                    }
                    let (hlit, hdist) = (usize::from(self.hlit), usize::from(self.hdist));
                    let t = &mut *self.tables;
                    if t.lens[256] == 0 {
                        return Err(Why::NoEndOfBlock);
                    }
                    t.lit.build(&t.lens[..hlit], Tree::Literals)?;
                    t.dist.build(&t.lens[hlit..hlit + hdist], Tree::Distances)?;
                    t.fixed = false;
                    self.state = State::Codes;
                }
                State::Codes => {
                    if !self.codes(input, at, out)? {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// The symbols of a Huffman block, to its end (`true`) or the input's (`false`).
    fn codes<B: Allocator>(&mut self, input: &[u8], at: &mut usize, out: &mut Vec<u8, B>) -> Result<bool, Why> {
        loop {
            self.refill(input, at);
            self.make_room(out)?;
            let (symbol, len) = match self.tables.lit.decode(self.bits, self.nbits) {
                Decoded::Symbol(symbol, len) => (symbol, len),
                Decoded::More => return Ok(false),
                Decoded::Bad => return Err(Why::BadCode(Tree::Literals)),
            };
            if symbol < 256 {
                self.room(1)?;
                self.take(len);
                self.put_byte(symbol as u8);
                continue;
            }
            if symbol == 256 {
                self.take(len);
                self.end_block();
                return Ok(true);
            }
            if symbol > 285 {
                return Err(Why::BadSymbol { tree: Tree::Literals, symbol });
            }
            let code = usize::from(symbol - 257);
            let len_extra = u32::from(LEN_EXTRA[code]);
            let head = len + len_extra;
            if self.nbits < head {
                return Ok(false);
            }
            let length = usize::from(LEN_BASE[code]) + ((self.bits >> len) & ((1 << len_extra) - 1)) as usize;
            let rest = self.bits >> head;
            let have = self.nbits - head;
            let (dsym, dlen) = match self.tables.dist.decode(rest, have) {
                Decoded::Symbol(symbol, len) => (symbol, len),
                Decoded::More => return Ok(false),
                Decoded::Bad => return Err(Why::BadCode(Tree::Distances)),
            };
            if dsym >= 30 {
                return Err(Why::BadSymbol { tree: Tree::Distances, symbol: dsym });
            }
            let dist_extra = u32::from(DIST_EXTRA[usize::from(dsym)]);
            if have < dlen + dist_extra {
                return Ok(false);
            }
            let distance = usize::from(DIST_BASE[usize::from(dsym)]) + ((rest >> dlen) & ((1 << dist_extra) - 1)) as usize;
            if distance as u64 > self.written {
                return Err(Why::TooFar { distance: distance as u32, have: self.written });
            }
            self.room(length as u64)?;
            self.take(head + dlen + dist_extra);
            self.put_match(distance, length);
        }
    }
}
