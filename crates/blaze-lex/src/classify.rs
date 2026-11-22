//! Byte classes of a 64-byte block, one `u64` mask per class (bit `i` for byte `i`).
//!
//! A class is a union of rectangles in the 16 by 16 grid of (high nibble, low nibble). Each
//! rectangle owns one bit of one of two table pairs: the low table maps a low nibble to the
//! bits of the rectangles whose columns hold it, the high table a high nibble to those whose
//! rows hold it, and a byte is in a rectangle when both lookups have its bit. Two lookups and
//! an `&` give a byte's 8 rectangle bits of one pair, for 16 or 32 bytes at once.
//!
//! | pair | bit 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
//! |---|---|---|---|---|---|---|---|---|
//! | A | `A-O a-o` | `P-Z p-z` | `0-9` | `_` | `\t \n VT FF \r` | space | `\n` | `"` |
//! | B | `'` | `/` | `*` | `#` | `\` | `{` | `}` | `[` |
//!
//! `]`, `(` and `)` are a byte compare each, and non-ASCII the top bit. The scalar twin reads
//! the same tables a byte at a time; the tests hold both to a byte predicate per class.

use core::simd::cmp::{SimdPartialEq, SimdPartialOrd};
#[cfg(target_arch = "aarch64")]
use core::simd::num::SimdInt;
use core::simd::{Mask, Simd};

use crate::Cpu;
use crate::cpu::Path;
#[cfg(target_arch = "x86_64")]
use crate::vector::Avx2;
use crate::vector::{Lookup, Portable, table};

// Pair A.
const LETTER_A_O: u8 = 1 << 0;
const LETTER_P_Z: u8 = 1 << 1;
const DIGIT: u8 = 1 << 2;
const UNDERSCORE: u8 = 1 << 3;
const CONTROL_SPACE: u8 = 1 << 4;
const SPACE: u8 = 1 << 5;
const NEWLINE: u8 = 1 << 6;
const DOUBLE_QUOTE: u8 = 1 << 7;
// Pair B.
const SINGLE_QUOTE: u8 = 1 << 0;
const SLASH: u8 = 1 << 1;
const STAR: u8 = 1 << 2;
const HASH: u8 = 1 << 3;
const BACKSLASH: u8 = 1 << 4;
const OPEN_BRACE: u8 = 1 << 5;
const CLOSE_BRACE: u8 = 1 << 6;
const OPEN_BRACKET: u8 = 1 << 7;

/// One rectangle: its bit in its pair, its rows (high nibbles) and columns (low nibbles) as
/// 16-bit sets.
struct Rect {
    pair_b: bool,
    bit: u8,
    rows: u16,
    columns: u16,
}

const fn nibbles(from: u32, to: u32) -> u16 {
    let mut set = 0u16;
    let mut n = from;
    while n <= to {
        set |= 1 << n;
        n += 1;
    }
    set
}

const RECTS: [Rect; 16] = [
    Rect { pair_b: false, bit: LETTER_A_O, rows: nibbles(4, 4) | nibbles(6, 6), columns: nibbles(1, 15) },
    Rect { pair_b: false, bit: LETTER_P_Z, rows: nibbles(5, 5) | nibbles(7, 7), columns: nibbles(0, 10) },
    Rect { pair_b: false, bit: DIGIT, rows: nibbles(3, 3), columns: nibbles(0, 9) },
    Rect { pair_b: false, bit: UNDERSCORE, rows: nibbles(5, 5), columns: nibbles(15, 15) },
    Rect { pair_b: false, bit: CONTROL_SPACE, rows: nibbles(0, 0), columns: nibbles(9, 13) },
    Rect { pair_b: false, bit: SPACE, rows: nibbles(2, 2), columns: nibbles(0, 0) },
    Rect { pair_b: false, bit: NEWLINE, rows: nibbles(0, 0), columns: nibbles(10, 10) },
    Rect { pair_b: false, bit: DOUBLE_QUOTE, rows: nibbles(2, 2), columns: nibbles(2, 2) },
    Rect { pair_b: true, bit: SINGLE_QUOTE, rows: nibbles(2, 2), columns: nibbles(7, 7) },
    Rect { pair_b: true, bit: SLASH, rows: nibbles(2, 2), columns: nibbles(15, 15) },
    Rect { pair_b: true, bit: STAR, rows: nibbles(2, 2), columns: nibbles(10, 10) },
    Rect { pair_b: true, bit: HASH, rows: nibbles(2, 2), columns: nibbles(3, 3) },
    Rect { pair_b: true, bit: BACKSLASH, rows: nibbles(5, 5), columns: nibbles(12, 12) },
    Rect { pair_b: true, bit: OPEN_BRACE, rows: nibbles(7, 7), columns: nibbles(11, 11) },
    Rect { pair_b: true, bit: CLOSE_BRACE, rows: nibbles(7, 7), columns: nibbles(13, 13) },
    Rect { pair_b: true, bit: OPEN_BRACKET, rows: nibbles(5, 5), columns: nibbles(11, 11) },
];

/// The four tables: `[low A, high A, low B, high B]`, built from [`RECTS`].
const TABLES: [[u8; 16]; 4] = {
    let mut tables = [[0u8; 16]; 4];
    let mut r = 0;
    while r < RECTS.len() {
        let rect = &RECTS[r];
        let (low, high) = if rect.pair_b { (2, 3) } else { (0, 1) };
        let mut n = 0;
        while n < 16 {
            if rect.columns & (1 << n) != 0 {
                tables[low][n] |= rect.bit;
            }
            if rect.rows & (1 << n) != 0 {
                tables[high][n] |= rect.bit;
            }
            n += 1;
        }
        r += 1;
    }
    tables
};

/// What a class is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sel {
    /// The bytes with any of these rectangle bits, of pair A and of pair B.
    Table { a: u8, b: u8 },
    /// The one byte.
    Byte(u8),
    /// The bytes at or above 0x80.
    NonAscii,
}

impl Sel {
    /// The scalar form, given the byte's rectangle bits.
    #[inline(always)]
    fn holds(self, byte: u8, a: u8, b: u8) -> bool {
        match self {
            Sel::Table { a: in_a, b: in_b } => (a & in_a) | (b & in_b) != 0,
            Sel::Byte(it) => byte == it,
            Sel::NonAscii => byte >= 0x80,
        }
    }
}

const fn uses_a<const K: usize>(sels: &[Sel; K]) -> bool {
    let mut i = 0;
    while i < K {
        if let Sel::Table { a, .. } = sels[i] && a != 0 {
            return true;
        }
        i += 1;
    }
    false
}

const fn uses_b<const K: usize>(sels: &[Sel; K]) -> bool {
    let mut i = 0;
    while i < K {
        if let Sel::Table { b, .. } = sels[i] && b != 0 {
            return true;
        }
        i += 1;
    }
    false
}

/// A fixed list of classes, as a type so that its list is a constant inside a
/// `#[target_feature]` function that cannot be inlined into its caller.
pub(crate) trait Selection<const K: usize> {
    const SELS: [Sel; K];
    const LOW_A: [u8; 16] = TABLES[0];
    const HIGH_A: [u8; 16] = TABLES[1];
    const USES_A: bool = uses_a(&Self::SELS);
    const USES_B: bool = uses_b(&Self::SELS);
}

/// The scalar twin: a byte at a time.
fn pick_scalar<S: Selection<K>, const K: usize>(block: &[u8; 64]) -> [u64; K] {
    let mut out = [0u64; K];
    for (at, &byte) in block.iter().enumerate() {
        let low = usize::from(byte & 0x0F);
        let high = usize::from(byte >> 4);
        let a = if S::USES_A { S::LOW_A[low] & S::HIGH_A[high] } else { 0 };
        let b = if S::USES_B { TABLES[2][low] & TABLES[3][high] } else { 0 };
        for (mask, sel) in out.iter_mut().zip(&S::SELS) {
            if sel.holds(byte, a, b) {
                *mask |= 1 << at;
            }
        }
    }
    out
}

#[inline(always)]
fn eval_sel<const N: usize>(
    sel: Sel,
    bytes: Simd<u8, N>,
    a: Simd<u8, N>,
    b: Simd<u8, N>,
    zero: Simd<u8, N>,
) -> Mask<i8, N> {
    match sel {
        Sel::Table { a: 0xFF, b: 0 } => a.simd_ne(zero),
        Sel::Table { a: in_a, b: in_b } => {
            if in_a != 0 && in_b != 0 {
                ((a & Simd::splat(in_a)) | (b & Simd::splat(in_b))).simd_ne(zero)
            } else if in_a != 0 {
                (a & Simd::splat(in_a)).simd_ne(zero)
            } else {
                (b & Simd::splat(in_b)).simd_ne(zero)
            }
        }
        Sel::Byte(it) => bytes.simd_eq(Simd::splat(it)),
        Sel::NonAscii => bytes.simd_ge(Simd::splat(0x80)),
    }
}

/// Batches four 16-lane masks into one `u64` bitmask (pairwise `addp` tree on NEON, zero unsafe).
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn bitmask64(m0: Mask<i8, 16>, m1: Mask<i8, 16>, m2: Mask<i8, 16>, m3: Mask<i8, 16>) -> u64 {
    const BITS: Simd<u8, 16> = Simd::from_array([
        1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128,
    ]);
    let t0 = m0.to_simd().cast::<u8>() & BITS;
    let t1 = m1.to_simd().cast::<u8>() & BITS;
    let t2 = m2.to_simd().cast::<u8>() & BITS;
    let t3 = m3.to_simd().cast::<u8>() & BITS;
    let (e0, o0) = t0.deinterleave(t1);
    let p0 = e0 + o0;
    let (e1, o1) = t2.deinterleave(t3);
    let p1 = e1 + o1;
    let (e2, o2) = p0.deinterleave(p1);
    let p2 = e2 + o2;
    let (e3, o3) = p2.deinterleave(p2);
    let p3 = e3 + o3;
    let a = p3.to_array();
    u64::from_le_bytes([a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]])
}

/// The 16-lane vector form: processes all 4 chunks of the 64-byte block and extracts one `u64`
/// mask per class in a single batched reduction, skipping unused table pairs at compile time.
#[inline(always)]
fn pick_16<L: Lookup<16>, S: Selection<K>, const K: usize>(lookup: L, block: &[u8; 64]) -> [u64; K] {
    let nibble = Simd::<u8, 16>::splat(0x0F);
    let four = Simd::<u8, 16>::splat(4);
    let zero = Simd::<u8, 16>::splat(0);

    let b0 = Simd::<u8, 16>::from_slice(&block[0..16]);
    let b1 = Simd::<u8, 16>::from_slice(&block[16..32]);
    let b2 = Simd::<u8, 16>::from_slice(&block[32..48]);
    let b3 = Simd::<u8, 16>::from_slice(&block[48..64]);

    let (l0, h0, l1, h1, l2, h2, l3, h3) = if S::USES_A || S::USES_B {
        (
            b0 & nibble,
            b0 >> four,
            b1 & nibble,
            b1 >> four,
            b2 & nibble,
            b2 >> four,
            b3 & nibble,
            b3 >> four,
        )
    } else {
        (zero, zero, zero, zero, zero, zero, zero, zero)
    };

    let (a0, a1, a2, a3) = if S::USES_A {
        let low_a = table::<16>(&S::LOW_A);
        let high_a = table::<16>(&S::HIGH_A);
        (
            lookup.lookup(low_a, l0) & lookup.lookup(high_a, h0),
            lookup.lookup(low_a, l1) & lookup.lookup(high_a, h1),
            lookup.lookup(low_a, l2) & lookup.lookup(high_a, h2),
            lookup.lookup(low_a, l3) & lookup.lookup(high_a, h3),
        )
    } else {
        (zero, zero, zero, zero)
    };

    let (p0, p1, p2, p3) = if S::USES_B {
        let low_b = table::<16>(&TABLES[2]);
        let high_b = table::<16>(&TABLES[3]);
        (
            lookup.lookup(low_b, l0) & lookup.lookup(high_b, h0),
            lookup.lookup(low_b, l1) & lookup.lookup(high_b, h1),
            lookup.lookup(low_b, l2) & lookup.lookup(high_b, h2),
            lookup.lookup(low_b, l3) & lookup.lookup(high_b, h3),
        )
    } else {
        (zero, zero, zero, zero)
    };

    let mut out = [0u64; K];
    let mut k = 0;
    while k < K {
        let sel = S::SELS[k];
        let m0 = eval_sel(sel, b0, a0, p0, zero);
        let m1 = eval_sel(sel, b1, a1, p1, zero);
        let m2 = eval_sel(sel, b2, a2, p2, zero);
        let m3 = eval_sel(sel, b3, a3, p3, zero);
        #[cfg(target_arch = "aarch64")]
        {
            out[k] = bitmask64(m0, m1, m2, m3);
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            out[k] = m0.to_bitmask()
                | (m1.to_bitmask() << 16)
                | (m2.to_bitmask() << 32)
                | (m3.to_bitmask() << 48);
        }
        k += 1;
    }
    out
}

/// The vector form at `N` lanes, `64 / N` vectors a block (used by AVX2 at `N = 32`).
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn pick<L: Lookup<N>, S: Selection<K>, const N: usize, const K: usize>(lookup: L, block: &[u8; 64]) -> [u64; K] {
    let low_a = table::<N>(&S::LOW_A);
    let high_a = table::<N>(&S::HIGH_A);
    let low_b = table::<N>(&TABLES[2]);
    let high_b = table::<N>(&TABLES[3]);
    let nibble = Simd::<u8, N>::splat(0x0F);
    let four = Simd::<u8, N>::splat(4);
    let zero = Simd::<u8, N>::splat(0);
    let mut out = [0u64; K];
    let mut at = 0;
    while at < 64 {
        let bytes = Simd::<u8, N>::from_slice(&block[at..at + N]);
        let low = bytes & nibble;
        let high = bytes >> four;
        let a = if S::USES_A { lookup.lookup(low_a, low) & lookup.lookup(high_a, high) } else { zero };
        let b = if S::USES_B { lookup.lookup(low_b, low) & lookup.lookup(high_b, high) } else { zero };
        let mut k = 0;
        while k < K {
            let lanes = eval_sel(S::SELS[k], bytes, a, b, zero);
            out[k] |= lanes.to_bitmask() << at;
            k += 1;
        }
        at += N;
    }
    out
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn pick_avx2<S: Selection<K>, const K: usize>(avx2: Avx2, block: &[u8; 64]) -> [u64; K] {
    pick::<Avx2, S, 32, K>(avx2, block)
}

/// The masks of the classes `S` lists, on `cpu`'s path.
#[inline(always)]
pub(crate) fn select<S: Selection<K>, const K: usize>(cpu: Cpu, block: &[u8; 64]) -> [u64; K] {
    match cpu.path {
        Path::Scalar => pick_scalar::<S, K>(block),
        Path::Portable => pick_16::<Portable, S, K>(Portable, block),
        #[cfg(target_arch = "x86_64")]
        Path::Avx2(avx2) => {
            // SAFETY: `pick_avx2` needs AVX2, which the `Avx2` value proves.
            unsafe { pick_avx2::<S, K>(avx2, block) }
        }
    }
}

/// The masks of the classes `S` lists for the 64 bytes at `at` of `bytes`, the bytes past the
/// end read as 0, which is in no class.
#[inline(always)]
pub(crate) fn select_at<S: Selection<K>, const K: usize>(cpu: Cpu, bytes: &[u8], at: usize) -> [u64; K] {
    let rest = bytes.get(at..).unwrap_or_default();
    match rest.first_chunk::<64>() {
        Some(block) => select::<S, K>(cpu, block),
        None => {
            let mut block = [0u8; 64];
            block[..rest.len()].copy_from_slice(rest);
            select::<S, K>(cpu, &block)
        }
    }
}

/// Every class of a block, bit `i` of each for byte `i`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Masks {
    /// `0-9 A-Z a-z _`: ASCII XID_Continue.
    pub ident: u64,
    /// `\t \n VT FF \r` and space: ASCII Pattern_White_Space.
    pub whitespace: u64,
    pub digit: u64,
    pub underscore: u64,
    pub double_quote: u64,
    pub single_quote: u64,
    pub slash: u64,
    pub star: u64,
    pub hash: u64,
    pub backslash: u64,
    pub open_paren: u64,
    pub close_paren: u64,
    pub open_brace: u64,
    pub close_brace: u64,
    pub open_bracket: u64,
    pub close_bracket: u64,
    pub newline: u64,
    /// Bytes at or above 0x80: any byte of a non-ASCII character.
    pub non_ascii: u64,
}

struct Every;

impl Selection<18> for Every {
    const SELS: [Sel; 18] = [
        Sel::Table { a: LETTER_A_O | LETTER_P_Z | DIGIT | UNDERSCORE, b: 0 },
        Sel::Table { a: CONTROL_SPACE | SPACE, b: 0 },
        Sel::Table { a: DIGIT, b: 0 },
        Sel::Table { a: UNDERSCORE, b: 0 },
        Sel::Table { a: DOUBLE_QUOTE, b: 0 },
        Sel::Table { a: 0, b: SINGLE_QUOTE },
        Sel::Table { a: 0, b: SLASH },
        Sel::Table { a: 0, b: STAR },
        Sel::Table { a: 0, b: HASH },
        Sel::Table { a: 0, b: BACKSLASH },
        Sel::Byte(b'('),
        Sel::Byte(b')'),
        Sel::Table { a: 0, b: OPEN_BRACE },
        Sel::Table { a: 0, b: CLOSE_BRACE },
        Sel::Table { a: 0, b: OPEN_BRACKET },
        Sel::Byte(b']'),
        Sel::Table { a: NEWLINE, b: 0 },
        Sel::NonAscii,
    ];
}

/// Every class of the 64 bytes of `block`.
pub fn classify(cpu: Cpu, block: &[u8; 64]) -> Masks {
    let [
        ident,
        whitespace,
        digit,
        underscore,
        double_quote,
        single_quote,
        slash,
        star,
        hash,
        backslash,
        open_paren,
        close_paren,
        open_brace,
        close_brace,
        open_bracket,
        close_bracket,
        newline,
        non_ascii,
    ] = select::<Every, 18>(cpu, block);
    Masks {
        ident,
        whitespace,
        digit,
        underscore,
        double_quote,
        single_quote,
        slash,
        star,
        hash,
        backslash,
        open_paren,
        close_paren,
        open_brace,
        close_brace,
        open_bracket,
        close_bracket,
        newline,
        non_ascii,
    }
}

/// The lexer's runs: identifier characters, whitespace, and the digits and `_` of a number.
pub(crate) struct Runs;

pub(crate) const RUN_IDENT: usize = 0;
pub(crate) const RUN_SPACE: usize = 1;
pub(crate) const RUN_DIGITS: usize = 2;

impl Selection<3> for Runs {
    const SELS: [Sel; 3] = [
        Sel::Table { a: LETTER_A_O | LETTER_P_Z | DIGIT | UNDERSCORE, b: 0 },
        Sel::Table { a: CONTROL_SPACE | SPACE, b: 0 },
        Sel::Table { a: DIGIT | UNDERSCORE, b: 0 },
    ];
}

/// The skipper's structure: the bytes that can open or close a brace, a string, a character or
/// lifetime, a comment, or a raw string's or guarded string's `#`.
pub(crate) struct Structure;

/// Dedicated single-pair nibble tables for [`Structure`] (`"` `#` `'` `/` `{` `}`), eliminating
/// Pair-B lookups and extra masking on every 64-byte block of `skip_body`.
const STRUCT_TABLES: [[u8; 16]; 2] = {
    let mut low = [0u8; 16];
    let mut high = [0u8; 16];
    // Bit 0: `"` (0x22), `#` (0x23), `'` (0x27), `/` (0x2F) -> high nibble 2.
    high[2] |= 1 << 0;
    low[2] |= 1 << 0;
    low[3] |= 1 << 0;
    low[7] |= 1 << 0;
    low[15] |= 1 << 0;
    // Bit 1: `{` (0x7B), `}` (0x7D) -> high nibble 7.
    high[7] |= 1 << 1;
    low[11] |= 1 << 1;
    low[13] |= 1 << 1;
    [low, high]
};

impl Selection<1> for Structure {
    const LOW_A: [u8; 16] = STRUCT_TABLES[0];
    const HIGH_A: [u8; 16] = STRUCT_TABLES[1];
    const SELS: [Sel; 1] = [Sel::Table { a: 0xFF, b: 0 }];
}
