//! Values back from bytes [`check`] has passed.

use serde::de::{self, Deserialize, DeserializeSeed, IntoDeserializer as _, Visitor};

use super::{ARRAY, BYTES, Error, MAP, NEGATIVE, NULL, RAW, SIMPLE, TEXT, UNSIGNED, check, from_half};

/// The value of type `T` that `bytes` encode: one item in the core deterministic encoding,
/// or the first offence against it.
pub fn from_slice<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, Error> {
    check(bytes)?;
    T::deserialize(&mut Decoder { bytes, at: 0 })
}

struct Decoder<'de> {
    bytes: &'de [u8],
    at: usize,
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

    /// The major type, additional information and argument of the next head.
    fn head(&mut self) -> Result<(u8, u8, u64), Error> {
        let first = self.peek()?;
        self.at += 1;
        let info = first & 0x1f;
        let n = match info {
            0..=23 => u64::from(info),
            24..=27 => {
                let arg = self.take(1 << (info - 24))?;
                arg.iter().fold(0, |n, &byte| (n << 8) | u64::from(byte))
            }
            _ => return Err(self.offence("an indefinite length or a reserved head")),
        };
        Ok((first >> 5, info, n))
    }

    /// How many items of a count of `n` can be in the bytes left: each takes one at least.
    fn bounded(&self, n: u64) -> usize {
        usize::try_from(n).unwrap_or(usize::MAX).min(self.bytes.len() - self.at)
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
            TEXT => {
                let text = self.take(n)?;
                let text = std::str::from_utf8(text).map_err(|_| self.offence("text not UTF-8"))?;
                visitor.visit_borrowed_str(text)
            }
            ARRAY => {
                let left = self.bounded(n);
                visitor.visit_seq(Items { de: self, left: n, hint: left })
            }
            MAP => {
                let left = self.bounded(n);
                visitor.visit_map(Items { de: self, left: n, hint: left / 2 })
            }
            SIMPLE => match info {
                20 => visitor.visit_bool(false),
                21 => visitor.visit_bool(true),
                22 => visitor.visit_unit(),
                25 => visitor.visit_f64(from_half(n as u16)),
                26 => visitor.visit_f64(f32::from_bits(n as u32).into()),
                27 => visitor.visit_f64(f64::from_bits(n)),
                _ => Err(self.offence("a simple value other than false, true and null")),
            },
            _ => Err(self.offence("a tag")),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.peek()? == NULL {
            self.at += 1;
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    /// A [`crate::cbor::Raw`] is handed its item's bytes, which [`check`] has passed.
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        if name == RAW {
            let start = self.at;
            crate::cbor::check::item(self.bytes, &mut self.at, 0)?;
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
                let name = self.take(n)?;
                let name = std::str::from_utf8(name).map_err(|_| self.offence("text not UTF-8"))?;
                visitor.visit_enum(name.into_deserializer())
            }
            MAP => match self.head()? {
                (_, _, 1) => visitor.visit_enum(Variant { de: self }),
                _ => Err(self.offence("a variant that is not a map of one entry")),
            },
            _ => Err(self.offence("a variant that is neither a name nor a map of one entry")),
        }
    }

    /// A field or a variant, named only as text.
    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.peek()? >> 5 != TEXT {
            return Err(self.offence("a field or variant not named as text"));
        }
        let (_, _, n) = self.head()?;
        let name = self.take(n)?;
        let name = std::str::from_utf8(name).map_err(|_| self.offence("text not UTF-8"))?;
        visitor.visit_borrowed_str(name)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        unit unit_struct seq tuple tuple_struct map struct ignored_any
    }
}

/// The items of an array, or the keys and values of a map, `left` of them still to come.
struct Items<'a, 'de> {
    de: &'a mut Decoder<'de>,
    left: u64,
    hint: usize,
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
        seed.deserialize(&mut *self.de).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
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
