//! The `resolve` of `cargo metadata`, against cargo 1.99.0's for the same workspace
//! (`metadata/metadata.sh` writes it).

// The fixtures are cargo's output for a workspace at `/ws` on Linux: `path+file:///ws` ids,
// which a Windows path is not, and `/ws` is no absolute path there.
#![cfg(unix)]

#[path = "resolver/helpers.rs"]
mod helpers;

use std::collections::BTreeMap;
use std::path::Path;

use blaze_resolve::metadata::{MetadataPackages, resolve_graph};
use blaze_resolve::resolver::{CompileKind, ResolveOpts, VersionOrdering};
use blaze_resolve::{
    Cfg, DepKind, Dependency, InternedString, PackageId, Platform, SourceId, Summary,
};

use helpers::resolve_summaries;

fn source(name: &str) -> SourceId {
    let path = if name == "foo" { "/ws".to_owned() } else { format!("/ws/{name}") };
    SourceId::for_path(Path::new(&path)).unwrap()
}

fn package(name: &str, version: &str, deps: Vec<Dependency>, features: &[&str]) -> Summary {
    let id = PackageId::try_new(name, version, source(name)).unwrap();
    let features: BTreeMap<InternedString, Vec<InternedString>> =
        features.iter().map(|f| (InternedString::new(f), vec![])).collect();
    Summary::new(id, deps, &features, None::<&String>, None).unwrap()
}

fn path_dep(name: &str) -> Dependency {
    Dependency::parse(name, None, source(name)).unwrap()
}

struct Packages;

impl MetadataPackages for Packages {
    fn package_id(&self, id: PackageId) -> PackageId {
        id
    }

    fn lib_crate_name(&self, id: PackageId) -> Option<InternedString> {
        let name =
            if id.name() == "bar" { "bar_lib".to_owned() } else { id.name().replace('-', "_") };
        Some(InternedString::new(&name))
    }

    fn platform_activated(&self, platform: &Platform, kind: CompileKind) -> bool {
        let (triple, cfg): (&str, Vec<Cfg>) = match kind {
            CompileKind::Host => ("aarch64-apple-darwin", vec!["unix".parse().unwrap()]),
            CompileKind::Target(triple) => (triple.as_str(), vec!["windows".parse().unwrap()]),
        };
        platform.matches(triple, &cfg)
    }
}

fn metadata(requested_kinds: &[CompileKind]) -> String {
    let mut renamed = path_dep("bar");
    renamed.set_explicit_name_in_toml("baz-x");
    let mut common = path_dep("common");
    common.set_features(["f1"]);
    let mut common_dev = path_dep("common");
    common_dev.set_kind(DepKind::Development);
    let mut cc = path_dep("cc");
    cc.set_kind(DepKind::Build);
    let mut unixy = path_dep("unixy");
    unixy.set_platform(Some("cfg(unix)".parse().unwrap()));
    let root = package("foo", "0.1.0", vec![renamed, common, common_dev, cc, unixy], &[]);
    let registry = vec![
        package("bar", "0.2.0", vec![], &[]),
        package("common", "1.0.0", vec![], &["f1", "f2"]),
        package("cc", "1.0.0", vec![], &[]),
        package("unixy", "1.0.0", vec![], &[]),
    ];
    let root_id = root.package_id();
    let resolve = resolve_summaries(
        &[(root, ResolveOpts::everything())],
        &registry,
        VersionOrdering::MaximumVersionsFirst,
    )
    .unwrap();
    let graph =
        resolve_graph(&resolve, &[root_id], Some(root_id), &Packages, requested_kinds).unwrap();
    serde_json::to_string(&graph).unwrap()
}

#[test]
fn host() {
    assert_eq!(metadata(&[CompileKind::Host]), include_str!("metadata/host.json"));
}

#[test]
fn filter_platform() {
    let windows = CompileKind::Target(InternedString::new("x86_64-pc-windows-msvc"));
    assert_eq!(metadata(&[windows]), include_str!("metadata/windows.json"));
}
