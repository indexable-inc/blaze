//! Compiler-backed enum reflection intrinsics and blanket trait impls (`cfg(blaze)` only).
//!
//! Rustc cannot parse `inline for` or `where const { ... }`; this submodule is gated on
//! `#[cfg(blaze)]` per `docs/v2/plan/design/std.md` ("Two builders").

use super::{
    Enum, EnumMeta, FromLowercaseStr, FromSnakeStr, FromVariantStr, UnitEnum,
    has_unique_lowercase_variants, has_unique_snake_variants, variant_count,
};

#[rustc_intrinsic]
pub(super) const fn blaze_is_enum<T: ?Sized>() -> bool;

#[rustc_intrinsic]
pub(super) const fn blaze_is_unit_enum<T: ?Sized>() -> bool;

#[rustc_intrinsic]
pub(super) const fn blaze_variant_count<T: ?Sized>() -> usize;

#[rustc_intrinsic]
pub(super) const fn blaze_variant_name<T: ?Sized>(index: usize) -> &'static str;

impl<E: Enum> EnumMeta for E {
    fn as_str(&self) -> &'static str {
        inline for arm in self { arm.name }
    }

    fn as_lowercase_str(&self) -> &'static str {
        inline for arm in self { arm.lower }
    }

    fn as_snake_str(&self) -> &'static str {
        inline for arm in self { arm.snake }
    }

    fn variant_index(&self) -> usize {
        inline for arm in self { arm.index }
    }

    fn variant_count() -> usize {
        variant_count::<Self>()
    }
}

impl<E: UnitEnum> FromVariantStr for E {
    fn from_variant_str(s: &str) -> Option<Self> {
        inline for v in variants::<Self>() {
            if s == v.name {
                return Some(v.value);
            }
        }
        None
    }
}

impl<E: UnitEnum> FromLowercaseStr for E
where
    const { has_unique_lowercase_variants::<E>() }
{
    fn from_lowercase_str(s: &str) -> Option<Self> {
        inline for v in variants::<Self>() {
            if s == v.lower {
                return Some(v.value);
            }
        }
        None
    }
}

impl<E: UnitEnum> FromSnakeStr for E
where
    const { has_unique_snake_variants::<E>() }
{
    fn from_snake_str(s: &str) -> Option<Self> {
        inline for v in variants::<Self>() {
            if s == v.snake {
                return Some(v.value);
            }
        }
        None
    }
}
