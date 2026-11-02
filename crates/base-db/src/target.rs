//! Information about the target.

use std::fmt;

use blaze_ids::{ContentId, Hasher};
use intern::Symbol;
use triomphe::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    // Only what we need is present here.
    Wasm32,
    Wasm64,
    Other,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)] // clone: setup: once per crate graph
pub struct TargetData {
    /// The target tuple, as `--target` names it (`x86_64-unknown-linux-gnu`): what rustc's
    /// messages about the target say.
    pub tuple: Symbol,
    pub data_layout: Box<str>,
    pub arch: Arch,
    /// The spec's `has-thumb-interworking`: whether `#[instruction_set(arm::a32)]` and
    /// `#[instruction_set(arm::t32)]` are accepted (rustc's `UnsupportedInstructionSet`).
    pub has_thumb_interworking: bool,
}

impl TargetData {
    /// Of the tuple, the data layout, the arch (a byte) and `has_thumb_interworking` (a byte).
    pub const DOMAIN: &'static str = "blaze v2 target data 2";

    pub fn content(&self) -> ContentId {
        let arch = match self.arch {
            Arch::Wasm32 => 0,
            Arch::Wasm64 => 1,
            Arch::Other => 2,
        };
        Hasher::new(TargetData::DOMAIN)
            .part(self.tuple.as_str().as_bytes())
            .part(self.data_layout.as_bytes())
            .update(&[arch, self.has_thumb_interworking as u8])
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TargetLoadError(Arc<str>);

impl fmt::Debug for TargetLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for TargetLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for TargetLoadError {}

impl From<String> for TargetLoadError {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl From<&str> for TargetLoadError {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

pub type TargetLoadResult = Result<TargetData, TargetLoadError>;
