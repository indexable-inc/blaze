//! What `blaze fetch-url '<json>'` takes: one JSON value, unknown fields refused.
//!
//! ```json
//! {"url": "https://…/x.tar.gz", "digest": "sha256:…"}
//! {"git": {"repo": "https://github.com/o/r", "rev": "<full commit id>"}, "digest": "blake3:…"}
//! {"url": "…", "digest": "…", "most_bytes": 1073741824, "out": "vendor/x"}
//! ```
//!
//! `most_bytes` bounds the download ([`crate::DEFAULT_MOST_BYTES`] when not given); `out` is a
//! directory to write the bytes out in, named by their BLAKE3. Without it they are only kept
//! in the store, and the answer is their BLAKE3.

use serde::Deserialize;

use crate::{
    digest::Digest,
    fetcher::{Source, is_commit},
};

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Json {
    url: Option<String>,
    git: Option<GitJson>,
    digest: String,
    most_bytes: Option<u64>,
    out: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct GitJson {
    repo: String,
    rev: String,
}

/// A `blaze fetch-url` request, read and checked.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Asked {
    pub source: Source,
    pub most_bytes: Option<u64>,
    /// As written: the caller makes it absolute against its own directory.
    pub out: Option<String>,
}

/// `json` as an [`Asked`], or why not: naming the field.
pub fn parse(json: &str) -> Result<Asked, String> {
    let ask: Json =
        serde_json::from_str(json).map_err(|e| format!("blaze fetch-url's request: {e}"))?; // alloc: error path
    let (repo, rev) = ask.git.map(|it| (it.repo, it.rev)).unzip();
    from_fields(ask.url, repo, rev, &ask.digest, ask.most_bytes, ask.out)
}

/// The one check of a request, wherever it arrives (the CLI's JSON, the daemon's
/// `Request::FetchUrl`): exactly one of `url` and `repo` with `rev`, `rev` a full commit id,
/// a pin of a known strong algorithm, and a `most_bytes` other than 0.
pub fn from_fields(
    url: Option<String>,
    repo: Option<String>,
    rev: Option<String>,
    digest: &str,
    most_bytes: Option<u64>,
    out: Option<String>,
) -> Result<Asked, String> {
    let refused = |why: &str| format!("blaze fetch-url's request: {why}"); // alloc: error path
    let digest = Digest::parse(digest).map_err(|e| refused(&format!("`digest`: {e}")))?; // alloc: error path
    let source = match (url, repo, rev) {
        (Some(url), None, None) => Source::Url { url, digest },
        (None, Some(repo), Some(rev)) => {
            if !is_commit(&rev) {
                return Err(refused(&format!(
                    "`git.rev` is `{rev}`, and it must be a full commit id (40 or 64 hex digits): a branch or tag moves"
                ))); // alloc: error path
            }
            Source::Git { repo, rev, digest }
        }
        (Some(_), ..) => return Err(refused("it has both `url` and `git`; it takes one")),
        (None, None, None) => return Err(refused("it has neither `url` nor `git`; it takes one")),
        (None, ..) => return Err(refused("`git` takes both `repo` and `rev`")),
    };
    if most_bytes == Some(0) {
        return Err(refused("`most_bytes` is 0, and no download fits in it"));
    }
    Ok(Asked { source, most_bytes, out })
}
