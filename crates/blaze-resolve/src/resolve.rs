//! A resolved graph of packages: `src/resolver/resolve.rs` of cargo 1.99.0 (5f94df4789).

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use blaze_hash::{HashMap, HashSet};

use crate::dependency::Dependency;
use crate::graph::Graph;
use crate::interning::InternedString;
use crate::package_id::PackageId;
use crate::summary::Summary;
use crate::version::{PartialVersion, RustVersion};

/// The encoding of a `Cargo.lock`. A lock file keeps the encoding it was found in until it
/// changes for another reason, and is then raised to the default.
#[derive(PartialEq, Eq, Clone, Copy, Debug, PartialOrd, Ord, Default)]
pub enum ResolveVersion {
    /// No `version`, checksums in `[metadata]`, every dependency written in full.
    V1,
    /// Checksums on each package and dependencies written as briefly as is unambiguous
    /// (cargo 1.38; the default from 1.41 to 1.52).
    V2,
    /// `version = 3`, and a git dependency on `branch = "master"` no longer written as the
    /// default branch (1.47; the default from 1.53 to 1.82).
    V3,
    /// Git references URL-encoded: `?branch=foo bar` is `?branch=foo+bar` (1.78; the default
    /// from 1.83).
    #[default]
    V4,
}

impl ResolveVersion {
    pub fn max_stable() -> ResolveVersion {
        ResolveVersion::V4
    }

    /// The encoding of a new lock file, for a workspace whose lowest `rust-version` is
    /// `rust_version`: the newest those toolchains read.
    pub fn with_rust_version(rust_version: Option<&RustVersion>) -> Self {
        let Some(rust_version) = rust_version else {
            return ResolveVersion::default();
        };
        let rust = |major, minor| -> RustVersion {
            let partial =
                PartialVersion { major, minor: Some(minor), patch: None, pre: None, build: None };
            partial.to_string().parse().unwrap()
        };
        if rust_version >= &rust(1, 83) {
            ResolveVersion::V4
        } else if rust_version >= &rust(1, 53) {
            ResolveVersion::V3
        } else if rust_version >= &rust(1, 41) {
            ResolveVersion::V2
        } else {
            ResolveVersion::V1
        }
    }

    /// The `version = N` written, if this encoding writes one.
    pub fn number(self) -> Option<u32> {
        match self {
            ResolveVersion::V4 => Some(4),
            ResolveVersion::V3 => Some(3),
            ResolveVersion::V2 | ResolveVersion::V1 => None,
        }
    }
}

/// Unrecognized `[metadata]` of a lock file, kept as found.
// BTreeMap: serialized into the lock file as a map.
pub type LockfileMetadata = BTreeMap<String, String>;

/// Packages and the edges between them, each edge with the dependencies of `Cargo.toml` it
/// stands for (more than one where a package is both a normal and a build dependency). One
/// read from a lock file has no dependencies on its edges and no features.
#[derive(Clone)]
pub struct Resolve {
    graph: Graph<PackageId, HashSet<Dependency>>,
    /// `[replace]`: the package replaced and its replacement.
    replacements: HashMap<PackageId, PackageId>,
    reverse_replacements: HashMap<PackageId, PackageId>,
    features: HashMap<PackageId, Vec<InternedString>>,
    /// The sha256 of each package's `.crate`, `None` for sources without one (path, git).
    checksums: HashMap<PackageId, Option<String>>,
    metadata: LockfileMetadata,
    /// `[patch]` entries that matched nothing, kept as `[[patch.unused]]`.
    unused_patches: Vec<PackageId>,
    version: ResolveVersion,
    summaries: HashMap<PackageId, Summary>,
}

impl Resolve {
    pub fn new(
        graph: Graph<PackageId, HashSet<Dependency>>,
        replacements: HashMap<PackageId, PackageId>,
        features: HashMap<PackageId, Vec<InternedString>>,
        checksums: HashMap<PackageId, Option<String>>,
        metadata: LockfileMetadata,
        unused_patches: Vec<PackageId>,
        version: ResolveVersion,
        summaries: HashMap<PackageId, Summary>,
    ) -> Resolve {
        let reverse_replacements = replacements.iter().map(|(&p, &r)| (r, p)).collect();
        Resolve {
            graph,
            replacements,
            reverse_replacements,
            features,
            checksums,
            metadata,
            unused_patches,
            version,
            summaries,
        }
    }

    /// Takes what a previous resolve (the lock file's) knew that this one keeps: its checksums,
    /// which must agree, its unknown metadata and its encoding.
    pub fn merge_from(&mut self, previous: &Resolve) -> Result<()> {
        for (id, cksum) in previous.checksums.iter() {
            let Some(mine) = self.checksums.get(id) else { continue };
            if mine == cksum {
                continue;
            }
            if cksum.is_none() {
                bail!(
                    "\
checksum for `{}` was not previously calculated, but a checksum could now \
be calculated

this could be indicative of a few possible situations:

    * the source `{}` did not previously support checksums,
      but was replaced with one that does
    * newer Cargo implementations know how to checksum this source, but this
      older implementation does not
    * the lock file is corrupt
",
                    id,
                    id.source_id()
                )
            } else if mine.is_none() {
                bail!(
                    "\
checksum for `{}` could not be calculated, but a checksum is listed in \
the existing lock file

this could be indicative of a few possible situations:

    * the source `{}` supports checksums,
      but was replaced with one that doesn't
    * the lock file is corrupt

unable to verify that `{0}` is the same as when the lockfile was generated
",
                    id,
                    id.source_id()
                )
            } else {
                bail!(
                    "\
checksum for `{}` changed between lock files

this could be indicative of a few possible errors:

    * the lock file is corrupt
    * a replacement source in use (e.g., a mirror) returned a different checksum
    * the source itself may be corrupt in one way or another

unable to verify that `{0}` is the same as when the lockfile was generated
",
                    id
                );
            }
        }
        self.metadata = previous.metadata.clone();
        self.version = previous.version;
        Ok(())
    }

    pub fn contains(&self, k: &PackageId) -> bool {
        self.graph.contains(k)
    }

    /// Dependencies before their dependents.
    pub fn sort(&self) -> Vec<PackageId> {
        self.graph.sort()
    }

    /// The packages in order.
    pub fn iter(&self) -> impl Iterator<Item = PackageId> + '_ {
        self.graph.iter().cloned()
    }

    pub fn len(&self) -> usize {
        self.graph.len()
    }

    pub fn graph(&self) -> &Graph<PackageId, HashSet<Dependency>> {
        &self.graph
    }

    /// The packages `pkg` depends on, replacements applied.
    pub fn deps(&self, pkg: PackageId) -> impl Iterator<Item = (PackageId, &HashSet<Dependency>)> {
        self.deps_not_replaced(pkg)
            .map(move |(id, deps)| (self.replacement(id).unwrap_or(id), deps))
    }

    pub fn deps_not_replaced(
        &self,
        pkg: PackageId,
    ) -> impl Iterator<Item = (PackageId, &HashSet<Dependency>)> {
        self.graph.edges(&pkg).map(|(id, deps)| (*id, deps))
    }

    /// The edges other than those only to dev-dependencies, which do not make cycles.
    pub fn transitive_deps_not_replaced(
        &self,
        pkg: PackageId,
    ) -> impl Iterator<Item = (PackageId, &Dependency)> {
        self.graph.edges(&pkg).filter_map(|(id, deps)| {
            deps.iter().find(|d| d.is_transitive()).map(|transitive_dep| (*id, transitive_dep))
        })
    }

    pub fn replacement(&self, pkg: PackageId) -> Option<PackageId> {
        self.replacements.get(&pkg).cloned()
    }

    pub fn replacements(&self) -> &HashMap<PackageId, PackageId> {
        &self.replacements
    }

    pub fn reverse_replacement(&self, pkg: PackageId) -> Option<PackageId> {
        self.reverse_replacements.get(&pkg).cloned()
    }

    /// The name `from`'s code calls `to` by, and the name of its dependency if renamed: the
    /// rename with `-` as `_`, else `to_crate_name`, the crate name of `to`'s target.
    pub fn extern_crate_name_and_dep_name(
        &self,
        from: PackageId,
        to: PackageId,
        to_crate_name: &str,
    ) -> Result<(InternedString, Option<InternedString>)> {
        let empty_set = HashSet::default();
        let deps = if from == to { &empty_set } else { self.dependencies_listed(from, to) };

        let target_crate_name = || (to_crate_name.to_owned(), None);
        let mut name_pairs = deps.iter().map(|d| {
            d.explicit_name_in_toml()
                .map(|s| (s.as_str().replace("-", "_"), Some(s)))
                .unwrap_or_else(target_crate_name)
        });
        let (extern_crate_name, dep_name) = name_pairs.next().unwrap_or_else(target_crate_name);
        for (n, _) in name_pairs {
            anyhow::ensure!(
                n == extern_crate_name,
                "the crate `{}` depends on crate `{}` multiple times with different names",
                from,
                to,
            );
        }
        Ok((InternedString::new(&extern_crate_name), dep_name))
    }

    fn dependencies_listed(&self, from: PackageId, to: PackageId) -> &HashSet<Dependency> {
        // We've got a dependency on `from` to `to`, but this dependency edge
        // may be affected by [replace]. If the `to` package is listed as the
        // target of a replacement (aka the key of a reverse replacement map)
        // then we try to find our dependency edge through that. If that fails
        // then we go down below assuming it's not replaced.
        //
        // Note that we don't treat `from` as if it's been replaced because
        // that's where the dependency originates from, and we only replace
        // targets of dependencies not the originator.
        if let Some(replace) = self.reverse_replacements.get(&to)
            && let Some(deps) = self.graph.edge(&from, replace)
        {
            return deps;
        }
        match self.graph.edge(&from, &to) {
            Some(ret) => ret,
            None => panic!("no Dependency listed for `{}` => `{}`", from, to),
        }
    }

    /// The features the dependency resolver enabled for `pkg` (all unified, as resolver 1
    /// builds them).
    pub fn features(&self, pkg: PackageId) -> &[InternedString] {
        self.features.get(&pkg).map(|v| &**v).unwrap_or(&[])
    }

    pub fn unused_patches(&self) -> &[PackageId] {
        &self.unused_patches
    }

    /// Records the `[patch]` entries the graph did not use.
    pub fn register_used_patches<'a>(&mut self, patches: impl Iterator<Item = &'a Summary>) {
        self.unused_patches = patches
            .filter(|p| !self.graph.contains(&p.package_id()))
            .map(|p| p.package_id())
            .collect();
    }

    pub fn checksums(&self) -> &HashMap<PackageId, Option<String>> {
        &self.checksums
    }

    pub fn set_checksum(&mut self, pkg_id: PackageId, checksum: String) {
        self.checksums.insert(pkg_id, Some(checksum));
    }

    pub fn metadata(&self) -> &LockfileMetadata {
        &self.metadata
    }

    pub fn version(&self) -> ResolveVersion {
        self.version
    }

    pub fn set_version(&mut self, version: ResolveVersion) {
        self.version = version;
    }

    pub fn summary(&self, pkg_id: PackageId) -> &Summary {
        &self.summaries[&pkg_id]
    }

    pub fn summaries(&self) -> &HashMap<PackageId, Summary> {
        &self.summaries
    }
}

/// Everything but the encoding.
impl PartialEq for Resolve {
    fn eq(&self, other: &Resolve) -> bool {
        macro_rules! compare {
            ($($fields:ident)* | $($ignored:ident)*) => {
                let Resolve { $($fields,)* $($ignored: _,)* } = self;
                $($fields == &other.$fields)&&*
            }
        }
        compare! {
            graph replacements reverse_replacements features checksums metadata unused_patches
            summaries
            | version
        }
    }
}

impl std::fmt::Debug for Resolve {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(fmt, "graph: {:?}", self.graph)?;
        writeln!(fmt, "\nfeatures: {{")?;
        for (pkg, features) in &self.features {
            writeln!(fmt, "  {}: {:?}", pkg, features)?;
        }
        write!(fmt, "}}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_for_rust_version() {
        let v = |s: &str| ResolveVersion::with_rust_version(Some(&s.parse().unwrap()));
        assert_eq!(ResolveVersion::with_rust_version(None), ResolveVersion::V4);
        assert_eq!(v("1.99"), ResolveVersion::V4);
        assert_eq!(v("1.83"), ResolveVersion::V4);
        assert_eq!(v("1.82.9"), ResolveVersion::V3);
        assert_eq!(v("1.53"), ResolveVersion::V3);
        assert_eq!(v("1.52"), ResolveVersion::V2);
        assert_eq!(v("1.41"), ResolveVersion::V2);
        assert_eq!(v("1.40"), ResolveVersion::V1);
        assert_eq!(v("1"), ResolveVersion::V1);
        assert_eq!(v("2"), ResolveVersion::V4);
    }
}
