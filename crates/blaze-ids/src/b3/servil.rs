//! BLAKE3 by `blake3-servil` (johnservil/BLAKE3, pinned in the root manifest): official BLAKE3
//! 1.8.7 with its own assembly kernels (integer + NEON and SME2 on AArch64; SSE4.1, AVX2 and
//! AVX-512 on x86-64), each proved against a Lean spec.
//!
//! The fork's contract: `blake3_servil::initialize()` runs once before any other call (a call
//! before it panics); it runs the startup self-test on the calling thread (about 45 KiB of its
//! stack, under 0.2 ms) and records the CPU's kernels. Every entry here passes [`ready`] first,
//! one acquire load once it has run. Only the single-threaded calls are made: the fork's pool
//! starts its threads in `initialize_multithreaded` alone, which blaze never calls, so no thread
//! is ever started here.

use std::sync::Once;

use blake3_servil::{Mode, convenience, performance};

use super::ContextKey;
use crate::{ContentId, many};

pub const NAME: &str = "blake3-servil";

static READY: Once = Once::new();

#[inline]
fn ready() {
    READY.call_once(blake3_servil::initialize);
}

pub fn hash(bytes: &[u8]) -> [u8; 32] {
    ready();
    *convenience::hash(bytes).as_bytes()
}

pub fn context_key(context: &str) -> ContextKey {
    ready();
    performance::context_key(context)
}

pub fn derive_key_with(key: &ContextKey, material: &[u8]) -> [u8; 32] {
    ready();
    *convenience::hash_with(Mode::DeriveKeyWith(key), material).as_bytes()
}

/// About 18 KiB (the fork stages short updates in 16 KiB before hashing them side by side):
/// [`derive_key_with`] and [`hash`] need none, and serve every message already in one slice.
pub struct Hasher(performance::Hasher);

impl Hasher {
    pub fn new() -> Hasher {
        ready();
        Hasher(performance::Hasher::new(Mode::Plain))
    }

    pub fn new_derive_key(context: &str) -> Hasher {
        ready();
        Hasher(performance::Hasher::new(Mode::DeriveKey(context)))
    }

    pub fn new_from_context_key(key: &ContextKey) -> Hasher {
        ready();
        Hasher(performance::Hasher::new(Mode::DeriveKeyWith(key)))
    }

    #[inline]
    pub fn update(&mut self, bytes: &[u8]) -> &mut Hasher {
        self.0.update(bytes);
        self
    }

    #[inline]
    pub fn finalize(&self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}

impl Default for Hasher {
    fn default() -> Hasher {
        Hasher::new()
    }
}

/// [`ContentId::of_many`]'s way here: the fork exports no kernel that stops short of a chunk's
/// root (its batches take messages of one length and give their roots), so every block of a
/// one-chunk input goes through blaze's own lanes ([`many::leading_lanes`]); a longer input
/// through [`derive_key_with`].
pub struct Many;

impl many::Leading for Many {
    fn leading_blocks(key: &[u32; 8], inputs: &[&[u8]], members: &[u32], blocks: usize, cvs: &mut Vec<[u32; 8]>) {
        many::leading_lanes(key, inputs, members, blocks, cvs);
    }

    fn long(key: &ContextKey, input: &[u8]) -> [u8; 32] {
        derive_key_with(key, input)
    }
}

/// [`ContentId::of_many_into`] by this backend, whichever one blaze builds with.
pub fn of_many_into(domain: &'static str, inputs: &[&[u8]], out: &mut Vec<ContentId>) {
    many::of_many_into::<Many>(crate::context_key(domain), inputs, out);
}
