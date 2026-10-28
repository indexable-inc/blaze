//! The source of the standard library blaze compiles: `library/` of the repository this blaze
//! was built from (docs/toolchain.md, "The standard library: library/"), the Rust project's
//! at the commit of blaze's nightly with blaze's changes to it. It is known by
//! its digest (`library/DIGEST`, written by `harness/library.py`), which is what the store keys
//! the standard library by, and it is the only source of the standard library blaze reads: no
//! toolchain's `rust-src` is.
//!
//! Where it is, found with nothing run or fetched ([`found`]), the first of:
//!
//! - the directory [`VAR`] names, for a process started by blaze, which says where it found
//!   it, or a person trying a changed library (its own `DIGEST` is then the one used);
//! - `share/blaze/library/` in a directory above the program (Nix's `$out`);
//! - blaze's own copy, `~/.blaze/library/<digest>/library`, which [`fetch`] makes once from
//!   the checkout this blaze was built in while it has the same library, else downloaded as
//!   the release's `library-<digest>.tar.xz`. It is checked file by file against the digest
//!   whichever way it came.
//!
//! It is never read in a checkout: what blaze stores of a crate whose data names the checkout
//! built is that checkout's alone, and the standard library's is everyone's.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

use sha2::{Digest as _, Sha256};

use crate::remote::{self, Https, Traffic};

/// The variable that names the library's directory to a process blaze starts.
pub const VAR: &str = "BLAZE_LIBRARY";

/// The file of the library that has its digest, and is not part of what it is a digest of.
const DIGEST_FILE: &str = "DIGEST";

/// The digest of the library this blaze was built with.
const BUILT_WITH: &str = include_str!("../../../../../library/DIGEST");

/// What the library this blaze was built with is a copy of (`library/UPSTREAM`).
const UPSTREAM: &str = include_str!("../../../../../library/UPSTREAM");

/// Where the assets of the releases are.
const RELEASES: &str = "https://github.com/indexable-inc/blaze/releases/download";

/// How long a download waits for the server to answer, or for the next of its bytes.
const PATIENCE: Duration = Duration::from_secs(60);

/// How many times the archive is asked for before the download gives up.
const TRIES: u32 = 4;

/// The digest of the library this blaze was built with.
pub fn digest() -> &'static str {
    BUILT_WITH.trim()
}

/// The commit of rust-lang/rust the library is a copy of, from `UPSTREAM`.
pub fn upstream_commit(upstream: &str) -> Option<&str> {
    upstream.lines().find_map(|it| it.strip_prefix("commit ")).map(str::trim)
}

/// A library blaze can read: its directory, its digest and the commit it is a copy of.
#[derive(Clone, Debug)]
pub struct Library {
    pub dir: PathBuf,
    pub digest: String,
    pub commit: String,
}

impl Library {
    /// The library at `dir`, when it has a digest and a commit. It is in a directory named
    /// `library`, as in rust-lang/rust, which the paths of its files are named from.
    fn at(dir: PathBuf) -> Option<Library> {
        if dir.file_name().is_none_or(|it| it != "library") {
            return None;
        }
        let digest = std::fs::read_to_string(dir.join(DIGEST_FILE)).ok()?.trim().to_owned();
        let upstream = std::fs::read_to_string(dir.join("UPSTREAM")).ok()?;
        let commit = upstream_commit(&upstream)?.to_owned();
        let whole = dir.join("Cargo.toml").is_file() && dir.join("core/src/lib.rs").is_file();
        (whole && !digest.is_empty()).then_some(Library { dir, digest, commit })
    }

    /// The library at `dir` when it is the one this blaze was built with.
    fn ours_at(dir: PathBuf) -> Option<Library> {
        Library::at(dir).filter(|it| it.digest == digest())
    }
}

static FOUND: OnceLock<Result<Library, String>> = OnceLock::new();

/// What [`fetch`] downloaded, when the library was not found before.
static FETCHED: OnceLock<Library> = OnceLock::new();

/// Why [`publish`] could not make blaze's copy from the checkout.
static NOT_MADE: OnceLock<String> = OnceLock::new();

/// Why there is no library here, for an error that says so: what finding it and making it
/// said.
pub fn why_not() -> String {
    let found = found().err().unwrap_or_default();
    match NOT_MADE.get() {
        Some(made) => format!("{found}; {made}"),
        None => found,
    }
}

/// Where the library is, found with nothing fetched (or fetched by this process); `Err` says
/// why there is none.
pub fn found() -> Result<&'static Library, String> {
    if let Some(fetched) = FETCHED.get() {
        return Ok(fetched);
    }
    FOUND.get_or_init(look).as_ref().map_err(Clone::clone)
}

/// The library's directory, when [`found`].
pub fn dir() -> Option<&'static Path> {
    found().ok().map(|it| it.dir.as_path())
}

/// Whether `path` is a file of the library.
pub fn has(path: &Path) -> bool {
    dir().is_some_and(|dir| path.starts_with(dir))
}

fn look() -> Result<Library, String> {
    if let Some(named) = std::env::var_os(VAR).filter(|it| !it.is_empty()) {
        let dir = PathBuf::from(&named);
        return Library::at(dir.clone()).ok_or_else(|| {
            format!(
                "{VAR} names {}, which is not a standard library blaze can read: it is a \
                 directory named `library` with `Cargo.toml`, `core/src/lib.rs`, `UPSTREAM` and \
                 `DIGEST` (`python3 \
                 harness/library.py digest '{{}}'` writes it)",
                dir.display()
            )
        });
    }
    candidates()
        .into_iter()
        .find_map(Library::ours_at)
        .ok_or_else(|| format!("blaze's standard library (digest {}) is not here", short()))
}

/// Where a copy of the library this blaze was built with can be, with nothing fetched.
fn candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe().and_then(std::fs::canonicalize) {
        dirs.extend(exe.ancestors().skip(1).take(3).map(|it| it.join("share/blaze/library")));
    }
    dirs.extend(own());
    dirs
}

/// blaze's own copy of the library: `~/.blaze/library/<digest>/library`, in a directory of
/// its own, so that every file under the one it is in is the library's.
fn own() -> Option<PathBuf> {
    Some(super::own::home()?.join("library").join(digest()).join("library"))
}

/// The library of the checkout this blaze was built in.
fn checkout() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library")
}

fn short() -> &'static str {
    &digest()[..digest().len().min(12)]
}

/// [`found`], else blaze's own copy, made in `~/.blaze/library` once: copied from the checkout
/// this blaze was built in while it has the same library, else downloaded from the release this
/// blaze is (the tag of its version, `v0.YYYYMMDD.N`). `Err` says what
/// failed and what to do.
pub fn fetch() -> Result<Library, String> {
    if let Ok(found) = found() {
        return Ok(found.clone());
    }
    let dir = own().ok_or_else(|| {
        "blaze keeps its standard library in .blaze in your home directory, and there is no \
         home directory: `BLAZE_HOME` names where to keep it"
            .to_owned()
    })?;
    let given = std::env::var("BLAZE_LIBRARY_FROM").ok().filter(|it| !it.is_empty());
    let from = match (Library::ours_at(checkout()), given) {
        (Some(_), _) => checkout().to_string_lossy().into_owned(),
        (None, Some(given)) => given,
        // The release this blaze is: its version, which only the engine's build.rs knows
        // (`BLAZE_RELEASE`), so that no release changes this crate.
        (None, None) => format!("{RELEASES}/v{}", crate::engine::get().version),
    };
    let fetched = fetch_into(&dir, &from, digest()).map_err(|e| {
        format!(
            "blaze compiles the standard library from its own source (digest {}), which it \
             gets once, and getting it from {from} failed: {e:#}. Running the command again \
             tries again; a checkout of blaze at the commit this blaze was built from has it in \
             library/, which `{VAR}` can name",
            short()
        )
    })?;
    Ok(FETCHED.get_or_init(|| fetched).clone())
}

/// The name of the archive of the library with `digest` among a release's assets.
pub fn archive_name(digest: &str) -> String {
    format!("library-{digest}.tar.xz")
}

/// The library with digest `want` in `dir`, unless it is there: copied from `from` when that
/// is a library, else unpacked from its archive in `from` (a release's address, or a directory
/// that has it).
fn fetch_into(dir: &Path, from: &str, want: &str) -> anyhow::Result<Library> {
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        anyhow::bail!("{} has no place for it", dir.display());
    };
    let name = name.to_string_lossy();
    std::fs::create_dir_all(parent)?;
    let lock = std::fs::File::create(parent.join(format!(".{name}.lock")))?;
    lock.lock()?;
    if let Some(there) = Library::at(dir.to_owned()).filter(|it| it.digest == want) {
        return Ok(there);
    }
    let archive = archive_name(want);
    let copied = Library::at(PathBuf::from(from)).map(|it| it.dir);
    let short = &want[..want.len().min(12)];
    // A copy says nothing: the first process may be one answering for rustc (`-vV`), whose
    // output is rustc's alone.
    let said = copied.is_none();
    if said {
        eprintln!("Downloading blaze's standard library (digest {short}, once) from {from}...");
    }
    let start = std::time::Instant::now();
    let staging = parent.join(format!(".{name}.unpacking"));
    _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let library = staging.join("library");
    let unpacked = (|| {
        match &copied {
            Some(from) => copy_all(from, &library)?,
            None => {
                let bytes = download(from, &archive)?;
                let mut xz = lzma_rust2::XzReader::new(std::io::Cursor::new(&bytes), false);
                tar::Archive::new(&mut xz).unpack(&staging)?;
                std::io::copy(&mut xz, &mut std::io::sink())?;
            }
        }
        let got = digest_of(&library)?;
        if got != want {
            let what = copied.as_ref().map_or(archive.clone(), |it| it.display().to_string());
            anyhow::bail!(
                "{what} is not the library blaze was built with: its files' digest is {got} \
                 (`python3 harness/library.py digest '{{}}'` writes a changed library's DIGEST)"
            );
        }
        _ = std::fs::remove_dir_all(dir);
        std::fs::rename(&library, dir)?;
        anyhow::Ok(())
    })();
    _ = std::fs::remove_dir_all(&staging);
    unpacked?;
    if said {
        eprintln!("Made blaze's standard library {short} in {:.1?}.", start.elapsed());
    }
    let fetched = Library::at(dir.to_owned()).filter(|it| it.digest == want);
    fetched.ok_or_else(|| anyhow::anyhow!("{archive} has no DIGEST that says {want}"))
}

/// Copies the files, links and directories under `from` to `to`, which is made.
fn copy_all(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (kind, there) = (entry.file_type()?, to.join(entry.file_name()));
        if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &there)?;
            #[cfg(not(unix))]
            std::fs::copy(entry.path(), &there).map(drop)?;
        } else if kind.is_dir() {
            copy_all(&entry.path(), &there)?;
        } else {
            std::fs::copy(entry.path(), &there)?;
        }
    }
    Ok(())
}

fn download(from: &str, file: &str) -> anyhow::Result<Vec<u8>> {
    if !from.contains("://") {
        return Ok(std::fs::read(Path::new(from).join(file))?);
    }
    let address = url::Url::parse(&format!("{}/", from.trim_end_matches('/')))?;
    let runtime = remote::runtime()?;
    runtime.block_on(async {
        let web = Https::open(&address, PATIENCE, Arc::new(Traffic::new(8)))?;
        let mut tries = 0;
        loop {
            let got = async {
                let mut body =
                    web.read_from(file, 0).await?.ok_or_else(|| anyhow::anyhow!("not there"))?;
                let mut bytes = Vec::new();
                while let Some(piece) =
                    tokio::time::timeout(PATIENCE, remote::https_next(&mut body)).await??
                {
                    bytes.extend_from_slice(&piece);
                }
                anyhow::Ok(bytes)
            }
            .await;
            match got {
                Ok(bytes) => return Ok(bytes),
                Err(e) if tries + 1 == TRIES => return Err(e.context(file.to_owned())),
                Err(_) => tries += 1,
            }
            tokio::time::sleep(Duration::from_secs(tries.into())).await;
        }
    })
}

/// The digest of the library at `dir`: the sha256 of a line per file and link under it but
/// `DIGEST`, in the order of their paths' bytes, each `<kind> <sha256 of its bytes, or of
/// where a link points> <path>`, kind `file` or `link`. `harness/library.py` writes the same.
/// Whether a file is executable is not in it: Windows does not say.
pub fn digest_of(dir: &Path) -> std::io::Result<String> {
    let mut found = Vec::new();
    list(dir, "", &mut found)?;
    found.sort_unstable_by(|a, b| a.1.as_bytes().cmp(b.1.as_bytes()));
    let mut all = Sha256::new();
    for (kind, path, full) in found {
        let bytes = match kind {
            "link" => std::fs::read_link(&full)?.to_string_lossy().into_owned().into_bytes(),
            _ => {
                let mut bytes = Vec::new();
                std::fs::File::open(&full)?.read_to_end(&mut bytes)?;
                bytes
            }
        };
        let hash = crate::content::hex(&Sha256::digest(&bytes));
        all.update(format!("{kind} {hash} {path}\n").as_bytes());
    }
    Ok(crate::content::hex(&all.finalize()))
}

fn list(
    dir: &Path,
    prefix: &str,
    found: &mut Vec<(&'static str, String, PathBuf)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            found.push(("link", path, entry.path()));
        } else if kind.is_dir() {
            list(&entry.path(), &path, found)?;
        } else if path != DIGEST_FILE {
            found.push(("file", path, entry.path()));
        }
    }
    Ok(())
}

/// The commit the library this blaze was built with is a copy of.
pub fn built_commit() -> &'static str {
    upstream_commit(UPSTREAM).expect("library/UPSTREAM has a commit: harness/library.py import")
}

/// Says where the library is to the processes this one starts ([`VAR`]), and to the front
/// end of this one, when no one did: what was found, else blaze's own copy made from the
/// checkout this blaze was built in (nothing is downloaded), else empty for none, which the
/// front end turns down saying so. Before any thread starts: it sets the environment.
pub fn publish() {
    if std::env::var_os(VAR).is_some_and(|it| !it.is_empty()) {
        return;
    }
    let copied = || {
        Library::ours_at(checkout())?;
        match fetch() {
            Ok(made) => Some(made),
            Err(why) => {
                _ = NOT_MADE.set(why);
                None
            }
        }
    };
    let dir = found().ok().cloned().or_else(copied);
    let dir = dir.map(|it| it.dir.into_os_string()).unwrap_or_default();
    // SAFETY: before any thread is started.
    unsafe { std::env::set_var(VAR, dir) };
}

/// [`publish`] of `library`, which [`fetch`] gave.
pub fn publish_fetched(library: &Library) {
    // SAFETY: before any thread is started.
    unsafe { std::env::set_var(VAR, &library.dir) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_library_is_the_one_its_digest_says() {
        assert_eq!(
            digest_of(&checkout()).unwrap(),
            digest(),
            "library/ changed: `python3 harness/library.py digest '{{}}'` writes its DIGEST"
        );
        assert_eq!(upstream_commit(UPSTREAM), Some(super::super::own::commit()), "the nightly's");
    }

    #[test]
    fn a_library_is_found_by_its_digest_and_fetched_checked() {
        let work = tempfile::tempdir().unwrap();
        let make = |at: &Path, core: &str, digest: Option<&str>| {
            std::fs::create_dir_all(at.join("core/src")).unwrap();
            std::fs::write(at.join("Cargo.toml"), "[workspace]\n").unwrap();
            std::fs::write(at.join("core/src/lib.rs"), core).unwrap();
            std::fs::write(at.join("UPSTREAM"), "commit abc\n").unwrap();
            let digest = digest.map_or_else(|| digest_of(at).unwrap(), str::to_owned);
            std::fs::write(at.join(DIGEST_FILE), format!("{digest}\n")).unwrap();
            digest
        };
        let archived = |dir: &Path, digest: &str, dist: &Path| {
            let mut tar = tar::Builder::new(Vec::new());
            tar.append_dir_all("library", dir).unwrap();
            let mut xz = lzma_rust2::XzWriter::new(Vec::new(), Default::default()).unwrap();
            std::io::Write::write_all(&mut xz, &tar.into_inner().unwrap()).unwrap();
            std::fs::create_dir_all(dist).unwrap();
            std::fs::write(dist.join(archive_name(digest)), xz.finish().unwrap()).unwrap();
        };
        let one = work.path().join("one/library");
        let want = make(&one, "// core\n", None);
        let found = Library::at(one.clone()).unwrap();
        assert_eq!((found.digest.as_str(), found.commit.as_str()), (want.as_str(), "abc"));
        assert!(Library::ours_at(one.clone()).is_none(), "another library than blaze's");
        let elsewhere = work.path().join("elsewhere");
        make(&elsewhere, "// core\n", None);
        assert!(Library::at(elsewhere).is_none(), "its files are named from `library/`");
        // DIGEST is not part of what it is the digest of.
        assert_eq!(digest_of(&one).unwrap(), want);

        // An archive whose files are not what its digest says is refused, and nothing is kept.
        let lying = work.path().join("lying/library");
        make(&lying, "// another core\n", Some(&want));
        let (bad, good) = (work.path().join("bad"), work.path().join("good"));
        archived(&lying, &want, &bad);
        let into = work.path().join("home/library").join(&want).join("library");
        let said = fetch_into(&into, bad.to_str().unwrap(), &want).unwrap_err().to_string();
        assert!(said.contains("is not the library blaze was built with"), "{said}");
        assert!(!into.exists());

        // The library is fetched whole, and found there after with nothing fetched.
        archived(&one, &want, &good);
        let fetched = fetch_into(&into, good.to_str().unwrap(), &want).unwrap();
        assert_eq!((fetched.dir.as_path(), fetched.digest.as_str()), (into.as_path(), &*want));
        assert_eq!(digest_of(&into).unwrap(), want);
        std::fs::remove_dir_all(&good).unwrap();
        assert!(fetch_into(&into, good.to_str().unwrap(), &want).is_ok());

        // A checkout's library is copied, checked as an archive is.
        let copy = work.path().join("home/library/copy/library");
        let said = fetch_into(&copy, lying.to_str().unwrap(), &want).unwrap_err().to_string();
        assert!(said.contains("is not the library blaze was built with"), "{said}");
        let copied = fetch_into(&copy, one.to_str().unwrap(), &want).unwrap();
        assert_eq!(digest_of(&copied.dir).unwrap(), want);
    }
}
