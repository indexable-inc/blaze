//! What the tests share: bytes from hex, and values made short.

use blaze_id::{ContentId, ItemKey};
use blaze_own::{AString, AVec, Alloc, Vec};

use crate::{Checkout, Path, WorldId};

/// The bytes `text` spells in hex, blanks between them allowed (tools/world/vectors.json's form).
pub(crate) fn hex<'a>(text: &str, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let digits = Vec::from_iter_in(text.bytes().filter(|b| !b.is_ascii_whitespace()), alloc);
    assert!(digits.len() % 2 == 0, "an odd number of hex digits in {text:?}");
    Vec::from_iter_in(
        digits.chunks(2).map(|pair| match core::str::from_utf8(pair).ok().and_then(|it| u8::from_str_radix(it, 16).ok()) {
            Some(byte) => byte,
            None => panic!("not hex: {pair:?} in {text:?}"),
        }),
        alloc,
    )
}

pub(crate) fn path<'a>(text: &str, alloc: Alloc<'a>) -> Path<'a> {
    match Path::new_in(text, alloc) {
        Ok(path) => path,
        Err(error) => panic!("{text:?}: {error}"),
    }
}

pub(crate) fn checkout<'a>(text: &str, alloc: Alloc<'a>) -> Checkout<'a> {
    match Checkout::new_in(text, alloc) {
        Ok(checkout) => checkout,
        Err(error) => panic!("{text:?}: {error}"),
    }
}

pub(crate) fn text<'a>(text: &str, alloc: Alloc<'a>) -> AString<'a> {
    AString::from_str_in(text, alloc)
}

pub(crate) fn world(byte: u8) -> WorldId {
    WorldId([byte; 32])
}

pub(crate) fn item(byte: u8) -> ItemKey {
    ItemKey(ContentId([byte; 32]))
}

/// A list of `items`, in `alloc`.
pub(crate) fn list<'a, T>(items: impl IntoIterator<Item = T>, alloc: Alloc<'a>) -> AVec<'a, T> {
    Vec::from_iter_in(items, alloc)
}
