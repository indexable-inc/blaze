//! A package's `.crate` file: found in blaze's store or cargo's cache, or downloaded by the
//! registry's `dl` template, all through blaze-fetch with its sha256 as the pin; kept in the
//! store and in cargo's cache; and unpacked where cargo unpacks it, with cargo 1.99's checks
//! (`src/sources/registry/{download,mod}.rs`).

use std::{
    fs::OpenOptions,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Context as _, bail};
use blaze_fetch::{FetchError, Fetcher, Form, Objects, Origin, Producer, Source, StoreOf};
use semver::Version;

use crate::registry::Registry;

pub use blaze_units::index::crate_url;

/// Marks a package unpacked whole. Not the `.cargo-ok` of a git checkout.
const PACKAGE_SOURCE_LOCK: &str = ".cargo-ok";
const MAX_UNPACK_SIZE: u64 = 512 * 1024 * 1024;
const MAX_COMPRESSION_RATIO: u64 = 20;
/// The time cargo gives the files `cargo package` makes, which a `.crate` has no time for.
const DETERMINISTIC_TIMESTAMP: i64 = 1153704088;

/// A package of a registry to fetch: the checksum is the lock file's, or the index's.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Package {
    pub name: String,
    pub version: Version,
    pub checksum: String,
}

impl std::fmt::Display for Package {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} v{}", self.name, self.version)
    }
}

/// Where a package was found, which a fetch counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum From {
    /// Already unpacked.
    Unpacked,
    Store,
    CargoCache,
    Network,
    /// Read where it is vendored, from a directory source.
    Vendored,
    /// In a checkout of a git source.
    Git,
}

/// The 32 bytes of a checksum written in hex.
pub fn parse_checksum(hex: &str) -> Option<[u8; 32]> {
    blaze_format::hex::decode(hex)
}

impl Registry {
    /// Where a package is unpacked: `$CARGO_HOME/registry/src/<registry>/<name>-<version>`.
    pub fn package_dir(&self, name: &str, version: &Version) -> PathBuf {
        self.src_dir().join(format!("{name}-{version}"))
    }

    /// Fetches the packages, `jobs` at a time, and unpacks each where cargo does. Holds cargo's
    /// package cache lock while it does, as cargo holds it while it downloads.
    pub fn fetch(&self, packages: &[Package]) -> anyhow::Result<Vec<(PathBuf, From)>> {
        let _lock = crate::PackageCacheLock::acquire(&self.context().cargo_home)?;
        let next = AtomicUsize::new(0);
        let results: Vec<blaze_core::lock::Lock<Option<anyhow::Result<(PathBuf, From)>>>> =
            packages.iter().map(|_| blaze_core::lock::Lock::new("registry.fetch.result", None)).collect();
        let jobs = self.context().net.jobs.max(1).min(packages.len().max(1));
        std::thread::scope(|scope| {
            for _ in 0..jobs {
                scope.spawn(|| {
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some(package) = packages.get(at) else { break };
                        let result = self.fetch_one(package);
                        *results[at].lock() = Some(result);
                    }
                });
            }
        });
        let mut out = Vec::with_capacity(packages.len());
        for (package, result) in packages.iter().zip(results) {
            let result = result.into_inner().context("a fetch did not finish")?;
            out.push(result.with_context(|| format!("failed to download `{package}`"))?);
        }
        Ok(out)
    }

    /// One package, unpacked, and where its bytes came from. The caller holds the lock.
    pub fn fetch_one(&self, package: &Package) -> anyhow::Result<(PathBuf, From)> {
        let dst = self.package_dir(&package.name, &package.version);
        if is_unpacked(&dst)? {
            return Ok((dst, From::Unpacked));
        }
        let (bytes, from) = self.crate_bytes(package)?;
        unpack_package(&bytes, &dst)
            .with_context(|| format!("failed to unpack package `{package}`"))?;
        Ok((dst, from))
    }

    /// The bytes of a package's `.crate` file, got through blaze-fetch with its checksum as the
    /// sha256 pin, and kept in the store and in cargo's cache.
    pub fn crate_bytes(&self, package: &Package) -> anyhow::Result<(Vec<u8>, From)> {
        let pin = crate::pinned::crate_pin(&package.checksum)
            .with_context(|| format!("`{}` is not a sha256 checksum", package.checksum))?;
        let context = self.context();
        let objects = context.store.map(Objects::new);
        let cache_file = self.crate_file(&package.name, &package.version);
        // cargo's words for a `.crate` that is not its checksum's, over blaze-fetch's (which
        // name both digests) as its cause.
        let checked = |e: FetchError| match e {
            FetchError::Mismatch { .. } => anyhow::Error::new(e)
                .context(format!("failed to verify the checksum of `{package}`")),
            e => anyhow::Error::new(e),
        };
        if let Some(objects) = objects
            && let Some(object) = objects.lookup(Origin::Pinned { pin: &pin, form: Form::Blob })?
        {
            // alloc: the `.crate` the caller is given, once a package
            let bytes = objects.read(&object)?;
            if !self.is_local() && context.keep_crate_files {
                keep_crate_file(&cache_file, &bytes)?;
            }
            return Ok((bytes, From::Store));
        }
        // alloc: once a package read from cargo's cache
        let cached = Producer::Given(cache_file.display().to_string());
        match std::fs::read(&cache_file) {
            Ok(bytes) if !bytes.is_empty() => {
                // Checked against the pin and, with a store, kept in it.
                let kept = match objects {
                    Some(objects) => objects.keep_pinned(&pin, cached, &bytes).map(drop),
                    None => blaze_fetch::verify(cached, &pin, &bytes).map(drop),
                };
                match kept {
                    Ok(()) => return Ok((bytes, From::CargoCache)),
                    Err(e @ FetchError::Mismatch { .. }) if self.is_local() => {
                        return Err(checked(e));
                    }
                    // Not what the index says: downloaded again in its place.
                    Err(FetchError::Mismatch { .. }) => _ = std::fs::remove_file(&cache_file),
                    Err(e) => return Err(e.into()),
                }
            }
            _ if self.is_local() => bail!("failed to read `{}`", cache_file.display()),
            // Cut short: downloaded again in its place.
            Ok(_) => _ = std::fs::remove_file(&cache_file),
            Err(_) => {}
        }
        if context.net.offline {
            bail!(
                "attempting to make an HTTP request, but --offline was specified \
                 (`{package}` is not on disk)"
            );
        }
        let config = self.config()?.context("config.json not found")?;
        let url =
            crate_url(&config.dl, &package.name, &package.version.to_string(), &package.checksum);
        let token = match config.auth_required {
            true => Some(self.download_token()?),
            false => None,
        };
        let headers: Vec<(&str, &str)> =
            token.as_deref().map(|it| ("authorization", it)).into_iter().collect();
        // The download's temporary file goes in cargo's cache directory when that cache is
        // kept; with none kept, cargo's directory is never made, and the file goes to a
        // directory of this download's own in the system's temporary directory, made new and
        // private (see `PrivateDir`), removed when the download ends.
        let private;
        let scratch = match context.keep_crate_files {
            true => cache_file.parent().context("a `.crate` file has a directory")?.to_owned(), // alloc: once a download
            false => {
                private = PrivateDir::new()?;
                private.0.clone() // clone: once a download
            }
        };
        let store = match context.store {
            Some(store) => StoreOf::Given(store),
            None => StoreOf::Unkept,
        };
        let net = Arc::new(crate::pinned::Net::new(Arc::clone(&context.http)));
        let fetcher = Fetcher::new(store, net, scratch)
            .with_policy(crate::pinned::policy(&context.net.retry));
        let bytes = match objects {
            Some(objects) => {
                let source = Source::Url { url, digest: pin };
                let object = fetcher.get_with(&source, &headers).map_err(checked)?;
                // alloc: the `.crate` the caller is given, once a package
                objects.read(&object)?
            }
            None => fetcher.download_verified(&url, &pin, &headers).map_err(checked)?.bytes,
        };
        if context.keep_crate_files {
            keep_crate_file(&cache_file, &bytes)?;
        }
        Ok((bytes, From::Network))
    }
}

/// A download's scratch directory in the shared temporary directory: made with `create` (never
/// one that is already there, which may be another user's) and, on unix, mode 0700, so no one
/// else can place or swap a file in it; removed, with whatever is left in it, when it drops.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn new() -> anyhow::Result<PrivateDir> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        // The name only has to be new: a name already taken is refused, not shared, and the
        // nanoseconds make one hard to take ahead of time.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |it| it.subsec_nanos());
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // alloc: once a download
        let dir =
            std::env::temp_dir().join(format!("blaze-fetch-{}-{n}-{nanos}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&dir).with_context(|| {
            format!("failed to make {}, a download's private scratch directory (a name already taken is never shared)", dir.display())
        })?;
        Ok(PrivateDir(dir))
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes a `.crate` into cargo's cache, as cargo would have, so that cargo finds it offline.
/// A cache asked for that cannot be written is the error.
fn keep_crate_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if std::fs::metadata(path).is_ok_and(|it| it.len() > 0) {
        return Ok(());
    }
    path.parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| crate::write_whole(path, bytes))
        .with_context(|| format!("failed to keep {}", path.display()))
}

/// Whether a package directory was unpacked whole by cargo 1.71 or later, or blaze; one left by
/// an interrupted or older unpacking is removed, to be unpacked again.
fn is_unpacked(dst: &Path) -> anyhow::Result<bool> {
    let ok = dst.join(PACKAGE_SOURCE_LOCK);
    match std::fs::read_to_string(&ok) {
        Ok(text) => {
            #[derive(serde_derive::Deserialize)]
            struct LockMetadata {
                v: u32,
            }
            if serde_json::from_str::<LockMetadata>(&text).is_ok_and(|it| it.v == 1) {
                return Ok(true);
            }
            std::fs::remove_dir_all(dst)
                .with_context(|| format!("failed to remove `{}`", dst.display()))?;
            Ok(false)
        }
        // No `.cargo-ok`: either nothing is there, or an unpacking that stopped (a Ctrl-C, a
        // crash, a refused archive, a cargo killed mid-unpack in a shared registry) left files,
        // which are removed so the unpacking, whose files are each created new, starts clean.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(dst) {
                Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(dst),
                Ok(_) => std::fs::remove_file(dst),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
            .with_context(|| {
                format!("failed to remove `{}`, an unpacking that did not finish", dst.display())
            })?;
            Ok(false)
        }
        Err(e) => bail!("unable to read .cargo-ok file at {ok:?}: {e}"),
    }
}

/// Unpacks a `.crate` into `dst`, `<registry src>/<name>-<version>`, and marks it unpacked
/// (cargo's `unpack_package`).
pub fn unpack_package(tarball: &[u8], dst: &Path) -> anyhow::Result<u64> {
    std::fs::create_dir_all(dst)
        .with_context(|| format!("failed to create `{}`", dst.display()))?;
    let written = unpack(tarball, dst, |_| true)?;
    set_generated_mtimes(dst);
    let path = dst.join(PACKAGE_SOURCE_LOCK);
    let mut ok = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open `{}`", path.display()))?;
    write!(ok, "{{\"v\":1}}")?;
    Ok(written)
}

/// Unpacks a `.crate` into `dst`, which is named `<name>-<version>`, with only the files
/// `include` takes by their path in the package, and no mark that it is unpacked: what
/// `cargo vendor` writes (cargo's `unpack_package_in`).
pub fn unpack_package_in(
    tarball: &[u8],
    dst: &Path,
    include: impl Fn(&Path) -> bool,
) -> anyhow::Result<u64> {
    let written = unpack(tarball, dst, include)?;
    set_generated_mtimes(dst);
    Ok(written)
}

/// The files `cargo package` writes, given one time whatever the tarball says
/// (rust-lang/cargo#16237).
fn set_generated_mtimes(dst: &Path) {
    for file in ["Cargo.lock", "Cargo.toml", ".cargo_vcs_info.json"] {
        let time = filetime::FileTime::from_unix_time(DETERMINISTIC_TIMESTAMP, 0);
        _ = filetime::set_file_mtime(dst.join(file), time);
    }
}

/// Unpacks a tarball whose every entry is under `<name>-<version>/`, files and directories
/// only, into `dst`, `<name>-<version>` itself, with no more than cargo lets one unpack to, but
/// for the entries `include` leaves out: through blaze-fetch's one checked unpacker
/// ([`blaze_fetch::unpack`]) with cargo's rules (tar's modes under the umask, each file's
/// time the entry's, a file listed again replacing the first, `.cargo-ok` dropped). Unlike
/// tar, an entry with a `..` or an absolute path is refused, not skipped.
fn unpack(tarball: &[u8], dst: &Path, include: impl Fn(&Path) -> bool) -> anyhow::Result<u64> {
    let limit = MAX_UNPACK_SIZE.max(tarball.len() as u64 * MAX_COMPRESSION_RATIO);
    let gz = flate2::read::GzDecoder::new(tarball);
    let prefix = dst
        .file_name()
        .and_then(|it| it.to_str())
        .context("a package directory has a UTF-8 name")?;
    std::fs::create_dir_all(dst)
        .with_context(|| format!("failed to create `{}`", dst.display()))?;
    #[cfg(unix)]
    let mask = umask();
    #[cfg(not(unix))]
    let mask = 0;
    let rules = blaze_fetch::unpack::Rules {
        names: blaze_fetch::unpack::NameRules {
            top: Some(prefix),
            fold: false,
            twice: blaze_fetch::unpack::Twice::Replace,
            reserved: &[],
            most_path: usize::MAX,
        },
        links: false,
        modes: blaze_fetch::unpack::Modes::Cargo { mask },
        skip: &[PACKAGE_SOURCE_LOCK],
        include: &include,
        sync: false,
        most_entries: u64::MAX,
        most_bytes: u64::MAX,
    };
    match blaze_fetch::unpack::unpack(LimitErrorReader { inner: gz, left: limit }, dst, &rules) {
        Ok(unpacked) => Ok(unpacked.bytes),
        Err(refused) => {
            let reserved = cfg!(windows)
                && refused
                    .path
                    .as_deref()
                    .is_some_and(|it| is_windows_reserved_path(Path::new(it)));
            let error = anyhow::anyhow!("invalid tarball downloaded for {prefix}: {refused}");
            match reserved {
                true => Err(error.context(format!(
                    "`{}` appears to contain a reserved Windows path, it cannot be extracted on Windows",
                    refused.path.unwrap_or_default()
                ))),
                false => Err(error),
            }
        }
    }
}

/// A reader that fails once more than `left` bytes are read from it, as cargo's
/// `LimitErrorReader`.
pub struct LimitErrorReader<R> {
    pub inner: R,
    pub left: u64,
}

impl<R: Read> Read for LimitErrorReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let most =
            buf.len().min(usize::try_from(self.left.saturating_add(1)).unwrap_or(usize::MAX));
        let read = self.inner.read(&mut buf[..most])?;
        if read as u64 > self.left {
            return Err(io::Error::other("maximum limit reached when reading"));
        }
        self.left -= read as u64;
        Ok(read)
    }
}

/// The process's umask, read once (cargo's `get_umask`).
#[cfg(unix)]
fn umask() -> u32 {
    #[expect(
        clippy::disallowed_types,
        reason = "the process's umask, which reading sets: read once, before any thread unpacks"
    )]
    static UMASK: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *UMASK.get_or_init(|| {
        // SAFETY: `umask` cannot fail; it is set straight back to what it was, once, before
        // any unpacking.
        unsafe {
            let mask = libc::umask(0o022);
            libc::umask(mask);
            u32::from(mask)
        }
    })
}

/// Whether a path names one of Windows' devices, as cargo's `is_windows_reserved_path`: each
/// of its components as `Path::iter` splits them (at `/`, and on Windows `\` too).
pub fn is_windows_reserved_path(path: &Path) -> bool {
    const RESERVED: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    path.iter().filter_map(|it| it.to_str()).any(|component| {
        let stem = component.split('.').next().unwrap_or(component);
        RESERVED.contains(&stem.to_ascii_lowercase().as_str())
    })
}

/// The sha256 of some bytes in hex, as an index's `cksum` writes it.
pub fn checksum(bytes: &[u8]) -> String {
    blaze_format::hex::encode(&blaze_sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// blaze-cargo's copy (`serve/fetch/archive.rs`), which took a tarball's `/`-separated
    /// entry path as text; kept to check this one agrees on such paths.
    fn reserved_text(path: &str) -> bool {
        const RESERVED: [&str; 22] = [
            "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7",
            "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
        ];
        path.split('/').any(|component| {
            let stem = component.split('.').next().unwrap_or(component);
            RESERVED.contains(&stem.to_ascii_lowercase().as_str())
        })
    }

    #[test]
    fn a_reserved_path_is_one_on_either_copy() {
        let parts =
            ["a", "con", "CON.txt", "nul.tar.gz", "com1", "com10", "lpt9.rs", ".", "", "x.con"];
        let mut paths = vec![String::new()];
        for first in parts {
            for second in parts {
                paths.push(format!("{first}/{second}"));
                paths.push(format!("pkg-1.0.0/{first}/{second}"));
            }
        }
        for path in &paths {
            assert_eq!(is_windows_reserved_path(Path::new(path)), reserved_text(path), "`{path}`");
        }
        assert!(is_windows_reserved_path(Path::new("pkg-1.0.0/src/aux.rs")));
        assert!(!is_windows_reserved_path(Path::new("pkg-1.0.0/src/auxiliary.rs")));
    }

    #[test]
    fn a_reader_fails_past_its_limit() {
        let mut ok = LimitErrorReader { inner: &[1u8, 2, 3][..], left: 3 };
        let mut out = Vec::new();
        assert_eq!(ok.read_to_end(&mut out).unwrap(), 3);
        let mut over = LimitErrorReader { inner: &[1u8, 2, 3][..], left: 2 };
        let error = over.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(error.to_string(), "maximum limit reached when reading");
    }
}
