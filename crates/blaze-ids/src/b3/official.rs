//! BLAKE3 by the official `blake3` crate: the backend on every target `blake3-servil` refuses to
//! build for (Windows, x86-64 macOS), and the reference the fork's differential test and
//! benchmark hold it to.

use blake3::{
    IncrementCounter,
    hazmat::{self, HasherExt},
    platform::{self, Platform},
};

use super::{BLOCK_LEN, ContextKey, OUT_LEN};
use crate::{ContentId, many};

pub const NAME: &str = "blake3";

pub fn hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

pub fn context_key(context: &str) -> ContextKey {
    hazmat::hash_derive_key_context(context)
}

pub fn derive_key_with(key: &ContextKey, material: &[u8]) -> [u8; 32] {
    *blake3::Hasher::new_from_context_key(key).update(material).finalize().as_bytes()
}

#[derive(Default)]
pub struct Hasher(blake3::Hasher);

impl Hasher {
    pub fn new() -> Hasher {
        Hasher(blake3::Hasher::new())
    }

    pub fn new_derive_key(context: &str) -> Hasher {
        Hasher(blake3::Hasher::new_derive_key(context))
    }

    pub fn new_from_context_key(key: &ContextKey) -> Hasher {
        Hasher(blake3::Hasher::new_from_context_key(key))
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

/// [`ContentId::of_many`]'s way here: a one-chunk input's blocks before its last through
/// `blake3::platform`'s `hash_many`, beside the other inputs with as many (AVX-512 hashes 16 at
/// once, NEON 4). `blake3::platform` is unstable: `of_many_is_the_hasher_at_every_length_and_group_size`
/// holds it to the hasher.
pub struct Many;

/// Inputs `hash_many` takes at once: a multiple of every SIMD degree (16 for AVX-512).
const BATCH: usize = 64;

/// BLAKE3's block flags.
const CHUNK_START: u8 = 1;
const DERIVE_KEY_MATERIAL: u8 = 64;

impl many::Leading for Many {
    fn leading_blocks(key: &[u32; 8], inputs: &[&[u8]], members: &[u32], blocks: usize, cvs: &mut Vec<[u32; 8]>) {
        let platform = Platform::detect();
        macro_rules! by_blocks {
            ($($blocks:literal)*) => {
                match blocks {
                    $($blocks => hash_leading::<{ $blocks * BLOCK_LEN }>(platform, key, inputs, members, cvs),)*
                    _ => unreachable!("a one-chunk input has at most 15 blocks before its last"),
                }
            };
        }
        by_blocks!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15)
    }

    fn long(key: &ContextKey, input: &[u8]) -> [u8; 32] {
        derive_key_with(key, input)
    }
}

fn hash_leading<const N: usize>(
    platform: Platform,
    key: &[u32; 8],
    inputs: &[&[u8]],
    members: &[u32],
    cvs: &mut Vec<[u32; 8]>,
) {
    let mut bytes = [0u8; BATCH * OUT_LEN];
    for batch in members.chunks(BATCH) {
        // Lanes past the batch repeat its last member; only `..batch.len()` is hashed.
        let prefixes: [&[u8; N]; BATCH] = std::array::from_fn(|lane| {
            let at = batch[lane.min(batch.len() - 1)];
            inputs[at as usize][..N].try_into().expect("a member has more than N bytes")
        });
        let out = &mut bytes[..batch.len() * OUT_LEN];
        platform.hash_many(
            &prefixes[..batch.len()],
            key,
            0,
            IncrementCounter::No,
            DERIVE_KEY_MATERIAL,
            CHUNK_START,
            0,
            out,
        );
        cvs.extend(out.chunks_exact(OUT_LEN).map(|cv| {
            platform::words_from_le_bytes_32(cv.try_into().expect("a 32-byte chaining value"))
        }));
    }
}

/// [`ContentId::of_many_into`] by this backend, whichever one blaze builds with.
pub fn of_many_into(domain: &'static str, inputs: &[&[u8]], out: &mut Vec<ContentId>) {
    many::of_many_into::<Many>(crate::context_key(domain), inputs, out);
}
