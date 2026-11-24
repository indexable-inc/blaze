//! Typed error enum for `blaze-site` (zero untyped string errors).

use blaze_ecs::EcsError;
use blaze_own::AllocFailure;
use blaze_ui::UiError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteError {
    Ui(UiError),
    Ecs(EcsError),
    Alloc(AllocFailure),
    JsonParse,
    InvalidModel,
}

impl From<UiError> for SiteError {
    #[inline]
    fn from(e: UiError) -> Self {
        Self::Ui(e)
    }
}

impl From<EcsError> for SiteError {
    #[inline]
    fn from(e: EcsError) -> Self {
        Self::Ecs(e)
    }
}

impl From<AllocFailure> for SiteError {
    #[inline]
    fn from(e: AllocFailure) -> Self {
        Self::Alloc(e)
    }
}
