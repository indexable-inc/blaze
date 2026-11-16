//! The fast decoder's tables: one lookup for every code of at most `MAIN` bits, one more through
//! a subtable for a longer one, each entry packed with all its symbol says.
//!
//! An entry is a `u32`:
//!
//! | bits | holds |
//! |---|---|
//! | 0..4 | the code's length in bits (a subtable pointer: 0) |
//! | 4 | [`LITERAL`]: bits 16..24 are the byte |
//! | 5 | [`SUB`]: bits 16..32 are the subtable's start, bits 8..12 its index bits |
//! | 6 | [`END`]: end of block |
//! | 7 | [`VALUE`]: a length or distance; bits 16..32 its base, bits 8..12 its extra bits |
//!
//! Zero is no code (an incomplete code's holes, symbols 286, 287, 30 and 31): the fast loop
//! stops before it and the careful decoder reports it.
//!
//! Sizes: zlib's `enough` program bounds the main table and its subtables together, over
//! complete codes of at most 15 bits (the only incomplete codes accepted, a single code of one
//! bit or none, need less): 2342 entries for 288 literal/length symbols at 11 bits, 402 for 32
//! distance symbols at 8 bits (the bounds libdeflate also uses).

#![blaze::sans_io]

use crate::tables::{DIST_BASE, DIST_EXTRA, LEN_BASE, LEN_EXTRA, reverse};

pub(crate) const LITERAL: u32 = 1 << 4;
pub(crate) const SUB: u32 = 1 << 5;
pub(crate) const END: u32 = 1 << 6;
pub(crate) const VALUE: u32 = 1 << 7;

pub(crate) const LIT_BITS: u32 = 11;
pub(crate) const DIST_BITS: u32 = 8;
pub(crate) const LIT_ENOUGH: usize = 2342;
pub(crate) const DIST_ENOUGH: usize = 402;

/// The entry of literal/length symbol `symbol`, without its code length.
const fn lit_entry(symbol: usize) -> u32 {
    match symbol {
        0..256 => LITERAL | ((symbol as u32) << 16),
        256 => END,
        257..286 => {
            VALUE | ((LEN_BASE[symbol - 257] as u32) << 16) | ((LEN_EXTRA[symbol - 257] as u32) << 8)
        }
        _ => 0,
    }
}

/// The entry of distance symbol `symbol`, without its code length.
const fn dist_entry(symbol: usize) -> u32 {
    match symbol {
        0..30 => VALUE | ((DIST_BASE[symbol] as u32) << 16) | ((DIST_EXTRA[symbol] as u32) << 8),
        _ => 0,
    }
}

/// The literal/length table of `lens` (a code the careful decoder already accepted).
pub(crate) fn build_lit(lens: &[u8], table: &mut [u32; LIT_ENOUGH]) {
    build(lens, LIT_BITS, table, lit_entry);
}

/// The distance table of `lens` (a code the careful decoder already accepted).
pub(crate) fn build_dist(lens: &[u8], table: &mut [u32; DIST_ENOUGH]) {
    build(lens, DIST_BITS, table, dist_entry);
}

fn build(lens: &[u8], main: u32, table: &mut [u32], entry: fn(usize) -> u32) {
    let size = 1usize << main;
    let mut counts = [0u32; 16];
    for &len in lens {
        counts[usize::from(len)] += 1;
    }
    counts[0] = 0;
    let mut next = [0u32; 16];
    let mut code = 0u32;
    for len in 1..16 {
        code = (code + counts[len - 1]) << 1;
        next[len] = code;
    }
    // Each long code's prefix gets a subtable as deep as its longest code under that prefix.
    let mut deepest = [0u8; 1 << LIT_BITS];
    let mut probe = next;
    for &len in lens {
        if u32::from(len) > main {
            let rev = reverse(probe[usize::from(len)], u32::from(len)) as usize;
            probe[usize::from(len)] += 1;
            let slot = &mut deepest[rev & (size - 1)];
            *slot = (*slot).max(len);
        } else if len != 0 {
            probe[usize::from(len)] += 1;
        }
    }
    table[..size].fill(0);
    let mut free = size;
    for (prefix, &depth) in deepest[..size].iter().enumerate() {
        if depth != 0 {
            let bits = u32::from(depth) - main;
            table[prefix] = SUB | ((free as u32) << 16) | (bits << 8);
            table[free..free + (1 << bits)].fill(0);
            free += 1 << bits;
        }
    }
    for (symbol, &len) in lens.iter().enumerate() {
        if len == 0 {
            continue;
        }
        let len = u32::from(len);
        let rev = reverse(next[len as usize], len) as usize;
        next[len as usize] += 1;
        let e = match entry(symbol) {
            0 => 0,
            e => e | len,
        };
        if len <= main {
            let mut i = rev;
            while i < size {
                table[i] = e;
                i += 1 << len;
            }
        } else {
            let pointer = table[rev & (size - 1)];
            let start = (pointer >> 16) as usize;
            let bits = (pointer >> 8) & 15;
            let mut i = rev >> main;
            while i < 1 << bits {
                table[start + i] = e;
                i += 1 << (len - main);
            }
        }
    }
}
