//! Any serde value as a [`Value`], refused when it has a float, an integer CBOR has no head
//! for, or a map key twice.

use serde::ser::{self, Serialize};

use crate::{Error, Value};

/// `value` in the CBOR data model.
pub fn to_value<T: Serialize + ?Sized>(value: &T) -> Result<Value, Error> {
    value.serialize(Serializer)
}

impl ser::Error for Error {
    fn custom<T: std::fmt::Display>(msg: T) -> Error {
        Error::Schema(msg.to_string())
    }
}

struct Serializer;

fn map(entries: Vec<(Value, Value)>) -> Result<Value, Error> {
    Value::map(entries).map_err(|key| Error::Schema(format!("the key {key:?} is there twice")))
}

fn tagged(variant: &str, content: Value) -> Result<Value, Error> {
    map(vec![(Value::Text(variant.to_owned()), content)])
}

fn integer(n: i128) -> Result<Value, Error> {
    match n {
        0.. => u64::try_from(n).map(Value::Uint),
        _ => u64::try_from(-1 - n).map(Value::Nint),
    }
    .map_err(|_| Error::Schema(format!("{n} is out of CBOR's range")))
}

impl ser::Serializer for Serializer {
    type Ok = Value;
    type Error = Error;
    type SerializeSeq = Seq;
    type SerializeTuple = Seq;
    type SerializeTupleStruct = Seq;
    type SerializeTupleVariant = Seq;
    type SerializeMap = Map;
    type SerializeStruct = Map;
    type SerializeStructVariant = Map;

    fn serialize_bool(self, v: bool) -> Result<Value, Error> {
        Ok(Value::Bool(v))
    }
    fn serialize_i8(self, v: i8) -> Result<Value, Error> {
        integer(v.into())
    }
    fn serialize_i16(self, v: i16) -> Result<Value, Error> {
        integer(v.into())
    }
    fn serialize_i32(self, v: i32) -> Result<Value, Error> {
        integer(v.into())
    }
    fn serialize_i64(self, v: i64) -> Result<Value, Error> {
        integer(v.into())
    }
    fn serialize_i128(self, v: i128) -> Result<Value, Error> {
        integer(v)
    }
    fn serialize_u8(self, v: u8) -> Result<Value, Error> {
        Ok(Value::Uint(v.into()))
    }
    fn serialize_u16(self, v: u16) -> Result<Value, Error> {
        Ok(Value::Uint(v.into()))
    }
    fn serialize_u32(self, v: u32) -> Result<Value, Error> {
        Ok(Value::Uint(v.into()))
    }
    fn serialize_u64(self, v: u64) -> Result<Value, Error> {
        Ok(Value::Uint(v))
    }
    fn serialize_u128(self, v: u128) -> Result<Value, Error> {
        u64::try_from(v)
            .map(Value::Uint)
            .map_err(|_| Error::Schema(format!("{v} is out of CBOR's range")))
    }
    fn serialize_f32(self, _: f32) -> Result<Value, Error> {
        Err(Error::Schema("a float, which the schema has none of".to_owned()))
    }
    fn serialize_f64(self, _: f64) -> Result<Value, Error> {
        Err(Error::Schema("a float, which the schema has none of".to_owned()))
    }
    fn serialize_char(self, v: char) -> Result<Value, Error> {
        Ok(Value::Text(v.to_string()))
    }
    fn serialize_str(self, v: &str) -> Result<Value, Error> {
        Ok(Value::Text(v.to_owned()))
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Value, Error> {
        Ok(Value::Bytes(v.to_vec()))
    }
    fn serialize_none(self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Value, Error> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
    ) -> Result<Value, Error> {
        Ok(Value::Text(variant.to_owned()))
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        value: &T,
    ) -> Result<Value, Error> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Value, Error> {
        tagged(variant, value.serialize(Serializer)?)
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Seq, Error> {
        Ok(Seq { items: Vec::with_capacity(len.unwrap_or(0)), variant: None })
    }
    fn serialize_tuple(self, len: usize) -> Result<Seq, Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(self, _: &'static str, len: usize) -> Result<Seq, Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Seq, Error> {
        Ok(Seq { items: Vec::with_capacity(len), variant: Some(variant) })
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Map, Error> {
        Ok(Map { entries: Vec::with_capacity(len.unwrap_or(0)), key: None, variant: None })
    }
    fn serialize_struct(self, _: &'static str, len: usize) -> Result<Map, Error> {
        self.serialize_map(Some(len))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Map, Error> {
        Ok(Map { entries: Vec::with_capacity(len), key: None, variant: Some(variant) })
    }
}

struct Seq {
    items: Vec<Value>,
    variant: Option<&'static str>,
}

impl Seq {
    fn push<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.items.push(value.serialize(Serializer)?);
        Ok(())
    }

    fn done(self) -> Result<Value, Error> {
        let array = Value::Array(self.items);
        match self.variant {
            Some(variant) => tagged(variant, array),
            None => Ok(array),
        }
    }
}

impl ser::SerializeSeq for Seq {
    type Ok = Value;
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push(value)
    }
    fn end(self) -> Result<Value, Error> {
        self.done()
    }
}

impl ser::SerializeTuple for Seq {
    type Ok = Value;
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push(value)
    }
    fn end(self) -> Result<Value, Error> {
        self.done()
    }
}

impl ser::SerializeTupleStruct for Seq {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push(value)
    }
    fn end(self) -> Result<Value, Error> {
        self.done()
    }
}

impl ser::SerializeTupleVariant for Seq {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push(value)
    }
    fn end(self) -> Result<Value, Error> {
        self.done()
    }
}

struct Map {
    entries: Vec<(Value, Value)>,
    key: Option<Value>,
    variant: Option<&'static str>,
}

impl Map {
    fn done(self) -> Result<Value, Error> {
        let map = map(self.entries)?;
        match self.variant {
            Some(variant) => tagged(variant, map),
            None => Ok(map),
        }
    }
}

impl ser::SerializeMap for Map {
    type Ok = Value;
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        self.key = Some(key.serialize(Serializer)?);
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        let key = self
            .key
            .take()
            .ok_or_else(|| Error::Schema("a map value without its key".to_owned()))?;
        self.entries.push((key, value.serialize(Serializer)?));
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        self.done()
    }
}

impl ser::SerializeStruct for Map {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.entries.push((Value::Text(key.to_owned()), value.serialize(Serializer)?));
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        self.done()
    }
}

impl ser::SerializeStructVariant for Map {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.entries.push((Value::Text(key.to_owned()), value.serialize(Serializer)?));
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        self.done()
    }
}
