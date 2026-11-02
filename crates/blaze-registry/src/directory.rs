//! A directory source, as cargo 1.99's `DirectorySource` (`src/sources/directory.rs`): packages
//! vendored into subdirectories of one directory (what `cargo vendor` writes), each with the
//! sha256 of every file in its `.cargo-checksum.json`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::{Context as _, bail};
use semver::{Version, VersionReq};
use serde_derive::Deserialize;

use blaze_source::SourceId;

use crate::Package;

/// A vendored package.
#[derive(Clone, Debug)]
pub struct Vendored {
    pub name: String,
    pub version: Version,
    pub root: PathBuf,
    /// The sha256 of the `.crate` file it was unpacked from, which a registry's lock entry
    /// names; none for a package vendored from git.
    pub checksum: Option<String>,
    // BTreeMap: `verify` reports the first changed file in path order, the same on every run.
    files: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Checksums {
    package: Option<String>,
    files: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Manifest {
    package: Option<ManifestPackage>,
}

#[derive(Deserialize)]
struct ManifestPackage {
    name: String,
    version: Option<toml::Value>,
}

pub struct Directory {
    source: SourceId,
    root: PathBuf,
    packages: OnceLock<Result<Vec<Vendored>, String>>,
}

impl Directory {
    pub fn new(source: SourceId, root: PathBuf) -> Directory {
        Directory { source, root, packages: OnceLock::new() }
    }

    pub fn source(&self) -> &SourceId {
        &self.source
    }

    /// Every package of the directory: each subdirectory with a `Cargo.toml` but those whose
    /// names start with `.`, as cargo reads them.
    pub fn packages(&self) -> anyhow::Result<&[Vendored]> {
        match self.packages.get_or_init(|| read(&self.root).map_err(|err| format!("{err:#}"))) {
            Ok(packages) => Ok(packages),
            Err(err) => bail!("{err}"),
        }
    }

    pub fn query(&self, name: &str, req: &VersionReq) -> anyhow::Result<Vec<&Vendored>> {
        Ok(self
            .packages()?
            .iter()
            .filter(|it| it.name == name && req.matches(&it.version))
            .collect())
    }

    /// The directory `package` is vendored in, once every file `.cargo-checksum.json` lists has
    /// its sha256 there, and the `.crate` file it came from is the one the lock file names.
    pub fn fetch(&self, package: &Package) -> anyhow::Result<PathBuf> {
        let Some(found) = self
            .packages()?
            .iter()
            .find(|it| it.name == package.name && it.version == package.version)
        else {
            bail!("failed to find package with id: {package} ({})", self.source);
        };
        let locked = (!package.checksum.is_empty()).then_some(package.checksum.as_str());
        match (locked, found.checksum.as_deref()) {
            (Some(locked), Some(found)) if locked != found => bail!(
                "checksum for `{package}` changed between lock files\n\nthis could be indicative of a \
                 few possible errors:\n\n    * the lock file is corrupt\n    * a replacement source in \
                 use (e.g., a mirror) returned a different checksum\n    * the source itself may be \
                 corrupt in one way or another\n\nunable to verify that `{package}` is the same as when \
                 the lockfile was generated"
            ),
            (Some(_), None) => bail!(
                "checksum for `{package}` could not be calculated, but a checksum is listed in the \
                 existing lock file\n\nthis could be indicative of a few possible situations:\n\n    * \
                 the source `{}` supports checksums,\n      but was replaced with one that doesn't\n    \
                 * the lock file is corrupt\n\nunable to verify that `{package}` is the same as when the \
                 lockfile was generated",
                self.source
            ),
            _ => {}
        }
        verify(found)?;
        Ok(found.root.clone())
    }
}

fn read(root: &Path) -> anyhow::Result<Vec<Vendored>> {
    let entries = std::fs::read_dir(root)
        .with_context(|| format!("failed to read root of directory source: {}", root.display()))?;
    let mut packages = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.file_name().and_then(|it| it.to_str()).is_some_and(|it| it.starts_with('.')) {
            continue;
        }
        let manifest = path.join("Cargo.toml");
        if !manifest.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest)
            .with_context(|| format!("failed to read `{}`", manifest.display()))?;
        let parsed: Manifest = toml::from_str(&text)
            .with_context(|| format!("failed to parse manifest at `{}`", manifest.display()))?;
        let Some(package) = parsed.package else {
            bail!(
                "failed to parse manifest at `{}`: a vendored package has a [package]",
                manifest.display()
            );
        };
        let version = match &package.version {
            None => Version::new(0, 0, 0),
            Some(toml::Value::String(it)) => Version::parse(it)
                .with_context(|| format!("failed to parse manifest at `{}`", manifest.display()))?,
            Some(_) => bail!(
                "failed to parse manifest at `{}`: `package.version` of a vendored package is a version",
                manifest.display()
            ),
        };
        let name = package.name;
        let file = path.join(".cargo-checksum.json");
        let text = std::fs::read(&file).with_context(|| {
            format!("failed to load checksum `.cargo-checksum.json` of {name} v{version}")
        })?;
        let checksums: Checksums = serde_json::from_slice(&text).with_context(|| {
            format!("failed to decode `.cargo-checksum.json` of {name} v{version}")
        })?;
        packages.push(Vendored {
            name,
            version,
            root: path,
            checksum: checksums.package,
            files: checksums.files,
        });
    }
    Ok(packages)
}

/// cargo's `DirectorySource::verify`.
fn verify(package: &Vendored) -> anyhow::Result<()> {
    for (file, expected) in &package.files {
        let path = package.root.join(file);
        let bytes = std::fs::read(&path)
            .with_context(|| format!("failed to calculate checksum of: {}", path.display()))?;
        let actual: String = blaze_sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
        if &actual != expected {
            bail!(
                "the listed checksum of `{}` has changed:\nexpected: {expected}\nactual:   {actual}\n\n\
                 directory sources are not intended to be edited, if modifications are required then it \
                 is recommended that `[patch]` is used with a forked copy of the source",
                path.display()
            );
        }
    }
    Ok(())
}
