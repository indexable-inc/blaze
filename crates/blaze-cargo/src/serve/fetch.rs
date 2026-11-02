//! The sources a build's load asks for ([`Fetch`]), fetched and checked as cargo fetches them
//! (docs/v2/plan/lanes/cargo/notes.md section 3): a `.crate` by its lock file's sha256 and
//! unpacked in memory with cargo's checks of its paths, a sparse index's file as served, a git
//! tree at exactly its locked commit. Called on an IO thread of the plane ([`super::work::Steps::fetch`]).

#![forbid(unsafe_code)]

mod archive;
mod git;
#[cfg(test)]
mod tests;

use std::{path::PathBuf, sync::Arc};

use blaze_api::ApiError;
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
}

impl Fetcher<HyperHttp> {
    /// A fetcher over blaze-registry's HTTP client (`HyperHttp`), with cargo's retries, doing
    /// git's work under `scratch`.
    pub fn new(scratch: PathBuf) -> Result<Fetcher<HyperHttp>, String> {
        let http = HyperHttp::shared().map_err(|e| redact(&format!("{e:#}")))?;
        Ok(Fetcher::with_http(http, scratch))
    }
}

impl<H: Http + 'static> Fetcher<H> {
    /// The same over any `Http` (tests use a fake).
    pub fn with_http(http: Arc<H>, scratch: PathBuf) -> Fetcher<H> {
        Fetcher { http, retry: Retry { quiet: true, ..Retry::default() }, scratch }
    }

    /// The same, tried again as `retry` says (cargo's `net.retry`); never said on stderr, which
    /// is the daemon's log.
    pub fn retrying(self, retry: Retry) -> Fetcher<H> {
        Fetcher { retry: Retry { quiet: true, ..retry }, ..self }
    }

    /// `fetch`, fetched and checked, as the daemon puts it into the VFS.
    pub fn fetch(&self, fetch: &Fetch) -> Result<Source, ApiError> {
        let source = match &fetch.kind {
            FetchKind::Crate { sha256 } => self.package(fetch, sha256)?,
            FetchKind::IndexFile => self.index_file(fetch)?,
            FetchKind::Git { commit } => git::tree(self, fetch, commit)?,
        };
        tracing::debug!(what = %fetch.what, files = source.files.len(), "fetched");
        Ok(source)
    }

    /// A `.crate`: downloaded, or read where a `local-registry` keeps it (a `file://` URL, as
    /// the load names one), checked by the lock file's sha256, unpacked (cargo's
    /// `RegistrySource::download`, `finish_download` and `unpack_package`, and
    /// `LocalRegistry::download`).
    fn package(&self, fetch: &Fetch, sha256: &str) -> Result<Source, ApiError> {
        let body = match fetch.url.strip_prefix("file://") {
            Some(path) => std::fs::read(path).map_err(|e| {
                failed(fetch, &[format!("failed to read `{path}`")], Some(&anyhow::Error::new(e)))
            })?,
            None => {
                let answer = self.get(&fetch.url, &[], &[]).map_err(|e| {
                    let texts = [
                        format!("failed to download `{}`", fetch.what),
                        format!("failed to download from `{}`", fetch.url),
                    ];
                    failed(fetch, &texts, Some(&e))
                })?;
                answer.body
            }
        };
        let actual = blaze_registry::download::checksum(&body);
        if !actual.eq_ignore_ascii_case(sha256) {
            return Err(failed(
                fetch,
                &[
                    format!("failed to verify the checksum of `{}`", fetch.what),
                    format!("the lock file's sha256 is `{sha256}`, the download's `{actual}`"),
                ],
                None,
            ));
        }
        let prefix = last_part(&fetch.at).ok_or_else(|| {
            failed(fetch, &[format!("`{}` names no package directory", fetch.at)], None)
        })?;
        let mut files = archive::unpack(&body, prefix).map_err(|cause| {
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
            bytes: br#"{"v":1}"#.to_vec(),
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
                vec![SourceFile { path: name.to_owned(), bytes: answer.body, mode: Mode::Regular }]
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
    ApiError { what: redact(&what), at: Some(fetch.what.clone()) }
}

/// `text` with the userinfo of every URL in it (`https://user:token@host/`) taken out, as
/// cargo's messages never show a credential.
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + "://".len());
        out.push_str(before);
        let ends = |it: char| it.is_whitespace() || "/?#`'\"<>".contains(it);
        let authority = after.find(ends).unwrap_or(after.len());
        rest = match after[..authority].rfind('@') {
            Some(user) => &after[user + 1..],
            None => after,
        };
    }
    out.push_str(rest);
    out
}
