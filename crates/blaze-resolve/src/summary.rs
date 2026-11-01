//! What the resolver knows of one version of a package, and its feature map:
//! `src/workspace/summary.rs` of cargo 1.99.0 (5f94df4789).

use std::collections::BTreeMap;

use blaze_hash::{HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::mem;
use std::sync::Arc;

use anyhow::{Result, bail};
use semver::Version;

use crate::dependency::Dependency;
use crate::edit_distance::closest_msg;
use crate::interning::InternedString;
use crate::names::validate_feature_name;
use crate::package_id::PackageId;
use crate::source_id::SourceId;
use crate::version::RustVersion;

/// A package's id, dependencies, features, `links`, `rust-version` and, from a registry, its
/// checksum and publish time. Equal and hashed by the id alone. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Summary {
    inner: Arc<Inner>,
}

#[derive(Debug, Clone)]
struct Inner {
    package_id: PackageId,
    dependencies: Vec<Dependency>,
    features: Arc<FeatureMap>,
    checksum: Option<String>,
    links: Option<InternedString>,
    rust_version: Option<RustVersion>,
    /// When the version was published, as the index writes it (RFC 3339).
    pubtime: Option<String>,
}

/// A `dep/feature` or `dep?/feature` naming a dependency that is not one: the resolver says
/// so with the name of the feature, where the summary alone could not.
#[derive(Debug)]
pub struct MissingDependencyError {
    pub dep_name: InternedString,
    pub feature: InternedString,
    pub feature_value: FeatureValue,
    /// Written `dep?/feature`.
    pub weak_optional: bool,
}

impl std::error::Error for MissingDependencyError {}

impl fmt::Display for MissingDependencyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { dep_name, feature, feature_value: fv, .. } = self;
        write!(f, "feature `{feature}` includes `{fv}`, but `{dep_name}` is not a dependency")
    }
}

impl Summary {
    /// Checks the features against the dependencies and builds the feature map, with the
    /// implicit feature of each optional dependency no `dep:` names.
    pub fn new(
        pkg_id: PackageId,
        dependencies: Vec<Dependency>,
        features: &BTreeMap<InternedString, Vec<InternedString>>,
        links: Option<impl Into<InternedString>>,
        rust_version: Option<RustVersion>,
    ) -> Result<Summary> {
        for dep in dependencies.iter() {
            if dep.is_optional() && !dep.is_transitive() {
                bail!("dev-dependencies are not allowed to be optional: `{}`", dep.name_in_toml())
            }
        }
        let feature_map = build_feature_map(
            features,
            dependencies
                .iter()
                .map(|d| FeatureDep { name_in_toml: d.name_in_toml(), optional: d.is_optional() }),
        )?;
        Ok(Summary {
            inner: Arc::new(Inner {
                package_id: pkg_id,
                dependencies,
                features: Arc::new(feature_map),
                checksum: None,
                links: links.map(|l| l.into()),
                rust_version,
                pubtime: None,
            }),
        })
    }

    pub fn package_id(&self) -> PackageId {
        self.inner.package_id
    }

    pub fn name(&self) -> InternedString {
        self.package_id().name()
    }

    pub fn version(&self) -> &'static Version {
        self.package_id().version()
    }

    pub fn source_id(&self) -> SourceId {
        self.package_id().source_id()
    }

    pub fn dependencies(&self) -> &[Dependency] {
        &self.inner.dependencies
    }

    pub fn features(&self) -> &FeatureMap {
        &self.inner.features
    }

    pub fn checksum(&self) -> Option<&str> {
        self.inner.checksum.as_deref()
    }

    pub fn links(&self) -> Option<InternedString> {
        self.inner.links
    }

    pub fn rust_version(&self) -> Option<&RustVersion> {
        self.inner.rust_version.as_ref()
    }

    pub fn pubtime(&self) -> Option<&str> {
        self.inner.pubtime.as_deref()
    }

    pub fn override_id(mut self, id: PackageId) -> Summary {
        Arc::make_mut(&mut self.inner).package_id = id;
        self
    }

    pub fn set_checksum(&mut self, cksum: String) {
        Arc::make_mut(&mut self.inner).checksum = Some(cksum);
    }

    pub fn set_pubtime(&mut self, pubtime: String) {
        Arc::make_mut(&mut self.inner).pubtime = Some(pubtime);
    }

    pub fn map_dependencies(self, mut f: impl FnMut(Dependency) -> Dependency) -> Summary {
        self.try_map_dependencies(|dep| Ok(f(dep))).unwrap()
    }

    pub fn try_map_dependencies(
        mut self,
        f: impl FnMut(Dependency) -> Result<Dependency>,
    ) -> Result<Summary> {
        let slot = &mut Arc::make_mut(&mut self.inner).dependencies;
        *slot = mem::take(slot).into_iter().map(f).collect::<Result<_>>()?;
        Ok(self)
    }

    pub fn map_source(self, to_replace: SourceId, replace_with: SourceId) -> Summary {
        let me = if self.package_id().source_id() == to_replace {
            let new_id = self.package_id().with_source_id(replace_with);
            self.override_id(new_id)
        } else {
            self
        };
        me.map_dependencies(|dep| dep.map_source(to_replace, replace_with))
    }
}

impl PartialEq for Summary {
    fn eq(&self, other: &Summary) -> bool {
        self.inner.package_id == other.inner.package_id
    }
}

impl Eq for Summary {}

impl Hash for Summary {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner.package_id.hash(state);
    }
}

/// What the feature map reads of a dependency.
#[derive(Clone, Copy, Debug)]
pub struct FeatureDep {
    /// The key of its `[dependencies]` entry.
    pub name_in_toml: InternedString,
    pub optional: bool,
}

/// cargo's `build_feature_map`: the declared features, plus the implicit feature of each
/// optional dependency no `dep:` names, with cargo's checks and messages. `dependencies` is
/// walked in its order, which decides which unused optional dependency an error names.
pub fn build_feature_map<I>(
    features: &BTreeMap<InternedString, Vec<InternedString>>,
    dependencies: I,
) -> Result<FeatureMap>
where
    I: Iterator<Item = FeatureDep> + Clone,
{
    use FeatureValue::*;
    // Each name in `[dependencies]` and whether any entry of that name is optional.
    let mut dep_map: HashMap<InternedString, bool> = HashMap::default();
    for dep in dependencies.clone() { // clone: an iterator over borrowed dependencies
        *dep_map.entry(dep.name_in_toml).or_insert(false) |= dep.optional;
    }

    let mut map: FeatureMap = features
        .iter()
        .map(|(feature, list)| (*feature, list.iter().map(|fv| FeatureValue::new(*fv)).collect())) // alloc: the feature map, once per package
        .collect(); // alloc: as above

    // The implicit feature of an optional dependency that no `dep:` names and no feature of
    // the same name replaces.
    let explicitly_listed: HashSet<_> =
        map.values().flatten().filter_map(|fv| fv.explicit_dep_name()).collect(); // alloc: once per package
    for dep in dependencies.clone() { // clone: an iterator over borrowed dependencies
        if !dep.optional {
            continue;
        }
        let dep_name = dep.name_in_toml;
        if features.contains_key(&dep_name) || explicitly_listed.contains(&dep_name) {
            continue;
        }
        map.insert(dep_name, vec![Dep { dep_name }]); // alloc: the feature map, once per package
    }

    for (feature, fvs) in &map {
        validate_feature_name(feature)?;
        for fv in fvs {
            let dep_data = dep_map.get(&fv.feature_or_dep_name());
            let is_any_dep = dep_data.is_some();
            let is_optional_dep = dep_data.is_some_and(|&o| o);
            match fv {
                Feature(f) => {
                    if !features.contains_key(f) {
                        if !is_any_dep {
                            let closest = closest_msg(f, features.keys(), |k| k, "feature");
                            bail!(
                                "feature `{feature}` includes `{fv}` which is neither a dependency \
                                 nor another feature{closest}"
                            );
                        }
                        if is_optional_dep {
                            if !map.contains_key(f) {
                                bail!(
                                    "feature `{feature}` includes `{fv}`, but `{f}` is an \
                                     optional dependency without an implicit feature\n\
                                     Use `dep:{f}` to enable the dependency."
                                );
                            }
                        } else {
                            bail!(
                                "feature `{feature}` includes `{fv}`, but `{f}` is not an optional dependency\n\
                                A non-optional dependency of the same name is defined; \
                                consider adding `optional = true` to its definition."
                            );
                        }
                    }
                }
                Dep { dep_name } => {
                    if !is_any_dep {
                        bail!(
                            "feature `{feature}` includes `{fv}`, but `{dep_name}` is not listed as a dependency"
                        );
                    }
                    if !is_optional_dep {
                        bail!(
                            "feature `{feature}` includes `{fv}`, but `{dep_name}` is not an optional dependency\n\
                             A non-optional dependency of the same name is defined; \
                             consider adding `optional = true` to its definition."
                        );
                    }
                }
                DepFeature { dep_name, dep_feature, weak } => {
                    if dep_feature.contains('/') {
                        bail!(
                            "multiple slashes in feature `{fv}` (included by feature `{feature}`) are not allowed"
                        );
                    }
                    if let Some(stripped_dep) = dep_name.strip_prefix("dep:") {
                        let has_other_dep = explicitly_listed.contains(stripped_dep);
                        let is_optional = dep_map.get(stripped_dep).is_some_and(|&o| o);
                        let extra_help = if *weak || has_other_dep || !is_optional {
                            String::new() // alloc: error path
                        } else {
                            format!( // alloc: error path
                                "\nIf the intent is to avoid creating an implicit feature \
                                 `{stripped_dep}` for an optional dependency, \
                                 then consider replacing this with two values:\n    \
                                 \"dep:{stripped_dep}\", \"{stripped_dep}/{dep_feature}\""
                            )
                        };
                        bail!(
                            "feature `{feature}` includes `{fv}` with both `dep:` and `/`\n\
                            To fix this, remove the `dep:` prefix.{extra_help}"
                        )
                    }
                    if !is_any_dep {
                        bail!(MissingDependencyError {
                            feature: *feature,
                            feature_value: fv.clone(), // clone: error path
                            dep_name: *dep_name,
                            weak_optional: *weak,
                        })
                    }
                    if *weak && !is_optional_dep {
                        bail!(
                            "feature `{feature}` includes `{fv}` with a `?`, but `{dep_name}` is not an optional dependency\n\
                            A non-optional dependency of the same name is defined; \
                            consider removing the `?` or changing the dependency to be optional"
                        );
                    }
                }
            }
        }
    }

    // Every optional dependency is enabled by some feature.
    let used: HashSet<_> = map
        .values()
        .flatten()
        .filter_map(|fv| match fv {
            Dep { dep_name } | DepFeature { dep_name, .. } => Some(dep_name),
            _ => None,
        })
        .collect(); // alloc: once per package
    if let Some(dep) = dependencies
        .map(|d| d.name_in_toml)
        .find(|dep| dep_map[dep] && !used.contains(dep))
    {
        bail!(
            "optional dependency `{dep}` is not included in any feature\n\
            Make sure that `dep:{dep}` is included in one of features in the [features] table."
        );
    }

    Ok(map)
}

/// One entry of a feature's list.
#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub enum FeatureValue {
    /// `feat`: another feature, or the implicit feature of an optional dependency.
    Feature(InternedString),
    /// `dep:name`: the optional dependency itself, with no feature of its name.
    Dep { dep_name: InternedString },
    /// `name/feat`, or with `weak` `name?/feat`, which does not enable an optional `name` but
    /// enables `feat` on it if something else does.
    DepFeature { dep_name: InternedString, dep_feature: InternedString, weak: bool },
}

impl FeatureValue {
    pub fn new(feature: InternedString) -> FeatureValue {
        match feature.split_once('/') {
            Some((dep, dep_feat)) => {
                let dep_name = dep.strip_suffix('?');
                FeatureValue::DepFeature {
                    dep_name: dep_name.unwrap_or(dep).into(),
                    dep_feature: dep_feat.into(),
                    weak: dep_name.is_some(),
                }
            }
            None => match feature.strip_prefix("dep:") {
                Some(dep_name) => FeatureValue::Dep { dep_name: dep_name.into() },
                None => FeatureValue::Feature(feature),
            },
        }
    }

    /// The dependency named with `dep:`, and only then.
    pub fn explicit_dep_name(&self) -> Option<InternedString> {
        match self {
            FeatureValue::Dep { dep_name, .. } => Some(*dep_name),
            _ => None,
        }
    }

    pub fn feature_or_dep_name(&self) -> InternedString {
        match self {
            FeatureValue::Feature(dep_name)
            | FeatureValue::Dep { dep_name, .. }
            | FeatureValue::DepFeature { dep_name, .. } => *dep_name,
        }
    }
}

impl fmt::Display for FeatureValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeatureValue::Feature(feat) => write!(f, "{feat}"),
            FeatureValue::Dep { dep_name } => write!(f, "dep:{dep_name}"),
            FeatureValue::DepFeature { dep_name, dep_feature, weak } => {
                let weak = if *weak { "?" } else { "" };
                write!(f, "{dep_name}{weak}/{dep_feature}")
            }
        }
    }
}

// BTreeMap: built once per package, read in order (`features_enabling_dependency_sorted`,
// `blaze-tree`, the index) many times.
pub type FeatureMap = BTreeMap<InternedString, Vec<FeatureValue>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependency::DepKind;

    fn dep(name: &str, optional: bool) -> Dependency {
        let mut d = Dependency::parse(name, Some("1"), SourceId::crates_io()).unwrap();
        d.set_optional(optional);
        d
    }

    fn features(list: &[(&str, &[&str])]) -> BTreeMap<InternedString, Vec<InternedString>> {
        list.iter()
            .map(|(k, v)| {
                (InternedString::new(k), v.iter().map(|s| InternedString::new(s)).collect())
            })
            .collect()
    }

    fn summary(deps: Vec<Dependency>, feats: &[(&str, &[&str])]) -> Result<Summary> {
        let id = PackageId::try_new("pkg", "1.0.0", SourceId::crates_io()).unwrap();
        Summary::new(id, deps, &features(feats), None::<&str>, None)
    }

    fn shown(map: &FeatureMap) -> Vec<String> {
        map.iter()
            .map(|(k, v)| {
                format!(
                    "{k} = [{}]",
                    v.iter().map(|fv| fv.to_string()).collect::<Vec<_>>().join(", ")
                )
            })
            .collect()
    }

    #[test]
    fn feature_values() {
        assert_eq!(FeatureValue::new("std".into()), FeatureValue::Feature("std".into()));
        assert_eq!(FeatureValue::new("dep:x".into()), FeatureValue::Dep { dep_name: "x".into() });
        assert_eq!(
            FeatureValue::new("x/feat".into()),
            FeatureValue::DepFeature {
                dep_name: "x".into(),
                dep_feature: "feat".into(),
                weak: false
            }
        );
        assert_eq!(
            FeatureValue::new("x?/feat".into()),
            FeatureValue::DepFeature {
                dep_name: "x".into(),
                dep_feature: "feat".into(),
                weak: true
            }
        );
        for s in ["std", "dep:x", "x/feat", "x?/feat"] {
            assert_eq!(FeatureValue::new(s.into()).to_string(), s);
        }
    }

    #[test]
    fn implicit_features() {
        // `a` gets an implicit feature; `b` does not, `dep:b` names it; `c` has a feature of
        // its own name.
        let s = summary(
            vec![dep("a", true), dep("b", true), dep("c", true), dep("d", false)],
            &[("default", &["a", "x"]), ("x", &["dep:b", "d/std"]), ("c", &["dep:c", "a?/foo"])],
        )
        .unwrap();
        assert_eq!(
            shown(s.features()),
            ["a = [dep:a]", "c = [dep:c, a?/foo]", "default = [a, x]", "x = [dep:b, d/std]"]
        );
    }

    #[test]
    fn renamed_optional_dep_feature_is_its_toml_name() {
        let mut d = dep("serde", true);
        d.set_explicit_name_in_toml("serde1");
        let s = summary(vec![d], &[]).unwrap();
        assert_eq!(shown(s.features()), ["serde1 = [dep:serde1]"]);
    }

    #[test]
    fn errors() {
        let err = |deps, feats: &[(&str, &[&str])]| summary(deps, feats).unwrap_err().to_string();
        assert_eq!(
            err(vec![], &[("a", &["stdd"]), ("std", &[])]),
            "feature `a` includes `stdd` which is neither a dependency nor another feature\n\n\
             help: a feature with a similar name exists: `std`"
        );
        assert!(
            err(vec![dep("x", true)], &[("f", &["dep:x"]), ("g", &["x"])])
                .contains("optional dependency without an implicit feature")
        );
        assert!(
            err(vec![dep("x", false)], &[("f", &["x"])]).contains("is not an optional dependency")
        );
        assert!(err(vec![], &[("f", &["dep:x"])]).contains("is not listed as a dependency"));
        assert!(
            err(vec![dep("x", false)], &[("f", &["dep:x"])])
                .contains("is not an optional dependency")
        );
        assert!(err(vec![dep("x", true)], &[("f", &["x/a/b"])]).contains("multiple slashes"));
        assert!(
            err(vec![dep("x", true)], &[("f", &["dep:x/a"])]).contains("with both `dep:` and `/`")
        );
        assert_eq!(
            err(vec![], &[("f", &["x/a"])]),
            "feature `f` includes `x/a`, but `x` is not a dependency"
        );
        assert!(err(vec![dep("x", false)], &[("f", &["x?/a"])]).contains("with a `?`"));
        // A feature may share the name of an optional dependency `dep:` enables elsewhere.
        summary(vec![dep("x", true)], &[("f", &["dep:x"]), ("x", &[])]).unwrap();
        assert!(
            err(vec![dep("x", true)], &[("dep:f", &[])])
                .contains("not allowed to start with `dep:`")
        );
        assert_eq!(
            err(vec![dep("x", true)], &[("x", &[])]),
            "optional dependency `x` is not included in any feature\n\
             Make sure that `dep:x` is included in one of features in the [features] table."
        );
        let mut dev = dep("t", true);
        dev.set_kind(DepKind::Development);
        assert_eq!(err(vec![dev], &[]), "dev-dependencies are not allowed to be optional: `t`");
    }

    #[test]
    fn missing_dependency_is_typed() {
        let e = summary(vec![], &[("f", &["x?/a"])]).unwrap_err();
        let missing = e.downcast_ref::<MissingDependencyError>().unwrap();
        assert!(missing.weak_optional);
        assert_eq!(missing.dep_name, "x");
    }
}
