//! The lexer's stage 1 (simdjson's): byte classes 64 at a time, as bitmaps.
//!
//! [`CLASS`] gives every byte six classes: an ASCII identifier byte (`[A-Za-z0-9_]`), ASCII
//! whitespace (`\t\n\x0B\x0C\r `), `\n`, `"` or `\`, `/` or `*`, and a special first byte (one
//! whose token needs more than its run: a digit, `"#'/\` and `` ` ``, a control byte that is not
//! whitespace, DEL, and every byte of 0x80 or more). A kernel turns 64 bytes into the six
//! bitmaps ([`Raw`]), and [`finish`] into a [`Block`]: where a token may start (the first byte
//! of an identifier or whitespace run and every other byte), which of those starts are special,
//! and the `\n`, quote and `/`-or-`*` bitmaps the comments and strings are found by.
//!
//! One kernel is compiled in, chosen by `target_feature` when the crate is built: AVX-512 VBMI
//! (the 128-entry class table in two registers, one `vpermi2b` and six `vptestmb` per 64 bytes),
//! AVX2 (simdjson's nibble tables, four `pshufb` per 32 bytes), SSE2 (compares, for x86-64
//! built without AVX2), NEON (the 128-entry table, `tbl`/`tbx`), and for every other target the
//! table a byte at a time. Every kernel gives the bitmaps the table gives: the tests run each
//! one the CPU has against [`raw_scalar`] on every length and alignment.
//!
//! The crate's unsafe module for the lexer (docs/v2/plan/design/unsafe.md): the vector loads
//! and the calls of `target_feature` functions. The safe reference is [`raw_scalar`].

/// A [`Block`]'s words.
pub(super) const START: usize = 0;
pub(super) const SPECIAL: usize = 1;
pub(super) const NEWLINE: usize = 2;
pub(super) const QUOTE: usize = 3;
pub(super) const SLASH_STAR: usize = 4;

/// The classes of 64 bytes, a bit per byte: [`START`], [`SPECIAL`] (a subset of it),
/// [`NEWLINE`], [`QUOTE`] (`"` or `\`) and [`SLASH_STAR`]. Interleaved, so a step of the lexer
/// reads what it needs of a block from one or two cache lines.
pub(super) type Block = [u64; 5];

const _: () = assert!(size_of::<Block>() == 40, "40 bytes of classes per 64 of text");

/// A kernel's bitmaps of 64 bytes: word `k` has bit `i` set when byte `i` has [`CLASS`] bit
/// `1 << k`.
pub(super) type Raw = [u64; 6];

const IDENT_BIT: u8 = 1 << 0;
const SPACE_BIT: u8 = 1 << 1;
const NEWLINE_BIT: u8 = 1 << 2;
const QUOTE_BIT: u8 = 1 << 3;
const SLASH_STAR_BIT: u8 = 1 << 4;
const SPECIAL_BIT: u8 = 1 << 5;

const R_IDENT: usize = 0;
const R_SPACE: usize = 1;
const R_NEWLINE: usize = 2;
const R_QUOTE: usize = 3;
const R_SLASH_STAR: usize = 4;
const R_SPECIAL: usize = 5;

/// The one-byte tokens `rustc_lexer` lexes from their byte alone: all its punctuation but `#`
/// (`#"` and `##`) and `/` (comments).
pub(super) const SIMPLE_PUNCT: &[u8; 25] = b";,.(){}[]@~?:$=!<>-&|+*^%";

/// The classes of each byte, bit `1 << k` for [`Raw`] word `k`.
pub(super) const CLASS: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut b = 0;
    while b < 256 {
        let c = b as u8;
        let ident = c.is_ascii_alphanumeric() || c == b'_';
        let space = matches!(c, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ');
        let mut punct = false;
        let mut i = 0;
        while i < SIMPLE_PUNCT.len() {
            punct |= SIMPLE_PUNCT[i] == c;
            i += 1;
        }
        let simple_first = c.is_ascii_alphabetic() || c == b'_' || space || punct;
        if ident {
            table[b] |= IDENT_BIT;
        }
        if space {
            table[b] |= SPACE_BIT;
        }
        if c == b'\n' {
            table[b] |= NEWLINE_BIT;
        }
        if c == b'"' || c == b'\\' {
            table[b] |= QUOTE_BIT;
        }
        if c == b'/' || c == b'*' {
            table[b] |= SLASH_STAR_BIT;
        }
        if !simple_first {
            table[b] |= SPECIAL_BIT;
        }
        b += 1;
    }
    table
};

/// [`CLASS`] as simdjson's nibble tables, for kernels whose table lookup is 16 entries wide
/// (`pshufb`): each class is a union of rectangles, a set of low nibbles times a set of high
/// nibbles, and each rectangle a bit of table A or B. A byte's bits in a table are its low
/// nibble's entry and its high nibble's. Fifteen rectangles: (low nibbles, high nibbles, table,
/// bit).
const RECTANGLES: [(u16, u16, usize, u8); 15] = [
    (0x03FF, 1 << 3, 0, 0),                 // 0-9
    (0xFFFE, 1 << 4 | 1 << 6, 0, 1),        // A-O, a-o
    (0x07FF, 1 << 5 | 1 << 7, 0, 2),        // P-Z, p-z
    (1 << 0xF, 1 << 5, 0, 3),               // _
    (0x3E00, 1 << 0, 0, 4),                 // \t \n \x0B \x0C \r
    (1 << 0, 1 << 2, 0, 5),                 // space
    (1 << 0xA, 1 << 0, 0, 6),               // \n
    (1 << 2, 1 << 2, 0, 7),                 // "
    (1 << 0xC, 1 << 5, 1, 0),               // \
    (1 << 0xA | 1 << 0xF, 1 << 2, 1, 1),    // * /
    (0xC1FF, 1 << 0, 1, 2),                 // 0x00-0x08, 0x0E, 0x0F
    (0xFFFF, 0xFF02, 1, 3),                 // 0x10-0x1F, 0x80-0xFF
    (1 << 2 | 1 << 3 | 1 << 7 | 1 << 0xF, 1 << 2, 1, 4), // " # ' /
    (1 << 0, 1 << 6, 1, 5),                 // `
    (1 << 0xF, 1 << 7, 1, 6),               // DEL
];

/// The tables: A by low nibble, A by high nibble, B by low nibble, B by high nibble.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
const NIBBLES: [[u8; 16]; 4] = {
    let mut tables = [[0u8; 16]; 4];
    let mut r = 0;
    while r < RECTANGLES.len() {
        let (low, high, table, bit) = RECTANGLES[r];
        let mut n = 0;
        while n < 16 {
            if (low >> n) & 1 != 0 {
                tables[2 * table][n] |= 1 << bit;
            }
            if (high >> n) & 1 != 0 {
                tables[2 * table + 1][n] |= 1 << bit;
            }
            n += 1;
        }
        r += 1;
    }
    tables
};

/// The bits of tables A and B that make each class, in [`Raw`] order.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
const NIBBLE_CLASSES: [(u8, u8); 6] = [(0x0F, 0), (0x30, 0), (0x40, 0), (0x80, 0x01), (0, 0x02), (0x01, 0x7D)];

const _: () = {
    let mut b = 0;
    while b < 256 {
        let a = NIBBLES[0][b & 15] & NIBBLES[1][b >> 4];
        let other = NIBBLES[2][b & 15] & NIBBLES[3][b >> 4];
        let mut k = 0;
        while k < 6 {
            let (in_a, in_b) = NIBBLE_CLASSES[k];
            let has = (a & in_a) | (other & in_b) != 0;
            assert!(has == ((CLASS[b] >> k) & 1 != 0), "the nibble tables give the class table");
            k += 1;
        }
        b += 1;
    }
};

/// Whether a token starting with `b` is lexed by its run alone (an identifier, whitespace or
/// one-byte punctuation), not by [`super::scan::special`].
#[inline]
pub(super) fn is_simple_first(b: u8) -> bool {
    CLASS[b as usize] & SPECIAL_BIT == 0
}

/// The classes of 64 bytes or fewer, one byte at a time: the reference every kernel matches.
pub(super) fn raw_scalar(bytes: &[u8]) -> Raw {
    debug_assert!(bytes.len() <= 64);
    let mut out = [0u64; 6];
    for (i, &b) in bytes.iter().enumerate() {
        let class = CLASS[b as usize] as u64;
        for (k, word) in out.iter_mut().enumerate() {
            *word |= ((class >> k) & 1) << i;
        }
    }
    out
}

/// A [`Block`] from a kernel's bitmaps. `carry` is the last byte of the block before in an
/// identifier run and in a whitespace run, and becomes this block's.
#[inline(always)]
fn finish(raw: Raw, carry: &mut (u64, u64)) -> Block {
    let (ident, space) = (raw[R_IDENT], raw[R_SPACE]);
    let runs = (ident & !(ident << 1 | carry.0)) | (space & !(space << 1 | carry.1));
    let start = runs | !(ident | space);
    *carry = (ident >> 63, space >> 63);
    [start, start & raw[R_SPECIAL], raw[R_NEWLINE], raw[R_QUOTE], raw[R_SLASH_STAR]]
}

/// The kernel this build has, by name, for the benchmark's bound.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi"))]
pub(super) const KERNEL: &str = "avx512vbmi";
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx2",
    not(all(target_feature = "avx512bw", target_feature = "avx512vbmi"))
))]
pub(super) const KERNEL: &str = "avx2";
#[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
pub(super) const KERNEL: &str = "sse2";
#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
pub(super) const KERNEL: &str = "neon";
#[cfg(not(any(target_arch = "x86_64", all(target_arch = "aarch64", target_feature = "neon"))))]
pub(super) const KERNEL: &str = "scalar";

/// The kernel this build has (see [`KERNEL`]).
#[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi"))]
#[inline(always)]
fn raw_block(chunk: &[u8; 64]) -> Raw {
    // SAFETY: the crate is built for AVX-512BW and VBMI, the `cfg` above.
    unsafe { x86::raw_avx512(chunk) }
}
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx2",
    not(all(target_feature = "avx512bw", target_feature = "avx512vbmi"))
))]
#[inline(always)]
fn raw_block(chunk: &[u8; 64]) -> Raw {
    // SAFETY: the crate is built for AVX2, the `cfg` above.
    unsafe { x86::raw_avx2(chunk) }
}
#[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
#[inline(always)]
fn raw_block(chunk: &[u8; 64]) -> Raw {
    // SAFETY: SSE2 is part of every x86-64 target.
    unsafe { x86::raw_sse2(chunk) }
}
#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline(always)]
fn raw_block(chunk: &[u8; 64]) -> Raw {
    // SAFETY: the crate is built for NEON, the `cfg` above.
    unsafe { arm::raw_neon(chunk) }
}
#[cfg(not(any(target_arch = "x86_64", all(target_arch = "aarch64", target_feature = "neon"))))]
#[inline(always)]
fn raw_block(chunk: &[u8; 64]) -> Raw {
    raw_scalar(chunk)
}

/// Every kernel this CPU can run, the scalar one first, for the differential tests.
#[cfg(test)]
pub(super) fn kernels() -> Vec<(&'static str, fn(&[u8; 64]) -> Raw)> {
    let mut out: Vec<(&'static str, fn(&[u8; 64]) -> Raw)> = vec![("scalar", |c| raw_scalar(c))];
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: SSE2 is part of every x86-64 target.
        out.push(("sse2", |c| unsafe { x86::raw_sse2(c) }));
        if is_x86_feature_detected!("avx2") {
            // SAFETY: listed only after `avx2` was detected.
            out.push(("avx2", |c| unsafe { x86::raw_avx2(c) }));
        }
        if is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512vbmi") {
            // SAFETY: listed only after `avx512bw` and `avx512vbmi` were detected.
            out.push(("avx512vbmi", |c| unsafe { x86::raw_avx512(c) }));
        }
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: listed only after `neon` was detected.
        out.push(("neon", |c| unsafe { arm::raw_neon(c) }));
    }
    out
}

/// The byte classes of a text, a [`Block`] per 64 bytes and one for the rest (empty when the
/// length is a multiple of 64). Bits past the text's end are set in every word, so every search
/// stops at the end and a token never runs past it. 40 bytes per 64 of text, in a buffer the
/// caller lends (the worker's scratch, so the warm loop allocates nothing for them).
#[derive(Clone, Copy)]
pub(super) struct Classes<'b> {
    pub(super) blocks: &'b [Block],
}

impl<'b> Classes<'b> {
    /// The classes of `bytes`, written over `words` (emptied first).
    pub(super) fn new(bytes: &[u8], words: &'b mut Vec<u64>) -> Classes<'b> {
        Classes::with(bytes, raw_block, words)
    }

    pub(super) fn with(
        bytes: &[u8],
        kernel: impl Fn(&[u8; 64]) -> Raw,
        words: &'b mut Vec<u64>,
    ) -> Classes<'b> {
        let (chunks, rest) = bytes.as_chunks::<64>();
        words.clear();
        words.reserve(5 * (chunks.len() + 1));
        let mut carry = (0, 0);
        for chunk in chunks {
            words.extend_from_slice(&finish(kernel(chunk), &mut carry));
        }
        let mut tail = raw_scalar(rest);
        // `rest` is shorter than 64 bytes, so the shift is in range.
        let past = !0u64 << rest.len();
        tail[R_IDENT] &= !past;
        tail[R_SPACE] &= !past;
        for class in [R_NEWLINE, R_QUOTE, R_SLASH_STAR, R_SPECIAL] {
            tail[class] |= past;
        }
        words.extend_from_slice(&finish(tail, &mut carry));
        Classes { blocks: words.as_chunks::<5>().0 }
    }

    /// The first offset at or after `at` (at most the text's length) with a bit in `class`;
    /// the text's length at the latest.
    #[inline]
    pub(super) fn next_bit(&self, class: usize, at: usize) -> usize {
        let mut block = at / 64;
        let found = self.blocks[block][class] >> (at % 64);
        if found != 0 {
            return at + found.trailing_zeros() as usize;
        }
        loop {
            block += 1;
            let found = self.blocks[block][class];
            if found != 0 {
                return block * 64 + found.trailing_zeros() as usize;
            }
        }
    }

    /// The first offset after `at` (below the text's length) where a token may start: the end
    /// of the identifier or whitespace run `at` is in, or `at + 1` after any other byte.
    #[inline]
    pub(super) fn next_start(&self, at: usize) -> usize {
        self.next_bit(START, at + 1)
    }
}

/// Every x86-64 kernel, built whatever the target features, so the tests run each the CPU has;
/// a build uses one.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(test), allow(dead_code))]
mod x86 {
    use std::arch::x86_64::*;

    use super::{
        CLASS, NIBBLE_CLASSES, NIBBLES, R_IDENT, R_NEWLINE, R_QUOTE, R_SLASH_STAR, R_SPACE, R_SPECIAL, Raw,
        SPECIAL_BIT,
    };

    /// `x <= k` per unsigned byte, as 0xFF or 0.
    #[target_feature(enable = "sse2")]
    #[inline]
    fn le_sse2(x: __m128i, k: u8) -> __m128i {
        _mm_cmpeq_epi8(_mm_min_epu8(x, _mm_set1_epi8(k as i8)), x)
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn eq_sse2(x: __m128i, k: u8) -> __m128i {
        _mm_cmpeq_epi8(x, _mm_set1_epi8(k as i8))
    }

    /// The six classes of 16 bytes, a bit per byte.
    #[target_feature(enable = "sse2")]
    #[inline]
    fn quarter_sse2(v: __m128i) -> [u32; 6] {
        let alpha = le_sse2(_mm_sub_epi8(_mm_or_si128(v, _mm_set1_epi8(0x20)), _mm_set1_epi8(b'a' as i8)), 25);
        let digit = le_sse2(_mm_sub_epi8(v, _mm_set1_epi8(b'0' as i8)), 9);
        let ident = _mm_or_si128(_mm_or_si128(alpha, digit), eq_sse2(v, b'_'));
        let blank = _mm_or_si128(le_sse2(_mm_sub_epi8(v, _mm_set1_epi8(b'\t' as i8)), 4), eq_sse2(v, b' '));
        let (dquote, backslash, slash) = (eq_sse2(v, b'"'), eq_sse2(v, b'\\'), eq_sse2(v, b'/'));
        let quote = _mm_or_si128(dquote, backslash);
        let slash_star = _mm_or_si128(slash, eq_sse2(v, b'*'));
        let control = _mm_andnot_si128(blank, _mm_or_si128(le_sse2(v, 0x1F), eq_sse2(v, 0x7F)));
        let marks = _mm_or_si128(
            _mm_or_si128(eq_sse2(v, b'#'), eq_sse2(v, b'\'')),
            _mm_or_si128(eq_sse2(v, b'`'), _mm_or_si128(quote, slash)),
        );
        let special = _mm_or_si128(_mm_or_si128(digit, control), marks);
        [
            _mm_movemask_epi8(ident) as u32,
            _mm_movemask_epi8(blank) as u32,
            _mm_movemask_epi8(eq_sse2(v, b'\n')) as u32,
            _mm_movemask_epi8(quote) as u32,
            _mm_movemask_epi8(slash_star) as u32,
            // A byte of 0x80 or more has its top bit set: `movemask` of the byte itself.
            (_mm_movemask_epi8(special) | _mm_movemask_epi8(v)) as u32,
        ]
    }

    /// [`super::raw_scalar`] on SSE2, 16 bytes per compare.
    ///
    /// # Safety
    ///
    /// The CPU has SSE2 (every x86-64 one does).
    #[target_feature(enable = "sse2")]
    pub(super) unsafe fn raw_sse2(chunk: &[u8; 64]) -> Raw {
        let mut out = [0u64; 6];
        for (q, quarter) in chunk.as_chunks::<16>().0.iter().enumerate() {
            // SAFETY: `quarter` is 16 readable bytes, the load's width, and `loadu` has no
            // alignment requirement.
            let v = unsafe { _mm_loadu_si128(quarter.as_ptr().cast()) };
            for (word, bits) in out.iter_mut().zip(quarter_sse2(v)) {
                *word |= (bits as u64) << (16 * q);
            }
        }
        out
    }

    /// A nibble table in both 128-bit lanes, as `pshufb` looks up within a lane.
    #[target_feature(enable = "avx2")]
    #[inline]
    fn table_avx2(table: &[u8; 16]) -> __m256i {
        // SAFETY: `table` is 16 readable bytes, the load's width, and `loadu` has no alignment
        // requirement.
        _mm256_broadcastsi128_si256(unsafe { _mm_loadu_si128(table.as_ptr().cast()) })
    }

    /// The bytes with a bit of `in_a` in `a` or of `in_b` in `b`, a bit per byte.
    #[target_feature(enable = "avx2")]
    #[inline]
    fn any_avx2(a: __m256i, b: __m256i, (in_a, in_b): (u8, u8)) -> u32 {
        let hits = _mm256_or_si256(
            _mm256_and_si256(a, _mm256_set1_epi8(in_a as i8)),
            _mm256_and_si256(b, _mm256_set1_epi8(in_b as i8)),
        );
        !(_mm256_movemask_epi8(_mm256_cmpeq_epi8(hits, _mm256_setzero_si256())) as u32)
    }

    /// The six classes of 32 bytes, a bit per byte: tables A and B by each byte's two nibbles.
    #[target_feature(enable = "avx2")]
    #[inline]
    fn half_avx2(v: __m256i, tables: &[__m256i; 4]) -> [u32; 6] {
        let nibble = _mm256_set1_epi8(0x0F);
        let low = _mm256_and_si256(v, nibble);
        let high = _mm256_and_si256(_mm256_srli_epi16::<4>(v), nibble);
        let a = _mm256_and_si256(_mm256_shuffle_epi8(tables[0], low), _mm256_shuffle_epi8(tables[1], high));
        let b = _mm256_and_si256(_mm256_shuffle_epi8(tables[2], low), _mm256_shuffle_epi8(tables[3], high));
        let mut out = [0u32; 6];
        for (word, &class) in out.iter_mut().zip(&NIBBLE_CLASSES) {
            *word = any_avx2(a, b, class);
        }
        out
    }

    /// [`super::raw_scalar`] on AVX2 by [`NIBBLES`], 32 bytes per lookup.
    ///
    /// # Safety
    ///
    /// The CPU has AVX2 (the crate is built for it, or the caller detected it).
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn raw_avx2(chunk: &[u8; 64]) -> Raw {
        let tables = [
            table_avx2(&NIBBLES[0]),
            table_avx2(&NIBBLES[1]),
            table_avx2(&NIBBLES[2]),
            table_avx2(&NIBBLES[3]),
        ];
        let (halves, _) = chunk.as_chunks::<32>();
        // SAFETY: `halves[0]` and `halves[1]` are 32 readable bytes each, the load's width, and
        // `loadu` has no alignment requirement.
        let (low, high) = unsafe {
            (_mm256_loadu_si256(halves[0].as_ptr().cast()), _mm256_loadu_si256(halves[1].as_ptr().cast()))
        };
        let (lo, hi) = (half_avx2(low, &tables), half_avx2(high, &tables));
        std::array::from_fn(|k| lo[k] as u64 | (hi[k] as u64) << 32)
    }

    /// [`super::raw_scalar`] on AVX-512 VBMI: [`CLASS`]'s first 128 entries are two registers,
    /// and one two-table permute looks up all 64 bytes; a byte of 0x80 or more is
    /// [`SPECIAL_BIT`] alone, as in the table. Each class is then one `vptestmb`.
    ///
    /// # Safety
    ///
    /// The CPU has AVX-512BW and VBMI (the crate is built for them, or the caller detected
    /// them).
    #[target_feature(enable = "avx512bw,avx512vbmi")]
    pub(super) unsafe fn raw_avx512(chunk: &[u8; 64]) -> Raw {
        let (table, _) = CLASS.as_chunks::<64>();
        // SAFETY: `table[0]` and `table[1]` are 64 readable bytes each, the load's width, and
        // `loadu` has no alignment requirement.
        let (low, high) = unsafe {
            (_mm512_loadu_si512(table[0].as_ptr().cast()), _mm512_loadu_si512(table[1].as_ptr().cast()))
        };
        // SAFETY: `chunk` is 64 readable bytes, as above.
        let v = unsafe { _mm512_loadu_si512(chunk.as_ptr().cast()) };
        let class = _mm512_permutex2var_epi8(low, v, high);
        let class = _mm512_mask_mov_epi8(class, _mm512_movepi8_mask(v), _mm512_set1_epi8(SPECIAL_BIT as i8));
        [
            test_avx512(class, R_IDENT),
            test_avx512(class, R_SPACE),
            test_avx512(class, R_NEWLINE),
            test_avx512(class, R_QUOTE),
            test_avx512(class, R_SLASH_STAR),
            test_avx512(class, R_SPECIAL),
        ]
    }

    /// The bytes of `class` with bit `1 << k`.
    #[target_feature(enable = "avx512bw")]
    #[inline]
    fn test_avx512(class: __m512i, k: usize) -> u64 {
        _mm512_test_epi8_mask(class, _mm512_set1_epi8((1u8 << k) as i8))
    }
}

#[cfg(target_arch = "aarch64")]
#[cfg_attr(not(any(test, target_feature = "neon")), allow(dead_code))]
mod arm {
    use std::arch::aarch64::*;

    use super::{CLASS, Raw, SPECIAL_BIT};

    /// Two classes of 64 bytes, as four 16-byte class vectors: the bytes with `bit_a` and the
    /// bytes with `bit_b`, byte `i` as bit `i`. Each byte keeps its weight `1 << (i % 8)` where
    /// its bit is set, and three rounds of pairwise adds sum eight bytes into one, leaving the
    /// first class in the low 64 bits and the second in the high.
    #[target_feature(enable = "neon")]
    #[inline]
    fn movemask_pair(class: &[uint8x16_t; 4], bit_a: u8, bit_b: u8, weights: uint8x16_t) -> (u64, u64) {
        let a = vpaddq_u8(
            vpaddq_u8(weigh(class[0], bit_a, weights), weigh(class[1], bit_a, weights)),
            vpaddq_u8(weigh(class[2], bit_a, weights), weigh(class[3], bit_a, weights)),
        );
        let b = vpaddq_u8(
            vpaddq_u8(weigh(class[0], bit_b, weights), weigh(class[1], bit_b, weights)),
            vpaddq_u8(weigh(class[2], bit_b, weights), weigh(class[3], bit_b, weights)),
        );
        let both = vreinterpretq_u64_u8(vpaddq_u8(a, b));
        (vgetq_lane_u64::<0>(both), vgetq_lane_u64::<1>(both))
    }

    /// Each byte's weight where it has `bit`, else 0.
    #[target_feature(enable = "neon")]
    #[inline]
    fn weigh(class: uint8x16_t, bit: u8, weights: uint8x16_t) -> uint8x16_t {
        vandq_u8(vtstq_u8(class, vdupq_n_u8(bit)), weights)
    }

    /// The [`CLASS`] bits of 16 bytes, by `tbl` below 64 and `tbx` from 64 to 127.
    #[target_feature(enable = "neon")]
    #[inline]
    fn quarter_neon(v: uint8x16_t, low: uint8x16x4_t, high: uint8x16x4_t) -> uint8x16_t {
        let ascii = vqtbx4q_u8(vqtbl4q_u8(low, v), high, vsubq_u8(v, vdupq_n_u8(64)));
        vorrq_u8(ascii, vandq_u8(vcgeq_u8(v, vdupq_n_u8(0x80)), vdupq_n_u8(SPECIAL_BIT)))
    }

    /// [`super::raw_scalar`] on NEON: [`CLASS`]'s first 128 entries are eight registers, `tbl`
    /// looks up the bytes below 64 and `tbx` those from 64 to 127; a byte of 0x80 or more is
    /// [`SPECIAL_BIT`] alone, as in the table.
    ///
    /// # Safety
    ///
    /// The CPU has NEON (the crate is built for it, or the caller detected it).
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn raw_neon(chunk: &[u8; 64]) -> Raw {
        const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        let (table, _) = CLASS.as_chunks::<64>();
        // SAFETY: `table[0]` and `table[1]` are 64 readable bytes each and `WEIGHTS` 16, the
        // loads' widths, and the loads have no alignment requirement.
        let (low, high, weights) = unsafe {
            (vld1q_u8_x4(table[0].as_ptr()), vld1q_u8_x4(table[1].as_ptr()), vld1q_u8(WEIGHTS.as_ptr()))
        };
        let mut class = [vdupq_n_u8(0); 4];
        for (c, quarter) in class.iter_mut().zip(chunk.as_chunks::<16>().0) {
            // SAFETY: `quarter` is 16 readable bytes, the load's width, and `vld1q_u8` has no
            // alignment requirement.
            *c = quarter_neon(unsafe { vld1q_u8(quarter.as_ptr()) }, low, high);
        }
        let (ident, space) = movemask_pair(&class, 1 << 0, 1 << 1, weights);
        let (newline, quote) = movemask_pair(&class, 1 << 2, 1 << 3, weights);
        let (slash_star, special) = movemask_pair(&class, 1 << 4, 1 << 5, weights);
        [ident, space, newline, quote, slash_star, special]
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::{CLASS, Classes, NEWLINE, QUOTE, SIMPLE_PUNCT, SLASH_STAR, SPECIAL, START, kernels, raw_scalar};

    fn texts() -> Vec<Vec<u8>> {
        let mut rng = SplitMix64::new(7);
        let mut texts: Vec<Vec<u8>> =
            (0..300).map(|len| (0..len).map(|_| rng.index(256) as u8).collect()).collect();
        // Every byte value at every position of a block.
        texts.push((0..=255u8).cycle().take(64 * 257).collect());
        texts.extend(super::super::differential::parser_files().into_iter().take(50).map(String::into_bytes));
        texts
    }

    /// The table says what its doc says, byte by byte.
    #[test]
    fn the_class_table_is_its_definition() {
        for b in 0..=255u8 {
            let class = CLASS[b as usize];
            let space = matches!(b, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ');
            assert_eq!(class & 1 != 0, b.is_ascii_alphanumeric() || b == b'_', "{b:#x}");
            assert_eq!(class & 2 != 0, space, "{b:#x}");
            assert_eq!(class & 4 != 0, b == b'\n', "{b:#x}");
            assert_eq!(class & 8 != 0, b == b'"' || b == b'\\', "{b:#x}");
            assert_eq!(class & 16 != 0, b == b'/' || b == b'*', "{b:#x}");
            let simple = b.is_ascii_alphabetic() || b == b'_' || space || SIMPLE_PUNCT.contains(&b);
            assert_eq!(class & 32 == 0, simple, "{b:#x}");
        }
    }

    /// Every kernel gives the scalar one's bitmaps for every 64 bytes.
    #[test]
    fn every_kernel_classifies_as_the_scalar_one() {
        for text in &texts() {
            for chunk in text.as_chunks::<64>().0 {
                let expected = raw_scalar(chunk);
                for (name, kernel) in kernels() {
                    assert_eq!(kernel(chunk), expected, "{name} on {chunk:?}");
                }
            }
        }
    }

    /// The classes of a text are the same from every kernel, for every length and alignment,
    /// and say what they mean: the next start after any offset is where its run ends, and the
    /// searches find the next `\n`, quote and `/` or `*`, or stop at the end.
    #[test]
    fn the_classes_mean_what_they_say() {
        let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        let space = |b: u8| matches!(b, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ');
        fn find(text: &[u8], at: usize, hit: impl Fn(u8) -> bool) -> usize {
            at + text[at..].iter().take_while(|&&b| !hit(b)).count()
        }
        for text in &texts() {
            for skip in [0, 1, 31, 63] {
                let text = &text[skip.min(text.len())..];
                let mut words = Vec::new();
                let scalar = Classes::with(text, |c| raw_scalar(c), &mut words).blocks.to_vec();
                for (name, kernel) in kernels() {
                    let mut words = vec![!0; 9];
                    let classes = Classes::with(text, kernel, &mut words);
                    assert_eq!(classes.blocks, scalar, "{name} on {} bytes", text.len());
                }
                let classes = Classes { blocks: &scalar };
                for at in 0..text.len() {
                    let b = text[at];
                    let end = if ident(b) {
                        find(text, at, |b| !ident(b))
                    } else if space(b) {
                        find(text, at, |b| !space(b))
                    } else {
                        at + 1
                    };
                    assert_eq!(classes.next_start(at), end, "start after {at} of {}", text.len());
                    let first_of_run = at == 0
                        || !(ident(b) && ident(text[at - 1]) || space(b) && space(text[at - 1]));
                    let bit = |class: usize| classes.blocks[at / 64][class] >> (at % 64) & 1 != 0;
                    assert_eq!(bit(START), first_of_run, "a start at {at}");
                    assert_eq!(bit(SPECIAL), first_of_run && CLASS[b as usize] & 32 != 0, "special at {at}");
                    assert_eq!(classes.next_bit(NEWLINE, at), find(text, at, |b| b == b'\n'));
                    assert_eq!(classes.next_bit(QUOTE, at), find(text, at, |b| b == b'"' || b == b'\\'));
                    assert_eq!(classes.next_bit(SLASH_STAR, at), find(text, at, |b| b == b'/' || b == b'*'));
                }
                let end = text.len();
                for class in [START, SPECIAL, NEWLINE, QUOTE, SLASH_STAR] {
                    assert_eq!(classes.next_bit(class, end), end, "the end stops class {class}");
                }
            }
        }
    }
}
