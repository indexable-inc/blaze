//! Values to their one encoding.

use serde::ser::{self, Serialize};

use super::{ARRAY, BYTES, Error, FALSE, MAP, NEGATIVE, NULL, TEXT, TRUE, UNSIGNED, float, head};

/// The deterministic encoding of `value`.
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    // alloc: test and fuzz reference only.
    let mut out = Vec::new();
    value.serialize(Encoder(&mut out))?;
    Ok(out)
}

struct Encoder<'a>(&'a mut Vec<u8>);

fn text(out: &mut Vec<u8>, text: &str) {
    head(out, TEXT, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

impl<'a> ser::Serializer for Encoder<'a> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Seq<'a>;
    type SerializeTuple = Seq<'a>;
    type SerializeTupleStruct = Seq<'a>;
    type SerializeTupleVariant = Seq<'a>;
    type SerializeMap = Map<'a>;
    type SerializeStruct = Map<'a>;
    type SerializeStructVariant = Map<'a>;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn serialize_bool(self, value: bool) -> Result<(), Error> {
        self.0.push(if value { TRUE } else { FALSE });
        Ok(())
    }

    fn serialize_i8(self, value: i8) -> Result<(), Error> {
        self.serialize_i64(value.into())
    }

    fn serialize_i16(self, value: i16) -> Result<(), Error> {
        self.serialize_i64(value.into())
    }

    fn serialize_i32(self, value: i32) -> Result<(), Error> {
        self.serialize_i64(value.into())
    }

    fn serialize_i64(self, value: i64) -> Result<(), Error> {
        match u64::try_from(value) {
            Ok(n) => head(self.0, UNSIGNED, n),
            // -1 - value, which is what a negative integer's argument is.
            Err(_) => head(self.0, NEGATIVE, !value as u64),
        }
        Ok(())
    }

    fn serialize_i128(self, value: i128) -> Result<(), Error> {
        if let Ok(n) = u64::try_from(value) {
            head(self.0, UNSIGNED, n);
        } else if let Ok(n) = u64::try_from(-1 - value) {
            head(self.0, NEGATIVE, n);
        } else {
            return Err(Error::Unsupported("an integer outside -2^64..2^64"));
        }
        Ok(())
    }

    fn serialize_u8(self, value: u8) -> Result<(), Error> {
        self.serialize_u64(value.into())
    }

    fn serialize_u16(self, value: u16) -> Result<(), Error> {
        self.serialize_u64(value.into())
    }

    fn serialize_u32(self, value: u32) -> Result<(), Error> {
        self.serialize_u64(value.into())
    }

    fn serialize_u64(self, value: u64) -> Result<(), Error> {
        head(self.0, UNSIGNED, value);
        Ok(())
    }

    fn serialize_u128(self, value: u128) -> Result<(), Error> {
        let value = u64::try_from(value)
            .map_err(|_| Error::Unsupported("an integer outside -2^64..2^64"))?;
        self.serialize_u64(value)
    }

    fn serialize_f32(self, value: f32) -> Result<(), Error> {
        float(self.0, value.into());
        Ok(())
    }

    fn serialize_f64(self, value: f64) -> Result<(), Error> {
        float(self.0, value);
        Ok(())
    }

    fn serialize_char(self, value: char) -> Result<(), Error> {
        text(self.0, value.encode_utf8(&mut [0; 4]));
        Ok(())
    }

    fn serialize_str(self, value: &str) -> Result<(), Error> {
        text(self.0, value);
        Ok(())
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<(), Error> {
        head(self.0, BYTES, value.len() as u64);
        self.0.extend_from_slice(value);
        Ok(())
    }

    fn serialize_none(self) -> Result<(), Error> {
        self.0.push(NULL);
        Ok(())
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), Error> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), Error> {
        self.0.push(NULL);
        Ok(())
    }

    fn serialize_unit_struct(self, _: &'static str) -> Result<(), Error> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
    ) -> Result<(), Error> {
        text(self.0, variant);
        Ok(())
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        head(self.0, MAP, 1);
        text(self.0, variant);
        value.serialize(self)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Seq<'a>, Error> {
        Ok(Seq::new(self.0, len))
    }

    fn serialize_tuple(self, len: usize) -> Result<Seq<'a>, Error> {
        Ok(Seq::new(self.0, Some(len)))
    }

    fn serialize_tuple_struct(self, _: &'static str, len: usize) -> Result<Seq<'a>, Error> {
        Ok(Seq::new(self.0, Some(len)))
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Seq<'a>, Error> {
        head(self.0, MAP, 1);
        text(self.0, variant);
        Ok(Seq::new(self.0, Some(len)))
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Map<'a>, Error> {
        Ok(Map::new(self.0))
    }

    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Map<'a>, Error> {
        Ok(Map::new(self.0))
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> Result<Map<'a>, Error> {
        head(self.0, MAP, 1);
        text(self.0, variant);
        Ok(Map::new(self.0))
    }
}

/// An array. One whose length is said up front is written as it comes; one whose is not
/// is kept until it is known, since a definite length comes first.
pub(crate) struct Seq<'a> {
    out: &'a mut Vec<u8>,
    held: Option<Vec<u8>>,
    said: Option<usize>,
    count: usize,
}

impl<'a> Seq<'a> {
    fn new(out: &'a mut Vec<u8>, len: Option<usize>) -> Seq<'a> {
        if let Some(len) = len {
            head(out, ARRAY, len as u64);
        }
        Seq { out, held: len.is_none().then(Vec::new), said: len, count: 0 }
    }

    fn item<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        match &mut self.held {
            Some(held) => value.serialize(Encoder(held))?,
            None => value.serialize(Encoder(&mut *self.out))?,
        }
        self.count += 1;
        Ok(())
    }

    fn end(self) -> Result<(), Error> {
        match self.held {
            Some(held) => {
                head(self.out, ARRAY, self.count as u64);
                self.out.extend(held);
            }
            None if self.said != Some(self.count) => {
                return Err(Error::Unsupported("a sequence of another length than it said"));
            }
            None => {}
        }
        Ok(())
    }
}

impl ser::SerializeSeq for Seq<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.item(value)
    }

    fn end(self) -> Result<(), Error> {
        Seq::end(self)
    }
}

impl ser::SerializeTuple for Seq<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.item(value)
    }

    fn end(self) -> Result<(), Error> {
        Seq::end(self)
    }
}

impl ser::SerializeTupleStruct for Seq<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.item(value)
    }

    fn end(self) -> Result<(), Error> {
        Seq::end(self)
    }
}

impl ser::SerializeTupleVariant for Seq<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.item(value)
    }

    fn end(self) -> Result<(), Error> {
        Seq::end(self)
    }
}

/// A map, kept until its last entry: its entries go out sorted by the bytes of their keys.
pub(crate) struct Map<'a> {
    out: &'a mut Vec<u8>,
    entries: Vec<u8>,
    /// Where each entry's key begins and ends in `entries`. Its value follows the key, up
    /// to where the next entry begins.
    keys: Vec<(usize, usize)>,
}

impl<'a> Map<'a> {
    fn new(out: &'a mut Vec<u8>) -> Map<'a> {
        // alloc: test and fuzz reference only.
        Map { out, entries: Vec::new(), keys: Vec::new() }
    }

    fn key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        let start = self.entries.len();
        key.serialize(Encoder(&mut self.entries))?;
        self.keys.push((start, self.entries.len()));
        Ok(())
    }

    fn value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(Encoder(&mut self.entries))
    }

    fn end(self) -> Result<(), Error> {
        let Map { out, entries, keys } = self;
        let ends = keys.iter().skip(1).map(|it| it.0).chain([entries.len()]);
        let mut spans: Vec<(usize, usize, usize)> =
            // alloc: test and fuzz reference only.
            keys.iter().zip(ends).map(|(&(start, key), end)| (start, key, end)).collect();
        let key = |&(start, key_end, _): &(usize, usize, usize)| &entries[start..key_end];
        spans.sort_unstable_by(|a, b| key(a).cmp(key(b)));
        if spans.windows(2).any(|pair| key(&pair[0]) == key(&pair[1])) {
            return Err(Error::Unsupported("a map with one key twice"));
        }
        head(out, MAP, spans.len() as u64);
        for (start, _, end) in spans {
            out.extend_from_slice(&entries[start..end]);
        }
        Ok(())
    }
}

impl ser::SerializeMap for Map<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        self.key(key)
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.value(value)
    }

    fn end(self) -> Result<(), Error> {
        Map::end(self)
    }
}

impl ser::SerializeStruct for Map<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.key(key)?;
        self.value(value)
    }

    fn end(self) -> Result<(), Error> {
        Map::end(self)
    }
}

impl ser::SerializeStructVariant for Map<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.key(key)?;
        self.value(value)
    }

    fn end(self) -> Result<(), Error> {
        Map::end(self)
    }
}
