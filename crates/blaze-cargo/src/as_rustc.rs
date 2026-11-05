//! What the cargo drop-in knows of blaze standing in for a build script's rustc; the rest is
//! the engine's (`crates/blaze-engine/src/as_rustc.rs`).

/// The variable that names, to this, the toolchain's rustc: what cargo gave the script as
/// `RUSTC`.
pub const TOOLCHAIN: &str = "BLAZE_SCRIPT_RUSTC";

/// The name blaze is started under to be this.
pub const NAME: &str = "rustc";
