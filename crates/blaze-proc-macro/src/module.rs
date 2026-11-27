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

use std::path::{Path, PathBuf};

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
}

impl std::fmt::Display for ClosureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClosureError::NotLocked { name, version } => write!(f, "the lock has no package {name} {version}"),
            ClosureError::Unpinned { name, version } => {
                write!(f, "{name} {version} is a path package under a proc-macro crate: the lock pins nothing of its sources")
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

/// The digest a path package's sources are pinned by: SHA-256 over its files, in the byte
/// order of their paths relative to the package's folder, each as the path, a zero byte, its
/// length as 8 little-endian bytes and its bytes. `files` holds every regular file of the
/// package by the one package rule (blaze-build-script `served::in_package`); the builder and
/// blaze walk it alike (`examples/tempo_derives.rs` computes it from the checkout and finds the
/// builder's module by it).
pub fn files_digest(files: &mut [(String, Vec<u8>)]) -> String {
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut all = Vec::new();
    for (path, bytes) in files.iter() {
        all.extend_from_slice(path.as_bytes());
        all.push(0);
        all.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        all.extend_from_slice(bytes);
    }
    sha256_hex(&all)
}

/// The files of the package at `dir` that [`files_digest`] covers, by their paths relative to
/// it (`/`-separated): each entry `served::in_package` takes. Walked with a stack of folders.
pub fn package_files(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, ModuleError> {
    let io = |path: &Path, error: std::io::Error| ModuleError::Io { path: path.to_owned(), error };
    let mut out = Vec::new();
    let mut folders = vec![(dir.to_owned(), String::new())];
    while let Some((folder, relative)) = folders.pop() {
        for entry in std::fs::read_dir(&folder).map_err(|e| io(&folder, e))? {
            let entry = entry.map_err(|e| io(&folder, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let kind = entry.file_type().map_err(|e| io(&path, e))?;
            let place = if relative.is_empty() { InPackage::Root } else { InPackage::Beneath };
            if !in_package(entry.file_name().as_encoded_bytes(), kind.is_dir(), place) {
                continue;
            }
            let at = if relative.is_empty() { name.clone() } else { format!("{relative}/{name}") };
            if kind.is_dir() {
                folders.push((path, at));
            } else if kind.is_file() {
                out.push((at, std::fs::read(&path).map_err(|e| io(&path, e))?));
            }
        }
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
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

/// Current proc-macro module identity header (`blaze proc-macro module wasm64 9`): target-bearing
/// so on-demand `wasm64-blaze-wasip1` self-compiled modules never share a write-once key with
/// `wasm32-wasip1` bootstrap records (`blaze proc-macro module wasm32 7`) or pre-v9 records.
/// Form 9 invalidates pre-fix wasm64 modules compiled before `blaze-build` `ScopeEnd` /
/// `TempScopeEnd` adjusted-repr settling (fat pointers truncated from 16B to 8B before drops).
pub const MODULE_HEADER: &str = "blaze proc-macro module wasm64 9";

/// Folds the resolved `wasm64-blaze-wasip1` dependency closure's unified feature sets (bound to
/// each package's source pin — registry checksum/source, git commit+URL, or path files digest)
/// into a proc-macro crate's `deps` digest so enabling, removing, or swapping features across
/// dependencies (including same-name/same-version packages from different sources) invalidates
/// the module key (rule 21).
pub fn deps_with_wasm_features<S: AsRef<str>, D: AsRef<str>, A: core::alloc::Allocator + Copy>(
    lock_deps: &str,
    lock: &Lock<'_>,
    wasm_graph: &blaze_unit_graph::UnitGraph<'_>,
    package: blaze_manifest::lock::LockId,
    features: &[S],
    mut path_files: impl FnMut(blaze_manifest::lock::LockId) -> Option<D>,
    alloc: A,
) -> blaze_own::String<A> {
    use core::fmt::Write as _;
    let mut want_feats = blaze_own::Vec::with_capacity_in(features.len(), alloc);
    for f in features {
        want_feats.push(f.as_ref());
    }
    want_feats.sort_unstable();
    want_feats.dedup();
    let feats_match = |u: &blaze_unit_graph::Unit<'_>| -> bool {
        let mut got = blaze_own::Vec::with_capacity_in(u.features.len(), alloc);
        for f in u.features.iter() {
            got.push(f.as_str());
        }
        got.sort_unstable();
        got.dedup();
        got.as_slice() == want_feats.as_slice()
    };
    let root_idx = wasm_graph
        .units
        .iter()
        .position(|u| u.package == package && u.kind == blaze_unit_graph::UnitKind::ProcMacro && feats_match(u))
        .or_else(|| {
            wasm_graph
                .units
                .iter()
                .position(|u| u.package == package && u.kind == blaze_unit_graph::UnitKind::ProcMacro)
        });
    let mut rows = blaze_own::Vec::<blaze_own::String<A>, A>::new_in(alloc);
    if let Some(root_idx) = root_idx {
        let mut visited = blaze_own::Vec::with_capacity_in(wasm_graph.units.len(), alloc);
        visited.resize(wasm_graph.units.len(), false);
        if let Some(slot) = visited.get_mut(root_idx) {
            *slot = true;
        }
        let mut stack = blaze_own::Vec::new_in(alloc);
        if let Some(root_u) = wasm_graph.units.get(root_idx) {
            for dep in root_u.deps.iter() {
                let idx = dep.unit.index();
                if wasm_graph
                    .units
                    .get(idx)
                    .is_some_and(|du| matches!(du.kind, blaze_unit_graph::UnitKind::Lib | blaze_unit_graph::UnitKind::ProcMacro))
                    && let Some(slot) = visited.get_mut(idx)
                    && !*slot
                {
                    *slot = true;
                    stack.push(idx);
                }
            }
        }
        while let Some(idx) = stack.pop() {
            let Some(u) = wasm_graph.units.get(idx) else { continue };
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
                (Checksum::Absent, LockSource::Path) => match path_files(u.package) {
                    Some(hex) => {
                        let _ = write!(row, "files:{}", hex.as_ref());
                    }
                    None => row.push_str("path:unpinned"),
                },
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
    out
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
}

/// A store's folder of compiled module code, under the store (host.rs, "Compiled records").
pub const COMPILED_DIR: &str = "proc-macro/compiled";

impl ModuleRecord {
    /// The macro `name` of `kind`.
    pub fn find(&self, kind: ProcKind, name: &str) -> Option<&Exported> {
        self.macros.iter().find(|it| it.kind == kind && it.name == name)
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

/// Whether `store` holds `id`'s module, by its key: the native `.dylib`'s digest (zero Wasm!)
/// when present, else the wasm's digest as the record states it.
pub fn find_module(store: &Path, id: &CrateId<'_>) -> Result<(String, String), ModuleError> {
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

/// The record of `id` in `store` (prefers native `.dylib`/`.so` over `module.wasm` when present).
pub fn read_module(store: &Path, id: &CrateId<'_>) -> Result<ModuleRecord, ModuleError> {
    read_module_by_key(store, module_key(id), id.name, id.version)
}

/// The record for `key` (`name` `version`) in `store` (prefers native `.dylib`/`.so` over
/// `module.wasm` when present). Used both on initial registration ([`read_module`]) and when
/// a `MacroModule` task self-compiles and publishes the module later in the same command.
pub fn read_module_by_key(store: &Path, key: String, name: &str, version: &str) -> Result<ModuleRecord, ModuleError> {
    let dir = store.join("proc-macro/modules").join(&key);
    let native_path = dir.join(native_dylib_filename());
    let is_native = dir.join("dylib-sha256").is_file() && native_path.is_file();
    if !is_native && !dir.join("wasm-sha256").exists() {
        return Err(ModuleError::Missing { name: name.to_owned(), version: version.to_owned(), key });
    }
    let read = |file: &str| -> Result<Vec<u8>, ModuleError> {
        let path = dir.join(file);
        std::fs::read(&path).map_err(|error| ModuleError::Io { path, error })
    };
    let text = |file: &str| -> Result<String, ModuleError> { Ok(String::from_utf8_lossy(&read(file)?).into_owned()) };
    let held = text(if is_native { "dylib-sha256" } else { "wasm-sha256" })?.trim().to_owned();
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
    let decls = if is_native {
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
        native_dylib: is_native.then_some(native_path),
    })
}

#[cfg(test)]
#[path = "module_lock_tests.rs"]
mod lock_tests;

#[cfg(test)]
mod header_tests {
    use super::*;

    #[test]
    fn proc_macro_module_identity_uses_form_9_and_misses_stale_form_8_7_6_5_and_4_store_entries() {
        let id = CrateId {
            name: "demo_derive",
            version: "0.1.0",
            source: Source::Registry { checksum: "a0" },
            deps: "deps-hash",
            features: &["default"],
        };
        let v9_identity = identity(&id);
        assert_eq!(MODULE_HEADER, "blaze proc-macro module wasm64 9");
        assert_eq!(v9_identity.lines().next(), Some(MODULE_HEADER));
        let v8_w64_identity = v9_identity.replacen(MODULE_HEADER, "blaze proc-macro module wasm64 8", 1);
        let v7_w32_identity = v9_identity.replacen(MODULE_HEADER, "blaze proc-macro module wasm32 7", 1);
        let v7_identity = v9_identity.replacen(MODULE_HEADER, "blaze proc-macro module 7", 1);
        let v6_identity = v9_identity.replacen(MODULE_HEADER, "blaze proc-macro module 6", 1);
        let v5_identity = v9_identity.replacen(MODULE_HEADER, "blaze proc-macro module 5", 1);
        let v4_identity = v9_identity.replacen(MODULE_HEADER, "blaze proc-macro module 4", 1);
        let v8_w64_key = sha256_hex(v8_w64_identity.as_bytes());
        let v7_w32_key = sha256_hex(v7_w32_identity.as_bytes());
        let v7_key = sha256_hex(v7_identity.as_bytes());
        let v6_key = sha256_hex(v6_identity.as_bytes());
        let v5_key = sha256_hex(v5_identity.as_bytes());
        let v4_key = sha256_hex(v4_identity.as_bytes());
        let v9_key = module_key(&id);
        assert_ne!(v8_w64_key, v9_key, "wasm64 form 9 key must differ from stale wasm64 form 8 key");
        assert_ne!(v7_w32_key, v9_key, "wasm64 form 9 key must differ from wasm32 bootstrap form 7 key");
        assert_ne!(v7_key, v9_key, "form 9 key must differ from stale form 7 key");
        assert_ne!(v6_key, v9_key, "form 9 key must differ from stale form 6 key");
        assert_ne!(v5_key, v9_key, "form 9 key must differ from stale form 5 key");
        assert_ne!(v4_key, v9_key, "form 9 key must differ from stale form 4 key");

        let store = std::env::temp_dir().join(format!("blaze-pm-module-v9-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let write_entry = |dir: &Path, id_text: &str, digest: &str, macros: &str| {
            std::fs::create_dir_all(dir).expect("create module dir");
            for (name, body) in [
                ("identity", id_text),
                ("wasm-sha256", digest),
                ("macros", macros),
                ("decls", "__rustc_proc_macro_decls_blaze__\n"),
            ] {
                std::fs::write(dir.join(name), body).expect(name);
            }
        };
        for (stale_key, stale_identity, stale_digest) in [
            (&v4_key, &v4_identity, "deadbeef\n"),
            (&v5_key, &v5_identity, "feedface\n"),
            (&v6_key, &v6_identity, "badc0ffe\n"),
            (&v7_key, &v7_identity, "01234567\n"),
            (&v7_w32_key, &v7_w32_identity, "76543210\n"),
            (&v8_w64_key, &v8_w64_identity, "89abcdef\n"),
        ] {
            write_entry(&store.join("proc-macro/modules").join(stale_key), stale_identity, stale_digest, "");
        }

        // Lookup by current wasm64 form 9 misses stale wasm64 form 8, wasm32 form 7, and form 7..4 directories cleanly.
        assert!(matches!(find_module(&store, &id), Err(ModuleError::Missing { ref key, .. }) if key == &v9_key));
        assert!(matches!(read_module(&store, &id), Err(ModuleError::Missing { ref key, .. }) if key == &v9_key));

        // Publishing form 9 alongside stale form 8, 7, 6, 5, and 4 resolves uniquely to form 9.
        let v9_dir = store.join("proc-macro/modules").join(&v9_key);
        write_entry(&v9_dir, &v9_identity, "cafebabe\n", "0 derive Demo\n");

        let (found_key, digest) = find_module(&store, &id).expect("find v9 module");
        assert_eq!(found_key, v9_key);
        assert_eq!(digest, "cafebabe");
        let rec = read_module(&store, &id).expect("read v9 module");
        assert_eq!(rec.key, v9_key);
        assert_eq!(rec.wasm_sha256, "cafebabe");
        assert_eq!(rec.macros.len(), 1);
        assert_eq!(rec.macros[0].name, "Demo");

        let _ = std::fs::remove_dir_all(&store);
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
        let d_on = deps_with_wasm_features("same-lock-closure", &lock, &wg_on, LockId(0), &[] as &[&str], |_| None::<&str>, alloc);
        let d_off = deps_with_wasm_features("same-lock-closure", &lock, &wg_off, LockId(0), &[] as &[&str], |_| None::<&str>, alloc);
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
        let d_ab = deps_with_wasm_features("same-lock-closure", &lock, &wg_ab, LockId(0), &[] as &[&str], |_| None::<&str>, alloc);
        let d_ba = deps_with_wasm_features("same-lock-closure", &lock, &wg_ba, LockId(0), &[] as &[&str], |_| None::<&str>, alloc);
        assert_ne!(
            d_ab.as_str(),
            d_ba.as_str(),
            "swapping features A and B between two same-name/version packages from different sources must change the digest"
        );
    }
}
