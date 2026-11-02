//! blaze's own toolchain, the Rust `blaze` builds with: `blaze-2026-10-25` ([`name`]), today
//! the Rust nightly of that date (docs/toolchain.md). The language and standard library are the
//! ones this blaze ships, and nothing else picks them: blaze ignores `RUSTC`,
//! `RUSTUP_TOOLCHAIN`, rustup's settings and the project's `rust-toolchain` files, and never
//! runs or asks a rustc to learn anything (docs/no-rustc.md). A project that needs another
//! Rust pins another blaze (`[workspace.metadata.blaze] version`).
//!
//! The files of the release (its cargo, rustc and lld, the standard library of each target)
//! are found on disk with nothing run: rustup's copy of the release when it has one, else
//! blaze's own copy (`own.rs`), which [`own::fetch`] downloads. The source of the
//! standard library is not the release's: it is blaze's own `library/` ([`library`]).

pub mod library;
pub mod own;

pub use own::home;

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use blaze_registry::http::{Http as _, HttpNotSuccessful, HyperHttp};

/// The toolchain blaze compiles, as rustup names it: `nightly-2026-10-25`.
pub const OURS: &str = include_str!("../../../harness/toolchain");

/// blaze's toolchain as blaze names it in what it prints: `blaze-2026-10-25`, after the date of
/// the Rust nightly it is (`harness/toolchain`), so it moves with every bump. rustup knows the
/// toolchain by the nightly's name, not this one.
pub fn name() -> String {
    let ours = OURS.trim();
    format!("blaze-{}", ours.strip_prefix("nightly-").unwrap_or(ours))
}

/// What that toolchain's rustc says it is after `rustc `: `1.101.0-nightly (12433c307 2026-10-24)`.
pub fn ours_says() -> &'static str {
    own::rustc_version()
}

/// That rustc's release, as `rustc -vV` says it: `1.101.0-nightly`.
pub fn ours_release() -> &'static str {
    ours_says().split_once(' ').map_or(ours_says(), |(release, _)| release)
}

/// The commit that toolchain was built from.
pub fn ours_commit() -> &'static str {
    own::commit()
}

/// The host's tuple, as the Rust project names its builds: what blaze answers `rustc -vV`'s
/// `host:` with. `None` on a host blaze ships no toolchain for.
pub fn host() -> Option<&'static str> {
    own::host()
}

/// Where the files of blaze's release are.
pub enum Chosen {
    Rustup(PathBuf),
    Own(PathBuf),
}

impl Chosen {
    /// Where the toolchain is: its sysroot.
    pub fn dir(&self) -> &Path {
        match self {
            Chosen::Rustup(dir) | Chosen::Own(dir) => dir,
        }
    }
}

impl std::fmt::Display for Chosen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (name, ours) = (name(), OURS.trim());
        let dir = self.dir().display();
        match self {
            Chosen::Rustup(_) => {
                write!(f, "{name}, blaze's toolchain (Rust {ours}), rustup's copy at {dir}")
            }
            Chosen::Own(_) => write!(f, "{name}, blaze's toolchain (Rust {ours}), at {dir}"),
        }
    }
}

/// The files of blaze's release, found with nothing run: rustup's copy, else blaze's own when it
/// is whole. `None` when neither is there.
pub fn found() -> Option<Chosen> {
    if let Some(dir) = FOUND.get() {
        return Some(chosen_at(dir.clone()));
    }
    let ours = OURS.trim();
    let chosen = match rustup_home().and_then(|home| installed(&home, ours)) {
        Some(dir) => Chosen::Rustup(dir),
        _ => {
            let release = own::Release::ours()?;
            let dir = release.dir()?;
            if !own::complete(&release, &dir) {
                return None;
            }
            Chosen::Own(dir)
        }
    };
    let _ = FOUND.set(chosen.dir().to_owned());
    Some(chosen)
}

/// [`found`], else blaze's own copy of the release, downloaded once: what a command needs
/// before the daemon builds with it. `Err` says why there is none and what to do.
pub fn ensure() -> Result<Chosen, String> {
    if let Some(chosen) = found() {
        return Ok(chosen);
    }
    let ours = OURS.trim();
    let Some(release) = own::Release::ours() else {
        return Err(format!(
            "blaze builds with Rust {ours}, and the Rust project publishes no copy of it for this machine that blaze knows"
        ));
    };
    let Some(dir) = release.dir() else {
        return Err(format!(
            "blaze builds with Rust {ours} and keeps it in .blaze in your home directory, and there is no home \
             directory: `BLAZE_HOME` names where to keep it"
        ));
    };
    own::fetch(&release, &dir)?;
    let _ = FOUND.set(dir.clone());
    Ok(Chosen::Own(dir))
}

/// Where [`found`] found blaze's release, or was told it is ([`known_at`]).
static FOUND: OnceLock<PathBuf> = OnceLock::new();

/// Tells this process where blaze's release is, as the blaze that started it found it: a
/// build script's sandbox may not see where rustup and blaze keep toolchains.
pub fn known_at(sysroot: PathBuf) {
    let _ = FOUND.set(sysroot);
}

fn chosen_at(dir: PathBuf) -> Chosen {
    let own = own::Release::ours().and_then(|it| it.dir());
    if own.as_deref() == Some(&*dir) { Chosen::Own(dir) } else { Chosen::Rustup(dir) }
}

/// The sysroot of blaze's release: what blaze answers `rustc --print sysroot` with. Looked for
/// on disk once ([`found`]), then fixed for the process: the daemon asks at its start, so a
/// build's task only reads it.
pub fn sysroot() -> Option<&'static Path> {
    if FOUND.get().is_none() {
        found()?;
    }
    FOUND.get().map(PathBuf::as_path)
}

/// `file` under `base` (an address ending in `/`) from byte `offset` on, through blaze-registry's
/// HTTP client; `None` when the server has no such file. A server that ignores the range sends
/// the whole file, of which what is had is left out.
async fn read_from(http: &HyperHttp, base: &url::Url, file: &str, offset: u64) -> anyhow::Result<Option<Vec<u8>>> {
    let url = base.join(file)?;
    let range = format!("bytes={offset}-");
    let headers: &[(&str, &str)] = if offset == 0 { &[] } else { &[("range", &range)] };
    let mut response = http.get_async(url.as_str(), headers).await?;
    match response.status {
        404 | 410 => Ok(None),
        206 => Ok(Some(response.body)),
        200 => {
            let had = usize::try_from(offset).unwrap_or(usize::MAX).min(response.body.len());
            Ok(Some(response.body.split_off(had)))
        }
        _ => Err(HttpNotSuccessful::new(url.as_str(), &response).into()),
    }
}

/// A runtime for a download's own waits, on this thread: the requests run on the HTTP client's.
fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

/// A download that failed every time it was tried; the error it is the context of is the last
/// failure.
#[derive(Debug)]
struct GaveUp {
    file: String,
    tries: u32,
}

impl std::fmt::Display for GaveUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: the download gave up after {} tries", self.file, self.tries)
    }
}

/// A variable that names a place to get blaze's files from and is not UTF-8.
#[derive(Debug)]
struct NotUtf8Var(&'static str);

impl std::fmt::Display for NotUtf8Var {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` is not UTF-8, and blaze reads the address or directory it names as text", self.0)
    }
}

impl std::error::Error for NotUtf8Var {}

/// The variable `name` when it is set and not empty.
fn given_var(name: &'static str) -> Result<Option<String>, NotUtf8Var> {
    match std::env::var(name) {
        Ok(it) if !it.is_empty() => Ok(Some(it)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(NotUtf8Var(name)),
    }
}

/// `address` as the base [`read_from`] joins file names to.
fn base(address: &str) -> anyhow::Result<url::Url> {
    Ok(url::Url::parse(&format!("{}/", address.trim_end_matches('/')))?)
}

/// Where rustup keeps its toolchains, which is only where to look for a copy of blaze's
/// release: what rustup would pick is not asked.
fn rustup_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("RUSTUP_HOME") {
        return Some(PathBuf::from(home));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join(".rustup")).filter(|it| it.is_dir())
}

/// The directory rustup installed `release` for this host in:
/// `nightly-2026-10-25-aarch64-apple-darwin`. One of another host's is none: its libraries
/// are not this host's.
fn installed(home: &Path, release: &str) -> Option<PathBuf> {
    let prefix = format!("{release}-");
    let cargo = format!("cargo{}", std::env::consts::EXE_SUFFIX);
    let mut found: Vec<PathBuf> = std::fs::read_dir(home.join("toolchains"))
        .ok()?
        .flatten()
        .filter(|it| it.file_name().to_str().is_some_and(|name| name.starts_with(&prefix)))
        .map(|it| it.path())
        .filter(|it| it.join("bin").join(&cargo).is_file())
        .collect();
    found.sort();
    let host = |dir: &PathBuf| {
        let name = dir.file_name().and_then(|it| it.to_str()).unwrap_or_default();
        let os = match std::env::consts::OS {
            "macos" => "apple-darwin",
            os => os,
        };
        name[prefix.len()..].starts_with(std::env::consts::ARCH) && name.contains(os)
    };
    found.into_iter().find(host)
}

/// The `rust-toolchain` or `rust-toolchain.toml` nearest `dir`, which rustup would follow and
/// blaze does not.
pub fn toolchain_file(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .flat_map(|it| ["rust-toolchain", "rust-toolchain.toml"].map(|name| it.join(name)))
        .find(|it| it.is_file())
}

/// Says, under `-v`, that the toolchain file nearest `dir` is not followed: one line, since
/// nothing about the build depends on it.
pub fn say_toolchain_file(dir: &Path, verbose: bool) {
    if let Some(file) = toolchain_file(dir).filter(|_| verbose) {
        eprintln!("blaze: {} is ignored: blaze builds with its own Rust, {} ({})", file.display(), ours_release(), name());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_toolchain_file_is_found() {
        let root = tempfile::tempdir().unwrap();
        let inner = root.path().join("a/b");
        std::fs::create_dir_all(&inner).unwrap();
        assert_eq!(toolchain_file(&inner).filter(|it| it.starts_with(root.path())), None);
        std::fs::write(root.path().join("a/rust-toolchain.toml"), "[toolchain]\nchannel = \"1.70\"\n").unwrap();
        assert_eq!(toolchain_file(&inner), Some(root.path().join("a/rust-toolchain.toml")));
        std::fs::write(root.path().join("a/b/rust-toolchain"), "1.70\n").unwrap();
        assert_eq!(toolchain_file(&inner), Some(inner.join("rust-toolchain")), "the nearest");
    }

    #[test]
    fn blazes_toolchain_is_named_after_its_nightly_and_its_rustc_says_its_release() {
        let date = OURS.trim().strip_prefix("nightly-").expect("harness/toolchain names a nightly");
        assert_eq!(name(), format!("blaze-{date}"));
        assert!(ours_says().starts_with(ours_release()), "{}", ours_says());
        semver::Version::parse(ours_release()).expect("a release is a version");
    }

    #[test]
    fn a_download_that_gave_up_says_how_often_it_tried_and_why() {
        let error = anyhow::anyhow!("connection reset").context(GaveUp { file: "library-ab.tar.xz".to_owned(), tries: 4 });
        assert_eq!(format!("{error:#}"), "library-ab.tar.xz: the download gave up after 4 tries: connection reset");
        assert!(error.downcast_ref::<GaveUp>().is_some_and(|it| it.tries == 4));
        assert_eq!(
            NotUtf8Var("BLAZE_RUST_DIST").to_string(),
            "`BLAZE_RUST_DIST` is not UTF-8, and blaze reads the address or directory it names as text"
        );
    }

    #[test]
    fn rustups_toolchain_of_another_host_is_not_this_ones() {
        let home = tempfile::tempdir().unwrap();
        let make = |name: &str| {
            let bin = home.path().join("toolchains").join(name).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join(format!("cargo{}", std::env::consts::EXE_SUFFIX)), "").unwrap();
        };
        make("nightly-2026-10-25-riscv64gc-unknown-none-elf");
        assert_eq!(installed(home.path(), "nightly-2026-10-25"), None);
        let os = match std::env::consts::OS {
            "macos" => "apple-darwin",
            os => os,
        };
        let ours = format!("nightly-2026-10-25-{}-{os}", std::env::consts::ARCH);
        make(&ours);
        assert_eq!(installed(home.path(), "nightly-2026-10-25"), Some(home.path().join("toolchains").join(ours)));
    }
}
