//! `cargo metadata --no-deps --format-version 1`: the workspace's members as cargo prints them,
//! in its field order (`src/ops/cargo_metadata.rs` and `src/workspace/package.rs` there).

use std::alloc::Global;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use blaze_data::Value;
use blaze_resolve::{PackageIdSpec, RustVersion};
use blaze_semver::Version;
use serde::Serialize;

use crate::dependency::SerializedDependency;
use crate::error::ManifestError;
use crate::files::Files;
use crate::manifest::{Context, Manifest};
use crate::paths::{ancestors, normalize_path};
use crate::schema::Hints;
use crate::targets::SerializedTarget;
use crate::workspace::Workspace;

const VERSION: u32 = 1;

/// Why no manifest could be chosen for `cargo metadata`, in cargo's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataError {
    /// `--manifest-path` names nothing.
    NoManifestPath { path: PathBuf },
    /// `--manifest-path` names a directory; `child` is its `Cargo.toml` when that exists.
    ManifestPathIsDir { path: PathBuf, child: Option<PathBuf> },
    /// `--manifest-path` names a file other than `Cargo.toml`.
    NotCargoToml { path: PathBuf },
    /// `--manifest-path` names a script.
    Embedded { path: PathBuf },
    /// No `Cargo.toml` from `cwd` up; `lowercase` when a `cargo.toml` was seen.
    NotFound { cwd: PathBuf, lowercase: bool },
}

impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetadataError::NoManifestPath { path } => {
                write!(f, "manifest path `{}` does not exist", path.display())
            }
            MetadataError::ManifestPathIsDir { path, child } => {
                write!(f, "manifest path `{}` is a directory but expected a file", path.display())?;
                if let Some(child) = child {
                    write!(f, "\nhelp: {} exists", child.display())?;
                }
                Ok(())
            }
            MetadataError::NotCargoToml { path } => write!(
                f,
                "the manifest-path must be a path to a Cargo.toml file: `{}`",
                path.display()
            ),
            MetadataError::Embedded { path } => {
                write!(f, "embedded manifest `{}` requires `-Zscript`", path.display())
            }
            MetadataError::NotFound { cwd, lowercase } => {
                write!(f, "could not find `Cargo.toml` in `{}` or any parent directory", cwd.display())?;
                if *lowercase {
                    f.write_str(", but found cargo.toml please try to rename it to Cargo.toml")?;
                }
                Ok(())
            }
        }
    }
}

impl core::error::Error for MetadataError {}

/// A value serialized as its `Display`: a version, a `rust-version`.
pub struct Shown<'a, T: fmt::Display>(pub &'a T);

impl<T: fmt::Display> Serialize for Shown<'_, T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self.0)
    }
}

/// A manifest's unread value (`metadata`, a hint) as JSON: maps in manifest order.
pub struct Data<'a>(pub &'a Value<Global>);

impl Serialize for Data<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self.0 {
            Value::Null => s.serialize_unit(),
            Value::Bool(b) => s.serialize_bool(*b),
            Value::Int(i) => match (i.as_u64(), i.as_i64()) {
                (Some(u), _) => s.serialize_u64(u),
                (None, Some(n)) => s.serialize_i64(n),
                (None, None) => s.serialize_i128(i.as_i128()),
            },
            Value::Float(x) => s.serialize_f64(*x),
            Value::Text(t) => s.serialize_str(t.as_str()),
            Value::Bytes(b) => s.serialize_bytes(b),
            Value::List(list) => {
                let mut seq = s.serialize_seq(Some(list.len()))?;
                for it in list.iter() {
                    seq.serialize_element(&Data(it))?;
                }
                seq.end()
            }
            Value::Map(map) => {
                let mut out = s.serialize_map(Some(map.len()))?;
                for (k, v) in map.iter() {
                    out.serialize_entry(k, &Data(v))?;
                }
                out.end()
            }
        }
    }
}

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
    metadata: Option<Data<'a>>,
}

/// A package as `cargo metadata` prints it.
#[derive(Serialize)]
pub struct SerializedPackage<'a> {
    pub name: &'a str,
    pub version: Shown<'a, Version<Global>>,
    pub id: PackageIdSpec,
    pub license: Option<&'a str>,
    pub license_file: Option<&'a str>,
    pub description: Option<&'a str>,
    pub source: Option<String>,
    pub dependencies: Vec<SerializedDependency<'a>>,
    pub targets: Vec<SerializedTarget<'a>>,
    pub features: BTreeMap<&'a str, Vec<String>>,
    pub manifest_path: &'a Path,
    pub metadata: Option<Data<'a>>,
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
    pub rust_version: Option<Shown<'a, RustVersion>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hints: Option<SerializedHints<'a>>,
}

/// `[hints]` as cargo 1.101.0 prints it: every hint it knows, null when not given.
/// cargo-util-schemas 0.14.3 reads no `min-opt-level`.
#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct SerializedHints<'a> {
    pub min_opt_level: Option<Data<'a>>,
    pub mostly_unused: Option<Data<'a>>,
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
        version: Shown(&m.package_id.version),
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
        metadata: m.custom_metadata.as_ref().map(Data),
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
        rust_version: meta.rust_version.as_ref().map(Shown),
        hints: m.hints.as_ref().map(|it: &Hints| SerializedHints {
            min_opt_level: None,
            mostly_unused: it.mostly_unused.as_ref().map(Data),
        }),
    }
}

/// What `cargo metadata --no-deps` prints for the workspace.
pub fn export_no_deps<'a, F: Files>(
    ws: &'a Workspace<'_, F>,
) -> Result<ExportInfo<'a>, ManifestError> {
    let packages = ws.members().map(|(path, m)| export_package(path, m)).collect();
    export(ws, packages, None)
}

/// What `cargo metadata` prints for the workspace with `packages` and `resolve`. Its
/// `target_directory` and `build_directory` are where blaze builds, under blaze's root unless
/// the configuration names one ([`Workspace::target_dir`]), so editors and rust-analyzer look
/// there and not in the checkout.
pub fn export<'a, R, F: Files>(
    ws: &'a Workspace<'_, F>,
    packages: Vec<SerializedPackage<'a>>,
    resolve: Option<R>,
) -> Result<ExportInfo<'a, R>, ManifestError> {
    Ok(ExportInfo {
        packages,
        workspace_members: ws.members().map(|(_, m)| m.package_id.to_spec()).collect(),
        workspace_default_members: ws
            .default_members()
            .map(|(_, m)| m.package_id.to_spec())
            .collect(),
        resolve,
        target_directory: ws.target_dir()?,
        build_directory: ws.build_dir()?,
        version: VERSION,
        workspace_root: ws.root(),
        metadata: ws.custom_metadata().map(Data),
    })
}

/// The manifest `--manifest-path` names, or the nearest `Cargo.toml` from the working
/// directory up, with cargo's messages for neither.
pub fn root_manifest<F: Files>(
    manifest_path: Option<&Path>,
    ctx: &Context<F>,
) -> Result<PathBuf, MetadataError> {
    let cwd = ctx.config.cwd();
    let files = ctx.files();
    let Some(manifest_path) = manifest_path else {
        return find_root_manifest_for_wd(files, cwd);
    };
    let path = normalize_path(&cwd.join(manifest_path));
    if !files.exists(&path) {
        return Err(MetadataError::NoManifestPath { path: manifest_path.to_owned() });
    } else if files.is_dir(&path) {
        let child_path = path.join("Cargo.toml");
        let child = files.exists(&child_path).then_some(child_path);
        return Err(MetadataError::ManifestPathIsDir { path: manifest_path.to_owned(), child });
    } else if !path.ends_with("Cargo.toml") && !is_embedded(&path) {
        return Err(MetadataError::NotCargoToml { path });
    }
    if is_embedded(&path) {
        return Err(MetadataError::Embedded { path });
    }
    Ok(path)
}

fn is_embedded(path: &Path) -> bool {
    let ext = path.extension();
    ext == Some(std::ffi::OsStr::new("rs")) || ext.is_none()
}

fn find_root_manifest_for_wd<F: Files>(files: &F, cwd: &Path) -> Result<PathBuf, MetadataError> {
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
    Err(MetadataError::NotFound { cwd: cwd.to_owned(), lowercase: invalid_cargo_toml_path_exists })
}
