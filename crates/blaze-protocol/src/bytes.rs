//! A `Vec<u8>` field as one byte string: `#[serde(with = "blaze_protocol::bytes")]`. serde's
//! default for a `Vec<u8>` is a sequence of one item per byte, which [`crate::cbor`] writes as
//! one to two bytes and reads as one decoder step each; a byte string is one head and one copy
//! each way.
//!
//! serde_json writes bytes as the array of numbers it writes the sequence as, so JSON is the
//! same either way. A reader that is not human-readable takes only a byte string, so the field
//! has one encoding; a human-readable one (JSON) also takes an array of numbers.

use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, SeqAccess, Visitor},
};

pub fn serialize<S: Serializer>(bytes: &[u8], to: S) -> Result<S::Ok, S::Error> {
    to.serialize_bytes(bytes)
}

pub fn deserialize<'de, D: Deserializer<'de>>(from: D) -> Result<Vec<u8>, D::Error> {
    let array = from.is_human_readable();
    from.deserialize_byte_buf(Owned { array })
}

/// A `&[u8]` field read where it lies, from one byte string only, as [`deserialize`] reads it
/// from a reader that is not human-readable (serde's own `&[u8]` takes text too):
/// `#[serde(deserialize_with = "blaze_protocol::bytes::borrow")]`.
pub fn borrow<'de, D: Deserializer<'de>>(from: D) -> Result<&'de [u8], D::Error> {
    from.deserialize_bytes(Lent)
}

struct Lent;

impl<'de> Visitor<'de> for Lent {
    type Value = &'de [u8];

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a byte string, read where it lies")
    }

    fn visit_borrowed_bytes<E: de::Error>(self, bytes: &'de [u8]) -> Result<&'de [u8], E> {
        Ok(bytes)
    }
}

/// Bytes lent to a serializer as one byte string, for a place `with` can't name (a tuple's
/// field, a list's item).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bytes<'a>(pub &'a [u8]);

impl Serialize for Bytes<'_> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        to.serialize_bytes(self.0)
    }
}

/// Bytes read back from one byte string, as [`deserialize`] reads them.
#[derive(PartialEq, Eq, Debug, Default)]
pub struct Buf(pub Vec<u8>);

impl<'de> Deserialize<'de> for Buf {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Buf, D::Error> {
        deserialize(from).map(Buf)
    }
}

struct Owned {
    /// An array of numbers is taken too (JSON's rendering of bytes).
    array: bool,
}

impl<'de> Visitor<'de> for Owned {
    type Value = Vec<u8>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.array {
            true => "a byte string or an array of bytes",
            false => "a byte string",
        })
    }

    fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Vec<u8>, E> {
        // alloc: the owned field's one copy of a borrowed byte string.
        Ok(bytes.to_vec())
    }

    fn visit_byte_buf<E: de::Error>(self, bytes: Vec<u8>) -> Result<Vec<u8>, E> {
        Ok(bytes)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
        if !self.array {
            return Err(de::Error::invalid_type(de::Unexpected::Seq, &self));
        }
        let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(1 << 20));
        while let Some(byte) = seq.next_element::<u8>()? {
            bytes.push(byte);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests;
