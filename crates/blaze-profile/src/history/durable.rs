//! Making a write of the history survive a power loss, the one place it is done: every file
//! is synced to stable storage under its hidden name before it is renamed into place
//! ([`sync`]), and the directory is synced around each rename of the manifest ([`sync_dir`]):
//! before it, so every file the manifest lists has its name on disk first; after it, so the
//! commit itself is on disk before anything that follows it (a reap's deletes, a recovered
//! mapping's or crash record's delete) can be.
//!
//! Until 2026-11-04 nothing was synced: a power loss could leave `manifest.json` torn (the
//! history then refused to open, and `blaze serve` with it) or list a merge's output whose
//! bytes never reached the disk while its inputs were already reaped.
//!
//! On macOS `fsync` only hands the bytes to the drive, whose cache may lose or reorder them;
//! `F_FULLFSYNC` asks the drive to write its cache out, which is what stable means here. On
//! other unix systems `fsync` is that already. On Windows a rename is journaled by NTFS and a
//! directory can't be opened to sync, so [`sync_dir`] has nothing to do there.

use std::{fs::File, io, path::Path};

/// Syncs `file`'s bytes and size to stable storage: `F_FULLFSYNC` on macOS, `fsync`
/// elsewhere.
pub(crate) fn sync(file: &File) -> io::Result<()> {
    let _span = tracing::info_span!(target: "blaze", "profile fsync").entered();
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd as _;
        // SAFETY: `F_FULLFSYNC` on a descriptor `file` keeps open takes no argument; it
        // returns -1 and sets errno on failure.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        file.sync_all()
    }
}

/// Syncs the directory `dir`'s entries (the renames into it, the files made and deleted in
/// it) to stable storage.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        sync(&File::open(dir)?)
    }
    #[cfg(not(unix))]
    {
        _ = dir;
        Ok(())
    }
}
