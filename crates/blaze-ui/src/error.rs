//! Typed error enum for `blaze-ui` (zero untyped string errors).

use blaze_ecs::{EcsError, Entity};
use blaze_own::AllocFailure;

/// Typed error returned by `blaze-ui` operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiError {
    /// Underlying relational ECS error.
    Ecs(EcsError),
    /// Explicit allocator failure.
    Alloc(AllocFailure),
    /// Shader IR formatting overflowed the target buffer.
    ShaderFormat,
    /// Referenced entity is dead or missing a required UI component.
    InvalidEntity(Entity),
    /// Bounded layout or dock tree capacity exceeded.
    CapacityExceeded,
    /// Malformed or truncated `BZG1` Slug vector glyph table.
    InvalidGlyphTable,
}

impl From<EcsError> for UiError {
    #[inline]
    fn from(e: EcsError) -> Self {
        Self::Ecs(e)
    }
}

impl From<AllocFailure> for UiError {
    #[inline]
    fn from(e: AllocFailure) -> Self {
        Self::Alloc(e)
    }
}

impl From<core::fmt::Error> for UiError {
    #[inline]
    fn from(_: core::fmt::Error) -> Self {
        Self::ShaderFormat
    }
}
