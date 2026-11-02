//! blaze-manifest's own `build_feature_map` before it called blaze-resolve's, over `String`s,
//! kept to check the shared one against: same map, same error, but for which unused optional
//! dependency an error names (this one took it from an `HashMap`, so the tests compare that
//! message only where one optional dependency is unused).

use std::collections::BTreeMap;
use std::fmt;

use anyhow::bail;
use blaze_hash::{HashMap, HashSet};
use cargo_util_schemas::manifest::FeatureName;

use super::Dependency;
use crate::edit_distance::closest_msg;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum FeatureValue {
    Feature(String),
    Dep { dep_name: String },
    DepFeature { dep_name: String, dep_feature: String, weak: bool },
}

impl FeatureValue {
    fn new(feature: &str) -> FeatureValue {
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

pub(super) type FeatureMap = BTreeMap<String, Vec<FeatureValue>>;

pub(super) fn build_summary_features(
    features: &BTreeMap<String, Vec<String>>,
    dependencies: &[Dependency],
) -> anyhow::Result<FeatureMap> {
    for dep in dependencies {
        if dep.optional && !dep.is_transitive() {
            bail!("dev-dependencies are not allowed to be optional: `{}`", dep.name_in_toml());
        }
    }

    let mut dep_map: HashMap<&str, bool> = HashMap::default();
    for dep in dependencies {
        *dep_map.entry(dep.name_in_toml()).or_insert(false) |= dep.optional;
    }

    let mut map: FeatureMap = features
        .iter()
        .map(|(feature, list)| {
            (feature.clone(), list.iter().map(|v| FeatureValue::new(v)).collect())
        })
        .collect();

    let explicitly_listed: HashSet<String> =
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

    let used: HashSet<&str> = map
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
