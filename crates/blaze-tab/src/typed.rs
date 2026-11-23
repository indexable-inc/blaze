//! Typed [`FromValue`] and [`ToValue`] helpers for `blaze-tab`.
//!
//! Reads bytes directly into a [`FromValue`] type (enforcing both `blaze-tab` syntax/schema rules
//! and the type's strict field/kind rules) and writes any [`ToValue`] type directly to compact or
//! pretty `blaze-tab` text.

use core::alloc::Allocator;
use core::fmt;

use blaze_data::{Cursor, FromValue, ShapeError, ToValue};
use blaze_own::{Alloc, String};

use crate::{Limits, ReadError, WriteError, WriteOptions, read, write, write_pretty, write_with};

/// Why [`read_typed`] produced no value: syntax/limit error, or the value does not fit `T`.
pub enum FromTabError<A: Allocator> {
    /// Text is not valid `blaze-tab` or exceeded a limit.
    Syntax(ReadError<A>),
    /// Parsed `Value` does not match the target type's schema.
    Shape(ShapeError<A>),
}

impl<A: Allocator> fmt::Display for FromTabError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FromTabError::Syntax(error) => fmt::Display::fmt(error, f),
            FromTabError::Shape(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl<A: Allocator> fmt::Debug for FromTabError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FromTabError::Syntax(error) => write!(f, "Syntax({error:?})"),
            FromTabError::Shape(error) => write!(f, "Shape({error:?})"),
        }
    }
}

impl<A: Allocator> core::error::Error for FromTabError<A> {}

impl<A: Allocator> From<ReadError<A>> for FromTabError<A> {
    fn from(error: ReadError<A>) -> FromTabError<A> {
        FromTabError::Syntax(error)
    }
}

impl<A: Allocator> From<ShapeError<A>> for FromTabError<A> {
    fn from(error: ShapeError<A>) -> FromTabError<A> {
        FromTabError::Shape(error)
    }
}

/// Parses `bytes` as `blaze-tab` under `limits` and converts it strictly into `T` in `alloc`.
pub fn read_typed<'a, T: FromValue<Alloc<'a>>>(
    bytes: &[u8],
    limits: Limits,
    alloc: Alloc<'a>,
) -> Result<T, FromTabError<Alloc<'a>>> {
    let value = read(bytes, limits, alloc)?;
    Ok(Cursor::root(&value, alloc).get::<T>()?)
}

/// Converts `value` via [`ToValue`] and formats it as compact `blaze-tab` text in `alloc`.
pub fn write_typed<'a, T: ToValue<Alloc<'a>> + ?Sized>(
    value: &T,
    alloc: Alloc<'a>,
) -> Result<String<Alloc<'a>>, WriteError<Alloc<'a>>> {
    let v = value.to_value(alloc);
    write(&v, alloc)
}

/// Converts `value` via [`ToValue`] and formats it as pretty `blaze-tab` text in `alloc`.
pub fn write_typed_pretty<'a, T: ToValue<Alloc<'a>> + ?Sized>(
    value: &T,
    alloc: Alloc<'a>,
) -> Result<String<Alloc<'a>>, WriteError<Alloc<'a>>> {
    let v = value.to_value(alloc);
    write_pretty(&v, alloc)
}

/// Converts `value` via [`ToValue`] and formats it as `blaze-tab` text with explicit `options`.
pub fn write_typed_with<'a, T: ToValue<Alloc<'a>> + ?Sized>(
    value: &T,
    options: WriteOptions,
    alloc: Alloc<'a>,
) -> Result<String<Alloc<'a>>, WriteError<Alloc<'a>>> {
    let v = value.to_value(alloc);
    write_with(&v, options, alloc)
}
