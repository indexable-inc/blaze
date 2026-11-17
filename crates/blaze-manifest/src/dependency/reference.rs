//! blaze-manifest's own `build_feature_map` before it called blaze-resolve's, over `String`s,
//! kept to check the shared one against: same map, same error, but for which unused optional
//! dependency an error names (this one took it from an `HashMap`, so the tests compare that
//! message only where one optional dependency is unused).

use std::collections::BTreeMap;
use std::fmt;

use blaze_hash::{FixedState, Map, MapOpts, Set};
use std::alloc::Global;

use super::{Dependency, DependencyError};
use crate::edit_distance::closest_msg;

/// What the old feature map refused, in its words (cargo's).
#[derive(Debug)]
pub(super) enum ReferenceError {
    Dependency(DependencyError),
    Name(blaze_resolve::ResolveError),
    NeitherDependencyNorFeature { feature: String, value: String, closest: String },
    OptionalWithoutImplicit { feature: String, value: String, name: String },
    NotOptionalFeature { feature: String, value: String, name: String },
    DepNotListed { feature: String, value: String, name: String },
    DepNotOptional { feature: String, value: String, name: String },
    MultipleSlashes { feature: String, value: String },
    DepAndSlash { feature: String, value: String, extra_help: String },
    NotADependency { feature: String, value: String, name: String },
    WeakNotOptional { feature: String, value: String, name: String },
    UnusedOptional { name: String },
}

impl fmt::Display for ReferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ReferenceError as E;
        match self {
            E::Dependency(e) => write!(f, "{e}"),
            E::Name(e) => write!(f, "{e}"),
            E::NeitherDependencyNorFeature { feature, value, closest } => write!(
                f,
                "feature `{feature}` includes `{value}` which is neither a dependency \
                 nor another feature{closest}"
            ),
            E::OptionalWithoutImplicit { feature, value, name } => write!(
                f,
                "feature `{feature}` includes `{value}`, but `{name}` is an \
                 optional dependency without an implicit feature\n\
                 Use `dep:{name}` to enable the dependency."
            ),
            E::NotOptionalFeature { feature, value, name } | E::DepNotOptional { feature, value, name } => write!(
                f,
                "feature `{feature}` includes `{value}`, but `{name}` is not an optional dependency\n\
                 A non-optional dependency of the same name is defined; \
                 consider adding `optional = true` to its definition."
            ),
            E::DepNotListed { feature, value, name } => write!(
                f,
                "feature `{feature}` includes `{value}`, but `{name}` is not listed as a dependency"
            ),
            E::MultipleSlashes { feature, value } => write!(
                f,
                "multiple slashes in feature `{value}` (included by feature `{feature}`) are not allowed"
            ),
            E::DepAndSlash { feature, value, extra_help } => write!(
                f,
                "feature `{feature}` includes `{value}` with both `dep:` and `/`\n\
                 To fix this, remove the `dep:` prefix.{extra_help}"
            ),
            E::NotADependency { feature, value, name } => write!(
                f,
                "feature `{feature}` includes `{value}`, but `{name}` is not a dependency"
            ),
            E::WeakNotOptional { feature, value, name } => write!(
                f,
                "feature `{feature}` includes `{value}` with a `?`, but `{name}` is not an optional dependency\n\
                 A non-optional dependency of the same name is defined; \
                 consider removing the `?` or changing the dependency to be optional"
            ),
            E::UnusedOptional { name } => write!(
                f,
                "optional dependency `{name}` is not included in any feature\n\
                 Make sure that `dep:{name}` is included in one of features in the [features] table."
            ),
        }
    }
}

impl core::error::Error for ReferenceError {}

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
) -> Result<FeatureMap, ReferenceError> {
    use ReferenceError as E;
    for dep in dependencies {
        if dep.optional && !dep.is_transitive() {
            let name = dep.name_in_toml().to_owned();
            return Err(E::Dependency(DependencyError::OptionalDevDependency { name }));
        }
    }

    let mut dep_map: Map<&str, bool, FixedState, Global> = Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None });
    for dep in dependencies {
        *dep_map.entry(dep.name_in_toml()).or_insert(false) |= dep.optional;
    }

    let mut map: FeatureMap = features
        .iter()
        .map(|(feature, list)| {
            (feature.clone(), list.iter().map(|v| FeatureValue::new(v)).collect())
        })
        .collect();

    let explicitly_listed: Set<String, FixedState, Global> =
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
        blaze_resolve::validate_feature_name(feature.as_str()).map_err(E::Name)?;
        for fv in fvs {
            let dep_data = dep_map.get(fv.feature_or_dep_name());
            let is_any_dep = dep_data.is_some();
            let is_optional_dep = dep_data.is_some_and(|&o| o);
            let (feature, value) = (feature.clone(), fv.to_string());
            match fv {
                FeatureValue::Feature(f) => {
                    if !features.contains_key(f) {
                        if !is_any_dep {
                            let closest = closest_msg(f, features.keys(), |k| k, "feature");
                            return Err(E::NeitherDependencyNorFeature { feature, value, closest });
                        }
                        if is_optional_dep {
                            if !map.contains_key(f) {
                                let name = f.clone();
                                return Err(E::OptionalWithoutImplicit { feature, value, name });
                            }
                        } else {
                            return Err(E::NotOptionalFeature { feature, value, name: f.clone() });
                        }
                    }
                }
                FeatureValue::Dep { dep_name } => {
                    if !is_any_dep {
                        return Err(E::DepNotListed { feature, value, name: dep_name.clone() });
                    }
                    if !is_optional_dep {
                        return Err(E::DepNotOptional { feature, value, name: dep_name.clone() });
                    }
                }
                FeatureValue::DepFeature { dep_name, dep_feature, weak } => {
                    if dep_feature.contains('/') {
                        return Err(E::MultipleSlashes { feature, value });
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
                        return Err(E::DepAndSlash { feature, value, extra_help });
                    }
                    if !is_any_dep {
                        return Err(E::NotADependency { feature, value, name: dep_name.clone() });
                    }
                    if *weak && !is_optional_dep {
                        return Err(E::WeakNotOptional { feature, value, name: dep_name.clone() });
                    }
                }
            }
        }
    }

    let used: Set<&str, FixedState, Global> = map
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
        return Err(E::UnusedOptional { name: (*dep).to_owned() });
    }

    Ok(map)
}
