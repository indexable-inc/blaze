use serde::{
    Deserialize, Deserializer,
    de::{Visitor, value},
};

use super::*;

fn id() -> ContentId {
    ContentId(std::array::from_fn(|at| at as u8 * 7 + 1))
}

fn fingerprint() -> Fingerprint {
    Fingerprint(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210)
}

/// A binary format's byte string: postcard writes a varint length, then the bytes.
fn framed(bytes: &[u8]) -> Vec<u8> {
    assert!(bytes.len() < 128, "one varint byte");
    std::iter::once(bytes.len() as u8).chain(bytes.iter().copied()).collect()
}

/// A reader that is not human-readable, handing over a byte string or an array of numbers.
enum Given<'a> {
    Bytes(&'a [u8]),
    Array(&'a [u8]),
}

struct Binary<'a>(Given<'a>);

impl<'de> Deserializer<'de> for Binary<'_> {
    type Error = value::Error;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, value::Error> {
        match self.0 {
            Given::Bytes(bytes) => visitor.visit_bytes(bytes),
            Given::Array(bytes) => visitor.visit_seq(value::SeqDeserializer::new(bytes.iter().copied())),
        }
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

/// In a binary format each ID is one byte string: its length, then its bytes, and back.
#[test]
fn ids_round_trip_as_one_byte_string() {
    let encoded = postcard::to_allocvec(&id()).unwrap();
    assert_eq!(encoded, framed(&id().0));
    assert_eq!(encoded.len(), 33);
    assert_eq!(postcard::from_bytes::<ContentId>(&encoded).unwrap(), id());

    let encoded = postcard::to_allocvec(&fingerprint()).unwrap();
    assert_eq!(encoded, framed(&fingerprint().0.to_le_bytes()));
    assert_eq!(postcard::from_bytes::<Fingerprint>(&encoded).unwrap(), fingerprint());

    let pair = (id(), fingerprint());
    let encoded = postcard::to_allocvec(&pair).unwrap();
    assert_eq!(encoded.len(), 33 + 17);
    assert_eq!(postcard::from_bytes::<(ContentId, Fingerprint)>(&encoded).unwrap(), pair);
}

/// A byte string one short or one long is refused, in every format.
#[test]
fn a_wrong_length_is_refused() {
    let long = [9u8; 40];
    for len in [0, 31, 33] {
        assert!(postcard::from_bytes::<ContentId>(&framed(&long[..len])).is_err(), "{len} bytes");
        assert!(ContentId::deserialize(Binary(Given::Bytes(&long[..len]))).is_err(), "{len} bytes");
    }
    for len in [0, 15, 17] {
        assert!(postcard::from_bytes::<Fingerprint>(&framed(&long[..len])).is_err(), "{len} bytes");
        assert!(Fingerprint::deserialize(Binary(Given::Bytes(&long[..len]))).is_err(), "{len} bytes");
    }
    assert_eq!(ContentId::deserialize(Binary(Given::Bytes(&id().0))).unwrap(), id());
}

/// A binary reader takes only the byte string: an array of the right numbers is refused.
#[test]
fn an_array_in_a_binary_format_is_refused() {
    let error = ContentId::deserialize(Binary(Given::Array(&id().0))).unwrap_err();
    assert!(error.to_string().contains("sequence"), "{error}");
    let bytes = fingerprint().0.to_le_bytes();
    assert!(Fingerprint::deserialize(Binary(Given::Array(&bytes))).is_err());
}

/// JSON writes bytes as an array of numbers and reads that back; an array of the wrong length
/// is refused.
#[test]
fn json_is_still_an_array_of_numbers() {
    let text = serde_json::to_string(&id()).unwrap();
    let numbers: Vec<u8> = serde_json::from_str(&text).unwrap();
    assert_eq!(numbers, id().0);
    assert_eq!(serde_json::from_str::<ContentId>(&text).unwrap(), id());

    let text = serde_json::to_string(&fingerprint()).unwrap();
    let numbers: Vec<u8> = serde_json::from_str(&text).unwrap();
    assert_eq!(numbers, fingerprint().0.to_le_bytes());
    assert_eq!(serde_json::from_str::<Fingerprint>(&text).unwrap(), fingerprint());

    for len in [31, 33] {
        let text = serde_json::to_string(&vec![1u8; len]).unwrap();
        assert!(serde_json::from_str::<ContentId>(&text).is_err(), "{len} numbers");
    }
    assert!(serde_json::from_str::<Fingerprint>("[1,2,3]").is_err());
    assert!(serde_json::from_str::<ContentId>("[256]").is_err());
}
