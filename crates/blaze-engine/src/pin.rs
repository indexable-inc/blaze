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
//! Asked before every command (`program.rs`), so it stays small and quick: std, `toml`,
//! blaze-units' version rule, and a hash to name the file a lookup is kept in ([`found`]): once
//! kept, a lookup stats the manifests on the way and reads and parses none.

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
    pin_of(manifest, &table_of(manifest, &text)?)
}

/// A workspace found from a directory and what it pins ([`found`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Found {
    /// The root manifest ([`root_manifest`]); none outside any package.
    pub(crate) manifest: Option<PathBuf>,
    /// What it pins ([`pinned`]); none when it pins nothing.
    pub(crate) pinned: Option<String>,
}

/// What `stat` says of a manifest: while it says this, the manifest's text is what was read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    len: u64,
    /// Its modification time, in nanoseconds since the epoch.
    modified_ns: u128,
    inode: u64,
}

/// `path`'s stamp, `None` where no file is (as [`manifest_text`] reads none there). `Err` is a
/// file whose stamp can't be had: nothing is kept of a lookup that met one.
fn stamp(path: &Path) -> Result<Option<Stamp>, ()> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(_) => return Err(()),
    };
    let modified = meta.modified().map_err(|_| ())?;
    let modified_ns =
        modified.duration_since(std::time::UNIX_EPOCH).map_err(|_| ())?.as_nanos();
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Ok(Some(Stamp { len: meta.len(), modified_ns, inode }))
}

/// The first line of a kept lookup: its format's name and version.
const KEPT: &str = "blaze pin lookup 1";

/// How old a manifest's modification must be before a lookup that read it is kept: an edit
/// that keeps its length and inode within the file system's time step of the last one would
/// leave its stamp as it was (git's "racily clean" file), and a pin's version is often edited
/// to one of the same length. Two seconds is past every file system's step blaze runs on.
const SETTLED_NS: u128 = 2_000_000_000;

/// Where the lookup from `start` is kept: `pins/<hash of start>` in blaze's home, beside
/// [`versions`]. None with no home, or for a directory that is not UTF-8 or has a tab or a
/// newline in its name (the file's lines could not name it): each lookup there reads the
/// manifests.
fn kept_at(start: &Path) -> Option<PathBuf> {
    let text = start.to_str().filter(|it| !it.contains(['\t', '\n']))?;
    let name = blaze_format::hex::encode(&blaze_ids::b3::hash(text.as_bytes()));
    Some(versions()?.parent()?.join("pins").join(name))
}

/// The lookup from `start` as `file` keeps it, when every manifest it read, and every place it
/// found none, is as it was: a stat of each, and no manifest read or parsed. `None` when there
/// is none kept, one of them moved, or the file is not what [`keep`] writes: the manifests are
/// read then, and the file written again.
fn kept(file: &Path, start: &Path) -> Option<Found> {
    let text = std::fs::read_to_string(file).ok()?;
    let mut lines = text.lines();
    if lines.next()? != KEPT || lines.next()?.strip_prefix("start\t")? != start.to_str()? {
        return None;
    }
    let (mut manifest, mut pin, mut whole) = (None, None, false);
    for line in lines {
        // Nothing follows the end.
        if whole {
            return None;
        }
        let mut parts = line.split('\t');
        match parts.next()? {
            "read" => {
                let path = Path::new(parts.next()?);
                let was = Stamp {
                    len: parts.next()?.parse().ok()?,
                    modified_ns: parts.next()?.parse().ok()?,
                    inode: parts.next()?.parse().ok()?,
                };
                if stamp(path) != Ok(Some(was)) {
                    return None;
                }
            }
            "none" => {
                if stamp(Path::new(parts.next()?)) != Ok(None) {
                    return None;
                }
            }
            "root" => manifest = Some(PathBuf::from(parts.next()?)),
            "pin" => {
                let version = parts.next()?;
                if !(is_version(version) || is_channel(version)) {
                    return None;
                }
                pin = Some(version.to_owned());
            }
            "end" => whole = true,
            _ => return None,
        }
        if parts.next().is_some() {
            return None;
        }
    }
    // A pin is a root manifest's.
    (whole && (pin.is_none() || manifest.is_some())).then_some(Found { manifest, pinned: pin })
}

/// Keeps the lookup from `start` that read `seen` and found `found`, in `file`, for [`kept`].
/// Nothing is kept when a manifest's stamp could not be had, changed while it was read, is
/// newer than [`SETTLED_NS`] (or the clock can't say), or a path can't be a line's. A file
/// that can't be written is no error: the next lookup reads the manifests, as this one did.
fn keep(file: &Path, start: &Path, seen: &[(PathBuf, Result<Option<Stamp>, ()>)], found: &Found) {
    use std::fmt::Write as _;
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return;
    };
    fn line(path: &Path) -> Option<&str> {
        path.to_str().filter(|it| !it.contains(['\t', '\n']))
    }
    let Some(start) = line(start) else { return };
    let mut text = format!("{KEPT}\nstart\t{start}\n"); // alloc: the file's text, once a lookup kept
    for (path, was) in seen {
        let (Ok(was), Some(named)) = (was, line(path)) else { return };
        // As it was before it was read: a manifest written meanwhile is not what was read.
        if stamp(path) != Ok(*was) {
            return;
        }
        // Writing to a String cannot fail.
        let _ = match was {
            Some(it) if now.as_nanos().saturating_sub(it.modified_ns) < SETTLED_NS => return,
            Some(it) => writeln!(text, "read\t{named}\t{}\t{}\t{}", it.len, it.modified_ns, it.inode),
            None => writeln!(text, "none\t{named}"),
        };
    }
    if let Some(manifest) = &found.manifest {
        let Some(named) = line(manifest) else { return };
        let _ = writeln!(text, "root\t{named}");
    }
    if let Some(pin) = &found.pinned {
        let _ = writeln!(text, "pin\t{pin}");
    }
    text.push_str("end\n");
    _ = crate::versions::kept(file, text.as_bytes());
}

/// The table of `manifest`'s `text`.
fn table_of(manifest: &Path, text: &str) -> Result<toml::Table, PinError> {
    toml::from_str::<toml::Table>(text)
        .map_err(|error| PinError::NotToml { manifest: manifest.to_owned(), error })
}

/// What `table`, the root manifest `manifest`'s, pins ([`pinned`]).
fn pin_of(manifest: &Path, table: &toml::Table) -> Result<Option<String>, PinError> {
    let blaze =
        table.get("workspace").and_then(|it| it.get("metadata")).and_then(|it| it.get("blaze"));
    let Some(version) = blaze.and_then(|it| it.get("version")) else { return Ok(None) };
    match version.as_str() {
        Some(version) if is_version(version) || is_channel(version) => Ok(Some(version.to_owned())),
        _ => Err(PinError::NotAVersion { manifest: manifest.to_owned(), pinned: version.clone() }),
    }
}

/// [`root_manifest`] of `dir` and what it [`pinned`], each manifest read and parsed once, and
/// each place a manifest was looked for noted in `seen` with its stamp as it was before the
/// read.
fn find(
    dir: &Path,
    seen: &mut Vec<(PathBuf, Result<Option<Stamp>, ()>)>,
) -> Result<Found, PinError> {
    let mut nearest = None;
    for dir in dir.ancestors() {
        let manifest = dir.join("Cargo.toml");
        seen.push((manifest.clone(), stamp(&manifest))); // clone: a path, once a directory on the way
        let Some(text) = manifest_text(&manifest)? else { continue };
        let table = table_of(&manifest, &text)?;
        if table.contains_key("workspace") {
            let pinned = pin_of(&manifest, &table)?;
            return Ok(Found { manifest: Some(manifest), pinned });
        }
        let named = table.get("package").and_then(|it| it.get("workspace"));
        if let Some(root) = named.and_then(toml::Value::as_str) {
            let manifest = dir.join(root).join("Cargo.toml");
            seen.push((manifest.clone(), stamp(&manifest))); // clone: a path, once
            let pinned = pinned(&manifest)?;
            return Ok(Found { manifest: Some(manifest), pinned });
        }
        if nearest.is_none() {
            nearest = Some((manifest, table));
        }
    }
    match nearest {
        Some((manifest, table)) => {
            let pinned = pin_of(&manifest, &table)?;
            Ok(Found { manifest: Some(manifest), pinned })
        }
        None => Ok(Found { manifest: None, pinned: None }),
    }
}

/// The workspace `start` is in and what it pins: [`root_manifest`] and [`pinned`] as one
/// lookup, which the launcher makes before every command. Kept in blaze's home by `start`
/// ([`kept_at`]) with the stamp of every manifest it read and every place it found none, so
/// the next lookup from `start` is a stat of each and one small read, and reads and parses no
/// manifest; a stamp that moved (a manifest edited, added or removed anywhere on the way)
/// reads them all again. What a lookup answers is the same either way, and an error is never
/// kept.
pub(crate) fn found(start: &Path) -> Result<Found, PinError> {
    found_kept_in(start, kept_at(start).as_deref())
}

/// [`found`], its lookup kept in `file` when there is one.
fn found_kept_in(start: &Path, file: Option<&Path>) -> Result<Found, PinError> {
    if let Some(found) = file.and_then(|file| kept(file, start)) {
        return Ok(found);
    }
    let mut seen = Vec::new(); // alloc: a place a directory on the way, once a lookup not kept
    let found = find(start, &mut seen)?;
    if let Some(file) = file {
        keep(file, start, &seen, &found);
    }
    Ok(found)
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
    Some(blaze_manifest::user_config::home()?.join("versions"))
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

    /// A manifest holding `text`, last modified `ago` seconds ago: past [`SETTLED_NS`], so a
    /// lookup that reads it is kept.
    fn manifest_aged(path: &Path, text: &str, ago: u64) {
        std::fs::write(path, text).unwrap();
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(ago);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(then).unwrap();
    }

    /// The invariant: a kept lookup answers what reading the manifests answers, whatever
    /// changed on the way since it was kept (the pin edited to one of the same length, a
    /// manifest added nearer, the root removed), and an unsettled manifest keeps nothing.
    /// The control at its end shows the kept answer is the one served while every stamp
    /// stands: a manifest rewritten in place with its stamp put back answers as kept, which is
    /// the very case [`SETTLED_NS`] keeps out of the file.
    #[test]
    fn a_kept_lookup_answers_as_the_manifests_do() {
        let root = std::env::temp_dir().join(format!("blaze-pin-kept-{}", std::process::id()));
        let start = root.join("crates/a/src");
        std::fs::create_dir_all(&start).unwrap();
        let file = root.join("kept");
        let manifest = root.join("Cargo.toml");
        let pins = |version: &str| {
            format!("[workspace]\n\n[workspace.metadata.blaze]\nversion = \"{version}\"\n")
        };
        let both = |file: &Path| {
            let kept = found_kept_in(&start, Some(file)).unwrap();
            assert_eq!(kept, found_kept_in(&start, None).unwrap(), "kept and read agree");
            kept
        };
        // Just written: read, and not kept.
        std::fs::write(&manifest, pins("0.20261027.0")).unwrap();
        assert_eq!(both(&file).pinned.as_deref(), Some("0.20261027.0"));
        assert!(!file.exists(), "a manifest modified this second keeps no lookup");
        // Settled: kept, and answered from the file.
        manifest_aged(&manifest, &pins("0.20261027.0"), 60);
        assert_eq!(both(&file).pinned.as_deref(), Some("0.20261027.0"));
        assert!(kept(&file, &start).is_some(), "a settled lookup is kept");
        // The pin edited to one of the same length: its stamp moved.
        manifest_aged(&manifest, &pins("0.20261027.1"), 30);
        assert_eq!(both(&file).pinned.as_deref(), Some("0.20261027.1"));
        // A manifest added nearer, naming no workspace: the root is still the root; one that
        // is a workspace is the root now, and pins nothing.
        manifest_aged(&root.join("crates/a/Cargo.toml"), "[package]\nname = \"a\"\n", 30);
        assert_eq!(both(&file).manifest.as_deref(), Some(manifest.as_path()));
        manifest_aged(&root.join("crates/a/Cargo.toml"), "[workspace]\n", 20);
        let nearer = both(&file);
        assert_eq!(nearer.manifest, Some(root.join("crates/a/Cargo.toml")));
        assert_eq!(nearer.pinned, None);
        std::fs::remove_file(root.join("crates/a/Cargo.toml")).unwrap();
        assert_eq!(both(&file).pinned.as_deref(), Some("0.20261027.1"));
        // The control: the manifest's bytes changed with its stamp put back is answered as
        // kept, so the file is what answers while the stamps stand.
        let was = std::fs::metadata(&manifest).unwrap().modified().unwrap();
        assert!(kept(&file, &start).is_some());
        std::fs::File::options()
            .write(true)
            .open(&manifest)
            .and_then(|mut it| {
                std::io::Write::write_all(&mut it, pins("0.20261027.2").as_bytes())?;
                it.set_modified(was)
            })
            .unwrap();
        assert_eq!(
            found_kept_in(&start, Some(&file)).unwrap().pinned.as_deref(),
            Some("0.20261027.1"),
            "the kept lookup answers while every stamp stands"
        );
        assert_eq!(found_kept_in(&start, None).unwrap().pinned.as_deref(), Some("0.20261027.2"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The control: a manifest rewritten in place with its stamp put back is the one change
    /// the kept file cannot see, so there the oracle (kept and read agree) fails. It shows the
    /// kept file is what answers while every stamp stands, and is the case [`SETTLED_NS`]
    /// keeps out of the file.
    #[test]
    fn the_oracle_catches_a_manifest_rewritten_under_its_stamp() {
        let root = std::env::temp_dir().join(format!("blaze-pin-control-{}", std::process::id()));
        let start = root.join("src");
        std::fs::create_dir_all(&start).unwrap();
        let file = root.join("kept");
        let manifest = root.join("Cargo.toml");
        let pins = |version: &str| {
            format!("[workspace]\n\n[workspace.metadata.blaze]\nversion = \"{version}\"\n")
        };
        manifest_aged(&manifest, &pins("0.20261027.1"), 60);
        found_kept_in(&start, Some(&file)).unwrap();
        assert!(kept(&file, &start).is_some(), "a settled lookup is kept");
        let was = std::fs::metadata(&manifest).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(&manifest)
            .and_then(|mut it| {
                std::io::Write::write_all(&mut it, pins("0.20261027.2").as_bytes())?;
                it.set_modified(was)
            })
            .unwrap();
        assert_ne!(
            found_kept_in(&start, Some(&file)).unwrap(),
            found_kept_in(&start, None).unwrap(),
            "kept and read disagree: the oracle sees it"
        );
        std::fs::remove_dir_all(&root).unwrap();
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
