//! `blaze fetch-url '<json>'`: the bytes with a digest, by way of the daemon's store, which
//! downloads them only when it does not have them (blaze-fetch). Takes one JSON value, unknown
//! fields refused:
//!
//! ```text
//! blaze fetch-url '{"url": "https://…/x.tar.gz", "digest": "sha256:…"}'
//! blaze fetch-url '{"git": {"repo": "https://github.com/o/r", "rev": "<full commit>"}, "digest": "blake3:…"}'
//! ```
//!
//! and prints `blake3:<hex>`, the bytes as the store keeps them. With `"out": "<dir>"` (made
//! absolute against this directory) the daemon also writes them out there, named by their
//! BLAKE3, a read-only file for a URL and a directory for a git tree, and the path is printed
//! instead. `"most_bytes": <n>` bounds the download. A mismatch prints the digest that came, to
//! pin once the bytes are trusted.

use std::ffi::OsString;

use blaze_api::{Request, ResponseRef};
use blaze_fetch::{Asked, Source};

use super::{daemon::Start, observe::ask, say};

const USAGE: &str = "blaze fetch-url takes one JSON value: '{\"url\": \"https://...\", \"digest\": \"sha256:<hex>\"}' \
                     or '{\"git\": {\"repo\": \"https://...\", \"rev\": \"<full commit id>\"}, \"digest\": \"blake3:<hex>\"}', \
                     with optional \"most_bytes\": <n> and \"out\": \"<directory>\"";

pub fn main(words: &[OsString], color: bool) -> i32 {
    let refused = |what: &str, code: i32| {
        eprint!("{}", say::error(what, color));
        code
    };
    let json = match words {
        [one] => match one.to_str() {
            Some(it) => it,
            None => return refused("blaze fetch-url's request is not UTF-8", 2),
        },
        _ => return refused(USAGE, 2),
    };
    let Asked { source, most_bytes, out } = match blaze_fetch::request::parse(json) {
        Ok(it) => it,
        Err(why) => return refused(&format!("{why}\n{USAGE}"), 2), // alloc: error path
    };
    // The daemon's directory is not this one: `out` goes to it absolute.
    let out = match out.map(|it| std::path::absolute(&it)) {
        None => None,
        Some(Ok(at)) => match at.into_os_string().into_string() {
            Ok(at) => Some(at),
            Err(_) => return refused("blaze fetch-url: `out` made absolute is not UTF-8", 2),
        },
        Some(Err(e)) => return refused(&format!("blaze fetch-url: `out` made absolute: {e}"), 2), // alloc: error path
    };
    let request = match source {
        Source::Url { url, digest } => {
            Request::FetchUrl {
                url: Some(url),
                repo: None,
                rev: None,
                digest: digest.to_string(),
                most_bytes,
                out,
            } // alloc: once a request
        }
        Source::Git { repo, rev, digest } => {
            Request::FetchUrl {
                url: None,
                repo: Some(repo),
                rev: Some(rev),
                digest: digest.to_string(),
                most_bytes,
                out,
            } // alloc: once a request
        }
    };
    let mut stored = None;
    let asked = ask(request, Start::IfMissing, |response| {
        if let ResponseRef::Stored { path, blake3 } = response {
            stored = Some(match path {
                Some(path) => path.to_owned(), // alloc: the answer, once a request
                None => format!("blake3:{blake3}"), // alloc: as above
            });
        }
        Ok(())
    });
    match (asked, stored) {
        (Ok(()), Some(said)) => {
            println!("{said}");
            0
        }
        (Ok(()), None) => refused("blaze fetch-url: the daemon answered with nothing stored", 1),
        (Err(why), _) => refused(&format!("blaze fetch-url: {why}"), 1), // alloc: error path
    }
}
