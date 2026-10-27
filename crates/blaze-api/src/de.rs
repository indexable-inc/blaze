//! A [`Value`] as any serde value.

use serde::de::{self, IntoDeserializer as _, Visitor};

use crate::{Error, Value};

/// The message `value` is.
pub fn from_value<T: de::DeserializeOwned>(value: Value) -> Result<T, Error> {
    T::deserialize(value)
}

impl de::Error for Error {
    fn custom<T: std::fmt::Display>(msg: T) -> Error {
        Error::Schema(msg.to_string())
    }
}

impl<'de> de::IntoDeserializer<'de, Error> for Value {
    type Deserializer = Value;
    fn into_deserializer(self) -> Value {
        self
    }
}

impl<'de> de::Deserializer<'de> for Value {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Value::Uint(n) => visitor.visit_u64(n),
            Value::Nint(n) => match i64::try_from(n) {
                Ok(n) => visitor.visit_i64(-1 - n),
                Err(_) => visitor.visit_i128(-1 - i128::from(n)),
            },
            Value::Bytes(bytes) => visitor.visit_byte_buf(bytes),
            Value::Text(text) => visitor.visit_string(text),
            Value::Array(items) => {
                let mut seq = de::value::SeqDeserializer::new(items.into_iter());
                let value = visitor.visit_seq(&mut seq)?;
                seq.end()?;
                Ok(value)
            }
            Value::Map(entries) => {
                let mut map = de::value::MapDeserializer::new(entries.into_iter());
                let value = visitor.visit_map(&mut map)?;
                map.end()?;
                Ok(value)
            }
            Value::Bool(v) => visitor.visit_bool(v),
            Value::Null => visitor.visit_unit(),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Value::Null => visitor.visit_none(),
            value => visitor.visit_some(value),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self {
            Value::Text(variant) => visitor.visit_enum(variant.into_deserializer()),
            Value::Map(entries) if entries.len() == 1 => {
                let Some((Value::Text(variant), content)) = entries.into_iter().next() else {
                    return Err(Error::Schema("an enum tagged by other than its name".to_owned()));
                };
                visitor.visit_enum(Tagged { variant, content })
            }
            _ => Err(Error::Schema(
                "an enum that is neither a name nor a map of one entry".to_owned(),
            )),
        }
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        unit unit_struct seq tuple tuple_struct map struct identifier ignored_any
    }
}

struct Tagged {
    variant: String,
    content: Value,
}

impl<'de> de::EnumAccess<'de> for Tagged {
    type Error = Error;
    type Variant = Value;
    fn variant_seed<V: de::DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Value), Error> {
        let variant = seed.deserialize(Value::Text(self.variant))?;
        Ok((variant, self.content))
    }
}

impl<'de> de::VariantAccess<'de> for Value {
    type Error = Error;
    fn unit_variant(self) -> Result<(), Error> {
        match self {
            Value::Null => Ok(()),
            _ => Err(Error::Schema("a unit variant with content".to_owned())),
        }
    }
    fn newtype_variant_seed<T: de::DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        seed.deserialize(self)
    }
    fn tuple_variant<V: Visitor<'de>>(self, _: usize, visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_any(self, visitor)
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_any(self, visitor)
    }
}
