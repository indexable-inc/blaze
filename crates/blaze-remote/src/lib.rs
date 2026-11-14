//! The shared cache. Everything CI builds goes to an object bucket (Cloudflare R2, spoken to as
//! S3), and anyone can pull it, the public namespace without a login. Remote workers can later
//! fill the same bucket the same way (target decisions 9 and 12-17 of 2026-11-13).
//!
//! The plain store's segment is the wire format: a segment file is an object, unchanged, read
//! and written only by the store's own layout code (`blaze_cas::plain::check_segment` and
//! `seal_segment`; [`segment`] holds no second reader). The vocabulary around it (the segment
//! reference, its length, the statement, the namespace, [`Source`](blaze_keyed::store::Source)
//! and [`Verifier`](blaze_keyed::store::Verifier)) is blaze-keyed's store, declared once. The
//! bucket adds four things around segments: a signed statement per segment, index shards that
//! find a key among very many segments, a signed root per namespace and toolchain, and bundles
//! that name the segments to fetch before a first build.
//!
//! # Layout
//!
//! Every name is built here from validated parts ([`name`]); nothing read from the bucket or
//! the network becomes a name except a digest's hex. `<ns>` is `public`, or the 16 hex digits
//! of an organisation's namespace; [`Namespace::LOCAL`](blaze_cas::plain::Namespace::LOCAL)
//! has no name and never leaves the machine. `<tc>` is a toolchain identity
//! (`[0-9a-z._-]{1,64}`, e.g. `nightly-2026-10-27-b3f1c2a`).
//!
//! - `<ns>/keys/<serial:016x>`: a key list, signed by an anchor key ([`trust`]). Immutable.
//! - `<ns>/t/<tc>/segments/<digest>`: a plain segment, byte for byte. Immutable.
//! - `<ns>/t/<tc>/segments/<digest>.statement`: its signed statement, 200 bytes. Immutable.
//! - `<ns>/t/<tc>/index/<generation:016x>/<shard digest>`: an index shard ([`shard`]).
//!   Immutable.
//! - `<ns>/t/<tc>/roots/<name>`: the root, the one object overwritten by compare-and-swap.
//! - `<ns>/t/<tc>/bundles/<target>/<cfg>`: a bundle manifest, overwritten by its pusher; latest
//!   wins, and every version is signed and names only immutable segments.
//!
//! The root is replaced with a conditional PutObject: `If-Match: <etag>`, or
//! `If-None-Match: *` for the first root. That R2 honours both on PutObject is a guess, not
//! measured. Where a bucket does not (`"cas": false`), the root is put unconditionally and then
//! read back. A read-back that is not ours fails as [`Failure::LostRace`]. Two pushers can then
//! still lose one run between them, never silently: the loser fails, and its segments wait
//! unindexed for the next push.
//!
//! # Counts
//!
//! A shard holds at most 65,536 entries of 128 bytes in blocks of 256 (32 KiB), under a head of
//! 32 + 64 bytes per block (at most 16,416 bytes). The root lists each shard (72 bytes). There
//! are at most [`shard::RUNS_KEPT`] runs, merged geometrically.
//!
//! "Which of N keys exist" in a namespace whose index has R runs costs:
//! - Warm, with the key list, shard heads and blocks cached: 1 round trip (the root) when every
//!   block is cached, otherwise 2. The requests are 1 + B, where B is the number of contiguous
//!   block ranges touched (at most min(N, blocks) per run). The bytes are the root plus
//!   32 KiB × blocks touched.
//! - Cold: 4 round trips (root, key list, shard heads, blocks). The requests are
//!   2 + H + B, where H is the number of shards touched (at most min(N, shards) per run). The
//!   bytes are the root + 56 bytes per listed key + 16 KiB × H + 32 KiB × blocks touched.
//!
//! A bundle of S segments costs 3 round trips: the manifest, then the key list it names (left
//! out when this process holds that list or a newer one), then every segment and statement, at
//! most [`pull::IN_FLIGHT`] segments at once (so ⌈S/8⌉ waves). That is 2 + 2S requests: the
//! manifest (232 + 56·S + 96 bytes), the key list (144 + 56 bytes per key), S statements of
//! 200 bytes and the S segments whole.
//!
//! # Verify once
//!
//! Decision 18: each check has one place and a proof. A segment is checked once, either by the
//! plain store as it pulls from [`pull::Remote`] (its statement by [`Trusted::verifier`] of the
//! namespace pulled, its bytes by the store) or by [`Trusted::prove`], which alone makes a
//! [`pull::Proven`]. A shard head or block is checked once per client and cached by digest. A
//! root is checked once per digest of its bytes. A key list is checked once a process. Counters
//! ([`trust::checks`]) count every signature and digest checked, and every second check of one
//! object; the simulation requires zero second checks.
//!
//! # Trust
//!
//! [`trust`]: Ed25519, implemented by `ring` 0.17. A segment's digest covers every byte of it
//! (header, payloads, index), so a statement that binds (namespace, toolchain, tier, segment
//! digest, length, record count) to a builder binds every key in the segment to its value's
//! digest. Each namespace trusts the anchor keys in its settings. Anchors sign key lists, and
//! key lists name builder keys with roles, validity and revocation. Roots, bundles and
//! statements are signed by builders. Nothing is mapped or handed on before its signature and
//! digest are checked. Everything decoded has a size limit, checked before it is read.
//!
//! # Not a fallback
//!
//! An unreachable bucket means the build builds locally, and says so once. This is the design's
//! one path, not a second route taken quietly: the cache is a cache of what a local build makes
//! byte for byte (rule deterministic-output), and the local build is always the definition. A
//! miss and an unreachable bucket both end in the same local build, so nothing about the output
//! changes. What is never done is the forbidden kind of fallback: a refused statement, a forged
//! byte or a failed push is never answered by trying elsewhere or again. It is a typed error
//! naming the object and the key.
//!
//! # Local only
//!
//! With no `remote.json`, or with `"enabled": false`, nothing here contacts anything
//! ([`settings::Settings::contacts`] is `None`, and a test checks that the fake bucket saw zero
//! requests). An organisation's namespace is opt-in, by naming it.

#![deny(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
// `#![blaze::sans_io]` in each core module; inert under rustc.
#![register_tool(blaze)]

pub mod bucket;
pub mod bundle;
pub mod cmd;
pub mod exec;
pub mod expire;
pub mod find;
pub mod http;
pub mod name;
pub mod privacy;
pub mod pull;
pub mod push;
pub mod s3;
pub mod segment;
pub mod settings;
pub mod shard;
pub mod trust;
mod wire;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

pub use bucket::{Bucket, Failure, Machine, Step, drive};
pub use trust::{PublicKey, Refusal, Signer, Trusted, Why};
pub use wire::Bounded;

/// A bug planted by a test, to show the oracle catches it: only ever in this crate's tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// A fetched segment is handed on with only its header and footer read (the store's
    /// `describe`), not the store's whole check (`check_segment`): its digest unchecked.
    SkipSegmentDigest,
    /// A statement is taken without checking its signature.
    SkipSignature,
    /// A statement of another namespace is taken.
    SkipNamespace,
    /// The root is put unconditionally even when compare-and-swap is on.
    IgnoreCas,
    /// The root is put before the index shards it names.
    RootBeforeShards,
    /// A segment of an organisation's namespace is pushed to public.
    SkipPrivacy,
}

#[cfg(test)]
thread_local! {
    pub(crate) static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

/// Whether `plant` is planted: only ever in this crate's tests.
#[cfg(test)]
pub(crate) fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

/// Whether `plant` is planted: never, outside tests.
#[cfg(not(test))]
#[inline(always)]
pub(crate) fn planted(_plant: Plant) -> bool {
    false
}
