//! A file read whose length the caller already knows, in ONE trip: [`Dir::read_sized`]
//! (docs/v2/plan/design/std.md, "End state: efficiency", piece 5).
//!
//! [`Dir::read_all`] is a stat (`found`, so a missing file is `NotFound`) and then a
//! [`Effect::ReadAll`]: two trips through the effect layer a file. A caller that has the
//! file's length from a look it just made ([`Dir::looks`], [`Dir::facts_of`]: the sweep, the
//! outline's reads) asks for one [`Effect::Read`] of `len + 1` bytes at 0 and no stat: the one
//! byte past `len` is what says the file grew since the look, and fewer than `len` bytes say it
//! shrank; either is refused by name ([`SizedError`]), never read as it is now. Saved: one
//! trip a file, and the stat's system call.
//!
//! What it does not prove: a file rewritten to the same length between the look and the read
//! reads as the new bytes. The look's stamp is what proves a version; this proves the length.
//! A missing file: the native drivers fail the open (its error, by name); the simulation reads
//! a missing file as empty, so it is [`SizedError::Shrank`] there unless `len` is 0.
//!
//! The simulated twin is the simulation's own `Effect::Read` (blaze-core sim.rs): nothing new.

use core::fmt;

use blaze_core::{Done, Effect, FileName};

use super::{Dir, FsError, FsKind, FsOp, Rights, call, unexpected};
use crate::{cx::Cx, mem::Bytes, path::Path};

/// Why [`Dir::read_sized`] read nothing.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum SizedError {
    /// The file has more than the `len` bytes asked: it grew since its length was looked at.
    Grew { file: FileName, len: u64 },
    /// The file has fewer bytes than `len`: it shrank since, or is gone (the simulation's
    /// read of a missing file is empty).
    Shrank { file: FileName, len: u64, read: u64 },
    /// `len + 1` bytes do not fit in an address.
    TooLong { file: FileName, len: u64 },
    /// The driver failed the read, or the wait stopped, or `into` had no room.
    Fs(FsError),
}

impl fmt::Display for SizedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SizedError::Grew { file, len } => {
                write!(f, "bstd::fs read_sized of {file}: more than the {len} bytes asked; it grew since its length was looked at")
            }
            SizedError::Shrank { file, len, read } => {
                write!(f, "bstd::fs read_sized of {file}: {read} bytes, not the {len} asked; it shrank since its length was looked at, or is gone")
            }
            SizedError::TooLong { file, len } => write!(f, "bstd::fs read_sized of {file}: {len} bytes and one more do not fit in an address"),
            SizedError::Fs(error) => write!(f, "bstd::fs read_sized: {error}"),
        }
    }
}

impl core::error::Error for SizedError {}

impl<P: Rights> Dir<P> {
    /// The file at `path`, which the caller knows has `len` bytes, added to `into`: one
    /// [`Effect::Read`] of `len + 1` bytes at 0, no stat before it. More bytes than `len` is
    /// [`SizedError::Grew`], fewer [`SizedError::Shrank`]; then `into` is as it was. A read:
    /// any rights may ask it.
    pub fn read_sized(&self, cx: &Cx, path: &Path, len: u64, into: &mut Bytes<'_>) -> Result<usize, SizedError> {
        self.read_sized_past(cx, path, len, 1, into)
    }

    /// [`Dir::read_sized`] asking `past` bytes beyond `len`: 1, except in the test of the
    /// planted bug (0: a file that grew is read cut at `len`, and not noticed).
    pub(crate) fn read_sized_past(&self, cx: &Cx, path: &Path, len: u64, past: usize, into: &mut Bytes<'_>) -> Result<usize, SizedError> {
        let file = self.beneath(path);
        // clone: the error's name, a refcount.
        let too_long = || SizedError::TooLong { file: file.clone(), len };
        let asked = usize::try_from(len).ok().and_then(|len| len.checked_add(past)).ok_or_else(too_long)?;
        // clone: the effect's name, a refcount.
        let named = file.clone();
        let done = call(&self.io, cx, FsOp::Read, &file, |tag| Effect::Read { file: named, at: 0, len: asked, tag }).map_err(SizedError::Fs)?;
        let bytes = match done {
            Done::Read(bytes) => bytes,
            done => return Err(SizedError::Fs(unexpected(FsOp::Read, &file, done))),
        };
        let read = bytes.len() as u64;
        if read > len {
            return Err(SizedError::Grew { file, len });
        }
        if read < len {
            return Err(SizedError::Shrank { file, len, read });
        }
        into.try_extend(&bytes)
            .map_err(|stopped| SizedError::Fs(FsError::new(FsOp::Read, &file, FsKind::Stopped(stopped))))?;
        Ok(bytes.len())
    }
}
