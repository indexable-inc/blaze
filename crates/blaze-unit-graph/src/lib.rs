//! The unit graph: which crates a request compiles, and with which features and cfgs, by
//! cargo's documented rules, without running cargo (docs/v2/plan/design/units.md).
//!
//! A *unit* is one crate compiled once: a package's library, proc macro, binary, build script or
//! test harness, for the target or for the host, with one feature set. A package appears once
//! per distinct configuration (its library for the target and again for the host when resolver
//! 2 gives the host other features).
//!
//! The function is sans-IO ([`Resolution`]): it is handed the lock ([`blaze_manifest::lock`]),
//! the workspace members' facts, two platform descriptions and the [`Ask`]; each
//! [`Resolution::pass`] answers [`Pass::Need`] with the locked packages whose manifests it must
//! read next, which the driver answers with [`Resolution::package`] (`None`: the manifest is not
//! on disk, never guessed); at the end, [`Pass::Done`] with the whole graph, or
//! [`Pass::Partial`] when a manifest it reached was answered `None`: a [`PartialGraph`] names
//! the missing packages and holds no units, so nothing compiles a graph that lacks a
//! dependency (docs/v2/plan/design/states.md, 35).
//!
//! ```compile_fail
//! // A partial graph has no units to compile.
//! fn units<'a>(partial: &blaze_unit_graph::PartialGraph<'a>) -> usize {
//!     partial.units().len()
//! }
//! ```
//!
//! ```compile_fail
//! // A whole graph holds no "unavailable" list: a graph that lacks a package is not one.
//! fn lacking<'a>(graph: &blaze_unit_graph::UnitGraph<'a>) -> usize {
//!     graph.unavailable.len()
//! }
//! ```
//!
//! # The rules (the specification)
//!
//! Each with its source in the Cargo Book (doc.rust-lang.org/cargo/reference/).
//!
//! 1. **Feature values** ("Features", "The `[features]` section", "Dependency features"). A
//!    feature lists values: `"f"` turns on feature `f` of the same package; `"dep:x"` turns on
//!    the optional dependency `x`; `"x/f"` turns on dependency `x` (when optional) and its
//!    feature `f`, and also the package's feature named `x` when there is one; `"x?/f"` turns on
//!    feature `f` of `x` only if `x` is on for another reason, and never turns `x` on.
//! 2. **Optional dependencies** ("Optional dependencies"). Each optional dependency `x` makes an
//!    implicit feature `x = ["dep:x"]`, unless some feature names `dep:x`, in every edition (the
//!    removal once proposed for 2024 did not ship; tempo, edition 2024, asks for one).
//!    blaze-manifest's layout adds them.
//! 3. **Defaults** ("The `default` feature", "Dependency features"). The feature `default` is
//!    on unless `default-features = false` on that dependency entry, or `--no-default-features`
//!    for a root. One entry anywhere with defaults on turns them on (features are unified).
//! 4. **Command line** ("Command-line feature options"). `--features f` turns on `f` of each
//!    root that has it; `--features p/f` feature `f` of root `p`, or of the dependency `p` of a
//!    root; `--all-features` every feature of each root; `--no-default-features` drops the
//!    roots' `default`. A feature no root has is an error.
//! 5. **Unification** ("Feature unification"). A package's features are the union of all that
//!    ask for it, in one namespace per resolver rule 6 gives.
//! 6. **Resolver versions** ("Dependency Resolution", "Feature resolver version 2"; "Resolver
//!    versions"). Resolver 1: one namespace; every dependency of every kind and platform counts,
//!    dev-dependencies of members included. Resolver 2 and 3: build-dependencies and proc macros,
//!    and all they depend on, get a namespace of their own (the host's); a target-specific
//!    dependency counts only where its platform matches (the host's for the host side and for
//!    build-dependencies); dev-dependencies count only when a target that needs them (tests) is
//!    built, and then for the whole build: features are resolved once per invocation, so a
//!    dev-dependency's features unify with the normal dependency of the same package for every
//!    target unit of that build (the library's unit as well as its test harness), and never
//!    reach the host's namespace unless the dev-dependency is itself a proc macro. A dependency
//!    `x` listed both as an optional normal dependency and as a dev-dependency is on in the
//!    test units through the dev entry even when the optional entry is off, and `x?/f` then
//!    turns `f` on (the dev entry is not optional, so nothing waits). The default: `workspace.resolver` (or the root package's `package.resolver`), else
//!    edition 2024: 3, 2021: 2, earlier: 1; a virtual workspace without it: 1 ("Workspaces",
//!    "The resolver setting"). Resolver 3 changes only version selection, which the lock holds.
//! 7. **Platform-specific dependencies** ("Specifying Dependencies", "Platform specific
//!    dependencies"). `[target.<triple>.dependencies]` hold for that target name;
//!    `[target.'cfg(..)'.dependencies]` where the predicate holds on the platform's cfgs.
//! 8. **Dependency kinds** ("Specifying Dependencies", "Development dependencies", "Build
//!    dependencies"). A library or binary depends on `[dependencies]`; a build script on
//!    `[build-dependencies]`, built for the host; a test on both `[dependencies]` and
//!    `[dev-dependencies]`. A binary also depends on its own package's library. Only a workspace
//!    member's dev-dependencies are read (a published package's are not in the lock).
//! 9. **Renamed dependencies** ("Renaming dependencies in Cargo.toml"). `key = { package = "p" }`
//!    is called `key` (with `-` as `_`) in code and in feature values; otherwise the extern
//!    name is the library's crate name.
//! 10. **Which package an entry is** ("Overriding Dependencies": `[patch]`, `[replace]"; "The
//!    lock file"). The lock holds the package a patch or replacement chose under its own source,
//!    and lists it among the dependent's dependencies; an entry is the locked dependency of that
//!    name, and of two, the one whose source kind and requirement fit.
//! 11. **Workspaces** ("Workspaces", "The `dependencies` table", "Inheriting a dependency from a
//!    workspace"). `workspace = true` entries take the workspace's source and its features, with
//!    their own `features` added; the layout resolves the inheritance.
//! 12. **Build scripts and `links`** ("Build Scripts"). A package with `build.rs` (or `build =
//!    "path"`) has a build script unit, built for the host with the package's features of that
//!    side and its build-dependencies; its outputs (`cargo::rustc-cfg`) join the unit later.
//!    `links` is carried for that later step.
//! 13. **cfgs** ("Features": "`feature = \"..\"`"; "Profiles": `debug-assertions`; "Conditional
//!    compilation" in the Reference). A unit's cfg set is its platform's atoms, a
//!    `feature = "f"` per feature on, `debug_assertions` (check builds the dev profile), `test`
//!    for a test unit, and `proc_macro` for a proc macro.
//!
//! # The output, and features as conditions
//!
//! [`UnitGraph::units`] is sorted by package (name, version, source), kind and configuration;
//! [`UnitId`]s are positions in it and dependencies are sorted by extern name, so two runs write
//! equal bytes (rule 25). Each unit holds its features as a sorted list: one configuration.
//! To check every configuration at once (blaze-cond), a unit's features become a condition over
//! feature variables instead: rule 1 to 5 turn into implications between a package's feature
//! variables (`a -> b`, `a -> dep_x`, `a & dep_x -> x.f`), and the unit's features become the
//! decision diagram of what they force under the asked roots; the walk stays, and what changes
//! is that a set grows by an implication rather than a bit.
//!
//! # What `blaze check` asks
//!
//! `blaze check` checks every workspace member, every target (library, binaries, the library's
//! and each binary's test harness, integration tests, examples, benches) with dev-dependencies
//! on and unified into the one build: cargo's `cargo check --workspace --all-targets`
//! (`Ask { roots: every member, tests: true }`, blaze-cli engine_check.rs).
//!
//! # Not covered
//!
//! Profiles beyond check's dev profile; artifact dependencies; registries that need a login;
//! `-Z` features.

#![no_std]
#![feature(allocator_api)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[cfg(test)]
extern crate std;

mod extra;
pub mod implicit;
pub mod input;
mod resolve;
pub mod record;
pub mod sources;

#[cfg(test)]
mod tests;

use core::fmt;

use blaze_manifest::lock::LockId;
use blaze_own::{AVec, Alloc, String};
use edition::PackageEdition;

/// A package's `CARGO_PKG_*` metadata: its home is blaze-manifest's layout (meta.rs).
pub use blaze_manifest::layout::{PackageMeta, PackageMetaVars};
pub use extra::{Extra, ExtraUnit};
pub use implicit::{Implicit, Prelude, StdIs, Sysroot, ToolchainUnit, implicit};
pub use input::{Ask, Bin, CfgAtom, Dep, DepSource, FeatureDef, FeatureValue, Lib, NoResolver, Package, Platform, Resolver, TargetDesc};
pub use resolve::{Resolution, ignores_profile_panic};
pub use sources::{Open, Records, SourceError, StoreSources};

/// Text in the caller's allocator.
pub type Text<'a> = String<Alloc<'a>>;

/// A unit: its position in [`UnitGraph::units`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct UnitId(pub u32);

impl UnitId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// What a pass gives.
pub enum Pass<'a> {
    /// The locked packages whose facts are needed next (sorted).
    Need(AVec<'a, LockId>),
    /// The graph, whole: every package it reached was read.
    Done(UnitGraph<'a>),
    /// No graph: packages it reached have no manifest. The caller refuses, naming them.
    Partial(PartialGraph<'a>),
}

/// A resolution that reached packages whose manifests were answered `None`: the missing
/// packages, sorted, each once, never empty. It offers no units: a graph without a dependency
/// is not compiled (states.md, 35).
#[derive(Debug)]
pub struct PartialGraph<'a> {
    missing: AVec<'a, LockId>,
}

impl<'a> PartialGraph<'a> {
    /// `missing` sorted and deduplicated; `None` when it is empty (the graph is whole).
    pub(crate) fn of(mut missing: AVec<'a, LockId>) -> Option<PartialGraph<'a>> {
        missing.sort_unstable();
        missing.dedup();
        (!missing.is_empty()).then_some(PartialGraph { missing })
    }

    /// The packages whose manifests are missing: sorted, each once, at least one.
    pub fn missing(&self) -> &[LockId] {
        &self.missing
    }
}

/// Which target of a package a unit compiles.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum UnitKind<'a> {
    Lib,
    ProcMacro,
    Bin(Text<'a>),
    BuildScript,
    /// The test harness of the library (`bin: None`) or of a binary.
    Test { bin: Option<Text<'a>> },
    /// An integration test: `tests/<name>.rs`, `tests/<name>/main.rs` or a `[[test]]` table.
    IntegrationTest(ExtraUnit<'a>),
    /// An example: `examples/..` or an `[[example]]` table.
    Example(ExtraUnit<'a>),
    /// A bench: `benches/..` or a `[[bench]]` table.
    Bench(ExtraUnit<'a>),
}

/// A dependency of a unit: the name the code calls it by, and the unit.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnitDep<'a> {
    pub name: Text<'a>,
    pub unit: UnitId,
}

/// One crate compiled once.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Unit<'a> {
    pub package: LockId,
    /// The package's name.
    pub name: Text<'a>,
    pub version: Text<'a>,
    pub kind: UnitKind<'a>,
    /// Built for the host (a build script, a proc macro, or what they depend on).
    pub for_host: bool,
    pub edition: PackageEdition,
    /// The crate's name.
    pub crate_name: Text<'a>,
    /// The root source file, relative to the package's directory.
    pub root: Text<'a>,
    /// Sorted.
    pub features: AVec<'a, Text<'a>>,
    /// Sorted: the platform's atoms, the features, and `debug_assertions`, `test`, `proc_macro`.
    pub cfg: AVec<'a, CfgAtom<'a>>,
    /// Sorted by name.
    pub deps: AVec<'a, UnitDep<'a>>,
    /// The package's build script, whose outputs (cfgs, env) join this unit when it has run.
    pub build_script: Option<UnitId>,
    /// The package's `links` (a build script's `CARGO_MANIFEST_LINKS`, and the `DEP_<LINKS>_*`
    /// its dependents' build scripts are given).
    pub links: Option<Text<'a>>,
}

/// The graph a request compiles: whole, every package it reached read ([`Pass::Partial`] is
/// the one that is not).
pub struct UnitGraph<'a> {
    pub units: AVec<'a, Unit<'a>>,
    pub resolver: Resolver,
    /// Each package of a unit with its `[package]` metadata (`CARGO_PKG_*`, blaze-manifest
    /// meta.rs), sorted by id, each id once. A package with no row is one whose metadata this
    /// graph does not know: a crate of it reads those variables refused by name, never `""`.
    pub meta: AVec<'a, (LockId, PackageMeta<'a>)>,
    /// The roots' targets the request leaves out by their `required-features`, sorted by
    /// (package, kind, name), each once: no unit, and a note each (rule 3: never left out
    /// quietly). In the record, so `run --example x` can say why `x` is not there.
    pub left_out: AVec<'a, LeftOut<'a>>,
}

/// What kind of target a [`LeftOut`] is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum LeftOutKind {
    Bin,
    Test,
    Example,
    Bench,
}

impl LeftOutKind {
    /// As the manifest's table names it (`[[bin]]`, `[[test]]`, `[[example]]`, `[[bench]]`).
    pub fn table(self) -> &'static str {
        match self {
            LeftOutKind::Bin => "bin",
            LeftOutKind::Test => "test",
            LeftOutKind::Example => "example",
            LeftOutKind::Bench => "bench",
        }
    }
}

/// A root's target with no unit because entries of its `required-features` are off (the Cargo
/// Book, "Cargo Targets", "The `required-features` field": "The `required-features` field
/// specifies which features the target needs in order to be built. If any of the required
/// features are not enabled, the target will be skipped.").
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LeftOut<'a> {
    pub package: LockId,
    /// The package's name (a unit's `name`), for the note.
    pub package_name: Text<'a>,
    pub kind: LeftOutKind,
    /// The target's name.
    pub name: Text<'a>,
    /// The entries of its `required-features` that are off, sorted, each once, never empty:
    /// a feature of the package (`bench`) or `dep/feature` as written.
    pub off: AVec<'a, Text<'a>>,
}

impl fmt::Display for LeftOut<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the {} target `{}` of package `{}` is skipped: its required feature", self.kind.table(), self.name, self.package_name)?;
        f.write_str(if self.off.len() == 1 { " " } else { "s " })?;
        for (i, feature) in self.off.iter().enumerate() {
            write!(f, "{}`{feature}`", if i == 0 { "" } else { ", " })?;
        }
        f.write_str(if self.off.len() == 1 { " is" } else { " are" })?;
        f.write_str(" not enabled (the Cargo Book, \"The `required-features` field\": \"If any of the required features are not enabled, the target will be skipped.\")")
    }
}

impl<'a> UnitGraph<'a> {
    /// The metadata of `package`, when the graph knows it ([`UnitGraph::meta`]).
    pub fn meta_of(&self, package: LockId) -> Option<&PackageMeta<'a>> {
        self.meta.binary_search_by(|(id, _)| id.cmp(&package)).ok().map(|at| &self.meta[at].1)
    }
}

/// Why no graph: each names the package and what of it.
#[derive(Debug)]
pub enum UnitError<'a> {
    /// A feature value names a feature the package lacks.
    NoSuchFeature { package: Text<'a>, feature: Text<'a>, asked_by: Text<'a> },
    /// A feature value names a dependency the package lacks.
    NoSuchDependency { package: Text<'a>, dep: Text<'a> },
    /// A `--features` entry no root has.
    NoRootHasFeature { feature: Text<'a> },
    /// The lock lists no package for a dependency entry.
    NotLocked { package: Text<'a>, dep: Text<'a> },
    /// The lock lists two packages that fit a dependency entry.
    AmbiguousLock { package: Text<'a>, dep: Text<'a> },
    /// A dependency's version requirement does not parse.
    Requirement { package: Text<'a>, dep: Text<'a>, req: Text<'a> },
    /// A `[target.'cfg(..)']` predicate does not parse.
    Platform { package: Text<'a>, platform: Text<'a>, error: blaze_cond::ParseError },
    /// A root was not answered with its facts.
    RootUnanswered { root: LockId },
}

impl fmt::Display for UnitError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitError::NoSuchFeature { package, feature, asked_by } => {
                write!(f, "package `{package}` has no feature `{feature}` (asked by `{asked_by}`)")
            }
            UnitError::NoSuchDependency { package, dep } => write!(f, "package `{package}` has no dependency `{dep}` that a feature names"),
            UnitError::NoRootHasFeature { feature } => write!(f, "no package asked for has the feature `{feature}`"),
            UnitError::NotLocked { package, dep } => write!(f, "`{package}`'s dependency `{dep}` is not in Cargo.lock"),
            UnitError::AmbiguousLock { package, dep } => write!(f, "`{package}`'s dependency `{dep}` fits two packages of Cargo.lock"),
            UnitError::Requirement { package, dep, req } => write!(f, "`{package}`'s dependency `{dep}`: requirement `{req}` does not parse"),
            UnitError::Platform { package, platform, error } => write!(f, "`{package}`: platform `cfg({platform})`: {error}"),
            UnitError::RootUnanswered { root } => write!(f, "the root package {} was not answered with its manifest", root.0),        }
    }
}

/// A known bug, planted for the tests' controls (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// Dev-dependencies count without tests.
    DevLeaks,
    /// Dev-dependencies never count in the feature fixpoint, tests asked or not: a test unit
    /// still links them, with only the features the normal build gave them (the control of
    /// tests/dev_unify.rs).
    DevIgnored,
    /// A feature value naming an optional dependency hidden by `dep:x` turns it on anyway.
    DepMakesImplicit,
    /// `x?/f` turns `x` on.
    WeakEnables,
}
