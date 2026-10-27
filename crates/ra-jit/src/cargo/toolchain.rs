//! blaze's own toolchain, the Rust `cargo blaze` builds with whatever toolchain the project
//! names: `blaze-2026-10-25` ([`name`]), today the Rust nightly of that date (docs/toolchain.md).
//! It is blaze's, not the project's: the base blaze replaces piece by piece (docs/no-rustc.md),
//! moved on blaze's schedule with the corpus as the gate. blaze lays out types, names symbols and links as that nightly does, so cargo,
//! rustc and the daemon all run under it. blaze supports the latest nightly only, pinned by date
//! in `harness/toolchain` (`harness/toolchain_bump.py` moves it): a project pinned to a stable
//! release or an older nightly builds with it, since Rust builds today what an older release
//! built; one that needs what the pinned nightly lacks fails with rustc's error and a line saying
//! which toolchain built it, and never goes to cargo for it ([#269]).
//!
//! The release comes from, in order: what `RUSTC` names, or the rustc cargo would run when it
//! is that release; rustup's copy of it; blaze's own copy (`own.rs`), downloaded the first
//! time none of those has it.
//!
//! [#269]: https://github.com/indexable-inc/blaze/issues/269

mod own;

pub(crate) use own::home;

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use super::state::rustc_says;

/// The toolchain blaze compiles, as rustup names it: `nightly-2026-10-25`.
pub(crate) const OURS: &str = include_str!("../../../../harness/toolchain");

/// blaze's toolchain as blaze names it in what it prints: `blaze-2026-10-25`, after the date of
/// the Rust nightly it is (`harness/toolchain`), so it moves with every bump. rustup knows the
/// toolchain by the nightly's name, not this one.
pub(crate) fn name() -> String {
    let ours = OURS.trim();
    format!("blaze-{}", ours.strip_prefix("nightly-").unwrap_or(ours))
}

/// What that toolchain's rustc says it is after `rustc `: `1.101.0-nightly (12433c307 2026-10-24)`.
pub(crate) fn ours_says() -> &'static str {
    own::rustc_version()
}

/// The commit that toolchain was built from.
pub(crate) fn ours_commit() -> &'static str {
    own::commit()
}

/// The project's own toolchain, as a failure names it, when blaze builds with its release in
/// its place.
static IN_PLACE_OF: OnceLock<(String, &'static str)> = OnceLock::new();

/// Where the Rust a command builds with came from.
pub(crate) enum Chosen {
    /// The rustc `RUSTC` names or cargo would run, at its sysroot.
    Given(PathBuf),
    Rustup(PathBuf),
    Own(PathBuf),
}

impl Chosen {
    /// Where the toolchain is: its sysroot.
    pub(crate) fn dir(&self) -> &Path {
        match self {
            Chosen::Given(dir) | Chosen::Rustup(dir) | Chosen::Own(dir) => dir,
        }
    }
}

impl std::fmt::Display for Chosen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (name, ours) = (name(), OURS.trim());
        let dir = self.dir().display();
        match self {
            Chosen::Given(_) => write!(f, "{name}, blaze's toolchain (Rust {ours}), at {dir}"),
            Chosen::Rustup(_) => {
                write!(f, "{name}, blaze's toolchain (Rust {ours}), rustup's copy at {dir}")
            }
            Chosen::Own(_) => write!(f, "{name}, blaze's toolchain (Rust {ours}), at {dir}"),
        }
    }
}

/// Has the cargo, rustc and daemon of this command be blaze's release. A rustc that is that
/// release is used as it is; rustup's copy is named in `RUSTUP_TOOLCHAIN`, which rustup's
/// proxies take over a toolchain file or an override, with its cargo in `CARGO`; blaze's own
/// is named in `CARGO`, `RUSTC` and `RUSTDOC` and is first on `PATH`, for the processes this one
/// starts and not the user's shell. `Err` is why the command can't be built, said for the user
/// to act on. Before any thread starts: it sets the environment.
pub(crate) fn settle() -> Result<Chosen, String> {
    let ours = OURS.trim();
    let other = super::state::other_toolchain_allowed();
    let rustc = super::state::rustc_path();
    let version = version_of(&rustc);
    let shown = version.as_deref().unwrap_or("of no release it says");
    if other {
        let sysroot = sysroot_of(&rustc);
        with_library(&sysroot, shown)?;
        return Ok(Chosen::Given(sysroot));
    }
    let named = super::state::rustc_named().is_some();
    if version.as_deref() == Some(ours_says()) {
        let sysroot = sysroot_of(&rustc);
        match with_library(&sysroot, ours) {
            Ok(()) => {
                rustup_named(&sysroot);
                return Ok(Chosen::Given(sysroot));
            }
            // What `RUSTC` names is used or refused; a rustc on the path without the library
            // leaves the release to rustup or blaze.
            Err(why) if named => return Err(why),
            Err(_) => {}
        }
    } else if named {
        return Err(format!(
            "`RUSTC` names rustc {shown}, and blaze builds with Rust {ours}, the release it \
             compiles: without `RUSTC` it builds with {ours} (`BLAZE_OTHER_TOOLCHAIN=1` builds \
             with {shown} anyway, laying its types out as {ours} does)"
        ));
    }
    if let Some(version) = version.filter(|it| it != ours_says()) {
        note_once(&version, named_by());
        let _ = IN_PLACE_OF.set((version, named_by()));
    }
    let rustup = rustup_home().and_then(|home| installed(&home, ours));
    if let Some(dir) = rustup.filter(|dir| with_library(dir, ours).is_ok()) {
        let name = dir.file_name().map_or_else(|| OsString::from(ours), OsStr::to_owned);
        let cargo = dir.join("bin").join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
        // SAFETY: before any thread is started.
        unsafe {
            std::env::set_var("RUSTUP_TOOLCHAIN", name);
            std::env::set_var("CARGO", cargo);
        }
        return Ok(Chosen::Rustup(dir));
    }
    let Some(release) = own::Release::ours() else {
        return Err(format!(
            "blaze builds with Rust {ours}, which is not installed, and the Rust project \
             publishes no copy of it for this machine that blaze knows: install Rust {ours} with \
             its `rust-src` and put it first on `PATH`"
        ));
    };
    let Some(dir) = release.dir() else {
        return Err(format!(
            "blaze builds with Rust {ours} and keeps it in .blaze in your home directory, and \
             there is no home directory: `BLAZE_HOME` names where to keep it"
        ));
    };
    if !release.whole_at(&dir) {
        own::fetch(&release, &dir)?;
    }
    use_own(&dir);
    Ok(Chosen::Own(dir))
}

/// Has every process this one starts run blaze's own toolchain at `dir`: cargo and the daemon
/// from it, and a rustc, rustdoc or cargo a build script finds on the path, or reaches through a
/// rustup proxy, from it too.
fn use_own(dir: &Path) {
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
        if let Ok(path) = std::env::join_paths(path) {
            std::env::set_var("PATH", path);
        }
    }
}

/// Names rustup's toolchain at `sysroot` one way however it was named (`nightly-2026-10-25`,
/// `nightly-2026-10-25-<host>`, a path), since the name is part of which build this is.
fn rustup_named(sysroot: &Path) {
    let home = rustup_home().and_then(|it| dunce::canonicalize(it.join("toolchains")).ok());
    let dir = dunce::canonicalize(sysroot).ok();
    let name = dir.filter(|dir| home.is_some_and(|home| dir.parent() == Some(&*home)));
    if std::env::var_os("RUSTUP_TOOLCHAIN").is_some()
        && let Some(name) = name.as_ref().and_then(|it| it.file_name())
    {
        // SAFETY: before any thread is started.
        unsafe { std::env::set_var("RUSTUP_TOOLCHAIN", name) };
    }
}

/// Says, once for each project directory and toolchain it names, that blaze builds with its
/// own toolchain and not that one: a note, not an error, and nothing is switched. The marker is
/// in `~/.blaze/noted`; with no home directory it is said every time.
fn note_once(version: &str, named: &str) {
    use sha2::Digest as _;
    let here = std::env::current_dir().unwrap_or_default();
    let mut hash = sha2::Sha256::new();
    hash.update(here.as_os_str().as_encoded_bytes());
    hash.update([0]);
    hash.update(version);
    hash.update([0]);
    hash.update(name());
    let key: String = hash.finalize().iter().take(12).map(|it| format!("{it:02x}")).collect();
    let marker = own::home().map(|home| home.join("noted").join(key));
    if marker.as_ref().is_some_and(|it| it.exists()) {
        return;
    }
    super::say::note(&format!(
        "blaze builds with its own toolchain, {}, not rustc {version}, {named}. blaze is its own \
         Rust toolchain whatever the project pins (docs/toolchain.md); the project's toolchain \
         and its CI stay the final word",
        name()
    ));
    if let Some(marker) = marker
        && let Some(dir) = marker.parent()
        && std::fs::create_dir_all(dir).is_ok()
    {
        let _ = std::fs::write(marker, version);
    }
}

/// What a build that failed says after its errors: which toolchain built it, when it was not
/// the project's own, since the failure may be what one has and the other has not.
pub(crate) fn failed_note() -> Option<String> {
    let (version, named) = IN_PLACE_OF.get()?;
    Some(format!(
        "blaze built this with its own toolchain, {} (Rust {}), not {version}, {named}; an error \
         {version} would not give comes from that difference",
        name(),
        OURS.trim()
    ))
}

pub(crate) fn note_failed() {
    if let Some(note) = failed_note() {
        super::say::note(&note);
    }
}

/// What `rustc` says it is after `rustc `, as `1.101.0-nightly (12433c307 2026-10-24)`: the
/// whole line, since every nightly of a release has the same number.
fn version_of(rustc: &OsStr) -> Option<String> {
    let said = rustc_says(rustc, &["--version"])?;
    said.lines().next()?.trim().strip_prefix("rustc ").map(str::to_owned)
}

fn sysroot_of(rustc: &OsStr) -> PathBuf {
    PathBuf::from(rustc_says(rustc, &["--print=sysroot"]).unwrap_or_default())
}

/// The `rust-lld` that rustc links MSVC programs with when a target says so, as the
/// repository's `.cargo/config.toml` does: rustc finds it in the toolchain, not on the PATH.
pub(crate) fn rust_lld() -> PathBuf {
    let host = hir_ty_codegen::Target::host();
    let lld = format!("rust-lld{}", std::env::consts::EXE_SUFFIX);
    sysroot_of(&super::state::rustc_path()).join("lib/rustlib").join(host).join("bin").join(lld)
}

/// What chose the project's toolchain, as rustup tells the programs its proxies start.
fn named_by() -> &'static str {
    match std::env::var("RUSTUP_TOOLCHAIN_SOURCE").as_deref() {
        Ok("toolchain-file") => "which the project's rust-toolchain file names",
        Ok("path-override" | "override") => "which `rustup override` names for the project",
        Ok("cli") => "which `+toolchain` names",
        Ok("env") => "which `RUSTUP_TOOLCHAIN` names",
        Ok("default") => "rustup's default toolchain",
        _ => "the toolchain cargo was started with",
    }
}

/// The sysroot of rustup's copy of blaze's release, the one [`settle`] picks when rustup has it,
/// found without running anything.
pub(crate) fn rustup_ours() -> Option<PathBuf> {
    rustup_home().and_then(|home| installed(&home, OURS.trim()))
}

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

/// `Ok` when the toolchain at `sysroot` has the source of its standard library, which the
/// daemon compiles.
fn with_library(sysroot: &Path, release: &str) -> Result<(), String> {
    if sysroot.join("lib/rustlib/src/rust/library").is_dir() {
        return Ok(());
    }
    Err(format!(
        "Rust {release} has no source of its standard library, which blaze compiles: \
         `rustup component add rust-src --toolchain {release}` adds it"
    ))
}
