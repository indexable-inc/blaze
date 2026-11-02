//! Which workspace members `-p`, `--workspace` and `--features` ask for, and the features of
//! each: `Workspace::members_with_features` of cargo 1.99.0 (5f94df4789),
//! `src/workspace/workspace.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::rc::Rc;

use anyhow::bail;

use super::features::CliFeatures;
use super::{CargoResult, HashMap, HashSet};
use crate::dependency::Dependency;
use crate::edit_distance::{closest_msg, edit_distance};
use crate::interning::InternedString;
use crate::package_id::PackageId;
use crate::package_id_spec::PackageIdSpec;
use crate::summary::{FeatureValue, Summary};

/// The workspace, as far as choosing its members' features goes.
#[derive(Clone, Copy)]
pub struct Members<'a> {
    /// The members, in the workspace's order.
    pub members: &'a [&'a Summary],
    /// The member whose manifest is in the current directory, none in a virtual workspace's
    /// root.
    pub current: Option<PackageId>,
    /// `resolver = "2"` or later, or a virtual workspace: `--features` applies to every package
    /// selected (cargo's `allows_new_cli_feature_behavior`).
    pub new_cli_feature_behavior: bool,
}

/// Returns a Vec of `(&Summary, CliFeatures)` tuples that
/// represent the workspace members that were requested on the command-line.
///
/// `specs` may be empty, which indicates it should return all workspace
/// members. In this case, `requested_features.all_features` must be
/// `true`. This is used for generating `Cargo.lock`, which must include
/// all members with all features enabled.
pub fn members_with_features<'a>(
    ws: &Members<'a>,
    specs: &[PackageIdSpec],
    cli_features: &CliFeatures,
) -> CargoResult<Vec<(&'a Summary, CliFeatures)>> {
    assert!(!specs.is_empty() || cli_features.all_features, "no specs requires all_features");
    if specs.is_empty() {
        // When resolving the entire workspace, resolve each member with
        // all features enabled.
        return Ok(ws.members.iter().map(|&m| (m, CliFeatures::new_all(true))).collect());
    }
    if ws.new_cli_feature_behavior {
        members_with_features_new(ws, specs, cli_features)
    } else {
        Ok(members_with_features_old(ws, specs, cli_features))
    }
}

/// Returns the requested features for the given member.
/// This filters out any named features that the member does not have.
fn collect_matching_features(
    member: &Summary,
    cli_features: &CliFeatures,
    found_features: &mut BTreeSet<FeatureValue>,
) -> CliFeatures {
    if cli_features.features.is_empty() {
        return cli_features.clone();
    }

    // Features defined in the manifest
    let summary_features = member.features();

    // Dependency name -> dependency
    let dependencies: HashMap<InternedString, &Dependency> =
        member.dependencies().iter().map(|dep| (dep.name_in_toml(), dep)).collect();

    // Features that enable optional dependencies
    let optional_dependency_names: HashSet<_> = dependencies
        .iter()
        .filter(|(_, dep)| dep.is_optional())
        .map(|(name, _)| name)
        .copied()
        .collect();

    // BTreeSet: becomes `CliFeatures::features`, which is hashed and compared.
    let mut features = BTreeSet::new();

    // Checks if a member contains the given feature.
    let summary_or_opt_dependency_feature = |feature: &InternedString| -> bool {
        summary_features.contains_key(feature) || optional_dependency_names.contains(feature)
    };

    for feature in cli_features.features.iter() {
        match feature {
            FeatureValue::Feature(f) => {
                if summary_or_opt_dependency_feature(f) {
                    // feature exists in this member.
                    features.insert(feature.clone());
                    found_features.insert(feature.clone());
                }
            }
            // This should be enforced by CliFeatures.
            FeatureValue::Dep { .. } => panic!("unexpected dep: syntax {}", feature),
            FeatureValue::DepFeature { dep_name, dep_feature, weak: _ } => {
                if dependencies.contains_key(dep_name) {
                    // pkg/feat for a dependency.
                    // Will rely on the dependency resolver to validate `dep_feature`.
                    features.insert(feature.clone());
                    found_features.insert(feature.clone());
                } else if *dep_name == member.name()
                    && summary_or_opt_dependency_feature(dep_feature)
                {
                    // member/feat where "feat" is a feature in member.
                    //
                    // `weak` can be ignored here, because the member
                    // either is or isn't being built.
                    features.insert(FeatureValue::Feature(*dep_feature));
                    found_features.insert(feature.clone());
                }
            }
        }
    }
    CliFeatures {
        features: Rc::new(features),
        all_features: cli_features.all_features,
        uses_default_features: cli_features.uses_default_features,
    }
}

fn missing_feature_spelling_suggestions(
    selected_members: &[&Summary],
    cli_features: &CliFeatures,
    found_features: &BTreeSet<FeatureValue>,
) -> Vec<String> {
    // Keeps track of which features were contained in summary of `member` to suggest similar features in errors
    let mut summary_features: Vec<InternedString> = Default::default();

    // Keeps track of `member` dependencies (`dep/feature`) and their features names to suggest similar features in error
    let mut dependencies_features: BTreeMap<InternedString, &[InternedString]> = Default::default();

    // Keeps track of `member` optional dependencies names (which can be enabled with feature) to suggest similar features in error
    let mut optional_dependency_names: Vec<InternedString> = Default::default();

    // Keeps track of which features were contained in summary of `member` to suggest similar features in errors
    let mut summary_features_per_member: BTreeMap<PackageId, BTreeSet<InternedString>> =
        Default::default();

    // Keeps track of `member` optional dependencies (which can be enabled with feature) to suggest similar features in error
    let mut optional_dependency_names_per_member: BTreeMap<PackageId, BTreeSet<InternedString>> =
        Default::default();

    for &member in selected_members {
        // Features defined in the manifest
        summary_features.extend(member.features().keys());
        summary_features_per_member
            .insert(member.package_id(), member.features().keys().copied().collect());

        // Dependency name -> dependency
        let dependencies: BTreeMap<InternedString, &Dependency> =
            member.dependencies().iter().map(|dep| (dep.name_in_toml(), dep)).collect();

        dependencies_features
            .extend(dependencies.iter().map(|(name, dep)| (*name, dep.features())));

        // Features that enable optional dependencies
        let optional_dependency_names_raw: BTreeSet<_> = dependencies
            .iter()
            .filter(|(_, dep)| dep.is_optional())
            .map(|(name, _)| name)
            .copied()
            .collect();

        optional_dependency_names.extend(optional_dependency_names_raw.iter());
        optional_dependency_names_per_member
            .insert(member.package_id(), optional_dependency_names_raw);
    }

    let edit_distance_test =
        |a: InternedString, b: InternedString| edit_distance(a.as_str(), b.as_str(), 3).is_some();

    let mut suggestions: Vec<String> = cli_features
        .features
        .difference(found_features)
        .flat_map(|feature| match feature {
            // Simple feature, check if any of the optional dependency features or member features are close enough
            FeatureValue::Feature(typo) => {
                // Finds member features which are similar to the requested feature.
                let summary_features = summary_features
                    .iter()
                    .filter(move |feature| edit_distance_test(**feature, *typo));

                // Finds optional dependencies which name is similar to the feature
                let optional_dependency_features = optional_dependency_names
                    .iter()
                    .filter(move |feature| edit_distance_test(**feature, *typo));

                summary_features
                    .chain(optional_dependency_features)
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            }
            FeatureValue::Dep { .. } => panic!("unexpected dep: syntax {}", feature),
            FeatureValue::DepFeature { dep_name, dep_feature, weak: _ } => {
                // Finds set of `pkg/feat` that are very similar to current `pkg/feat`.
                let pkg_feat_similar = dependencies_features
                    .iter()
                    .filter(|(name, _)| edit_distance_test(**name, *dep_name))
                    .flat_map(|(name, features)| {
                        features
                            .iter()
                            .filter(|feature| edit_distance_test(**feature, *dep_feature))
                            .map(move |feature| format!("{}/{}", name, feature))
                    });

                // Finds set of `member/optional_dep` features which name is similar to current `pkg/feat`.
                let optional_dependency_features = optional_dependency_names_per_member
                    .iter()
                    .filter(|(package, _)| edit_distance_test(package.name(), *dep_name))
                    .flat_map(|(package, optional_dependencies)| {
                        optional_dependencies
                            .iter()
                            .filter(|optional_dependency| {
                                edit_distance_test(**optional_dependency, *dep_name)
                            })
                            .map(move |optional_dependency| {
                                format!("{}/{}", package.name(), optional_dependency)
                            })
                    });

                // Finds set of `member/feat` features which name is similar to current `pkg/feat`.
                let summary_features = summary_features_per_member
                    .iter()
                    .filter(|(package, _)| edit_distance_test(package.name(), *dep_name))
                    .flat_map(|(package, summary_features)| {
                        summary_features
                            .iter()
                            .filter(|summary_feature| {
                                edit_distance_test(**summary_feature, *dep_feature)
                            })
                            .map(move |summary_feature| {
                                format!("{}/{}", package.name(), summary_feature)
                            })
                    });

                pkg_feat_similar
                    .chain(optional_dependency_features)
                    .chain(summary_features)
                    .collect::<Vec<_>>()
            }
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|element| {
            let feature = FeatureValue::new(element.as_str().into());
            !cli_features.features.contains(&feature) && !found_features.contains(&feature)
        })
        .collect();
    suggestions.truncate(5);
    suggestions
}

fn report_unknown_features_error(
    ws: &Members<'_>,
    specs: &[PackageIdSpec],
    cli_features: &CliFeatures,
    found_features: &BTreeSet<FeatureValue>,
) -> CargoResult<()> {
    let mut unknown: Vec<_> = cli_features
        .features
        .difference(found_features)
        .map(|feature| feature.to_string())
        .collect();
    unknown.sort();

    let (selected_members, unselected_members): (Vec<&Summary>, Vec<&Summary>) = ws
        .members
        .iter()
        .partition(|member| specs.iter().any(|spec| spec.matches(member.package_id())));

    let missing_packages_with_the_features: Vec<_> = unselected_members
        .into_iter()
        .filter(|member| {
            unknown.iter().any(|feature| member.features().contains_key(feature.as_str()))
        })
        .map(|m| m.name().to_string())
        .collect();

    let these_features = if unknown.len() == 1 { "this feature" } else { "these features" };
    let mut msg = if let [singular] = &selected_members[..] {
        format!(
            "the package '{}' does not contain {these_features}: {}",
            singular.name(),
            unknown.join(", ")
        )
    } else {
        let names =
            selected_members.iter().map(|m| m.name().to_string()).collect::<Vec<_>>().join(", ");
        format!(
            "none of the selected packages contains {these_features}: {}\nselected packages: {names}",
            unknown.join(", ")
        )
    };

    if !missing_packages_with_the_features.is_empty() {
        write!(
            &mut msg,
            "\nhelp: package{} with the missing feature{}: {}",
            if missing_packages_with_the_features.len() != 1 { "s" } else { "" },
            if unknown.len() != 1 { "s" } else { "" },
            missing_packages_with_the_features.join(", ")
        )?;
    } else {
        let suggestions =
            missing_feature_spelling_suggestions(&selected_members, cli_features, found_features);
        if !suggestions.is_empty() {
            write!(
                &mut msg,
                "\nhelp: there {}: {}",
                if suggestions.len() == 1 {
                    "is a similarly named feature"
                } else {
                    "are similarly named features"
                },
                suggestions.join(", ")
            )?;
        }
    }

    bail!("{msg}")
}

/// New command-line feature selection behavior with resolver = "2" or the
/// root of a virtual workspace. See `allows_new_cli_feature_behavior`.
fn members_with_features_new<'a>(
    ws: &Members<'a>,
    specs: &[PackageIdSpec],
    cli_features: &CliFeatures,
) -> CargoResult<Vec<(&'a Summary, CliFeatures)>> {
    // Keeps track of which features matched `member` to produce an error
    // if any of them did not match anywhere.
    let mut found_features = Default::default();

    let members: Vec<(&Summary, CliFeatures)> = ws
        .members
        .iter()
        .filter(|m| specs.iter().any(|spec| spec.matches(m.package_id())))
        .map(|&m| (m, collect_matching_features(m, cli_features, &mut found_features)))
        .collect();

    if members.is_empty() {
        // `cargo build -p foo`, where `foo` is not a member.
        // Do not allow any command-line flags (defaults only).
        if !(cli_features.features.is_empty()
            && !cli_features.all_features
            && cli_features.uses_default_features)
        {
            let hint = specs
                .iter()
                .map(|spec| {
                    closest_msg(
                        spec.name(),
                        ws.members.iter(),
                        |m| m.name().as_str(),
                        "workspace member",
                    )
                })
                .find(|msg| !msg.is_empty())
                .unwrap_or_default();
            bail!("cannot specify features for packages outside of workspace{hint}");
        }
        // Add all members from the workspace so we can ensure `-p nonmember`
        // is in the resolve graph.
        return Ok(ws.members.iter().map(|&m| (m, CliFeatures::new_all(false))).collect());
    }
    if *cli_features.features != found_features {
        report_unknown_features_error(ws, specs, cli_features, &found_features)?;
    }
    Ok(members)
}

/// This is the "old" behavior for command-line feature selection.
/// See `allows_new_cli_feature_behavior`.
fn members_with_features_old<'a>(
    ws: &Members<'a>,
    specs: &[PackageIdSpec],
    cli_features: &CliFeatures,
) -> Vec<(&'a Summary, CliFeatures)> {
    // Split off any features with the syntax `member-name/feature-name` into a map
    // so that those features can be applied directly to those workspace-members.
    let mut member_specific_features: HashMap<InternedString, BTreeSet<FeatureValue>> =
        HashMap::default();
    // Features for the member in the current directory.
    let mut cwd_features = BTreeSet::new();
    for feature in cli_features.features.iter() {
        match feature {
            FeatureValue::Feature(_) => {
                cwd_features.insert(feature.clone());
            }
            // This should be enforced by CliFeatures.
            FeatureValue::Dep { .. } => panic!("unexpected dep: syntax {}", feature),
            FeatureValue::DepFeature { dep_name, dep_feature, weak: _ } => {
                // I think weak can be ignored here.
                // * With `--features member?/feat -p member`, the ? doesn't
                //   really mean anything (either the member is built or it isn't).
                // * With `--features nonmember?/feat`, cwd_features will
                //   handle processing it correctly.
                let is_member = ws.members.iter().any(|member| {
                    // Check if `dep_name` is member of the workspace, but isn't associated with current package.
                    ws.current != Some(member.package_id()) && member.name() == *dep_name
                });
                if is_member && specs.iter().any(|spec| spec.name() == dep_name.as_str()) {
                    member_specific_features
                        .entry(*dep_name)
                        .or_default()
                        .insert(FeatureValue::Feature(*dep_feature));
                } else {
                    cwd_features.insert(feature.clone());
                }
            }
        }
    }

    let ms: Vec<_> = ws
        .members
        .iter()
        .filter_map(|&member| {
            let member_id = member.package_id();
            match ws.current {
                // The features passed on the command-line only apply to
                // the "current" package (determined by the cwd).
                Some(current) if member_id == current => {
                    let feats = CliFeatures {
                        features: Rc::new(cwd_features.clone()),
                        all_features: cli_features.all_features,
                        uses_default_features: cli_features.uses_default_features,
                    };
                    Some((member, feats))
                }
                _ => {
                    // Ignore members that are not enabled on the command-line.
                    if specs.iter().any(|spec| spec.matches(member_id)) {
                        // -p for a workspace member that is not the "current"
                        // one.
                        //
                        // The odd behavior here is due to backwards
                        // compatibility. `--features` and
                        // `--no-default-features` used to only apply to the
                        // "current" package. As an extension, this allows
                        // member-name/feature-name to set member-specific
                        // features, which should be backwards-compatible.
                        let feats = CliFeatures {
                            features: Rc::new(
                                member_specific_features.remove(&member.name()).unwrap_or_default(),
                            ),
                            uses_default_features: true,
                            all_features: cli_features.all_features,
                        };
                        Some((member, feats))
                    } else {
                        // This member was not requested on the command-line, skip.
                        None
                    }
                }
            }
        })
        .collect();

    // If any member specific features were not removed while iterating over members
    // some features will be ignored.
    assert!(member_specific_features.is_empty());

    ms
}
