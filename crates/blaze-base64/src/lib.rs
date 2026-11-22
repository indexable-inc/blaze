//! base64 (RFC 4648, sections 4 and 5), in place of the `base64` crate (docs/v2/plan/third-party.md:
//! blaze-http's proxy credentials, blaze-pg's SCRAM, blaze-profile's history manifest).
//!
//! An [`Engine`] is an alphabet and a padding rule: [`STANDARD`], [`STANDARD_NO_PAD`],
//! [`URL_SAFE`] and [`URL_SAFE_NO_PAD`] are the four RFC 4648 names. Encoding writes into the
//! caller's buffer ([`Engine::encode_into`], sized by [`Engine::encoded_len`]) or appends to a
//! [`String<A>`](blaze_own::String) in the allocator the caller names; nothing allocates
//! otherwise.
//!
//! Decoding is canonical, so one text has one meaning: a padded engine requires the padding and
//! an unpadded one refuses it; bits past the last whole byte must be zero; a byte outside the
//! alphabet, a length no encoding has, and padding anywhere but the end are each a
//! [`DecodeError`] naming the byte offset. Line breaks are not skipped (MIME's form is not
//! read): a caller that has them removes them first.
//!
//! The tables and the quantum loop are adapted from makepad/relax (MIT), `libs/base64/src/base64.rs`;
//! its decoder read `=` as a zero symbol anywhere and did not check the trailing bits, both of
//! which are refused here.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

use core::{alloc::Allocator, fmt};

use blaze_own::{
    String, Vec,
    planted::{NoPlant, Planted},
};

mod plant;
#[cfg(test)]
mod tests;

pub use plant::Base64Plant;

/// The value of a byte that is no symbol of an alphabet.
const NOT_A_SYMBOL: u8 = 0xFF;

/// An alphabet of 64 symbols and the table back from a byte to its value.
#[derive(Debug)]
pub struct Alphabet {
    symbols: [u8; 64],
    values: [u8; 256],
}

impl Alphabet {
    /// The alphabet of `symbols`, in value order. Panics at compile time (the constants below
    /// are built in a `const`) when a symbol repeats or is `=`.
    #[expect(
        blaze::as_cast,
        reason = "a `const fn`, where `From` is not callable: a byte widened to an index, and an index below 64 narrowed to the byte of its value"
    )]
    pub const fn new(symbols: &[u8; 64]) -> Alphabet {
        let mut values = [NOT_A_SYMBOL; 256];
        let mut i = 0;
        while i < 64 {
            let symbol = symbols[i];
            assert!(symbol != b'=', "`=` is the padding, never a symbol");
            assert!(values[symbol as usize] == NOT_A_SYMBOL, "a base64 symbol repeats");
            values[symbol as usize] = i as u8;
            i += 1;
        }
        Alphabet { symbols: *symbols, values }
    }

    /// The symbol for the value `v` (its low six bits).
    #[inline]
    fn symbol(&self, v: u8) -> u8 {
        self.symbols[usize::from(v & 0x3F)]
    }
}

/// RFC 4648 section 4: `A`-`Z`, `a`-`z`, `0`-`9`, `+`, `/`.
pub const STANDARD_ALPHABET: Alphabet =
    Alphabet::new(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/");

/// RFC 4648 section 5: as the standard one with `-` and `_` for `+` and `/`.
pub const URL_SAFE_ALPHABET: Alphabet =
    Alphabet::new(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_");

/// Whether an encoding ends in `=` up to a multiple of four symbols.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Padding {
    /// Written on encode, required on decode.
    Padded,
    /// Never written, refused on decode.
    Unpadded,
}

/// An alphabet and a padding rule.
#[derive(Clone, Copy, Debug)]
pub struct Engine {
    alphabet: &'static Alphabet,
    padding: Padding,
}

static STANDARD_TABLE: Alphabet = STANDARD_ALPHABET;
static URL_SAFE_TABLE: Alphabet = URL_SAFE_ALPHABET;

/// The standard alphabet, padded: what HTTP Basic credentials and SCRAM use.
pub const STANDARD: Engine = Engine::new(&STANDARD_TABLE, Padding::Padded);
/// The standard alphabet, unpadded.
pub const STANDARD_NO_PAD: Engine = Engine::new(&STANDARD_TABLE, Padding::Unpadded);
/// The URL-safe alphabet, padded.
pub const URL_SAFE: Engine = Engine::new(&URL_SAFE_TABLE, Padding::Padded);
/// The URL-safe alphabet, unpadded: what a name in a URL or a path uses.
pub const URL_SAFE_NO_PAD: Engine = Engine::new(&URL_SAFE_TABLE, Padding::Unpadded);

/// The caller's buffer is shorter than the output.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutputTooSmall {
    /// The bytes the output needs.
    pub need: usize,
    /// The bytes the buffer has.
    pub have: usize,
}

impl fmt::Display for OutputTooSmall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "base64 output needs {} bytes, the buffer has {}", self.need, self.have)
    }
}

impl core::error::Error for OutputTooSmall {}

/// Why a text is no canonical base64 of an engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    /// The byte at `at` is no symbol of the alphabet.
    NotASymbol { at: usize, byte: u8 },
    /// No encoding is `len` bytes long: a padded one is a multiple of four, an unpadded one is
    /// never one more than a multiple of four.
    Length { len: usize },
    /// `=` at `at`: inside the text, more than two, or in an unpadded engine's input. (A
    /// padded engine's text missing its padding has a length no multiple of four: `Length`.)
    Padding { at: usize },
    /// The symbol at `at` carries bits past the last whole byte that are not zero: another
    /// text means the same bytes, so this one is not canonical.
    TrailingBits { at: usize },
    /// The caller's buffer is shorter than the decoded bytes.
    OutputTooSmall(OutputTooSmall),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DecodeError::NotASymbol { at, byte } => {
                write!(f, "byte {byte:#04x} at offset {at} is no base64 symbol of this alphabet")
            }
            DecodeError::Length { len } => write!(f, "no base64 text of this engine is {len} bytes long"),
            DecodeError::Padding { at } => write!(f, "`=` at offset {at} is not padding this engine allows"),
            DecodeError::TrailingBits { at } => write!(
                f,
                "the base64 symbol at offset {at} sets bits past the last byte (not canonical)"
            ),
            DecodeError::OutputTooSmall(small) => small.fmt(f),
        }
    }
}

impl core::error::Error for DecodeError {}

impl Engine {
    /// An engine of `alphabet` and `padding`.
    pub const fn new(alphabet: &'static Alphabet, padding: Padding) -> Engine {
        Engine { alphabet, padding }
    }

    /// The padding rule.
    pub const fn padding(&self) -> Padding {
        self.padding
    }

    /// The length of the encoding of `len` bytes; `None` past `usize`.
    pub const fn encoded_len(&self, len: usize) -> Option<usize> {
        let Some(full) = (len / 3).checked_mul(4) else { return None };
        let tail = match (len % 3, self.padding) {
            (0, _) => 0,
            (_, Padding::Padded) => 4,
            (1, Padding::Unpadded) => 2,
            _ => 3,
        };
        full.checked_add(tail)
    }

    /// The most bytes a text of `len` bytes decodes to.
    pub const fn decoded_len_most(&self, len: usize) -> usize {
        len / 4 * 3 + match len % 4 {
            2 => 1,
            3 => 2,
            _ => 0,
        }
    }

    /// Writes the encoding of `input` to the start of `out`; the bytes written.
    pub fn encode_into(&self, input: &[u8], out: &mut [u8]) -> Result<usize, OutputTooSmall> {
        self.encode_into_planted(input, out, NoPlant)
    }

    /// [`Engine::encode_into`] with its plant seam: `plant` is [`NoPlant`] where it ships and a
    /// [`Base64Plant`] under the round trip's control (D165).
    pub fn encode_into_planted(
        &self,
        input: &[u8],
        out: &mut [u8],
        plant: impl Planted<Base64Plant>,
    ) -> Result<usize, OutputTooSmall> {
        let need = match self.encoded_len(input.len()) {
            Some(need) => need,
            None => return Err(OutputTooSmall { need: usize::MAX, have: out.len() }),
        };
        let Some(out) = out.get_mut(..need) else {
            return Err(OutputTooSmall { need, have: out.len() });
        };
        let a = self.alphabet;
        let mut chunks = input.chunks_exact(3);
        let mut o = 0;
        for chunk in chunks.by_ref() {
            let (b0, b1, b2) = (chunk[0], chunk[1], chunk[2]);
            let quad = &mut out[o..o + 4];
            quad[0] = a.symbol(b0 >> 2);
            quad[1] = a.symbol((b0 << 4) | (b1 >> 4));
            quad[2] = a.symbol((b1 << 2) | (b2 >> 6));
            quad[3] = a.symbol(b2);
            o += 4;
        }
        match *chunks.remainder() {
            [] => {}
            [b0] => {
                out[o] = a.symbol(b0 >> 2);
                out[o + 1] = a.symbol(b0 << 4);
                o += 2;
            }
            [b0, b1] => {
                out[o] = a.symbol(b0 >> 2);
                out[o + 1] = a.symbol((b0 << 4) | (b1 >> 4));
                let third = b1 << 2;
                let third = if plant.has(Base64Plant::TailSetsBit) { third | 1 } else { third };
                out[o + 2] = a.symbol(third);
                o += 3;
            }
            _ => unreachable!("`chunks_exact(3)` leaves fewer than three"),
        }
        while o < need {
            out[o] = b'=';
            o += 1;
        }
        Ok(need)
    }

    /// Appends the encoding of `input` to `out`.
    pub fn encode_append<A: Allocator>(&self, input: &[u8], out: &mut String<A>) {
        if let Some(need) = self.encoded_len(input.len()) {
            out.reserve(need);
        }
        // 48 bytes are 64 symbols with no padding; the last piece is padded as a whole input.
        let mut buf = [0u8; 64];
        let mut pieces = input.chunks(48).peekable();
        while let Some(piece) = pieces.next() {
            let engine = match pieces.peek() {
                Some(_) => Engine { padding: Padding::Unpadded, ..*self },
                None => *self,
            };
            let n = match engine.encode_into(piece, &mut buf) {
                Ok(n) => n,
                Err(small) => unreachable!("64 symbols hold 48 bytes: {small}"),
            };
            match core::str::from_utf8(&buf[..n]) {
                Ok(text) => out.push_str(text),
                Err(error) => unreachable!("base64 symbols are ASCII: {error}"),
            }
        }
    }

    /// The encoding of `input`, in `alloc`.
    pub fn encode_in<A: Allocator>(&self, input: &[u8], alloc: A) -> String<A> {
        let mut out = String::new_in(alloc);
        self.encode_append(input, &mut out);
        out
    }

    /// Decodes `input` into the start of `out`; the bytes written. Nothing is written past
    /// them; on an error, what `out` holds is unspecified.
    pub fn decode_into(&self, input: &[u8], out: &mut [u8]) -> Result<usize, DecodeError> {
        let symbols = self.symbols_of(input)?;
        let need = self.decoded_len_most(symbols);
        let Some(out) = out.get_mut(..need) else {
            return Err(DecodeError::OutputTooSmall(OutputTooSmall { need, have: out.len() }));
        };
        let values = &self.alphabet.values;
        let value = |at: usize| -> Result<u8, DecodeError> {
            let byte = input[at];
            match values[usize::from(byte)] {
                NOT_A_SYMBOL if byte == b'=' => Err(DecodeError::Padding { at }),
                NOT_A_SYMBOL => Err(DecodeError::NotASymbol { at, byte }),
                v => Ok(v),
            }
        };
        let full = symbols / 4 * 4;
        let mut o = 0;
        let mut i = 0;
        while i < full {
            let (v0, v1, v2, v3) = (value(i)?, value(i + 1)?, value(i + 2)?, value(i + 3)?);
            out[o] = (v0 << 2) | (v1 >> 4);
            out[o + 1] = (v1 << 4) | (v2 >> 2);
            out[o + 2] = (v2 << 6) | v3;
            i += 4;
            o += 3;
        }
        match symbols - full {
            0 => {}
            2 => {
                let (v0, v1) = (value(i)?, value(i + 1)?);
                if v1 & 0x0F != 0 {
                    return Err(DecodeError::TrailingBits { at: i + 1 });
                }
                out[o] = (v0 << 2) | (v1 >> 4);
                o += 1;
            }
            3 => {
                let (v0, v1, v2) = (value(i)?, value(i + 1)?, value(i + 2)?);
                if v2 & 0x03 != 0 {
                    return Err(DecodeError::TrailingBits { at: i + 2 });
                }
                out[o] = (v0 << 2) | (v1 >> 4);
                out[o + 1] = (v1 << 4) | (v2 >> 2);
                o += 2;
            }
            _ => unreachable!("`symbols_of` refuses a last quantum of one symbol"),
        }
        Ok(o)
    }

    /// The decoding of `input`, in `alloc`.
    pub fn decode_in<A: Allocator>(&self, input: &[u8], alloc: A) -> Result<Vec<u8, A>, DecodeError> {
        let symbols = self.symbols_of(input)?;
        let mut out = Vec::new_in(alloc);
        out.resize(self.decoded_len_most(symbols), 0);
        let n = self.decode_into(input, out.as_mut_slice())?;
        out.truncate(n);
        Ok(out)
    }

    /// How many of `input`'s bytes are symbols, the padding checked off the end.
    fn symbols_of(&self, input: &[u8]) -> Result<usize, DecodeError> {
        let len = input.len();
        let symbols = match self.padding {
            Padding::Unpadded => len,
            Padding::Padded => {
                if len % 4 != 0 {
                    return Err(DecodeError::Length { len });
                }
                let pads = input.iter().rev().take(2).take_while(|&&b| b == b'=').count();
                len - pads
            }
        };
        match symbols % 4 {
            1 => Err(DecodeError::Length { len }),
            _ => Ok(symbols),
        }
    }
}
