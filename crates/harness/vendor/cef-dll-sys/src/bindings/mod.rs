// Only the Mac's: harness's browser host runs on Apple silicon (crates/harness/harness-browser/README.md).
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod aarch64_apple_darwin;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use aarch64_apple_darwin::*;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("harness's cef-dll-sys carries only aarch64-apple-darwin's bindings (vendor/cef-dll-sys/SOURCE.md)");
