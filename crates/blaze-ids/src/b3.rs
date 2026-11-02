//! BLAKE3 for all of blaze: the one module that names a BLAKE3 crate, so the backend is one
//! choice, made per target when blaze is built.
//!
//! - [`servil`], `blake3-servil`, on the targets its build script builds for: x86-64 Linux and
//!   little-endian AArch64 Linux and macOS.
//! - [`official`], the `blake3` crate, on every other target (Windows, x86-64 macOS), which the
//!   fork's build script stops on; and beside the fork, as the reference its differential test
//!   (`tests.rs`) and the `blake3` benchmark hold it to.
//!
//! Both are the same function: every hash, cache key and content ID is the same bytes whichever
//! one built it.

pub mod official;
#[cfg(all(
    target_endian = "little",
    any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")),
    ),
))]
pub mod servil;

#[cfg(all(
    target_endian = "little",
    any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")),
    ),
))]
use servil as backend;
#[cfg(not(all(
    target_endian = "little",
    any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")),
    ),
)))]
use official as backend;

#[cfg(all(
    test,
    target_endian = "little",
    any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")),
    ),
))]
mod tests;

pub const OUT_LEN: usize = 32;
pub const BLOCK_LEN: usize = 64;
pub const CHUNK_LEN: usize = 1024;

/// What [`context_key`] makes of a key-derivation context, the key every input under that
/// context is hashed with.
pub type ContextKey = [u8; 32];

/// The backend blaze is built with, `"blake3-servil"` or `"blake3"`.
pub const BACKEND: &str = backend::NAME;

/// A message's hash, in BLAKE3's own mode, its key the IV.
#[inline]
pub fn hash(bytes: &[u8]) -> [u8; 32] {
    backend::hash(bytes)
}

/// `context`'s key for [`derive_key_with`] and [`Hasher::new_from_context_key`]: official
/// BLAKE3's `hazmat::hash_derive_key_context`.
#[inline]
pub fn context_key(context: &str) -> ContextKey {
    backend::context_key(context)
}

/// `material`'s derived key under the context whose [`context_key`] is `key`: what a
/// [`Hasher::new_from_context_key`] fed `material` finalizes to, with no hasher.
#[inline]
pub fn derive_key_with(key: &ContextKey, material: &[u8]) -> [u8; 32] {
    backend::derive_key_with(key, material)
}

/// A message fed in pieces: `new` (the plain hash), `new_derive_key(context)` and
/// `new_from_context_key(key)`, then `update`s and `finalize`.
pub use backend::Hasher;

/// The backend's way through [`crate::ContentId::of_many`].
pub(crate) use backend::Many;
