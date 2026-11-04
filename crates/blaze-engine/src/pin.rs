//! Which blaze a workspace pins: `version` under `[workspace.metadata.blaze]` in the root
//! manifest, a release's version (`0.YYYYMMDD.N`). The `blaze` the user runs runs that
//! release when it is not that release itself, from `~/.blaze/versions/<version>`, fetched
//! once when it is not there (`versions.rs`), as Bazelisk runs the Bazel a workspace names.
//! A project pins blaze, and so the Rust blaze ships; Rust's own toolchain files are not read
//! (docs/toolchain.md).
//!
//! A build of the source tree (`0.0.0-dev`) runs itself in blaze's own tree, whatever that pins,
//! so the tree builds itself; anywhere else that pins it hands off to the pinned release as a
//! release does, saying so in one line. `--ignore-pin` runs this blaze, release or not,
//! whatever the workspace pins ([`choose`]).
//!
//! Asked before every command (`program.rs`), so it stays small and quick: std, `toml` and blaze-units' version rule only.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

/// Whether a version is a release's (`0.YYYYMMDD.N`): one rule, blaze-units', which
/// `blaze.lock` asks too.
use blaze_units::lock::is_version;

/// The variable the launcher sets for the blaze it runs: the manifest that pinned it. A blaze
/// started with it in that workspace is the pinned one, and runs no other; every process under
/// it inherits it, so in another workspace (a test's temporary one) it says nothing
/// ([`names`]).
pub(crate) const PINNED_BY: &str = "BLAZE_PINNED_BY";

/// The variable a blaze given [`IGNORE_PIN`] sets for every process under it: the root manifest
/// the word applied to. A blaze run there by one of them (a test or build script running
/// `$CARGO`, which is this blaze) runs itself as the word says; in another workspace it says
/// nothing, so the word reaches no workspace it was not given in ([`ignores`]).
pub(crate) const IGNORE_PIN_FOR: &str = "BLAZE_IGNORE_PIN_FOR";

/// Whether `var` (the value of [`PINNED_BY`] or [`IGNORE_PIN_FOR`]) names `manifest`, the root
/// manifest found now. The launcher and the blaze it runs find it from the same arguments in the
/// same directory, so the same path names it; a process under it elsewhere finds another.
pub(crate) fn names(var: Option<&OsStr>, manifest: &Path) -> bool {
    var.is_some_and(|it| Path::new(it) == manifest)
}

/// Whether [`IGNORE_PIN`] applies where `manifest` is the root manifest: `given` here, or given
/// to a blaze above this process for that same manifest ([`IGNORE_PIN_FOR`]).
pub(crate) fn ignores(given: bool, manifest: &Path) -> bool {
    given || names(std::env::var_os(IGNORE_PIN_FOR).as_deref(), manifest)
}

/// This blaze's version, as a pin names it: a release's `0.YYYYMMDD.N`, or `0.0.0-dev` for a
/// build of the tree, which no pin names (build.rs, from `BLAZE_RELEASE`).
pub(crate) const OURS: &str = env!("BLAZE_VERSION");

/// The word that runs this blaze in a pinned workspace in place of the release the pin names,
/// as the first word after `blaze` (`blaze --ignore-pin build`): how a blaze that is no
/// release of this workspace's pin builds it, as when trying a build of blaze on a pinned
/// project. It reaches the processes under it only for the workspace it was given in
/// ([`IGNORE_PIN_FOR`]).
pub(crate) const IGNORE_PIN: &str = "--ignore-pin";

/// `args` (the program's, its name first) without [`IGNORE_PIN`] when it is the first word
/// after the program's name (after `blaze` when cargo runs `cargo-blaze blaze ...`), and
/// whether it was. Anywhere else it is the command's own (`blaze run --bin tool x --ignore-pin`
/// gives it to `tool`, as blaze's parser hands a run program every word after its first) and
/// stays.
pub(crate) fn take_ignore_pin(args: impl Iterator<Item = OsString>) -> (Vec<OsString>, bool) {
    let mut kept: Vec<OsString> = args.collect(); // alloc: the program's arguments, once per process
    let at = if kept.get(1).is_some_and(|it| it == "blaze") { 2 } else { 1 };
    let ignore = kept.get(at).is_some_and(|it| it == IGNORE_PIN);
    if ignore {
        kept.remove(at);
    }
    (kept, ignore)
}

/// Which blaze runs in a workspace whose root manifest pins another than this one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    /// This one: the pin names it.
    Pinned,
    /// This one, as [`IGNORE_PIN`] says.
    Ignored,
    /// This one, a build of the source tree, in blaze's tree ([`is_blaze_tree`]).
    OwnTree,
    /// The release the pin names (or the channel names), run in this one's place: this one is
    /// a release too, and hands off as Bazelisk does.
    Release,
    /// The release the pin names (or the channel names), run in this one's place: this one is
    /// a build of the source tree outside blaze's tree, and hands off as a release does, saying
    /// so in one line on stderr, since what runs is not the blaze that was built.
    ReleaseForTree,
}

/// Which blaze runs where `manifest` pins `wanted` (a version or a channel), this one being
/// `ours` ([`OURS`]), `ignore` when [`IGNORE_PIN`] was given. A build of the source tree runs
/// itself in blaze's tree and hands off anywhere else, as a release does. `Err` is blaze's tree
/// told by a blaze-engine manifest that can't be read.
pub(crate) fn choose(
    ours: &str,
    manifest: &Path,
    wanted: &str,
    ignore: bool,
) -> Result<Choice, PinError> {
    if wanted == ours {
        return Ok(Choice::Pinned);
    }
    if ignore {
        return Ok(Choice::Ignored);
    }
    if is_version(ours) {
        return Ok(Choice::Release);
    }
    if is_blaze_tree(manifest)? {
        return Ok(Choice::OwnTree);
    }
    Ok(Choice::ReleaseForTree)
}

/// Whether the workspace whose root manifest is `manifest` is blaze's tree: beside it,
/// `crates/blaze-engine/Cargo.toml` names its package `blaze-engine`. Told by what is there, not
/// where it is, so every checkout of blaze is the tree (a worktree, a copy, a VM's, the source
/// of a Nix build, a fork) and no build path is kept in the binary; no path is resolved (WASI,
/// where the seed runs, has no `canonicalize`).
pub(crate) fn is_blaze_tree(manifest: &Path) -> Result<bool, PinError> {
    let dir = manifest.parent().unwrap_or(Path::new(""));
    let engine = dir.join("crates").join("blaze-engine").join("Cargo.toml");
    let Some(text) = manifest_text(&engine)? else { return Ok(false) };
    let table = toml::from_str::<toml::Table>(&text)
        .map_err(|error| PinError::NotToml { manifest: engine.clone(), error })?;
    let name = table.get("package").and_then(|it| it.get("name")).and_then(toml::Value::as_str);
    Ok(name == Some("blaze-engine"))
}

/// How a pin names what it pins, in a sentence: "blaze 0.20261030.1", "blaze's canary channel".
pub(crate) fn pinned_text(wanted: &str) -> String {
    if is_channel(wanted) { format!("blaze's {wanted} channel") } else { format!("blaze {wanted}") }
}

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
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
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
        // Only `0.<8 digits>.<N>`: a date that is missing or of another length is no release's.
        for not in
            ["5", "0.1.0", "0.99999999999.0", "1.20261027.0", "0.20261027.0.1", "0.2026102a.0"]
        {
            assert!(!is_version(not), "{not}");
        }
    }

    /// `BLAZE_PINNED_BY` and `BLAZE_IGNORE_PIN_FOR` say something only in the workspace they
    /// name: a process under them in another workspace is not held to them.
    #[test]
    fn the_variables_hold_only_for_the_manifest_they_name() {
        let here = Path::new("/w/a/Cargo.toml");
        assert!(names(Some(OsStr::new("/w/a/Cargo.toml")), here));
        assert!(!names(Some(OsStr::new("/tmp/test-ws/Cargo.toml")), here));
        assert!(!names(None, here));
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

    /// A build of blaze's tree builds any checkout of that tree whatever it pins (the
    /// bootstrap: stage 0 builds blaze), told by its blaze-engine manifest, not its path; in
    /// another pinned workspace it hands off to the pinned release as a release does.
    #[test]
    fn a_tree_build_runs_itself_in_blazes_tree_and_hands_off_elsewhere() {
        let root = std::env::temp_dir().join(format!("blaze-pin-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tree = |at: &str, name: &str| {
            let engine = root.join(at).join("crates/blaze-engine");
            std::fs::create_dir_all(&engine).unwrap();
            std::fs::write(
                root.join(at).join("Cargo.toml"),
                "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.metadata.blaze]\nversion = \"0.20261030.1\"\n",
            )
            .unwrap();
            std::fs::write(engine.join("Cargo.toml"), format!("[package]\nname = \"{name}\"\n"))
                .unwrap();
            root.join(at).join("Cargo.toml")
        };
        // Two checkouts of the tree (a worktree, a copy in a VM) are each the tree.
        let own = tree("tree", "blaze-engine");
        let copy = tree("lanes/worktree", "blaze-engine");
        // A workspace with a crate at that path named otherwise is not.
        let lookalike = tree("lookalike", "not-blaze-engine");
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(
            root.join("other/Cargo.toml"),
            "[workspace]\n\n[workspace.metadata.blaze]\nversion = \"canary\"\n",
        )
        .unwrap();
        let other = root.join("other/Cargo.toml");
        for manifest in [&own, &copy] {
            assert_eq!(
                choose("0.0.0-dev", manifest, "0.20261030.1", false).unwrap(),
                Choice::OwnTree
            );
        }
        assert_eq!(
            choose("0.0.0-dev", &lookalike, "0.20261030.1", false).unwrap(),
            Choice::ReleaseForTree
        );
        assert_eq!(choose("0.0.0-dev", &other, "canary", false).unwrap(), Choice::ReleaseForTree);
        assert_eq!(choose("0.0.0-dev", &other, "canary", true).unwrap(), Choice::Ignored);
        // A blaze-engine manifest that can't be read says so, naming it.
        std::fs::write(root.join("lookalike/crates/blaze-engine/Cargo.toml"), "[package\n")
            .unwrap();
        let refused = choose("0.0.0-dev", &lookalike, "0.20261030.1", false).unwrap_err();
        assert!(refused.to_string().contains("crates/blaze-engine/Cargo.toml"), "{refused}");
        // A release hands off to the one pinned, in blaze's tree or not, unless told.
        assert_eq!(choose("0.20261101.0", &own, "0.20261030.1", false).unwrap(), Choice::Release);
        assert_eq!(choose("0.20261101.0", &own, "0.20261030.1", true).unwrap(), Choice::Ignored);
        assert_eq!(choose("0.20261030.1", &own, "0.20261030.1", false).unwrap(), Choice::Pinned);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `--ignore-pin` is blaze's only as the first word after `blaze`; anywhere else it is the
    /// command's, and a run program's argument reaches the program.
    #[test]
    fn ignore_pin_is_taken_only_as_the_first_word() {
        let words = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        let taken = |line: &str| take_ignore_pin(words(line).into_iter());
        assert_eq!(taken("blaze --ignore-pin build -p a"), (words("blaze build -p a"), true));
        assert_eq!(
            taken("cargo-blaze blaze --ignore-pin test"),
            (words("cargo-blaze blaze test"), true)
        );
        for kept in [
            "blaze run --bin tool x --ignore-pin",
            "blaze run --ignore-pin",
            "cargo-blaze blaze test --ignore-pin",
            "blaze run -- --ignore-pin",
            "blaze",
        ] {
            assert_eq!(taken(kept), (words(kept), false), "{kept}");
        }
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
