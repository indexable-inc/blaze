//! Registries read and sources fetched as cargo 1.99 reads and fetches them, with no cargo:
//! the sparse index with cargo's cache of it, `.crate` files checked by their sha256, kept in
//! blaze's store by that hash and unpacked where cargo unpacks them, `[source]` replacement,
//! vendored directories and git checkouts, so that the paths a build sees (`file!()`, panics,
//! `CARGO_MANIFEST_DIR`) are cargo's and cargo and blaze share `$CARGO_HOME`. What is ported
//! from where is said at the top of each module; the plan is `docs/self-contained.md`.

pub mod api;
pub mod auth;
pub mod directory;
pub mod download;
pub mod git;
mod git_index;
pub mod http;
pub mod index;
pub mod query;
pub mod registry;
pub mod sources;
pub mod sparse;

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use blaze_source::SourceId;

pub use download::Package;
pub use query::IndexRegistry;
pub use registry::Registry;
pub use sources::{Locked, SourceConfig, SourceDef, SourceMap, Sources};

/// A remote registry named by its index URL alone: sparse when it starts with `sparse+`, else
/// a git index.
pub fn registry_source(url: &str) -> anyhow::Result<SourceId> {
    SourceId::for_registry(&url::Url::parse(url).with_context(|| format!("`{url}` is not a URL"))?)
}

/// The directory of a path, directory or local-registry source.
pub(crate) fn path_of(id: SourceId) -> anyhow::Result<PathBuf> {
    id.url().to_file_path().map_err(|()| anyhow::anyhow!("{} is not a directory", id.url()))
}

/// What fetching reads of cargo's configuration (`[net]`, `[http]`, `--offline`), as a plain
/// struct: `crates/blaze-manifest` fills it from `.cargo/config.toml`, `CARGO_*` variables and
/// flags.
#[derive(Clone, Copy, Debug)]
pub struct NetConfig {
    /// `net.offline` or `--offline` (which `--frozen` implies): nothing is asked of a server,
    /// and only what is on disk is used.
    pub offline: bool,
    /// `net.retry`.
    pub retry: http::Retry,
    /// How many downloads run at once.
    pub jobs: usize,
}

impl Default for NetConfig {
    fn default() -> NetConfig {
        NetConfig { offline: false, retry: http::Retry::default(), jobs: 16 }
    }
}

/// What every registry of one fetch shares.
pub struct Context {
    /// `$CARGO_HOME`, as cargo finds it: the `CARGO_HOME` variable, else `~/.cargo`.
    pub cargo_home: PathBuf,
    pub net: NetConfig,
    // dyn: the I/O driver, chosen once per process; each call is a network round trip.
    pub http: Arc<dyn http::Http>,
    /// blaze's store, which keeps `.crate` files by their sha256.
    pub store: Option<&'static blaze_cas::Handle>,
    /// Whether a `.crate` is also written to cargo's cache,
    /// `$CARGO_HOME/registry/cache/<registry>/`, so that cargo finds it there offline.
    pub keep_crate_files: bool,
    /// Where a registry that requires authentication gets its token; with none, such a registry
    /// is refused.
    pub auth: Option<Arc<auth::Auth>>,
}

impl Context {
    // dyn: the I/O driver, as on `Context::http`.
    pub fn new(cargo_home: PathBuf, http: Arc<dyn http::Http>) -> Context {
        Context {
            cargo_home,
            net: NetConfig::default(),
            http,
            store: None,
            keep_crate_files: true,
            auth: None,
        }
    }
}

/// `$CARGO_HOME` as cargo finds it.
pub fn cargo_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME").filter(|it| !it.is_empty()) {
        let home = PathBuf::from(home);
        return Some(if home.is_absolute() {
            home
        } else {
            std::env::current_dir().ok()?.join(home)
        });
    }
    std::env::home_dir().map(|it| it.join(".cargo"))
}

/// cargo's lock on its package cache (`$CARGO_HOME/.package-cache`, the lock of
/// `CacheLockMode::DownloadExclusive`), held while the index cache, `.crate` files and
/// unpacked sources are written, so that cargo and blaze do not write them at once.
pub struct PackageCacheLock {
    _file: File,
}

impl PackageCacheLock {
    pub fn acquire(cargo_home: &Path) -> anyhow::Result<PackageCacheLock> {
        use anyhow::Context as _;
        std::fs::create_dir_all(cargo_home)
            .with_context(|| format!("failed to create `{}`", cargo_home.display()))?;
        let path = cargo_home.join(".package-cache");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open `{}`", path.display()))?;
        if file.try_lock().is_err() {
            eprintln!("    Blocking waiting for file lock on package cache");
            file.lock().with_context(|| format!("failed to lock `{}`", path.display()))?;
        }
        Ok(PackageCacheLock { _file: file })
    }
}

/// Writes a file under a temporary name and renames it into place, so that a reader finds
/// all of it or none.
pub(crate) fn write_whole(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path.file_name().and_then(|it| it.to_str()).unwrap_or("file");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let temporary = path.with_file_name(format!(".{name}.{}-{nanos}.tmp", std::process::id()));
    let written = File::create(&temporary).and_then(|mut file| file.write_all(bytes));
    match written.and_then(|()| std::fs::rename(&temporary, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            _ = std::fs::remove_file(&temporary);
            Err(e)
        }
    }
}
