//! [`SourceError`]: why a source could not be named, each naming the text, URL or path it was
//! about, in the words cargo 1.99.0 (5f94df4789) uses for the same refusal.

use std::fmt;
use std::path::PathBuf;

/// Why a [`SourceId`](crate::SourceId) or a [`CanonicalUrl`](crate::CanonicalUrl) could not be
/// made.
#[derive(Debug)]
pub enum SourceError {
    /// A source written without its kind's `kind+` prefix (`registry+`, `git+`, ...).
    NoKind { source: String },
    /// A `kind+` prefix that names no kind of source.
    UnsupportedProtocol { kind: String, source: String },
    /// A URL that does not parse.
    InvalidUrl { url: String, error: url::ParseError },
    /// A URL with no path to normalize (`mailto:x@y`).
    CannotBeABase { url: String },
    /// A directory that has no `file://` URL (a relative path).
    InvalidPathUrl { path: PathBuf },
    /// `cargo update --precise` with a version that is not SemVer.
    PreciseVersion { precise: String, error: blaze_semver::Error },
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::NoKind { source } => write!(f, "invalid source `{source}`"),
            SourceError::UnsupportedProtocol { kind, source: _ } => {
                write!(f, "unsupported source protocol: {kind}")
            }
            SourceError::InvalidUrl { url, error } => write!(f, "invalid url `{url}`: {error}"),
            SourceError::CannotBeABase { url } => {
                write!(f, "invalid url `{url}`: cannot-be-a-base-URLs are not supported")
            }
            SourceError::InvalidPathUrl { path } => {
                write!(f, "invalid path url `{}`", path.display())
            }
            SourceError::PreciseVersion { precise, error: _ } => match precise.strip_prefix('v') {
                Some(stripped) => write!(
                    f,
                    "the version provided, `{precise}` is not a valid SemVer version\n\n\
                     help: try changing the version to `{stripped}`"
                ),
                None => write!(f, "invalid version format for precise version `{precise}`"),
            },
        }
    }
}

impl core::error::Error for SourceError {
    // dyn: `core::error::Error::source`'s own signature, on the error path.
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            SourceError::InvalidUrl { error, .. } => Some(error),
            SourceError::PreciseVersion { error, .. } => Some(error),
            SourceError::NoKind { .. }
            | SourceError::UnsupportedProtocol { .. }
            | SourceError::CannotBeABase { .. }
            | SourceError::InvalidPathUrl { .. } => None,
        }
    }
}

// Callers box it into errors that cross threads.
const _: () = {
    const fn send_sync<T: Send + Sync + 'static>() {}
    send_sync::<SourceError>();
};
