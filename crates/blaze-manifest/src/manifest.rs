//! One `Cargo.toml` read as the pinned nightly's cargo reads it (`src/workspace/parser/mod.rs`
//! there): parsed into cargo's own schema, `workspace = true` fields and dependencies
//! inherited, targets discovered, dependencies and features checked.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use blaze_data::Value;
use blaze_hash::{FixedState, Map, MapOpts};
use blaze_resolve::{PackageIdSpec, PartialVersion, Platform, RustVersion};
use blaze_semver::{Comparator, Op, Version, VersionReq};
use std::alloc::Global;
use url::Url;

use crate::config::Config;
use crate::error::{Context as During, ManifestError, ResultExt as _};
use crate::schema::{
    self as manifest, Hints, InheritableField, InheritablePackage, PackageName, PathBaseName,
    StringOrBool, TomlDependency, TomlDetailedDependency, TomlInheritedDependency,
    TomlLints, TomlManifest, TomlPackage, TomlPackageBuild, TomlWorkspace, VecStringOrBool,
};
use crate::config::path::ConfigRelativePath;
use crate::dependency::{DepKind, Dependency, FeatureMap, build_summary_features};
use crate::diagnostic::ManifestDiagnostic;
use crate::edit_distance::closest_msg;
use crate::features::{Edition, Feature, Features, ResolveBehavior};
use crate::files::Files;
use crate::paths::{normalize_path, relative_to};
use crate::profiles::validate_profiles;
use crate::source::{self, CRATES_IO_INDEX, CRATES_IO_REGISTRY, GitReference, SourceId, into_url};
use crate::targets::{self, Target};
use crate::workspace::find_workspace_root;

/// What reading manifests shares: the configuration, and the workspace roots read so far, which
/// cargo consults before walking up the file system (its `GlobalContext::ws_roots`).
/// Every file is read through the configuration's [`Files`].
pub struct Context<F: Files> {
    pub config: Config<F>,
    pub(crate) ws_roots: RefCell<Map<PathBuf, WorkspaceRootConfig, FixedState, Global>>,
}

impl<F: Files> Context<F> {
    pub fn new(config: Config<F>) -> Context<F> {
        Context { config, ws_roots: RefCell::new(Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None })) }
    }

    pub fn files(&self) -> &F {
        self.config.files()
    }
}

/// A package's identity: name, version and source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageId {
    pub name: String,
    pub version: Version<Global>,
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
            .with_kind(self.source_id.kind().clone()) // clone: the spec owns its kind, once per spec
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
    pub custom_metadata: Option<Value<Global>>,
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
    ) -> Result<Vec<(PathBuf, Option<&'g str>)>, ManifestError> {
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

fn expand_member_path<F: Files>(files: &F, path: &Path) -> Result<Vec<PathBuf>, ManifestError> {
    let Some(path) = path.to_str() else {
        return Ok(Vec::new());
    };
    let res = crate::files::glob(files, path)
        .context(|| During::Pattern { pattern: path.to_owned() })?;
    res.into_iter()
        .map(|p| p.context(|| During::PatternMatch { pattern: path.to_owned() }))
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
            fn $field(&self) -> Result<$ret, ManifestError> {
                let Some(val) = self.package.as_ref().and_then(|p| p.$field.as_ref()) else {
                    return Err(InvalidManifest::WorkspacePackageField { key: $key }.into());
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
        ("version",       version       -> Version<Global>),
    }

    fn get_dependency(
        &self,
        name: &str,
        package_root: &Path,
    ) -> Result<TomlDependency, ManifestError> {
        let Some(deps) = &self.dependencies else {
            return Err(InvalidManifest::WorkspaceDependencies.into());
        };
        let Some(dep) = deps.get(name) else {
            return Err(InvalidManifest::WorkspaceDependency { name: name.to_owned() }.into());
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

    fn lints(&self) -> Result<TomlLints, ManifestError> {
        let Some(val) = &self.lints else {
            return Err(InvalidManifest::WorkspaceLints.into());
        };
        Ok(val.clone())
    }

    fn license_file(&self, package_root: &Path) -> Result<String, ManifestError> {
        let Some(license_file) = self.package.as_ref().and_then(|p| p.license_file.as_ref()) else {
            return Err(InvalidManifest::WorkspacePackageField { key: "license-file" }.into());
        };
        resolve_relative_path("license-file", &self.ws_root, package_root, license_file)
    }

    fn readme<F: Files>(
        &self,
        files: &F,
        package_root: &Path,
    ) -> Result<StringOrBool, ManifestError> {
        let Some(readme) = normalize_package_readme(
            files,
            &self.ws_root,
            self.package.as_ref().and_then(|p| p.readme.as_ref()),
        ) else {
            return Err(InvalidManifest::WorkspacePackageField { key: "readme" }.into());
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
) -> Result<String, ManifestError> {
    let joined_path = normalize_path(&old_root.join(rel_path));
    match relative_to(&joined_path, new_root) {
        None => Err(InvalidManifest::Unrelatable {
            label: label.to_owned(),
            old_root: old_root.to_owned(),
            new_root: new_root.to_owned(),
        }
        .into()),
        Some(path) => match path.to_str() {
            Some(text) => Ok(text.to_owned()),
            None => Err(InvalidManifest::NotUtf8Path { label: label.to_owned(), path }.into()),
        },
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
    pub custom_metadata: Option<Value<Global>>,
    pub publish: Option<Vec<String>>,
    pub replace: Vec<(PackageIdSpec, Dependency)>,
    pub patch: Map<Url, Vec<Dependency>, FixedState, Global>,
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
    pub patch: Map<Url, Vec<Dependency>, FixedState, Global>,
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
pub fn parse_manifest(path: &Path, bytes: &[u8]) -> Result<TomlManifest, ManifestError> {
    let contents = std::str::from_utf8(bytes)
        .map_err(|_| ManifestError::NotUtf8 { path: path.to_owned() })?;
    let doc = blaze_toml::Document::parse_in(contents, blaze_toml::Limits::CARGO, Global)
        .map_err(|e| ManifestDiagnostic::toml(e, contents, path))?;
    TomlManifest::read(doc.root()).map_err(|e| ManifestDiagnostic::schema(e, contents, path))
}

pub fn read_manifest<F: Files>(
    path: &Path,
    source_id: SourceId,
    ctx: &Context<F>,
) -> Result<EitherManifest, ManifestError> {
    let ext = path.extension();
    if ext.is_none() || ext == Some(std::ffi::OsStr::new("rs")) {
        return Err(InvalidManifest::RequiresScript { path: path.to_owned() }.into());
    }
    let original_toml = ctx.files().manifest(path)?;

    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    (|| -> Result<EitherManifest, ManifestError> {
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
                &original_toml,
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
            Err(InvalidManifest::NeitherPackageNorWorkspace.into())
        }
    })()
    .context(|| During::ParseManifest { path: path.to_owned() })
}

fn to_workspace_config(
    original_toml: &TomlManifest,
    manifest_file: &Path,
    warnings: &mut Vec<String>,
) -> Result<WorkspaceConfig, ManifestError> {
    match (
        original_toml.workspace.as_ref(),
        original_toml.package().and_then(|p| p.workspace.as_ref()),
    ) {
        (Some(toml_config), None) => {
            verify_lints(toml_config.lints.as_ref(), warnings)?;
            if let Some(ws_deps) = &toml_config.dependencies {
                for (name, dep) in ws_deps {
                    if dep.is_optional() {
                        let name = name.to_string();
                        return Err(InvalidManifest::OptionalWorkspaceDependency { name }.into());
                    }
                    if dep.is_public() {
                        let name = name.to_string();
                        return Err(InvalidManifest::PublicWorkspaceDependency { name }.into());
                    }
                }
            }
            Ok(WorkspaceConfig::Root(Box::new(WorkspaceRootConfig::new(
                toml_config,
                manifest_file,
            ))))
        }
        (None, root) => Ok(WorkspaceConfig::Member { root: root.cloned() }),
        (Some(..), Some(..)) => Err(InvalidManifest::BothWorkspaces.into()),
    }
}

/// What `[workspace]` gives a member: read from its workspace root the first time a field
/// inherits, so a manifest that inherits nothing never reads the root.
struct Inherit<'a, F: Files> {
    #[expect(
        clippy::disallowed_types,
        reason = "the workspace root's fields, read the first time one is inherited: per manifest read, not global"
    )]
    fields: std::cell::OnceCell<InheritableFields>,
    ctx: &'a Context<F>,
    manifest_file: &'a Path,
    workspace_config: &'a WorkspaceConfig,
}

impl<F: Files> Inherit<'_, F> {
    fn fields(&self) -> Result<&InheritableFields, ManifestError> {
        if let Some(fields) = self.fields.get() {
            return Ok(fields);
        }
        let fields = load_inheritable_fields(self.ctx, self.manifest_file, self.workspace_config)?;
        Ok(self.fields.get_or_init(|| fields))
    }

    fn ws_root(&self) -> Result<&Path, ManifestError> {
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
) -> Result<TomlManifest, ManifestError> {
    let package_root = manifest_file.parent().unwrap();
    let files = ctx.files();

    let inherit = Inherit { fields: Default::default(), ctx, manifest_file, workspace_config };

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
        return Err(InvalidManifest::VirtualWithPackageField { field }.into());
    }

    Ok(normalized_toml)
}

fn normalize_patch<F: Files>(
    config: &Config<F>,
    original_patch: Option<&BTreeMap<String, BTreeMap<PackageName, TomlDependency>>>,
    inherit: &Inherit<'_, F>,
    features: &Features,
) -> Result<Option<BTreeMap<String, BTreeMap<PackageName, TomlDependency>>>, ManifestError> {
    let Some(patch) = original_patch else {
        return Ok(None);
    };
    let mut normalized_patch = BTreeMap::new();
    for (name, packages) in patch {
        let mut normalized_packages = BTreeMap::new();
        for (pkg, dep) in packages {
            let dep = if let TomlDependency::Detailed(dep) = dep {
                let mut dep = dep.clone();
                normalize_path_dependency(config, &mut dep, inherit, features).context(|| {
                    During::PatchPath { package: pkg.to_string(), source: name.clone() }
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
    get_ws_inheritable: impl FnOnce() -> Result<T, ManifestError>,
) -> Result<T, ManifestError> {
    match field {
        InheritableField::Value(value) => Ok(value),
        InheritableField::Inherit(_) => {
            get_ws_inheritable().context(|| During::InheritField { label: label.to_owned() })
        }
    }
}

fn normalize_package_toml<F: Files>(
    original_package: &TomlPackage,
    manifest_file: &Path,
    inherit: &Inherit<'_, F>,
    features: &Features,
) -> Result<Box<TomlPackage>, ManifestError> {
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
            .ok_or(InvalidManifest::MissingPackageName)?,
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
                field_inherit_with(value, "readme", || {
                    inherit.fields()?.readme(files, package_root)
                })
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
) -> Result<Option<BTreeMap<PackageName, manifest::InheritableDependency>>, ManifestError> {
    let Some(dependencies) = orig_deps else {
        return Ok(None);
    };
    let mut deps = BTreeMap::new();
    for (name_in_toml, v) in dependencies {
        let mut resolved = match v {
            manifest::InheritableDependency::Value(value) => value.clone(),
            manifest::InheritableDependency::Inherit(w) => {
                inner_dependency_inherit_with(
                    w,
                    name_in_toml,
                    inherit,
                    package_root,
                    edition,
                    warnings,
                )
                .context(|| During::InheritDependency { name: name_in_toml.to_string() })?
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
                .context(|| During::PathDependency { name: name_in_toml.to_string() })?;
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
) -> Result<(), ManifestError> {
    if let Some(base) = detailed_dep.base.take() {
        if let Some(path) = detailed_dep.path.as_mut() {
            let new_path = lookup_path_base(&base, config, inherit, features)?.join(&path);
            *path = new_path.to_str().unwrap().to_owned();
        } else {
            return Err(InvalidManifest::BaseWithoutPath.into());
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
) -> Result<PathBuf, ManifestError> {
    features.require(Feature::path_bases)?;
    if let Some(path) =
        config.get::<Option<ConfigRelativePath>>(&format!("path-bases.{}", base.as_str()))?
    {
        return Ok(path.resolve_path(config.cwd()));
    }
    match base.as_str() {
        "workspace" => Ok(inherit.ws_root()?.to_path_buf()),
        _ => Err(InvalidManifest::UndefinedPathBase { base: base.to_string() }.into()),
    }
}

fn load_inheritable_fields<F: Files>(
    ctx: &Context<F>,
    normalized_path: &Path,
    workspace_config: &WorkspaceConfig,
) -> Result<InheritableFields, ManifestError> {
    match workspace_config {
        WorkspaceConfig::Root(root) => Ok(root.inheritable.clone()),
        WorkspaceConfig::Member { root: Some(path_to_root) } => {
            let path = normalized_path.parent().unwrap().join(path_to_root).join("Cargo.toml");
            inheritable_from_path(ctx, normalize_path(&path))
        }
        WorkspaceConfig::Member { root: None } => {
            match find_workspace_root(normalized_path, ctx)? {
                Some(path_to_root) => inheritable_from_path(ctx, path_to_root),
                None => Err(InvalidManifest::NoWorkspaceRoot.into()),
            }
        }
    }
}

fn inheritable_from_path<F: Files>(
    ctx: &Context<F>,
    workspace_path: PathBuf,
) -> Result<InheritableFields, ManifestError> {
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
        _ => Err(InvalidManifest::InferredRootNotRoot { path: workspace_path }.into()),
    }
}

fn lints_inherit_with(
    lints: manifest::InheritableLints,
    get_ws_inheritable: impl FnOnce() -> Result<TomlLints, ManifestError>,
) -> Result<TomlLints, ManifestError> {
    if lints.workspace {
        if !lints.lints.is_empty() {
            return Err(InvalidManifest::OverrideWorkspaceLints.into());
        }
        get_ws_inheritable().context(|| During::InheritLints)
    } else {
        Ok(lints.lints)
    }
}

fn inner_dependency_inherit_with<F: Files>(
    pkg_dep: &TomlInheritedDependency,
    name: &str,
    inherit: &Inherit<'_, F>,
    package_root: &Path,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> Result<TomlDependency, ManifestError> {
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
) -> Result<(), ManifestError> {
    let old_path = new_path.replace('-', "_");
    if old.is_some() && Edition::Edition2024 <= edition {
        return Err(InvalidManifest::UnderscoreKey {
            old_path,
            new_path: new_path.to_owned(),
            name: name.to_owned(),
            kind: kind.to_owned(),
        }
        .into());
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
    original_toml: &TomlManifest,
    normalized_toml: TomlManifest,
    features: Features,
    workspace_config: WorkspaceConfig,
    source_id: SourceId,
    manifest_file: &Path,
    ctx: &Context<F>,
    warnings: &mut Vec<String>,
) -> Result<Manifest, ManifestError> {
    let package_root = manifest_file.parent().unwrap();
    if !ctx.files().is_dir(package_root) {
        return Err(InvalidManifest::RootNotDirectory { path: package_root.to_owned() }.into());
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
        let edition: Edition = edition.parse().context(|| During::EditionKey)?;
        if let Some(pkg_msrv) = &rust_version
            && let Some(edition_msrv) = edition.first_version()
        {
            let edition_msrv =
                RustVersion::new(edition_msrv.major, edition_msrv.minor, edition_msrv.patch);
            if !edition_msrv.is_compatible_with(&pkg_msrv.to_partial()) {
                return Err(InvalidManifest::EditionRustVersion {
                    rust_version: pkg_msrv.clone(),
                    edition_version: edition_msrv,
                    edition,
                }
                .into());
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
                            RustVersion::new(e.major, e.minor, e.patch)
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
            return Err(InvalidManifest::Project.into());
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
        (Some(_), Some(_)) => return Err(InvalidManifest::TwoResolvers.into()),
    };

    let targets = targets::to_targets(
        &features,
        original_toml,
        &normalized_toml,
        package_root,
        edition,
        &normalized_package.metabuild,
        warnings,
    )?;

    if targets.iter().all(|t| t.is_custom_build()) {
        return Err(InvalidManifest::NoTargets.into());
    }

    if let Some(links) = &normalized_package.links
        && !targets.iter().any(|t| t.is_custom_build())
    {
        return Err(InvalidManifest::LinksWithoutBuild { links: links.clone() }.into());
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
        let mut names_sources = Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None });
        for dep in &deps {
            let name = dep.name_in_toml();
            let prev = names_sources.insert(name, &dep.source_id);
            if prev.is_some() && prev != Some(&dep.source_id) {
                let name = name.to_owned();
                return Err(InvalidManifest::SourcePerTarget { name }.into());
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
        return Err(InvalidManifest::PublishWithoutVersion.into());
    }

    let package_id = PackageId {
        name: package_name.to_string(),
        version: version.cloned().unwrap_or(Version::new(0, 0, 0)),
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
        return Err(InvalidManifest::DefaultRun { run: run.clone(), suggestion }.into());
    }

    if normalized_package.im_a_teapot.is_some() {
        features.require(Feature::test_dummy_unstable).context(|| During::ImATeapot)?;
    }
    if normalized_package.default_target.is_some() || normalized_package.forced_target.is_some() {
        features.require(Feature::per_package_target).context(|| During::PerPackageTarget)?;
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
) -> Result<VirtualManifest, ManifestError> {
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
        return Err(InvalidManifest::VirtualMember.into());
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
) -> Result<(), ManifestError> {
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
) -> Result<Vec<(PackageIdSpec, Dependency)>, ManifestError> {
    if me.patch.is_some() && me.replace.is_some() {
        return Err(InvalidManifest::ReplaceAndPatch.into());
    }
    let mut replace = Vec::new();
    for (spec, replacement) in me.replace.iter().flatten() {
        let mut spec =
            PackageIdSpec::parse(spec).context(|| During::ReplaceSpec { spec: spec.clone() })?;
        if spec.url().is_none() {
            spec = spec.with_url(CRATES_IO_INDEX.parse().unwrap());
        }
        if replacement.is_version_specified() {
            return Err(InvalidManifest::ReplaceWithVersion { spec: spec.to_string() }.into());
        }
        let mut dep = dep_to_dependency(replacement, spec.name(), manifest_ctx, None)?;
        let Some(version) = spec.version() else {
            return Err(InvalidManifest::ReplaceWithoutVersion { spec: spec.to_string() }.into());
        };
        let mut comparators = blaze_own::Vec::new_in(Global);
        comparators.push(Comparator {
            op: Op::Exact,
            major: version.major,
            minor: Some(version.minor),
            patch: Some(version.patch),
            pre: version.pre.clone(),
        });
        dep.req = Some(VersionReq { comparators });
        replace.push((spec, dep));
    }
    Ok(replace)
}

fn patch<F: Files>(
    me: &TomlManifest,
    manifest_ctx: &mut ManifestContext<'_, '_, F>,
) -> Result<Map<Url, Vec<Dependency>, FixedState, Global>, ManifestError> {
    let mut patch = Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None });
    for (toml_url, deps) in me.patch.iter().flatten() {
        let url = match &toml_url[..] {
            CRATES_IO_REGISTRY => CRATES_IO_INDEX.parse().unwrap(),
            _ => manifest_ctx
                .config
                .registry_index(toml_url)
                .map_err(ManifestError::from)
                .or_else(|_| into_url(toml_url).map_err(ManifestError::from))
                .context(|| During::PatchEntry { url: toml_url.clone() })?,
        };
        let deps = deps
            .iter()
            .map(|(name, dep)| dep_to_dependency(dep, name, manifest_ctx, None))
            .collect::<Result<Vec<_>, ManifestError>>()?;
        patch.insert(url, deps);
    }
    Ok(patch)
}

fn dep_to_dependency<F: Files>(
    orig: &TomlDependency,
    name_in_toml: &str,
    manifest_ctx: &mut ManifestContext<'_, '_, F>,
    kind: Option<DepKind>,
) -> Result<Dependency, ManifestError> {
    let simple;
    let orig = match orig {
        TomlDependency::Simple(version) => {
            simple =
                TomlDetailedDependency { version: Some(version.clone()), ..Default::default() };
            &simple
        }
        TomlDependency::Detailed(details) => details,
    };

    let name = || name_in_toml.to_owned();
    if orig.version.is_none() && orig.path.is_none() && orig.git.is_none() {
        return Err(InvalidManifest::DependencyWithoutSource { name: name() }.into());
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
                let name = name();
                return Err(InvalidManifest::GitKeyWithoutGit { key: key_name, name }.into());
            }
        }
    }

    if let Some(features) = &orig.features {
        for feature in features {
            if feature.contains('/') {
                let feature = feature.clone();
                return Err(InvalidManifest::FeatureSlash { feature, name: name() }.into());
            }
            if feature.starts_with("dep:") {
                let feature = feature.clone();
                return Err(InvalidManifest::FeatureDepSyntax { feature, name: name() }.into());
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

    // blaze's `artifact = "gpu"` is its own opt-in extension, outside cargo's unstable bindeps.
    if let Some(artifact) = &orig.artifact
        && !crate::dependency::Artifact::asks_gpu(&artifact.0)
        && !manifest_ctx.config.bindeps()
    {
        return Err(InvalidManifest::ArtifactWithoutBindeps { name: name() }.into());
    }
    match (&orig.artifact, orig.lib, orig.target.as_deref()) {
        (Some(artifact), lib, target) => {
            let artifact =
                crate::dependency::Artifact::parse(&artifact.0, lib.unwrap_or(false), target)?;
            if dep.kind != DepKind::Build && artifact.target.as_deref() == Some("target") {
                return Err(InvalidManifest::TargetTargetNotBuild { name: name() }.into());
            }
            if artifact.is_gpu() && dep.kind == DepKind::Build {
                return Err(InvalidManifest::GpuBuildDependency { name: name() }.into());
            }
            dep.artifact = Some(artifact);
        }
        (None, Some(_), _) => {
            let name = name();
            return Err(InvalidManifest::ArtifactKeyWithoutArtifact { key: "lib", name }.into());
        }
        (None, None, Some(_)) => {
            let name = name();
            return Err(InvalidManifest::ArtifactKeyWithoutArtifact { key: "target", name }.into());
        }
        (None, None, None) => {}
    }
    Ok(dep)
}

fn to_dependency_source_id<F: Files>(
    orig: &TomlDetailedDependency,
    name_in_toml: &str,
    manifest_ctx: &mut ManifestContext<'_, '_, F>,
) -> Result<SourceId, ManifestError> {
    match (
        orig.git.as_ref(),
        orig.path.as_ref(),
        orig.registry.as_deref(),
        orig.registry_index.as_ref(),
    ) {
        (Some(_git), Some(_path), _, _) => Err(InvalidManifest::Ambiguous {
            name: name_in_toml.to_owned(),
            keys: "`git` or `path`",
        }
        .into()),
        (_, _, Some(_registry), Some(_registry_index)) => Err(InvalidManifest::Ambiguous {
            name: name_in_toml.to_owned(),
            keys: "`registry` or `registry-index`",
        }
        .into()),
        (Some(git), None, _, _) => {
            let n_details =
                [&orig.branch, &orig.tag, &orig.rev].iter().filter(|d| d.is_some()).count();
            if n_details > 1 {
                return Err(InvalidManifest::Ambiguous {
                    name: name_in_toml.to_owned(),
                    keys: "`branch`, `tag` or `rev`",
                }
                .into());
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
            Ok(SourceId::for_git(&loc, reference)?)
        }
        (None, Some(path), _, _) => {
            if manifest_ctx.source_id.is_path() {
                let path = manifest_ctx.file.parent().unwrap().join(path);
                Ok(SourceId::for_path(&normalize_path(&path))?)
            } else {
                Ok(manifest_ctx.source_id)
            }
        }
        (None, None, Some(registry), None) => source::alt_registry(manifest_ctx.config, registry),
        (None, None, None, Some(registry_index)) => {
            Ok(SourceId::for_registry(&into_url(registry_index)?)?)
        }
        (None, None, None, None) => source::crates_io(manifest_ctx.config),
    }
}

fn verify_lints(
    lints: Option<&TomlLints>,
    warnings: &mut Vec<String>,
) -> Result<(), ManifestError> {
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
                let known_prefix = tool == prefix || (tool == "rust" && supported.contains(&prefix));
                let try_instead = known_prefix.then(|| format!("lints.{prefix}.{suffix}"));
                return Err(InvalidManifest::LintName {
                    lint: format!("lints.{tool}.{name}"),
                    try_instead,
                }
                .into());
            }
        }
    }
    Ok(())
}

/// A manifest that parses and fits the schema but that cargo refuses, each with cargo's message.
#[derive(Debug)]
pub enum InvalidManifest {
    WorkspacePackageField { key: &'static str },
    WorkspaceDependencies,
    WorkspaceDependency { name: String },
    WorkspaceLints,
    Unrelatable { label: String, old_root: PathBuf, new_root: PathBuf },
    NotUtf8Path { label: String, path: PathBuf },
    RequiresScript { path: PathBuf },
    NeitherPackageNorWorkspace,
    OptionalWorkspaceDependency { name: String },
    PublicWorkspaceDependency { name: String },
    BothWorkspaces,
    VirtualWithPackageField { field: &'static str },
    MissingPackageName,
    BaseWithoutPath,
    UndefinedPathBase { base: String },
    NoWorkspaceRoot,
    InferredRootNotRoot { path: PathBuf },
    OverrideWorkspaceLints,
    UnderscoreKey { old_path: String, new_path: String, name: String, kind: String },
    RootNotDirectory { path: PathBuf },
    EditionRustVersion {
        rust_version: RustVersion,
        edition_version: RustVersion,
        edition: Edition,
    },
    Project,
    TwoResolvers,
    NoTargets,
    LinksWithoutBuild { links: String },
    SourcePerTarget { name: String },
    PublishWithoutVersion,
    DefaultRun { run: String, suggestion: String },
    VirtualMember,
    ReplaceAndPatch,
    ReplaceWithVersion { spec: String },
    ReplaceWithoutVersion { spec: String },
    DependencyWithoutSource { name: String },
    GitKeyWithoutGit { key: &'static str, name: String },
    FeatureSlash { feature: String, name: String },
    FeatureDepSyntax { feature: String, name: String },
    ArtifactWithoutBindeps { name: String },
    TargetTargetNotBuild { name: String },
    GpuBuildDependency { name: String },
    ArtifactKeyWithoutArtifact { key: &'static str, name: String },
    Ambiguous { name: String, keys: &'static str },
    LintName { lint: String, try_instead: Option<String> },
}

impl fmt::Display for InvalidManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use InvalidManifest as I;
        match self {
            I::WorkspacePackageField { key } => {
                write!(f, "`workspace.package.{key}` was not defined")
            }
            I::WorkspaceDependencies => f.write_str("`workspace.dependencies` was not defined"),
            I::WorkspaceDependency { name } => {
                write!(f, "`dependency.{name}` was not found in `workspace.dependencies`")
            }
            I::WorkspaceLints => f.write_str("`workspace.lints` was not defined"),
            I::Unrelatable { label, old_root, new_root } => write!(
                f,
                "`{label}` was defined in {} but could not be resolved with {}",
                old_root.display(),
                new_root.display()
            ),
            I::NotUtf8Path { label, path } => {
                write!(f, "`{label}` resolved to non-UTF value (`{}`)", path.display())
            }
            I::RequiresScript { path } => {
                write!(f, "parsing `{}` requires `-Zscript`", path.display())
            }
            I::NeitherPackageNorWorkspace => {
                f.write_str("manifest is missing either a `[package]` or a `[workspace]`")
            }
            I::OptionalWorkspaceDependency { name } => {
                write!(f, "{name} is optional, but workspace dependencies cannot be optional")
            }
            I::PublicWorkspaceDependency { name } => {
                write!(f, "{name} is public, but workspace dependencies cannot be public")
            }
            I::BothWorkspaces => f.write_str(
                "cannot configure both `package.workspace` and `[workspace]`, only one can be \
                 specified",
            ),
            I::VirtualWithPackageField { field } => {
                write!(
                    f,
                    "this virtual manifest specifies a `{field}` section, which is not allowed"
                )?;
                if *field == "lints" {
                    f.write_str("\nhelp: a similar field exists: `[workspace.lints]`")?;
                }
                Ok(())
            }
            I::MissingPackageName => f.write_str("missing field `package.name`"),
            I::BaseWithoutPath => f.write_str("`base` can only be used with path dependencies"),
            I::UndefinedPathBase { base } => write!(
                f,
                "path base `{base}` is undefined. You must add an entry for `{base}` in the Cargo \
                 configuration [path-bases] table."
            ),
            I::NoWorkspaceRoot => f.write_str("failed to find a workspace root"),
            I::InferredRootNotRoot { path } => {
                write!(f, "root of a workspace inferred but wasn't a root: {}", path.display())
            }
            I::OverrideWorkspaceLints => f.write_str(
                "cannot override `workspace.lints` in `lints`, either remove the overrides or \
                 `lints.workspace = true` and manually specify the lints",
            ),
            I::UnderscoreKey { old_path, new_path, name, kind } => write!(
                f,
                "`{old_path}` is unsupported as of the 2024 edition; instead use `{new_path}`\n\
                 (in the `{name}` {kind})"
            ),
            I::RootNotDirectory { path } => {
                write!(f, "package root '{}' is not a directory", path.display())
            }
            I::EditionRustVersion { rust_version, edition_version, edition } => write!(
                f,
                "rust-version {rust_version} is incompatible with the version ({edition_version}) \
                 required by the specified edition ({edition})"
            ),
            I::Project => f.write_str(
                "`[project]` is not supported as of the 2024 Edition, please use `[package]`",
            ),
            I::TwoResolvers => {
                f.write_str("cannot specify `resolver` field in both `[workspace]` and `[package]`")
            }
            I::NoTargets => f.write_str(
                "no targets specified in the manifest\neither src/lib.rs, src/main.rs, a [lib] \
                 section, or [[bin]] section must be present",
            ),
            I::LinksWithoutBuild { links } => write!(
                f,
                "package specifies that it links to `{links}` but does not have a custom build \
                 script"
            ),
            I::SourcePerTarget { name } => write!(
                f,
                "Dependency '{name}' has different source paths depending on the build target. \
                 Each dependency must have a single canonical source path irrespective of build \
                 target."
            ),
            I::PublishWithoutVersion => {
                f.write_str("`package.publish` requires `package.version` be specified")
            }
            I::DefaultRun { run, suggestion } => {
                write!(f, "default-run target `{run}` not found{suggestion}")
            }
            I::VirtualMember => {
                f.write_str("virtual manifests must be configured with [workspace]")
            }
            I::ReplaceAndPatch => f.write_str("cannot specify both [replace] and [patch]"),
            I::ReplaceWithVersion { spec } => write!(
                f,
                "replacements cannot specify a version requirement, but found one for `{spec}`"
            ),
            I::ReplaceWithoutVersion { spec } => write!(
                f,
                "replacements must specify a version to replace, but `{spec}` does not"
            ),
            I::DependencyWithoutSource { name } => write!(
                f,
                "dependency ({name}) specified without providing a local path, Git repository, \
                 version, or workspace dependency to use"
            ),
            I::GitKeyWithoutGit { key, name } => {
                write!(f, "key `{key}` is ignored for dependency ({name}).")
            }
            I::FeatureSlash { feature, name } => write!(
                f,
                "feature `{feature}` in dependency `{name}` is not allowed to contain slashes\n\
                 If you want to enable features of a transitive dependency, the direct dependency \
                 needs to re-export those features from the `[features]` table."
            ),
            I::FeatureDepSyntax { feature, name } => write!(
                f,
                "feature `{feature}` in dependency `{name}` is not allowed to use explicit `dep:` \
                 syntax\nIf you want to enable an optional dependency, specify the name of the \
                 optional dependency without the `dep:` prefix, or specify a feature from the \
                 dependency's `[features]` table that enables the optional dependency."
            ),
            I::ArtifactWithoutBindeps { name } => {
                write!(f, "`artifact = …` requires `-Z bindeps` ({name})")
            }
            I::TargetTargetNotBuild { name } => write!(
                f,
                "`target = \"target\"` in normal- or dev-dependencies has no effect ({name})"
            ),
            I::GpuBuildDependency { name } => write!(
                f,
                "artifact `gpu` (blaze's device build) is for a normal or dev dependency, not a \
                 build dependency ({name})"
            ),
            I::ArtifactKeyWithoutArtifact { key, name } => write!(
                f,
                "'{key}' specifier cannot be used without an 'artifact = …' value ({name})"
            ),
            I::Ambiguous { name, keys } => write!(
                f,
                "dependency ({name}) specification is ambiguous. Only one of {keys} is allowed."
            ),
            I::LintName { lint, try_instead: Some(instead) } => {
                write!(f, "`{lint}` is not valid lint name; try `{instead}`")
            }
            I::LintName { lint, try_instead: None } => {
                write!(f, "`{lint}` is not a valid lint name")
            }
        }
    }
}

impl core::error::Error for InvalidManifest {}
