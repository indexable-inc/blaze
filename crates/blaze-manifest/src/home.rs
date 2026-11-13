//! Where blaze keeps what is no checkout's own: one root for the daemon's state (`v2/`) and
//! every workspace's build directory (`build/`), so that a build writes nothing into the
//! checkout (docs/cargo.md, "Where a build writes").
//!
//! The root is `$BLAZE_HOME`, else `blaze` in `$XDG_CACHE_HOME`, else `.cache/blaze` in the
//! user's home (`blaze_core::home`: `HOME`, else the system's record of the user). A workspace
//! builds into
//! `<root>/build/<name>-<id>`, its name the last part of its root's path and its id
//! [`workspace_id`] of that path, unless `--target-dir`, `CARGO_TARGET_DIR` or
//! `build.target-dir` names a directory: tools and scripts that pass one keep it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use blaze_core::home::{NoHome, NoRecord};
use blaze_ids::{ContentId, Hasher};

/// The domain [`workspace_id`] hashes a workspace root's path in.
pub const WORKSPACE_ID_DOMAIN: &str = "blaze workspace build dir 1";

/// How many hex digits of [`workspace_id`] name a build directory: 128 bits, as many as a
/// query's fingerprint, so two roots on one machine never share one.
const ID_DIGITS: usize = 32;

/// blaze's root on this machine, from the variables `var` reads: `$BLAZE_HOME`, else
/// `$XDG_CACHE_HOME/blaze`, else `.cache/blaze` in the user's home, which is
/// [`blaze_core::home::decide`] of `var` and `record` (the system's record of the user, asked
/// only when no variable answers: a driver passes [`blaze_core::home::record`]). An empty
/// variable is unset.
///
/// # Errors
///
/// [`NoHome`] when no variable names a root and the user has no home; the caller says what
/// needed it.
pub fn root(
    var: impl Fn(&str) -> Option<OsString>,
    record: impl FnOnce() -> Result<PathBuf, NoRecord>,
) -> Result<PathBuf, NoHome> {
    let set = |name: &str| var(name).filter(|it| !it.is_empty());
    if let Some(home) = set("BLAZE_HOME") {
        return Ok(PathBuf::from(home));
    }
    if let Some(caches) = set("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(caches).join("blaze"));
    }
    Ok(blaze_core::home::decide(&var, record)?.join(".cache/blaze"))
}

/// [`root`] of this process's environment and user. A relative one (`BLAZE_HOME=.blaze`, a
/// relative `HOME`) is taken against the current directory, as cargo takes a relative
/// `CARGO_HOME`; when the current directory cannot be read that is an error naming the root
/// and why. With no root, the error says which variables name one and why the user has no
/// home.
pub fn process_root() -> Result<PathBuf, String> {
    let it = root(|name| std::env::var_os(name), blaze_core::home::record).map_err(|why| {
        // alloc: error path
        format!("no BLAZE_HOME or XDG_CACHE_HOME names blaze's root, and {why}")
    })?;
    std::path::absolute(&it).map_err(|error| {
        format!(
            "blaze's root {} is relative, and the current directory it is taken against: {error}",
            it.display()
        )
    })
}

/// Where the daemon keeps what outlives it: `v2` under [`root`].
pub fn state(root: &Path) -> PathBuf {
    root.join("v2")
}

/// Where every workspace's build directory is: `build` under [`root`]. No `v2` in it, on
/// purpose: a build directory is the user's to open and delete, not the daemon's state, and it
/// keeps its place across the daemon's formats.
pub fn builds(root: &Path) -> PathBuf {
    root.join("build")
}

/// Where the daemon keeps the packages it fetches: `sources` under [`root`], laid out as
/// cargo lays out its home (`registry/src/<registry dir>/<name>-<version>`,
/// `git/checkouts/<ident>/<commit>`), each package mounted where it is and written there
/// for what reads it from disk (a build script, the C compiler it runs). Never `$CARGO_HOME`:
/// blaze keeps no packages of cargo's there. No `v2` in it: a fetched package is
/// the same in every format of the daemon's state.
pub fn sources(root: &Path) -> PathBuf {
    root.join("sources")
}

/// The id of the workspace at `workspace_root`: the BLAKE3 of its path's bytes in
/// [`WORKSPACE_ID_DOMAIN`]. The caller names the root as the daemon does, absolute with
/// symlinks resolved (the process's real current directory), so that a client and the daemon
/// name one directory; two checkouts of one repository, worktrees among them, are two roots
/// and get two ids.
pub fn workspace_id(workspace_root: &Path) -> ContentId {
    Hasher::new(WORKSPACE_ID_DOMAIN).part(workspace_root.as_os_str().as_encoded_bytes()).finish()
}

/// The default build directory of the workspace at `workspace_root`: `<builds>/<name>-<id>`,
/// `<name>` the root's last part (to tell them apart in a listing) and `<id>` the first
/// [`ID_DIGITS`] hex digits of [`workspace_id`].
pub fn build_dir(builds: &Path, workspace_root: &Path) -> PathBuf {
    let id = format!("{:x}", workspace_id(workspace_root));
    let name = workspace_root.file_name().map(|it| it.to_string_lossy()).unwrap_or_default();
    let kept = |it: char| it.is_ascii_alphanumeric() || matches!(it, '-' | '_' | '.');
    let name: String = name.chars().map(|it| if kept(it) { it } else { '_' }).collect();
    match name.is_empty() {
        true => builds.join(&id[..ID_DIGITS]),
        false => builds.join(format!("{name}-{}", &id[..ID_DIGITS])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace builds under the global root, never inside its checkout: one directory per
    /// root path, the same each time it is asked, another for another checkout.
    #[test]
    fn a_workspace_builds_in_one_global_directory_of_its_root() {
        let builds = Path::new("/h/blaze/build");
        let root = Path::new("/src/my repo");
        let dir = build_dir(builds, root);
        assert_eq!(dir, build_dir(builds, root));
        assert_eq!(dir.parent(), Some(builds));
        assert!(!dir.starts_with(root));
        let name = dir.file_name().unwrap().to_str().unwrap();
        let (prefix, id) = name.rsplit_once('-').unwrap();
        assert_eq!(prefix, "my_repo");
        assert_eq!(id.len(), ID_DIGITS);
        assert!(id.bytes().all(|it| it.is_ascii_hexdigit()));
        assert_ne!(dir, build_dir(builds, Path::new("/src/worktrees/my repo")));
        assert_ne!(dir, build_dir(builds, Path::new("/src/my repo/sub")));
        assert_eq!(build_dir(builds, Path::new("/")).parent(), Some(builds));
    }

    #[test]
    fn the_root_is_blaze_home_then_the_cache_directory() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| OsString::from(*value))
            }
        };
        let none = || Err(NoRecord::NoEntry { uid: 7 });
        let recorded = || Ok(PathBuf::from("/recorded"));
        assert_eq!(root(env(&[("BLAZE_HOME", "/b"), ("HOME", "/u")]), none), Ok(PathBuf::from("/b")));
        let cache = env(&[("BLAZE_HOME", ""), ("XDG_CACHE_HOME", "/c"), ("HOME", "/u")]);
        assert_eq!(root(cache, none), Ok(PathBuf::from("/c/blaze")));
        assert_eq!(root(env(&[("HOME", "/u")]), none), Ok(PathBuf::from("/u/.cache/blaze")));
        // No `HOME` (a systemd unit, cron): the system's record of the user.
        assert_eq!(root(env(&[]), recorded), Ok(PathBuf::from("/recorded/.cache/blaze")));
        assert_eq!(root(env(&[]), none), Err(NoHome { record: NoRecord::NoEntry { uid: 7 } }));
        assert_eq!(state(Path::new("/b")), PathBuf::from("/b/v2"));
        assert_eq!(builds(Path::new("/b")), PathBuf::from("/b/build"));
    }
}
