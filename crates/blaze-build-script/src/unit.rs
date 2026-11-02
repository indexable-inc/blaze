//! What a run of a build script is made from: the plain description of the unit cargo calls
//! `RunCustomBuild`, which blaze's manifest reader and resolver fill, and which the harness
//! fills from `cargo metadata` and `cargo build --unit-graph` meanwhile.

use std::path::PathBuf;

use serde_derive::{Deserialize, Serialize};

/// One run of a package's build script.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Unit {
    pub package: Package,
    /// The package's features turned on for this unit, as cargo sorts them.
    pub features: Vec<String>,
    /// The profile of the package's library, which cargo gives the script (not the script's
    /// own profile).
    pub profile: Profile,
    /// `TARGET`: the target triple the package is built for, or the name of its target spec
    /// file without `.json`.
    pub target: String,
    /// `HOST`: the triple of the machine that runs the script.
    pub host: String,
    /// What `rustc --print cfg` says of the target with the unit's flags, one cfg a line as it
    /// prints them: `unix`, `target_os="linux"`.
    pub target_cfgs: Vec<String>,
    /// The flags cargo passes rustc for the unit's target (`RUSTFLAGS`, `build.rustflags`,
    /// `target.<triple>.rustflags`), each one argument.
    pub rustflags: Vec<String>,
    /// `NUM_JOBS`.
    pub jobs: u32,
    /// The executable the script was compiled to.
    pub script: PathBuf,
    /// `OUT_DIR`.
    pub out_dir: PathBuf,
    pub tools: Tools,
    /// Whether the package is a member of the workspace, which `RUSTC_WORKSPACE_WRAPPER` is
    /// given to.
    pub workspace_member: bool,
    /// Each package the unit depends on directly that has a build script, with what that
    /// script printed as metadata: `DEP_<links>_<key>` for those with `links`.
    pub dependencies: Vec<Dependency>,
    /// `[env]` of cargo's configuration, its paths already made absolute.
    pub config_env: Vec<ConfigEnv>,
    pub dylib_search: DylibSearch,
    /// The jobserver the script shares with the build, as its auth string: `R,W` (two file
    /// descriptors), `fifo:PATH`, or a semaphore's name on Windows. `CARGO_MAKEFLAGS` says it.
    pub jobserver: Option<String>,
}

/// A unit that lacks what cargo always gives a script, or gives it in a form cargo never does:
/// the script is not run with a value guessed in its place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unfilled {
    /// A field cargo always fills, empty (`TARGET`, `HOST`, `OUT_DIR`, `RUSTC` and the rest).
    Empty { package: String, field: &'static str },
    /// A version that is not semver's `MAJOR.MINOR.PATCH[-PRE][+BUILD]`.
    Version { package: String, version: String },
    /// A line of `target_cfgs` that is not as `rustc --print cfg` prints one.
    Cfg { package: String, line: String },
    /// A `rust-version` that is not one to three numbers.
    RustVersion { package: String, rust_version: String },
}

impl std::fmt::Display for Unfilled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unfilled::Empty { package, field } => write!(
                f,
                "the build script of `{package}` has no {field}, which cargo always gives a script"
            ),
            Unfilled::Version { package, version } => write!(
                f,
                "the version `{version}` of `{package}` is not a semver version (MAJOR.MINOR.PATCH)"
            ),
            Unfilled::Cfg { package, line } => write!(
                f,
                "the cfg `{line}` of `{package}`'s target is not as `rustc --print cfg` prints one \
                 (`name` or `name=\"value\"`)"
            ),
            Unfilled::RustVersion { package, rust_version } => write!(
                f,
                "the `rust-version` `{rust_version}` of `{package}` is not one to three numbers"
            ),
        }
    }
}

impl std::error::Error for Unfilled {}

impl From<Unfilled> for crate::Error {
    fn from(it: Unfilled) -> crate::Error {
        // alloc: error path.
        crate::Error(it.to_string())
    }
}

impl Unit {
    /// The unit with each field cargo always fills filled.
    pub fn check(&self) -> Result<(), Unfilled> {
        let package = &self.package;
        let paths = [
            ("CARGO_MANIFEST_DIR", package.manifest_dir.as_os_str()),
            ("CARGO_MANIFEST_PATH", package.manifest_path.as_os_str()),
            ("the script's executable", self.script.as_os_str()),
            ("OUT_DIR", self.out_dir.as_os_str()),
            ("CARGO", self.tools.cargo.as_os_str()),
            ("RUSTC", self.tools.rustc.as_os_str()),
            ("RUSTDOC", self.tools.rustdoc.as_os_str()),
        ];
        let texts = [
            ("CARGO_PKG_NAME", package.name.as_str()),
            ("CARGO_PKG_VERSION", package.version.as_str()),
            ("TARGET", self.target.as_str()),
            ("HOST", self.host.as_str()),
            ("OPT_LEVEL", self.profile.opt_level.as_str()),
        ];
        let empty = paths.iter().find(|(_, it)| it.is_empty()).map(|(field, _)| *field);
        let empty = empty.or_else(|| texts.iter().find(|(_, it)| it.is_empty()).map(|(field, _)| *field));
        match empty {
            // alloc: error path.
            Some(field) => Err(Unfilled::Empty { package: package.descr(), field }),
            None => Ok(()),
        }
    }
}

/// The package whose script runs: what of its manifest a run reads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Package {
    pub name: String,
    /// As written in the manifest, `1.2.3-pre+build`.
    pub version: String,
    /// How cargo names the package in messages: `foo v0.1.0 (/path/to/foo)`, with no
    /// directory for crates.io's. `name vversion` when not given.
    pub descr: Option<String>,
    pub manifest_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub authors: Vec<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub license: Option<String>,
    pub license_file: Option<String>,
    pub readme: Option<String>,
    /// `rust-version`, as written.
    pub rust_version: Option<String>,
    pub links: Option<String>,
    /// The package's targets, as `cargo metadata` lists them.
    pub targets: Vec<Target>,
}

/// One of a package's targets, as `cargo metadata` lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Target {
    pub name: String,
    /// `["lib"]`, `["cdylib", "rlib"]`, `["proc-macro"]`, `["bin"]`, `["example"]`,
    /// `["test"]`, `["bench"]` or `["custom-build"]`.
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
}

/// The kinds of a library target, which `cargo metadata` gives as its crate types.
const LIB_KINDS: [&str; 6] = ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];

impl Target {
    pub fn is_lib(&self) -> bool {
        self.kind.iter().any(|it| LIB_KINDS.contains(&it.as_str()))
    }

    pub fn is_cdylib(&self) -> bool {
        self.is_lib() && self.crate_types.iter().any(|it| it == "cdylib")
    }

    pub fn is_bin(&self) -> bool {
        self.kind.iter().any(|it| it == "bin")
    }

    pub fn is_example(&self) -> bool {
        self.kind.iter().any(|it| it == "example")
    }

    /// An example built as a program, not a library.
    pub fn is_exe_example(&self) -> bool {
        self.is_example() && self.crate_types.iter().all(|it| it == "bin")
    }

    pub fn is_test(&self) -> bool {
        self.kind.iter().any(|it| it == "test")
    }

    pub fn is_bench(&self) -> bool {
        self.kind.iter().any(|it| it == "bench")
    }

    /// The name of the target's crate: its name with `-` as `_`.
    pub fn crate_name(&self) -> String {
        self.name.replace('-', "_")
    }
}

impl Package {
    /// How cargo names the package in messages.
    pub fn descr(&self) -> String {
        // alloc: a message's text, made when one is.
        self.descr.clone().unwrap_or_else(|| format!("{} v{}", self.name, self.version))
    }

    /// [`Package::descr`], written into `f` with nothing allocated.
    pub fn write_descr(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        match &self.descr {
            Some(descr) => f.write_str(descr),
            None => write!(f, "{} v{}", self.name, self.version),
        }
    }

    /// The name of the package's library's crate, if it has one.
    pub fn library_crate_name(&self) -> Option<String> {
        self.targets.iter().find(|it| it.is_lib()).map(Target::crate_name)
    }
}

/// The profile cargo built the package's library with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// `dev`, `release` or a custom profile's name.
    pub name: String,
    /// Whether the profile is `release` or inherits from it, rather than from `dev`: `PROFILE`.
    pub release: bool,
    /// `0`, `1`, `2`, `3`, `s` or `z`.
    pub opt_level: String,
    /// Whether any debug info is asked for.
    pub debuginfo: bool,
    pub debug_assertions: bool,
}

/// The programs cargo names to a script.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tools {
    /// `CARGO`.
    pub cargo: PathBuf,
    /// `RUSTC`: the compiler, not its wrapper.
    pub rustc: PathBuf,
    /// `RUSTDOC`.
    pub rustdoc: PathBuf,
    /// `build.rustc-wrapper`, or `RUSTC_WRAPPER` from cargo's environment.
    pub rustc_wrapper: Option<PathBuf>,
    /// `build.rustc-workspace-wrapper`, or `RUSTC_WORKSPACE_WRAPPER`.
    pub rustc_workspace_wrapper: Option<PathBuf>,
    /// The linker configured for the unit's target (`target.<triple>.linker`, or a
    /// `target.'cfg(..)'.linker` that matches): `RUSTC_LINKER`.
    pub linker: Option<PathBuf>,
}

/// A package the unit depends on directly whose build script has run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dependency {
    /// The name the unit gives it, which is its package's name unless renamed.
    pub name: String,
    pub links: Option<String>,
    /// What its script printed as metadata, in order.
    pub metadata: Vec<(String, String)>,
}

/// A variable of `[env]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigEnv {
    pub name: String,
    pub value: String,
    /// `force = true`: set even when cargo's environment has it.
    pub force: bool,
}

/// Where the script's dynamic libraries are looked for: what cargo puts in front of
/// `LD_LIBRARY_PATH`, `DYLD_FALLBACK_LIBRARY_PATH` or `PATH` for a program it runs on the host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DylibSearch {
    /// The directory of the host's artifacts: `target/debug`. None when the build writes none
    /// there (`cargo check` without `--timings`), and cargo then adds neither it nor
    /// `plugin_search`.
    pub host_root: Option<PathBuf>,
    /// Where the host's dependencies are: `target/debug/deps`.
    pub host_deps: Vec<PathBuf>,
    /// The host's library directory in the sysroot (`rustc --print target-libdir`). Empty for
    /// a toolchain that has none (blaze's, which ships no dynamic std): then nothing is added,
    /// as an empty entry would have the loader search the working directory.
    pub sysroot_libdir: PathBuf,
    /// What the scripts of the host-only dependencies (build dependencies, proc macros) of the
    /// unit added with `rustc-link-search`, in order, kinds included (`native=/x`).
    pub plugin_search: Vec<PathBuf>,
}
