//! Git registry indexes, read from the fetched tree without a checkout, as cargo's
//! `sources/registry/git_remote.rs` does. Cargo's `.cache` uses the blob's id as its version;
//! old caches that use the current commit are also valid. A fetch failure is an error, never
//! permission to answer from an older tree.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, bail};
use blaze_source::SourceId;
use parking_lot::Mutex;

use crate::{
    Context, git,
    sparse::{LoadResponse, RegistryConfig},
};

pub(crate) struct GitIndex {
    source: SourceId,
    db: PathBuf,
    context: Arc<Context>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    head: Option<String>,
    needs_update: bool,
    updated: bool,
}

impl GitIndex {
    pub(crate) fn new(source: SourceId, dir: PathBuf, context: Arc<Context>) -> GitIndex {
        GitIndex { source, db: dir.join(".git"), context, state: Mutex::default() }
    }

    pub(crate) fn invalidate(&self) {
        self.state.lock().needs_update = true;
    }

    pub(crate) fn is_updated(&self) -> bool {
        self.state.lock().updated
    }

    pub(crate) fn config_is_cached(&self) -> bool {
        let state = self.state.lock();
        (!state.needs_update || state.updated || self.context.net.offline)
            && self.db.join("HEAD").exists()
    }

    pub(crate) fn config(&self) -> anyhow::Result<Option<RegistryConfig>> {
        match self.load(RegistryConfig::NAME, None)? {
            LoadResponse::Data { raw, .. } => serde_json::from_slice(&raw)
                .with_context(|| format!("the config.json of {}", self.source.url()))
                .map(Some),
            LoadResponse::NotFound => Ok(None),
            LoadResponse::CacheValid => unreachable!("config.json is read without a cache version"),
        }
    }

    fn update(&self, state: &mut State) -> anyhow::Result<()> {
        if state.updated {
            return Ok(());
        }
        if !self.context.net.offline {
            if !self.context.net.retry.quiet {
                eprintln!("    Updating `{}` index", self.source.url());
            }
            git::fetch_registry(&self.context, self.source.url().as_str(), &self.db)
                .with_context(|| format!("failed to fetch `{}`", self.source.url()))?;
            state.head = None;
        }
        state.needs_update = false;
        state.updated = true;
        Ok(())
    }

    pub(crate) fn load(&self, path: &str, version: Option<&str>) -> anyhow::Result<LoadResponse> {
        let mut state = self.state.lock();
        if state.needs_update || !self.db.join("HEAD").exists() {
            self.update(&mut state)?;
        }
        if !self.db.join("HEAD").exists() {
            return Ok(LoadResponse::NotFound);
        }
        if state.head.is_none() {
            // An empty index has no remote HEAD yet. Its first read fetches it once.
            let mut head = self.head()?;
            if head.is_none() && !state.updated {
                self.update(&mut state)?;
                head = self.head()?;
            }
            state.head = head;
        }
        let Some(head) = state.head.clone() else { return Ok(LoadResponse::NotFound) };
        if version == Some(head.as_str()) {
            return Ok(LoadResponse::CacheValid);
        }
        let mut entry = self.entry(&head, path)?;
        if entry.is_none() && !state.updated {
            self.update(&mut state)?;
            state.head = self.head()?;
            if let Some(head) = &state.head {
                entry = self.entry(head, path)?;
            }
        }
        let Some(blob) = entry else { return Ok(LoadResponse::NotFound) };
        if version == Some(blob.as_str()) {
            return Ok(LoadResponse::CacheValid);
        }
        Ok(LoadResponse::Data {
            raw: self.run(&["cat-file", "blob", &blob])?,
            index_version: Some(blob),
        })
    }

    fn head(&self) -> anyhow::Result<Option<String>> {
        let raw =
            self.run(&["for-each-ref", "--format=%(objectname)", "refs/remotes/origin/HEAD"])?;
        let head = std::str::from_utf8(&raw)?.trim();
        Ok((!head.is_empty()).then(|| head.to_owned()))
    }

    fn entry(&self, head: &str, path: &str) -> anyhow::Result<Option<String>> {
        let raw = self.run(&["ls-tree", "-z", head, "--", path])?;
        if raw.is_empty() {
            return Ok(None);
        }
        let text = std::str::from_utf8(&raw)?;
        let (meta, _) = text.split_once('\t').context("git ls-tree returned no path")?;
        let mut fields = meta.split_whitespace();
        let (_, kind, oid) = (fields.next(), fields.next(), fields.next());
        if kind != Some("blob") {
            bail!("path `{path}` is not a blob in the git index of {}", self.source.url());
        }
        Ok(Some(oid.context("git ls-tree returned no object id")?.to_owned()))
    }

    fn run(&self, args: &[&str]) -> anyhow::Result<Vec<u8>> {
        let output =
            git::registry_command(&self.db).args(args).output().context("could not run `git`")?;
        if !output.status.success() {
            bail!(
                "git {} in {} failed ({}): {}",
                args.join(" "),
                self.db.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim_end()
            );
        }
        Ok(output.stdout)
    }
}
