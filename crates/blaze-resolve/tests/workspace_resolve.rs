//! Resolving a workspace as cargo does around its resolver: the resolve `Cargo.lock` is
//! written from, then the one asked for, with `[patch]` and the locks of the previous resolve.
//! The `patched` workspace's fixtures are cargo 1.99.0's (`workspace_resolve/patched.sh`
//! writes them).

// The fixtures are cargo's output for a workspace at `/ws` on Linux: `path+file:///ws` ids,
// which a Windows path is not, and `/ws` is no absolute path there.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::Path;

use blaze_resolve::lockfile::{self, PathDeps};
use blaze_resolve::metadata::{MetadataPackages, resolve_graph};
use blaze_resolve::resolver::{CliFeatures, CompileKind, HasDevUnits, Members, VersionOrdering};
use blaze_resolve::workspace_resolve::{PackageRegistry, Patch, Workspace, resolve_ws};
use blaze_resolve::{
    Dependency, InternedString, MemRegistry, PackageId, PackageIdSpec, Platform, Resolve,
    ResolveVersion, SourceId, Summary,
};
use rustc_hash::FxHashMap;
use url::Url;

fn path_source(dir: &str) -> SourceId {
    SourceId::for_path(Path::new(dir)).unwrap()
}

fn summary(name: &str, version: &str, source: SourceId, deps: Vec<Dependency>) -> Summary {
    summary_with_features(name, version, source, deps, &[])
}

fn summary_with_features(
    name: &str,
    version: &str,
    source: SourceId,
    deps: Vec<Dependency>,
    features: &[&str],
) -> Summary {
    let id = PackageId::try_new(name, version, source).unwrap();
    let features: BTreeMap<InternedString, Vec<InternedString>> =
        features.iter().map(|f| (InternedString::new(f), vec![])).collect();
    Summary::new(id, deps, &features, None::<&String>, None).unwrap()
}

struct Packages;

impl MetadataPackages for Packages {
    fn package_id(&self, id: PackageId) -> PackageId {
        id
    }

    fn lib_crate_name(&self, id: PackageId) -> Option<InternedString> {
        Some(InternedString::new(&id.name().replace('-', "_")))
    }

    fn platform_activated(&self, _platform: &Platform, _kind: CompileKind) -> bool {
        true
    }
}

/// Resolves `members` as `cargo metadata` does, from `lock` if given, and returns the
/// resolve asked for.
fn resolve(
    members: &[&Summary],
    packages: &[Summary],
    patch: &[(Url, Vec<Patch>)],
    lock: Option<&str>,
) -> Resolve {
    let path_packages: FxHashMap<SourceId, Summary> = members
        .iter()
        .copied()
        .chain(packages)
        .filter(|s| s.source_id().is_path())
        .map(|s| (s.source_id(), s.clone()))
        .collect();
    let lock = lock.map(|text| {
        let mut path_deps = PathDeps::default();
        for s in path_packages.values() {
            path_deps
                .entry(s.name().to_string())
                .or_default()
                .insert(s.version().clone(), s.source_id());
        }
        lockfile::into_resolve(lockfile::parse(text).unwrap(), text, &path_deps).unwrap()
    });
    let ws = Workspace {
        members: Members { members, current: None, new_cli_feature_behavior: true },
        path_packages: &path_packages,
        patch,
        replace: &[],
        require_optional_deps: true,
        version_ordering: VersionOrdering::MaximumVersionsFirst,
        rust_versions: None,
        resolve_version: ResolveVersion::default(),
    };
    let registry: MemRegistry = members.iter().copied().chain(packages).cloned().collect();
    let specs: Vec<PackageIdSpec> = members.iter().map(|m| m.package_id().to_spec()).collect();
    let (_, resolve) = resolve_ws(
        &mut PackageRegistry::new(registry),
        &ws,
        lock.as_ref(),
        &CliFeatures::new_all(false),
        &specs,
        HasDevUnits::Yes,
    )
    .unwrap();
    resolve
}

/// A member patched in for a crates.io package another member depends on, with a feature only
/// that dependency enables.
fn patched(lock: Option<&str>) -> String {
    let t_source = path_source("/ws/t");
    let b = summary("b", "0.1.0", path_source("/ws/b"), vec![]);
    let b_dep = Dependency::parse("b", None, b.source_id()).unwrap();
    let t = summary_with_features("zz-blaze-t", "1.0.0", t_source, vec![b_dep], &["f"]);
    let mut t_dep = Dependency::parse("zz-blaze-t", Some("1"), SourceId::crates_io()).unwrap();
    t_dep.set_features(["f"]);
    let tt = summary("tt", "0.1.0", path_source("/ws/tt"), vec![t_dep]);
    let patch = vec![(
        SourceId::crates_io().url().clone(),
        vec![Patch {
            dep: Dependency::parse("zz-blaze-t", None, t_source).unwrap(),
            loc: "/ws/Cargo.toml".to_owned(),
        }],
    )];
    let members = [&t, &tt];
    let resolve = resolve(&members, &[b], &patch, lock);
    let ids: Vec<PackageId> = members.iter().map(|m| m.package_id()).collect();
    let graph = resolve_graph(&resolve, &ids, None, &Packages, &[CompileKind::Host]).unwrap();
    serde_json::to_string(&graph).unwrap()
}

#[test]
fn patched_member_without_lock_file() {
    // The patch is locked in the first resolve, when nothing is: the second, guided by the
    // first, reaches `zz-blaze-t` both as a member and as the patch, with two dependencies on
    // `b` that differ in being locked, and cargo lists both.
    assert_eq!(patched(None), include_str!("workspace_resolve/fresh.json"));
}

#[test]
fn patched_member_with_lock_file() {
    let lock = include_str!("workspace_resolve/patched.lock");
    assert_eq!(patched(Some(lock)), include_str!("workspace_resolve/locked.json"));
}

/// A member on `log = "0.4"`, with 0.4.1 and 0.4.2 in the registry.
fn log_version(lock: Option<&str>) -> String {
    let log = |v| summary("log", v, SourceId::crates_io(), vec![]);
    let dep = Dependency::parse("log", Some("0.4"), SourceId::crates_io()).unwrap();
    let member = summary("m", "0.1.0", path_source("/ws"), vec![dep]);
    let resolve = resolve(&[&member], &[log("0.4.1"), log("0.4.2")], &[], lock);
    let log = resolve.iter().find(|id| id.name() == "log").unwrap();
    log.version().to_string()
}

#[test]
fn lock_file_keeps_its_version() {
    let lock = r#"version = 4

[[package]]
name = "log"
version = "0.4.1"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "m"
version = "0.1.0"
dependencies = [
 "log",
]
"#;
    assert_eq!(log_version(None), "0.4.2");
    assert_eq!(log_version(Some(lock)), "0.4.1");
}
