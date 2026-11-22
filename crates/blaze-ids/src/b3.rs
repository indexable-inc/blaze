//! BLAKE3 for all of blaze: `blaze-blake3`, the workspace's own, the one crate that computes it.
//! It is official BLAKE3 bit for bit (its tests hold it to the official test vectors and to the
//! `blake3` crate), so every hash, cache key and content ID is the bytes official BLAKE3 gives,
//! as every record written before it was.

#[cfg(all(test, not(blaze_check)))]
mod tests;

pub use blaze_blake3::{BLOCK_LEN, CHUNK_LEN, OUT_LEN};

/// What [`context_key`] makes of a key-derivation context, the key every input under that
/// context is hashed with.
pub type ContextKey = [u8; 32];

/// The BLAKE3 blaze is built with.
pub const BACKEND: &str = "blaze-blake3";

/// A message's hash, in BLAKE3's own mode, its key the IV.
#[inline]
#[blaze::pure(
    trusted = "blaze-blake3 picks its SIMD width by CPU detection std caches in a static: every width gives the same bytes, so no result depends on whether or when that ran"
)]
pub fn hash(bytes: &[u8]) -> [u8; 32] {
    blaze_blake3::hash(bytes)
}

/// `context`'s key for [`derive_key_with`] and [`Hasher::new_from_context_key`]: official
/// BLAKE3's `hazmat::hash_derive_key_context`.
#[inline]
#[blaze::pure(
    trusted = "blaze-blake3 picks its SIMD width by CPU detection std caches in a static: every width gives the same bytes, so no result depends on whether or when that ran"
)]
pub fn context_key(context: &str) -> ContextKey {
    blaze_blake3::context_key(context)
}

/// `material`'s derived key under the context whose [`context_key`] is `key`: what a
/// [`Hasher::new_from_context_key`] fed `material` finalizes to, with no hasher.
#[inline]
#[blaze::pure(
    trusted = "blaze-blake3 picks its SIMD width by CPU detection std caches in a static: every width gives the same bytes, so no result depends on whether or when that ran"
)]
pub fn derive_key_with(key: &ContextKey, material: &[u8]) -> [u8; 32] {
    blaze_blake3::derive_key_with(key, material)
}

/// A message fed in pieces: `new` (the plain hash), `new_derive_key(context)` and
/// `new_from_context_key(key)`, then `update`s and `finalize`.
pub use blaze_blake3::Hasher;

/// [`crate::ContentId::of_many`]'s way through what [`crate::many`]'s last blocks don't
/// cover: every block of a one-chunk input before its last in [`crate::many::leading_lanes`],
/// a lane each, and an input longer than a chunk through [`derive_key_with`].
pub(crate) struct Many;

impl crate::many::Leading for Many {
    fn leading_blocks(
        key: &[u32; 8],
        inputs: &[&[u8]],
        members: &[u32],
        blocks: usize,
        cvs: &mut Vec<[u32; 8]>,
    ) {
        crate::many::leading_lanes(key, inputs, members, blocks, cvs);
    }

    fn long(key: &ContextKey, input: &[u8]) -> [u8; 32] {
        derive_key_with(key, input)
    }
}
