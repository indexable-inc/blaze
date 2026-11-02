//! One `Cargo.toml` read as the pinned nightly's cargo reads it (`src/workspace/parser/mod.rs`
//! there): parsed into cargo's own schema, `workspace = true` fields and dependencies
//! inherited, targets discovered, dependencies and features checked.

use std::cell::{OnceCell, RefCell};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use blaze_hash::HashMap;
use cargo_platform::Platform;
use cargo_util_schemas::core::{PackageIdSpec, PartialVersion};
use cargo_util_schemas::manifest::{
    self, Hints, InheritableField, InheritablePackage, PackageName, PathBaseName, RustVersion,
    StringOrBool, TomlDependency, TomlDetailedDependency, TomlInheritedDependency, TomlLints,
    TomlManifest, TomlPackage, TomlPackageBuild, TomlWorkspace, VecStringOrBool,
};
use url::Url;

use crate::config::Config;
use crate::config::path::ConfigRelativePath;
use crate::dependency::{DepKind, Dependency, FeatureMap, build_summary_features};
use crate::diagnostic::ManifestDiagnostic;
use crate::edit_distance::closest_msg;
use crate::features::{Edition, Feature, Features, ResolveBehavior};
use crate::files::Files;
use crate::paths::normalize_path;
use crate::profiles::validate_profiles;
use crate::source::{self, CRATES_IO_INDEX, CRATES_IO_REGISTRY, GitReference, SourceId, into_url};
use crate::targets::{self, Target};
use crate::workspace::find_workspace_root;

/// What reading manifests shares: the configuration, and the workspace roots read so far, which
/// cargo consults before walking up the file system (its `GlobalContext::ws_roots`).
/// Every file is read through the configuration's [`Files`].
pub struct Context<F: Files> {
    pub config: Config<F>,
    pub(crate) ws_roots: RefCell<HashMap<PathBuf, WorkspaceRootConfig>>,
}

impl<F: Files> Context<F> {
    pub fn new(config: Config<F>) -> Context<F> {
        Context { config, ws_roots: RefCell::new(HashMap::default()) }
    }

    pub fn files(&self) -> &F {
        self.config.files()
    }
}

/// A package's identity: name, version and source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageId {
    pub name: String,
    pub version: semver::Version,
    pub source_id: SourceId,
}

/// cargo's `PackageId` display: `name vX`, with the source unless it is crates.io.
impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} v{}", self.name, self.version)?;
        if !self.source_id.is_crates_io() {
            write!(f, " ({})", self.source_id)?;
        }
        Ok(())
    }
}

impl PackageId {
    /// The spec `cargo metadata` names the package by, e.g. `path+file:///w/a#0.1.0`.
    pub fn to_spec(&self) -> PackageIdSpec {
        PackageIdSpec::new(self.name.clone())
            .with_version(PartialVersion::from(self.version.clone()))
            .with_url(self.source_id.url().clone())
            .with_kind(source::spec_kind(self.source_id.kind()))
    }
}

#[derive(Clone, Debug)]
pub enum WorkspaceConfig {
    /// `[workspace]`.
    Root(Box<WorkspaceRootConfig>),
    /// No `[workspace]`; `root` is `package.workspace`.
    Member { root: Option<String> },
}

impl WorkspaceConfig {
    /// The manifest of the workspace root this configuration names, when it names one.
    pub(crate) fn get_ws_root(&self, self_path: &Path, look_from: &Path) -> Option<PathBuf> {
        match self {
            WorkspaceConfig::Root(root) => {
                (!root.is_excluded(look_from)).then(|| self_path.to_owned())
            }
            WorkspaceConfig::Member { root: Some(path_to_root) } => {
                Some(read_root_pointer(self_path, path_to_root))
            }
            WorkspaceConfig::Member { root: None } => None,
        }
    }
}

pub(crate) fn read_root_pointer(member_manifest: &Path, root_link: &str) -> PathBuf {
    normalize_path(&member_manifest.parent().unwrap().join(root_link).join("Cargo.toml"))
}

#[derive(Clone, Debug)]
pub struct WorkspaceRootConfig {
    pub root_dir: PathBuf,
    pub members: Option<Vec<String>>,
    pub default_members: Option<Vec<String>>,
    pub exclude: Vec<String>,
    pub inheritable: InheritableFields,
    pub custom_metadata: Option<toml::Value>,
}

impl WorkspaceRootConfig {
    fn new(toml: &TomlWorkspace, manifest_file: &Path) -> WorkspaceRootConfig {
        let root_dir = manifest_file.parent().unwrap().to_owned();
        WorkspaceRootConfig {
            members: toml.members.clone(),
            default_members: toml.default_members.clone(),
            exclude: toml.exclude.clone().unwrap_or_default(),
            inheritable: InheritableFields {
                package: toml.package.clone(),
                dependencies: toml.dependencies.clone(),
                lints: toml.lints.clone(),
                ws_root: root_dir.clone(),
            },
            custom_metadata: toml.metadata.clone(),
            root_dir,
        }
    }

    /// Whether `exclude` leaves the path out, unless `members` names it.
    pub(crate) fn is_excluded(&self, manifest_path: &Path) -> bool {
        let excluded =
            self.exclude.iter().any(|ex| manifest_path.starts_with(self.root_dir.join(ex)));
        let explicit_member = match &self.members {
            Some(members) => {
                members.iter().any(|mem| manifest_path.starts_with(self.root_dir.join(mem)))
            }
            None => false,
        };
        !explicit_member && excluded
    }

    pub(crate) fn has_members_list(&self) -> bool {
        self.members.is_some()
    }

    /// The directories `globs` name, each with the glob it came from when it was a pattern.
    pub(crate) fn members_paths<'g, F: Files>(
        &self,
        files: &F,
        globs: &'g [String],
    ) -> anyhow::Result<Vec<(PathBuf, Option<&'g str>)>> {
        let mut expanded_list = Vec::new();
        for glob in globs {
            let pathbuf = self.root_dir.join(glob);
            let expanded_paths = expand_member_path(files, &pathbuf)?;
            if expanded_paths.is_empty() {
                expanded_list.push((pathbuf, None));
            } else {
                let used_glob_pattern = expanded_paths.len() > 1 || expanded_paths[0] != pathbuf;
                let glob = used_glob_pattern.then_some(glob.as_str());
                for expanded_path in expanded_paths {
                    if files.is_dir(&expanded_path) {
                        expanded_list.push((expanded_path, glob));
                    }
                }
            }
        }
        Ok(expanded_list)
    }
}

fn expand_member_path<F: Files>(files: &F, path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let Some(path) = path.to_str() else {
        return Ok(Vec::new());
    };
    let res = crate::files::glob(files, path)
        .with_context(|| format!("could not parse pattern `{path}`"))?;
    res.into_iter()
        .map(|p| p.with_context(|| format!("unable to match path to pattern `{path}`")))
        .collect()
}

/// What `[workspace]` offers its members to inherit.
#[derive(Clone, Debug, Default)]
pub struct InheritableFields {
    package: Option<InheritablePackage>,
    dependencies: Option<BTreeMap<PackageName, TomlDependency>>,
    lints: Option<TomlLints>,
    ws_root: PathBuf,
}

macro_rules! package_field_getter {
    ( $(($key:literal, $field:ident -> $ret:ty),)* ) => (
        $(
            fn $field(&self) -> anyhow::Result<$ret> {
                let Some(val) = self.package.as_ref().and_then(|p| p.$field.as_ref()) else {
                    bail!("`workspace.package.{}` was not defined", $key);
                };
                Ok(val.clone())
            }
        )*
    )
}

impl InheritableFields {
    package_field_getter! {
        ("authors",       authors       -> Vec<String>),
        ("categories",    categories    -> Vec<String>),
        ("description",   description   -> String),
        ("documentation", documentation -> String),
        ("edition",       edition       -> String),
        ("exclude",       exclude       -> Vec<String>),
        ("homepage",      homepage      -> String),
        ("include",       include       -> Vec<String>),
        ("keywords",      keywords      -> Vec<String>),
        ("license",       license       -> String),
        ("publish",       publish       -> VecStringOrBool),
        ("repository",    repository    -> String),
        ("rust-version",  rust_version  -> RustVersion),
        ("version",       version       -> semver::Version),
    }

    fn get_dependency(&self, name: &str, package_root: &Path) -> anyhow::Result<TomlDependency> {
        let Some(deps) = &self.dependencies else {
            bail!("`workspace.dependencies` was not defined");
        };
        let Some(dep) = deps.get(name) else {
            bail!("`dependency.{name}` was not found in `workspace.dependencies`");
        };
        let mut dep = dep.clone();
        if let TomlDependency::Detailed(detailed) = &mut dep
            && detailed.base.is_none()
            && let Some(rel_path) = &detailed.path
        {
            detailed.path =
                Some(resolve_relative_path(name, &self.ws_root, package_root, rel_path)?);
        }
        Ok(dep)
    }

    fn lints(&self) -> anyhow::Result<TomlLints> {
        let Some(val) = &self.lints else {
            bail!("`workspace.lints` was not defined");
        };
        Ok(val.clone())
    }

    fn license_file(&self, package_root: &Path) -> anyhow::Result<String> {
        let Some(license_file) = self.package.as_ref().and_then(|p| p.license_file.as_ref()) else {
            bail!("`workspace.package.license-file` was not defined");
        };
        resolve_relative_path("license-file", &self.ws_root, package_root, license_file)
    }

    fn readme<F: Files>(&self, files: &F, package_root: &Path) -> anyhow::Result<StringOrBool> {
        let Some(readme) = normalize_package_readme(
            files,
            &self.ws_root,
            self.package.as_ref().and_then(|p| p.readme.as_ref()),
        ) else {
            bail!("`workspace.package.readme` was not defined");
        };
        resolve_relative_path("readme", &self.ws_root, package_root, &readme)
            .map(StringOrBool::String)
    }

    pub fn ws_root(&self) -> &Path {
        &self.ws_root
    }
}

/// `rel_path` from `old_root`, made relative to `new_root`.
pub(crate) fn resolve_relative_path(
    label: &str,
    old_root: &Path,
    new_root: &Path,
    rel_path: &str,
) -> anyhow::Result<String> {
    let joined_path = normalize_path(&old_root.join(rel_path));
    match pathdiff::diff_paths(joined_path, new_root) {
        None => Err(anyhow!(
            "`{label}` was defined in {} but could not be resolved with {}",
            old_root.display(),
            new_root.display()
        )),
        Some(path) => Ok(path
            .to_str()
            .ok_or_else(|| anyhow!("`{label}` resolved to non-UTF value (`{}`)", path.display()))?
            .to_owned()),
    }
}

/// Fields of `[package]` that `cargo metadata` prints.
#[derive(Clone, Debug, Default)]
pub struct ManifestMetadata {
    pub authors: Vec<String>,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
    pub license: Option<String>,
    pub license_file: Option<String>,
    pub description: Option<String>,
    pub readme: Option<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub documentation: Option<String>,
    pub links: Option<String>,
    pub rust_version: Option<RustVersion>,
}

/// A manifest with a `[package]`.
#[derive(Debug)]
pub struct Manifest {
    pub package_id: PackageId,
    pub dependencies: Vec<Dependency>,
    pub features: FeatureMap,
    pub targets: Vec<Target>,
    pub metadata: ManifestMetadata,
    pub custom_metadata: Option<toml::Value>,
    pub publish: Option<Vec<String>>,
    pub replace: Vec<(PackageIdSpec, Dependency)>,
    pub patch: HashMap<Url, Vec<Dependency>>,
    pub workspace: WorkspaceConfig,
    pub unstable_features: Features,
    pub edition: Edition,
    pub default_run: Option<String>,
    pub metabuild: Option<Vec<String>>,
    pub resolve_behavior: Option<ResolveBehavior>,
    pub hints: Option<Hints>,
    pub normalized_toml: TomlManifest,
    pub warnings: Vec<String>,
}

/// A manifest with only a `[workspace]`.
#[derive(Debug)]
pub struct VirtualManifest {
    pub replace: Vec<(PackageIdSpec, Dependency)>,
    pub patch: HashMap<Url, Vec<Dependency>>,
    pub workspace: WorkspaceConfig,
    pub unstable_features: Features,
    pub resolve_behavior: Option<ResolveBehavior>,
    pub normalized_toml: TomlManifest,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub enum EitherManifest {
    Real(Box<Manifest>),
    Virtual(Box<VirtualManifest>),
}

impl EitherManifest {
    pub fn workspace_config(&self) -> &WorkspaceConfig {
        match self {
            EitherManifest::Real(m) => &m.workspace,
            EitherManifest::Virtual(vm) => &vm.workspace,
        }
    }
}

/// Reads the manifest at `path`, a package or a virtual workspace.
/// `bytes`, the manifest at `path`, parsed: not UTF-8, or TOML that is no manifest, is cargo's
/// error naming `path` (with the TOML error's snippet).
pub fn parse_manifest(path: &Path, bytes: &[u8]) -> anyhow::Result<TomlManifest> {
    let contents = std::str::from_utf8(bytes)
        .map_err(|_| anyhow!("path at `{}` was not valid utf-8", path.display()))?;
    toml::from_str(contents).map_err(|e| ManifestDiagnostic::from_toml(e, contents, path))
}

pub fn read_manifest<F: Files>(
    path: &Path,
    source_id: SourceId,
    ctx: &Context<F>,
) -> anyhow::Result<EitherManifest> {
    let ext = path.extension();
    if ext.is_none() || ext == Some(std::ffi::OsStr::new("rs")) {
        bail!("parsing `{}` requires `-Zscript`", path.display());
    }
    let original_toml = ctx.files().manifest(path)?;

    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    (|| {
        let empty = Vec::new();
        let cargo_features = original_toml.cargo_features.as_ref().unwrap_or(&empty);
        let features = Features::new(cargo_features, &mut warnings, source_id.is_path())?;
        let workspace_config = to_workspace_config(&original_toml, path, &mut warnings)?;
        if let WorkspaceConfig::Root(ws_root_config) = &workspace_config {
            let package_root = path.parent().unwrap();
            ctx.ws_roots.borrow_mut().insert(package_root.to_owned(), (**ws_root_config).clone());
        }
        let normalized_toml = normalize_toml(
            &original_toml,
            &features,
            &workspace_config,
            path,
            ctx,
            &mut warnings,
            &mut errors,
        )?;
        if normalized_toml.package().is_some() {
            to_real_manifest(
                original_toml,
                normalized_toml,
                features,
                workspace_config,
                source_id,
                path,
                ctx,
                &mut warnings,
            )
            .map(|mut m| {
                m.warnings = std::mem::take(&mut warnings);
                EitherManifest::Real(Box::new(m))
            })
        } else if normalized_toml.workspace.is_some() {
            to_virtual_manifest(normalized_toml, features, workspace_config, source_id, path, ctx)
                .map(|mut m| {
                    warnings.append(&mut m.warnings);
                    m.warnings = std::mem::take(&mut warnings);
                    EitherManifest::Virtual(Box::new(m))
                })
        } else {
            bail!("manifest is missing either a `[package]` or a `[workspace]`")
        }
    })()
    .with_context(|| format!("failed to parse manifest at `{}`", path.display()))
}

fn to_workspace_config(
    original_toml: &TomlManifest,
    manifest_file: &Path,
    warnings: &mut Vec<String>,
) -> anyhow::Result<WorkspaceConfig> {
    match (
        original_toml.workspace.as_ref(),
        original_toml.package().and_then(|p| p.workspace.as_ref()),
    ) {
        (Some(toml_config), None) => {
            verify_lints(toml_config.lints.as_ref(), warnings)?;
            if let Some(ws_deps) = &toml_config.dependencies {
                for (name, dep) in ws_deps {
                    if dep.is_optional() {
                        bail!("{name} is optional, but workspace dependencies cannot be optional");
                    }
                    if dep.is_public() {
                        bail!("{name} is public, but workspace dependencies cannot be public");
                    }
                }
            }
            Ok(WorkspaceConfig::Root(Box::new(WorkspaceRootConfig::new(
                toml_config,
                manifest_file,
            ))))
        }
        (None, root) => Ok(WorkspaceConfig::Member { root: root.cloned() }),
        (Some(..), Some(..)) => bail!(
            "cannot configure both `package.workspace` and \
             `[workspace]`, only one can be specified"
        ),
    }
}

/// What `[workspace]` gives a member: read from its workspace root the first time a field
/// inherits, so a manifest that inherits nothing never reads the root.
struct Inherit<'a, F: Files> {
    fields: OnceCell<InheritableFields>,
    ctx: &'a Context<F>,
    manifest_file: &'a Path,
    workspace_config: &'a WorkspaceConfig,
}

impl<F: Files> Inherit<'_, F> {
    fn fields(&self) -> anyhow::Result<&InheritableFields> {
        if let Some(fields) = self.fields.get() {
            return Ok(fields);
        }
        let fields = load_inheritable_fields(self.ctx, self.manifest_file, self.workspace_config)?;
        Ok(self.fields.get_or_init(|| fields))
    }

    fn ws_root(&self) -> anyhow::Result<&Path> {
        self.fields().map(InheritableFields::ws_root)
    }
}

fn normalize_toml<F: Files>(
    original_toml: &TomlManifest,
    features: &Features,
    workspace_config: &WorkspaceConfig,
    manifest_file: &Path,
    ctx: &Context<F>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> anyhow::Result<TomlManifest> {
    let package_root = manifest_file.parent().unwrap();
    let files = ctx.files();

    let inherit = Inherit { fields: OnceCell::new(), ctx, manifest_file, workspace_config };

    let mut normalized_toml = TomlManifest {
        cargo_features: original_toml.cargo_features.clone(),
        workspace: original_toml.workspace.clone(),
        profile: original_toml.profile.clone(),
        patch: normalize_patch(&ctx.config, original_toml.patch.as_ref(), &inherit, features)?,
        replace: original_toml.replace.clone(),
        ..Default::default()
    };

    if let Some(original_package) = original_toml.package() {
        let normalized_package =
            normalize_package_toml(original_package, manifest_file, &inherit, features)?;
        let package_name =
            normalized_package.normalized_name().expect("previously normalized").clone();
        let edition = normalized_package
            .normalized_edition()
            .expect("previously normalized")
            .map_or(Edition::default(), |e| e.parse().unwrap_or_default());
        normalized_toml.package = Some(normalized_package);
        normalized_toml.features = original_toml.features.clone();

        normalized_toml.lib = targets::normalize_lib(
            files,
            original_toml.lib.as_ref(),
            package_root,
            &package_name,
            edition,
            original_package.autolib,
            warnings,
        )?;
        normalized_toml.bin = Some(targets::normalize_bins(
            files,
            original_toml.bin.as_ref(),
            package_root,
            &package_name,
            edition,
            original_package.autobins,
            warnings,
            errors,
            normalized_toml.lib.is_some(),
        )?);
        normalized_toml.example = Some(targets::normalize_examples(
            files,
            original_toml.example.as_ref(),
            package_root,
            edition,
            original_package.autoexamples,
            warnings,
            errors,
        )?);
        normalized_toml.test = Some(targets::normalize_tests(
            files,
            original_toml.test.as_ref(),
            package_root,
            edition,
            original_package.autotests,
            warnings,
            errors,
        )?);
        normalized_toml.bench = Some(targets::normalize_benches(
            files,
            original_toml.bench.as_ref(),
            package_root,
            edition,
            original_package.autobenches,
            warnings,
            errors,
        )?);

        normalized_toml.dependencies = normalize_dependencies(
            edition,
            features,
            &ctx.config,
            original_toml.dependencies.as_ref(),
            DepKind::Normal,
            &inherit,
            package_root,
            warnings,
        )?;
        deprecated_underscore(
            &original_toml.dev_dependencies2,
            &original_toml.dev_dependencies,
            "dev-dependencies",
            &package_name,
            "package",
            edition,
            warnings,
        )?;
        normalized_toml.dev_dependencies = normalize_dependencies(
            edition,
            features,
            &ctx.config,
            original_toml.dev_dependencies(),
            DepKind::Development,
            &inherit,
            package_root,
            warnings,
        )?;
        deprecated_underscore(
            &original_toml.build_dependencies2,
            &original_toml.build_dependencies,
            "build-dependencies",
            &package_name,
            "package",
            edition,
            warnings,
        )?;
        normalized_toml.build_dependencies = normalize_dependencies(
            edition,
            features,
            &ctx.config,
            original_toml.build_dependencies(),
            DepKind::Build,
            &inherit,
            package_root,
            warnings,
        )?;
        let mut normalized_target = BTreeMap::new();
        for (name, platform) in original_toml.target.iter().flatten() {
            let dependencies = normalize_dependencies(
                edition,
                features,
                &ctx.config,
                platform.dependencies.as_ref(),
                DepKind::Normal,
                &inherit,
                package_root,
                warnings,
            )?;
            deprecated_underscore(
                &platform.dev_dependencies2,
                &platform.dev_dependencies,
                "dev-dependencies",
                name,
                "platform target",
                edition,
                warnings,
            )?;
            let dev_dependencies = normalize_dependencies(
                edition,
                features,
                &ctx.config,
                platform.dev_dependencies(),
                DepKind::Development,
                &inherit,
                package_root,
                warnings,
            )?;
            deprecated_underscore(
                &platform.build_dependencies2,
                &platform.build_dependencies,
                "build-dependencies",
                name,
                "platform target",
                edition,
                warnings,
            )?;
            let build_dependencies = normalize_dependencies(
                edition,
                features,
                &ctx.config,
                platform.build_dependencies(),
                DepKind::Build,
                &inherit,
                package_root,
                warnings,
            )?;
            normalized_target.insert(
                name.clone(),
                manifest::TomlPlatform {
                    dependencies,
                    build_dependencies,
                    build_dependencies2: None,
                    dev_dependencies,
                    dev_dependencies2: None,
                },
            );
        }
        normalized_toml.target = (!normalized_target.is_empty()).then_some(normalized_target);

        let normalized_lints = original_toml
            .lints
            .clone()
            .map(|value| lints_inherit_with(value, || inherit.fields()?.lints()))
            .transpose()?;
        normalized_toml.lints =
            normalized_lints.map(|lints| manifest::InheritableLints { workspace: false, lints });
        normalized_toml.hints = original_toml.hints.clone();
        normalized_toml.badges = original_toml.badges.clone();
    } else if let Some(field) = original_toml.requires_package().next() {
        let suggestion = if field == "lints" {
            "\nhelp: a similar field exists: `[workspace.lints]`"
        } else {
            ""
        };
        bail!(
            "this virtual manifest specifies a `{field}` section, which is not allowed{suggestion}"
        );
    }

    Ok(normalized_toml)
}

fn normalize_patch<F: Files>(
    config: &Config<F>,
    original_patch: Option<&BTreeMap<String, BTreeMap<PackageName, TomlDependency>>>,
    inherit: &Inherit<'_, F>,
    features: &Features,
) -> anyhow::Result<Option<BTreeMap<String, BTreeMap<PackageName, TomlDependency>>>> {
    let Some(patch) = original_patch else {
        return Ok(None);
    };
    let mut normalized_patch = BTreeMap::new();
    for (name, packages) in patch {
        let mut normalized_packages = BTreeMap::new();
        for (pkg, dep) in packages {
            let dep = if let TomlDependency::Detailed(dep) = dep {
                let mut dep = dep.clone();
                normalize_path_dependency(config, &mut dep, inherit, features)
                    .with_context(|| {
                        format!("resolving path for patch of ({pkg}) for source ({name})")
                    })?;
                TomlDependency::Detailed(dep)
            } else {
                dep.clone()
            };
            normalized_packages.insert(pkg.clone(), dep);
        }
        normalized_patch.insert(name.clone(), normalized_packages);
    }
    Ok(Some(normalized_patch))
}

fn field_inherit_with<T>(
    field: InheritableField<T>,
    label: &str,
    get_ws_inheritable: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    match field {
        InheritableField::Value(value) => Ok(value),
        InheritableField::Inherit(_) => get_ws_inheritable().with_context(|| {
            format!(
                "error inheriting `{label}` from workspace root manifest's `workspace.package.{label}`",
            )
        }),
    }
}

fn normalize_package_toml<F: Files>(
    original_package: &TomlPackage,
    manifest_file: &Path,
    inherit: &Inherit<'_, F>,
    features: &Features,
) -> anyhow::Result<Box<TomlPackage>> {
    let package_root = manifest_file.parent().unwrap();
    let files = inherit.ctx.files();
    macro_rules! inherit_field {
        ($field:ident, $label:literal) => {
            original_package
                .$field
                .clone()
                .map(|value| field_inherit_with(value, $label, || inherit.fields()?.$field()))
                .transpose()?
                .map(InheritableField::Value)
        };
    }

    let edition = inherit_field!(edition, "edition");
    let rust_version = inherit_field!(rust_version, "rust-version");
    let name = Some(
        original_package
            .name
            .clone()
            .ok_or_else(|| anyhow::format_err!("missing field `package.name`"))?,
    );
    let version = inherit_field!(version, "version");
    let authors = inherit_field!(authors, "authors");
    if let Some(TomlPackageBuild::MultipleScript(_)) = original_package.build {
        features.require(Feature::multiple_build_scripts)?;
    }
    let build = targets::normalize_build(files, original_package.build.as_ref(), package_root)?;
    let exclude = inherit_field!(exclude, "exclude");
    let include = inherit_field!(include, "include");
    let publish = inherit_field!(publish, "publish");
    let description = inherit_field!(description, "description");
    let homepage = inherit_field!(homepage, "homepage");
    let documentation = inherit_field!(documentation, "documentation");
    let readme = normalize_package_readme(
        files,
        package_root,
        original_package
            .readme
            .clone()
            .map(|value| {
                field_inherit_with(value, "readme", || inherit.fields()?.readme(files, package_root))
            })
            .transpose()?
            .as_ref(),
    )
    .map(|s| InheritableField::Value(StringOrBool::String(s)))
    .or(Some(InheritableField::Value(StringOrBool::Bool(false))));
    let keywords = inherit_field!(keywords, "keywords");
    let categories = inherit_field!(categories, "categories");
    let license = inherit_field!(license, "license");
    let license_file = original_package
        .license_file
        .clone()
        .map(|value| {
            field_inherit_with(value, "license-file", || {
                inherit.fields()?.license_file(package_root)
            })
        })
        .transpose()?
        .map(InheritableField::Value);
    let repository = inherit_field!(repository, "repository");

    Ok(Box::new(TomlPackage {
        edition,
        rust_version,
        name,
        version,
        authors,
        build,
        metabuild: original_package.metabuild.clone(),
        default_target: original_package.default_target.clone(),
        forced_target: original_package.forced_target.clone(),
        links: original_package.links.clone(),
        exclude,
        include,
        publish,
        workspace: original_package.workspace.clone(),
        im_a_teapot: original_package.im_a_teapot,
        autolib: Some(false),
        autobins: Some(false),
        autoexamples: Some(false),
        autotests: Some(false),
        autobenches: Some(false),
        default_run: original_package.default_run.clone(),
        description,
        homepage,
        documentation,
        readme,
        keywords,
        categories,
        license,
        license_file,
        repository,
        resolver: original_package.resolver.clone(),
        metadata: original_package.metadata.clone(),
        _invalid_cargo_features: Default::default(),
    }))
}

const DEFAULT_README_FILES: [&str; 3] = ["README.md", "README.txt", "README"];

fn normalize_package_readme<F: Files>(
    files: &F,
    package_root: &Path,
    readme: Option<&StringOrBool>,
) -> Option<String> {
    match readme {
        None => DEFAULT_README_FILES
            .iter()
            .find(|f| files.is_file(&package_root.join(f)))
            .map(|f| (*f).to_owned()),
        Some(StringOrBool::Bool(false)) => None,
        Some(StringOrBool::Bool(true)) => Some("README.md".to_owned()),
        Some(StringOrBool::String(v)) => Some(v.clone()),
    }
}

fn normalize_dependencies<F: Files>(
    edition: Edition,
    features: &Features,
    config: &Config<F>,
    orig_deps: Option<&BTreeMap<PackageName, manifest::InheritableDependency>>,
    kind: DepKind,
    inherit: &Inherit<'_, F>,
    package_root: &Path,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Option<BTreeMap<PackageName, manifest::InheritableDependency>>> {
    let Some(dependencies) = orig_deps else {
        return Ok(None);
    };
    let mut deps = BTreeMap::new();
    for (name_in_toml, v) in dependencies {
        let mut resolved = match v.clone() {
            manifest::InheritableDependency::Value(value) => value,
            manifest::InheritableDependency::Inherit(w) => {
                inner_dependency_inherit_with(w, name_in_toml, inherit, package_root, edition, warnings)
                    .with_context(|| {
                        format!(
                            "error inheriting `{name_in_toml}` from workspace root manifest's `workspace.dependencies.{name_in_toml}`",
                        )
                    })?
            }
        };
        if let TomlDependency::Detailed(ref mut d) = resolved {
            deprecated_underscore(
                &d.default_features2,
                &d.default_features,
                "default-features",
                name_in_toml,
                "dependency",
                edition,
                warnings,
            )?;
            if d.public.is_some() {
                match kind {
                    DepKind::Normal => {
                        d.public = None;
                        warnings.push(format!(
                            "ignoring `public` on dependency {name_in_toml}, pass `-Zpublic-dependency` to enable support for it"
                        ));
                    }
                    DepKind::Development | DepKind::Build => {
                        let kind_name = kind.kind_table();
                        warnings.push(format!(
                            "'public' specifier can only be used on regular dependencies, not {kind_name}",
                        ));
                        d.public = None;
                    }
                }
            }
            normalize_path_dependency(config, d, inherit, features)
                .with_context(|| format!("resolving path dependency {name_in_toml}"))?;
        }
        deps.insert(name_in_toml.clone(), manifest::InheritableDependency::Value(resolved));
    }
    Ok(Some(deps))
}

/// A path dependency's `base` (the unstable `path-bases`) joined to its `path`, as nightly
/// cargo's `normalize_path_dependency` does.
fn normalize_path_dependency<F: Files>(
    config: &Config<F>,
    detailed_dep: &mut TomlDetailedDependency,
    inherit: &Inherit<'_, F>,
    features: &Features,
) -> anyhow::Result<()> {
    if let Some(base) = detailed_dep.base.take() {
        if let Some(path) = detailed_dep.path.as_mut() {
            let new_path = lookup_path_base(&base, config, inherit, features)?.join(&path);
            *path = new_path.to_str().unwrap().to_owned();
        } else {
            bail!("`base` can only be used with path dependencies");
        }
    }
    Ok(())
}

/// Where the path base `base` points: the configuration's `[path-bases]` entry, else the
/// built-in `workspace`, cargo's `lookup_path_base`.
fn lookup_path_base<F: Files>(
    base: &PathBaseName,
    config: &Config<F>,
    inherit: &Inherit<'_, F>,
    features: &Features,
) -> anyhow::Result<PathBuf> {
    features.require(Feature::path_bases)?;
    if let Some(path) =
        config.get::<Option<ConfigRelativePath>>(&format!("path-bases.{}", base.as_str()))?
    {
        return Ok(path.resolve_path(config.cwd()));
    }
    match base.as_str() {
        "workspace" => Ok(inherit.ws_root()?.to_path_buf()),
        _ => bail!(
            "path base `{base}` is undefined. \
             You must add an entry for `{base}` in the Cargo configuration [path-bases] table."
        ),
    }
}

fn load_inheritable_fields<F: Files>(
    ctx: &Context<F>,
    normalized_path: &Path,
    workspace_config: &WorkspaceConfig,
) -> anyhow::Result<InheritableFields> {
    match workspace_config {
        WorkspaceConfig::Root(root) => Ok(root.inheritable.clone()),
        WorkspaceConfig::Member { root: Some(path_to_root) } => {
            let path = normalized_path.parent().unwrap().join(path_to_root).join("Cargo.toml");
            inheritable_from_path(ctx, normalize_path(&path))
        }
        WorkspaceConfig::Member { root: None } => {
            match find_workspace_root(normalized_path, ctx)? {
                Some(path_to_root) => inheritable_from_path(ctx, path_to_root),
                None => Err(anyhow!("failed to find a workspace root")),
            }
        }
    }
}

fn inheritable_from_path<F: Files>(
    ctx: &Context<F>,
    workspace_path: PathBuf,
) -> anyhow::Result<InheritableFields> {
    let workspace_path_root = workspace_path.parent().unwrap();
    if let Some(ws_root) = ctx.ws_roots.borrow().get(workspace_path_root) {
        return Ok(ws_root.inheritable.clone());
    }
    let source_id = source::for_manifest_path(&workspace_path)?;
    let man = read_manifest(&workspace_path, source_id, ctx)?;
    match man.workspace_config() {
        WorkspaceConfig::Root(root) => {
            ctx.ws_roots.borrow_mut().insert(workspace_path, (**root).clone());
            Ok(root.inheritable.clone())
        }
        _ => bail!("root of a workspace inferred but wasn't a root: {}", workspace_path.display()),
    }
}

fn lints_inherit_with(
    lints: manifest::InheritableLints,
    get_ws_inheritable: impl FnOnce() -> anyhow::Result<TomlLints>,
) -> anyhow::Result<TomlLints> {
    if lints.workspace {
        if !lints.lints.is_empty() {
            bail!(
                "cannot override `workspace.lints` in `lints`, either remove the overrides or `lints.workspace = true` and manually specify the lints"
            );
        }
        get_ws_inheritable()
            .context("error inheriting `lints` from workspace root manifest's `workspace.lints`")
    } else {
        Ok(lints.lints)
    }
}

fn inner_dependency_inherit_with<F: Files>(
    pkg_dep: TomlInheritedDependency,
    name: &str,
    inherit: &Inherit<'_, F>,
    package_root: &Path,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> anyhow::Result<TomlDependency> {
    let ws_dep = inherit.fields()?.get_dependency(name, package_root)?;
    let mut merged_dep = match ws_dep {
        TomlDependency::Simple(ws_version) => {
            TomlDetailedDependency { version: Some(ws_version), ..Default::default() }
        }
        TomlDependency::Detailed(ws_dep) => ws_dep,
    };
    let default_features = pkg_dep.default_features.or(pkg_dep.default_features2);

    if edition >= Edition::Edition2024 {
        merged_dep.default_features = default_features.or(merged_dep.default_features);
    } else {
        match (default_features, merged_dep.default_features()) {
            (Some(true), Some(false)) => merged_dep.default_features = Some(true),
            (Some(false), Some(true)) => deprecated_ws_default_features(name, Some(true), warnings),
            (Some(false), None) => deprecated_ws_default_features(name, None, warnings),
            _ => {}
        }
    }
    merged_dep.features = match (merged_dep.features.clone(), pkg_dep.features.clone()) {
        (Some(dep_feat), Some(inherit_feat)) => {
            Some(dep_feat.into_iter().chain(inherit_feat).collect())
        }
        (Some(dep_fet), None) => Some(dep_fet),
        (None, Some(inherit_feat)) => Some(inherit_feat),
        (None, None) => None,
    };
    merged_dep.optional = pkg_dep.optional;
    merged_dep.public = pkg_dep.public;
    Ok(TomlDependency::Detailed(merged_dep))
}

fn deprecated_ws_default_features(
    label: &str,
    ws_def_feat: Option<bool>,
    warnings: &mut Vec<String>,
) {
    let ws_def_feat = match ws_def_feat {
        Some(true) => "true",
        Some(false) => "false",
        None => "not specified",
    };
    warnings.push(format!(
        "`default-features` is ignored for {label}, since `default-features` was \
         {ws_def_feat} for `workspace.dependencies.{label}`; \
         overriding workspace `default-features` to false requires Rust 1.99+ \
         and the 2024 edition"
    ));
}

/// The rule for a key spelled with `_` where cargo wants `-`: refused from the 2024 edition on.
pub(crate) fn deprecated_underscore<T>(
    old: &Option<T>,
    new: &Option<T>,
    new_path: &str,
    name: &str,
    kind: &str,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    let old_path = new_path.replace('-', "_");
    if old.is_some() && Edition::Edition2024 <= edition {
        bail!(
            "`{old_path}` is unsupported as of the 2024 edition; instead use `{new_path}`\n(in the `{name}` {kind})"
        );
    } else if old.is_some() && new.is_some() {
        warnings.push(format!(
            "`{old_path}` is redundant with `{new_path}`, preferring `{new_path}` in the `{name}` {kind}"
        ))
    } else if old.is_some() {
        warnings.push(format!(
            "`{old_path}` is deprecated in favor of `{new_path}` and will not work in the 2024 edition\n(in the `{name}` {kind})"
        ))
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn to_real_manifest<F: Files>(
    original_toml: TomlManifest,
    normalized_toml: TomlManifest,
    features: Features,
    workspace_config: WorkspaceConfig,
    source_id: SourceId,
    manifest_file: &Path,
    ctx: &Context<F>,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Manifest> {
    let package_root = manifest_file.parent().unwrap();
    if !ctx.files().is_dir(package_root) {
        bail!("package root '{}' is not a directory", package_root.display());
    }

    let normalized_package =
        normalized_toml.package().expect("previously verified to have a `[package]`");
    let package_name = normalized_package.normalized_name().expect("previously normalized");
    if package_name.contains(':') {
        features.require(Feature::open_namespaces)?;
    }
    let rust_version =
        normalized_package.normalized_rust_version().expect("previously normalized").cloned();

    let edition = if let Some(edition) =
        normalized_package.normalized_edition().expect("previously normalized")
    {
        let edition: Edition = edition.parse().context("failed to parse the `edition` key")?;
        if let Some(pkg_msrv) = &rust_version
            && let Some(edition_msrv) = edition.first_version()
        {
            let edition_msrv = RustVersion::try_from(edition_msrv).unwrap();
            if !edition_msrv.is_compatible_with(&pkg_msrv.to_partial()) {
                bail!(
                    "rust-version {pkg_msrv} is incompatible with the version ({edition_msrv}) required by \
                         the specified edition ({edition})",
                )
            }
        }
        edition
    } else {
        let msrv_edition = if let Some(pkg_msrv) = &rust_version {
            Edition::ALL
                .iter()
                .filter(|e| {
                    e.first_version()
                        .map(|e| {
                            RustVersion::try_from(e)
                                .unwrap()
                                .is_compatible_with(&pkg_msrv.to_partial())
                        })
                        .unwrap_or_default()
                })
                .max()
                .copied()
        } else {
            None
        }
        .unwrap_or_default();
        let default_edition = Edition::default();
        let latest_edition = Edition::LATEST_STABLE;
        if msrv_edition != default_edition || rust_version.is_none() {
            let tip = if msrv_edition == latest_edition || rust_version.is_none() {
                format!(" while the latest is `{latest_edition}`")
            } else {
                format!(" while {msrv_edition} is compatible with `rust-version`")
            };
            warnings.push(format!(
                "`package.edition` is unspecified, defaulting to `{default_edition}`{tip}"
            ));
        }
        default_edition
    };
    if !edition.is_stable() {
        let version = normalized_package
            .normalized_version()
            .expect("previously normalized")
            .map(|v| format!("@{v}"))
            .unwrap_or_default();
        let hint = rust_version
            .as_ref()
            .map(|rv| format!("help: {package_name}{version} requires rust {rv}"));
        features.require_with_hint(Feature::unstable_editions, hint.as_deref())?;
    }

    if original_toml.project.is_some() {
        if Edition::Edition2024 <= edition {
            bail!("`[project]` is not supported as of the 2024 Edition, please use `[package]`");
        } else {
            warnings.push("`[project]` is deprecated in favor of `[package]`".to_owned());
        }
    }

    if normalized_package.metabuild.is_some() {
        features.require(Feature::metabuild)?;
    }

    let resolve_behavior = match (
        normalized_package.resolver.as_ref(),
        normalized_toml.workspace.as_ref().and_then(|ws| ws.resolver.as_ref()),
    ) {
        (None, None) => None,
        (Some(s), None) | (None, Some(s)) => Some(ResolveBehavior::from_manifest(s)?),
        (Some(_), Some(_)) => {
            bail!("cannot specify `resolver` field in both `[workspace]` and `[package]`")
        }
    };

    let targets = targets::to_targets(
        &features,
        &original_toml,
        &normalized_toml,
        package_root,
        edition,
        &normalized_package.metabuild,
        warnings,
    )?;

    if targets.iter().all(|t| t.is_custom_build()) {
        bail!(
            "no targets specified in the manifest\n\
             either src/lib.rs, src/main.rs, a [lib] section, or \
             [[bin]] section must be present"
        )
    }

    if let Some(links) = &normalized_package.links
        && !targets.iter().any(|t| t.is_custom_build())
    {
        bail!(
            "package specifies that it links to `{links}` but does not have a custom build script"
        )
    }

    for (name, _) in original_toml.target.iter().flatten() {
        let platform: Platform = name.parse()?;
        platform.check_cfg_attributes(warnings);
        platform.check_cfg_keywords(warnings, manifest_file);
    }

    let mut deps = Vec::new();
    let mut manifest_ctx = ManifestContext {
        deps: &mut deps,
        source_id,
        config: &ctx.config,
        warnings,
        platform: None,
        file: manifest_file,
    };
    gather_dependencies(&mut manifest_ctx, normalized_toml.dependencies.as_ref(), None)?;
    gather_dependencies(
        &mut manifest_ctx,
        normalized_toml.dev_dependencies(),
        Some(DepKind::Development),
    )?;
    gather_dependencies(
        &mut manifest_ctx,
        normalized_toml.build_dependencies(),
        Some(DepKind::Build),
    )?;
    for (name, platform) in normalized_toml.target.iter().flatten() {
        manifest_ctx.platform = Some(name.parse()?);
        gather_dependencies(&mut manifest_ctx, platform.dependencies.as_ref(), None)?;
        gather_dependencies(
            &mut manifest_ctx,
            platform.build_dependencies(),
            Some(DepKind::Build),
        )?;
        gather_dependencies(
            &mut manifest_ctx,
            platform.dev_dependencies(),
            Some(DepKind::Development),
        )?;
    }
    let replace = replace(&normalized_toml, &mut manifest_ctx)?;
    let patch = patch(&normalized_toml, &mut manifest_ctx)?;

    {
        let mut names_sources = HashMap::default();
        for dep in &deps {
            let name = dep.name_in_toml();
            let prev = names_sources.insert(name, &dep.source_id);
            if prev.is_some() && prev != Some(&dep.source_id) {
                bail!(
                    "Dependency '{name}' has different source paths depending on the build \
                     target. Each dependency must have a single canonical source path \
                     irrespective of build target."
                );
            }
        }
    }

    verify_lints(normalized_toml.normalized_lints().expect("previously normalized"), warnings)?;

    let metadata = ManifestMetadata {
        description: normalized_package
            .normalized_description()
            .expect("previously normalized")
            .cloned(),
        homepage: normalized_package.normalized_homepage().expect("previously normalized").cloned(),
        documentation: normalized_package
            .normalized_documentation()
            .expect("previously normalized")
            .cloned(),
        readme: normalized_package.normalized_readme().expect("previously normalized").cloned(),
        authors: normalized_package
            .normalized_authors()
            .expect("previously normalized")
            .cloned()
            .unwrap_or_default(),
        license: normalized_package.normalized_license().expect("previously normalized").cloned(),
        license_file: normalized_package
            .normalized_license_file()
            .expect("previously normalized")
            .cloned(),
        repository: normalized_package
            .normalized_repository()
            .expect("previously normalized")
            .cloned(),
        keywords: normalized_package
            .normalized_keywords()
            .expect("previously normalized")
            .cloned()
            .unwrap_or_default(),
        categories: normalized_package
            .normalized_categories()
            .expect("previously normalized")
            .cloned()
            .unwrap_or_default(),
        links: normalized_package.links.clone(),
        rust_version,
    };

    if let Some(profiles) = &normalized_toml.profile {
        validate_profiles(profiles, &features, ctx.config.unstable(), warnings)?;
    }

    let version = normalized_package.normalized_version().expect("previously normalized");
    let publish = match normalized_package.normalized_publish().expect("previously normalized") {
        Some(VecStringOrBool::VecString(vecstring)) => Some(vecstring.clone()),
        Some(VecStringOrBool::Bool(false)) => Some(vec![]),
        Some(VecStringOrBool::Bool(true)) => None,
        None => version.is_none().then_some(vec![]),
    };
    if version.is_none() && publish != Some(vec![]) {
        bail!("`package.publish` requires `package.version` be specified");
    }

    let package_id = PackageId {
        name: package_name.to_string(),
        version: version.cloned().unwrap_or_else(|| semver::Version::new(0, 0, 0)),
        source_id,
    };
    // BTreeMap: `closest_msg` in `build_summary_features` breaks ties in its key order.
    let declared_features: BTreeMap<String, Vec<String>> = normalized_toml
        .features
        .iter()
        .flatten()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    let feature_map = build_summary_features(&declared_features, &deps)?;

    if feature_map.contains_key("default-features") {
        warnings.push(
            "`[features]` defines a feature named `default-features`
note: only a feature named `default` will be enabled by default"
                .to_owned(),
        )
    }

    if let Some(run) = &normalized_package.default_run
        && !targets.iter().filter(|t| t.is_bin()).any(|t| t.name == *run)
    {
        let suggestion =
            closest_msg(run, targets.iter().filter(|t| t.is_bin()), |t| t.name.as_str(), "target");
        bail!("default-run target `{run}` not found{suggestion}");
    }

    if normalized_package.im_a_teapot.is_some() {
        features.require(Feature::test_dummy_unstable).context(
            "the `im-a-teapot` manifest key is unstable and may not work properly in England",
        )?;
    }
    if normalized_package.default_target.is_some() || normalized_package.forced_target.is_some() {
        features.require(Feature::per_package_target).context(
            "the `package.default-target` and `package.forced-target` \
             manifest keys are unstable and may not work properly",
        )?;
    }

    if normalized_package.license_file.is_some() && normalized_package.license.is_some() {
        warnings.push(
            "only one of `license` or `license-file` is necessary\n\
             `license` should be used if the package license can be expressed \
             with a standard SPDX expression.\n\
             `license-file` should be used if the package uses a non-standard license.\n\
             See https://doc.rust-lang.org/cargo/reference/manifest.html#the-license-and-license-file-fields \
             for more information."
                .to_owned(),
        );
    }

    Ok(Manifest {
        package_id,
        dependencies: deps,
        features: feature_map,
        targets,
        metadata,
        custom_metadata: normalized_package.metadata.clone(),
        publish,
        replace,
        patch,
        workspace: workspace_config,
        unstable_features: features,
        edition,
        default_run: normalized_package.default_run.clone(),
        metabuild: normalized_package.metabuild.clone().map(|sov| sov.0),
        resolve_behavior,
        hints: normalized_toml.hints.clone(),
        normalized_toml,
        warnings: Vec::new(),
    })
}

fn to_virtual_manifest<F: Files>(
    normalized_toml: TomlManifest,
    features: Features,
    workspace_config: WorkspaceConfig,
    source_id: SourceId,
    manifest_file: &Path,
    ctx: &Context<F>,
) -> anyhow::Result<VirtualManifest> {
    let mut deps = Vec::new();
    let mut warnings = Vec::new();
    let (replace, patch) = {
        let mut manifest_ctx = ManifestContext {
            deps: &mut deps,
            source_id,
            config: &ctx.config,
            warnings: &mut warnings,
            platform: None,
            file: manifest_file,
        };
        (replace(&normalized_toml, &mut manifest_ctx)?, patch(&normalized_toml, &mut manifest_ctx)?)
    };
    if let Some(profiles) = &normalized_toml.profile {
        validate_profiles(profiles, &features, ctx.config.unstable(), &mut warnings)?;
    }
    let resolve_behavior = normalized_toml
        .workspace
        .as_ref()
        .and_then(|ws| ws.resolver.as_deref())
        .map(ResolveBehavior::from_manifest)
        .transpose()?;
    if let WorkspaceConfig::Member { .. } = &workspace_config {
        bail!("virtual manifests must be configured with [workspace]");
    }
    Ok(VirtualManifest {
        replace,
        patch,
        workspace: workspace_config,
        unstable_features: features,
        resolve_behavior,
        normalized_toml,
        warnings,
    })
}

struct ManifestContext<'a, 'b, F: Files> {
    deps: &'a mut Vec<Dependency>,
    source_id: SourceId,
    config: &'b Config<F>,
    warnings: &'a mut Vec<String>,
    platform: Option<Platform>,
    file: &'a Path,
}

fn gather_dependencies<F: Files>(
    manifest_ctx: &mut ManifestContext<'_, '_, F>,
    normalized_deps: Option<&BTreeMap<PackageName, manifest::InheritableDependency>>,
    kind: Option<DepKind>,
) -> anyhow::Result<()> {
    let Some(dependencies) = normalized_deps else {
        return Ok(());
    };
    for (n, v) in dependencies {
        let resolved = v.normalized().expect("previously normalized");
        let dep = dep_to_dependency(resolved, n, manifest_ctx, kind)?;
        manifest_ctx.deps.push(dep);
    }
    Ok(())
}

fn replace<F: Files>(
    me: &TomlManifest,
    manifest_ctx: &mut ManifestContext<'_, '_, F>,
) -> anyhow::Result<Vec<(PackageIdSpec, Dependency)>> {
    if me.patch.is_some() && me.replace.is_some() {
        bail!("cannot specify both [replace] and [patch]");
    }
    let mut replace = Vec::new();
    for (spec, replacement) in me.replace.iter().flatten() {
        let mut spec = PackageIdSpec::parse(spec).with_context(|| {
            format!(
                "replacements must specify a valid semver \
                 version to replace, but `{spec}` does not"
            )
        })?;
        if spec.url().is_none() {
            spec.set_url(CRATES_IO_INDEX.parse().unwrap());
        }
        if replacement.is_version_specified() {
            bail!(
                "replacements cannot specify a version \
                 requirement, but found one for `{spec}`"
            );
        }
        let mut dep = dep_to_dependency(replacement, spec.name(), manifest_ctx, None)?;
        let version = spec.version().ok_or_else(|| {
            anyhow!("replacements must specify a version to replace, but `{spec}` does not")
        })?;
        dep.req = Some(semver::VersionReq {
            comparators: vec![semver::Comparator {
                op: semver::Op::Exact,
                major: version.major,
                minor: Some(version.minor),
                patch: Some(version.patch),
                pre: version.pre.clone(),
            }],
        });
        replace.push((spec, dep));
    }
    Ok(replace)
}

fn patch<F: Files>(
    me: &TomlManifest,
    manifest_ctx: &mut ManifestContext<'_, '_, F>,
) -> anyhow::Result<HashMap<Url, Vec<Dependency>>> {
    let mut patch = HashMap::default();
    for (toml_url, deps) in me.patch.iter().flatten() {
        let url = match &toml_url[..] {
            CRATES_IO_REGISTRY => CRATES_IO_INDEX.parse().unwrap(),
            _ => manifest_ctx
                .config
                .registry_index(toml_url)
                .or_else(|_| into_url(toml_url))
                .with_context(|| {
                    format!(
                        "[patch] entry `{toml_url}` should be a URL or registry name{}",
                        if toml_url == "crates" {
                            "\nFor crates.io, use [patch.crates-io] (with a dash)"
                        } else {
                            ""
                        }
                    )
                })?,
        };
        let deps = deps
            .iter()
            .map(|(name, dep)| dep_to_dependency(dep, name, manifest_ctx, None))
            .collect::<anyhow::Result<Vec<_>>>()?;
        patch.insert(url, deps);
    }
    Ok(patch)
}

fn dep_to_dependency<F: Files>(
    orig: &TomlDependency,
    name_in_toml: &str,
    manifest_ctx: &mut ManifestContext<'_, '_, F>,
    kind: Option<DepKind>,
) -> anyhow::Result<Dependency> {
    let simple;
    let orig = match orig {
        TomlDependency::Simple(version) => {
            simple =
                TomlDetailedDependency { version: Some(version.clone()), ..Default::default() };
            &simple
        }
        TomlDependency::Detailed(details) => details,
    };

    if orig.version.is_none() && orig.path.is_none() && orig.git.is_none() {
        bail!(
            "dependency ({name_in_toml}) specified without \
             providing a local path, Git repository, version, or \
             workspace dependency to use"
        );
    }

    if let Some(version) = &orig.version
        && version.contains('+')
    {
        manifest_ctx.warnings.push(format!(
            "version requirement `{version}` for dependency `{name_in_toml}` \
                 includes semver metadata which will be ignored, removing the \
                 metadata is recommended to avoid confusion"
        ));
    }

    if orig.git.is_none() {
        for (key, key_name) in [(&orig.branch, "branch"), (&orig.tag, "tag"), (&orig.rev, "rev")] {
            if key.is_some() {
                bail!("key `{key_name}` is ignored for dependency ({name_in_toml}).");
            }
        }
    }

    if let Some(features) = &orig.features {
        for feature in features {
            if feature.contains('/') {
                bail!(
                    "feature `{feature}` in dependency `{name_in_toml}` is not allowed to contain slashes\n\
                     If you want to enable features of a transitive dependency, \
                     the direct dependency needs to re-export those features from \
                     the `[features]` table."
                );
            }
            if feature.starts_with("dep:") {
                bail!(
                    "feature `{feature}` in dependency `{name_in_toml}` is not allowed to use explicit \
                     `dep:` syntax\n\
                     If you want to enable an optional dependency, specify the name \
                     of the optional dependency without the `dep:` prefix, or specify \
                     a feature from the dependency's `[features]` table that enables \
                     the optional dependency."
                );
            }
        }
    }

    let new_source_id = to_dependency_source_id(orig, name_in_toml, manifest_ctx)?;

    let (pkg_name, explicit_name_in_toml) = match orig.package {
        Some(ref s) => (&s[..], Some(name_in_toml)),
        None => (name_in_toml, None),
    };

    let mut dep = Dependency::parse(pkg_name, orig.version.as_deref(), new_source_id)?;
    dep.features = orig.features.iter().flatten().cloned().collect();
    dep.default_features = orig.default_features().unwrap_or(true);
    dep.optional = orig.optional.unwrap_or(false);
    dep.platform = manifest_ctx.platform.clone();
    if let Some(registry) = &orig.registry {
        dep.registry_id = Some(source::alt_registry(manifest_ctx.config, registry)?);
    }
    if let Some(registry_index) = &orig.registry_index {
        dep.registry_id = Some(SourceId::for_registry(&into_url(registry_index)?)?);
    }
    if let Some(kind) = kind {
        dep.kind = kind;
    }
    dep.explicit_name_in_toml = explicit_name_in_toml.map(str::to_owned);
    if let Some(p) = orig.public {
        dep.public = p;
    }

    if orig.artifact.is_some() && !manifest_ctx.config.bindeps() {
        bail!("`artifact = …` requires `-Z bindeps` ({name_in_toml})");
    }
    match (&orig.artifact, orig.lib, orig.target.as_deref()) {
        (Some(artifact), lib, target) => {
            let artifact =
                crate::dependency::Artifact::parse(&artifact.0, lib.unwrap_or(false), target)?;
            if dep.kind != DepKind::Build && artifact.target.as_deref() == Some("target") {
                bail!(
                    r#"`target = "target"` in normal- or dev-dependencies has no effect ({name_in_toml})"#
                );
            }
            dep.artifact = Some(artifact);
        }
        (None, Some(_), _) => {
            bail!("'lib' specifier cannot be used without an 'artifact = …' value ({name_in_toml})")
        }
        (None, None, Some(_)) => bail!(
            "'target' specifier cannot be used without an 'artifact = …' value ({name_in_toml})"
        ),
        (None, None, None) => {}
    }
    Ok(dep)
}

fn to_dependency_source_id<F: Files>(
    orig: &TomlDetailedDependency,
    name_in_toml: &str,
    manifest_ctx: &mut ManifestContext<'_, '_, F>,
) -> anyhow::Result<SourceId> {
    match (
        orig.git.as_ref(),
        orig.path.as_ref(),
        orig.registry.as_deref(),
        orig.registry_index.as_ref(),
    ) {
        (Some(_git), Some(_path), _, _) => bail!(
            "dependency ({name_in_toml}) specification is ambiguous. \
             Only one of `git` or `path` is allowed.",
        ),
        (_, _, Some(_registry), Some(_registry_index)) => bail!(
            "dependency ({name_in_toml}) specification is ambiguous. \
             Only one of `registry` or `registry-index` is allowed.",
        ),
        (Some(git), None, _, _) => {
            let n_details =
                [&orig.branch, &orig.tag, &orig.rev].iter().filter(|d| d.is_some()).count();
            if n_details > 1 {
                bail!(
                    "dependency ({name_in_toml}) specification is ambiguous. \
                     Only one of `branch`, `tag` or `rev` is allowed.",
                );
            }
            let reference = orig
                .branch
                .clone()
                .map(GitReference::Branch)
                .or_else(|| orig.tag.clone().map(GitReference::Tag))
                .or_else(|| orig.rev.clone().map(GitReference::Rev))
                .unwrap_or(GitReference::DefaultBranch);
            let loc = into_url(git)?;
            if let Some(fragment) = loc.fragment() {
                manifest_ctx.warnings.push(format!(
                    "URL fragment `#{fragment}` in git URL is ignored for dependency ({name_in_toml}). \
                     If you were trying to specify a specific git revision, \
                     use `rev = \"{fragment}\"` in the dependency declaration.",
                ));
            }
            SourceId::for_git(&loc, reference)
        }
        (None, Some(path), _, _) => {
            if manifest_ctx.source_id.is_path() {
                let path = manifest_ctx.file.parent().unwrap().join(path);
                SourceId::for_path(&normalize_path(&path))
            } else {
                Ok(manifest_ctx.source_id)
            }
        }
        (None, None, Some(registry), None) => source::alt_registry(manifest_ctx.config, registry),
        (None, None, None, Some(registry_index)) => {
            SourceId::for_registry(&into_url(registry_index)?)
        }
        (None, None, None, None) => source::crates_io(manifest_ctx.config),
    }
}

fn verify_lints(lints: Option<&TomlLints>, warnings: &mut Vec<String>) -> anyhow::Result<()> {
    let Some(lints) = lints else {
        return Ok(());
    };
    let supported = ["cargo", "clippy", "rust", "rustdoc"];
    for (tool, lints) in lints {
        if !supported.contains(&tool.as_str()) {
            warnings.push(format!(
                "unrecognized lint tool `lints.{tool}`, specifying unrecognized tools may break in the future.
supported tools: {}",
                supported.join(", "),
            ));
            continue;
        }
        for name in lints.keys() {
            if let Some((prefix, suffix)) = name.split_once("::") {
                if tool == prefix || (tool == "rust" && supported.contains(&prefix)) {
                    bail!(
                        "`lints.{tool}.{name}` is not valid lint name; try `lints.{prefix}.{suffix}`"
                    )
                } else {
                    bail!("`lints.{tool}.{name}` is not a valid lint name")
                }
            }
        }
    }
    Ok(())
}
