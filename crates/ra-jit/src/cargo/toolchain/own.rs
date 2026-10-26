//! blaze's own copy of the Rust release it builds with, for a machine with no rustup or whose
//! rustup does not have that release (docs/self-contained.md, "blaze's own toolchain"). It is
//! the release's `rustc`, `cargo`, `rust-std` and `rust-src` as the Rust project publishes them,
//! each checked against the sha256 `pins.txt` has for it (`harness/toolchain_pins.py` writes
//! that from the release's manifest), and laid out in `~/.blaze/toolchains/<release>-<host>`
//! as rustup lays out a toolchain, its records and channel manifest included, so that what
//! reads a sysroot reads this one unchanged.
//!
//! It is a stopgap: blaze still runs cargo and rustc for the first build and the proc macros
//! (docs/no-rustc.md). This gives them a home that no project's pin and no `PATH` changes; it
//! adds no place that starts either.

use std::{
    fs::File,
    io::{self, BufReader, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;

use crate::remote::{self, Https, Traffic};

/// The release, and the sha256 and size of each of its files blaze downloads, by host.
const PINS: &str = include_str!("pins.txt");

/// Where the Rust project publishes its releases, under the release's date.
const DIST: &str = "https://static.rust-lang.org/dist";

/// How many times a file is asked for before the download gives up. Each time after the first
/// goes on from where the one before stopped.
const TRIES: u32 = 4;

/// How long a download waits for the server to answer, or for the next of its bytes.
const PATIENCE: Duration = Duration::from_secs(60);

/// How many pieces of a file are held between the download and its unpacking.
const AHEAD: usize = 256;

pub(crate) struct Release {
    /// The toolchain as rustup names it, `nightly-2026-10-25`: what blaze's directory of it and
    /// its messages are called.
    pub(crate) version: &'static str,
    /// The channel its files are named with on the Rust project's site, `nightly`.
    channel: &'static str,
    date: &'static str,
    /// What its `rustc --version` says after `rustc `: `1.101.0-nightly (12433c307 2026-10-24)`.
    /// Pins without it are refused; the daemon reads it on every host ([`rustc_version`]).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) rustc: &'static str,
    host: &'static str,
    manifest: Pin,
    components: Vec<Component>,
}

#[derive(Clone, Copy)]
struct Pin {
    sha256: &'static str,
    bytes: u64,
}

struct Component {
    package: &'static str,
    /// `*` for one that is the same on every host, as `rust-src` is.
    target: &'static str,
    pin: Pin,
}

impl Component {
    fn file(&self, version: &str) -> String {
        match self.target {
            "*" => format!("{}-{version}.tar.xz", self.package),
            target => format!("{}-{version}-{target}.tar.xz", self.package),
        }
    }

    /// As rustup names it in a toolchain's records.
    fn name(&self) -> String {
        match self.target {
            "*" => self.package.to_owned(),
            target => format!("{}-{target}", self.package),
        }
    }
}

impl Release {
    /// The release blaze builds with, with the files of this host; `None` on a host the Rust
    /// project builds no toolchain for that blaze has pinned.
    pub(crate) fn ours() -> Option<Release> {
        Release::read(PINS, host()?)
    }

    fn read(pins: &'static str, host: &'static str) -> Option<Release> {
        let (mut release, mut manifest, mut components) = (None, None, Vec::new());
        let mut rustc = None;
        for line in pins.lines().filter(|it| !it.starts_with('#')) {
            if let Some(said) = line.strip_prefix("rustc ") {
                rustc = Some(said.trim());
                continue;
            }
            let words: Vec<&'static str> = line.split_whitespace().collect();
            match words[..] {
                ["release", version, date, channel] => release = Some((version, date, channel)),
                ["manifest", sha256, bytes] => {
                    manifest = Some(Pin { sha256, bytes: bytes.parse().ok()? })
                }
                [of, package, target, sha256, bytes] if of == host => {
                    let pin = Pin { sha256, bytes: bytes.parse().ok()? };
                    components.push(Component { package, target, pin });
                }
                _ => {}
            }
        }
        let ((version, date, channel), rustc, manifest) = (release?, rustc?, manifest?);
        (!components.is_empty()).then_some(Release {
            version,
            channel,
            date,
            rustc,
            host,
            manifest,
            components,
        })
    }

    /// What the download comes to, in megabytes as people count them.
    pub(crate) fn megabytes(&self) -> u64 {
        let bytes: u64 = self.components.iter().map(|it| it.pin.bytes).sum();
        (bytes + self.manifest.bytes + 500_000) / 1_000_000
    }

    /// Where blaze keeps this release.
    pub(crate) fn dir(&self) -> Option<PathBuf> {
        Some(home()?.join("toolchains").join(format!("{}-{}", self.version, self.host)))
    }
}

/// What the rustc of the toolchain blaze builds with says it is after `rustc `, on every host:
/// `1.101.0-nightly (12433c307 2026-10-24)`. A nightly's release number is shared by every
/// nightly of six weeks, so a rustc is told for blaze's by this whole line.
pub(crate) fn rustc_version() -> &'static str {
    PINS.lines()
        .find_map(|it| it.strip_prefix("rustc "))
        .map(str::trim)
        .expect("pins.txt has the rustc line: harness/toolchain_pins.py --write")
}

/// The host's tuple, as the Rust project names its builds.
fn host() -> Option<&'static str> {
    Some(match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "linux") if cfg!(target_env = "gnu") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") if cfg!(target_env = "gnu") => "aarch64-unknown-linux-gnu",
        ("x86_64", "windows") if cfg!(target_env = "msvc") => "x86_64-pc-windows-msvc",
        _ => return None,
    })
}

/// blaze's own directory: `BLAZE_HOME`, else `.blaze` in the user's home.
pub(crate) fn home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("BLAZE_HOME").filter(|it| !it.is_empty()) {
        return Some(PathBuf::from(home));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join(".blaze"))
}

/// Whether `dir` has a whole toolchain: what is moved into place is whole, so this tells one
/// from a directory someone else left there.
pub(crate) fn complete(dir: &Path) -> bool {
    let exe = std::env::consts::EXE_SUFFIX;
    let needed = [
        format!("bin/cargo{exe}"),
        format!("bin/rustc{exe}"),
        "lib/rustlib/multirust-channel-manifest.toml".to_owned(),
        "lib/rustlib/src/rust/library".to_owned(),
    ];
    needed.iter().all(|it| dir.join(it).exists())
}

/// Downloads `release` into `dir` unless it is there. Processes that ask at once download it
/// once: the others wait for the first. `Err` says what failed and what to do.
pub(crate) fn fetch(release: &Release, dir: &Path) -> Result<(), String> {
    fetch_from(release, dir, || Source::named(release.date))
}

fn fetch_from(
    release: &Release,
    dir: &Path,
    source: impl FnOnce() -> anyhow::Result<Source>,
) -> Result<(), String> {
    let failed = |e: &dyn std::fmt::Display| {
        format!(
            "blaze builds with Rust {0}, which it downloads once, and the download failed. \
             Running the command again tries again, going on from where a download stopped; on \
             a machine with no network, `rustup toolchain install {0} --component rust-src` \
             gives blaze the same Rust.\n  {e}",
            release.version
        )
    };
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err(failed(&"blaze's directory has no place for it"));
    };
    let name = name.to_string_lossy();
    std::fs::create_dir_all(parent).map_err(|e| failed(&e))?;
    let lock = File::create(parent.join(format!(".{name}.lock"))).map_err(|e| failed(&e))?;
    lock.lock().map_err(|e| failed(&e))?;
    if complete(dir) {
        return Ok(());
    }
    eprintln!(
        "Downloading Rust {} for blaze ({} MB, once)...",
        release.version,
        release.megabytes()
    );
    let start = Instant::now();
    let downloads = parent.join(format!(".{name}.download"));
    let staging = parent.join(format!(".{name}.unpacking"));
    let fetched = (|| {
        remove_dir(&staging)?;
        std::fs::create_dir_all(&downloads)?;
        std::fs::create_dir_all(&staging)?;
        let source = source()?;
        let manifest = download_all(release, &source, &downloads, &staging)?;
        lay_out(release, &manifest, &staging)?;
        remove_dir(dir)?;
        std::fs::rename(&staging, dir)?;
        remove_dir(&downloads)?;
        anyhow::Ok(())
    })();
    if let Err(e) = fetched {
        _ = remove_dir(&staging);
        return Err(match e.downcast_ref::<NotPublished>() {
            Some(it) => it.to_string(),
            None => failed(&format!("{e:#}")),
        });
    }
    eprintln!("Downloaded Rust {} for blaze in {:.1?}.", release.version, start.elapsed());
    Ok(())
}

fn remove_dir(dir: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// A file whose bytes are not the ones the Rust project published.
#[derive(Debug)]
struct NotPublished {
    file: String,
    version: &'static str,
}

impl std::fmt::Display for NotPublished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the download of Rust {}'s {} is not the file the Rust project published (its \
             sha256 differs), so blaze did not use it; running the command again downloads it \
             again",
            self.version, self.file
        )
    }
}

impl std::error::Error for NotPublished {}

/// Where the release's files come from: the Rust project's site, or what `RA_JIT_RUST_DIST`
/// names, an address or a directory with the files in it (a mirror, a machine with no network).
enum Source {
    Web(Https),
    Dir(PathBuf),
}

impl Source {
    fn named(date: &str) -> anyhow::Result<Source> {
        let given = std::env::var("RA_JIT_RUST_DIST").ok().filter(|it| !it.is_empty());
        if let Some(dir) = given.as_ref().filter(|it| !it.contains("://")) {
            return Ok(Source::Dir(PathBuf::from(dir)));
        }
        let address = given.unwrap_or_else(|| format!("{DIST}/{date}"));
        let address = url::Url::parse(&address)?;
        Ok(Source::Web(Https::open(&address, PATIENCE, Arc::new(Traffic::new(8)))?))
    }

    /// `file` from `offset` on.
    async fn read(&self, file: &str, offset: u64) -> anyhow::Result<Body> {
        let missing = || anyhow::anyhow!("{file} is not there");
        match self {
            Source::Web(web) => {
                Ok(Body::Web(web.read_from(file, offset).await?.ok_or_else(missing)?))
            }
            Source::Dir(dir) => {
                let mut opened = File::open(dir.join(file)).map_err(|_| missing())?;
                opened.seek(io::SeekFrom::Start(offset))?;
                Ok(Body::File(opened))
            }
        }
    }
}

enum Body {
    Web(hyper::body::Incoming),
    File(File),
}

impl Body {
    async fn next(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        match self {
            Body::Web(body) => Ok(tokio::time::timeout(PATIENCE, remote::https_next(body))
                .await
                .map_err(|_| anyhow::anyhow!("no bytes for {PATIENCE:?}"))??),
            Body::File(file) => {
                let mut bytes = vec![0; 1 << 16];
                let read = file.read(&mut bytes)?;
                bytes.truncate(read);
                Ok((read != 0).then_some(bytes))
            }
        }
    }
}

/// Downloads every component into `downloads` and unpacks each into a directory of its own
/// under `staging` as its bytes arrive, all at once. The result is the channel manifest.
fn download_all(
    release: &Release,
    source: &Source,
    downloads: &Path,
    staging: &Path,
) -> anyhow::Result<Vec<u8>> {
    let runtime = remote::runtime()?;
    let manifest_file = format!("channel-rust-{}.toml", release.channel);
    let unpacked = |at: usize| staging.join(".unpacked").join(at.to_string());
    let (manifest, fetched, unpacking) = std::thread::scope(|scope| {
        let mut downloading = Vec::new();
        let mut unpacking = Vec::new();
        for (at, component) in release.components.iter().enumerate() {
            let (to, from) = mpsc::channel(AHEAD);
            let into = unpacked(at);
            unpacking.push(scope.spawn(move || unpack(Pieces::new(from), &into)));
            let file = component.file(release.channel);
            let part = downloads.join(format!("{file}.part"));
            downloading.push(async move {
                let sent = download(source, &file, &part, component.pin, Some(to)).await;
                sent.map_err(|e| e.context(file))
            });
        }
        let manifest_part = downloads.join(&manifest_file);
        let (manifest, fetched) = runtime.block_on(async {
            let manifest = download(source, &manifest_file, &manifest_part, release.manifest, None);
            futures_util::future::join(manifest, futures_util::future::join_all(downloading)).await
        });
        let unpacking: Vec<_> = unpacking.into_iter().map(|it| it.join()).collect();
        (manifest.map(|()| manifest_part), fetched, unpacking)
    });
    drop(runtime);
    for (got, unpacked) in fetched.into_iter().zip(unpacking) {
        let unpacked = unpacked.map_err(|_| anyhow::anyhow!("unpacking panicked"))?;
        match got {
            Err(e) if e.is::<Unpacking>() => unpacked.map_err(anyhow::Error::from)?,
            Err(e) => return Err(e.into_published(release)),
            Ok(()) => unpacked?,
        }
    }
    let manifest = manifest.map_err(|e| e.into_published(release))?;
    Ok(std::fs::read(manifest)?)
}

/// The unpacking of a file stopped before its download did; why is the unpacking's to say.
#[derive(Debug)]
struct Unpacking;

impl std::fmt::Display for Unpacking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the unpacking stopped")
    }
}

impl std::error::Error for Unpacking {}

trait IntoPublished {
    fn into_published(self, release: &Release) -> anyhow::Error;
}

impl IntoPublished for anyhow::Error {
    /// Names the release in a [`NotPublished`].
    fn into_published(self, release: &Release) -> anyhow::Error {
        match self.downcast::<NotPublished>() {
            Ok(it) => NotPublished { version: release.version, ..it }.into(),
            Err(e) => e,
        }
    }
}

/// Downloads `file` into `part`, going on from what an earlier download left there, and sends
/// each piece on to `to`, what was there first included. Its sha256 is checked once it is
/// whole: a file that is not the one published is removed, and is [`NotPublished`].
async fn download(
    source: &Source,
    file: &str,
    part: &Path,
    pin: Pin,
    to: Option<mpsc::Sender<Vec<u8>>>,
) -> anyhow::Result<()> {
    let send = async |piece: Vec<u8>| match &to {
        Some(to) => to.send(piece).await.map_err(|_| anyhow::Error::new(Unpacking)),
        None => Ok(()),
    };
    let mut written = std::fs::OpenOptions::new().create(true).append(true).open(part)?;
    let mut hasher = Sha256::new();
    let mut have = written.metadata()?.len();
    if have > pin.bytes {
        written.set_len(0)?;
        have = 0;
    }
    let mut kept = BufReader::new(File::open(part)?).take(have);
    loop {
        let mut piece = vec![0; 1 << 16];
        let read = kept.read(&mut piece)?;
        if read == 0 {
            break;
        }
        piece.truncate(read);
        hasher.update(&piece);
        send(piece).await?;
    }
    let mut tries = 0;
    while have < pin.bytes {
        let got = async {
            let mut body = source.read(file, have).await?;
            while let Some(piece) = body.next().await? {
                if have + piece.len() as u64 > pin.bytes {
                    anyhow::bail!("it is longer than the {} bytes published", pin.bytes);
                }
                written.write_all(&piece)?;
                have += piece.len() as u64;
                hasher.update(&piece);
                send(piece).await?;
            }
            anyhow::Ok(())
        }
        .await;
        match got {
            Err(e) if e.is::<Unpacking>() => return Err(e),
            Ok(()) if have == pin.bytes => break,
            Ok(()) if tries + 1 == TRIES => {
                anyhow::bail!("it ended at {have} of its {} bytes", pin.bytes)
            }
            Err(e) if tries + 1 == TRIES => return Err(e),
            _ => tries += 1,
        }
        tokio::time::sleep(Duration::from_secs(tries.into())).await;
    }
    let got: String = hasher.finalize().iter().map(|it| format!("{it:02x}")).collect();
    if got != pin.sha256 {
        drop(written);
        _ = std::fs::remove_file(part);
        return Err(NotPublished { file: file.to_owned(), version: "" }.into());
    }
    Ok(())
}

/// What a download sends on, read as one stream.
struct Pieces {
    from: mpsc::Receiver<Vec<u8>>,
    piece: Vec<u8>,
    at: usize,
}

impl Pieces {
    fn new(from: mpsc::Receiver<Vec<u8>>) -> Pieces {
        Pieces { from, piece: Vec::new(), at: 0 }
    }
}

impl Read for Pieces {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        while self.at == self.piece.len() {
            match self.from.blocking_recv() {
                Some(piece) => (self.piece, self.at) = (piece, 0),
                None => return Ok(0),
            }
        }
        let n = into.len().min(self.piece.len() - self.at);
        into[..n].copy_from_slice(&self.piece[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Unpacks the `.tar.xz` `from` gives into `into`, and reads the rest of its stream, whose end
/// has the checks of what came before.
fn unpack(from: impl Read, into: &Path) -> io::Result<()> {
    std::fs::create_dir_all(into)?;
    let mut xz = lzma_rust2::XzReader::new(BufReader::with_capacity(1 << 20, from), false);
    tar::Archive::new(&mut xz).unpack(into)?;
    io::copy(&mut xz, &mut io::sink())?;
    Ok(())
}

/// Moves what each component unpacked into `toolchain` as rust-installer would, and writes the
/// records rustup keeps beside it, with `manifest`, the release's channel manifest.
fn lay_out(release: &Release, manifest: &[u8], toolchain: &Path) -> anyhow::Result<()> {
    let rustlib = toolchain.join("lib/rustlib");
    std::fs::create_dir_all(&rustlib)?;
    let unpacked = toolchain.join(".unpacked");
    let mut names = Vec::new();
    let mut config = "config_version = \"1\"\n".to_owned();
    for (at, component) in release.components.iter().enumerate() {
        let files = install(&unpacked.join(at.to_string()), toolchain)?;
        let name = component.name();
        let listed: String = files.iter().map(|it| format!("file:{it}\n")).collect();
        std::fs::write(rustlib.join(format!("manifest-{name}")), listed)?;
        config += &format!(
            "\n[[components]]\npkg = \"{}\"\ntarget = \"{}\"\nis_extension = {}\n",
            component.package,
            component.target,
            component.target == "*"
        );
        names.push(name);
    }
    std::fs::remove_dir_all(&unpacked)?;
    std::fs::write(rustlib.join("components"), names.join("\n") + "\n")?;
    std::fs::write(rustlib.join("rust-installer-version"), "3")?;
    std::fs::write(rustlib.join("multirust-config.toml"), config)?;
    std::fs::write(rustlib.join("multirust-channel-manifest.toml"), manifest)?;
    Ok(())
}

/// Moves the files and directories each component of the package unpacked at `unpacked` lists
/// in its `manifest.in` into `toolchain`. The result is the files, by their paths in it.
fn install(unpacked: &Path, toolchain: &Path) -> anyhow::Result<Vec<String>> {
    let mut entries = std::fs::read_dir(unpacked)?.collect::<Result<Vec<_>, _>>()?;
    let Some(top) = entries.pop().filter(|_| entries.is_empty()) else {
        anyhow::bail!("a package of the release is not one directory");
    };
    let top = top.path();
    let mut files = Vec::new();
    for component in std::fs::read_to_string(top.join("components"))?.lines() {
        let from = top.join(component.trim());
        for line in std::fs::read_to_string(from.join("manifest.in"))?.lines() {
            let (kind, path) = line.split_once(':').unwrap_or_default();
            let inside = Path::new(path)
                .components()
                .all(|it| matches!(it, std::path::Component::Normal(_)));
            if path.is_empty() || !inside {
                anyhow::bail!("{component} lists `{line}`, which is no place in a toolchain");
            }
            let to = toolchain.join(path);
            std::fs::create_dir_all(to.parent().unwrap_or(toolchain))?;
            std::fs::rename(from.join(path), &to)?;
            match kind {
                "file" => files.push(path.to_owned()),
                "dir" => list_files(&to, path, &mut files)?,
                _ => {
                    anyhow::bail!("{component} lists `{line}`, which rust-installer does not know")
                }
            }
        }
    }
    Ok(files)
}

fn list_files(dir: &Path, named: &str, files: &mut Vec<String>) -> io::Result<()> {
    let mut entries = std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|it| it.file_name());
    for entry in entries {
        let name = format!("{named}/{}", entry.file_name().to_string_lossy());
        if entry.file_type()?.is_dir() {
            list_files(&entry.path(), &name, files)?;
        } else {
            files.push(name);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A package as the Rust project makes them: one directory with `components`, and each
    /// component's files with its `manifest.in`, as a `.tar.xz`.
    fn package(name: &str, component: &str, files: &[(&str, &str)], dirs: &[&str]) -> Vec<u8> {
        let mut listed = String::new();
        for (path, _) in files {
            if !dirs.iter().any(|dir| path.starts_with(&format!("{dir}/"))) {
                listed += &format!("file:{path}\n");
            }
        }
        for dir in dirs {
            listed += &format!("dir:{dir}\n");
        }
        listing(name, component, files, &listed)
    }

    /// A package whose `manifest.in` is `listed`.
    fn listing(name: &str, component: &str, files: &[(&str, &str)], listed: &str) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        let mut add = |path: String, text: &str, mode: u32| {
            let mut header = tar::Header::new_gnu();
            header.set_size(text.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            tar.append_data(&mut header, path, text.as_bytes()).unwrap();
        };
        add(format!("{name}/components"), &format!("{component}\n"), 0o644);
        for (path, text) in files {
            let mode = if path.starts_with("bin/") { 0o755 } else { 0o644 };
            add(format!("{name}/{component}/{path}"), text, mode);
        }
        add(format!("{name}/{component}/manifest.in"), listed, 0o644);
        let tarred = tar.into_inner().unwrap();
        let mut xz = lzma_rust2::XzWriter::new(Vec::new(), Default::default()).unwrap();
        xz.write_all(&tarred).unwrap();
        xz.finish().unwrap()
    }

    fn sha256(bytes: &[u8]) -> &'static str {
        let hex: String = Sha256::digest(bytes).iter().map(|it| format!("{it:02x}")).collect();
        hex.leak()
    }

    /// A release of two packages, written as the Rust project's site has them into `dist`,
    /// and its pins.
    fn release(dist: &Path, rustc: &[u8]) -> Release {
        let src = package(
            "rust-src-9.9.9",
            "rust-src",
            &[("lib/rustlib/src/rust/library/core/lib.rs", "// core\n")],
            &["lib/rustlib/src/rust"],
        );
        let manifest = b"[pkg.rust]\ngit_commit_hash = \"abc\"\n";
        std::fs::create_dir_all(dist).unwrap();
        std::fs::write(dist.join("rustc-9.9.9-test-host.tar.xz"), rustc).unwrap();
        std::fs::write(dist.join("rust-src-9.9.9.tar.xz"), &src).unwrap();
        std::fs::write(dist.join("channel-rust-9.9.9.toml"), manifest).unwrap();
        let pins = format!(
            "# test\nrelease 9.9.9 2000-01-01 9.9.9\nrustc 9.9.9 (abc 2000-01-01)\nmanifest {} {}\n\
             test-host rustc test-host {} {}\ntest-host rust-src * {} {}\n\
             other-host rustc other-host {} 1\n",
            sha256(manifest),
            manifest.len(),
            sha256(rustc),
            rustc.len(),
            sha256(&src),
            src.len(),
            sha256(b"other"),
        );
        Release::read(pins.leak(), "test-host").unwrap()
    }

    fn rustc() -> Vec<u8> {
        package(
            "rustc-9.9.9-test-host",
            "rustc",
            &[
                ("bin/rustc", "#!/bin/sh\n"),
                ("bin/cargo", "#!/bin/sh\n"),
                ("lib/librustc_driver-0.so", ""),
                ("share/doc/rust/README.md", "read me\n"),
            ],
            &["share/doc/rust"],
        )
    }

    fn fetched(dist: &Path, release: &Release, dir: &Path) -> Result<(), String> {
        fetch_from(release, dir, || Ok(Source::Dir(dist.to_owned())))
    }

    #[test]
    fn pins_are_of_the_host_and_the_release_blaze_builds_with() {
        let ours = Release::read(PINS, "x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(ours.version, super::super::OURS.trim(), "harness/toolchain_pins.py --write");
        assert_eq!(ours.channel, "nightly");
        assert!(ours.rustc.contains("-nightly ("), "{}", ours.rustc);
        let packages: Vec<_> = ours.components.iter().map(|it| it.package).collect();
        assert_eq!(packages, ["rustc", "cargo", "rust-std", "rust-src"]);
        assert_eq!(ours.components[3].file(ours.channel), "rust-src-nightly.tar.xz");
        assert!(Release::read(PINS, "riscv64gc-unknown-linux-gnu").is_none());
        for host in ["aarch64-apple-darwin", "aarch64-unknown-linux-gnu", "x86_64-pc-windows-msvc"]
        {
            assert_eq!(Release::read(PINS, host).unwrap().components.len(), 4, "{host}");
        }
    }

    #[test]
    fn laid_out_as_rustup_lays_out_a_toolchain() {
        let work = tempfile::tempdir().unwrap();
        let dist = work.path().join("dist");
        let release = release(&dist, &rustc());
        let dir = work.path().join("toolchains/9.9.9-test-host");
        fetched(&dist, &release, &dir).unwrap();
        assert!(complete(&dir));
        let read = |path: &str| std::fs::read_to_string(dir.join(path)).unwrap();
        assert_eq!(read("lib/rustlib/components"), "rustc-test-host\nrust-src\n");
        assert_eq!(
            read("lib/rustlib/manifest-rustc-test-host"),
            "file:bin/rustc\nfile:bin/cargo\nfile:lib/librustc_driver-0.so\nfile:share/doc/rust/README.md\n"
        );
        assert_eq!(
            read("lib/rustlib/manifest-rust-src"),
            "file:lib/rustlib/src/rust/library/core/lib.rs\n"
        );
        assert_eq!(
            read("lib/rustlib/multirust-channel-manifest.toml"),
            "[pkg.rust]\ngit_commit_hash = \"abc\"\n"
        );
        assert_eq!(read("lib/rustlib/rust-installer-version"), "3");
        assert!(
            read("lib/rustlib/multirust-config.toml")
                .contains("pkg = \"rust-src\"\ntarget = \"*\"\nis_extension = true")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join("bin/rustc")).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "rustc can be run");
        }
        let left: Vec<_> = std::fs::read_dir(work.path().join("toolchains"))
            .unwrap()
            .map(|it| it.unwrap().file_name().into_string().unwrap())
            .filter(|it| !it.ends_with(".lock"))
            .collect();
        assert_eq!(left, ["9.9.9-test-host"], "nothing of the download is left beside it");
        // Whole, it is not downloaded again: the files it came from can be gone.
        std::fs::remove_dir_all(&dist).unwrap();
        fetched(&dist, &release, &dir).unwrap();
    }

    #[test]
    fn a_file_that_is_not_the_published_one_is_refused() {
        let work = tempfile::tempdir().unwrap();
        let dist = work.path().join("dist");
        let release = release(&dist, &rustc());
        let mut other = std::fs::read(dist.join("rustc-9.9.9-test-host.tar.xz")).unwrap();
        let last = other.len() - 1;
        other[last] ^= 1;
        std::fs::write(dist.join("rustc-9.9.9-test-host.tar.xz"), &other).unwrap();
        let dir = work.path().join("toolchains/9.9.9-test-host");
        let said = fetched(&dist, &release, &dir).unwrap_err();
        assert!(
            said.contains("Rust 9.9.9's rustc-9.9.9-test-host.tar.xz is not the file the Rust project published"),
            "{said}"
        );
        assert!(!dir.exists(), "nothing is put in place");
        let part = work
            .path()
            .join("toolchains/.9.9.9-test-host.download/rustc-9.9.9-test-host.tar.xz.part");
        assert!(!part.exists(), "what was downloaded is not gone on with");
    }

    #[test]
    fn a_download_that_stopped_goes_on_from_where_it_stopped() {
        let work = tempfile::tempdir().unwrap();
        let dist = work.path().join("dist");
        let whole = rustc();
        let release = release(&dist, &whole);
        let downloads = work.path().join("toolchains/.9.9.9-test-host.download");
        std::fs::create_dir_all(&downloads).unwrap();
        std::fs::write(
            downloads.join("rustc-9.9.9-test-host.tar.xz.part"),
            &whole[..whole.len() / 2],
        )
        .unwrap();
        // The site has only the rest: a download from the start would not be the file.
        let mut rest = vec![0; whole.len() / 2];
        rest.extend_from_slice(&whole[whole.len() / 2..]);
        std::fs::write(dist.join("rustc-9.9.9-test-host.tar.xz"), &rest).unwrap();
        let dir = work.path().join("toolchains/9.9.9-test-host");
        fetched(&dist, &release, &dir).unwrap();
        assert!(complete(&dir));
    }

    #[test]
    fn a_package_that_names_a_place_outside_the_toolchain_is_refused() {
        let work = tempfile::tempdir().unwrap();
        let dist = work.path().join("dist");
        let bad =
            listing("rustc-9.9.9-test-host", "rustc", &[("bin/rustc", "")], "file:../escaped\n");
        let release = release(&dist, &bad);
        let dir = work.path().join("toolchains/9.9.9-test-host");
        let said = fetched(&dist, &release, &dir).unwrap_err();
        assert!(said.contains("the download failed"), "{said}");
        assert!(!work.path().join("toolchains/escaped").exists());
        assert!(!dir.exists());
    }
}
