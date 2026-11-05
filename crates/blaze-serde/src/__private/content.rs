//! A value read once and read again: what an internally tagged enum buffers to find its tag, an
//! untagged enum to try each variant on, and a struct with `#[serde(flatten)]` fields to hand
//! them the entries its own fields did not name.

use std::{fmt, marker::PhantomData, slice};

use serde::de::{
    self, Deserialize, DeserializeSeed, Deserializer, EnumAccess, Expected, MapAccess, SeqAccess,
    Unexpected, VariantAccess, Visitor,
};

/// Any value, as the input's `deserialize_any` gave it. A string or byte string the input lent
/// stays borrowed (`Str`, `Bytes`), so a field that borrows it can still borrow it.
pub enum Content<'de> {
    Bool(bool),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    U128(u128),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    I128(i128),
    F32(f32),
    F64(f64),
    Char(char),
    String(String),
    Str(&'de str),
    ByteBuf(Vec<u8>),
    Bytes(&'de [u8]),
    None,
    Some(Box<Content<'de>>),
    Unit,
    Newtype(Box<Content<'de>>),
    Seq(Vec<Content<'de>>),
    Map(Vec<(Content<'de>, Content<'de>)>),
}

/// The most a length hint the input sends may preallocate: a hint is only a hint, and a wrong
/// one must not allocate gigabytes.
const PREALLOCATE: usize = 4096;

impl<'de> Content<'de> {
    /// Whether this is the map key `key`, as a string or as its bytes.
    pub(crate) fn is_key(&self, key: &str) -> bool {
        match *self {
            Content::String(ref it) => it == key,
            Content::Str(it) => it == key,
            Content::ByteBuf(ref it) => it.as_slice() == key.as_bytes(),
            Content::Bytes(it) => it == key.as_bytes(),
            _ => false,
        }
    }

    pub(crate) fn unexpected(&self) -> Unexpected<'_> {
        match *self {
            Content::Bool(it) => Unexpected::Bool(it),
            Content::U8(it) => Unexpected::Unsigned(u64::from(it)),
            Content::U16(it) => Unexpected::Unsigned(u64::from(it)),
            Content::U32(it) => Unexpected::Unsigned(u64::from(it)),
            Content::U64(it) => Unexpected::Unsigned(it),
            Content::U128(_) => Unexpected::Other("a u128"),
            Content::I8(it) => Unexpected::Signed(i64::from(it)),
            Content::I16(it) => Unexpected::Signed(i64::from(it)),
            Content::I32(it) => Unexpected::Signed(i64::from(it)),
            Content::I64(it) => Unexpected::Signed(it),
            Content::I128(_) => Unexpected::Other("an i128"),
            Content::F32(it) => Unexpected::Float(f64::from(it)),
            Content::F64(it) => Unexpected::Float(it),
            Content::Char(it) => Unexpected::Char(it),
            Content::String(ref it) => Unexpected::Str(it),
            Content::Str(it) => Unexpected::Str(it),
            Content::ByteBuf(ref it) => Unexpected::Bytes(it),
            Content::Bytes(it) => Unexpected::Bytes(it),
            Content::None | Content::Some(_) => Unexpected::Option,
            Content::Unit => Unexpected::Unit,
            Content::Newtype(_) => Unexpected::NewtypeStruct,
            Content::Seq(_) => Unexpected::Seq,
            Content::Map(_) => Unexpected::Map,
        }
    }
}

impl<'de> Deserialize<'de> for Content<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ContentVisitor)
    }
}

struct ContentVisitor;

impl<'de> Visitor<'de> for ContentVisitor {
    type Value = Content<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Content::Bool(value))
    }

    fn visit_i8<E: de::Error>(self, value: i8) -> Result<Self::Value, E> {
        Ok(Content::I8(value))
    }

    fn visit_i16<E: de::Error>(self, value: i16) -> Result<Self::Value, E> {
        Ok(Content::I16(value))
    }

    fn visit_i32<E: de::Error>(self, value: i32) -> Result<Self::Value, E> {
        Ok(Content::I32(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Content::I64(value))
    }

    fn visit_i128<E: de::Error>(self, value: i128) -> Result<Self::Value, E> {
        Ok(Content::I128(value))
    }

    fn visit_u8<E: de::Error>(self, value: u8) -> Result<Self::Value, E> {
        Ok(Content::U8(value))
    }

    fn visit_u16<E: de::Error>(self, value: u16) -> Result<Self::Value, E> {
        Ok(Content::U16(value))
    }

    fn visit_u32<E: de::Error>(self, value: u32) -> Result<Self::Value, E> {
        Ok(Content::U32(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Content::U64(value))
    }

    fn visit_u128<E: de::Error>(self, value: u128) -> Result<Self::Value, E> {
        Ok(Content::U128(value))
    }

    fn visit_f32<E: de::Error>(self, value: f32) -> Result<Self::Value, E> {
        Ok(Content::F32(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
        Ok(Content::F64(value))
    }

    fn visit_char<E: de::Error>(self, value: char) -> Result<Self::Value, E> {
        Ok(Content::Char(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Content::String(value.to_owned()))
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(Content::Str(value))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(Content::String(value))
    }

    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
        Ok(Content::ByteBuf(value.to_vec()))
    }

    fn visit_borrowed_bytes<E: de::Error>(self, value: &'de [u8]) -> Result<Self::Value, E> {
        Ok(Content::Bytes(value))
    }

    fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
        Ok(Content::ByteBuf(value))
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Content::None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        Content::deserialize(deserializer).map(|it| Content::Some(Box::new(it)))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Content::Unit)
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        Content::deserialize(deserializer).map(|it| Content::Newtype(Box::new(it)))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(PREALLOCATE));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Content::Seq(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0).min(PREALLOCATE));
        while let Some(entry) = map.next_entry()? {
            entries.push(entry);
        }
        Ok(Content::Map(entries))
    }

    fn visit_enum<A: EnumAccess<'de>>(self, _data: A) -> Result<Self::Value, A::Error> {
        Err(de::Error::custom("untagged and internally tagged enums do not support enum input"))
    }
}

/// [`Content`] read as the input it was read from: borrowed strings stay borrowed, and
/// `is_human_readable` says what the input's did.
pub struct ContentRefDeserializer<'a, 'de, E> {
    content: &'a Content<'de>,
    human: bool,
    error: PhantomData<E>,
}

impl<'a, 'de, E> ContentRefDeserializer<'a, 'de, E> {
    pub fn new(content: &'a Content<'de>, human_readable: bool) -> Self {
        ContentRefDeserializer { content, human: human_readable, error: PhantomData }
    }
}

// Not derived: a derive would ask `E: Copy`, and an error type seldom is.
impl<E> Clone for ContentRefDeserializer<'_, '_, E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for ContentRefDeserializer<'_, '_, E> {}

impl<'a, 'de, E: de::Error> ContentRefDeserializer<'a, 'de, E> {
    fn of(&self, content: &'a Content<'de>) -> Self {
        ContentRefDeserializer::new(content, self.human)
    }

    fn invalid_type(&self, expected: &dyn Expected) -> E {
        E::invalid_type(self.content.unexpected(), expected)
    }
}

impl<'a, 'de, E: de::Error> Deserializer<'de> for ContentRefDeserializer<'a, 'de, E> {
    type Error = E;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
        match *self.content {
            Content::Bool(it) => visitor.visit_bool(it),
            Content::U8(it) => visitor.visit_u8(it),
            Content::U16(it) => visitor.visit_u16(it),
            Content::U32(it) => visitor.visit_u32(it),
            Content::U64(it) => visitor.visit_u64(it),
            Content::U128(it) => visitor.visit_u128(it),
            Content::I8(it) => visitor.visit_i8(it),
            Content::I16(it) => visitor.visit_i16(it),
            Content::I32(it) => visitor.visit_i32(it),
            Content::I64(it) => visitor.visit_i64(it),
            Content::I128(it) => visitor.visit_i128(it),
            Content::F32(it) => visitor.visit_f32(it),
            Content::F64(it) => visitor.visit_f64(it),
            Content::Char(it) => visitor.visit_char(it),
            Content::String(ref it) => visitor.visit_str(it),
            Content::Str(it) => visitor.visit_borrowed_str(it),
            Content::ByteBuf(ref it) => visitor.visit_bytes(it),
            Content::Bytes(it) => visitor.visit_borrowed_bytes(it),
            Content::None => visitor.visit_none(),
            Content::Some(ref it) => visitor.visit_some(self.of(it)),
            Content::Unit => visitor.visit_unit(),
            Content::Newtype(ref it) => visitor.visit_newtype_struct(self.of(it)),
            Content::Seq(ref items) => visit_seq(items, self.human, visitor),
            Content::Map(ref entries) => visit_map(entries, self.human, visitor),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
        match *self.content {
            Content::None => visitor.visit_none(),
            Content::Some(ref it) => visitor.visit_some(self.of(it)),
            Content::Unit => visitor.visit_unit(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
        match *self.content {
            Content::Unit => visitor.visit_unit(),
            _ => Err(self.invalid_type(&visitor)),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, E> {
        match *self.content {
            Content::Map(ref entries) if entries.is_empty() => visitor.visit_unit(),
            Content::Seq(ref items) if items.is_empty() => visitor.visit_unit(),
            _ => self.deserialize_unit(visitor),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, E> {
        match *self.content {
            Content::Newtype(ref it) => visitor.visit_newtype_struct(self.of(it)),
            _ => visitor.visit_newtype_struct(self),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, E> {
        let (variant, value) = match *self.content {
            Content::Map(ref entries) => match entries.as_slice() {
                [(variant, value)] => (variant, Some(value)),
                _ => return Err(E::invalid_value(Unexpected::Map, &"map with a single key")),
            },
            Content::String(_) | Content::Str(_) => (self.content, None),
            ref other => return Err(E::invalid_type(other.unexpected(), &"string or map")),
        };
        visitor.visit_enum(EnumRef { variant, value, human: self.human, error: PhantomData })
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
        match *self.content {
            Content::String(ref it) => visitor.visit_str(it),
            Content::Str(it) => visitor.visit_borrowed_str(it),
            Content::ByteBuf(ref it) => visitor.visit_bytes(it),
            Content::Bytes(it) => visitor.visit_borrowed_bytes(it),
            Content::U8(it) => visitor.visit_u8(it),
            Content::U64(it) => visitor.visit_u64(it),
            _ => Err(self.invalid_type(&visitor)),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
        visitor.visit_unit()
    }

    fn is_human_readable(&self) -> bool {
        self.human
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf seq
        tuple tuple_struct map struct
    }
}

/// How many elements a sequence or map of `count` was expected to have, for `invalid_length`.
struct Count {
    count: usize,
    of: &'static str,
}

impl Expected for Count {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} elements in {}", self.count, self.of)
    }
}

fn visit_seq<'a, 'de, V: Visitor<'de>, E: de::Error>(
    items: &'a [Content<'de>],
    human: bool,
    visitor: V,
) -> Result<V::Value, E> {
    let mut seq = SeqRef { items: items.iter(), human, read: 0, error: PhantomData };
    let value = visitor.visit_seq(&mut seq)?;
    match seq.items.len() {
        0 => Ok(value),
        left => Err(E::invalid_length(seq.read + left, &Count { count: seq.read, of: "sequence" })),
    }
}

fn visit_map<'a, 'de, V: Visitor<'de>, E: de::Error>(
    entries: &'a [(Content<'de>, Content<'de>)],
    human: bool,
    visitor: V,
) -> Result<V::Value, E> {
    let mut map =
        MapRef { entries: entries.iter(), value: None, human, read: 0, error: PhantomData };
    let value = visitor.visit_map(&mut map)?;
    match map.entries.len() {
        0 => Ok(value),
        left => Err(E::invalid_length(map.read + left, &Count { count: map.read, of: "map" })),
    }
}

struct SeqRef<'a, 'de, E> {
    items: slice::Iter<'a, Content<'de>>,
    human: bool,
    read: usize,
    error: PhantomData<E>,
}

impl<'a, 'de, E: de::Error> SeqAccess<'de> for SeqRef<'a, 'de, E> {
    type Error = E;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, E> {
        match self.items.next() {
            Some(item) => {
                self.read += 1;
                seed.deserialize(ContentRefDeserializer::new(item, self.human)).map(Some)
            }
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len())
    }
}

struct MapRef<'a, 'de, E> {
    entries: slice::Iter<'a, (Content<'de>, Content<'de>)>,
    /// The value of the key last read.
    value: Option<&'a Content<'de>>,
    human: bool,
    read: usize,
    error: PhantomData<E>,
}

impl<'a, 'de, E: de::Error> MapAccess<'de> for MapRef<'a, 'de, E> {
    type Error = E;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, E> {
        match self.entries.next() {
            Some((key, value)) => {
                self.read += 1;
                self.value = Some(value);
                seed.deserialize(ContentRefDeserializer::new(key, self.human)).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, E> {
        match self.value.take() {
            Some(value) => seed.deserialize(ContentRefDeserializer::new(value, self.human)),
            None => Err(E::custom("a map value was read before its key")),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

/// An enum as serde writes one externally tagged: its variant's name alone, or a map from it
/// to the variant's value.
struct EnumRef<'a, 'de, E> {
    variant: &'a Content<'de>,
    value: Option<&'a Content<'de>>,
    human: bool,
    error: PhantomData<E>,
}

impl<'a, 'de, E: de::Error> EnumAccess<'de> for EnumRef<'a, 'de, E> {
    type Error = E;
    type Variant = VariantRef<'a, 'de, E>;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), E> {
        let variant = seed.deserialize(ContentRefDeserializer::new(self.variant, self.human))?;
        Ok((variant, VariantRef { value: self.value, human: self.human, error: PhantomData }))
    }
}

struct VariantRef<'a, 'de, E> {
    value: Option<&'a Content<'de>>,
    human: bool,
    error: PhantomData<E>,
}

impl<'a, 'de, E: de::Error> VariantAccess<'de> for VariantRef<'a, 'de, E> {
    type Error = E;

    fn unit_variant(self) -> Result<(), E> {
        match self.value {
            None | Some(Content::Unit) => Ok(()),
            Some(other) => Err(E::invalid_type(other.unexpected(), &"unit variant")),
        }
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, E> {
        match self.value {
            Some(value) => seed.deserialize(ContentRefDeserializer::new(value, self.human)),
            None => Err(E::invalid_type(Unexpected::UnitVariant, &"newtype variant")),
        }
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, E> {
        match self.value {
            Some(Content::Seq(items)) => visit_seq(items, self.human, visitor),
            Some(other) => Err(E::invalid_type(other.unexpected(), &"tuple variant")),
            None => Err(E::invalid_type(Unexpected::UnitVariant, &"tuple variant")),
        }
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, E> {
        match self.value {
            Some(Content::Map(entries)) => visit_map(entries, self.human, visitor),
            Some(Content::Seq(items)) => visit_seq(items, self.human, visitor),
            Some(other) => Err(E::invalid_type(other.unexpected(), &"struct variant")),
            None => Err(E::invalid_type(Unexpected::UnitVariant, &"struct variant")),
        }
    }
}
