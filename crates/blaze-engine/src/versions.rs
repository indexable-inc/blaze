//! The releases of blaze a workspace pins (`pin.rs`), fetched once into
//! `~/.blaze/versions/<version>`: the release's archive for this host from blaze's releases ([`RELEASES`]),
//! checked against every list of sums the release publishes ([`SUMS`]: `B3SUMS` and
//! `SHA256SUMS`, as `b3sum` and `sha256sum` write them), unpacked there. The launcher asks this
//! only when the pinned release is not there, so a pinned build reaches the network once per
//! release. A release that can't be fetched or checked is an error: no other blaze builds in
//! its place.

use std::{
    io::{IsTerminal as _, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, bail};
use blaze_fetch::progress::{Bytes, LOG_EVERY, Line, STALL, Secs};
use blaze_registry::http::{Bounds, HttpNotSuccessful, HyperHttp, Response, Retry};

use crate::pin;

/// Where the releases are: `<this>/v<version>/<asset>` and `<this>/channels/<channel>`, blaze's
/// R2 bucket (docs/release.md, "Where the bytes are").
const RELEASES: &str = blaze_cargo::release::RELEASES;

/// The variable that names another place the releases are: an `https://` address laid out as
/// [`RELEASES`], or a directory with a `v<version>` directory of each release's assets. A
/// plain-HTTP one is refused ([`FetchError::NotHttps`]), as is a redirect to one: the sums come
/// from the same place as the archive, so anyone on the path could change both.
const RELEASES_FROM: &str = "BLAZE_RELEASES";

/// How long a download may go without a byte before it is stopped as stalled, everywhere
/// blaze downloads.
const PATIENCE: Duration = STALL;

/// The archive formats a release's build for a host may come in, the one fetched first: the
/// `.tar.xz` (a third the size, from 0.20261105.2 on) where the release's sums name it, else the
/// `.tar.gz` every release publishes.
const ARCHIVES: [&str; 2] = ["tar.xz", "tar.gz"];

/// How long a download runs before its progress is said: a release's small assets (its sums, a
/// channel's pointer) come and go unsaid.
const SAID_AFTER: Duration = Duration::from_secs(1);

/// The most an asset of a release may be, as sent and once decoded: reading stops there.
const MOST: u64 = 1 << 30;

/// `blaze version-fetch <version>`: has the release `version` in `~/.blaze/versions`.
pub(crate) fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let version = args.next().context("`version-fetch` is told the version to fetch")?;
    fetch(&version)?;
    Ok(())
}

/// The `blaze` of the release `version`, fetched when it is not there yet, for a pin.
pub(crate) fn fetch(version: &str) -> anyhow::Result<PathBuf> {
    fetch_for(version, "which this workspace pins")
}

/// The `blaze` of the release `version`, fetched when it is not there yet, saying it fetches it
/// and `why` ("which this workspace pins"; `blaze update`'s). Every release blaze runs comes
/// through here: the pinned ones, the default `blaze update` installs, and `auto`'s.
pub(crate) fn fetch_for(version: &str, why: &str) -> anyhow::Result<PathBuf> {
    if !blaze_units::lock::is_version(version) {
        bail!("{version} is not a version of blaze (`0.YYYYMMDD.N`)");
    }
    let exe = pin::installed(version)
        .context("blaze keeps releases in .blaze in your home directory, and there is none: `BLAZE_HOME` names where")?;
    if exe.is_file() {
        return Ok(exe);
    }
    let host = hir_ty_codegen::Target::host();
    eprintln!("blaze: fetching blaze {version} for {host}, {why}");
    fetched(version, host, &exe, |name| read_at(&format!("v{version}"), name))?;
    Ok(exe)
}

/// The lists of sums a release may publish, each a line of hex digest and archive name per
/// archive, and the hash each is of. Every release publishes `SHA256SUMS` (what install.sh
/// checks); those dated [`B3SUMS_SINCE`] on publish `B3SUMS` too (release.yaml). A fetch checks
/// the archive against each one the release has, so an older release is checked by its
/// `SHA256SUMS` and a newer one by both, and needs one at least: none is no release.
const SUMS: [(&str, &str, fn(&[u8]) -> [u8; 32]); 2] =
    [("B3SUMS", "BLAKE3", blaze_ids::b3::hash), ("SHA256SUMS", "SHA-256", blaze_sha256::digest)];

/// The first date (`YYYYMMDD`, a version's middle number) of releases that all publish
/// `B3SUMS`: one dated this or later without it is refused, not checked by `SHA256SUMS` alone.
/// release.yaml publishes it since f1dbbc5ffc, on main from c0ecb0b8d5 (2026-11-01 03:56 UTC),
/// and every tag (train_release.py, canary.yaml) is on main's head of its UTC day. No release
/// is dated 2026-10-31 or 2026-11-01 (the newest before is 0.20261030.1), so from 20261031 on
/// every release has it.
const B3SUMS_SINCE: u32 = 20261031;

/// Whether the release `version` (`0.YYYYMMDD.N`, which [`blaze_units::lock::is_version`] holds every
/// version to, so the date is always there) is dated [`B3SUMS_SINCE`] or later.
fn has_b3sums(version: &str) -> bool {
    let date = version.split('.').nth(1).and_then(|it| it.parse::<u32>().ok());
    date.is_some_and(|it| it >= B3SUMS_SINCE)
}

/// Has the release `version`'s archive for `host` unpacked where `exe` is, its assets read
/// with `read` (`None` for one the release does not have), checked against every list of
/// [`SUMS`] the release publishes.
fn fetched(
    version: &str,
    host: &str,
    exe: &Path,
    read: impl Fn(&str) -> anyhow::Result<Option<Vec<u8>>>,
) -> anyhow::Result<()> {
    let top = format!("blaze-v{version}-{host}");
    let mut texts = Vec::new(); // alloc: at most two lists, once per fetched release
    for (list, hash, of) in SUMS {
        let Some(text) = read(list)? else { continue };
        texts.push((list, hash, of, String::from_utf8_lossy(&text).into_owned()));
    }
    let digest_in = |text: &str, archive: &str| {
        text.lines().find_map(|line| {
            let (digest, name) = line.split_once(char::is_whitespace)?;
            (name.trim_start().trim_start_matches('*') == archive).then(|| digest.to_owned())
        })
    };
    // The smallest format any list names; every list is then held to naming it.
    let archive = ARCHIVES
        .iter()
        .map(|kind| format!("{top}.{kind}"))
        .find(|archive| texts.iter().any(|(.., text)| digest_in(text, archive).is_some()))
        .unwrap_or_else(|| format!("{top}.{}", ARCHIVES[ARCHIVES.len() - 1]));
    let published: Vec<_> = texts
        .iter()
        .map(|(list, hash, of, text)| (*list, *hash, *of, digest_in(text, &archive)))
        .collect(); // alloc: as above
    let lists = || published.iter().map(|(list, ..)| *list).collect::<Vec<_>>().join(" and "); // alloc: an error's text
    if published.is_empty() {
        let names = SUMS.map(|(list, ..)| list).join(" or ");
        bail!("blaze {version} has no {names}: is {version} a release of blaze?");
    }
    if has_b3sums(version) && !published.iter().any(|(list, ..)| *list == "B3SUMS") {
        bail!(
            "blaze {version} has no B3SUMS, which every release dated {B3SUMS_SINCE} or later \
             publishes (release.yaml): it is not checked by its {} alone",
            lists()
        );
    }
    if published.iter().all(|(.., wanted)| wanted.is_none()) {
        bail!("blaze {version} has no build for {host}: its {} name no {archive}", lists());
    }
    if let Some((list, ..)) = published.iter().find(|(.., wanted)| wanted.is_none()) {
        bail!(
            "blaze {version}'s {} disagree: {list} names no {archive}, which the others name",
            lists()
        );
    }
    let bytes = read(&archive)?
        .with_context(|| format!("blaze {version} has no {archive}, which its {} name", lists()))?;
    for (list, hash, of, wanted) in &published {
        let wanted = wanted.as_deref().unwrap_or_default();
        let got = blaze_format::hex::encode(&of(&bytes));
        if !got.eq_ignore_ascii_case(wanted) {
            bail!(
                "{archive} of blaze {version} has {hash} hash {got}, and its {list} says {wanted}"
            );
        }
    }
    let dir = exe.parent().context("a release in no directory")?;
    unpacked(&bytes, dir, &top, &archive)
        .with_context(|| format!("unpacking {archive} into {}", dir.display()))?;
    if !exe.is_file() {
        bail!("{archive} of blaze {version} has no `blaze`");
    }
    Ok(())
}

/// `blaze pin [VERSION]`: `VERSION` (a release's version or a channel), else this blaze's,
/// under `[workspace.metadata.blaze]` in the workspace's root manifest, the rest of the
/// manifest as it was. A version given is fetched and checked first, so a pin names a release
/// that is there.
pub(crate) fn pin(args: &[String]) -> anyhow::Result<i32> {
    let given = named(args)?;
    if let Some(given) = &given {
        if blaze_units::lock::is_version(given) {
            fetch_for(given, "which `blaze pin` pins")?;
        } else if !pin::is_channel(given) {
            bail!(
                "`blaze pin {given}`: {given} is not a release of blaze (`0.YYYYMMDD.N`) or a channel (stable, canary)"
            );
        }
    }
    if given.is_none() && !blaze_units::lock::is_version(pin::OURS) {
        bail!(
            "this blaze is {}, a build of blaze's source tree and no release, so there is \
             nothing to pin: run `blaze pin` with a release (install.sh, or `blaze-v<version>` \
             from https://github.com/indexable-inc/blaze/releases)",
            pin::OURS
        );
    }
    let cwd = std::env::current_dir()?;
    let manifest = pin::root_manifest(&pin::start(args, &cwd))?.context(
        "`blaze pin` pins blaze in the workspace it is run in, and there is no Cargo.toml here \
         or above",
    )?;
    let shown = manifest.display();
    let text = std::fs::read_to_string(&manifest).with_context(|| format!("reading {shown}"))?;
    let mut doc: toml_edit::DocumentMut =
        text.parse().with_context(|| format!("reading {shown}"))?;
    let was = pin::pinned(&manifest).ok().flatten();
    let ours = given.as_deref().unwrap_or(pin::OURS);
    pinned_in(&mut doc, ours).with_context(|| format!("pinning blaze in {shown}"))?;
    std::fs::write(&manifest, doc.to_string()).with_context(|| format!("writing {shown}"))?;
    match was {
        Some(was) if was == ours => eprintln!("{shown} pins blaze {ours} already"),
        Some(was) => eprintln!("{shown} pins blaze {ours}, in place of {was}"),
        None => eprintln!("{shown} pins blaze {ours}"),
    }
    Ok(0)
}

/// The version or channel `blaze pin` is given: `blaze pin [VERSION] [--manifest-path PATH]`.
/// `--manifest-path` (the one flag it takes, with its value as the next word or after `=`) is
/// read by `pin::start`; any other flag is refused, naming it, rather than its value taken for
/// the version.
fn named(args: &[String]) -> anyhow::Result<Option<String>> {
    let mut words = args.iter();
    let mut named = None;
    while let Some(word) = words.next() {
        if word == "--manifest-path" {
            words.next().context("`blaze pin --manifest-path` takes the manifest's path")?;
            continue;
        }
        if word.starts_with("--manifest-path=") {
            continue;
        }
        if word.starts_with('-') {
            bail!("`blaze pin` takes no {word}: `blaze pin [VERSION] [--manifest-path PATH]`");
        }
        if let Some(first) = named.replace(word.clone()) {
            bail!("`blaze pin` pins one release, and is given {first} and {word}");
        }
    }
    Ok(named)
}

/// `doc` with `version` under `[workspace.metadata.blaze]`.
fn pinned_in(doc: &mut toml_edit::DocumentMut, version: &str) -> anyhow::Result<()> {
    let mut table = doc.as_table_mut();
    for (key, implicit) in [("workspace", true), ("metadata", true), ("blaze", false)] {
        let item = table.entry(key).or_insert_with(|| {
            let mut new = toml_edit::Table::new();
            new.set_implicit(implicit);
            toml_edit::Item::Table(new)
        });
        table = item.as_table_mut().with_context(|| format!("`{key}` is not a table"))?;
    }
    table.insert("version", toml_edit::value(version));
    Ok(())
}

/// For `blaze --version`: which blaze this is, and why it is the one that runs (`given`:
/// [`pin::IGNORE_PIN`] was given here; [`pin::ignores`]).
pub(crate) fn why_this_one(given: bool) -> String {
    let this = std::env::current_exe()
        .map_or_else(|_| "this blaze".to_owned(), |it| it.display().to_string());
    let ours = pin::OURS;
    let found = std::env::current_dir()
        .map_err(pin::PinError::NoWorkingDirectory)
        .and_then(|it| pin::root_manifest(&it));
    let manifest = match found {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return format!("{this}, as no workspace is here to pin one"),
        Err(why) => return format!("{this}: {why}"),
    };
    let shown = manifest.display();
    if pin::names(std::env::var_os(pin::PINNED_BY).as_deref(), &manifest) {
        return format!("{this}, as {shown} pins it");
    }
    let ignore = pin::ignores(given, &manifest);
    match pin::pinned(&manifest) {
        Ok(Some(wanted)) => {
            let pinned = pin::pinned_text(&wanted);
            match pin::choose(ours, &manifest, &wanted, ignore) {
                Ok(pin::Choice::Pinned) => format!("{this}, as {shown} pins it"),
                Ok(pin::Choice::Ignored) => format!(
                    "{this}, as `{}` says, in place of {pinned}, which {shown} pins",
                    pin::IGNORE_PIN
                ),
                Ok(pin::Choice::OwnTree) => format!(
                    "{this}, a build of blaze's source tree, which builds blaze's tree in place \
                     of {pinned}, which {shown} pins"
                ),
                Ok(pin::Choice::Release | pin::Choice::ReleaseForTree) => {
                    format!("{this}, run itself: {shown} pins {pinned}, which `blaze` runs there")
                }
                Err(why) => format!("{this}: {why}"),
            }
        }
        Ok(None) => format!(
            "{this}, as {shown} pins none (`[workspace.metadata.blaze] version`, which \
             `blaze pin` writes)"
        ),
        Err(why) => format!("{this}: {why}"),
    }
}

/// Where blaze's releases are read from: [`RELEASES`], or what [`RELEASES_FROM`] names.
pub(crate) enum Source {
    /// [`RELEASES`], and `latest.json` from blaze's site (`update.rs`).
    Ours,
    /// A directory of `v<version>` directories of each release's assets, `latest.json` beside them.
    Dir(PathBuf),
    /// An `https://` mirror laid out as [`RELEASES`], `latest.json` at its top.
    Mirror(String),
}

/// Where blaze's releases are read from now: one reading of [`RELEASES_FROM`] for the pins'
/// fetches, the channels' pointers and `latest.json`.
pub(crate) fn source() -> Result<Source, FetchError> {
    let given = match std::env::var(RELEASES_FROM) {
        Ok(given) => Some(given).filter(|it| !it.is_empty()),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(value)) => {
            return Err(FetchError::ReleasesNotText(value));
        }
    };
    Ok(match given {
        None => Source::Ours,
        Some(given) if !given.contains("://") => Source::Dir(PathBuf::from(given)),
        Some(given) if given.starts_with("https://") => Source::Mirror(given),
        Some(given) => return Err(FetchError::NotHttps(given)),
    })
}

/// The asset `name` of the release tagged `tag`, `None` when it has none.
fn read_at(tag: &str, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let base = match source()? {
        Source::Dir(dir) => {
            let file = dir.join(tag).join(name);
            return match std::fs::read(&file) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
            };
        }
        Source::Ours => RELEASES.to_owned(),
        Source::Mirror(base) => base,
    };
    let address = url::Url::parse(&format!("{}/{tag}/", base.trim_end_matches('/')))?.join(name)?;
    let http = HyperHttp::new(PATIENCE)?;
    let bounds = Bounds { most: MOST, https_only: true };
    let mut body = Vec::new(); // alloc: the asset, handed to the caller
    let mut shown = Shown::new(name);
    let answer: anyhow::Result<Response> = Retry::default().run(|| {
        body.clear();
        let mut sink = |offset: u64, bytes: &[u8]| -> std::io::Result<()> {
            if offset != body.len() as u64 {
                if offset != 0 {
                    return Err(std::io::Error::other(format!(
                        "a piece at byte {offset} came after {} bytes",
                        body.len()
                    )));
                }
                // The server sent the whole body over again.
                body.clear();
            }
            body.extend_from_slice(bytes);
            Ok(())
        };
        http.stream_watched(address.as_str(), &[], 0, bounds, &mut sink, name, &mut |line| {
            shown.watch(line);
        })
    });
    shown.end(answer.is_ok());
    let answer = answer?;
    match answer.status {
        404 => Ok(None),
        200 | 206 => Ok(Some(body)),
        _ => Err(HttpNotSuccessful::new(address.as_str(), &answer).into()),
    }
}

/// A download's progress on stderr, once it has run [`SAID_AFTER`]: on a terminal one line
/// redrawn in place as it comes, off one (a CI log, a pipe) a line every [`LOG_EVERY`], and at
/// its end what came, how fast.
struct Shown<'n> {
    name: &'n str,
    terminal: bool,
    /// When the last line was said, by the download's clock; `None` before the first.
    said: Option<Duration>,
    last: Option<Line>,
}

impl<'n> Shown<'n> {
    fn new(name: &'n str) -> Shown<'n> {
        Shown { name, terminal: std::io::stderr().is_terminal(), said: None, last: None }
    }

    fn watch(&mut self, line: &Line) {
        self.last = Some(*line);
        if line.elapsed < SAID_AFTER {
            return;
        }
        let due = match self.said {
            None => true,
            Some(_) if self.terminal => true,
            Some(at) => line.elapsed.saturating_sub(at) >= LOG_EVERY,
        };
        if !due {
            return;
        }
        self.said = Some(line.elapsed);
        let mut err = std::io::stderr().lock();
        if self.terminal {
            _ = write!(err, "\r\x1b[2Kblaze: {}: {line}", self.name);
            _ = err.flush();
        } else {
            _ = writeln!(err, "blaze: {}: {line}", self.name);
        }
    }

    /// The download ended, `ok` or not: the in-place line gives way to what came.
    fn end(&self, ok: bool) {
        let (Some(_), Some(line)) = (self.said, self.last) else { return };
        let mut err = std::io::stderr().lock();
        if self.terminal {
            _ = write!(err, "\r\x1b[2K");
        }
        if ok {
            let rate = line.average().map(|it| format!(", {}/s", Bytes(it as u64))).unwrap_or_default();
            _ = writeln!(
                err,
                "blaze: fetched {}: {} in {}{rate}",
                self.name,
                Bytes(line.done),
                Secs(line.elapsed)
            );
        } else {
            _ = writeln!(err, "blaze: {}: {line}, then stopped", self.name);
        }
    }
}

/// The tag of the release whose assets are the channels' pointers (docs/release.md).
const POINTERS: &str = "channels";

/// `blaze version-resolve <channel>`: the version `channel` names now, printed, and kept in
/// its [`pin::channel_file`] for the launcher, which runs that release for a pin to the channel.
pub(crate) fn resolve_command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let channel = args.next().context("`version-resolve` is told the channel")?;
    println!("{}", resolve(&channel)?);
    Ok(())
}

/// The version `channel` names: its pointer, the asset `<channel>` of the release `channels`.
pub(crate) fn resolve(channel: &str) -> anyhow::Result<String> {
    if !pin::is_channel(channel) {
        bail!("blaze has no channel {channel}: the channels are stable and canary");
    }
    let bytes = read_at(POINTERS, channel)?.with_context(|| {
        format!("blaze's releases name no release for the {channel} channel yet")
    })?;
    let version = String::from_utf8_lossy(&bytes).trim().to_owned();
    if !blaze_units::lock::is_version(&version) {
        bail!(
            "the {channel} channel of blaze's releases names {version:?}, which is not a version"
        );
    }
    let file = pin::channel_file(channel).ok_or(FetchError::NoHome)?;
    kept(&file, format!("{version}\n").as_bytes()).map_err(|error| FetchError::ChannelUnkept {
        channel: channel.to_owned(),
        file,
        error,
    })?;
    Ok(version)
}

/// `file` holding `bytes`, written beside it and renamed into place, so a reader sees the whole
/// of what was there or the whole of this, never part: what a channel named, the default
/// release, the update state.
pub(crate) fn kept(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut staged = file.as_os_str().to_owned();
    staged.push(format!(".{}", std::process::id()));
    let staged = PathBuf::from(staged);
    std::fs::write(&staged, bytes)?;
    std::fs::rename(&staged, file).inspect_err(|_| _ = std::fs::remove_file(&staged))
}

/// What a fetch of a release or a channel can't do, which no other path does in its place.
#[derive(Debug)]
pub(crate) enum FetchError {
    /// [`RELEASES_FROM`] is set to what is not text.
    ReleasesNotText(std::ffi::OsString),
    /// [`RELEASES_FROM`] is set to an address that is not `https://`.
    NotHttps(String),
    /// No home directory, where blaze keeps releases and what a channel named.
    NoHome,
    /// What a channel names can't be kept where the launcher reads it.
    ChannelUnkept { channel: String, file: PathBuf, error: std::io::Error },
    /// An earlier fetch's staging directory is there and can't be removed.
    Staging { dir: PathBuf, error: std::io::Error },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::ReleasesNotText(value) => {
                write!(
                    f,
                    "`{RELEASES_FROM}` is {value:?}, which is not text: it names an address or a directory"
                )
            }
            FetchError::NotHttps(given) => write!(
                f,
                "`{RELEASES_FROM}` is {given}, which is not an https:// address or a directory: \
                 a release and its sums come from there together, so they are fetched over HTTPS only"
            ),
            FetchError::NoHome => f.write_str(
                "blaze keeps releases and what a channel names in .blaze in your home directory, \
                 and there is none (`BLAZE_HOME` names where)",
            ),
            FetchError::ChannelUnkept { channel, file, error } => write!(
                f,
                "keeping the {channel} channel's version in {}, where `blaze` reads it: {error}",
                file.display()
            ),
            FetchError::Staging { dir, error } => {
                write!(f, "removing {}, left by an earlier fetch: {error}", dir.display())
            }
        }
    }
}

impl std::error::Error for FetchError {}

/// Unpacks the archive `bytes` (named `archive`: a `.tar.xz` or a `.tar.gz`), whose files are
/// under `top`, into `dir`. Unpacked beside it first and moved into place whole, so a `dir` that
/// is there is a whole release.
fn unpacked(bytes: &[u8], dir: &Path, top: &str, archive: &str) -> anyhow::Result<()> {
    let parent = dir.parent().context("a release in no directory")?;
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".{}.{}", top, std::process::id()));
    match std::fs::remove_dir_all(&staging) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(FetchError::Staging { dir: staging, error }.into());
        }
        _ => {}
    }
    if archive.ends_with(".tar.xz") {
        let xz = lzma_rust2::XzReader::new(std::io::Cursor::new(bytes), false);
        tar::Archive::new(xz).unpack(&staging)?;
    } else if archive.ends_with(".tar.gz") {
        tar::Archive::new(flate2::read::GzDecoder::new(bytes)).unpack(&staging)?;
    } else {
        bail!("{archive} is neither a .tar.xz nor a .tar.gz");
    }
    let moved = std::fs::rename(staging.join(top), dir);
    _ = std::fs::remove_dir_all(&staging);
    match moved {
        Ok(()) => Ok(()),
        // Another blaze fetched it at the same time.
        Err(_) if dir.is_dir() => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_is_written_into_the_workspace_table() {
        let mut doc: toml_edit::DocumentMut =
            "[workspace]\nmembers = [\"a\"] # kept\n".parse().unwrap();
        pinned_in(&mut doc, "0.20261027.0").unwrap();
        assert_eq!(
            doc.to_string(),
            "[workspace]\nmembers = [\"a\"] # kept\n\n[workspace.metadata.blaze]\n\
             version = \"0.20261027.0\"\n"
        );
        pinned_in(&mut doc, "0.20261028.1").unwrap();
        assert!(doc.to_string().ends_with("version = \"0.20261028.1\"\n"));
        let mut doc: toml_edit::DocumentMut = "[package]\nname = \"a\"\n".parse().unwrap();
        pinned_in(&mut doc, "0.1.0").unwrap();
        assert_eq!(
            doc.to_string(),
            "[package]\nname = \"a\"\n\n[workspace.metadata.blaze]\nversion = \"0.1.0\"\n"
        );
    }

    /// A pinned release is checked by every list of sums it publishes: one from before
    /// `B3SUMS` by its `SHA256SUMS`, a newer one by both; a wrong hash in either, a list that
    /// leaves the archive out, or no list at all is refused and nothing is unpacked.
    #[test]
    fn a_pinned_release_is_checked_by_every_sum_it_publishes() {
        let root = std::env::temp_dir().join(format!("blaze-versions-sums-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host = "x86_64-unknown-linux-gnu";
        let top = format!("blaze-v0.1.0-{host}");
        let archive = format!("{top}.tar.gz");
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, format!("{top}/blaze"), &b"run"[..]).unwrap();
        let plain = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &plain).unwrap();
        let bytes = gz.finish().unwrap();
        let sha = blaze_format::hex::encode(&blaze_sha256::digest(&bytes));
        let b3 = blaze_format::hex::encode(&blaze_ids::b3::hash(&bytes));
        let wrong = "0".repeat(64);
        let line =
            |digest: &str| Some(format!("{digest}  {archive}\nffff  blaze-v0.1.0-other.tar.gz\n"));
        let run = |case: &str, b3sums: Option<String>, sha256sums: Option<String>| {
            let exe = root.join(case).join("versions/0.1.0/blaze");
            let got = fetched("0.1.0", host, &exe, |name| {
                Ok(match name {
                    "B3SUMS" => b3sums.clone().map(String::into_bytes),
                    "SHA256SUMS" => sha256sums.clone().map(String::into_bytes),
                    it if it == archive => Some(bytes.clone()),
                    _ => None,
                })
            });
            (got.map_err(|error| format!("{error:#}")), exe.is_file())
        };
        // A release from before B3SUMS: its SHA256SUMS checks it.
        assert_eq!(run("sha-only", None, line(&sha)), (Ok(()), true));
        let (got, unpacked) = run("sha-wrong", None, line(&wrong));
        assert!(got.unwrap_err().contains("SHA-256 hash") && !unpacked);
        // A newer release: both are checked, each on its own.
        assert_eq!(run("both", line(&b3), line(&sha)), (Ok(()), true));
        let (got, unpacked) = run("both-sha-wrong", line(&b3), line(&wrong));
        assert!(got.unwrap_err().contains("its SHA256SUMS says") && !unpacked);
        let (got, unpacked) = run("both-b3-wrong", line(&wrong), line(&sha));
        assert!(got.unwrap_err().contains("its B3SUMS says") && !unpacked);
        let (got, unpacked) = run("b3-leaves-it-out", Some(String::new()), line(&sha));
        assert!(got.unwrap_err().contains("B3SUMS names no") && !unpacked);
        let (got, unpacked) = run("none", None, None);
        assert!(got.unwrap_err().contains("has no B3SUMS or SHA256SUMS") && !unpacked);
        let (got, _) = run("no-host", Some(String::new()), Some(String::new()));
        assert!(got.unwrap_err().contains("has no build for x86_64-unknown-linux-gnu"));
        // A release with a `.tar.xz` beside its `.tar.gz`: the `.tar.xz` is fetched, checked and
        // unpacked, and a list that names only the `.tar.gz` then disagrees.
        let xz_name = format!("{top}.tar.xz");
        let mut xz = lzma_rust2::XzWriter::new(Vec::new(), Default::default()).unwrap();
        std::io::Write::write_all(&mut xz, &plain).unwrap();
        let xz_bytes = xz.finish().unwrap();
        let both = |of: fn(&[u8]) -> [u8; 32], named_xz: bool| {
            let mut text = line(&blaze_format::hex::encode(&of(&bytes))).unwrap();
            if named_xz {
                text.push_str(&format!("{}  {xz_name}\n", blaze_format::hex::encode(&of(&xz_bytes))));
            }
            text
        };
        let asked = std::cell::RefCell::new(Vec::new());
        let exe = root.join("xz").join("versions/0.1.0/blaze");
        fetched("0.1.0", host, &exe, |name| {
            asked.borrow_mut().push(name.to_owned());
            Ok(match name {
                "B3SUMS" => Some(both(blaze_ids::b3::hash, true).into_bytes()),
                "SHA256SUMS" => Some(both(blaze_sha256::digest, true).into_bytes()),
                it if it == xz_name => Some(xz_bytes.clone()),
                _ => None,
            })
        })
        .unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"run");
        assert!(asked.borrow().contains(&xz_name) && !asked.borrow().contains(&archive));
        let exe = root.join("xz-disagree").join("versions/0.1.0/blaze");
        let got = fetched("0.1.0", host, &exe, |name| {
            Ok(match name {
                "B3SUMS" => Some(both(blaze_ids::b3::hash, true).into_bytes()),
                "SHA256SUMS" => Some(both(blaze_sha256::digest, false).into_bytes()),
                _ => None,
            })
        });
        assert!(format!("{:#}", got.unwrap_err()).contains("SHA256SUMS names no"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A release dated after `B3SUMS` began that lacks it is refused, naming it, rather than
    /// checked by `SHA256SUMS` alone; one from before is checked by what it has.
    #[test]
    fn a_release_dated_after_b3sums_began_needs_them() {
        assert!(
            !has_b3sums("0.20261030.1") && has_b3sums("0.20261031.0") && has_b3sums("0.20270101.3")
        );
        let exe = std::env::temp_dir()
            .join(format!("blaze-versions-b3-{}", std::process::id()))
            .join("versions/0.20261102.0/blaze");
        let got = fetched("0.20261102.0", "x86_64-unknown-linux-gnu", &exe, |name| {
            Ok((name == "SHA256SUMS")
                .then(|| b"00  blaze-v0.20261102.0-x86_64-unknown-linux-gnu.tar.gz\n".to_vec()))
        });
        let said = format!("{:#}", got.unwrap_err());
        assert!(
            said.contains("blaze 0.20261102.0 has no B3SUMS") && said.contains("20261031"),
            "{said}"
        );
        assert!(!exe.exists());
    }

    #[test]
    fn a_release_from_a_directory_is_checked_and_unpacked() {
        let root = std::env::temp_dir().join(format!("blaze-versions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let top = format!("blaze-v0.1.0-{}", hir_ty_codegen::Target::host());
        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o755);
        header.set_cksum();
        archive.append_data(&mut header, format!("{top}/blaze"), &b"run"[..]).unwrap();
        let tar = archive.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        let bytes = gz.finish().unwrap();
        let dest = root.join("home/versions/0.1.0");
        unpacked(&bytes, &dest, &top, "it.tar.gz").unwrap();
        assert_eq!(std::fs::read(dest.join("blaze")).unwrap(), b"run");
        // a second fetch at once finds it there
        unpacked(&bytes, &dest, &top, "it.tar.gz").unwrap();
        // what an earlier fetch left that can't be removed stops this one
        let elsewhere = root.join("home/versions/0.2.0");
        let staging = root.join(format!("home/versions/.{top}.{}", std::process::id()));
        std::fs::write(&staging, b"not a directory").unwrap();
        let error = unpacked(&bytes, &elsewhere, &top, "it.tar.gz").unwrap_err();
        assert!(
            matches!(error.downcast_ref::<FetchError>(), Some(FetchError::Staging { .. })),
            "{error:#}"
        );
        assert!(!elsewhere.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
