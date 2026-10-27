//! blaze's agent API: one schema, one encoding.
//!
//! Every message is deterministic CBOR (RFC 8949 section 4.2.1): integers and lengths in their
//! shortest form, definite lengths only, map keys sorted bytewise by their encodings and none
//! twice, no floats, no tags. One value has one encoding, so a request, a transaction or a
//! result can be hashed and kept by its hash, and file contents and hashes travel as byte
//! strings. The decoder refuses anything else and says the first thing wrong with it, so what
//! the daemon accepts is what it would have written.
//!
//! On a Unix socket each message is a [`frame`]: four bytes of big-endian length, at most
//! [`frame::LONGEST`], then the CBOR. Over HTTP the body is the CBOR (`application/cbor`).
//! JSON is a rendering for people ([`json`]), never what is sent.
//!
//! Messages are serde types: [`to_vec`] and [`from_slice`] go through [`Value`], the CBOR
//! data model without floats. Enums are tagged by a map of one entry from the variant's name
//! to its content, or are the name alone without content, as serde's JSON has them.

mod de;
pub mod frame;
pub mod json;
mod ser;
#[cfg(test)]
mod tests;
mod value;

pub use de::from_value;
pub use ser::to_value;
pub use value::{Offence, Value};

/// The version of the schema, which every envelope says. A message of another is refused.
pub const SCHEMA: u64 = 1;

/// Why a message could not be written or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The bytes are not deterministic CBOR.
    Encoding(Offence),
    /// The value is not one of the schema's.
    Schema(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Encoding(offence) => write!(f, "not deterministic CBOR: {offence}"),
            Error::Schema(why) => write!(f, "not of the schema: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// The deterministic encoding of `value`.
pub fn to_vec<T: serde::Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    Ok(to_value(value)?.encode())
}

/// The message `bytes` encode, when they are deterministic CBOR of one.
pub fn from_slice<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    from_value(Value::decode(bytes).map_err(Error::Encoding)?)
}

/// File contents and hashes: a CBOR byte string rather than an array of numbers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Bytes(pub Vec<u8>);

impl serde::Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Bytes, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Bytes;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Bytes, E> {
                Ok(Bytes(bytes.to_vec()))
            }
            fn visit_byte_buf<E: serde::de::Error>(self, bytes: Vec<u8>) -> Result<Bytes, E> {
                Ok(Bytes(bytes))
            }
        }
        deserializer.deserialize_byte_buf(Visitor)
    }
}
