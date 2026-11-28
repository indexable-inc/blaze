//! A unit graph's build scripts, run for the units they serve: each script's module record
//! found by its package and exact feature set, its environment made from the unit, its outputs
//! from a run in the sandbox ([`ScriptRuntime::run`]; the command's `Script` task record is the
//! only cache of a run, D115).
//!
//! This is the step a driver's build-script task takes, and what feeds the outputs' readers:
//! the world's cfg of a crate (blaze-world's `ScriptOutputs`, by the served unit) and
//! `blaze_build_script::cx::ScriptCx` for `env!("OUT_DIR")` and `include!`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use blaze_build_script::{
    Env, Outputs, OutputsRef,
    env::{DepMetadata, Given, PackageFacts},
    key::{CMode, RunLimits},
    native::Runner,
    probe::Row,
    toolchain::Mode,
};
use blaze_own::Alloc;
use blaze_unit_graph::{CfgAtom, Unit, UnitGraph, UnitId, UnitKind};

use crate::{
    native_unit::{NativeSetting, NativeUnitError, native_outputs, placed},
    script::{LoadedScript, ScriptError, ScriptRun, ScriptRuntime},
    script_sandbox::{
        Script, ScriptRoot, Tree,
        dep_mount::{self, DepProducer, DepViewError, DepViews},
        owned_env,
    },
};

/// What every script of one graph shares.
pub struct Setting<'s> {
    /// The store holding `build-script/modules` (the bootstrap's module records).
    pub store: &'s Path,
    pub target: &'s str,
    pub host: &'s str,
    /// The probe table's rows (`probe::pinned_rows`).
    pub table: Arc<Vec<Row>>,
    /// The probe checker and its records when the command's setting turns it on
    /// (script_probes.rs); `None`: the table answers the probes.
    pub probes: Option<crate::script_probes::ScriptProbes>,
    /// The C compiler probe setting (`probe_clang.rs`: `Ready` or deferred `Unavailable`).
    pub c_probes: Option<crate::probe_clang::CProbeSetting>,
    pub c_mode: CMode,
    /// What the command asked for: with each script's closure, where it runs (D69 (3),
    /// [`crate::native_unit::placed`]).
    pub mode: Mode,
    /// The pinned C toolchain and SDK native scripts run with; `None` where the command has
    /// none (a native script is then refused by name, never run as wasm).
    pub native: Option<NativeSetting<'s>>,
    /// What the command's setting says about spawns, chosen once per command
    /// (`crate::fact_pins::command_spawn_setting`): every script of the graph gets it.
    pub spawn_setting: Arc<crate::spawn_route::SpawnSetting>,
    /// For a workspace-member or external path package's build script: the `files <sha256>`
    /// digest of its build-script files (`script_files_digest`), matched in module lookup so
    /// edits invalidate and distinct path packages never collide (rule 21). `None` for
    /// registry/git packages whose sources are already pinned by the lock.
    pub files_digest: Option<&'s str>,
    /// When the build script has transitive `build-dependencies`, the `deps <sha256>` digest
    /// (`script_deps_digest`) covering every dependency's version, features, and source pin
    /// (checksum, git commit, or path files digest) so edits to build-dependencies invalidate
    /// cached modules (rule 21). `None` when the build script has no build-dependencies.
    pub deps_digest: Option<&'s str>,
}

/// A script ready to run: its loaded module, its sandboxed inputs, its environment.
pub struct Prepared<'a> {
    pub loaded: LoadedScript,
    pub script: Arc<Script>,
    pub env: Env<'a>,
}

/// Why a unit's script gave no outputs.
#[derive(Debug)]
pub enum UnitScriptError {
    /// The unit is not a build script.
    NotAScript { unit: UnitId },
    /// No unit names it as its build script.
    ServesNothing { package: String },
    /// No module record of the package with exactly these features.
    NoModule { package: String, features: Vec<String>, store: PathBuf },
    /// The module store could not answer the lookup.
    Lookup(ModuleLookupError),
    /// The module record's bytes could not be read.
    ReadModule { path: PathBuf, error: std::io::Error },
    /// The caller has no files for the package.
    NoFiles { package: String },
    /// The script's own failure: a load, a limit, a refusal, an exit, the records.
    Script(ScriptError),
    /// A direct dependency with `links` gave no outputs, so its metadata (`DEP_<LINKS>_*`)
    /// cannot be given: the script is not run without it.
    Dependency { package: String, dependency: String, links: String },
    /// The scripts' `links` inputs form a cycle through this script.
    LinksCycle { package: String },
    /// A native script's run (D65): its setting, binary, key, record or run.
    Native(NativeUnitError),
    /// A `links` dependency's metadata names a path the script cannot be served (brief S1).
    DepView { package: String, why: DepViewError },
    /// A script placed native (D112) asked to run for a side's cfg (`target`): the native
    /// runner builds for the machine, never for that side (ruling D115 (2), rule 3).
    NativeOnSide { package: String, target: String },
}

impl std::fmt::Display for UnitScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnitScriptError::NotAScript { unit } => write!(f, "unit {} is not a build script", unit.index()),
            UnitScriptError::ServesNothing { package } => write!(f, "the build script of {package} is no unit's build script"),
            UnitScriptError::NoModule { package, features, store } => write!(
                f,
                "no build-script module of {package} with features [{}] in {}/build-script/modules: build it with proc_macro_modules' build-script mode",
                features.join(","),
                store.display()
            ),
            UnitScriptError::Lookup(error) => write!(f, "{error}"),
            UnitScriptError::ReadModule { path, error } => write!(f, "{}: {error}", path.display()),
            UnitScriptError::NoFiles { package } => write!(f, "no files of {package} to run its build script over"),
            UnitScriptError::Script(error) => write!(f, "{error}"),
            UnitScriptError::Dependency { package, dependency, links } => write!(
                f,
                "the build script of {package} needs the metadata of {dependency} (`links = \"{links}\"`, its DEP_* variables), whose build script gave no outputs"
            ),
            UnitScriptError::LinksCycle { package } => write!(f, "the build script of {package} is in a cycle of `links` metadata"),
            UnitScriptError::Native(error) => write!(f, "{error}"),
            UnitScriptError::DepView { package, why } => write!(f, "the build script of {package}: {why}"),
            UnitScriptError::NativeOnSide { package, target } => write!(
                f,
                "the build script of {package} runs native (D112), and a native run builds for the machine, not for `{target}`: it is not run for that side (D115 (2))"
            ),
        }
    }
}

impl std::error::Error for UnitScriptError {}

/// The module record (`<store>/build-script/modules/<key>`) of `name version` compiled with
/// exactly `features`: the one whose identity names all three (and no `files` line, for a
/// lock-pinned package). A native script's record (its identity has a `target` line, D69) is
/// never it: that one holds `script.bin`, not a module. `Ok(None)`: the store holds no such
/// record (no `modules` folder at all included).
pub fn module_of(store: &Path, name: &str, version: &str, features: &[&str]) -> Result<Option<PathBuf>, ModuleLookupError> {
    module_where(store, name, version, features, None, None, None)
}

/// [`module_of`] for a package with optional `files <sha256>` identity line (workspace members
/// and external path packages pass `Some(hex)`; lock-pinned packages pass `None`).
pub fn module_of_files(store: &Path, name: &str, version: &str, features: &[&str], files: Option<&str>) -> Result<Option<PathBuf>, ModuleLookupError> {
    module_where(store, name, version, features, None, files, None)
}

/// [`module_of_files`] with optional `deps <sha256>` identity line covering the build script's
/// transitive build-dependencies (rule 21).
pub fn module_of_files_and_deps(
    store: &Path,
    name: &str,
    version: &str,
    features: &[&str],
    files: Option<&str>,
    deps: Option<&str>,
) -> Result<Option<PathBuf>, ModuleLookupError> {
    module_where(store, name, version, features, None, files, deps)
}

/// The native binary record (`<store>/build-script/modules/<key>`, holding `script.bin`) of
/// `name version` built for `host` with exactly `features` (D69: the interim builder's
/// `native` mode, whose identity adds `target <host>`).
pub fn module_of_native(store: &Path, name: &str, version: &str, features: &[&str], host: &str) -> Result<Option<PathBuf>, ModuleLookupError> {
    module_where(store, name, version, features, Some(host), None, None)
}

/// [`module_of_native`] with optional `files <sha256>` identity line for path packages.
/// Prefers an exact `files <sha256>` match when present; falls back to an untagged native
/// bootstrap record (`blaze build-script module native 1`, which `placed_in_store` selects via
/// `module_of_native`) when no `files`-tagged native record exists.
pub fn module_of_native_files(store: &Path, name: &str, version: &str, features: &[&str], host: &str, files: Option<&str>) -> Result<Option<PathBuf>, ModuleLookupError> {
    if let Some(found) = module_where(store, name, version, features, Some(host), files, None)? {
        return Ok(Some(found));
    }
    if files.is_some() {
        return module_where(store, name, version, features, Some(host), None, None);
    }
    Ok(None)
}

/// Failure allocating the scratch frame needed to walk a build script's dependency closure
/// (`script_deps_digest`). Never falls back to a weaker digest (rule 3, rule 21).
#[derive(Debug)]
pub struct DepsDigestError(pub bstd::mem::MemError);

impl core::fmt::Display for DepsDigestError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "allocating build-script dependency-digest scratch frame: {:?}", self.0)
    }
}

impl core::error::Error for DepsDigestError {}

/// Canonical SHA-256 digest of a build-script unit's transitive `UnitKind::Lib | UnitKind::ProcMacro`
/// build-dependencies (resolved against `wasm_graph` when present and matching the script's exact
/// host-feature side and feature set, so every WASI-side unified dependency feature and WASI-only
/// direct/transitive build-dep enters the module key without pulling in the package's normal or
/// dev dependencies from `Cargo.lock`, rules 21 & 60).
///
/// When `wasm_graph` is absent (e.g. native build scripts or minimal toolchains without
/// `wasm64-blaze-wasip1`) or does not contain an exact side-and-feature match for `script`, falls
/// back to supplementing host `graph` build-deps with lock-pinned (non-path) `Cargo.lock` metadata
/// so registry/git WASI-only deps still invalidate the digest while unkeyed normal/dev checkout
/// path files are never read into `UnitGraph`'s sealed `CrateList` (Rule 21: prevents `TwoValues`).
/// Returns `Ok(None)` when the build script has no build-dependencies (and, in fallback mode, no
/// lock dependencies).
pub fn script_deps_digest(
    graph: &UnitGraph<'_>,
    wasm_graph: Option<&UnitGraph<'_>>,
    lock: &blaze_manifest::lock::Lock<'_>,
    script: UnitId,
    path_files: impl FnMut(blaze_manifest::lock::LockId) -> Option<String>,) -> Result<Option<String>, DepsDigestError> {
    let frame = bstd::mem::Frame::new(bstd::mem::Tag::Other).map_err(DepsDigestError)?;
    Ok(script_deps_digest_with(graph, wasm_graph, lock, script, path_files, &frame)?.map(|s| s.as_str().to_owned()))
}

/// Like [`script_deps_digest`], accepting any `AsRef<str>` return from `path_files` (e.g. `&str`
/// from a caller-side memoization cache) and allocating the returned hex digest in `alloc` so
/// callers avoid global heap allocations on both cache hits and results.
pub fn script_deps_digest_with<S: AsRef<str>, A: core::alloc::Allocator>(
    graph: &UnitGraph<'_>,
    wasm_graph: Option<&UnitGraph<'_>>,
    lock: &blaze_manifest::lock::Lock<'_>,
    script: UnitId,
    mut path_files: impl FnMut(blaze_manifest::lock::LockId) -> Option<S>,
    alloc: A,
) -> Result<Option<blaze_own::String<A>>, DepsDigestError> {
    use blaze_manifest::lock::{Checksum, LockId, LockSource};
    use core::fmt::Write as _;
    let Some(root) = graph.units.get(script.index()) else { return Ok(None) };
    let frame = bstd::mem::Frame::new(bstd::mem::Tag::Other).map_err(DepsDigestError)?;

    // When the resolved `wasm64-blaze-wasip1` host-unit graph is available and contains an exact
    // side-and-feature match for `script`, walk its exact build-dependency closure so every
    // WASI-only dependency and workspace-unified WASI feature set enters the digest without
    // touching `Cargo.lock` normal/dev dependencies (rule 21: `UnitGraph`'s key only covers
    // build-deps, rule 60: normal `.rs` edits never invalidate `build.rs` modules). Any missing or
    // mismatched side/feature falls back to the host graph + conservative lock walk instead of
    // silently binding to another side's build-script unit.
    let (active_graph, active_root, resolved_wasm) = match wasm_graph {
        Some(wg) => {
            let want_side = graph.host_unit_side(script);
            let mut want_feats = blaze_own::Vec::with_capacity_in(root.features.len(), &frame);
            for f in root.features.iter() {
                want_feats.push(f.as_str());
            }
            want_feats.sort_unstable();
            want_feats.dedup();
            let feats_match = |u: &Unit<'_>| -> bool {
                let mut got = blaze_own::Vec::with_capacity_in(u.features.len(), &frame);
                for f in u.features.iter() {
                    got.push(f.as_str());
                }
                got.sort_unstable();
                got.dedup();
                got.as_slice() == want_feats.as_slice()
            };
            let exact = wg.units.iter().enumerate().find_map(|(i, u)| {
                let uid = UnitId(u32::try_from(i).ok()?);
                (u.package == root.package
                    && u.kind == UnitKind::BuildScript
                    && wg.host_unit_side(uid) == want_side
                    && feats_match(u))
                .then_some(uid)
            });
            match exact {
                Some(uid) => (wg, uid, true),
                None => (graph, script, false),
            }
        }
        None => (graph, script, false),
    };

    let active_root_unit = active_graph.units.get(active_root.index());
    if resolved_wasm && active_root_unit.is_none_or(|u| u.deps.is_empty()) {
        return Ok(None);
    }

    let mut visited = blaze_own::Vec::with_capacity_in(active_graph.units.len(), &frame);
    visited.resize(active_graph.units.len(), false);
    let mut stack = blaze_own::Vec::<UnitId, _>::new_in(&frame);
    let push_deps = |u: &Unit<'_>, visited: &mut blaze_own::Vec<bool, _>, stack: &mut blaze_own::Vec<UnitId, _>| {
        for dep in u.deps.iter() {
            let idx = dep.unit.index();
            if active_graph
                .units
                .get(idx)
                .is_some_and(|du| matches!(du.kind, UnitKind::Lib | UnitKind::ProcMacro))
                && let Some(slot) = visited.get_mut(idx)
                && !*slot
            {
                *slot = true;
                stack.push(dep.unit);
            }
        }
    };
    if let Some(slot) = visited.get_mut(active_root.index()) {
        *slot = true;
    }
    if let Some(active_root_unit) = active_root_unit {
        push_deps(active_root_unit, &mut visited, &mut stack);
    }

    // Memoize `path_files(pkg_id)` inside this call so each `LockId` is hashed at most once even
    // when multiple units in `active_graph` share a package.
    let mut path_cache: blaze_own::Vec<Option<Option<blaze_own::String<&bstd::mem::Frame>>>, _> =
        blaze_own::Vec::with_capacity_in(lock.packages.len(), &frame);
    for _ in 0..lock.packages.len() {
        path_cache.push(None);
    }
    let mut write_path_pin = |row: &mut blaze_own::String<&bstd::mem::Frame>, pkg_id: LockId| {
        let cached = if let Some(slot) = path_cache.get_mut(pkg_id.index()) {
            slot.get_or_insert_with(|| {
                path_files(pkg_id).map(|hex| blaze_own::String::from_str_in(hex.as_ref(), &frame))
            })
            .as_deref()
        } else {
            None
        };
        match cached {
            Some(hex) => {
                let _ = write!(row, "files:{hex}");
            }
            None => row.push_str("path:unpinned"),
        }
    };

    let mut lock_seen = blaze_own::Vec::with_capacity_in(if resolved_wasm { 0 } else { lock.packages.len() }, &frame);
    let mut lock_stack = blaze_own::Vec::<LockId, _>::new_in(&frame);
    if !resolved_wasm {
        lock_seen.resize(lock.packages.len(), false);
        // Without an exact `wasm_graph` match, seed the lock walk from `root.package` so WASI-only
        // registry/git build-deps (absent from the host `UnitGraph`) still enter the digest.
        // Path packages NOT in `active_graph` are NEVER hashed via `path_files` here because
        // `Cargo.lock` mixes normal/dev/build deps and `UnitGraph`'s key only sweeps build-deps
        // (rule 21).
        if let Some(slot) = lock_seen.get_mut(root.package.index()) {
            *slot = true;
        }
        for &child in lock.package(root.package).deps.iter() {
            if let Some(slot) = lock_seen.get_mut(child.index())
                && !*slot
            {
                *slot = true;
                lock_stack.push(child);
            }
        }
    }

    let mut lines = blaze_own::Vec::<blaze_own::String<&bstd::mem::Frame>, _>::new_in(&frame);
    while let Some(uid) = stack.pop() {
        let Some(u) = active_graph.units.get(uid.index()) else { continue };
        push_deps(u, &mut visited, &mut stack);
        if let Some(dep_bs) = u.build_script
            && let Some(slot) = visited.get_mut(dep_bs.index())
            && !*slot
        {
            *slot = true;
            if let Some(bs_unit) = active_graph.units.get(dep_bs.index()) {
                push_deps(bs_unit, &mut visited, &mut stack);
            }
        }
        if !resolved_wasm
            && let Some(slot) = lock_seen.get_mut(u.package.index())
            && !*slot
        {
            *slot = true;
            lock_stack.push(u.package);
        }
        let mut feats = blaze_own::Vec::with_capacity_in(u.features.len(), &frame);
        for f in u.features.iter() {
            feats.push(f.as_str());
        }
        feats.sort_unstable();
        feats.dedup();
        let kind_word = match u.kind {
            UnitKind::ProcMacro => "proc-macro",
            _ => "lib",
        };
        let pkg = lock.package(u.package);
        let mut row = blaze_own::String::new_in(&frame);
        let _ = write!(row, "{} {} {} ", u.name.as_str(), u.version.as_str(), kind_word);
        for (i, f) in feats.iter().enumerate() {
            if i > 0 {
                row.push(',');
            }
            row.push_str(f);
        }
        row.push(' ');
        match (&pkg.checksum, &pkg.source) {
            (Checksum::Sha256(sum), _) => {
                let _ = write!(row, "checksum:{} {}", sum.as_str(), pkg.source_text.as_str());
            }
            (Checksum::Absent, LockSource::Path) => write_path_pin(&mut row, u.package),
            (Checksum::Absent, LockSource::Git { commit, .. }) => {
                let _ = write!(row, "git:{} {}", commit.as_str(), pkg.source_text.as_str());
            }
            (Checksum::Absent, LockSource::Registry { .. }) => {
                let _ = write!(row, "source:{}", pkg.source_text.as_str());
            }
        }
        lines.push(row);
    }

    let mut lock_lines = blaze_own::Vec::<blaze_own::String<&bstd::mem::Frame>, _>::new_in(&frame);
    while let Some(pkg_id) = lock_stack.pop() {
        let pkg = lock.package(pkg_id);
        for &child in pkg.deps.iter() {
            if let Some(slot) = lock_seen.get_mut(child.index())
                && !*slot
            {
                *slot = true;
                lock_stack.push(child);
            }
        }
        let mut row = blaze_own::String::new_in(&frame);
        let _ = write!(row, "lock {} {} ", pkg.name.as_str(), pkg.version);
        match (&pkg.checksum, &pkg.source) {
            (Checksum::Sha256(sum), _) => {
                let _ = write!(row, "checksum:{}", sum.as_str());
            }
            // When `wasm_graph` was supplied for a wasm script but lacked an exact side-and-feature
            // match, pin lock-reachable path dependencies by their actual file contents so WASI-only
            // path build-dependencies absent from the host graph still invalidate the digest
            // (rule 21). When `wasm_graph` is `None` (native build scripts or minimal toolchains),
            // never read checkout files for lock-only path entries outside `active_graph` because
            // `Cargo.lock` mixes normal/dev deps not in `UnitGraph`'s key (prevents `TwoValues`).
            (Checksum::Absent, LockSource::Path) => {
                if wasm_graph.is_some() {
                    write_path_pin(&mut row, pkg_id);
                } else {
                    row.push_str("path:lock");
                }
            }
            (Checksum::Absent, LockSource::Git { commit, .. }) => {
                let _ = write!(row, "git:{}", commit.as_str());
            }
            (Checksum::Absent, LockSource::Registry { .. }) => {
                let _ = write!(row, "source:{}", pkg.source_text.as_str());
            }
        }
        lock_lines.push(row);
    }
    if lines.is_empty() && lock_lines.is_empty() {
        return Ok(None);
    }
    lines.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    lines.dedup_by(|a, b| a.as_str() == b.as_str());
    lock_lines.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    lock_lines.dedup_by(|a, b| a.as_str() == b.as_str());
    let mut joined = blaze_own::String::new_in(&frame);
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            joined.push('\n');
        }
        joined.push_str(l.as_str());
    }
    for l in lock_lines.iter() {
        if !joined.is_empty() {
            joined.push('\n');
        }
        joined.push_str(l.as_str());
    }
    let digest = blaze_sha256::digest(joined.as_str().as_bytes());
    let mut out = blaze_own::String::with_capacity_in(digest.len() * 2, alloc);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    Ok(Some(out))
}

/// Relative paths under a package directory that contribute to a workspace-member or path
/// package's build script (`Cargo.toml`, `script_root`, top-level `*.rs`, and any `*.rs` under
/// `script_root`'s parent directory when that directory is neither `""` nor `"src"`). Never
/// descends into `src/`, `tests/`, `benches/`, or `examples/`, so ordinary edits to library or
/// binary sources do not recompile `build.rs` (rule 60) while edits to `build.rs`, its helper
/// modules, or `Cargo.toml` immediately invalidate the build-script module key (rule 21).
pub fn script_files_digest(pkg_dir: &Path, script_root: &str) -> Result<String, std::io::Error> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut add_path = |rel: &str| {
        let norm = rel.trim_start_matches("./");
        if !norm.is_empty() {
            seen.insert(norm.to_owned());
        }
    };
    add_path("Cargo.toml");
    add_path(script_root);
    if let Ok(entries) = std::fs::read_dir(pkg_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if let Some(s) = name.to_str()
                && s.ends_with(".rs")
                && entry.file_type().is_ok_and(|t| t.is_file())
            {
                add_path(s);
            }
        }
    }
    let script_rel = script_root.trim_start_matches("./");
    if let Some((parent, _)) = script_rel.rsplit_once('/')
        && !parent.is_empty()
        && parent != "src"
        && !parent.starts_with("src/")
    {
        let mut stack = vec![parent.to_owned()];
        while let Some(dir_rel) = stack.pop() {
            let full_dir = pkg_dir.join(&dir_rel);
            let Ok(entries) = std::fs::read_dir(&full_dir) else { continue };
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                let child = format!("{dir_rel}/{name}");
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    stack.push(child);
                } else if ft.is_file() && name.ends_with(".rs") {
                    add_path(&child);
                }
            }
        }
    }
    // `BTreeSet<String>` iterates in ascending byte order of relative path.
    let mut hasher = crate::module::FilesHasher::new();
    for norm in seen.iter() {
        let full = pkg_dir.join(norm);
        if full.is_file() {
            let bytes = std::fs::read(&full)?;
            hasher.update_file(norm.as_str(), bytes.as_slice());
        }
    }
    Ok(crate::module::hex(&hasher.finish()))
}

/// Why the module store could not answer a lookup: each names the path (rule 19), and none is
/// taken as "no record" (rule 3).
#[derive(Debug)]
pub enum ModuleLookupError {
    /// The store's `build-script/modules` folder could not be listed (other than absent).
    List { folder: PathBuf, error: std::io::Error },
    /// A record's `identity` could not be read: a record without one is not a record.
    Identity { path: PathBuf, error: std::io::Error },
    /// Two records answer one ask (a toolchain or closure each, same name, version, features
    /// and runner): which is meant is not decided by the store's listing order (rules 21, 25).
    Ambiguous { first: PathBuf, second: PathBuf },
}

impl std::fmt::Display for ModuleLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModuleLookupError::List { folder, error } => write!(f, "{}: the module store could not be listed: {error}", folder.display()),
            ModuleLookupError::Identity { path, error } => write!(f, "{}: a module record's identity could not be read: {error}", path.display()),
            ModuleLookupError::Ambiguous { first, second } => write!(
                f,
                "{} and {}: two module records answer one ask (name, version, features, runner); remove the stale one",
                first.display(),
                second.display()
            ),
        }
    }
}

impl std::error::Error for ModuleLookupError {}

/// Each record of the store as (its folder, its identity's text), sorted by folder; none when
/// the store has no `modules` folder.
///
/// A listing and one read a record: the interim builder names a record by the SHA-256 of its
/// whole identity (scripts proc_macro_modules build_scripts.rs: origin, the lock's closure,
/// toolchain, shim), which an ask (name, version, features, runner) cannot compute, so the
/// folder cannot be joined from the ask until the builder also writes an ask-named pointer.
fn records(store: &Path) -> Result<Vec<(PathBuf, String)>, ModuleLookupError> {
    let folder = store.join("build-script/modules");
    let entries = match std::fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(ModuleLookupError::List { folder, error }),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| ModuleLookupError::List { folder: folder.clone(), error })?;
        // Skip dot-prefixed entries (staging directories like `.tmp.*` or OS metadata like `.DS_Store`)
        // and non-directory files so concurrent publishers never race with a store scan.
        if entry.file_name().to_str().is_some_and(|n| n.starts_with('.')) {
            continue;
        }
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path().join("identity");
        let identity = std::fs::read_to_string(&path).map_err(|error| ModuleLookupError::Identity { path, error })?;
        found.push((entry.path(), identity));
    }
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Every record of the store whose identity names `name` and `version`, whatever its features
/// and runner: (its folder, its identity's text), sorted by folder. What a refusal of a lookup
/// shows beside the asked identity (rule 19): which line of the nearest record differs.
pub fn records_named(store: &Path, name: &str, version: &str) -> Result<Vec<(PathBuf, String)>, ModuleLookupError> {
    let (name, version) = (format!("name {name}"), format!("version {version}"));
    let mut found = records(store)?;
    found.retain(|(_, identity)| identity.lines().any(|l| l == name) && identity.lines().any(|l| l == version));
    Ok(found)
}

/// The process shim this binary's build-script host answers (guest/script_process.rs, as built
/// into it): the bootstrap builder writes `shim <its SHA-256>` into a wasm module's identity
/// (scripts proc_macro_modules build_scripts.rs `library`), the same digest of the same file.
const SHIM: &[u8] = include_bytes!("../guest/script_process.rs");

/// Current wasm64 build-script module identity header (`blaze build-script module wasm64 8`):
/// target-bearing so on-demand `wasm64-blaze-wasip1` self-compiled modules never collide with
/// `wasm32-wasip1` bootstrap records (`blaze build-script module wasm32 5`) or stale pre-v8
/// modules. v8 enables target-level `getrandom_backend="custom"` for `wasm64-blaze-wasip1`
/// (Rule 21), invalidating pre-v8 wasm64 build-script modules.
pub const WASM_MODULE_HEADER: &str = "blaze build-script module wasm64 8";

/// Current native host build-script module identity header (`blaze build-script module 3`): kept
/// at v3 so existing Tier-2 native host records remain usable without re-bootstrapping while
/// still selecting one version cleanly (no `Ambiguous` collisions).
pub const NATIVE_MODULE_HEADER: &str = "blaze build-script module 3";

/// Native host build-script module identity header written by the native bootstrap builder
/// (`blaze build-script module native 1`, D69). Accepted by native lookups alongside
/// [`NATIVE_MODULE_HEADER`] so pre-built Tier-2 native host binaries (`reth-mdbx-sys`,
/// `sha3-asm`, `libproc`, `librocksdb-sys`, etc.) resolve cleanly without re-bootstrapping.
pub const NATIVE_BOOTSTRAP_HEADER: &str = "blaze build-script module native 1";

/// The identity line a wasm module must carry to be run by this host: `shim <sha256 hex>`.
pub fn shim_line() -> String {
    format!("shim {}", crate::module::sha256_hex(SHIM))
}

/// The record whose header matches [`NATIVE_MODULE_HEADER`] (for `host: Some`) or
/// [`WASM_MODULE_HEADER`] (for `host: None`), and whose identity names `name`, `version`,
/// `features` and, for `host`, `target <host>`; for `None`, one with no `target` line and this
/// host's [`shim_line`] (a wasm module). Two such records are a typed refusal, never the first in
/// the listing's order.
fn module_where(
    store: &Path,
    name: &str,
    version: &str,
    features: &[&str],
    host: Option<&str>,
    files: Option<&str>,
    deps: Option<&str>,
) -> Result<Option<PathBuf>, ModuleLookupError> {
    let mut sorted = features.to_vec();
    sorted.sort_unstable();
    let lines = [format!("name {name}"), format!("version {version}"), format!("features {}", sorted.join(","))];
    let target = host.map(|host| format!("target {host}"));
    let expected_header = if target.is_some() { NATIVE_MODULE_HEADER } else { WASM_MODULE_HEADER };
    let files_line = files.map(|hex| format!("files {hex}"));
    let deps_line = deps.map(|hex| format!("deps {hex}"));
    let shim = shim_line();
    let mut fits = records(store)?.into_iter().filter(|(_, identity)| {
        // Every build-script record must carry the expected header for its runner (`WASM_MODULE_HEADER`
        // for wasm, `NATIVE_MODULE_HEADER` or `NATIVE_BOOTSTRAP_HEADER` for native), and a wasm
        // module must also carry the shim this host speaks to: one built with another header or
        // shim is not this ask's record, so stale records never match and never collide as `Ambiguous`.
        let first_line = identity.lines().next();
        let header_fits = if target.is_some() {
            first_line == Some(NATIVE_MODULE_HEADER) || first_line == Some(NATIVE_BOOTSTRAP_HEADER)
        } else {
            first_line == Some(expected_header)
        };
        let runner_fits = match &target {
            Some(want) => identity.lines().any(|l| l == want),
            None => !identity.lines().any(|l| l.starts_with("target ")) && identity.lines().any(|l| l == shim),
        };
        let files_fits = match &files_line {
            Some(want) => identity.lines().any(|l| l == want),
            None => !identity.lines().any(|l| l.starts_with("files ")),
        };
        let deps_fits = match &deps_line {
            Some(want) => identity.lines().any(|l| l == want),
            None => !identity.lines().any(|l| l.starts_with("deps ")),
        };
        header_fits && runner_fits && files_fits && deps_fits && lines.iter().all(|want| identity.lines().any(|l| l == want))
    });
    let Some((first, _)) = fits.next() else { return Ok(None) };
    match fits.next() {
        Some((second, _)) => Err(ModuleLookupError::Ambiguous { first, second }),
        None => Ok(Some(first)),
    }
}

/// The unit build-script unit `script` serves: the library (or proc macro) whose
/// `build_script` it is, else the first such unit.
pub fn served(graph: &UnitGraph<'_>, script: UnitId) -> Option<UnitId> {
    let mut first = None;
    for (at, unit) in graph.units.iter().enumerate() {
        if unit.build_script != Some(script) {
            continue;
        }
        let id = UnitId(at as u32);
        if matches!(unit.kind, UnitKind::Lib | UnitKind::ProcMacro) {
            return Some(id);
        }
        first.get_or_insert(id);
    }
    first
}

fn package(unit: &Unit<'_>) -> String {
    format!("{} {}", unit.name.as_str(), unit.version.as_str())
}

/// `script`'s module loaded and its inputs made: `tree` and `root` are its package's files
/// and what it sees (`script_sandbox::dir_tree` of the workspace with `ScriptRoot::member(dir)`
/// for a member, `mem_tree` with `ScriptRoot::package()` for a fetched package; D96). `deps` is
/// the `links` metadata of the served unit's
/// direct dependencies ([`dep_metadata`] of each of [`links_inputs`]); it is in the
/// environment, so in the run's key.
#[allow(clippy::too_many_arguments)]
pub fn prepare<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    tree: Tree,
    root: ScriptRoot,
    deps: &[DepMetadata<'_>],
    alloc: Alloc<'a>,
) -> Result<Prepared<'a>, UnitScriptError> {
    prepare_viewing(runtime, setting, graph, script, tree, root, deps, DepViews::none(), alloc)
}

/// [`prepare`] with `views`, the read-only mounts of the script's `links` dependencies
/// ([`dep_mount::view`], brief S1); `deps` are then the values as the script sees them
/// ([`dep_mount::Viewed::values`]).
#[allow(clippy::too_many_arguments)]
pub fn prepare_viewing<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    tree: Tree,
    root: ScriptRoot,
    deps: &[DepMetadata<'_>],
    views: DepViews,
    alloc: Alloc<'a>,
) -> Result<Prepared<'a>, UnitScriptError> {
    prepare_viewing_on(runtime, setting, graph, script, None, tree, root, deps, views, alloc)
}

/// [`prepare_viewing`] for the side whose cfg is `side_cfg` (sorted; `CARGO_CFG_*` and the
/// `OUT_DIR` configuration are made from it): the cfg of the crate of the list the run is for
/// (ruling D115 (2): a macro-side run sees wasm32-wasip1's atoms, with `setting.target` that
/// triple). `None`: the served unit's cfg as the graph holds it.
#[allow(clippy::too_many_arguments)]
pub fn prepare_viewing_on<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    side_cfg: Option<&[CfgAtom<'_>]>,
    tree: Tree,
    root: ScriptRoot,
    deps: &[DepMetadata<'_>],
    views: DepViews,
    alloc: Alloc<'a>,
) -> Result<Prepared<'a>, UnitScriptError> {
    let unit = graph.units.get(script.index()).ok_or(UnitScriptError::NotAScript { unit: script })?;
    if unit.kind != UnitKind::BuildScript {
        return Err(UnitScriptError::NotAScript { unit: script });
    }
    let package = package(unit);
    let serves = served(graph, script).ok_or_else(|| UnitScriptError::ServesNothing { package: package.clone() })?;
    let serves = &graph.units[serves.index()];
    let features: Vec<&str> = unit.features.iter().map(|f| f.as_str()).collect();
    let module = module_of_files_and_deps(
        setting.store,
        unit.name.as_str(),
        unit.version.as_str(),
        &features,
        setting.files_digest,
        setting.deps_digest,
    )
    .map_err(UnitScriptError::Lookup)?
    .ok_or_else(|| UnitScriptError::NoModule {
        package: package.clone(),
        features: features.iter().map(|f| (*f).to_owned()).collect(),
        store: setting.store.to_owned(),
    })?;
    let path = module.join("module.wasm");
    let wasm = std::fs::read(&path).map_err(|error| UnitScriptError::ReadModule { path, error })?;
    let loaded = runtime.load(&package, &wasm).map_err(UnitScriptError::Script)?;
    let facts = PackageFacts {
        name: unit.name.as_str(),
        version: unit.version.as_str(),
        meta: graph.meta_of(unit.package).map(|meta| meta.vars()),
        links: unit.links.as_ref().map(|l| l.as_str()),
    };
    let target_cfg: &[CfgAtom<'_>] = match side_cfg {
        Some(cfg) => cfg,
        None => &serves.cfg,
    };
    let given = Given { package: facts, seen: root.seen(), features: &unit.features, target_cfg, target: setting.target, host: setting.host, deps };
    let env = blaze_build_script::env(&given, alloc);
    let script = Arc::new(Script {
        package,
        tree,
        root,
        env: owned_env(&env, alloc),
        target: setting.target.to_owned(),
        host: setting.host.to_owned(),
        c_mode: setting.c_mode,
        limits: RunLimits::STANDARD,
        table: Arc::clone(&setting.table),
        probes: setting.probes.as_ref().map(blaze_own::Share::share),
        c_probes: setting.c_probes.as_ref().map(blaze_own::Share::share),
        spawner: None,
        step: blaze_ids::ContentId::default(),
        spawn_setting: Arc::clone(&setting.spawn_setting),
        deps: Arc::new(views),
    });
    Ok(Prepared { loaded, script, env })
}

/// `script`'s outputs, run as wasm, or native when [`placed`] says so for the setting's mode
/// (D65, D69). The choice is made here, before
/// anything runs, and is in the run's key; a failure either way is returned, never tried the
/// other way (rule 3).
#[allow(clippy::too_many_arguments)]
pub fn outputs<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    tree: Tree,
    root: ScriptRoot,
    deps: &[DepMetadata<'_>],
    alloc: Alloc<'a>,
) -> Result<ScriptRun<'a>, UnitScriptError> {
    let unit = graph.units.get(script.index()).ok_or(UnitScriptError::NotAScript { unit: script })?;
    if unit.kind != UnitKind::BuildScript {
        return Err(UnitScriptError::NotAScript { unit: script });
    }
    match crate::native_unit::placed_in_store(setting.store, graph, script, setting.mode, setting.host, alloc) {
        Runner::Wasm => {
            let prepared = prepare(runtime, setting, graph, script, tree, root, deps, alloc)?;
            runtime.run(&prepared.loaded, prepared.script, &prepared.env, alloc).map_err(UnitScriptError::Script)
        }
        Runner::Native { because } => {
            let serves = served(graph, script).ok_or_else(|| UnitScriptError::ServesNothing { package: package(unit) })?;
            // No producers given: a `links` value is refused by name (brief N8), never passed as written.
            native_outputs(setting, graph, script, serves, because, tree, root, deps, &[], alloc).map_err(UnitScriptError::Native)
        }
    }
}

/// [`outputs`] for a script whose `links` dependencies' runs are `producers` (each with its
/// `links`), so the paths in `deps` are served (brief S1): a wasm script is given the values
/// as it sees them and the producers' files read-only ([`dep_mount::view`]); a native script
/// (D112) the same values as IT sees them, the producers' trees laid read-only under its own
/// folder ([`dep_mount::native::lay`], brief N8): one resolver, two sights.
#[allow(clippy::too_many_arguments)]
pub fn outputs_viewing<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    tree: Tree,
    root: ScriptRoot,
    deps: &[DepMetadata<'_>],
    producers: &[(&str, &DepProducer)],
    alloc: Alloc<'a>,
) -> Result<ScriptRun<'a>, UnitScriptError> {
    outputs_viewing_on(runtime, setting, graph, script, None, tree, root, deps, producers, alloc)
}

/// [`outputs_viewing`] for the side whose cfg is `side_cfg` ([`prepare_viewing_on`]). A script
/// placed native is refused when given a side's cfg ([`UnitScriptError::NativeOnSide`]): the
/// native runner builds for the machine. `None`: as [`outputs_viewing`].
#[allow(clippy::too_many_arguments)]
pub fn outputs_viewing_on<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    side_cfg: Option<&[CfgAtom<'_>]>,
    tree: Tree,
    root: ScriptRoot,
    deps: &[DepMetadata<'_>],
    producers: &[(&str, &DepProducer)],
    alloc: Alloc<'a>,
) -> Result<ScriptRun<'a>, UnitScriptError> {
    let unit = graph.units.get(script.index()).ok_or(UnitScriptError::NotAScript { unit: script })?;
    if unit.kind != UnitKind::BuildScript {
        return Err(UnitScriptError::NotAScript { unit: script });
    }
    match crate::native_unit::placed_in_store(setting.store, graph, script, setting.mode, setting.host, alloc) {
        Runner::Wasm => {
            let file_area = setting.store.join(blaze_build_script::outputs::FILE_AREA);
            let viewed = dep_mount::view_with_area(&tree, Some(&file_area), producers, deps, alloc)
                .map_err(|why| UnitScriptError::DepView { package: package(unit), why })?;
            let seen: Vec<DepMetadata<'_>> =
                deps.iter().zip(viewed.values.iter()).map(|(dep, value)| DepMetadata { links: dep.links, key: dep.key, value: value.as_str() }).collect();
            let prepared = prepare_viewing_on(runtime, setting, graph, script, side_cfg, tree, root, &seen, viewed.views, alloc)?;
            runtime.run(&prepared.loaded, prepared.script, &prepared.env, alloc).map_err(UnitScriptError::Script)
        }
        Runner::Native { .. } if side_cfg.is_some() && setting.target != setting.host => {
            Err(UnitScriptError::NativeOnSide { package: package(unit), target: setting.target.to_owned() })
        }
        Runner::Native { because } => {
            let serves = served(graph, script).ok_or_else(|| UnitScriptError::ServesNothing { package: package(unit) })?;
            native_outputs(setting, graph, script, serves, because, tree, root, deps, producers, alloc).map_err(UnitScriptError::Native)
        }
    }
}

/// One run a reading script is served of: a script of its `links` closure ([`links_closure`]),
/// handed as its outputs RECORD (read in place), with the tree its package was served and the
/// root it saw it at. The command hands the `Script` records the drive read for it; an example
/// hands the records of the runs it made; both through [`outputs_given`], the one function that
/// turns them into the reader's `DEP_<LINKS>_*` and what its run is served (a wasm run's mounts,
/// a native run's laid folders), so the two never drift (rule 30).
pub struct GivenRun<'r> {
    pub script: UnitId,
    /// The `links` of the unit it serves.
    pub links: &'r str,
    pub outputs: OutputsRef<'r>,
    pub tree: Tree,
    pub root: ScriptRoot,
    /// A `links` input of the reader itself (its metadata is the reader's `DEP_<LINKS>_*`), not
    /// only upstream of one (whose files serve a forwarded path, brief S1b).
    pub direct: bool,
}

/// What a reading script is given of its closure's runs ([`served_links`]): its `DEP_<LINKS>_*`
/// pairs as the producers' records hold them, and each direct producer, with its own closure
/// upstream, by its `links`.
pub struct ServedLinks<'g> {
    pub deps: Vec<DepMetadata<'g>>,
    pub producers: Vec<(&'g str, DepProducer)>,
}

/// The scripts whose runs `script` is served of: each of its [`links_inputs`] and, walked from
/// those, each of theirs (the transitive closure brief S1b serves a forwarded path from), with
/// the `links` each serves and whether it is a direct input. A work list, no recursion; sorted
/// by unit, each once (a script reached directly and upstream is direct).
pub fn links_closure<'g>(graph: &'g UnitGraph<'_>, script: UnitId) -> Vec<(UnitId, &'g str, bool)> {
    let mut found: Vec<(UnitId, &'g str, bool)> = Vec::new();
    let mut stack: Vec<(UnitId, &'g str, bool)> = links_inputs(graph, script).into_iter().map(|(unit, links)| (unit, links, true)).collect();
    while let Some((unit, links, direct)) = stack.pop() {
        match found.iter_mut().find(|(seen, _, _)| *seen == unit) {
            Some(seen) => seen.2 |= direct,
            None => {
                found.push((unit, links, direct));
                stack.extend(links_inputs(graph, unit).into_iter().map(|(up, links)| (up, links, false)));
            }
        }
    }
    found.sort_unstable_by_key(|(unit, _, _)| *unit);
    found
}

/// The `links` metadata and producers build-script unit `script` is served from `given`, the
/// runs of its [`links_closure`]: each producer made from its record upstream first, so each
/// takes its own inputs' closures ([`DepProducer::closure`], one flatten); the metadata of each
/// direct one is a pair of the reader's environment. A direct `links` input of `script` that
/// `given` lacks is refused naming it ([`UnitScriptError::Dependency`]): a script is never run
/// without a `DEP_<LINKS>_*` cargo gives it (rule 3).
pub fn served_links<'g, 'r: 'g>(graph: &UnitGraph<'_>, script: UnitId, given: &'g [GivenRun<'r>]) -> Result<ServedLinks<'g>, UnitScriptError> {
    let unit = graph.units.get(script.index()).ok_or(UnitScriptError::NotAScript { unit: script })?;
    for (input, links) in links_inputs(graph, script) {
        if !given.iter().any(|run| run.script == input && run.direct) {
            return Err(UnitScriptError::Dependency { package: package(unit), dependency: package(&graph.units[input.index()]), links: links.to_owned() });
        }
    }
    let mut ids: Vec<UnitId> = given.iter().map(|run| run.script).collect();
    ids.sort_unstable();
    ids.dedup();
    let inputs: Vec<Vec<UnitId>> = ids.iter().map(|id| links_inputs(graph, *id).into_iter().map(|(up, _)| up).collect()).collect();
    let order = order_after(&ids, &inputs).map_err(|_| UnitScriptError::LinksCycle { package: package(unit) })?;
    let mut deps: Vec<DepMetadata<'g>> = Vec::new();
    let mut made: Vec<(UnitId, DepProducer)> = Vec::with_capacity(order.len());
    for id in order {
        let Some(run) = given.iter().find(|run| run.script == id) else { continue };
        if run.direct {
            deps.extend(run.outputs.metadata().map(|(key, value)| DepMetadata { links: run.links, key, value }));
        }
        let ups: Vec<(&str, &DepProducer)> =
            links_inputs(graph, id).into_iter().filter_map(|(up, links)| made.iter().find(|(s, _)| *s == up).map(|(_, producer)| (links, producer))).collect();
        let upstream = DepProducer::closure(&ups);
        made.push((id, DepProducer::of_record(blaze_own::Share::share(&run.tree), blaze_own::Share::share(&run.root), &run.outputs).with_upstream(upstream)));
    }
    let producers = given
        .iter()
        .filter(|run| run.direct)
        .filter_map(|run| made.iter().find(|(s, _)| *s == run.script).map(|(_, producer)| (run.links, producer.clone())))
        .collect();
    Ok(ServedLinks { deps, producers })
}

/// `script`'s outputs for the side whose cfg is `side_cfg` ([`outputs_viewing_on`]), served the
/// runs `given` of its `links` closure ([`served_links`]): THE run a command's `Script` task and
/// the census's exporter both make, wasm or native as [`placed`] says, the producers' files
/// served to either alike.
#[allow(clippy::too_many_arguments)]
pub fn outputs_given<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    side_cfg: Option<&[CfgAtom<'_>]>,
    tree: Tree,
    root: ScriptRoot,
    given: &[GivenRun<'_>],
    alloc: Alloc<'a>,
) -> Result<ScriptRun<'a>, UnitScriptError> {
    let served = served_links(graph, script, given)?;
    let producers: Vec<(&str, &DepProducer)> = served.producers.iter().map(|(links, producer)| (*links, producer)).collect();
    outputs_viewing_on(runtime, setting, graph, script, side_cfg, tree, root, &served.deps, &producers, alloc)
}

/// The build-script units whose metadata `script` is given: for each direct dependency of the
/// unit it serves that has `links` and a build script, that script, with the dependency's
/// `links` (cargo: a package's `cargo::metadata` reaches the scripts of the packages that
/// depend on it directly, as `DEP_<LINKS>_<KEY>`). Sorted by unit, each once.
pub fn links_inputs<'g>(graph: &'g UnitGraph<'_>, script: UnitId) -> Vec<(UnitId, &'g str)> {
    let Some(serves) = served(graph, script) else { return Vec::new() };
    let mut inputs: Vec<(UnitId, &'g str)> = graph.units[serves.index()]
        .deps
        .iter()
        .filter_map(|dep| {
            let unit = &graph.units[dep.unit.index()];
            Some((unit.build_script?, unit.links.as_ref()?.as_str()))
        })
        .collect();
    inputs.sort_unstable();
    inputs.dedup();
    inputs
}

/// The build-script units of `graph` in an order where each comes after every script of its
/// [`links_inputs`]: a work list over those edges (Kahn's), ties by unit id, no recursion.
/// A script left over is in a cycle, refused by name.
pub fn links_order(graph: &UnitGraph<'_>) -> Result<Vec<UnitId>, UnitScriptError> {
    let scripts: Vec<UnitId> = (0..graph.units.len() as u32).map(UnitId).filter(|id| graph.units[id.index()].kind == UnitKind::BuildScript).collect();
    let inputs: Vec<Vec<UnitId>> = scripts.iter().map(|s| links_inputs(graph, *s).into_iter().map(|(u, _)| u).collect()).collect();
    order_after(&scripts, &inputs).map_err(|at| UnitScriptError::LinksCycle { package: package(&graph.units[scripts[at].index()]) })
}

/// `items` ordered so each comes after its `inputs` (those of them among `items`; others are
/// not waited on), ties in the order given. `Err` names the index of an item in a cycle.
fn order_after(items: &[UnitId], inputs: &[Vec<UnitId>]) -> Result<Vec<UnitId>, usize> {
    let at = |id: UnitId| items.binary_search(&id).ok();
    let mut waiting: Vec<usize> = inputs.iter().map(|i| i.iter().filter(|u| at(**u).is_some()).count()).collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); items.len()];
    for (i, ins) in inputs.iter().enumerate() {
        for d in ins.iter().filter_map(|u| at(*u)) {
            dependents[d].push(i);
        }
    }
    let mut ready: std::collections::BTreeSet<usize> = (0..items.len()).filter(|i| waiting[*i] == 0).collect();
    let mut order = Vec::with_capacity(items.len());
    while let Some(i) = ready.pop_first() {
        order.push(items[i]);
        for &d in &dependents[i] {
            waiting[d] -= 1;
            if waiting[d] == 0 {
                ready.insert(d);
            }
        }
    }
    match waiting.iter().position(|w| *w != 0) {
        Some(stuck) => Err(stuck),
        None => Ok(order),
    }
}

/// A dependency's `links` metadata as its dependents' scripts take it: each `cargo::metadata`
/// (or old `cargo:K=V`) pair of its outputs, under its `links`.
pub fn dep_metadata<'o>(links: &'o str, outputs: &'o Outputs<'_>) -> impl Iterator<Item = DepMetadata<'o>> {
    outputs.metadata.iter().map(move |(key, value)| DepMetadata { links, key: key.as_str(), value: value.as_str() })
}

/// Every build-script unit of `graph` run for the unit it serves, in [`links_order`], keyed by
/// that unit: what a reader of the outputs by library takes (blaze-world's `ScriptOutputs`).
/// `files` gives a package's tree and what its script sees (D96), or `None`. A script whose `links` input gave no
/// outputs is not run: it fails naming that dependency.
pub fn outputs_by_served<'a>(
    runtime: &ScriptRuntime,
    setting: &Setting<'_>,
    graph: &UnitGraph<'_>,
    mut files: impl FnMut(&Unit<'_>) -> Option<(Tree, ScriptRoot)>,
    alloc: Alloc<'a>,
) -> (Vec<(UnitId, ScriptRun<'a>)>, Vec<(UnitId, UnitScriptError)>) {
    let (mut done, mut failed): (Vec<(UnitId, ScriptRun<'a>)>, Vec<(UnitId, UnitScriptError)>) = (Vec::new(), Vec::new());
    let order = match links_order(graph) {
        Ok(order) => order,
        Err(error) => return (done, vec![(UnitId(u32::MAX), error)]),
    };
    // Each script's run, by the script unit, for its dependents' metadata; and, for a script
    // whose served unit has `links`, what its dependents' scripts are served of it (brief S1).
    let mut ran_by_script: Vec<(UnitId, usize)> = Vec::new();
    let mut producers: Vec<(UnitId, DepProducer)> = Vec::new();
    for script in order {
        let unit = &graph.units[script.index()];
        let Some(serves) = served(graph, script) else {
            failed.push((script, UnitScriptError::ServesNothing { package: package(unit) }));
            continue;
        };
        let mut deps = Vec::new();
        let mut given: Vec<(&str, &DepProducer)> = Vec::new();
        let mut missing = None;
        for (input, links) in links_inputs(graph, script) {
            let producer = producers.iter().find(|(s, _)| *s == input).map(|(_, producer)| producer);
            match (ran_by_script.iter().find(|(s, _)| *s == input), producer) {
                (Some((_, at)), Some(producer)) => {
                    deps.extend(dep_metadata(links, &done[*at].1.outputs));
                    given.push((links, producer));
                }
                _ => {
                    let dependency = package(&graph.units[input.index()]);
                    missing = Some(UnitScriptError::Dependency { package: package(unit), dependency, links: links.to_owned() });
                    break;
                }
            }
        }
        if let Some(error) = missing {
            failed.push((serves, error));
            continue;
        }
        let Some((tree, root)) = files(unit) else {
            failed.push((serves, UnitScriptError::NoFiles { package: package(unit) }));
            continue;
        };
        // Brief S1b: what is upstream of this script, for its readers (a forwarded path).
        let upstream = DepProducer::closure(&given);
        let ran = outputs_viewing(runtime, setting, graph, script, blaze_own::Share::share(&tree), blaze_own::Share::share(&root), &deps, &given, alloc);
        match ran {
            Ok(ran) => {
                if graph.units[serves.index()].links.is_some() {
                    producers.push((script, DepProducer::of(tree, root, &ran.outputs).with_upstream(upstream)));
                }
                ran_by_script.push((script, done.len()));
                done.push((serves, ran));
            }
            Err(error) => failed.push((serves, error)),
        }
    }
    (done, failed)
}

#[cfg(test)]
mod tests {
    use blaze_build_script::{Given, Outputs, env::PackageFacts, instr::OutputLimits};
    use blaze_own::Alloc;
    use blaze_unit_graph::UnitId;

    use super::{dep_metadata, order_after};

    /// Two packages: `sys` has `links = "aws-lc"` and prints its include folder both ways;
    /// `user` depends on it directly. `user`'s script runs after `sys`'s and is given
    /// `DEP_AWS_LC_INCLUDE` and `DEP_AWS_LC_CONF`, and a changed value changes its environment
    /// (so its key). The control: without the dependency's metadata neither variable is there.
    #[test]
    fn a_links_dependencys_metadata_reaches_its_dependents_script() {
        let alloc = Alloc::global();
        let printed = b"cargo:include=/workspace/include\ncargo::metadata=conf=fips\ncargo:rustc-cfg=x\n";
        let sys = Outputs::read(printed, "/target/debug/build/sys-0123456789abcdef/out", OutputLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
        let deps: Vec<_> = dep_metadata("aws-lc", &sys).collect();
        let given = |deps| Given {
            package: PackageFacts { name: "user", version: "1.0.0", ..PackageFacts::default() },
            seen: blaze_expand::seen::SeenRoot::Package,
            features: &[],
            target_cfg: &[],
            target: "aarch64-apple-darwin",
            host: "aarch64-apple-darwin",
            deps,
        };
        let env = blaze_build_script::env(&given(&deps), alloc);
        assert_eq!(env.get("DEP_AWS_LC_INCLUDE"), Some("/workspace/include"));
        assert_eq!(env.get("DEP_AWS_LC_CONF"), Some("fips"));
        let bare = blaze_build_script::env(&given(&[]), alloc);
        assert_eq!(bare.get("DEP_AWS_LC_INCLUDE"), None);
        let other = Outputs::read(b"cargo:include=/workspace/other\n", "/target/debug/build/sys-0123456789abcdef/out", OutputLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
        let changed: Vec<_> = dep_metadata("aws-lc", &other).collect();
        assert_ne!(blaze_build_script::env(&given(&changed), alloc), env);
        // The order: `user` (unit 3) waits on `sys` (unit 7), though its id is lower; a third
        // script with no inputs keeps its place by id.
        let items = [UnitId(1), UnitId(3), UnitId(7)];
        let inputs = [vec![], vec![UnitId(7)], vec![]];
        assert_eq!(order_after(&items, &inputs), Ok(vec![UnitId(1), UnitId(7), UnitId(3)]));
        // A cycle is refused, never run in some order.
        let cycle = [vec![UnitId(3)], vec![UnitId(1)], vec![]];
        assert!(order_after(&items, &cycle).is_err());
    }

    #[test]
    fn wasm_lookup_requires_current_module_header_and_ignores_stale_v3() {
        let store = std::env::temp_dir().join(format!("blaze-script-unit-header-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let shim = super::shim_line();
        let v3_dir = store.join("build-script/modules/stale-v3");
        std::fs::create_dir_all(&v3_dir).expect("create v3 dir");
        std::fs::write(
            v3_dir.join("identity"),
            format!("blaze build-script module 3\nname demo\nversion 0.1.0\nfeatures \n{shim}\nbuilder blaze-wasm64\n"),
        )
        .expect("write v3 identity");

        // Stale v3, v4, v5, and wasm32-bootstrap v5 modules are not returned (forces recompile with current semantics).
        for (dir_name, stale_header) in [
            ("stale-v3", "blaze build-script module 3"),
            ("stale-v4", "blaze build-script module 4"),
            ("stale-v5", "blaze build-script module 5"),
            ("stale-wasm32-v5", "blaze build-script module wasm32 5"),
            ("stale-wasm64-v6", "blaze build-script module wasm64 6"),
            ("stale-wasm64-v7", "blaze build-script module wasm64 7"),
        ] {
            let stale_dir = store.join(format!("build-script/modules/{dir_name}"));
            std::fs::create_dir_all(&stale_dir).expect("create stale dir");
            std::fs::write(
                stale_dir.join("identity"),
                format!("{stale_header}\nname demo\nversion 0.1.0\nfeatures \n{shim}\nbuilder blaze-wasm64\n"),
            )
            .expect("write stale identity");
        }
        let miss = super::module_of_files_and_deps(&store, "demo", "0.1.0", &[], None, None).expect("lookup stale");
        assert_eq!(miss, None, "stale/wasm32 modules must not match WASM_MODULE_HEADER ({})", super::WASM_MODULE_HEADER);

        // Current wasm64 v8 module placed alongside stale/wasm32 modules matches uniquely (never Ambiguous).
        let fresh_dir = store.join("build-script/modules/fresh-wasm64-v8");
        std::fs::create_dir_all(&fresh_dir).expect("create fresh dir");
        std::fs::write(
            fresh_dir.join("identity"),
            format!("{}\nname demo\nversion 0.1.0\nfeatures \n{shim}\nbuilder blaze-wasm64\n", super::WASM_MODULE_HEADER),
        )
        .expect("write fresh identity");
        let hit = super::module_of_files_and_deps(&store, "demo", "0.1.0", &[], None, None).expect("lookup v7");
        assert_eq!(hit, Some(fresh_dir));

        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn native_lookup_keeps_v3_usable_and_ignores_non_v3() {
        let store = std::env::temp_dir().join(format!("blaze-script-unit-native-header-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let host = "aarch64-apple-darwin";
        let other_dir = store.join("build-script/modules/non-v3-native");
        std::fs::create_dir_all(&other_dir).expect("create non-v3 native dir");
        std::fs::write(
            other_dir.join("identity"),
            format!("blaze build-script module 2\nname libproc\nversion 0.14.11\nfeatures \ntarget {host}\nbuilder bootstrap\n"),
        )
        .expect("write non-v3 native identity");

        // Non-v3 native record is ignored.
        let miss = super::module_of_native(&store, "libproc", "0.14.11", &[], host).expect("lookup non-v3 native");
        assert_eq!(miss, None, "non-v3 native module must not match NATIVE_MODULE_HEADER ({})", super::NATIVE_MODULE_HEADER);

        // Existing v3 native record coexisting beside non-v3 matches uniquely (no re-bootstrap, never Ambiguous).
        let v3_dir = store.join("build-script/modules/existing-native-v3");
        std::fs::create_dir_all(&v3_dir).expect("create native v3 dir");
        std::fs::write(
            v3_dir.join("identity"),
            format!("{}\nname libproc\nversion 0.14.11\nfeatures \ntarget {host}\nbuilder bootstrap\n", super::NATIVE_MODULE_HEADER),
        )
        .expect("write native v3 identity");
        let hit = super::module_of_native(&store, "libproc", "0.14.11", &[], host).expect("lookup native v3");
        assert_eq!(hit, Some(v3_dir));

        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn native_lookup_accepts_native_1_bootstrap_header() {
        let store = std::env::temp_dir().join(format!("blaze-script-unit-native1-header-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let host = "aarch64-apple-darwin";
        let native1_dir = store.join("build-script/modules/existing-native-1");
        std::fs::create_dir_all(&native1_dir).expect("create native 1 dir");
        std::fs::write(
            native1_dir.join("identity"),
            format!("{}\nname reth-mdbx-sys\nversion 2.11.0\nfeatures \ntarget {host}\n", super::NATIVE_BOOTSTRAP_HEADER),
        )
        .expect("write native 1 identity");
        let hit = super::module_of_native_files(&store, "reth-mdbx-sys", "2.11.0", &[], host, None).expect("lookup native 1");
        assert_eq!(hit, Some(native1_dir));

        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn script_deps_digest_covers_wasm64_only_lock_closure_deps() {
        use blaze_manifest::lock::{Lock, LockId};
        use blaze_own::Vec as OwnVec;
        use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitKind};
        let alloc = blaze_own::testing::heap().alloc();

        let mk_lock = |errno_sum: &str| {
            format!(
                "version = 4\n\n\
                 [[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"rustix\"]\n\n\
                 [[package]]\nname = \"rustix\"\nversion = \"1.1.4\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\ndependencies = [\"errno\"]\n\n\
                 [[package]]\nname = \"errno\"\nversion = \"0.3.14\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{errno_sum}\"\n"
            )
        };
        let lock_a_text = mk_lock("1111111111111111111111111111111111111111111111111111111111111111");
        let lock_b_text = mk_lock("2222222222222222222222222222222222222222222222222222222222222222");
        let lock_a = Lock::read(lock_a_text.as_bytes(), alloc).expect("lock_a");
        let lock_b = Lock::read(lock_b_text.as_bytes(), alloc).expect("lock_b");

        // Host Linux UnitGraph has app(BuildScript) -> rustix(Lib), with NO `errno` unit!
        let mut bs_deps = OwnVec::new_in(alloc);
        bs_deps.push(UnitDep { name: Text::from_str_in("rustix", alloc), unit: UnitId(1) });
        let mut units = OwnVec::new_in(alloc);
        units.push(Unit {
            package: LockId(0),
            name: Text::from_str_in("app", alloc),
            version: Text::from_str_in("0.1.0", alloc),
            kind: UnitKind::BuildScript,
            for_host: true,
            feature_side: blaze_unit_graph::HostFeatureSide::Target,
            edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
            crate_name: Text::from_str_in("build_script_build", alloc),
            root: Text::from_str_in("build.rs", alloc),
            features: OwnVec::new_in(alloc),
            cfg: OwnVec::new_in(alloc),
            deps: bs_deps,
            build_script: None,
            links: None,
        });
        units.push(Unit {
            package: LockId(1),
            name: Text::from_str_in("rustix", alloc),
            version: Text::from_str_in("1.1.4", alloc),
            kind: UnitKind::Lib,
            for_host: true,
            feature_side: blaze_unit_graph::HostFeatureSide::Host,
            edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
            crate_name: Text::from_str_in("rustix", alloc),
            root: Text::from_str_in("src/lib.rs", alloc),
            features: OwnVec::new_in(alloc),
            cfg: OwnVec::new_in(alloc),
            deps: OwnVec::new_in(alloc),
            build_script: None,
            links: None,
        });
        let graph = UnitGraph { units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };

        let d1 = super::script_deps_digest(&graph, None, &lock_a, UnitId(0), |_| None)
            .expect("frame ok")
            .expect("digest a");
        let d2 = super::script_deps_digest(&graph, None, &lock_b, UnitId(0), |_| None)
            .expect("frame ok")
            .expect("digest b");
        assert_ne!(d1, d2, "script_deps_digest must change when a wasm64-only transitive locked dep (`errno`) changes");
    }

    #[test]
    fn script_deps_digest_covers_direct_wasi_only_build_dep_even_when_host_deps_empty() {
        use blaze_manifest::lock::{Lock, LockId};
        use blaze_own::Vec as OwnVec;
        use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitKind};
        let alloc = blaze_own::testing::heap().alloc();

        // Case A: `app`'s ONLY build-dep is a WASI-only dep `wasi_only`, so host `Unit.deps` is EMPTY!
        let mk_lock_only = |wasi_sum: &str| {
            format!(
                "version = 4\n\n\
                 [[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"wasi_only\"]\n\n\
                 [[package]]\nname = \"wasi_only\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{wasi_sum}\"\n"
            )
        };
        let lock_only_a_txt = mk_lock_only("1111111111111111111111111111111111111111111111111111111111111111");
        let lock_only_b_txt = mk_lock_only("2222222222222222222222222222222222222222222222222222222222222222");
        let lock_only_a = Lock::read(lock_only_a_txt.as_bytes(), alloc).expect("lock_only_a");
        let lock_only_b = Lock::read(lock_only_b_txt.as_bytes(), alloc).expect("lock_only_b");

        let mut units_empty = OwnVec::new_in(alloc);
        units_empty.push(Unit {
            package: LockId(0),
            name: Text::from_str_in("app", alloc),
            version: Text::from_str_in("0.1.0", alloc),
            kind: UnitKind::BuildScript,
            for_host: true,
            feature_side: blaze_unit_graph::HostFeatureSide::Target,
            edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
            crate_name: Text::from_str_in("build_script_build", alloc),
            root: Text::from_str_in("build.rs", alloc),
            features: OwnVec::new_in(alloc),
            cfg: OwnVec::new_in(alloc),
            deps: OwnVec::new_in(alloc),
            build_script: None,
            links: None,
        });
        let graph_empty = UnitGraph { units: units_empty, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };
        let only_a = super::script_deps_digest(&graph_empty, None, &lock_only_a, UnitId(0), |_| None)
            .expect("frame ok")
            .expect("must return Some even when host Unit.deps is empty");
        let only_b = super::script_deps_digest(&graph_empty, None, &lock_only_b, UnitId(0), |_| None)
            .expect("frame ok")
            .expect("must return Some for lock b");
        assert_ne!(only_a, only_b, "digest must change when the sole WASI-only direct build-dep changes");

        // Control: when `app` truly has NO lock deps at all, still returns `Ok(None)`.
        let lock_none = Lock::read(b"version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n", alloc).expect("lock_none");
        assert_eq!(super::script_deps_digest(&graph_empty, None, &lock_none, UnitId(0), |_| None).expect("frame ok"), None);

        // Case B: `app` has a host-active build-dep `host_dep`, a WASI-only direct path build-dep
        // `wasi_path` (resolved in `wasm_graph`), AND a normal workspace member dep `normal_dep`
        // (present in `Cargo.lock` of `app` but NOT a build-dep). Editing `wasi_path` MUST change
        // the digest via `wasm_graph`, while editing `normal_dep` MUST NEVER invoke `path_files`
        // or change the digest (Rule 21: `UnitGraph`'s key does not hash normal deps -> prevents
        // `TwoValues`; Rule 60: editing normal workspace crates never invalidates `build.rs`).
        let lock_sib_txt = "version = 4\n\n\
            [[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"host_dep\", \"normal_dep\", \"wasi_path\"]\n\n\
            [[package]]\nname = \"host_dep\"\nversion = \"0.1.0\"\ndependencies = [\"dev_dep\"]\n\n\
            [[package]]\nname = \"wasi_path\"\nversion = \"0.1.0\"\n\n\
            [[package]]\nname = \"normal_dep\"\nversion = \"0.1.0\"\n\n\
            [[package]]\nname = \"dev_dep\"\nversion = \"0.1.0\"\n";
        let lock_sib = Lock::read(lock_sib_txt.as_bytes(), alloc).expect("lock_sib");
        let mk_unit = |pkg: u32, name: &str, kind: UnitKind<'static>, deps: &[(&str, u32)]| {
            let mut udeps = OwnVec::new_in(alloc);
            for &(dname, uid) in deps {
                udeps.push(UnitDep { name: Text::from_str_in(dname, alloc), unit: UnitId(uid) });
            }
            Unit {
                package: LockId(pkg),
                name: Text::from_str_in(name, alloc),
                version: Text::from_str_in("0.1.0", alloc),
                kind,
                for_host: true,
                feature_side: blaze_unit_graph::HostFeatureSide::Target,
                edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
                crate_name: Text::from_str_in(name, alloc),
                root: Text::from_str_in(if name == "app" { "build.rs" } else { "src/lib.rs" }, alloc),
                features: OwnVec::new_in(alloc),
                cfg: OwnVec::new_in(alloc),
                deps: udeps,
                build_script: None,
                links: None,
            }
        };
        let mut units_sib = OwnVec::new_in(alloc);
        units_sib.push(mk_unit(0, "app", UnitKind::BuildScript, &[("host_dep", 1)]));
        units_sib.push(mk_unit(1, "host_dep", UnitKind::Lib, &[]));
        let graph_sib = UnitGraph { units: units_sib, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };

        let mut wg_sib_units = OwnVec::new_in(alloc);
        wg_sib_units.push(mk_unit(0, "app", UnitKind::BuildScript, &[("host_dep", 1), ("wasi_path", 2)]));
        wg_sib_units.push(mk_unit(1, "host_dep", UnitKind::Lib, &[]));
        wg_sib_units.push(mk_unit(2, "wasi_path", UnitKind::Lib, &[]));
        let wg_sib = UnitGraph { units: wg_sib_units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };

        // With `wasm_graph`, `wasi_path` is hashed and changes the digest, while `normal_dep` (3)
        // and `dev_dep` (4) are never even passed to `path_files`.
        let sib_a = super::script_deps_digest(&graph_sib, Some(&wg_sib), &lock_sib, UnitId(0), |id| match id {
            LockId(1) => Some(String::from("host-v1")),
            LockId(2) => Some(String::from("wasi-v1")),
            LockId(3) | LockId(4) => panic!("must never hash normal/dev path dep {id:?} when wasm_graph is present"),
            _ => None,
        })
        .expect("frame ok")
        .expect("sib a");
        let sib_b = super::script_deps_digest(&graph_sib, Some(&wg_sib), &lock_sib, UnitId(0), |id| match id {
            LockId(1) => Some(String::from("host-v1")),
            LockId(2) => Some(String::from("wasi-v2")),
            LockId(3) | LockId(4) => panic!("must never hash normal/dev path dep {id:?} when wasm_graph is present"),
            _ => None,
        })
        .expect("frame ok")
        .expect("sib b");
        assert_ne!(sib_a, sib_b, "digest must change when a sibling direct WASI-only path build-dep changes");

        // Without `wasm_graph` (minimal toolchain fallback), `normal_dep` (3) and `dev_dep` (4)
        // must STILL never be passed to `path_files` (Rule 21: prevents `TwoValues` when normal
        // or dev workspace crates are edited).
        let no_wg_a = super::script_deps_digest(&graph_sib, None, &lock_sib, UnitId(0), |id| match id {
            LockId(1) => Some("host-v1".into()),
            LockId(3) | LockId(4) => panic!("must never hash normal/dev path dep {id:?} outside active_graph"),
            _ => None,
        })
        .expect("frame ok")
        .expect("no_wg_a");
        let no_wg_b = super::script_deps_digest(&graph_sib, None, &lock_sib, UnitId(0), |id| match id {
            LockId(1) => Some("host-v2".into()),
            LockId(3) | LockId(4) => panic!("must never hash normal/dev path dep {id:?} outside active_graph"),
            _ => None,
        })
        .expect("frame ok")
        .expect("no_wg_b");
        assert_ne!(no_wg_a, no_wg_b, "editing actual host build-dep must still move digest without wasm_graph");

        // Case C: when `wasm_graph` toggles a WASI-only dep feature (e.g. unified from a sibling),
        // `script_deps_digest` changes even when host `UnitGraph` and `Cargo.lock` are identical!
        let mk_wg = |wasi_feats: &[&str]| {
            let mut wg_bs_deps = OwnVec::new_in(alloc);
            wg_bs_deps.push(UnitDep { name: Text::from_str_in("wasi_only", alloc), unit: UnitId(1) });
            let mut wf = OwnVec::new_in(alloc);
            for f in wasi_feats {
                wf.push(Text::from_str_in(f, alloc));
            }
            let mut u = OwnVec::new_in(alloc);
            u.push(Unit {
                package: LockId(0),
                name: Text::from_str_in("app", alloc),
                version: Text::from_str_in("0.1.0", alloc),
                kind: UnitKind::BuildScript,
                for_host: true,
                feature_side: blaze_unit_graph::HostFeatureSide::Target,
                edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
                crate_name: Text::from_str_in("build_script_build", alloc),
                root: Text::from_str_in("build.rs", alloc),
                features: OwnVec::new_in(alloc),
                cfg: OwnVec::new_in(alloc),
                deps: wg_bs_deps,
                build_script: None,
                links: None,
            });
            u.push(Unit {
                package: LockId(1),
                name: Text::from_str_in("wasi_only", alloc),
                version: Text::from_str_in("0.1.0", alloc),
                kind: UnitKind::Lib,
                for_host: true,
                feature_side: blaze_unit_graph::HostFeatureSide::Host,
                edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
                crate_name: Text::from_str_in("wasi_only", alloc),
                root: Text::from_str_in("src/lib.rs", alloc),
                features: wf,
                cfg: OwnVec::new_in(alloc),
                deps: OwnVec::new_in(alloc),
                build_script: None,
                links: None,
            });
            UnitGraph { units: u, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) }
        };
        let wg_on = mk_wg(&["extra_wasi"]);
        let wg_off = mk_wg(&[]);
        let feat_on = super::script_deps_digest(&graph_empty, Some(&wg_on), &lock_only_a, UnitId(0), |_| None)
            .expect("frame ok")
            .expect("feat_on");
        let feat_off = super::script_deps_digest(&graph_empty, Some(&wg_off), &lock_only_a, UnitId(0), |_| None)
            .expect("frame ok")
            .expect("feat_off");
        assert_ne!(feat_on, feat_off, "script_deps_digest must change when a WASI-only dep feature toggles in wasm_graph");
    }

    #[test]
    fn script_deps_digest_distinguishes_same_name_version_different_source_feature_swap() {
        use blaze_manifest::lock::{Lock, LockId};
        use blaze_own::Vec as OwnVec;
        use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitKind};
        let alloc = blaze_own::testing::heap().alloc();

        let lock_text = "version = 4\n\n\
            [[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"dup 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)\",\n \"dup 0.1.0 (git+https://example.com/dup.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)\",\n]\n\n\
            [[package]]\nname = \"dup\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"1111111111111111111111111111111111111111111111111111111111111111\"\n\n\
            [[package]]\nname = \"dup\"\nversion = \"0.1.0\"\nsource = \"git+https://example.com/dup.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n";
        let lock = Lock::read(lock_text.as_bytes(), alloc).expect("lock");

        let mk_graph = |feats_reg: &[&str], feats_git: &[&str]| {
            let mut bs_deps = OwnVec::new_in(alloc);
            bs_deps.push(UnitDep { name: Text::from_str_in("dup_reg", alloc), unit: UnitId(1) });
            bs_deps.push(UnitDep { name: Text::from_str_in("dup_git", alloc), unit: UnitId(2) });
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
                name: Text::from_str_in("app", alloc),
                version: Text::from_str_in("0.1.0", alloc),
                kind: UnitKind::BuildScript,
                for_host: true,
                feature_side: blaze_unit_graph::HostFeatureSide::Target,
                edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
                crate_name: Text::from_str_in("build_script_build", alloc),
                root: Text::from_str_in("build.rs", alloc),
                features: OwnVec::new_in(alloc),
                cfg: OwnVec::new_in(alloc),
                deps: bs_deps,
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

        let host_empty = {
            let mut units = OwnVec::new_in(alloc);
            units.push(Unit {
                package: LockId(0),
                name: Text::from_str_in("app", alloc),
                version: Text::from_str_in("0.1.0", alloc),
                kind: UnitKind::BuildScript,
                for_host: true,
                feature_side: blaze_unit_graph::HostFeatureSide::Target,
                edition: edition::PackageEdition::of_manifest("2021").expect("2021"),
                crate_name: Text::from_str_in("build_script_build", alloc),
                root: Text::from_str_in("build.rs", alloc),
                features: OwnVec::new_in(alloc),
                cfg: OwnVec::new_in(alloc),
                deps: OwnVec::new_in(alloc),
                build_script: None,
                links: None,
            });
            UnitGraph { units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) }
        };
        let wg_ab = mk_graph(&["feat_a"], &["feat_b"]);
        let wg_ba = mk_graph(&["feat_b"], &["feat_a"]);
        let d_ab = super::script_deps_digest(&host_empty, Some(&wg_ab), &lock, UnitId(0), |_| None)
            .expect("frame ok")
            .expect("d_ab");
        let d_ba = super::script_deps_digest(&host_empty, Some(&wg_ba), &lock, UnitId(0), |_| None)
            .expect("frame ok")
            .expect("d_ba");
        assert_ne!(
            d_ab, d_ba,
            "swapping features A and B between two same-name/version packages from different sources must change script_deps_digest"
        );
    }

    #[test]
    fn script_deps_digest_scopes_to_build_deps_when_wasm_graph_present_and_memoizes_path_files() {
        use blaze_manifest::lock::{Lock, LockId};
        use blaze_own::Vec as OwnVec;
        use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitKind};
        let alloc = blaze_own::testing::heap().alloc();

        // Lock packages:
        //   0: app (depends on build_dep_direct, normal_or_dev_path in flat Cargo.lock deps)
        //   1: build_dep_direct (path, depends on build_dep_trans)
        //   2: build_dep_trans (path)
        //   3: normal_or_dev_path (path — normal/dev dep of app, NOT a build-dependency!)
        //   4: dep_bs_helper (path — build-dep of build_dep_direct's own build.rs)
        let lock_txt = "version = 4\n\n\
            [[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"build_dep_direct\", \"normal_or_dev_path\"]\n\n\
            [[package]]\nname = \"build_dep_direct\"\nversion = \"0.1.0\"\ndependencies = [\"build_dep_trans\", \"dep_bs_helper\"]\n\n\
            [[package]]\nname = \"build_dep_trans\"\nversion = \"0.1.0\"\n\n\
            [[package]]\nname = \"normal_or_dev_path\"\nversion = \"0.1.0\"\n\n\
            [[package]]\nname = \"dep_bs_helper\"\nversion = \"0.1.0\"\n";
        let lock = Lock::read(lock_txt.as_bytes(), alloc).expect("lock");

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
                feature_side: blaze_unit_graph::HostFeatureSide::Target,
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

        // Negative control 1: std-only build.rs with ZERO build-dependencies in wasm_graph,
        // even though Cargo.lock lists path dependencies on `app`.
        let mut units_zero = OwnVec::new_in(alloc);
        units_zero.push(mk_unit(0, "app", UnitKind::BuildScript, &[], &[], None));
        let graph_zero = UnitGraph { units: units_zero, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };
        let mut zero_calls = Vec::new();
        let zero_digest = super::script_deps_digest(&graph_zero, Some(&graph_zero), &lock, UnitId(0), |id| {
            zero_calls.push(id);
            Some(format!("digest-{}", id.0))
        })
        .expect("frame ok");
        assert_eq!(zero_digest, None, "std-only build.rs with zero build-deps must return None when wasm_graph is present");
        assert!(zero_calls.is_empty(), "zero build-deps must never hash normal/dev path deps in Cargo.lock");

        // Positive + Negative control 2:
        //   0: app BuildScript -> [1 (build_dep_direct), 5 (second unit of build_dep_direct with extra feature)]
        //   1: build_dep_direct Lib -> [2 (build_dep_trans)], build_script = Some(3)
        //   2: build_dep_trans Lib -> []
        //   3: build_dep_direct BuildScript -> [4 (dep_bs_helper)]
        //   4: dep_bs_helper Lib -> []
        //   5: build_dep_direct Lib (feat "extra") -> [2 (build_dep_trans)]
        // Note: LockId(3) (`normal_or_dev_path`) is in `app`'s Cargo.lock deps, NOT in the build-dep closure!
        let mut units = OwnVec::new_in(alloc);
        units.push(mk_unit(0, "app", UnitKind::BuildScript, &[], &[("build_dep_direct", 1), ("build_dep_direct_extra", 5)], None));
        units.push(mk_unit(1, "build_dep_direct", UnitKind::Lib, &[], &[("build_dep_trans", 2)], Some(3)));
        units.push(mk_unit(2, "build_dep_trans", UnitKind::Lib, &[], &[], None));
        units.push(mk_unit(1, "build_dep_direct", UnitKind::BuildScript, &[], &[("dep_bs_helper", 4)], None));
        units.push(mk_unit(4, "dep_bs_helper", UnitKind::Lib, &[], &[], None));
        units.push(mk_unit(1, "build_dep_direct", UnitKind::Lib, &["extra"], &[("build_dep_trans", 2)], None));
        let wg = UnitGraph { units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };

        let run = |d1: &str, d2: &str, d3_normal: &str, d4_bs: &str| {
            let mut calls = Vec::new();
            let digest = super::script_deps_digest(&wg, Some(&wg), &lock, UnitId(0), |id| {
                calls.push(id);
                match id {
                    LockId(1) => Some(d1.to_owned()),
                    LockId(2) => Some(d2.to_owned()),
                    LockId(3) => Some(d3_normal.to_owned()),
                    LockId(4) => Some(d4_bs.to_owned()),
                    _ => None,
                }
            })
            .expect("frame ok")
            .expect("some digest");
            (digest, calls)
        };

        let (base, calls) = run("dir-v1", "trans-v1", "normal-v1", "helper-v1");
        // Each reachable build-dep LockId must be queried AT MOST ONCE (even though LockId(1) has 2 units!).
        assert_eq!(calls.iter().filter(|&&id| id == LockId(1)).count(), 1, "LockId(1) must be memoized to 1 call");
        assert_eq!(calls.iter().filter(|&&id| id == LockId(2)).count(), 1, "LockId(2) must be queried once");
        assert_eq!(calls.iter().filter(|&&id| id == LockId(4)).count(), 1, "LockId(4) (dep build-script dep) must be queried once");
        // Negative control: normal/dev path dep LockId(3) must NEVER be queried or hashed.
        assert!(!calls.contains(&LockId(3)), "normal/dev path dep must NEVER trigger path_files when wasm_graph is present");
        let (same_after_normal_edit, _) = run("dir-v1", "trans-v1", "normal-v2", "helper-v1");
        assert_eq!(base, same_after_normal_edit, "editing a normal/dev path dep must NOT change script_deps_digest");

        // Positive controls: direct, transitive, and dep-build-script path deps MUST change digest.
        let (changed_direct, _) = run("dir-v2", "trans-v1", "normal-v1", "helper-v1");
        assert_ne!(base, changed_direct, "editing direct path build-dep must change script_deps_digest");
        let (changed_trans, _) = run("dir-v1", "trans-v2", "normal-v1", "helper-v1");
        assert_ne!(base, changed_trans, "editing transitive path build-dep must change script_deps_digest");
        let (changed_helper, _) = run("dir-v1", "trans-v1", "normal-v1", "helper-v2");
        assert_ne!(base, changed_helper, "editing path build-dep of a build-dep's build.rs must change script_deps_digest");

        // Fallback mode (`wasm_graph = None`) must ALSO memoize `path_files` to at most once per LockId
        // even when a package appears in both `lines` and `lock_lines`.
        let mut fallback_calls = Vec::new();
        let _ = super::script_deps_digest(&wg, None, &lock, UnitId(0), |id| {
            fallback_calls.push(id);
            Some(format!("fb-{}", id.0))
        })
        .expect("frame ok");
        for pkg_idx in 0..5u32 {
            assert!(
                fallback_calls.iter().filter(|&&id| id == LockId(pkg_idx)).count() <= 1,
                "fallback mode must call path_files at most once for LockId({pkg_idx})"
            );
        }
    }

    #[test]
    fn streaming_script_files_digest_matches_exact_wire_format() {
        let dir = std::env::temp_dir().join(format!("blaze-script-files-streaming-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("build")).expect("mkdir build");
        std::fs::create_dir_all(dir.join("src")).expect("mkdir src");
        std::fs::write(dir.join("Cargo.toml"), b"[package]\nname = \"s\"\n").expect("Cargo.toml");
        std::fs::write(dir.join("build/main.rs"), b"mod helper; fn main() {}\n").expect("build/main.rs");
        std::fs::write(dir.join("build/helper.rs"), b"pub fn h() {}\n").expect("build/helper.rs");
        std::fs::write(dir.join("top.rs"), b"// top\n").expect("top.rs");
        std::fs::write(dir.join("src/lib.rs"), b"// ignored by script_files_digest\n").expect("src/lib.rs");

        let mut entries = vec![
            ("Cargo.toml", &b"[package]\nname = \"s\"\n"[..]),
            ("build/helper.rs", &b"pub fn h() {}\n"[..]),
            ("build/main.rs", &b"mod helper; fn main() {}\n"[..]),
            ("top.rs", &b"// top\n"[..]),
        ];
        entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let mut wire = Vec::new();
        for (p, b) in &entries {
            wire.extend_from_slice(p.as_bytes());
            wire.push(0);
            wire.extend_from_slice(&(b.len() as u64).to_le_bytes());
            wire.extend_from_slice(b);
        }
        let oracle = crate::module::sha256_hex(&wire);
        assert_eq!(super::script_files_digest(&dir, "./build/main.rs").expect("digest"), oracle);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn script_deps_digest_rejects_mismatched_wasm_side_or_features_and_falls_back() {
        use blaze_manifest::lock::{Lock, LockId};
        use blaze_own::Vec as OwnVec;
        use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitKind};
        let alloc = blaze_own::testing::heap().alloc();

        let mk_lock = |reg_sum: &str| {
            format!(
                "version = 4\n\n\
                 [[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"host_dep\", \"reg_dep\", \"wasi_only_path\"]\n\n\
                 [[package]]\nname = \"host_dep\"\nversion = \"0.1.0\"\n\n\
                 [[package]]\nname = \"reg_dep\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{reg_sum}\"\n\n\
                 [[package]]\nname = \"wasi_only_path\"\nversion = \"0.1.0\"\n"
            )
        };
        let lock_a_txt = mk_lock("1111111111111111111111111111111111111111111111111111111111111111");
        let lock_b_txt = mk_lock("2222222222222222222222222222222222222222222222222222222222222222");
        let lock_a = Lock::read(lock_a_txt.as_bytes(), alloc).expect("lock_a");
        let lock_b = Lock::read(lock_b_txt.as_bytes(), alloc).expect("lock_b");

        let mk_unit = |pkg: u32, name: &str, kind: UnitKind<'static>, for_host: bool, feature_side: blaze_unit_graph::HostFeatureSide, feats: &[&str], deps: &[(&str, u32)], bs: Option<u32>| {
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
                for_host,
                feature_side,
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

        use blaze_unit_graph::HostFeatureSide;
        // Host graph: target-side `app` BuildScript (features=["want"]) -> `host_dep` (path).
        let mut host_units = OwnVec::new_in(alloc);
        host_units.push(mk_unit(0, "app", UnitKind::BuildScript, true, HostFeatureSide::Target, &["want"], &[("host_dep", 1)], None));
        host_units.push(mk_unit(1, "host_dep", UnitKind::Lib, true, HostFeatureSide::Host, &[], &[], None));
        host_units.push(mk_unit(0, "app", UnitKind::Lib, false, HostFeatureSide::Target, &["want"], &[], Some(0)));
        let host_graph = UnitGraph { units: host_units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };

        // Case 1: wasm_graph only has `app` BuildScript with WRONG features (`["other"]`) and empty deps.
        let mut wg_feats_units = OwnVec::new_in(alloc);
        wg_feats_units.push(mk_unit(0, "app", UnitKind::BuildScript, true, HostFeatureSide::Target, &["other"], &[], None));
        wg_feats_units.push(mk_unit(0, "app", UnitKind::Lib, false, HostFeatureSide::Target, &["other"], &[], Some(0)));
        let wg_wrong_feats = UnitGraph { units: wg_feats_units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };

        // Case 2: wasm_graph only has `app` BuildScript with WRONG HostFeatureSide (host-side) and empty deps.
        let mut wg_side_units = OwnVec::new_in(alloc);
        wg_side_units.push(mk_unit(0, "app", UnitKind::BuildScript, true, HostFeatureSide::Host, &["want"], &[], None));
        wg_side_units.push(mk_unit(0, "app", UnitKind::Lib, true, HostFeatureSide::Host, &["want"], &[], Some(0)));
        let wg_wrong_side = UnitGraph { units: wg_side_units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };

        let fallback_v1 = super::script_deps_digest(&host_graph, None, &lock_a, UnitId(0), |id| (id == LockId(1)).then(|| "h-v1".to_owned()))
            .expect("ok")
            .expect("fallback digest");
        let fallback_v2 = super::script_deps_digest(&host_graph, None, &lock_a, UnitId(0), |id| (id == LockId(1)).then(|| "h-v2".to_owned()))
            .expect("ok")
            .expect("fallback digest v2");
        assert_ne!(fallback_v1, fallback_v2);

        for (label, wg) in [("wrong features", &wg_wrong_feats), ("wrong side", &wg_wrong_side)] {
            let run_wg = |lock: &Lock<'_>, host_hex: &str, wasi_hex: &str| {
                super::script_deps_digest(&host_graph, Some(wg), lock, UnitId(0), |id| match id {
                    LockId(1) => Some(host_hex.to_owned()),
                    LockId(3) => Some(wasi_hex.to_owned()),
                    _ => None,
                })
                .expect("ok")
                .expect("mismatched wasm root must not return None")
            };
            let d1 = run_wg(&lock_a, "h-v1", "w-v1");
            let d_host_edit = run_wg(&lock_a, "h-v2", "w-v1");
            let d_wasi_path_edit = run_wg(&lock_a, "h-v1", "w-v2");
            let d_lock_b = run_wg(&lock_b, "h-v1", "w-v1");
            assert_ne!(d1, d_host_edit, "{label}: must still cover host path build-dep");
            assert_ne!(d1, d_wasi_path_edit, "{label}: must pin WASI-only lock path build-dep by its actual contents");
            assert_ne!(d1, d_lock_b, "{label}: must still cover lock-pinned dep in fallback mode");
        }

        // Case 3 (Wave 14 PR 3 positive control): host-side `app` BuildScript in host_graph matches
        // an orphan (`saw_server == false`) host-side `app` BuildScript in wasm_graph with zero
        // deps, returning `Ok(None)` instead of falling back to the lock walk.
        let mut host_side_units = OwnVec::new_in(alloc);
        host_side_units.push(mk_unit(0, "app", UnitKind::BuildScript, true, HostFeatureSide::Host, &["want"], &[("host_dep", 1)], None));
        host_side_units.push(mk_unit(1, "host_dep", UnitKind::Lib, true, HostFeatureSide::Host, &[], &[], None));
        host_side_units.push(mk_unit(0, "app", UnitKind::Lib, true, HostFeatureSide::Host, &["want"], &[], Some(0)));
        let host_side_graph = UnitGraph { units: host_side_units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };

        let mut wg_orphan_host_units = OwnVec::new_in(alloc);
        wg_orphan_host_units.push(mk_unit(0, "app", UnitKind::BuildScript, true, HostFeatureSide::Host, &["want"], &[], None));
        let wg_orphan_host = UnitGraph { units: wg_orphan_host_units, resolver: Resolver::V2, meta: OwnVec::new_in(alloc), left_out: OwnVec::new_in(alloc) };

        assert_eq!(
            super::script_deps_digest(&host_side_graph, Some(&wg_orphan_host), &lock_a, UnitId(0), |_| None).expect("ok"),
            None,
            "orphan Host-side BuildScript in wasm_graph must match Host-side root instead of falling back"
        );
    }
}
