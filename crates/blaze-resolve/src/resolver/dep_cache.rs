//! There are 2 sources of facts for the resolver:
//!
//! - The `Registry` tells us for a `Dependency` what versions are available to fulfil it.
//! - The `Summary` tells us for a version (and features) what dependencies need to be fulfilled for it to be activated.
//!
//! These constitute immutable facts, the soled ground truth that all other inference depends on.
//! Theoretically this could all be enumerated ahead of time, but we want to be lazy and only
//! look up things we need to. The compromise is to cache the results as they are computed.
//!
//! This module impl that cache in all the gory details

use crate::edit_distance::closest_msg;
use crate::interning::InternedString;
use crate::registry::IndexSummary;
use crate::registry::{Lookup, QueryKind};
use crate::resolver::CargoResult;
use crate::resolver::context::ResolverContext;
use crate::resolver::errors::describe_path_in_context;
use crate::resolver::types::{ConflictReason, DepInfo, FeaturesSet};
use crate::resolver::{
    ActivateError, ActivateResult, CliFeatures, RequestedFeatures, ResolveOpts, VersionOrdering,
    VersionPreferences,
};
use crate::{Dependency, FeatureValue, PackageId, PackageIdSpec, Registry, ResolveError, Summary};

use crate::resolver::{FixedState, Map, MapOpts, Set, SetOpts};
use blaze_ids::radix;
use blaze_own::{LocalShare, Share};
use std::alloc::Global;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::rc::Rc;
use blaze_trace::debug;

type QueryKey = (Dependency, Option<VersionOrdering>);

/// The registry as the resolver queries it: `[replace]` applied, each dependency's candidates
/// sorted and kept, and a cache of the dependencies a summary needs.
pub(super) struct RegistryQueryer<'a, T: Registry> {
    registry: &'a T,
    replacements: &'a [(PackageIdSpec, Dependency)],
    version_prefs: &'a VersionPreferences,
    /// all the cases we ended up using a supplied replacement
    used_replacements: RefCell<Map<PackageId, Summary, FixedState, Global>>,
    /// Each query's candidates, once answered. A failure is not kept: the registry is asked
    /// again and answers it again.
    candidates: Map<QueryKey, Rc<[Summary]>, FixedState, Global>,

    /// a cache of `Dependency`s that are required for a `Summary`
    ///
    /// HACK: `first_version` is not kept in the cache key is it is 1:1 with
    /// `parent.is_none()` (the first element of the cache key) as it doesn't change through
    /// execution. An entry made while a query was [`Lookup::Unknown`] lacks that dependency;
    /// it stands for this round only, which the resolve machine throws away.
    summary_cache: Map<(Option<PackageId>, Summary, ResolveOpts), Rc<(Set<InternedString, FixedState, Global>, Rc<[DepInfo]>)>, FixedState, Global>,
}

impl<'a, T: Registry> RegistryQueryer<'a, T> {
    pub(super) fn new(
        registry: &'a T,
        replacements: &'a [(PackageIdSpec, Dependency)],
        version_prefs: &'a VersionPreferences,
    ) -> Self {
        RegistryQueryer {
            registry,
            replacements,
            version_prefs,
            used_replacements: RefCell::new(Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None })),
            candidates: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }),
            summary_cache: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }),
        }
    }

    pub(super) fn registry(&self) -> &T {
        self.registry
    }

    /// The candidates for `dep`, sorted, from the cache or the registry.
    pub(super) fn query(
        &mut self,
        dep: &Dependency,
        first_version: Option<VersionOrdering>,
    ) -> CargoResult<Lookup<Rc<[Summary]>>> {
        let key = (dep.share(), first_version);
        if let Some(found) = self.candidates.get(&key) {
            return Ok(Lookup::Answered(Rc::clone(found)));
        }
        let found = match self.ask(&key)? {
            Lookup::Answered(found) => found,
            Lookup::Unknown => return Ok(Lookup::Unknown),
        };
        self.candidates.insert(key, Rc::clone(&found));
        Ok(Lookup::Answered(found))
    }

    /// Queries the `registry` to return a list of candidates for `dep`.
    ///
    /// This method is the location where overrides are taken into account. If
    /// any candidates are returned which match an override then the override is
    /// applied by performing a second query for what the override should
    /// return.
    fn ask(&self, key: &QueryKey) -> CargoResult<Lookup<Rc<[Summary]>>> {
        let (dep, first_version) = key;
        let mut summaries = Vec::new();
        let asked = self
            .registry
            .query(dep, QueryKind::Exact, &mut |s| match s {
                IndexSummary::Candidate(summary) => summaries.push(summary),
                // Prefer yanked only when
                //
                // * it is recorded in lock file or a `[patch]` entry
                // * it is specified in `cargo update --precise`
                IndexSummary::Yanked(summary) => {
                    let pkg_id = summary.package_id();
                    let allow_precise = pkg_id
                        .source_id()
                        .precise_registry_version(pkg_id.name().as_str())
                        .is_some_and(|(_, to)| to == pkg_id.version());
                    if allow_precise || self.version_prefs.should_prefer(&pkg_id) {
                        summaries.push(summary);
                    }
                }
                _ => {}
            })?;
        if asked == Lookup::Unknown {
            return Ok(Lookup::Unknown);
        }

        for summary in summaries.iter() {
            let mut potential_matches =
                self.replacements.iter().filter(|(spec, _)| spec.matches(summary.package_id()));

            let Some((spec, dep)) = potential_matches.next() else {
                continue;
            };
            debug!("found an override for {} {}", dep.package_name(), dep.version_req());

            let replacing = match self.registry.query_vec(dep, QueryKind::Exact)? {
                Lookup::Answered(replacing) => replacing,
                Lookup::Unknown => return Ok(Lookup::Unknown),
            };
            let mut summaries = replacing.into_iter().filter_map(|s| match s {
                IndexSummary::Candidate(s) => Some(s),
                _ => None,
            });
            let s = summaries.next().ok_or_else(|| ResolveError::OverrideNotFound {
                spec: spec.clone(), // clone: error path
                source: dep.source_id(),
                req: dep.version_req().clone(), // clone: error path
            })?;
            let summaries = summaries.collect::<Vec<_>>();
            if !summaries.is_empty() {
                return Err(ResolveError::OverrideAmbiguous {
                    spec: spec.clone(), // clone: error path
                    first: s.package_id(),
                    rest: summaries.iter().map(|s| s.package_id()).collect(),
                });
            }

            assert_eq!(
                s.name(),
                summary.name(),
                "dependency should be hard coded to have the same name"
            );
            if s.version() != summary.version() {
                return Err(ResolveError::OverrideVersion {
                    spec: spec.clone(), // clone: error path
                    version: summary.version().clone(),
                    with: s.version().clone(),
                });
            }

            let replace = if s.source_id() == summary.source_id() {
                debug!("Preventing\n{:?}\nfrom replacing\n{:?}", summary, s);
                None
            } else {
                Some(s)
            };
            let matched_spec = spec.clone();

            // Make sure no duplicates
            if let Some((spec, _)) = potential_matches.next() {
                return Err(ResolveError::OverrideOverlap {
                    first: matched_spec,
                    second: spec.clone(), // clone: error path
                    package: summary.package_id(),
                });
            }

            for dep in summary.dependencies() {
                debug!("\t{} => {}", dep.package_name(), dep.version_req());
            }
            if let Some(r) = replace {
                self.used_replacements.borrow_mut().insert(summary.package_id(), r);
            }
        }

        self.version_prefs.sort_summaries(&mut summaries, *first_version);
        Ok(Lookup::Answered(summaries.into()))
    }

    pub(super) fn used_replacement_for(&self, p: PackageId) -> Option<(PackageId, PackageId)> {
        self.used_replacements.borrow().get(&p).map(|r| (p, r.package_id()))
    }

    pub(super) fn replacement_summary(&self, p: PackageId) -> Option<Summary> {
        self.used_replacements.borrow().get(&p).cloned()
    }

    /// Find out what dependencies will be added by activating `candidate`,
    /// with features described in `opts`. Then look up in the `registry`
    /// the candidates that will fulfil each of these dependencies, as it is the
    /// next obvious question.
    pub(super) fn build_deps(
        &mut self,
        cx: &ResolverContext,
        parent: Option<PackageId>,
        candidate: &Summary,
        opts: &ResolveOpts,
        first_version: Option<VersionOrdering>,
    ) -> ActivateResult<Rc<(Set<InternedString, FixedState, Global>, Rc<[DepInfo]>)>> {
        // if we have calculated a result before, then we can just return it,
        // as it is a "pure" query of its arguments.
        if let Some(out) = self.summary_cache.get(&(parent, candidate.share(), opts.local_share())) {
            return Ok(Rc::clone(out));
        }
        // First, figure out our set of dependencies based on the requested set
        // of features. This also calculates what features we're going to enable
        // for our own dependencies.
        let (used_features, deps) = resolve_features(parent, candidate, opts)?;

        // Next, transform all dependencies into a list of possible candidates
        // which can satisfy that dependency.
        let mut deps = deps
            .into_iter()
            .filter_map(|(dep, features)| match self.query(&dep, first_version) {
                Ok(Lookup::Answered(candidates)) => Some(Ok((dep, candidates, features))),
                // Left out: the resolve machine asks for it and throws this round away.
                Ok(Lookup::Unknown) => None,
                Err(e) => Some(Err(ResolveError::DependencyQuery {
                    dep: dep.package_name(),
                    path: describe_path_in_context(cx, &candidate.package_id()),
                    cause: Box::new(e),
                })),
            })
            .collect::<CargoResult<Vec<DepInfo>>>()?;

        // Attempt to resolve dependencies with fewer candidates before trying
        // dependencies with more candidates. This way if the dependency with
        // only one candidate can't be resolved we don't have to do a bunch of
        // work before we figure that out.
        radix::sort_by_key(&mut deps, |(_, a, _)| a.len());

        let out = Rc::new((used_features, deps.into()));

        // If we succeed we add the result to the cache so we can use it again next time.
        // We don't cache the failure cases as they don't impl Clone.
        self.summary_cache.insert((parent, candidate.share(), opts.local_share()), Rc::clone(&out));

        Ok(out)
    }
}

/// Returns the features we ended up using and
/// all dependencies and the features we want from each of them.
pub(super) fn resolve_features<'b>(
    parent: Option<PackageId>,
    s: &'b Summary,
    opts: &'b ResolveOpts,
) -> ActivateResult<(Set<InternedString, FixedState, Global>, Vec<(Dependency, FeaturesSet)>)> {
    // First, filter by dev-dependencies.
    let deps = s.dependencies();
    let deps = deps.iter().filter(|d| d.is_transitive() || opts.dev_deps);

    let reqs = build_requirements(parent, s, opts)?;
    let mut ret = Vec::new();
    let default_dep = BTreeSet::new();
    let mut valid_dep_names = Set::new(SetOpts { hasher: FixedState, alloc: Global, capacity: None });

    // Next, collect all actually enabled dependencies and their features.
    for dep in deps {
        // Skip optional dependencies, but not those enabled through a
        // feature
        if dep.is_optional() && !reqs.deps.contains_key(&dep.name_in_toml()) {
            continue;
        }
        valid_dep_names.insert(dep.name_in_toml());
        // So we want this dependency. Move the features we want from
        // `feature_deps` to `ret` and register ourselves as using this
        // name.
        let mut base = reqs.deps.get(&dep.name_in_toml()).unwrap_or(&default_dep).clone();
        base.extend(dep.features().iter());
        ret.push((dep.share(), Rc::new(base)));
    }

    // This is a special case for command-line `--features
    // dep_name/feat_name` where `dep_name` does not exist. All other
    // validation is done either in `build_requirements` or
    // `build_feature_map`.
    if parent.is_none() {
        for dep_name in reqs.deps.keys() {
            if !valid_dep_names.contains(dep_name) {
                let e = RequirementError::MissingDependency(*dep_name);
                return Err(e.into_activate_error(parent, s));
            }
        }
    }

    Ok((reqs.into_features(), ret))
}

/// Takes requested features for a single package from the input `ResolveOpts` and
/// recurses to find all requested features, dependencies and requested
/// dependency features in a `Requirements` object, returning it to the resolver.
fn build_requirements<'a, 'b: 'a>(
    parent: Option<PackageId>,
    s: &'a Summary,
    opts: &'b ResolveOpts,
) -> ActivateResult<Requirements<'a>> {
    let mut reqs = Requirements::new(s);

    let handle_default = |uses_default_features, reqs: &mut Requirements<'_>| {
        if uses_default_features
            && s.features().contains_key("default")
            && let Err(e) = reqs.require_feature(InternedString::new("default"))
        {
            return Err(e.into_activate_error(parent, s));
        }
        Ok(())
    };

    match &opts.features {
        RequestedFeatures::CliFeatures(CliFeatures {
            features,
            all_features,
            uses_default_features,
        }) => {
            if *all_features {
                for key in s.features().keys() {
                    if let Err(e) = reqs.require_feature(*key) {
                        return Err(e.into_activate_error(parent, s));
                    }
                }
            }

            for fv in features.iter() {
                if let Err(e) = reqs.require_value(fv) {
                    return Err(e.into_activate_error(parent, s));
                }
            }
            handle_default(*uses_default_features, &mut reqs)?;
        }
        RequestedFeatures::DepFeatures { features, uses_default_features } => {
            for feature in features.iter() {
                if let Err(e) = reqs.require_feature(*feature) {
                    return Err(e.into_activate_error(parent, s));
                }
            }
            handle_default(*uses_default_features, &mut reqs)?;
        }
    }

    Ok(reqs)
}

/// Set of feature and dependency requirements for a package.
#[derive(Debug)]
struct Requirements<'a> {
    summary: &'a Summary,
    /// The deps map is a mapping of dependency name to list of features enabled.
    ///
    /// The resolver will activate all of these dependencies, with the given
    /// features enabled.
    // BTreeSet: each becomes a `FeaturesSet`, part of the `DepsCache` key.
    deps: Map<InternedString, BTreeSet<InternedString>, FixedState, Global>,
    /// The set of features enabled on this package which is later used when
    /// compiling to instruct the code what features were enabled.
    features: Set<InternedString, FixedState, Global>,
}

/// An error for a requirement.
///
/// This will later be converted to an `ActivateError` depending on whether or
/// not this is a dependency or a root package.
enum RequirementError {
    /// The package does not have the requested feature.
    MissingFeature(InternedString),
    /// The package does not have the requested dependency.
    MissingDependency(InternedString),
    /// A feature has a direct cycle to itself.
    ///
    /// Note that cycles through multiple features are allowed (but perhaps
    /// they shouldn't be?).
    Cycle(InternedString),
}

impl Requirements<'_> {
    fn new(summary: &Summary) -> Requirements<'_> {
        Requirements { summary, deps: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }), features: Set::new(SetOpts { hasher: FixedState, alloc: Global, capacity: None }) }
    }

    fn into_features(self) -> Set<InternedString, FixedState, Global> {
        self.features
    }

    fn require_dep_feature(
        &mut self,
        package: InternedString,
        feat: InternedString,
        weak: bool,
    ) -> Result<(), RequirementError> {
        // If `package` is indeed an optional dependency then we activate the
        // feature named `package`, but otherwise if `package` is a required
        // dependency then there's no feature associated with it.
        if !weak
            && self
                .summary
                .dependencies()
                .iter()
                .any(|dep| dep.name_in_toml() == package && dep.is_optional())
        {
            // This optional dependency may not have an implicit feature of
            // the same name if the `dep:` syntax is used to avoid creating
            // that implicit feature.
            if self.summary.features().contains_key(&package) {
                self.require_feature(package)?;
            }
        }
        self.deps.entry(package).or_default().insert(feat);
        Ok(())
    }

    fn require_dependency(&mut self, pkg: InternedString) {
        self.deps.entry(pkg).or_default();
    }

    fn require_feature(&mut self, feat: InternedString) -> Result<(), RequirementError> {
        if !self.features.insert(feat) {
            // Already seen this feature.
            return Ok(());
        }

        let Some(fvs) = self.summary.features().get(&feat) else {
            return Err(RequirementError::MissingFeature(feat));
        };

        for fv in fvs {
            if let FeatureValue::Feature(dep_feat) = fv
                && *dep_feat == feat
            {
                return Err(RequirementError::Cycle(feat));
            }
            self.require_value(fv)?;
        }
        Ok(())
    }

    fn require_value(&mut self, fv: &FeatureValue) -> Result<(), RequirementError> {
        match fv {
            FeatureValue::Feature(feat) => self.require_feature(*feat)?,
            FeatureValue::Dep { dep_name } => self.require_dependency(*dep_name),
            FeatureValue::DepFeature {
                dep_name,
                dep_feature,
                // Weak features are always activated in the dependency
                // resolver. They will be narrowed inside the new feature
                // resolver.
                weak,
            } => self.require_dep_feature(*dep_name, *dep_feature, *weak)?,
        };
        Ok(())
    }
}

impl RequirementError {
    fn into_activate_error(self, parent: Option<PackageId>, summary: &Summary) -> ActivateError {
        match self {
            RequirementError::MissingFeature(feat) => {
                let deps: Vec<_> = summary
                    .dependencies()
                    .iter()
                    .filter(|dep| dep.name_in_toml() == feat)
                    .collect();
                if deps.is_empty() {
                    return match parent {
                        None => {
                            let closest = closest_msg(
                                feat.as_str(),
                                summary.features().keys(),
                                |key| key,
                                "feature",
                            );
                            ActivateError::Fatal(ResolveError::NoFeature {
                                package: summary.package_id(),
                                feature: feat,
                                closest,
                            })
                        }
                        Some(p) => ActivateError::Conflict(p, ConflictReason::MissingFeature(feat)),
                    };
                }
                if deps.iter().any(|dep| dep.is_optional()) {
                    match parent {
                        None => {
                            let mut features =
                                features_enabling_dependency_sorted(summary, feat).peekable();
                            let mut suggestion = String::new();
                            if features.peek().is_some() {
                                suggestion = format!(
                                    "\nDependency `{}` would be enabled by these features:",
                                    feat
                                );
                                for feature in (&mut features).take(3) {
                                    let _ = write!(&mut suggestion, "\n\t- `{}`", feature);
                                }
                                if features.peek().is_some() {
                                    suggestion.push_str("\n\t  ...");
                                }
                            }
                            ActivateError::Fatal(ResolveError::NonImplicitFeature {
                                package: summary.package_id(),
                                feature: feat,
                                suggestion,
                            })
                        }
                        Some(p) => ActivateError::Conflict(
                            p,
                            ConflictReason::NonImplicitDependencyAsFeature(feat),
                        ),
                    }
                } else {
                    match parent {
                        None => ActivateError::Fatal(ResolveError::RequiredDependencyAsFeature {
                            package: summary.package_id(),
                            feature: feat,
                        }),
                        Some(p) => ActivateError::Conflict(
                            p,
                            ConflictReason::RequiredDependencyAsFeature(feat),
                        ),
                    }
                }
            }
            RequirementError::MissingDependency(dep_name) => {
                match parent {
                    None => ActivateError::Fatal(ResolveError::NoDependencyNamed {
                        package: summary.package_id(),
                        dep: dep_name,
                    }),
                    // This code path currently isn't used, since `foo/bar`
                    // and `dep:` syntax is not allowed in a dependency.
                    Some(p) => ActivateError::Conflict(p, ConflictReason::MissingFeature(dep_name)),
                }
            }
            RequirementError::Cycle(feat) => {
                ActivateError::Fatal(ResolveError::FeatureCycle { feature: feat })
            }
        }
    }
}

/// Collect any features which enable the optional dependency "target_dep".
///
/// The returned value will be sorted.
fn features_enabling_dependency_sorted(
    summary: &Summary,
    target_dep: InternedString,
) -> impl Iterator<Item = InternedString> + '_ {
    let iter = summary
        .features()
        .iter()
        .filter(move |(_, values)| {
            for value in *values {
                match value {
                    FeatureValue::Dep { dep_name }
                    | FeatureValue::DepFeature { dep_name, weak: false, .. }
                        if dep_name == &target_dep =>
                    {
                        return true;
                    }
                    _ => (),
                }
            }
            false
        })
        .map(|(name, _)| *name);
    // iter is already sorted because it was constructed from a BTreeMap.
    iter
}
