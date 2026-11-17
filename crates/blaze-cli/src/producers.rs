//! The producers' table, made once at the entry (decision D105; docs/v2/plan/design/producer-id.md).
//!
//! Each source row is the digest of its producer's crates as this binary was built
//! (build/producers.rs writes `$OUT_DIR/producer_sources.rs`) with the build setting that changes
//! what Rust code computes mixed in; `Row::Whole` is the binary's linker id. This is the one seam
//! that changes when blaze builds blaze: the row becomes the code identity of the producer's
//! entry function by reflection (`fn_info(f).code_id`), the include below goes, and nothing else
//! moves.

use blaze_ids::{
    Hasher,
    producer::{Producers, Row, Sources, SourcesRefused},
};

/// The build's digest of each producer's source, `None` where the build step could not say it
/// (it warned at build time naming the crate and the row; the generated file says why).
const SOURCES: [(&str, Option<[u8; 32]>); Row::FROM_SOURCE.len()] =
    include!(concat!(env!("OUT_DIR"), "/producer_sources.rs"));

const BITS_DOMAIN: &str = "blaze-cli producer build bits 1";

/// The table, from the running binary's linker id (bstd `Host::build_id`) and the build's
/// sources. A refusal is a build whose step and `blaze_ids` disagree on the rows.
///
/// Each source row also holds whether this binary was built with debug assertions: with them,
/// an overflow or a failed `debug_assert!` stops a producer where a build without them goes on
/// with another value, so the two must not share records (remote-read.md, "the two build
/// settings"). Overflow checks follow debug assertions under the workspace's profiles (the root
/// manifest sets neither apart), so the one bit stands for both.
///
/// A row the step could not digest (a lock of another moment than the sources, as a snapshot mix
/// builds) is keyed by the build id instead: piece 1's key, which every rebuilt blaze moves, so
/// that row's records are made again and never read stale; the other rows keep theirs.
pub fn table(build_id: &[u8]) -> Result<Producers, SourcesRefused> {
    let bits = [u8::from(cfg!(debug_assertions))];
    let rows = SOURCES.map(|(name, digest)| {
        let mut hasher = Hasher::new(BITS_DOMAIN);
        match digest {
            Some(digest) => hasher.part(b"source").part(&digest),
            None => hasher.part(b"build id").part(build_id),
        };
        hasher.part(&bits);
        (name, hasher.finish().0)
    });
    Producers::of_sources(build_id, &Sources { rows })
}
