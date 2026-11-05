//! blaze-fetch: give me the bytes with this hash, and download them only if the store doesn't
//! already have them.
//!
//! ```text
//! get(Source) -> Object        Source::Url { url, digest } | Source::Git { repo, rev, digest }
//! ```
//!
//! - A pin is a [`Digest`], written `<algo>:<hex>`: sha256, sha512 or blake3. md5 and sha1 are
//!   refused by name, as is any algorithm not listed. A git commit id selects a commit and
//!   verifies nothing; a git source's pin is over its tree's canonical listing ([`tree`]).
//! - Bytes live in blaze's store ([`blaze_cas`]) filed by plain BLAKE3, with a side index from
//!   each pin (and, for the fetch graph, each pure step over its inputs) to the object
//!   ([`store`]). The index is store values, so it is persisted, verified on read, and made
//!   durable by the store's own commit.
//! - A download streams into a temporary file, up to a byte limit; the store's side reads it
//!   back a chunk at a time, hashes it in the pin's algorithm and BLAKE3 together, and puts it,
//!   chunk by chunk, only on a match. A mismatch names the URL, the algorithm and both digests
//!   and keeps nothing ([`FetchError::Mismatch`]). Every read of an object is checked against
//!   its BLAKE3.
//! - One download per pin and store at a time ([`flight`]), its followers waiting in a span that names the
//!   pin, with a deadline; tries are bounded by a sans-IO policy ([`decisions`]).
//!
//! Prior art, and what is taken from it:
//!
//! - Nix's fixed-output derivations: pin the output, not the source, so any mirror or method
//!   that makes the same bytes is the same; print `specified:` and `got:` on a mismatch; hash a
//!   tree as a canonical serialization that keeps the executable bit and symlinks, in sorted
//!   order (NAR's recursive mode, here the simpler [`tree`] listing). Unlike Nix, the steps after
//!   the download are proved pure by the compiler ([`graph::Transform`]) instead of trusted to a
//!   sandbox.
//! - Bazel's repository cache: a download is keyed by its hash in a content-addressed directory,
//!   checked before it is moved into place, and skipped when present. Unlike Bazel, which files
//!   by sha256 alone, objects are filed once by BLAKE3 and every algorithm's pin is an index
//!   record onto them.
//! - cargo's `.crate` checksums: the registry's sha256 is the pin, now checked here once.

#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

mod casefold;
pub mod decisions;
pub mod digest;
pub mod downloads;
pub mod error;
pub mod fetcher;
pub mod flight;
pub mod graph;
pub mod progress;
pub mod request;
pub mod store;
pub mod tree;
pub mod unpack;

#[cfg(test)]
mod tests;

pub use crate::{
    decisions::{Failure, Policy},
    digest::{Algo, Blake3, Digest, DigestRefused, Hashed, Hashing},
    error::{FetchError, Producer, redact, scrub},
    fetcher::{
        DEFAULT_MOST_BYTES, Fetcher, Git, Net, Source, StoreOf, Verified, is_commit, verify,
    },
    flight::Flights,
    graph::{StepError, Transform, Tree, TreeFile},
    request::Asked,
    store::{CHUNK_LEN, Form, Made, Object, Objects, Origin, Produced, ensure},
};
