//! serde with blaze's own derive. `Serialize` and `Deserialize` name both serde's trait and
//! blaze-serde-derive's derive, as `serde::{Serialize, Deserialize}` do with serde's derive
//! feature, so `use blaze_serde::{Deserialize, Serialize};` is the one import a crate needs for
//! `#[derive(..)]`, `impl Serialize for ..` and `T: Deserialize<'de>` alike.
//!
//! The derive's impls name this crate as `::blaze_serde`: [`serde`] for the traits and
//! [`__private`] for what every impl would otherwise repeat.
//!
//! An attribute the derive does not support is a compile error naming it, the item and where it
//! is (not run here, `doctest = false`; blaze-engine's repro
//! `a_serde_attribute_blaze_serde_derive_lacks_is_an_error_naming_it` builds it):
//!
//! ```compile_fail
//! #[derive(blaze_serde::Serialize)]
//! #[serde(rename = "other")]
//! struct Renamed {
//!     a: u8,
//! }
//! ```

pub use blaze_serde_derive::{Deserialize, Serialize};
pub use serde;
// From `ser` and `de`, not the crate root, which with serde's `derive` feature also has
// serde_derive's derives under these names.
pub use serde::{de::Deserialize, ser::Serialize};

#[doc(hidden)]
pub mod __private;
