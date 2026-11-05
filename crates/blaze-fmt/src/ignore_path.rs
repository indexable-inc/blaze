//! rustfmt's `src/ignore_path.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0). rustfmt matches
//! the `ignore` option with the `ignore` crate's gitignore rules; those are not ported, so a
//! non-empty `ignore` list is an error rather than a silently formatted file.

use std::fmt;

use crate::config::{FileName, IgnoreList};

pub(crate) struct IgnorePathSet {
    _private: (),
}

#[derive(Debug)]
pub struct IgnoreUnsupported {
    first: String,
}

impl fmt::Display for IgnoreUnsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the `ignore` option (here `{}`) is not supported by blaze fmt yet; remove it from \
             rustfmt.toml",
            self.first
        )
    }
}

impl std::error::Error for IgnoreUnsupported {}

impl IgnorePathSet {
    pub(crate) fn from_ignore_list(ignore_list: &IgnoreList) -> Result<Self, IgnoreUnsupported> {
        if let Some(first) = ignore_list.into_iter().next() {
            return Err(IgnoreUnsupported {
                first: first.display().to_string(),
            });
        }
        Ok(IgnorePathSet { _private: () })
    }

    pub(crate) fn is_match(&self, file_name: &FileName) -> bool {
        match file_name {
            FileName::Stdin | FileName::Real(_) => false,
        }
    }
}
