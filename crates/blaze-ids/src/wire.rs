//! [`ContentId`] and [`Fingerprint`] as one byte string each: serde's default for a `[u8; N]` is
//! a tuple of `N` integers, which `blaze_protocol::cbor` writes as one to two bytes and reads as
//! one decoder step each; a byte string is one head and one copy each way (34 bytes for a
//! `ContentId`, against about 62).
//!
//! A reader that is not human-readable takes only a byte string of exactly the type's length,
//! so each ID has one encoding. serde_json writes bytes as an array of numbers, so a
//! human-readable reader (JSON) also takes an array of exactly that many.

use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, SeqAccess, Visitor},
};

use crate::{ContentId, Fingerprint};

impl Serialize for ContentId {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        to.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for ContentId {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<ContentId, D::Error> {
        exact(from).map(ContentId)
    }
}

impl Serialize for Fingerprint {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        to.serialize_bytes(&self.0.to_le_bytes())
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Fingerprint, D::Error> {
        exact(from).map(|bytes| Fingerprint(u128::from_le_bytes(bytes)))
    }
}

fn exact<'de, const N: usize, D: Deserializer<'de>>(from: D) -> Result<[u8; N], D::Error> {
    let array = from.is_human_readable();
    from.deserialize_bytes(Exact::<N> { array })
}

/// `N` bytes from one byte string of exactly `N`.
struct Exact<const N: usize> {
    /// An array of exactly `N` numbers is taken too (JSON's rendering of bytes).
    array: bool,
}

impl<'de, const N: usize> Visitor<'de> for Exact<N> {
    type Value = [u8; N];

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.array {
            true => write!(f, "a byte string or an array of {N} bytes"),
            false => write!(f, "a byte string of {N} bytes"),
        }
    }

    fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<[u8; N], E> {
        <[u8; N]>::try_from(bytes).map_err(|_| E::invalid_length(bytes.len(), &self))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; N], A::Error> {
        if !self.array {
            return Err(de::Error::invalid_type(de::Unexpected::Seq, &self));
        }
        let mut bytes = [0u8; N];
        for (at, byte) in bytes.iter_mut().enumerate() {
            *byte = seq.next_element::<u8>()?.ok_or_else(|| de::Error::invalid_length(at, &self))?;
        }
        if seq.next_element::<u8>()?.is_some() {
            return Err(de::Error::invalid_length(N + 1, &self));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests;
