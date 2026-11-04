//! Internally tagged enums (`#[serde(tag = "..")]`), whose tag is one entry of the variant's
//! map, and untagged ones (`#[serde(untagged)]`), which try each variant in turn.

use std::fmt;

use serde::{
    de::{
        self, Deserialize, DeserializeSeed, Deserializer, Expected, IgnoredAny, MapAccess,
        SeqAccess, Visitor,
    },
    ser::{self, Impossible, Serialize, SerializeMap, SerializeStruct, Serializer},
};

use super::{
    VariantSeed,
    content::{Content, ContentRefDeserializer},
};

/// What an internally tagged enum expects, for a type error.
struct TaggedEnum(&'static str);

impl Expected for TaggedEnum {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "internally tagged enum {}", self.0)
    }
}

/// The input read whole, its variant found by its `tag` entry among `variants`, or `other` for
/// a tag naming none of them: the variant's index and the input without the tag. A sequence
/// holds the tag first.
pub fn tagged<'de, D: Deserializer<'de>>(
    deserializer: D,
    name: &'static str,
    tag: &'static str,
    variants: &'static [&'static str],
    other: Option<usize>,
) -> Result<(usize, Content<'de>), D::Error> {
    let human = deserializer.is_human_readable();
    let variant = |content: &Content<'de>| {
        VariantSeed { names: variants, other }
            .deserialize(ContentRefDeserializer::<D::Error>::new(content, human))
    };
    match Content::deserialize(deserializer)? {
        Content::Map(entries) => {
            let mut index = None;
            let mut rest = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                if !key.is_key(tag) {
                    rest.push((key, value));
                    continue;
                }
                if index.is_some() {
                    return Err(de::Error::duplicate_field(tag));
                }
                index = Some(variant(&value)?);
            }
            match index {
                Some(index) => Ok((index, Content::Map(rest))),
                None => Err(de::Error::missing_field(tag)),
            }
        }
        Content::Seq(items) => {
            let mut items = items.into_iter();
            match items.next() {
                Some(first) => Ok((variant(&first)?, Content::Seq(items.collect()))),
                None => Err(de::Error::missing_field(tag)),
            }
        }
        other => Err(de::Error::invalid_type(other.unexpected(), &TaggedEnum(name))),
    }
}

/// A unit variant of an internally tagged enum, once its tag is read: the entries left are
/// ignored, as serde ignores them.
pub fn tagged_unit<'de, D: Deserializer<'de>>(
    deserializer: D,
    name: &'static str,
    variant: &'static str,
) -> Result<(), D::Error> {
    deserializer.deserialize_any(TaggedUnit { name, variant })
}

struct TaggedUnit {
    name: &'static str,
    variant: &'static str,
}

impl<'de> Visitor<'de> for TaggedUnit {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unit variant {}::{}", self.name, self.variant)
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(())
    }
}

/// A unit variant of an untagged enum: a unit or `None`.
pub fn untagged_unit<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    deserializer.deserialize_any(UntaggedUnit)
}

struct UntaggedUnit;

impl<'de> Visitor<'de> for UntaggedUnit {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unit variant")
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_none<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
}

/// The error of an untagged enum `name` none of whose variants read the input.
pub fn no_variant<E: de::Error>(name: &'static str) -> E {
    E::custom(format_args!("data did not match any variant of untagged enum {name}"))
}

/// The newtype variant `name::variant` of an internally tagged enum, its value written with the
/// entry `tag: key` added: a struct or map gains the entry, a unit becomes the entry alone, and
/// a newtype or enum variant inside is unwrapped as serde unwraps it.
pub fn serialize_tagged_newtype<S: Serializer, T: Serialize + ?Sized>(
    serializer: S,
    name: &'static str,
    variant: &'static str,
    tag: &'static str,
    key: &'static str,
    value: &T,
) -> Result<S::Ok, S::Error> {
    value.serialize(TaggedSerializer { name, variant, tag, key, delegate: serializer })
}

struct TaggedSerializer<S> {
    name: &'static str,
    variant: &'static str,
    tag: &'static str,
    key: &'static str,
    delegate: S,
}

impl<S: Serializer> TaggedSerializer<S> {
    fn cannot(&self, what: &str) -> S::Error {
        ser::Error::custom(format_args!(
            "cannot serialize tagged newtype variant {}::{} containing {what}",
            self.name, self.variant
        ))
    }

    /// The tag alone, for a unit.
    fn tag_alone(self) -> Result<S::Ok, S::Error> {
        let mut state = self.delegate.serialize_struct(self.name, 1)?;
        state.serialize_field(self.tag, self.key)?;
        state.end()
    }
}

impl<S: Serializer> Serializer for TaggedSerializer<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = Impossible<S::Ok, S::Error>;
    type SerializeTuple = Impossible<S::Ok, S::Error>;
    type SerializeTupleStruct = Impossible<S::Ok, S::Error>;
    type SerializeTupleVariant = Impossible<S::Ok, S::Error>;
    type SerializeMap = S::SerializeMap;
    type SerializeStruct = S::SerializeStruct;
    type SerializeStructVariant = Impossible<S::Ok, S::Error>;

    fn is_human_readable(&self) -> bool {
        self.delegate.is_human_readable()
    }

    fn serialize_bool(self, _value: bool) -> Result<S::Ok, S::Error> {
        Err(self.cannot("bool"))
    }

    fn serialize_i8(self, _value: i8) -> Result<S::Ok, S::Error> {
        Err(self.cannot("i8"))
    }

    fn serialize_i16(self, _value: i16) -> Result<S::Ok, S::Error> {
        Err(self.cannot("i16"))
    }

    fn serialize_i32(self, _value: i32) -> Result<S::Ok, S::Error> {
        Err(self.cannot("i32"))
    }

    fn serialize_i64(self, _value: i64) -> Result<S::Ok, S::Error> {
        Err(self.cannot("i64"))
    }

    fn serialize_u8(self, _value: u8) -> Result<S::Ok, S::Error> {
        Err(self.cannot("u8"))
    }

    fn serialize_u16(self, _value: u16) -> Result<S::Ok, S::Error> {
        Err(self.cannot("u16"))
    }

    fn serialize_u32(self, _value: u32) -> Result<S::Ok, S::Error> {
        Err(self.cannot("u32"))
    }

    fn serialize_u64(self, _value: u64) -> Result<S::Ok, S::Error> {
        Err(self.cannot("u64"))
    }

    fn serialize_f32(self, _value: f32) -> Result<S::Ok, S::Error> {
        Err(self.cannot("f32"))
    }

    fn serialize_f64(self, _value: f64) -> Result<S::Ok, S::Error> {
        Err(self.cannot("f64"))
    }

    fn serialize_char(self, _value: char) -> Result<S::Ok, S::Error> {
        Err(self.cannot("char"))
    }

    fn serialize_str(self, _value: &str) -> Result<S::Ok, S::Error> {
        Err(self.cannot("str"))
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<S::Ok, S::Error> {
        Err(self.cannot("bytes"))
    }

    fn serialize_none(self) -> Result<S::Ok, S::Error> {
        Err(self.cannot("Option"))
    }

    fn serialize_some<T: Serialize + ?Sized>(self, _value: &T) -> Result<S::Ok, S::Error> {
        Err(self.cannot("Option"))
    }

    fn serialize_unit(self) -> Result<S::Ok, S::Error> {
        self.tag_alone()
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<S::Ok, S::Error> {
        self.tag_alone()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        inner: &'static str,
    ) -> Result<S::Ok, S::Error> {
        let mut state = self.delegate.serialize_map(Some(2))?;
        state.serialize_entry(self.tag, self.key)?;
        state.serialize_entry(inner, &())?;
        state.end()
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<S::Ok, S::Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        inner: &'static str,
        value: &T,
    ) -> Result<S::Ok, S::Error> {
        let mut state = self.delegate.serialize_map(Some(2))?;
        state.serialize_entry(self.tag, self.key)?;
        state.serialize_entry(inner, value)?;
        state.end()
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, S::Error> {
        Err(self.cannot("a sequence"))
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, S::Error> {
        Err(self.cannot("a tuple"))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, S::Error> {
        Err(self.cannot("a tuple struct"))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _inner: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, S::Error> {
        Err(self.cannot("a tuple variant"))
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, S::Error> {
        let mut state = self.delegate.serialize_map(len.map(|it| it + 1))?;
        state.serialize_entry(self.tag, self.key)?;
        Ok(state)
    }

    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, S::Error> {
        let mut state = self.delegate.serialize_struct(name, len + 1)?;
        state.serialize_field(self.tag, self.key)?;
        Ok(state)
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _inner: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, S::Error> {
        Err(self.cannot("a struct variant"))
    }
}
