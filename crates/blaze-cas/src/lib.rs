//! blaze's store: the plain store ([`plain`]), content-keyed records in a few immutable segment
//! files under a two-slot head, read in place with no lock; and the [`Key`] a value is kept
//! under, the BLAKE3 hash of exactly what it was made from (the source, cfg, features, target,
//! toolchain, the layout facts read, the schema and code of what made it). A key that misses an
//! input is a bug.
//!
//! The content-addressed store of packs that was here (its index, pool, pump, collector and
//! remote tier) is deleted: nothing live reached it once the plain store was the only form a
//! root opens.

#![feature(allocator_api)]
// `#![blaze::sans_io]` marks a sans-IO core for `blaze::sans_io_core` (the plain store's core,
// format, probe, image and proofs).
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

pub mod plain;

use std::fmt;

use blaze_core::FileName;
use blaze_format::SchemaHash;
use blaze_ids::{ContentId, Hasher};

/// What a value was made from, hashed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Key(pub ContentId);

impl Key {
    /// The key of what `producer` makes from `inputs` (#1439): `schema` is the schema of the
    /// value made, `producer` the content ID of the code that made it (the code in its own
    /// dependency closure, never a hash of all of blaze's sources), `inputs` the content ID of
    /// every input it read, in the order it read them. An input left out is a bug: the value
    /// would be reused for inputs it was not made from.
    pub fn of(schema: SchemaHash, producer: ContentId, inputs: &[ContentId]) -> Key {
        let mut hasher = Hasher::new("blaze v2 store key 1");
        hasher.id(&schema.0).id(&producer).update(&(inputs.len() as u64).to_le_bytes());
        for input in inputs {
            hasher.id(input);
        }
        Key(hasher.finish())
    }
}

/// A value whose bytes are not what the store recorded.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Corrupt {
    pub key: Key,
    pub pack: FileName,
    pub what: String,
}

impl fmt::Display for Corrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the value of {:?} in {} is corrupt: {}", self.key.0, self.pack, self.what)
    }
}
