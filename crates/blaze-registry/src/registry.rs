//! A registry: its index, read through cargo's cache of it, and where its packages are kept,
//! as cargo 1.99's `RegistrySource` and `RegistryIndex` (`src/sources/registry/mod.rs`,
//! `index/mod.rs`) have them.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    task::{Poll, Waker},
};

use anyhow::{Context as _, bail};
use blaze_hash::SharedMap;
use parking_lot::Mutex;
use semver::{Version, VersionReq};

use blaze_source::SourceId;

use crate::{
    Context,
    index::{IndexLine, Status, Summaries, dep_path},
    sparse::{LoadResponse, RegistryConfig, SparseIndex},
};

/// Where a registry's index files come from.
enum Files {
    Sparse(Box<SparseIndex>),
    Git(crate::git_index::GitIndex),
    /// A `local-registry`: `index/` and the `.crate` files beside it, on disk.
    Local {
        root: PathBuf,
    },
}

pub struct Registry {
    source: SourceId,
    /// The name of its directories under `$CARGO_HOME/registry`.
    name: String,
    files: Files,
    /// Where cargo keeps the cache of its index files: `.cache` under the index.
    cache_root: PathBuf,
    src_dir: PathBuf,
    /// Where its `.crate` files are kept: cargo's cache, or a local registry's own directory.
    crate_dir: PathBuf,
    context: Arc<Context>,
    /// The index files read since the last update, or being read.
    read: SharedMap<String, Arc<Read>>,
}

/// One index file read once for every query that asks for it while it is read.
#[derive(Default)]
struct Read {
    state: Mutex<ReadState>,
}

enum ReadState {
    /// Being read; the queries waiting for it.
    Reading(Vec<Waker>),
    Done(Result<Arc<Summaries>, String>),
}

impl Default for ReadState {
    fn default() -> ReadState {
        ReadState::Reading(Vec::new())
    }
}

impl Read {
    fn finish(&self, read: Result<Arc<Summaries>, String>) {
        let state = std::mem::replace(&mut *self.lock(), ReadState::Done(read));
        if let ReadState::Reading(waiting) = state {
            waiting.into_iter().for_each(Waker::wake);
        }
    }

    async fn wait(&self) -> anyhow::Result<Arc<Summaries>> {
        std::future::poll_fn(|cx| match &mut *self.lock() {
            ReadState::Reading(waiting) => {
                waiting.push(cx.waker().clone());
                Poll::Pending
            }
            ReadState::Done(read) => Poll::Ready(read.clone().map_err(anyhow::Error::msg)),
        })
        .await
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, ReadState> {
        self.state.lock()
    }
}

/// Ends a [`Read`] whose reader stopped before it finished, so that no query waits for it.
struct Reading<'a> {
    registry: &'a Registry,
    name: &'a str,
    read: Arc<Read>,
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        if matches!(*self.read.lock(), ReadState::Reading(_)) {
            self.registry.forget(self.name, &self.read);
            self.read.finish(Err(format!("the read of `{}`'s index file was dropped", self.name)));
        }
    }
}

impl Registry {
    /// A remote registry, whose index is sparse or git, sharing cargo's on-disk cache.
    pub fn remote(source: SourceId, context: Arc<Context>) -> anyhow::Result<Registry> {
        let name = source.registry_dir_name();
        let registry = context.cargo_home.join("registry");
        let index_dir = registry.join("index").join(&name);
        let files = if source.is_sparse() {
            Files::Sparse(Box::new(SparseIndex::new(
                source,
                index_dir.clone(),
                context.http.clone(),
                context.net.retry,
                context.net.offline,
                context.auth.clone(),
            )?))
        } else {
            Files::Git(crate::git_index::GitIndex::new(source, index_dir.clone(), context.clone()))
        };
        Ok(Registry {
            src_dir: registry.join("src").join(&name),
            crate_dir: registry.join("cache").join(&name),
            cache_root: index_dir.join(".cache"),
            files,
            source,
            name,
            context,
            read: SharedMap::default(),
        })
    }

    /// A `local-registry` at `root`, whose source is the one it replaces.
    pub fn local(source: SourceId, root: &Path, context: Arc<Context>) -> Registry {
        let name = source.registry_dir_name();
        Registry {
            src_dir: context.cargo_home.join("registry").join("src").join(&name),
            crate_dir: root.to_owned(),
            cache_root: root.join("index").join(".cache"),
            files: Files::Local { root: root.to_owned() },
            source,
            name,
            context,
            read: SharedMap::default(),
        }
    }

    pub fn source(&self) -> &SourceId {
        &self.source
    }

    pub fn dir_name(&self) -> &str {
        &self.name
    }

    /// `$CARGO_HOME/registry/src/<name>`, where its packages are unpacked.
    pub fn src_dir(&self) -> &Path {
        &self.src_dir
    }

    pub fn crate_dir(&self) -> &Path {
        &self.crate_dir
    }

    pub(crate) fn context(&self) -> &Context {
        &self.context
    }

    pub fn is_local(&self) -> bool {
        matches!(self.files, Files::Local { .. })
    }

    pub(crate) fn sparse(&self) -> Option<&SparseIndex> {
        match &self.files {
            Files::Sparse(sparse) => Some(sparse),
            Files::Git(_) | Files::Local { .. } => None,
        }
    }

    pub(crate) fn download_token(&self) -> anyhow::Result<String> {
        if let Some(sparse) = self.sparse() {
            return sparse.token(false);
        }
        let auth = self.context.auth.as_deref().context(
            "registry requires authentication, but no credential configuration was supplied",
        )?;
        let name = auth.name_of(&self.source)?;
        auth.token(
            crate::auth::Named { source: &self.source, name: name.as_deref() },
            None,
            crate::auth::Operation::Read,
            &[],
            true,
        )
    }

    /// The registry's `config.json`; a local registry has none.
    pub fn config(&self) -> anyhow::Result<Option<RegistryConfig>> {
        match &self.files {
            Files::Sparse(sparse) => sparse.config(),
            Files::Git(git) => git.config(),
            Files::Local { .. } => Ok(None),
        }
    }

    /// Whether [`Registry::config`] would ask no server: a local registry, or a sparse one
    /// whose `config.json` on disk is fresh.
    pub fn config_is_cached(&self) -> bool {
        match &self.files {
            Files::Sparse(sparse) => sparse.config_is_cached(),
            Files::Git(git) => git.config_is_cached(),
            Files::Local { .. } => true,
        }
    }

    /// Asks the server about every file from now on, once, as cargo does when it updates an
    /// index; until then what is in the cache is used as it is.
    pub fn invalidate(&self) {
        self.read.pin().clear();
        match &self.files {
            Files::Sparse(sparse) => sparse.invalidate(),
            Files::Git(git) => git.invalidate(),
            Files::Local { .. } => {}
        }
    }

    pub fn is_updated(&self) -> bool {
        match &self.files {
            Files::Sparse(sparse) => sparse.is_updated(),
            Files::Git(git) => git.is_updated(),
            Files::Local { .. } => true,
        }
    }

    /// The versions of `name` the index has, read once per update: a query that asks while
    /// another reads the file waits for that read. An error is not kept.
    pub async fn summaries(&self, name: &str) -> anyhow::Result<Arc<Summaries>> {
        let (read, reader) = {
            let open = self.read.pin();
            match open.get(name) {
                Some(read) => (read.clone(), false),
                None => match open.try_insert(name.to_owned(), Arc::new(Read::default())) {
                    Ok(read) => (read.clone(), true),
                    Err(it) => (it.current.clone(), false),
                },
            }
        };
        if !reader {
            return read.wait().await;
        }
        let reading = Reading { registry: self, name, read };
        let loaded = self.load_summaries(name).await.map(Arc::new);
        if loaded.is_err() {
            self.forget(name, &reading.read);
        }
        reading.read.finish(loaded.as_ref().map(Arc::clone).map_err(|e| format!("{e:#}")));
        loaded
    }

    /// [`Registry::summaries`] on this thread.
    pub fn summaries_blocking(&self, name: &str) -> anyhow::Result<Arc<Summaries>> {
        blaze_resolve::block_on(self.summaries(name))
    }

    fn forget(&self, name: &str, read: &Arc<Read>) {
        _ = self.read.pin().remove_if(name, |_, it| Arc::ptr_eq(it, read));
    }

    /// As cargo's `Summaries::parse`: the cache when the files say it is still good, else the
    /// file, whose cache is then written.
    async fn load_summaries(&self, name: &str) -> anyhow::Result<Summaries> {
        let lowered = name.to_lowercase();
        let relative = dep_path(&lowered, false);
        let cache_path = self.cache_root.join(&relative);
        let cached = std::fs::read(&cache_path).ok().and_then(|it| Summaries::from_cache(it).ok());
        let (cached, index_version) = match cached {
            Some((summaries, version)) => (Some(summaries), Some(version)),
            None => (None, None),
        };
        let was_updated = self.is_updated();
        let response = match &self.files {
            Files::Sparse(sparse) => sparse.load(&relative, index_version.as_deref()).await?,
            Files::Local { root } => local_load(root, &relative)?,
            Files::Git(git) => git.load(&relative, index_version.as_deref())?,
        };
        // A missing git entry can update the entire tree, including entries already read.
        // Keep this read, but ask for the others again from that snapshot next time.
        if matches!(&self.files, Files::Git(_)) && !was_updated && self.is_updated() {
            self.read.pin().retain(|it, _| it == name);
        }
        match response {
            LoadResponse::CacheValid => {
                cached.context("registry said cache valid when no cache exists")
            }
            LoadResponse::NotFound => {
                _ = std::fs::remove_file(&cache_path);
                Ok(Summaries::default())
            }
            LoadResponse::Data { raw, index_version } => {
                let summaries = Summaries::from_index_file(raw);
                if let Some(index_version) = index_version {
                    let written = cache_path
                        .parent()
                        .map_or(Ok(()), std::fs::create_dir_all)
                        .and_then(|()| {
                            crate::write_whole(&cache_path, &summaries.to_cache(&index_version))
                        });
                    if let Err(e) = written {
                        eprintln!(
                            "warning: failed to write cache, path: {}, error: {e}",
                            cache_path.display()
                        );
                    }
                }
                Ok(summaries)
            }
        }
    }

    /// The versions of `name` that match `req`, as cargo's `query_inner`: offline, those whose
    /// `.crate` is on disk when there are any, and all of them when there are none.
    pub fn query(&self, name: &str, req: &VersionReq) -> anyhow::Result<Vec<IndexLine>> {
        let summaries = self.summaries_blocking(name)?;
        let lines: Vec<IndexLine> = summaries.matching(req).collect();
        if !self.context.net.offline {
            return Ok(lines);
        }
        let on_disk: Vec<IndexLine> = lines
            .iter()
            .filter(|it| self.has_crate(&it.entry.name, &it.entry.version, &it.entry.checksum))
            .cloned()
            .collect();
        Ok(if on_disk.is_empty() { lines } else { on_disk })
    }

    /// The checksum the index has for one version, which a download is checked against.
    pub fn checksum(&self, name: &str, version: &Version) -> anyhow::Result<String> {
        let exact = VersionReq::parse(&format!("={version}")).expect("a version is a requirement");
        let summaries = self.summaries_blocking(name)?;
        let line = summaries.matching(&exact).find(|it| &it.entry.version == version);
        let line = line.with_context(|| format!("no hash listed for {name} v{version}"))?;
        if line.status == Status::Invalid || line.entry.checksum.is_empty() {
            bail!("no hash listed for {name} v{version}");
        }
        Ok(line.entry.checksum)
    }

    /// The `.crate` file of a package in cargo's cache, or a local registry's.
    pub fn crate_file(&self, name: &str, version: &Version) -> PathBuf {
        self.crate_dir.join(format!("{name}-{version}.crate"))
    }

    /// Whether a package's `.crate` is on disk: in cargo's cache (as cargo asks), or in
    /// blaze's store, from which it is unpacked as well.
    pub fn has_crate(&self, name: &str, version: &Version, checksum: &str) -> bool {
        let in_cache =
            std::fs::metadata(self.crate_file(name, version)).is_ok_and(|it| it.len() > 0);
        in_cache
            || crate::download::parse_checksum(checksum).is_some_and(|key| {
                let key = crate::download::crate_key(&key);
                self.context.store.is_some_and(|store| store.has(&key))
            })
    }
}

/// An index file of a local registry, as cargo's `LocalRegistry::load`.
fn local_load(root: &Path, relative: &str) -> anyhow::Result<LoadResponse> {
    if !root.is_dir() {
        bail!("local registry path is not a directory: {}", root.display());
    }
    let index = root.join("index");
    if !index.is_dir() {
        bail!("local registry index path is not a directory: {}", index.display());
    }
    match std::fs::read(index.join(relative)) {
        Ok(raw) => Ok(LoadResponse::Data { raw, index_version: None }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LoadResponse::NotFound),
        Err(e) => {
            Err(e).with_context(|| format!("failed to read `{}`", index.join(relative).display()))
        }
    }
}
