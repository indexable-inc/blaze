//! The advisory file locks of [`Effect::Lock`] and [`Effect::Unlock`], for every native driver:
//! each takes a lock with one call that never waits (std's `File::try_lock`, `flock` on Unix and
//! `LockFileEx` on Windows), so a driver makes it inline as it is submitted. A lock lives as long
//! as its open file, which this keeps under the lock's tag until it is let go of; the process
//! ending lets every lock go, as the system does on a crash.
//!
//! [`Effect::Lock`]: crate::Effect::Lock
//! [`Effect::Unlock`]: crate::Effect::Unlock

use std::{
    alloc::Global,
    fs::{File, OpenOptions, TryLockError},
    io,
    path::Path,
};

use blaze_hash::HashMap;

use crate::{Done, FileName, IoError, LockMode, Tag, fs};

/// [`Done::Failed`] of a lock's or an unlock's error, which names what it is about.
pub(crate) fn failed(error: &io::Error) -> Done {
    // alloc: an error path.
    Done::Failed(IoError(error.to_string()))
}

/// The locks a driver holds, each by the tag of the [`crate::Effect::Lock`] that took it.
pub(crate) struct Locks {
    held: HashMap<Tag, File, Global>,
}

impl Locks {
    pub(crate) fn new() -> Locks {
        // alloc: none until the first lock; then a few entries, for the driver's life.
        Locks { held: HashMap::new_in(Global) }
    }

    /// The lock of `file` under `root` taken in `mode` without waiting, kept under `tag`:
    /// [`Done::Locked`], or [`Done::Contended`] when another holder excludes it and nothing is
    /// held. An error names the file (and the tag, when it holds a lock already).
    pub(crate) fn lock(
        &mut self,
        root: &Path,
        file: &FileName,
        mode: LockMode,
        tag: Tag,
    ) -> io::Result<Done> {
        if self.held.contains_key(&tag) {
            // alloc: an error path.
            return Err(io::Error::other(format!(
                "{file}: lock {} asked under a tag that holds a lock already",
                tag.0
            )));
        }
        let opened = blaze_mem::scratch::with_path(|path| {
            path.push(root);
            path.push(file.as_str());
            let path: &Path = path;
            let open = || {
                OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)
            };
            fs::making_parent(path, open)
        });
        // alloc: an error path.
        let named = |error: io::Error| io::Error::new(error.kind(), format!("{file}: {error}"));
        let opened = opened.map_err(named)?;
        let taken = match mode {
            LockMode::Shared => opened.try_lock_shared(),
            LockMode::Exclusive => opened.try_lock(),
        };
        match taken {
            Ok(()) => {
                // alloc: grows once to the most locks held at once.
                self.held.insert(tag, opened);
                Ok(Done::Locked)
            }
            // The file closes here, holding nothing.
            Err(TryLockError::WouldBlock) => Ok(Done::Contended),
            Err(TryLockError::Error(error)) => Err(named(error)),
        }
    }

    /// The lock `lock` took let go of, its file closed: [`Done::Unlocked`], or an error naming
    /// `lock` when it holds none.
    pub(crate) fn unlock(&mut self, lock: Tag) -> io::Result<Done> {
        match self.held.remove(&lock) {
            // Closing the file lets its lock go.
            Some(file) => {
                drop(file);
                Ok(Done::Unlocked)
            }
            // alloc: an error path.
            None => Err(io::Error::other(format!("unlock of lock {}: it holds no lock", lock.0))),
        }
    }
}
