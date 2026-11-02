//! Which blaze a workspace pins: `version` under `[workspace.metadata.blaze]` in the root
//! manifest, a release's version (`0.YYYYMMDD.N`). The `blaze` the user runs runs that
//! release when it is not that release itself, from `~/.blaze/versions/<version>`, fetched
//! once when it is not there (`versions.rs`), as Bazelisk runs the Bazel a workspace names.
//! A project pins blaze, and so the Rust blaze ships; Rust's own toolchain files are not read
//! (docs/toolchain.md).
//!
//! Asked before every command (`program.rs`), so it stays small and quick: std and `toml` only.

use std::path::{Path, PathBuf};

/// The variable the launcher sets for the blaze it runs: the manifest that pinned it. A blaze
/// started with it is the pinned one, and runs no other.
pub(crate) const PINNED_BY: &str = "BLAZE_PINNED_BY";

/// This blaze's version, as a pin names it: a release's `0.YYYYMMDD.N`, or `0.0.0-dev` for a
/// build of the tree, which no pin names (build.rs, from `BLAZE_RELEASE`).
pub(crate) const OURS: &str = env!("BLAZE_VERSION");

/// Where a command run with `args` in `cwd` starts looking for its workspace: the directory
/// of `--manifest-path`, else `cwd`.
pub(crate) fn start(args: &[String], cwd: &Path) -> PathBuf {
    let mut words = args.iter().take_while(|it| *it != "--");
    while let Some(word) = words.next() {
        let named = match word.strip_prefix("--manifest-path") {
            Some("") => words.next().map(String::as_str),
            Some(rest) => rest.strip_prefix('='),
            None => None,
        };
        if let Some(manifest) = named {
            let manifest = cwd.join(manifest);
            return manifest.parent().map_or_else(|| cwd.to_owned(), Path::to_owned);
        }
    }
    cwd.to_owned()
}

/// Why which blaze a workspace pins can't be told: blaze stops rather than run itself in the
/// pinned one's place.
#[derive(Debug)]
pub(crate) enum PinError {
    /// A manifest on the way to the workspace's root that is there and can't be read.
    Unread { manifest: PathBuf, error: std::io::Error },
    /// A manifest on the way to the workspace's root that is not TOML.
    NotToml { manifest: PathBuf, error: toml::de::Error },
    /// A pin that names neither a version nor a channel.
    NotAVersion { manifest: PathBuf, pinned: toml::Value },
    /// The working directory, where the workspace is looked for, can't be read.
    NoWorkingDirectory(std::io::Error),
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinError::Unread { manifest, error } => write!(
                f,
                "reading {}, which may pin the blaze that runs here: {error}",
                manifest.display()
            ),
            PinError::NotToml { manifest, error } => write!(
                f,
                "{} is not TOML, so which blaze it pins can't be told: {error}",
                manifest.display()
            ),
            PinError::NotAVersion { manifest, pinned } => write!(
                f,
                "{} pins blaze to {pinned} under `[workspace.metadata.blaze]`, which is not a \
                 version of blaze (`0.YYYYMMDD.N`, as `blaze pin` writes it) or a channel \
                 (\"stable\", \"canary\")",
                manifest.display()
            ),
            PinError::NoWorkingDirectory(error) => write!(
                f,
                "the working directory, whose workspace may pin the blaze that runs here, can't \
                 be read: {error}"
            ),
        }
    }
}

impl std::error::Error for PinError {}

/// The text of `manifest`, `None` when there is none.
fn manifest_text(manifest: &Path) -> Result<Option<String>, PinError> {
    match std::fs::read_to_string(manifest) {
        Ok(text) => Ok(Some(text)),
        Err(error)
            if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) =>
        {
            Ok(None)
        }
        Err(error) => Err(PinError::Unread { manifest: manifest.to_owned(), error }),
    }
}

/// The root manifest of the workspace `dir` is in, as cargo finds it: the nearest
/// `Cargo.toml` that has a `[workspace]`, or the one its `package.workspace` names, or else
/// the nearest `Cargo.toml` itself. `None` outside any package. A manifest on the way that
/// can't be read is an error, as it is to cargo.
pub(crate) fn root_manifest(dir: &Path) -> Result<Option<PathBuf>, PinError> {
    let mut nearest = None;
    for dir in dir.ancestors() {
        let manifest = dir.join("Cargo.toml");
        let Some(text) = manifest_text(&manifest)? else { continue };
        let table = toml::from_str::<toml::Table>(&text)
            .map_err(|error| PinError::NotToml { manifest: manifest.clone(), error })?;
        if table.contains_key("workspace") {
            return Ok(Some(manifest));
        }
        let named = table.get("package").and_then(|it| it.get("workspace"));
        if let Some(root) = named.and_then(toml::Value::as_str) {
            return Ok(Some(dir.join(root).join("Cargo.toml")));
        }
        nearest.get_or_insert(manifest);
    }
    Ok(nearest)
}

/// The version of blaze `manifest` pins, `None` when it pins none or there is no manifest.
/// `Err` is a manifest that can't be read, or a pin that is not a version.
pub(crate) fn pinned(manifest: &Path) -> Result<Option<String>, PinError> {
    let Some(text) = manifest_text(manifest)? else { return Ok(None) };
    let table = toml::from_str::<toml::Table>(&text)
        .map_err(|error| PinError::NotToml { manifest: manifest.to_owned(), error })?;
    let blaze =
        table.get("workspace").and_then(|it| it.get("metadata")).and_then(|it| it.get("blaze"));
    let Some(version) = blaze.and_then(|it| it.get("version")) else { return Ok(None) };
    match version.as_str() {
        Some(version) if is_version(version) || is_channel(version) => Ok(Some(version.to_owned())),
        _ => Err(PinError::NotAVersion { manifest: manifest.to_owned(), pinned: version.clone() }),
    }
}

/// Whether a pin names a channel (docs/release.md): `stable`, the releases that passed the heavy
/// tests, or `canary`, the newest release. It runs the release the channel names, read again
/// once [`CHANNEL_FRESH`] has passed.
pub(crate) fn is_channel(name: &str) -> bool {
    name == "stable" || name == "canary"
}

/// How long the version a channel named is the one a pin to it runs, before it is read again.
pub(crate) const CHANNEL_FRESH: std::time::Duration = std::time::Duration::from_secs(3600);

/// Where blaze keeps the version `channel` named when it was last read: `channels/<channel>`
/// beside [`versions`].
pub(crate) fn channel_file(channel: &str) -> Option<PathBuf> {
    Some(versions()?.parent()?.join("channels").join(channel))
}

/// The version `file` (a [`channel_file`]) holds, and whether it was read within
/// [`CHANNEL_FRESH`]: one dated in the future, whose age can't be told, is not. `None` when
/// none is kept, which has the channel read again.
pub(crate) fn channel_known(file: &Path) -> Option<(String, bool)> {
    let version = std::fs::read_to_string(file).ok()?.trim().to_owned();
    if !is_version(&version) {
        return None;
    }
    let age = std::fs::metadata(file).ok()?.modified().ok()?.elapsed();
    Some((version, age.is_ok_and(|age| age < CHANNEL_FRESH)))
}

/// Whether `version` is a release's version: numbers with dots between, so that it names a
/// directory and an asset and nothing else.
pub(crate) fn is_version(version: &str) -> bool {
    !version.is_empty()
        && version.split('.').all(|it| !it.is_empty() && it.bytes().all(|b| b.is_ascii_digit()))
}

/// Where blaze keeps the releases a pin ran: `versions` in `BLAZE_HOME`, else in `.blaze` in
/// the user's home.
pub(crate) fn versions() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("BLAZE_HOME").filter(|it| !it.is_empty()) {
        return Some(PathBuf::from(home).join("versions"));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join(".blaze").join("versions"))
}

/// The `blaze` of `version` where [`versions`] keeps it.
pub(crate) fn installed(version: &str) -> Option<PathBuf> {
    Some(versions()?.join(version).join(format!("blaze{}", std::env::consts::EXE_SUFFIX)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_read() {
        assert!(is_version("0.20261027.0"));
        assert!(!is_version("0.20261027."));
        assert!(!is_version("../0.1"));
        assert!(!is_version("nightly"));
        assert!(!is_version(""));
    }

    #[test]
    fn the_root_manifest_holds_the_pin() {
        let root = std::env::temp_dir().join(format!("blaze-pin-{}", std::process::id()));
        let member = root.join("crates/a");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\"]\n\n[workspace.metadata.blaze]\nversion = \"0.20261027.0\"\n",
        )
        .unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
        let found = root_manifest(&member.join("src")).unwrap().unwrap();
        assert_eq!(found, root.join("Cargo.toml"));
        assert_eq!(pinned(&found).unwrap().as_deref(), Some("0.20261027.0"));
        assert_eq!(pinned(&member.join("Cargo.toml")).unwrap(), None);
        assert_eq!(pinned(&root.join("none/Cargo.toml")).unwrap(), None);
        std::fs::write(root.join("Cargo.toml"), "[workspace.metadata.blaze]\nversion = 3\n")
            .unwrap();
        assert!(matches!(pinned(&found), Err(PinError::NotAVersion { .. })));
        // A manifest that can't be read pins what can't be told: no blaze runs in its place.
        std::fs::write(root.join("Cargo.toml"), "[workspace\n").unwrap();
        assert!(matches!(pinned(&found), Err(PinError::NotToml { .. })));
        assert!(matches!(root_manifest(&member), Err(PinError::NotToml { .. })));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_pin_names_a_channel() {
        let root = std::env::temp_dir().join(format!("blaze-pin-channel-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let manifest = root.join("Cargo.toml");
        for (pin, read) in
            [("stable", Ok(Some("stable"))), ("canary", Ok(Some("canary"))), ("nightly", Err(()))]
        {
            std::fs::write(&manifest, format!("[workspace.metadata.blaze]\nversion = \"{pin}\"\n"))
                .unwrap();
            let got = pinned(&manifest);
            assert_eq!(got.as_ref().map(|it| it.as_deref()).map_err(|_| ()), read, "{pin}");
        }
        let file = root.join("channels/canary");
        assert_eq!(channel_known(&file), None);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "0.20261028.1\n").unwrap();
        assert_eq!(channel_known(&file), Some(("0.20261028.1".to_owned(), true)));
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(86_400);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(future).unwrap();
        assert_eq!(channel_known(&file), Some(("0.20261028.1".to_owned(), false)), "dated ahead");
        std::fs::write(&file, "../x\n").unwrap();
        assert_eq!(channel_known(&file), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_manifest_path_starts_the_search() {
        let cwd = Path::new("/w");
        let args = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(start(&args("build --manifest-path x/Cargo.toml"), cwd), cwd.join("x"));
        assert_eq!(start(&args("build --manifest-path=/y/Cargo.toml"), cwd), Path::new("/y"));
        assert_eq!(start(&args("run -- --manifest-path z/Cargo.toml"), cwd), cwd);
    }
}
