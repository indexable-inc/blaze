//! A git package's tree at its locked commit, through blaze-registry's `git::fetch`: cargo's
//! database and checkout (`git/db/<ident>`, `git/checkouts/<ident>/<short id>`), fetched with
//! the `git` command as cargo runs it under `net.git-fetch-with-cli`, with the submodules cargo
//! checks out. It is cloned into a `$CARGO_HOME` of its own under the fetcher's scratch
//! directory, read whole (every file with its mode, but each `.git`: the clone's own, not the
//! tree's) and removed; the VFS mounts the tree where the load names it, cargo's
//! `$CARGO_HOME/git/checkouts/...`. Nothing is written to the user's `$CARGO_HOME` and no lock
//! is taken: the plane fetches each source once at a time, and no two fetches share a
//! directory.

#![forbid(unsafe_code)]

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context as _, bail};
use blaze_api::ApiError;
use blaze_registry::http::Http;
use blaze_source::{GitReference, SourceId};
use blaze_units::query::Fetch;
use blaze_vfs::Mode;

use super::{Fetcher, failed};
use crate::serve::work::{Source, SourceFile};

pub(super) fn tree<H: Http + 'static>(fetcher: &Fetcher<H>, fetch: &Fetch, commit: &str) -> Result<Source, ApiError> {
    let commit = commit.to_ascii_lowercase();
    let short = commit.get(..8).unwrap_or(&commit);
    let top = format!("Unable to update {}#{short}", fetch.url);
    let files = checkout(fetcher, fetch, &commit).map_err(|e| failed(fetch, &[top], Some(&e)))?;
    Ok(Source { files, under: String::new() })
}

fn checkout<H: Http + 'static>(fetcher: &Fetcher<H>, fetch: &Fetch, commit: &str) -> anyhow::Result<Vec<SourceFile>> {
    if !(matches!(commit.len(), 40 | 64) && commit.bytes().all(|it| it.is_ascii_hexdigit())) {
        bail!("the locked revision `{commit}` is not a full commit id");
    }
    let url = url::Url::parse(&fetch.url).with_context(|| format!("invalid url `{}`", fetch.url))?;
    let id = SourceId::for_git(&url, GitReference::Rev(commit.to_owned()))?;
    let ident = id.git_ident();
    let named = cargo_home(&fetch.at, &ident, commit)?;
    let short = Path::new(&fetch.at).strip_prefix(named).expect("`cargo_home` is an ancestor").to_owned();
    let home = Scratch::new(&fetcher.scratch, commit)?;
    let mut context = blaze_registry::Context::new(home.0.clone(), fetcher.http.clone());
    context.net.retry = fetcher.retry;
    let checkout = blaze_registry::git::fetch(&context, id, Some(commit))?;
    if !checkout.rev.eq_ignore_ascii_case(commit) {
        bail!("the repository gave commit `{}` for the locked `{commit}`", checkout.rev);
    }
    if checkout.root != home.0.join(&short) {
        bail!(
            "the checkout of `{commit}` is at `{}`, not at `{}` as the load names it (`{}`)",
            checkout.root.display(),
            home.0.join(&short).display(),
            fetch.at
        );
    }
    read_tree(&checkout.root)
}

/// One fetch's own `$CARGO_HOME`, removed when the fetch is over.
struct Scratch(PathBuf);

impl Scratch {
    fn new(under: &Path, commit: &str) -> anyhow::Result<Scratch> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = under.join(format!("git-{}-{}-{n}", &commit[..12], std::process::id()));
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("failed to clear `{}`", dir.display())),
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("failed to create `{}`", dir.display()))?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!(dir = %self.0.display(), %error, "a git fetch's scratch directory was not removed");
        }
    }
}

/// `$CARGO_HOME` of a checkout at `at`, `$CARGO_HOME/git/checkouts/<ident>/<short id>`, when
/// `ident` is the repository's and the short id names `commit`.
pub(super) fn cargo_home<'a>(at: &'a str, ident: &str, commit: &str) -> anyhow::Result<&'a Path> {
    let shape = || {
        format!("`{at}` is not a checkout as cargo keeps one, `$CARGO_HOME/git/checkouts/{ident}/<short id>`")
    };
    let at = Path::new(at);
    let short = at.file_name().and_then(|it| it.to_str()).with_context(shape)?;
    let named = at.parent().with_context(shape)?;
    let checkouts = named.parent().with_context(shape)?;
    let git = checkouts.parent().with_context(shape)?;
    let home = git.parent().with_context(shape)?;
    let is = |dir: &Path, name: &str| dir.file_name().is_some_and(|it| it == name);
    if !(is(named, ident) && is(checkouts, "checkouts") && is(git, "git")) || !home.is_absolute() {
        bail!(shape());
    }
    if short.len() < 7 || !commit.starts_with(&short.to_ascii_lowercase()) {
        bail!("`{at}` is not a checkout of `{commit}`", at = at.display());
    }
    Ok(home)
}

/// Every file under `root`, by its `/`-separated path, none of a `.git`.
fn read_tree(root: &Path) -> anyhow::Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    let mut dirs = vec![(root.to_owned(), String::new())];
    while let Some((dir, under)) = dirs.pop() {
        let listed =
            std::fs::read_dir(&dir).with_context(|| format!("failed to read `{}`", dir.display()))?;
        for entry in listed {
            let entry = entry.with_context(|| format!("failed to read `{}`", dir.display()))?;
            let at = entry.path();
            let name = entry.file_name();
            let name = name.to_str().with_context(|| format!("`{}` is not UTF-8", at.display()))?;
            if name == ".git" {
                continue;
            }
            let path = if under.is_empty() { name.to_owned() } else { format!("{under}/{name}") };
            let kind =
                entry.file_type().with_context(|| format!("failed to read `{}`", at.display()))?;
            if kind.is_dir() {
                dirs.push((at, path));
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&at)
                    .with_context(|| format!("failed to read `{}`", at.display()))?;
                files.push(SourceFile { path, bytes: link_bytes(target, &at)?, mode: Mode::Symlink });
            } else if kind.is_file() {
                let bytes =
                    std::fs::read(&at).with_context(|| format!("failed to read `{}`", at.display()))?;
                let metadata =
                    entry.metadata().with_context(|| format!("failed to read `{}`", at.display()))?;
                let mode = if is_executable(&metadata) { Mode::Executable } else { Mode::Regular };
                files.push(SourceFile { path, bytes, mode });
            } else {
                bail!("`{}` is neither a file, a directory nor a symlink", at.display());
            }
        }
    }
    blaze_ids::names::sort_by_name(&mut files, |it| it.path.as_bytes());
    Ok(files)
}

#[cfg(unix)]
fn link_bytes(target: std::path::PathBuf, _at: &Path) -> anyhow::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStringExt;
    Ok(target.into_os_string().into_vec())
}

#[cfg(not(unix))]
fn link_bytes(target: std::path::PathBuf, at: &Path) -> anyhow::Result<Vec<u8>> {
    let target = target.into_os_string().into_string().map_err(|_| {
        anyhow::anyhow!("the target of the symlink `{}` is not UTF-8", at.display())
    })?;
    Ok(target.replace('\\', "/").into_bytes())
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}
