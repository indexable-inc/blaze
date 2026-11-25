//! UTF-8 decoding one byte at a time, as WHATWG's decoder does: an invalid byte or a sequence
//! cut short yields U+FFFD once per maximal subpart (Unicode 15, "U+FFFD substitution of maximal
//! subparts"), which is what ghostty's UTF8Decoder.zig produces (its test at :141).

/// What one byte gave.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Step {
    /// Mid-sequence: nothing yet.
    More,
    /// A whole code point.
    Char(char),
    /// U+FFFD for a maximal invalid subpart that ended before this byte; the byte was NOT
    /// consumed and must be fed again.
    InvalidRetry,
    /// U+FFFD for an invalid byte, which was consumed.
    Invalid,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Decoder {
    code: u32,
    needed: u8,
    seen: u8,
    lower: u8,
    upper: u8,
}

impl Decoder {
    pub(crate) const fn new() -> Decoder {
        Decoder { code: 0, needed: 0, seen: 0, lower: 0x80, upper: 0xBF }
    }

    /// Whether a sequence is part-way through.
    pub(crate) fn pending(&self) -> bool {
        self.needed != 0
    }

    pub(crate) fn next(&mut self, b: u8) -> Step {
        if self.needed == 0 {
            return match b {
                0x00..=0x7F => Step::Char(char::from(b)),
                0xC2..=0xDF => {
                    self.needed = 1;
                    self.code = u32::from(b & 0x1F);
                    Step::More
                }
                0xE0..=0xEF => {
                    if b == 0xE0 {
                        self.lower = 0xA0;
                    }
                    if b == 0xED {
                        self.upper = 0x9F;
                    }
                    self.needed = 2;
                    self.code = u32::from(b & 0x0F);
                    Step::More
                }
                0xF0..=0xF4 => {
                    if b == 0xF0 {
                        self.lower = 0x90;
                    }
                    if b == 0xF4 {
                        self.upper = 0x8F;
                    }
                    self.needed = 3;
                    self.code = u32::from(b & 0x07);
                    Step::More
                }
                _ => Step::Invalid,
            };
        }
        if b < self.lower || b > self.upper {
            *self = Decoder::new();
            return Step::InvalidRetry;
        }
        self.lower = 0x80;
        self.upper = 0xBF;
        self.code = (self.code << 6) | u32::from(b & 0x3F);
        self.seen += 1;
        if self.seen != self.needed {
            return Step::More;
        }
        let code = self.code;
        *self = Decoder::new();
        // The bounds above admit only scalar values: no surrogates (ED's upper 0x9F), nothing
        // past U+10FFFF (F4's upper 0x8F), no overlongs (E0's and F0's lower bounds).
        match char::from_u32(code) {
            Some(c) => Step::Char(c),
            None => Step::Invalid,
        }
    }
}
