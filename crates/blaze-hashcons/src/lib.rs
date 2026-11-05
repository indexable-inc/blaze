//! blaze v2's hash-consing of query values (docs/v2/plan/design/compile-time.md, change 5;
//! docs/v2/plan/lanes/fastcompile/hashcons.md).
//!
//! - Equal values are stored once: a [`HashCons`] of `T` hands out one [`Interned`] handle (and
//!   one dense [`Idx`]) per distinct value, so comparing two values is comparing two pointers.
//! - A value's [`ContentId`] is computed once, when it is interned ([`Consed::hash_content`]),
//!   and kept beside it: its first 128 bits are the value's [`Fingerprint`], which early cutoff
//!   compares without hashing the value again. A value made of interned parts hashes their
//!   content IDs, not their values, so interning a new composite costs its direct parts only.
//! - Reads take no lock and write nothing shared: an [`Idx`] or a fingerprint to `&T` is acquire
//!   loads and index arithmetic. An insert locks only the shard its fingerprint falls in.
//! - Two unequal values with one content ID, or one fingerprint, stop the process naming the
//!   type ([`HashCons::intern`]): they would alias in every memo that compares fingerprints.
//! - Determinism: an [`Idx`] (and an [`Interned`]'s address) depends on the order threads
//!   interned in, so neither goes into a hash, a key or an output. [`Interned`] hashes as its
//!   fingerprint, has no `Ord`, and persists as its value's bytes; a pack of a whole interner
//!   ([`HashCons::write_pack`]) is sorted by content ID; [`blaze_ids::Hasher`] takes no `Idx`.
//! - Values persist by content and are interned again on load ([`Load`], [`HashCons::load`],
//!   [`HashCons::load_pack`]), each checked against the content ID it was written with.

// No unsafe code of its own; `blaze_format::record!` writes the `unsafe impl Record` of the
// pack's records, allowed on that module alone.
#![deny(unsafe_code)]

mod persist;
mod std_hash;
mod table;

#[cfg(test)]
mod tests;

/// Counts allocations so the read and hit paths' tests hold them to none.
#[cfg(all(test, not(blaze_check)))]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use blaze_ids::{ContentId, Domain, Hasher};

pub use crate::{
    persist::{Load, PACK_SCHEMA},
    std_hash::hash_std,
    table::{HashCons, Interned},
};

/// A value that is hash-consed: fingerprinted once, when it is interned.
///
/// [`Consed::hash_content`] must feed everything [`Eq`] compares, and nothing it ignores:
/// equal values feed equal bytes, unequal values different bytes. An interned part is fed by
/// its content ID ([`Interned::hash_into`]), never by its [`Idx`] or address. A value that
/// breaks this is found when its twin is interned, and the process stops naming its type.
pub trait Consed: Eq + Send + Sync + Sized {
    /// The domain of the type's content IDs, naming it and the version of its content
    /// (`"blaze v2 <what> <n>"`): declared once as a `static`.
    fn domain() -> &'static Domain;
    /// Feeds the value's content to `hasher`, which [`content_of`] made under
    /// [`Consed::domain`].
    fn hash_content(&self, hasher: &mut Hasher);
}

/// The content ID of `value`: what [`HashCons::intern`] computes once per value it is given.
pub fn content_of<T: Consed>(value: &T) -> ContentId {
    let mut hasher = Hasher::of(T::domain());
    value.hash_content(&mut hasher);
    hasher.finish()
}
