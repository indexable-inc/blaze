//! cargo 1.99.0's (5f94df4789) `crates/resolver-tests` helpers (`src/helpers.rs` and the
//! resolving half of `src/lib.rs`), without its SAT solver and property tests. cargo is MIT or
//! Apache-2.0.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::Result as CargoResult;
use blaze_resolve::resolver::{
    self, ResolveOpts, ResolveSettings, VersionOrdering, VersionPreferences,
};
use blaze_resolve::{
    DepKind, Dependency, GitReference, IndexSummary, InternedString, PackageId, QueryKind,
    Registry, Resolve, ResolveVersion, SourceId, Summary,
};
use url::Url;

trait IntoUrl {
    fn into_url(self) -> CargoResult<Url>;
}

impl IntoUrl for &str {
    fn into_url(self) -> CargoResult<Url> {
        Ok(Url::parse(self)?)
    }
}

pub(crate) trait ToDep {
    fn to_dep(self) -> Dependency;
    fn opt(self) -> Dependency;
    fn with(self, features: &[&'static str]) -> Dependency;
    fn with_default(self) -> Dependency;
    fn rename(self, name: &str) -> Dependency;
}

impl ToDep for &'static str {
    fn to_dep(self) -> Dependency {
        Dependency::parse(self, Some("1.0.0"), registry_loc()).unwrap()
    }
    fn opt(self) -> Dependency {
        let mut dep = self.to_dep();
        dep.set_optional(true);
        dep
    }
    fn with(self, features: &[&'static str]) -> Dependency {
        let mut dep = self.to_dep();
        dep.set_default_features(false);
        dep.set_features(features.iter().copied());
        dep
    }
    fn with_default(self) -> Dependency {
        let mut dep = self.to_dep();
        dep.set_default_features(true);
        dep
    }
    fn rename(self, name: &str) -> Dependency {
        let mut dep = self.to_dep();
        dep.set_explicit_name_in_toml(name);
        dep
    }
}

impl ToDep for Dependency {
    fn to_dep(self) -> Dependency {
        self
    }
    fn opt(mut self) -> Dependency {
        self.set_optional(true);
        self
    }
    fn with(mut self, features: &[&'static str]) -> Dependency {
        self.set_default_features(false);
        self.set_features(features.iter().copied());
        self
    }
    fn with_default(mut self) -> Dependency {
        self.set_default_features(true);
        self
    }
    fn rename(mut self, name: &str) -> Dependency {
        self.set_explicit_name_in_toml(name);
        self
    }
}

pub(crate) trait ToPkgId {
    fn to_pkgid(&self) -> PackageId;
}

impl ToPkgId for PackageId {
    fn to_pkgid(&self) -> PackageId {
        *self
    }
}

impl ToPkgId for &str {
    fn to_pkgid(&self) -> PackageId {
        PackageId::try_new(*self, "1.0.0", registry_loc()).unwrap()
    }
}

impl<T: AsRef<str>, U: AsRef<str>> ToPkgId for (T, U) {
    fn to_pkgid(&self) -> PackageId {
        let (name, vers) = self;
        PackageId::try_new(name.as_ref(), vers.as_ref(), registry_loc()).unwrap()
    }
}

fn registry_loc() -> SourceId {
    static EXAMPLE_DOT_COM: OnceLock<SourceId> = OnceLock::new();
    let example_dot = EXAMPLE_DOT_COM.get_or_init(|| {
        SourceId::for_registry(&"https://example.com".into_url().unwrap()).unwrap()
    });
    *example_dot
}

pub(crate) fn pkg<T: ToPkgId>(name: T) -> Summary {
    pkg_dep(&name, Vec::new())
}

pub(crate) fn pkg_dep<T: ToPkgId>(name: &T, dep: Vec<Dependency>) -> Summary {
    let pkgid = name.to_pkgid();
    let link = if pkgid.name().ends_with("-sys") { Some(pkgid.name()) } else { None };
    Summary::new(name.to_pkgid(), dep, &BTreeMap::new(), link, None).unwrap()
}

pub(crate) fn pkg_dep_with<T: ToPkgId>(
    name: &T,
    dep: Vec<Dependency>,
    features: &[(&'static str, &[&'static str])],
) -> Summary {
    let pkgid = name.to_pkgid();
    let link = if pkgid.name().ends_with("-sys") { Some(pkgid.name()) } else { None };
    let features = features
        .iter()
        .map(|&(name, values)| (name.into(), values.iter().map(|&v| v.into()).collect()))
        .collect();
    Summary::new(name.to_pkgid(), dep, &features, link, None).unwrap()
}

pub(crate) fn pkg_dep_link<T: ToPkgId>(name: &T, link: &str, dep: Vec<Dependency>) -> Summary {
    Summary::new(name.to_pkgid(), dep, &BTreeMap::new(), Some(link), None).unwrap()
}

pub(crate) fn pkg_id(name: &str) -> PackageId {
    PackageId::try_new(name, "1.0.0", registry_loc()).unwrap()
}

pub(crate) fn pkg_id_source(name: &str, source: &str) -> PackageId {
    PackageId::try_new(name, "1.0.0", SourceId::for_registry(&source.into_url().unwrap()).unwrap())
        .unwrap()
}

fn pkg_id_loc(name: &str, loc: &str) -> PackageId {
    let remote = loc.into_url();
    let master = GitReference::Branch("master".to_owned());
    let source_id = SourceId::for_git(&remote.unwrap(), master).unwrap();

    PackageId::try_new(name, "1.0.0", source_id).unwrap()
}

pub(crate) fn pkg_loc(name: &str, loc: &str) -> Summary {
    let link = if name.ends_with("-sys") { Some(name) } else { None };
    Summary::new(pkg_id_loc(name, loc), Vec::new(), &BTreeMap::new(), link, None).unwrap()
}

pub(crate) fn remove_dep(sum: &Summary, ind: usize) -> Summary {
    let mut deps = sum.dependencies().to_vec();
    deps.remove(ind);
    // note: more things will need to be copied over in the future, but it works for now.
    Summary::new(sum.package_id(), deps, &BTreeMap::new(), sum.links(), None).unwrap()
}

pub(crate) fn dep(name: &str) -> Dependency {
    dep_req(name, "*")
}

pub(crate) fn dep_req(name: &str, req: &str) -> Dependency {
    Dependency::parse(name, Some(req), registry_loc()).unwrap()
}

pub(crate) fn dep_req_kind(name: &str, req: &str, kind: DepKind) -> Dependency {
    let mut dep = dep_req(name, req);
    dep.set_kind(kind);
    dep
}

pub(crate) fn dep_req_platform(name: &str, req: &str, platform: &str) -> Dependency {
    let mut dep = dep_req(name, req);
    dep.set_platform(Some(platform.parse().unwrap()));
    dep
}

pub(crate) fn dep_loc(name: &str, location: &str) -> Dependency {
    let url = location.into_url().unwrap();
    let master = GitReference::Branch("master".to_owned());
    let source_id = SourceId::for_git(&url, master).unwrap();
    Dependency::parse(name, Some("1.0.0"), source_id).unwrap()
}

pub(crate) fn dep_kind(name: &str, kind: DepKind) -> Dependency {
    let mut dep = dep(name);
    dep.set_kind(kind);
    dep
}

pub(crate) fn dep_platform(name: &str, platform: &str) -> Dependency {
    let mut dep = dep(name);
    dep.set_platform(Some(platform.parse().unwrap()));
    dep
}

pub(crate) fn registry(pkgs: Vec<Summary>) -> Vec<Summary> {
    pkgs
}

pub(crate) fn names<P: ToPkgId>(names: &[P]) -> Vec<PackageId> {
    names.iter().map(|name| name.to_pkgid()).collect()
}

pub(crate) fn loc_names(names: &[(&'static str, &'static str)]) -> Vec<PackageId> {
    names.iter().map(|&(name, loc)| pkg_id_loc(name, loc)).collect()
}

/// Assert `xs` contains `elems`
#[track_caller]
pub(crate) fn assert_contains<A: PartialEq + Debug>(xs: &[A], elems: &[A]) {
    for elem in elems {
        assert!(xs.contains(elem), "missing element\nset: {xs:?}\nmissing: {elem:?}");
    }
}

#[track_caller]
pub(crate) fn assert_same<A: PartialEq + Debug>(a: &[A], b: &[A]) {
    assert_eq!(a.len(), b.len(), "not equal\n{a:?}\n{b:?}");
    assert_contains(b, a);
}

pub(crate) fn resolve(deps: Vec<Dependency>, registry: &[Summary]) -> CargoResult<Vec<PackageId>> {
    Ok(resolve_with_ordering(deps, registry, VersionOrdering::MaximumVersionsFirst)?
        .into_iter()
        .map(|(pkg, _)| pkg)
        .collect())
}

/// cargo's `resolve_and_validated`, less its check that a SAT solver agrees.
pub(crate) fn resolve_and_validated(
    deps: Vec<Dependency>,
    registry: &[Summary],
) -> CargoResult<Vec<(PackageId, Vec<InternedString>)>> {
    let root_pkg_id = pkg_id("root");
    let resolve = resolve_raw(deps, registry, root_pkg_id, VersionOrdering::MaximumVersionsFirst)?;
    let mut stack = vec![root_pkg_id];
    let mut used = BTreeSet::new();
    let mut links = BTreeSet::new();
    while let Some(p) = stack.pop() {
        assert!(resolve.contains(&p));
        if used.insert(p) {
            // in the tests all `links` crates end in `-sys`
            if p.name().ends_with("-sys") {
                assert!(links.insert(p.name()));
            }
            stack.extend(resolve.deps(p).map(|(dp, deps)| {
                for d in deps {
                    assert!(d.matches_id(dp));
                }
                dp
            }));
        }
    }
    let out = collect_features(&resolve);
    assert_eq!(out.len(), used.len());
    Ok(out)
}

fn collect_features(resolve: &Resolve) -> Vec<(PackageId, Vec<InternedString>)> {
    resolve.sort().iter().map(|&pkg| (pkg, resolve.features(pkg).to_vec())).collect()
}

/// cargo's `resolve_with_global_context`: `-Zminimal-versions` is the one setting its tests
/// give.
pub(crate) fn resolve_with_ordering(
    deps: Vec<Dependency>,
    registry: &[Summary],
    ordering: VersionOrdering,
) -> CargoResult<Vec<(PackageId, Vec<InternedString>)>> {
    let resolve = resolve_raw(deps, registry, pkg_id("root"), ordering)?;
    Ok(collect_features(&resolve))
}

pub(crate) fn resolve_raw(
    deps: Vec<Dependency>,
    registry: &[Summary],
    root_pkg_id: PackageId,
    ordering: VersionOrdering,
) -> CargoResult<Resolve> {
    let root_summary =
        Summary::new(root_pkg_id, deps, &BTreeMap::new(), None::<&String>, None).unwrap();
    let start = Instant::now();
    let resolve =
        resolve_summaries(&[(root_summary, ResolveOpts::everything())], registry, ordering);
    // The largest test in our suite takes less then 30 secs.
    // So let's fail the test if we have been running for more than 60 secs.
    assert!(start.elapsed().as_secs() < 60);
    resolve
}

/// Resolves the workspace members `roots` against `registry`.
pub(crate) fn resolve_summaries(
    roots: &[(Summary, ResolveOpts)],
    registry: &[Summary],
    ordering: VersionOrdering,
) -> CargoResult<Resolve> {
    struct MyRegistry<'a> {
        list: &'a [Summary],
    }
    impl Registry for MyRegistry<'_> {
        async fn query(
            &self,
            dep: &Dependency,
            kind: QueryKind,
            f: &mut impl FnMut(IndexSummary),
        ) -> CargoResult<()> {
            for summary in self.list.iter() {
                let matched = match kind {
                    QueryKind::Exact => dep.matches(summary),
                    QueryKind::RejectedVersions => dep.matches(summary),
                    QueryKind::AlternativeNames => true,
                    QueryKind::Normalized => true,
                };
                if matched {
                    f(IndexSummary::Candidate(summary.clone()));
                }
            }
            Ok(())
        }

        fn describe_source(&self, _src: SourceId) -> String {
            String::new()
        }

        fn is_replaced(&self, _src: SourceId) -> bool {
            false
        }
    }
    let registry = MyRegistry { list: registry };

    let mut version_prefs = VersionPreferences::default();
    version_prefs.version_ordering(ordering);

    resolver::resolve(
        roots,
        &[],
        &registry,
        &version_prefs,
        ResolveVersion::with_rust_version(None),
        &ResolveSettings::default(),
    )
}
