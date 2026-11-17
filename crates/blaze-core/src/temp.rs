//! The one place blaze makes a directory under the system's temporary directory (rule
//! `blaze::tmp_path`, docs/lints.md#tmp_path): [`TempDir`], a directory of its own that is
//! removed with all it holds when it drops, so a test's home or a probe's scratch never
//! outlives what made it. Nothing else names `/tmp`, `/private/tmp`, `/var/tmp` or calls
//! `std::env::temp_dir`: a home left there leaked, and a reboot wiped a round's notes kept
//! there.

use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

/// The system's temporary directory: `TMPDIR` where it is set, else the platform's (`/tmp` on
/// Linux). Only [`TempDir`] and what makes a temporary file next to it read it.
pub fn base() -> PathBuf {
    std::env::temp_dir()
}

/// A new directory under [`base`], removed with all it holds when this drops.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// A new empty directory named `<prefix><pid>-<n>`, `n` counting the directories this
    /// process made. One left by a killed process of the same id is removed first: it is not
    /// this one's. `Err` names the directory that could not be made.
    pub fn new(prefix: &str) -> io::Result<TempDir> {
        TempDir::new_in(&base(), prefix)
    }

    /// A new directory as [`TempDir::new`] makes it, for one that will hold a Unix socket:
    /// under `/tmp` where that is a directory, else under [`base`]. A socket's whole path must
    /// fit in its address (104 bytes on macOS, 108 on Linux), which a long `TMPDIR` (macOS's
    /// `/var/folders/...`, a Nix shell's) leaves no room for.
    pub fn for_socket(prefix: &str) -> io::Result<TempDir> {
        let short = Path::new("/tmp");
        match short.is_dir() {
            true => TempDir::new_in(short, prefix),
            false => TempDir::new(prefix),
        }
    }

    /// [`TempDir::for_socket`] for a test: a failure to make it fails the test, naming the
    /// directory.
    pub fn for_test_socket(prefix: &str) -> TempDir {
        match TempDir::for_socket(prefix) {
            Ok(dir) => dir,
            Err(e) => panic!("a test's socket directory: {e}"),
        }
    }

    fn new_in(base: &Path, prefix: &str) -> io::Result<TempDir> {
        static MADE: AtomicU32 = AtomicU32::new(0);
        let n = MADE.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("{prefix}{}-{n}", std::process::id()));
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(named(&path, "could not be cleared", e)),
        }
        std::fs::create_dir_all(&path).map_err(|e| named(&path, "could not be made", e))?;
        Ok(TempDir { path })
    }

    /// A new directory as [`TempDir::new`] makes it, for a test: a failure to make it fails
    /// the test, naming the directory.
    pub fn for_test(prefix: &str) -> TempDir {
        match TempDir::new(prefix) {
            Ok(dir) => dir,
            Err(e) => panic!("a test's directory: {e}"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `path` joined to the directory.
    pub fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.path.join(path)
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<std::ffi::OsStr> for TempDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.path.as_os_str()
    }
}

/// The guard reads as the directory's path (`&*dir`, `dir.display()`), so it is passed where a
/// `&Path` is wanted and lives to the end of what made it.
impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    #[expect(
        clippy::print_stderr,
        reason = "a drop returns nothing and must not panic: a directory left behind is said on stderr, \
                  the one place blaze-core, which has no tracing, can say it"
    )]
    fn drop(&mut self) {
        // A directory already gone is what drop wants; anything else is said, not hidden, and
        // never panics a drop.
        match std::fs::remove_dir_all(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("blaze: {} could not be removed: {e}", self.path.display()),
        }
    }
}

fn named(path: &Path, what: &str, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{} {what}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant: a `TempDir` is a new empty directory of its own, and is gone, with what
    /// it held, once dropped.
    #[test]
    fn a_dir_is_fresh_and_gone_once_dropped() {
        let first = TempDir::for_test("blaze-core-temp-");
        let second = TempDir::for_test("blaze-core-temp-");
        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir());
        assert_eq!(std::fs::read_dir(first.path()).unwrap().count(), 0);
        std::fs::create_dir_all(first.join("a/b")).unwrap();
        std::fs::write(first.join("a/b/c"), b"x").unwrap();
        let path = first.path().to_path_buf();
        drop(first);
        assert!(!path.exists(), "{} outlived its TempDir", path.display());
        assert!(second.path().is_dir());
    }
}
