//! A registry served over HTTP, read file by file as cargo 1.99's `HttpRegistry`
//! (`src/sources/registry/http_remote.rs`) reads it: `config.json`, then each package's file,
//! asked for again only with `If-None-Match` or `If-Modified-Since` once it is in the cache,
//! and only when the index is to be updated. A registry that requires authentication gets the
//! token of [`Auth`] in `Authorization` once `config.json` says so or answers 401.

use std::{
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, bail};
use blaze_hash::SharedSet;
use parking_lot::Mutex;

use blaze_source::SourceId;

use crate::{
    auth::{Auth, Named, Operation},
    http::{Http, HttpNotSuccessful, Response, Retry},
};

pub use blaze_units::index::RegistryConfig;

/// What reading an index file came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadResponse {
    /// The cache of it is up to date.
    CacheValid,
    Data {
        raw: Vec<u8>,
        /// What the cache of it is made from, to ask with next time: `etag: <value>`,
        /// `last-modified: <value>` or `Unknown`; none for an index that is files on disk.
        index_version: Option<String>,
    },
    NotFound,
}

/// The files of one sparse registry.
pub struct SparseIndex {
    source: SourceId,
    /// The index's URL without `sparse+`, ending in `/`.
    base: String,
    /// `$CARGO_HOME/registry/index/<name>`.
    dir: PathBuf,
    // dyn: the I/O driver, chosen once per process; each call is a network round trip.
    http: Arc<dyn Http>,
    retry: Retry,
    offline: bool,
    /// Whether files in the cache are to be checked with the server: cargo checks only when
    /// it updates the index (`invalidate_cache`), which it does unless every package it is
    /// asked about is locked and found.
    update: AtomicBool,
    /// The files asked for since the index was last updated.
    fresh: SharedSet<String>,
    config: Mutex<Option<RegistryConfig>>,
    auth: Option<Arc<Auth>>,
    /// The registry's name in the configuration, found once a token is first asked for.
    name: OnceLock<Option<String>>,
    /// Whether requests carry the token.
    auth_required: AtomicBool,
    /// The `login_url` of the last 401's `www-authenticate: Cargo` challenge.
    login_url: Mutex<Option<String>>,
    /// The headers of the last 401, which the providers are given.
    auth_error_headers: Mutex<Vec<String>>,
}

impl SparseIndex {
    pub fn new(
        source: SourceId,
        dir: PathBuf,
        // dyn: the I/O driver, as on the field.
        http: Arc<dyn Http>,
        retry: Retry,
        offline: bool,
        auth: Option<Arc<Auth>>,
    ) -> anyhow::Result<SparseIndex> {
        let url = source.url().as_str();
        if !url.ends_with('/') {
            bail!("sparse registry url must end in a slash `/`: {url}");
        }
        let base = url.strip_prefix("sparse+").context("a sparse registry's URL")?.to_owned();
        Ok(SparseIndex {
            source,
            base,
            dir,
            http,
            retry,
            offline,
            update: AtomicBool::new(false),
            fresh: SharedSet::default(),
            config: Mutex::default(),
            auth,
            name: OnceLock::new(),
            auth_required: AtomicBool::new(false),
            login_url: Mutex::default(),
            auth_error_headers: Mutex::default(),
        })
    }

    pub fn source(&self) -> &SourceId {
        &self.source
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// From now on, check every file with the server once.
    pub fn invalidate(&self) {
        self.fresh.pin().clear();
        self.update.store(true, Ordering::Relaxed);
    }

    pub fn is_updated(&self) -> bool {
        self.update.load(Ordering::Relaxed)
    }

    fn is_fresh(&self, path: &str) -> bool {
        !self.is_updated() || self.offline || self.fresh.pin().contains(path)
    }

    /// Whether [`Sparse::config`] would read `config.json` from disk, asking no server.
    pub fn config_is_cached(&self) -> bool {
        let path = self.dir.join(RegistryConfig::NAME);
        self.is_fresh(RegistryConfig::NAME)
            && std::fs::read(&path)
                .ok()
                .is_some_and(|it| serde_json::from_slice::<RegistryConfig>(&it).is_ok())
    }

    /// The registry's `config.json`: the copy on disk while it is fresh, else the server's,
    /// which is then written to disk. `None` when the server has none.
    pub fn config(&self) -> anyhow::Result<Option<RegistryConfig>> {
        let mut config = self.config.lock();
        if let Some(config) = &*config
            && self.is_fresh(RegistryConfig::NAME)
        {
            return Ok(Some(config.clone()));
        }
        let path = self.dir.join(RegistryConfig::NAME);
        let on_disk = || serde_json::from_slice::<RegistryConfig>(&std::fs::read(&path).ok()?).ok();
        if self.is_fresh(RegistryConfig::NAME)
            && let Some(found) = on_disk()
        {
            *config = Some(found.clone());
            return Ok(Some(found));
        }
        if on_disk().is_some_and(|it| it.auth_required) {
            self.auth_required.store(true, Ordering::Relaxed);
        }
        let fetched = match blaze_resolve::block_on(self.fetch(RegistryConfig::NAME, None)) {
            Err(e)
                if !self.auth_required.load(Ordering::Relaxed)
                    && e.downcast_ref::<HttpNotSuccessful>().is_some_and(|it| it.code == 401) =>
            {
                self.auth_required.store(true, Ordering::Relaxed);
                blaze_resolve::block_on(self.fetch(RegistryConfig::NAME, None))
            }
            fetched => fetched,
        };
        let read = match fetched? {
            LoadResponse::Data { raw, .. } => {
                let read: RegistryConfig = serde_json::from_slice(&raw)
                    .with_context(|| format!("the config.json of {}", self.base))?;
                if std::fs::create_dir_all(&self.dir).is_ok() {
                    _ = crate::write_whole(&path, &raw);
                }
                Some(read)
            }
            LoadResponse::NotFound => None,
            LoadResponse::CacheValid => bail!("config.json is never stored in the index cache"),
        };
        *config = read.clone();
        Ok(read)
    }

    /// An index file, `path` under the index's URL: [`LoadResponse::CacheValid`] when the
    /// cached copy of `index_version` is fresh or the server says it has not changed. Asked of
    /// the server, it waits without holding the thread, so that the files of a resolver's round
    /// are fetched together.
    pub async fn load(
        &self,
        path: &str,
        index_version: Option<&str>,
    ) -> anyhow::Result<LoadResponse> {
        let Some(config) = self.config()? else { return Ok(LoadResponse::NotFound) };
        if config.auth_required {
            self.auth_required.store(true, Ordering::Relaxed);
        }
        if index_version.is_some() {
            if self.is_fresh(path) {
                return Ok(LoadResponse::CacheValid);
            }
        } else if self.fresh.pin().contains(path) {
            return Ok(LoadResponse::NotFound);
        }
        let condition =
            index_version.and_then(|it| it.split_once(':')).and_then(|(key, value)| match key {
                "etag" => Some(("if-none-match", value.trim())),
                "last-modified" => Some(("if-modified-since", value.trim())),
                _ => None,
            });
        self.fetch(path, condition).await
    }

    async fn fetch(
        &self,
        path: &str,
        condition: Option<(&str, &str)>,
    ) -> anyhow::Result<LoadResponse> {
        if self.offline {
            return Ok(LoadResponse::NotFound);
        }
        let url = format!("{}{path}", self.base);
        let token = match self.auth_required.load(Ordering::Relaxed) {
            true => Some(self.token(true)?),
            false => None,
        };
        let mut headers = vec![("cargo-protocol", "version=1"), ("accept", "text/plain")];
        headers.extend(condition);
        headers.extend(token.as_deref().map(|it| ("authorization", it)));
        let answer = self
            .retry
            .run_async(&*self.http, || {
                let answer = self.http.get_async(&url, &headers);
                async {
                    let answer =
                        answer.await.with_context(|| format!("download of {path} failed"))?;
                    match answer.status {
                        200 | 304 | 404 => Ok(answer),
                        401 => Err(self.unauthorized(&url, &answer)?),
                        _ => Err(HttpNotSuccessful::new(&url, &answer).into()),
                    }
                }
            })
            .await;
        self.fresh.pin().insert(path.to_owned());
        let answer = answer?;
        Ok(match answer.status {
            200 => {
                let index_version = if let Some(etag) = answer.header("etag") {
                    format!("etag: {etag}")
                } else if let Some(modified) = answer.header("last-modified") {
                    format!("last-modified: {modified}")
                } else {
                    "Unknown".to_owned()
                };
                LoadResponse::Data { raw: answer.body, index_version: Some(index_version) }
            }
            304 => LoadResponse::CacheValid,
            _ => LoadResponse::NotFound,
        })
    }

    fn named<'a>(&'a self, auth: &Auth) -> anyhow::Result<Named<'a>> {
        let name = match self.name.get() {
            Some(name) => name,
            None => {
                let found = auth.name_of(&self.source)?;
                self.name.get_or_init(|| found)
            }
        };
        Ok(Named { source: &self.source, name: name.as_deref() })
    }

    /// The token sent to the registry: for an index file `after_401`, with what the last 401
    /// said, as cargo asks for it; for a download, with nothing of it.
    pub(crate) fn token(&self, after_401: bool) -> anyhow::Result<String> {
        let auth = self.auth.as_deref().with_context(|| {
            format!(
                "{} requires authentication, and blaze-registry was given no configuration to \
                 find its token in",
                self.base
            )
        })?;
        let registry = self.named(auth)?;
        if !after_401 {
            return auth.token(registry, None, Operation::Read, &[], true);
        }
        let login_url = self.login_url.lock().clone();
        let headers = self.auth_error_headers.lock().clone();
        auth.token(registry, login_url.as_deref(), Operation::Read, &headers, true)
    }

    /// A 401: its headers and its `Cargo` challenge's `login_url` kept for the next request,
    /// and, when a token was sent, cargo's words of a token refused over the answer.
    fn unauthorized(&self, url: &str, answer: &Response) -> anyhow::Result<anyhow::Error> {
        let headers = answer.headers.iter().map(|(name, value)| format!("{name}: {value}"));
        *self.auth_error_headers.lock() = headers.collect();
        for (_, challenges) in answer.headers.iter().filter(|(it, _)| it == "www-authenticate") {
            if let Some(found) = crate::auth::login_url(challenges)? {
                *self.login_url.lock() = Some(found);
            }
        }
        let err = anyhow::Error::from(HttpNotSuccessful::new(url, answer));
        if !self.auth_required.load(Ordering::Relaxed) {
            return Ok(err);
        }
        let auth = self.auth.as_deref().context("a token was sent")?;
        let login_url = self.login_url.lock().clone();
        Ok(err.context(auth.rejected(self.named(auth)?, login_url.as_deref())?))
    }
}
