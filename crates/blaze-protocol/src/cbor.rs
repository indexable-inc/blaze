//! CBOR in RFC 8949 section 4.2.1's core deterministic encoding, for any type serde
//! serializes:
//!
//! - every integer, length and count in the shortest head that holds it;
//! - definite lengths only;
//! - a map's entries sorted bytewise by the encoding of their keys, each key once, whatever
//!   order the type gave them in (a `HashMap` too): "b" comes before "aa";
//! - a float in the shortest of half, single and double precision that holds it exactly, and
//!   NaN as `f9 7e00`;
//! - no tags, and of the simple values only false, true and null.
//!
//! serde's shapes map as serde_json's do: a struct is a map keyed by its field names, a unit
//! variant is its name, any other variant a map of its name to its contents, `None` and `()`
//! null. Integers wider than 64 bits that do not fit CBOR's are refused.
//!
//! [`from_slice`] refuses bytes that are not one item in this encoding, naming the first
//! offence and where (as [`check`] does), rather than read an encoding the writer should not
//! have sent. It reads them once, checking as it goes, and borrows text and byte strings from
//! them for a type that borrows; [`Raw`] and [`List`] read an item, or an array's items, where
//! they lie. [`append`] and a kept [`Writer`] write onto the end of a buffer the caller reuses,
//! each item in place.

mod borrowed;
mod check;
mod de;
#[cfg(any(test, feature = "reference"))]
#[doc(hidden)]
pub mod reference;
mod ser;
#[cfg(test)]
mod tests;

pub use borrowed::{Iter, List, Raw};
pub use check::check;
pub use de::from_slice;
pub use ser::{Writer, append, to_vec};

/// The name [`Raw`] is read under as a newtype struct: the decoders hand it its item's bytes,
/// checked and passed over, rather than the item.
const RAW: &str = "$blaze_protocol::cbor::Raw";

use std::{cmp::Ordering, fmt};

/// The bytewise order of two encoded map keys, decided by their first bytes when those differ:
/// a text key's first byte holds its length below 24, so keys of two lengths are ordered with
/// no call to compare the rest.
#[inline]
fn key_order(a: &[u8], b: &[u8]) -> Ordering {
    match (a.first(), b.first()) {
        (Some(x), Some(y)) if x != y => x.cmp(y),
        _ => a.cmp(b),
    }
}

/// The order of two text keys' encodings, from the text: a text head grows with its length,
/// so a shorter key comes first and keys of one length go bytewise.
#[inline]
fn name_order(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

/// Why a value has no encoding here, or bytes are no value.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The bytes are not one item in the core deterministic encoding: the first offence, at
    /// this offset.
    NotDeterministic { at: usize, what: &'static str },
    /// The value has a shape this encoding does not give one.
    Unsupported(&'static str),
    /// What the type's own serializer or deserializer said.
    Message(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotDeterministic { at, what } => {
                write!(f, "not deterministic CBOR at byte {at}: {what}")
            }
            Error::Unsupported(what) => write!(f, "no CBOR for {what}"),
            Error::Message(said) => f.write_str(said),
        }
    }
}

impl std::error::Error for Error {}

impl serde::ser::Error for Error {
    fn custom<T: fmt::Display>(said: T) -> Error {
        // alloc: error path, what the type's serializer said.
        Error::Message(said.to_string())
    }
}

impl serde::de::Error for Error {
    fn custom<T: fmt::Display>(said: T) -> Error {
        // alloc: error path, what the type's deserializer said.
        Error::Message(said.to_string())
    }
}

const UNSIGNED: u8 = 0;
const NEGATIVE: u8 = 1;
const BYTES: u8 = 2;
const TEXT: u8 = 3;
const ARRAY: u8 = 4;
const MAP: u8 = 5;
const SIMPLE: u8 = 7;

const FALSE: u8 = 0xf4;
const TRUE: u8 = 0xf5;
const NULL: u8 = 0xf6;
const HALF: u8 = 0xf9;
const SINGLE: u8 = 0xfa;
const DOUBLE: u8 = 0xfb;
/// The one NaN, in half precision.
const NAN: u16 = 0x7e00;

/// Writes the head of an item of `major` type with argument `n`, in its shortest form.
fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    if n < 24 {
        out.push((major << 5) | n as u8);
        return;
    }
    let (bytes, len) = head_bytes(major, n);
    out.extend_from_slice(&bytes[..len]);
}

/// The head of an item of `major` type with argument `n`, in its shortest form, and how many
/// of the bytes it is.
fn head_bytes(major: u8, n: u64) -> ([u8; 9], usize) {
    let major = major << 5;
    let mut bytes = [0; 9];
    let len = match n {
        0..=23 => {
            bytes[0] = major | n as u8;
            1
        }
        24..=0xff => {
            bytes[..2].copy_from_slice(&[major | 24, n as u8]);
            2
        }
        0x100..=0xffff => {
            bytes[0] = major | 25;
            bytes[1..3].copy_from_slice(&(n as u16).to_be_bytes());
            3
        }
        0x1_0000..=0xffff_ffff => {
            bytes[0] = major | 26;
            bytes[1..5].copy_from_slice(&(n as u32).to_be_bytes());
            5
        }
        _ => {
            bytes[0] = major | 27;
            bytes[1..].copy_from_slice(&n.to_be_bytes());
            9
        }
    };
    (bytes, len)
}

/// `value` in half precision, when it is exactly one. NaN has its own encoding and is not
/// asked about.
fn half_of(value: f32) -> Option<u16> {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x7f_ffff;
    match exponent {
        0xff => (mantissa == 0).then_some(sign | 0x7c00),
        0 if mantissa == 0 => Some(sign),
        // Below the smallest half's subnormal.
        0 => None,
        _ => {
            let exponent = exponent - 127;
            match exponent {
                -14..=15 => (mantissa & 0x1fff == 0)
                    .then(|| sign | (((exponent + 15) as u16) << 10) | (mantissa >> 13) as u16),
                // A half's subnormal is m * 2^-24 with m below 2^10.
                -24..=-15 => {
                    let whole = (1 << 23) | mantissa;
                    let shift = -(exponent + 1) as u32;
                    (whole & ((1 << shift) - 1) == 0).then(|| sign | (whole >> shift) as u16)
                }
                _ => None,
            }
        }
    }
}

fn from_half(half: u16) -> f64 {
    let sign = if half & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((half >> 10) & 0x1f);
    let mantissa = half & 0x3ff;
    sign * match exponent {
        0 => f64::from(mantissa) * 2f64.powi(-24),
        0x1f if mantissa == 0 => f64::INFINITY,
        0x1f => f64::NAN,
        _ => (1.0 + f64::from(mantissa) / 1024.0) * 2f64.powi(exponent - 15),
    }
}

/// Writes `value` in the fewest bytes that hold it exactly.
fn float(out: &mut Vec<u8>, value: f64) {
    if value.is_nan() {
        out.push(HALF);
        out.extend(NAN.to_be_bytes());
        return;
    }
    let single = value as f32;
    if f64::from(single).to_bits() != value.to_bits() {
        out.push(DOUBLE);
        out.extend(value.to_bits().to_be_bytes());
    } else if let Some(half) = half_of(single) {
        out.push(HALF);
        out.extend(half.to_be_bytes());
    } else {
        out.push(SINGLE);
        out.extend(single.to_bits().to_be_bytes());
    }
}
