//! [`ToValue`] and [`FromValue`], written by hand for the standard shapes.
//!
//! A struct implements them as reflection will (docs/v2/plan/design/std.md, `bstd::data`), and
//! as [`fields!`](crate::fields) writes them for a plain struct. Reading is strict by default:
//! [`Cursor::fields`] refuses every key the closure did not take, after it returns, so a reader
//! cannot leave the check out; [`Cursor::fields_allowing_extra`] is the opt-out, visible at the
//! call and handing the extra keys back. No kind is read as another (`1` is no `true`, `"1"` no
//! integer, `1.0` no integer, `1` no float but through [`Cursor::f64_or_int`]), an integer
//! outside its type is refused, and an enum's name outside its variants is refused by
//! [`Cursor::one_of`] listing them.
//!
//! ```ignore
//! impl<A: Allocator + Clone> FromValue<A> for Run<A> {
//!     fn from_value(c: &Cursor<'_, '_, A>) -> Result<Run<A>, ShapeError<A>> {
//!         c.fields(|f| Ok(Run { suite: f.take("suite")?, ids: f.take_opt("ids")? }))
//!     }
//! }
//! impl<A: Allocator + Clone> ToValue<A> for Run<A> {
//!     fn to_value(&self, alloc: A) -> Value<A> {
//!         let mut map = Map::new_in(alloc.clone());
//!         map.insert(String::from_str_in("suite", alloc.clone()), self.suite.to_value(alloc.clone()));
//!         map.insert(String::from_str_in("ids", alloc.clone()), self.ids.to_value(alloc));
//!         Value::Map(map)
//!     }
//! }
//! ```

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_own::{String, Vec};

use crate::{Cursor, Int, Kind, Map, Shape, ShapeError, Value};

/// A type written as a [`Value`] in the allocator given.
pub trait ToValue<A: Allocator + Clone> {
    fn to_value(&self, alloc: A) -> Value<A>;
}

/// A type read from the value under a [`Cursor`], with errors naming its path.
pub trait FromValue<A: Allocator + Clone>: Sized {
    fn from_value(cursor: &Cursor<'_, '_, A>) -> Result<Self, ShapeError<A>>;
}

impl<A: Allocator + Clone, T: ToValue<A> + ?Sized> ToValue<A> for &T {
    fn to_value(&self, alloc: A) -> Value<A> {
        (**self).to_value(alloc)
    }
}

impl<A: Allocator + Clone> ToValue<A> for () {
    fn to_value(&self, _alloc: A) -> Value<A> {
        Value::Null
    }
}

impl<A: Allocator + Clone> ToValue<A> for bool {
    fn to_value(&self, _alloc: A) -> Value<A> {
        Value::Bool(*self)
    }
}

impl<A: Allocator + Clone> ToValue<A> for Int {
    fn to_value(&self, _alloc: A) -> Value<A> {
        Value::Int(*self)
    }
}

macro_rules! unsigned {
    ($($t:ty),*) => {$(
        impl<A: Allocator + Clone> ToValue<A> for $t {
            fn to_value(&self, _alloc: A) -> Value<A> {
                Value::Int(Int::from_u64(u64::from(*self)))
            }
        }

        impl<A: Allocator + Clone> FromValue<A> for $t {
            fn from_value(c: &Cursor<'_, '_, A>) -> Result<$t, ShapeError<A>> {
                let n = c.value().as_int().ok_or_else(|| c.expected(Kind::Int))?;
                n.as_u64()
                    .and_then(|n| <$t>::try_from(n).ok())
                    .ok_or_else(|| c.error(Shape::OutOfRange { ty: stringify!($t) }))
            }
        }
    )*};
}

macro_rules! signed {
    ($($t:ty),*) => {$(
        impl<A: Allocator + Clone> ToValue<A> for $t {
            fn to_value(&self, _alloc: A) -> Value<A> {
                Value::Int(Int::from_i64(i64::from(*self)))
            }
        }

        impl<A: Allocator + Clone> FromValue<A> for $t {
            fn from_value(c: &Cursor<'_, '_, A>) -> Result<$t, ShapeError<A>> {
                let n = c.value().as_int().ok_or_else(|| c.expected(Kind::Int))?;
                n.as_i64()
                    .and_then(|n| <$t>::try_from(n).ok())
                    .ok_or_else(|| c.error(Shape::OutOfRange { ty: stringify!($t) }))
            }
        }
    )*};
}

unsigned!(u8, u16, u32, u64);
signed!(i8, i16, i32, i64);

impl<A: Allocator + Clone> ToValue<A> for usize {
    fn to_value(&self, _alloc: A) -> Value<A> {
        match u64::try_from(*self) {
            Ok(n) => Value::Int(Int::from_u64(n)),
            Err(_) => unreachable!("blaze's targets have 64-bit or smaller `usize`"),
        }
    }
}

impl<A: Allocator + Clone> FromValue<A> for usize {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<usize, ShapeError<A>> {
        let n = c.value().as_int().ok_or_else(|| c.expected(Kind::Int))?;
        n.as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| c.error(Shape::OutOfRange { ty: "usize" }))
    }
}

impl<A: Allocator + Clone> ToValue<A> for isize {
    fn to_value(&self, _alloc: A) -> Value<A> {
        match i64::try_from(*self) {
            Ok(n) => Value::Int(Int::from_i64(n)),
            Err(_) => unreachable!("blaze's targets have 64-bit or smaller `isize`"),
        }
    }
}

impl<A: Allocator + Clone> ToValue<A> for f64 {
    fn to_value(&self, _alloc: A) -> Value<A> {
        Value::Float(*self)
    }
}

impl<A: Allocator + Clone> ToValue<A> for f32 {
    fn to_value(&self, _alloc: A) -> Value<A> {
        Value::Float(f64::from(*self))
    }
}

impl<A: Allocator + Clone> ToValue<A> for char {
    fn to_value(&self, alloc: A) -> Value<A> {
        let mut text = String::new_in(alloc);
        text.push(*self);
        Value::Text(text)
    }
}

impl<A: Allocator + Clone> ToValue<A> for str {
    fn to_value(&self, alloc: A) -> Value<A> {
        Value::Text(String::from_str_in(self, alloc))
    }
}

impl<A: Allocator + Clone, B: Allocator> ToValue<A> for String<B> {
    fn to_value(&self, alloc: A) -> Value<A> {
        Value::Text(String::from_str_in(self.as_str(), alloc))
    }
}

impl<A: Allocator + Clone, T: ToValue<A>> ToValue<A> for Option<T> {
    fn to_value(&self, alloc: A) -> Value<A> {
        match self {
            Some(value) => value.to_value(alloc),
            None => Value::Null,
        }
    }
}

impl<A: Allocator + Clone, T: ToValue<A>> ToValue<A> for [T] {
    fn to_value(&self, alloc: A) -> Value<A> {
        let mut list = Vec::with_capacity_in(self.len(), alloc.clone());
        for item in self {
            list.push(item.to_value(alloc.clone()));
        }
        Value::List(list)
    }
}

impl<A: Allocator + Clone, T: ToValue<A>, const N: usize> ToValue<A> for [T; N] {
    fn to_value(&self, alloc: A) -> Value<A> {
        self.as_slice().to_value(alloc)
    }
}

impl<A: Allocator + Clone, B: Allocator, T: ToValue<A>> ToValue<A> for Vec<T, B> {
    fn to_value(&self, alloc: A) -> Value<A> {
        self.as_slice().to_value(alloc)
    }
}

impl<A: Allocator + Clone, B: Allocator> ToValue<A> for Value<B> {
    fn to_value(&self, alloc: A) -> Value<A> {
        self.copy_in(alloc)
    }
}

impl<A: Allocator + Clone, B: Allocator> ToValue<A> for Map<B> {
    fn to_value(&self, alloc: A) -> Value<A> {
        Value::Map(self.copy_in(alloc))
    }
}

impl<A: Allocator + Clone> FromValue<A> for bool {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<bool, ShapeError<A>> {
        c.bool()
    }
}

impl<A: Allocator + Clone> FromValue<A> for Int {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<Int, ShapeError<A>> {
        c.value().as_int().ok_or_else(|| c.expected(Kind::Int))
    }
}

/// A float only: an integer is another kind, refused as every kind is (strict by default);
/// [`Cursor::f64_or_int`] reads one where a reader asks for it by name.
impl<A: Allocator + Clone> FromValue<A> for f64 {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<f64, ShapeError<A>> {
        match c.value() {
            Value::Float(f) => Ok(*f),
            _ => Err(c.expected(Kind::Float)),
        }
    }
}

impl<A: Allocator + Clone> FromValue<A> for String<A> {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<String<A>, ShapeError<A>> {
        Ok(String::from_str_in(c.text()?, c.alloc().clone()))
    }
}

/// `None` for null.
impl<A: Allocator + Clone, T: FromValue<A>> FromValue<A> for Option<T> {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<Option<T>, ShapeError<A>> {
        match c.is_null() {
            true => Ok(None),
            false => c.get().map(Some),
        }
    }
}

impl<A: Allocator + Clone, T: FromValue<A>> FromValue<A> for Vec<T, A> {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<Vec<T, A>, ShapeError<A>> {
        let mut out = Vec::with_capacity_in(c.list()?.len(), c.alloc().clone());
        c.each(|item| {
            out.push(item.get()?);
            Ok::<(), ShapeError<A>>(())
        })?;
        Ok(out)
    }
}

impl<A: Allocator + Clone> FromValue<A> for Value<A> {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<Value<A>, ShapeError<A>> {
        Ok(c.value().copy_in(c.alloc().clone()))
    }
}
