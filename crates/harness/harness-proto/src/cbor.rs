//! Deterministic CBOR (RFC 8949 section 4.2.1, plus dCBOR's numeric reduction): the only encoding harness writes.
//!
//! [`encode`] serializes a value through serde into a CBOR data model value, makes it canonical, and writes it:
//!
//! - every map's entries sorted by the bytes of their encoded keys (for text keys: shorter first, then bytewise);
//! - integers and lengths in their shortest form, definite lengths only (no indefinite arrays, maps or strings);
//! - a float in the shortest of half, single or double precision that holds it exactly, and a float with an integral
//!   value (in the range of a 64-bit integer) written as that integer, `-0.0` as `0`.
//!
//! The kernel's codec (`mcp/src/cbor.ts`) applies the same rules, so both sides write the same bytes for the same
//! value. [`decode`] reads one whole item and refuses trailing bytes; it accepts any well-formed CBOR, canonical or not.
use ciborium::value::{Integer, Value};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Why bytes are not the value asked for (or a value cannot be written).
#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// `v` as deterministic CBOR.
///
/// # Panics
/// Only for a value serde cannot represent in CBOR's data model (a map key that is not a value): no type in this crate.
#[must_use]
pub fn encode<T: Serialize + ?Sized>(v: &T) -> Vec<u8> {
    let value = Value::serialized(v).unwrap_or_else(|e| panic!("harness-proto: a value CBOR cannot hold: {e}"));
    let mut out = Vec::new();
    ciborium::into_writer(&canonical(value), &mut out).expect("writing CBOR into a Vec cannot fail");
    out
}

/// One whole CBOR item as `T`, with nothing after it.
///
/// # Errors
/// Bytes that are not one well-formed item, or an item that is not a `T` (named in the error).
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    let mut r = bytes;
    let v: T = ciborium::from_reader(&mut r).map_err(|e| Error(format!("{} as CBOR: {e}", std::any::type_name::<T>())))?;
    if !r.is_empty() {
        return Err(Error(format!("{} trailing bytes after one CBOR item", r.len())));
    }
    Ok(v)
}

/// `v` canonical: maps sorted by encoded key, floats reduced.
#[must_use]
pub fn canonical(v: Value) -> Value {
    match v {
        Value::Map(entries) => {
            let mut keyed: Vec<(Vec<u8>, Value, Value)> = entries
                .into_iter()
                .map(|(k, v)| {
                    let k = canonical(k);
                    let mut kb = Vec::new();
                    ciborium::into_writer(&k, &mut kb).expect("writing CBOR into a Vec cannot fail");
                    (kb, k, canonical(v))
                })
                .collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Map(keyed.into_iter().map(|(_, k, v)| (k, v)).collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(canonical).collect()),
        Value::Tag(t, inner) => Value::Tag(t, Box::new(canonical(*inner))),
        Value::Float(f) => reduce(f),
        v => v,
    }
}

/// dCBOR's numeric reduction: a float with an integral value a 64-bit integer holds is that integer.
fn reduce(f: f64) -> Value {
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    const TWO_64: f64 = 18_446_744_073_709_551_616.0;
    if f.is_finite() && f.fract() == 0.0 {
        if (0.0..TWO_64).contains(&f) {
            return Value::Integer(Integer::from(f as u64));
        }
        if (-TWO_63..0.0).contains(&f) {
            return Value::Integer(Integer::from(f as i64));
        }
    }
    Value::Float(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The canonical form, by example: keys shorter-first then bytewise (not serde's field order, not alphabetical),
    /// shortest ints, floats reduced to integers or the shortest exact width.
    #[test]
    fn canonical_bytes() {
        #[derive(Serialize)]
        struct S {
            zz: u64,
            a: f64,
            bb: f64,
            c: f64,
        }
        let b = encode(&S { zz: 500, a: 2.0, bb: 0.5, c: 0.1 });
        assert_eq!(
            b,
            [
                0xa4, // map(4)
                0x61, b'a', 0x02, // "a": 2 (an integral float is an integer)
                0x61, b'c', 0xfb, 0x3f, 0xb9, 0x99, 0x99, 0x99, 0x99, 0x99, 0x9a, // "c": 0.1 needs a double
                0x62, b'b', b'b', 0xf9, 0x38, 0x00, // "bb": 0.5 as a half
                0x62, b'z', b'z', 0x19, 0x01, 0xf4, // "zz": 500 in two bytes
            ]
        );
        let mut m = BTreeMap::new();
        m.insert("-0".to_owned(), -0.0f64);
        assert_eq!(encode(&m), [0xa1, 0x62, b'-', b'0', 0x00]);
        assert!(decode::<u64>(&[0x01, 0x02]).is_err(), "trailing bytes are refused");
    }
}
