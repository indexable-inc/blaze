//! Items read where they lie: [`Raw`], one item's bytes, and [`List`], an array whose items are
//! decoded from their bytes as they are iterated, so a type of lists reads with no allocation.
//! Only this crate's decoders hand an item's bytes out; another deserializer (JSON) refuses
//! both.

use std::{fmt, marker::PhantomData};

use serde::de::{self, Deserialize, Deserializer, Visitor};

use super::{
    RAW,
    de::{array_head, from_front},
};

/// One item's bytes as they lie in what was read, checked as deterministic CBOR (depth
/// included), its type not yet asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Raw<'a>(pub &'a [u8]);

impl<'de: 'a, 'a> Deserialize<'de> for Raw<'a> {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Raw<'a>, D::Error> {
        from.deserialize_newtype_struct(RAW, Lent)
    }
}

struct Lent;

impl<'de> Visitor<'de> for Lent {
    type Value = Raw<'de>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an item of blaze's CBOR, read where it lies")
    }

    fn visit_borrowed_bytes<E: de::Error>(self, bytes: &'de [u8]) -> Result<Raw<'de>, E> {
        Ok(Raw(bytes))
    }
}

/// An array of `T` read where it lies. Each item is decoded as `T` once as the list is read, so
/// bytes that are no list of `T` are refused there, where a `Vec<T>` is, with what the `T`
/// found; a `T` that owns nothing makes that read allocate nothing. [`List::iter`] decodes them
/// again from their bytes.
pub struct List<'a, T> {
    /// The items' bytes, after the array's head.
    items: &'a [u8],
    len: usize,
    of: PhantomData<fn() -> T>,
}

impl<T> Clone for List<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for List<'_, T> {}

/// Two lists are equal when their items' encodings are: the encoding is deterministic.
impl<T> PartialEq for List<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.items == other.items
    }
}

impl<T> Eq for List<'_, T> {}

impl<'a, T> List<'a, T> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<'a, T: Deserialize<'a>> List<'a, T> {
    pub fn iter(&self) -> Iter<'a, T> {
        Iter { rest: self.items, left: self.len, of: PhantomData }
    }

    pub fn first(&self) -> Option<T> {
        self.iter().next()
    }
}

impl<'a, T: Deserialize<'a>> IntoIterator for List<'a, T> {
    type Item = T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

impl<'a, T: Deserialize<'a>> IntoIterator for &List<'a, T> {
    type Item = T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

impl<'a, T: Deserialize<'a> + fmt::Debug> fmt::Debug for List<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'de: 'a, 'a, T: Deserialize<'a>> Deserialize<'de> for List<'a, T> {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<List<'a, T>, D::Error> {
        let Raw(bytes) = Raw::deserialize(from)?;
        let Some((len, head)) = array_head(bytes) else {
            return Err(de::Error::invalid_type(de::Unexpected::Other("an item that is no array"), &"an array"));
        };
        // Fits: `Raw` passed every item, each at least a byte of `bytes`.
        let len = len as usize;
        let items = &bytes[head..];
        let mut at = 0;
        for _ in 0..len {
            let (_, used) = from_front::<T>(&items[at..]).map_err(de::Error::custom)?;
            at += used;
        }
        Ok(List { items, len, of: PhantomData })
    }
}

/// A [`List`]'s items, each decoded from its bytes as it comes.
pub struct Iter<'a, T> {
    rest: &'a [u8],
    left: usize,
    of: PhantomData<fn() -> T>,
}

impl<'a, T: Deserialize<'a>> Iterator for Iter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        // The list's read decoded this item as `T` from these same bytes, and a decode depends
        // on nothing else.
        let (item, used) = from_front::<T>(self.rest).expect("an item its list's read decoded as `T`");
        self.rest = &self.rest[used..];
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl<'a, T: Deserialize<'a>> ExactSizeIterator for Iter<'a, T> {}
