//! The releases of blaze a workspace pins (`pin.rs`), fetched once into
//! `~/.blaze/versions/<version>`: the release's archive for this host from its GitHub release,
//! checked against the release's `B3SUMS` (BLAKE3, as `b3sum` writes it), unpacked there. The launcher asks this only when
//! the pinned release is not there, so a pinned build reaches the network once per release.
//! A release that can't be fetched is an error: no other blaze builds in its place.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, bail};
use blaze_registry::http::{Http as _, HttpNotSuccessful, HyperHttp, Retry};

use crate::pin;

/// Where the releases are: `<this>/v<version>/<asset>`.
const RELEASES: &str = "https://github.com/indexable-inc/blaze/releases/download";

/// The variable that names another place the releases are: an address laid out as
/// [`RELEASES`], or a directory with a `v<version>` directory of each release's assets.
const RELEASES_FROM: &str = "BLAZE_RELEASES";

/// How long one answer may take.
const PATIENCE: Duration = Duration::from_secs(120);

/// The most an archive of a release may be.
const MOST: u64 = 1 << 30;

/// `blaze version-fetch <version>`: has the release `version` in `~/.blaze/versions`.
pub(crate) fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let version = args.next().context("`version-fetch` is told the version to fetch")?;
    fetch(&version)?;
    Ok(())
}

/// The `blaze` of the release `version`, fetched when it is not there yet.
pub(crate) fn fetch(version: &str) -> anyhow::Result<PathBuf> {
    if !pin::is_version(version) {
        bail!("{version} is not a version of blaze (`0.YYYYMMDD.N`)");
    }
    let exe = pin::installed(version)
        .context("blaze keeps releases in .blaze in your home directory, and there is none: `BLAZE_HOME` names where")?;
    if exe.is_file() {
        return Ok(exe);
    }
    let host = hir_ty_codegen::Target::host();
    let archive = format!("blaze-v{version}-{host}.tar.gz");
    eprintln!("blaze: fetching blaze {version} for {host}, which this workspace pins");
    let sums = read(version, "B3SUMS")?;
    let sums = String::from_utf8_lossy(&sums);
    let wanted = sums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        (name.trim_start().trim_start_matches('*') == archive).then(|| hash.to_owned())
    });
    let wanted = wanted.with_context(|| {
        format!("blaze {version} has no build for {host}: its B3SUMS names no {archive}")
    })?;
    let bytes = read(version, &archive)?;
    let got = blaze_format::hex::encode(&blaze_ids::b3::hash(&bytes));
    if got != wanted {
        bail!("{archive} of blaze {version} has BLAKE3 hash {got}, and its B3SUMS says {wanted}");
    }
    let dir = exe.parent().context("a release in no directory")?;
    unpacked(&bytes, dir, &format!("blaze-v{version}-{host}"))
        .with_context(|| format!("unpacking {archive} into {}", dir.display()))?;
    if !exe.is_file() {
        bail!("{archive} of blaze {version} has no `blaze`");
    }
    Ok(exe)
}

/// `blaze pin`: this blaze's version under `[workspace.metadata.blaze]` in the workspace's
/// root manifest, the rest of the manifest as it was.
pub(crate) fn pin(args: &[String]) -> anyhow::Result<i32> {
    if !pin::is_version(pin::OURS) {
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
    pinned_in(&mut doc, pin::OURS).with_context(|| format!("pinning blaze in {shown}"))?;
    std::fs::write(&manifest, doc.to_string()).with_context(|| format!("writing {shown}"))?;
    let ours = pin::OURS;
    match was {
        Some(was) if was == ours => eprintln!("{shown} pins blaze {ours} already"),
        Some(was) => eprintln!("{shown} pins blaze {ours}, in place of {was}"),
        None => eprintln!("{shown} pins blaze {ours}"),
    }
    Ok(0)
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

/// For `blaze --version`: which blaze this is, and why it is the one that runs.
pub(crate) fn why_this_one() -> String {
    let this = std::env::current_exe()
        .map_or_else(|_| "this blaze".to_owned(), |it| it.display().to_string());
    if let Some(by) = std::env::var_os(pin::PINNED_BY) {
        return format!("{this}, as {} pins it", Path::new(&by).display());
    }
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
    match pin::pinned(&manifest) {
        Ok(Some(wanted)) if wanted == ours => format!("{this}, as {shown} pins it"),
        Ok(Some(wanted)) => {
            format!("{this}, run itself: {shown} pins blaze {wanted}, which `blaze` runs there")
        }
        Ok(None) => format!(
            "{this}, as {shown} pins none (`[workspace.metadata.blaze] version`, which \
             `blaze pin` writes)"
        ),
        Err(why) => format!("{this}: {why}"),
    }
}

/// The asset `name` of the release `version`.
fn read(version: &str, name: &str) -> anyhow::Result<Vec<u8>> {
    let read = read_at(&format!("v{version}"), name)?;
    read.with_context(|| format!("blaze {version} has no {name}: is {version} a release of blaze?"))
}

/// The asset `name` of the release tagged `tag`, `None` when it has none.
fn read_at(tag: &str, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let given = match std::env::var(RELEASES_FROM) {
        Ok(given) => Some(given).filter(|it| !it.is_empty()),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(value)) => return Err(FetchError::ReleasesNotText(value).into()),
    };
    if let Some(dir) = given.as_ref().filter(|it| !it.contains("://")) {
        let file = Path::new(dir).join(tag).join(name);
        return match std::fs::read(&file) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
        };
    }
    let base = given.unwrap_or_else(|| RELEASES.to_owned());
    let address = url::Url::parse(&format!("{}/{tag}/", base.trim_end_matches('/')))?.join(name)?;
    let http = HyperHttp::new(PATIENCE)?;
    let answer = Retry::default().run(|| http.get(address.as_str(), &[]))?;
    match answer.status {
        404 => Ok(None),
        200..300 if answer.body.len() as u64 > MOST => {
            bail!("{address} is {} bytes, more than the {MOST} a release's asset may be", answer.body.len())
        }
        200..300 => Ok(Some(answer.body)),
        _ => Err(HttpNotSuccessful::new(address.as_str(), &answer).into()),
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
    if !pin::is_version(&version) {
        bail!(
            "the {channel} channel of blaze's releases names {version:?}, which is not a version"
        );
    }
    let file = pin::channel_file(channel).ok_or(FetchError::NoHome)?;
    let kept = file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| {
        let staged = file.with_extension(format!("{}", std::process::id()));
        std::fs::write(&staged, format!("{version}\n"))?;
        std::fs::rename(&staged, &file)
    });
    kept.map_err(|error| FetchError::ChannelUnkept { channel: channel.to_owned(), file, error })?;
    Ok(version)
}

/// What a fetch of a release or a channel can't do, which no other path does in its place.
#[derive(Debug)]
enum FetchError {
    /// [`RELEASES_FROM`] is set to what is not text.
    ReleasesNotText(std::ffi::OsString),
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
                write!(f, "`{RELEASES_FROM}` is {value:?}, which is not text: it names an address or a directory")
            }
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

/// Unpacks the `.tar.gz` `bytes`, whose files are under `top`, into `dir`. Unpacked beside it
/// first and moved into place whole, so a `dir` that is there is a whole release.
fn unpacked(bytes: &[u8], dir: &Path, top: &str) -> anyhow::Result<()> {
    let parent = dir.parent().context("a release in no directory")?;
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".{}.{}", top, std::process::id()));
    match std::fs::remove_dir_all(&staging) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(FetchError::Staging { dir: staging, error }.into());
        }
        _ => {}
    }
    tar::Archive::new(flate2::read::GzDecoder::new(bytes)).unpack(&staging)?;
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
        unpacked(&bytes, &dest, &top).unwrap();
        assert_eq!(std::fs::read(dest.join("blaze")).unwrap(), b"run");
        // a second fetch at once finds it there
        unpacked(&bytes, &dest, &top).unwrap();
        // what an earlier fetch left that can't be removed stops this one
        let elsewhere = root.join("home/versions/0.2.0");
        let staging = root.join(format!("home/versions/.{top}.{}", std::process::id()));
        std::fs::write(&staging, b"not a directory").unwrap();
        let error = unpacked(&bytes, &elsewhere, &top).unwrap_err();
        assert!(matches!(error.downcast_ref::<FetchError>(), Some(FetchError::Staging { .. })), "{error:#}");
        assert!(!elsewhere.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
