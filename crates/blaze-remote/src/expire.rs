//! What expires from the bucket, as a plan. Executing a plan (S3 ListObjectsV2 and
//! DeleteObjects under each prefix) is not built here. The pinned nightly moves daily, so the
//! bucket turns over by toolchain, which the owner accepts.
//!
//! - By toolchain: everything of one toolchain lies under `<ns>/t/<tc>/`. A toolchain's prefix
//!   expires when it is not the current one, is not among the newest `keep` by last use, and
//!   was last used more than `idle_s` ago. The current toolchain is never expired.
//! - By generation, within a toolchain: index shards lie under `index/<generation>/`. A
//!   generation no run of the current root names, older than the root by more than `grace`,
//!   expires. A reader holding the previous root finishes well within `grace` generations.
//!
//! Last use is the last push: the root's `issued`. Readers are anonymous and write nothing, so
//! a read is not a use. A toolchain someone still pulls but CI no longer pushes therefore
//! expires; it costs that reader a local build, never a wrong answer. Segments are never
//! expired one by one, only with their toolchain: nothing is rewritten in place
//! (decision 17).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_cas::plain::Namespace;

use crate::{
    name::{self, BadName, Toolchain},
    shard::Root,
};

/// A toolchain seen in a namespace, with its root's `issued`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Seen {
    pub toolchain: Toolchain,
    pub last_push: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Policy {
    /// Toolchains kept by last use, whatever their age.
    pub keep: usize,
    /// Idle time past which a toolchain not kept expires.
    pub idle_s: u64,
    /// Generations an unnamed index generation is kept.
    pub grace: u64,
}

/// The default: the newest 7 toolchains (a week of nightlies), others after 14 days idle,
/// index generations after 64.
pub const POLICY: Policy = Policy { keep: 7, idle_s: 14 * 86_400, grace: 64 };

/// The prefixes of toolchains to delete, sorted.
pub fn toolchains(namespace: Namespace, current: &Toolchain, seen: &[Seen], now_s: u64, policy: Policy) -> Result<Vec<String>, BadName> {
    let mut by_use: Vec<&Seen> = seen.iter().collect();
    by_use.sort_by(|a, b| b.last_push.cmp(&a.last_push).then(a.toolchain.cmp(&b.toolchain)));
    let mut out = Vec::new();
    for (rank, it) in by_use.into_iter().enumerate() {
        if it.toolchain == *current || rank < policy.keep || now_s.saturating_sub(it.last_push) <= policy.idle_s {
            continue;
        }
        out.push(name::toolchain_prefix(namespace, &it.toolchain)?);
    }
    out.sort();
    Ok(out)
}

/// The index generations under `root`'s toolchain to delete: listed, not named by a run, and
/// older than the root by more than the grace.
pub fn generations(root: &Root, listed: &[u64], policy: Policy) -> Vec<u64> {
    let mut out: Vec<u64> = listed
        .iter()
        .copied()
        .filter(|g| !root.runs.iter().any(|run| run.generation == *g) && root.generation.saturating_sub(*g) > policy.grace)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}
