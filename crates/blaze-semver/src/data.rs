//! [`Version`] and [`VersionReq`] as [`blaze_data::Value`]s: each is its text, as Cargo's
//! manifests, lockfiles and registry indexes write it. Reading is strict: only text, parsed by
//! the same strict grammar under [`Limits::CARGO`]; a number, a list or a map is refused by
//! kind, and text that is no version names the offset and part in the error's note.

use core::{alloc::Allocator, fmt};

use blaze_data::{Cursor, FromValue, ShapeError, ToValue, Value};
use blaze_own::String;

use crate::{Limits, Version, VersionReq};

/// `shown` as a text value in `alloc`.
fn text_of<A: Allocator + Clone>(shown: &dyn fmt::Display, alloc: A) -> Value<A> {
    let mut text = String::new_in(alloc);
    if fmt::write(&mut text, format_args!("{shown}")).is_err() {
        unreachable!("writing into a `String` does not fail");
    }
    Value::Text(text)
}

impl<A: Allocator + Clone, B: Allocator> ToValue<A> for Version<B> {
    fn to_value(&self, alloc: A) -> Value<A> {
        text_of(self, alloc)
    }
}

impl<A: Allocator + Clone, B: Allocator> ToValue<A> for VersionReq<B> {
    fn to_value(&self, alloc: A) -> Value<A> {
        text_of(self, alloc)
    }
}

impl<A: Allocator + Clone> FromValue<A> for Version<A> {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<Version<A>, ShapeError<A>> {
        Version::parse_in(c.text()?, Limits::CARGO, c.alloc().clone()).map_err(|why| c.invalid("version", &why))
    }
}

impl<A: Allocator + Clone> FromValue<A> for VersionReq<A> {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<VersionReq<A>, ShapeError<A>> {
        VersionReq::parse_in(c.text()?, Limits::CARGO, c.alloc().clone())
            .map_err(|why| c.invalid("version requirement", &why))
    }
}
