//! Values back from bytes in one pass: the core deterministic encoding is checked as the bytes
//! are read (shortest heads, key order, floats, UTF-8, depth), and what the type does not read
//! (an ignored field, the rest of a tuple) is checked as it is passed over. Only bytes that do
//! not decode are read again, by [`check`], so the error is the one the two passes gave: the
//! first offence against the encoding wherever it is, else what the type found.

use std::cmp::Ordering;

use serde::de::{self, Deserialize, DeserializeSeed, IntoDeserializer as _, Visitor};

use super::{
    ARRAY, BYTES, Error, MAP, NAN, NEGATIVE, NULL, RAW, SIMPLE, TEXT, UNSIGNED, check, from_half,
    half_of, key_order,
};
use super::check::{DEEPEST, item};

/// The value of type `T` that `bytes` encode: one item in the core deterministic encoding,
/// or the first offence against it.
pub fn from_slice<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, Error> {
    let mut decoder = Decoder { bytes, at: 0, depth: 0 };
    let read = T::deserialize(&mut decoder).and_then(|value| match decoder.at == bytes.len() {
        true => Ok(value),
        false => Err(decoder.offence("bytes after the item")),
    });
    read.map_err(|error| check(bytes).err().unwrap_or(error))
}

/// The value of type `T` that the item at the start of `bytes` encodes, and how many bytes the
/// item is. For bytes already checked (a [`super::Raw`]'s), so an error is what the type found.
pub(super) fn from_front<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<(T, usize), Error> {
    let mut decoder = Decoder { bytes, at: 0, depth: 0 };
    let value = T::deserialize(&mut decoder)?;
    Ok((value, decoder.at))
}

/// The count of the array at the start of `bytes` and how many bytes its head is; `None` when
/// the item there is not an array.
pub(super) fn array_head(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut decoder = Decoder { bytes, at: 0, depth: 0 };
    match decoder.head() {
        Ok((ARRAY, _, n)) => Some((n, decoder.at)),
        _ => None,
    }
}

struct Decoder<'de> {
    bytes: &'de [u8],
    at: usize,
    /// How deep the next item is.
    depth: u32,
}

impl<'de> Decoder<'de> {
    fn offence(&self, what: &'static str) -> Error {
        Error::NotDeterministic { at: self.at, what }
    }

    fn peek(&self) -> Result<u8, Error> {
        self.bytes.get(self.at).copied().ok_or_else(|| self.offence("the item is cut short"))
    }

    fn take(&mut self, len: u64) -> Result<&'de [u8], Error> {
        let left = self.bytes.len() - self.at;
        let len = usize::try_from(len).ok().filter(|&len| len <= left);
        let len = len.ok_or_else(|| self.offence("a length past the end"))?;
        let taken = &self.bytes[self.at..self.at + len];
        self.at += len;
        Ok(taken)
    }

    fn deep(&self) -> Result<(), Error> {
        match self.depth > DEEPEST {
            true => Err(self.offence("items nested deeper than 256")),
            false => Ok(()),
        }
    }

    /// The major type, additional information and argument of the next head, which is in
    /// its shortest form.
    fn head(&mut self) -> Result<(u8, u8, u64), Error> {
        self.deep()?;
        let first = self.peek()?;
        self.at += 1;
        let (major, info) = (first >> 5, first & 0x1f);
        let n = match info {
            0..=23 => return Ok((major, info, u64::from(info))),
            24 => u64::from(self.take(1)?[0]),
            25 => u64::from(u16::from_be_bytes(self.take(2)?.try_into().expect("two bytes"))),
            26 => u64::from(u32::from_be_bytes(self.take(4)?.try_into().expect("four bytes"))),
            27 => u64::from_be_bytes(self.take(8)?.try_into().expect("eight bytes")),
            28..=30 => return Err(self.offence("a reserved head")),
            _ => return Err(self.offence("an indefinite length")),
        };
        let shortest = match info {
            24 => n >= 24,
            25 => n > 0xff,
            26 => n > 0xffff,
            _ => n > 0xffff_ffff,
        };
        if major != SIMPLE && !shortest {
            return Err(self.offence("an argument in more bytes than it needs"));
        }
        Ok((major, info, n))
    }

    fn text(&mut self, n: u64) -> Result<&'de str, Error> {
        let text = self.take(n)?;
        std::str::from_utf8(text).map_err(|_| self.offence("text not UTF-8"))
    }

    /// How many items of a count of `n` can be in the bytes left: each takes one at least.
    fn bounded(&self, n: u64) -> usize {
        usize::try_from(n).unwrap_or(usize::MAX).min(self.bytes.len() - self.at)
    }

    /// Passes over the next item, checking it.
    fn pass(&mut self) -> Result<(), Error> {
        item(self.bytes, &mut self.at, self.depth)
    }
}

impl<'de> de::Deserializer<'de> for &mut Decoder<'de> {
    type Error = Error;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let (major, info, n) = self.head()?;
        match major {
            UNSIGNED => visitor.visit_u64(n),
            NEGATIVE => match i64::try_from(n) {
                Ok(n) => visitor.visit_i64(-1 - n),
                Err(_) => visitor.visit_i128(-1 - i128::from(n)),
            },
            BYTES => visitor.visit_borrowed_bytes(self.take(n)?),
            TEXT => visitor.visit_borrowed_str(self.text(n)?),
            ARRAY => {
                let hint = self.bounded(n);
                self.depth += 1;
                let mut items = Items { de: &mut *self, left: n, hint, last: None, due: false };
                let value = visitor.visit_seq(&mut items)?;
                items.pass_items()?;
                self.depth -= 1;
                Ok(value)
            }
            MAP => {
                let hint = self.bounded(n) / 2;
                self.depth += 1;
                let mut items = Items { de: &mut *self, left: n, hint, last: None, due: false };
                let value = visitor.visit_map(&mut items)?;
                items.pass_entries()?;
                self.depth -= 1;
                Ok(value)
            }
            SIMPLE => match info {
                20 => visitor.visit_bool(false),
                21 => visitor.visit_bool(true),
                22 => visitor.visit_unit(),
                25 => {
                    if from_half(n as u16).is_nan() && n != u64::from(NAN) {
                        return Err(self.offence("a NaN other than f9 7e00"));
                    }
                    visitor.visit_f64(from_half(n as u16))
                }
                26 => {
                    let single = f32::from_bits(n as u32);
                    if single.is_nan() || half_of(single).is_some() {
                        return Err(self.offence("a single that a half holds"));
                    }
                    visitor.visit_f64(single.into())
                }
                27 => {
                    let double = f64::from_bits(n);
                    if double.is_nan() || f64::from(double as f32).to_bits() == n {
                        return Err(self.offence("a double that a single holds"));
                    }
                    visitor.visit_f64(double)
                }
                _ => Err(self.offence("a simple value other than false, true and null")),
            },
            _ => Err(self.offence("a tag")),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.peek()? == NULL {
            self.deep()?;
            self.at += 1;
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    /// A [`super::Raw`] is handed its item's bytes, checked as far down as any item is (depth
    /// included) and passed over.
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        if name == RAW {
            let start = self.at;
            self.pass()?;
            return visitor.visit_borrowed_bytes(&self.bytes[start..self.at]);
        }
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self.peek()? >> 5 {
            TEXT => {
                let (_, _, n) = self.head()?;
                let name = self.text(n)?;
                visitor.visit_enum(name.into_deserializer())
            }
            MAP => match self.head()? {
                (_, _, 1) => {
                    self.depth += 1;
                    let value = visitor.visit_enum(Variant { de: &mut *self })?;
                    self.depth -= 1;
                    Ok(value)
                }
                _ => Err(self.offence("a variant that is not a map of one entry")),
            },
            _ => Err(self.offence("a variant that is neither a name nor a map of one entry")),
        }
    }

    /// A field or a variant, named only as text: serde's derives also take its index or its
    /// name as bytes, which would give one value more than one item.
    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.peek()? >> 5 != TEXT {
            return Err(self.offence("a field or variant not named as text"));
        }
        let (_, _, n) = self.head()?;
        visitor.visit_borrowed_str(self.text(n)?)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.pass()?;
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        unit unit_struct seq tuple tuple_struct map struct
    }
}

/// The items of an array, or the keys and values of a map, `left` of them still to come.
struct Items<'a, 'de> {
    de: &'a mut Decoder<'de>,
    left: u64,
    hint: usize,
    /// Where the last key of a map is, which the next must come after.
    last: Option<(usize, usize)>,
    /// A key was read and its value not yet.
    due: bool,
}

impl Items<'_, '_> {
    /// The key that was just read, at `start..` up to where the decoder is, after the last.
    fn in_order(&mut self, start: usize) -> Result<(), Error> {
        let bytes = self.de.bytes;
        let key = &bytes[start..self.de.at];
        if let Some((from, to)) = self.last {
            match key_order(&bytes[from..to], key) {
                Ordering::Less => {}
                Ordering::Equal => return Err(self.de.offence("a key twice")),
                Ordering::Greater => return Err(self.de.offence("map keys out of order")),
            }
        }
        self.last = Some((start, self.de.at));
        Ok(())
    }

    /// The items the visitor did not read, checked and passed over.
    fn pass_items(&mut self) -> Result<(), Error> {
        while self.left > 0 {
            self.left -= 1;
            self.de.pass()?;
        }
        Ok(())
    }

    /// The entries the visitor did not read, checked and passed over.
    fn pass_entries(&mut self) -> Result<(), Error> {
        if self.due {
            self.due = false;
            self.de.pass()?;
        }
        while self.left > 0 {
            self.left -= 1;
            let start = self.de.at;
            self.de.pass()?;
            self.in_order(start)?;
            self.de.pass()?;
        }
        Ok(())
    }
}

impl<'de> de::SeqAccess<'de> for Items<'_, 'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        self.hint = self.hint.saturating_sub(1);
        seed.deserialize(&mut *self.de).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.hint)
    }
}

impl<'de> de::MapAccess<'de> for Items<'_, 'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        self.hint = self.hint.saturating_sub(1);
        let start = self.de.at;
        let key = seed.deserialize(&mut *self.de)?;
        self.in_order(start)?;
        self.due = true;
        Ok(Some(key))
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        self.due = false;
        seed.deserialize(&mut *self.de)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.hint)
    }
}

/// A variant with contents: the one entry of a map, its name to them.
struct Variant<'a, 'de> {
    de: &'a mut Decoder<'de>,
}

impl<'a, 'de> de::EnumAccess<'de> for Variant<'a, 'de> {
    type Error = Error;
    type Variant = Variant<'a, 'de>;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self), Error> {
        let name = seed.deserialize(&mut *self.de)?;
        Ok((name, self))
    }
}

impl<'de> de::VariantAccess<'de> for Variant<'_, 'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        <()>::deserialize(self.de)
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        seed.deserialize(self.de)
    }

    fn tuple_variant<V: Visitor<'de>>(self, _: usize, visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_any(self.de, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_any(self.de, visitor)
    }
}
