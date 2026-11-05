//! `#[serde(flatten)]`: a field written as entries of its parent's map, and read back from the
//! entries the parent's own fields did not name.

use std::{marker::PhantomData, slice};

use serde::{
    de::{self, DeserializeSeed, Deserializer, MapAccess, Visitor},
    ser::{self, Impossible, Serialize, SerializeMap, SerializeStruct, Serializer},
};

use super::content::{Content, ContentRefDeserializer};

const CANNOT_FLATTEN: &str = "can only flatten structs and maps";

/// The entries of a map its struct's own fields did not name, for its flattened fields. A
/// flattened struct takes the entries it names; a flattened map sees every entry left.
#[derive(Default)]
pub struct Collect<'de> {
    entries: Vec<Option<(Content<'de>, Content<'de>)>>,
}

impl<'de> Collect<'de> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, key: Content<'de>, value: Content<'de>) {
        self.entries.push(Some((key, value)));
    }

    /// The entries left, read as a map. It says the input is human-readable whatever the input
    /// said: a flattened field whose type writes itself differently for the two (an IP
    /// address) reads only its human-readable form.
    pub fn deserializer<E>(&mut self) -> FlatMapDeserializer<'_, 'de, E> {
        FlatMapDeserializer { entries: &mut self.entries, error: PhantomData }
    }
}

pub struct FlatMapDeserializer<'a, 'de, E> {
    entries: &'a mut Vec<Option<(Content<'de>, Content<'de>)>>,
    error: PhantomData<E>,
}

macro_rules! cannot_flatten {
    ($($method:ident)*) => {$(
        fn $method<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, E> {
            Err(E::custom(CANNOT_FLATTEN))
        }
    )*};
}

impl<'a, 'de, E: de::Error> Deserializer<'de> for FlatMapDeserializer<'a, 'de, E> {
    type Error = E;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
        self.deserialize_map(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
        visitor.visit_map(FlatMapAccess {
            entries: self.entries.iter(),
            value: None,
            error: PhantomData,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, E> {
        visitor.visit_map(FlatStructAccess {
            entries: self.entries.iter_mut(),
            fields,
            value: None,
            error: PhantomData,
        })
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
        visitor.visit_some(self)
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, E> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, E> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
        visitor.visit_unit()
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, _visitor: V) -> Result<V::Value, E> {
        Err(E::custom(CANNOT_FLATTEN))
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        _visitor: V,
    ) -> Result<V::Value, E> {
        Err(E::custom(CANNOT_FLATTEN))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, E> {
        Err(E::custom(CANNOT_FLATTEN))
    }

    cannot_flatten! {
        deserialize_bool deserialize_i8 deserialize_i16 deserialize_i32 deserialize_i64
        deserialize_i128 deserialize_u8 deserialize_u16 deserialize_u32 deserialize_u64
        deserialize_u128 deserialize_f32 deserialize_f64 deserialize_char deserialize_str
        deserialize_string deserialize_bytes deserialize_byte_buf deserialize_seq
        deserialize_identifier
    }
}

/// Every entry left, none taken: a flattened map.
struct FlatMapAccess<'a, 'de, E> {
    entries: slice::Iter<'a, Option<(Content<'de>, Content<'de>)>>,
    value: Option<&'a Content<'de>>,
    error: PhantomData<E>,
}

impl<'a, 'de, E: de::Error> MapAccess<'de> for FlatMapAccess<'a, 'de, E> {
    type Error = E;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, E> {
        match self.entries.by_ref().flatten().next() {
            Some((key, value)) => {
                self.value = Some(value);
                seed.deserialize(ContentRefDeserializer::new(key, true)).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, E> {
        match self.value.take() {
            Some(value) => seed.deserialize(ContentRefDeserializer::new(value, true)),
            None => Err(E::custom("a map value was read before its key")),
        }
    }
}

/// The entries a flattened struct names, taken so no later flattened field sees them.
struct FlatStructAccess<'a, 'de, E> {
    entries: slice::IterMut<'a, Option<(Content<'de>, Content<'de>)>>,
    fields: &'static [&'static str],
    value: Option<Content<'de>>,
    error: PhantomData<E>,
}

impl<'a, 'de, E: de::Error> MapAccess<'de> for FlatStructAccess<'a, 'de, E> {
    type Error = E;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, E> {
        for entry in self.entries.by_ref() {
            let named = matches!(entry, Some((key, _)) if self.fields.iter().any(|field| key.is_key(field)));
            if !named {
                continue;
            }
            if let Some((key, value)) = entry.take() {
                self.value = Some(value);
                return seed.deserialize(ContentRefDeserializer::new(&key, true)).map(Some);
            }
        }
        Ok(None)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, E> {
        match self.value.take() {
            Some(value) => seed.deserialize(ContentRefDeserializer::new(&value, true)),
            None => Err(E::custom("a map value was read before its key")),
        }
    }
}

/// A flattened field written into its parent's map: a struct or map as its entries, a unit or
/// `None` as nothing, a newtype or `Some` as what it holds, an enum variant as one entry.
pub struct FlatMapSerializer<'a, M>(pub &'a mut M);

fn cannot_flatten<E: ser::Error>(what: &str) -> E {
    E::custom(format_args!("{CANNOT_FLATTEN} (got {what})"))
}

impl<'a, M: SerializeMap + 'a> Serializer for FlatMapSerializer<'a, M> {
    type Ok = ();
    type Error = M::Error;
    type SerializeSeq = Impossible<(), M::Error>;
    type SerializeTuple = Impossible<(), M::Error>;
    type SerializeTupleStruct = Impossible<(), M::Error>;
    type SerializeTupleVariant = Impossible<(), M::Error>;
    type SerializeMap = FlatMapSerializeMap<'a, M>;
    type SerializeStruct = FlatMapSerializeStruct<'a, M>;
    type SerializeStructVariant = Impossible<(), M::Error>;

    fn serialize_bool(self, _value: bool) -> Result<(), M::Error> {
        Err(cannot_flatten("a boolean"))
    }

    fn serialize_i8(self, _value: i8) -> Result<(), M::Error> {
        Err(cannot_flatten("an integer"))
    }

    fn serialize_i16(self, _value: i16) -> Result<(), M::Error> {
        Err(cannot_flatten("an integer"))
    }

    fn serialize_i32(self, _value: i32) -> Result<(), M::Error> {
        Err(cannot_flatten("an integer"))
    }

    fn serialize_i64(self, _value: i64) -> Result<(), M::Error> {
        Err(cannot_flatten("an integer"))
    }

    fn serialize_u8(self, _value: u8) -> Result<(), M::Error> {
        Err(cannot_flatten("an integer"))
    }

    fn serialize_u16(self, _value: u16) -> Result<(), M::Error> {
        Err(cannot_flatten("an integer"))
    }

    fn serialize_u32(self, _value: u32) -> Result<(), M::Error> {
        Err(cannot_flatten("an integer"))
    }

    fn serialize_u64(self, _value: u64) -> Result<(), M::Error> {
        Err(cannot_flatten("an integer"))
    }

    fn serialize_f32(self, _value: f32) -> Result<(), M::Error> {
        Err(cannot_flatten("a float"))
    }

    fn serialize_f64(self, _value: f64) -> Result<(), M::Error> {
        Err(cannot_flatten("a float"))
    }

    fn serialize_char(self, _value: char) -> Result<(), M::Error> {
        Err(cannot_flatten("a char"))
    }

    fn serialize_str(self, _value: &str) -> Result<(), M::Error> {
        Err(cannot_flatten("a string"))
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<(), M::Error> {
        Err(cannot_flatten("a byte array"))
    }

    fn serialize_none(self) -> Result<(), M::Error> {
        Ok(())
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), M::Error> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), M::Error> {
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), M::Error> {
        Ok(())
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<(), M::Error> {
        self.0.serialize_entry(variant, &())
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), M::Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<(), M::Error> {
        self.0.serialize_entry(variant, value)
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, M::Error> {
        Err(cannot_flatten("a sequence"))
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, M::Error> {
        Err(cannot_flatten("a tuple"))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, M::Error> {
        Err(cannot_flatten("a tuple struct"))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, M::Error> {
        Err(cannot_flatten("a tuple variant"))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, M::Error> {
        Ok(FlatMapSerializeMap(self.0))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, M::Error> {
        Ok(FlatMapSerializeStruct(self.0))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, M::Error> {
        Err(cannot_flatten("a struct variant"))
    }
}

pub struct FlatMapSerializeMap<'a, M>(&'a mut M);

impl<'a, M: SerializeMap + 'a> SerializeMap for FlatMapSerializeMap<'a, M> {
    type Ok = ();
    type Error = M::Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), M::Error> {
        self.0.serialize_key(key)
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), M::Error> {
        self.0.serialize_value(value)
    }

    fn serialize_entry<K: Serialize + ?Sized, V: Serialize + ?Sized>(
        &mut self,
        key: &K,
        value: &V,
    ) -> Result<(), M::Error> {
        self.0.serialize_entry(key, value)
    }

    fn end(self) -> Result<(), M::Error> {
        Ok(())
    }
}

pub struct FlatMapSerializeStruct<'a, M>(&'a mut M);

impl<'a, M: SerializeMap + 'a> SerializeStruct for FlatMapSerializeStruct<'a, M> {
    type Ok = ();
    type Error = M::Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), M::Error> {
        self.0.serialize_entry(key, value)
    }

    fn end(self) -> Result<(), M::Error> {
        Ok(())
    }
}
