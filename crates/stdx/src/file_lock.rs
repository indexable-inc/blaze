//! A file under an advisory lock (`flock` on unix, `LockFileEx` on Windows, through std's
//! [`File::lock`] family) that lets go of the lock when dropped, before the file closes.
//!
//! Closing a locked file is not enough to let go of its lock. A `flock` lock belongs to the open
//! file, which every copy of the descriptor shares, and a process another thread starts has a
//! copy of every descriptor from its fork until it runs its program (or, on macOS, through
//! `posix_spawn`'s copy of the table): a lock dropped while any thread of the process spawns
//! stays held by that child for a moment, or for as long as a child that never execs lives. An
//! unlock through any copy lets go of it for all of them, so [`Locked`] unlocks first.

use std::{
    fs::{File, TryLockError},
    io,
    ops::{Deref, DerefMut},
};

/// A file this process holds locked (shared or exclusive), unlocked when this drops.
///
/// The one way blaze takes a file lock: `std::fs::File`'s `lock`, `lock_shared`, `try_lock`
/// and `try_lock_shared` are disallowed everywhere else (`.clippy.toml`), so no lock is taken
/// that only a close lets go of.
#[derive(Debug)]
pub struct Locked(File);

impl Locked {
    /// `file` locked exclusively, waiting for another holder to let go.
    pub fn lock(file: File) -> io::Result<Locked> {
        #[allow(
            clippy::disallowed_methods,
            reason = "the one place a file is locked: it is unlocked as `Locked` drops"
        )]
        file.lock()?;
        Ok(Locked(file))
    }

    /// `file` locked shared, waiting for an exclusive holder to let go.
    pub fn lock_shared(file: File) -> io::Result<Locked> {
        #[allow(
            clippy::disallowed_methods,
            reason = "the one place a file is locked: it is unlocked as `Locked` drops"
        )]
        file.lock_shared()?;
        Ok(Locked(file))
    }

    /// `file` locked exclusively if no one else holds it now: `Ok(Err(file))`, the file given
    /// back unlocked, while another holds it.
    pub fn try_lock(file: File) -> io::Result<Result<Locked, File>> {
        #[allow(
            clippy::disallowed_methods,
            reason = "the one place a file is locked: it is unlocked as `Locked` drops"
        )]
        let tried = file.try_lock();
        Self::tried(file, tried)
    }

    /// `file` locked shared if no one holds it exclusively now: `Ok(Err(file))`, the file given
    /// back unlocked, while another does.
    pub fn try_lock_shared(file: File) -> io::Result<Result<Locked, File>> {
        #[allow(
            clippy::disallowed_methods,
            reason = "the one place a file is locked: it is unlocked as `Locked` drops"
        )]
        let tried = file.try_lock_shared();
        Self::tried(file, tried)
    }

    fn tried(file: File, tried: Result<(), TryLockError>) -> io::Result<Result<Locked, File>> {
        match tried {
            Ok(()) => Ok(Ok(Locked(file))),
            Err(TryLockError::WouldBlock) => Ok(Err(file)),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
}

impl Deref for Locked {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl DerefMut for Locked {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // An unlock that fails leaves the lock to the close, which is all that can be done.
        _ = self.0.unlock();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs::{File, OpenOptions};

    use super::Locked;

    /// A lock dropped while a child spawned meanwhile still has the file open (here, a copy of
    /// the descriptor kept open) is let go at once, not when that copy closes.
    #[test]
    fn a_dropped_lock_is_let_go_while_a_copy_of_its_file_is_open() {
        let path = std::env::temp_dir().join(format!("stdx-file-lock-{}", std::process::id()));
        let open = || {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .unwrap()
        };
        let file = Locked::try_lock(open()).unwrap().unwrap();
        // A copy of the descriptor shares the open file, and so its lock, as a forked child's does.
        let copy: File = file.try_clone().unwrap();
        drop(file);
        let other = Locked::try_lock(open()).unwrap();
        assert!(other.is_ok(), "the lock is let go though a copy of its file is open");
        drop(copy);
        drop(other);
        std::fs::remove_file(&path).unwrap();
        // Without the unlock, the copy would keep it: what `Locked` exists for.
        #[allow(clippy::disallowed_methods, reason = "the bare lock `Locked` exists to replace")]
        let file = {
            let file = open();
            file.try_lock().unwrap();
            file
        };
        let copy = file.try_clone().unwrap();
        drop(file);
        assert!(
            Locked::try_lock(open()).unwrap().is_err(),
            "a closed copy alone lets go of nothing"
        );
        drop(copy);
        std::fs::remove_file(&path).unwrap();
    }
}
