//! See [`CargoWorkspace`].

use std::{borrow::Cow, fmt, ops, str::from_utf8};

use anyhow::Context;
use base_db::{ConstEvalBudget, Env, TargetKind};
use cargo_metadata::{CargoOpt, MetadataCommand, PackageId};
use la_arena::{Arena, Idx};
use paths::{AbsPath, AbsPathBuf, Utf8Path, Utf8PathBuf};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_derive::Deserialize;
use serde_json::from_value;
use span::Edition;
use stdx::process::spawn_with_streaming_output;
use toolchain::{NO_RUSTUP_AUTO_INSTALL_ENV, Tool};
use triomphe::Arc;

use crate::{
    CfgOverrides, InvocationStrategy, ManifestPath, Sysroot,
    cargo_config_file::{LockfileCopy, LockfileUsage, make_lockfile_copy},
};

/// [`CargoWorkspace`] represents the logical structure of, well, a Cargo
/// workspace. It pretty closely mirrors `cargo metadata` output.
///
/// Note that internally, rust-analyzer uses a different structure:
/// `CrateGraph`. `CrateGraph` is lower-level: it knows only about the crates,
/// while this knows about `Packages` & `Targets`: purely cargo-related
/// concepts.
///
/// We use absolute paths here, `cargo metadata` guarantees to always produce
/// abs paths.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CargoWorkspace {
    packages: Arena<PackageData>,
    targets: Arena<TargetData>,
    workspace_root: AbsPathBuf,
    target_directory: AbsPathBuf,
    build_directory: Option<AbsPathBuf>,
    manifest_path: ManifestPath,
    is_virtual_workspace: bool,
    /// Whether this workspace represents the sysroot workspace.
    is_sysroot: bool,
    /// Environment variables set in the `.cargo/config` file and the extraEnv
    /// configuration option.
    env: Env,
    requires_rustc_private: bool,
    /// The directory the build put the executables of the workspace in.
    executables: Option<AbsPathBuf>,
    /// The directory cargo gives the integration tests and benchmarks of the build to write
    /// in, `CARGO_TARGET_TMPDIR`.
    scratch: Option<AbsPathBuf>,
}

impl ops::Index<Package> for CargoWorkspace {
    type Output = PackageData;
    fn index(&self, index: Package) -> &PackageData {
        &self.packages[index]
    }
}

impl ops::Index<Target> for CargoWorkspace {
    type Output = TargetData;
    fn index(&self, index: Target) -> &TargetData {
        &self.targets[index]
    }
}

/// Describes how to set the rustc source directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RustLibSource {
    /// Explicit path for the rustc source directory.
    Path(AbsPathBuf),
    /// Try to automatically detect where the rustc source directory is.
    Discover,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CargoFeatures {
    All,
    Selected {
        /// List of features to activate.
        features: Vec<String>,
        /// Do not activate the `default` feature.
        no_default_features: bool,
    },
}

impl Default for CargoFeatures {
    fn default() -> Self {
        CargoFeatures::Selected { features: vec![], no_default_features: false }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TargetDirectoryConfig {
    #[default]
    None,
    UseSubdirectory,
    Directory(Utf8PathBuf),
}

impl TargetDirectoryConfig {
    pub fn target_dir<'a>(
        &'a self,
        ws_target_dir: Option<&'a Utf8Path>,
    ) -> Option<Cow<'a, Utf8Path>> {
        match self {
            TargetDirectoryConfig::None => None,
            TargetDirectoryConfig::UseSubdirectory => {
                Some(Cow::Owned(ws_target_dir?.join("rust-analyzer")))
            }
            TargetDirectoryConfig::Directory(dir) => Some(Cow::Borrowed(dir)),
        }
    }
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct CargoConfig {
    /// Whether to pass `--all-targets` to cargo invocations.
    pub all_targets: bool,
    /// List of features to activate.
    pub features: CargoFeatures,
    /// rustc target
    pub target: Option<String>,
    /// Sysroot loading behavior
    pub sysroot: Option<RustLibSource>,
    pub sysroot_src: Option<AbsPathBuf>,
    /// rustc private crate source
    pub rustc_source: Option<RustLibSource>,
    /// Extra includes to add to the VFS.
    pub extra_includes: Vec<AbsPathBuf>,
    pub cfg_overrides: CfgOverrides,
    /// Invoke `cargo check` through the RUSTC_WRAPPER.
    pub wrap_rustc_in_build_scripts: bool,
    /// The command to run instead of `cargo check` for building build scripts.
    pub run_build_script_command: Option<Vec<String>>,
    /// Extra args to pass to the cargo command.
    pub extra_args: Vec<String>,
    /// Extra args passed only to `cargo metadata`, not other cargo commands.
    pub metadata_extra_args: Vec<String>,
    /// Path to an extra config file passed to every cargo invocation via `--config`.
    pub config_path: Option<AbsPathBuf>,
    /// Extra env vars to set when invoking the cargo command
    pub extra_env: FxHashMap<String, Option<String>>,
    /// Whether `extra_env` is only what cargo and rustc are run with, and no crate is told it.
    pub extra_env_for_commands_only: bool,
    pub invocation_strategy: InvocationStrategy,
    /// Optional path to use instead of `target` when building
    pub target_dir_config: TargetDirectoryConfig,
    /// Gate `#[test]` behind `#[cfg(test)]`
    pub set_test: bool,
    /// Load the project without any dependencies
    pub no_deps: bool,
    /// Run the build scripts as a build of the manifest's package has them, not as a build of
    /// every member of its workspace does: the features of the two differ.
    pub only_manifest_package: bool,
    /// The directory a build put the executables of the workspace in, which the tests of a
    /// package are told as `CARGO_BIN_EXE_<name>`.
    pub executables: Option<AbsPathBuf>,
    /// The directory the integration tests and benchmarks of a build are told to write in as
    /// `CARGO_TARGET_TMPDIR`, which cargo makes `<target-dir>/tmp`.
    pub scratch: Option<AbsPathBuf>,
    /// Where the workspace and its `cargo metadata` come from instead of cargo. When it fails,
    /// so does the load: cargo is not asked instead.
    pub own_metadata: Option<OwnMetadata>,
    /// What builds and runs the build scripts and proc macros of a workspace in place of
    /// `cargo check --compile-time-deps`.
    pub own_build_scripts: Option<OwnBuildScripts>,
}

/// The root manifest of the workspace a manifest is in, and `cargo metadata` of it for the
/// target with the triple and the `cfg` given, cargo run with the variables given.
type Root = dyn Fn(&ManifestPath) -> anyhow::Result<ManifestPath> + Send + Sync;
type Metadata = dyn Fn(
        &ManifestPath,
        Option<&str>,
        &[cfg::CfgAtom],
        &FxHashMap<String, Option<String>>,
    ) -> anyhow::Result<cargo_metadata::Metadata>
    + Send
    + Sync;

/// `cargo metadata` of the standard library's workspace at the manifest given (a toolchain's
/// `library/Cargo.toml`), as `Sysroot::load_workspace` asks cargo for it: with the features
/// given, for the target with the triple given (none: every platform's), `--no-deps` or not,
/// cargo run with the variables given.
type Library = dyn Fn(
        &ManifestPath,
        &CargoFeatures,
        Option<&str>,
        bool,
        &FxHashMap<String, Option<String>>,
    ) -> anyhow::Result<cargo_metadata::Metadata>
    + Send
    + Sync;

/// What reads a workspace in place of `cargo locate-project` and `cargo metadata`.
#[derive(Clone)]
pub struct OwnMetadata {
    pub root: std::sync::Arc<Root>,
    pub metadata: std::sync::Arc<Metadata>,
    /// The standard library's workspace, which cargo is never asked for when this is set.
    pub library: std::sync::Arc<Library>,
}

impl PartialEq for OwnMetadata {
    fn eq(&self, other: &OwnMetadata) -> bool {
        std::sync::Arc::ptr_eq(&self.root, &other.root)
            && std::sync::Arc::ptr_eq(&self.metadata, &other.metadata)
            && std::sync::Arc::ptr_eq(&self.library, &other.library)
    }
}

impl Eq for OwnMetadata {}

impl fmt::Debug for OwnMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OwnMetadata")
    }
}

/// What a load is asked to build its build scripts and proc macros for: the toolchain and
/// what `rustc --print cfg` says of the target.
pub struct OwnBuildAsked<'a> {
    pub sysroot: &'a crate::Sysroot,
    pub rustc_cfg: &'a [cfg::CfgAtom],
    pub config: &'a CargoConfig,
}

/// What the build script of a package said and where it wrote, and the proc macro the
/// package has, as a build of `cargo check --compile-time-deps` has them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnBuildOutput {
    /// `cargo::rustc-cfg`, as cargo prints them in `build-script-executed`.
    pub cfgs: Vec<String>,
    /// `cargo::rustc-env`.
    pub envs: Vec<(String, String)>,
    /// `OUT_DIR`, when the package has a build script that ran.
    pub out_dir: Option<AbsPathBuf>,
    /// The library of the package's proc macro.
    pub proc_macro_dylib: Option<AbsPathBuf>,
}

type BuildScripts = dyn Fn(&CargoWorkspace, &OwnBuildAsked<'_>) -> anyhow::Result<Vec<(Package, OwnBuildOutput)>>
    + Send
    + Sync;

/// What builds and runs the build scripts and proc macros of a workspace in place of cargo.
/// A failure is the load's: cargo is not asked instead.
#[derive(Clone)]
pub struct OwnBuildScripts(pub std::sync::Arc<BuildScripts>);

impl PartialEq for OwnBuildScripts {
    fn eq(&self, other: &OwnBuildScripts) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for OwnBuildScripts {}

impl fmt::Debug for OwnBuildScripts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OwnBuildScripts")
    }
}

pub type Package = Idx<PackageData>;

pub type Target = Idx<TargetData>;

/// Information associated with a cargo crate
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PackageData {
    /// Version given in the `Cargo.toml`
    pub version: semver::Version,
    /// Name as given in the `Cargo.toml`
    pub name: String,
    /// Repository as given in the `Cargo.toml`
    pub repository: Option<String>,
    /// Path containing the `Cargo.toml`
    pub manifest: ManifestPath,
    /// Targets provided by the crate (lib, bin, example, test, ...)
    pub targets: Vec<Target>,
    /// Does this package come from the local filesystem (and is editable)?
    pub is_local: bool,
    /// Whether this package is a member of the workspace
    pub is_member: bool,
    /// List of packages this package depends on
    pub dependencies: Vec<PackageDependency>,
    /// Rust edition for this package
    pub edition: Edition,
    /// Features provided by the crate, mapped to the features required by that feature.
    pub features: FxHashMap<String, Vec<String>>,
    /// List of features enabled on this package
    pub active_features: Vec<String>,
    /// What the profile cargo builds the package with changes of the toolchain's cfg, when the
    /// build is known.
    pub profile_cfg: Option<cfg::CfgDiff>,
    /// Whether the package is one the build was asked for, which cargo tells its crates with
    /// `CARGO_PRIMARY_PACKAGE`.
    pub primary: bool,
    /// Package id
    pub id: Arc<PackageId>,
    /// Authors as given in the `Cargo.toml`
    pub authors: Vec<String>,
    /// Description as given in the `Cargo.toml`
    pub description: Option<String>,
    /// Homepage as given in the `Cargo.toml`
    pub homepage: Option<String>,
    /// License as given in the `Cargo.toml`
    pub license: Option<String>,
    /// License file as given in the `Cargo.toml`
    pub license_file: Option<Utf8PathBuf>,
    /// Readme file as given in the `Cargo.toml`
    pub readme: Option<Utf8PathBuf>,
    /// Rust version as given in the `Cargo.toml`
    pub rust_version: Option<semver::Version>,
    /// `links` as given in the `Cargo.toml`: the native library its build script links.
    pub links: Option<String>,
    /// The contents of [package.metadata.rust-analyzer]
    pub metadata: RustAnalyzerPackageMetaData,
    /// `const-macros` in [package.metadata.blaze]: the file whose functions are the package's
    /// const macros (docs/future/const-macros.md), or why it can't be.
    pub const_macros: Option<Result<AbsPathBuf, String>>,
    /// If this package is a member of the workspace, store all direct and transitive
    /// dependencies as long as they are workspace members, to track dependency relationships
    /// between members.
    pub all_member_deps: Option<FxHashSet<Package>>,
}

#[derive(Deserialize, Default, Debug, Clone, Eq, PartialEq)]
pub struct RustAnalyzerPackageMetaData {
    pub rustc_private: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PackageDependency {
    pub pkg: Package,
    pub name: String,
    pub kind: DepKind,
    /// The name a feature of the package turns the dependency on with (`dep:name`), when
    /// every entry of the manifest for it of its kind is optional: whether it is built is
    /// then the package's features' to say, which [`PackageData::activates`] reads.
    pub optional: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepKind {
    /// Available to the library, binary, and dev targets in the package (but not the build script).
    Normal,
    /// Available only to test and bench targets (and the library target, when built with `cfg(test)`).
    Dev,
    /// Available only to the build script target.
    Build,
}

impl DepKind {
    fn iter(list: &[cargo_metadata::DepKindInfo]) -> impl Iterator<Item = Self> {
        let mut dep_kinds = [None; 3];
        if list.is_empty() {
            dep_kinds[0] = Some(Self::Normal);
        }
        for info in list {
            match info.kind {
                cargo_metadata::DependencyKind::Normal => dep_kinds[0] = Some(Self::Normal),
                cargo_metadata::DependencyKind::Development => dep_kinds[1] = Some(Self::Dev),
                cargo_metadata::DependencyKind::Build => dep_kinds[2] = Some(Self::Build),
                cargo_metadata::DependencyKind::Unknown => continue,
            }
        }
        dep_kinds.into_iter().flatten()
    }
}

/// Information associated with a package's target
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TargetData {
    /// Package that provided this target
    pub package: Package,
    /// Name as given in the `Cargo.toml` or generated from the file name
    pub name: String,
    /// Path to the main source file of the target
    pub root: AbsPathBuf,
    /// Kind of target
    pub kind: TargetKind,
    /// Required features of the target without which it won't build
    pub required_features: Vec<String>,
    /// Whether `cargo test` builds it as tests (`test = true`), which an example is only when
    /// its manifest says so.
    pub tested: bool,
}

fn convert_target_kind(kinds: &[cargo_metadata::TargetKind]) -> TargetKind {
    for kind in kinds {
        return match kind {
            cargo_metadata::TargetKind::Bin => TargetKind::Bin,
            cargo_metadata::TargetKind::Test => TargetKind::Test,
            cargo_metadata::TargetKind::Bench => TargetKind::Bench,
            cargo_metadata::TargetKind::Example => TargetKind::Example,
            cargo_metadata::TargetKind::CustomBuild => TargetKind::BuildScript,
            cargo_metadata::TargetKind::ProcMacro => TargetKind::Lib { is_proc_macro: true },
            cargo_metadata::TargetKind::Lib
            | cargo_metadata::TargetKind::DyLib
            | cargo_metadata::TargetKind::CDyLib
            | cargo_metadata::TargetKind::StaticLib
            | cargo_metadata::TargetKind::RLib => TargetKind::Lib { is_proc_macro: false },
            _ => continue,
        };
    }
    TargetKind::Other
}

/// The budget `const-eval` under `[workspace.metadata.blaze]` sets ([`ConstEvalBudget`]), from
/// the workspace's `metadata` as `cargo metadata` prints it: the default for what it leaves
/// out, and an error naming the key for anything that is not a positive integer of a known key.
pub(crate) fn const_eval_budget(metadata: &serde_json::Value) -> anyhow::Result<ConstEvalBudget> {
    let mut budget = ConstEvalBudget::default();
    let Some(table) = metadata.get("blaze").and_then(|it| it.get(ConstEvalBudget::KEY)) else {
        return Ok(budget);
    };
    let key = ConstEvalBudget::KEY;
    let table = table.as_object().with_context(|| {
        format!("`{key}` is a table: `{key} = {{ steps = N, memory = BYTES }}`")
    })?;
    for (name, value) in table {
        let slot = match name.as_str() {
            "steps" => &mut budget.steps,
            "memory" => &mut budget.memory,
            _ => anyhow::bail!("`{key}.{name}` is not a setting (known: `steps`, `memory`)"),
        };
        *slot = value
            .as_u64()
            .filter(|it| *it > 0)
            .with_context(|| format!("`{key}.{name}` is a positive integer, not `{value}`"))?;
    }
    Ok(budget)
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct CargoMetadataConfig {
    /// List of features to activate.
    pub features: CargoFeatures,
    /// rustc targets
    pub targets: Vec<String>,
    /// Extra args to pass to the cargo command.
    pub extra_args: Vec<String>,
    /// Extra args passed directly to `cargo metadata` without filtering.
    pub metadata_extra_args: Vec<String>,
    /// Path to an extra config file passed to `cargo metadata` via `--config`.
    pub config_path: Option<AbsPathBuf>,
    /// Extra env vars to set when invoking the cargo command
    pub extra_env: FxHashMap<String, Option<String>>,
    /// What kind of metadata are we fetching: workspace, rustc, or sysroot.
    pub kind: &'static str,
    /// The toolchain version, if known.
    /// Used to conditionally enable unstable cargo features.
    pub toolchain_version: Option<semver::Version>,
    /// What reads the standard library's workspace in place of cargo (`kind` "sysroot").
    pub own_metadata: Option<OwnMetadata>,
}

// Deserialize helper for the cargo metadata
#[derive(Deserialize, Default)]
struct PackageMetadata {
    #[serde(rename = "rust-analyzer")]
    rust_analyzer: Option<RustAnalyzerPackageMetaData>,
}

/// The experiment const macros are behind, in `experimental` of [workspace.metadata.blaze].
pub const CONST_MACROS: &str = "const-macros";

/// Whether the root manifest's [workspace.metadata.blaze] lists [`CONST_MACROS`] in
/// `experimental`. That list's other names and its shape are blaze-cargo's to check.
fn const_macros_on(workspace_metadata: &serde_json::Value) -> bool {
    let listed = workspace_metadata.get("blaze").and_then(|it| it.get("experimental"));
    listed
        .and_then(|it| it.as_array())
        .is_some_and(|names| names.iter().any(|it| it.as_str() == Some(CONST_MACROS)))
}

/// `const-macros` of a package's [package.metadata.blaze], whose manifest is at `manifest`.
fn const_macros_of(
    metadata: &serde_json::Value,
    manifest: &Utf8Path,
    root_manifest: &ManifestPath,
    on: bool,
) -> Option<Result<AbsPathBuf, String>> {
    let value = metadata.get("blaze")?.get(CONST_MACROS)?;
    let Some(path) = value.as_str() else {
        return Some(Err(format!(
            "`{CONST_MACROS}` under [package.metadata.blaze] in {manifest} is a path, as a string"
        )));
    };
    if !on {
        return Some(Err(format!(
            "`{CONST_MACROS}` under [package.metadata.blaze] in {manifest} is an experiment: \
             `experimental = [\"{CONST_MACROS}\"]` under [workspace.metadata.blaze] in \
             {root_manifest} turns it on"
        )));
    }
    let file = AbsPath::assert(manifest).parent()?.join(path);
    if !std::fs::exists(&file).unwrap_or(false) {
        return Some(Err(format!(
            "`{CONST_MACROS}` under [package.metadata.blaze] in {manifest} names {file}, \
             which does not exist"
        )));
    }
    Some(Ok(file))
}

impl CargoWorkspace {
    /// Why a package's `const-macros` can't be used, for the first package of the workspace
    /// whose can't.
    pub fn const_macros_error(&self) -> Option<&str> {
        self.packages
            .iter()
            .find_map(|(_, it)| it.const_macros.as_ref()?.as_ref().err())
            .map(String::as_str)
    }

    pub fn new(
        mut meta: cargo_metadata::Metadata,
        ws_manifest_path: ManifestPath,
        cargo_env: Env,
        is_sysroot: bool,
    ) -> CargoWorkspace {
        let mut pkg_by_id = FxHashMap::default();
        // For each package, its manifest's dependencies: the name it is known by in the
        // package (its `rename`, or its own, with `_` for `-`), the name its features turn it
        // on with, its kind, and whether it is optional.
        let mut manifest_deps: FxHashMap<Arc<PackageId>, Vec<(String, String, DepKind, bool)>> =
            FxHashMap::default();
        let mut packages = Arena::default();
        let mut targets = Arena::default();

        let ws_members = &meta.workspace_members;

        let workspace_root = AbsPathBuf::assert(meta.workspace_root);
        let target_directory = AbsPathBuf::assert(meta.target_directory);
        let build_directory = meta.build_directory.map(AbsPathBuf::assert);
        let mut is_virtual_workspace = true;
        let mut requires_rustc_private = false;

        let mut members = FxHashSet::default();
        let const_macros_on = const_macros_on(&meta.workspace_metadata);

        meta.packages.sort_by(|a, b| a.id.cmp(&b.id));
        for meta_pkg in meta.packages {
            let cargo_metadata::Package {
                name,
                version,
                id,
                source,
                targets: meta_targets,
                features,
                manifest_path,
                repository,
                edition,
                metadata,
                authors,
                description,
                homepage,
                license,
                license_file,
                readme,
                rust_version,
                dependencies: declared,
                links,
                ..
            } = meta_pkg;
            let id = Arc::new(id);
            let declared = declared.into_iter().filter_map(|dep| {
                let kind = match dep.kind {
                    cargo_metadata::DependencyKind::Normal => DepKind::Normal,
                    cargo_metadata::DependencyKind::Development => DepKind::Dev,
                    cargo_metadata::DependencyKind::Build => DepKind::Build,
                    cargo_metadata::DependencyKind::Unknown => return None,
                };
                let key = dep.rename.unwrap_or(dep.name);
                Some((key.replace('-', "_"), key, kind, dep.optional))
            });
            manifest_deps.insert(id.clone(), declared.collect());
            let const_macros =
                const_macros_of(&metadata, &manifest_path, &ws_manifest_path, const_macros_on);
            let meta = from_value::<PackageMetadata>(metadata).unwrap_or_default();
            let edition = match edition {
                cargo_metadata::Edition::E2015 => Edition::Edition2015,
                cargo_metadata::Edition::E2018 => Edition::Edition2018,
                cargo_metadata::Edition::E2021 => Edition::Edition2021,
                cargo_metadata::Edition::E2024 => Edition::Edition2024,
                _ => {
                    tracing::error!("Unsupported edition `{:?}`", edition);
                    Edition::CURRENT
                }
            };
            // We treat packages without source as "local" packages. That includes all members of
            // the current workspace, as well as any path dependency outside the workspace.
            let is_local = source.is_none();
            let is_member = ws_members.contains(&id);

            let manifest = ManifestPath::try_from(AbsPathBuf::assert(manifest_path)).unwrap();
            is_virtual_workspace &= manifest != ws_manifest_path;
            let pkg = packages.alloc(PackageData {
                id: id.clone(),
                name: name.to_string(),
                version,
                manifest: manifest.clone(),
                targets: Vec::new(),
                is_local,
                is_member,
                edition,
                repository,
                authors,
                description,
                homepage,
                license,
                license_file,
                readme,
                rust_version,
                links,
                dependencies: Vec::new(),
                features: features.into_iter().collect(),
                active_features: Vec::new(),
                profile_cfg: None,
                primary: false,
                metadata: meta.rust_analyzer.unwrap_or_default(),
                const_macros,
                all_member_deps: None,
            });
            if is_member {
                members.insert(pkg);
            }
            let pkg_data = &mut packages[pkg];
            requires_rustc_private |= pkg_data.metadata.rustc_private;
            pkg_by_id.insert(id, pkg);
            for meta_tgt in meta_targets {
                let cargo_metadata::Target {
                    name,
                    kind,
                    required_features,
                    src_path,
                    test: tested,
                    ..
                } = meta_tgt;
                let kind = convert_target_kind(&kind);
                let tgt = targets.alloc(TargetData {
                    package: pkg,
                    name,
                    root: if kind == TargetKind::Bin
                        && manifest.extension().is_some_and(|ext| ext == "rs")
                    {
                        // cargo strips the script part of a cargo script away and places the
                        // modified manifest file into a special target dir which is then used as
                        // the source path. We don't want that, we want the original here so map it
                        // back
                        manifest.clone().into()
                    } else {
                        AbsPathBuf::assert(src_path)
                    },
                    kind,
                    required_features,
                    tested,
                });
                pkg_data.targets.push(tgt);
            }
        }
        for mut node in meta.resolve.map_or_else(Vec::new, |it| it.nodes) {
            let &source = pkg_by_id.get(&node.id).unwrap();
            node.deps.sort_by(|a, b| a.pkg.cmp(&b.pkg));
            let dependencies = node
                .deps
                .iter()
                .flat_map(|dep| DepKind::iter(&dep.dep_kinds).map(move |kind| (dep, kind)));
            let declared = manifest_deps.get(&node.id).map_or(&[][..], |it| &**it);
            for (dep_node, kind) in dependencies {
                let &pkg = pkg_by_id.get(&dep_node.pkg).unwrap();
                // `cargo metadata`'s graph has an optional dependency that only a weak
                // feature (`dep?/feature`) names, as hashbrown's `nightly` names foldhash for
                // the standard library, which cargo does not build: the features say.
                let mut entries = declared
                    .iter()
                    .filter(|(named, _, of, _)| *named == *dep_node.name && *of == kind)
                    .peekable();
                let key = entries.peek().map(|(_, key, _, _)| key.clone());
                let optional = key.filter(|_| entries.all(|(_, _, _, optional)| *optional));
                let dep =
                    PackageDependency { name: dep_node.name.to_string(), pkg, kind, optional };
                packages[source].dependencies.push(dep);
            }
            packages[source]
                .active_features
                .extend(node.features.into_iter().map(|it| it.to_string()));
        }

        fn saturate_all_member_deps(
            packages: &mut Arena<PackageData>,
            to_visit: Package,
            visited: &mut FxHashSet<Package>,
            members: &FxHashSet<Package>,
        ) {
            let pkg_data = &mut packages[to_visit];

            if !visited.insert(to_visit) {
                return;
            }

            let deps: Vec<_> = pkg_data
                .dependencies
                .iter()
                .filter_map(|dep| {
                    let pkg = dep.pkg;
                    if members.contains(&pkg) { Some(pkg) } else { None }
                })
                .collect();

            let mut all_member_deps = FxHashSet::from_iter(deps.iter().copied());
            for dep in deps {
                saturate_all_member_deps(packages, dep, visited, members);
                if let Some(transitives) = &packages[dep].all_member_deps {
                    all_member_deps.extend(transitives);
                }
            }

            packages[to_visit].all_member_deps = Some(all_member_deps);
        }

        let mut visited = FxHashSet::default();
        for member in members.iter() {
            saturate_all_member_deps(&mut packages, *member, &mut visited, &members);
        }

        CargoWorkspace {
            packages,
            targets,
            workspace_root,
            target_directory,
            build_directory,
            manifest_path: ws_manifest_path,
            is_virtual_workspace,
            requires_rustc_private,
            executables: None,
            scratch: None,
            is_sysroot,
            env: cargo_env,
        }
    }

    /// Replaces the features `cargo metadata` resolved for a package, which are those of a
    /// build of the whole workspace.
    pub fn set_active_features(&mut self, package: Package, features: Vec<String>) {
        self.packages[package].active_features = features;
    }

    /// Sets what the profile the package is built with changes of the toolchain's cfg.
    pub fn set_profile_cfg(&mut self, package: Package, cfg: cfg::CfgDiff) {
        self.packages[package].profile_cfg = Some(cfg);
    }

    /// Sets whether the package is one the build was asked for.
    pub fn set_primary(&mut self, package: Package, primary: bool) {
        self.packages[package].primary = primary;
    }

    /// Leaves out what only tests, examples and benchmarks depend on.
    pub fn drop_dev_dependencies(&mut self) {
        for (_, package) in self.packages.iter_mut() {
            package.dependencies.retain(|it| it.kind != DepKind::Dev);
        }
    }

    /// The workspace with each member package `moved` picks, by its directory, at the place
    /// under `to` that it has under `from`. The workspace is told to be where it was.
    pub fn placed(
        &self,
        from: &AbsPath,
        to: &AbsPath,
        moved: &dyn Fn(&AbsPath) -> bool,
    ) -> CargoWorkspace {
        let place = |path: &AbsPath| match path.strip_prefix(from) {
            Some(inside) => to.join(inside),
            None => path.to_path_buf(),
        };
        let mut placed = self.clone();
        // What the tests of the build are told of where it puts things is of its checkout.
        placed.executables = self.executables.as_deref().map(place);
        placed.scratch = self.scratch.as_deref().map(place);
        let mut targets = Vec::new();
        for (_, package) in placed.packages.iter_mut() {
            if !package.is_member || !moved(package.manifest.parent()) {
                continue;
            }
            let manifest: &AbsPath = package.manifest.as_ref();
            package.manifest =
                ManifestPath::try_from(place(manifest)).expect("a manifest is in a directory");
            targets.extend(package.targets.iter().copied());
        }
        for target in targets {
            let root = place(&placed.targets[target].root);
            placed.targets[target].root = root;
        }
        placed
    }

    pub fn packages(&self) -> impl ExactSizeIterator<Item = Package> + '_ {
        self.packages.iter().map(|(id, _pkg)| id)
    }

    pub fn target_by_root(&self, root: &AbsPath) -> Option<Target> {
        self.packages()
            .filter(|&pkg| self[pkg].is_member)
            .find_map(|pkg| self[pkg].targets.iter().find(|&&it| self[it].root == root))
            .copied()
    }

    pub fn workspace_root(&self) -> &AbsPath {
        &self.workspace_root
    }

    pub fn manifest_path(&self) -> &ManifestPath {
        &self.manifest_path
    }

    pub fn target_directory(&self) -> &AbsPath {
        &self.target_directory
    }

    pub fn build_directory(&self) -> Option<&AbsPath> {
        self.build_directory.as_deref()
    }

    pub fn package_flag(&self, package: &PackageData) -> String {
        if self.is_unique(&package.name) {
            package.name.clone()
        } else {
            format!("{}:{}", package.name, package.version)
        }
    }

    pub fn parent_manifests(&self, manifest_path: &ManifestPath) -> Option<Vec<ManifestPath>> {
        let mut found = false;
        let parent_manifests = self
            .packages()
            .filter_map(|pkg| {
                if !found && &self[pkg].manifest == manifest_path {
                    found = true
                }
                self[pkg].dependencies.iter().find_map(|dep| {
                    (&self[dep.pkg].manifest == manifest_path).then(|| self[pkg].manifest.clone())
                })
            })
            .collect::<Vec<ManifestPath>>();

        // some packages has this pkg as dep. return their manifests
        if !parent_manifests.is_empty() {
            return Some(parent_manifests);
        }

        // this pkg is inside this cargo workspace, fallback to workspace root
        if found {
            return Some(vec![
                ManifestPath::try_from(self.workspace_root().join("Cargo.toml")).ok()?,
            ]);
        }

        // not in this workspace
        None
    }

    /// Returns the union of the features of all member crates in this workspace.
    pub fn workspace_features(&self) -> FxHashSet<String> {
        self.packages()
            .filter_map(|package| {
                let package = &self[package];
                if package.is_member {
                    Some(package.features.keys().cloned().chain(
                        package.features.keys().map(|key| format!("{}/{key}", package.name)),
                    ))
                } else {
                    None
                }
            })
            .flatten()
            .collect()
    }

    fn is_unique(&self, name: &str) -> bool {
        self.packages.iter().filter(|(_, v)| v.name == name).count() == 1
    }

    pub fn is_virtual_workspace(&self) -> bool {
        self.is_virtual_workspace
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    pub fn executables(&self) -> Option<&AbsPath> {
        self.executables.as_deref()
    }

    pub fn set_executables(&mut self, directory: Option<AbsPathBuf>) {
        self.executables = directory;
    }

    pub fn scratch(&self) -> Option<&AbsPath> {
        self.scratch.as_deref()
    }

    pub fn set_scratch(&mut self, directory: Option<AbsPathBuf>) {
        self.scratch = directory;
    }

    pub fn is_sysroot(&self) -> bool {
        self.is_sysroot
    }

    pub fn requires_rustc_private(&self) -> bool {
        self.requires_rustc_private
    }
}

pub(crate) struct FetchMetadata {
    command: cargo_metadata::MetadataCommand,
    #[expect(dead_code)]
    manifest_path: ManifestPath,
    lockfile_copy: Option<LockfileCopy>,
    #[expect(dead_code)]
    kind: &'static str,
    no_deps: bool,
    no_deps_result: anyhow::Result<cargo_metadata::Metadata>,
    other_options: Vec<String>,
}

impl FetchMetadata {
    /// Builds a command to fetch metadata for the given `cargo_toml` manifest.
    ///
    /// Performs a lightweight pre-fetch using the `--no-deps` option,
    /// available via `FetchMetadata::no_deps_metadata`, to gather basic
    /// information such as the `target-dir`.
    ///
    /// The provided sysroot is used to set the `RUSTUP_TOOLCHAIN`
    /// environment variable when invoking Cargo, ensuring that the
    /// rustup proxy selects the correct toolchain.
    pub(crate) fn new(
        cargo_toml: &ManifestPath,
        current_dir: &AbsPath,
        config: &CargoMetadataConfig,
        sysroot: &Sysroot,
        no_deps: bool,
    ) -> Self {
        let mut cargo = sysroot.tool(Tool::Cargo, current_dir, &config.extra_env);
        crate::ready_question(&mut cargo);
        let mut command = MetadataCommand::new();
        command.env(NO_RUSTUP_AUTO_INSTALL_ENV.0, NO_RUSTUP_AUTO_INSTALL_ENV.1);
        command.cargo_path(cargo.get_program());
        cargo.get_envs().for_each(|(var, val)| _ = command.env(var, val.unwrap_or_default()));
        command.manifest_path(cargo_toml.to_path_buf());
        match &config.features {
            CargoFeatures::All => {
                command.features(CargoOpt::AllFeatures);
            }
            CargoFeatures::Selected { features, no_default_features } => {
                if *no_default_features {
                    command.features(CargoOpt::NoDefaultFeatures);
                }
                if !features.is_empty() {
                    command.features(CargoOpt::SomeFeatures(features.clone()));
                }
            }
        }
        command.current_dir(current_dir);

        let mut other_options = vec![];
        // cargo metadata only supports a subset of flags of what cargo usually accepts, and usually
        // the only relevant flags for metadata here are unstable ones, so we pass those along
        // but nothing else
        let mut extra_args = config.extra_args.iter();
        while let Some(arg) = extra_args.next() {
            if arg == "-Z"
                && let Some(arg) = extra_args.next()
            {
                other_options.push("-Z".to_owned());
                other_options.push(arg.to_owned());
            }
        }
        other_options.extend(config.metadata_extra_args.iter().cloned());
        if let Some(config_path) = &config.config_path {
            other_options.push("--config".to_owned());
            other_options.push(config_path.to_string());
        }

        let mut lockfile_copy = None;
        if cargo_toml.is_rust_manifest() {
            other_options.push("-Zscript".to_owned());
        } else if let Some(v) = config.toolchain_version.as_ref() {
            lockfile_copy = make_lockfile_copy(
                v,
                &<_ as AsRef<Utf8Path>>::as_ref(cargo_toml).with_extension("lock"),
            );
        }

        if !config.targets.is_empty() {
            let mut has_json_target = false;
            other_options.extend(config.targets.iter().flat_map(|target| {
                has_json_target |= target.ends_with(".json");
                ["--filter-platform".to_owned(), target.clone()]
            }));
            if has_json_target
                && config.toolchain_version.as_ref().is_some_and(|version| {
                    *version >= toolchain::MINIMUM_TOOLCHAIN_VERSION_REQUIRING_JSON_TARGET_SPEC_FLAG
                })
            {
                other_options.push("-Zjson-target-spec".to_owned());
            }
        }

        command.other_options(other_options.clone());

        // Pre-fetch basic metadata using `--no-deps`, which:
        // - avoids fetching registries like crates.io,
        // - skips dependency resolution and does not modify lockfiles,
        // - and thus doesn't require progress reporting or copying lockfiles.
        //
        // Useful as a fast fallback to extract info like `target-dir`.
        let cargo_command;
        let no_deps_result = if no_deps {
            command.no_deps();
            cargo_command = command.cargo_command();
            command.exec()
        } else {
            let mut no_deps_command = command.clone();
            no_deps_command.no_deps();
            cargo_command = no_deps_command.cargo_command();
            no_deps_command.exec()
        }
        .with_context(|| format!("Failed to run `{cargo_command:?}`"));

        Self {
            manifest_path: cargo_toml.clone(),
            command,
            lockfile_copy,
            kind: config.kind,
            no_deps,
            no_deps_result,
            other_options,
        }
    }

    /// Executes the metadata-fetching command.
    ///
    /// A successful result may still contain a metadata error if the full fetch failed,
    /// but the fallback `--no-deps` pre-fetch succeeded during command construction.
    pub(crate) fn exec(
        self,
        locked: bool,
        progress: &dyn Fn(String),
    ) -> anyhow::Result<(cargo_metadata::Metadata, Option<anyhow::Error>)> {
        let Self {
            mut command,
            manifest_path: _,
            lockfile_copy,
            kind: _,
            no_deps,
            no_deps_result,
            mut other_options,
        } = self;

        if no_deps {
            return no_deps_result.map(|m| (m, None));
        }

        let mut using_lockfile_copy = false;
        if let Some(lockfile_copy) = &lockfile_copy {
            match lockfile_copy.usage {
                LockfileUsage::WithFlag => {
                    other_options.push("--lockfile-path".to_owned());
                    other_options.push(lockfile_copy.path.to_string());
                }
                LockfileUsage::WithEnvVarUnstable => {
                    other_options.push("-Zlockfile-path".to_owned());
                    command.env("CARGO_RESOLVER_LOCKFILE_PATH", lockfile_copy.path.as_os_str());
                }
                LockfileUsage::WithEnvVar => {
                    command.env("CARGO_RESOLVER_LOCKFILE_PATH", lockfile_copy.path.as_os_str());
                }
            }
            using_lockfile_copy = true;
        }
        if using_lockfile_copy || other_options.iter().any(|it| it.starts_with("-Z")) {
            command.env("__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS", "nightly");
            other_options.push("-Zunstable-options".to_owned());
        }
        // No need to lock it if we copied the lockfile, we won't modify the original after all/
        // This way cargo cannot error out on us if the lockfile requires updating.
        if !using_lockfile_copy && locked {
            other_options.push("--locked".to_owned());
        }
        command.other_options(other_options);

        progress("cargo metadata: started".to_owned());

        let res = (|| -> anyhow::Result<(_, _)> {
            let mut errored = false;
            tracing::debug!("Running `{:?}`", command.cargo_command());
            let output =
                spawn_with_streaming_output(command.cargo_command(), &mut |_| (), &mut |line| {
                    errored = errored || line.starts_with("error") || line.starts_with("warning");
                    if errored {
                        progress("cargo metadata: ?".to_owned());
                        return;
                    }
                    progress(format!("cargo metadata: {line}"));
                })?;
            if !output.status.success() {
                progress(format!("cargo metadata: failed {}", output.status));
                let error = cargo_metadata::Error::CargoMetadata {
                    stderr: String::from_utf8(output.stderr)?,
                }
                .into();
                if !no_deps {
                    // If we failed to fetch metadata with deps, return pre-fetched result without them.
                    // This makes r-a still work partially when offline.
                    if let Ok(metadata) = no_deps_result {
                        tracing::warn!(
                            ?error,
                            "`cargo metadata` failed and returning succeeded result with `--no-deps`"
                        );
                        return Ok((metadata, Some(error)));
                    }
                }
                return Err(error);
            }
            let stdout = from_utf8(&output.stdout)?
                .lines()
                .find(|line| line.starts_with('{'))
                .ok_or(cargo_metadata::Error::NoJson)?;
            Ok((cargo_metadata::MetadataCommand::parse(stdout)?, None))
        })()
        .with_context(|| format!("Failed to run `{:?}`", command.cargo_command()));
        progress("cargo metadata: finished".to_owned());
        res
    }
}

impl PackageData {
    /// Whether the package's active features turn on its optional dependency `key`: its
    /// implicit feature of that name, or a feature that names `dep:key` or `key/feature`
    /// (not the weak `key?/feature`, which only adds to a dependency something else turns on).
    /// The active features are all of them, those the ones written imply too.
    pub fn activates(&self, key: &str) -> bool {
        let turns_on = |value: &String| {
            value.strip_prefix("dep:") == Some(key)
                || value.strip_prefix(key).is_some_and(|rest| rest.starts_with('/'))
        };
        self.active_features.iter().any(|feature| {
            *feature == key || self.features.get(feature).is_some_and(|it| it.iter().any(turns_on))
        })
    }

    /// `rust-version` as the manifest has it, which is what cargo hands to `env!`: cargo's
    /// metadata, and so `rust_version`, has `1.70` as `1.70.0`.
    pub fn rust_version_as_written(&self) -> Option<String> {
        let parsed = self.rust_version.as_ref()?.to_string();
        let written = |manifest: &Utf8Path, tables: &[&str]| {
            let manifest: toml::Table = std::fs::read_to_string(manifest).ok()?.parse().ok()?;
            let table = tables.iter().try_fold(&manifest, |table, key| table.get(*key)?.as_table());
            table?.get("rust-version")?.as_str().map(str::to_owned)
        };
        let manifest: &Utf8Path = self.manifest.as_ref();
        let of_workspace = || {
            manifest
                .ancestors()
                .skip(2)
                .find_map(|dir| written(&dir.join("Cargo.toml"), &["workspace", "package"]))
        };
        let written = written(manifest, &["package"]).or_else(of_workspace);
        // Only what parses to the version cargo reports is the manifest's.
        let same = |it: &String| match it.matches('.').count() {
            0 => format!("{it}.0.0") == parsed,
            1 => format!("{it}.0") == parsed,
            _ => *it == parsed,
        };
        Some(written.filter(same).unwrap_or(parsed))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn const_macros_are_a_file_of_the_package_and_an_experiment_of_the_root() {
        let dir = std::env::temp_dir().join(format!("blaze-const-macros-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/macros.rs"), "").unwrap();
        let manifest = Utf8PathBuf::from_path_buf(dir.join("Cargo.toml")).unwrap();
        let root = ManifestPath::try_from(AbsPathBuf::assert(manifest.clone())).unwrap();
        let of = |metadata: serde_json::Value, on: bool| {
            const_macros_of(&metadata, &manifest, &root, on)
        };

        assert!(const_macros_on(&json!({ "blaze": { "experimental": ["const-macros"] } })));
        assert!(!const_macros_on(&json!({ "blaze": { "experimental": ["sandboxed-tests"] } })));
        assert!(!const_macros_on(&serde_json::Value::Null));

        let declared = json!({ "blaze": { "const-macros": "src/macros.rs" } });
        let file = of(declared.clone(), true).unwrap().unwrap();
        assert_eq!(file, AbsPathBuf::assert(manifest.parent().unwrap().join("src/macros.rs")));
        let off = of(declared, false).unwrap().unwrap_err();
        assert!(
            off.contains("`experimental = [\"const-macros\"]` under [workspace.metadata.blaze]")
        );
        let missing = of(json!({ "blaze": { "const-macros": "src/none.rs" } }), true);
        assert!(missing.unwrap().unwrap_err().contains("which does not exist"));
        let wrong = of(json!({ "blaze": { "const-macros": true } }), true);
        assert!(wrong.unwrap().unwrap_err().contains("is a path, as a string"));
        assert!(of(json!({ "blaze": {} }), true).is_none());
        assert!(of(serde_json::Value::Null, true).is_none());
        _ = std::fs::remove_dir_all(&dir);
    }
}
