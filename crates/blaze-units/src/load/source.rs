//! Where cargo has each package the resolve pins, and the registries' indexes the resolve reads
//! (docs/v2/plan.md section 3): every file read through the view's mounts,
//! never the disk or the network. What is not mounted yet is a [`Fetch`] the daemon makes and
//! mounts where cargo keeps it; offline, it is cargo's error.
//!
//! - `[source]` replacement is cargo 1.99's `SourceConfigMap` (`src/sources/config.rs`):
//!   [`crate::sources::SourceMap`], made of blaze-manifest's reading of the configuration.
//! - Fetched packages are where blaze keeps them, `<sources>` ([`crate::query::BuildConfig::sources`],
//!   under blaze's root), laid out as cargo lays out its home, and never in `$CARGO_HOME`: a
//!   registry package is its `.crate` unpacked at `<sources>/registry/src/<dir>/<name>-<version>`
//!   (cargo's `RegistrySource`); a sparse index file is at
//!   `<sources>/registry/index/<dir>/.cache/<prefix path>`, as served; a git package is in its
//!   checkout at `<sources>/git/checkouts/<ident>/<commit>` (cargo's `GitSource`, with the whole
//!   commit where cargo takes a short one). The
//!   daemon writes each package it fetches there on disk too, for a build script to read.
//! - A `local-registry` and a `directory` source are read in place. A git index is not read:
//!   blaze reads sparse indexes only.

mod registry;
#[cfg(all(test, unix))]
mod tests;
mod tree;

use anyhow::{anyhow, bail};
use blaze_manifest::Config;
use blaze_manifest::files::Files;
use blaze_query::{Cancelled, Cx};
use blaze_resolve::{CRATES_IO_REGISTRY, PackageId, SourceId, SourceKind};

use crate::index::{self, RegistryConfig, crate_url, index_path};
use crate::load::files::{Found, normal, read_dir, read_file};
use crate::load::load_error;
use crate::query::{Fetch, FetchKind, LoadError};
use crate::sources::SourceMap;

pub(crate) use self::registry::Index;

/// The sources a build reads packages from, as its configuration names them.
pub(crate) struct Sources {
    map: SourceMap,
    /// Where fetched packages are kept ([`crate::query::BuildConfig::sources`]), absolute,
    /// `/`-separated.
    kept: String,
    offline: Option<&'static str>,
}

/// Where a package is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Located {
    /// The package's `Cargo.toml`, absolute, under a mount.
    Here {
        manifest: String,
    },
    Needs(Fetch),
    Failed(LoadError),
    /// A package of a sparse registry or a git source that blaze's sources don't hold, which
    /// this offline build may not fetch: cargo's error, as a download's ([`unfetched`]). One
    /// run that may fetch has it, unlike every [`Located::Failed`].
    Unfetched(LoadError),
}

/// A registry's index file, as read.
pub(crate) enum IndexFile {
    Here(Vec<u8>),
    /// The registry has no such file (or, offline, it is not fetched).
    Absent,
    Needs(Fetch),
    Failed(anyhow::Error),
}

impl Sources {
    /// From the configuration the workspace was read with: `[source]` replacements,
    /// `[registries]`, `registry.index`, `registries.crates-io.protocol` (sparse by default).
    /// `kept` is where the daemon keeps fetched packages ([`crate::query::BuildConfig::sources`]).
    /// `offline` is `Some("--offline")` or `Some("--frozen")` when the command line or
    /// `net.offline` makes the build offline.
    pub(crate) fn new<F: Files>(
        config: &Config<F>,
        kept: &str,
        offline: Option<&'static str>,
    ) -> anyhow::Result<Sources> {
        let kept = normal(std::path::Path::new(kept)).ok_or_else(|| {
            anyhow!("the daemon names `{kept}`, no absolute directory, for where it keeps the packages it fetches")
        })?;
        Ok(Sources { map: source_map(config)?, kept, offline })
    }

    /// A package the resolve pins that is not a path package: where cargo has its `Cargo.toml`,
    /// or what to fetch first.
    pub(crate) fn locate(
        &self,
        cx: &mut Cx<'_>,
        id: PackageId,
        checksum: Option<&str>,
    ) -> Result<Located, Cancelled> {
        let source = id.source_id();
        let at = Some(format!("{} v{}", id.name(), id.version()));
        let failed = |error: anyhow::Error| Located::Failed(load_error(&error, at.clone()));
        if source.is_path() {
            return Ok(failed(anyhow!("`{id}` is a path package, read where it is")));
        }
        let from = match self.map.replacement(source) {
            Ok(from) => from,
            Err(error) => return Ok(failed(error)),
        };
        Ok(match from.kind() {
            SourceKind::SparseRegistry | SourceKind::LocalRegistry => {
                match self.registry_package(cx, id, from, checksum)? {
                    Ok(located) => located,
                    Err(error) => failed(download_error(id, error)),
                }
            }
            SourceKind::Registry => failed(git_index(from)),
            SourceKind::Git(_) => match self.git_package(cx, id, from)? {
                Ok(located) => located,
                Err(error) => failed(error),
            },
            SourceKind::Directory => {
                let Some(root) = local_path(from) else {
                    return Ok(failed(anyhow!("{from} is not a directory blaze can name")));
                };
                match tree::vendored_package(cx, &root, &from.to_string(), id, checksum)? {
                    Ok(manifest) => Located::Here { manifest },
                    Err(error) if from != source => failed(
                        error.context(format!("failed to download replaced source {source}")),
                    ),
                    Err(error) => failed(error),
                }
            }
            SourceKind::Path => {
                failed(anyhow!("{source} is replaced with the path {from}, which is not a source"))
            }
        })
    }

    /// A package of a sparse or local registry: its directory unpacked, or the `.crate` to
    /// fetch, checked by the lock file's checksum or else its index's.
    fn registry_package(
        &self,
        cx: &mut Cx<'_>,
        id: PackageId,
        from: SourceId,
        checksum: Option<&str>,
    ) -> Result<anyhow::Result<Located>, Cancelled> {
        let name = id.name().as_str();
        let version = id.version().to_string();
        // The name is a directory's name on disk, where the daemon writes the package before any
        // sandbox runs: a lock file's `../..` never gets that far (cargo refuses such a name
        // when it parses a manifest; a lock file's is checked here).
        if let Err(error) = blaze_resolve::validate_package_name(name) {
            return Ok(Err(error.context(format!(
                "the lock file names the package `{name} v{version}`, whose name cargo refuses, so \
                 blaze makes no directory of it"
            ))));
        }
        let at = self.package_dir(from, name, &version);
        if let Found::Here(_) = read_dir(cx, &at)? {
            return Ok(Ok(Located::Here { manifest: format!("{at}/Cargo.toml") }));
        }
        if let Some(flag) = self.offline
            && from.kind() != &SourceKind::LocalRegistry
        {
            return Ok(Ok(unfetched(id, flag)));
        }
        let sha256 = match checksum.filter(|it| !it.is_empty()) {
            Some(it) => it.to_owned(),
            None => {
                let no_hash = || anyhow!("no hash listed for {id}");
                match self.index_file(cx, from, name)? {
                    IndexFile::Here(bytes) => {
                        let lines = index::Lines::new(bytes);
                        match lines.matching(|it| it == id.version()).next() {
                            Some(line)
                                if line.status != index::Status::Invalid
                                    && !line.entry.checksum.is_empty() =>
                            {
                                // clone: the one checksum the download needs, out of the file's lines.
                                line.entry.checksum.clone()
                            }
                            _ => return Ok(Err(no_hash())),
                        }
                    }
                    IndexFile::Absent => return Ok(Err(no_hash())),
                    IndexFile::Needs(fetch) => return Ok(Ok(Located::Needs(fetch))),
                    IndexFile::Failed(error) => return Ok(Err(error)),
                }
            }
        };
        let url = if from.kind() == &SourceKind::LocalRegistry {
            let Some(root) = local_path(from) else {
                return Ok(Err(anyhow!("{from} is not a directory blaze can name")));
            };
            let file = format!("{root}/{name}-{version}.crate");
            match read_file(cx, &file)? {
                Found::Here(_) => format!("file://{file}"),
                Found::Absent => return Ok(Err(anyhow!("failed to read `{file}`"))),
                Found::Unmounted => return Ok(Err(unmounted_local(&root))),
            }
        } else if from.is_crates_io() {
            format!("https://static.crates.io/crates/{name}/{version}/download")
        } else {
            let base = match sparse_base(from) {
                Ok(base) => base,
                Err(error) => return Ok(Err(error)),
            };
            let config_path = format!("{}/config.json", self.index_dir(from));
            let config = match read_file(cx, &config_path)? {
                Found::Here(bytes) => match serde_json::from_slice::<RegistryConfig>(&bytes) {
                    Ok(config) => config,
                    Err(error) => {
                        return Ok(Err(anyhow::Error::new(error)
                            .context("failed to parse registry's config.json")));
                    }
                },
                Found::Absent => return Ok(Err(anyhow!("config.json not found"))),
                Found::Unmounted => {
                    return Ok(Ok(Located::Needs(Fetch {
                        what: from.display_index(),
                        url: format!("{base}config.json"),
                        kind: FetchKind::IndexFile,
                        at: config_path,
                    })));
                }
            };
            if config.auth_required {
                return Ok(Err(anyhow!(
                    "{} requires authentication (`auth-required` in its config.json), and blaze \
                     sends no registry token yet",
                    from.display_index()
                )));
            }
            crate_url(&config.dl, name, &version, &sha256)
        };
        Ok(Ok(Located::Needs(Fetch {
            what: format!("{name} v{version}"),
            url,
            kind: FetchKind::Crate { sha256 },
            at,
        })))
    }

    /// A package of a git source: in the checkout of the commit the lock file pins.
    fn git_package(
        &self,
        cx: &mut Cx<'_>,
        id: PackageId,
        from: SourceId,
    ) -> Result<anyhow::Result<Located>, Cancelled> {
        let source = id.source_id();
        let Some(commit) = source.precise_git_fragment() else {
            return Ok(Err(anyhow!(
                "blaze reads the git source {source} only at the commit Cargo.lock pins, and it \
                 pins none for `{} v{}`",
                id.name(),
                id.version()
            )));
        };
        // The whole commit names its checkout, never a prefix: two commits of one repository
        // that share a prefix are two directories. It is a directory's name on disk, so a lock
        // file's fragment that is no object id (`../..`) is refused.
        if commit.len() < 40 || !commit.bytes().all(|it| it.is_ascii_hexdigit()) {
            return Ok(Err(anyhow!(
                "Cargo.lock pins `{} v{}` to `{commit}` of the git source {source}, which is no \
                 git object id (40 or 64 hex digits)",
                id.name(),
                id.version()
            )));
        }
        let at = format!("{}/git/checkouts/{}/{commit}", self.kept, from.git_ident());
        if let Found::Here(_) = read_dir(cx, &at)? {
            return Ok(
                tree::checkout_package(cx, &at, id)?.map(|manifest| Located::Here { manifest })
            );
        }
        if let Some(flag) = self.offline {
            return Ok(Ok(unfetched(id, flag)));
        }
        Ok(Ok(Located::Needs(Fetch {
            what: format!("git repository `{}`", from.url()),
            url: from.url().to_string(),
            kind: FetchKind::Git { commit: commit.to_owned() },
            at,
        })))
    }

    /// The index file of `name` in the registry `from` (a registry after `[source]`
    /// replacement): a sparse index's where cargo keeps it, a local registry's in place.
    pub(crate) fn index_file(
        &self,
        cx: &mut Cx<'_>,
        from: SourceId,
        name: &str,
    ) -> Result<IndexFile, Cancelled> {
        let relative = index_path(name);
        Ok(match from.kind() {
            SourceKind::SparseRegistry => {
                let base = match sparse_base(from) {
                    Ok(base) => base,
                    Err(error) => return Ok(IndexFile::Failed(error)),
                };
                let at = format!("{}/.cache/{relative}", self.index_dir(from));
                match read_file(cx, &at)? {
                    Found::Here(bytes) => IndexFile::Here(bytes),
                    Found::Absent => IndexFile::Absent,
                    Found::Unmounted if self.offline.is_some() => IndexFile::Absent,
                    Found::Unmounted => IndexFile::Needs(Fetch {
                        what: from.display_index(),
                        url: format!("{base}{relative}"),
                        kind: FetchKind::IndexFile,
                        at,
                    }),
                }
            }
            SourceKind::LocalRegistry => {
                let Some(root) = local_path(from) else {
                    return Ok(IndexFile::Failed(anyhow!(
                        "{from} is not a directory blaze can name"
                    )));
                };
                match read_dir(cx, &root)? {
                    Found::Here(_) => {}
                    Found::Absent => {
                        return Ok(IndexFile::Failed(anyhow!(
                            "local registry path is not a directory: {root}"
                        )));
                    }
                    Found::Unmounted => return Ok(IndexFile::Failed(unmounted_local(&root))),
                }
                let index = format!("{root}/index");
                if !matches!(read_dir(cx, &index)?, Found::Here(_)) {
                    return Ok(IndexFile::Failed(anyhow!(
                        "local registry index path is not a directory: {index}"
                    )));
                }
                match read_file(cx, &format!("{index}/{relative}"))? {
                    Found::Here(bytes) => IndexFile::Here(bytes),
                    Found::Absent => IndexFile::Absent,
                    Found::Unmounted => IndexFile::Failed(unmounted_local(&root)),
                }
            }
            SourceKind::Registry => IndexFile::Failed(git_index(from)),
            _ => IndexFile::Failed(anyhow!("{from} has no index")),
        })
    }

    /// Whether a registry package is fetched, which offline is what cargo's
    /// `is_crate_downloaded` asks: a local registry's always are.
    pub(crate) fn is_downloaded(
        &self,
        cx: &mut Cx<'_>,
        from: SourceId,
        name: &str,
        version: &semver::Version,
    ) -> Result<bool, Cancelled> {
        if from.kind() == &SourceKind::LocalRegistry {
            return Ok(true);
        }
        let at = self.package_dir(from, name, &version.to_string());
        Ok(matches!(read_dir(cx, &at)?, Found::Here(_)))
    }

    pub(crate) fn offline(&self) -> Option<&'static str> {
        self.offline
    }

    /// Where fetched packages are kept ([`crate::query::BuildConfig::sources`]), as errors say
    /// where blaze looked.
    pub(crate) fn kept(&self) -> &str {
        &self.kept
    }

    /// The registry `id`'s packages come from, when `id` is a registry and what replaces it is
    /// one too.
    pub(crate) fn registry_of(&self, id: SourceId) -> anyhow::Result<Option<SourceId>> {
        self.map.registry_of(id)
    }

    /// As cargo's `ReplacedSource::describe`: the source read, and the one it replaces unless
    /// that is crates.io read over its sparse index.
    pub(crate) fn describe(&self, source: SourceId) -> String {
        self.map.describe(source)
    }

    pub(crate) fn is_replaced(&self, source: SourceId) -> bool {
        self.map.is_replaced(source)
    }

    /// `<sources>/registry/src/<dir>/<name>-<version>`.
    fn package_dir(&self, from: SourceId, name: &str, version: &str) -> String {
        format!("{}/registry/src/{}/{name}-{version}", self.kept, from.registry_dir_name())
    }

    /// `<sources>/registry/index/<dir>`.
    fn index_dir(&self, from: SourceId) -> String {
        format!("{}/registry/index/{}", self.kept, from.registry_dir_name())
    }
}

/// cargo's error for a package it could not fetch.
fn download_error(id: PackageId, error: anyhow::Error) -> anyhow::Error {
    error.context(format!("failed to download `{} v{}`", id.name(), id.version()))
}

/// `id`, which blaze's sources don't hold and an offline build (`flag`) may not fetch, as
/// cargo says it: a download refused.
fn unfetched(id: PackageId, flag: &str) -> Located {
    let error =
        download_error(id, anyhow!("attempting to make an HTTP request, but {flag} was specified"));
    Located::Unfetched(load_error(&error, Some(format!("{} v{}", id.name(), id.version()))))
}

fn git_index(from: SourceId) -> anyhow::Error {
    anyhow!(
        "blaze reads sparse registry indexes only, and {from} has a git index (`{}`): read it \
         over its sparse index (a `sparse+` index URL, or `registries.crates-io.protocol = \
         \"sparse\"` for crates.io)",
        from.url()
    )
}

fn unmounted_local(root: &str) -> anyhow::Error {
    anyhow!(
        "blaze reads the local registry `{root}` in place, and it is under no mount of the view"
    )
}

/// A sparse index's URL without `sparse+`, ending in `/`, as cargo's `HttpRegistry::new` takes
/// it.
fn sparse_base(from: SourceId) -> anyhow::Result<String> {
    let url = from.url().as_str();
    if !from.url().path().ends_with('/') {
        bail!("sparse registry url must end in a slash `/`: {url}");
    }
    Ok(url.strip_prefix("sparse+").unwrap_or(url).to_owned())
}

/// The directory of a `local-registry` or `directory` source, as the load names paths.
fn local_path(id: SourceId) -> Option<String> {
    normal(&blaze_source::url_file(id.url()).ok()?)
}

/// The sources `config` names and which replaces which, as cargo's `SourceConfigMap::new`
/// makes them: each `[source]` table, then each `[registries]` name a `replace-with` gives.
fn source_map<F: Files>(config: &Config<F>) -> anyhow::Result<SourceMap> {
    let crates_io = config.crates_io_maybe_sparse_http()?;
    let mut map = SourceMap::new(crates_io);
    for (name, def) in config.source_configs()? {
        if name == CRATES_IO_REGISTRY && def.replace_with.is_none() {
            continue;
        }
        let id = match name.as_str() {
            CRATES_IO_REGISTRY => crates_io,
            _ => def.id,
        };
        map.add(&name, id, def.replace_with)?;
    }
    for name in map.unknown_replacements() {
        if let Ok(id) = blaze_manifest::source::alt_registry(config, &name) {
            map.add_registry(name, id);
        }
    }
    Ok(map)
}
