//! zlib (RFC 1950) over deflate (RFC 1951), and CRC-32, for PNG.
//!
//! - **Inflate** is miniz_oxide's no-alloc core (`inflate::core::decompress`) writing into one
//!   buffer the caller sized from the image's header and its limits: the stream cannot make it
//!   write past that buffer, so a hostile stream's output is bounded before it starts.
//! - **Deflate** is written here: greedy LZ77 over a 32 KiB window with hash chains, each block
//!   written as fixed Huffman or stored, whichever is fewer bits. miniz_oxide's compressor keeps
//!   its state in boxes on the global heap; this one keeps every table in the allocator handed
//!   in. Its output is a function of its input alone.
//! - **CRC-32** (ISO-HDLC, PNG's) and **Adler-32** are written here too: a table made as the
//!   crate compiles, and two sums.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::alloc::Allocator;

use miniz_oxide::inflate::{
    TINFLStatus,
    core::{DecompressorOxide, decompress, inflate_flags},
};

use super::ImageError;

/// CRC-32's table (polynomial 0xEDB88320, reflected), made as the crate compiles.
const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

/// A running CRC-32.
pub(crate) struct Crc(u32);

impl Crc {
    pub(crate) fn new() -> Crc {
        Crc(0xFFFF_FFFF)
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = CRC_TABLE[((self.0 ^ u32::from(b)) & 0xFF) as usize] ^ (self.0 >> 8);
        }
    }

    pub(crate) fn finish(&self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

/// Adler-32 of `bytes`.
pub(crate) fn adler32(bytes: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    // 5552 bytes is the most that can be summed before `b` could pass u32 (RFC 1950's NMAX).
    let (mut a, mut b) = (1u32, 0u32);
    for run in bytes.chunks(5552) {
        for &x in run {
            a += u32::from(x);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// One zlib stream inflated into a buffer whose size the caller fixed: fed in pieces (PNG's
/// IDAT chunks), ending once the stream does.
pub(crate) struct Inflate {
    state: DecompressorOxide,
    /// Bytes written into the buffer so far.
    at: usize,
    done: bool,
}

impl Inflate {
    pub(crate) fn new() -> Inflate {
        Inflate { state: DecompressorOxide::new(), at: 0, done: false }
    }

    /// Bytes written so far.
    pub(crate) fn written(&self) -> usize {
        self.at
    }

    /// Whether the stream has ended (and its Adler-32 matched).
    pub(crate) fn done(&self) -> bool {
        self.done
    }

    /// Inflate `input` into `out` after what is already there; `more` says another piece
    /// follows. Writing stops at `out`'s end: a stream that would write past it is an error,
    /// so `out`'s length bounds what any input can make. Bytes after the stream's end are
    /// ignored, as PNG decoders do.
    pub(crate) fn feed(&mut self, input: &[u8], more: bool, out: &mut [u8]) -> Result<(), ImageError> {
        let mut flags = inflate_flags::TINFL_FLAG_PARSE_ZLIB_HEADER
            | inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF
            | inflate_flags::TINFL_FLAG_COMPUTE_ADLER32;
        if more {
            flags |= inflate_flags::TINFL_FLAG_HAS_MORE_INPUT;
        }
        let mut input = input;
        // Each round either takes input, writes output or ends: the input and the room left
        // bound the rounds (rule bounded-progress).
        let mut fuel = input.len().saturating_add(out.len()).saturating_add(2);
        while !self.done {
            let (status, read, wrote) = decompress(&mut self.state, input, out, self.at, flags);
            input = input.get(read..).unwrap_or(&[]);
            self.at = self.at.saturating_add(wrote);
            match status {
                TINFLStatus::Done => self.done = true,
                TINFLStatus::NeedsMoreInput => {
                    if input.is_empty() {
                        return Ok(());
                    }
                    if read == 0 && wrote == 0 {
                        return Err(ImageError::Inflate {
                            why: "the zlib stream stopped taking input with input left",
                        });
                    }
                }
                TINFLStatus::HasMoreOutput => {
                    return Err(ImageError::Inflate {
                        why: "the image data inflates past the size its header gives",
                    });
                }
                TINFLStatus::FailedCannotMakeProgress => {
                    return Err(ImageError::Inflate { why: "the zlib stream ends before its last block" });
                }
                TINFLStatus::Adler32Mismatch => {
                    return Err(ImageError::Inflate { why: "the zlib stream's Adler-32 does not match its data" });
                }
                TINFLStatus::Failed => {
                    return Err(ImageError::Inflate { why: "the zlib stream is not valid deflate" });
                }
                TINFLStatus::BadParam => {
                    return Err(ImageError::Inflate { why: "the zlib stream's header is not deflate with a 32 KiB window" });
                }
                // `TINFLStatus` is non-exhaustive: its `block-boundary` feature adds a status
                // returned only when asked for, which this never asks.
                _ => {
                    return Err(ImageError::Inflate { why: "the inflater stopped at a block boundary it was not asked for" });
                }
            }
            fuel = fuel.saturating_sub(1);
            if fuel == 0 && !self.done {
                return Err(ImageError::Inflate { why: "the inflater made no progress within its bound" });
            }
        }
        Ok(())
    }
}

/// LZ77's window: the farthest back a match may reach.
const WINDOW: usize = 1 << 15;
/// Hash chain heads: one per 15-bit hash of three bytes.
const HASH_SIZE: usize = 1 << 15;
/// The most candidates a match search looks at.
const CHAIN: u32 = 64;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
/// Input bytes a block starts with before it is written; a match may carry it up to
/// `MAX_MATCH - 1` past, so a stored block (at most 65535 bytes) always fits it.
const BLOCK: usize = 1 << 15;
/// A token that is a match: the length less 3 in bits 16..24, the distance less 1 in 0..16.
const MATCH: u32 = 1 << 31;

const LEN_BASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// The index into [`LEN_BASE`] of a match length in `3..=258`.
fn len_index(len: u32) -> usize {
    let mut i = LEN_BASE.len() - 1;
    while i > 0 && u32::from(LEN_BASE[i]) > len {
        i -= 1;
    }
    i
}

/// The index into [`DIST_BASE`] (the distance code) of a distance in `1..=32768`.
fn dist_index(dist: u32) -> usize {
    let mut i = DIST_BASE.len() - 1;
    while i > 0 && u32::from(DIST_BASE[i]) > dist {
        i -= 1;
    }
    i
}

/// The fixed Huffman code of a literal/length symbol (RFC 1951, 3.2.6): its bits, most
/// significant first, and its length.
fn fixed_code(symbol: u32) -> (u32, u32) {
    match symbol {
        0..=143 => (0x30 + symbol, 8),
        144..=255 => (0x190 + symbol - 144, 9),
        256..=279 => (symbol - 256, 7),
        _ => (0xC0 + symbol - 280, 8),
    }
}

/// `code`'s `len` bits in the order deflate sends a Huffman code (first bit lowest).
fn reversed(code: u32, len: u32) -> u32 {
    code.reverse_bits() >> (32 - len)
}

/// The bits a token costs in a fixed Huffman block.
fn fixed_cost(token: u32) -> u64 {
    if token & MATCH == 0 {
        return u64::from(fixed_code(token).1);
    }
    let len = ((token >> 16) & 0xFF) + 3;
    let dist = (token & 0xFFFF) + 1;
    let li = len_index(len);
    let di = dist_index(dist);
    u64::from(fixed_code(257 + li as u32).1) + u64::from(LEN_EXTRA[li]) + 5 + u64::from(DIST_EXTRA[di])
}

/// Bits written first-bit-lowest into a byte vector with room reserved for all of them.
struct Bits<'v, A: Allocator> {
    out: &'v mut Vec<u8, A>,
    acc: u64,
    n: u32,
}

impl<A: Allocator> Bits<'_, A> {
    fn put(&mut self, value: u32, len: u32) {
        self.acc |= u64::from(value) << self.n;
        self.n += len;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// Up to the next byte boundary with zero bits.
    fn align(&mut self) {
        if self.n > 0 {
            self.out.push(self.acc as u8);
            self.acc = 0;
            self.n = 0;
        }
    }

    fn token(&mut self, token: u32) {
        if token & MATCH == 0 {
            let (code, len) = fixed_code(token);
            self.put(reversed(code, len), len);
            return;
        }
        let len = ((token >> 16) & 0xFF) + 3;
        let dist = (token & 0xFFFF) + 1;
        let li = len_index(len);
        let (code, bits) = fixed_code(257 + li as u32);
        self.put(reversed(code, bits), bits);
        self.put(len - u32::from(LEN_BASE[li]), u32::from(LEN_EXTRA[li]));
        let di = dist_index(dist);
        self.put(reversed(di as u32, 5), 5);
        self.put(dist - u32::from(DIST_BASE[di]), u32::from(DIST_EXTRA[di]));
    }
}

/// A vector of `len` zeroes in `alloc`, or the error naming `what`.
pub(crate) fn zeroed<T: Copy + Default, A: Allocator>(
    len: usize,
    alloc: A,
    what: &'static str,
) -> Result<Vec<T, A>, ImageError> {
    let mut v = Vec::new_in(alloc);
    v.try_reserve_exact(len)
        .map_err(|_| ImageError::OutOfMemory { what, asked: len.saturating_mul(size_of::<T>()) })?;
    v.resize(len, T::default());
    Ok(v)
}

/// `data` as one zlib stream, made in `alloc`.
pub(crate) fn compress<A: Allocator + Clone>(data: &[u8], alloc: A) -> Result<Vec<u8, A>, ImageError> {
    let n = data.len();
    // The chain heads and links hold a position plus one; 0 is none.
    let mut head: Vec<usize, A> = zeroed(HASH_SIZE, alloc.clone(), "deflate's hash heads")?;
    let mut prev: Vec<usize, A> = zeroed(WINDOW, alloc.clone(), "deflate's hash chains")?;
    let mut tokens: Vec<u32, A> = Vec::new_in(alloc.clone());
    tokens
        .try_reserve_exact(BLOCK + MAX_MATCH)
        .map_err(|_| ImageError::OutOfMemory { what: "deflate's tokens", asked: (BLOCK + MAX_MATCH) * 4 })?;
    // No block is longer than its stored form (5 header bytes and its input), and every block
    // but the last takes at least BLOCK bytes: with the zlib header and trailer, this is the
    // most the stream can be, so the pushes below never grow the vector.
    let most = n.saturating_add(6usize.saturating_mul(n / BLOCK + 2)).saturating_add(16);
    let mut out = Vec::new_in(alloc);
    out.try_reserve_exact(most).map_err(|_| ImageError::OutOfMemory { what: "the zlib stream", asked: most })?;
    // CMF: deflate with a 32 KiB window; FLG: no dictionary, level 0, and the check bits.
    out.extend_from_slice(&[0x78, 0x01]);
    let mut bits = Bits { out: &mut out, acc: 0, n: 0 };

    let hash = |at: usize| -> usize {
        ((usize::from(data[at]) << 10) ^ (usize::from(data[at + 1]) << 5) ^ usize::from(data[at + 2])) & (HASH_SIZE - 1)
    };
    let mut pos = 0usize;
    loop {
        let start = pos;
        let end = start.saturating_add(BLOCK).min(n);
        tokens.clear();
        while pos < end {
            let (len, dist) = longest(data, pos, &head, &prev, hash);
            let take = if len >= MIN_MATCH {
                tokens.push(MATCH | (((len - MIN_MATCH) as u32) << 16) | ((dist - 1) as u32));
                len
            } else {
                tokens.push(u32::from(data[pos]));
                1
            };
            for at in pos..pos + take {
                if at + MIN_MATCH <= n {
                    let h = hash(at);
                    prev[at & (WINDOW - 1)] = head[h];
                    head[h] = at + 1;
                }
            }
            pos += take;
        }
        let last = pos >= n;
        let fixed: u64 = 3 + tokens.iter().map(|&t| fixed_cost(t)).sum::<u64>() + 7;
        let pad = u64::from((8 - (bits.n + 3) % 8) % 8);
        let stored: u64 = 3 + pad + 32 + 8 * (pos - start) as u64;
        if fixed <= stored {
            bits.put(u32::from(last), 1);
            bits.put(1, 2);
            for &t in tokens.iter() {
                bits.token(t);
            }
            // End of block: symbol 256, seven zero bits.
            bits.put(0, 7);
        } else {
            bits.put(u32::from(last), 1);
            bits.put(0, 2);
            bits.align();
            let len = (pos - start) as u16;
            bits.out.extend_from_slice(&len.to_le_bytes());
            bits.out.extend_from_slice(&(!len).to_le_bytes());
            bits.out.extend_from_slice(&data[start..pos]);
        }
        if last {
            break;
        }
    }
    bits.align();
    out.extend_from_slice(&adler32(data).to_be_bytes());
    Ok(out)
}

/// The longest earlier match for the bytes at `pos` within the window: its length and
/// distance, or a length under [`MIN_MATCH`] for none.
fn longest(
    data: &[u8],
    pos: usize,
    head: &[usize],
    prev: &[usize],
    hash: impl Fn(usize) -> usize,
) -> (usize, usize) {
    if pos + MIN_MATCH > data.len() {
        return (0, 0);
    }
    let most = (data.len() - pos).min(MAX_MATCH);
    let (mut best, mut best_dist) = (0, 0);
    let mut candidate = head[hash(pos)];
    let mut chain = 0;
    while candidate != 0 && chain < CHAIN {
        let at = candidate - 1;
        if at >= pos || pos - at > WINDOW {
            break;
        }
        let len = data[at..at + most].iter().zip(&data[pos..pos + most]).take_while(|(a, b)| a == b).count();
        if len > best {
            best = len;
            best_dist = pos - at;
            if len == most {
                break;
            }
        }
        let next = prev[at & (WINDOW - 1)];
        // Links only go back: a link that does not is a slot the window has reused.
        if next == 0 || next - 1 >= at {
            break;
        }
        candidate = next;
        chain += 1;
    }
    (best, best_dist)
}
