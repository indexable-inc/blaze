//! A JSON-shaped text format, enough to hold serde's derives to what they promise: what a
//! value serializes to as text, and a tree of borrowed values it deserializes from.

use serde::de::value::{Error, MapDeserializer, SeqDeserializer};
use serde::de::{self, IntoDeserializer, Visitor};
use serde::ser::{self, Serialize};

/// `value` as text: maps and structs as `{"k":v}`, sequences and tuples as `[..]`, a variant
/// with data as `{"name":data}`, a unit variant as `"name"`, `None` and `()` as `null`.
pub fn to_text<T: Serialize + ?Sized>(value: &T) -> Result<String, Error> {
    let mut text = Text(String::new());
    value.serialize(&mut text)?;
    Ok(text.0)
}

pub struct Text(String);

pub struct Compound<'a> {
    text: &'a mut Text,
    first: bool,
    close: &'static str,
}

impl Text {
    fn open(&mut self, open: &str, close: &'static str) -> Compound<'_> {
        self.0.push_str(open);
        Compound { text: self, first: true, close }
    }

    fn variant(&mut self, variant: &str, open: &str, close: &'static str) -> Compound<'_> {
        self.0.push_str(&format!("{{{variant:?}:{open}"));
        Compound { text: self, first: true, close }
    }
}

impl Compound<'_> {
    fn comma(&mut self) {
        if !self.first {
            self.text.0.push(',');
        }
        self.first = false;
    }

    fn element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.comma();
        value.serialize(&mut *self.text)
    }

    fn field<T: Serialize + ?Sized>(&mut self, key: &str, value: &T) -> Result<(), Error> {
        self.comma();
        self.text.0.push_str(&format!("{key:?}:"));
        value.serialize(&mut *self.text)
    }

    fn close(self) -> Result<(), Error> {
        self.text.0.push_str(self.close);
        Ok(())
    }
}

impl<'a> ser::Serializer for &'a mut Text {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Compound<'a>;
    type SerializeTuple = Compound<'a>;
    type SerializeTupleStruct = Compound<'a>;
    type SerializeTupleVariant = Compound<'a>;
    type SerializeMap = Compound<'a>;
    type SerializeStruct = Compound<'a>;
    type SerializeStructVariant = Compound<'a>;

    fn serialize_bool(self, v: bool) -> Result<(), Error> {
        self.0.push_str(if v { "true" } else { "false" });
        Ok(())
    }
    fn serialize_i8(self, v: i8) -> Result<(), Error> {
        self.serialize_i64(v.into())
    }
    fn serialize_i16(self, v: i16) -> Result<(), Error> {
        self.serialize_i64(v.into())
    }
    fn serialize_i32(self, v: i32) -> Result<(), Error> {
        self.serialize_i64(v.into())
    }
    fn serialize_i64(self, v: i64) -> Result<(), Error> {
        self.0.push_str(&v.to_string());
        Ok(())
    }
    fn serialize_u8(self, v: u8) -> Result<(), Error> {
        self.serialize_u64(v.into())
    }
    fn serialize_u16(self, v: u16) -> Result<(), Error> {
        self.serialize_u64(v.into())
    }
    fn serialize_u32(self, v: u32) -> Result<(), Error> {
        self.serialize_u64(v.into())
    }
    fn serialize_u64(self, v: u64) -> Result<(), Error> {
        self.0.push_str(&v.to_string());
        Ok(())
    }
    fn serialize_f32(self, v: f32) -> Result<(), Error> {
        self.serialize_f64(v.into())
    }
    fn serialize_f64(self, v: f64) -> Result<(), Error> {
        self.0.push_str(&v.to_string());
        Ok(())
    }
    fn serialize_char(self, v: char) -> Result<(), Error> {
        self.serialize_str(v.encode_utf8(&mut [0; 4]))
    }
    fn serialize_str(self, v: &str) -> Result<(), Error> {
        self.0.push_str(&format!("{v:?}"));
        Ok(())
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
        let mut seq = self.open("[", "]");
        for byte in v {
            seq.element(byte)?;
        }
        seq.close()
    }
    fn serialize_none(self) -> Result<(), Error> {
        self.serialize_unit()
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), Error> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<(), Error> {
        self.0.push_str("null");
        Ok(())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Error> {
        self.serialize_unit()
    }
    fn serialize_unit_variant(self, _name: &'static str, _index: u32, variant: &'static str) -> Result<(), Error> {
        self.serialize_str(variant)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _name: &'static str, value: &T) -> Result<(), Error> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.0.push_str(&format!("{{{variant:?}:"));
        value.serialize(&mut *self)?;
        self.0.push('}');
        Ok(())
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Compound<'a>, Error> {
        Ok(self.open("[", "]"))
    }
    fn serialize_tuple(self, _len: usize) -> Result<Compound<'a>, Error> {
        Ok(self.open("[", "]"))
    }
    fn serialize_tuple_struct(self, _name: &'static str, _len: usize) -> Result<Compound<'a>, Error> {
        Ok(self.open("[", "]"))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Compound<'a>, Error> {
        Ok(self.variant(variant, "[", "]}"))
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Compound<'a>, Error> {
        Ok(self.open("{", "}"))
    }
    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Compound<'a>, Error> {
        Ok(self.open("{", "}"))
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Compound<'a>, Error> {
        Ok(self.variant(variant, "{", "}}"))
    }
}

impl ser::SerializeSeq for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.element(value)
    }
    fn end(self) -> Result<(), Error> {
        self.close()
    }
}

impl ser::SerializeTuple for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.element(value)
    }
    fn end(self) -> Result<(), Error> {
        self.close()
    }
}

impl ser::SerializeTupleStruct for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.element(value)
    }
    fn end(self) -> Result<(), Error> {
        self.close()
    }
}

impl ser::SerializeTupleVariant for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.element(value)
    }
    fn end(self) -> Result<(), Error> {
        self.close()
    }
}

impl ser::SerializeMap for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        self.comma();
        key.serialize(&mut *self.text)?;
        self.text.0.push(':');
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut *self.text)
    }
    fn end(self) -> Result<(), Error> {
        self.close()
    }
}

impl ser::SerializeStruct for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), Error> {
        self.field(key, value)
    }
    fn end(self) -> Result<(), Error> {
        self.close()
    }
}

impl ser::SerializeStructVariant for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), Error> {
        self.field(key, value)
    }
    fn end(self) -> Result<(), Error> {
        self.close()
    }
}

/// A value to deserialize from, its strings borrowed so `#[serde(borrow)]` fields can borrow.
#[derive(Clone, Debug)]
pub enum In<'a> {
    Null,
    U(u64),
    I(i64),
    F(f64),
    S(&'a str),
    Seq(Vec<In<'a>>),
    Map(Vec<(&'a str, In<'a>)>),
}

impl<'a> IntoDeserializer<'a, Error> for In<'a> {
    type Deserializer = In<'a>;
    fn into_deserializer(self) -> In<'a> {
        self
    }
}

impl<'a> de::Deserializer<'a> for In<'a> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            In::Null => visitor.visit_unit(),
            In::U(v) => visitor.visit_u64(v),
            In::I(v) => visitor.visit_i64(v),
            In::F(v) => visitor.visit_f64(v),
            In::S(v) => visitor.visit_borrowed_str(v),
            In::Seq(items) => visitor.visit_seq(SeqDeserializer::new(items.into_iter())),
            In::Map(entries) => {
                let entries = entries.into_iter().map(|(key, value)| (In::S(key), value));
                visitor.visit_map(MapDeserializer::new(entries))
            }
        }
    }

    fn deserialize_option<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            In::Null => visitor.visit_none(),
            some => visitor.visit_some(some),
        }
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier
        ignored_any
    }
}

/// `T` from `value`.
pub fn from<'a, T: de::Deserialize<'a>>(value: In<'a>) -> Result<T, Error> {
    T::deserialize(value)
}
