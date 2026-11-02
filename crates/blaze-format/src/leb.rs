//! LEB128, the one implementation: what DWARF, `.eh_frame`, LSDAs, Mach-O's dyld opcodes and
//! export tries, the wasm binary format and blaze's own cold columns write numbers as.
//!
//! A value is 7 bits a byte, least significant first, the top bit of every byte but the last
//! set. Signed values are two's complement, the last byte's bit 6 their sign.
//!
//! Readers take the bytes and a position, move the position past the number, and leave it where
//! it was on an error. They refuse what doesn't fit: past 64 bits (more than [`MAX_LEN`] bytes,
//! or a 10th byte carrying more than bit 63), or for [`read_u32`] and [`read_i32`] past 32 bits
//! or 5 bytes, as the wasm spec does.
//!
//! The decode has no loop past the first byte: with 8 bytes left, one load finds the last
//! byte from the clear top bits (`trailing_zeros`) and packs the 7-bit groups by three
//! shift-and-mask steps (the unrolled form of protobuf's `ReadVarint64` and `varint-simd`, no
//! BMI2 `pext`, which is slow on AMD before Zen 3). The encode is the same steps backwards and
//! one 8-byte store. docs/v2/plan/lanes/dedup/codec-leb.md gives the bound.

use std::fmt;

/// The most bytes a 64-bit value takes.
pub const MAX_LEN: usize = 10;

/// The bytes a wasm object leaves for an index or an address it relocates: a 32-bit value in
/// the padded form ([`padded_u32`], [`padded_i32`]).
pub const PADDED: usize = 5;

/// The top bit of each byte of a word: set on every byte of a LEB128 but its last.
const STOPS: u64 = 0x8080_8080_8080_8080;

/// The 56 bits 8 bytes of LEB128 hold.
const LOW56: u64 = (1 << 56) - 1;

/// Why bytes are not a LEB128.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LebError {
    /// The bytes end before the number's last byte.
    Truncated,
    /// The number is longer than its type allows, or its value past it.
    Overflow,
}

impl LebError {
    pub const fn as_str(self) -> &'static str {
        match self {
            LebError::Truncated => "a LEB128 cut off by the end of its bytes",
            LebError::Overflow => "a LEB128 past the bits of its type",
        }
    }
}

impl fmt::Display for LebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for LebError {}

/// How many bytes `value` takes unsigned.
#[inline]
pub const fn len_u64(value: u64) -> usize {
    (u64::BITS - (value | 1).leading_zeros()).div_ceil(7) as usize
}

/// How many bytes `value` takes signed: its bits past its sign, and the sign.
#[inline]
pub const fn len_i64(value: i64) -> usize {
    (u64::BITS + 1 - (value ^ (value >> 63)).leading_zeros()).div_ceil(7) as usize
}

/// Spreads the low 56 bits of `value` 7 to a byte, each byte's top bit clear.
#[inline]
const fn spread(value: u64) -> u64 {
    let x = value & LOW56;
    let x = (x & 0x0fff_ffff) | ((x & 0x00ff_ffff_f000_0000) << 4);
    let x = (x & 0x0000_3fff_0000_3fff) | ((x & 0x0fff_c000_0fff_c000) << 2);
    (x & 0x007f_007f_007f_007f) | ((x & 0x3f80_3f80_3f80_3f80) << 1)
}

/// [`spread`] backwards: the low 7 bits of each byte of `word`, packed into 56 bits.
#[inline]
const fn compact(word: u64) -> u64 {
    let x = word & !STOPS;
    let x = (x & 0x007f_007f_007f_007f) | ((x & 0x7f00_7f00_7f00_7f00) >> 1);
    let x = (x & 0x0000_3fff_0000_3fff) | ((x & 0x3fff_0000_3fff_0000) >> 2);
    (x & 0x0000_0000_0fff_ffff) | ((x & 0x0fff_ffff_0000_0000) >> 4)
}

/// The top bits of the first `len - 1` bytes of a word, for `len` from 1 to 8; all 8 past it.
#[inline]
const fn continues(len: usize) -> u64 {
    match len {
        1..=8 => STOPS & ((1u64 << (8 * (len - 1))) - 1),
        _ => STOPS,
    }
}

/// The `len` bytes of `value` (the first `len` of the array): the low 8 in one word, the 9th
/// and 10th after it. `signed` says whether a 10th byte extends bit 63 or holds it.
#[inline]
const fn encode(value: u64, len: usize, signed: bool) -> [u8; MAX_LEN] {
    let word = (spread(value) | continues(len)).to_le_bytes();
    let ninth = ((value >> 56) & 0x7f) as u8 | if len == MAX_LEN { 0x80 } else { 0 };
    let tenth = match signed {
        true => ((value as i64) >> 63) as u8 & 0x7f,
        false => (value >> 63) as u8,
    };
    [word[0], word[1], word[2], word[3], word[4], word[5], word[6], word[7], ninth, tenth]
}

/// Appends the first `len` of `bytes`: one 8-byte store when the vector has room for 8, so a
/// vector sized exactly never grows for it.
#[inline]
fn append(out: &mut Vec<u8>, bytes: &[u8; MAX_LEN], len: usize) {
    if len <= 8 && out.capacity() - out.len() >= 8 {
        let start = out.len();
        out.extend_from_slice(&bytes[..8]);
        out.truncate(start + len);
    } else {
        out.extend_from_slice(&bytes[..len]);
    }
}

/// Appends `value` unsigned.
#[inline]
pub fn write_u64(out: &mut Vec<u8>, value: u64) {
    if value < 0x80 {
        out.push(value as u8);
        return;
    }
    let len = len_u64(value);
    append(out, &encode(value, len, false), len);
}

/// Appends `value` signed.
#[inline]
pub fn write_i64(out: &mut Vec<u8>, value: i64) {
    if (-64..64).contains(&value) {
        out.push(value as u8 & 0x7f);
        return;
    }
    let len = len_i64(value);
    append(out, &encode(value as u64, len, true), len);
}

/// Writes `value` unsigned at the start of `place`, touching only the bytes it takes, and says
/// how many that is. Panics when `place` is shorter than [`len_u64`] of it.
#[inline]
pub fn put_u64(place: &mut [u8], value: u64) -> usize {
    let len = len_u64(value);
    place[..len].copy_from_slice(&encode(value, len, false)[..len]);
    len
}

/// Writes `value` signed at the start of `place`, as [`put_u64`] does.
#[inline]
pub fn put_i64(place: &mut [u8], value: i64) -> usize {
    let len = len_i64(value);
    place[..len].copy_from_slice(&encode(value as u64, len, true)[..len]);
    len
}

/// `value` unsigned in exactly [`PADDED`] bytes: what a wasm object leaves for an index the
/// linker writes.
#[inline]
pub const fn padded_u32(value: u32) -> [u8; PADDED] {
    let word = (spread(value as u64) | continues(PADDED)).to_le_bytes();
    [word[0], word[1], word[2], word[3], word[4]]
}

/// `value` signed in exactly [`PADDED`] bytes, as [`padded_u32`].
#[inline]
pub const fn padded_i32(value: i32) -> [u8; PADDED] {
    let word = (spread(value as i64 as u64) | continues(PADDED)).to_le_bytes();
    [word[0], word[1], word[2], word[3], word[4]]
}

/// Writes `value` unsigned in exactly `place.len()` bytes, 1 to [`MAX_LEN`]: the padded form a
/// patched field keeps its size with. [`LebError::Overflow`] when it takes more, and then
/// nothing is written.
pub fn put_padded_u64(place: &mut [u8], value: u64) -> Result<(), LebError> {
    if place.is_empty() || place.len() > MAX_LEN || len_u64(value) > place.len() {
        return Err(LebError::Overflow);
    }
    let last = place.len() - 1;
    for (at, byte) in place.iter_mut().enumerate() {
        let bits = (value >> (7 * at).min(63)) as u8 & 0x7f;
        *byte = if at == last { bits } else { bits | 0x80 };
    }
    Ok(())
}

/// Writes `value` signed in exactly `place.len()` bytes, as [`put_padded_u64`].
pub fn put_padded_i64(place: &mut [u8], value: i64) -> Result<(), LebError> {
    if place.is_empty() || place.len() > MAX_LEN || len_i64(value) > place.len() {
        return Err(LebError::Overflow);
    }
    let last = place.len() - 1;
    for (at, byte) in place.iter_mut().enumerate() {
        let bits = (value >> (7 * at).min(63)) as u8 & 0x7f;
        *byte = if at == last { bits } else { bits | 0x80 };
    }
    Ok(())
}

/// An unsigned value at the start of `rest`, with how many bytes it took.
#[inline]
fn decode_u64(rest: &[u8]) -> Result<(u64, usize), LebError> {
    match rest.first() {
        Some(&byte) if byte < 0x80 => return Ok((u64::from(byte), 1)),
        None => return Err(LebError::Truncated),
        Some(_) => {}
    }
    let Some(word) = rest.first_chunk::<8>() else {
        return decode_short(rest).map(|(value, len, _)| (value, len));
    };
    let word = u64::from_le_bytes(*word);
    let ends = !word & STOPS;
    if ends != 0 {
        let stop = ends.trailing_zeros();
        return Ok((compact(word & (u64::MAX >> (63 - stop))), (stop / 8 + 1) as usize));
    }
    let low = compact(word);
    let ninth = *rest.get(8).ok_or(LebError::Truncated)?;
    if ninth < 0x80 {
        return Ok((low | (u64::from(ninth) << 56), 9));
    }
    match *rest.get(9).ok_or(LebError::Truncated)? {
        tenth @ 0..=1 => {
            Ok((low | (u64::from(ninth & 0x7f) << 56) | (u64::from(tenth) << 63), 10))
        }
        _ => Err(LebError::Overflow),
    }
}

/// A value in `rest`, fewer than 8 bytes: its bits, its length, and its bit count.
#[inline]
fn decode_short(rest: &[u8]) -> Result<(u64, usize, u32), LebError> {
    let mut value = 0;
    for (at, &byte) in rest.iter().enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * at);
        if byte < 0x80 {
            return Ok((value, at + 1, 7 * (at as u32 + 1)));
        }
    }
    Err(LebError::Truncated)
}

/// A signed value at the start of `rest`, with how many bytes it took.
#[inline]
fn decode_i64(rest: &[u8]) -> Result<(i64, usize), LebError> {
    match rest.first() {
        Some(&byte) if byte < 0x80 => return Ok(((i64::from(byte) << 57) >> 57, 1)),
        None => return Err(LebError::Truncated),
        Some(_) => {}
    }
    let Some(word) = rest.first_chunk::<8>() else {
        let (value, len, bits) = decode_short(rest)?;
        return Ok((((value << (64 - bits)) as i64) >> (64 - bits), len));
    };
    let word = u64::from_le_bytes(*word);
    let ends = !word & STOPS;
    if ends != 0 {
        let stop = ends.trailing_zeros();
        let len = stop / 8 + 1;
        let unused = 64 - 7 * len;
        let value = compact(word & (u64::MAX >> (63 - stop)));
        return Ok((((value << unused) as i64) >> unused, len as usize));
    }
    let low = compact(word);
    let ninth = *rest.get(8).ok_or(LebError::Truncated)?;
    if ninth < 0x80 {
        return Ok(((((low | (u64::from(ninth) << 56)) << 1) as i64) >> 1, 9));
    }
    match *rest.get(9).ok_or(LebError::Truncated)? {
        tenth @ (0 | 0x7f) => {
            let high = (u64::from(ninth & 0x7f) << 56) | (u64::from(tenth & 1) << 63);
            Ok(((low | high) as i64, 10))
        }
        _ => Err(LebError::Overflow),
    }
}

/// Reads an unsigned value at `*at` in `bytes`, moving `*at` past it.
#[inline]
pub fn read_u64(bytes: &[u8], at: &mut usize) -> Result<u64, LebError> {
    let (value, len) = decode_u64(bytes.get(*at..).ok_or(LebError::Truncated)?)?;
    *at += len;
    Ok(value)
}

/// Reads a signed value at `*at` in `bytes`, moving `*at` past it.
#[inline]
pub fn read_i64(bytes: &[u8], at: &mut usize) -> Result<i64, LebError> {
    let (value, len) = decode_i64(bytes.get(*at..).ok_or(LebError::Truncated)?)?;
    *at += len;
    Ok(value)
}

/// Reads an unsigned value of at most 5 bytes and 32 bits, as wasm's `u32`.
#[inline]
pub fn read_u32(bytes: &[u8], at: &mut usize) -> Result<u32, LebError> {
    let (value, len) = decode_u64(bytes.get(*at..).ok_or(LebError::Truncated)?)?;
    let value = u32::try_from(value).ok().filter(|_| len <= PADDED).ok_or(LebError::Overflow)?;
    *at += len;
    Ok(value)
}

/// Reads a signed value of at most 5 bytes and 32 bits, as wasm's `i32`.
#[inline]
pub fn read_i32(bytes: &[u8], at: &mut usize) -> Result<i32, LebError> {
    let (value, len) = decode_i64(bytes.get(*at..).ok_or(LebError::Truncated)?)?;
    let value = i32::try_from(value).ok().filter(|_| len <= PADDED).ok_or(LebError::Overflow)?;
    *at += len;
    Ok(value)
}

/// Moves `*at` past a signed or unsigned value of at most [`MAX_LEN`] bytes, not decoding it.
#[inline]
pub fn skip(bytes: &[u8], at: &mut usize) -> Result<(), LebError> {
    let rest = bytes.get(*at..).ok_or(LebError::Truncated)?;
    let len = match rest.first_chunk::<8>() {
        Some(word) => match !u64::from_le_bytes(*word) & STOPS {
            0 => match rest.get(8..MAX_LEN.min(rest.len())).unwrap_or(&[]) {
                [ninth, ..] if *ninth < 0x80 => 9,
                [_, tenth] if *tenth < 0x80 => 10,
                [_, _] => return Err(LebError::Overflow),
                _ => return Err(LebError::Truncated),
            },
            ends => (ends.trailing_zeros() / 8 + 1) as usize,
        },
        None => rest.iter().position(|&byte| byte < 0x80).ok_or(LebError::Truncated)? + 1,
    };
    *at += len;
    Ok(())
}

#[cfg(test)]
mod tests;
