//! The resolve graph of one build: what `cargo build` with a selection of packages and features
//! compiles, as cargo's `unit_dependencies` walks it, folded into `cargo metadata`'s shape, a
//! node a package. `cargo metadata` itself unifies features over every member.

use anyhow::Result;
use blaze_hash::{HashMap, HashSet};
use blaze_resolve::metadata::{Dep, DepKindInfo, MetadataResolve, MetadataResolveNode};
use blaze_resolve::resolver::{FeaturesFor, ResolvedFeatures};
use blaze_resolve::{DepKind, InternedString, PackageId, Resolve};

use blaze_units::Read;

use crate::Packages;

/// Unordered while the walk fills it; [`of_build`] sorts each part once when it writes the node.
#[derive(Default)]
struct Node {
    features: HashSet<InternedString>,
    /// Each dependency's name and its kinds, a kind once per edge it passes on.
    deps: HashMap<PackageId, (InternedString, Vec<DepKindInfo>)>,
}

/// Whether a `cargo build` of the member `pkg` builds anything: its library, or a binary
/// whose `required-features` it is built with. One with only tests, examples or benchmarks,
/// or binaries that need features it lacks, has no units.
fn builds_anything(
    pkg: PackageId,
    features: &ResolvedFeatures,
    reads: &HashMap<PackageId, Read>,
) -> bool {
    let Some(read) = reads.get(&pkg) else { return false };
    let on =
        features.activated_features_unverified(pkg, FeaturesFor::NormalOrDev).unwrap_or_default();
    // A feature of a dependency (`dep/feature`) is taken to be on.
    let has =
        |required: &String| required.contains('/') || on.iter().any(|it| it.as_str() == required);
    read.lib.is_some() || read.bins.iter().any(|it| it.iter().all(has))
}

/// From each of `roots`, the packages the build reaches, each with the features it is built
/// with (both builds of one built for the target and for the host) and the dependencies it
/// is built against. `dev_units` is whether the roots' tests, examples and benchmarks are
/// built; `packages` says whether a `[target.'cfg(..)']` platform holds.
pub(crate) fn of_build(
    resolve: &Resolve,
    features: &ResolvedFeatures,
    roots: &[PackageId],
    dev_units: bool,
    reads: &HashMap<PackageId, Read>,
    packages: &Packages<'_>,
    root: Option<PackageId>,
) -> Result<MetadataResolve> {
    let mut nodes: HashMap<PackageId, Node> = HashMap::default();
    let mut seen = HashSet::default();
    let mut queue = Vec::new();
    for &member in roots {
        if !dev_units && !builds_anything(member, features, reads) {
            continue;
        }
        queue.push((member, FeaturesFor::NormalOrDev));
        if reads.get(&member).is_some_and(|it| it.any_proc_macro) {
            queue.push((member, FeaturesFor::HostDep));
        }
    }
    while let Some((pkg, fk)) = queue.pop() {
        if !seen.insert((pkg, fk)) {
            continue;
        }
        let node = nodes.entry(pkg).or_default();
        node.features.extend(features.activated_features_unverified(pkg, fk).unwrap_or_default());
        let is_root = roots.contains(&pkg);
        let build_script = reads.get(&pkg).is_some_and(|it| it.build_script);
        for (dep_id, deps) in resolve.deps(pkg) {
            let Some(read) = reads.get(&dep_id) else { continue };
            let Some(lib) = read.lib else { continue };
            let passing: Vec<_> = deps
                .iter()
                .filter(|dep| {
                    let kind_built = match dep.kind() {
                        DepKind::Development => is_root && dev_units,
                        DepKind::Normal => true,
                        DepKind::Build => build_script,
                    };
                    let name = dep.name_in_toml();
                    let enabled = !dep.is_optional()
                        || features.is_dep_activated(pkg, fk, name)
                        || (dep.kind() == DepKind::Build
                            && features.is_dep_activated(pkg, FeaturesFor::HostDep, name));
                    kind_built && dep.platform().is_none_or(|it| packages.activates(it)) && enabled
                })
                .collect();
            if passing.is_empty() {
                continue;
            }
            let (name, _) = resolve.extern_crate_name_and_dep_name(pkg, dep_id, lib.as_str())?;
            let entry = node.deps.entry(dep_id).or_insert_with(|| (name, Vec::new()));
            for dep in &passing {
                entry.1.push(DepKindInfo { kind: dep.kind(), target: dep.platform().cloned() });
                let host = dep.kind() == DepKind::Build || read.proc_macro_lib;
                queue.push((dep_id, if host { FeaturesFor::HostDep } else { fk }));
            }
        }
    }
    Ok(MetadataResolve { nodes: written(nodes), root: root.map(|it| it.to_spec()) })
}

/// The nodes as `cargo metadata` writes them: by package, each one's dependencies by package,
/// a dependency's kinds sorted and each once, its features sorted.
fn written(nodes: HashMap<PackageId, Node>) -> Vec<MetadataResolveNode> {
    let mut nodes: Vec<(PackageId, Node)> = nodes.into_iter().collect();
    nodes.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    nodes
        .into_iter()
        .map(|(id, node)| {
            let mut deps: Vec<(PackageId, (InternedString, Vec<DepKindInfo>))> =
                node.deps.into_iter().collect();
            deps.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
            let deps: Vec<Dep> = deps
                .into_iter()
                .map(|(pkg_id, (name, mut kinds))| {
                    kinds.sort_unstable();
                    kinds.dedup();
                    Dep { name, pkg: pkg_id.to_spec(), pkg_id, dep_kinds: kinds }
                })
                .collect();
            let mut features: Vec<InternedString> = node.features.into_iter().collect();
            features.sort_unstable();
            MetadataResolveNode {
                id: id.to_spec(),
                dependencies: deps.iter().map(|it| it.pkg_id.to_spec()).collect(),
                deps,
                features,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use blaze_resolve::SourceId;

    use super::*;

    fn id(name: &str, version: &str) -> PackageId {
        PackageId::try_new(name, version, SourceId::crates_io()).unwrap()
    }

    fn kind(kind: DepKind, target: Option<&str>) -> DepKindInfo {
        DepKindInfo { kind, target: target.map(|it| it.parse().unwrap()) }
    }

    /// The same nodes written from ordered maps, as before the walk filled hash maps.
    fn reference(
        nodes: &[(PackageId, &[&str], &[(PackageId, &str, &[(DepKind, Option<&str>)])])],
    ) -> Vec<MetadataResolveNode> {
        type Deps = BTreeMap<PackageId, (InternedString, BTreeSet<DepKindInfo>)>;
        let mut tree: BTreeMap<PackageId, (BTreeSet<InternedString>, Deps)> = BTreeMap::new();
        for (pkg, features, deps) in nodes {
            let node = tree.entry(*pkg).or_default();
            node.0.extend(features.iter().map(|it| InternedString::new(it)));
            for (dep, name, kinds) in *deps {
                let entry = node.1.entry(*dep).or_insert_with(|| (InternedString::new(name), BTreeSet::new()));
                entry.1.extend(kinds.iter().map(|(k, t)| kind(*k, *t)));
            }
        }
        tree.into_iter()
            .map(|(id, (features, deps))| {
                let deps: Vec<Dep> = deps
                    .into_iter()
                    .map(|(pkg_id, (name, kinds))| Dep {
                        name,
                        pkg: pkg_id.to_spec(),
                        pkg_id,
                        dep_kinds: kinds.into_iter().collect(),
                    })
                    .collect();
                MetadataResolveNode {
                    id: id.to_spec(),
                    dependencies: deps.iter().map(|it| it.pkg_id.to_spec()).collect(),
                    deps,
                    features: features.into_iter().collect(),
                }
            })
            .collect()
    }

    #[test]
    fn nodes_are_written_as_the_ordered_maps_wrote_them() {
        let (a, b, c, d) = (id("a", "1.0.0"), id("b", "0.1.0"), id("b", "0.2.0"), id("zed", "2.0.0"));
        let unix = Some("cfg(unix)");
        let input: &[(PackageId, &[&str], &[(PackageId, &str, &[(DepKind, Option<&str>)])])] = &[
            (d, &["std", "alloc"], &[]),
            (a, &["z", "default", "m"], &[
                (d, "zed", &[(DepKind::Normal, None), (DepKind::Build, None)]),
                (c, "b2", &[(DepKind::Build, unix), (DepKind::Normal, None)]),
                (b, "b", &[(DepKind::Development, None)]),
            ]),
            (a, &["m", "a"], &[
                (c, "b2", &[(DepKind::Normal, None), (DepKind::Build, unix)]),
                (b, "b", &[(DepKind::Normal, None)]),
            ]),
            (c, &[], &[(d, "zed", &[(DepKind::Normal, unix), (DepKind::Normal, None)])]),
            (b, &["x"], &[]),
        ];
        let mut nodes: HashMap<PackageId, Node> = HashMap::default();
        for (pkg, features, deps) in input {
            let node = nodes.entry(*pkg).or_default();
            node.features.extend(features.iter().map(|it| InternedString::new(it)));
            for (dep, name, kinds) in *deps {
                let entry = node.deps.entry(*dep).or_insert_with(|| (InternedString::new(name), Vec::new()));
                entry.1.extend(kinds.iter().map(|(k, t)| kind(*k, *t)));
            }
        }
        let got = serde_json::to_string(&written(nodes)).unwrap();
        let want = serde_json::to_string(&reference(input)).unwrap();
        assert_eq!(got, want);
    }
}
