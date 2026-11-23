//! Strict, zero-dependency RFC 4648 §8 hexadecimal encoding and decoding.
//!
//! - Buffer-first (`encode_into`, `encode_upper_into`, `decode_into`) and fixed-array
//!   (`encode_array`, `decode_array`) forms allocate nothing.
//! - Allocating forms (`encode_in`, `encode_upper_in`, `decode_in`) take the caller's allocator.
//! - Zero panicking slice indexes, zero integer `as` casts, zero unsafe.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String, Vec};

#[inline]
pub(crate) const fn lower_nibble(n: u8) -> u8 {
    match n & 0x0f {
        0 => b'0',
        1 => b'1',
        2 => b'2',
        3 => b'3',
        4 => b'4',
        5 => b'5',
        6 => b'6',
        7 => b'7',
        8 => b'8',
        9 => b'9',
        10 => b'a',
        11 => b'b',
        12 => b'c',
        13 => b'd',
        14 => b'e',
        _ => b'f',
    }
}

#[inline]
const fn upper_nibble(n: u8) -> u8 {
    match n & 0x0f {
        0 => b'0',
        1 => b'1',
        2 => b'2',
        3 => b'3',
        4 => b'4',
        5 => b'5',
        6 => b'6',
        7 => b'7',
        8 => b'8',
        9 => b'9',
        10 => b'A',
        11 => b'B',
        12 => b'C',
        13 => b'D',
        14 => b'E',
        _ => b'F',
    }
}

/// Returned when a caller-provided output slice is shorter than required.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BufferTooSmall {
    pub needed: usize,
    pub available: usize,
}

impl fmt::Display for BufferTooSmall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bstd::hex: output buffer too small (need {} bytes, have {})",
            self.needed, self.available
        )
    }
}

impl core::error::Error for BufferTooSmall {}

/// Why decoding hexadecimal text failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HexError {
    /// Input had an odd byte length.
    OddLength { len: usize },
    /// Input contained a non-hex byte at `offset`.
    InvalidByte { offset: usize, byte: u8 },
    /// Output buffer or fixed array had the wrong length.
    WrongLength { expected: usize, actual: usize },
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl fmt::Display for HexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            HexError::OddLength { len } => write!(f, "bstd::hex: odd hex input length ({len})"),
            HexError::InvalidByte { offset, byte } => {
                write!(f, "bstd::hex: invalid hex byte 0x{byte:02x} at byte offset {offset}")
            }
            HexError::WrongLength { expected, actual } => {
                write!(f, "bstd::hex: expected {expected} decoded bytes, got {actual}")
            }
            HexError::Alloc(err) => write!(f, "bstd::hex: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for HexError {}

/// Number of hex ASCII bytes produced for `raw_len` input bytes.
#[inline]
#[must_use]
pub const fn encoded_len(raw_len: usize) -> usize {
    raw_len * 2
}

/// Number of raw bytes decoded from `hex_len` hex bytes (rounded down; odd lengths fail decode).
#[inline]
#[must_use]
pub const fn decoded_len(hex_len: usize) -> usize {
    hex_len / 2
}

#[inline]
pub(crate) const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn encode_with<'a>(
    bytes: &[u8],
    out: &'a mut [u8],
    nibble: fn(u8) -> u8,
) -> Result<&'a str, BufferTooSmall> {
    let needed = encoded_len(bytes.len());
    let Some(dst) = out.get_mut(..needed) else {
        return Err(BufferTooSmall { needed, available: out.len() });
    };
    for (&b, pair) in bytes.iter().zip(dst.chunks_exact_mut(2)) {
        if let [hi, lo] = pair {
            *hi = nibble(b >> 4);
            *lo = nibble(b & 0x0f);
        }
    }
    Ok(core::str::from_utf8(dst).unwrap_or(""))
}

/// Encode `bytes` as lowercase hex into `out`, returning the written `&str` prefix.
pub fn encode_into<'a>(bytes: &[u8], out: &'a mut [u8]) -> Result<&'a str, BufferTooSmall> {
    encode_with(bytes, out, lower_nibble)
}

/// Encode `bytes` as uppercase hex into `out`, returning the written `&str` prefix.
pub fn encode_upper_into<'a>(bytes: &[u8], out: &'a mut [u8]) -> Result<&'a str, BufferTooSmall> {
    encode_with(bytes, out, upper_nibble)
}

/// Encode `bytes` as lowercase hex into a [`String`] allocated in `alloc`.
pub fn encode_in<A: Allocator>(bytes: &[u8], alloc: A) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(encoded_len(bytes.len()), alloc)?;
    for &b in bytes {
        out.try_push(char::from(lower_nibble(b >> 4)))?;
        out.try_push(char::from(lower_nibble(b & 0x0f)))?;
    }
    Ok(out)
}

/// Encode `bytes` as uppercase hex into a [`String`] allocated in `alloc`.
pub fn encode_upper_in<A: Allocator>(bytes: &[u8], alloc: A) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(encoded_len(bytes.len()), alloc)?;
    for &b in bytes {
        out.try_push(char::from(upper_nibble(b >> 4)))?;
        out.try_push(char::from(upper_nibble(b & 0x0f)))?;
    }
    Ok(out)
}

/// Zero-alloc fixed-size lowercase hex encoding (e.g. `encode_array::<32, 64>(&hash)`).
///
/// If `OUT` differs from `N * 2`, only `min(N, OUT / 2)` bytes are written (zero panic).
pub fn encode_array<const N: usize, const OUT: usize>(bytes: &[u8; N]) -> [u8; OUT] {
    let mut out = [b'0'; OUT];
    for (&b, pair) in bytes.iter().zip(out.chunks_exact_mut(2)) {
        if let [hi, lo] = pair {
            *hi = lower_nibble(b >> 4);
            *lo = lower_nibble(b & 0x0f);
        }
    }
    out
}

/// Decode hex `input` into `out`, returning the decoded byte slice.
pub fn decode_into<'a>(input: &[u8], out: &'a mut [u8]) -> Result<&'a [u8], HexError> {
    decode_into_planted(input, out, false)
}

fn decode_into_planted<'a>(
    input: &[u8],
    out: &'a mut [u8],
    plant_swap_nibbles: bool,
) -> Result<&'a [u8], HexError> {
    if !input.len().is_multiple_of(2) {
        return Err(HexError::OddLength { len: input.len() });
    }
    let needed = input.len() / 2;
    let Some(dst) = out.get_mut(..needed) else {
        return Err(HexError::WrongLength { expected: needed, actual: out.len() });
    };
    for (i, (pair, slot)) in input.chunks_exact(2).zip(dst.iter_mut()).enumerate() {
        let [hi_byte, lo_byte] = *pair else { continue };
        let hi = hex_nibble(hi_byte).ok_or(HexError::InvalidByte { offset: i * 2, byte: hi_byte })?;
        let lo =
            hex_nibble(lo_byte).ok_or(HexError::InvalidByte { offset: i * 2 + 1, byte: lo_byte })?;
        *slot = if plant_swap_nibbles { (lo << 4) | hi } else { (hi << 4) | lo };
    }
    Ok(dst)
}

/// Decode hex `input` into a [`Vec<u8, A>`] allocated in `alloc`.
pub fn decode_in<A: Allocator>(input: &[u8], alloc: A) -> Result<Vec<u8, A>, HexError> {
    if !input.len().is_multiple_of(2) {
        return Err(HexError::OddLength { len: input.len() });
    }
    let needed = input.len() / 2;
    let mut out = Vec::try_with_capacity_in(needed, alloc).map_err(HexError::Alloc)?;
    for (i, pair) in input.chunks_exact(2).enumerate() {
        let [hi_byte, lo_byte] = *pair else { continue };
        let hi = hex_nibble(hi_byte).ok_or(HexError::InvalidByte { offset: i * 2, byte: hi_byte })?;
        let lo =
            hex_nibble(lo_byte).ok_or(HexError::InvalidByte { offset: i * 2 + 1, byte: lo_byte })?;
        out.try_push((hi << 4) | lo).map_err(|r| HexError::Alloc(r.why))?;
    }
    Ok(out)
}

/// Decode exactly `2 * N` hex bytes into a fixed `[u8; N]` array with zero allocation.
pub fn decode_array<const N: usize>(input: &[u8]) -> Result<[u8; N], HexError> {
    if !input.len().is_multiple_of(2) {
        return Err(HexError::OddLength { len: input.len() });
    }
    if input.len() != N * 2 {
        return Err(HexError::WrongLength { expected: N, actual: input.len() / 2 });
    }
    let mut out = [0u8; N];
    decode_into(input, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use blaze_own::{Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config};

    use super::*;

    #[test]
    fn hex_vectors_and_errors() {
        let h = heap();
        assert_eq!(encode_in(b"", h.alloc()).expect("empty").as_str(), "");
        assert_eq!(encode_in(b"\xde\xad\xbe\xef", h.alloc()).expect("lower").as_str(), "deadbeef");
        assert_eq!(
            encode_upper_in(b"\xde\xad\xbe\xef", h.alloc()).expect("upper").as_str(),
            "DEADBEEF"
        );
        assert_eq!(decode_array::<4>(b"DeAdBeEf").expect("mixed"), [0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(encode_array::<4, 8>(&[0x01, 0x23, 0xab, 0xff]), *b"0123abff");
        assert_eq!(decode_in(b"abc", h.alloc()), Err(HexError::OddLength { len: 3 }));
        assert_eq!(
            decode_in(b"00zz", h.alloc()),
            Err(HexError::InvalidByte { offset: 2, byte: b'z' })
        );
    }

    /// The invariant: hex encoding and decoding round-trip arbitrary byte slices exactly.
    #[test]
    fn property_hex_round_trip() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let len = usize::try_from(tape.int_in(0..=64, "len")).unwrap_or(0);
            let mut raw = OwnVec::new_in(h.alloc());
            for _ in 0..len {
                raw.push(u8::try_from(tape.below(256, "byte")).unwrap_or(0));
            }
            let lower = encode_in(raw.as_slice(), h.alloc()).expect("lower");
            let upper = encode_upper_in(raw.as_slice(), h.alloc()).expect("upper");
            assert_eq!(lower.len(), raw.len() * 2);
            assert_eq!(
                decode_in(lower.as_bytes(), h.alloc()).expect("dec lower").as_slice(),
                raw.as_slice()
            );
            assert_eq!(
                decode_in(upper.as_bytes(), h.alloc()).expect("dec upper").as_slice(),
                raw.as_slice()
            );
        });
    }

    #[test]
    fn the_oracle_catches_swapped_nibbles() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let len = usize::try_from(tape.int_in(1..=16, "len")).unwrap_or(1);
            let mut raw = OwnVec::new_in(h.alloc());
            for _ in 0..len {
                raw.push(u8::try_from(tape.below(256, "byte")).unwrap_or(0));
            }
            let enc = encode_in(raw.as_slice(), h.alloc()).expect("enc");
            let mut dec = OwnVec::new_in(h.alloc());
            for _ in 0..len {
                dec.push(0u8);
            }
            let got = decode_into_planted(enc.as_bytes(), dec.as_mut_slice(), plant)
                .map_err(|e| Broke::Oracle(format_in!(h.alloc(), "{e}").as_str().into()))?;
            if got != raw.as_slice() {
                return Err(Broke::Oracle(
                    format_in!(h.alloc(), "expected {:?}, got {got:?}", raw.as_slice())
                        .as_str()
                        .into(),
                ));
            }
            Ok(())
        });
    }
}
