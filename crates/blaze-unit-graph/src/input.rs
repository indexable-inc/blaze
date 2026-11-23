//! What the rules read: a package's facts (from blaze-manifest's `layout::Member`), a platform
//! a dependency table is for, a target description, and what was asked.

use core::cmp::Ordering;

use blaze_manifest::layout::{DepKind, Member, PackageMeta, Source};
use blaze_own::{AVec, Alloc, String, Vec};
use edition::PackageEdition;

use crate::Text;
use crate::extra::Extra;

/// One value of a feature's list, as the Cargo Book's "Features" chapter spells them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FeatureValue<'a> {
    /// `"f"`: another feature of the same package.
    Feature(Text<'a>),
    /// `"dep:x"`: the optional dependency `x`, with no feature of its name.
    Dep(Text<'a>),
    /// `"x/f"` (`weak: false`) or `"x?/f"` (`weak: true`): feature `f` of dependency `x`.
    DepFeature { dep: Text<'a>, feature: Text<'a>, weak: bool },
}

impl<'a> FeatureValue<'a> {
    /// `text` as a feature list's entry spells it.
    pub fn parse(text: &str, alloc: Alloc<'a>) -> FeatureValue<'a> {
        let own = |s: &str| String::from_str_in(s, alloc);
        if let Some(dep) = text.strip_prefix("dep:") {
            return FeatureValue::Dep(own(dep));
        }
        match text.split_once('/') {
            Some((dep, feature)) => match dep.strip_suffix('?') {
                Some(dep) => FeatureValue::DepFeature { dep: own(dep), feature: own(feature), weak: true },
                None => FeatureValue::DepFeature { dep: own(dep), feature: own(feature), weak: false },
            },
            None => FeatureValue::Feature(own(text)),
        }
    }
}

/// A feature and what it turns on.
#[derive(Clone, Debug)]
pub struct FeatureDef<'a> {
    pub name: Text<'a>,
    pub values: AVec<'a, FeatureValue<'a>>,
}

/// The platform of a `[target.<platform>.dependencies]` table.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Platform<'a> {
    /// A target name: holds for that triple only.
    Triple(Text<'a>),
    /// `cfg(<predicate>)`: the predicate, without `cfg(` and `)`.
    Cfg(Text<'a>),
}

impl<'a> Platform<'a> {
    pub fn parse(key: &str, alloc: Alloc<'a>) -> Platform<'a> {
        let trimmed = key.trim();
        // An unclosed `cfg(` stays a predicate, so its parse names the error.
        match trimmed.strip_prefix("cfg(") {
            Some(rest) => Platform::Cfg(String::from_str_in(rest.strip_suffix(')').unwrap_or(rest), alloc)),
            None => Platform::Triple(String::from_str_in(trimmed, alloc)),
        }
    }
}

/// Where a dependency says it comes from: what picks among two locked packages of one name.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DepSource<'a> {
    /// By `path`: locked with the source of the package that names it (no source for a
    /// workspace's, the git repository's for a package in one).
    Path,
    /// From a registry, with its requirement (`None`: `*`).
    Registry { req: Option<Text<'a>> },
    Git { url: Text<'a> },
}

/// A dependency as its package writes it.
#[derive(Clone, Debug)]
pub struct Dep<'a> {
    /// The key: the name the code calls it by when renamed.
    pub key: Text<'a>,
    /// The package's own name (`package = ".."`, else the key).
    pub package: Text<'a>,
    pub kind: DepKind,
    pub platform: Option<Platform<'a>>,
    pub optional: bool,
    pub default_features: bool,
    pub features: AVec<'a, Text<'a>>,
    pub source: DepSource<'a>,
}

impl Dep<'_> {
    /// Renamed with `package = ".."`: its extern name is the key, not the library's name.
    pub fn renamed(&self) -> bool {
        self.key != self.package
    }
}

/// A library target.
#[derive(Clone, Debug)]
pub struct Lib<'a> {
    /// The crate name (`[lib] name`, else the package's with `-` as `_`).
    pub crate_name: Text<'a>,
    /// The root file, relative to the package's directory.
    pub root: Text<'a>,
    pub proc_macro: bool,
}

/// A binary target.
#[derive(Clone, Debug)]
pub struct Bin<'a> {
    pub name: Text<'a>,
    /// Relative to the package's directory.
    pub root: Text<'a>,
    /// `required-features`, sorted: a root's binary whose features are not all on is no unit
    /// (resolve.rs `required_on`).
    pub required_features: AVec<'a, Text<'a>>,
}

/// What the rules read of one package.
#[derive(Clone, Debug)]
pub struct Package<'a> {
    pub name: Text<'a>,
    pub edition: PackageEdition,
    pub lib: Option<Lib<'a>>,
    pub bins: AVec<'a, Bin<'a>>,
    /// Integration tests, examples and benches, in the layout's order (kind, then name bytes).
    pub extras: AVec<'a, Extra<'a>>,
    /// The build script's root, relative to the package's directory.
    pub build: Option<Text<'a>>,
    pub links: Option<Text<'a>>,
    /// Sorted by name; the implicit features of optional dependencies included (the layout
    /// adds them, by the edition's rule).
    pub features: AVec<'a, FeatureDef<'a>>,
    pub deps: AVec<'a, Dep<'a>>,
    /// Its `[package]` metadata (`CARGO_PKG_*`): carried into [`crate::UnitGraph::meta`].
    pub meta: PackageMeta<'a>,
    /// The `[patch.<source>]` source table name(s) that directly named this path package in the
    /// workspace root manifest; used to disambiguate same-name Git dependencies patched to
    /// different local paths.
    pub patched_from: AVec<'a, Text<'a>>,
}

impl<'a> Package<'a> {
    /// The facts of a package the layout read; `dir` is its directory (the member's `dir`),
    /// which target roots are made relative to.
    pub fn from_member(member: &Member<'_>, alloc: Alloc<'a>) -> Package<'a> {
        let own = |s: &str| String::from_str_in(s, alloc);
        let relative = |root: &str| {
            let dir = member.dir.as_str();
            match dir.is_empty() {
                true => own(root),
                false => own(root.strip_prefix(dir).and_then(|r| r.strip_prefix('/')).unwrap_or(root)),
            }
        };
        let lib = member.lib.as_ref().map(|lib| Lib { crate_name: own(&lib.name), root: relative(&lib.root), proc_macro: lib.proc_macro });
        let mut bins = Vec::with_capacity_in(member.bins.len(), alloc);
        for bin in member.bins.iter() {
            let mut required_features = Vec::with_capacity_in(bin.required_features.len(), alloc);
            for feature in bin.required_features.iter() {
                required_features.push(own(feature));
            }
            // Sorted at the boundary, as an extra's are: the written order means nothing.
            required_features.sort_by(|a: &Text<'a>, b: &Text<'a>| a.as_str().cmp(b.as_str()));
            required_features.dedup();
            bins.push(Bin { name: own(&bin.name), root: relative(&bin.root), required_features });
        }
        // The layout keeps every such root under the package's directory, so `relative` never
        // leaves one anchor-relative here (a unit's root holds nothing of the checkout).
        let mut extras = Vec::with_capacity_in(member.extras.len(), alloc);
        for target in member.extras.iter() {
            extras.push(Extra::of(target, relative(&target.root), alloc));
        }
        let mut features = Vec::with_capacity_in(member.features.len(), alloc);
        for feature in member.features.iter() {
            let mut values = Vec::with_capacity_in(feature.enables.len(), alloc);
            for value in feature.enables.iter() {
                values.push(FeatureValue::parse(value, alloc));
            }
            features.push(FeatureDef { name: own(&feature.name), values });
        }
        features.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        let mut deps = Vec::with_capacity_in(member.dependencies.len(), alloc);
        for dep in member.dependencies.iter() {
            let mut wanted = Vec::with_capacity_in(dep.features.len(), alloc);
            for feature in dep.features.iter() {
                wanted.push(own(feature));
            }
            let source = match &dep.source {
                Source::Member(_) | Source::Path(_) | Source::Outside(_) => DepSource::Path,
                Source::Registry { req, .. } => DepSource::Registry { req: req.as_ref().map(|r| own(r)) },
                Source::Git { url, .. } => DepSource::Git { url: own(url) },
            };
            deps.push(Dep {
                key: own(&dep.name),
                package: own(&dep.package),
                kind: dep.kind,
                platform: dep.target.as_ref().map(|t| Platform::parse(t, alloc)),
                optional: dep.optional,
                default_features: dep.default_features,
                features: wanted,
                source,
            });
        }
        let mut patched_from = Vec::with_capacity_in(member.patched_from.len(), alloc);
        for src in member.patched_from.iter() {
            patched_from.push(own(src));
        }
        Package {
            name: own(&member.name),
            edition: member.edition,
            lib,
            bins,
            extras,
            build: member.build.as_ref().map(|b| relative(&b.root)),
            links: member.links.as_ref().map(|l| own(l)),
            features,
            deps,
            meta: member.meta.clone_in(alloc),
            patched_from,
        }
    }

    /// The index of the feature `name`.
    pub fn feature(&self, name: &str) -> Option<usize> {
        self.features.binary_search_by(|f| f.name.as_str().cmp(name)).ok()
    }

    pub fn is_proc_macro(&self) -> bool {
        self.lib.as_ref().is_some_and(|lib| lib.proc_macro)
    }
}

/// One cfg atom: `name` or `name = "value"`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CfgAtom<'a> {
    pub name: Text<'a>,
    pub value: Option<Text<'a>>,
}

impl<'a> CfgAtom<'a> {
    pub fn flag(name: &str, alloc: Alloc<'a>) -> CfgAtom<'a> {
        CfgAtom { name: String::from_str_in(name, alloc), value: None }
    }

    pub fn pair(name: &str, value: &str, alloc: Alloc<'a>) -> CfgAtom<'a> {
        CfgAtom { name: String::from_str_in(name, alloc), value: Some(String::from_str_in(value, alloc)) }
    }

    fn key(&self) -> (&str, Option<&str>) {
        (self.name.as_str(), self.value.as_ref().map(|v| v.as_str()))
    }
}

impl Ord for CfgAtom<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

impl PartialOrd for CfgAtom<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A platform built for: its target name and its cfg atoms (`rustc --print cfg`'s lines: `unix`,
/// `target_os = "macos"`, `target_feature = "neon"`, `panic = "unwind"`, ...), a passed value:
/// nothing here asks the machine (rule 75).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TargetDesc<'a> {
    pub triple: Text<'a>,
    /// Sorted.
    pub cfg: AVec<'a, CfgAtom<'a>>,
}

impl<'a> TargetDesc<'a> {
    pub fn new(triple: &str, mut cfg: AVec<'a, CfgAtom<'a>>, alloc: Alloc<'a>) -> TargetDesc<'a> {
        cfg.sort();
        cfg.dedup();
        TargetDesc { triple: String::from_str_in(triple, alloc), cfg }
    }

    fn has(&self, name: &str, value: Option<&str>) -> bool {
        self.cfg.binary_search_by(|atom| atom.key().cmp(&(name, value))).is_ok()
    }

    /// Whether `platform` holds here; a predicate that does not parse is an error naming it.
    pub fn holds(&self, platform: &Platform<'_>, alloc: Alloc<'_>) -> Result<bool, blaze_cond::ParseError> {
        let predicate = match platform {
            Platform::Triple(triple) => return Ok(*triple == self.triple),
            Platform::Cfg(predicate) => predicate,
        };
        let expr = blaze_cond::cfg::parse(predicate, alloc)?;
        Ok(expr.holds(|atom| self.has(atom.name, atom.value), alloc))
    }
}

/// Which feature resolver the workspace uses (the Cargo Book, "Dependency Resolution",
/// "Feature resolver version 2"; "Resolver versions").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resolver {
    V1,
    V2,
    V3,
}

/// A `resolver` value that is none cargo knows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NoResolver;

impl Resolver {
    /// The workspace's resolver: the one written (`workspace.resolver`, else the root package's
    /// `package.resolver`), else the root package's edition's default (2015 and 2018: 1, 2021:
    /// 2, 2024: 3), else, for a virtual workspace, 1.
    pub fn of(written: Option<&str>, root_edition: Option<PackageEdition>) -> Result<Resolver, NoResolver> {
        match written {
            Some("1") => Ok(Resolver::V1),
            Some("2") => Ok(Resolver::V2),
            Some("3") => Ok(Resolver::V3),
            Some(_) => Err(NoResolver),
            None => Ok(match root_edition.map(|e| e.syntax) {
                Some(edition::Edition::Edition2021) => Resolver::V2,
                Some(edition::Edition::Edition2024) => Resolver::V3,
                Some(edition::Edition::Edition2015 | edition::Edition::Edition2018) | None => Resolver::V1,
            }),
        }
    }

    /// Build-dependencies and proc macros (and all they depend on) get features of their own.
    pub fn decouples_host(self) -> bool {
        !matches!(self, Resolver::V1)
    }
}

/// What the user asked: the root packages (`-p`, `--workspace`), the `--features`,
/// `--all-features` and `--no-default-features` flags, and whether tests are built (`check
/// --tests`, `test`), which brings in the roots' dev-dependencies.
#[derive(Clone, Debug)]
pub struct Ask<'a> {
    pub roots: AVec<'a, blaze_manifest::lock::LockId>,
    /// `f` (each root that has it) or `package/f`.
    pub features: AVec<'a, Text<'a>>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub tests: bool,
}
