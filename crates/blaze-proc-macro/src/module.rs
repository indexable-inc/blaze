//! The module record: a proc-macro crate built for `wasm32-wasip1` with blaze's shim linked in,
//! found by the crate's identity. A bootstrap builder writes it (decision D52:
//! scripts/src/bin/proc_macro_modules.rs), never `blaze`; a call whose record is absent is
//! [`ModuleError::Missing`], a finding naming the crate.
//!
//! The key is the SHA-256 of [`identity`]: the name, version and [`Source`] of the crate (the
//! lock's checksum, a git commit and path, or a path package's [`files_digest`]), the
//! dependencies the lock pins under it ([`LockedPackages::closure`]: two locks pinning another syn under
//! one serde_derive are two modules),
//! its features (the host side's unified set, sorted), the toolchain, the digests of the target
//! JSON and of the shim, and the builder. The builder computes the same text; the example
//! `tempo_derives` finds each built module by the key computed here.
//!
//! `<store>/proc-macro/modules/<key>/` holds `identity`, `module.wasm`, `wasm-sha256`,
//! `macros` (one line per macro: its index in the module, `derive`, `attr` or `bang`, its name,
//! a derive's helpers joined by commas) and `decls` (the exported global holding the list).
//! The runtime keeps the module's machine code apart, in the store's one compiled folder
//! `<store>/proc-macro/compiled/<code key>` and its `.seal`, keyed by the wasm's digest and the
//! runtime ([`COMPILED_DIR`]; `host.rs`, "Compiled records").

use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::{fs::{self, File, read, read_dir}, io::{Error as IoError, ErrorKind, Read, Write as _}, path::{Path, PathBuf}, process};

use blaze_build_script::served::{InPackage, in_package};
use blaze_expand::ProcKind;
use blaze_manifest::lock::{Checksum, Lock, LockSource};

/// The target a module is built for.
pub const TARGET_JSON: &[u8] = include_bytes!("../guest/wasm32-wasip1-macro.json");
/// The shim linked into each module.
pub const SHIM: &[u8] = include_bytes!("../guest/shim.rs");

/// A proc-macro crate as the lock and the unit graph name it.
#[derive(Clone, Debug)]
pub struct CrateId<'s> {
    pub name: &'s str,
    pub version: &'s str,
    /// Where its sources are, as what pins their bytes.
    pub source: Source<'s>,
    /// [`LockedPackages::closure`] of it: the dependencies the lock pins under it.
    pub deps: &'s str,
    /// The features the host side of the graph unifies to.
    pub features: &'s [&'s str],
}

/// What pins a proc-macro crate's sources: the line of [`identity`] that names them.
#[derive(Clone, Copy, Debug)]
pub enum Source<'s> {
    /// From a registry: the lock's `checksum` of the package (`checksum <hex>`).
    Registry { checksum: &'s str },
    /// From a git repository: the commit the lock pins and the package's folder in it, `.` at
    /// the root (`git <commit> <path>`).
    Git { commit: &'s str, path: &'s str },
    /// A workspace or path package: [`files_digest`] of its folder as it is
    /// (`files <hex>`), so an edit to the macro crate makes a new module.
    Path { files_sha256: &'s str },
}

/// Why a macro crate's dependency closure could not be read from a lock. A malformed lock never
/// reaches here: blaze-manifest's one reader ([`Lock::read`]) refuses it first, naming the line
/// of the entry (rule 30: one lock reader; rule 3: never a package named "").
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ClosureError {
    /// The lock has no package of this name and version.
    NotLocked { name: String, version: String },
    /// A path package in the closure: its sources are pinned by nothing in the lock.
    Unpinned { name: String, version: String },
    /// The resolved `wasm64-blaze-wasip1` unit graph has no unique `ProcMacro` unit matching this
    /// package and containing its requested feature set.
    WasmRootMissing { name: String, version: String },
}

impl std::fmt::Display for ClosureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClosureError::NotLocked { name, version } => write!(f, "the lock has no package {name} {version}"),
            ClosureError::Unpinned { name, version } => {
                write!(f, "{name} {version} is a path package under a proc-macro crate: the lock pins nothing of its sources")
            }
            ClosureError::WasmRootMissing { name, version } => {
                write!(f, "{name} {version}: no unique matching ProcMacro unit containing requested features in the wasm64 unit graph")
            }
        }
    }
}

/// A lock's packages as blaze-manifest read them ([`Lock::read`], read once a command), under
/// the source corrections: what [`LockedPackages::closure`] walks for every macro crate of the
/// lock.
pub struct LockedPackages<'l, 'a> {
    lock: &'l Lock<'a>,
    /// The source corrections (D58): a package a row corrects is pinned by its checksum and
    /// the row's digest.
    corrections: blaze_fetch::correct::Table<std::alloc::Global>,
}

impl<'l, 'a> LockedPackages<'l, 'a> {
    /// The packages of `lock`, under the checked-in source corrections.
    pub fn of(lock: &'l Lock<'a>) -> LockedPackages<'l, 'a> {
        LockedPackages::of_with(lock, blaze_fetch::correct::Table::checked_in(std::alloc::Global))
    }

    /// [`LockedPackages::of`] under another table of corrections (a test's edited row).
    pub fn of_with(lock: &'l Lock<'a>, corrections: blaze_fetch::correct::Table<std::alloc::Global>) -> LockedPackages<'l, 'a> {
        LockedPackages { lock, corrections }
    }

    /// The digest of the macro crate `name` `version`'s dependency closure as the lock pins it
    /// (the `deps` line of [`identity`]): every package reachable from it through the lock's
    /// `dependencies` lists, the crate itself left out, each as `name version pin` (the
    /// checksum, else the source as written), sorted, one a line, SHA-256. The lock lists every
    /// kind of dependency of every feature, so the closure is a superset of what a build links,
    /// keyed from the lock's text alone. The builder computes the same
    /// (`scripts/src/bin/proc_macro_modules.rs`). Two packages of one name and version (two
    /// sources): the first in the file's order is the root, as it was before (`Lock::named`
    /// keeps the file's order within a name).
    pub fn closure(&self, name: &str, version: &str) -> Result<String, ClosureError> {
        match self.closure_with(name, version, |_| Ok::<Option<String>, core::convert::Infallible>(None)) {
            Ok(result) => result,
            Err(never) => match never {},
        }
    }

    /// Like [`Self::closure`], but allows pinning workspace/path dependencies in the transitive
    /// closure via `path_pin(dep_lock_id)` (returning `Some(files_sha256)`). Any path dependency
    /// for which `path_pin` returns `None` is still refused as [`ClosureError::Unpinned`].
    pub fn closure_with<D: AsRef<str>, E>(
        &self,
        name: &str,
        version: &str,
        path_pin: impl FnMut(blaze_manifest::lock::LockId) -> Result<Option<D>, E>,
    ) -> Result<Result<String, ClosureError>, E> {
        let lock = self.lock;
        let Some(root) = lock
            .named(name)
            .find(|id| display_eq(&lock.package(*id).version, version))
        else {
            return Ok(Err(ClosureError::NotLocked { name: name.to_owned(), version: version.to_owned() }));
        };
        let alloc = std::alloc::Global;
        let mut seen = blaze_own::Vec::with_capacity_in(lock.packages.len(), alloc);
        seen.resize(lock.packages.len(), false);
        if let Some(slot) = seen.get_mut(root.index()) {
            *slot = true;
        }
        let mut stack = blaze_own::Vec::new_in(alloc);
        stack.push(root);
        while let Some(at) = stack.pop() {
            for dep in lock.package(at).deps.iter() {
                if let Some(slot) = seen.get_mut(dep.index())
                    && !*slot
                {
                    *slot = true;
                    stack.push(*dep);
                }
            }
        }
        let deps = seen
            .iter()
            .enumerate()
            .filter_map(|(at, &hit)| hit.then(|| u32::try_from(at).ok().map(blaze_manifest::lock::LockId)).flatten());
        match self.closure_of_packages(root, deps, path_pin, alloc)? {
            Ok(hex) => Ok(Ok(hex.as_str().to_owned())),
            Err(err) => Ok(Err(err)),
        }
    }

    /// Like [`Self::closure_with`], but hashes only the explicitly supplied dependency packages
    /// (`dep_packages`, excluding `root`) rather than walking `Cargo.lock`'s `dependencies` lists
    /// (which include `dev-dependencies` and inactive optional/target dependencies of workspace
    /// path packages).
    pub fn closure_of_packages<D: AsRef<str>, E, A: core::alloc::Allocator + Copy>(
        &self,
        root: blaze_manifest::lock::LockId,
        dep_packages: impl IntoIterator<Item = blaze_manifest::lock::LockId>,
        mut path_pin: impl FnMut(blaze_manifest::lock::LockId) -> Result<Option<D>, E>,
        alloc: A,
    ) -> Result<Result<blaze_own::String<A>, ClosureError>, E> {
        use core::fmt::Write as _;
        let lock = self.lock;
        let mut seen = blaze_own::Vec::with_capacity_in(lock.packages.len(), alloc);
        seen.resize(lock.packages.len(), false);
        for dep in dep_packages {
            if let Some(slot) = seen.get_mut(dep.index()) {
                *slot = true;
            }
        }
        let mut lines = blaze_own::Vec::<blaze_own::String<A>, A>::new_in(alloc);
        let mut version = blaze_own::String::new_in(alloc);
        let mut owned_path_pin = blaze_own::String::new_in(alloc);
        for (at, package) in lock.packages.iter().enumerate() {
            if !seen.get(at).copied().unwrap_or(false) || at == root.index() {
                continue;
            }
            // The version as the lock writes it: a parsed semver prints as its text (no
            // leading zero, nothing dropped), so the line is the text the old reader cut out.
            version.clear();
            let _ = write!(version, "{}", package.version);
            let pin: &str = match (&package.checksum, &package.source) {
                (Checksum::Sha256(sum), _) => sum.as_str(),
                (Checksum::Absent, LockSource::Path) => {
                    let Some(id) = u32::try_from(at).ok().map(blaze_manifest::lock::LockId) else { continue };
                    match path_pin(id)? {
                        Some(files_sha256) => {
                            owned_path_pin.clear();
                            let _ = write!(owned_path_pin, "files:{}", files_sha256.as_ref());
                            owned_path_pin.as_str()
                        }
                        None => {
                            return Ok(Err(ClosureError::Unpinned {
                                name: package.name.as_str().to_owned(),
                                version: version.as_str().to_owned(),
                            }));
                        }
                    }
                }
                (Checksum::Absent, LockSource::Registry { .. } | LockSource::Git { .. }) => package.source_text.as_str(),
            };
            let pin = self.corrections.pin(package.name.as_str(), version.as_str(), pin);
            let mut line = blaze_own::String::new_in(alloc);
            let _ = write!(line, "{} {} {pin}", package.name, version.as_str());
            lines.push(line);
        }
        lines.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        let mut joined = blaze_own::String::new_in(alloc);
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                joined.push('\n');
            }
            joined.push_str(line.as_str());
        }
        let digest = blaze_sha256::digest(joined.as_bytes());
        let mut out = blaze_own::String::with_capacity_in(digest.len() * 2, alloc);
        for byte in digest {
            let _ = write!(out, "{byte:02x}");
        }
        Ok(Ok(out))
    }
}

fn display_eq(value: &impl core::fmt::Display, expected: &str) -> bool {
    struct EqWriter<'a> {
        rest: &'a [u8],
        ok: bool,
    }
    impl core::fmt::Write for EqWriter<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let bytes = s.as_bytes();
            if self.ok && let Some(tail) = self.rest.strip_prefix(bytes) {
                self.rest = tail;
            } else {
                self.ok = false;
            }
            Ok(())
        }
    }
    use core::fmt::Write as _;
    let mut writer = EqWriter { rest: expected.as_bytes(), ok: true };
    write!(writer, "{value}").is_ok() && writer.ok && writer.rest.is_empty()
}

/// Streaming SHA-256 hasher for [`files_digest`]: feeds each `(path, bytes)` entry in sorted
/// byte order of `path` as `path || 0u8 || (bytes.len() as u64).to_le_bytes() || bytes` without
/// concatenating file contents into a buffer.
pub struct FilesHasher {
    hasher: blaze_sha256::Sha256,
}

impl Default for FilesHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl FilesHasher {
    pub const fn new() -> Self {
        Self { hasher: blaze_sha256::Sha256::new() }
    }

    /// Starts streaming one regular file entry with its raw `/`-separated relative path bytes and
    /// declared byte length. Callers must follow with [`Self::update_chunk`] calls totaling
    /// exactly `len` bytes.
    pub fn begin_file(&mut self, path_bytes: &[u8], len: u64) {
        self.hasher.update(path_bytes);
        self.hasher.update(&[0]);
        self.hasher.update(&len.to_le_bytes());
    }

    /// Streams a chunk of bytes for the file started with [`Self::begin_file`].
    pub fn update_chunk(&mut self, chunk: &[u8]) {
        self.hasher.update(chunk);
    }

    /// Feeds one regular file entry by its raw `/`-separated relative path bytes. Callers must
    /// pass entries in ascending byte order of `path_bytes` to match [`package_files_digest_in`]
    /// and [`files_digest`].
    pub fn update_file_bytes(&mut self, path_bytes: &[u8], bytes: &[u8]) {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.begin_file(path_bytes, len);
        self.update_chunk(bytes);
    }

    /// Feeds one regular file entry. Callers must pass entries in ascending byte order of `path`
    /// to match [`files_digest`].
    pub fn update_file(&mut self, path: &str, bytes: &[u8]) {
        self.update_file_bytes(path.as_bytes(), bytes);
    }

    pub fn finish(self) -> [u8; 32] {
        self.hasher.finish()
    }

    pub fn finish_hex_in<A: core::alloc::Allocator>(self, alloc: A) -> blaze_own::String<A> {
        use core::fmt::Write as _;
        let digest = self.hasher.finish();
        let mut out = blaze_own::String::with_capacity_in(digest.len() * 2, alloc);
        for byte in digest {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

/// The digest a path package's sources are pinned by: SHA-256 over its files, in the byte
/// order of their paths relative to the package's folder, each as the path, a zero byte, its
/// length as 8 little-endian bytes and its bytes. `files` holds every regular file of the
/// package by the one package rule (blaze-build-script `served::in_package`); the builder and
/// blaze walk it alike (`examples/tempo_derives.rs` computes it from the checkout and finds the
/// builder's module by it).
pub fn files_digest(files: &mut [(String, Vec<u8>)]) -> String {
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut hasher = FilesHasher::new();
    for (path, bytes) in files.iter() {
        hasher.update_file(path.as_str(), bytes.as_slice());
    }
    hex(&hasher.finish())
}

/// Walks `dir` by the one package rule (`served::in_package`), collecting `(raw_rel_bytes, full_path)`
/// for every regular file and sorting strictly by ascending `raw_rel_bytes` (`/`-separated raw
/// encoded OS filename bytes, never lossy UTF-8) so ordering is unique and filesystem-independent.
fn walk_package_entries<'f>(
    dir: &Path,
    frame: &'f bstd::mem::Frame,
) -> Result<blaze_own::Vec<(blaze_own::Vec<u8, &'f bstd::mem::Frame>, PathBuf), &'f bstd::mem::Frame>, ModuleError> {
    let io = |path: &Path, error: IoError| ModuleError::Io { path: path.to_owned(), error };
    let mut files: blaze_own::Vec<(blaze_own::Vec<u8, &'f bstd::mem::Frame>, PathBuf), &'f bstd::mem::Frame> =
        blaze_own::Vec::new_in(frame);
    let mut folders: blaze_own::Vec<(blaze_own::Vec<u8, &'f bstd::mem::Frame>, PathBuf), &'f bstd::mem::Frame> =
        blaze_own::Vec::new_in(frame);
    folders.push((blaze_own::Vec::new_in(frame), dir.to_path_buf()));
    while let Some((relative, folder)) = folders.pop() {
        for entry in read_dir(&folder).map_err(|e| io(&folder, e))? {
            let entry = entry.map_err(|e| io(&folder, e))?;
            let file_name = entry.file_name();
            let raw_name = file_name.as_encoded_bytes();
            let path = entry.path();
            let kind = entry.file_type().map_err(|e| io(&path, e))?;
            let place = if relative.is_empty() { InPackage::Root } else { InPackage::Beneath };
            if !in_package(raw_name, kind.is_dir(), place) {
                continue;
            }
            let mut at = blaze_own::Vec::with_capacity_in(relative.len() + usize::from(!relative.is_empty()) + raw_name.len(), frame);
            if !relative.is_empty() {
                at.extend_from_slice(relative.as_slice());
                at.push(b'/');
            }
            at.extend_from_slice(raw_name);
            if kind.is_dir() {
                folders.push((at, path));
            } else if kind.is_file() {
                files.push((at, path));
            }
        }
    }
    files.sort_unstable_by(|a, b| a.0.as_slice().cmp(b.0.as_slice()));
    Ok(files)
}

/// Streams one regular file from `reader` directly into `hasher` in fixed `chunk`-sized pieces
/// without retaining the file's full contents in memory, verifying that the observed byte count
/// matches `expected_len` (Rule 3: never hash a concurrent grow/shrink under a stale length).
fn stream_file_into_hasher(
    hasher: &mut FilesHasher,
    rel_bytes: &[u8],
    reader: &mut impl Read,
    expected_len: u64,
    chunk: &mut [u8],
) -> Result<(), IoError> {
    hasher.begin_file(rel_bytes, expected_len);
    let mut total: u64 = 0;
    loop {
        match reader.read(chunk) {
            Ok(0) => break,
            Ok(n) => {
                let Some(slice) = chunk.get(..n) else { break };
                total = total.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
                if total > expected_len {
                    return Err(IoError::new(
                        ErrorKind::InvalidData,
                        "file grew while streaming package files digest",
                    ));
                }
                hasher.update_chunk(slice);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    if total != expected_len {
        return Err(IoError::new(
            ErrorKind::UnexpectedEof,
            "file shrank while streaming package files digest",
        ));
    }
    Ok(())
}

/// Computes the canonical package-wide files digest directly from `dir`, streaming one file at a
/// time in bounded 64 KiB chunks into [`FilesHasher`] in strict ascending byte order of raw
/// `/`-separated relative paths (`as_encoded_bytes()`, bit-for-bit identical to [`files_digest`]
/// for UTF-8 paths and uniquely ordered for distinct non-UTF-8 Unix filenames). Allocates the
/// returned 64-byte hex digest in `alloc` with zero per-file heap or frame buffering.
pub fn package_files_digest_in<A: core::alloc::Allocator>(dir: &Path, alloc: A) -> Result<blaze_own::String<A>, ModuleError> {
    let io = |path: &Path, error: IoError| ModuleError::Io { path: path.to_owned(), error };
    let frame = bstd::mem::Frame::new(bstd::mem::Tag::Other)
        .map_err(|e| io(dir, IoError::other(format!("allocating package_files_digest_in frame: {e:?}"))))?;
    let files = walk_package_entries(dir, &frame)?;
    let mut hasher = FilesHasher::new();
    let mut chunk = [0u8; 64 * 1024];
    for (rel_bytes, full_path) in files.iter() {
        let mut file = File::open(full_path).map_err(|e| io(full_path, e))?;
        let expected_len = file.metadata().map_err(|e| io(full_path, e))?.len();
        stream_file_into_hasher(&mut hasher, rel_bytes.as_slice(), &mut file, expected_len, &mut chunk)
            .map_err(|e| io(full_path, e))?;
    }
    Ok(hasher.finish_hex_in(alloc))
}

/// The files of the package at `dir` that [`files_digest`] covers, by their UTF-8 paths relative
/// to it (`/`-separated): each entry `served::in_package` takes. Fails cleanly on non-UTF-8
/// filenames instead of corrupting them lossily; callers computing a package's digest should use
/// [`package_files_digest_in`] directly.
pub fn package_files(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, ModuleError> {
    let io = |path: &Path, error: IoError| ModuleError::Io { path: path.to_owned(), error };
    let frame = bstd::mem::Frame::new(bstd::mem::Tag::Other)
        .map_err(|e| io(dir, IoError::other(format!("allocating package_files frame: {e:?}"))))?;
    let files = walk_package_entries(dir, &frame)?;
    let mut out = Vec::with_capacity(files.len());
    for (rel_bytes, path) in files.iter() {
        let at = core::str::from_utf8(rel_bytes.as_slice())
            .map(str::to_owned)
            .map_err(|_| io(path, IoError::new(ErrorKind::InvalidData, "non-UTF-8 path in package_files; use package_files_digest_in")))?;
        out.push((at, read(path).map_err(|e| io(path, e))?));
    }
    Ok(out)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The SHA-256 of `bytes`, as hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&blaze_sha256::digest(bytes))
}

/// Current proc-macro module identity header (`blaze proc-macro module wasm64 11`): target-bearing
/// so on-demand `wasm64-blaze-wasip1` self-compiled modules never share a write-once key with
/// `wasm32-wasip1` bootstrap records (`blaze proc-macro module wasm32 7`) or pre-v11 records.
/// Form 11 registers `blaze_wasm64_pm_shim` in `export_override_crates` so `libc::getenv` binds
/// to `blaze_wasm64_pm_shim::getenv` -> `blaze_proc_macro_64::env_var` (Rule 21), invalidating
/// stale form-10 wasm64 proc-macro modules whose `std::env::var` calls missed host env vars.
pub const MODULE_HEADER: &str = "blaze proc-macro module wasm64 11";

/// Result of comparing a candidate `wasm_graph` unit's feature set against the requested
/// `host_graph` feature set (`want_sorted_dedup`, sorted and deduplicated).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FeatureMatch {
    /// Candidate is missing at least one requested feature.
    Missing,
    /// Candidate has the exact same feature set (`got == want`).
    Exact,
    /// Candidate contains every requested feature plus WASI-added features (`got ⊃ want`).
    Superset,
}

/// Classifies `unit.features` against `want_sorted_dedup` without global heap allocation.
pub fn classify_unit_features<A: core::alloc::Allocator + Copy>(
    unit: &blaze_unit_graph::Unit<'_>,
    want_sorted_dedup: &[&str],
    alloc: A,
) -> FeatureMatch {
    if unit.features.len() < want_sorted_dedup.len() {
        return FeatureMatch::Missing;
    }
    let mut got = blaze_own::Vec::with_capacity_in(unit.features.len(), alloc);
    for f in unit.features.iter() {
        got.push(f.as_str());
    }
    got.sort_unstable();
    got.dedup();
    if got.len() < want_sorted_dedup.len() {
        return FeatureMatch::Missing;
    }
    let mut gi = 0usize;
    for &want in want_sorted_dedup {
        while let Some(&g) = got.get(gi) {
            if g < want {
                gi += 1;
            } else {
                break;
            }
        }
        if got.get(gi).copied() != Some(want) {
            return FeatureMatch::Missing;
        }
        gi += 1;
    }
    if got.len() == want_sorted_dedup.len() {
        FeatureMatch::Exact
    } else {
        FeatureMatch::Superset
    }
}

/// Folds the resolved `wasm64-blaze-wasip1` dependency closure's unified feature sets (bound to
/// each package's source pin — registry checksum/source, git commit+URL, or path files digest)
/// into a proc-macro crate's `deps` digest so enabling, removing, or swapping features across
/// dependencies (including same-name/same-version packages from different sources and dependency
/// build-script closures) invalidates the module key (rule 21).
///
/// Prefers a unique exact `(package, UnitKind::ProcMacro, features)` match in `wasm_graph`, else
/// accepts a unique feature superset when WASI target-gated dependencies unify extra features onto
/// the proc-macro unit (folding those extra root features into the digest, rule 21). Returns
/// [`ClosureError::WasmRootMissing`] when missing or ambiguous, and refuses any reachable
/// `LockSource::Path` dependency whose digest cannot be supplied ([`ClosureError::Unpinned`],
/// rule 3).
pub fn deps_with_wasm_features<S: AsRef<str>, D: AsRef<str>, A: core::alloc::Allocator + Copy>(
    lock_deps: &str,
    lock: &Lock<'_>,
    wasm_graph: &blaze_unit_graph::UnitGraph<'_>,
    package: blaze_manifest::lock::LockId,
    features: &[S],
    mut path_files: impl FnMut(blaze_manifest::lock::LockId) -> Option<D>,
    alloc: A,
) -> Result<blaze_own::String<A>, ClosureError> {
    use core::fmt::Write as _;
    let mut want_feats = blaze_own::Vec::with_capacity_in(features.len(), alloc);
    for f in features {
        want_feats.push(f.as_ref());
    }
    want_feats.sort_unstable();
    want_feats.dedup();
    let mut exact_idx: Option<usize> = None;
    let mut exact_count: usize = 0;
    let mut superset_idx: Option<usize> = None;
    let mut superset_count: usize = 0;
    for (i, u) in wasm_graph.units.iter().enumerate() {
        if u.package != package || u.kind != blaze_unit_graph::UnitKind::ProcMacro {
            continue;
        }
        match classify_unit_features(u, want_feats.as_slice(), alloc) {
            FeatureMatch::Missing => {}
            FeatureMatch::Exact => {
                exact_idx.get_or_insert(i);
                exact_count += 1;
            }
            FeatureMatch::Superset => {
                superset_idx.get_or_insert(i);
                superset_count += 1;
            }
        }
    }
    #[derive(Clone, Copy)]
    enum MatchedWasmRoot {
        Exact(usize),
        Superset(usize),
    }
    let matched = if exact_count == 1 {
        exact_idx.map(MatchedWasmRoot::Exact)
    } else if exact_count == 0 && superset_count == 1 {
        superset_idx.map(MatchedWasmRoot::Superset)
    } else {
        None
    };
    let Some(matched_root) = matched else {
        let root_pkg = lock.package(package);
        let mut version = blaze_own::String::new_in(alloc);
        let _ = write!(version, "{}", root_pkg.version);
        return Err(ClosureError::WasmRootMissing {
            name: root_pkg.name.as_str().to_owned(),
            version: version.as_str().to_owned(),
        });
    };
    let root_idx = match matched_root {
        MatchedWasmRoot::Exact(i) | MatchedWasmRoot::Superset(i) => i,
    };
    let mut visited = blaze_own::Vec::with_capacity_in(wasm_graph.units.len(), alloc);
    visited.resize(wasm_graph.units.len(), false);
    let mut stack = blaze_own::Vec::new_in(alloc);
    let push_deps = |u: &blaze_unit_graph::Unit<'_>,
                     visited: &mut blaze_own::Vec<bool, A>,
                     stack: &mut blaze_own::Vec<usize, A>| {
        for dep in u.deps.iter() {
            let didx = dep.unit.index();
            if wasm_graph
                .units
                .get(didx)
                .is_some_and(|du| matches!(du.kind, blaze_unit_graph::UnitKind::Lib | blaze_unit_graph::UnitKind::ProcMacro))
                && let Some(slot) = visited.get_mut(didx)
                && !*slot
            {
                *slot = true;
                stack.push(didx);
            }
        }
    };
    let push_build_script = |u: &blaze_unit_graph::Unit<'_>,
                             visited: &mut blaze_own::Vec<bool, A>,
                             stack: &mut blaze_own::Vec<usize, A>| {
        if let Some(bs_id) = u.build_script
            && let Some(slot) = visited.get_mut(bs_id.index())
            && !*slot
        {
            *slot = true;
            if let Some(bs_unit) = wasm_graph.units.get(bs_id.index()) {
                push_deps(bs_unit, visited, stack);
            }
        }
    };
    if let Some(slot) = visited.get_mut(root_idx) {
        *slot = true;
    }
    if let Some(root_u) = wasm_graph.units.get(root_idx) {
        push_deps(root_u, &mut visited, &mut stack);
        push_build_script(root_u, &mut visited, &mut stack);
    }
    let mut path_cache: blaze_own::Vec<Option<blaze_own::String<A>>, A> =
        blaze_own::Vec::with_capacity_in(lock.packages.len(), alloc);
    for _ in 0..lock.packages.len() {
        path_cache.push(None);
    }
    let mut rows = blaze_own::Vec::<blaze_own::String<A>, A>::new_in(alloc);
    if matches!(matched_root, MatchedWasmRoot::Superset(_))
        && let Some(root_u) = wasm_graph.units.get(root_idx)
    {
        let mut rfeats = blaze_own::Vec::with_capacity_in(root_u.features.len(), alloc);
        for f in root_u.features.iter() {
            rfeats.push(f.as_str());
        }
        rfeats.sort_unstable();
        rfeats.dedup();
        let mut row = blaze_own::String::new_in(alloc);
        row.push_str("root-wasm-features ");
        for (i, f) in rfeats.iter().enumerate() {
            if i > 0 {
                row.push(',');
            }
            row.push_str(f);
        }
        rows.push(row);
    }
    while let Some(idx) = stack.pop() {
        let Some(u) = wasm_graph.units.get(idx) else { continue };
        push_deps(u, &mut visited, &mut stack);
        push_build_script(u, &mut visited, &mut stack);
        let mut ufeats = blaze_own::Vec::with_capacity_in(u.features.len(), alloc);
        for f in u.features.iter() {
            ufeats.push(f.as_str());
        }
        ufeats.sort_unstable();
        ufeats.dedup();
        let kind_word = match u.kind {
            blaze_unit_graph::UnitKind::ProcMacro => "proc-macro",
            _ => "lib",
        };
        let pkg = lock.package(u.package);
        let mut row = blaze_own::String::new_in(alloc);
        let _ = write!(row, "{} {} {} ", u.name.as_str(), u.version.as_str(), kind_word);
        match (&pkg.checksum, &pkg.source) {
            (Checksum::Sha256(sum), _) => {
                let _ = write!(row, "checksum:{} {}", sum.as_str(), pkg.source_text.as_str());
            }
            (Checksum::Absent, LockSource::Path) => {
                let cached = match path_cache.get_mut(u.package.index()) {
                    Some(Some(hex)) => Some(hex.as_str()),
                    Some(slot @ None) => match path_files(u.package) {
                        Some(hex) => Some(slot.insert(blaze_own::String::from_str_in(hex.as_ref(), alloc)).as_str()),
                        None => None,
                    },
                    None => None,
                };
                match cached {
                    Some(hex) => {
                        let _ = write!(row, "files:{hex}");
                    }
                    None => {
                        let mut version = blaze_own::String::new_in(alloc);
                        let _ = write!(version, "{}", pkg.version);
                        return Err(ClosureError::Unpinned {
                            name: pkg.name.as_str().to_owned(),
                            version: version.as_str().to_owned(),
                        });
                    }
                }
            }
            (Checksum::Absent, LockSource::Git { commit, .. }) => {
                let _ = write!(row, "git:{} {}", commit.as_str(), pkg.source_text.as_str());
            }
            (Checksum::Absent, LockSource::Registry { .. }) => {
                let _ = write!(row, "source:{}", pkg.source_text.as_str());
            }
        }
        row.push(' ');
        for (i, f) in ufeats.iter().enumerate() {
            if i > 0 {
                row.push(',');
            }
            row.push_str(f);
        }
        rows.push(row);
    }
    rows.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    rows.dedup_by(|a, b| a.as_str() == b.as_str());
    let mut joined = blaze_own::String::new_in(alloc);
    joined.push_str(lock_deps);
    joined.push_str("\nwasm64-dep-features\n");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            joined.push('\n');
        }
        joined.push_str(row.as_str());
    }
    let digest = blaze_sha256::digest(joined.as_str().as_bytes());
    let mut out = blaze_own::String::with_capacity_in(64, alloc);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// The crate's identity as text: what the module key is the digest of.
pub fn identity(id: &CrateId<'_>) -> String {
    let mut features: Vec<&str> = id.features.to_vec();
    features.sort_unstable();
    features.dedup();
    let source = match id.source {
        Source::Registry { checksum } => format!("checksum {checksum}"),
        Source::Git { commit, path } => format!("git {commit} {path}"),
        Source::Path { files_sha256 } => format!("files {files_sha256}"),
    };
    format!(
        // Form 3 (2026-11-18): the profile is an input of the module. A macro's own code reads
        // `cfg(debug_assertions)` (tempo's `tempo-precompiles-macros` writes its `*_BYTES`
        // constants only under it), and `check` models cargo's dev profile, where it is on.
        // Form 2's modules were built without it and are another key.
        // Form 4 (2026-11-20): the standard library's crates (core, alloc, std, proc_macro and
        // the rest of std's own workspace) are built WITHOUT debug assertions, and no crate of
        // the module emits UB checks (`-Zub-checks=no`); the macro crate and its dependencies
        // keep `cfg(debug_assertions)`. Form 3's std carried both into every call. The builder
        // (scripts proc_macro_modules.rs) writes the same text.
        // Form 5 (2026-11-27): wasm64 self-compilation no longer pulls ambient `bstd` (and its
        // transitive closure) into proc-macro sysroot records unless the unit closure references it.
        // Form 6 (2026-11-27): proc-macro `_DECLS` and `macros` exports are synthesized after
        // macro expansion (#2746), invalidating pre-#2746 form-5 records whose `macros` files
        // missed macro-generated `#[proc_macro*]` exports (e.g. `zerocopy-derive`, `derive_more-impl`).
        // Form 7 (2026-11-27): `blaze-build` `bind_let_place` uses `adjusted_ty(init)` so
        // `let S { ref a, ref b } = self;` over a reference place dereferences `self` instead of
        // projecting off `self`'s stack slot, invalidating stale form-6 `clap_derive` modules.
        // Form 8 (2026-11-27): wasm64 self-compilation resolves `[target.'cfg(...)'.dependencies]`
        // against `wasm64-blaze-wasip1` instead of the host triple, invalidating pre-fix form-7 records.
        // Form 9 (2026-11-27): `blaze-build` `Task::ScopeEnd` / `Task::TempScopeEnd` use adjusted
        // repr for already-adjusted arm/branch bodies (`Box<T> -> Box<dyn Trait>`, `&T -> &dyn Trait`,
        // `&[T; N] -> &[T]`), invalidating stale form-8 wasm64 modules (e.g. `snafu-derive`).
        // Form 10 (2026-11-27): `wasm64-blaze-wasip1` target cfg includes `getrandom_backend="custom"`
        // matching `library/wasi_rt/src/rt/random.rs` and `.cargo/config.toml` (Rule 21).
        // Form 11 (2026-11-28): `wasm64_compile` registers `blaze_wasm64_pm_shim` as an
        // `export_override_crates` shim so `export_shim` binds `libc::getenv` to
        // `blaze_wasm64_pm_shim::getenv` (`blaze_proc_macro_64::env_var`) instead of
        // `wasi_rt::rt::env::getenv`, invalidating stale form-10 wasm64 proc-macro modules (Rule 21).
        "{MODULE_HEADER}\nname {}\nversion {}\n{source}\ndeps {}\nfeatures {}\ntoolchain {}\ntarget {}\nshim {}\nprofile debug-assertions std-release ub-checks-off\nbuilder bootstrap\n",
        id.name,
        id.version,
        id.deps,
        features.join(","),
        // The pinned nightly: the bridge's sources and the std a module is built with.
        blaze_pins::TOOLCHAIN,
        sha256_hex(TARGET_JSON),
        sha256_hex(SHIM),
    )
}

/// The module record's key.
pub fn module_key(id: &CrateId<'_>) -> String {
    sha256_hex(identity(id).as_bytes())
}

/// Identity header for Tier-2 standalone native proc-macro executables (`native.bin`), strictly
/// disjoint from wasm64 [`MODULE_HEADER`] keys and keyed by [`NativeTarget`].
pub const NATIVE_MODULE_HEADER: &str = "blaze proc-macro module native-bin 1";

/// Host target identity for a Tier-2 `native.bin` proc-macro module (rule 21): ensures native
/// binaries compiled for different OSes, architectures, or ABIs never share a store directory
/// with each other or with wasm64 modules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NativeTarget<'a> {
    /// Canonical host target triple (e.g. `"aarch64-unknown-linux-gnu"`, `"x86_64-unknown-linux-gnu"`,
    /// `"aarch64-apple-darwin"`).
    pub triple: &'a str,
    /// Host ABI / runtime / bridge protocol version identity.
    pub abi_id: &'a str,
}

impl NativeTarget<'static> {
    /// Returns the running host process's default Tier-2 native proc-macro target identity.
    pub const fn host() -> NativeTarget<'static> {
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        const TRIPLE: &str = "aarch64-unknown-linux-gnu";
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        const TRIPLE: &str = "x86_64-unknown-linux-gnu";
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        const TRIPLE: &str = "aarch64-apple-darwin";
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        const TRIPLE: &str = "x86_64-apple-darwin";
        #[cfg(not(any(
            all(target_os = "linux", target_arch = "aarch64"),
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "macos", target_arch = "x86_64"),
        )))]
        const TRIPLE: &str = "unknown-native-host";

        NativeTarget { triple: TRIPLE, abi_id: "bzpm-v1" }
    }
}

/// The Tier-2 native module's identity text in `alloc`, keyed by both crate inputs and `target`.
pub fn native_identity<'a>(
    id: &CrateId<'_>,
    target: NativeTarget<'_>,
    alloc: blaze_own::Alloc<'a>,
) -> blaze_own::AString<'a> {
    let mut features = blaze_own::AVec::new_in(alloc);
    for &feat in id.features {
        features.push(feat);
    }
    features.sort_unstable();
    features.dedup();
    let mut out = blaze_own::AString::new_in(alloc);
    let _ = core::fmt::Write::write_fmt(
        &mut out,
        format_args!("{NATIVE_MODULE_HEADER}\nname {}\nversion {}\n", id.name, id.version),
    );
    match id.source {
        Source::Registry { checksum } => {
            let _ = core::fmt::Write::write_fmt(&mut out, format_args!("checksum {checksum}\n"));
        }
        Source::Git { commit, path } => {
            let _ = core::fmt::Write::write_fmt(&mut out, format_args!("git {commit} {path}\n"));
        }
        Source::Path { files_sha256 } => {
            let _ = core::fmt::Write::write_fmt(&mut out, format_args!("files {files_sha256}\n"));
        }
    }
    let _ = core::fmt::Write::write_fmt(&mut out, format_args!("deps {}\nfeatures ", id.deps));
    for (idx, feat) in features.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(feat);
    }
    let _ = core::fmt::Write::write_fmt(
        &mut out,
        format_args!(
            "\ntoolchain {}\ntarget {}\nabi {}\nprofile debug-assertions std-release ub-checks-off\nbuilder native-bin\n",
            blaze_pins::TOOLCHAIN,
            target.triple,
            target.abi_id,
        ),
    );
    out
}

/// Computes the host-target-keyed store key for a Tier-2 `native.bin` module in `alloc`.
pub fn native_module_key<'a>(
    id: &CrateId<'_>,
    target: NativeTarget<'_>,
    alloc: blaze_own::Alloc<'a>,
) -> blaze_own::AString<'a> {
    let text = native_identity(id, target, alloc);
    let digest = blaze_sha256::digest(text.as_str().as_bytes());
    let mut out = blaze_own::AString::new_in(alloc);
    for byte in digest {
        let _ = core::fmt::Write::write_fmt(&mut out, format_args!("{byte:02x}"));
    }
    out
}

/// One macro a module exports.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Exported {
    /// Its index in the harness's list, which the shim runs.
    pub index: u32,
    pub kind: ProcKind,
    pub name: String,
    /// A derive's helper attributes.
    pub helpers: Vec<String>,
}

/// A module record: its key, its macros and the digest its `wasm-sha256` states. The module's
/// wasm is NOT held: it is read only by a compile ([`ModuleRecord::wasm`]), on the one load
/// whose compiled record is missing (host.rs `load_now`), checked against the digest there,
/// and dropped with the compile. A load from a compiled record (every command after the
/// first) reads no wasm at all: the record's seal names the digest it was compiled from.
/// (Before, every command read and hashed every macro crate's wasm and kept it to the end:
/// 114 MB in 79 blocks on tempo's 2026-11-18 heap log.)
#[derive(Debug)]
pub struct ModuleRecord {
    pub key: String,
    pub name: String,
    pub version: String,
    /// The digest of `module.wasm` as `wasm-sha256` states it; [`ModuleRecord::wasm`]
    /// checks the bytes against it when it reads them.
    pub wasm_sha256: String,
    pub macros: Vec<Exported>,
    /// The name of the global holding the address of the harness's list.
    pub decls: String,
    /// The record's folder in the module store, `<store>/proc-macro/modules/<key>`.
    pub dir: PathBuf,
    /// The store's folder of compiled code, `<store>/`[`COMPILED_DIR`]: where the runtime keeps
    /// the module's machine code, keyed by what it is made from (host.rs `Runtime::code_key`),
    /// not by this record's key.
    pub compiled: PathBuf,
    /// When `Some`, this proc-macro module is a native host `.dylib`/`.so` executed directly
    /// through `NativeServerPool` (zero Wasmtime, zero `-Zbuild-std`, D175 fast path).
    pub native_dylib: Option<PathBuf>,
    /// When `Some`, this proc-macro module is a Blaze-compiled standalone native executable
    /// (`native.bin`, Tier-2) run confined under Landlock/seccomp (Linux) or Seatbelt (macOS)
    /// over `CHILD_FD` via `NativeBinPool` (zero `rustc`, zero `rust-analyzer-proc-macro-srv`,
    /// zero `dlopen`).
    pub native_bin: Option<PathBuf>,
}

/// Filename of a Tier-2 Blaze-compiled standalone native proc-macro executable in a module record.
pub const NATIVE_BIN_FILENAME: &str = "native.bin";

/// Filename holding the SHA-256 hex digest of [`NATIVE_BIN_FILENAME`] in a module record.
pub const BIN_SHA256_FILENAME: &str = "bin-sha256";

/// A store's folder of compiled module code, under the store (host.rs, "Compiled records").
pub const COMPILED_DIR: &str = "proc-macro/compiled";

impl ModuleRecord {
    /// The macro `name` of `kind`.
    pub fn find(&self, kind: ProcKind, name: &str) -> Option<&Exported> {
        self.macros.iter().find(|it| it.kind == kind && it.name == name)
    }

    /// Verifies `native.bin` against [`Self::wasm_sha256`] and returns its path.
    pub fn verify_native_bin(&self) -> Result<&Path, ModuleError> {
        let Some(ref path) = self.native_bin else {
            return Err(ModuleError::Missing {
                name: self.name.as_str().into(),
                version: self.version.as_str().into(),
                key: self.key.as_str().into(),
            });
        };
        let bytes = read(path).map_err(|error| ModuleError::Io { path: path.to_path_buf(), error })?;
        let found = sha256_hex(&bytes);
        if !is_valid_sha256_hex(self.wasm_sha256.as_str()) || found != self.wasm_sha256 {
            return Err(ModuleError::Digest {
                key: self.key.as_str().into(),
                held: self.wasm_sha256.as_str().into(),
                found,
            });
        }
        Ok(path.as_path())
    }

    /// The module's wasm, `module.wasm` in its folder, checked against [`Self::wasm_sha256`]:
    /// what a compile reads, once, and drops when it is done. Not kept here (the type's docs).
    pub fn wasm(&self) -> Result<Vec<u8>, ModuleError> {
        let path = self.dir.join("module.wasm");
        let wasm = std::fs::read(&path).map_err(|error| ModuleError::Io { path, error })?;
        let found = sha256_hex(&wasm);
        if found != self.wasm_sha256 {
            return Err(ModuleError::Digest { key: self.key.clone(), held: self.wasm_sha256.clone(), found });
        }
        Ok(wasm)
    }
}

/// Why a module could not be had.
#[derive(Debug)]
pub enum ModuleError {
    /// No record under the crate's key: built by nobody yet.
    Missing { name: String, version: String, key: String },
    /// A file of the record could not be read.
    Io { path: PathBuf, error: std::io::Error },
    /// The record's wasm is not the bytes its digest names.
    Digest { key: String, held: String, found: String },
    /// A line of `macros` that is not one.
    Macros { key: String, line: String },
}

impl core::fmt::Display for ModuleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ModuleError::Missing { name, version, key } => write!(
                f,
                "{name} {version}: no compiled module in the store or the cache (module key {key}); build it with scripts/src/bin/proc_macro_modules.rs (D52)"
            ),
            ModuleError::Io { path, error } => write!(f, "{}: {error}", path.display()),
            ModuleError::Digest { key, held, found } => {
                write!(f, "module {key}: its wasm-sha256 says {held}, its module.wasm is {found}")
            }
            ModuleError::Macros { key, line } => write!(f, "module {key}: `macros` has the line {line:?}, which lists no macro"),
        }
    }
}

impl core::error::Error for ModuleError {}

fn native_dylib_filename() -> &'static str {
    if cfg!(target_os = "macos") {
        "native.dylib"
    } else if cfg!(target_os = "windows") {
        "native.dll"
    } else {
        "native.so"
    }
}

fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

/// Verifies `bin-sha256` (strict 64-char lowercase hex) and hashes `native.bin` against it before
/// declaring any Tier-2 native module present or readable (rule 74 / trust).
fn verify_native_bin_in_dir<'a>(
    dir: &Path,
    key: &str,
    alloc: blaze_own::Alloc<'a>,
) -> Result<blaze_own::AString<'a>, ModuleError> {
    let digest_path = dir.join(BIN_SHA256_FILENAME);
    let raw = read(&digest_path).map_err(|error| ModuleError::Io { path: digest_path, error })?;
    let held_str = core::str::from_utf8(&raw).map(str::trim).unwrap_or("");
    let bin_path = dir.join(NATIVE_BIN_FILENAME);
    let bin_bytes = read(&bin_path).map_err(|error| ModuleError::Io { path: bin_path, error })?;
    let found = sha256_hex(&bin_bytes);
    if !is_valid_sha256_hex(held_str) || found != held_str {
        return Err(ModuleError::Digest {
            key: key.into(),
            held: held_str.into(),
            found,
        });
    }
    Ok(blaze_own::AString::from_str_in(held_str, alloc))
}

/// Looks up a Tier-2 `native.bin` module for `id` under its host-target-keyed directory
/// ([`native_module_key`]), verifying `bin-sha256` and `native.bin`'s SHA-256 digest before
/// returning `Ok((key, digest))`. Rejects truncated/corrupt binaries and malformed digests with
/// [`ModuleError::Digest`] (never treats a corrupted record as present).
pub fn find_native_module<'a>(
    store: &Path,
    id: &CrateId<'_>,
    target: NativeTarget<'_>,
    alloc: blaze_own::Alloc<'a>,
) -> Result<(blaze_own::AString<'a>, blaze_own::AString<'a>), ModuleError> {
    let key = native_module_key(id, target, alloc);
    let dir = store.join("proc-macro/modules").join(key.as_str());
    let has_digest = dir.join(BIN_SHA256_FILENAME).is_file();
    let has_bin = dir.join(NATIVE_BIN_FILENAME).is_file();
    if !has_digest && !has_bin {
        return Err(ModuleError::Missing {
            name: id.name.into(),
            version: id.version.into(),
            key: key.as_str().into(),
        });
    }
    if !has_digest || !has_bin {
        return Err(ModuleError::Digest {
            key: key.as_str().into(),
            held: if has_digest { "present" } else { "missing" }.into(),
            found: if has_bin { "present" } else { "missing" }.into(),
        });
    }
    let verified = verify_native_bin_in_dir(&dir, key.as_str(), alloc)?;
    Ok((key, verified))
}

/// Reads a Tier-2 `native.bin` module for `id` under its host-target-keyed directory
/// ([`native_module_key`]), verifying `native.bin`'s SHA-256 against `bin-sha256`.
pub fn read_native_module(
    store: &Path,
    id: &CrateId<'_>,
    target: NativeTarget<'_>,
) -> Result<ModuleRecord, ModuleError> {
    let key = native_module_key(id, target, blaze_own::Alloc::global());
    read_module_by_key(store, key.as_str().into(), id.name, id.version)
}

/// Whether `store` holds `id`'s module, by its key: checks Tier-2 `native.bin` under its
/// host-target-keyed directory ([`NativeTarget::host`], verifying `native.bin`'s SHA-256 before
/// declaring it present), then native `.dylib`/`.so`, then `module.wasm` under `module_key(id)`.
pub fn find_module(store: &Path, id: &CrateId<'_>) -> Result<(String, String), ModuleError> {
    match find_native_module(store, id, NativeTarget::host(), blaze_own::Alloc::global()) {
        Ok((key, digest)) => return Ok((key.as_str().into(), digest.as_str().into())),
        Err(ModuleError::Missing { .. }) => {}
        Err(err) => return Err(err),
    }
    let key = module_key(id);
    let dir = store.join("proc-macro/modules").join(&key);
    let dylib_digest = dir.join("dylib-sha256");
    if let Ok(held) = std::fs::read(&dylib_digest) {
        if dir.join(native_dylib_filename()).is_file() {
            return Ok((key, String::from_utf8_lossy(&held).trim().to_owned()));
        }
    }
    let path = dir.join("wasm-sha256");
    match std::fs::read(&path) {
        Ok(held) => Ok((key, String::from_utf8_lossy(&held).trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(ModuleError::Missing { name: id.name.to_owned(), version: id.version.to_owned(), key }),
        Err(error) => Err(ModuleError::Io { path, error }),
    }
}

/// The record of `id` in `store` (prefers verified Tier-2 `native.bin` under its host-target key,
/// then native `.dylib`/`.so`, then `module.wasm` under `module_key(id)`).
pub fn read_module(store: &Path, id: &CrateId<'_>) -> Result<ModuleRecord, ModuleError> {
    match read_native_module(store, id, NativeTarget::host()) {
        Ok(rec) => return Ok(rec),
        Err(ModuleError::Missing { .. }) => {}
        Err(err) => return Err(err),
    }
    read_module_by_key(store, module_key(id), id.name, id.version)
}

/// The record for `key` (`name` `version`) in `store` (reads `native.bin` when `bin-sha256` is
/// present, else native `.dylib`/`.so`, else `module.wasm`). Verifies `native.bin`'s SHA-256
/// digest against strict 64-char lowercase hex `bin-sha256` before returning the record.
pub fn read_module_by_key(store: &Path, key: String, name: &str, version: &str) -> Result<ModuleRecord, ModuleError> {
    let dir = store.join("proc-macro/modules").join(&key);
    let bin_path = dir.join(NATIVE_BIN_FILENAME);
    let has_bin_digest = dir.join(BIN_SHA256_FILENAME).is_file();
    let has_bin_file = bin_path.is_file();
    if has_bin_digest != has_bin_file {
        return Err(ModuleError::Digest {
            key,
            held: if has_bin_digest { "present" } else { "missing" }.into(),
            found: if has_bin_file { "present" } else { "missing" }.into(),
        });
    }
    let is_native_bin = has_bin_digest && has_bin_file;
    let native_path = dir.join(native_dylib_filename());
    let is_native_dylib = !is_native_bin && dir.join("dylib-sha256").is_file() && native_path.is_file();
    if !is_native_bin && !is_native_dylib && !dir.join("wasm-sha256").exists() {
        return Err(ModuleError::Missing { name: name.to_owned(), version: version.to_owned(), key });
    }
    let read = |file: &str| -> Result<Vec<u8>, ModuleError> {
        let path = dir.join(file);
        std::fs::read(&path).map_err(|error| ModuleError::Io { path, error })
    };
    let text = |file: &str| -> Result<String, ModuleError> { Ok(String::from_utf8_lossy(&read(file)?).into_owned()) };
    let held = if is_native_bin {
        let verified = verify_native_bin_in_dir(&dir, key.as_str(), blaze_own::Alloc::global())?;
        verified.as_str().into()
    } else if is_native_dylib {
        text("dylib-sha256")?.trim().to_owned()
    } else {
        text("wasm-sha256")?.trim().to_owned()
    };
    let mut macros = Vec::new();
    for line in text("macros")?.lines() {
        let bad = || ModuleError::Macros { key: key.clone(), line: line.to_owned() };
        let mut parts = line.split(' ');
        let index = parts.next().and_then(|it| it.parse().ok()).ok_or_else(bad)?;
        let kind = match parts.next() {
            Some("derive") => ProcKind::Derive,
            Some("attr") => ProcKind::Attr,
            Some("bang") => ProcKind::FnLike,
            _ => return Err(bad()),
        };
        let macro_name = parts.next().filter(|it| !it.is_empty()).ok_or_else(bad)?.to_owned();
        let helpers = parts.next().unwrap_or("").split(',').filter(|it| !it.is_empty()).map(str::to_owned).collect();
        macros.push(Exported { index, kind, name: macro_name, helpers });
    }
    let decls = if is_native_bin || is_native_dylib {
        dir.join("decls")
            .is_file()
            .then(|| text("decls").ok())
            .flatten()
            .unwrap_or_default()
            .trim()
            .to_owned()
    } else {
        text("decls")?.trim().to_owned()
    };
    Ok(ModuleRecord {
        key,
        name: name.to_owned(),
        version: version.to_owned(),
        wasm_sha256: held,
        macros,
        decls,
        dir,
        compiled: store.join(COMPILED_DIR),
        native_dylib: is_native_dylib.then_some(native_path),
        native_bin: is_native_bin.then_some(bin_path),
    })
}

static WRITE_STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PublishPlant {
    None,
    #[cfg(test)]
    SkipStageDirSync,
    #[cfg(test)]
    RenameBeforeDigest,
}

trait PublishFs {
    fn create_dir_all(&mut self, path: &Path) -> Result<(), IoError>;
    fn remove_dir_all(&mut self, path: &Path) -> Result<(), IoError>;
    fn write_and_sync_file(&mut self, path: &Path, bytes: &[u8], executable: bool) -> Result<(), IoError>;
    fn sync_dir(&mut self, path: &Path) -> Result<(), IoError>;
    fn rename(&mut self, from: &Path, to: &Path) -> Result<(), IoError>;
    fn is_file(&self, path: &Path) -> bool;
}

struct OsPublishFs;

impl PublishFs for OsPublishFs {
    fn create_dir_all(&mut self, path: &Path) -> Result<(), IoError> {
        fs::create_dir_all(path)
    }

    fn remove_dir_all(&mut self, path: &Path) -> Result<(), IoError> {
        fs::remove_dir_all(path)
    }

    fn write_and_sync_file(&mut self, path: &Path, bytes: &[u8], executable: bool) -> Result<(), IoError> {
        let mut file = File::create(path)?;
        file.write_all(bytes)?;
        #[cfg(unix)]
        if executable {
            file.set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        #[cfg(not(unix))]
        let _ = executable;
        file.sync_all()
    }

    fn sync_dir(&mut self, path: &Path) -> Result<(), IoError> {
        let dir = File::open(path)?;
        dir.sync_all()
    }

    fn rename(&mut self, from: &Path, to: &Path) -> Result<(), IoError> {
        fs::rename(from, to)
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
}

struct PublishPayload<'a> {
    identity: &'a [u8],
    digest_line: &'a [u8],
    macros: &'a [u8],
    decls_line: &'a [u8],
    bin_bytes: &'a [u8],
}

fn publish_native_bin_dir(
    fs_ops: &mut impl PublishFs,
    modules_dir: &Path,
    stage: &Path,
    final_dir: &Path,
    payload: &PublishPayload<'_>,
    plant: PublishPlant,
) -> Result<(), ModuleError> {
    #[cfg(not(test))]
    let _ = plant;
    fs_ops.create_dir_all(modules_dir).map_err(|error| ModuleError::Io {
        path: modules_dir.to_path_buf(),
        error,
    })?;
    let _ = fs_ops.remove_dir_all(stage);
    fs_ops.create_dir_all(stage).map_err(|error| ModuleError::Io {
        path: stage.to_path_buf(),
        error,
    })?;

    let write_stage = |ops: &mut dyn PublishFs| -> Result<(), ModuleError> {
        let mut put = |file: &str, bytes: &[u8], exec: bool| -> Result<(), ModuleError> {
            let path = stage.join(file);
            ops.write_and_sync_file(&path, bytes, exec)
                .map_err(|error| ModuleError::Io { path, error })
        };
        put("identity", payload.identity, false)?;
        put("macros", payload.macros, false)?;
        put("decls", payload.decls_line, false)?;
        put(NATIVE_BIN_FILENAME, payload.bin_bytes, true)?;
        #[cfg(test)]
        let defer_digest = plant == PublishPlant::RenameBeforeDigest;
        #[cfg(not(test))]
        let defer_digest = false;
        if !defer_digest {
            put(BIN_SHA256_FILENAME, payload.digest_line, false)?;
        }
        #[cfg(test)]
        let skip_stage_sync = plant == PublishPlant::SkipStageDirSync;
        #[cfg(not(test))]
        let skip_stage_sync = false;
        if !skip_stage_sync {
            ops.sync_dir(stage).map_err(|error| ModuleError::Io {
                path: stage.to_path_buf(),
                error,
            })?;
        }
        Ok(())
    };

    if let Err(err) = write_stage(fs_ops) {
        let _ = fs_ops.remove_dir_all(stage);
        return Err(err);
    }

    match fs_ops.rename(stage, final_dir) {
        Ok(()) => {
            #[cfg(test)]
            if plant == PublishPlant::RenameBeforeDigest {
                let path = final_dir.join(BIN_SHA256_FILENAME);
                fs_ops
                    .write_and_sync_file(&path, payload.digest_line, false)
                    .map_err(|error| ModuleError::Io { path, error })?;
            }
            fs_ops.sync_dir(modules_dir).map_err(|error| ModuleError::Io {
                path: modules_dir.to_path_buf(),
                error,
            })?;
        }
        Err(error) => {
            let _ = fs_ops.remove_dir_all(stage);
            let race_ok = matches!(error.kind(), ErrorKind::AlreadyExists | ErrorKind::DirectoryNotEmpty)
                && fs_ops.is_file(&final_dir.join(BIN_SHA256_FILENAME));
            if !race_ok {
                return Err(ModuleError::Io { path: final_dir.to_path_buf(), error });
            }
        }
    }
    Ok(())
}

/// Crash-durably publishes a Tier-2 Blaze-compiled standalone native proc-macro module (`native.bin`
/// with `0o755` executable mode, `bin-sha256`, `identity`, `macros`, and `decls`) under its
/// host-target-keyed directory `<store>/proc-macro/modules/<native_module_key(id, target)>`.
///
/// Every file is fsynced before `sync_all` on the staging directory, atomic directory rename, and
/// `sync_all` on the parent `modules/` directory.
pub fn write_native_bin_module(
    store: &Path,
    id: &CrateId<'_>,
    target: NativeTarget<'_>,
    bin_bytes: &[u8],
    macros: &[Exported],
    decls: &str,
) -> Result<ModuleRecord, ModuleError> {
    let shared = blaze_own::SharedAlloc::global();
    let alloc = blaze_own::Alloc::global();
    let identity_text = native_identity(id, target, alloc);
    let key = native_module_key(id, target, alloc);
    let modules_dir = store.join("proc-macro/modules");
    let seq = WRITE_STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = process::id();
    let mut stage_name = blaze_own::String::new_in(shared);
    let _ = core::fmt::Write::write_fmt(
        &mut stage_name,
        format_args!(".tmp-{}-{pid}-{seq}", key.as_str()),
    );
    let stage = modules_dir.join(stage_name.as_str());
    let final_dir = modules_dir.join(key.as_str());

    let digest = sha256_hex(bin_bytes);
    let mut digest_line = blaze_own::String::new_in(shared);
    let _ = core::fmt::Write::write_fmt(&mut digest_line, format_args!("{digest}\n"));

    let mut macros_text = blaze_own::String::new_in(shared);
    for exported in macros {
        let kind_word = match exported.kind {
            ProcKind::Derive => "derive",
            ProcKind::Attr => "attr",
            ProcKind::FnLike => "bang",
        };
        let _ = core::fmt::Write::write_fmt(
            &mut macros_text,
            format_args!("{} {} {}", exported.index, kind_word, exported.name),
        );
        if !exported.helpers.is_empty() {
            macros_text.push(' ');
            for (idx, helper) in exported.helpers.iter().enumerate() {
                if idx > 0 {
                    macros_text.push(',');
                }
                macros_text.push_str(helper);
            }
        }
        macros_text.push('\n');
    }

    let mut decls_line = blaze_own::String::new_in(shared);
    let _ = core::fmt::Write::write_fmt(&mut decls_line, format_args!("{decls}\n"));

    let payload = PublishPayload {
        identity: identity_text.as_str().as_bytes(),
        digest_line: digest_line.as_str().as_bytes(),
        macros: macros_text.as_str().as_bytes(),
        decls_line: decls_line.as_str().as_bytes(),
        bin_bytes,
    };

    let mut os_fs = OsPublishFs;
    publish_native_bin_dir(&mut os_fs, &modules_dir, &stage, &final_dir, &payload, PublishPlant::None)?;
    read_module_by_key(store, key.as_str().into(), id.name, id.version)
}

#[cfg(test)]
#[path = "module_lock_tests.rs"]
mod lock_tests;

#[cfg(test)]
mod header_tests {
    use super::*;

    #[test]
    fn proc_macro_module_identity_uses_form_11_and_misses_stale_form_10_through_4_store_entries() {
        let id = CrateId {
            name: "demo_derive",
            version: "0.1.0",
            source: Source::Registry { checksum: "a0" },
            deps: "deps-hash",
            features: &["default"],
        };
        let v11_identity = identity(&id);
        assert_eq!(MODULE_HEADER, "blaze proc-macro module wasm64 11");
        assert_eq!(v11_identity.lines().next(), Some(MODULE_HEADER));
        let v10_w64_identity = v11_identity.replacen(MODULE_HEADER, "blaze proc-macro module wasm64 10", 1);
        let v9_w64_identity = v11_identity.replacen(MODULE_HEADER, "blaze proc-macro module wasm64 9", 1);
        let v8_w64_identity = v11_identity.replacen(MODULE_HEADER, "blaze proc-macro module wasm64 8", 1);
        let v7_w32_identity = v11_identity.replacen(MODULE_HEADER, "blaze proc-macro module wasm32 7", 1);
        let v7_identity = v11_identity.replacen(MODULE_HEADER, "blaze proc-macro module 7", 1);
        let v6_identity = v11_identity.replacen(MODULE_HEADER, "blaze proc-macro module 6", 1);
        let v5_identity = v11_identity.replacen(MODULE_HEADER, "blaze proc-macro module 5", 1);
        let v4_identity = v11_identity.replacen(MODULE_HEADER, "blaze proc-macro module 4", 1);
        let v10_w64_key = sha256_hex(v10_w64_identity.as_bytes());
        let v9_w64_key = sha256_hex(v9_w64_identity.as_bytes());
        let v8_w64_key = sha256_hex(v8_w64_identity.as_bytes());
        let v7_w32_key = sha256_hex(v7_w32_identity.as_bytes());
        let v7_key = sha256_hex(v7_identity.as_bytes());
        let v6_key = sha256_hex(v6_identity.as_bytes());
        let v5_key = sha256_hex(v5_identity.as_bytes());
        let v4_key = sha256_hex(v4_identity.as_bytes());
        let v11_key = module_key(&id);
        assert_ne!(v10_w64_key, v11_key, "wasm64 form 11 key must differ from stale wasm64 form 10 key");
        assert_ne!(v9_w64_key, v11_key, "wasm64 form 11 key must differ from stale wasm64 form 9 key");
        assert_ne!(v8_w64_key, v11_key, "wasm64 form 11 key must differ from stale wasm64 form 8 key");
        assert_ne!(v7_w32_key, v11_key, "wasm64 form 11 key must differ from wasm32 bootstrap form 7 key");
        assert_ne!(v7_key, v11_key, "form 11 key must differ from stale form 7 key");
        assert_ne!(v6_key, v11_key, "form 11 key must differ from stale form 6 key");
        assert_ne!(v5_key, v11_key, "form 11 key must differ from stale form 5 key");
        assert_ne!(v4_key, v11_key, "form 11 key must differ from stale form 4 key");

        use std::{env, fs, process};
        let store = env::temp_dir().join(format!("blaze-pm-module-v11-test-{}", process::id()));
        let _ = fs::remove_dir_all(&store);
        let write_entry = |dir: &Path, id_text: &str, digest: &str, macros: &str| {
            fs::create_dir_all(dir).expect("create module dir");
            for (name, body) in [
                ("identity", id_text),
                ("wasm-sha256", digest),
                ("macros", macros),
                ("decls", "__rustc_proc_macro_decls_blaze__\n"),
            ] {
                fs::write(dir.join(name), body).expect(name);
            }
        };
        for (stale_key, stale_identity, stale_digest) in [
            (&v4_key, &v4_identity, "deadbeef\n"),
            (&v5_key, &v5_identity, "feedface\n"),
            (&v6_key, &v6_identity, "badc0ffe\n"),
            (&v7_key, &v7_identity, "01234567\n"),
            (&v7_w32_key, &v7_w32_identity, "76543210\n"),
            (&v8_w64_key, &v8_w64_identity, "89abcdef\n"),
            (&v9_w64_key, &v9_w64_identity, "fedcba98\n"),
            (&v10_w64_key, &v10_w64_identity, "11223344\n"),
        ] {
            write_entry(&store.join("proc-macro/modules").join(stale_key), stale_identity, stale_digest, "");
        }

        // Lookup by current wasm64 form 11 misses stale wasm64 form 10..8, wasm32 form 7, and form 7..4 directories cleanly.
        assert!(matches!(find_module(&store, &id), Err(ModuleError::Missing { ref key, .. }) if key == &v11_key));
        assert!(matches!(read_module(&store, &id), Err(ModuleError::Missing { ref key, .. }) if key == &v11_key));

        // Publishing form 11 alongside stale form 10..4 resolves uniquely to form 11.
        let v11_dir = store.join("proc-macro/modules").join(&v11_key);
        write_entry(&v11_dir, &v11_identity, "cafebabe\n", "0 derive Demo\n");

        let (found_key, digest) = find_module(&store, &id).expect("find v11 module");
        assert_eq!(found_key, v11_key);
        assert_eq!(digest, "cafebabe");
        let rec = read_module(&store, &id).expect("read v11 module");
        assert_eq!(rec.key, v11_key);
        assert_eq!(rec.wasm_sha256, "cafebabe");
        assert_eq!(rec.macros.len(), 1);
        assert_eq!(rec.macros[0].name, "Demo");

        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn write_and_read_native_bin_module_verifies_digest_and_exec_permissions() {
        use std::{env, fs, process};
        let store = env::temp_dir().join(format!("blaze-pm-native-bin-test-{}", process::id()));
        let _ = fs::remove_dir_all(&store);

        let id = CrateId {
            name: "sqlx_macros",
            version: "0.9.0",
            source: Source::Registry { checksum: "ab12" },
            deps: "deps-tier2",
            features: &["runtime-tokio", "postgres"],
        };
        let host_target = NativeTarget::host();
        let expected_key = native_module_key(&id, host_target, blaze_own::Alloc::global());
        let bin_bytes = b"\x7fELF-synthetic-native-bin-payload";
        let exports = vec![
            Exported { index: 0, kind: ProcKind::FnLike, name: "expand_query".to_owned(), helpers: Vec::new() },
            Exported { index: 1, kind: ProcKind::Derive, name: "FromRow".to_owned(), helpers: vec!["sqlx".to_owned()] },
            Exported { index: 2, kind: ProcKind::Attr, name: "test".to_owned(), helpers: Vec::new() },
        ];

        let rec = write_native_bin_module(
            &store,
            &id,
            host_target,
            bin_bytes,
            &exports,
            "__blaze_pm_decls__",
        )
        .expect("write native.bin module");

        assert_eq!(rec.key, expected_key.as_str());
        assert_ne!(rec.key, module_key(&id), "native.bin key must never collide with wasm64 module_key");
        assert_eq!(rec.wasm_sha256, sha256_hex(bin_bytes));
        assert_eq!(rec.macros, exports);
        assert_eq!(rec.decls, "__blaze_pm_decls__");
        assert!(rec.native_dylib.is_none());
        let bin_path = rec.native_bin.as_ref().expect("native_bin present");
        assert_eq!(bin_path, &rec.dir.join(NATIVE_BIN_FILENAME));
        assert_eq!(rec.verify_native_bin().expect("verify"), bin_path.as_path());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(bin_path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "native.bin must have 0o755 executable permissions");
        }

        let (found_key, found_digest) = find_module(&store, &id).expect("find_module");
        assert_eq!(found_key, expected_key.as_str());
        assert_eq!(found_digest, sha256_hex(bin_bytes));

        let reread = read_module(&store, &id).expect("read_module");
        assert_eq!(reread.native_bin.as_deref(), Some(bin_path.as_path()));
        assert_eq!(reread.macros, exports);

        // Truncating or tampering with native.bin MUST fail find_module AND read_module with Digest!
        fs::write(bin_path, b"\x7fELF-trunc").expect("truncate");
        assert!(matches!(find_module(&store, &id), Err(ModuleError::Digest { .. })));
        assert!(matches!(read_module(&store, &id), Err(ModuleError::Digest { .. })));
        assert!(matches!(rec.verify_native_bin(), Err(ModuleError::Digest { .. })));

        // Restoring binary but writing malformed/non-UTF-8 bin-sha256 MUST fail find_module!
        fs::write(bin_path, bin_bytes).expect("restore bin");
        fs::write(rec.dir.join(BIN_SHA256_FILENAME), b"\xff\xfe\x00bad").expect("bad digest");
        assert!(matches!(find_module(&store, &id), Err(ModuleError::Digest { .. })));
        fs::write(rec.dir.join(BIN_SHA256_FILENAME), b"not-64-hex\n").expect("short digest");
        assert!(matches!(find_module(&store, &id), Err(ModuleError::Digest { .. })));

        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn native_module_key_is_distinct_across_host_targets_and_wasm() {
        use std::{env, fs, process};
        let alloc = blaze_own::testing::heap().alloc();
        let id = CrateId {
            name: "sqlx_macros",
            version: "0.9.0",
            source: Source::Registry { checksum: "cd34" },
            deps: "deps-tier2",
            features: &["runtime-tokio"],
        };
        let linux_arm64 = NativeTarget { triple: "aarch64-unknown-linux-gnu", abi_id: "bzpm-v1" };
        let linux_x64 = NativeTarget { triple: "x86_64-unknown-linux-gnu", abi_id: "bzpm-v1" };
        let macos_arm64 = NativeTarget { triple: "aarch64-apple-darwin", abi_id: "bzpm-v1" };
        let linux_arm64_v2 = NativeTarget { triple: "aarch64-unknown-linux-gnu", abi_id: "bzpm-v2" };

        let k_la = native_module_key(&id, linux_arm64, alloc);
        let k_lx = native_module_key(&id, linux_x64, alloc);
        let k_ma = native_module_key(&id, macos_arm64, alloc);
        let k_v2 = native_module_key(&id, linux_arm64_v2, alloc);
        let k_wasm = module_key(&id);

        let all = [k_la.as_str(), k_lx.as_str(), k_ma.as_str(), k_v2.as_str(), k_wasm.as_str()];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "keys {i} and {j} must not collide");
            }
        }

        // Publishing two different host targets in the same store keeps them completely isolated.
        let store = env::temp_dir().join(format!("blaze-pm-multi-target-test-{}", process::id()));
        let _ = fs::remove_dir_all(&store);
        let exports = vec![Exported { index: 0, kind: ProcKind::FnLike, name: "q".to_owned(), helpers: Vec::new() }];
        let r_la = write_native_bin_module(&store, &id, linux_arm64, b"elf-arm64", &exports, "").expect("la");
        let r_ma = write_native_bin_module(&store, &id, macos_arm64, b"macho-arm64", &exports, "").expect("ma");
        assert_ne!(r_la.dir, r_ma.dir);
        assert_eq!(
            find_native_module(&store, &id, linux_arm64, alloc).expect("find la").1.as_str(),
            sha256_hex(b"elf-arm64")
        );
        assert_eq!(
            find_native_module(&store, &id, macos_arm64, alloc).expect("find ma").1.as_str(),
            sha256_hex(b"macho-arm64")
        );
        assert!(matches!(find_native_module(&store, &id, linux_x64, alloc), Err(ModuleError::Missing { .. })));
        let _ = fs::remove_dir_all(&store);
    }

    /// Power-loss and sync-failure Deterministic Simulation Test (AGENTS.md rule 43) for
    /// `publish_native_bin_dir`: models volatile vs durable disk state, injects write/sync/rename
    /// errors and power-loss crashes at every operation boundary, checks the atomic recovery oracle
    /// (final directory is either completely absent or 100% intact with matching `bin-sha256` and
    /// executable `native.bin`), and proves two planted-bug controls fail the oracle.
    #[test]
    fn write_native_bin_module_crash_and_sync_failure_dst() {
        use std::collections::{BTreeMap, BTreeSet};

        #[derive(Clone, Default)]
        struct SimFile {
            bytes: Vec<u8>,
            exec: bool,
        }

        #[derive(Clone, Default)]
        struct SimDisk {
            dirs: BTreeSet<PathBuf>,
            files: BTreeMap<PathBuf, SimFile>,
        }

        struct SimCrashFs {
            volatile: SimDisk,
            durable: SimDisk,
            synced_dirs: BTreeSet<PathBuf>,
            op_count: usize,
            fail_at: Option<usize>,
            crash_at: Option<usize>,
            crashed: bool,
            /// Models ext4/APFS power-loss reordering: if a directory was renamed without first
            /// syncing the staging directory, its child entries are lost on crash.
            drop_unsynced_rename_children: bool,
        }

        impl SimCrashFs {
            fn new(fail_at: Option<usize>, crash_at: Option<usize>, drop_unsynced: bool) -> Self {
                Self {
                    volatile: SimDisk::default(),
                    durable: SimDisk::default(),
                    synced_dirs: BTreeSet::new(),
                    op_count: 0,
                    fail_at,
                    crash_at,
                    crashed: false,
                    drop_unsynced_rename_children: drop_unsynced,
                }
            }

            fn tick(&mut self, what: &str) -> Result<(), IoError> {
                let idx = self.op_count;
                self.op_count += 1;
                if self.crash_at == Some(idx) {
                    self.crashed = true;
                    return Err(IoError::new(ErrorKind::Other, format!("power loss at op {idx} ({what})")));
                }
                if self.fail_at == Some(idx) {
                    return Err(IoError::new(ErrorKind::Other, format!("injected I/O error at op {idx} ({what})")));
                }
                Ok(())
            }
        }

        impl PublishFs for SimCrashFs {
            fn create_dir_all(&mut self, path: &Path) -> Result<(), IoError> {
                self.tick("create_dir_all")?;
                let mut cur = PathBuf::new();
                for comp in path.components() {
                    cur.push(comp);
                    self.volatile.dirs.insert(cur.clone());
                }
                Ok(())
            }

            fn remove_dir_all(&mut self, path: &Path) -> Result<(), IoError> {
                self.volatile.dirs.retain(|d| !d.starts_with(path));
                self.volatile.files.retain(|f, _| !f.starts_with(path));
                self.synced_dirs.remove(path);
                Ok(())
            }

            fn write_and_sync_file(&mut self, path: &Path, bytes: &[u8], executable: bool) -> Result<(), IoError> {
                self.tick("write_file")?;
                self.volatile.files.insert(path.to_path_buf(), SimFile { bytes: bytes.to_vec(), exec: executable });
                self.tick("sync_file")?;
                // File data is durable in its inode, but directory entry is only durable once
                // the containing directory is synced!
                if let Some(parent) = path.parent() {
                    self.synced_dirs.remove(parent);
                }
                Ok(())
            }

            fn sync_dir(&mut self, path: &Path) -> Result<(), IoError> {
                self.tick("sync_dir")?;
                self.synced_dirs.insert(path.to_path_buf());
                if self.volatile.dirs.contains(path) {
                    self.durable.dirs.insert(path.to_path_buf());
                }
                // Persist direct children of `path` into `durable`.
                for (fpath, file) in &self.volatile.files {
                    if fpath.parent() == Some(path) {
                        self.durable.files.insert(fpath.clone(), file.clone());
                    }
                }
                for dpath in &self.volatile.dirs {
                    if dpath.parent() == Some(path) {
                        self.durable.dirs.insert(dpath.clone());
                    }
                }
                Ok(())
            }

            fn rename(&mut self, from: &Path, to: &Path) -> Result<(), IoError> {
                self.tick("rename")?;
                if self.volatile.dirs.contains(to) {
                    return Err(IoError::new(ErrorKind::DirectoryNotEmpty, "target exists"));
                }
                let stage_was_synced = self.synced_dirs.contains(from);
                let moved_files: Vec<(PathBuf, SimFile)> = self
                    .volatile
                    .files
                    .iter()
                    .filter(|(p, _)| p.starts_with(from))
                    .map(|(p, f)| (to.join(p.strip_prefix(from).expect("strip")), f.clone()))
                    .collect();
                self.volatile.dirs.remove(from);
                self.volatile.files.retain(|p, _| !p.starts_with(from));
                self.volatile.dirs.insert(to.to_path_buf());
                for (new_p, f) in &moved_files {
                    self.volatile.files.insert(new_p.clone(), f.clone());
                }
                // Model power-loss visibility of the rename: directory rename becomes durable on
                // disk (or immediately upon journal flush), bringing along ONLY files whose stage
                // dir was synced beforehand (unless `drop_unsynced_rename_children` is false).
                self.durable.dirs.insert(to.to_path_buf());
                if stage_was_synced || !self.drop_unsynced_rename_children {
                    for (new_p, f) in moved_files {
                        self.durable.files.insert(new_p, f);
                    }
                }
                Ok(())
            }

            fn is_file(&self, path: &Path) -> bool {
                self.volatile.files.contains_key(path)
            }
        }

        let modules_dir = PathBuf::from("/store/proc-macro/modules");
        let stage = modules_dir.join(".tmp-key-1-0");
        let final_dir = modules_dir.join("key-native-1");
        let bin_bytes = b"\x7fELF-crash-dst-native-bin";
        let expected_digest = sha256_hex(bin_bytes);
        let digest_line = format!("{expected_digest}\n");
        let payload = PublishPayload {
            identity: b"id\n",
            digest_line: digest_line.as_bytes(),
            macros: b"0 bang query\n",
            decls_line: b"__decls__\n",
            bin_bytes,
        };

        let check_durable_oracle = |disk: &SimDisk, must_exist: bool| -> Result<(), String> {
            let exists = disk.dirs.contains(&final_dir);
            if !exists {
                if must_exist {
                    return Err("expected final_dir to be durable after Ok publish".to_owned());
                }
                // No orphan file under final_dir may exist if final_dir itself is absent.
                if disk.files.keys().any(|p| p.starts_with(&final_dir)) {
                    return Err("orphan durable files under absent final_dir".to_owned());
                }
                return Ok(());
            }
            for req in ["identity", "macros", "decls", BIN_SHA256_FILENAME, NATIVE_BIN_FILENAME] {
                if !disk.files.contains_key(&final_dir.join(req)) {
                    return Err(format!("durable final_dir missing `{req}` after crash"));
                }
            }
            let bin = &disk.files[&final_dir.join(NATIVE_BIN_FILENAME)];
            if !bin.exec {
                return Err("durable native.bin lost executable mode".to_owned());
            }
            let held = core::str::from_utf8(&disk.files[&final_dir.join(BIN_SHA256_FILENAME)].bytes)
                .unwrap_or("")
                .trim();
            let found = sha256_hex(&bin.bytes);
            if !is_valid_sha256_hex(held) || held != found {
                return Err(format!("durable digest mismatch: held={held:?} found={found:?}"));
            }
            Ok(())
        };

        let run_matrix = |plant: PublishPlant| -> Result<(), String> {
            // 1. Clean publish must succeed and satisfy oracle with must_exist=true.
            let mut clean = SimCrashFs::new(None, None, true);
            publish_native_bin_dir(&mut clean, &modules_dir, &stage, &final_dir, &payload, plant)
                .map_err(|e| format!("clean publish failed: {e}"))?;
            check_durable_oracle(&clean.durable, true)?;
            let total_ops = clean.op_count;

            // 2. Inject I/O error at EVERY op index 0..total_ops.
            for fail_at in 0..total_ops {
                let mut fs_sim = SimCrashFs::new(Some(fail_at), None, true);
                let _ = publish_native_bin_dir(&mut fs_sim, &modules_dir, &stage, &final_dir, &payload, plant);
                check_durable_oracle(&fs_sim.durable, false)?;
            }

            // 3. Inject power-loss crash at EVERY op index 0..=total_ops.
            for crash_at in 0..=total_ops {
                let mut fs_sim = SimCrashFs::new(None, Some(crash_at), true);
                let res = publish_native_bin_dir(&mut fs_sim, &modules_dir, &stage, &final_dir, &payload, plant);
                check_durable_oracle(&fs_sim.durable, res.is_ok())?;
            }
            Ok(())
        };

        // Clean implementation passes every failure & crash point.
        run_matrix(PublishPlant::None).expect("clean publish DST must pass");

        // Planted-bug control 1: skipping stage dir sync MUST fail the crash oracle.
        assert!(
            run_matrix(PublishPlant::SkipStageDirSync).is_err(),
            "DST must catch PublishPlant::SkipStageDirSync"
        );

        // Planted-bug control 2: renaming before writing bin-sha256 MUST fail the crash oracle.
        assert!(
            run_matrix(PublishPlant::RenameBeforeDigest).is_err(),
            "DST must catch PublishPlant::RenameBeforeDigest"
        );
    }

    #[test]
    fn deps_with_wasm_features_invalidates_when_wasi_dep_feature_changes() {
        use blaze_manifest::lock::LockId;
        use blaze_own::Vec as OwnVec;
        use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
        let alloc = blaze_own::testing::heap().alloc();

        let lock_text = "version = 4\n\n\
            [[package]]\nname = \"demo_pm\"\nversion = \"0.1.0\"\ndependencies = [\"wasi_helper\"]\n\n\
            [[package]]\nname = \"wasi_helper\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"1111111111111111111111111111111111111111111111111111111111111111\"\n";
        let lock = Lock::read(lock_text.as_bytes(), alloc).expect("lock");

        let mk_wasm_graph = |wasi_feats: &[&str]| {
            let mut pm_deps = OwnVec::new_in(alloc);
            pm_deps.push(UnitDep { name: Text::from_str_in("wasi_helper", alloc), unit: UnitId(1) });
            let mut dep_feats = OwnVec::new_in(alloc);
            for f in wasi_feats {
                dep_feats.push(Text::from_str_in(f, alloc));
            }
            let mut units = OwnVec::new_in(alloc);
            units.push(Unit {
                package: LockId(0),
                name: Text::from_str_in("demo_pm", alloc),
                version: Text::from_str_in("0.1.0", alloc),
                kind: UnitKind::ProcMacro,
                for_host: true,
                feature_side: blaze_unit_graph::HostFeatureSide::Host,
                edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
                crate_name: Text::from_str_in("demo_pm", alloc),
                root: Text::from_str_in("src/lib.rs", alloc),
                features: OwnVec::new_in(alloc),
                cfg: OwnVec::new_in(alloc),
                deps: pm_deps,
                build_script: None,
                links: None,
            });
            units.push(Unit {
                package: LockId(1),
                name: Text::from_str_in("wasi_helper", alloc),
                version: Text::from_str_in("0.1.0", alloc),
                kind: UnitKind::Lib,
                for_host: true,
                feature_side: blaze_unit_graph::HostFeatureSide::Host,
                edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
                crate_name: Text::from_str_in("wasi_helper", alloc),
                root: Text::from_str_in("src/lib.rs", alloc),
                features: dep_feats,
                cfg: OwnVec::new_in(alloc),
                deps: OwnVec::new_in(alloc),
                build_script: None,
                links: None,
            });
            UnitGraph { units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) }
        };

        let wg_on = mk_wasm_graph(&["extra_wasi"]);
        let wg_off = mk_wasm_graph(&[]);
        let d_on = deps_with_wasm_features("same-lock-closure", &lock, &wg_on, LockId(0), &[] as &[&str], |_| None::<&str>, alloc).expect("d_on");
        let d_off = deps_with_wasm_features("same-lock-closure", &lock, &wg_off, LockId(0), &[] as &[&str], |_| None::<&str>, alloc).expect("d_off");
        assert_ne!(d_on.as_str(), d_off.as_str(), "proc-macro deps digest must change when a WASI-only dep feature toggles");
    }

    #[test]
    fn deps_with_wasm_features_distinguishes_same_name_version_different_source_feature_swap() {
        use blaze_manifest::lock::LockId;
        use blaze_own::Vec as OwnVec;
        use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
        let alloc = blaze_own::testing::heap().alloc();

        // Two dependencies with the EXACT same name ("dup") and version ("0.1.0"), from two
        // different sources (registry checksum vs git commit). Swapping feature sets A and B
        // between them must produce a distinct digest even with identical `lock_deps`.
        let lock_text = "version = 4\n\n\
            [[package]]\nname = \"demo_pm\"\nversion = \"0.1.0\"\ndependencies = [\n \"dup 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)\",\n \"dup 0.1.0 (git+https://example.com/dup.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)\",\n]\n\n\
            [[package]]\nname = \"dup\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"1111111111111111111111111111111111111111111111111111111111111111\"\n\n\
            [[package]]\nname = \"dup\"\nversion = \"0.1.0\"\nsource = \"git+https://example.com/dup.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n";
        let lock = Lock::read(lock_text.as_bytes(), alloc).expect("lock");

        let mk_wasm_graph = |feats_reg: &[&str], feats_git: &[&str]| {
            let mut pm_deps = OwnVec::new_in(alloc);
            pm_deps.push(UnitDep { name: Text::from_str_in("dup_reg", alloc), unit: UnitId(1) });
            pm_deps.push(UnitDep { name: Text::from_str_in("dup_git", alloc), unit: UnitId(2) });
            let mk_feats = |fs: &[&str]| {
                let mut v = OwnVec::new_in(alloc);
                for f in fs {
                    v.push(Text::from_str_in(f, alloc));
                }
                v
            };
            let mut units = OwnVec::new_in(alloc);
            units.push(Unit {
                package: LockId(0),
                name: Text::from_str_in("demo_pm", alloc),
                version: Text::from_str_in("0.1.0", alloc),
                kind: UnitKind::ProcMacro,
                for_host: true,
                feature_side: blaze_unit_graph::HostFeatureSide::Host,
                edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
                crate_name: Text::from_str_in("demo_pm", alloc),
                root: Text::from_str_in("src/lib.rs", alloc),
                features: OwnVec::new_in(alloc),
                cfg: OwnVec::new_in(alloc),
                deps: pm_deps,
                build_script: None,
                links: None,
            });
            units.push(Unit {
                package: LockId(1),
                name: Text::from_str_in("dup", alloc),
                version: Text::from_str_in("0.1.0", alloc),
                kind: UnitKind::Lib,
                for_host: true,
                feature_side: blaze_unit_graph::HostFeatureSide::Host,
                edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
                crate_name: Text::from_str_in("dup", alloc),
                root: Text::from_str_in("src/lib.rs", alloc),
                features: mk_feats(feats_reg),
                cfg: OwnVec::new_in(alloc),
                deps: OwnVec::new_in(alloc),
                build_script: None,
                links: None,
            });
            units.push(Unit {
                package: LockId(2),
                name: Text::from_str_in("dup", alloc),
                version: Text::from_str_in("0.1.0", alloc),
                kind: UnitKind::Lib,
                for_host: true,
                feature_side: blaze_unit_graph::HostFeatureSide::Host,
                edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
                crate_name: Text::from_str_in("dup", alloc),
                root: Text::from_str_in("src/lib.rs", alloc),
                features: mk_feats(feats_git),
                cfg: OwnVec::new_in(alloc),
                deps: OwnVec::new_in(alloc),
                build_script: None,
                links: None,
            });
            UnitGraph { units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) }
        };

        let wg_ab = mk_wasm_graph(&["feat_a"], &["feat_b"]);
        let wg_ba = mk_wasm_graph(&["feat_b"], &["feat_a"]);
        let d_ab = deps_with_wasm_features("same-lock-closure", &lock, &wg_ab, LockId(0), &[] as &[&str], |_| None::<&str>, alloc).expect("d_ab");
        let d_ba = deps_with_wasm_features("same-lock-closure", &lock, &wg_ba, LockId(0), &[] as &[&str], |_| None::<&str>, alloc).expect("d_ba");
        assert_ne!(
            d_ab.as_str(),
            d_ba.as_str(),
            "swapping features A and B between two same-name/version packages from different sources must change the digest"
        );
    }

    #[test]
    fn deps_with_wasm_features_rejects_mismatched_root_traverses_dep_build_scripts_and_refuses_unpinned_paths() {
        use blaze_manifest::lock::LockId;
        use blaze_own::Vec as OwnVec;
        use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
        let alloc = blaze_own::testing::heap().alloc();

        // Packages:
        //   0: demo_pm (proc-macro)
        //   1: dep_lib (registry lib with a build.rs)
        //   2: wasi_bs_helper (WASI-only path build-dependency of dep_lib's build.rs)
        let lock_text = "version = 4\n\n\
            [[package]]\nname = \"demo_pm\"\nversion = \"0.1.0\"\ndependencies = [\"dep_lib\"]\n\n\
            [[package]]\nname = \"dep_lib\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"1111111111111111111111111111111111111111111111111111111111111111\"\ndependencies = [\"wasi_bs_helper\"]\n\n\
            [[package]]\nname = \"wasi_bs_helper\"\nversion = \"0.1.0\"\n";
        let lock = Lock::read(lock_text.as_bytes(), alloc).expect("lock");

        let mk_unit = |pkg: u32, name: &str, kind: UnitKind<'static>, feats: &[&str], deps: &[(&str, u32)], bs: Option<u32>| {
            let mut d = OwnVec::new_in(alloc);
            for &(dn, du) in deps {
                d.push(UnitDep { name: Text::from_str_in(dn, alloc), unit: UnitId(du) });
            }
            let mut f = OwnVec::new_in(alloc);
            for &feat in feats {
                f.push(Text::from_str_in(feat, alloc));
            }
            let root = if kind == UnitKind::BuildScript { "build.rs" } else { "src/lib.rs" };
            Unit {
                package: LockId(pkg),
                name: Text::from_str_in(name, alloc),
                version: Text::from_str_in("0.1.0", alloc),
                kind,
                for_host: true,
                feature_side: blaze_unit_graph::HostFeatureSide::Host,
                edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
                crate_name: Text::from_str_in(name, alloc),
                root: Text::from_str_in(root, alloc),
                features: f,
                cfg: OwnVec::new_in(alloc),
                deps: d,
                build_script: bs.map(UnitId),
                links: None,
            }
        };

        // wasm_graph:
        //   0: demo_pm (ProcMacro, features=["feat_a"]) -> [1 (dep_lib)]
        //   1: dep_lib (Lib, build_script=Some(2))
        //   2: dep_lib (BuildScript) -> [3 (wasi_bs_helper)]
        //   3: wasi_bs_helper (Lib, path)
        let mut units = OwnVec::new_in(alloc);
        units.push(mk_unit(0, "demo_pm", UnitKind::ProcMacro, &["feat_a"], &[("dep_lib", 1)], None));
        units.push(mk_unit(1, "dep_lib", UnitKind::Lib, &[], &[], Some(2)));
        units.push(mk_unit(1, "dep_lib", UnitKind::BuildScript, &[], &[("wasi_bs_helper", 3)], None));
        units.push(mk_unit(2, "wasi_bs_helper", UnitKind::Lib, &[], &[], None));
        let wg = UnitGraph { units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };

        // 1. Mismatched feature set (asking for ["feat_b"] when wasm_graph only has ["feat_a"])
        //    must return `Err(ClosureError::WasmRootMissing)` instead of folding another variant.
        let err_mismatch = deps_with_wasm_features("base", &lock, &wg, LockId(0), &["feat_b"], |_| Some("helper-v1"), alloc)
            .expect_err("mismatched proc-macro features must be rejected");
        assert_eq!(
            err_mismatch,
            ClosureError::WasmRootMissing { name: "demo_pm".to_owned(), version: "0.1.0".to_owned() }
        );

        // 2. Reachable dep's `build_script` WASI-only path build-dep MUST enter and invalidate the key.
        let d_v1 = deps_with_wasm_features("base", &lock, &wg, LockId(0), &["feat_a"], |id| (id == LockId(2)).then_some("helper-v1"), alloc)
            .expect("d_v1");
        let d_v2 = deps_with_wasm_features("base", &lock, &wg, LockId(0), &["feat_a"], |id| (id == LockId(2)).then_some("helper-v2"), alloc)
            .expect("d_v2");
        assert_ne!(
            d_v1.as_str(),
            d_v2.as_str(),
            "editing a dependency build-script's WASI-only path helper must move the proc-macro key"
        );

        // 3. Unpinned/missing digest for a reachable path dependency must return `Err(ClosureError::Unpinned)`
        //    instead of silently succeeding with `"path:unpinned"`.
        let err_unpinned = deps_with_wasm_features("base", &lock, &wg, LockId(0), &["feat_a"], |_| None::<&str>, alloc)
            .expect_err("missing digest for reachable path dep must be refused");
        assert_eq!(
            err_unpinned,
            ClosureError::Unpinned { name: "wasi_bs_helper".to_owned(), version: "0.1.0".to_owned() }
        );

        // 4. WASI-added superset feature on ProcMacro root succeeds and invalidates the key (rule 21),
        //    while missing requested features and ambiguous supersets still return `WasmRootMissing`.
        let mut wg_sup_units = OwnVec::new_in(alloc);
        wg_sup_units.push(mk_unit(0, "demo_pm", UnitKind::ProcMacro, &["feat_a", "wasi_extra"], &[("dep_lib", 1)], None));
        wg_sup_units.push(mk_unit(1, "dep_lib", UnitKind::Lib, &[], &[], Some(2)));
        wg_sup_units.push(mk_unit(1, "dep_lib", UnitKind::BuildScript, &[], &[("wasi_bs_helper", 3)], None));
        wg_sup_units.push(mk_unit(2, "wasi_bs_helper", UnitKind::Lib, &[], &[], None));
        let wg_sup = UnitGraph { units: wg_sup_units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };

        let d_sup = deps_with_wasm_features("base", &lock, &wg_sup, LockId(0), &["feat_a"], |id| (id == LockId(2)).then_some("helper-v1"), alloc)
            .expect("unique WASI feature superset must succeed");
        assert_ne!(
            d_sup.as_str(),
            d_v1.as_str(),
            "WASI-added root feature on ProcMacro must invalidate the module key (rule 21)"
        );

        let err_sup_missing = deps_with_wasm_features("base", &lock, &wg_sup, LockId(0), &["feat_a", "missing"], |id| (id == LockId(2)).then_some("helper-v1"), alloc)
            .expect_err("missing requested feature must still fail closed");
        assert_eq!(
            err_sup_missing,
            ClosureError::WasmRootMissing { name: "demo_pm".to_owned(), version: "0.1.0".to_owned() }
        );

        let mut wg_ambig_units = OwnVec::new_in(alloc);
        wg_ambig_units.push(mk_unit(0, "demo_pm", UnitKind::ProcMacro, &["feat_a", "wasi_x"], &[], None));
        wg_ambig_units.push(mk_unit(0, "demo_pm", UnitKind::ProcMacro, &["feat_a", "wasi_y"], &[], None));
        let wg_ambig = UnitGraph { units: wg_ambig_units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };
        let err_ambig = deps_with_wasm_features("base", &lock, &wg_ambig, LockId(0), &["feat_a"], |_| None::<&str>, alloc)
            .expect_err("ambiguous superset units must fail closed");
        assert_eq!(
            err_ambig,
            ClosureError::WasmRootMissing { name: "demo_pm".to_owned(), version: "0.1.0".to_owned() }
        );
    }

    #[test]
    fn streaming_files_digest_and_package_files_digest_match_exact_wire_format() {
        let dir = std::env::temp_dir().join(format!("blaze-module-streaming-digest-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/nested")).expect("mkdir src/nested");
        std::fs::create_dir_all(dir.join("target/ignored")).expect("mkdir target");
        std::fs::write(dir.join("Cargo.toml"), b"[package]\nname = \"demo\"\n").expect("write Cargo.toml");
        std::fs::write(dir.join("src/lib.rs"), b"pub fn a() -> u32 { 1 }\n").expect("write src/lib.rs");
        std::fs::write(dir.join("src/nested/mod.rs"), b"pub const X: &[u8] = b\"\x00\xff\";\n").expect("write nested");
        std::fs::write(dir.join("target/ignored/out.bin"), b"must be excluded").expect("write ignored");

        // Reference oracle using explicit `path || 0u8 || len_le64 || bytes` concatenation.
        let mut expected_entries = vec![
            ("Cargo.toml", &b"[package]\nname = \"demo\"\n"[..]),
            ("src/lib.rs", &b"pub fn a() -> u32 { 1 }\n"[..]),
            ("src/nested/mod.rs", &b"pub const X: &[u8] = b\"\x00\xff\";\n"[..]),
        ];
        expected_entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let mut wire = Vec::new();
        for (path, bytes) in &expected_entries {
            wire.extend_from_slice(path.as_bytes());
            wire.push(0);
            wire.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            wire.extend_from_slice(bytes);
        }
        let oracle = sha256_hex(&wire);

        // `files_digest` (unsorted input) must match oracle bit-for-bit.
        let mut unsorted: Vec<(String, Vec<u8>)> = expected_entries
            .iter()
            .rev()
            .map(|(p, b)| ((*p).to_owned(), (*b).to_vec()))
            .collect();
        assert_eq!(files_digest(&mut unsorted), oracle);

        // `package_files` + `files_digest` must match oracle.
        let mut collected = package_files(&dir).expect("package_files");
        assert_eq!(files_digest(&mut collected), oracle);

        // `FilesHasher::finish_hex_in` and `package_files_digest_in` must match oracle as well.
        let alloc = blaze_own::testing::heap().alloc();
        let mut hasher = FilesHasher::new();
        for (path, bytes) in &expected_entries {
            hasher.update_file(path, bytes);
        }
        assert_eq!(hasher.finish_hex_in(alloc).as_str(), oracle);
        assert_eq!(package_files_digest_in(&dir, alloc).expect("package_files_digest_in").as_str(), oracle);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn package_files_digest_in_orders_colliding_non_utf8_paths_canonically() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;
        let dir = std::env::temp_dir().join(format!("blaze-module-non-utf8-order-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Both filenames collapse to `bad_\u{FFFD}.rs` under `to_string_lossy()`, so raw byte
        // ordering (`0xfe < 0xff`) must distinguish and order them deterministically.
        std::fs::write(dir.join(OsStr::from_bytes(b"bad_\xff.rs")), b"ff-bytes").expect("write ff");
        std::fs::write(dir.join(OsStr::from_bytes(b"bad_\xfe.rs")), b"fe-bytes").expect("write fe");

        let mut wire = Vec::new();
        for (rel, bytes) in [(&b"bad_\xfe.rs"[..], &b"fe-bytes"[..]), (&b"bad_\xff.rs"[..], &b"ff-bytes"[..])] {
            wire.extend_from_slice(rel);
            wire.push(0);
            wire.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            wire.extend_from_slice(bytes);
        }
        let oracle = sha256_hex(&wire);
        let alloc = blaze_own::testing::heap().alloc();
        assert_eq!(
            package_files_digest_in(&dir, alloc).expect("digest").as_str(),
            oracle,
            "package_files_digest_in must sort and hash colliding non-UTF-8 names by raw relative bytes"
        );
        // Legacy UTF-8 `package_files` refuses non-UTF-8 names loudly instead of corrupting them lossily.
        assert!(package_files(&dir).is_err(), "package_files must refuse non-UTF-8 paths instead of lossy replacement");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunked_file_streaming_matches_update_file_bytes_and_detects_size_mismatch() {
        let payload: Vec<u8> = (0..200_000usize).map(|i| (i.wrapping_mul(31) & 0xff) as u8).collect();
        let mut expected = FilesHasher::new();
        expected.update_file_bytes(b"assets/large.bin", &payload);

        let mut streamed = FilesHasher::new();
        let mut small_chunk = [0u8; 4096];
        let mut cursor = std::io::Cursor::new(&payload);
        stream_file_into_hasher(&mut streamed, b"assets/large.bin", &mut cursor, payload.len() as u64, &mut small_chunk)
            .expect("chunked streaming must succeed when observed length matches expected_len");
        assert_eq!(streamed.finish(), expected.finish(), "chunked streaming digest must match update_file_bytes bit-for-bit");

        // Negative control 1: file grew beyond declared metadata length.
        let mut grew = FilesHasher::new();
        let mut cur_grew = std::io::Cursor::new(&payload);
        let err_grew = stream_file_into_hasher(&mut grew, b"assets/large.bin", &mut cur_grew, (payload.len() - 1) as u64, &mut small_chunk)
            .expect_err("file growth beyond expected_len must fail");
        assert_eq!(err_grew.kind(), ErrorKind::InvalidData);

        // Negative control 2: file shrank below declared metadata length.
        let mut shrank = FilesHasher::new();
        let mut cur_shrank = std::io::Cursor::new(&payload);
        let err_shrank = stream_file_into_hasher(&mut shrank, b"assets/large.bin", &mut cur_shrank, (payload.len() + 1) as u64, &mut small_chunk)
            .expect_err("file shrink below expected_len must fail");
        assert_eq!(err_shrank.kind(), ErrorKind::UnexpectedEof);
    }
}
