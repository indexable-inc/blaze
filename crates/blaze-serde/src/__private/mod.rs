//! What every impl blaze-serde-derive writes would otherwise repeat. Not API: only the derive's
//! output names it, and it changes with the derive.

mod content;
mod flat;
mod tagged;

use std::{borrow::Cow, fmt, marker::PhantomData};

pub use content::{Content, ContentRefDeserializer};
pub use flat::{
    Collect, FlatMapDeserializer, FlatMapSerializeMap, FlatMapSerializeStruct, FlatMapSerializer,
};
use serde::de::{self, DeserializeSeed, Deserializer, Unexpected, Visitor};
pub use tagged::{no_variant, serialize_tagged_newtype, tagged, tagged_unit, untagged_unit};

/// A struct's field key: the index of the field it names among `names`, `None` for a key
/// naming none (an error instead with `deny`, as `#[serde(deny_unknown_fields)]` asks).
pub struct FieldSeed {
    pub names: &'static [&'static str],
    pub deny: bool,
}

impl FieldSeed {
    fn unknown<E: de::Error>(&self, key: &str) -> Result<Option<usize>, E> {
        match self.deny {
            true => Err(E::unknown_field(key, self.names)),
            false => Ok(None),
        }
    }
}

impl<'de> DeserializeSeed<'de> for FieldSeed {
    type Value = Option<usize>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Option<usize>, D::Error> {
        deserializer.deserialize_identifier(self)
    }
}

impl<'de> Visitor<'de> for FieldSeed {
    type Value = Option<usize>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("field identifier")
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Option<usize>, E> {
        match index_in(value, self.names) {
            Some(index) => Ok(Some(index)),
            None if self.deny => Err(E::invalid_value(
                Unexpected::Unsigned(value),
                &format!("field index 0 <= i < {}", self.names.len()).as_str(),
            )),
            None => Ok(None),
        }
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Option<usize>, E> {
        match self.names.iter().position(|it| *it == value) {
            Some(index) => Ok(Some(index)),
            None => self.unknown(value),
        }
    }

    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Option<usize>, E> {
        match self.names.iter().position(|it| it.as_bytes() == value) {
            Some(index) => Ok(Some(index)),
            None => self.unknown(&String::from_utf8_lossy(value)),
        }
    }
}

/// `value` as an index into `names`, when it is one.
fn index_in(value: u64, names: &[&str]) -> Option<usize> {
    match usize::try_from(value) {
        Ok(index) if index < names.len() => Some(index),
        _ => None,
    }
}

/// The key of a struct with flattened fields: the index of a field among `names`, or the key
/// itself, kept for the flattened fields.
pub struct KeySeed {
    pub names: &'static [&'static str],
}

pub enum Key<'de> {
    Field(usize),
    Other(Content<'de>),
}

impl<'de> DeserializeSeed<'de> for KeySeed {
    type Value = Key<'de>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Key<'de>, D::Error> {
        deserializer.deserialize_identifier(self)
    }
}

impl<'de> Visitor<'de> for KeySeed {
    type Value = Key<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("field identifier")
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Key<'de>, E> {
        Ok(match index_in(value, self.names) {
            Some(index) => Key::Field(index),
            None => Key::Other(Content::U64(value)),
        })
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Key<'de>, E> {
        Ok(match self.names.iter().position(|it| *it == value) {
            Some(index) => Key::Field(index),
            None => Key::Other(Content::String(value.to_owned())),
        })
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Key<'de>, E> {
        Ok(match self.names.iter().position(|it| *it == value) {
            Some(index) => Key::Field(index),
            None => Key::Other(Content::Str(value)),
        })
    }

    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Key<'de>, E> {
        Ok(match self.names.iter().position(|it| it.as_bytes() == value) {
            Some(index) => Key::Field(index),
            None => Key::Other(Content::ByteBuf(value.to_vec())),
        })
    }

    fn visit_borrowed_bytes<E: de::Error>(self, value: &'de [u8]) -> Result<Key<'de>, E> {
        Ok(match self.names.iter().position(|it| it.as_bytes() == value) {
            Some(index) => Key::Field(index),
            None => Key::Other(Content::Bytes(value)),
        })
    }
}

/// An enum's variant identifier: the index of the variant it names among `names`, or `other`'s
/// (`#[serde(other)]`) for a name none has.
pub struct VariantSeed {
    pub names: &'static [&'static str],
    pub other: Option<usize>,
}

impl<'de> DeserializeSeed<'de> for VariantSeed {
    type Value = usize;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<usize, D::Error> {
        deserializer.deserialize_identifier(self)
    }
}

impl<'de> Visitor<'de> for VariantSeed {
    type Value = usize;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("variant identifier")
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<usize, E> {
        match (index_in(value, self.names), self.other) {
            (Some(index), _) | (None, Some(index)) => Ok(index),
            (None, None) => Err(E::invalid_value(
                Unexpected::Unsigned(value),
                &format!("variant index 0 <= i < {}", self.names.len()).as_str(),
            )),
        }
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<usize, E> {
        match (self.names.iter().position(|it| *it == value), self.other) {
            (Some(index), _) | (None, Some(index)) => Ok(index),
            (None, None) => Err(E::unknown_variant(value, self.names)),
        }
    }

    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<usize, E> {
        match (self.names.iter().position(|it| it.as_bytes() == value), self.other) {
            (Some(index), _) | (None, Some(index)) => Ok(index),
            (None, None) => Err(E::unknown_variant(&String::from_utf8_lossy(value), self.names)),
        }
    }
}

/// The value of a struct field missing from the input: `None` for an `Option`, as serde's
/// derive reads it, and the error naming `field` for anything else.
pub fn missing_field<'de, V: de::Deserialize<'de>, E: de::Error>(
    field: &'static str,
) -> Result<V, E> {
    V::deserialize(Missing { field, error: PhantomData })
}

struct Missing<E> {
    field: &'static str,
    error: PhantomData<E>,
}

impl<'de, E: de::Error> Deserializer<'de> for Missing<E> {
    type Error = E;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, E> {
        Err(E::missing_field(self.field))
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
        visitor.visit_none()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf unit
        unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier ignored_any
    }
}

/// A `Cow<str>` field with `#[serde(borrow)]`: borrowed when the input lends the string.
pub fn borrow_cow_str<'de: 'a, 'a, D: Deserializer<'de>, R: From<Cow<'a, str>>>(
    deserializer: D,
) -> Result<R, D::Error> {
    deserializer.deserialize_str(CowStr(PhantomData)).map(From::from)
}

struct CowStr<'a>(PhantomData<&'a ()>);

impl<'de: 'a, 'a> Visitor<'de> for CowStr<'a> {
    type Value = Cow<'a, str>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a string")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Cow::Owned(value.to_owned()))
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(Cow::Borrowed(value))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(Cow::Owned(value))
    }

    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
        match std::str::from_utf8(value) {
            Ok(text) => Ok(Cow::Owned(text.to_owned())),
            Err(_) => Err(E::invalid_value(Unexpected::Bytes(value), &self)),
        }
    }

    fn visit_borrowed_bytes<E: de::Error>(self, value: &'de [u8]) -> Result<Self::Value, E> {
        match std::str::from_utf8(value) {
            Ok(text) => Ok(Cow::Borrowed(text)),
            Err(_) => Err(E::invalid_value(Unexpected::Bytes(value), &self)),
        }
    }

    fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
        match String::from_utf8(value) {
            Ok(text) => Ok(Cow::Owned(text)),
            Err(error) => Err(E::invalid_value(Unexpected::Bytes(error.as_bytes()), &self)),
        }
    }
}

/// A `Cow<[u8]>` field with `#[serde(borrow)]`: borrowed when the input lends the bytes.
pub fn borrow_cow_bytes<'de: 'a, 'a, D: Deserializer<'de>, R: From<Cow<'a, [u8]>>>(
    deserializer: D,
) -> Result<R, D::Error> {
    deserializer.deserialize_bytes(CowBytes(PhantomData)).map(From::from)
}

struct CowBytes<'a>(PhantomData<&'a ()>);

impl<'de: 'a, 'a> Visitor<'de> for CowBytes<'a> {
    type Value = Cow<'a, [u8]>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a byte array")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Cow::Owned(value.as_bytes().to_vec()))
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(Cow::Borrowed(value.as_bytes()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(Cow::Owned(value.into_bytes()))
    }

    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
        Ok(Cow::Owned(value.to_vec()))
    }

    fn visit_borrowed_bytes<E: de::Error>(self, value: &'de [u8]) -> Result<Self::Value, E> {
        Ok(Cow::Borrowed(value))
    }

    fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
        Ok(Cow::Owned(value))
    }
}
