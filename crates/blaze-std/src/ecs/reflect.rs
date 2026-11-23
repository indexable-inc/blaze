//! Live `bstd::data`-backed ECS component reflection (`#![forbid(unsafe_code)]`, `#![no_std]`).
//!
//! - [`ReflectError`]: typed reflection, decoding, and lookup error enum.
//! - [`ReflectComponent`]: reflection trait bridging Rust component types with
//!   [`crate::data::Value`], schema inspection, full decoding, and partial-map in-place patching.
//! - [`reflect_struct!`]: zero-boilerplate declarative macro implementing [`crate::data::ToValue`],
//!   [`crate::data::FromValue`], and [`ReflectComponent`] for any plain Rust struct.

use core::alloc::Allocator;
use core::fmt::{self, Write as _};

use blaze_own::{AllocFailure, FixedString, String as OwnString};

use crate::data::{Cursor, FromValue, Kind, Map, Shape, ShapeError, ToValue, Value};

use super::error::EcsError;
use super::id::{ComponentId, Entity};

/// Typed error returned by live ECS reflection, inspection, and patching operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReflectError {
    /// Caller-provided allocator refused an allocation.
    Alloc(AllocFailure),
    /// Underlying ECS world error (table/row/archetype/lifecycle).
    Ecs(EcsError),
    /// Reflected value did not match the target component's expected shape or numeric range.
    Shape(Shape),
    /// Component was registered without reflection metadata.
    NotReflected(ComponentId),
    /// Target entity is null, out of bounds, or has a stale generation.
    EntityNotFound(Entity),
    /// No registered component matches the requested name.
    ComponentNotFound,
    /// Entity does not currently possess the target component.
    MissingComponent(ComponentId),
    /// Input bytes failed strict RFC 8259 JSON parsing.
    InvalidJson(crate::json::Error),
    /// Floating-point value was NaN or infinite and cannot be represented in JSON or checked f32.
    NotFinite,
}

impl From<AllocFailure> for ReflectError {
    #[inline]
    fn from(err: AllocFailure) -> Self {
        Self::Alloc(err)
    }
}

impl From<EcsError> for ReflectError {
    #[inline]
    fn from(err: EcsError) -> Self {
        match err {
            EcsError::DeadEntity(e) => Self::EntityNotFound(e),
            EcsError::MissingComponent(cid) => Self::MissingComponent(cid),
            EcsError::Alloc(a) => Self::Alloc(a),
            other => Self::Ecs(other),
        }
    }
}

impl<A: Allocator> From<ShapeError<A>> for ReflectError {
    #[inline]
    fn from(err: ShapeError<A>) -> Self {
        Self::Shape(err.shape)
    }
}

impl From<crate::json::Error> for ReflectError {
    #[inline]
    fn from(err: crate::json::Error) -> Self {
        Self::InvalidJson(err)
    }
}

impl fmt::Display for ReflectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Alloc(err) => write!(f, "allocation failed: {err}"),
            Self::Ecs(err) => write!(f, "{err}"),
            Self::Shape(shape) => match shape {
                Shape::Expected { expected, found } => {
                    write!(f, "shape mismatch: expected {expected}, found {found}")
                }
                Shape::Missing => f.write_str("missing required field"),
                Shape::UnknownFields { count } => write!(f, "unknown field(s) ({count})"),
                Shape::UnknownVariant => f.write_str("unknown enum variant"),
                Shape::OutOfRange { ty } => write!(f, "value out of range for {ty}"),
                Shape::NoIndex { len } => write!(f, "index past end of list of length {len}"),
                Shape::Invalid { ty } => write!(f, "invalid value for {ty}"),
            },
            Self::NotReflected(cid) => write!(f, "component {cid:?} is not reflected"),
            Self::EntityNotFound(e) => write!(f, "entity not found: {e}"),
            Self::ComponentNotFound => f.write_str("component not found"),
            Self::MissingComponent(cid) => write!(f, "entity does not have component {cid:?}"),
            Self::InvalidJson(err) => write!(f, "invalid json: {err}"),
            Self::NotFinite => f.write_str("non-finite floating-point value"),
        }
    }
}

impl core::error::Error for ReflectError {}

/// Live runtime reflection interface for ECS components backed by `bstd::data::Value`.
pub trait ReflectComponent: 'static {
    /// Convert `&self` into a format-neutral [`Value`].
    fn reflect_to_value<A: Allocator + Copy>(&self, alloc: A) -> Result<Value<A>, ReflectError>;

    /// Decode `Self` from a [`Cursor`] (accepting JSON integers as floats for floating-point
    /// fields and checking all numeric ranges strictly).
    fn reflect_from_cursor<A: Allocator + Copy>(
        cursor: &Cursor<'_, '_, A>,
    ) -> Result<Self, ShapeError<A>>
    where
        Self: Sized;

    /// Describe this component's type and fields as a [`Value`]:
    /// - Primitives: `{"kind": "primitive", "type": "i32"}`
    /// - Structs: `{"kind": "struct", "name": "Pos", "fields": [{"name": "x", "type": "f32"}, ...]}`
    fn schema<A: Allocator + Copy>(alloc: A) -> Result<Value<A>, AllocFailure>;

    /// Construct a fresh value from a [`Value`] (used by `POST /api/entity` and component attach).
    fn from_patch_value<A: Allocator + Copy>(
        val: &Value<A>,
        alloc: A,
    ) -> Result<Self, ReflectError>
    where
        Self: Sized,
    {
        let cursor = Cursor::root(val, alloc);
        Self::reflect_from_cursor(&cursor).map_err(ReflectError::from)
    }

    /// Patch `self` in-place from a full or partial [`Value`]:
    /// - If both `patch` and `self.reflect_to_value(alloc)` are `Value::Map`, merges `patch`
    ///   entries over the current map (so partial edits like `{"x": 42}` preserve `y`) and
    ///   decodes back into `Self`.
    /// - Otherwise decodes `patch` directly into `Self`.
    fn patch_value<A: Allocator + Copy>(
        &mut self,
        patch: &Value<A>,
        alloc: A,
    ) -> Result<(), ReflectError>
    where
        Self: Sized,
    {
        if let Value::Map(patch_map) = patch {
            let mut current = self.reflect_to_value(alloc)?;
            if let Value::Map(ref mut cur_map) = current {
                for (k, v) in patch_map.iter() {
                    let key_owned = OwnString::try_from_str_in(k, alloc)?;
                    let val_copy = v.copy_in(alloc);
                    let _ = cur_map.insert(key_owned, val_copy);
                }
                let updated = Self::from_patch_value(&current, alloc)?;
                *self = updated;
                return Ok(());
            }
        }
        let updated = Self::from_patch_value(patch, alloc)?;
        *self = updated;
        Ok(())
    }
}

/// Build a `{"kind": "primitive", "type": ty_name}` schema value.
pub(crate) fn primitive_schema<A: Allocator + Copy>(
    ty_name: &str,
    alloc: A,
) -> Result<Value<A>, AllocFailure> {
    let mut map = Map::new_in(alloc);
    let _ = map.insert(
        OwnString::try_from_str_in("kind", alloc)?,
        Value::Text(OwnString::try_from_str_in("primitive", alloc)?),
    );
    let _ = map.insert(
        OwnString::try_from_str_in("type", alloc)?,
        Value::Text(OwnString::try_from_str_in(ty_name, alloc)?),
    );
    Ok(Value::Map(map))
}

/// Convert a finite `f64` to `f32` without integer/float `as` casts by formatting to a stack
/// buffer and parsing via Eisel-Lemire exact rounding.
fn f64_to_f32_no_cast(v: f64) -> Option<f32> {
    if !v.is_finite() {
        return None;
    }
    let mut buf = FixedString::<64>::new();
    write!(buf, "{v}").ok()?;
    let parsed = buf.as_str().parse::<f32>().ok()?;
    if parsed.is_finite() { Some(parsed) } else { None }
}

/// Convert a `blaze_data::Int` to `f32` without `as` casts.
fn int_to_f32_no_cast(n: crate::data::Int) -> Option<f32> {
    let mut buf = FixedString::<64>::new();
    write!(buf, "{n}").ok()?;
    let parsed = buf.as_str().parse::<f32>().ok()?;
    if parsed.is_finite() { Some(parsed) } else { None }
}

/// Convert a `blaze_data::Int` to `f64` without `as` casts.
fn int_to_f64_no_cast(n: crate::data::Int) -> Option<f64> {
    let mut buf = FixedString::<64>::new();
    write!(buf, "{n}").ok()?;
    let parsed = buf.as_str().parse::<f64>().ok()?;
    if parsed.is_finite() { Some(parsed) } else { None }
}

macro_rules! impl_reflect_from_value_primitive {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl ReflectComponent for $ty {
                #[inline]
                fn reflect_to_value<A: Allocator + Copy>(
                    &self,
                    alloc: A,
                ) -> Result<Value<A>, ReflectError> {
                    Ok(ToValue::to_value(self, alloc))
                }

                #[inline]
                fn reflect_from_cursor<A: Allocator + Copy>(
                    cursor: &Cursor<'_, '_, A>,
                ) -> Result<Self, ShapeError<A>> {
                    <Self as FromValue<A>>::from_value(cursor)
                }

                #[inline]
                fn schema<A: Allocator + Copy>(alloc: A) -> Result<Value<A>, AllocFailure> {
                    primitive_schema(stringify!($ty), alloc)
                }
            }
        )+
    };
}

impl_reflect_from_value_primitive!(bool, i8, i16, i32, i64, u8, u16, u32, u64, usize);

impl ReflectComponent for isize {
    #[inline]
    fn reflect_to_value<A: Allocator + Copy>(&self, alloc: A) -> Result<Value<A>, ReflectError> {
        Ok(ToValue::to_value(self, alloc))
    }

    #[inline]
    fn reflect_from_cursor<A: Allocator + Copy>(
        cursor: &Cursor<'_, '_, A>,
    ) -> Result<Self, ShapeError<A>> {
        let n = cursor.value().as_int().ok_or_else(|| cursor.expected(Kind::Int))?;
        n.as_i64()
            .and_then(|v| isize::try_from(v).ok())
            .ok_or_else(|| cursor.error(Shape::OutOfRange { ty: "isize" }))
    }

    #[inline]
    fn schema<A: Allocator + Copy>(alloc: A) -> Result<Value<A>, AllocFailure> {
        primitive_schema("isize", alloc)
    }
}

impl ReflectComponent for f32 {
    #[inline]
    fn reflect_to_value<A: Allocator + Copy>(&self, alloc: A) -> Result<Value<A>, ReflectError> {
        Ok(ToValue::to_value(self, alloc))
    }

    #[inline]
    fn reflect_from_cursor<A: Allocator + Copy>(
        cursor: &Cursor<'_, '_, A>,
    ) -> Result<Self, ShapeError<A>> {
        match cursor.value() {
            Value::Float(f) => f64_to_f32_no_cast(*f)
                .ok_or_else(|| cursor.error(Shape::OutOfRange { ty: "f32" })),
            Value::Int(n) => int_to_f32_no_cast(*n)
                .ok_or_else(|| cursor.error(Shape::OutOfRange { ty: "f32" })),
            _ => Err(cursor.expected(Kind::Float)),
        }
    }

    #[inline]
    fn schema<A: Allocator + Copy>(alloc: A) -> Result<Value<A>, AllocFailure> {
        primitive_schema("f32", alloc)
    }
}

impl ReflectComponent for f64 {
    #[inline]
    fn reflect_to_value<A: Allocator + Copy>(&self, alloc: A) -> Result<Value<A>, ReflectError> {
        Ok(ToValue::to_value(self, alloc))
    }

    #[inline]
    fn reflect_from_cursor<A: Allocator + Copy>(
        cursor: &Cursor<'_, '_, A>,
    ) -> Result<Self, ShapeError<A>> {
        match cursor.value() {
            Value::Float(f) => {
                if f.is_finite() {
                    Ok(*f)
                } else {
                    Err(cursor.error(Shape::OutOfRange { ty: "f64" }))
                }
            }
            Value::Int(n) => int_to_f64_no_cast(*n)
                .ok_or_else(|| cursor.error(Shape::OutOfRange { ty: "f64" })),
            _ => Err(cursor.expected(Kind::Float)),
        }
    }

    #[inline]
    fn schema<A: Allocator + Copy>(alloc: A) -> Result<Value<A>, AllocFailure> {
        primitive_schema("f64", alloc)
    }
}

impl<T: ReflectComponent> ReflectComponent for Option<T> {
    fn reflect_to_value<A: Allocator + Copy>(&self, alloc: A) -> Result<Value<A>, ReflectError> {
        match self {
            Some(v) => v.reflect_to_value(alloc),
            None => Ok(Value::Null),
        }
    }

    fn reflect_from_cursor<A: Allocator + Copy>(
        cursor: &Cursor<'_, '_, A>,
    ) -> Result<Self, ShapeError<A>> {
        if cursor.is_null() {
            Ok(None)
        } else {
            T::reflect_from_cursor(cursor).map(Some)
        }
    }

    fn schema<A: Allocator + Copy>(alloc: A) -> Result<Value<A>, AllocFailure> {
        let mut map = Map::new_in(alloc);
        let _ = map.insert(
            OwnString::try_from_str_in("kind", alloc)?,
            Value::Text(OwnString::try_from_str_in("option", alloc)?),
        );
        let _ = map.insert(OwnString::try_from_str_in("inner", alloc)?, T::schema(alloc)?);
        Ok(Value::Map(map))
    }
}

/// Internal helpers used by [`crate::reflect_struct!`]; not part of the stable public API.
#[doc(hidden)]
pub mod __private {
    use core::alloc::Allocator;

    pub use blaze_own::{AllocFailure, String as OwnString};

    use crate::data::{Map, Value};

    use super::{ReflectComponent, ReflectError};

    /// Insert a field into a [`Map`] inside `ToValue::to_value` without calling `.clone()`.
    #[inline]
    pub fn insert_field_value<A: Allocator + Copy>(
        map: &mut Map<A>,
        name: &str,
        val: Value<A>,
        alloc: A,
    ) {
        let key = OwnString::from_str_in(name, alloc);
        let _ = map.insert(key, val);
    }

    /// Insert a fallibly reflected field into a [`Map`].
    #[inline]
    pub fn try_insert_reflected_field<T: ReflectComponent, A: Allocator + Copy>(
        map: &mut Map<A>,
        name: &str,
        field: &T,
        alloc: A,
    ) -> Result<(), ReflectError> {
        let key = OwnString::try_from_str_in(name, alloc)?;
        let val = field.reflect_to_value(alloc)?;
        let _ = map.insert(key, val);
        Ok(())
    }

    /// Build a canonical struct schema:
    /// `{"kind": "struct", "name": "<Name>", "fields": [{"name": "x", "type": "f32"}, ...]}`.
    pub fn struct_schema<A: Allocator + Copy>(
        name: &str,
        fields: &[(&str, &str)],
        alloc: A,
    ) -> Result<Value<A>, AllocFailure> {
        let mut root = Map::new_in(alloc);
        let _ = root.insert(
            OwnString::try_from_str_in("kind", alloc)?,
            Value::Text(OwnString::try_from_str_in("struct", alloc)?),
        );
        let _ = root.insert(
            OwnString::try_from_str_in("name", alloc)?,
            Value::Text(OwnString::try_from_str_in(name, alloc)?),
        );
        let mut list = blaze_own::Vec::try_with_capacity_in(fields.len(), alloc)?;
        for &(fname, ftype) in fields {
            let mut fobj = Map::new_in(alloc);
            let _ = fobj.insert(
                OwnString::try_from_str_in("name", alloc)?,
                Value::Text(OwnString::try_from_str_in(fname, alloc)?),
            );
            let _ = fobj.insert(
                OwnString::try_from_str_in("type", alloc)?,
                Value::Text(OwnString::try_from_str_in(ftype, alloc)?),
            );
            list.try_push(Value::Map(fobj)).map_err(|r| r.why)?;
        }
        let _ = root.insert(OwnString::try_from_str_in("fields", alloc)?, Value::List(list));
        Ok(Value::Map(root))
    }

    /// Build a canonical single-field newtype struct schema:
    /// `{"kind": "newtype", "name": "<Name>", "type": "<Inner>"}`.
    pub fn newtype_schema<A: Allocator + Copy>(
        name: &str,
        inner_type: &str,
        alloc: A,
    ) -> Result<Value<A>, AllocFailure> {
        let mut root = Map::new_in(alloc);
        let _ = root.insert(
            OwnString::try_from_str_in("kind", alloc)?,
            Value::Text(OwnString::try_from_str_in("newtype", alloc)?),
        );
        let _ = root.insert(
            OwnString::try_from_str_in("name", alloc)?,
            Value::Text(OwnString::try_from_str_in(name, alloc)?),
        );
        let _ = root.insert(
            OwnString::try_from_str_in("type", alloc)?,
            Value::Text(OwnString::try_from_str_in(inner_type, alloc)?),
        );
        Ok(Value::Map(root))
    }
}

/// Implement [`crate::data::ToValue`], [`crate::data::FromValue`], and
/// [`crate::ecs::ReflectComponent`] for an existing plain struct in one line, with zero `unsafe`,
/// zero panics, zero casts, zero clones, and explicit allocator support.
///
/// Supports named-field structs and single-field newtype structs:
/// ```ignore
/// #[derive(Clone, Copy, PartialEq, Debug)]
/// struct Pos { x: f32, y: f32 }
/// reflect_struct!(Pos { x: f32, y: f32 });
///
/// #[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// struct Health(i32);
/// reflect_struct!(Health(i32));
/// ```
#[macro_export]
macro_rules! reflect_struct {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        impl<A: ::core::alloc::Allocator + ::core::marker::Copy> $crate::data::ToValue<A> for $name {
            fn to_value(&self, alloc: A) -> $crate::data::Value<A> {
                let mut map = $crate::data::Map::new_in(alloc);
                $(
                    $crate::ecs::reflect::__private::insert_field_value(
                        &mut map,
                        ::core::stringify!($field),
                        $crate::data::ToValue::to_value(&self.$field, alloc),
                        alloc,
                    );
                )*
                $crate::data::Value::Map(map)
            }
        }

        impl<A: ::core::alloc::Allocator + ::core::marker::Copy> $crate::data::FromValue<A> for $name {
            fn from_value(
                cursor: &$crate::data::Cursor<'_, '_, A>,
            ) -> ::core::result::Result<Self, $crate::data::ShapeError<A>> {
                cursor.fields(|f| {
                    ::core::result::Result::Ok(Self {
                        $(
                            $field: {
                                let fc = f.cursor(::core::stringify!($field))?;
                                <$ty as $crate::ecs::ReflectComponent>::reflect_from_cursor(&fc)?
                            },
                        )*
                    })
                })
            }
        }

        impl $crate::ecs::ReflectComponent for $name {
            fn reflect_to_value<A: ::core::alloc::Allocator + ::core::marker::Copy>(
                &self,
                alloc: A,
            ) -> ::core::result::Result<$crate::data::Value<A>, $crate::ecs::ReflectError> {
                let mut map = $crate::data::Map::new_in(alloc);
                $(
                    $crate::ecs::reflect::__private::try_insert_reflected_field(
                        &mut map,
                        ::core::stringify!($field),
                        &self.$field,
                        alloc,
                    )?;
                )*
                ::core::result::Result::Ok($crate::data::Value::Map(map))
            }

            #[inline]
            fn reflect_from_cursor<A: ::core::alloc::Allocator + ::core::marker::Copy>(
                cursor: &$crate::data::Cursor<'_, '_, A>,
            ) -> ::core::result::Result<Self, $crate::data::ShapeError<A>> {
                <Self as $crate::data::FromValue<A>>::from_value(cursor)
            }

            #[inline]
            fn schema<A: ::core::alloc::Allocator + ::core::marker::Copy>(
                alloc: A,
            ) -> ::core::result::Result<$crate::data::Value<A>, $crate::ecs::reflect::__private::AllocFailure> {
                $crate::ecs::reflect::__private::struct_schema(
                    ::core::stringify!($name),
                    &[$( (::core::stringify!($field), ::core::stringify!($ty)) ),*],
                    alloc,
                )
            }
        }
    };
    ($name:ident ( $ty:ty )) => {
        impl<A: ::core::alloc::Allocator + ::core::marker::Copy> $crate::data::ToValue<A> for $name {
            #[inline]
            fn to_value(&self, alloc: A) -> $crate::data::Value<A> {
                $crate::data::ToValue::to_value(&self.0, alloc)
            }
        }

        impl<A: ::core::alloc::Allocator + ::core::marker::Copy> $crate::data::FromValue<A> for $name {
            #[inline]
            fn from_value(
                cursor: &$crate::data::Cursor<'_, '_, A>,
            ) -> ::core::result::Result<Self, $crate::data::ShapeError<A>> {
                let inner = <$ty as $crate::ecs::ReflectComponent>::reflect_from_cursor(cursor)?;
                ::core::result::Result::Ok(Self(inner))
            }
        }

        impl $crate::ecs::ReflectComponent for $name {
            #[inline]
            fn reflect_to_value<A: ::core::alloc::Allocator + ::core::marker::Copy>(
                &self,
                alloc: A,
            ) -> ::core::result::Result<$crate::data::Value<A>, $crate::ecs::ReflectError> {
                <$ty as $crate::ecs::ReflectComponent>::reflect_to_value(&self.0, alloc)
            }

            #[inline]
            fn reflect_from_cursor<A: ::core::alloc::Allocator + ::core::marker::Copy>(
                cursor: &$crate::data::Cursor<'_, '_, A>,
            ) -> ::core::result::Result<Self, $crate::data::ShapeError<A>> {
                <Self as $crate::data::FromValue<A>>::from_value(cursor)
            }

            #[inline]
            fn schema<A: ::core::alloc::Allocator + ::core::marker::Copy>(
                alloc: A,
            ) -> ::core::result::Result<$crate::data::Value<A>, $crate::ecs::reflect::__private::AllocFailure> {
                $crate::ecs::reflect::__private::newtype_schema(
                    ::core::stringify!($name),
                    ::core::stringify!($ty),
                    alloc,
                )
            }
        }
    };
}
