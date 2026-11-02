//! `cargo metadata --no-deps --format-version 1`: the workspace's members as cargo prints them,
//! in its field order (`src/ops/cargo_metadata.rs` and `src/workspace/package.rs` there).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::bail;
use cargo_util_schemas::core::PackageIdSpec;
use cargo_util_schemas::manifest::{Hints, RustVersion};
use serde::Serialize;

use crate::dependency::SerializedDependency;
use crate::files::Files;
use crate::manifest::{Context, Manifest};
use crate::paths::{ancestors, normalize_path};
use crate::targets::SerializedTarget;
use crate::workspace::Workspace;

const VERSION: u32 = 1;

#[derive(Serialize)]
pub struct ExportInfo<'a, R = ()> {
    packages: Vec<SerializedPackage<'a>>,
    workspace_members: Vec<PackageIdSpec>,
    workspace_default_members: Vec<PackageIdSpec>,
    resolve: Option<R>,
    target_directory: PathBuf,
    build_directory: PathBuf,
    version: u32,
    workspace_root: &'a Path,
    metadata: Option<&'a toml::Value>,
}

/// A package as `cargo metadata` prints it.
#[derive(Serialize)]
pub struct SerializedPackage<'a> {
    pub name: &'a str,
    pub version: &'a semver::Version,
    pub id: PackageIdSpec,
    pub license: Option<&'a str>,
    pub license_file: Option<&'a str>,
    pub description: Option<&'a str>,
    pub source: Option<String>,
    pub dependencies: Vec<SerializedDependency<'a>>,
    pub targets: Vec<SerializedTarget<'a>>,
    pub features: BTreeMap<&'a str, Vec<String>>,
    pub manifest_path: &'a Path,
    pub metadata: Option<&'a toml::Value>,
    pub publish: Option<&'a [String]>,
    pub authors: &'a [String],
    pub categories: &'a [String],
    pub keywords: &'a [String],
    pub readme: Option<&'a str>,
    pub repository: Option<&'a str>,
    pub homepage: Option<&'a str>,
    pub documentation: Option<&'a str>,
    pub edition: String,
    pub links: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metabuild: Option<&'a [String]>,
    pub default_run: Option<&'a str>,
    pub rust_version: Option<&'a RustVersion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hints: Option<SerializedHints<'a>>,
}

/// `[hints]` as cargo 1.101.0 prints it: every hint it knows, null when not given.
/// cargo-util-schemas 0.14.3 reads no `min-opt-level`.
#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct SerializedHints<'a> {
    pub min_opt_level: Option<&'a toml::Value>,
    pub mostly_unused: Option<&'a toml::Value>,
}

impl SerializedPackage<'_> {
    /// Has the package's `source` be `source`, as the lock file pins it: a git source with
    /// its commit. Its dependencies on packages of its own source, as a path in its
    /// repository names them, are pinned with it.
    pub fn set_source(&mut self, source: String) {
        for dep in &mut self.dependencies {
            if dep.source.is_some() && dep.source == self.source {
                dep.source = Some(source.clone());
            }
        }
        self.source = Some(source);
    }
}

/// The package of the manifest at `manifest_path` as `cargo metadata` prints it.
pub fn export_package<'a>(manifest_path: &'a Path, m: &'a Manifest) -> SerializedPackage<'a> {
    let meta = &m.metadata;
    SerializedPackage {
        name: &m.package_id.name,
        version: &m.package_id.version,
        id: m.package_id.to_spec(),
        license: meta.license.as_deref(),
        license_file: meta.license_file.as_deref(),
        description: meta.description.as_deref(),
        source: (!m.package_id.source_id.is_path())
            .then(|| m.package_id.source_id.as_url().to_string()),
        dependencies: m.dependencies.iter().map(|d| d.serialized()).collect(),
        targets: m.targets.iter().filter_map(|t| t.serialized()).collect(),
        features: m
            .features
            .iter()
            .map(|(k, v)| (k.as_str(), v.iter().map(|fv| fv.to_string()).collect()))
            .collect(),
        manifest_path,
        metadata: m.custom_metadata.as_ref(),
        publish: m.publish.as_deref(),
        authors: &meta.authors,
        categories: &meta.categories,
        keywords: &meta.keywords,
        readme: meta.readme.as_deref(),
        repository: meta.repository.as_deref(),
        homepage: meta.homepage.as_deref(),
        documentation: meta.documentation.as_deref(),
        edition: m.edition.to_string(),
        links: meta.links.as_deref(),
        metabuild: m.metabuild.as_deref(),
        default_run: m.default_run.as_deref(),
        rust_version: meta.rust_version.as_ref(),
        hints: m.hints.as_ref().map(|it: &Hints| SerializedHints {
            min_opt_level: None,
            mostly_unused: it.mostly_unused.as_ref(),
        }),
    }
}

/// What `cargo metadata --no-deps` prints for the workspace.
pub fn export_no_deps<'a, F: Files>(ws: &'a Workspace<'_, F>) -> ExportInfo<'a> {
    let packages = ws.members().map(|(path, m)| export_package(path, m)).collect();
    export(ws, packages, None)
}

/// What `cargo metadata` prints for the workspace with `packages` and `resolve`.
pub fn export<'a, R, F: Files>(
    ws: &'a Workspace<'_, F>,
    packages: Vec<SerializedPackage<'a>>,
    resolve: Option<R>,
) -> ExportInfo<'a, R> {
    ExportInfo {
        packages,
        workspace_members: ws.members().map(|(_, m)| m.package_id.to_spec()).collect(),
        workspace_default_members: ws
            .default_members()
            .map(|(_, m)| m.package_id.to_spec())
            .collect(),
        resolve,
        target_directory: ws.target_dir(),
        build_directory: ws.build_dir(),
        version: VERSION,
        workspace_root: ws.root(),
        metadata: ws.custom_metadata(),
    }
}

/// The manifest `--manifest-path` names, or the nearest `Cargo.toml` from the working
/// directory up, with cargo's messages for neither.
pub fn root_manifest<F: Files>(
    manifest_path: Option<&Path>,
    ctx: &Context<F>,
) -> anyhow::Result<PathBuf> {
    let cwd = ctx.config.cwd();
    let files = ctx.files();
    let Some(manifest_path) = manifest_path else {
        return find_root_manifest_for_wd(files, cwd);
    };
    let path = normalize_path(&cwd.join(manifest_path));
    if !files.exists(&path) {
        bail!("manifest path `{}` does not exist", manifest_path.display())
    } else if files.is_dir(&path) {
        let child_path = path.join("Cargo.toml");
        let suggested_path = if files.exists(&child_path) {
            format!("\nhelp: {} exists", child_path.display())
        } else {
            String::new()
        };
        bail!(
            "manifest path `{}` is a directory but expected a file{suggested_path}",
            manifest_path.display()
        )
    } else if !path.ends_with("Cargo.toml") && !is_embedded(&path) {
        bail!("the manifest-path must be a path to a Cargo.toml file: `{}`", path.display())
    }
    if is_embedded(&path) {
        bail!("embedded manifest `{}` requires `-Zscript`", path.display())
    }
    Ok(path)
}

fn is_embedded(path: &Path) -> bool {
    let ext = path.extension();
    ext == Some(std::ffi::OsStr::new("rs")) || ext.is_none()
}

fn find_root_manifest_for_wd<F: Files>(files: &F, cwd: &Path) -> anyhow::Result<PathBuf> {
    let mut invalid_cargo_toml_path_exists = false;
    for current in ancestors(cwd, None) {
        let manifest = current.join("Cargo.toml");
        if files.exists(&manifest) {
            return Ok(manifest);
        }
        if files.exists(&current.join("cargo.toml")) {
            invalid_cargo_toml_path_exists = true;
        }
    }
    if invalid_cargo_toml_path_exists {
        bail!(
            "could not find `Cargo.toml` in `{}` or any parent directory, but found cargo.toml please try to rename it to Cargo.toml",
            cwd.display()
        )
    }
    bail!("could not find `Cargo.toml` in `{}` or any parent directory", cwd.display())
}
