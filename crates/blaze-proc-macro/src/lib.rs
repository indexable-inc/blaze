//! Proc macros as `wasm32-wasip1` modules run in Wasmtime (rule 10; docs/v2/plan/design/bottom.md,
//! section 7). blaze-expand hands a call over ([`blaze_expand::ProcCall`]); this crate runs it
//! and gives back tokens with their origins, the outside reads the run made, and the macro's
//! diagnostics ([`blaze_expand::Produced`]), or an error naming what stopped it.
//!
//! - [`module`]: the module record a bootstrap builder wrote (D52), found by the macro crate's
//!   identity, and the macros it lists.
//! - [`server`]: blaze's server of the pinned bridge ([`protocol`], [`wire`]), a sans-IO state
//!   machine over blaze-expand's tokens. Spans are item-relative.
//! - [`host`]: the driver: a fresh instance per call, fuel and memory limits, and the sandbox
//!   (`sandbox.rs`), which serves the environment and the workspace's files read-only and
//!   records every read.
//! - [`record`]: the expansion record: keyed by the module's wasm digest, the macro, the kind
//!   and the input, with the reads as its read-list.

#![feature(allocator_api)]

// Without `runtime`: a module's identity and key, the module store's lookup, the expansion
// record's key and lookup. A command that only asks whether a module or an answer is kept
// links no Wasmtime.
pub mod comptime_patch;
pub mod module;
pub mod package_env;
pub mod parts;
pub mod record;
pub mod sandbox;

/// O(1) refcount share of a `std::sync::Arc` handle.
#[cfg(feature = "runtime")]
pub(crate) trait ShareArc {
    fn share_arc(&self) -> Self;
}
#[cfg(feature = "runtime")]
impl<T: ?Sized> ShareArc for std::sync::Arc<T> { #[inline] fn share_arc(&self) -> Self { std::sync::Arc::clone(self) } }

// With `runtime`: running a module (the bridge, the host, the hook, build scripts).
#[cfg(feature = "runtime")]
pub mod fact_pins;
#[cfg(feature = "runtime")]
pub mod hook;
#[cfg(feature = "runtime")]
pub mod host;
#[cfg(feature = "runtime")]
pub mod jobserver;
#[cfg(feature = "runtime")]
pub mod native_bridge;
#[cfg(feature = "runtime")]
pub mod native_c;
#[cfg(feature = "runtime")]
pub mod native_c_tasks;
#[cfg(feature = "runtime")]
pub mod native_facts;
#[cfg(feature = "runtime")]
pub mod native_script;
#[cfg(feature = "runtime")]
pub mod native_setting;
#[cfg(feature = "runtime")]
pub mod native_unit;
#[cfg(feature = "runtime")]
pub mod protocol;
#[cfg(feature = "runtime")]
pub mod script;
#[cfg(feature = "runtime")]
pub mod script_probes;
#[cfg(feature = "runtime")]
pub mod script_sandbox;
#[cfg(feature = "runtime")]
pub mod script_unit;
#[cfg(feature = "runtime")]
pub mod server;
#[cfg(feature = "runtime")]
pub mod spawn_records;
#[cfg(feature = "runtime")]
pub mod spawn_route;
#[cfg(feature = "runtime")]
pub mod wire;

#[cfg(all(test, feature = "runtime"))]
mod fact_pins_tests;
#[cfg(all(test, feature = "runtime"))]
mod native_setting_tests;
#[cfg(all(test, feature = "runtime"))]
mod native_setting_nm_tests;
#[cfg(all(test, feature = "runtime"))]
mod script_probes_tests;
#[cfg(all(test, feature = "runtime"))]
mod script_tests;
#[cfg(all(test, feature = "runtime"))]
mod spawn_route_tests;
#[cfg(all(test, feature = "runtime"))]
mod tests;
