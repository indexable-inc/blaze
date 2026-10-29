//! blaze's own toolchain, the Rust `blaze` builds with: `blaze-2026-10-25` ([`name`]), today
//! the Rust nightly of that date (docs/toolchain.md). The language and standard library are the
//! ones this blaze ships, and nothing else picks them: blaze ignores `RUSTC`,
//! `RUSTUP_TOOLCHAIN`, rustup's settings and the project's `rust-toolchain` files, and never
//! runs or asks a rustc to learn anything (docs/no-rustc.md). A project that needs another
//! Rust pins another blaze (`[workspace.metadata.blaze] version`).
//!
//! The files of the release (its cargo, rustc and lld, the standard library of each target)
//! are found on disk with nothing run: rustup's copy of the release when it has one, else
//! blaze's own copy (`own.rs`), downloaded the first time neither is there. The source of the
//! standard library is not the release's: it is blaze's own `library/` ([`library`]).

pub mod library;
pub mod own;

pub use own::home;

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// The toolchain blaze compiles, as rustup names it: `nightly-2026-10-25`.
pub const OURS: &str = include_str!("../../../../harness/toolchain");

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

/// The sysroot of blaze's release: what blaze answers `rustc --print sysroot` with.
pub fn sysroot() -> Option<PathBuf> {
    found().map(|it| it.dir().to_owned())
}

/// Has the cargo, rustc and daemon of this command be blaze's release, downloading blaze's
/// own copy of it when there is none ([`found`]), and the standard library's source be blaze's
/// ([`library::fetch`], downloaded beside it when it is not here). They are named in `CARGO`,
/// `RUSTC`, `RUSTDOC`, `RUSTUP_TOOLCHAIN` and [`library::VAR`] and are first on `PATH`, for
/// the processes this one starts and not the user's shell, whatever the user set them to or a
/// toolchain file names. `Err` is why the command can't be built, said for the user to act on.
/// Before any thread starts: it sets the environment, once the threads it starts are done.
pub fn settle() -> Result<Chosen, String> {
    let library = std::thread::scope(|scope| {
        let library = scope.spawn(library::fetch);
        let chosen = release();
        let library = library.join().unwrap_or_else(|_| Err("finding it panicked".to_owned()));
        chosen.map(|chosen| (chosen, library))
    });
    let (chosen, library) = library?;
    library::publish_fetched(&library?);
    use_toolchain(chosen.dir());
    Ok(chosen)
}

/// [`found`], else blaze's own copy of the release, downloaded.
fn release() -> Result<Chosen, String> {
    let ours = OURS.trim();
    let chosen = match found() {
        Some(chosen) => chosen,
        None => {
            let Some(release) = own::Release::ours() else {
                return Err(format!(
                    "blaze builds with Rust {ours}, and the Rust project publishes no copy of it \
                     for this machine that blaze knows"
                ));
            };
            let Some(dir) = release.dir() else {
                return Err(format!(
                    "blaze builds with Rust {ours} and keeps it in .blaze in your home \
                     directory, and there is no home directory: `BLAZE_HOME` names where to \
                     keep it"
                ));
            };
            own::fetch(&release, &dir)?;
            Chosen::Own(dir)
        }
    };
    Ok(chosen)
}

/// Has every process this one starts run blaze's release at `dir`: cargo and the daemon from
/// it, and a rustc, rustdoc or cargo a build script finds on the path, or reaches through a
/// rustup proxy, from it too.
fn use_toolchain(dir: &Path) {
    let bin = dir.join("bin");
    let exe = |name: &str| bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let path = std::env::var_os("PATH");
    let path = std::iter::once(bin.clone()).chain(path.iter().flat_map(std::env::split_paths));
    // SAFETY: before any thread is started.
    unsafe {
        std::env::set_var("CARGO", exe("cargo"));
        std::env::set_var("RUSTC", exe("rustc"));
        std::env::set_var("RUSTDOC", exe("rustdoc"));
        std::env::set_var("RUSTUP_TOOLCHAIN", dir);
        std::env::remove_var("RUSTUP_TOOLCHAIN_SOURCE");
        if let Ok(path) = std::env::join_paths(path) {
            std::env::set_var("PATH", path);
        }
    }
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
        eprintln!(
            "blaze: {} is ignored: blaze builds with its own Rust, {} ({})",
            file.display(),
            ours_release(),
            name()
        );
    }
}

/// Why this blaze can't build `packages`: the first whose `rust-version` is newer than the
/// Rust blaze ships. A nightly counts as its release, as cargo counts it.
pub fn too_new(packages: &[&cargo_metadata::Package]) -> Option<String> {
    let package = packages.iter().find(|it| {
        it.rust_version.as_ref().is_some_and(|it| newer_than_ours(it.major, it.minor, it.patch))
    })?;
    let needs = package.rust_version.as_ref()?;
    Some(format!(
        "`{}` needs Rust {needs} (its `rust-version`), and this blaze ({}) is Rust {}: update \
         blaze to one with Rust {needs} or newer, and pin it in the workspace's `Cargo.toml` \
         under `[workspace.metadata.blaze] version`",
        package.name,
        crate::engine::get().version,
        ours_release()
    ))
}

/// Whether Rust `major.minor.patch` is newer than blaze's release, its nightly counted as the
/// release it leads to.
fn newer_than_ours(major: u64, minor: u64, patch: u64) -> bool {
    let ours = semver::Version::parse(ours_release()).expect("blaze's release is a version");
    (major, minor, patch) > (ours.major, ours.minor, ours.patch)
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
/// `nightly-2026-10-25-aarch64-apple-darwin`.
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
    found.iter().find(|it| host(it)).or(found.first()).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rust_version_up_to_blazes_release_builds() {
        let ours = semver::Version::parse(ours_release()).unwrap();
        assert!(!newer_than_ours(1, 56, 0));
        assert!(!newer_than_ours(ours.major, ours.minor, ours.patch));
        assert!(newer_than_ours(ours.major, ours.minor, ours.patch + 1));
        assert!(newer_than_ours(ours.major, ours.minor + 1, 0));
    }

    #[test]
    fn the_nearest_toolchain_file_is_found() {
        let root =
            std::env::temp_dir().join(format!("blaze-toolchain-file-{}", std::process::id()));
        let inner = root.join("a/b");
        std::fs::create_dir_all(&inner).unwrap();
        assert_eq!(toolchain_file(&inner).filter(|it| it.starts_with(&root)), None);
        std::fs::write(root.join("a/rust-toolchain.toml"), "[toolchain]\nchannel = \"1.70\"\n")
            .unwrap();
        assert_eq!(toolchain_file(&inner), Some(root.join("a/rust-toolchain.toml")));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
