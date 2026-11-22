//! The LZ4 block format (https://github.com/lz4/lz4/blob/dev/doc/lz4_Block_format.md), which is
//! Parquet's LZ4_RAW: a page is one block, no frame. The tree's earlier LZ4 was `lz4_flex`, a
//! crate from outside, gone with the store's hot tier; this is the same greedy single-pass
//! compressor (a 4096-entry hash of 4-byte sequences, LZ4's level 1) in about a hundred lines.
//!
//! A block is sequences: a token (the literals' length in its high four bits, the match's less
//! four in its low four, 15 meaning more bytes follow, each 255 meaning more again), the literals,
//! the match's offset back (two bytes, 1 to 65 535) and the length's extra bytes. The last
//! sequence is literals only, at least five of them, and the last match starts at least twelve
//! bytes before the end.

use core::alloc::Allocator;

use blaze_int::Int;
use blaze_own::Vec;

/// The least bytes past a match's start before the end: LZ4's MFLIMIT.
const MATCH_ROOM: usize = 12;
/// The bytes a block ends with that are always literals: LZ4's LASTLITERALS.
const LAST_LITERALS: usize = 5;
const HASH_BITS: u32 = 12;
const MOST_OFFSET: usize = 65_535;

/// The most bytes `n` bytes compress to.
pub const fn bound(n: usize) -> usize {
    n + n / 255 + 16
}

fn read4(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn hash(word: u32) -> usize {
    (word.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)).widen_to::<usize>()
}

/// A length past 15, as LZ4 writes it after a token: 255s, then the rest.
fn length<A: Allocator>(out: &mut Vec<u8, A>, mut rest: usize) {
    while rest >= 255 {
        out.push(255);
        rest -= 255;
    }
    out.push(rest.truncate_to::<u8>());
}

/// One sequence: `literals`, then a match of `len` bytes `offset` back (none in the last).
fn sequence<A: Allocator>(out: &mut Vec<u8, A>, literals: &[u8], matched: Option<(usize, usize)>) {
    let lit = literals.len();
    let extra = matched.map_or(0, |(_, len)| len - 4);
    out.push((lit.min(15).truncate_to::<u8>() << 4) | extra.min(15).truncate_to::<u8>());
    if lit >= 15 {
        length(out, lit - 15);
    }
    out.extend_from_slice(literals);
    if let Some((offset, _)) = matched {
        out.extend_from_slice(&offset.truncate_to::<u16>().to_le_bytes());
        if extra >= 15 {
            length(out, extra - 15);
        }
    }
}

/// `input` as one LZ4 block, appended to `out`.
pub fn compress<A: Allocator>(input: &[u8], out: &mut Vec<u8, A>) {
    let n = input.len();
    out.reserve(bound(n));
    let mut anchor = 0;
    if n > MATCH_ROOM {
        let mut table = [0u32; 1 << HASH_BITS];
        let end = n - LAST_LITERALS;
        let mut i = 0;
        while i + MATCH_ROOM <= n {
            let word = read4(input, i);
            let slot = &mut table[hash(word)];
            let mut from = slot.widen_to::<usize>();
            // Below 4 GiB: a page is below 2 GiB.
            *slot = i.truncate_to::<u32>();
            if from < i && i - from <= MOST_OFFSET && read4(input, from) == word {
                let mut len = 4;
                while i + len < end && input[from + len] == input[i + len] {
                    len += 1;
                }
                while i > anchor && from > 0 && input[i - 1] == input[from - 1] {
                    i -= 1;
                    from -= 1;
                    len += 1;
                }
                sequence(out, &input[anchor..i], Some((i - from, len)));
                i += len;
                anchor = i;
            } else {
                // Past a run of misses, step faster (LZ4's acceleration).
                i += 1 + ((i - anchor) >> 6);
            }
        }
    }
    sequence(out, &input[anchor..], None);
}

/// Why an LZ4 block could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Corrupt(pub &'static str);

fn read_length(input: &[u8], at: &mut usize, mut len: usize) -> Result<usize, Corrupt> {
    if len == 15 {
        loop {
            let byte = *input.get(*at).ok_or(Corrupt("a length past the block"))?;
            *at += 1;
            len += usize::from(byte);
            if byte != 255 {
                break;
            }
        }
    }
    Ok(len)
}

/// The LZ4 block `input` into `out`, which it must fill exactly.
pub fn decompress(input: &[u8], out: &mut [u8]) -> Result<(), Corrupt> {
    let (mut i, mut o) = (0, 0);
    loop {
        let token = *input.get(i).ok_or(Corrupt("a block that ends before its last literals"))?;
        i += 1;
        let lit = read_length(input, &mut i, usize::from(token >> 4))?;
        let src = input.get(i..i + lit).ok_or(Corrupt("literals past the block"))?;
        out.get_mut(o..o + lit).ok_or(Corrupt("literals past the page's size"))?.copy_from_slice(src);
        i += lit;
        o += lit;
        if i == input.len() {
            break;
        }
        let offset = input.get(i..i + 2).ok_or(Corrupt("an offset past the block"))?;
        let offset = usize::from(u16::from_le_bytes([offset[0], offset[1]]));
        i += 2;
        if offset == 0 || offset > o {
            return Err(Corrupt("a match before the page's start"));
        }
        let len = read_length(input, &mut i, usize::from(token & 15))? + 4;
        if o + len > out.len() {
            return Err(Corrupt("a match past the page's size"));
        }
        if offset >= len {
            out.copy_within(o - offset..o - offset + len, o);
        } else {
            for at in o..o + len {
                out[at] = out[at - offset];
            }
        }
        o += len;
    }
    if o != out.len() {
        return Err(Corrupt("a block of fewer bytes than the page's size"));
    }
    Ok(())
}
