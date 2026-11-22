//! The content hash of a world's answer, which a read list keeps (typeck.md, "`World`").
//!
//! FNV-1a over 64 bits, written here because the crate is at zero dependencies. It becomes
//! blaze-ids' `ContentId` (BLAKE3, 256 bits) once that crate is at zero third-party crates; until
//! then a collision makes a stale read look fresh with probability about `2^-64` per read.

/// A stable, seedless hasher: equal inputs give equal hashes on every machine and in every
/// process (rule 25). No state of the machine reaches it.
#[derive(Clone, Copy, Debug)]
pub struct StableHasher {
    state: u64,
}

const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

impl StableHasher {
    /// A hasher in a named domain, so two kinds of value never hash alike by construction.
    pub const fn new(domain: &str) -> StableHasher {
        let mut h = StableHasher { state: OFFSET };
        h = h.bytes(domain.as_bytes());
        h.byte(0xff)
    }

    /// The hash of the bytes so far, after `bytes`.
    pub const fn bytes(mut self, bytes: &[u8]) -> StableHasher {
        let mut i = 0;
        while i < bytes.len() {
            self.state ^= bytes[i] as u64;
            self.state = self.state.wrapping_mul(PRIME);
            i += 1;
        }
        self
    }

    /// One byte.
    pub const fn byte(mut self, b: u8) -> StableHasher {
        self.state ^= b as u64;
        self.state = self.state.wrapping_mul(PRIME);
        self
    }

    /// A `u32`, little-endian.
    pub fn u32(&mut self, v: u32) {
        *self = self.bytes(&v.to_le_bytes());
    }

    /// A `u64`, little-endian.
    pub fn u64(&mut self, v: u64) {
        *self = self.bytes(&v.to_le_bytes());
    }

    /// A `u128`, little-endian.
    pub fn u128(&mut self, v: u128) {
        *self = self.bytes(&v.to_le_bytes());
    }

    /// A tag byte, separating variants of an enum.
    pub fn tag(&mut self, v: u8) {
        *self = self.byte(v);
    }

    /// The hash.
    pub const fn finish(self) -> u64 {
        self.state
    }
}

/// A value whose content a read list may hash. Implemented by every answer of `World`, so the
/// recorder hashes what the driver was told, negative answers included.
pub trait ContentHash {
    /// Feeds this value's content, and nothing of where it lives, to `h`.
    fn hash_into(&self, h: &mut StableHasher);
}

impl<T: ContentHash> ContentHash for Option<T> {
    fn hash_into(&self, h: &mut StableHasher) {
        match self {
            None => h.tag(0),
            Some(v) => {
                h.tag(1);
                v.hash_into(h);
            }
        }
    }
}

impl<T: ContentHash> ContentHash for [T] {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u64(self.len() as u64);
        for v in self {
            v.hash_into(h);
        }
    }
}

impl ContentHash for u32 {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(*self);
    }
}

impl ContentHash for u64 {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u64(*self);
    }
}

impl ContentHash for bool {
    fn hash_into(&self, h: &mut StableHasher) {
        h.tag(u8::from(*self));
    }
}
