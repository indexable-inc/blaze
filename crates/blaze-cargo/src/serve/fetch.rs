//! The sources a build's load asks for ([`Fetch`]), fetched and checked as cargo fetches them
//! (docs/v2/plan.md section 3): a `.crate` got through blaze-fetch with its
//! lock file's sha256 as the pin (from the store when it has it, else downloaded, checked and
//! kept there) and unpacked in memory with cargo's checks of its paths, a sparse index's file
//! as served, a git tree at exactly its locked commit. Called on an IO thread of the plane ([`super::work::Steps::fetch`]).

#![forbid(unsafe_code)]

mod archive;
mod git;
pub mod local;
#[cfg(test)]
mod tests;

use std::{path::PathBuf, sync::Arc};

use blaze_api::ApiError;
use blaze_fetch::{FetchError, Producer, StoreOf, scrub};
use blaze_ids::{ContentId, Hasher};
use blaze_registry::http::{Http, HttpNotSuccessful, HyperHttp, Response, Retry};
use blaze_units::query::{Fetch, FetchKind};
use blaze_vfs::Mode;

use super::work::{Source, SourceFile};

/// What cargo writes into a package directory once it is unpacked whole (`{"v":1}`).
const PACKAGE_SOURCE_LOCK: &str = ".cargo-ok";

/// The answers of a sparse index that say it has no such file (cargo's `http_remote`).
const NOT_IN_INDEX: [u16; 3] = [404, 410, 451];

/// Fetches over `H`: blaze-registry's [`HyperHttp`] in the daemon, a fake in the tests.
pub struct Fetcher<H: Http> {
    http: Arc<H>,
    retry: Retry,
    /// The daemon's own directory for git's work: each fetch clones into a directory of its
    /// own under it, removed once the tree is read. Never the user's `$CARGO_HOME`, so no
    /// lock is shared with cargo or another fetch.
    scratch: PathBuf,
    /// Where a `.crate` is kept and looked up: the daemon's store as of each fetch.
    store: StoreOf,
}

impl Fetcher<HyperHttp> {
    /// A fetcher over blaze-registry's HTTP client (`HyperHttp`), with cargo's retries, doing
    /// git's work under `scratch`.
    pub fn new(scratch: PathBuf) -> Result<Fetcher<HyperHttp>, String> {
        let http = HyperHttp::shared().map_err(|e| scrub(&format!("{e:#}")))?;
        Ok(Fetcher::with_http(http, scratch))
    }
}

impl<H: Http + 'static> Fetcher<H> {
    /// The same over any `Http` (tests use a fake).
    pub fn with_http(http: Arc<H>, scratch: PathBuf) -> Fetcher<H> {
        Fetcher {
            http,
            retry: Retry { quiet: true, ..Retry::default() },
            scratch,
            store: StoreOf::Installed,
        }
    }

    /// The same, keeping `.crate` files in `store` in place of the process's (a test's own).
    pub fn keeping_in(self, store: StoreOf) -> Fetcher<H> {
        Fetcher { store, ..self }
    }

    /// The same, tried again as `retry` says (cargo's `net.retry`); never said on stderr, which
    /// is the daemon's log.
    pub fn retrying(self, retry: Retry) -> Fetcher<H> {
        Fetcher { retry: Retry { quiet: true, ..retry }, ..self }
    }

    /// blaze-fetch over this fetcher's HTTP, retries and store: what gets a `.crate` and a
    /// pinned native input ([`super::native::ensure`]) by its digest, from the store first.
    pub fn pinned(&self) -> blaze_fetch::Fetcher {
        // dyn: the I/O driver, as blaze-fetch takes it
        let http: Arc<dyn Http> = self.http.clone();
        let net = Arc::new(blaze_registry::pinned::Net::new(http));
        blaze_fetch::Fetcher::new(self.store, net, self.scratch.join("downloads"))
            .with_policy(blaze_registry::pinned::policy(&self.retry))
    }

    /// `fetch`, fetched and checked, as the daemon puts it into the VFS.
    pub fn fetch(&self, fetch: &Fetch) -> Result<Source, ApiError> {
        let source = match &fetch.kind {
            FetchKind::Crate { sha256 } => self.package(fetch, sha256)?,
            FetchKind::IndexFile => self.index_file(fetch)?,
            FetchKind::Git { commit } => git::tree(self, fetch, commit)?,
            // Read where it is, never fetched: the job reads it through `Steps::local`.
            FetchKind::Local { .. } => {
                return Err(failed(fetch, &[format!("`{}` is a path on the disk, which is read where it is, not fetched", fetch.at)], None));
            }
        };
        tracing::debug!(what = %fetch.what, files = source.files.len(), "fetched");
        Ok(source)
    }

    /// A `.crate`: from the store, else downloaded through blaze-fetch, or read where a
    /// `local-registry` keeps it (a `file://` URL, as the load names one); checked by the lock
    /// file's sha256 as the pin; unpacked (cargo's `RegistrySource::download`,
    /// `finish_download` and `unpack_package`, and `LocalRegistry::download`).
    fn package(&self, fetch: &Fetch, sha256: &str) -> Result<Source, ApiError> {
        let pin = blaze_registry::pinned::crate_pin(sha256).ok_or_else(|| {
            failed(
                fetch,
                &[format!(
                    "the lock file's checksum `{sha256}` of `{}` is not a sha256",
                    fetch.what
                )],
                None,
            )
        })?;
        let refused = |e: FetchError| {
            let first = match e {
                FetchError::Mismatch { .. } => {
                    format!("failed to verify the checksum of `{}`", fetch.what)
                }
                _ => format!("failed to download `{}`", fetch.what),
            };
            failed(fetch, &[first], Some(&anyhow::Error::new(e)))
        };
        let read;
        let kept;
        let body: &[u8] = match fetch.url.strip_prefix("file://") {
            Some(path) => {
                read = std::fs::read(path).map_err(|e| {
                    failed(
                        fetch,
                        &[format!("failed to read `{path}`")],
                        Some(&anyhow::Error::new(e)),
                    )
                })?;
                blaze_fetch::verify(Producer::Given(path.to_owned()), &pin, &read)
                    .map_err(refused)?;
                &read
            }
            None => {
                let fetcher = self.pinned();
                // clone: once a package
                let source = blaze_fetch::Source::Url { url: fetch.url.clone(), digest: pin };
                let object = fetcher.get(&source).map_err(refused)?;
                kept = fetcher.bytes(&object).map_err(refused)?;
                &kept
            }
        };
        let prefix = last_part(&fetch.at).ok_or_else(|| {
            failed(fetch, &[format!("`{}` names no package directory", fetch.at)], None)
        })?;
        let mut files = archive::unpack(body, prefix).map_err(|cause| {
            failed(fetch, &[format!("failed to unpack package `{}`", fetch.what), cause], None)
        })?;
        let under_lock = format!("{PACKAGE_SOURCE_LOCK}/");
        if files.iter().any(|it| it.path.starts_with(&under_lock)) {
            return Err(failed(
                fetch,
                &[
                    format!("failed to unpack package `{}`", fetch.what),
                    format!("failed to open `{}/{PACKAGE_SOURCE_LOCK}`", fetch.at),
                ],
                None,
            ));
        }
        files.push(SourceFile {
            path: PACKAGE_SOURCE_LOCK.to_owned(),
            bytes: blaze_vfs::Bytes::from_static(br#"{"v":1}"#),
            mode: Mode::Regular,
        });
        blaze_ids::names::sort_by_name(&mut files, |it| it.path.as_bytes());
        Ok(Source { files, under: String::new() })
    }

    /// One file of a sparse index as the server has it, with cargo's headers. A 404, 410 or 451
    /// is the index saying it has no such file: a source with the file absent, which the load
    /// reads as cargo reads `LoadResponse::NotFound` (no such package). An empty file is served
    /// and kept as it is: cargo parses it, as any file, into no versions.
    fn index_file(&self, fetch: &Fetch) -> Result<Source, ApiError> {
        let name = last_part(&fetch.at)
            .ok_or_else(|| failed(fetch, &[format!("`{}` names no index file", fetch.at)], None))?;
        let headers = [("cargo-protocol", "version=1"), ("accept", "text/plain")];
        let answer = self.get(&fetch.url, &headers, &NOT_IN_INDEX).map_err(|e| {
            failed(fetch, &[format!("failed to download from `{}`", fetch.url)], Some(&e))
        })?;
        let files = match NOT_IN_INDEX.contains(&answer.status) {
            true => Vec::new(),
            false => {
                let bytes = blaze_vfs::Bytes::copy_kept(blaze_mem::Tag::Vfs, &answer.body);
                vec![SourceFile { path: name.to_owned(), bytes, mode: Mode::Regular }]
            }
        };
        Ok(Source { files, under: name.to_owned() })
    }

    /// `url` answered 200, or one of `also`, tried again as cargo's `net.retry` tries a
    /// transfer that failed or a server error.
    fn get(&self, url: &str, headers: &[(&str, &str)], also: &[u16]) -> anyhow::Result<Response> {
        self.retry.run(|| {
            let answer = self.http.get(url, headers)?;
            if answer.status != 200 && !also.contains(&answer.status) {
                return Err(HttpNotSuccessful::new(url, &answer).into());
            }
            Ok(answer)
        })
    }
}

/// The file [`keep_on_disk`] writes last into each package directory, holding the
/// [`on_disk_id`] of what it wrote: a directory without it, or with another id, is not the
/// package its mount holds.
pub const ON_DISK_MARK: &str = ".blaze-ok";

const ON_DISK_DOMAIN: &str = "blaze v2 fetched package on disk 1";

/// What a package directory written on disk holds, by each file's `/`-separated path under it
/// (each once), mode and [`blaze_vfs::FILE_DOMAIN`] content id: the one id [`keep_on_disk`]
/// writes into [`ON_DISK_MARK`] and the daemon's start checks a recovered mount's root
/// against ([`super::mounts::Mounts::forget_unwritten`]), so the files a build script reads
/// from disk are the files the front end compiles from the mount.
pub fn on_disk_id<'a>(files: impl Iterator<Item = (&'a str, Mode, ContentId)>) -> ContentId {
    // alloc: once per package written or checked, its files sorted by path
    let mut files: Vec<(&str, Mode, ContentId)> = files.collect();
    files.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut hasher = Hasher::new(ON_DISK_DOMAIN);
    for (path, mode, id) in files {
        let mode: u8 = match mode {
            Mode::Regular => 0,
            Mode::Executable => 1,
            Mode::Symlink => 2,
        };
        hasher.part(path.as_bytes()).update(&[mode]).id(&id);
    }
    hasher.finish()
}

/// [`on_disk_id`] of a fetched source's files.
pub fn source_on_disk_id(source: &Source) -> ContentId {
    on_disk_id(
        source.files.iter().map(|it| {
            (it.path.as_str(), it.mode, ContentId::of(blaze_vfs::FILE_DOMAIN, &it.bytes))
        }),
    )
}

/// The id `dir`'s [`ON_DISK_MARK`] holds: `Ok(None)` for no mark, an error for one that is
/// not an id.
pub fn on_disk_mark(dir: &std::path::Path) -> Result<Option<ContentId>, String> {
    let mark = dir.join(ON_DISK_MARK);
    let text = match std::fs::read_to_string(&mark) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading {}: {e}", mark.display())),
    };
    let hex = text.trim_end();
    let bytes: Option<Vec<u8>> = (hex.len() == 64)
        .then(|| {
            (0..32).map(|at| u8::from_str_radix(hex.get(2 * at..2 * at + 2)?, 16).ok()).collect()
        })
        .flatten();
    let bytes: [u8; 32] = bytes
        .and_then(|it| it.try_into().ok())
        .ok_or_else(|| format!("{} holds `{hex}`, which is no content id", mark.display()))?;
    Ok(Some(ContentId(bytes)))
}

/// Why `at` is not a place [`keep_on_disk`] writes a package: it must be an absolute path with
/// no empty, `.` or `..` part, under `<sources>/registry/src/` or `<sources>/git/checkouts/`.
/// A lock file names the package (its name, a git commit), and nothing it says may move the
/// write out of blaze's sources.
fn outside_sources(at: &str, sources: &std::path::Path) -> Option<String> {
    let Some(rest) = at.strip_prefix('/') else {
        return Some("it is not an absolute path".to_owned());
    };
    if let Some(part) = rest.split('/').find(|it| matches!(*it, "" | "." | "..")) {
        return Some(format!("it has a part `{part}`, which names no directory of its own"));
    }
    let sources = sources.to_string_lossy();
    let sources = sources.trim_end_matches('/');
    let under = ["registry/src/", "git/checkouts/"];
    match under.iter().any(|it| {
        at.strip_prefix(sources)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rest| rest.starts_with(it))
    }) {
        true => None,
        false => Some(format!(
            "it is under neither {sources}/registry/src/ nor {sources}/git/checkouts/"
        )),
    }
}

/// A fetched package (a `.crate`'s files, a git tree) written on disk where it is mounted,
/// `fetch.at` under blaze's own sources directory `sources` ([`blaze_manifest::home::sources`]):
/// a build script runs in its package's directory and reads it from disk, as does the C
/// compiler it runs (`cc`'s `src/lib.c`), so the sandbox is given a directory that exists
/// without cargo ever having unpacked the package into `$CARGO_HOME`. A sparse index's file is
/// only the load's and is not written. An `at` outside `<sources>/registry/src/` and
/// `<sources>/git/checkouts/`, or with a `..` in it, is refused ([`outside_sources`]).
///
/// Written whole or not at all: into a directory of its own beside `at`, its files read-only,
/// [`ON_DISK_MARK`] last, renamed into place. A directory already at `at` is kept only when its
/// mark holds this source's [`on_disk_id`]; one with no mark or another id is an error naming
/// both, never read as the package.
pub fn keep_on_disk(
    fetch: &Fetch,
    source: &Source,
    sources: &std::path::Path,
) -> Result<(), ApiError> {
    if matches!(fetch.kind, FetchKind::IndexFile) {
        return Ok(());
    }
    let fail = |what: String| ApiError::new(what, Some(fetch.what.clone()));
    if let Some(why) = outside_sources(&fetch.at, sources) {
        return Err(fail(format!(
            "blaze will not write the package `{}` to {}: {why}",
            fetch.what, fetch.at
        )));
    }
    let at = std::path::Path::new(&fetch.at);
    let id = source_on_disk_id(source);
    if at.is_dir() {
        return same_on_disk(at, id).map_err(fail);
    }
    if source.files.iter().any(|it| it.path == ON_DISK_MARK) {
        return Err(fail(format!(
            "the package `{}` has a file `{ON_DISK_MARK}`, the name of the mark blaze writes into it on disk",
            fetch.what
        )));
    }
    let (Some(parent), Some(name)) = (at.parent(), last_part(&fetch.at)) else {
        return Err(fail(format!(
            "`{}` names no package directory to write the package into",
            fetch.at
        )));
    };
    std::fs::create_dir_all(parent)
        .map_err(|e| fail(format!("creating {}: {e}", parent.display())))?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let part = parent.join(format!(".{name}{PART}{}-{n}", std::process::id()));
    let written = write_tree(&part, &source.files)
        .and_then(|()| {
            let mark = part.join(ON_DISK_MARK);
            std::fs::write(&mark, format!("{id:x}\n"))
                .map_err(|e| format!("{}: {e}", mark.display()))
        })
        .and_then(|()| match std::fs::rename(&part, at) {
            Ok(()) => Ok(()),
            // Another fetch of the same package put it there first: kept if it is the same.
            Err(_) if at.is_dir() => {
                std::fs::remove_dir_all(&part)
                    .map_err(|e| format!("removing {}: {e}", part.display()))?;
                same_on_disk(at, id)
            }
            Err(e) => Err(format!("renaming {} to {}: {e}", part.display(), at.display())),
        });
    written.map_err(|why| {
        _ = std::fs::remove_dir_all(&part);
        fail(format!("writing the package to {}: {why}", at.display()))
    })
}

/// What a package directory being written is named, after a `.` and the package's directory
/// name: `.<name>.part-<pid>-<n>`.
const PART: &str = ".part-";

/// Whether the package directory `at` holds what `id` says, by its [`ON_DISK_MARK`].
fn same_on_disk(at: &std::path::Path, id: ContentId) -> Result<(), String> {
    match on_disk_mark(at)? {
        Some(it) if it == id => Ok(()),
        Some(it) => Err(format!(
            "{} holds the files of {it:x} by its {ON_DISK_MARK}, and the package fetched for it is \
             {id:x}: something changed it or another package was written there; delete it and \
             blaze writes it again",
            at.display()
        )),
        None => Err(format!(
            "{} is there without the {ON_DISK_MARK} blaze writes last into a package it fetched \
             ({id:x}), so it is not known to hold that package; delete it and blaze writes it again",
            at.display()
        )),
    }
}

/// The package directories a write left half done (`.<name>.part-<pid>-<n>`, beside where each
/// package goes under `<sources>/registry/src/<dir>/` and `<sources>/git/checkouts/<ident>/`),
/// removed: a daemon that stopped mid-write leaves one, which nothing reads. Returns each
/// removed; an error names the one that could not be.
pub fn sweep_parts(sources: &std::path::Path) -> Result<Vec<std::path::PathBuf>, String> {
    let mut removed = Vec::new();
    for under in ["registry/src", "git/checkouts"] {
        let Ok(groups) = std::fs::read_dir(sources.join(under)) else { continue };
        for group in groups.flatten() {
            let Ok(entries) = std::fs::read_dir(group.path()) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') && name.contains(PART) {
                    let path = entry.path();
                    std::fs::remove_dir_all(&path).map_err(|e| {
                        format!("removing the half-written package {}: {e}", path.display())
                    })?;
                    removed.push(path);
                }
            }
        }
    }
    removed.sort();
    Ok(removed)
}

/// `files` written under the new directory `dir`, each with its mode, read-only: a fetched
/// package is never edited in place (its [`ON_DISK_MARK`] would no longer hold).
fn write_tree(dir: &std::path::Path, files: &[SourceFile]) -> Result<(), String> {
    std::fs::create_dir(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    for file in files {
        let path = dir.join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        match file.mode {
            Mode::Symlink => {
                let to = String::from_utf8_lossy(&file.bytes);
                #[cfg(unix)]
                std::os::unix::fs::symlink(to.as_ref(), &path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                #[cfg(not(unix))]
                return Err(format!(
                    "{} is a symlink to {to}, and this platform has none to write",
                    path.display()
                ));
            }
            Mode::Regular | Mode::Executable => {
                std::fs::write(&path, &file.bytes)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                let mut permissions = std::fs::metadata(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))?
                    .permissions();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    permissions.set_mode(if file.mode == Mode::Executable { 0o555 } else { 0o444 });
                }
                #[cfg(not(unix))]
                permissions.set_readonly(true);
                std::fs::set_permissions(&path, permissions)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
            }
        }
    }
    Ok(())
}

/// The last `/`-separated part of an absolute path, when it is a name.
fn last_part(at: &str) -> Option<&str> {
    let (_, name) = at.rsplit_once('/')?;
    (!matches!(name, "" | "." | "..")).then_some(name)
}

/// An error as cargo prints one, after `error: `: `texts`, the first the message and each next a
/// cause, then `error`'s chain, each cause as `\n\nCaused by:\n  ` and its lines indented; about
/// `fetch.what`, with no URL's userinfo in it.
fn failed(fetch: &Fetch, texts: &[String], error: Option<&anyhow::Error>) -> ApiError {
    let causes = error.into_iter().flat_map(|it| it.chain()).map(|it| it.to_string());
    let mut what = String::new();
    for (at, text) in texts.iter().cloned().chain(causes).enumerate() {
        if at > 0 {
            what.push_str("\n\nCaused by:\n  ");
            what.push_str(&text.trim_end().replace('\n', "\n  "));
        } else {
            what.push_str(text.trim_end());
        }
    }
    ApiError::new(scrub(&what), Some(fetch.what.clone()))
}
