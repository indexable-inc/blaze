//! [`ToValue`] and [`FromValue`] for std's `String`, `Vec` and `PathBuf`, behind the `std`
//! feature, for callers (tools, scripts) that still hold std types. The same strict rules as
//! the crate's own impls: a string is text only, a vector is a list only.
//!
//! A path is text when it is UTF-8. On unix a path that is not is its bytes, and a bytes value
//! reads back as that path; nothing is replaced or dropped. Paths are unix-only here: Windows is
//! out of scope, and its paths have no byte form to fall back on.

#![blaze::sans_io]

use core::alloc::Allocator;
use std::{borrow::ToOwned, string::String as StdString, vec::Vec as StdVec};

use crate::{Cursor, FromValue, ShapeError, ToValue, Value};

impl<A: Allocator + Clone> ToValue<A> for StdString {
    fn to_value(&self, alloc: A) -> Value<A> {
        self.as_str().to_value(alloc)
    }
}

impl<A: Allocator + Clone> FromValue<A> for StdString {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<StdString, ShapeError<A>> {
        c.text().map(ToOwned::to_owned)
    }
}

impl<A: Allocator + Clone, T: ToValue<A>> ToValue<A> for StdVec<T> {
    fn to_value(&self, alloc: A) -> Value<A> {
        self.as_slice().to_value(alloc)
    }
}

impl<A: Allocator + Clone, T: FromValue<A>> FromValue<A> for StdVec<T> {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<StdVec<T>, ShapeError<A>> {
        let mut out = StdVec::with_capacity(c.list()?.len());
        c.each(|item| {
            out.push(item.get()?);
            Ok::<(), ShapeError<A>>(())
        })?;
        Ok(out)
    }
}

#[cfg(unix)]
mod path {
    use core::alloc::Allocator;
    use std::{
        ffi::OsStr,
        os::unix::ffi::OsStrExt,
        path::{Path, PathBuf},
    };

    use blaze_own::Vec;

    use crate::{Cursor, FromValue, Kind, ShapeError, ToValue, Value};

    impl<A: Allocator + Clone> ToValue<A> for Path {
        fn to_value(&self, alloc: A) -> Value<A> {
            match self.to_str() {
                Some(text) => text.to_value(alloc),
                None => {
                    let bytes = self.as_os_str().as_bytes();
                    let mut out = Vec::with_capacity_in(bytes.len(), alloc);
                    out.extend_from_slice(bytes);
                    Value::Bytes(out)
                }
            }
        }
    }

    impl<A: Allocator + Clone> ToValue<A> for PathBuf {
        fn to_value(&self, alloc: A) -> Value<A> {
            self.as_path().to_value(alloc)
        }
    }

    /// Text, or on unix the bytes of a path that is not UTF-8.
    impl<A: Allocator + Clone> FromValue<A> for PathBuf {
        fn from_value(c: &Cursor<'_, '_, A>) -> Result<PathBuf, ShapeError<A>> {
            match c.value() {
                Value::Text(text) => Ok(PathBuf::from(text.as_str())),
                Value::Bytes(bytes) => Ok(PathBuf::from(OsStr::from_bytes(bytes.as_slice()))),
                _ => Err(c.expected(Kind::Text)),
            }
        }
    }
}
