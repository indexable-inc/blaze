//! The unit graph as `cargo build --unit-graph -Z unstable-options` prints it (version 1,
//! cargo's `src/compiler/unit_graph.rs`): read and written with serde, so that blaze's graph
//! and cargo's are one type and print as one JSON.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The version of the schema, which cargo prints first.
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnitGraph {
    pub version: u32,
    pub units: Vec<Unit>,
    /// The units asked for, by their index in `units`.
    pub roots: Vec<usize>,
}

/// One rustc (or build script) run of the build.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    /// The package, as `cargo metadata` writes its id (`path+file:///..#0.1.0`).
    pub pkg_id: String,
    pub target: Target,
    pub profile: Profile,
    /// The triple built for; none for the host's own units (a build without `--target`, or a
    /// build script and what it depends on). Codegen reads it; what tells two units apart is
    /// [`Unit::built_for`], which also says [`Unit::macro_wasm`].
    pub platform: Option<String>,
    /// blaze's: the unit is a proc macro, or what one depends on, built for `platform` (always
    /// `wasm32-wasip1`) to run as a wasm module in the macro host. A build for `--target
    /// wasm32-wasip1` has units of both for one package and triple, told apart by this alone.
    /// cargo's graph has none, and prints no such key.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub macro_wasm: bool,
    pub mode: Mode,
    /// Whether a `check` unit is checked as a test, with `--test`: cargo prints it as any
    /// other `check`, so it is not in the JSON.
    #[serde(skip)]
    pub checked_as_test: bool,
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_std: bool,
    pub dependencies: Vec<UnitDep>,
}

impl Unit {
    /// What the unit is built for, as two units of one package and target are told apart by
    /// it: every key, directory and group that is per platform is per this.
    pub fn built_for(&self) -> BuiltFor<'_> {
        BuiltFor { platform: self.platform.as_deref(), macro_wasm: self.macro_wasm }
    }

    /// Whether the unit's code runs on the machine that builds it: [`runs_here`] of what it
    /// is built for and its profile.
    pub fn runs_here(&self) -> bool {
        runs_here(self.built_for(), &self.profile.name)
    }

    /// Whether the unit builds a proc macro's module, whose expanders the front end gives its
    /// crate: a `proc-macro` target's `Build` unit. Its `Check` unit (a member's `blaze check`)
    /// shares that crate but links nothing, and a `--test` one is a test program, so neither
    /// gives the crate expanders. The one rule the front end's lowering and the daemon's
    /// "build what the front end applies first" read.
    pub fn gives_expanders(&self) -> bool {
        self.mode == Mode::Build && self.target.is_proc_macro()
    }

    /// Whether the unit is built for the device ([`crate::GPU_DEVICE`]): an `artifact = "gpu"`
    /// dependency or what it depends on. The front end alone builds it: it has no native code,
    /// no host image links it, and its library is a crate record. The one rule the plan, the
    /// unit graph and the daemon's phases read.
    pub fn for_device(&self) -> bool {
        !self.macro_wasm && self.platform.as_deref() == Some(crate::GPU_DEVICE)
    }
}

/// Whether code built for `built_for` under the profile named `profile` runs on the machine
/// that builds it (`hir_ty_codegen::v2::Runs::Here`, whose code may use every feature of this
/// machine) or elsewhere (`Runs::Elsewhere`, the target's baseline). A dev build of the host's
/// target (no `--target`) runs where it is built: `dev` and `test` are cargo's profiles of
/// `build`, `run`, `test` and their build scripts and proc macros. A `--target` unit, a
/// `release` or `bench` build, or one of a custom profile, which may be shipped, runs
/// elsewhere. The one rule: the code generator's toolchain of a unit and the standard
/// library's key ([`crate::query::StdKey::runs_here`]) both read it.
pub fn runs_here(built_for: BuiltFor<'_>, profile: &str) -> bool {
    built_for.platform.is_none() && matches!(profile, "dev" | "test")
}

/// [`Unit::built_for`]: the triple (none for the host's own units) and whether the unit is
/// built to run as a wasm proc macro. Printed for errors: `the host`, the triple, or the triple
/// and `(proc macros)`.
///
/// The source of truth of what a unit is built for. The front end's `base_db::BuiltFor`
/// (base-db cannot depend on this crate) is made from it only by
/// `blaze_lower::crates::built_for`, and prints the same text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BuiltFor<'a> {
    pub platform: Option<&'a str>,
    pub macro_wasm: bool,
}

impl std::fmt::Display for BuiltFor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.platform, self.macro_wasm) {
            (None, _) => f.write_str("the host"),
            (Some(triple), false) => f.write_str(triple),
            (Some(triple), true) => write!(f, "{triple} (proc macros)"),
        }
    }
}

/// A target as `cargo metadata` prints it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Target {
    /// `lib`, `rlib`, …, `bin`, `example`, `test`, `bench`, `custom-build`, `proc-macro`.
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
    pub name: String,
    pub src_path: PathBuf,
    pub edition: String,
    #[serde(rename = "required-features", default, skip_serializing_if = "Option::is_none")]
    pub required_features: Option<Vec<String>>,
    pub doc: bool,
    pub doctest: bool,
    pub test: bool,
}

impl Target {
    pub fn is_custom_build(&self) -> bool {
        self.kind.iter().any(|it| it == "custom-build")
    }

    pub fn is_proc_macro(&self) -> bool {
        self.kind.iter().any(|it| it == "proc-macro")
    }

    /// Whether it is a library of its package (any crate type), not an example's.
    pub fn is_lib(&self) -> bool {
        self.kind.iter().any(|it| {
            matches!(it.as_str(), "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro")
        })
    }
}

/// A unit's profile as cargo prints it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub opt_level: String,
    pub lto: String,
    pub codegen_backend: Option<String>,
    pub codegen_units: Option<u32>,
    pub debuginfo: DebugInfo,
    pub split_debuginfo: Option<String>,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub rpath: bool,
    pub incremental: bool,
    pub panic: String,
    pub strip: Strip,
    /// blaze: whether the unit checks its native contract clauses at run time (the profile's
    /// `contract-checks`; docs/v2/plan/design/contracts.md, part 1). cargo prints no such key; a
    /// graph that lacks it (cargo's own) has it off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub contract_checks: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rustflags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim_paths: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_mostly_unused: Option<bool>,
}

impl Profile {
    /// cargo's built-in `dev` profile as `--unit-graph` prints it for a dependency: nothing set
    /// in a manifest, no `incremental` (cargo turns that on for members only).
    pub fn dev() -> Profile {
        Profile {
            name: "dev".to_owned(),
            opt_level: "0".to_owned(),
            lto: "false".to_owned(),
            codegen_backend: None,
            codegen_units: None,
            debuginfo: DebugInfo::Level(2),
            split_debuginfo: None,
            debug_assertions: true,
            overflow_checks: true,
            rpath: false,
            incremental: false,
            panic: "unwind".to_owned(),
            strip: Strip::Deferred(Stripped::None),
            contract_checks: true,
            rustflags: Vec::new(),
            trim_paths: None,
            hint_mostly_unused: None,
        }
    }
}

/// A `debug` level: by its number (`0`, `1`, `2`) or by its name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DebugInfo {
    Level(u8),
    Named(String),
}

/// A `strip` setting; `deferred` is cargo's own choice, which nothing in the profile set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strip {
    Resolved(Stripped),
    Deferred(Stripped),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stripped {
    None,
    Named(String),
}

/// What is done with the unit's target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Compiled with `--test`.
    Test,
    Build,
    /// Checked (metadata only), with `--test` or not: cargo prints both as `check`.
    Check,
    Doc,
    Doctest,
    Docscrape,
    /// The build script run, after it was compiled (a `build` unit of its `custom-build`
    /// target).
    RunCustomBuild,
}

/// An edge of the graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitDep {
    /// The unit depended on, by its index in `units`.
    pub index: usize,
    /// The name the dependent's code calls it by.
    pub extern_crate_name: String,
    /// The three below are printed on a nightly cargo only, the options being unstable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noprelude: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nounused: Option<bool>,
    /// Only a dev-dependency makes the edge (cargo's `dev_dependency_edges`): blaze's, not in
    /// cargo's graph. An edge of a build script's compile is a build-dependency either way.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dev: bool,
    /// blaze's: the edge is an `artifact = "gpu"` dependency's, to the unit that builds it for
    /// the device ([`crate::GPU_DEVICE`]). The dependent links nothing of it and has no extern
    /// crate of it; it is given the MSL that unit writes (`BLAZE_GPU_MSL_<CRATE>`). That unit
    /// alone writes MSL: what it depends on is built for the device as libraries.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gpu: bool,
}
