//! DEFLATE's fixed tables (RFC 1951 section 3.2.5 and 3.2.6), shared by both directions.

/// Length codes 257..=285: the least length of each, and its extra bits.
pub(crate) const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
pub(crate) const LEN_EXTRA: [u8; 29] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];

/// Distance codes 0..=29: the least distance of each, and its extra bits.
pub(crate) const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
pub(crate) const DIST_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// The order the code-length code's lengths are sent in.
pub(crate) const PRECODE_ORDER: [usize; 19] =
    [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// The longest match and the shortest.
pub(crate) const MAX_MATCH: usize = 258;
pub(crate) const MIN_MATCH: usize = 3;

/// The window: the farthest a match may reach back.
pub(crate) const WINDOW: usize = 1 << 15;

/// The literal/length symbols a block may use (286, 287 are never sent) and the distance
/// symbols (30, 31 are never sent).
pub(crate) const LIT_SYMBOLS: usize = 286;
pub(crate) const DIST_SYMBOLS: usize = 30;

/// The fixed code's literal/length lengths, all 288 (286 and 287 complete the code).
pub(crate) const FIXED_LIT_LENS: [u8; 288] = fixed_lit_lens();
/// The fixed code's distance lengths, all 32 (30 and 31 complete the code).
pub(crate) const FIXED_DIST_LENS: [u8; 32] = [5; 32];

const fn fixed_lit_lens() -> [u8; 288] {
    let mut lens = [0u8; 288];
    let mut i = 0;
    while i < 288 {
        lens[i] = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
        i += 1;
    }
    lens
}

/// The length code (0..=28, to add to 257) of each match length 3..=258, at `length - 3`:
/// made by walking the codes, so 258 ends as code 28, not code 27's last value (zlib's way).
pub(crate) const LEN_CODE: [u8; 256] = len_codes();

const fn len_codes() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut code = 0;
    while code < 29 {
        let base = LEN_BASE[code] as usize - MIN_MATCH;
        let count = 1usize << LEN_EXTRA[code];
        let mut e = 0;
        while e < count && base + e < 256 {
            table[base + e] = code as u8;
            e += 1;
        }
        code += 1;
    }
    table
}

/// The distance code of `distance` (1..=32768).
pub(crate) const fn dist_code(distance: usize) -> usize {
    let v = distance - 1;
    if v < 4 {
        return v;
    }
    let log = (usize::BITS - 1 - v.leading_zeros()) as usize;
    2 * log + ((v >> (log - 1)) & 1)
}

/// `code`'s low `len` bits reversed: DEFLATE sends a Huffman code from its first bit, and a
/// bit buffer is read from its lowest.
pub(crate) const fn reverse(code: u32, len: u32) -> u32 {
    if len == 0 {
        return 0;
    }
    code.reverse_bits() >> (32 - len)
}
