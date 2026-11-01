//! Maps keyed by something that is a hash already (a content ID, a store key, a 64-bit hash):
//! the key's first word spreads over a table as well as any hash of it would, so a lookup
//! hashes nothing.
//!
//! [`KeyHasher`] takes the first eight bytes of each `write` (a `u64` arrives as its eight
//! bytes) and ignores lengths, so a `[u8; 32]` hashes as its first word and a `u64` as itself.
//! A key of fewer than eight bytes per write (a `u32`, a `bool`) hashes to 0: such a key is not
//! a hash and wants [`FixedState`](crate::FixedState).

/// The hasher of [`ByHash`] and [`ByHashSet`].
#[derive(Default, Clone, Copy)]
pub struct KeyHasher(u64);

impl std::hash::Hasher for KeyHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        if let Some(word) = bytes.first_chunk::<8>() {
            self.0 ^= u64::from_le_bytes(*word);
        }
    }

    #[inline]
    fn write_u64(&mut self, word: u64) {
        self.0 ^= word;
    }

    #[inline]
    fn write_usize(&mut self, _: usize) {}
}

/// The `BuildHasher` of [`KeyHasher`], for a table built elsewhere (`hashbrown`, `with_hasher`).
pub type BuildKeyHasher = std::hash::BuildHasherDefault<KeyHasher>;

/// A map on hashes, indexed by the hash itself.
#[expect(clippy::disallowed_types, reason = "std's map on the identity hasher")]
pub type ByHash<K, V> = std::collections::HashMap<K, V, BuildKeyHasher>; // alloc: not SipHash: hashed by `KeyHasher`, the key's first word

/// A set of hashes, indexed by the hash itself.
#[expect(clippy::disallowed_types, reason = "std's set on the identity hasher")]
pub type ByHashSet<K> = std::collections::HashSet<K, BuildKeyHasher>; // alloc: not SipHash: as above
