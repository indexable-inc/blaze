//! What a fetch fails with: each names what was fetched ([`Producer`]: a URL with no userinfo, a
//! repository at a commit, a step) and the pin, so the message is the whole story.

use std::{fmt, time::Duration};

use crate::{
    decisions::Failure,
    digest::{Blake3, Digest},
    tree::TreeRefused,
};

/// What made, or was to make, some bytes: said in every error about them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Producer {
    /// A download; the URL as [`redact`] leaves it.
    Url(String),
    /// A git repository's tree at a full commit id.
    Git { repo: String, rev: String },
    /// Bytes the caller already had (a `.crate` in cargo's cache), named by where they were.
    Given(String),
    /// A step of a fetch graph ([`crate::graph`]), by its name.
    Step(String),
}

impl fmt::Display for Producer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Producer::Url(url) => write!(f, "{url}"),
            Producer::Git { repo, rev } => write!(f, "{repo} at {rev}"),
            Producer::Given(what) => write!(f, "{what}"),
            Producer::Step(name) => write!(f, "the step `{name}`"),
        }
    }
}

#[derive(Debug)]
pub enum FetchError {
    /// What came is not what was pinned: nothing of it is kept. `actual` is printed whole, in
    /// the pin's algorithm, to be pasted as the pin once the bytes are trusted.
    Mismatch { from: Producer, expected: Digest, actual: Digest },
    /// Every try failed, or one failed in a way trying again does not change.
    GaveUp { from: Producer, pin: Digest, tries: u32, last: Failure },
    /// Another request of the same pin was downloading it, and did not finish within `waited`.
    WaitedTooLong { pin: Digest, waited: Duration },
    /// Another request of the same pin finished, and the store still has no object for it,
    /// `rounds` times over.
    NoLeader { pin: Digest, rounds: u32 },
    /// The process has no store to keep anything in (no daemon installed one).
    NoStore { from: Producer },
    /// The store refused or failed: a read that failed its check, a put of other bytes under a
    /// key it has, an index record that is not one.
    Store { pin: Option<Digest>, what: String },
    /// A file of the fetch's own (its temporary file, a checkout's file) could not be read or
    /// written.
    Io { what: String, error: std::io::Error },
    /// A git checkout's tree is not one a listing can hold, or a listing read back is not one.
    Tree { from: Producer, refused: TreeRefused },
    /// A git source with no git driver to fetch it.
    NoGit { from: Producer },
    /// The body went past the fetcher's limit of `most` bytes ([`crate::Fetcher::with_most`]):
    /// the transfer was stopped and nothing kept. Not tried again.
    TooBig { from: Producer, most: u64 },
    /// The bytes of `object` are not the length the index says (the store's own check passed).
    Length { object: Blake3, expected: u64, actual: u64 },
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Mismatch { from, expected, actual } => write!(
                f,
                "{from} is not the bytes pinned by {}, so nothing of it was kept\n  specified: {expected}\n  got:       {actual}",
                expected.algo().name()
            ),
            FetchError::GaveUp { from, pin, tries, last } => {
                let how = match last {
                    Failure::Transient(_) => "the last of",
                    Failure::Permanent(_) => "try",
                };
                let tries_said =
                    if *tries == 1 { "1 try".to_owned() } else { format!("{tries} tries") }; // alloc: error path
                write!(
                    f,
                    "fetching {from} (pinned {pin}) failed after {tries_said}; {how} them: {}",
                    last.said()
                )
            }
            FetchError::WaitedTooLong { pin, waited } => {
                write!(f, "another request was fetching {pin} and did not finish within {waited:?}")
            }
            FetchError::NoLeader { pin, rounds } => write!(
                f,
                "the requests fetching {pin} finished {rounds} times without the store having it"
            ),
            FetchError::NoStore { from } => write!(
                f,
                "blaze-fetch has no store to keep {from} in: the process has none installed (only \
                 the daemon has one), and fetched bytes are never handed out unkept"
            ),
            FetchError::Store { pin: Some(pin), what } => write!(f, "the store, for {pin}: {what}"),
            FetchError::Store { pin: None, what } => write!(f, "the store: {what}"),
            FetchError::Io { what, error } => write!(f, "{what}: {error}"),
            FetchError::Tree { from, refused } => write!(f, "{from}: {refused}"),
            FetchError::NoGit { from } => {
                write!(f, "{from} is a git source, and this fetcher was made with no git driver")
            }
            FetchError::TooBig { from, most } => write!(
                f,
                "{from} is longer than the limit of {most} bytes, so it was stopped and nothing of it was kept \
                 (`most_bytes` in the request raises it)"
            ),
            FetchError::Length { object, expected, actual } => {
                write!(f, "the store's {object} is {actual} bytes, and its index says {expected}")
            }
        }
    }
}

impl std::error::Error for FetchError {}

/// `url` with its userinfo (`https://user:token@host/`), its query (`?X-Amz-Signature=…`, a
/// signed URL's token) and its fragment taken out, as no message shows a credential.
pub fn redact(url: &str) -> String {
    let url = &url[..url.find(['?', '#']).unwrap_or(url.len())];
    let Some(at) = url.find("://") else { return url.to_owned() }; // alloc: a URL said in a message, once a message
    let (scheme, rest) = url.split_at(at + 3);
    let authority = rest.find('/').unwrap_or(rest.len());
    match rest[..authority].rfind('@') {
        Some(user) => format!("{scheme}{}", &rest[user + 1..]), // alloc: as above
        None => url.to_owned(),                                 // alloc: as above
    }
}

/// `text` with every URL in it ([`redact`]ed): for a message made by something that may have
/// put an address in it raw (a client library's error, git's stderr, a redirect it followed).
/// A URL runs from its scheme's `://` to the first space, quote, backtick or bracket after it.
pub fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len()); // alloc: a message, once a message
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let scheme = rest[..at]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map_or(0, |it| it + 1);
        let end = rest[at..]
            .find(|c: char| {
                c.is_whitespace()
                    || matches!(c, '\'' | '"' | '`' | '<' | '>' | '(' | ')' | '[' | ']')
            })
            .map_or(rest.len(), |it| at + it);
        out.push_str(&rest[..scheme]);
        out.push_str(&redact(&rest[scheme..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}
