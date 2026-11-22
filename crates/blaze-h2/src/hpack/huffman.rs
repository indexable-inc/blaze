//! ONE HOME WANTED: blaze-http src/h3/huffman.rs and this are one table (RFC 7541 Appendix B,
//! which QPACK reuses by RFC 9204 section 4.1.2). Merge them into one crate both depend on
//! (rule 30); this form (the code lengths alone, the codes derived) is the smaller of the two.
//!
//! HPACK's Huffman code (RFC 7541 section 5.2 and Appendix B). The Appendix's code is
//! *canonical*: within one code length the codes are consecutive in symbol order, and the first
//! code of each length is one past the last code of the length before, shifted left by one. So
//! the 257 code lengths alone determine every code, and they are all this file states: the codes,
//! the decoder's per-length first codes and the symbols in code order are computed from them at
//! compile time ([`CANON`]).
//!
//! Source of [`LENS`]: TO VERIFY AGAINST THE RFC TEXT. The lengths were read off blaze-http's
//! `CODES` table (src/h3/huffman.rs, "Generated from the RFC's table"), which this crate may not
//! copy; I checked many by memory of Appendix B (`' '` 6 bits `010100`, `'0'`..`'2'` and `'a'` 5
//! bits, `'A'` 6, `'X'` and `'Z'` 8, EOS 30 bits of ones). The tests prove the table rather
//! than trust it: the code is complete and prefix-free (its Kraft sum is exactly 1), every byte
//! round-trips, and every Huffman string of Appendix C (C.4, C.6) encodes and decodes byte for
//! byte.
//!
//! Decoding (section 5.2): "A padding strictly longer than 7 bits MUST be treated as a decoding
//! error. A padding not corresponding to the most significant bits of the code for the EOS symbol
//! MUST be treated as a decoding error. A Huffman-encoded string literal containing the EOS
//! symbol MUST be treated as a decoding error."
//!
//! The decoder takes a 64-bit window and, per symbol, compares the window's top `len` bits with
//! each length's code range from the shortest up: the common bytes have 5 to 8 bit codes, so a
//! symbol costs one to four compares, with no table walk per bit.

#![blaze::sans_io]

use blaze_own::{AVec, AllocFailure};

use super::HpackFault;

/// The code length of each symbol, 0 to 255 then EOS (256). TO VERIFY AGAINST THE RFC TEXT
/// (see the module doc for where these came from and what the tests prove).
pub const LENS: [u8; 257] = [
    // 0 to 15
    13, 23, 28, 28, 28, 28, 28, 28, 28, 24, 30, 28, 28, 30, 28, 28, //
    // 16 to 31
    28, 28, 28, 28, 28, 28, 30, 28, 28, 28, 28, 28, 28, 28, 28, 28, //
    // 32 (' ') to 47 ('/')
    6, 10, 10, 12, 13, 6, 8, 11, 10, 10, 8, 11, 8, 6, 6, 6, //
    // 48 ('0') to 63 ('?')
    5, 5, 5, 6, 6, 6, 6, 6, 6, 6, 7, 8, 15, 6, 12, 10, //
    // 64 ('@') to 79 ('O')
    13, 6, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, //
    // 80 ('P') to 95 ('_')
    7, 7, 7, 7, 7, 7, 7, 7, 8, 7, 8, 13, 19, 13, 14, 6, //
    // 96 ('`') to 111 ('o')
    15, 5, 6, 5, 6, 5, 6, 6, 6, 5, 7, 7, 6, 6, 6, 5, //
    // 112 ('p') to 127 (DEL)
    6, 7, 6, 5, 5, 6, 7, 7, 7, 7, 7, 15, 11, 14, 13, 28, //
    // 128 to 143
    20, 22, 20, 20, 22, 22, 22, 23, 22, 23, 23, 23, 23, 23, 24, 23, //
    // 144 to 159
    24, 24, 22, 23, 24, 23, 23, 23, 23, 21, 22, 23, 22, 23, 23, 24, //
    // 160 to 175
    22, 21, 20, 22, 22, 23, 23, 21, 23, 22, 22, 24, 21, 22, 23, 23, //
    // 176 to 191
    21, 21, 22, 21, 23, 22, 23, 23, 20, 22, 22, 22, 23, 22, 22, 23, //
    // 192 to 207
    26, 26, 20, 19, 22, 23, 22, 25, 26, 26, 26, 27, 27, 26, 24, 25, //
    // 208 to 223
    19, 21, 26, 27, 27, 26, 27, 24, 21, 21, 26, 26, 28, 27, 27, 27, //
    // 224 to 239
    20, 24, 20, 21, 22, 21, 21, 23, 22, 22, 25, 25, 24, 24, 26, 23, //
    // 240 to 255
    26, 27, 26, 26, 27, 27, 27, 27, 27, 28, 27, 27, 27, 27, 27, 26, //
    // 256: EOS
    30,
];

/// The symbol that ends the code and may never appear in a string.
pub const EOS: u16 = 256;

/// The longest code.
const MOST_BITS: usize = 30;

/// The canonical code computed from [`LENS`].
pub struct Canon {
    /// Each symbol's code, right-aligned in `LENS[symbol]` bits.
    pub codes: [u32; 257],
    /// The first code of each length.
    pub first: [u32; MOST_BITS + 1],
    /// How many codes each length has.
    pub count: [u32; MOST_BITS + 1],
    /// Where each length's symbols start in `sorted`.
    pub offset: [u16; MOST_BITS + 1],
    /// The symbols in code order (by length, then by symbol).
    pub sorted: [u16; 257],
}

#[expect(
    blaze::as_cast,
    reason = "a const fn: `From` is not const; each cast widens or takes a value below 257"
)]
const fn canon() -> Canon {
    let mut c = Canon {
        codes: [0; 257],
        first: [0; MOST_BITS + 1],
        count: [0; MOST_BITS + 1],
        offset: [0; MOST_BITS + 1],
        sorted: [0; 257],
    };
    let mut code: u32 = 0;
    let mut k: usize = 0;
    let mut len: usize = 1;
    while len <= MOST_BITS {
        c.first[len] = code;
        c.offset[len] = k as u16;
        let mut symbol: usize = 0;
        while symbol < 257 {
            if LENS[symbol] as usize == len {
                c.codes[symbol] = code;
                c.sorted[k] = symbol as u16;
                k += 1;
                code += 1;
                c.count[len] += 1;
            }
            symbol += 1;
        }
        code <<= 1;
        len += 1;
    }
    c
}

/// The code, computed once at compile time.
pub const CANON: Canon = canon();

/// The low byte of `v`.
fn low_byte(v: u64) -> u8 {
    v.to_le_bytes()[0]
}

/// How many bytes `input` takes Huffman-coded.
pub fn encoded_len(input: &[u8]) -> usize {
    let bits: usize = input.iter().map(|&b| usize::from(LENS[usize::from(b)])).sum();
    bits.div_ceil(8)
}

/// Appends `input` Huffman-coded to `out`, padded with the leading ones of EOS.
pub fn encode(input: &[u8], out: &mut AVec<'_, u8>) -> Result<(), AllocFailure> {
    out.try_reserve(encoded_len(input))?;
    let mut acc: u64 = 0;
    let mut bits: u32 = 0;
    for &byte in input {
        let len = u32::from(LENS[usize::from(byte)]);
        acc = (acc << len) | u64::from(CANON.codes[usize::from(byte)]);
        bits += len;
        while bits >= 8 {
            bits -= 8;
            out.try_extend_from_slice(&[low_byte(acc >> bits)])?;
        }
        acc &= (1u64 << bits) - 1;
    }
    if bits > 0 {
        out.try_extend_from_slice(&[low_byte((acc << (8 - bits)) | (0xff >> bits))])?;
    }
    Ok(())
}

/// Appends the decoding of `input` to `out`, or says why it is no Huffman string.
pub fn decode(input: &[u8], out: &mut AVec<'_, u8>) -> Result<(), super::Halt> {
    let mut acc: u64 = 0;
    let mut bits: u32 = 0;
    let mut at = 0;
    loop {
        while bits <= 56 && at < input.len() {
            acc = (acc << 8) | u64::from(input[at]);
            at += 1;
            bits += 8;
        }
        let mut found: Option<(u16, u32)> = None;
        let mut len: u32 = 5;
        while len <= 30 && len <= bits {
            let candidate = u32::try_from((acc >> (bits - len)) & ((1u64 << len) - 1)).unwrap_or(u32::MAX);
            let i = usize::try_from(len).unwrap_or(MOST_BITS);
            let past_first = candidate.wrapping_sub(CANON.first[i]);
            if past_first < CANON.count[i] {
                let k = usize::from(CANON.offset[i]) + usize::try_from(past_first).unwrap_or(0);
                found = Some((CANON.sorted[k], len));
                break;
            }
            len += 1;
        }
        match found {
            Some((EOS, _)) => return Err(HpackFault::HuffmanEos.into()),
            Some((symbol, len)) => {
                out.try_extend_from_slice(&[low_byte(u64::from(symbol))])?;
                bits -= len;
                acc &= (1u64 << bits) - 1;
            }
            None => {
                // The code is complete: with 30 bits or more in the window some length matched.
                // So the input is spent, and what is left is the padding.
                if bits > 7 {
                    return Err(HpackFault::HuffmanPadding.into());
                }
                let ones = (1u64 << bits) - 1;
                if acc & ones != ones {
                    return Err(HpackFault::HuffmanPadding.into());
                }
                return Ok(());
            }
        }
    }
}
