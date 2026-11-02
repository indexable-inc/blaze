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
    /// build script or proc macro and what they depend on).
    pub platform: Option<String>,
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
}
