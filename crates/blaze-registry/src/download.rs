//! A package's `.crate` file: found in blaze's store or cargo's cache, or downloaded by the
//! registry's `dl` template; checked against its sha256; kept in the store by that hash and in
//! cargo's cache; and unpacked where cargo unpacks it, with cargo 1.99's checks
//! (`src/sources/registry/{download,mod}.rs`).

use std::{
    fs::OpenOptions,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context as _, bail};
use semver::Version;
use tar::{Archive, EntryType};

use crate::{http::HttpNotSuccessful, registry::Registry};

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

/// The schema of a `.crate` file as the store keeps it: the file's bytes, as downloaded.
const CRATE_FILE: blaze_format::SchemaHash = blaze_format::schema("a registry's .crate file 1");

/// What the store keeps the `.crate` file whose sha256 is `sha256` under: the file is named by
/// that hash alone, so every registry and every fetch shares it; nothing made it but the
/// download, which is checked against the hash before it is kept.
pub fn crate_key(sha256: &[u8; 32]) -> blaze_cas::Key {
    let producer = blaze_ids::ContentId::of("blaze v2 registry download of a .crate file 1", &[]);
    blaze_cas::Key::of(CRATE_FILE, producer, &[blaze_ids::ContentId(*sha256)])
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
        let results: Vec<parking_lot::Mutex<Option<anyhow::Result<(PathBuf, From)>>>> =
            packages.iter().map(|_| parking_lot::Mutex::new(None)).collect();
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

    /// The bytes of a package's `.crate` file, checked against its checksum, and kept in the
    /// store and in cargo's cache.
    pub fn crate_bytes(&self, package: &Package) -> anyhow::Result<(Vec<u8>, From)> {
        let key = parse_checksum(&package.checksum)
            .with_context(|| format!("`{}` is not a sha256 checksum", package.checksum))?;
        let store = self.context().store;
        let matches = |bytes: &[u8]| blaze_sha256::digest(bytes) == key;
        let keep = |bytes: &[u8]| -> anyhow::Result<()> {
            let Some(store) = store else { return Ok(()) };
            // clone: a download kept, once a package
            let put = store.put(crate_key(&key), blaze_cas::Kind::Output, bytes.to_vec());
            put.map(drop).map_err(|it| {
                anyhow::anyhow!("the store keeps other bytes than `{package}`'s under its sha256 ({:x})", it.kept)
            })
        };
        let cache_file = self.crate_file(&package.name, &package.version);
        let kept = match store {
            Some(store) => store
                // alloc: the `.crate` the caller is given, once a package
                .read(&crate_key(&key), |it| it.bytes.to_vec())
                // alloc: error path
                .map_err(|e| anyhow::anyhow!("the store's `.crate` of `{package}`: {e}"))?,
            None => None,
        };
        if let Some(bytes) = kept {
            if !matches(&bytes) {
                bail!("the store's `.crate` of `{package}` is not of its sha256 `{}`", package.checksum);
            }
            if !self.is_local() && self.context().keep_crate_files {
                keep_crate_file(&cache_file, &bytes)?;
            }
            return Ok((bytes, From::Store));
        }
        match std::fs::read(&cache_file) {
            Ok(bytes) if !bytes.is_empty() && matches(&bytes) => {
                keep(&bytes)?;
                return Ok((bytes, From::CargoCache));
            }
            Ok(bytes) if !bytes.is_empty() && self.is_local() => {
                bail!("failed to verify the checksum of `{package}`")
            }
            _ if self.is_local() => bail!("failed to read `{}`", cache_file.display()),
            // Not what the index says, or cut short: downloaded again in its place.
            Ok(_) => _ = std::fs::remove_file(&cache_file),
            Err(_) => {}
        }
        if self.context().net.offline {
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
        let http = &self.context().http;
        let bytes = self.context().net.retry.run(|| {
            let answer = http.get(&url, &headers)?;
            if answer.status != 200 {
                return Err(HttpNotSuccessful::new(&url, &answer).into());
            }
            Ok(answer.body)
        })?;
        if !matches(&bytes) {
            bail!("failed to verify the checksum of `{package}`");
        }
        keep(&bytes)?;
        if self.context().keep_crate_files {
            keep_crate_file(&cache_file, &bytes)?;
        }
        Ok((bytes, From::Network))
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
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
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
/// only, into the directory above `dst`, with no more than cargo lets one unpack to, but for
/// the entries `include` leaves out.
fn unpack(tarball: &[u8], dst: &Path, include: impl Fn(&Path) -> bool) -> anyhow::Result<u64> {
    let limit = MAX_UNPACK_SIZE.max(tarball.len() as u64 * MAX_COMPRESSION_RATIO);
    let gz = flate2::read::GzDecoder::new(tarball);
    let mut tar = Archive::new(LimitErrorReader { inner: gz, left: limit });
    #[cfg(unix)]
    tar.set_mask(umask());
    let prefix = dst.file_name().context("a package directory has a name")?;
    let parent = dst.parent().context("a package directory has a parent")?;
    let mut written = 0;
    for entry in tar.entries()? {
        let mut entry = entry.context("failed to iterate over archive")?;
        let entry_path = entry.path().context("failed to read entry path")?.into_owned();
        match entry_path.strip_prefix(prefix) {
            Ok(inside) if !include(inside) => continue,
            Ok(_) => {}
            Err(_) => bail!(
                "invalid tarball downloaded, contains a file at {entry_path:?} which isn't under {prefix:?}"
            ),
        }
        match entry.header().entry_type() {
            EntryType::Regular | EntryType::Directory => {}
            kind => bail!(
                "invalid tarball downloaded, contains an entry at {entry_path:?} with invalid type {kind:?}"
            ),
        }
        if entry_path.file_name().is_some_and(|it| it == PACKAGE_SOURCE_LOCK) {
            continue;
        }
        written += entry.size();
        let mut result = entry.unpack_in(parent).map_err(anyhow::Error::from);
        if cfg!(windows) && is_windows_reserved_path(&entry_path) {
            result = result.with_context(|| {
                format!(
                    "`{}` appears to contain a reserved Windows path, it cannot be extracted on Windows",
                    entry_path.display()
                )
            });
        }
        result.with_context(|| format!("failed to unpack entry at `{}`", entry_path.display()))?;
    }
    Ok(written)
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
        let parts = ["a", "con", "CON.txt", "nul.tar.gz", "com1", "com10", "lpt9.rs", ".", "", "x.con"];
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
