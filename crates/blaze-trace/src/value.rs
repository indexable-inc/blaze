//! A field's value: a number as itself, text borrowed, anything else borrowed as `Display` or
//! `Debug` and formatted only by a sink that wants its text.
//!
//! [`ToValue`] is implemented for blaze-own's `String`, `Box`, `Arc` and `Rc` over any allocator,
//! and still for `alloc`'s `String`, `Cow<str>`, `Box`, `Arc` and `Rc`: those impls name the types
//! only (nothing here allocates), and exist because crates give such values as plain fields
//! today. They go when no caller gives one (each caller's crate on blaze-own's types).

// A core: a value borrowed from the site, read by a visitor; no allocation, no clock, no IO.
#![blaze::sans_io]

use alloc::{borrow::Cow, boxed::Box, rc::Rc, string::String, sync::Arc};
use core::{alloc::Allocator, fmt, num};

use crate::field::{Field, Visit};

/// One field's value, borrowed from the site for the call that gives it to the sink.
#[derive(Clone, Copy)]
pub enum Value<'a> {
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
    I128(i128),
    U128(u128),
    Str(&'a str),
    /// `field = ?value`.
    Debug(&'a dyn fmt::Debug),
    /// `field = %value`, and an event's message.
    Display(&'a dyn fmt::Display),
}

impl Value<'_> {
    /// Gives the value to `visitor`'s method of its type.
    pub fn visit(&self, field: &Field, visitor: &mut dyn Visit) {
        match *self {
            Value::I64(it) => visitor.record_i64(field, it),
            Value::U64(it) => visitor.record_u64(field, it),
            Value::F64(it) => visitor.record_f64(field, it),
            Value::Bool(it) => visitor.record_bool(field, it),
            Value::I128(it) => visitor.record_i128(field, it),
            Value::U128(it) => visitor.record_u128(field, it),
            Value::Str(it) => visitor.record_str(field, it),
            Value::Debug(it) => visitor.record_debug(field, it),
            Value::Display(it) => visitor.record_debug(field, &Shown(it)),
        }
    }
}

impl fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Value::I64(it) => it.fmt(f),
            Value::U64(it) => it.fmt(f),
            Value::F64(it) => it.fmt(f),
            Value::Bool(it) => it.fmt(f),
            Value::I128(it) => it.fmt(f),
            Value::U128(it) => it.fmt(f),
            Value::Str(it) => it.fmt(f),
            Value::Debug(it) => it.fmt(f),
            Value::Display(it) => it.fmt(f),
        }
    }
}

/// A `Display` value read through `Debug`: prints its `Display`.
struct Shown<'a>(&'a dyn fmt::Display);

impl fmt::Debug for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A field declared and given no value yet: `krate = field::Empty`, then `span.record(..)`.
#[derive(Clone, Copy, Debug)]
pub struct Empty;

/// What a plain field (`field = value`, or `field` alone) may hold: numbers, text, and those
/// behind a reference, a box or an `Option` (`None` is no value). Anything else is written
/// `%value` or `?value`.
pub trait ToValue {
    fn to_value(&self) -> Option<Value<'_>>;
}

macro_rules! to_value {
    ($variant:ident as $to:ty: $($t:ty),*) => {
        $(impl ToValue for $t {
            #[inline]
            fn to_value(&self) -> Option<Value<'_>> {
                Some(Value::$variant(*self as $to))
            }
        })*
    };
}

to_value!(I64 as i64: i8, i16, i32, i64, isize);
to_value!(U64 as u64: u8, u16, u32, u64, usize);
to_value!(F64 as f64: f32, f64);
to_value!(I128 as i128: i128);
to_value!(U128 as u128: u128);

macro_rules! non_zero {
    ($v:ident as $w:ty: $($t:ty),*) => {
        $(impl ToValue for num::NonZero<$t> {
            #[inline]
            fn to_value(&self) -> Option<Value<'_>> {
                Some(Value::$v(self.get() as $w))
            }
        })*
    };
}

non_zero!(I64 as i64: i8, i16, i32, i64, isize);
non_zero!(U64 as u64: u8, u16, u32, u64, usize);
non_zero!(I128 as i128: i128);
non_zero!(U128 as u128: u128);

impl ToValue for bool {
    fn to_value(&self) -> Option<Value<'_>> {
        Some(Value::Bool(*self))
    }
}

impl ToValue for str {
    fn to_value(&self) -> Option<Value<'_>> {
        Some(Value::Str(self))
    }
}

impl ToValue for String {
    fn to_value(&self) -> Option<Value<'_>> {
        Some(Value::Str(self))
    }
}

impl ToValue for Cow<'_, str> {
    fn to_value(&self) -> Option<Value<'_>> {
        Some(Value::Str(self))
    }
}

impl ToValue for fmt::Arguments<'_> {
    fn to_value(&self) -> Option<Value<'_>> {
        Some(Value::Display(self))
    }
}

impl ToValue for dyn core::error::Error + 'static {
    fn to_value(&self) -> Option<Value<'_>> {
        Some(Value::Display(self))
    }
}

impl ToValue for Empty {
    fn to_value(&self) -> Option<Value<'_>> {
        None
    }
}

impl<T: ToValue> ToValue for Option<T> {
    fn to_value(&self) -> Option<Value<'_>> {
        self.as_ref()?.to_value()
    }
}

impl<T: ToValue + ?Sized> ToValue for &T {
    fn to_value(&self) -> Option<Value<'_>> {
        (**self).to_value()
    }
}

impl<T: ToValue + ?Sized> ToValue for &mut T {
    fn to_value(&self) -> Option<Value<'_>> {
        (**self).to_value()
    }
}

impl<T: ToValue + ?Sized> ToValue for Box<T> {
    fn to_value(&self) -> Option<Value<'_>> {
        (**self).to_value()
    }
}

impl<T: ToValue + ?Sized> ToValue for Arc<T> {
    fn to_value(&self) -> Option<Value<'_>> {
        (**self).to_value()
    }
}

impl<T: ToValue + ?Sized> ToValue for Rc<T> {
    fn to_value(&self) -> Option<Value<'_>> {
        (**self).to_value()
    }
}

impl<A: Allocator> ToValue for blaze_own::String<A> {
    fn to_value(&self) -> Option<Value<'_>> {
        Some(Value::Str(self))
    }
}

impl<T: ToValue + ?Sized, A: Allocator> ToValue for blaze_own::Box<T, A> {
    fn to_value(&self) -> Option<Value<'_>> {
        (**self).to_value()
    }
}

impl<T: ToValue + ?Sized, A: Allocator> ToValue for blaze_own::Arc<T, A> {
    fn to_value(&self) -> Option<Value<'_>> {
        (**self).to_value()
    }
}

impl<T: ToValue + ?Sized, A: Allocator> ToValue for blaze_own::Rc<T, A> {
    fn to_value(&self) -> Option<Value<'_>> {
        (**self).to_value()
    }
}
