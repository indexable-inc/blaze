//! A folder made the user's alone ([`Dir::make_private`]), over blaze-core's `Effect::Private`:
//! made with its parents where missing (on Unix mode 0o700), then it and everything under it
//! checked to be the user's with no one else able to write it, and on Unix the group's and
//! others' bits taken off it. What is refused is never mended: the caller is told what and why
//! ([`PrivateError::NotAlone`]) and stops (rule 3).
//!
//! Its first user is the journal helper's socket folder (blaze-watch helper.rs
//! `SOCKET_FOLDER`): only this user can make an entry in a folder that is theirs alone, so a
//! socket found there was made by this user, and only this user can reach it to connect.

use core::fmt;

use blaze_core::{Access, Done, Effect, FileName};

use super::{Dir, FsError, FsOp, ReadWrite, call, unexpected};
use crate::{cx::Cx, path::Path};

/// Why [`Dir::make_private`] did not give a folder that is the user's alone.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum PrivateError {
    /// The folder could not be made or looked at.
    Fs(FsError),
    /// `at` (the folder, or a file or folder under it) is not the user's alone, as `access`
    /// says. Nothing was mended.
    NotAlone { at: FileName, access: Access },
}

impl fmt::Display for PrivateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrivateError::Fs(error) => write!(f, "{error}"),
            PrivateError::NotAlone { at, access } => write!(
                f,
                "bstd::fs make_private: {at} {access}, so the folder is not the user's alone; nothing was changed \
                 (make it the user's, or remove it, and run again)"
            ),
        }
    }
}

impl core::error::Error for PrivateError {}

impl Dir<ReadWrite> {
    /// The folder `path` beneath this one made the user's alone (the module's doc), and given
    /// with this one's rights.
    pub fn make_private(&self, cx: &Cx, path: &Path) -> Result<Dir<ReadWrite>, PrivateError> {
        let name = self.beneath(path);
        // clone: the effect's name, a refcount.
        let dir = name.clone();
        match call(&self.io, cx, FsOp::Private, &name, |tag| Effect::Private { dir, tag }).map_err(PrivateError::Fs)? {
            Done::Private(None) => Ok(self.subdir(path)),
            Done::Private(Some((at, access))) => Err(PrivateError::NotAlone { at, access }),
            done => Err(PrivateError::Fs(unexpected(FsOp::Private, &name, done))),
        }
    }
}
