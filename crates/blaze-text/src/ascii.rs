//! [`Ascii<N>`]: a stack buffer of ASCII bytes filled from either end, read as a `str` with no
//! check. The one `unsafe` of the crate.
//!
//! The invariant: every byte in `bytes[start..end]` is below 0x80. It holds by construction:
//! the only writes are [`Ascii::front`]/[`Ascii::back`], which assert the byte is ASCII (the
//! assert folds away for a literal), and [`Ascii::front_pair`]/[`Ascii::front_digit`], which
//! copy from [`PAIRS`], [`LOWER`] and [`UPPER`], whose bytes are all ASCII (checked at compile
//! time below).

use blaze_own::planted::Planted;

use crate::plant::TextPlant;

/// `"00" "01" .. "99"`: the two digits of `r` at `2 * r`.
pub(crate) const PAIRS: [u8; 200] = {
    let mut out = [0u8; 200];
    let mut r = 0;
    while r < 100 {
        out[2 * r] = b'0' + digit_of(r / 10);
        out[2 * r + 1] = b'0' + digit_of(r % 10);
        r += 1;
    }
    out
};

/// Lowercase digits for radix up to 16.
pub(crate) const LOWER: [u8; 16] = *b"0123456789abcdef";
/// Uppercase digits for radix up to 16.
pub(crate) const UPPER: [u8; 16] = *b"0123456789ABCDEF";

const fn digit_of(d: usize) -> u8 {
    blaze_int::Cast::<usize, u8>::truncate(d)
}

const fn all_ascii(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] >= 0x80 {
            return false;
        }
        i += 1;
    }
    true
}

const _: () = assert!(all_ascii(&PAIRS) && all_ascii(&LOWER) && all_ascii(&UPPER), "the digit tables are ASCII");

/// Up to `N` ASCII bytes, `bytes[start..end]`.
pub(crate) struct Ascii<const N: usize> {
    bytes: [u8; N],
    start: usize,
    end: usize,
}

impl<const N: usize> Ascii<N> {
    /// Empty, to be filled from the end backwards (integers).
    #[inline(always)]
    pub(crate) const fn at_end() -> Ascii<N> {
        Ascii { bytes: [0; N], start: N, end: N }
    }

    /// Empty, to be filled from the start forwards.
    #[inline(always)]
    pub(crate) const fn at_start() -> Ascii<N> {
        Ascii { bytes: [0; N], start: 0, end: 0 }
    }

    /// `byte` before the first.
    ///
    /// # Panics
    ///
    /// When `byte` is not ASCII, or the buffer is full: a bug in the writer that sized it.
    #[inline(always)]
    pub(crate) fn front(&mut self, byte: u8) {
        assert!(byte.is_ascii(), "blaze_text: 0x{byte:02x} is not ASCII");
        self.start -= 1;
        self.bytes[self.start] = byte;
    }

    /// `byte` after the last.
    ///
    /// # Panics
    ///
    /// As [`Ascii::front`].
    #[inline(always)]
    pub(crate) fn back(&mut self, byte: u8) {
        assert!(byte.is_ascii(), "blaze_text: 0x{byte:02x} is not ASCII");
        self.bytes[self.end] = byte;
        self.end += 1;
    }

    /// `bytes`, each ASCII, after the last.
    #[inline]
    pub(crate) fn back_all(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.back(byte);
        }
    }

    /// The two digits of `r` (below 100) before the first, from [`PAIRS`];
    /// [`TextPlant::DigitPairOffByOne`] reads the pair after it.
    #[inline(always)]
    pub(crate) fn front_pair(&mut self, r: usize, plant: impl Planted<TextPlant>) {
        let r = match plant.has(TextPlant::DigitPairOffByOne) {
            true => (r + 1) % 100,
            false => r,
        };
        self.start -= 2;
        self.bytes[self.start] = PAIRS[2 * r];
        self.bytes[self.start + 1] = PAIRS[2 * r + 1];
    }

    /// Digit `d` of `table` before the first.
    #[inline(always)]
    pub(crate) fn front_digit(&mut self, table: &[u8; 16], d: usize) {
        self.start -= 1;
        self.bytes[self.start] = table[d];
    }

    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.end - self.start
    }

    /// The bytes written.
    #[inline(always)]
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes[self.start..self.end]
    }

    #[inline(always)]
    pub(crate) fn as_str(&self) -> &str {
        // SAFETY: every byte of `bytes[start..end]` is ASCII (the module's invariant: each was
        // written by `front`/`back`, which assert it, or copied from a table checked ASCII at
        // compile time), and ASCII is UTF-8.
        unsafe { core::str::from_utf8_unchecked(self.as_bytes()) }
    }
}
