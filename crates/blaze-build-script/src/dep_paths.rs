//! What a script reading another package's `links` metadata (`DEP_<LINKS>_<KEY>`) sees of the
//! paths in it (decision D96 (5)). A producing script prints paths of ITS sandbox: under its
//! `OUT_DIR` (`cargo::metadata=include=/target/debug/build/zstd-sys-<config>/out/include`), or
//! under the tree it was served (`/package/zstd/lib` for a dependency, `/workspace/<dir>/..`
//! for a member). The reading script's sandbox serves neither by itself, and its own
//! `/package` is ANOTHER tree, so the value cannot be passed on as text alone.
//!
//! The plan, one result for anyone:
//! - a path under the producer's `OUT_DIR` is passed as written, and the reader's sandbox mounts
//!   the producer's `OUT_DIR` files (its outputs record) read-only at that same path
//!   ([`DepMount::Out`]): an `OUT_DIR` is `/target/debug/build/<package>-<config>/out`, and no
//!   two packages of one graph share a `links` value (the cargo book, "links": "only one
//!   package may link to a given native library"), so it never meets the reader's own;
//! - a path under the tree the producer was served is written under
//!   [`DEPS_ROOT`]`/<links>` (`/deps/zstd/zstd/lib`), and the reader's sandbox mounts that tree
//!   read-only there ([`DepMount::Tree`]): the producer's mount point is the reader's own;
//! - any other absolute path names the machine (`/usr/include` from pkg-config): refused by
//!   name ([`DepPathError`]), never passed on (rule 3);
//! - a value not starting with `/` (`fips`, `30000000`, `include`) is passed as written.
//!
//! A value joined by `:` with each part absolute (`std::env::join_paths` on Unix:
//! `/a/include:/b/include`) is taken part by part. Nothing else is split.
//!
//! The reading script's record key reads, per producer it mounts (the caller's reads list):
//! its `links`, its outputs record's id (which names every metadata pair, the `OUT_DIR` text
//! and every file in it), and for a [`DepMount::Tree`] the served tree's digest. The rewritten
//! values are a function of those, so nothing else enters the key.
#![blaze::sans_io]

use core::fmt;

use blaze_expand::seen::SeenRoot;
use blaze_own::{AString, AVec, Alloc};

use crate::env::DepMetadata;

/// Where a reading script finds the tree a producer was served: `/deps/<links>`.
pub const DEPS_ROOT: &str = "/deps";

/// Where one consuming run sees a producer's two trees (its `OUT_DIR` tree and the tree it was
/// served): a fact of THAT run, never of a record (D112, brief N8). A value keeps its place
/// inside its tree; only where the tree is seen differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sight<'r> {
    /// A wasm sandbox: the `OUT_DIR` tree at the path its record names, the served tree at
    /// [`DEPS_ROOT`]`/<links>` (mounted read-only).
    Wasm,
    /// A native run: both trees laid as real folders under `root`, the run's own:
    /// `<root>/<links>/out` and `<root>/<links>/package` (granted read-only).
    Native { root: &'r str },
}

impl Sight<'_> {
    /// Writes where this run sees the tree `mount` names (no trailing `/`).
    pub fn seen_at(&self, mount: DepMount<'_>, out: &mut AString<'_>) {
        match (*self, mount) {
            (Sight::Wasm, DepMount::Out { at, .. }) => out.push_str(at.trim_end_matches('/')),
            (Sight::Wasm, DepMount::Tree { links }) => {
                out.push_str(DEPS_ROOT);
                out.push('/');
                out.push_str(links);
            }
            (Sight::Native { root }, DepMount::Out { links, .. }) => {
                out.push_str(root.trim_end_matches('/'));
                out.push('/');
                out.push_str(links);
                out.push_str("/out");
            }
            (Sight::Native { root }, DepMount::Tree { links }) => {
                out.push_str(root.trim_end_matches('/'));
                out.push('/');
                out.push_str(links);
                out.push_str("/package");
            }
        }
    }
}

/// One producer of `links` metadata, as the reading script's plan needs it.
#[derive(Clone, Copy, Debug)]
pub struct Producer<'s> {
    /// Its package's `links` value.
    pub links: &'s str,
    /// What its script saw as its package's root (D96): its served tree's mount.
    pub seen: SeenRoot<'s>,
    /// Its `OUT_DIR` as its outputs record holds it.
    pub out_dir: &'s str,
}

/// A read-only mount the reading script's sandbox adds for one producer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DepMount<'s> {
    /// The producer's `OUT_DIR` files at `at`, the path they were made at.
    Out { links: &'s str, at: &'s str },
    /// The tree the producer was served, at [`DEPS_ROOT`]`/<links>`.
    Tree { links: &'s str },
}

/// A metadata value naming a path the reading script's sandbox cannot serve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepPathError<'s> {
    /// An absolute path under neither the producer's `OUT_DIR` nor its served tree.
    Outside { links: &'s str, key: &'s str, path: &'s str },
    /// A pair whose `links` names no producer given.
    NoProducer { links: &'s str, key: &'s str },
    /// A path that climbs out (`..`) of the tree it starts in.
    Climbs { links: &'s str, key: &'s str, path: &'s str },
}

impl fmt::Display for DepPathError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DepPathError::Outside { links, key, path } => write!(
                f,
                "DEP_{links}_{key} names `{path}`, outside the `links = \"{links}\"` package's OUT_DIR and its tree: a path of the machine is never given to a build script (decision D96 (5))"
            ),
            DepPathError::NoProducer { links, key } => {
                write!(f, "DEP_{links}_{key}: no package with `links = \"{links}\"` was given to plan its mounts (decision D96 (5))")
            }
            DepPathError::Climbs { links, key, path } => {
                write!(f, "DEP_{links}_{key} names `{path}`, which climbs (`..`) out of the tree it is under (decision D96 (5))")
            }
        }
    }
}

impl core::error::Error for DepPathError<'_> {}

/// What the reading script is given: each pair's value as it sees it (in the order of `deps`)
/// and the mounts its sandbox adds, sorted, each once.
#[derive(Debug)]
pub struct DepPlan<'s, 'a> {
    pub values: AVec<'a, AString<'a>>,
    pub mounts: AVec<'a, DepMount<'s>>,
}

/// Plans `deps` (the reading script's metadata pairs) against `producers` for a wasm run: see
/// the module's doc.
pub fn plan<'s, 'a>(producers: &[Producer<'s>], deps: &[DepMetadata<'s>], alloc: Alloc<'a>) -> Result<DepPlan<'s, 'a>, DepPathError<'s>> {
    plan_seen(Sight::Wasm, producers, deps, alloc)
}

/// Plans `deps` against `producers` for a run that sees the producers' trees by `sight`: THE
/// one resolution of a `links` metadata value (D112, brief N8); [`plan`] is its wasm form. The
/// mounts are the same for every sight; only the values' text differs.
pub fn plan_seen<'s, 'a>(sight: Sight<'_>, producers: &[Producer<'s>], deps: &[DepMetadata<'s>], alloc: Alloc<'a>) -> Result<DepPlan<'s, 'a>, DepPathError<'s>> {
    let mut values: AVec<'a, AString<'a>> = AVec::with_capacity_in(deps.len(), alloc);
    let mut mounts: AVec<'a, DepMount<'s>> = AVec::new_in(alloc);
    for dep in deps {
        let producer = producers
            .iter()
            .find(|p| p.links == dep.links)
            .ok_or(DepPathError::NoProducer { links: dep.links, key: dep.key })?;
        let mut out = AString::new_in(alloc);
        if !dep.value.starts_with('/') {
            out.push_str(dep.value);
        } else {
            let joined = dep.value.split(':').all(|part| part.starts_with('/'));
            let mut first = true;
            for part in dep.value.split(':') {
                // A `:` inside one absolute path (`/a:b`) is not a join: the whole is one path.
                let part = if joined { part } else { dep.value };
                if !first {
                    out.push(':');
                }
                first = false;
                mounts.push(one_path(sight, producer, dep, part, &mut out)?);
                if !joined {
                    break;
                }
            }
        }
        values.push(out);
    }
    mounts.sort_unstable();
    mounts.dedup();
    Ok(DepPlan { values, mounts })
}

/// Writes the one absolute `path` of `dep` as a run of `sight` sees it and says what serves
/// it: its tree, and its place in that tree kept as written.
fn one_path<'s>(sight: Sight<'_>, producer: &Producer<'s>, dep: &DepMetadata<'s>, path: &'s str, out: &mut AString<'_>) -> Result<DepMount<'s>, DepPathError<'s>> {
    let climbs = |rest: &str| rest.split('/').any(|part| part == "..");
    let (mount, rest) = if let Some(rest) = under(path, producer.out_dir) {
        (DepMount::Out { links: producer.links, at: producer.out_dir }, rest)
    } else if let Some(rest) = under(path, producer.seen.mount()) {
        (DepMount::Tree { links: producer.links }, rest)
    } else {
        return Err(DepPathError::Outside { links: dep.links, key: dep.key, path });
    };
    if climbs(rest) {
        return Err(DepPathError::Climbs { links: dep.links, key: dep.key, path });
    }
    sight.seen_at(mount, out);
    if !rest.is_empty() {
        out.push('/');
        out.push_str(rest);
    }
    Ok(mount)
}

/// The part of `path` under the folder `root` (`""` for `root` itself), or `None`.
fn under<'w>(path: &'w str, root: &str) -> Option<&'w str> {
    let root = root.trim_end_matches('/');
    let rest = path.strip_prefix(root)?;
    if rest.is_empty() {
        return Some(rest);
    }
    rest.strip_prefix('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZSTD_OUT: &str = "/target/debug/build/zstd-sys-0123456789abcdef/out";

    fn texts<'v>(plan: &'v DepPlan<'_, '_>) -> std::vec::Vec<&'v str> {
        plan.values.iter().map(|v| v.as_str()).collect()
    }

    /// The cargo book, "The links Manifest Key": "the values [of `cargo::metadata`] are passed
    /// to the build scripts of dependent packages as `DEP_<LINKS>_<KEY>`". A path under the
    /// producer's OUT_DIR is passed as written and that OUT_DIR is mounted at it; a path under
    /// its served tree (`/package` for a dependency) moves to `/deps/<links>`, since the
    /// reader's own `/package` is another tree; a plain value is passed as written; a path of
    /// the machine is refused by name. Expected line:
    /// `test dep_paths::tests::a_dependency_s_paths_are_served_where_the_reader_looks ... ok`.
    #[test]
    fn a_dependency_s_paths_are_served_where_the_reader_looks() {
        let alloc = blaze_own::testing::heap().alloc();
        let producers = [Producer { links: "zstd", seen: SeenRoot::Package, out_dir: ZSTD_OUT }];
        let include = std::format!("{ZSTD_OUT}/include");
        let deps = [
            DepMetadata { links: "zstd", key: "include", value: include.as_str() },
            DepMetadata { links: "zstd", key: "root", value: "/package/zstd/lib" },
            DepMetadata { links: "zstd", key: "conf", value: "fips" },
        ];
        let plan = plan(&producers, &deps, alloc).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(texts(&plan), [include.as_str(), "/deps/zstd/zstd/lib", "fips"]);
        assert_eq!(plan.mounts[..], [DepMount::Out { links: "zstd", at: ZSTD_OUT }, DepMount::Tree { links: "zstd" }]);

        let system = [DepMetadata { links: "zstd", key: "include", value: "/usr/include" }];
        assert_eq!(
            super::plan(&producers, &system, alloc).map(|_| ()),
            Err(DepPathError::Outside { links: "zstd", key: "include", path: "/usr/include" })
        );
        let climbing = [DepMetadata { links: "zstd", key: "root", value: "/package/../etc" }];
        assert!(matches!(super::plan(&producers, &climbing, alloc), Err(DepPathError::Climbs { .. })));
    }

    /// A member producer was served its workspace at `/workspace`: its `/workspace/crates/sys/x`
    /// is `/deps/<links>/crates/sys/x` to the reader (the whole served tree is mounted, so the
    /// member's dir stays in the path). A `join_paths` value is taken part by part, and one
    /// mount is listed once. Expected line:
    /// `test dep_paths::tests::a_member_producer_s_tree_and_joined_paths ... ok`.
    #[test]
    fn a_member_producer_s_tree_and_joined_paths() {
        let alloc = blaze_own::testing::heap().alloc();
        let seen = SeenRoot::member("crates/sys").unwrap_or_else(|e| panic!("{e}"));
        let producers = [Producer { links: "sys", seen, out_dir: "/target/debug/build/sys-00/out" }];
        let deps = [DepMetadata { links: "sys", key: "include", value: "/workspace/crates/sys/a:/workspace/crates/sys/b" }];
        let plan = plan(&producers, &deps, alloc).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(texts(&plan), ["/deps/sys/crates/sys/a:/deps/sys/crates/sys/b"]);
        assert_eq!(plan.mounts[..], [DepMount::Tree { links: "sys" }]);
        // A dependency's `/package` is not a member's tree.
        let wrong = [DepMetadata { links: "sys", key: "root", value: "/package/x" }];
        assert!(matches!(super::plan(&producers, &wrong, alloc), Err(DepPathError::Outside { .. })));
    }
}
