//! Typed shortcuts: JSON bytes straight to a [`FromValue`] type, and a [`ToValue`] type straight
//! to JSON text, through a [`Value`] in between (`serde_json::from_slice` and `to_string`'s
//! places). Reading is strict twice over: the JSON reader's rules, then the type's (unknown
//! fields refused, no kind read as another).

use core::{alloc::Allocator, fmt};

use blaze_data::{Cursor, FromValue, ShapeError, ToValue};
use blaze_own::String;

use crate::{Error, Limits, WriteError, WriteOptions, read_in, to_string_in};

/// Why [`from_slice_in`] made no value: the bytes are no JSON, or the JSON does not fit the type.
pub enum FromJsonError<A: Allocator> {
    /// The text is no JSON or passes a limit; the byte offset and what.
    Syntax(Error),
    /// The JSON does not fit the type; the path in it and why.
    Shape(ShapeError<A>),
}

impl<A: Allocator> fmt::Display for FromJsonError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FromJsonError::Syntax(error) => fmt::Display::fmt(error, f),
            FromJsonError::Shape(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl<A: Allocator> fmt::Debug for FromJsonError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FromJsonError::Syntax(error) => write!(f, "Syntax({error:?})"),
            FromJsonError::Shape(error) => write!(f, "Shape({error:?})"),
        }
    }
}

impl<A: Allocator> core::error::Error for FromJsonError<A> {}

impl<A: Allocator> From<Error> for FromJsonError<A> {
    fn from(error: Error) -> FromJsonError<A> {
        FromJsonError::Syntax(error)
    }
}

impl<A: Allocator> From<ShapeError<A>> for FromJsonError<A> {
    fn from(error: ShapeError<A>) -> FromJsonError<A> {
        FromJsonError::Shape(error)
    }
}

/// `bytes` as JSON under `limits`, read as a `T`; the value and `T` in `alloc`.
pub fn from_slice_in<T: FromValue<A>, A: Allocator + Clone>(
    bytes: &[u8],
    limits: Limits,
    alloc: A,
) -> Result<T, FromJsonError<A>> {
    let value = read_in(bytes, limits, alloc.clone())?;
    Ok(Cursor::root(&value, alloc).get::<T>()?)
}

/// `bytes` as JSON under [`Limits::SHIPPED`], read as a `T` in `A`'s default allocator
/// (`from_slice::<Settings, Global>(bytes)`).
pub fn from_slice<T: FromValue<A>, A: Allocator + Clone + Default>(bytes: &[u8]) -> Result<T, FromJsonError<A>> {
    from_slice_in(bytes, Limits::SHIPPED, A::default())
}

/// `value` as JSON text written as `options` says, in `alloc`.
pub fn to_string_of_in<T: ToValue<B> + ?Sized, B: Allocator + Clone>(
    value: &T,
    options: WriteOptions,
    alloc: B,
) -> Result<String<B>, WriteError<B>> {
    to_string_in(&value.to_value(alloc.clone()), options, alloc)
}

/// `value` as compact JSON text, keys sorted, in `B`'s default allocator.
pub fn to_string<T: ToValue<B> + ?Sized, B: Allocator + Clone + Default>(value: &T) -> Result<String<B>, WriteError<B>> {
    to_string_of_in(value, WriteOptions::COMPACT, B::default())
}

/// `value` as JSON text with a two-space indent, keys sorted, in `B`'s default allocator.
pub fn to_string_pretty<T: ToValue<B> + ?Sized, B: Allocator + Clone + Default>(
    value: &T,
) -> Result<String<B>, WriteError<B>> {
    to_string_of_in(value, WriteOptions::PRETTY, B::default())
}
