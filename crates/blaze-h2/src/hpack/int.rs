//! Integers with an N-bit prefix (RFC 7541 section 5.1):
//!
//! > If the integer value is small enough, i.e., strictly less than 2^N-1, it is encoded within
//! > the N-bit prefix. Otherwise, all the bits of the prefix are set to 1, and the value,
//! > decreased by 2^N-1, is encoded using a list of one or more octets. The most significant bit
//! > of each octet is used as a continuation flag ... The remaining bits of the octets are used
//! > to encode the decreased value, least significant group first.
//!
//! Values are bounded to `u32`: no length, index or size here needs more, and the section asks
//! that "Integer encodings that exceed implementation limits ... MUST be treated as decoding
//! errors".

#![blaze::sans_io]

use blaze_own::{AVec, AllocFailure};

use super::HpackFault;

/// The prefix mask of an `n`-bit prefix, `n` in 1..=8.
const fn mask(n: u8) -> u32 {
    (1u32 << n) - 1
}

/// The low byte of `v`.
fn low_byte(v: u32) -> u8 {
    v.to_le_bytes()[0]
}

/// Appends `value` with an `n`-bit prefix (`n` in 1..=8) to `out`, the bits above the prefix in
/// the first byte taken from `flags`.
pub fn put_int(out: &mut AVec<'_, u8>, flags: u8, n: u8, value: u32) -> Result<(), AllocFailure> {
    let mask = mask(n);
    if value < mask {
        return out.try_extend_from_slice(&[flags | low_byte(value)]);
    }
    let mut bytes = [0u8; 6];
    bytes[0] = flags | low_byte(mask);
    let mut len = 1;
    let mut rest = value - mask;
    while rest >= 128 {
        bytes[len] = low_byte(rest & 0x7f) | 0x80;
        len += 1;
        rest >>= 7;
    }
    bytes[len] = low_byte(rest);
    len += 1;
    out.try_extend_from_slice(&bytes[..len])
}

/// The integer with an `n`-bit prefix starting at `input[*at]`, `*at` moved past it.
pub fn get_int(input: &[u8], at: &mut usize, n: u8) -> Result<u32, HpackFault> {
    let mask = mask(n);
    let Some(&first) = input.get(*at) else {
        return Err(HpackFault::Truncated);
    };
    *at += 1;
    let prefix = u32::from(first) & mask;
    if prefix < mask {
        return Ok(prefix);
    }
    let mut value = u64::from(mask);
    let mut shift = 0u32;
    loop {
        let Some(&byte) = input.get(*at) else {
            return Err(HpackFault::Truncated);
        };
        *at += 1;
        value += u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        // Five continuation bytes carry 35 bits: past them no u32 is left to write.
        if shift > 28 {
            return Err(HpackFault::IntegerOverflow);
        }
    }
    u32::try_from(value).map_err(|_| HpackFault::IntegerOverflow)
}
