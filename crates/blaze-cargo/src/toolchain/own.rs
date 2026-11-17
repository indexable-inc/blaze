//! blaze's own copy of the Rust release it builds with, for a machine with no rustup or whose
//! rustup does not have that release (docs/v2/plan.md, "blaze's own toolchain"). It is
//! the release's `rustc`, `cargo` and `rust-std` as the Rust project publishes them (the source
//! of the standard library is blaze's own, `library.rs`), each checked against the sha256 `pins.txt` has for it (`harness/toolchain_pins.py` writes
//! that from the release's manifest), and laid out in `~/.blaze/toolchains/<release>-<host>`
//! (the machine's, whatever `BLAZE_HOME` says, or where `BLAZE_TOOLCHAINS` names:
//! [`toolchains`]) as rustup lays out a toolchain,
//! its records and channel manifest included, so that what reads a sysroot reads this one
//! unchanged.
//!
//! It is a stopgap: blaze still runs cargo and rustc for the first build and the proc macros
//! (docs/no-rustc.md). This gives them a home that no project's pin and no `PATH` changes; it
//! adds no place that starts either.

use std::{
    alloc::Global,
    fs::File,
    io::{self, BufReader, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use blaze_core::wait::{Clock as _, Deadline, Monotonic, Parker, Waited};
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};

use blaze_registry::http::{Bounds, HttpNotSuccessful, HyperHttp};

#[blaze::sans_io]
mod keep;

use keep::Keep;

/// The release, and the sha256 and size of each of its files blaze downloads, by host.
const PINS: &str = include_str!("pins.txt");

/// Where the Rust project publishes its releases, under the release's date.
const DIST: &str = "https://static.rust-lang.org/dist";

/// How many pieces of a file are held between the download and its unpacking: the most the
/// download is ahead of it, [`AHEAD`] times [`PIECE`] bytes (16 MiB), a bounded crossbeam
/// channel ([`handoff`]).
const AHEAD: usize = 256;

/// The longest the download of a file waits for its unpacking to take a piece, and the
/// unpacking for the download to give one: far past any pause of a working stage (a piece is
/// 64 KiB of xz; a download with no byte for blaze-fetch's stall bound, 30 s, is stopped by the
/// HTTP client, which ends the unpacking's wait), so only a stage that stopped reaches it.
const STAGE_WAIT: Duration = Duration::from_secs(300);

pub struct Release {
    /// The toolchain as rustup names it, `nightly-2026-10-25`: what blaze's directory of it and
    /// its messages are called.
    pub version: &'static str,
    /// The channel its files are named with on the Rust project's site, `nightly`.
    channel: &'static str,
    date: &'static str,
    /// What its `rustc --version` says after `rustc `: `1.101.0-nightly (12433c307 2026-10-24)`.
    /// Pins without it are refused; the daemon reads it on every host ([`rustc_version`]).
    #[cfg_attr(not(test), allow(dead_code))]
    pub rustc: &'static str,
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
    /// The target it is for: the host's, or wasm32-wasip1 for the C runtime blaze's linker links
    /// proc macros' modules with (wasi-libc). Every package blaze pins is of one target, so its files
    /// are all named `<package>-<version>-<target>`.
    target: &'static str,
    pin: Pin,
}

impl Component {
    fn file(&self, version: &str) -> String {
        format!("{}-{version}-{}.tar.xz", self.package, self.target)
    }

    /// As rustup names it in a toolchain's records.
    fn name(&self) -> String {
        format!("{}-{}", self.package, self.target)
    }
}

impl Release {
    /// The release blaze builds with, with the files of this host; `None` on a host the Rust
    /// project builds no toolchain for that blaze has pinned.
    pub fn ours() -> Option<Release> {
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
    pub fn megabytes(&self) -> u64 {
        (self.bytes() + 500_000) / 1_000_000
    }

    /// What the download comes to, in bytes: each pinned file and the manifest.
    pub fn bytes(&self) -> u64 {
        self.components.iter().map(|it| it.pin.bytes).sum::<u64>() + self.manifest.bytes
    }

    /// Where blaze keeps this release: under this machine's [`toolchains`], whatever
    /// `BLAZE_HOME` says.
    ///
    /// # Errors
    ///
    /// As [`toolchains`].
    pub fn dir(&self) -> Result<PathBuf, String> {
        Ok(toolchains()?.join(format!("{}-{}", self.version, self.host)))
    }
}

/// What the rustc of the toolchain blaze builds with says it is after `rustc `, on every host:
/// `1.101.0-nightly (12433c307 2026-10-24)`. A nightly's release number is shared by every
/// nightly of six weeks, so a rustc is told for blaze's by this whole line.
pub fn rustc_version() -> &'static str {
    PINS.lines()
        .find_map(|it| it.strip_prefix("rustc "))
        .map(str::trim)
        .expect("pins.txt has the rustc line: harness/toolchain_pins.py '{\"write\": true}'")
}

/// The commit of rust-lang/rust the toolchain blaze builds with was built from: what a
/// nightly's channel manifest names only by the short hash in its version.
pub fn commit() -> &'static str {
    PINS.lines()
        .find_map(|it| it.strip_prefix("commit "))
        .map(str::trim)
        .expect("pins.txt has the commit line: harness/toolchain_pins.py '{\"write\": true}'")
}

/// The host's tuple, as the Rust project names its builds.
pub fn host() -> Option<&'static str> {
    Some(match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "linux") if cfg!(target_env = "gnu") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") if cfg!(target_env = "gnu") => "aarch64-unknown-linux-gnu",
        ("x86_64", "windows") if cfg!(target_env = "msvc") => "x86_64-pc-windows-msvc",
        _ => return None,
    })
}

/// Where this machine keeps blaze's toolchains: `.blaze/toolchains` in the user's home, for
/// every `BLAZE_HOME`. A release is its pinned bytes, each file checked against its sha256 as
/// it is downloaded, so every home of the machine reads the one copy: `BLAZE_HOME` moves a
/// daemon's settings and state, never what it builds with. A home of its own (a harness's, a
/// CI job's, a second daemon's) neither downloads the release again nor fails for not having
/// it. [`fetch`]'s lock keeps two processes from laying it out at once.
///
/// [`VAR`] (`BLAZE_TOOLCHAINS`) names another directory: for a machine whose home is small,
/// read-only or not there (a CI runner, a sandbox, a unit of a user with no home). The home is
/// [`blaze_core::home::user`]'s (`HOME`, else the system's record of the user), so a unit
/// with no `HOME` finds the same copy a shell does. A relative one is taken against the
/// current directory.
///
/// # Errors
///
/// What blaze keeps there, why no home was found (what was tried) and that [`VAR`] names a
/// directory; or the current directory, when a relative one cannot be taken against it.
pub fn toolchains() -> Result<PathBuf, String> {
    let dir = toolchains_in(|name| std::env::var_os(name), blaze_core::home::record).map_err(|why| {
        // alloc: error path
        format!(
            "blaze keeps the Rust it builds with in .blaze/toolchains in your home directory, \
             shared by every BLAZE_HOME, and {why}; {VAR} names another directory to keep it in"
        )
    })?;
    std::path::absolute(&dir).map_err(|error| {
        // alloc: error path
        format!("blaze's toolchains directory {} against the current directory: {error}", dir.display())
    })
}

/// The one setting for where blaze's toolchains are kept ([`toolchains`]): a directory, in
/// place of `.blaze/toolchains` in the user's home. Like that default it is the machine's,
/// not a `BLAZE_HOME`'s: every home that sets the same one reads the one copy. The daemon
/// keeps the one it was started with (`serve::settings::STARTED_WITH`).
pub const VAR: &str = "BLAZE_TOOLCHAINS";

/// [`toolchains`]' decision, with no IO of its own: [`VAR`] when `var` has it set and not
/// empty, else `.blaze/toolchains` in the user's home, which is [`blaze_core::home::decide`]
/// of `var` and `record` (the system's record of the user, asked only when neither [`VAR`]
/// nor `HOME` answers).
///
/// # Errors
///
/// [`blaze_core::home::NoHome`] when [`VAR`] is unset and the user has no home.
pub fn toolchains_in(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    record: impl FnOnce() -> Result<PathBuf, blaze_core::home::NoRecord>,
) -> Result<PathBuf, blaze_core::home::NoHome> {
    if let Some(named) = var(VAR).filter(|it| !it.is_empty()) {
        return Ok(PathBuf::from(named));
    }
    Ok(blaze_core::home::decide(&var, record)?.join(".blaze").join("toolchains"))
}

/// Whether `dir` has a whole toolchain: what is moved into place is whole, so this tells one
/// from a directory someone else left there.
fn whole(dir: &Path) -> bool {
    let exe = std::env::consts::EXE_SUFFIX;
    let needed = [
        format!("bin/cargo{exe}"),
        format!("bin/rustc{exe}"),
        "lib/rustlib/multirust-channel-manifest.toml".to_owned(),
    ];
    needed.iter().all(|it| dir.join(it).exists())
}

/// Whether `dir` is a whole toolchain with every component `release` pins: one downloaded
/// before a component was pinned is downloaded again.
pub fn complete(release: &Release, dir: &Path) -> bool {
    let Ok(has) = std::fs::read_to_string(dir.join("lib/rustlib/components")) else {
        return false;
    };
    let has: Vec<&str> = has.lines().collect();
    whole(dir) && release.components.iter().all(|it| has.contains(&it.name().as_str()))
}

/// Downloads `release` into `dir` unless it is there. Processes that ask at once download it
/// once: the others wait for the first. `Err` says what failed and what to do.
pub fn fetch(release: &Release, dir: &Path) -> Result<(), String> {
    fetch_from(release, dir, || Source::named(release.date))
}

fn fetch_from(
    release: &Release,
    dir: &Path,
    source: impl FnOnce() -> anyhow::Result<Source>,
) -> Result<(), String> {
    // dyn: what failed, said once when the download fails.
    let failed = |e: &dyn std::fmt::Display| {
        format!(
            "blaze builds with Rust {0}, which it downloads once, and the download failed. \
             Running the command again tries again, going on from where a download stopped; on \
             a machine with no network, `rustup toolchain install {0}` \
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
    // Held until the download is done, and let go of then even while a child spawned meanwhile
    // still has the file open.
    let _lock = stdx::file_lock::Locked::lock(lock).map_err(|e| failed(&e))?;
    if complete(release, dir) {
        return Ok(());
    }
    eprintln!(
        "Downloading Rust {} for blaze ({} MB, once)...",
        release.version,
        release.megabytes()
    );
    let start = Instant::now();
    // A span in the client's flight recorder, which the client's rows take to the history.
    let _span = tracing::info_span!(target: "blaze", "toolchain download", toolchain = release.version, bytes = release.bytes()).entered();
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
    for (old, name) in superseded(release, parent) {
        remove_dir(&old).map_err(|e| {
            // alloc: error path, said once.
            format!(
                "blaze downloaded Rust {} and could not remove {}, which an older release of blaze \
                 built with and nothing uses: {e}",
                release.version,
                old.display()
            )
        })?;
        _ = std::fs::remove_file(parent.join(format!(".{name}.lock")));
        eprintln!("Removed {name}, which an older release of blaze built with.");
    }
    Ok(())
}

/// blaze's own copies of older nightlies than `release`, for its host, beside it in `parent`.
/// The pin moves every day (docs/toolchain.md, "The daily bump"), and each release of blaze
/// downloads its own once: what an older release downloaded is not used again. A newer one, one
/// of another host and a directory that is not a whole toolchain are left alone.
fn superseded(release: &Release, parent: &Path) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(parent) else { return Vec::new() };
    let (prefix, suffix) = (format!("{}-", release.channel), format!("-{}", release.host));
    let mut old = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else { continue };
        let Some(date) = name.strip_prefix(&prefix).and_then(|it| it.strip_suffix(&suffix)) else {
            continue;
        };
        let dated = date.len() == 10
            && date
                .bytes()
                .enumerate()
                .all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() });
        if dated && date < release.date && whole(&entry.path()) {
            old.push((entry.path(), name));
        }
    }
    old.sort();
    old
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

/// Where the release's files come from: the Rust project's site, or what `BLAZE_RUST_DIST`
/// names, an address or a directory with the files in it (a mirror, a machine with no network).
enum Source {
    Web { http: Arc<HyperHttp>, base: url::Url },
    Dir(PathBuf),
}

impl Source {
    fn named(date: &str) -> anyhow::Result<Source> {
        let given = super::given_var("BLAZE_RUST_DIST")?;
        if let Some(dir) = given.as_ref().filter(|it| !it.contains("://")) {
            return Ok(Source::Dir(PathBuf::from(dir)));
        }
        let address = given.unwrap_or_else(|| format!("{DIST}/{date}"));
        Ok(Source::Web { http: HyperHttp::shared()?, base: super::base(&address)? })
    }

    /// Where `file` is read from: its address, or its path.
    fn place(&self, file: &str) -> String {
        match self {
            Source::Web { base, .. } => base.join(file).map_or_else(|_| format!("{base}{file}"), String::from),
            Source::Dir(dir) => dir.join(file).display().to_string(),
        }
    }

    /// `file` from byte `from` on, asked once (rule no-fallback: no retry), each piece handed
    /// to `each` with its offset in the whole file, in order. A server that ignores the range
    /// sends the whole from 0, which `each` is given as it comes ([`Keep::piece`] leaves out
    /// what is had). An error of `each` stops the transfer and is the result, as it was.
    fn read(
        &self,
        file: &str,
        from: u64,
        each: &mut (dyn FnMut(u64, &[u8]) -> anyhow::Result<()> + Send),
    ) -> anyhow::Result<()> {
        match self {
            Source::Web { http, base } => {
                let url = base.join(file)?;
                // The keeper's own error (the unpacking stopped or is stuck, a write failed,
                // the file is longer than published), kept so that it is the result, not the
                // client's message of a sink that refused a piece.
                let mut kept_error = None;
                let mut sink = |offset: u64, bytes: &[u8]| -> io::Result<()> {
                    for (at, piece) in (0u64..).step_by(PIECE).zip(bytes.chunks(PIECE)) {
                        if let Err(error) = each(offset + at, piece) {
                            kept_error = Some(error);
                            return Err(io::Error::other("the download's piece was not kept"));
                        }
                    }
                    Ok(())
                };
                // In parallel ranges, said in blaze-fetch's downloads (the status line, `blaze
                // status`, the build's stall bound) and stopped after its stall bound without
                // a byte: a blocking call, parked on blaze-core's deadline wait by the client.
                let answer =
                    http.stream_watched(url.as_str(), &[], from, Bounds::NONE, &mut sink, file, &mut |_| {});
                if let Some(error) = kept_error {
                    return Err(error);
                }
                let answer = answer?;
                match answer.status {
                    200 | 206 => Ok(()),
                    404 | 410 => anyhow::bail!("{url} answered {}: the server has no such file", answer.status),
                    _ => Err(HttpNotSuccessful::new(url.as_str(), &answer).into()),
                }
            }
            Source::Dir(dir) => {
                let path = dir.join(file);
                let mut opened = File::open(&path).map_err(|e| match e.kind() {
                    io::ErrorKind::NotFound => anyhow::anyhow!("{} is not there", path.display()),
                    _ => anyhow::anyhow!("opening {}: {e}", path.display()),
                })?;
                opened.seek(io::SeekFrom::Start(from))?;
                let mut piece = Vec::with_capacity_in(PIECE, Global);
                piece.resize(PIECE, 0);
                // Bounded by the file's length, and by the pin's: `each` refuses a byte past it.
                let mut at = from;
                loop {
                    let read = opened.read(&mut piece)?;
                    if read == 0 {
                        return Ok(());
                    }
                    each(at, &piece[..read])?;
                    at += read as u64;
                }
            }
        }
    }
}

/// The bytes of a piece of a file a download gives on: what is read of a file at once, and the
/// most a server's piece is handed on in.
const PIECE: usize = 1 << 16;

/// Downloads every component into `downloads` and unpacks each into a directory of its own
/// under `staging` as its bytes arrive, all at once: each component's download and its
/// unpacking on threads of their own, the manifest on this one. The result is the channel
/// manifest.
fn download_all(
    release: &Release,
    source: &Source,
    downloads: &Path,
    staging: &Path,
) -> anyhow::Result<Vec<u8>> {
    let manifest_file = format!("channel-rust-{}.toml", release.channel);
    let manifest_part = downloads.join(&manifest_file);
    let unpacked = |at: usize| staging.join(".unpacked").join(at.to_string());
    let (manifest, joined) = std::thread::scope(|scope| -> anyhow::Result<_> {
        let mut started = Vec::with_capacity_in(release.components.len(), Global);
        for (at, component) in release.components.iter().enumerate() {
            let file = component.file(release.channel);
            let (feed, pieces) = handoff(&file, crossbeam_channel::bounded(AHEAD), STAGE_WAIT);
            let into = unpacked(at);
            let unpacking = std::thread::Builder::new()
                .name(format!("toolchain: unpack {file}"))
                .spawn_scoped(scope, move || unpack(pieces, &into))?;
            let part = downloads.join(format!("{file}.part"));
            let named = file.clone(); // clone: the file's name, for its download's thread
            let downloading = std::thread::Builder::new()
                .name(format!("toolchain: download {file}"))
                .spawn_scoped(scope, move || {
                    let sent = download(source, &named, &part, component.pin, Some(&feed));
                    // Its end, said to the unpacking: the stream is whole, or stopped.
                    drop(feed);
                    sent.map_err(|e| e.context(super::DownloadFailed::of(&named, source.place(&named))))
                })?;
            started.push((file, downloading, unpacking));
        }
        let manifest = download(source, &manifest_file, &manifest_part, release.manifest, None)
            .map_err(|e| e.context(super::DownloadFailed::of(&manifest_file, source.place(&manifest_file))));
        let mut joined = Vec::with_capacity_in(started.len(), Global);
        for (file, downloading, unpacking) in started {
            joined.push((file, downloading.join(), unpacking.join()));
        }
        Ok((manifest, joined))
    })?;
    for (file, got, unpacked) in joined {
        let got = got.map_err(|_| anyhow::anyhow!("the download of {file} panicked"))?;
        let unpacked = unpacked.map_err(|_| anyhow::anyhow!("the unpacking of {file} panicked"))?;
        let unpacked = || unpacked.map_err(|e| anyhow::Error::new(e).context(format!("unpacking {file}")));
        match got {
            Err(e) if e.is::<Unpacking>() => {
                unpacked()?;
                anyhow::bail!("the unpacking of {file} ended before its download did: the archive ended early")
            }
            Err(e) => return Err(e.into_published(release)),
            Ok(()) => unpacked()?,
        }
    }
    manifest.map_err(|e| e.into_published(release))?;
    Ok(std::fs::read(manifest_part)?)
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

/// One stage of a file's download waited [`STAGE_WAIT`] for the other and it did nothing:
/// that stage is stuck (rule bounded-progress).
#[derive(Debug)]
struct Stuck {
    file: String,
    /// The stage waited on: `the unpacking` or `the download`.
    stage: &'static str,
    /// What it was to do.
    to: &'static str,
    within: Duration,
}

impl std::fmt::Display for Stuck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Stuck { file, stage, to, within } = self;
        write!(
            f,
            "{stage} of {file} did not {to} in {within:?}, so it is stuck and blaze stopped the \
             download (rule bounded-progress)"
        )
    }
}

impl std::error::Error for Stuck {}

/// The pieces of one file between its download ([`Feed`]) and its unpacking ([`Pieces`]):
/// `channel`, a bounded crossbeam channel (the download is at most its bound ahead), and a
/// [`Parker`] on each side, so that each side's wait is blaze-core's deadline wait, named and
/// bounded by `within`. Each side unparks the other after each piece it moves and when it goes.
fn handoff(file: &str, channel: (Sender<Vec<u8>>, Receiver<Vec<u8>>), within: Duration) -> (Feed, Pieces) {
    let (to, from) = channel;
    let (download, unpacking) = (Arc::new_in(Parker::new(), Global), Arc::new_in(Parker::new(), Global));
    let feed = Feed {
        to,
        held: Arc::clone(&download),
        reader: Wakes(Arc::clone(&unpacking)),
        file: file.to_owned(),
        within,
    };
    let pieces = Pieces {
        from,
        waiting: unpacking,
        feed: Wakes(download),
        file: file.to_owned(),
        within,
        piece: Vec::new_in(Global),
        at: 0,
    };
    (feed, pieces)
}

/// Unparks the other side of a [`handoff`] when told, and when it goes: declared after the
/// channel's end in its side, so that end is gone first and the other side sees it.
struct Wakes(Arc<Parker, Global>);

impl Wakes {
    fn wake(&self) {
        self.0.unpark();
    }
}

impl Drop for Wakes {
    fn drop(&mut self) {
        self.0.unpark();
    }
}

/// A download's side of a [`handoff`].
struct Feed {
    to: Sender<Vec<u8>>,
    /// Parked on by the download while the channel is full.
    held: Arc<Parker, Global>,
    reader: Wakes,
    file: String,
    within: Duration,
}

impl Feed {
    /// Hands `piece` to the unpacking, waiting while it is [`AHEAD`] pieces behind: at most
    /// `within`, then [`Stuck`]. [`Unpacking`] once the unpacking is gone.
    fn give(&self, mut piece: Vec<u8>) -> anyhow::Result<()> {
        let clock = Monotonic::new();
        let deadline = Deadline::after(clock.now(), self.within);
        // Bounded by `deadline`: each turn hands the piece on, or parks until the unpacking
        // takes one (or goes) or the deadline passes, which ends the loop.
        loop {
            match self.to.try_send(piece) {
                Ok(()) => {
                    self.reader.wake();
                    return Ok(());
                }
                Err(TrySendError::Disconnected(_)) => return Err(Unpacking.into()),
                Err(TrySendError::Full(back)) => piece = back,
            }
            let what = "a toolchain file's unpacking to take the next piece of its download";
            match self.held.park(what, Some(deadline), &clock)? {
                Waited::Ready(()) => {}
                Waited::Deadline => {
                    let (file, within) = (self.file.clone(), self.within); // clone: an error's name
                    let to = "take a piece of its download";
                    return Err(Stuck { file, stage: "the unpacking", to, within }.into());
                }
            }
        }
    }
}

/// Downloads `file` into `part`, going on from what an earlier download left there, and gives
/// each piece on to `to`, what was there first included: the driver of [`Keep`], which decides
/// what is kept. The source is asked once (rule no-fallback); a download that ends short is an
/// error saying how far it got. Its sha256 is checked once it is whole: a file that is not the
/// one published is removed, and is [`NotPublished`].
fn download(source: &Source, file: &str, part: &Path, pin: Pin, to: Option<&Feed>) -> anyhow::Result<()> {
    let give = |piece: &[u8]| match to {
        Some(to) if !piece.is_empty() => to.give(piece.to_vec_in(Global)),
        _ => Ok(()),
    };
    let mut keep = Keep::new(pin.sha256, pin.bytes)?;
    let mut written = std::fs::OpenOptions::new().create(true).append(true).open(part)?;
    let have = keep.resume(written.metadata()?.len());
    written.set_len(have)?;
    let mut kept = BufReader::new(File::open(part)?).take(have);
    let mut piece = Vec::with_capacity_in(PIECE, Global);
    piece.resize(PIECE, 0);
    // Bounded by `have`, the part file's bytes gone on from.
    loop {
        let read = kept.read(&mut piece)?;
        if read == 0 {
            break;
        }
        keep.kept(&piece[..read])?;
        give(&piece[..read])?;
    }
    if !keep.whole() {
        let from = keep.have();
        source.read(file, from, &mut |offset, bytes| {
            let new = keep.piece(offset, bytes)?;
            written.write_all(new)?;
            give(new)
        })?;
    }
    match keep.end() {
        Ok(()) => Ok(()),
        Err(keep::Refused::NotPublished) => {
            drop(written);
            _ = std::fs::remove_file(part);
            Err(NotPublished { file: file.to_owned(), version: "" }.into())
        }
        Err(refused) => Err(refused.into()),
    }
}

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

/// An unpacking's side of a [`handoff`]: what the download gives, read as one stream.
struct Pieces {
    from: Receiver<Vec<u8>>,
    /// Parked on by the unpacking while the channel is empty.
    waiting: Arc<Parker, Global>,
    /// Declared after `from`, so the download sees the channel gone when it is woken.
    feed: Wakes,
    file: String,
    within: Duration,
    piece: Vec<u8>,
    at: usize,
}

impl Pieces {
    /// The next piece the download gives, waiting while none is here: at most `within`, then
    /// [`Stuck`]. `None` once the download is gone and every piece is read.
    fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        let clock = Monotonic::new();
        let deadline = Deadline::after(clock.now(), self.within);
        // Bounded by `deadline`: each turn takes a piece, or parks until the download gives
        // one (or goes) or the deadline passes, which ends the loop.
        loop {
            match self.from.try_recv() {
                Ok(piece) => {
                    self.feed.wake();
                    return Ok(Some(piece));
                }
                Err(TryRecvError::Disconnected) => return Ok(None),
                Err(TryRecvError::Empty) => {}
            }
            let what = "a toolchain file's download to give its next piece (bounded by the HTTP client's stall bound)";
            match self.waiting.park(what, Some(deadline), &clock)? {
                Waited::Ready(()) => {}
                Waited::Deadline => {
                    let (file, within) = (self.file.clone(), self.within); // clone: an error's name
                    let to = "give its next piece";
                    return Err(io::Error::other(Stuck { file, stage: "the download", to, within }));
                }
            }
        }
    }
}

impl Read for Pieces {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        // Bounded by the download's pieces: each turn takes one or ends.
        while self.at == self.piece.len() {
            match self.next()? {
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
            component.target != release.host
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

    /// The invariant: blaze's toolchains are where `BLAZE_TOOLCHAINS` names, else in
    /// `.blaze/toolchains` of the user's home (`HOME`, else the system's record), whatever
    /// `BLAZE_HOME` says; with neither, an error that says what was tried. The record is
    /// asked only when no variable answers.
    #[test]
    fn toolchains_are_where_the_setting_names_else_in_the_users_home() {
        use blaze_core::home::{NoHome, NoRecord};
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| std::ffi::OsString::from(*value))
            }
        };
        let none = || Err(NoRecord::NoEntry { uid: 0 });
        let recorded = || Ok(PathBuf::from("/root"));
        let unasked = || -> Result<PathBuf, NoRecord> { panic!("a variable answered, and the user database was asked") };
        // `HOME` set, the setting not: the default, and `BLAZE_HOME` moves nothing.
        let home = PathBuf::from("/u/.blaze/toolchains");
        assert_eq!(toolchains_in(env(&[("HOME", "/u")]), unasked), Ok(home.clone())); // clone: a test's expected path
        assert_eq!(toolchains_in(env(&[("HOME", "/u"), ("BLAZE_HOME", "/b")]), unasked), Ok(home));
        // `HOME` empty or unset, and the system has a record (the unit that found this:
        // `BLAZE_HOME=/root/bh`, no `HOME`, uid 0).
        let unit = PathBuf::from("/root/.blaze/toolchains");
        assert_eq!(toolchains_in(env(&[("HOME", ""), ("BLAZE_HOME", "/root/bh")]), recorded), Ok(unit.clone())); // clone: a test's expected path
        assert_eq!(toolchains_in(env(&[("BLAZE_HOME", "/root/bh")]), recorded), Ok(unit));
        // Unset, and the system has none: the error says both.
        let refused = toolchains_in(env(&[("BLAZE_HOME", "/root/bh")]), none).unwrap_err();
        assert_eq!(refused, NoHome { record: NoRecord::NoEntry { uid: 0 } });
        // The setting wins over the home, needs none, and an empty one is unset.
        let named = PathBuf::from("/t");
        assert_eq!(toolchains_in(env(&[(VAR, "/t"), ("HOME", "/u")]), unasked), Ok(named.clone())); // clone: a test's expected path
        assert_eq!(toolchains_in(env(&[(VAR, "/t")]), unasked), Ok(named));
        assert_eq!(toolchains_in(env(&[(VAR, ""), ("HOME", "/u")]), unasked), Ok(PathBuf::from("/u/.blaze/toolchains")));
        // The control (rule test-the-cause): with everything else the same, taking the
        // setting away is what moves the directory back under the home.
        assert_ne!(
            toolchains_in(env(&[(VAR, "/t"), ("HOME", "/u")]), none),
            toolchains_in(env(&[("HOME", "/u")]), none)
        );
    }

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
        crate::content::hex(&blaze_sha256::digest(bytes)).leak()
    }

    /// A release of two packages, written as the Rust project's site has them into `dist`,
    /// and its pins.
    fn release(dist: &Path, rustc: &[u8]) -> Release {
        let stdlib = package(
            "rust-std-9.9.9-test-host",
            "rust-std-test-host",
            &[("lib/rustlib/test-host/lib/libcore.rlib", "core\n")],
            &["lib/rustlib/test-host"],
        );
        let manifest = b"[pkg.rust]\ngit_commit_hash = \"abc\"\n";
        std::fs::create_dir_all(dist).unwrap();
        std::fs::write(dist.join("rustc-9.9.9-test-host.tar.xz"), rustc).unwrap();
        std::fs::write(dist.join("rust-std-9.9.9-test-host.tar.xz"), &stdlib).unwrap();
        std::fs::write(dist.join("channel-rust-9.9.9.toml"), manifest).unwrap();
        let pins = format!(
            "# test\nrelease 9.9.9 2000-01-01 9.9.9\nrustc 9.9.9 (abc 2000-01-01)\nmanifest {} {}\n\
             test-host rustc test-host {} {}\ntest-host rust-std test-host {} {}\n\
             other-host rustc other-host {} 1\n",
            sha256(manifest),
            manifest.len(),
            sha256(rustc),
            rustc.len(),
            sha256(&stdlib),
            stdlib.len(),
            sha256(b"other"),
        );
        Release::read(pins.leak(), "test-host").unwrap()
    }

    /// The programs of the package as the host's are named: `bin/rustc.exe` on Windows.
    fn exe(name: &str) -> &'static str {
        format!("bin/{name}{}", std::env::consts::EXE_SUFFIX).leak()
    }

    fn rustc() -> Vec<u8> {
        package(
            "rustc-9.9.9-test-host",
            "rustc",
            &[
                (exe("rustc"), "#!/bin/sh\n"),
                (exe("cargo"), "#!/bin/sh\n"),
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
        assert_eq!(
            ours.version,
            blaze_units::lock::toolchain(),
            "toolchain_pins.py '{{\"write\": true}}'"
        );
        assert_eq!(ours.channel, "nightly");
        assert!(ours.rustc.contains("-nightly ("), "{}", ours.rustc);
        let names: Vec<_> = ours.components.iter().map(Component::name).collect();
        assert_eq!(
            names,
            [
                "rustc-x86_64-unknown-linux-gnu",
                "cargo-x86_64-unknown-linux-gnu",
                "rust-std-x86_64-unknown-linux-gnu",
                "rust-std-wasm32-wasip1",
            ]
        );
        assert!(
            !names.iter().any(|it| it == "rust-src"),
            "the standard library's source is blaze's library/, not the release's"
        );
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
        assert!(complete(&release, &dir));
        let read = |path: &str| std::fs::read_to_string(dir.join(path)).unwrap();
        assert_eq!(read("lib/rustlib/components"), "rustc-test-host\nrust-std-test-host\n");
        assert_eq!(
            read("lib/rustlib/manifest-rustc-test-host"),
            format!(
                "file:{}\nfile:{}\nfile:lib/librustc_driver-0.so\nfile:share/doc/rust/README.md\n",
                exe("rustc"),
                exe("cargo")
            )
        );
        assert_eq!(
            read("lib/rustlib/manifest-rust-std-test-host"),
            "file:lib/rustlib/test-host/lib/libcore.rlib\n"
        );
        assert_eq!(
            read("lib/rustlib/multirust-channel-manifest.toml"),
            "[pkg.rust]\ngit_commit_hash = \"abc\"\n"
        );
        assert_eq!(read("lib/rustlib/rust-installer-version"), "3");
        assert!(
            read("lib/rustlib/multirust-config.toml")
                .contains("pkg = \"rust-std\"\ntarget = \"test-host\"\nis_extension = false")
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
        // One laid out before a component was pinned is not whole.
        std::fs::write(dir.join("lib/rustlib/components"), "rustc-test-host\n").unwrap();
        assert!(!complete(&release, &dir));
    }

    #[test]
    fn an_older_nightly_blaze_downloaded_goes_once_the_new_one_is_there() {
        let work = tempfile::tempdir().unwrap();
        let dist = work.path().join("dist");
        let release = release(&dist, &rustc());
        let toolchains = work.path().join("toolchains");
        let whole = |name: &str| {
            let dir = toolchains.join(name);
            for path in [exe("cargo"), exe("rustc"), "lib/rustlib/multirust-channel-manifest.toml"]
            {
                std::fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
                std::fs::write(dir.join(path), "").unwrap();
            }
        };
        // The test release is channel 9.9.9 of 2000-01-01.
        whole("9.9.9-1999-12-31-test-host");
        whole("9.9.9-2000-01-02-test-host");
        whole("9.9.9-1999-12-31-other-host");
        std::fs::create_dir_all(toolchains.join("9.9.9-1999-12-30-test-host/bin")).unwrap();
        std::fs::write(toolchains.join(".9.9.9-1999-12-31-test-host.lock"), "").unwrap();
        fetched(&dist, &release, &toolchains.join("9.9.9-test-host")).unwrap();
        let mut left: Vec<_> = std::fs::read_dir(&toolchains)
            .unwrap()
            .map(|it| it.unwrap().file_name().into_string().unwrap())
            .filter(|it| !it.ends_with(".lock") || it.contains("1999-12-31"))
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "9.9.9-1999-12-30-test-host",
                "9.9.9-1999-12-31-other-host",
                "9.9.9-2000-01-02-test-host",
                "9.9.9-test-host"
            ],
            "the older whole toolchain of this host and its lock are gone"
        );
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
        assert!(complete(&release, &dir));
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

    /// The invariant: a download is held back by its unpacking. With the unpacking taking
    /// nothing, the download of a file of 32 pieces into a channel of 4 has written at most
    /// 4 + 1 pieces (4 held, 1 waiting to be handed on) and is still waiting. The control: the
    /// same download into an unbounded channel runs to the file's end, which fails the bound.
    #[test]
    fn a_download_waits_for_its_unpacking_by_the_channels_bound() {
        const BOUND: usize = 4;
        const PIECES: usize = 32;
        let work = tempfile::tempdir().unwrap();
        let dist = work.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let bytes: Vec<u8> = (0..PIECES * PIECE).map(|it| (it % 251) as u8).collect();
        std::fs::write(dist.join("big.tar.xz"), &bytes).unwrap();
        let pin = Pin { sha256: sha256(&bytes), bytes: bytes.len() as u64 };
        let source = Source::Dir(dist);
        // How far a download into `channel` gets while the unpacking takes nothing: the bytes
        // it wrote once they stop growing, and whether it ended.
        let ahead = |name: &str, channel: (Sender<Vec<u8>>, Receiver<Vec<u8>>)| -> (u64, bool) {
            let part = work.path().join(format!("{name}.part"));
            let (feed, pieces) = handoff("big.tar.xz", channel, Duration::from_secs(60));
            std::thread::scope(|scope| {
                let downloading = std::thread::Builder::new()
                    .name(format!("test: download into {name}"))
                    .spawn_scoped(scope, || download(&source, "big.tar.xz", &part, pin, Some(&feed)))
                    .unwrap();
                let clock = Monotonic::new();
                let (mut last, mut still) = (u64::MAX, 0);
                // Bounded: at most 1000 looks 10 ms apart; it ends once the download ended or
                // its part, once begun, has not grown for 20 looks.
                for _ in 0..1000 {
                    let len = std::fs::metadata(&part).map_or(0, |it| it.len());
                    (still, last) = if len == last && len > 0 { (still + 1, len) } else { (0, len) };
                    if downloading.is_finished() || still == 20 {
                        break;
                    }
                    let next = Deadline::after(blaze_core::wait::Clock::now(&clock), Duration::from_millis(10));
                    blaze_core::wait::sleep_until("the test's next look at the part", next, &clock).unwrap();
                }
                let ended = downloading.is_finished();
                // The unpacking goes: a download still waiting ends, as stopped.
                drop(pieces);
                let got = downloading.join().unwrap();
                if !ended {
                    assert!(got.unwrap_err().is::<Unpacking>(), "a held download ends as the unpacking goes");
                }
                (last, ended)
            })
        };
        let most = ((BOUND + 1) * PIECE) as u64;
        let (held, ended) = ahead("bounded", crossbeam_channel::bounded(BOUND));
        assert!(held <= most && !ended, "{held} bytes written of {most} at most, ended: {ended}");
        let (ran, ended) = ahead("unbounded", crossbeam_channel::unbounded());
        assert!(
            ran > most && ended,
            "the control: with no bound the download runs ahead, {ran} bytes, ended: {ended}"
        );
        assert_eq!(ran, bytes.len() as u64);
    }

    /// The invariant (rule no-fallback): a download that fails is asked once, file by file, and
    /// the error names the file and its address. The server answers every request with a 500;
    /// the control is what it saw: one request for each file of the release and none again.
    #[test]
    fn a_failed_download_asks_each_file_once_and_names_its_address() {
        let work = tempfile::tempdir().unwrap();
        let dist = work.path().join("dist");
        let release = release(&dist, &rustc());
        let (address, asked) = super::super::tests::failing_server(16);
        let http = Arc::new_in(HyperHttp::new(Duration::from_secs(10)).unwrap(), Global);
        let base = super::super::base(&address).unwrap();
        let dir = work.path().join("toolchains/9.9.9-test-host");
        let said = fetch_from(&release, &dir, || Ok(Source::Web { http, base })).unwrap_err();
        let rustc = "rustc-9.9.9-test-host.tar.xz";
        assert!(said.contains(&format!("downloading {rustc} from {address}{rustc}: ")), "{said}");
        assert!(said.contains("got 500"), "{said}");
        let mut asked = asked.lock().clone(); // clone: what the server saw, to sort
        asked.sort();
        assert_eq!(
            asked,
            ["/channel-rust-9.9.9.toml", "/rust-std-9.9.9-test-host.tar.xz", "/rustc-9.9.9-test-host.tar.xz"],
            "each file asked once, and none again"
        );
        assert!(!dir.exists(), "nothing is put in place");
    }
}
