//! Entropy for the `getrandom` crate on this target, from the host's `random_get` (WASI preview
//! 1), the one source std's `sys::random` reads here too: blaze-seed-run serves it from the
//! host's `getentropy`, and stops the seed, naming the call, when the host has none.
//!
//! getrandom has no backend for wasm64 WASI (its `wasi_p1` is wasm32's, with 32-bit pointers),
//! so this target selects its `custom` backend (`.cargo/config.toml`'s `getrandom_backend`), and
//! every program built for it links the backend's function from here, where the rest of the
//! target's C runtime is, rather than from each program.

use core::num::NonZeroI32;

/// getrandom's `custom` backend: fills `len` bytes at `dest`. getrandom 0.3 and 0.4 both call
/// this one symbol, each declaring it as returning `Result<(), getrandom::Error>`.
///
/// ABI: each `getrandom::Error` is a struct of one `NonZeroI32` (both versions, every target but
/// UEFI), so `Result<(), Error>` and `Result<(), NonZeroI32>` are both an `i32` with 0 for `Ok`.
/// An OS error is held negated (getrandom's `Error::from_neg_error_code`), so a failure here is
/// the WASI errno negated and `Error::raw_os_error` gives it back.
#[unsafe(no_mangle)]
pub unsafe extern "Rust" fn __getrandom_v03_custom(dest: *mut u8, len: usize) -> Result<(), NonZeroI32> {
    match wasip1::random_get(dest, len) {
        Ok(()) => Ok(()),
        Err(errno) => Err(NonZeroI32::new(-i32::from(errno.raw())).expect("a WASI error's errno is never 0")),
    }
}
