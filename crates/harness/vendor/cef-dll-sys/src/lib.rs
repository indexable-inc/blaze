//! cef-rs's generated bindings of CEF 156.0.15's C API (aarch64-apple-darwin), with a build script that compiles the
//! framework loader stubs from the pinned, verified CEF that `crates/harness/scripts/cef.py fetch` put in the cache.
#[allow(non_snake_case, non_camel_case_types, non_upper_case_globals, dead_code, clippy::all)]
mod bindings;
pub use bindings::*;

/// The framework's binary, relative to the directory holding the framework.
pub const FRAMEWORK_PATH: &str = "Chromium Embedded Framework.framework/Chromium Embedded Framework";

/// The CEF distribution this crate was built against (its `Release/` holds the framework).
#[must_use]
pub fn get_cef_dir() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(env!("HARNESS_CEF_DIR")))
}
