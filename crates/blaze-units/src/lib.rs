//! cargo's unit graph made by blaze: the units a `cargo build`, `check`, `test` or `bench`
//! compiles and runs, each with its target, profile, features and platform, and the edges
//! between them, as `cargo <command> --unit-graph -Z unstable-options` prints them (schema
//! version 1, [`schema`]).
//!
//! Nothing here reads a file or runs a process (issue #458): the workspace's manifests, the
//! resolve and its features and the profiles come in ([`Request`]), already read by
//! blaze-manifest and resolved by blaze-resolve, and the graph goes out, equal to cargo's.
//!
//! The code follows the pinned nightly's cargo (`cad07cf8e`): `ops/cargo_compile/mod.rs`
//! (`create_bcx`, `rebuild_unit_graph_shared`), `ops/cargo_compile/unit_generator.rs` (the
//! root units), `compiler/unit_dependencies.rs` (their dependencies) and
//! `compiler/unit_graph.rs` (the JSON), keeping cargo's names so the two read side by side.
//! What cargo does with an unstable flag (`-Zbuild-std`, artifact dependencies,
//! `-Zdual-proc-macros`, multiple build scripts, per-package targets) is not ported: a
//! workspace that needs one is an error naming it.

#![forbid(unsafe_code)]

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

mod graph;
pub mod index;
mod order;
#[cfg(feature = "query")]
pub mod load;
#[cfg(feature = "query")]
pub mod query;
pub mod read;
pub mod schema;
pub mod sources;
#[cfg(feature = "query")]
pub mod stdlib;

use blaze_hash::HashMap;
use blaze_manifest::manifest::Manifest;
use blaze_manifest::profiles::Profiles;
use blaze_resolve::resolver::ResolvedFeatures;
use blaze_resolve::{Cfg, PackageId, Platform, Resolve};

pub use read::Read;
pub use schema::UnitGraph;

/// What the user asked cargo to do (cargo's `UserIntent`, less `doctest`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    Build,
    /// `cargo check`; `test` with `--profile test`, which checks the tests too.
    Check {
        test: bool,
    },
    Test,
    Bench,
    /// `cargo doc`: the targets cargo documents, checked, for the front end the pages are
    /// written from. With or without `--no-deps` every dependency is checked, as cargo checks
    /// each for the metadata rustdoc reads. `open` is `--open`, whose browser the plan reads.
    Doc {
        open: bool,
    },
}

impl Intent {
    /// The profile the command builds with when `--release` and `--profile` say none.
    pub fn default_profile(self) -> &'static str {
        match self {
            Intent::Build | Intent::Check { .. } | Intent::Doc { .. } => "dev",
            Intent::Test => "test",
            Intent::Bench => "bench",
        }
    }
}

/// Which targets of the selected packages are built (cargo's `CompileFilter`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    /// No target flag: the command's own default targets.
    Default {
        /// Whether a target whose `required-features` are not on is left out rather than an
        /// error: true unless the targets were named some other way.
        required_features_filterable: bool,
    },
    Only {
        all_targets: bool,
        lib: LibRule,
        bins: Rule,
        examples: Rule,
        tests: Rule,
        benches: Rule,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibRule {
    /// `--lib`.
    True,
    /// The library when there is one (`--all-targets`).
    Default,
    False,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// `--bins`, `--tests`, ...: every target of the kind.
    All,
    /// `--bin NAME`, ...: the targets named (glob patterns are not read yet); none when empty.
    Just(Vec<String>),
}

impl Rule {
    pub fn none() -> Rule {
        Rule::Just(Vec::new())
    }

    fn is_specific(&self) -> bool {
        match self {
            Rule::All => true,
            Rule::Just(names) => !names.is_empty(),
        }
    }
}

impl Filter {
    /// The filter of the command line's target flags, as cargo's `from_raw_arguments`.
    #[allow(clippy::too_many_arguments)]
    pub fn from_flags(
        lib_only: bool,
        bins: Vec<String>,
        all_bins: bool,
        tests: Vec<String>,
        all_tests: bool,
        examples: Vec<String>,
        all_examples: bool,
        benches: Vec<String>,
        all_benches: bool,
        all_targets: bool,
    ) -> Filter {
        if all_targets {
            return Filter::all_targets();
        }
        let rule = |names: Vec<String>, all: bool| if all { Rule::All } else { Rule::Just(names) };
        let lib = if lib_only { LibRule::True } else { LibRule::False };
        let (bins, tests) = (rule(bins, all_bins), rule(tests, all_tests));
        let (examples, benches) = (rule(examples, all_examples), rule(benches, all_benches));
        if lib == LibRule::True
            || bins.is_specific()
            || tests.is_specific()
            || examples.is_specific()
            || benches.is_specific()
        {
            Filter::Only { all_targets: false, lib, bins, examples, tests, benches }
        } else {
            Filter::Default { required_features_filterable: true }
        }
    }

    pub fn all_targets() -> Filter {
        Filter::Only {
            all_targets: true,
            lib: LibRule::Default,
            bins: Rule::All,
            examples: Rule::All,
            tests: Rule::All,
            benches: Rule::All,
        }
    }

    /// Whether the build needs the dev-dependencies resolved (`HasDevUnits`).
    pub fn need_dev_deps(&self, intent: Intent) -> bool {
        match intent {
            Intent::Test | Intent::Bench | Intent::Check { test: true } => true,
            Intent::Build | Intent::Check { test: false } | Intent::Doc { .. } => match self {
                Filter::Default { .. } => false,
                Filter::Only { examples, tests, benches, .. } => {
                    examples.is_specific() || tests.is_specific() || benches.is_specific()
                }
            },
        }
    }

    fn is_specific(&self) -> bool {
        matches!(self, Filter::Only { .. })
    }
}

/// A platform built for: its triple and what `rustc --print cfg` says of it.
#[derive(Clone, Debug)]
pub struct Platformed {
    pub triple: String,
    pub cfg: Vec<Cfg>,
}

impl Platformed {
    fn activates(&self, platform: &Platform) -> bool {
        platform.matches(&self.triple, &self.cfg)
    }
}

/// A package of the resolve, as read from its manifest.
#[derive(Clone, Copy)]
pub struct Package<'a> {
    pub manifest: &'a Manifest,
    /// Whether it is a member of the workspace.
    pub is_member: bool,
}

/// Everything a unit graph is made from.
pub struct Request<'a> {
    /// Every package of the resolve, by its id.
    pub packages: &'a HashMap<PackageId, Package<'a>>,
    /// The resolve of the build (cargo's `targeted_resolve`).
    pub resolve: &'a Resolve,
    /// The features the feature resolver activated for the build.
    pub features: &'a ResolvedFeatures,
    pub profiles: &'a Profiles,
    /// The packages asked for (`-p`, `--workspace` or the default members), by their ids.
    pub roots: &'a [PackageId],
    pub intent: Intent,
    pub filter: &'a Filter,
    /// The host, which build scripts and proc macros are built for.
    pub host: &'a Platformed,
    /// `--target`, when the build is for another platform than the host's.
    pub target: Option<&'a Platformed>,
    /// Whether the dev-dependencies were resolved ([`Filter::need_dev_deps`]).
    pub has_dev_units: bool,
}

/// The unit graph `request` makes.
pub fn unit_graph(request: &Request<'_>) -> anyhow::Result<UnitGraph> {
    graph::build(request)
}
