//! A dependency as a manifest declares it (cargo's `Dependency`), and the feature map of a
//! package (cargo's `build_feature_map`), with cargo's checks and messages.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use cargo_platform::Platform;
use cargo_util_schemas::manifest::FeatureName;
use rustc_hash::{FxHashMap, FxHashSet};
use semver::VersionReq;
use serde::Serialize;

use crate::edit_distance::closest_msg;
use crate::source::SourceId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepKind {
    Normal,
    Development,
    Build,
}

impl DepKind {
    pub fn kind_table(&self) -> &'static str {
        match self {
            DepKind::Normal => "dependencies",
            DepKind::Development => "dev-dependencies",
            DepKind::Build => "build-dependencies",
        }
    }
}

impl Serialize for DepKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            DepKind::Normal => None,
            DepKind::Development => Some("dev"),
            DepKind::Build => Some("build"),
        }
        .serialize(s)
    }
}

#[derive(Clone, Debug)]
pub struct Dependency {
    /// The name of the package depended on, which is what `name_in_toml` renames.
    pub name: String,
    pub source_id: SourceId,
    /// The registry named by `registry` or `registry-index`, when one is.
    pub registry_id: Option<SourceId>,
    /// `None` is any version, cargo's `OptVersionReq::Any`.
    pub req: Option<VersionReq>,
    pub kind: DepKind,
    pub optional: bool,
    pub public: bool,
    pub features: Vec<String>,
    pub default_features: bool,
    pub platform: Option<Platform>,
    pub explicit_name_in_toml: Option<String>,
}

impl Dependency {
    pub fn parse(name: &str, version: Option<&str>, source_id: SourceId) -> anyhow::Result<Self> {
        let req = version
            .map(|v| {
                VersionReq::parse(v).with_context(|| {
                    format!("failed to parse the version requirement `{v}` for dependency `{name}`")
                })
            })
            .transpose()?;
        Ok(Dependency {
            name: name.to_owned(),
            source_id,
            registry_id: None,
            req,
            kind: DepKind::Normal,
            optional: false,
            public: false,
            features: Vec::new(),
            default_features: true,
            platform: None,
            explicit_name_in_toml: None,
        })
    }

    /// What the manifest calls it: the key in its dependency table.
    pub fn name_in_toml(&self) -> &str {
        self.explicit_name_in_toml.as_deref().unwrap_or(&self.name)
    }

    /// Whether it is built for the package's own targets, which dev-dependencies are not.
    pub fn is_transitive(&self) -> bool {
        std::hint::black_box(self.kind != DepKind::Development)
    }

    pub fn serialized(&self) -> SerializedDependency<'_> {
        SerializedDependency {
            name: &self.name,
            source: (!self.source_id.is_path()).then(|| self.source_id.as_url().to_string()),
            req: match &self.req {
                Some(req) => req.to_string(),
                None => "*".to_owned(),
            },
            kind: self.kind,
            rename: self.explicit_name_in_toml.as_deref(),
            optional: self.optional,
            uses_default_features: self.default_features,
            features: &self.features,
            target: self.platform.as_ref(),
            registry: self.registry_id.as_ref().map(|id| id.url.to_string()),
            path: self.source_id.local_path(),
        }
    }
}

/// A dependency as `cargo metadata` prints it, fields in cargo's order.
#[derive(Serialize)]
pub struct SerializedDependency<'a> {
    name: &'a str,
    pub(crate) source: Option<String>,
    req: String,
    kind: DepKind,
    rename: Option<&'a str>,
    optional: bool,
    uses_default_features: bool,
    features: &'a [String],
    target: Option<&'a Platform>,
    registry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
}

/// One entry of a feature's list.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FeatureValue {
    Feature(String),
    Dep { dep_name: String },
    DepFeature { dep_name: String, dep_feature: String, weak: bool },
}

impl FeatureValue {
    pub fn new(feature: &str) -> FeatureValue {
        match feature.split_once('/') {
            Some((dep, dep_feat)) => {
                let dep_name = dep.strip_suffix('?');
                FeatureValue::DepFeature {
                    dep_name: dep_name.unwrap_or(dep).to_owned(),
                    dep_feature: dep_feat.to_owned(),
                    weak: dep_name.is_some(),
                }
            }
            None => match feature.strip_prefix("dep:") {
                Some(dep_name) => FeatureValue::Dep { dep_name: dep_name.to_owned() },
                None => FeatureValue::Feature(feature.to_owned()),
            },
        }
    }

    fn explicit_dep_name(&self) -> Option<&str> {
        match self {
            FeatureValue::Dep { dep_name } => Some(dep_name),
            _ => None,
        }
    }

    fn feature_or_dep_name(&self) -> &str {
        match self {
            FeatureValue::Feature(name) => name,
            FeatureValue::Dep { dep_name } | FeatureValue::DepFeature { dep_name, .. } => dep_name,
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

pub type FeatureMap = BTreeMap<String, Vec<FeatureValue>>;

/// The checks of cargo's `Summary::new`, then its feature map: the declared features, plus one
/// implicit feature per optional dependency that no `dep:` names.
pub fn build_summary_features(
    features: &BTreeMap<String, Vec<String>>,
    dependencies: &[Dependency],
) -> anyhow::Result<FeatureMap> {
    for dep in dependencies {
        if dep.optional && !dep.is_transitive() {
            bail!("dev-dependencies are not allowed to be optional: `{}`", dep.name_in_toml());
        }
    }

    let mut dep_map: FxHashMap<&str, bool> = FxHashMap::default();
    for dep in dependencies {
        *dep_map.entry(dep.name_in_toml()).or_insert(false) |= dep.optional;
    }

    let mut map: FeatureMap = features
        .iter()
        .map(|(feature, list)| {
            (feature.clone(), list.iter().map(|v| FeatureValue::new(v)).collect())
        })
        .collect();

    let explicitly_listed: FxHashSet<String> =
        map.values().flatten().filter_map(|fv| fv.explicit_dep_name()).map(str::to_owned).collect();

    for dep in dependencies {
        if !dep.optional {
            continue;
        }
        let dep_name = dep.name_in_toml();
        if features.contains_key(dep_name) || explicitly_listed.contains(dep_name) {
            continue;
        }
        map.insert(dep_name.to_owned(), vec![FeatureValue::Dep { dep_name: dep_name.to_owned() }]);
    }

    for (feature, fvs) in &map {
        FeatureName::new(feature.as_str())?;
        for fv in fvs {
            let dep_data = dep_map.get(fv.feature_or_dep_name());
            let is_any_dep = dep_data.is_some();
            let is_optional_dep = dep_data.is_some_and(|&o| o);
            match fv {
                FeatureValue::Feature(f) => {
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
                FeatureValue::Dep { dep_name } => {
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
                FeatureValue::DepFeature { dep_name, dep_feature, weak } => {
                    if dep_feature.contains('/') {
                        bail!(
                            "multiple slashes in feature `{fv}` (included by feature `{feature}`) are not allowed"
                        );
                    }
                    if let Some(stripped_dep) = dep_name.strip_prefix("dep:") {
                        let has_other_dep = explicitly_listed.contains(stripped_dep);
                        let is_optional = dep_map.get(stripped_dep).is_some_and(|&o| o);
                        let extra_help = if *weak || has_other_dep || !is_optional {
                            String::new()
                        } else {
                            format!(
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
                        bail!(
                            "feature `{feature}` includes `{fv}`, but `{dep_name}` is not a dependency"
                        );
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

    let used: FxHashSet<&str> = map
        .values()
        .flatten()
        .filter_map(|fv| match fv {
            FeatureValue::Dep { dep_name } | FeatureValue::DepFeature { dep_name, .. } => {
                Some(dep_name.as_str())
            }
            FeatureValue::Feature(_) => None,
        })
        .collect();
    if let Some((dep, _)) =
        dep_map.iter().find(|&(dep, &is_optional)| is_optional && !used.contains(dep))
    {
        bail!(
            "optional dependency `{dep}` is not included in any feature\n\
             Make sure that `dep:{dep}` is included in one of features in the [features] table."
        );
    }

    Ok(map)
}
