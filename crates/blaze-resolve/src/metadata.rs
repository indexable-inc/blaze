//! The `resolve` of `cargo metadata`: its nodes, their dependencies and features, as cargo
//! 1.99.0 (5f94df4789) writes them (`src/ops/cargo_metadata.rs`). cargo's unstable artifact
//! dependencies (`-Zbindeps`) and the fields they add are not ported.

use blaze_hash::{FixedState, HashMap};
use cargo_platform::Platform;
use serde::Serialize;

use crate::dependency::DepKind;
use crate::interning::InternedString;
use crate::package_id::PackageId;
use crate::package_id_spec::PackageIdSpec;
use crate::resolve::Resolve;
use crate::resolver::{CargoResult, CompileKind};

/// What `cargo metadata` reads of the packages in a resolve.
pub trait MetadataPackages {
    /// The id as the package's own manifest gives it. A lock file may write a git source
    /// another way (an older cargo's `?branch=master` for the default branch); the JSON uses
    /// the package's.
    fn package_id(&self, id: PackageId) -> PackageId;
    /// The crate name of the package's library target, `-` written `_`; none without one.
    fn lib_crate_name(&self, id: PackageId) -> Option<InternedString>;
    /// Whether a `[target.'cfg(..)'.dependencies]` platform holds for `kind`.
    fn platform_activated(&self, platform: &Platform, kind: CompileKind) -> bool;
}

#[derive(Serialize)]
pub struct MetadataResolve {
    pub nodes: Vec<MetadataResolveNode>,
    pub root: Option<PackageIdSpec>,
}

#[derive(Serialize)]
pub struct MetadataResolveNode {
    pub id: PackageIdSpec,
    pub dependencies: Vec<PackageIdSpec>,
    pub deps: Vec<Dep>,
    pub features: Vec<InternedString>,
}

#[derive(Serialize)]
pub struct Dep {
    pub name: InternedString,
    pub pkg: PackageIdSpec,
    #[serde(skip)]
    pub pkg_id: PackageId,
    pub dep_kinds: Vec<DepKindInfo>,
}

#[derive(Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct DepKindInfo {
    pub kind: DepKind,
    pub target: Option<Platform>,
}

/// The resolve graph as `cargo metadata` shows it: from each of `members`, every package
/// reached, with the features the dependency resolver gave it. `requested_kinds` are the
/// `--filter-platform`s, `[CompileKind::Host]` without one; `root` is the member in the
/// current directory.
pub fn resolve_graph(
    resolve: &Resolve,
    members: &[PackageId],
    root: Option<PackageId>,
    packages: &impl MetadataPackages,
    requested_kinds: &[CompileKind],
) -> CargoResult<MetadataResolve> {
    let mut node_map = HashMap::with_capacity_and_hasher(resolve.len(), FixedState);
    for &member in members {
        build_resolve_graph_r(&mut node_map, member, resolve, packages, requested_kinds)?;
    }
    let mut nodes: Vec<_> = node_map.into_iter().collect();
    nodes.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    Ok(MetadataResolve {
        nodes: nodes.into_iter().map(|(_, node)| node).collect(),
        root: root.map(|id| id.to_spec()),
    })
}

fn build_resolve_graph_r<P: MetadataPackages>(
    node_map: &mut HashMap<PackageId, MetadataResolveNode>,
    pkg_id: PackageId,
    resolve: &Resolve,
    packages: &P,
    requested_kinds: &[CompileKind],
) -> CargoResult<()> {
    if node_map.contains_key(&pkg_id) {
        return Ok(());
    }
    let features = resolve.features(pkg_id).to_vec();

    let deps = {
        let mut dep_metadatas = Vec::new();
        let iter = resolve.deps(pkg_id).filter(|(_dep_id, deps)| {
            if requested_kinds == [CompileKind::Host] {
                true
            } else {
                requested_kinds.iter().any(|kind| {
                    deps.iter().any(|dep| match dep.platform() {
                        Some(platform) => packages.platform_activated(platform, *kind),
                        None => true,
                    })
                })
            }
        });
        for (dep_id, deps) in iter {
            // A dependency with no library (a parent depending on a bin-only crate) is left
            // out.
            let Some(lib_crate_name) = packages.lib_crate_name(dep_id) else {
                continue;
            };
            let mut dep_kinds: Vec<DepKindInfo> = deps
                .iter()
                .map(|dep| DepKindInfo { kind: dep.kind(), target: dep.platform().cloned() })
                .collect();
            dep_kinds.sort();

            let (name, _) =
                resolve.extern_crate_name_and_dep_name(pkg_id, dep_id, lib_crate_name.as_str())?;
            let dep_pkg_id = packages.package_id(dep_id);
            dep_metadatas.push(Dep {
                name,
                pkg: dep_pkg_id.to_spec(),
                pkg_id: dep_pkg_id,
                dep_kinds,
            })
        }
        dep_metadatas
    };

    let to_visit: Vec<PackageId> = deps.iter().map(|dep| dep.pkg_id).collect();
    let node = MetadataResolveNode {
        id: packages.package_id(pkg_id).to_spec(),
        dependencies: to_visit.iter().map(|id| id.to_spec()).collect(),
        deps,
        features,
    };
    node_map.insert(pkg_id, node);
    for dep_id in to_visit {
        build_resolve_graph_r(node_map, dep_id, resolve, packages, requested_kinds)?;
    }

    Ok(())
}
