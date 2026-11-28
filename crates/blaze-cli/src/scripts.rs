//! The `Script` task: one build script's run, one a (script unit, target) (decision D115;
//! docs/v2/plan/design/drive.md, "Build scripts in the command").
//!
//! - NAMED by the `UnitGraph` record: one `Locate::Source` part per build-script unit
//!   ([`parts`]), its subject the unit's package, version, the side of the unit it serves and
//!   its features ([`subject`]), its source that subject over the graph's source (the lock's
//!   pins, the manifests, the target, the ask and the home's module listings, among them
//!   [`MODULE_LISTING`]) and the command's [`MODE`]. Nothing of the machine is in it: the roots a
//!   script sees are `/package` or `/workspace/<dir>` (D96).
//! - READS: the `UnitGraph` record (its graph); the `Script` record of each script whose
//!   `links` metadata it is given (`DEP_<LINKS>_*`: the links graph) AND of every script
//!   upstream of those (their `links_inputs` closure: a producer may forward an upstream
//!   script's path, brief S1b), each listed in the record's reads with the out it had, so an
//!   equal output moves no reader and another output reruns it. A path in a value is served
//!   through the producers' views (`script_unit::outputs_viewing`, blaze-build-script
//!   `dep_paths`): under a producer's `OUT_DIR` its record's files, under its served tree that
//!   tree at `/deps/<links>`; a machine path is refused by name. The engine records a member's
//!   own and mounted workspace reads as checkout cells; a request that planned no such cells
//!   ([`CheckoutReads::Hole`]) treats them as holes.
//! - OUT: the outputs record (`blaze_build_script::Outputs::encode`: the `cargo:` lines in
//!   order and `OUT_DIR`'s files), its digest the out. Its readers (the unit's `CrateScope`)
//!   read it as a read (decision D115 (B)).
//! - RUNS on the pool, in its worker's task frame ([`pooled`]): the graph decoded there from
//!   the record's bytes, the package's files served from its fetched record or the member's
//!   folder, the run `script_unit::outputs` (wasm, or the D112 native runner where
//!   `native_unit::placed` says so: one choice point, before anything runs). The run's limits
//!   (fuel, memory, `OUT_DIR` size; the native runner's deadline) end it; an expiry is the
//!   run's own typed error, a finding by the script's name.
//! - SETTING: made once a command, at the first `Script` task, from what `main` read at the
//!   edge (rule 75: the home, the SDK folder): the facts table, the spawn setting
//!   (`fact_pins::command_spawn_setting`) and the native setting
//!   (`native_setting::command_native_setting`). A missing home, shelf or SDK is a typed error
//!   naming the path: the scripts that need it fail by name (a native one), or every script
//!   does (the runtime, the spawn setting); none is run without it, none is skipped.
//!
//! A native run's source holds the macOS SDK's identity (its version, blaze-apple-sdk; never
//! its folder), resolved once at the command's edge (sdk.rs): [`parts`].
//!
//! Owed (said in the closing note's words of each refusal): the setting's other machine facts
//! (the OS pins) become a fact the record reads when the drive takes command
//! facts (`Ask::facts`, DRIVER runner 17); a checkout file read becomes a read when
//! `key::content_of` exists; `cargo:warning` lines are not printed yet.

use std::alloc::Global;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use blaze_build_script::key::CMode;
use blaze_build_script::native::Runner;
use blaze_build_script::outputs::OutputsRef;
use blaze_build_script::probe::Row;
use blaze_build_script::toolchain::Mode;
use blaze_drive::{Input, Kind, Name, Task, Want};
use blaze_ids::ContentId;
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{AVec, Alloc, Erased};
use blaze_proc_macro::native_unit::{NativeSetting, placed_in_store_with_lock_and_source, placed_with_lock_and_source};
use blaze_proc_macro::script::ScriptRuntime;
use blaze_proc_macro::script_sandbox::{ScriptRoot, Tree, dir_tree, mem_tree_with_submodules};
use blaze_proc_macro::script_unit::{self, Setting, links_closure};
use blaze_proc_macro::spawn_route::SpawnSetting;
use blaze_unit_graph::{CfgAtom, Records, StoreSources, Unit, UnitGraph, UnitId, UnitKind};
use blaze_world::Side;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;

use crate::CliError;
use crate::fetch::FetchEnv;
use crate::store::Store;
use crate::work::Record;

#[cfg(test)]
#[path = "scripts_tests.rs"]
mod tests;
const SOURCE_DOMAIN: &str = "blaze-cli script unit source 1";
/// The out: the outputs record's bytes ("1": `bzbsout4`, blaze-build-script's form).
const OUT_DOMAIN: &str = "blaze-cli script outputs out 1";
/// The rule a refusal of this task names.
pub(crate) const RULE: &str = "scripts.rs (D115)";

/// How a `Script` record's run was made: its payload's first byte, a field of the record's own
/// bytes (decision D71 as ruled for D115: the store's sharing seam keeps a natively run
/// record, and so every record that reads it, off the public namespace).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum RanAs {
    /// In blaze, as wasm (`script::ScriptRuntime`).
    Wasm = 1,
    /// Through the D112 native runner: the machine's tools ran it.
    Native = 2,
}

impl RanAs {
    /// How a script placed on `runner` runs: the one map from D112's choice point.
    pub(crate) fn of(runner: &Runner) -> RanAs {
        match runner {
            Runner::Native { .. } => RanAs::Native,
            Runner::Wasm => RanAs::Wasm,
        }
    }
}

/// Whether the run of a crate's build script may be shared (ruling D115 sharing (i)): its crate
/// is public (the toolchain's, a checksummed registry archive's) AND it is placed on the wasm
/// path. A native run never is: its record, and by the store's public-reads rule every record
/// reading it, stays this machine's (D112's "local records only").
pub(crate) fn run_is_public(crate_public: bool, ran_as: RanAs) -> bool {
    crate_public && ran_as == RanAs::Wasm
}

/// How the run of build-script unit `script` is placed: D112 / D175 / Tier-2 pre-run choice point,
/// made before anything runs; the same function the job asks (`job`), so the key and the run agree.
pub(crate) fn placement(
    home: Option<&str>,
    graph: &UnitGraph<'_>,
    lock: Option<&Lock<'_>>,
    script: UnitId,
    source_reason: Option<blaze_build_script::native::NativeSourceReason>,
    alloc: Alloc<'_>,
) -> RanAs {
    match home {
        Some(home) => {
            let store = Path::new(home).join(STORE_UNDER_HOME);
            RanAs::of(&placed_in_store_with_lock_and_source(
                &store,
                graph,
                lock,
                script,
                MODE,
                crate::graph::TRIPLE,
                source_reason,
                alloc,
            ))
        }
        None => RanAs::of(&placed_with_lock_and_source(graph, lock, script, MODE, source_reason, alloc)),
    }
}

/// Classifies a build script's full source tree (`script_root` plus active external `mod`,
/// `#[path = "..."] mod`, and static `include!("...")` files) from `files` into `scratch` without
/// global heap allocation, computing a SHA-256 snapshot fingerprint over all inspected files.
/// Refuses missing root files, corrupt/unreadable pack entries, and non-UTF-8 sources instead of
/// assuming Wasm.
fn classify_script_source_in_files<'s>(
    cx: &Cx,
    checkout: &Dir<bstd::fs::ReadOnly>,
    files: &Files<'_>,
    script_root: &str,
    scratch: &'s Frame,
) -> Result<(Option<blaze_build_script::native::NativeSourceReason>, [u8; 32]), &'s str> {
    let erased = blaze_own::Erased::new(scratch);
    let alloc = erased.handle();
    let mut hasher = blaze_proc_macro::module::FilesHasher::new();
    let reason = blaze_build_script::native::source_tree_requires_native(script_root, alloc, |rel, is_root, visit| {
        match files {
            Files::Member { dir } | Files::External { dir } => {
                let full = if dir.is_empty() {
                    blaze_own::String::from_str_in(rel, alloc)
                } else {
                    blaze_own::format_in!(alloc, "{dir}/{rel}")
                };
                let path = bstd::path::Path::new(full.as_str()).map_err(|err| {
                    let msg = blaze_own::format_in!(alloc, "invalid path `{}`: {err}", full.as_str());
                    &*scratch.alloc_str(msg.as_str())
                })?;
                let mut bytes = bstd::mem::Bytes::new_in(scratch, "script source for Tier-2 placement");
                if let Err(err) = checkout.read_all(cx, &path, &mut bytes) {
                    if matches!(err.kind, bstd::fs::FsKind::NotFound | bstd::fs::FsKind::IsADirectory) && !is_root {
                        return Ok(false);
                    }
                    let msg = blaze_own::format_in!(alloc, "cannot read `{}`: {err}", full.as_str());
                    return Err(&*scratch.alloc_str(msg.as_str()));
                }
                let text = core::str::from_utf8(bytes.as_slice()).map_err(|err| {
                    let msg = blaze_own::format_in!(alloc, "`{}` is not valid UTF-8: {err}", full.as_str());
                    &*scratch.alloc_str(msg.as_str())
                })?;
                hasher.update_file(rel, text.as_bytes());
                visit(text);
                Ok(true)
            }
            Files::Pack { view, dir, .. } => {
                let full = if dir.is_empty() {
                    blaze_own::String::from_str_in(rel, alloc)
                } else {
                    blaze_own::format_in!(alloc, "{dir}/{rel}")
                };
                let bytes = match view.read_slice(full.as_str(), scratch) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        if is_root {
                            let msg = blaze_own::format_in!(alloc, "build-script root `{}` is missing from pack", full.as_str());
                            return Err(&*scratch.alloc_str(msg.as_str()));
                        }
                        return Ok(false);
                    }
                    Err(why) => {
                        let msg = blaze_own::format_in!(alloc, "pack entry `{}` failed to decode: {}", full.as_str(), blaze_text::Shown(&why));
                        return Err(&*scratch.alloc_str(msg.as_str()));
                    }
                };
                let text = core::str::from_utf8(bytes).map_err(|err| {
                    let msg = blaze_own::format_in!(alloc, "`{}` is not valid UTF-8: {err}", full.as_str());
                    &*scratch.alloc_str(msg.as_str())
                })?;
                hasher.update_file(rel, text.as_bytes());
                visit(text);
                Ok(true)
            }
        }
    })?;
    Ok((reason, hasher.finish()))
}

/// A `Script` record's payload split into how it ran and its outputs record (`bzbsout`, read
/// in place with `OutputsRef::open`). `None`: not a `Script` payload (empty, or another first
/// byte).
pub(crate) fn payload_parts(payload: &[u8]) -> Option<(RanAs, &[u8])> {
    let (&first, record) = payload.split_first()?;
    let ran_as = match first {
        1 => RanAs::Wasm,
        2 => RanAs::Native,
        _ => return None,
    };
    Some((ran_as, record))
}

/// The stored files of `outputs` as this command reads them: from the home's file area
/// (`<home>/.blaze/v2/build-script/files/<sha256 hex>`, `HOME` read at the edge), each mapped
/// on its first ask ([`blaze_world::StoredFiles`]); no home, no area (each ask refused).
pub(crate) fn file_area(home: Option<&str>, outputs: &OutputsRef<'_>) -> blaze_world::StoredFiles {
    let folder = home.map(|home| Path::new(home).join(STORE_UNDER_HOME).join(blaze_build_script::outputs::FILE_AREA));
    blaze_world::StoredFiles::new(folder, outputs.stored().map(|file| *file.sha256))
}

/// The build-script module store's folder under blaze's folder in the home (`<home>/.blaze`):
/// `build-script/modules/<key>/{identity,module.wasm,wasm-sha256}`, filled by the bootstrap
/// builder (`toolrun.sh proc_macro_modules`, build-script mode). Its listing is a
/// `HomeListing` row read with the layout (main.rs), so it is in the units' source, the
/// graph's source and every `Script` part's: a module that comes or goes asks again.
pub(crate) const MODULE_LISTING: &str = "v2/build-script/modules";

/// The store `script_unit::Setting::store` names: `<home>/.blaze/v2` (its `build-script/`).
pub(crate) const STORE_UNDER_HOME: &str = ".blaze/v2";

/// What every command asks of a script today: `check`'s placement and C mode (C requests kept,
/// nothing compiled; `NATIVE_EVERY_MODE` native). `run` and `build` take the same records
/// until the command links C: then the mode joins [`subject`] (rule 21), and a deferred record
/// is never what a build takes (`CMode::Deferred`'s contract).
pub(crate) const MODE: Mode = Mode::Check;
const C_MODE: CMode = CMode::Deferred;

/// The reserve of one script's task frame on the pool: the graph decoded again, the package's
/// files, the outputs (`OUT_DIR` holds up to 1 GiB, `FileLimits`) and their encoding. Virtual
/// until touched; a full frame fails that script alone (rule 20).
pub(crate) const SCRIPT_FRAME: usize = 4 << 30;

/// The native work folder's command name (`<home>/.blaze/v2/native-work/<this>`).
const NATIVE_WORK: &str = "blaze";

/// The macro side's target (D89 (a), rule 10): a macro-side crate's script runs for it.
/// (blaze-scope's `macro_side::MACRO_TARGET` is the same text, private to it.)
const MACRO_TARGET: &str = "wasm32-wasip1";

/// The subject of the run of build-script unit `script` for `side`: the one function,
/// `blaze_world::script_subject`, over the unit it serves (package, version, features) and the
/// world's side, never `for_host` (ruling D115 (2)). `None`: no build-script unit, or one that
/// serves no unit.
pub(crate) fn subject(graph: &UnitGraph<'_>, script: UnitId, side: Side, alloc: Alloc<'_>) -> Option<ContentId> {
    let unit = graph.units.get(script.index())?;
    if unit.kind != UnitKind::BuildScript {
        return None;
    }
    let serves = &graph.units[script_unit::served(graph, script)?.index()];
    let mut features: AVec<'_, &str> = AVec::with_capacity_in(serves.features.len(), alloc.clone());
    features.extend(serves.features.iter().map(|f| f.as_str()));
    Some(blaze_world::script_subject(serves.name.as_str(), serves.version.as_str(), side, &features, alloc))
}

/// The `Script` name of the run of build-script unit `script` for `side`.
pub(crate) fn name_of(graph: &UnitGraph<'_>, script: UnitId, side: Side, alloc: Alloc<'_>) -> Option<Name> {
    subject(graph, script, side, alloc).map(|subject| Name::new(Kind::Script, subject))
}

/// The identity of the command's macOS SDK (blaze-apple-sdk `Sdk::identity`: its version and
/// build, never its folder), which a native run's key holds; `None` when the command has none,
/// and then a native run fails naming why ([`run`]).
pub(crate) fn sdk_identity(env: &FetchEnv<'_>) -> Option<blaze_apple_sdk::SdkId> {
    env.sdk.ok().map(|sdk| sdk.identity())
}

/// What a run's key holds of the macOS SDK (rule 21): a native run's compiles and links are
/// made against it, so its identity ([`sdk_identity`]; a marker for none); a wasm run reads no
/// SDK, so nothing, and its key moves with no SDK change.
fn sdk_bytes(ran_as: RanAs, sdk: Option<blaze_apple_sdk::SdkId>) -> ([u8; 1 + blaze_apple_sdk::SdkId::KEY_LEN], usize) {
    let mut out = [0; 1 + blaze_apple_sdk::SdkId::KEY_LEN];
    match (ran_as, sdk) {
        (RanAs::Wasm, _) => (out, 0),
        (RanAs::Native, None) => (out, 1),
        (RanAs::Native, Some(id)) => {
            out[0] = 1;
            out[1..].copy_from_slice(&id.key());
            (out, out.len())
        }
    }
}

/// Selects `wasm_graph` only for `RanAs::Wasm` build scripts; `RanAs::Native` build scripts run as
/// host executables and must be keyed from the host `UnitGraph` dependency closure so host-only
/// target-specific build-dependencies invalidate both the module subject and the script run's
/// `deps_digest` (Rule 21).
pub(crate) fn deps_wasm_graph<'g>(
    ran_as: RanAs,
    wasm_graph: Option<&'g UnitGraph<'g>>,
) -> Option<&'g UnitGraph<'g>> {
    match ran_as {
        RanAs::Wasm => wasm_graph,
        RanAs::Native => None,
    }
}

/// The `Script` parts of a world whose crates read the runs `keys` (`Names::script_keys`: each
/// run's subject and its reading crate's world key, named with the crates' `CrateScope` parts),
/// for a command whose macOS SDK is `sdk` ([`sdk_identity`]): one a subject, sorted and without
/// repeats (two crates of one subject read one run, and have one crate key: ruling D115 (3)).
pub(crate) fn parts(keys: impl Iterator<Item = (ContentId, ContentId, RanAs)>, sdk: Option<blaze_apple_sdk::SdkId>) -> std::vec::Vec<(Name, ContentId), Global> {
    let mut parts = std::vec::Vec::new_in(Global);
    // Piece 2c (ruling D115 (5), (c)): no module listing in a run's source. Its module is a
    // read (script_modules.rs); its crate's world key holds the package's identity and its
    // dependencies' (which `links` inputs it has). Its placement is in it too (ruling D115
    // sharing (ii), rule 21): a native run and a wasm run are never one record. A native run's
    // holds the SDK's identity, never its folder (rules 21, 25).
    for (subject, crate_key, ran_as) in keys {
        let mut h = blaze_ids::Hasher::new(SOURCE_DOMAIN);
        h.id(&subject).id(&crate_key).update(&[mode_byte(MODE), C_MODE as u8, ran_as as u8]);
        let (bytes, len) = sdk_bytes(ran_as, sdk);
        h.update(&bytes[..len]);
        parts.push((Name::new(Kind::Script, subject), h.finish()));
    }
    parts.sort_unstable_by_key(|(name, _)| name.subject);
    parts.dedup_by_key(|(name, _)| name.subject);
    parts
}

/// The cfg `cfg` (a world row's, sorted, each once) as the atoms a script's environment is made
/// from, in `alloc`, in the same order.
pub(crate) fn side_atoms<'a>(cfg: &[(&str, Option<&str>)], alloc: Alloc<'a>) -> AVec<'a, CfgAtom<'a>> {
    let mut atoms: AVec<'a, CfgAtom<'a>> = AVec::with_capacity_in(cfg.len(), alloc.clone());
    for (name, value) in cfg.iter() {
        atoms.push(match value {
            Some(value) => CfgAtom::pair(name, value, alloc.clone()),
            None => CfgAtom::flag(name, alloc.clone()),
        });
    }
    atoms
}

const fn mode_byte(mode: Mode) -> u8 {
    match mode {
        Mode::Check => 1,
        Mode::Build => 2,
        Mode::Run => 3,
    }
}

/// What every `Script` job of the command shares on the pool: `Sync`, made once
/// ([`Scripts::open`]) and held in the request's frame to its end.
pub(crate) struct Shared<'f> {
    result_alloc: blaze_own::SharedAlloc<'static>,
    runtime: ScriptRuntime,
    /// `<home>/.blaze/v2`.
    store: PathBuf,
    /// The checkout's root, resolved once at the command's edge (engine_script_reads.rs
    /// `Served::at_edge`); a simulated world's member script run is refused naming it.
    checkout: crate::engine_script_reads::Served<'f>,
    /// The probe rows of each side, keyed for its triple (blaze-world `script_setting::probe_table`).
    table: Arc<std::vec::Vec<Row>>,
    macro_table: Arc<std::vec::Vec<Row>>,
    spawn: Arc<SpawnSetting>,
    /// Hermetic pinned `clang-23` probe runner and CAS store, or retained refusal when the host
    /// has no clang pin or unpacked shelf (fails only scripts that actually ask a C probe).
    c_probes: blaze_proc_macro::probe_clang::CProbeSetting,
    /// The native setting, or why the command has none (its words, naming the path).
    native: Result<NativeSetting<'f>, &'f str>,
    /// The command resolved no macOS SDK (sdk.rs). A native run's key then holds "no SDK"
    /// ([`sdk_bytes`]), so its refusal is a function of its key: filed as its record, standing
    /// until an SDK resolves, which moves the key. Never a hole, which left tempo's answer
    /// uncertified and every dependent walked again each command (check-g043851: 37 s a no-op).
    no_sdk: bool,
    /// The graph record's bytes, decoded again on each worker (its `Alloc` is a task's).
    graph_record: &'f [u8],
}

/// The command's scripts: what the pool shares, and what only the main thread touches (the
/// names world's graph, the fetched sources' records).
pub(crate) struct Scripts<'f> {
    /// What the pool shares, opened at the first job made ready to run ([`Scripts::shared`]):
    /// a command whose scripts all stand, or whose jobs only ask for inputs, opens none.
    shared: Option<Result<&'f Shared<'f>, &'f str>>,
    /// The graph record's bytes, held in the request's frame for the workers (each run decodes
    /// them again in its own frame, [`run`]).
    graph_record: &'f [u8],
    /// The decoded unit graph: either the names world's (`NamesWorld::graph`) or decoded once
    /// into `frame` when no `Names` task ran yet ([`Scripts::open_light`]).
    graph: &'f UnitGraph<'f>,
    /// Optional decoded `wasm64-blaze-wasip1` host-unit graph carried beside `graph` so
    /// `script_deps_digest` includes WASI-side unified dependency features (Rule 21).
    wasm_graph: Option<&'f UnitGraph<'f>>,
    /// Lightweight toolchain + script-cfg world when `Names` has not built the full world yet.
    script_world: Option<&'f blaze_world::ScriptWorld<'f>>,
    /// (subject, script unit, side), sorted: each run a crate of the world reads, by the world's
    /// own subject (names.rs `Names::script_runs`): the runs a `Script` part can name.
    by_subject: blaze_own::Vec<(ContentId, UnitId, Side), &'f Frame>,
    sources: StoreSources<'f, 'f, 'f, SourcesDir<'f>>,
    lock: &'f Lock<'f>,
    /// Each checkout path package's lock entry, folder (checkout-relative), and kind (member vs
    /// external), copied into the request's frame.
    members: &'f [blaze_world::CheckoutPath<'f>],
    /// Each fetched package's files as first located ([`files_of`]), sorted by lock id: a
    /// package's manifests are laid out once a command, not once for each job that runs its
    /// script or is served by it as a `links` producer (rule 77).
    files: std::vec::Vec<(LockId, Files<'f>), Global>,
    /// Memoized checkout path-package `files_digest` hex strings (in `frame`), sorted by `LockId`,
    /// so `script_deps_digest_with` hashes each path build-dependency at most once per command.
    dep_files: blaze_own::Vec<(LockId, Option<&'f str>), &'f Frame>,
    /// The shared setups opened ([`Scripts::shared`]): 1 when a job was made ready, else 0.
    setups_opened: u32,
}

/// Why the command's scripts could not be set up: every `Script` task fails with these
/// words (a hole: a fact outside every key, asked again next command).
#[derive(Debug)]
pub(crate) enum SetupError<'f> {
    NoHome,
    NoLock,
    NoMembers,
    Runtime(&'f str),
    /// The shared setup (blaze-world `script_setting::ScriptSetupError`, rendered): a side's
    /// probe table (a toolchain crate's library features not listed) or the spawn setting.
    Setting(&'f str),
    Sources(&'f str),
}

impl core::fmt::Display for SetupError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SetupError::NoHome => f.write_str("HOME is not set: the build-script module store and the native setting are under it"),
            SetupError::NoLock => f.write_str("no lock was read this command: a build script's package is pinned by it"),
            SetupError::NoMembers => f.write_str("the workspace's members were not matched to the lock this command: a member's script has no folder"),
            SetupError::Runtime(words) => write!(f, "the build-script runtime: {words}"),
            SetupError::Setting(words) => write!(f, "the build scripts' setting: {words}"),
            SetupError::Sources(words) => write!(f, "the fetched sources: {words}"),
        }
    }
}

/// The fetched sources' records, `<home>/.blaze/v2/sources/<hex of the key>`, MAPPED read-only
/// and read in place, as graph.rs `Mapped` does (rule 51): a package's record (a `-sys` crate's
/// is megabytes) is never copied into the request's frame, which holds only the mapping's
/// handle. `StoreSources` opens each record once a command.
pub(crate) struct SourcesDir<'f> {
    /// The sources store's folder as the machine names it (fetch.rs `sources_native`).
    folder: &'f str,
    frame: &'f Frame,
}

impl<'f> Records<'f> for SourcesDir<'f> {
    fn record(&mut self, source: &blaze_fetch::SourceId<'_>) -> Option<&'f [u8]> {
        let hex = crate::fetch::key_hex(&source.key());
        let mut path = std::string::String::with_capacity(self.folder.len() + 1 + hex.len());
        path.push_str(self.folder);
        path.push('/');
        path.extend(hex.iter().map(|digit| char::from(*digit)));
        // A record that cannot be mapped is the package's `NotFetched` (blaze-unit-graph
        // `SourceError`), named by its package and key: the same answer a missing one gets.
        let mapping = blaze_core::Mapping::map_file(std::path::Path::new(&path)).ok()?;
        // Kept for the command: the view over its bytes is read by every later job.
        let mapping: &'f blaze_core::Mapping = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(mapping, self.frame));
        Some(mapping.bytes())
    }
}

/// The library-features records (blaze-world lib_features_record.rs) in the command's store:
/// a hit is read in place and never walks (rule 77); a miss is walked once and put. Each record
/// read or put is kept under the toolchain's root ([`Store::keep_with_toolchain`]): its key is
/// of the toolchain and blaze's build alone, so it lives as long as the toolchain's crates'
/// records, not as long as one checkout's answer.
pub(crate) struct CommandFeatures<'c, 's, 'm> {
    pub(crate) cx: &'c Cx,
    pub(crate) store: &'s mut dyn Store<'m>,
    /// Owned copy of the last record read (`Store::get_into`), so reading an uncommitted
    /// features record never pins its shared arena in `Lent::kept_arenas`.
    pub(crate) held: bstd::mem::Bytes<'c>,
}

impl blaze_world::lib_features_record::FeatureRecords for CommandFeatures<'_, '_, '_> {
    type Error = CliError;

    fn get(&mut self, key: ContentId) -> Result<Option<&[u8]>, CliError> {
        self.held = bstd::mem::Bytes::new_in(self.cx.frame(), "a library-features record");
        if !self.store.get_into(self.cx, blaze_drive::Key(key), &mut self.held)? {
            return Ok(None);
        }
        self.store.keep_with_toolchain(blaze_drive::Key(key))?;
        Ok(Some(self.held.as_slice()))
    }

    fn put(&mut self, key: ContentId, bytes: &[u8]) -> Result<(), CliError> {
        self.store.put(self.cx, blaze_drive::Key(key), bytes)?;
        self.store.keep_with_toolchain(blaze_drive::Key(key))
    }
}

/// `bytes` copied into `frame`, held as long as it is.
fn held_bytes<'f>(bytes: &[u8], frame: &'f Frame) -> &'f [u8] {
    frame.alloc_slice_copy(bytes)
}

/// Words held in `frame`.
fn said<'f>(frame: &'f Frame, words: core::fmt::Arguments<'_>) -> &'f str {
    let text = bstd::mem::format_in!(frame, "{words}");
    crate::work::held_str(text.as_str(), frame)
}

/// Empty checkout tree for [`blaze_world::script_world`]: toolchain packs never read tree 0.
struct NoCheckout;

impl blaze_world::Tree for NoCheckout {
    fn file(&self, _path: &str) -> Option<&[u8]> {
        None
    }
}

static NO_CHECKOUT: NoCheckout = NoCheckout;

/// Places only checkout path packages for [`blaze_world::open_list`], without touching any
/// fetched registry or git package.
struct MembersOnly<'f> {
    members: &'f [blaze_world::CheckoutPath<'f>],
    lock: &'f Lock<'f>,
    bump: blaze_own::Bump<'f>,
}

impl<'f> blaze_world::Packages<'f> for MembersOnly<'f> {
    fn place(&mut self, package: LockId) -> Result<blaze_world::Place<'f>, blaze_world::WorldError<'f>> {
        match self.members.iter().find(|entry| entry.id == package) {
            Some(blaze_world::CheckoutPath { dir, kind: blaze_world::PathKind::Member, .. }) => Ok(blaze_world::Place::Checkout { dir }),
            Some(blaze_world::CheckoutPath { dir, kind: blaze_world::PathKind::External, .. }) => Ok(blaze_world::Place::Path { dir }),
            None => Err(blaze_world::WorldError::PathNotPlaced { package: self.bump.alloc_str(&self.lock.package(package).name) }),
        }
    }
}

impl<'f> Scripts<'f> {
    /// The command's scripts, made once at its first `Script` job from an already-made names world.
    pub(crate) fn open(
        cx: &Cx,
        env: &FetchEnv<'_>,
        names: &crate::names::NamesWorld<'_>,
        graph_record: &'f [u8],
        lock: Option<&'f Lock<'static>>,
        members: Option<&[crate::graph::CheckoutDir]>,
        frame: &'f Frame,
    ) -> Result<Scripts<'f>, SetupError<'f>> {
        let by_subject = {
            let _cx = cx.span("scripts: the world's runs by subject");
            names.script_runs(frame)
        };
        Self::finish_open(cx, env, names.graph(), names.wasm_graph(), None, by_subject, graph_record, lock, members, frame)
    }

    /// Lightweight scripts setup when no `Names` task has built the full world yet: decodes the
    /// unit graph and opens only the toolchain shelf (for `ScriptSetup::open_recorded`) and
    /// script-reading crates' `cfg`s, without mapping or laying out any fetched package.
    #[expect(clippy::too_many_arguments, reason = "lightweight Scripts setup taking graph and optional wasm64 host graph")]
    pub(crate) fn open_light(
        cx: &Cx,
        env: &FetchEnv<'_>,
        names: &crate::names::Names<'f>,
        graph_record: &'f [u8],
        wasm_graph_record: &'f [u8],
        lock: Option<&'f Lock<'static>>,
        members: Option<&[crate::graph::CheckoutDir]>,
        frame: &'f Frame,
    ) -> Result<Scripts<'f>, SetupError<'f>> {
        let home = env.home.ok_or(SetupError::NoHome)?;
        let lock_ref: &'f Lock<'f> = lock.ok_or(SetupError::NoLock)?;
        let listed_members = members.ok_or(SetupError::NoMembers)?;
        let mut held_members = blaze_own::Vec::with_capacity_in(listed_members.len(), frame);
        for entry in listed_members.iter() {
            held_members.push(blaze_world::CheckoutPath { id: entry.id, dir: crate::work::held_str(entry.dir.as_str(), frame), kind: entry.kind });
        }
        let member_slice: &'f [blaze_world::CheckoutPath<'f>] = held_members.into_bump_slice();
        let bump: blaze_own::Bump<'f> = frame.alloc(Erased::new(frame)).bump();
        let decoded = {
            let _cx = cx.span("scripts: unit graph decoded");
            blaze_unit_graph::record::decode(graph_record, bump.alloc()).map_err(|e| SetupError::Setting(said(frame, format_args!("unit graph decode: {e}"))))?
        };
        let decoded: &'f blaze_unit_graph::record::Decoded<'f> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(decoded, frame));
        let graph: &'f UnitGraph<'f> = &decoded.graph;
        let wasm_graph: Option<&'f UnitGraph<'f>> = if wasm_graph_record.is_empty() {
            None
        } else {
            let wd = blaze_unit_graph::record::decode(wasm_graph_record, bump.alloc())
                .map_err(|e| SetupError::Setting(said(frame, format_args!("wasm64 unit graph decode: {e}"))))?;
            let wd: &'f blaze_unit_graph::record::Decoded<'f> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(wd, frame));
            Some(&wd.graph)
        };
        let mut shelf = {
            let _cx = cx.span("scripts: toolchain shelf opened");
            crate::toolchain::HomeShelf::open(cx, &env.fs, home, frame).map_err(|e| SetupError::Setting(said(frame, format_args!("toolchain shelf: {e}"))))?
        };
        let mut packages = MembersOnly { members: member_slice, lock: lock_ref, bump };
        let listed = {
            let _cx = cx.span("scripts: toolchain list opened");
            blaze_world::open_list(&mut shelf, env.toolchain, crate::graph::TRIPLE, graph, lock_ref, &mut packages, bump)
                .map_err(|e| SetupError::Setting(said(frame, format_args!("script list: {e}"))))?
        };
        let listed: &'f blaze_world::Listed<'f, crate::toolchain::Held<'f>> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(listed, frame));
        let sw = {
            let _cx = cx.span("scripts: toolchain world made");
            blaze_world::script_world(graph, listed, &NO_CHECKOUT, bump).map_err(|e| SetupError::Setting(said(frame, format_args!("script world: {e}"))))?
        };
        let sw: &'f blaze_world::ScriptWorld<'f> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(sw, frame));
        let by_subject = names.script_runs(frame);
        Self::finish_open(cx, env, graph, wasm_graph, Some(sw), by_subject, graph_record, lock, members, frame)
    }

    #[expect(clippy::too_many_arguments, reason = "shared Scripts constructor")]
    fn finish_open(
        cx: &Cx,
        env: &FetchEnv<'_>,
        graph: &'f UnitGraph<'f>,
        wasm_graph: Option<&'f UnitGraph<'f>>,
        script_world: Option<&'f blaze_world::ScriptWorld<'f>>,
        by_subject: blaze_own::Vec<(ContentId, UnitId, Side), &'f Frame>,
        graph_record: &'f [u8],
        lock: Option<&'f Lock<'static>>,
        members: Option<&[crate::graph::CheckoutDir]>,
        frame: &'f Frame,
    ) -> Result<Scripts<'f>, SetupError<'f>> {
        if env.home.is_none() {
            return Err(SetupError::NoHome);
        }
        let lock: &'f Lock<'f> = lock.ok_or(SetupError::NoLock)?;
        let listed = members.ok_or(SetupError::NoMembers)?;
        let mut held = blaze_own::Vec::with_capacity_in(listed.len(), frame);
        for entry in listed.iter() {
            held.push(blaze_world::CheckoutPath { id: entry.id, dir: crate::work::held_str(entry.dir.as_str(), frame), kind: entry.kind });
        }
        let members: &'f [blaze_world::CheckoutPath<'f>] = held.into_bump_slice();
        let alloc: Alloc<'f> = env.heap.alloc();
        let _cx = cx.span("scripts: the fetched sources");
        let folder = crate::fetch::sources_native(env, frame).ok_or(SetupError::NoHome)?;
        let dir = SourcesDir { folder: crate::work::held_str(folder.as_str(), frame), frame };
        let sources = StoreSources::new(lock, dir, blaze_fetch::pack::PackView::open_source_put, alloc).map_err(|error| SetupError::Sources(said(frame, format_args!("{error}"))))?;
        Ok(Scripts { shared: None, graph_record, graph, wasm_graph, script_world, by_subject, sources, lock, members, files: std::vec::Vec::new_in(Global), dep_files: blaze_own::Vec::new_in(frame), setups_opened: 0 })
    }

    /// The shared setups opened this command: 1 once a job was made ready to run, else 0.
    pub(crate) fn setups_opened(&self) -> u32 {
        self.setups_opened
    }

    /// What every job of the command shares on the pool, opened once, at the first job made
    /// ready to run: the runtime under the home, each side's probe answers, the spawn setting and
    /// the native runner. `world` names each side's sysroot (blaze-world
    /// `script_setting::shipped`); `workers` is the command's worker count, which its native
    /// scripts' children share (the jobserver). `Err`: the setup's words, kept, so every later job
    /// fails with them and none opens it again.
    #[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or fact of it")]
    fn shared(&mut self, cx: &Cx, env: &FetchEnv<'_>, probes: CommandProbes<'_>, store: &mut dyn Store<'_>, tool: blaze_drive::Tool, workers: u32, world: &blaze_world::World<'_>, frame: &'f Frame) -> Result<&'f Shared<'f>, &'f str> {
        if let Some(opened) = self.shared {
            return opened;
        }
        let _cx = cx.span("scripts: the shared setup opened (the runtime, each side's probe answers, the native runner)");
        self.setups_opened = self.setups_opened.saturating_add(1);
        let opened = Scripts::open_shared(cx, env, probes, store, tool, workers, world, self.graph_record, frame).map_err(|error| said(frame, format_args!("{error}")));
        self.shared = Some(opened);
        opened
    }

    /// [`Scripts::shared`]'s one opening. Each side's library-features listing is read from
    /// `records` (the command's store) by its key, or walked once and put there ([`CommandFeatures`]); `tool` is the
    /// `Script` kind's tool, so a change of the walk's code moves the key (rule 52).
    #[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or fact of it")]
    fn open_shared(cx: &Cx, env: &FetchEnv<'_>, probes: CommandProbes<'_>, records: &mut dyn Store<'_>, tool: blaze_drive::Tool, workers: u32, world: &blaze_world::World<'_>, graph_record: &'f [u8], frame: &'f Frame) -> Result<&'f Shared<'f>, SetupError<'f>> {
        let home = env.home.ok_or(SetupError::NoHome)?;
        // The heap, for the setting: it lives to the command's end ([`Scripts::open`]).
        let alloc: Alloc<'f> = env.heap.alloc();
        let mut store = PathBuf::from(home);
        store.push(STORE_UNDER_HOME);
        let runtime = ScriptRuntime::new(&store).map_err(|error| SetupError::Runtime(said(frame, format_args!("{error}"))))?;
        let host = crate::graph::TRIPLE;
        // The native setting: only a script `placed` native needs it; without it that script
        // fails naming why ([`pooled`]), never run as wasm (D112). What this edge read (the
        // SDK resolved once, sdk.rs; the running blaze's folder, main.rs, beside which the fact
        // shim is, N7; the worker count) is checked here; the runner itself (the shim placed,
        // the jobserver made) is opened by the one setup both this command and the census's
        // exporter call (blaze-world `script_setting::ScriptSetup`, rule 30).
        let shim = env.exe_dir.map(|dir| std::format!("{dir}/{}", blaze_build_script::fact_shim::SHIM_NAME));
        let native_opts = match env.sdk {
            Err(refused) => Err(said(frame, format_args!("{refused}"))),
            Ok(sdk) => match blaze_proc_macro::native_setting::command_native_setting(home, sdk.folder(), NATIVE_WORK, host, alloc.clone()) {
                Err(error) => Err(said(frame, format_args!("the native runner's setting (D112): {error}"))),
                Ok(owned) => match (&shim, core::num::NonZeroU32::new(workers)) {
                    (Err(why), _) => Err(said(frame, format_args!("the native runner's fact shim (D112, N7): {why}"))),
                    (_, None) => Err(said(frame, format_args!("the command's worker count is 0: the native runner's jobserver needs at least one (scripts.rs)"))),
                    (Ok(shim), Some(workers)) => Ok(blaze_proc_macro::native_setting::NativeRunOpts { setting: owned, shim: shim.as_str(), workers }),
                },
            },
        };
        let spawn_snapshot = match probes.spawn_setting_snapshot(cx) {
            Ok(spawn) => spawn,
            Err(error) => return Err(SetupError::Setting(said(frame, format_args!("{error}")))),
        };
        let setup = {
            let _cx = cx.span("scripts: each side's probe answers (library features listed) and the native runner");
            let mut features = CommandFeatures { cx, store: records, held: bstd::mem::Bytes::new_in(cx.frame(), "a library-features record") };
            blaze_world::script_setting::ScriptSetup::open_recorded(
                blaze_world::script_setting::ScriptSetupOpts { world, target: host, macro_target: MACRO_TARGET, native: native_opts, spawn: Some(spawn_snapshot), alloc: alloc.clone() },
                tool.0,
                &mut features,
            )
            .map_err(|error| SetupError::Setting(said(frame, format_args!("{error}"))))?
        };
        let blaze_world::script_setting::ScriptSetup { table, macro_table, spawn, native } = setup;
        // Held to the command's end; its jobserver is ended, its fifo removed, when `Scripts`
        // drops.
        let native: Result<NativeSetting<'f>, &'f str> = match native {
            Ok(run) => {
                let run: &'f blaze_proc_macro::native_setting::NativeRun<'f> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(run, frame));
                Ok(run.setting())
            }
            Err(blaze_world::script_setting::NativeRefused::Caller(words)) => Err(words),
            Err(blaze_world::script_setting::NativeRefused::Open(error)) => Err(said(frame, format_args!("{error}"))),
        };
        // The edge's resolved root, copied into the frame the shared state lives in: never
        // resolved again here.
        let checkout = env.served.within(frame);
        let no_sdk = env.sdk.is_err();
        let c_probes = match probes.c_probes(cx) {
            Ok(Some(c_probes)) => c_probes,
            Ok(None) => return Err(SetupError::NoHome),
            Err(error) => return Err(SetupError::Setting(said(frame, format_args!("{error}")))),
        };
        let shared: &'f Shared<'f> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(Shared { result_alloc: env.heap.shared(), runtime, store, checkout, table, macro_table, spawn, c_probes, native, no_sdk, graph_record }, frame));
        Ok(shared)
    }

    fn unit_of(&self, name: Name) -> Option<(UnitId, Side)> {
        self.by_subject.binary_search_by_key(&name.subject, |(subject, _, _)| *subject).ok().map(|at| (self.by_subject[at].1, self.by_subject[at].2))
    }
}

impl Drop for Scripts<'_> {
    /// The command is over (its scripts' runs all ended): the jobserver's fifo is removed.
    fn drop(&mut self) {
        // No shared setup opened (no job was made ready): no jobserver was made.
        if let Some(Ok(shared)) = self.shared
            && let Ok(native) = &shared.native
        {
            end_jobs(native.jobs);
        }
    }
}

/// Stable semantic class of a dependency build-script probe/pin setup failure (hashed into
/// error-tagged task keys instead of diagnostic prose or machine paths, Rule 21).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ProbeErrorKind {
    CProbesOpen = 1,
    SpawnPinsOpen = 2,
    AnswererIdentity = 3,
    WasmGraphMissing = 4,
    WasmRootMissing = 5,
    FrameReserve = 6,
    PlannedDrift = 7,
}

impl ProbeErrorKind {
    pub(crate) const fn tag(self) -> u8 {
        match self {
            ProbeErrorKind::CProbesOpen => 1,
            ProbeErrorKind::SpawnPinsOpen => 2,
            ProbeErrorKind::AnswererIdentity => 3,
            ProbeErrorKind::WasmGraphMissing => 4,
            ProbeErrorKind::WasmRootMissing => 5,
            ProbeErrorKind::FrameReserve => 6,
            ProbeErrorKind::PlannedDrift => 7,
        }
    }
}

/// Error initializing shared command-level C-probe or derived-fact pin settings.
#[derive(Debug)]
pub(crate) struct ProbeSetupError {
    pub(crate) kind: ProbeErrorKind,
    pub(crate) detail: blaze_own::String<blaze_own::SharedAlloc<'static>>,
}

impl ProbeSetupError {
    pub(crate) fn new(kind: ProbeErrorKind, detail: blaze_own::String<blaze_own::SharedAlloc<'static>>) -> Self {
        Self { kind, detail }
    }

    pub(crate) fn from_str(kind: ProbeErrorKind, detail: &str, shared: blaze_own::SharedAlloc<'static>) -> Self {
        Self { kind, detail: blaze_own::String::from_str_in(detail, shared) }
    }

    pub(crate) fn duplicate(&self) -> Self {
        Self::from_str(self.kind, self.detail.as_str(), *self.detail.allocator())
    }
}

impl core::fmt::Display for ProbeSetupError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.detail.as_str())
    }
}

/// Fault-injection plant for [`SharedCommandProbes`] DST negative controls (`AGENTS.md` `dst-not-review`).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeOncePlant {
    /// Production behavior: coalesce concurrent initializers through `bstd::sync::Once`.
    None,
    /// Planted bug: bypass `bstd::sync::Once` so racing callers initialize independently.
    SkipOnce,
}

/// Test-only bug plant for [`CheckedHexSlot`] DST negative controls (rule 23).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckedHexPlant {
    None,
    /// Skips seqlock odd/mismatch verification so a concurrent mid-write read observes torn words.
    SkipSeqCheck,
}

/// Lock-free updatable `Option<Hex64>` snapshot (`Send + Sync`, zero allocations, zero `std`).
///
/// Uses a seqlock over the 32-byte `ContentId` representation of `Hex64` so a completed check
/// (certificate hit, accepted ready preflight, phase planning, or pinned JIT launch) can always
/// publish or clear the exact dependency-probe tag it selected — overwriting any earlier
/// preflight observation without blocking or allocating.
struct CheckedHexSlot {
    seq: core::sync::atomic::AtomicU64,
    present: core::sync::atomic::AtomicBool,
    words: [core::sync::atomic::AtomicU64; 4],
    #[cfg(test)]
    plant: CheckedHexPlant,
}

impl CheckedHexSlot {
    fn new() -> Self {
        Self {
            seq: core::sync::atomic::AtomicU64::new(0),
            present: core::sync::atomic::AtomicBool::new(false),
            words: [
                core::sync::atomic::AtomicU64::new(0),
                core::sync::atomic::AtomicU64::new(0),
                core::sync::atomic::AtomicU64::new(0),
                core::sync::atomic::AtomicU64::new(0),
            ],
            #[cfg(test)]
            plant: CheckedHexPlant::None,
        }
    }

    #[cfg(test)]
    fn new_with_plant(plant: CheckedHexPlant) -> Self {
        Self {
            seq: core::sync::atomic::AtomicU64::new(0),
            present: core::sync::atomic::AtomicBool::new(false),
            words: [
                core::sync::atomic::AtomicU64::new(0),
                core::sync::atomic::AtomicU64::new(0),
                core::sync::atomic::AtomicU64::new(0),
                core::sync::atomic::AtomicU64::new(0),
            ],
            plant,
        }
    }

    fn acquire_write(&self) -> u64 {
        use core::sync::atomic::Ordering;
        let mut seq = self.seq.load(Ordering::Relaxed);
        loop {
            if seq & 1 != 0 {
                core::hint::spin_loop();
                seq = self.seq.load(Ordering::Relaxed);
                continue;
            }
            let odd = seq.wrapping_add(1);
            match self.seq.compare_exchange_weak(seq, odd, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => return odd,
                Err(actual) => seq = actual,
            }
        }
    }

    fn publish(&self, hex: crate::wasm64_compile::Hex64) {
        self.publish_with_hook(hex, || {});
    }

    fn publish_with_hook(&self, hex: crate::wasm64_compile::Hex64, on_mid_write: impl FnOnce()) {
        use core::sync::atomic::Ordering;
        let odd = self.acquire_write();
        let bytes = hex.to_content_id().0;
        let mut hook = Some(on_mid_write);
        for (idx, (dst, chunk)) in self.words.iter().zip(bytes.chunks_exact(8)).enumerate() {
            if let Ok(arr) = <[u8; 8]>::try_from(chunk) {
                dst.store(u64::from_le_bytes(arr), Ordering::Relaxed);
            }
            if idx == 1
                && let Some(cb) = hook.take()
            {
                cb();
            }
        }
        self.present.store(true, Ordering::Relaxed);
        self.seq.store(odd.wrapping_add(1), Ordering::Release);
    }

    fn clear(&self) {
        use core::sync::atomic::Ordering;
        let odd = self.acquire_write();
        self.present.store(false, Ordering::Relaxed);
        self.seq.store(odd.wrapping_add(1), Ordering::Release);
    }

    /// Single non-blocking seqlock read attempt: returns `Ok(val)` when a consistent snapshot was
    /// observed, or `Err(())` when a concurrent write was in progress and the caller must retry.
    fn try_load_once(&self) -> Result<Option<crate::wasm64_compile::Hex64>, ()> {
        use core::sync::atomic::Ordering;
        let [w0, w1, w2, w3] = &self.words;
        let s1 = self.seq.load(Ordering::Acquire);
        #[cfg(test)]
        let skip_seq = matches!(self.plant, CheckedHexPlant::SkipSeqCheck);
        #[cfg(not(test))]
        let skip_seq = false;
        if !skip_seq && (s1 & 1 != 0) {
            return Err(());
        }
        let present = self.present.load(Ordering::Relaxed);
        let words = [
            w0.load(Ordering::Relaxed),
            w1.load(Ordering::Relaxed),
            w2.load(Ordering::Relaxed),
            w3.load(Ordering::Relaxed),
        ];
        core::sync::atomic::fence(Ordering::Acquire);
        let s2 = self.seq.load(Ordering::Relaxed);
        if !skip_seq && s1 != s2 {
            return Err(());
        }
        if !present {
            return Ok(None);
        }
        let mut bytes = [0u8; 32];
        for (dst, word) in bytes.chunks_exact_mut(8).zip(words) {
            dst.copy_from_slice(&word.to_le_bytes());
        }
        Ok(Some(crate::wasm64_compile::Hex64::from_bytes(bytes)))
    }

    fn load(&self) -> Option<crate::wasm64_compile::Hex64> {
        loop {
            match self.try_load_once() {
                Ok(val) => return val,
                Err(()) => core::hint::spin_loop(),
            }
        }
    }
}

/// Lifetime-free command-level lazy holder for `CProbeSetting` and derived-fact `SpawnSetting`,
/// instantiated once at the command's edge and paired with `&FetchEnv` via [`CommandProbes`] so:
/// - Zero home/clang-shelf/userland I/O runs until actually needed (workspace `Script` run, or a
///   unit whose wasm64 closure runs dependency build scripts).
/// - `open_command_c_probes` (`HermeticClangProbe`'s `verify_flight` and `VerifiedExec`) and
///   `fact_pins::command_spawn_setting` are each opened at most once per command and reused
///   between planning, wave-0 self-compiles/keying, and wave-1 `Script` runs (Rule 30).
/// - `fact_pins::command_spawn_setting` errors are preserved and propagated verbatim (Rule 3).
pub(crate) struct SharedCommandProbes {
    c_probes: bstd::sync::Once<Option<blaze_proc_macro::probe_clang::CProbeSetting>>,
    spawn_setting: bstd::sync::Once<Result<SpawnSetting, ProbeSetupError>>,
    /// Dependency-probe identity published by the completed `check` (or pinned certificate),
    /// reused by start-time proc-macro runners (`RunProcs` via `Standing`) so `run`/`test`/`export`
    /// look up the exact `MacroModule` that `check` selected and produced even if a host closure
    /// shifts afterwards or shifted between an earlier preflight and phase planning.
    checked_dep_probe_hex: CheckedHexSlot,
    #[cfg(test)]
    plant: ProbeOncePlant,
}

impl SharedCommandProbes {
    /// Creates an uninitialized lazy holder (zero I/O, zero allocations).
    pub(crate) fn new() -> Self {
        Self {
            c_probes: bstd::sync::Once::new("command.c_probes"),
            spawn_setting: bstd::sync::Once::new("command.spawn_setting"),
            checked_dep_probe_hex: CheckedHexSlot::new(),
            #[cfg(test)]
            plant: ProbeOncePlant::None,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_plant(plant: ProbeOncePlant) -> Self {
        Self {
            c_probes: bstd::sync::Once::new("command.c_probes"),
            spawn_setting: bstd::sync::Once::new("command.spawn_setting"),
            checked_dep_probe_hex: CheckedHexSlot::new(),
            plant,
        }
    }

    /// Pairs this shared command holder with the resolved command `env` (zero cost, `Copy`).
    pub(crate) fn with_env<'a>(&'a self, env: &'a FetchEnv<'a>) -> CommandProbes<'a> {
        CommandProbes { inner: self, env, sdk: env.sdk, reuse_checked_dep_probes: false }
    }

    /// Publishes the verified dependency-probe identity selected by the completed check
    /// (certificate hit, accepted ready preflight, phase planning, or pinned JIT launch),
    /// overwriting any earlier preflight observation so start-time `RunProcs` with
    /// `.with_checked_snapshot()` always look up the exact `MacroModule` tag produced.
    pub(crate) fn publish_checked_dep_probe_hex(&self, hex: crate::wasm64_compile::Hex64) {
        self.checked_dep_probe_hex.publish(hex);
    }

    /// Clears any previously published dependency-probe snapshot when the completed check has no
    /// verified probe-dependent `MacroModule` tag.
    pub(crate) fn clear_checked_dep_probe_hex(&self) {
        self.checked_dep_probe_hex.clear();
    }

    #[cfg(test)]
    pub(crate) fn c_probes_initialized(&self) -> bool {
        self.c_probes.get().is_some()
    }

    #[cfg(test)]
    pub(crate) fn spawn_setting_initialized(&self) -> bool {
        self.spawn_setting.get().is_some()
    }
}

/// Borrowed handle combining the single command-level [`SharedCommandProbes`] with the command's
/// resolved [`FetchEnv`] and macOS SDK snapshot.
#[derive(Clone, Copy)]
pub(crate) struct CommandProbes<'a> {
    inner: &'a SharedCommandProbes,
    env: &'a FetchEnv<'a>,
    sdk: Result<blaze_apple_sdk::Sdk<'a>, blaze_apple_sdk::SdkRefused<'a>>,
    /// When `true` (start-time `RunProcs` via `Standing`), `dep_probe_identity_hex` reuses the
    /// verified check-time digest stored in `inner.checked_dep_probe_hex` instead of rescanning.
    reuse_checked_dep_probes: bool,
}

impl<'a> CommandProbes<'a> {
    /// Shared allocator handle carried by this command's resolved environment.
    pub(crate) fn shared(&self) -> blaze_own::SharedAlloc<'static> {
        self.env.heap.shared()
    }

    /// Overrides the macOS SDK snapshot carried by this probe handle (used by start-time
    /// `RunProcs` so `run`/`test`/`export` share the exact SDK resolved by `check` without
    /// cloning `FetchEnv` handles).
    pub(crate) fn with_sdk(
        self,
        sdk: Result<blaze_apple_sdk::Sdk<'a>, blaze_apple_sdk::SdkRefused<'a>>,
    ) -> Self {
        Self { sdk, ..self }
    }

    /// Marks this handle as a start-time consumer (`run`/`test`/`export` via `Standing`) that
    /// reuses the verified check-time probe identity snapshot when present.
    pub(crate) fn with_checked_snapshot(self) -> Self {
        Self { reuse_checked_dep_probes: true, ..self }
    }

    /// Lazily opens and shares the single command-level `CProbeSetting` (`Ok(None)` when `HOME`
    /// is unset).
    pub(crate) fn c_probes(
        &self,
        cx: &Cx,
    ) -> Result<Option<blaze_proc_macro::probe_clang::CProbeSetting>, ProbeSetupError> {
        self.c_probes_with(cx, |cx| {
            self.env.home.map(|home| {
                open_c_probes_with_sdk(cx, self.env, self.sdk, home, crate::graph::TRIPLE, self.env.heap.alloc())
            })
        })
    }

    fn c_probes_with(
        &self,
        cx: &Cx,
        open: impl FnOnce(&Cx) -> Option<blaze_proc_macro::probe_clang::CProbeSetting>,
    ) -> Result<Option<blaze_proc_macro::probe_clang::CProbeSetting>, ProbeSetupError> {
        #[cfg(test)]
        if matches!(self.inner.plant, ProbeOncePlant::SkipOnce) {
            return Ok(open(cx));
        }
        let shared = self.env.heap.shared();
        let slot = self
            .inner
            .c_probes
            .get_or_make(cx, |cx| Ok::<_, core::convert::Infallible>(open(cx)))
            .map_err(|error| {
                let mut msg = blaze_own::String::new_in(shared);
                let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("opening command C probes: {error}"));
                ProbeSetupError::new(ProbeErrorKind::CProbesOpen, msg)
            })?;
        Ok(slot.as_ref().map(blaze_own::Share::share))
    }

    /// Lazily computes the command's derived-fact `SpawnSetting` (`command_spawn_setting`),
    /// propagating any `CommandPinsError` verbatim (never falling back to `SpawnSetting::off()`).
    pub(crate) fn spawn_setting(&self, cx: &Cx) -> Result<&'a SpawnSetting, ProbeSetupError> {
        let shared = self.env.heap.shared();
        self.spawn_setting_with(cx, |_cx| open_spawn_setting(self.env, shared))
    }

    fn spawn_setting_with(
        &self,
        cx: &Cx,
        open: impl FnOnce(&Cx) -> Result<SpawnSetting, ProbeSetupError>,
    ) -> Result<&'a SpawnSetting, ProbeSetupError> {
        let shared = self.env.heap.shared();
        #[cfg(test)]
        if matches!(self.inner.plant, ProbeOncePlant::SkipOnce) {
            // Planted bug: invoke `open(cx)` on every call bypassing `Once` so the DST oracle
            // immediately catches duplicate initialization.
            let opened = open(cx);
            let slot = self
                .inner
                .spawn_setting
                .get_or_make(cx, |_cx| Ok::<_, core::convert::Infallible>(opened))
                .map_err(|error| {
                    let mut msg = blaze_own::String::new_in(shared);
                    let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("opening command spawn pins: {error}"));
                    ProbeSetupError::new(ProbeErrorKind::SpawnPinsOpen, msg)
                })?;
            return match slot {
                Ok(setting) => Ok(setting),
                Err(why) => Err(why.duplicate()),
            };
        }
        let slot = self
            .inner
            .spawn_setting
            .get_or_make(cx, |cx| Ok::<_, core::convert::Infallible>(open(cx)))
            .map_err(|error| {
                let mut msg = blaze_own::String::new_in(shared);
                let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("opening command spawn pins: {error}"));
                ProbeSetupError::new(ProbeErrorKind::SpawnPinsOpen, msg)
            })?;
        match slot {
            Ok(setting) => Ok(setting),
            Err(why) => Err(why.duplicate()),
        }
    }

    /// Returns an owned hermetic snapshot of the single command-level `SpawnSetting` (`Once`
    /// cached and shared between workspace `Script` execution and wasm64 dep-script execution /
    /// keying).
    pub(crate) fn spawn_setting_snapshot(&self, cx: &Cx) -> Result<SpawnSetting, ProbeSetupError> {
        let shared = self.env.heap.shared();
        self.spawn_setting_snapshot_with(cx, |_cx| open_spawn_setting(self.env, shared))
    }

    fn spawn_setting_snapshot_with(
        &self,
        cx: &Cx,
        open: impl FnOnce(&Cx) -> Result<SpawnSetting, ProbeSetupError>,
    ) -> Result<SpawnSetting, ProbeSetupError> {
        #[cfg(test)]
        if matches!(self.inner.plant, ProbeOncePlant::SkipOnce) {
            return match open(cx) {
                Ok(setting) => Ok(crate::wasm64_compile::dep_script_spawn_setting(Some(&setting))),
                Err(why) => Err(why),
            };
        }
        let shared = self.env.heap.shared();
        let slot = self
            .inner
            .spawn_setting
            .get_or_make(cx, |cx| Ok::<_, core::convert::Infallible>(open(cx)))
            .map_err(|error| {
                let mut msg = blaze_own::String::new_in(shared);
                let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("opening command spawn pins: {error}"));
                ProbeSetupError::new(ProbeErrorKind::SpawnPinsOpen, msg)
            })?;
        match slot {
            Ok(setting) => Ok(crate::wasm64_compile::dep_script_spawn_setting(Some(setting))),
            Err(why) => Err(why.duplicate()),
        }
    }

    /// Computes the deterministic 64-hex digest of all probe/pin inputs observable by a dependency
    /// build script — including the shared `SpawnSetting` derived-fact pins AND the live C-probe
    /// answerer's identity bounded by `cx` (`ready.answerer_id_in_cx(cx, alloc)` on Linux, which
    /// folds the live host ELF runtime closure of `clang-23`) — reusing the single coalesced
    /// command-level `CProbeSetting`.
    pub(crate) fn dep_probe_identity_hex(
        &self,
        cx: &Cx,
    ) -> Result<crate::wasm64_compile::Hex64, ProbeSetupError> {
        if self.reuse_checked_dep_probes
            && let Some(hex) = self.inner.checked_dep_probe_hex.load()
        {
            return Ok(hex);
        }
        let spawn = self.spawn_setting(cx)?;
        let c_probes = self.c_probes(cx)?;
        let mut sdk_buf = [0u8; blaze_apple_sdk::SdkId::KEY_LEN * 2];
        let sdk_id = encode_sdk_hex(self.sdk, &mut sdk_buf).map(|(_, id)| id);
        crate::wasm64_compile::dep_probe_identity_from_setting(
            cx,
            crate::graph::TRIPLE,
            spawn,
            c_probes.as_ref(),
            sdk_id,
            self.env.heap.alloc(),
            self.env.heap.shared(),
        )
    }

    #[cfg(test)]
    pub(crate) fn c_probes_initialized(&self) -> bool {
        self.inner.c_probes_initialized()
    }

    #[cfg(test)]
    pub(crate) fn spawn_setting_initialized(&self) -> bool {
        self.inner.spawn_setting_initialized()
    }
}

fn open_spawn_setting(
    env: &FetchEnv<'_>,
    shared: blaze_own::SharedAlloc<'static>,
) -> Result<SpawnSetting, ProbeSetupError> {
    blaze_proc_macro::fact_pins::command_spawn_setting(crate::graph::TRIPLE, env.heap.alloc()).map_err(|error| {
        let mut msg = blaze_own::String::new_in(shared);
        let _ = core::fmt::Write::write_fmt(
            &mut msg,
            format_args!("the command's spawn setting (the derived facts' pins): {error}"),
        );
        ProbeSetupError::new(ProbeErrorKind::SpawnPinsOpen, msg)
    })
}

/// Encodes any resolved Apple SDK identity into `(folder, 64-hex-id)` on `buf`.
fn encode_sdk_hex<'a>(
    sdk: Result<blaze_apple_sdk::Sdk<'a>, blaze_apple_sdk::SdkRefused<'a>>,
    buf: &'a mut [u8; blaze_apple_sdk::SdkId::KEY_LEN * 2],
) -> Option<(&'a str, &'a str)> {
    let sdk = sdk.ok()?;
    let key = sdk.identity().key();
    for (i, byte) in key.iter().enumerate() {
        let hi = byte >> 4;
        let lo = byte & 15;
        if let Some(slot) = buf.get_mut(2 * i) {
            *slot = if hi < 10 { b'0' + hi } else { b'a' + (hi - 10) };
        }
        if let Some(slot) = buf.get_mut(2 * i + 1) {
            *slot = if lo < 10 { b'0' + lo } else { b'a' + (lo - 10) };
        }
    }
    let id_str = core::str::from_utf8(buf).unwrap_or("");
    Some((sdk.folder(), id_str))
}

/// Opens the command's hermetic C compiler probe setting (`probe_clang::open_command_c_probes`)
/// for `host`, encoding any resolved Apple SDK identity into the `(folder, hex_id)` pair.
/// Called lazily at most once per command through [`SharedCommandProbes`] (rule 30).
fn open_c_probes_with_sdk(
    cx: &Cx,
    env: &FetchEnv<'_>,
    sdk: Result<blaze_apple_sdk::Sdk<'_>, blaze_apple_sdk::SdkRefused<'_>>,
    home: &str,
    host: &str,
    alloc: Alloc<'_>,
) -> blaze_proc_macro::probe_clang::CProbeSetting {
    let mut sdk_hex = [0u8; blaze_apple_sdk::SdkId::KEY_LEN * 2];
    let sdk_pair = encode_sdk_hex(sdk, &mut sdk_hex);
    blaze_proc_macro::probe_clang::open_command_c_probes(cx, &env.fs, home, host, sdk_pair, alloc)
}

/// The command's jobserver ended: its descriptor closed, its fifo removed. A drop cannot return
/// the failure, so it is said on standard error naming the fifo (rule 3: never quiet).
fn end_jobs(jobs: &blaze_proc_macro::jobserver::Jobserver) {
    if let Err(error) = jobs.end() {
        _ = std::io::Write::write_fmt(&mut std::io::stderr(), format_args!("blaze: {error}\n"));
    }
}

/// Where a script's package files come from, as a job carries them to its worker.
#[derive(Clone, Copy)]
enum Files<'j> {
    /// A workspace member at `dir` (checkout-relative): its script sees `/workspace/<dir>`.
    Member { dir: &'j str },
    /// An external checkout path package (`[patch.*]` or excluded path dep) at `dir`
    /// (checkout-relative): its script is confined to `/package` (its own folder, never the whole
    /// workspace).
    External { dir: &'j str },
    /// A fetched package's record: its files under `dir`; a git source answers `git` from the
    /// lock's commit (D54).
    Pack { view: blaze_fetch::pack::PackView<'j>, dir: &'j str, git: Option<blaze_build_script::git::Oid> },
}

/// One `Script` task's job: all of it `Sync`, so it runs on any worker.
pub(crate) struct ScriptJob<'j> {
    name: Name,
    shared: &'j Shared<'j>,
    script: UnitId,
    /// The side of the crates that read it: a macro-side run has TARGET [`MACRO_TARGET`].
    side: Side,
    /// The cfg of the crate of the list that reads this run (its world row's, sorted): a
    /// macro-side run's `CARGO_CFG_*` (wasm32-wasip1's atoms, the profile's, its features).
    cfg: &'j [(&'j str, Option<&'j str>)],
    files: Files<'j>,
    /// Each script of its `links` closure ([`links_closure`]), sorted by unit, each once.
    producers: &'j [Producer<'j>],
    /// What a member's run that read the checkout gives ([`CheckoutReads`]): a hole unless its
    /// caller keeps the reads as cells.
    pub(crate) checkout: CheckoutReads,
    /// Transitive build-dependency digest (`deps <sha256>`, rule 21) when the build script has
    /// build-dependencies; `None` when it has none.
    deps_digest: Option<&'j str>,
    /// The verified planned runner matching the `RanAs` that named the `Script` and `MacroModule`
    /// tasks: reused directly at execution time (Rule 21, never independently recomputed).
    planned_runner: Runner,
    /// When source classification participated in `planned_runner`, the exact `NativeSourceReason`
    /// and SHA-256 snapshot fingerprint observed at job preparation time so `run` refuses any
    /// mid-command source mutation as a hole instead of filing a mismatched payload.
    planned_source: Option<(Option<blaze_build_script::native::NativeSourceReason>, [u8; 32])>,
}

/// What a member build script's reads of the checkout give (engine_scripts.rs sets it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CheckoutReads {
    /// The request planned no `Given` cells for the reads (engine_scripts.rs `plan_reads` not
    /// called): a run that read the checkout is a hole, its outputs not kept, run again next
    /// command; it has no cell for a read.
    Hole,
    /// The engine keeps the outputs when its cells or a producer's pin cover every observation.
    /// An unnameable read leaves the outputs usable for this command and the script owed.
    Kept { plant: crate::engine_script_reads::ReadsPlant, status: Option<crate::engine_script_reads::StatusView> },
}

/// A script whose run a reading script is served of: a direct `links` input (its metadata is
/// the reader's `DEP_<LINKS>_*`) or one upstream of those (its files serve a forwarded path).
struct Producer<'j> {
    script: UnitId,
    /// The `links` of the unit it serves.
    links: &'j str,
    /// Its `Script` name and the out its record had: a read of the reader.
    name: Name,
    out: ContentId,
    /// Its outputs record, as handed.
    bytes: &'j [u8],
    /// Where its package's files are: the tree it was served, which a path under it names.
    files: Files<'j>,
    /// A `links` input of the reader itself (not only upstream of one).
    direct: bool,
}

/// Where the package of `unit` has its files: a member's folder (checkout-relative), else its
/// fetched record, located once a command and kept in `found` (its folder in `frame`). `Err`:
/// the job's refusal, naming the package (a manifest of it refused, the manifest too).
fn files_of<'f: 'j, 'j>(
    sources: &mut StoreSources<'f, 'f, 'f, SourcesDir<'f>>,
    found: &mut std::vec::Vec<(LockId, Files<'f>), Global>,
    members: &'f [blaze_world::CheckoutPath<'f>],
    lock: &'f Lock<'f>,
    unit: &Unit<'_>,
    frame: &'f Frame,
    jobs: &'j Frame,
) -> Result<Files<'j>, Prepared<'j>> {
    if let Some(entry) = members.iter().find(|entry| entry.id == unit.package) {
        return Ok(match entry.kind {
            blaze_world::PathKind::Member => Files::Member { dir: entry.dir },
            blaze_world::PathKind::External => Files::External { dir: entry.dir },
        });
    }
    let at = match found.binary_search_by_key(&unit.package, |(id, _)| *id) {
        Ok(at) => return Ok(found[at].1),
        Err(at) => at,
    };
    match sources.files(unit.package) {
        Err(error @ blaze_unit_graph::SourceError::Layout { .. }) => {
            Err(Prepared::Said { what: "a build script's package manifests were refused", words: said(jobs, format_args!("the build script of {} {}: {error}", unit.name.as_str(), unit.version.as_str())), hole: true })
        }
        Ok((view, dir)) => {
            let git = match &lock.packages[unit.package.index()].source {
                LockSource::Git { commit, .. } => match blaze_build_script::git::Oid::parse_hex(commit.as_bytes()) {
                    Some(oid) => Some(oid),
                    None => {
                        return Err(Prepared::Said { what: "a git package whose lock commit is not 40 hex digits", words: said(jobs, format_args!("{} {}: the lock's commit `{commit}` is not 40 hex digits", unit.name.as_str(), unit.version.as_str())), hole: false });
                    }
                },
                LockSource::Path | LockSource::Registry { .. } => None,
            };
            let files: Files<'f> = Files::Pack { view, dir: crate::work::held_str(dir.as_str(), frame), git };
            found.insert(at, (unit.package, files));
            Ok(files)
        }
        Err(error) => Err(Prepared::Said { what: "a build script's package has no fetched sources", words: said(jobs, format_args!("the build script of {} {}: {error}", unit.name.as_str(), unit.version.as_str())), hole: true }),
    }
}

/// Both the run and the next command's replay use the checkout granted at the
/// command edge. Its host path is never a subject in the read record.
pub(crate) fn checkout_path(checkout: &str) -> &Path {
    Path::new(if checkout.is_empty() { "." } else { checkout })
}

/// The tree `files` serves a script of `package`, and the root it sees it at. `Err`: the
/// run's failure, naming the package and the folder.
fn tree_of(files: &Files<'_>, checkout: crate::engine_script_reads::Served<'_>, package: &str) -> Result<(Tree, ScriptRoot), Ran> {
    match files {
        Files::Member { dir } | Files::External { dir } => match (ScriptRoot::member(dir), checkout) {
            (Ok(root), crate::engine_script_reads::Served::Root(checkout)) => Ok((dir_tree(checkout), root)),
            (Ok(_), crate::engine_script_reads::Served::Simulated) => {
                Err(failed("a member's script with no served checkout", format_args!("the build script of {package}: its folder '{dir}': a simulated world serves no checkout on disk"), false))
            }
            (Err(error), _) => Err(failed("a member folder a script cannot see", format_args!("the build script of {package}: its folder '{dir}': {error}"), false)),
        },
        Files::Pack { view, dir, git } => {
            let Ok(frame) = bstd::mem::Frame::new(bstd::mem::Tag::Other) else {
                return Err(failed("a build script's decode frame failed", format_args!("the build script of {package}: out of memory"), false));
            };
            let mut decoded: blaze_own::Vec<(&str, blaze_fetch::FileBytes<'_, &bstd::mem::Frame>), &bstd::mem::Frame> =
                blaze_own::Vec::with_capacity_in(view.len(), &frame);
            let mut submodules: blaze_own::Vec<(&str, [u8; 20]), &bstd::mem::Frame> = blaze_own::Vec::new_in(&frame);
            for entry in view.entries() {
                if entry.mode == blaze_fetch::Mode::Submodule {
                    if let Some(commit) = entry.submodule_commit() {
                        submodules.push((entry.path, commit));
                    }
                    continue;
                }
                let bytes = entry.read_in(&frame).map_err(|why| {
                    failed("a build script's pack entry failed to decode", format_args!("the build script of {package}: {}: {}", entry.path, blaze_text::Shown(&why)), false)
                })?;
                decoded.push((entry.path, bytes));
            }
            Ok((
                mem_tree_with_submodules(
                    decoded.iter().map(|(p, b)| (*p, b.as_slice())),
                    submodules.iter().copied(),
                    dir,
                    *git,
                ),
                ScriptRoot::package(),
            ))
        }
    }
}

/// What making a job gave.
pub(crate) enum Prepared<'j> {
    Ready(ScriptJob<'j>),
    /// The inputs it lacks: the graph, or a `links` input's record.
    Need(std::vec::Vec<Want, Global>),
    /// It fails with these words; `hole` when the cause is outside its key (asked again).
    Said { what: &'static str, words: &'j str, hole: bool },
    /// A need of it failed this command (its module's lookup, a `links` producer): it fails
    /// `Waited` on it, alone (rule 20); asking again would loop.
    Waited(Name),
}

/// The `Script` job of `task`, made on the engine's scripts stage from record bytes
/// (engine_scripts.rs): the command's scripts opened at the first task, the unit found by its
/// subject, its `links` inputs' records read from the store into `jobs`, its files located.
#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
pub(crate) fn job<'f: 'j, 'j>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store<'_>,
    scripts: &mut Option<Result<Scripts<'f>, &'f str>>,
    names: &mut Option<crate::names::Names<'f>>,
    env: &FetchEnv<'_>,
    probes: CommandProbes<'_>,
    planned_script_probes: &[(ContentId, crate::engine_scripts::PlannedProbe)],
    tool: blaze_drive::Tool,
    workers: u32,
    lock: Option<&'f Lock<'static>>,
    members: Option<&[crate::graph::CheckoutDir]>,
    task: &Task<'_>,
    frame: &'f Frame,
    jobs: &'j Frame,
) -> Result<Prepared<'j>, CliError> {
    let cx = cx.span("script: make the job");
    if let Some(failed) = task.failed.first() {
        return Ok(Prepared::Waited(*failed));
    }
    const NOT_SET_UP: &str = "the command's build scripts could not be set up";
    if let Some(Err(words)) = scripts {
        return Ok(Prepared::Said { what: NOT_SET_UP, words: said(jobs, format_args!("{words}")), hole: true });
    }
    if scripts.is_none() || names.is_none() {
        let Some(graph) = task.inputs.iter().find(|input: &&Input| input.name == Name::unit_graph()) else {
            let mut wants = std::vec::Vec::new_in(Global);
            wants.push(Want { name: Name::unit_graph(), home: Name::unit_graph() });
            return Ok(Prepared::Need(wants));
        };
        // Held in the request's frame: every job of the command decodes it again on its worker.
        let read = crate::work::payload(&cx, store, graph, frame)?;
        let sections = read.unit_graph(graph.name)?;
        // The world (the crate list's rows, both sides): made once a command, here when no
        // `CrateScope` task made it first, as engine_units.rs does.
        if names.is_none() {
            match crate::names::world(&cx, root, sections.graph, sections.wasm_graph, lock, members, env) {
                Ok(made) => {
                    crate::names::tell_public(store, &made);
                    *names = Some(made);
                }
                Err(error) => return Ok(Prepared::Said { what: "the crates' inputs could not be made", words: said(jobs, format_args!("names not resolved: {error}")), hole: false }),
            }
        }
        if scripts.is_none() {
            let bytes: &'f [u8] = held_bytes(sections.graph, frame);
            let wasm_bytes: &'f [u8] = held_bytes(sections.wasm_graph, frame);
            let Some(listed) = names.as_ref() else { return Err(CliError::Text { what: "the names world, made above, is gone" }) };
            // Avoid building the full 1,200-crate world (mapping all fetched packages) when no
            // `Names` task has needed it yet: `open_light` opens only the toolchain shelf and
            // unit graph for script execution.
            let opened = match listed.made().filter(|made| made.has_open_packs()) {
                Some(made) => Scripts::open(&cx, env, made, bytes, lock, members, frame),
                None => Scripts::open_light(&cx, env, listed, bytes, wasm_bytes, lock, members, frame),
            };
            match opened {
                Ok(opened) => *scripts = Some(Ok(opened)),
                Err(error) => {
                    // Kept for the command: the next `Script` task says it, never opens again.
                    let words = said(frame, format_args!("{error}"));
                    *scripts = Some(Err(words));
                    return Ok(Prepared::Said { what: NOT_SET_UP, words: said(jobs, format_args!("{words}")), hole: true });
                }
            }
        }
    }
    let Some(Ok(scripts)) = scripts.as_mut() else { return Err(CliError::Text { what: "the command's scripts, opened above, are not there" }) };
    let Some((script, side)) = scripts.unit_of(task.name) else {
        return Ok(Prepared::Said { what: "a `Script` part names no build script of this command's graph", words: "a `Script` part names no build-script unit of this command's graph", hole: false });
    };
    // The crate of the list that reads this run: its cfg is the run's side's.
    let script_world = scripts.script_world;
    let (world, raw_cfg): (&blaze_world::World<'_>, &[(&str, Option<&str>)]) = match names.as_ref().and_then(|n| n.made()).filter(|made| made.has_open_packs()) {
        Some(made) => {
            let world = made.world();
            let wanted = blaze_world::Script::Read { subject: task.name.subject };
            let Some(row) = world.crates.iter().find(|krate| krate.script == wanted) else {
                return Ok(Prepared::Said { what: "a `Script` part that no crate of the world reads", words: "a `Script` part names a run that no crate of this command's world reads", hole: false });
            };
            if row.side != side {
                return Ok(Prepared::Said {
                    what: "a `Script` part whose world row is of another side",
                    words: said(jobs, format_args!("the build script run read by crate `{}` is of the {:?} side in the world and of the {side:?} side in the graph's subjects: one subject function names both (blaze_world::script_subject)", row.input.name, row.side)),
                    hole: false,
                });
            }
            (world, row.input.cfg)
        }
        None => {
            let Some(sw) = script_world else {
                return Err(CliError::Text { what: "neither the names world nor the lightweight script world is open" });
            };
            let Some((_, row_side, row_cfg)) = sw.script_cfgs.iter().find(|(subject, _, _)| *subject == task.name.subject) else {
                return Ok(Prepared::Said { what: "a `Script` part that no crate of the world reads", words: "a `Script` part names a run that no crate of this command's world reads", hole: false });
            };
            if *row_side != side {
                return Ok(Prepared::Said {
                    what: "a `Script` part whose world row is of another side",
                    words: said(jobs, format_args!("the build script run is of the {row_side:?} side in the world and of the {side:?} side in the graph's subjects")),
                    hole: false,
                });
            }
            (&sw.world, *row_cfg)
        }
    };
    let mut cfg: blaze_own::Vec<(&'j str, Option<&'j str>), &'j Frame> = blaze_own::Vec::with_capacity_in(raw_cfg.len(), jobs);
    for (name, value) in raw_cfg.iter() {
        cfg.push((crate::work::held_str(name, jobs), value.map(|value| crate::work::held_str(value, jobs))));
    }
    let cfg: &'j [(&'j str, Option<&'j str>)] = cfg.into_bump_slice();
    // Copied out of `scripts`, which the sources' lookup below borrows mutably.
    let (graph, members, lock) = (scripts.graph, scripts.members, scripts.lock);
    let unit = &graph.units[script.index()];
    // The links closure: each producing script's record (direct and upstream), asked for until
    // handed, and where its package's files are (a path in its metadata may name them).
    let closure = links_closure(graph, script);
    let mut wants = std::vec::Vec::new_in(Global);
    // Its module, looked up by its own task (script_modules.rs, ruling D115 (c)): read, so a
    // rebuilt module reruns this run alone and an equal one moves nothing.
    let Some(module_row) = names.as_ref().and_then(|n| n.script_readers(task.name).next().and_then(|at| n.script_module_of(at))) else {
        return Ok(Prepared::Said { what: "a build script with no module lookup", words: "the build script read here has no unit in the graph to name its module by", hole: false });
    };
    let module_subject = module_row.subject;
    let module = crate::script_modules::name(module_subject);
    if !task.inputs.iter().any(|input: &Input| input.name == module) {
        wants.push(Want { name: module, home: Name::unit_graph() });
    }
    let mut producers: blaze_own::Vec<Producer<'j>, &'j Frame> = blaze_own::Vec::with_capacity_in(closure.len(), jobs);
    for (input, links, direct) in closure.iter() {
        // A producer of a reader on one side is that side's run (its `links` crate is a crate
        // of the same side's list).
        let erased = Erased::new(jobs);
        let Some(dep) = name_of(graph, *input, side, erased.handle()) else {
            return Ok(Prepared::Said { what: "a `links` input that serves no unit", words: said(jobs, format_args!("the build script of {} {} reads the `links` metadata of a script that serves no unit", unit.name.as_str(), unit.version.as_str())), hole: false });
        };
        match task.inputs.iter().find(|handed: &&Input| handed.name == dep) {
            None => wants.push(Want { name: dep, home: Name::unit_graph() }),
            Some(handed) => {
                let bytes = held_bytes(crate::work::payload(&cx, store, handed, jobs)?.payload(), jobs);
                let files = match files_of(&mut scripts.sources, &mut scripts.files, members, lock, &graph.units[input.index()], frame, jobs) {
                    Ok(files) => files,
                    Err(refused) => return Ok(refused),
                };
                producers.push(Producer { script: *input, links: crate::work::held_str(links, jobs), name: dep, out: handed.out, bytes, files, direct: *direct });
            }
        }
    }
    if !wants.is_empty() {
        return Ok(Prepared::Need(wants));
    }
    let files = match files_of(&mut scripts.sources, &mut scripts.files, members, lock, unit, frame, jobs) {
        Ok(files) => files,
        Err(refused) => return Ok(refused),
    };
    // The first job made ready opens what the pool shares; a job that only asked for inputs
    // (above) opened none.
    let shared = match scripts.shared(&cx, env, probes, store, tool, workers, world, frame) {
        Ok(shared) => shared,
        Err(words) => return Ok(Prepared::Said { what: NOT_SET_UP, words: said(jobs, format_args!("{words}")), hole: true }),
    };
    let mut io_scratch = match bstd::mem::Frame::new(bstd::mem::Tag::Other) {
        Ok(scratch) => scratch,
        Err(error) => return Ok(Prepared::Said { what: NOT_SET_UP, words: said(jobs, format_args!("{error}")), hole: true }),
    };
    let planned_ran_as = module_row.ran_as;
    let (planned_runner, planned_source) = {
        let erased = blaze_own::Erased::new(frame);
        let base_runner = placed_in_store_with_lock_and_source(
            &shared.store,
            graph,
            Some(lock),
            script,
            MODE,
            crate::graph::TRIPLE,
            None,
            erased.handle(),
        );
        let (runner, source_snap) = if matches!(base_runner, Runner::Native { .. }) {
            (base_runner, None)
        } else {
            let (reason, fingerprint) = match classify_script_source_in_files(
                &cx,
                &root.read_only(),
                &files,
                unit.root.as_str(),
                &io_scratch,
            ) {
                Ok(classified) => classified,
                Err(why) => {
                    return Ok(Prepared::Said {
                        what: "a build script's source could not be read",
                        words: said(jobs, format_args!("the build script of {} {}: {why}", unit.name.as_str(), unit.version.as_str())),
                        hole: true,
                    });
                }
            };
            let runner = placed_in_store_with_lock_and_source(
                &shared.store,
                graph,
                Some(lock),
                script,
                MODE,
                crate::graph::TRIPLE,
                reason,
                erased.handle(),
            );
            (runner, Some((reason, fingerprint)))
        };
        io_scratch.reset();
        if RanAs::of(&runner) != planned_ran_as {
            return Ok(Prepared::Said {
                what: "a build script's placement changed since planning",
                words: said(
                    jobs,
                    format_args!(
                        "the build script of {} {} was planned as {:?}, but its current source/store classifies as {:?} (rule 21)",
                        unit.name.as_str(),
                        unit.version.as_str(),
                        planned_ran_as,
                        RanAs::of(&runner),
                    ),
                ),
                hole: true,
            });
        }
        (runner, source_snap)
    };
    let ran_as = planned_ran_as;
    let dep_files = &mut scripts.dep_files;
    let deps_digest = match blaze_proc_macro::script_unit::script_deps_digest_with(
        graph,
        deps_wasm_graph(ran_as, scripts.wasm_graph),
        lock,
        script,
        |dep_pkg| match dep_files.binary_search_by_key(&dep_pkg, |(id, _)| *id) {
            Ok(at) => dep_files.get(at).and_then(|(_, hex)| *hex),
            Err(at) => {
                let held = members
                    .iter()
                    .find(|m| m.id == dep_pkg)
                    .and_then(|m| crate::macro_files::package_files_digest_in_dir(&cx, &root.read_only(), m.dir, frame, &io_scratch));
                io_scratch.reset();
                dep_files.insert_shift(at, (dep_pkg, held));
                held
            }
        },
        frame,
    ) {
        Ok(opt) => opt.map(|hex| crate::work::held_str(hex.as_str(), jobs)),
        Err(error) => return Ok(Prepared::Said { what: NOT_SET_UP, words: said(jobs, format_args!("{error}")), hole: true }),
    };
    let deps_digest = match fold_wasm_script_dep_probes(
        &cx,
        ran_as,
        graph,
        scripts.wasm_graph,
        script,
        module_subject,
        Some(planned_script_probes),
        deps_digest,
        probes,
        jobs,
    ) {
        Ok(folded) => folded,
        Err(error) => return Ok(Prepared::Said { what: NOT_SET_UP, words: said(jobs, format_args!("{error}")), hole: true }),
    };
    Ok(Prepared::Ready(ScriptJob {
        name: task.name,
        shared,
        script,
        side,
        cfg,
        files,
        producers: producers.into_bump_slice(),
        checkout: CheckoutReads::Hole,
        deps_digest,
        planned_runner,
        planned_source,
    }))
}

/// Folds the dependency build-script probe identity digest into `deps_digest` ONLY for
/// `RanAs::Wasm` build scripts (which execute wasm64 self-compiled modules and require
/// `wasm_graph`). When `planned_script_probes` is supplied by `ScriptsStage`, binds strictly to
/// the planned `MacroModule` probe tag instead of rescanning live probe state so wave-1 `Script`
/// runs always open the exact module keyed and compiled in wave 0 (Rule 21). Native build scripts
/// (`RanAs::Native`) skip wasm64 dependency scanning so minimal toolchains without
/// `wasm64-blaze-wasip1` never fail native preparation.
#[expect(clippy::too_many_arguments, reason = "build-script deps-digest folding over cx, placement, graphs, unit, module_subject, planned, digest, probes, and bump")]
fn fold_wasm_script_dep_probes<'j>(
    cx: &Cx,
    ran_as: RanAs,
    graph: &UnitGraph<'_>,
    wasm_graph: Option<&UnitGraph<'_>>,
    script: UnitId,
    module_subject: ContentId,
    planned_script_probes: Option<&[(ContentId, crate::engine_scripts::PlannedProbe)]>,
    deps_digest: Option<&'j str>,
    probes: CommandProbes<'_>,
    jobs: &'j Frame,
) -> Result<Option<&'j str>, ProbeSetupError> {
    if ran_as != RanAs::Wasm {
        return Ok(deps_digest);
    }
    let planned_entry = planned_script_probes.and_then(|table| {
        let at = table.binary_search_by_key(&module_subject, |(s, _)| *s).ok()?;
        table.get(at).map(|(_, entry)| entry)
    });
    match planned_entry {
        Some(crate::engine_scripts::PlannedProbe::Digest(planned_hex)) => {
            Ok(Some(crate::wasm64_compile::fold_dep_probes_opt(deps_digest, planned_hex.as_str(), jobs)))
        }
        Some(crate::engine_scripts::PlannedProbe::Failed(err)) => Err(err.duplicate()),
        None => match crate::wasm64_compile::unit_has_dep_build_scripts(graph, wasm_graph, script, false, probes.shared())? {
            true if planned_script_probes.is_some() => Err(ProbeSetupError::from_str(
                ProbeErrorKind::PlannedDrift,
                "build script has dependency build scripts at job time without a planned module probe identity",
                probes.shared(),
            )),
            true => {
                let probes_hex = probes.dep_probe_identity_hex(cx)?;
                Ok(Some(crate::wasm64_compile::fold_dep_probes_opt(deps_digest, probes_hex.as_str(), jobs)))
            }
            false => Ok(deps_digest),
        },
    }
}

/// What a script's run gave on its worker, in `Global` (its task frame ends with it): the
/// record, or a failure with its words.
pub(crate) enum Ran {
    Made(Record<Global>),
    Failed { what: &'static str, words: blaze_own::Vec<u8, Global>, hole: bool },
}

/// What the pool hands back.
pub(crate) struct PooledScript {
    pub(crate) did: Result<Ran, blaze_drive::Why>,
    pub(crate) used: usize,
    pub(crate) ns: u64,
    /// The run's reads of the checkout as cells, its record's `Given` reads in the same order:
    /// filled only for a kept member run ([`CheckoutReads::Kept`]) that made its record.
    pub(crate) checkout: crate::engine_script_reads::ReadList,
    /// Why the record made is right for this command only: a read no cell names, so no read
    /// list makes it stand. The engine uses it this command, owes its row so it runs again next
    /// command, and hands on none of its reads (engine_scripts.rs `run_phase`).
    pub(crate) unkept: Option<crate::engine_script_reads::NoCell>,
}

/// `job` on a pool worker, in the worker's task frame: a panic or a full frame fails this
/// script alone (rule 20).
pub(crate) fn pooled(cx: &Cx, job: &ScriptJob<'_>, clock: &bstd::clock::Clock) -> PooledScript {
    let began = clock.now();
    let scratch = cx.frame();
    let mut checkout = crate::engine_script_reads::ReadList::new_in(job.shared.result_alloc);
    let mut unkept = None;
    let ran = crate::work::unwinding(|| run(cx, job, scratch, &mut checkout, &mut unkept));
    let (used, limit) = (scratch.peak(), scratch.reserved());
    let did = ran.map_err(|crate::work::Panicked| blaze_drive::Why::Panicked { used: used as u64, limit: limit as u64 });
    // Only a kept record stands on its reads: a failure's, a panic's or an unkept record's are
    // not handed on.
    if !matches!(did, Ok(Ran::Made(_))) || unkept.is_some() {
        checkout.clear();
    }
    if !matches!(did, Ok(Ran::Made(_))) {
        unkept = None;
    }
    PooledScript { did, used, ns: clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)), checkout, unkept }
}

/// A run's failure with its words; `hole` when its cause is outside its key and its reads (it is
/// asked again next command). Also a test's runner's (engine_scripts.rs `ScriptRunner`).
pub(crate) fn failed(what: &'static str, words: core::fmt::Arguments<'_>, hole: bool) -> Ran {
    let mut held = blaze_own::Vec::new_in(Global);
    let text = std::fmt::format(words);
    held.extend_from_slice(text.as_bytes());
    Ran::Failed { what, words: held, hole }
}

/// The run on the worker; a kept member run's reads of the checkout onto `checkout`, or why its
/// record is right for this command only onto `unkept`.
fn run(cx: &Cx, job: &ScriptJob<'_>, scratch: &Frame, checkout: &mut crate::engine_script_reads::ReadList, unkept: &mut Option<crate::engine_script_reads::NoCell>) -> Ran {
    let _cx = cx.span("script: run one build script");
    let erased = Erased::new(scratch);
    let alloc = erased.handle();
    let shared = job.shared;
    let decoded = match blaze_unit_graph::record::decode(shared.graph_record, alloc) {
        Ok(decoded) => decoded,
        Err(refused) => return failed("the unit graph record does not decode", format_args!("the unit graph record at byte {}: {}", refused.at, refused.why), false),
    };
    let graph = &decoded.graph;
    let unit: &Unit<'_> = &graph.units[job.script.index()];
    let package = std::format!("{} {}", unit.name.as_str(), unit.version.as_str());
    // Each run of its `links` closure, as its record was handed (read in place) with the tree
    // its package is served: `script_unit::outputs_given` makes the `DEP_<LINKS>_*` pairs and
    // what each producer serves (brief S1), wasm or native alike, the one function the census's
    // exporter calls too (rule 30).
    let mut given: std::vec::Vec<script_unit::GivenRun<'_>, Global> = std::vec::Vec::with_capacity(job.producers.len());
    for producer in job.producers.iter() {
        let (links, dep) = (producer.links, producer.name);
        let opened = match payload_parts(producer.bytes) {
            Some((_, record)) => OutputsRef::open(record),
            None => Err(blaze_build_script::outputs::RecordError::Layout),
        };
        let outputs = match opened {
            Ok(outputs) => outputs,
            Err(why) => return failed("a `links` input's record is not an outputs record", format_args!("the build script of {package}: the record of {dep:?}, whose `links = \"{links}\"` metadata it is served, is not an outputs record: {why:?}"), false),
        };
        let (tree, root) = match tree_of(&producer.files, shared.checkout, &package) {
            Ok(served) => served,
            Err(ran) => return ran,
        };
        given.push(script_unit::GivenRun { script: producer.script, links, outputs, tree, root, direct: producer.direct });
    }
    let (tree, root) = match tree_of(&job.files, shared.checkout, &package) {
        Ok(served) => served,
        Err(ran) => return ran,
    };
    // Reuse the verified planned runner that named the `Script` and `MacroModule` tasks (Rule 21).
    // When source classification participated in placement, re-verify the live source-tree snapshot
    // against the planned `(reason, fingerprint)` and refuse any mid-command mutation as a hole
    // rather than silently running Native under a Wasm key (or vice versa).
    if let Some((planned_reason, planned_fingerprint)) = job.planned_source {
        let mut hasher = blaze_proc_macro::module::FilesHasher::new();
        let live_reason = match script_unit::classify_script_tree_in(&tree, &root, unit.root.as_str(), true, alloc, |rel, text| {
            hasher.update_file(rel, text.as_bytes());
        }) {
            Ok(reason) => reason,
            Err(why) => {
                return failed(
                    "a build script's source could not be read",
                    format_args!("the build script of {package}: {}", why.as_str()),
                    true,
                );
            }
        };
        let live_fingerprint = hasher.finish();
        if live_reason != planned_reason || live_fingerprint != planned_fingerprint {
            return failed(
                "a build script's source changed since planning",
                format_args!("the build script of {package} changed during the command (rule 21); refusing stale placement"),
                true,
            );
        }
    }
    let runner = job.planned_runner;
    let ran_as = RanAs::of(&runner);
    let native = match (runner, &shared.native) {
        // No SDK: a function of the run's key, which holds "no SDK" ([`Shared::no_sdk`]), so
        // filed. An SDK and the setting still refused (its shelf, its fact shim): a fact of the
        // machine no key holds yet, so a hole, asked again next command.
        (Runner::Native { .. }, Err(why)) => return failed("a native build script with no native setting", format_args!("the build script of {package} runs native (D112): {why}"), !shared.no_sdk),
        (_, Ok(native)) => Some(*native),
        (Runner::Wasm, Err(_)) => None,
    };
    let files_digest = match (&job.files, shared.checkout) {
        (Files::Member { dir } | Files::External { dir }, crate::engine_script_reads::Served::Root(checkout)) => {
            let pkg_dir = if dir.is_empty() { checkout.path().to_path_buf() } else { checkout.path().join(dir) };
            script_unit::script_files_digest(&pkg_dir, unit.root.as_str()).ok()
        }
        _ => None,
    };
    let setting = Setting {
        store: &shared.store,
        // Its crates' side's target (ruling D115 (2)); native zero-Wasm macro scripts run for host.
        target: match (job.side, ran_as) {
            (Side::Target, _) | (Side::Macro, RanAs::Native) => crate::graph::TRIPLE,
            (Side::Macro, RanAs::Wasm) => MACRO_TARGET,
        },
        host: crate::graph::TRIPLE,
        table: Arc::clone(match (job.side, ran_as) {
            (Side::Target, _) | (Side::Macro, RanAs::Native) => &shared.table,
            (Side::Macro, RanAs::Wasm) => &shared.macro_table,
        }),
        // The probe checker (blaze-proc-macro script_probes.rs) is off: the pinned table
        // answers until the engine's checker has answered the real probes' ladder
        // (handoff p1-probe-checker.txt).
        probes: None,
        c_probes: Some(shared.c_probes.with_cx(cx)),
        c_mode: C_MODE,
        mode: MODE,
        native,
        spawn_setting: Arc::clone(&shared.spawn),
        files_digest: files_digest.as_deref(),
        deps_digest: job.deps_digest,
    };
    // A macro-side run is made with its row's cfg (wasm32-wasip1's atoms); a target-side run
    // with the graph's, which its row copies (and a native target-side script may run).
    let side_cfg = match job.side {
        Side::Target => None,
        Side::Macro => Some(side_atoms(job.cfg, alloc.clone())),
    };
    let checkout_prefix = match job.files {
        Files::Member { .. } | Files::External { .. } => Some(""),
        Files::Pack { .. } => None,
    };
    let is_checkout = checkout_prefix.is_some();
    // A kept checkout-backed run's failure is a hole: the reads it made are not handed back with
    // its error (`ScriptError`), so no read list makes it stand, and standing on its drive inputs
    // alone would keep it failed past the edit that fixes it (rule 21). On Linux, wasm runs do
    // not hash the C-probe table/answerer into `key::base` (probes are tracked per-probe in
    // `ran.reads`), so a failed Linux wasm script that may have consumed a C probe must likewise
    // stay a hole rather than filing a failure without `cc-probe` read dependencies.
    let failed_hole = (is_checkout && matches!(job.checkout, CheckoutReads::Kept { .. }))
        || (cfg!(target_os = "linux") && ran_as == RanAs::Wasm);
    let ran = match script_unit::outputs_given_placed(
        &shared.runtime,
        &setting,
        graph,
        job.script,
        side_cfg.as_ref().map(|atoms| atoms.as_slice()),
        tree,
        root,
        &given,
        Some(runner),
        alloc,
    ) {
        Ok(ran) => ran,
        Err(error) => return failed("a build script failed", format_args!("the build script of {package} failed: {error}"), failed_hole),
    };
    if let Some(error) = ran.outputs.errors.first() {
        let probed = ran.reads.iter().any(|read| blaze_build_script::facts::parse_cc_probe_subject(read.subject.as_str()).is_some());
        return failed("a build script printed `cargo::error`", format_args!("the build script of {package} failed: cargo::error={}", error.as_str()), failed_hole || probed);
    }
    // The record's reads, in the run's frame until the record is made.
    let mut reads: blaze_own::Vec<(Name, ContentId), &Frame> = blaze_own::Vec::with_capacity_in(ran.reads.len() + job.producers.len(), scratch);
    // A member's tree is the checkout: what it read there is a read of today's file. A fetched
    // package's tree is its record, pinned by the lock in the graph's source: its reads (files,
    // listings, git facts from the lock's commit) are functions of that pin.
    //
    // On the engine a member's `file:`, `dir:`, `stat:` and `git:` reads are cells whose digests
    // are the sandbox's own answers (engine_script_reads.rs), so the record stands until one
    // moves. An unknown or incomplete observation leaves the record right for this command only: its row
    // owed, so it runs again next command (`unkept`; engine_scripts.rs `run_phase`); it fails no
    // dependent. Under [`CheckoutReads::Hole`] every read of the checkout is a hole: its outputs
    // serve nothing and it runs again next command.
    match job.checkout {
        CheckoutReads::Kept { plant, status } => match admit_reads_with_probes(&ran.reads, ran_as, &shared.spawn, shared.c_probes.ready(), checkout_prefix, plant, status, checkout, alloc) {
            Ok(()) => {
                for read in checkout.iter() {
                    reads.push((read.name(), read.answer));
                }
            }
            Err(AdmitError::Unkept(no_cell)) => *unkept = Some(no_cell),
            Err(AdmitError::FactFailed(why)) => {
                return failed(
                    "a build script fact validation failed",
                    format_args!("the build script of {package} failed to validate a compiler probe fact: {why}"),
                    failed_hole,
                );
            }
        },
        // No cells planned: a hole.
        CheckoutReads::Hole => {
            if is_checkout && let Some(read) = ran.reads.first() {
                return failed(
                    "a member's build script read a checkout file the drive cannot name yet",
                    format_args!("the build script of {package} read `{}` of the checkout: a read of a checkout file is not a drive read on this path (engine_script_reads.rs keeps them on the engine's), so its outputs are not kept; it runs again next command", read.subject),
                    true,
                );
            }
        }
    }
    // Every producer of the closure, by its out (dep_paths.rs: per producer mounted, its outputs
    // record's id; a fetched tree is under the graph's source, a member tree has checkout reads).
    for producer in job.producers.iter() {
        reads.push((producer.name, producer.out));
    }
    outputs_record(job.name, &reads[..], ran_as, &ran.outputs, &package)
}

/// Package files in a fetched tree stand on the lock. A checkout read needs a cell.
/// The fact mount is shared by members and fetched packages. A reply file is named by the
/// whole question and followed under the next command's pins. The wasm answerer is part of
/// the producer. A native reply additionally depends on the shim image the runner observed.
/// Rebases a package-relative sandbox read subject (`file:`, `stat:`, `dir:`, `git:`) under
/// `prefix` (checkout-relative external package dir) so `collect_reads` tracks the real checkout
/// leaf path.
fn rebase_read_subject<'a>(prefix: &str, subject: &str, alloc: Alloc<'a>) -> Option<blaze_own::AString<'a>> {
    let mut out = blaze_own::AString::new_in(alloc);
    if let Some((tag, rel)) = subject.split_once(':') {
        match tag {
            "file" | "stat" | "dir" => {
                out.push_str(tag);
                out.push(':');
                out.push_str(prefix);
                if !rel.is_empty() {
                    out.push('/');
                    out.push_str(rel);
                }
                return Some(out);
            }
            "git" => {
                if let Some((folder, rest)) = rel.split_once('\n') {
                    out.push_str("git:");
                    out.push_str(prefix);
                    if !folder.is_empty() {
                        out.push('/');
                        out.push_str(folder);
                    }
                    out.push('\n');
                    out.push_str(rest);
                    return Some(out);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) enum AdmitError<'a> {
    Unkept(crate::engine_script_reads::NoCell),
    FactFailed(blaze_own::AString<'a>),
}

pub(crate) fn admit_reads(
    observed: &[blaze_proc_macro::script_sandbox::Read],
    ran_as: RanAs,
    setting: &SpawnSetting,
    checkout_prefix: Option<&str>,
    plant: crate::engine_script_reads::ReadsPlant,
    status: Option<crate::engine_script_reads::StatusView>,
    cells: &mut crate::engine_script_reads::ReadList,
    alloc: Alloc<'_>,
) -> Result<(), crate::engine_script_reads::NoCell> {
    match admit_reads_with_probes(observed, ran_as, setting, None, checkout_prefix, plant, status, cells, alloc) {
        Ok(()) => Ok(()),
        Err(AdmitError::Unkept(no_cell)) => Err(no_cell),
        Err(AdmitError::FactFailed(why)) => Err(crate::engine_script_reads::NoCell::Unobserved {
            subject: crate::engine_script_reads::Subject::from_str_in(why.as_str(), *cells.allocator()),
        }),
    }
}

#[expect(clippy::too_many_arguments, reason = "build-script read admission: each input one capability or mode")]
pub(crate) fn admit_reads_with_probes<'a>(
    observed: &[blaze_proc_macro::script_sandbox::Read],
    ran_as: RanAs,
    setting: &SpawnSetting,
    c_probes: Option<&blaze_proc_macro::probe_clang::CScriptProbes>,
    checkout_prefix: Option<&str>,
    plant: crate::engine_script_reads::ReadsPlant,
    status: Option<crate::engine_script_reads::StatusView>,
    cells: &mut crate::engine_script_reads::ReadList,
    alloc: Alloc<'a>,
) -> Result<(), AdmitError<'a>> {
    use crate::engine_script_reads::{self as reads, NoCell, READS_CAP, Subject};

    let result_alloc = *cells.allocator();
    if let Some(extra) = observed.get(READS_CAP) {
        let subject = extra.subject.strip_prefix("read-limit:").unwrap_or(&extra.subject);
        return Err(AdmitError::Unkept(NoCell::TooMany { reads: READS_CAP + 1, subject: Subject::from_str_in(subject, result_alloc) }));
    }
    let image_subject = blaze_proc_macro::native_facts::IMAGE_SUBJECT;
    let image = observed.iter().find(|read| read.subject == image_subject).and_then(|read| read.digest);
    if ran_as == RanAs::Native && image.is_none() {
        let missing = observed.iter().find(|read| read.subject.starts_with("pkg-config\n")).map_or(image_subject, |read| read.subject.as_str());
        return Err(AdmitError::Unkept(NoCell::Unobserved { subject: Subject::from_str_in(missing, result_alloc) }));
    }
    let mut receipts = blaze_own::BTreeMap::new_in(alloc);
    let mut file = |path: Subject, answer| cells.push(reads::CheckoutRead { kind: reads::ReadKind::File, path, answer });
    for read in observed {
        let subject = read.subject.as_str();
        let rejected = || AdmitError::Unkept(NoCell::Unobserved { subject: Subject::from_str_in(subject, result_alloc) });
        if ran_as == RanAs::Wasm && let Some((grant, key)) = blaze_build_script::host_tools::spawn_receipt(subject) {
            let pair = receipts.entry(key).or_insert([None; 2]);
            let digest = read.digest.ok_or_else(rejected)?;
            if pair[usize::from(grant)].replace(digest).is_some_and(|was| was != digest) {
                return Err(AdmitError::Unkept(NoCell::Conflicting { subject: Subject::from_str_in(subject, result_alloc) }));
            }
        } else if subject == image_subject && ran_as == RanAs::Native {
            let digest = read.digest.ok_or_else(rejected)?;
            if image != Some(digest) {
                return Err(AdmitError::Unkept(NoCell::Conflicting { subject: Subject::from_str_in(subject, result_alloc) }));
            }
            file(Subject::from_str_in(crate::sweep::SHIM_FILE, result_alloc), digest);
            file(Subject::from_str_in(crate::sweep::PINS_FILE, result_alloc), blaze_proc_macro::native_facts::pins_digest(setting.pins.as_ref(), alloc));
        } else if blaze_proc_macro::spawn_route::is_fact_reply(subject, alloc) {
            // A derived fact (pkg-config, sw_vers, cc-version, cc-probe): its reply is a function
            // of the pins or cached c-probe record, so it is a keyed read of a reply file the next
            // command answers again.
            let digest = read.digest.ok_or_else(rejected)?;
            let matched = blaze_proc_macro::spawn_route::fact_reply_matches_with_probes(
                subject,
                Some(digest),
                setting.pins.as_ref(),
                c_probes,
                alloc,
            )
            .map_err(AdmitError::FactFailed)?;
            if !matched {
                return Err(rejected());
            }
            let mut path = Subject::from_str_in(crate::sweep::REPLY_FILES, result_alloc);
            path.push_str(subject);
            file(path, digest);
        }
    }
    if !receipts.is_empty() {
        // The request key binds argv, the effective child env, stdin, the served input tree
        // and previous OUT_DIR writes. The script and its earlier reads determine those
        // values. The externally supplied tool images and userland must stand independently.
        let context = setting.executor_id(alloc);
        for (key, [reply, grant]) in receipts.iter() {
            if reply.is_none() || grant.is_none() || *grant != context {
                let mut subject = Subject::from_str_in("spawn:", result_alloc);
                subject.push_str(key);
                return Err(AdmitError::Unkept(NoCell::Unobserved { subject }));
            }
        }
        file(Subject::from_str_in(crate::sweep::EXECUTOR_FILE, result_alloc), context.expect("validated every grant"));
    }
    let tree_reads = observed.iter().filter_map(|read| {
        let subject = read.subject.as_str();
        let executor = ran_as == RanAs::Wasm && blaze_build_script::host_tools::spawn_receipt(subject).is_some();
        (!executor && !blaze_proc_macro::spawn_route::is_fact_reply(subject, alloc) && !(subject == image_subject && ran_as == RanAs::Native)).then_some((subject, read.digest))
    });
    match checkout_prefix {
        Some("") => reads::collect_reads(tree_reads, plant, status, cells).map_err(AdmitError::Unkept),
        Some(prefix) => {
            let mut rebased: AVec<'_, (blaze_own::AString<'_>, Option<ContentId>)> = AVec::new_in(alloc);
            for (subject, digest) in tree_reads {
                let text = rebase_read_subject(prefix, subject, alloc).unwrap_or_else(|| blaze_own::AString::from_str_in(subject, alloc));
                rebased.push((text, digest));
            }
            reads::collect_reads(rebased.iter().map(|(s, d)| (s.as_str(), *d)), plant, status, cells).map_err(AdmitError::Unkept)
        }
        None => reads::pinned_reads(tree_reads, result_alloc).map_err(AdmitError::Unkept),
    }
}

/// The record `name` of a run that read `reads` and made `outputs`: how it ran, then the
/// outputs record ([`payload_parts`]); the out is the outputs' alone, so a reader moves only
/// when what it reads moves. Encoded on the heap and handed whole to the record, which outlives
/// the worker's frame (rules 11 and 51): never encoded in the frame and copied out. The one
/// maker of a script's record: the real run's and a test's runner's (engine_scripts.rs
/// `ScriptRunner`).
pub(crate) fn outputs_record(name: Name, reads: &[(Name, ContentId)], ran_as: RanAs, outputs: &blaze_build_script::outputs::Outputs<'_>, package: &str) -> Ran {
    let mut made = Record::new_in(name, Global);
    made.reads.extend_from_slice(reads);
    let heap = Erased::new(Global);
    let mut payload: AVec<'_, u8> = AVec::new_in(heap.handle());
    payload.push(ran_as as u8);
    outputs.encode(&mut payload);
    made.out = ContentId::of(OUT_DOMAIN, &payload[1..]);
    let Ok(payload) = heap.adopt(payload) else {
        return failed("a build script's outputs record was not made on its own heap handle", format_args!("the build script of {package}: its outputs record was not made over the handle `adopt` was asked of (scripts.rs `run`)"), false);
    };
    if let Err(error) = made.hand(payload) {
        return failed("a build script's record refused its outputs", format_args!("the build script of {package}: {error}"), false);
    }
    Ran::Made(made)
}

#[cfg(test)]
mod shared_command_probes_dst {
    use core::sync::atomic::{AtomicU32, Ordering};

    use blaze_own::SharedAlloc;
    use blaze_proc_macro::probe_clang::CProbeSetting;
    use blaze_proc_macro::spawn_route::{FactPins, SpawnSetting};
    use blaze_sim::{Broke, Config, Tape, check as sim_check, control};
    use bstd::cx::Cx;
    use bstd::io::{Io, Limits, SimWorld};

    use super::{FetchEnv, ProbeErrorKind, ProbeOncePlant, ProbeSetupError, SharedCommandProbes};

    fn sim_env<'a>(io: &Io) -> FetchEnv<'a> {
        FetchEnv {
            clock: io.need_clock().expect("sim clock"),
            checkout: "",
            served: crate::engine_script_reads::Served::Simulated,
            cert_file: None,
            cert_dir: None,
            rustflags: blaze_manifest::rustflags::RustflagsEnv::default(),
            home: Some("/h"),
            cargo_home: None,
            start: None,
            toolchain: blaze_toolchain::PINNED,
            fs: io.need_fs().expect("sim fs"),
            sdk: crate::sdk::NONE_IN_TESTS,
            exe_dir: Err(crate::fetch::ExeDirWhy::NotRead),
            lan_peer: crate::fetch::LanPeerConfig::DISABLED,
            heap: blaze_own::testing::heap(),
        }
    }

    /// Operation executed by one simulated caller against the real production `SharedCommandProbes`
    /// instance under `blaze_sim::Tape` control.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ProbeOp {
        CallCProbes,
        CallSpawnSetting,
        CallSpawnSnapshot,
        CallDepIdentity,
        CancelledIdentity,
    }

    /// Tape-driven deterministic schedule explorer that drives the **actual production**
    /// `SharedCommandProbes` / `CommandProbes` (`c_probes_with`, `spawn_setting_with`,
    /// `spawn_setting_snapshot_with`, and `dep_probe_identity_from_setting`) on `Io::simulated`
    /// across interleaved caller sequences, injected spawn-pin errors, and cancelled caller `Cx`s.
    /// Every interleaving is 100% single-threaded and replayable from `tape`.
    fn probe_init_schedule(tape: &Tape, plant: ProbeOncePlant) -> Result<(), Broke> {
        let seed = *tape.one_of(&[1u64, 7, 19, 42, 99, 137], "sim seed");
        let callers = *tape.one_of(&[2usize, 3, 4, 5, 6], "callers");
        let steps = *tape.one_of(&[6usize, 10, 16, 24], "steps");
        let inject_spawn_error = tape.one_in(2, "inject spawn-pin error");
        let c_probes_present = tape.one_in(2, "c_probes present");

        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        let cx = io
            .cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME)
            .map_err(|_| Broke::Oracle("sim cx failed".into()))?;
        let env = sim_env(&io);
        let holder = SharedCommandProbes::new_with_plant(plant);
        let probes = holder.with_env(&env);

        let c_inits = AtomicU32::new(0);
        let s_inits = AtomicU32::new(0);
        let mut first_c_tag: Option<Option<blaze_own::AString<'_>>> = None;
        let mut first_spawn_ok: Option<blaze_own::AString<'_>> = None;
        let mut first_spawn_err: Option<blaze_own::AString<'_>> = None;
        let mut first_dep_hex: Option<crate::wasm64_compile::Hex64> = None;

        let test_alloc = env.heap.alloc();
        let test_shared = env.heap.shared();
        let open_c = |_cx: &Cx| -> Option<CProbeSetting> {
            let n = c_inits.fetch_add(1, Ordering::SeqCst) + 1;
            if !c_probes_present {
                return None;
            }
            Some(CProbeSetting::Unavailable(blaze_own::Arc::from_str_in(
                bstd::mem::format_in!(cx.frame(), "unavail-init-{n}").as_str(),
                test_shared,
            )))
        };
        let open_s = |_cx: &Cx| -> Result<SpawnSetting, ProbeSetupError> {
            let n = s_inits.fetch_add(1, Ordering::SeqCst) + 1;
            if inject_spawn_error {
                Err(ProbeSetupError::from_str(
                    ProbeErrorKind::SpawnPinsOpen,
                    bstd::mem::format_in!(cx.frame(), "injected-pin-err-{n}").as_str(),
                    test_shared,
                ))
            } else {
                Ok(SpawnSetting::facts_only(blaze_proc_macro::spawn_route::FactPins {
                    clang_version_text: bstd::mem::format_in!(cx.frame(), "clang version 23.0.{n}").as_str().into(),
                    macos_product_version: Default::default(),
                    macos_build_version: Default::default(),
                }))
            }
        };

        let mut c_calls: u32 = 0;
        let mut s_calls: u32 = 0;
        let ops = [
            ProbeOp::CallCProbes,
            ProbeOp::CallSpawnSetting,
            ProbeOp::CallSpawnSnapshot,
            ProbeOp::CallDepIdentity,
            ProbeOp::CancelledIdentity,
        ];
        for step in 0..steps {
            let _caller = step % callers;
            let op = *tape.one_of(&ops, "probe op");
            match op {
                ProbeOp::CallCProbes => {
                    c_calls += 1;
                    let got = probes
                        .c_probes_with(&cx, &open_c)
                        .map_err(|e| Broke::Oracle(bstd::mem::format_in!(cx.frame(), "c_probes_with failed: {e}").as_str().into()))?;
                    let tag = match got {
                        None => None,
                        Some(CProbeSetting::Unavailable(reason)) => Some(blaze_own::AString::from_str_in(reason.as_ref(), test_alloc)),
                        Some(CProbeSetting::Ready(_)) => Some(blaze_own::AString::from_str_in("ready", test_alloc)),
                    };
                    if let Some(prev) = &first_c_tag {
                        if prev != &tag {
                            return Err(Broke::Oracle(bstd::mem::format_in!(
                                cx.frame(),
                                "duplicate or divergent c_probes outcome: got {tag:?}, expected {prev:?}"
                            ).as_str().into()));
                        }
                    } else {
                        first_c_tag = Some(tag);
                    }
                }
                ProbeOp::CallSpawnSetting => {
                    s_calls += 1;
                    match probes.spawn_setting_with(&cx, &open_s) {
                        Ok(setting) => {
                            if inject_spawn_error {
                                return Err(Broke::Oracle(
                                    "spawn_setting_with returned Ok under injected error".into(),
                                ));
                            }
                            let ver = blaze_own::AString::from_str_in(
                                setting.pins.as_ref().map_or("", |p| p.clang_version_text.as_str()),
                                test_alloc,
                            );
                            if let Some(prev) = &first_spawn_ok {
                                if prev != &ver {
                                    return Err(Broke::Oracle(bstd::mem::format_in!(
                                        cx.frame(),
                                        "duplicate or divergent spawn_setting: got {ver:?}, expected {prev:?}"
                                    ).as_str().into()));
                                }
                            } else {
                                first_spawn_ok = Some(ver);
                            }
                        }
                        Err(err) => {
                            if !inject_spawn_error {
                                return Err(Broke::Oracle(bstd::mem::format_in!(
                                    cx.frame(),
                                    "unexpected spawn_setting_with error: {err}"
                                ).as_str().into()));
                            }
                            if err.kind != ProbeErrorKind::SpawnPinsOpen {
                                return Err(Broke::Oracle(bstd::mem::format_in!(
                                    cx.frame(),
                                    "wrong ProbeErrorKind: {:?}",
                                    err.kind
                                ).as_str().into()));
                            }
                            let detail = blaze_own::AString::from_str_in(err.detail.as_str(), test_alloc);
                            if let Some(prev) = &first_spawn_err {
                                if prev != &detail {
                                    return Err(Broke::Oracle(bstd::mem::format_in!(
                                        cx.frame(),
                                        "duplicate or divergent spawn error: got {detail:?}, expected {prev:?}"
                                    ).as_str().into()));
                                }
                            } else {
                                first_spawn_err = Some(detail);
                            }
                        }
                    }
                }
                ProbeOp::CallSpawnSnapshot => {
                    s_calls += 1;
                    match probes.spawn_setting_snapshot_with(&cx, &open_s) {
                        Ok(setting) => {
                            if inject_spawn_error {
                                return Err(Broke::Oracle(
                                    "spawn_setting_snapshot_with returned Ok under injected error".into(),
                                ));
                            }
                            let ver = blaze_own::AString::from_str_in(
                                setting.pins.as_ref().map_or("", |p| p.clang_version_text.as_str()),
                                test_alloc,
                            );
                            if let Some(prev) = &first_spawn_ok {
                                if prev != &ver {
                                    return Err(Broke::Oracle(bstd::mem::format_in!(
                                        cx.frame(),
                                        "duplicate or divergent spawn snapshot: got {ver:?}, expected {prev:?}"
                                    ).as_str().into()));
                                }
                            } else {
                                first_spawn_ok = Some(ver);
                            }
                        }
                        Err(err) => {
                            if !inject_spawn_error {
                                return Err(Broke::Oracle(bstd::mem::format_in!(
                                    cx.frame(),
                                    "unexpected spawn_setting_snapshot_with error: {err}"
                                ).as_str().into()));
                            }
                            let detail = blaze_own::AString::from_str_in(err.detail.as_str(), test_alloc);
                            if let Some(prev) = &first_spawn_err {
                                if prev != &detail {
                                    return Err(Broke::Oracle(bstd::mem::format_in!(
                                        cx.frame(),
                                        "duplicate or divergent snapshot error: got {detail:?}, expected {prev:?}"
                                    ).as_str().into()));
                                }
                            } else {
                                first_spawn_err = Some(detail);
                            }
                        }
                    }
                }
                ProbeOp::CallDepIdentity => {
                    c_calls += 1;
                    s_calls += 1;
                    let cp = probes
                        .c_probes_with(&cx, &open_c)
                        .map_err(|e| Broke::Oracle(bstd::mem::format_in!(cx.frame(), "c_probes_with failed: {e}").as_str().into()))?;
                    match probes.spawn_setting_snapshot_with(&cx, &open_s) {
                        Ok(sp) => {
                            if inject_spawn_error {
                                return Err(Broke::Oracle(
                                    "dep identity saw Ok spawn setting under injected error".into(),
                                ));
                            }
                            let hex = crate::wasm64_compile::dep_probe_identity_from_setting(
                                &cx,
                                crate::graph::TRIPLE,
                                &sp,
                                cp.as_ref(),
                                None,
                                env.heap.alloc(),
                                env.heap.shared(),
                            )
                            .map_err(|e| Broke::Oracle(bstd::mem::format_in!(cx.frame(), "dep_probe_identity_from_setting failed: {e}").as_str().into()))?;
                            if let Some(prev) = first_dep_hex {
                                if prev != hex {
                                    return Err(Broke::Oracle(bstd::mem::format_in!(
                                        cx.frame(),
                                        "duplicate or divergent dep_probe_identity: got {}, expected {}",
                                        hex.as_str(),
                                        prev.as_str()
                                    ).as_str().into()));
                                }
                            } else {
                                first_dep_hex = Some(hex);
                            }
                        }
                        Err(err) => {
                            if !inject_spawn_error {
                                return Err(Broke::Oracle(bstd::mem::format_in!(
                                    cx.frame(),
                                    "unexpected dep identity spawn error: {err}"
                                ).as_str().into()));
                            }
                            let detail = blaze_own::AString::from_str_in(err.detail.as_str(), test_alloc);
                            if let Some(prev) = &first_spawn_err {
                                if prev != &detail {
                                    return Err(Broke::Oracle(bstd::mem::format_in!(
                                        cx.frame(),
                                        "duplicate or divergent dep identity spawn error: got {detail:?}, expected {prev:?}"
                                    ).as_str().into()));
                                }
                            } else {
                                first_spawn_err = Some(detail);
                            }
                        }
                    }
                }
                ProbeOp::CancelledIdentity => {
                    let mut cancelled_ok = false;
                    let _ = cx.scope_with(|scope| {
                        scope.cancel();
                        let sp = SpawnSetting::off();
                        let res = crate::wasm64_compile::dep_probe_identity_from_setting(
                            scope.cx(),
                            crate::graph::TRIPLE,
                            &sp,
                            None,
                            None,
                            env.heap.alloc(),
                            env.heap.shared(),
                        );
                        // With `c_probes = None`, no answerer I/O runs so `dep_probe_identity` is pure.
                        cancelled_ok = res.is_ok();
                    });
                    if !cancelled_ok {
                        return Err(Broke::Oracle(
                            "pure c_probes=None identity unexpectedly failed under cancelled scope".into(),
                        ));
                    }
                }
            }

            // Safety invariants checked after EVERY single real production call:
            let c_count = c_inits.load(Ordering::SeqCst);
            if c_count > 1 {
                return Err(Broke::Oracle(bstd::mem::format_in!(
                    cx.frame(),
                    "duplicate c_probes initialization in SharedCommandProbes: {c_count} inits"
                ).as_str().into()));
            }
            let s_count = s_inits.load(Ordering::SeqCst);
            if s_count > 1 {
                return Err(Broke::Oracle(bstd::mem::format_in!(
                    cx.frame(),
                    "duplicate spawn_setting initialization in SharedCommandProbes: {s_count} inits"
                ).as_str().into()));
            }
        }

        // Exact-once quiescence oracle on the real `SharedCommandProbes` slots:
        let expected_c = u32::from(c_calls > 0);
        let expected_s = u32::from(s_calls > 0);
        if c_inits.load(Ordering::SeqCst) != expected_c {
            return Err(Broke::Oracle(bstd::mem::format_in!(
                cx.frame(),
                "duplicate or missing c_probes init: got {}, expected {expected_c}",
                c_inits.load(Ordering::SeqCst)
            ).as_str().into()));
        }
        if s_inits.load(Ordering::SeqCst) != expected_s {
            return Err(Broke::Oracle(bstd::mem::format_in!(
                cx.frame(),
                "duplicate or missing spawn_setting init: got {}, expected {expected_s}",
                s_inits.load(Ordering::SeqCst)
            ).as_str().into()));
        }
        Ok(())
    }

    #[test]
    fn shared_command_probes_dst_tape_schedule_initializes_once_across_interleavings() {
        sim_check(Config::cases(500), |tape| probe_init_schedule(tape, ProbeOncePlant::None));
    }

    #[test]
    fn shared_command_probes_dst_negative_control_catches_duplicate_init() {
        control(
            Config::cases(500),
            ProbeOncePlant::None,
            ProbeOncePlant::SkipOnce,
            "duplicate",
            probe_init_schedule,
        );
    }

    /// Directly exercises the real `SharedCommandProbes` `bstd::sync::Once` slots on `Io::sim`
    /// (zero OS threads): verifies single-init caching, sticky error publication across callers,
    /// and the `ProbeOncePlant::SkipOnce` negative control.
    #[test]
    fn shared_command_probes_real_once_single_init_and_sticky_error() {
        let io = Io::simulated(SimWorld::new(42, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root sim cx");
        let env = sim_env(&io);

        let shared = env.heap.shared();
        // 1. Real `c_probes` initializes exactly once and returns the exact same setting.
        let holder = SharedCommandProbes::new();
        let probes = holder.with_env(&env);
        let c_inits = AtomicU32::new(0);
        for _ in 0..4 {
            let res = probes
                .c_probes_with(&cx, |_| {
                    let n = c_inits.fetch_add(1, Ordering::SeqCst) + 1;
                    Some(CProbeSetting::Unavailable(blaze_own::Arc::from_str_in(
                        bstd::mem::format_in!(cx.frame(), "ok-{n}").as_str(),
                        shared,
                    )))
                })
                .expect("c_probes succeeds");
            assert!(matches!(res, Some(CProbeSetting::Unavailable(ref s)) if s.as_ref() == "ok-1"));
        }
        assert_eq!(c_inits.load(Ordering::SeqCst), 1);

        // 2. Injected spawn-pin error is published once and returned identically to all callers.
        let err_holder = SharedCommandProbes::new();
        let err_probes = err_holder.with_env(&env);
        let s_inits = AtomicU32::new(0);
        for _ in 0..4 {
            let err = err_probes
                .spawn_setting_snapshot_with(&cx, |_| {
                    let n = s_inits.fetch_add(1, Ordering::SeqCst) + 1;
                    Err(ProbeSetupError::from_str(
                        ProbeErrorKind::SpawnPinsOpen,
                        bstd::mem::format_in!(cx.frame(), "injected plist error #{n}").as_str(),
                        shared,
                    ))
                })
                .expect_err("injected spawn error must propagate");
            assert_eq!(err.detail.as_str(), "injected plist error #1");
        }
        assert_eq!(s_inits.load(Ordering::SeqCst), 1);

        // 3. Real `SharedCommandProbes` negative control under `ProbeOncePlant::SkipOnce`.
        let planted_holder = SharedCommandProbes::new_with_plant(ProbeOncePlant::SkipOnce);
        let planted = planted_holder.with_env(&env);
        let p_inits = AtomicU32::new(0);
        for _ in 0..2 {
            let _ = planted.spawn_setting_snapshot_with(&cx, |_| {
                p_inits.fetch_add(1, Ordering::SeqCst);
                Ok(SpawnSetting::facts_only(FactPins {
                    clang_version_text: "clang version 23.0.0".into(),
                    macos_product_version: Default::default(),
                    macos_build_version: Default::default(),
                }))
            });
        }
        assert_eq!(p_inits.load(Ordering::SeqCst), 2, "SkipOnce plant must duplicate init");
    }

    #[test]
    fn native_script_skips_wasm_dep_scan_when_wasm_graph_is_none() {
        let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root sim cx");
        let env = sim_env(&io);
        let holder = SharedCommandProbes::new();
        let probes = holder.with_env(&env);

        let frame = bstd::mem::Frame::with_reserve(2 << 20, bstd::mem::Tag::Other).expect("frame");
        let erased = blaze_own::Erased::new(&frame);
        let a = erased.handle();
        let mut units = blaze_own::Vec::new_in(a);
        units.push(blaze_unit_graph::Unit {
            package: blaze_manifest::lock::LockId(0),
            name: blaze_own::String::from_str_in("native-pkg", a),
            version: blaze_own::String::from_str_in("0.1.0", a),
            kind: blaze_unit_graph::UnitKind::BuildScript,
            for_host: true,
            feature_side: blaze_unit_graph::HostFeatureSide::Target,
            edition: edition::PackageEdition::of_manifest("2024").expect("edition"),
            crate_name: blaze_own::String::from_str_in("build_script_build", a),
            root: blaze_own::String::from_str_in("build.rs", a),
            features: blaze_own::Vec::new_in(a),
            cfg: blaze_own::Vec::new_in(a),
            deps: blaze_own::Vec::new_in(a),
            build_script: None,
            links: None,
        });
        let graph = blaze_unit_graph::UnitGraph {
            resolver: blaze_unit_graph::Resolver::V3,
            units,
            meta: blaze_own::Vec::new_in(a),
            left_out: blaze_own::Vec::new_in(a),
        };

        // Native build script with `wasm_graph = None` MUST succeed untouched without
        // requiring `wasm64-blaze-wasip1` or initializing `SharedCommandProbes`.
        let mod_subj = blaze_ids::ContentId([0x33; 32]);
        let native_res = super::fold_wasm_script_dep_probes(
            &cx,
            super::RanAs::Native,
            &graph,
            None,
            blaze_unit_graph::UnitId(0),
            mod_subj,
            Some(&[]),
            Some("orig-deps"),
            probes,
            &frame,
        )
        .expect("native script must skip wasm_graph scan when wasm_graph is None");
        assert_eq!(native_res, Some("orig-deps"));
        assert!(!holder.c_probes_initialized());
        assert!(!holder.spawn_setting_initialized());

        // Contrast: Wasm build script with `wasm_graph = None` MUST refuse with `WasmGraphMissing`.
        let wasm_err = super::fold_wasm_script_dep_probes(
            &cx,
            super::RanAs::Wasm,
            &graph,
            None,
            blaze_unit_graph::UnitId(0),
            mod_subj,
            Some(&[]),
            Some("orig-deps"),
            probes,
            &frame,
        )
        .expect_err("wasm script must refuse when wasm_graph is None");
        assert_eq!(wasm_err.kind, ProbeErrorKind::WasmGraphMissing);
    }

    #[test]
    fn start_time_probes_reuse_check_time_snapshot_after_live_answerer_drift() {
        use blaze_build_script::c_tasks_probe::{CanonicalProbe, ProbeAnswer, ProbeAnswerer};
        use blaze_ids::ContentId;
        use blaze_proc_macro::probe_clang::{CProbeStore, CScriptProbes, MemCProbeStore};

        struct MutableAnswerer(blaze_own::Arc<core::sync::atomic::AtomicU8, SharedAlloc<'static>>);
        impl ProbeAnswerer for MutableAnswerer {
            fn id<'a>(&self, _alloc: blaze_own::Alloc<'a>) -> Result<ContentId, blaze_own::AString<'a>> {
                Ok(ContentId([self.0.load(Ordering::SeqCst); 32]))
            }
            fn answer<'a, 'b>(
                &self,
                _probe: &CanonicalProbe<'_, '_>,
                _input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
                alloc: blaze_own::Alloc<'a>,
            ) -> Result<ProbeAnswer<'a>, blaze_own::AString<'a>> {
                Err(blaze_own::AString::from_str_in("unused", alloc))
            }
        }

        let io = Io::simulated(SimWorld::new(72, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root sim cx");
        let env = sim_env(&io);
        let shared = env.heap.shared();
        let state = blaze_own::Arc::new_in(core::sync::atomic::AtomicU8::new(0xaa), shared);
        let answerer: blaze_own::Arc<dyn ProbeAnswerer + Send + Sync, SharedAlloc<'static>> =
            blaze_own::Arc::new_in(MutableAnswerer(blaze_own::Share::share(&state)), shared);
        let records: blaze_own::Arc<dyn CProbeStore, SharedAlloc<'static>> =
            blaze_own::Arc::new_in(MemCProbeStore::default(), shared);
        let ready = CProbeSetting::Ready(CScriptProbes {
            answerer,
            #[cfg(target_os = "linux")]
            hermetic: None,
            records,
            sdk_dir: blaze_own::Arc::from_str_in("", shared),
            sdk_id: blaze_own::Arc::from_str_in("", shared),
            sdk_digest: [0; 32],
            clang_sha256: [0x99; 32],
        });

        // Simulate check phase populating `SharedCommandProbes` once (as `checking` does).
        let holder = SharedCommandProbes::new();
        let check_probes = holder.with_env(&env);
        let c_opens = AtomicU32::new(0);
        let s_opens = AtomicU32::new(0);
        let _ = check_probes
            .c_probes_with(&cx, |_| {
                c_opens.fetch_add(1, Ordering::SeqCst);
                Some(ready)
            })
            .expect("seed c_probes");
        let _ = check_probes
            .spawn_setting_with(&cx, |_| {
                s_opens.fetch_add(1, Ordering::SeqCst);
                Ok(SpawnSetting::facts_only(FactPins {
                    clang_version_text: "clang version 23.0.0".into(),
                    macos_product_version: Default::default(),
                    macos_build_version: Default::default(),
                }))
            })
            .expect("seed spawn_setting");
        let check_hex = check_probes
            .dep_probe_identity_hex(&cx)
            .expect("check-time dep_probe_identity_hex");
        holder.publish_checked_dep_probe_hex(check_hex);
        assert_eq!(c_opens.load(Ordering::SeqCst), 1);
        assert_eq!(s_opens.load(Ordering::SeqCst), 1);

        // Host ELF closure shifts between `check` and start-time `run`/`test`/`export`!
        state.store(0xbb, Ordering::SeqCst);

        // Check-time handles (without `.with_checked_snapshot()`) still observe live drift so
        // mid-compile revalidation keeps refusing drifted publications.
        let live_drifted_hex = check_probes
            .dep_probe_identity_hex(&cx)
            .expect("live check handle observes drift");
        assert_ne!(live_drifted_hex, check_hex);

        // Start-time `RunProcs` handle (borrowing the same `SharedCommandProbes` via `Standing`
        // with `.with_checked_snapshot()`) MUST reuse the exact check-time probe snapshot and
        // never reopen `c_probes` or `spawn_setting`.
        let start_probes = holder.with_env(&env).with_sdk(env.sdk).with_checked_snapshot();
        let start_hex = start_probes
            .dep_probe_identity_hex(&cx)
            .expect("start-time dep_probe_identity_hex reuses check snapshot");
        assert_eq!(
            start_hex, check_hex,
            "start-time RunProcs must reuse check-time probe identity snapshot instead of rereading drifted closure"
        );
        assert_eq!(c_opens.load(Ordering::SeqCst), 1);
        assert_eq!(s_opens.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn certificate_dep_probes_leaf_validates_populates_snapshot_and_invalidates_on_drift() {
        use blaze_build_script::c_tasks_probe::{CanonicalProbe, ProbeAnswer, ProbeAnswerer};
        use blaze_ids::ContentId;
        use blaze_proc_macro::probe_clang::{CProbeStore, CScriptProbes, MemCProbeStore};

        struct MutableAnswerer(blaze_own::Arc<core::sync::atomic::AtomicU8, SharedAlloc<'static>>);
        impl ProbeAnswerer for MutableAnswerer {
            fn id<'a>(&self, _alloc: blaze_own::Alloc<'a>) -> Result<ContentId, blaze_own::AString<'a>> {
                Ok(ContentId([self.0.load(Ordering::SeqCst); 32]))
            }
            fn answer<'a, 'b>(
                &self,
                _probe: &CanonicalProbe<'_, '_>,
                _input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
                alloc: blaze_own::Alloc<'a>,
            ) -> Result<ProbeAnswer<'a>, blaze_own::AString<'a>> {
                Err(blaze_own::AString::from_str_in("unused", alloc))
            }
        }

        let io = Io::simulated(SimWorld::new(73, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root sim cx");
        let fs = io.need_fs().expect("sim fs");
        let root = fs.root();
        let env = sim_env(&io);
        let shared = env.heap.shared();
        let frame = cx.frame();

        let make_ready = |state: &blaze_own::Arc<core::sync::atomic::AtomicU8, SharedAlloc<'static>>| {
            let answerer: blaze_own::Arc<dyn ProbeAnswerer + Send + Sync, SharedAlloc<'static>> =
                blaze_own::Arc::new_in(MutableAnswerer(blaze_own::Share::share(state)), shared);
            let records: blaze_own::Arc<dyn CProbeStore, SharedAlloc<'static>> =
                blaze_own::Arc::new_in(MemCProbeStore::default(), shared);
            CProbeSetting::Ready(CScriptProbes {
                answerer,
                #[cfg(target_os = "linux")]
                hermetic: None,
                records,
                sdk_dir: blaze_own::Arc::from_str_in("", shared),
                sdk_id: blaze_own::Arc::from_str_in("", shared),
                sdk_digest: [0; 32],
                clang_sha256: [0x99; 32],
            })
        };

        // 0. Zero-overhead workspace without `LeafKind::DepProbes`: sweep NEVER invokes `dep_probes_fn`.
        let plain_rows = crate::sweep::Rows::new_in(frame);
        let plain_bytes = crate::sweep::Leaves::bytes_in(&plain_rows, frame).expect("plain leaves");
        let (plain_leaves, _) = crate::sweep::Leaves::of(&plain_bytes).expect("parse plain leaves");
        let plain_calls = AtomicU32::new(0);
        let plain_cb = || -> Result<ContentId, crate::CliError> {
            plain_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ContentId([0; 32]))
        };
        let follow = crate::sweep::Follow {
            stamps: crate::sweep::FromMeta::now(&env.clock),
            plant: crate::sweep::Plant::None,
        };
        let (plain_followed, _) = follow
            .follow_served_with_sdk(&cx, &root, None, None, &plain_leaves, None, None, None, Some(&plain_cb), frame)
            .expect("follow plain");
        assert_eq!(plain_calls.load(Ordering::SeqCst), 0, "workspaces without DepProbes leaf must pay zero probe overhead");
        assert!(!plain_followed.now.rows.iter().zip(plain_followed.was.iter()).any(|(n, w)| n.state() != *w));

        // 1. Seed initial stored leaf table with probe identity A (answerer 0xaa, clang 23.0.0).
        let state = blaze_own::Arc::new_in(core::sync::atomic::AtomicU8::new(0xaa), shared);
        let holder1 = SharedCommandProbes::new();
        let probes1 = holder1.with_env(&env);
        let _ = probes1.c_probes_with(&cx, |_| Some(make_ready(&state))).expect("seed c1");
        let _ = probes1
            .spawn_setting_with(&cx, |_| {
                Ok(SpawnSetting::facts_only(FactPins {
                    clang_version_text: "clang version 23.0.0".into(),
                    macos_product_version: Default::default(),
                    macos_build_version: Default::default(),
                }))
            })
            .expect("seed s1");
        let hex_a = probes1.dep_probe_identity_hex(&cx).expect("hex A");

        let mut stored_rows = crate::sweep::Rows::new_in(frame);
        crate::sweep::record_dep_probes(&mut stored_rows, hex_a.to_content_id());
        let stored_bytes = crate::sweep::Leaves::bytes_in(&stored_rows, frame).expect("encode leaves");
        let (stored_leaves, _) = crate::sweep::Leaves::of(&stored_bytes).expect("parse leaves");

        // 2. No-op certificate command with UNCHANGED probe inputs (fresh `SharedCommandProbes`
        //    as a new `blaze run`/`test`/`export` command creates):
        //    - Sweep sees `!moved` (certificate hit!),
        //    - AND populates `holder2`'s checked snapshot so start-time `with_checked_snapshot()`
        //      returns `hex_a` even if the live answerer shifts after the certificate check!
        let holder2 = SharedCommandProbes::new();
        let probes2 = holder2.with_env(&env);
        let _ = probes2.c_probes_with(&cx, |_| Some(make_ready(&state))).expect("seed c2");
        let _ = probes2
            .spawn_setting_with(&cx, |_| {
                Ok(SpawnSetting::facts_only(FactPins {
                    clang_version_text: "clang version 23.0.0".into(),
                    macos_product_version: Default::default(),
                    macos_build_version: Default::default(),
                }))
            })
            .expect("seed s2");
        let cert_hex_cell = core::cell::Cell::new(None);
        let cert_cb = || -> Result<ContentId, crate::CliError> {
            let hex = probes2.dep_probe_identity_hex(&cx).expect("live cert hex");
            cert_hex_cell.set(Some(hex));
            Ok(hex.to_content_id())
        };
        let (cert_followed, _) = follow
            .follow_served_with_sdk(&cx, &root, None, None, &stored_leaves, Some(&[false]), None, None, Some(&cert_cb), frame)
            .expect("follow cert hit");
        let cert_moved = cert_followed.layout_moved
            || cert_followed.now.rows.iter().zip(cert_followed.was.iter()).any(|(n, w)| n.state() != *w);
        assert!(!cert_moved, "unchanged probe inputs must allow certificate hit (even with unmarked journal slice)");
        holder2.publish_checked_dep_probe_hex(cert_hex_cell.get().expect("cert callback ran"));

        // Now shift live answerer AFTER the certificate hit: start-time runner reusing `holder2`
        // MUST return `hex_a` from the certificate-populated checked snapshot!
        state.store(0xbb, Ordering::SeqCst);
        let start_hex = holder2
            .with_env(&env)
            .with_checked_snapshot()
            .dep_probe_identity_hex(&cx)
            .expect("start snapshot after cert hit");
        assert_eq!(start_hex, hex_a, "certificate hit must populate checked snapshot for start-time runners");

        // 3. Changing clang runtime closure (0xaa -> 0xbb) MUST invalidate the certificate (`moved == true`),
        //    while keeping `layout_moved == false` so `Stood` graph read-back still applies.
        let holder3 = SharedCommandProbes::new();
        let probes3 = holder3.with_env(&env);
        let _ = probes3.c_probes_with(&cx, |_| Some(make_ready(&state))).expect("seed c3");
        let _ = probes3
            .spawn_setting_with(&cx, |_| {
                Ok(SpawnSetting::facts_only(FactPins {
                    clang_version_text: "clang version 23.0.0".into(),
                    macos_product_version: Default::default(),
                    macos_build_version: Default::default(),
                }))
            })
            .expect("seed s3");
        let drift_cb = || -> Result<ContentId, crate::CliError> {
            let hex = probes3.dep_probe_identity_hex(&cx).expect("drifted hex");
            Ok(hex.to_content_id())
        };
        let (drift_followed, _) = follow
            .follow_served_with_sdk(&cx, &root, None, None, &stored_leaves, Some(&[false]), None, None, Some(&drift_cb), frame)
            .expect("follow drifted closure");
        let drift_moved = drift_followed.layout_moved
            || drift_followed.now.rows.iter().zip(drift_followed.was.iter()).any(|(n, w)| n.state() != *w);
        assert!(drift_moved, "changed clang runtime closure must invalidate certificate hit");
        assert!(!drift_followed.layout_moved, "DepProbes leaf must keep POST_GRAPH_MACRO so Stood graph stands");

        // 4. Changing derived-fact pins (clang version 23.0.0 -> 23.1.0) with restored answerer 0xaa
        //    MUST also invalidate the certificate (`moved == true`).
        state.store(0xaa, Ordering::SeqCst);
        let holder4 = SharedCommandProbes::new();
        let probes4 = holder4.with_env(&env);
        let _ = probes4.c_probes_with(&cx, |_| Some(make_ready(&state))).expect("seed c4");
        let _ = probes4
            .spawn_setting_with(&cx, |_| {
                Ok(SpawnSetting::facts_only(FactPins {
                    clang_version_text: "clang version 23.1.0".into(),
                    macos_product_version: Default::default(),
                    macos_build_version: Default::default(),
                }))
            })
            .expect("seed s4");
        let pin_cb = || -> Result<ContentId, crate::CliError> {
            let hex = probes4.dep_probe_identity_hex(&cx).expect("pin drifted hex");
            Ok(hex.to_content_id())
        };
        let (pin_followed, _) = follow
            .follow_served_with_sdk(&cx, &root, None, None, &stored_leaves, Some(&[false]), None, None, Some(&pin_cb), frame)
            .expect("follow pin drift");
        let pin_moved = pin_followed.layout_moved
            || pin_followed.now.rows.iter().zip(pin_followed.was.iter()).any(|(n, w)| n.state() != *w);
        assert!(pin_moved, "changed derived-fact pins must invalidate certificate hit");
    }

    #[test]
    fn pinned_standing_seeds_checked_probe_snapshot_from_leaf_or_stored_record() {
        use blaze_build_script::c_tasks_probe::{CanonicalProbe, ProbeAnswer, ProbeAnswerer};
        use blaze_drive::{Kind, Name, Tool};
        use blaze_engine::{Cause, Gen, Phase, Row, Stage, Tools, pages};
        use blaze_ids::ContentId;
        use blaze_proc_macro::probe_clang::{CProbeStore, CScriptProbes, MemCProbeStore};

        struct MutableAnswerer(blaze_own::Arc<core::sync::atomic::AtomicU8, SharedAlloc<'static>>);
        impl ProbeAnswerer for MutableAnswerer {
            fn id<'a>(&self, _alloc: blaze_own::Alloc<'a>) -> Result<ContentId, blaze_own::AString<'a>> {
                Ok(ContentId([self.0.load(Ordering::SeqCst); 32]))
            }
            fn answer<'a, 'b>(
                &self,
                _probe: &CanonicalProbe<'_, '_>,
                _input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
                alloc: blaze_own::Alloc<'a>,
            ) -> Result<ProbeAnswer<'a>, blaze_own::AString<'a>> {
                Err(blaze_own::AString::from_str_in("unused", alloc))
            }
        }

        let io = Io::simulated(SimWorld::new(74, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root sim cx");
        let fs = io.need_fs().expect("sim fs");
        let clock = io.need_clock().expect("sim clock");
        let err = io.need_err().expect("sim err");
        let env = sim_env(&io);
        let shared = env.heap.shared();
        let home = crate::store::Home::of(&cx, &fs, Some("/h")).expect("home");
        let heap = blaze_own::testing::heap();
        let alloc = heap.shared();
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);
        let lent = home
            .open_store(&blaze_cas::plain::Settings::default(), crate::store::Facts { boot: Some(1), nonce: 74 }, heap.alloc())
            .expect("open store");
        let mut store = crate::store::CommandStore::new(&cx, &lent, false, 0, None, clock, &err, erased.handle())
            .expect("command store");
        let tools = Tools([Tool(ContentId([0x44; 32])); blaze_drive::key::KINDS]);

        let hex_a = crate::wasm64_compile::Hex64::from_bytes([0xa5; 32]);
        assert_eq!(
            crate::wasm64_compile::Hex64::from_bytes(hex_a.to_content_id().0),
            hex_a,
            "Hex64 <-> ContentId round-trip must be exact"
        );

        // Build a pinned `MacroModule` answer containing `hex_a` in its record payload.
        let pm_subj = crate::macros::subject_of("sqlx-macros", "0.9.0");
        let pm_name = Name::new(Kind::MacroModule, pm_subj);
        let pm_src = ContentId([0x55; 32]);
        let pm_rec_bytes = blaze_drive::record::write_plain(
            pm_name,
            pm_src,
            ContentId([0x88; 32]),
            &[],
            hex_a.as_str().as_bytes(),
            erased.handle(),
        );
        let pm_rec_key = blaze_drive::key::content_key(&pm_rec_bytes);
        crate::store::Store::put(&mut store, &cx, pm_rec_key, &pm_rec_bytes).expect("put pm rec");

        let mut kept = pages::Kept::new_in(alloc);
        kept.answer.serial = Gen(1);
        kept.answer.by_file.entry(pm_name).or_insert_with(|| blaze_own::Vec::new_in(alloc)).push(pm_name);
        kept.answer.rows.insert(
            pm_name,
            Row {
                key: tools.key(pm_name, &pm_src),
                phase: Phase::new(Stage::Scripts, 0),
                file: pm_name,
                source: pm_src,
                out: ContentId([0x88; 32]),
                serial: Gen(1),
                defines: blaze_own::Vec::new_in(alloc),
                reads: blaze_own::Vec::new_in(alloc),
                by_cell: blaze_own::Vec::new_in(alloc),
                findings: 0,
                inline: false,
                failed: false,
            },
        );
        kept.causes.insert(pm_name, (Gen(1), Cause::New));
        kept.records.insert(pm_name, pm_rec_key);
        let enc = pages::encode_at_bits(&kept, 4, alloc);
        for (k, b) in enc.puts.iter() {
            crate::store::Store::put(&mut store, &cx, *k, b).expect("put page");
        }
        let cert = enc.root.0;
        let stood_root = crate::engine_answer::open_root(&cx, &mut store, enc.root, alloc)
            .expect("open root io")
            .expect("valid root");
        let snap = store.snapshot_in(alloc).expect("snapshot");
        let headers = pages::Headers::Absent { since: Gen(0) };
        let stored = crate::engine_answer::Stored::new(&stood_root, &snap, &headers, pages::Check::Store, alloc);

        // Live host answerer has ALREADY drifted to 0xbb before the pinned launch runs!
        let drifted_state = blaze_own::Arc::new_in(core::sync::atomic::AtomicU8::new(0xbb), shared);
        let make_drifted_ready = || {
            let answerer: blaze_own::Arc<dyn ProbeAnswerer + Send + Sync, SharedAlloc<'static>> =
                blaze_own::Arc::new_in(MutableAnswerer(blaze_own::Share::share(&drifted_state)), shared);
            let records: blaze_own::Arc<dyn CProbeStore, SharedAlloc<'static>> =
                blaze_own::Arc::new_in(MemCProbeStore::default(), shared);
            CProbeSetting::Ready(CScriptProbes {
                answerer,
                #[cfg(target_os = "linux")]
                hermetic: None,
                records,
                sdk_dir: blaze_own::Arc::from_str_in("", shared),
                sdk_id: blaze_own::Arc::from_str_in("", shared),
                sdk_digest: [0; 32],
                clang_sha256: [0x99; 32],
            })
        };

        // Case 1: Checkout pointer absent/moved -> `seed_pinned_dep_probes` recovers `hex_a`
        // directly from the pinned `stood_root`'s `MacroModule` record payload.
        let holder_rec = SharedCommandProbes::new();
        let seeded_rec = crate::engine_check::seed_pinned_dep_probes(
            &cx,
            &mut store,
            &stored,
            &stood_root,
            cert,
            "/pinned-checkout",
            crate::engine_check::CheckScope::Whole,
            &holder_rec,
            frame,
        )
        .expect("seed from record");
        assert_eq!(seeded_rec, Some(hex_a));
        let _ = holder_rec.with_env(&env).c_probes_with(&cx, |_| Some(make_drifted_ready()));
        let got_rec = holder_rec
            .with_env(&env)
            .with_checked_snapshot()
            .dep_probe_identity_hex(&cx)
            .expect("pinned start snapshot from record");
        assert_eq!(got_rec, hex_a, "pinned launch must return cert's probe tag even after live drift");

        // Case 2: Pointer matching `cert` with `LeafKind::DepProbes` carrying `hex_b` seeds
        // directly from the certificate's `DepProbes` leaf (O(leaves), zero MacroModule reads).
        let hex_b = crate::wasm64_compile::Hex64::from_bytes([0xb6; 32]);
        let mut ptr_rows = crate::sweep::Rows::new_in(frame);
        crate::sweep::record_dep_probes(&mut ptr_rows, hex_b.to_content_id());
        let root_name = crate::engine_answer::root_name("/pinned-checkout", frame);
        let leaves = crate::sweep::Leaves::bytes_in(&ptr_rows, frame).expect("leaves");
        let mut ptr_bytes: blaze_own::Vec<u8, &bstd::mem::Frame> = blaze_own::Vec::new_in(frame);
        ptr_bytes.extend_from_slice(crate::engine_answer::POINTER_MAGIC);
        ptr_bytes.extend_from_slice(&enc.root.0.0);
        ptr_bytes.extend_from_slice(&[0x99u8; 32]);
        ptr_bytes.extend_from_slice(&enc.files.to_le_bytes());
        ptr_bytes.push(0x07u8);
        ptr_bytes.extend_from_slice(&enc.findings.to_le_bytes());
        ptr_bytes.extend_from_slice(&enc.owed.to_le_bytes());
        ptr_bytes.extend_from_slice(&[0u8; 32]);
        ptr_bytes.push(1u8);
        ptr_bytes.extend_from_slice(&0u64.to_le_bytes());
        ptr_bytes.extend_from_slice(&[0u8; 32 * pages::HeaderPages::COUNT]);
        crate::journal_sweep::Stored::encode_units(core::iter::empty(), None, &mut ptr_bytes);
        let sealed = crate::seal::seal(&ptr_bytes, &leaves, frame).expect("seal");
        let ptr_key = blaze_drive::key::content_key(&sealed);
        crate::store::Store::put(&mut store, &cx, ptr_key, &sealed).expect("put pointer");
        store.pin(root_name.as_str(), &[ptr_key]).expect("pin pointer");
        lent.commit().expect("commit pointer");
        let loaded_ptr = crate::engine_answer::load_pointer(&cx, &mut store, root_name.as_str())
            .expect("load pointer io")
            .expect("pointer present")
            .expect("pointer readable");
        assert_eq!(loaded_ptr.root, enc.root);

        let holder_leaf = SharedCommandProbes::new();
        let seeded_leaf = crate::engine_check::seed_pinned_dep_probes(
            &cx,
            &mut store,
            &stored,
            &stood_root,
            cert,
            "/pinned-checkout",
            crate::engine_check::CheckScope::Whole,
            &holder_leaf,
            frame,
        )
        .expect("seed from pointer leaf");
        assert_eq!(seeded_leaf, Some(hex_b));
        let got_leaf = holder_leaf
            .with_env(&env)
            .with_checked_snapshot()
            .dep_probe_identity_hex(&cx)
            .expect("pinned start snapshot from leaf");
        assert_eq!(got_leaf, hex_b, "pinned launch must restore pointer DepProbes leaf tag");
    }

    #[test]
    fn completed_check_probe_snapshot_overwrites_earlier_preflight_observation() {
        use blaze_build_script::c_tasks_probe::{CanonicalProbe, ProbeAnswer, ProbeAnswerer};
        use blaze_ids::ContentId;
        use blaze_proc_macro::probe_clang::{CProbeStore, CScriptProbes, MemCProbeStore};

        struct MutableAnswerer(blaze_own::Arc<core::sync::atomic::AtomicU8, SharedAlloc<'static>>);
        impl ProbeAnswerer for MutableAnswerer {
            fn id<'a>(&self, _alloc: blaze_own::Alloc<'a>) -> Result<ContentId, blaze_own::AString<'a>> {
                Ok(ContentId([self.0.load(Ordering::SeqCst); 32]))
            }
            fn answer<'a, 'b>(
                &self,
                _probe: &CanonicalProbe<'_, '_>,
                _input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
                alloc: blaze_own::Alloc<'a>,
            ) -> Result<ProbeAnswer<'a>, blaze_own::AString<'a>> {
                Err(blaze_own::AString::from_str_in("unused", alloc))
            }
        }

        let io = Io::simulated(SimWorld::new(75, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root sim cx");
        let env = sim_env(&io);
        let shared = env.heap.shared();
        let state = blaze_own::Arc::new_in(core::sync::atomic::AtomicU8::new(0xaa), shared);

        let holder = SharedCommandProbes::new();
        let check_probes = holder.with_env(&env);
        let _ = check_probes
            .c_probes_with(&cx, |_| {
                let answerer: blaze_own::Arc<dyn ProbeAnswerer + Send + Sync, SharedAlloc<'static>> =
                    blaze_own::Arc::new_in(MutableAnswerer(blaze_own::Share::share(&state)), shared);
                let records: blaze_own::Arc<dyn CProbeStore, SharedAlloc<'static>> =
                    blaze_own::Arc::new_in(MemCProbeStore::default(), shared);
                Some(CProbeSetting::Ready(CScriptProbes {
                    answerer,
                    #[cfg(target_os = "linux")]
                    hermetic: None,
                    records,
                    sdk_dir: blaze_own::Arc::from_str_in("", shared),
                    sdk_id: blaze_own::Arc::from_str_in("", shared),
                    sdk_digest: [0; 32],
                    clang_sha256: [0x99; 32],
                }))
            })
            .expect("seed c_probes");
        let _ = check_probes
            .spawn_setting_with(&cx, |_| {
                Ok(SpawnSetting::facts_only(FactPins {
                    clang_version_text: "clang version 23.0.0".into(),
                    macos_product_version: Default::default(),
                    macos_build_version: Default::default(),
                }))
            })
            .expect("seed spawn_setting");

        // 1. Certificate check or ready preflight observes tag A (and even if an earlier check
        //    stage published A, subsequent phase planning must be able to supersede it).
        let hex_a = check_probes.dep_probe_identity_hex(&cx).expect("preflight hex A");
        holder.publish_checked_dep_probe_hex(hex_a);

        // 2. Host clang runtime closure shifts to B before phase planning runs.
        state.store(0xbb, Ordering::SeqCst);
        let hex_b = check_probes.dep_probe_identity_hex(&cx).expect("phase-plan hex B");
        assert_ne!(hex_b, hex_a);
        holder.publish_checked_dep_probe_hex(hex_b);

        // 3. Host clang runtime closure shifts AGAIN to C after phase planning (mid-compile /
        //    post-check drift): live revalidation observes C WITHOUT clobbering published B.
        state.store(0xcc, Ordering::SeqCst);
        let hex_c = check_probes.dep_probe_identity_hex(&cx).expect("live drifted hex C");
        assert_ne!(hex_c, hex_b);

        // 4. Start-time `RunProcs` (`with_checked_snapshot`) MUST return B — the tag selected
        //    by the completed check — NOT stale preflight A and NOT post-check drifted C.
        let start_hex = holder
            .with_env(&env)
            .with_checked_snapshot()
            .dep_probe_identity_hex(&cx)
            .expect("start snapshot must return B");
        assert_eq!(start_hex, hex_b, "start snapshot must return phase-planned tag B, not stale preflight A or drifted C");

        // 5. Clearing the published snapshot drops B so a subsequent fallback sees live C.
        holder.clear_checked_dep_probe_hex();
        let after_clear = holder
            .with_env(&env)
            .with_checked_snapshot()
            .dep_probe_identity_hex(&cx)
            .expect("cleared snapshot falls back to live");
        assert_eq!(after_clear, hex_c);
    }

    fn checked_hex_slot_schedule(tape: &Tape, plant: super::CheckedHexPlant) -> Result<(), Broke> {
        let slot = super::CheckedHexSlot::new_with_plant(plant);
        let steps = *tape.one_of(&[8usize, 16, 24, 32], "seqlock steps");
        let palette = [
            crate::wasm64_compile::Hex64::from_bytes([0x11; 32]),
            crate::wasm64_compile::Hex64::from_bytes([0x22; 32]),
            crate::wasm64_compile::Hex64::from_bytes([0x33; 32]),
            crate::wasm64_compile::Hex64::from_bytes([0x44; 32]),
        ];
        let mut expected: Option<crate::wasm64_compile::Hex64> = None;
        for _ in 0..steps {
            let op = *tape.one_of(&[0u8, 1, 2, 3], "seqlock op");
            match op {
                0 => {
                    // Interleaved publish with a concurrent mid-write reader probe after words[0..2]
                    // have been overwritten while words[2..4] still hold the previous digest!
                    let next = *tape.one_of(&palette, "next hex");
                    let mut mid_err: Option<Broke> = None;
                    let prev = expected;
                    slot.publish_with_hook(next, || match slot.try_load_once() {
                        Err(()) => {}
                        Ok(observed) => {
                            // A concurrent read mid-write must NEVER observe a torn combination of
                            // old and new words (nor an unverified in-flight write).
                            let valid = observed == prev || observed == Some(next);
                            if !valid || (prev.is_some() && prev != Some(next)) {
                                let frame = bstd::mem::Frame::new(bstd::mem::Tag::Other).expect("frame");
                                mid_err = Some(Broke::Oracle(bstd::mem::format_in!(
                                    &frame,
                                    "mid-write seqlock read returned {:?} while transitioning {:?} -> {:?}",
                                    observed.map(|h| blaze_own::AString::from_str_in(h.as_str(), blaze_own::testing::heap().alloc())),
                                    prev.map(|h| blaze_own::AString::from_str_in(h.as_str(), blaze_own::testing::heap().alloc())),
                                    next.as_str(),
                                ).as_str().into()));
                            }
                        }
                    });
                    if let Some(err) = mid_err {
                        return Err(err);
                    }
                    expected = Some(next);
                }
                1 => {
                    slot.clear();
                    expected = None;
                }
                2 => {
                    let got = slot.load();
                    if got != expected {
                        let frame = bstd::mem::Frame::new(bstd::mem::Tag::Other).expect("frame");
                        return Err(Broke::Oracle(bstd::mem::format_in!(
                            &frame,
                            "load() returned {:?}, expected {:?}",
                            got.map(|h| blaze_own::AString::from_str_in(h.as_str(), blaze_own::testing::heap().alloc())),
                            expected.map(|h| blaze_own::AString::from_str_in(h.as_str(), blaze_own::testing::heap().alloc())),
                        ).as_str().into()));
                    }
                }
                _ => {
                    let got = slot
                        .try_load_once()
                        .map_err(|()| Broke::Oracle("quiescent try_load_once unexpectedly retried".into()))?;
                    if got != expected {
                        let frame = bstd::mem::Frame::new(bstd::mem::Tag::Other).expect("frame");
                        return Err(Broke::Oracle(bstd::mem::format_in!(
                            &frame,
                            "quiescent try_load_once returned {:?}, expected {:?}",
                            got.map(|h| blaze_own::AString::from_str_in(h.as_str(), blaze_own::testing::heap().alloc())),
                            expected.map(|h| blaze_own::AString::from_str_in(h.as_str(), blaze_own::testing::heap().alloc())),
                        ).as_str().into()));
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn checked_hex_slot_dst_never_returns_torn_reads_and_catches_planted_race() {
        sim_check(Config::cases(500), |tape| {
            checked_hex_slot_schedule(tape, super::CheckedHexPlant::None)
        });
        control(
            Config::cases(500),
            super::CheckedHexPlant::None,
            super::CheckedHexPlant::SkipSeqCheck,
            "mid-write",
            checked_hex_slot_schedule,
        );
    }

    #[test]
    fn pinned_standing_propagates_missing_and_malformed_macro_module_records() {
        use blaze_drive::{Key, Kind, Name, Tool};
        use blaze_engine::{Cause, Gen, Phase, Row, Stage, Tools, pages};
        use blaze_ids::ContentId;

        let io = Io::simulated(SimWorld::new(77, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root sim cx");
        let fs = io.need_fs().expect("sim fs");
        let clock = io.need_clock().expect("sim clock");
        let err = io.need_err().expect("sim err");
        let home = crate::store::Home::of(&cx, &fs, Some("/h")).expect("home");
        let heap = blaze_own::testing::heap();
        let alloc = heap.shared();
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);
        let lent = home
            .open_store(&blaze_cas::plain::Settings::default(), crate::store::Facts { boot: Some(1), nonce: 77 }, heap.alloc())
            .expect("open store");
        let mut store = crate::store::CommandStore::new(&cx, &lent, false, 0, None, clock, &err, erased.handle())
            .expect("command store");
        let tools = Tools([Tool(ContentId([0x44; 32])); blaze_drive::key::KINDS]);

        let build_pinned = |store: &mut crate::store::CommandStore<'_, '_, _, _>,
                            pm_name: Name,
                            rec_key: Option<Key>| {
            let pm_src = ContentId([0x55; 32]);
            let mut kept = pages::Kept::new_in(alloc);
            kept.answer.serial = Gen(1);
            kept.answer.by_file.entry(pm_name).or_insert_with(|| blaze_own::Vec::new_in(alloc)).push(pm_name);
            kept.answer.rows.insert(
                pm_name,
                Row {
                    key: tools.key(pm_name, &pm_src),
                    phase: Phase::new(Stage::Scripts, 0),
                    file: pm_name,
                    source: pm_src,
                    out: ContentId([0x88; 32]),
                    serial: Gen(1),
                    defines: blaze_own::Vec::new_in(alloc),
                    reads: blaze_own::Vec::new_in(alloc),
                    by_cell: blaze_own::Vec::new_in(alloc),
                    findings: 0,
                    inline: false,
                    failed: false,
                },
            );
            kept.causes.insert(pm_name, (Gen(1), Cause::New));
            if let Some(k) = rec_key {
                kept.records.insert(pm_name, k);
            }
            let enc = pages::encode_at_bits(&kept, 4, alloc);
            for (k, b) in enc.puts.iter() {
                crate::store::Store::put(store, &cx, *k, b).expect("put page");
            }
            let root = crate::engine_answer::open_root(&cx, store, enc.root, alloc)
                .expect("open root io")
                .expect("valid root");
            (enc.root.0, root)
        };

        let pm_name = Name::new(Kind::MacroModule, crate::macros::subject_of("sqlx-macros", "0.9.0"));

        // 1. Missing CAS record -> MUST return Err(StartPage) instead of silently Ok(None).
        let missing_key = Key(ContentId([0xde; 32]));
        let (cert_missing, root_missing) = build_pinned(&mut store, pm_name, Some(missing_key));
        let snap1 = store.snapshot_in(alloc).expect("snap1");
        let headers = pages::Headers::Absent { since: Gen(0) };
        let stored1 = crate::engine_answer::Stored::new(&root_missing, &snap1, &headers, pages::Check::Store, alloc);
        let res_missing = crate::engine_check::seed_pinned_dep_probes(
            &cx,
            &mut store,
            &stored1,
            &root_missing,
            cert_missing,
            "/missing-pinned",
            crate::engine_check::CheckScope::Whole,
            &SharedCommandProbes::new(),
            frame,
        );
        assert!(res_missing.is_err(), "missing pinned MacroModule record must return Err instead of falling back to live probes");

        // 2. Corrupt/malformed CAS record bytes -> MUST return Err(StartPage).
        let corrupt_bytes = b"not a valid blaze_drive record";
        let corrupt_key = blaze_drive::key::content_key(corrupt_bytes);
        crate::store::Store::put(&mut store, &cx, corrupt_key, corrupt_bytes).expect("put corrupt");
        let (cert_corrupt, root_corrupt) = build_pinned(&mut store, pm_name, Some(corrupt_key));
        let snap2 = store.snapshot_in(alloc).expect("snap2");
        let stored2 = crate::engine_answer::Stored::new(&root_corrupt, &snap2, &headers, pages::Check::Store, alloc);
        let res_corrupt = crate::engine_check::seed_pinned_dep_probes(
            &cx,
            &mut store,
            &stored2,
            &root_corrupt,
            cert_corrupt,
            "/corrupt-pinned",
            crate::engine_check::CheckScope::Whole,
            &SharedCommandProbes::new(),
            frame,
        );
        assert!(res_corrupt.is_err(), "malformed pinned MacroModule record must return Err");

        // 3. Non-empty malformed probe payload -> MUST return Err(StartPage).
        let bad_payload_bytes = blaze_drive::record::write_plain(
            pm_name,
            ContentId([0x55; 32]),
            ContentId([0x88; 32]),
            &[],
            b"not-64-hex-bytes",
            erased.handle(),
        );
        let bad_payload_key = blaze_drive::key::content_key(&bad_payload_bytes);
        crate::store::Store::put(&mut store, &cx, bad_payload_key, &bad_payload_bytes).expect("put bad payload");
        let (cert_bad, root_bad) = build_pinned(&mut store, pm_name, Some(bad_payload_key));
        let snap3 = store.snapshot_in(alloc).expect("snap3");
        let stored3 = crate::engine_answer::Stored::new(&root_bad, &snap3, &headers, pages::Check::Store, alloc);
        let res_bad = crate::engine_check::seed_pinned_dep_probes(
            &cx,
            &mut store,
            &stored3,
            &root_bad,
            cert_bad,
            "/bad-payload-pinned",
            crate::engine_check::CheckScope::Whole,
            &SharedCommandProbes::new(),
            frame,
        );
        assert!(res_bad.is_err(), "non-empty malformed probe payload must return Err");

        // 4. Legitimate empty payload (probe-free MacroModule) -> Ok(None).
        let empty_payload_bytes = blaze_drive::record::write_plain(
            pm_name,
            ContentId([0x55; 32]),
            ContentId([0x88; 32]),
            &[],
            &[],
            erased.handle(),
        );
        let empty_payload_key = blaze_drive::key::content_key(&empty_payload_bytes);
        crate::store::Store::put(&mut store, &cx, empty_payload_key, &empty_payload_bytes).expect("put empty payload");
        let (cert_empty, root_empty) = build_pinned(&mut store, pm_name, Some(empty_payload_key));
        let snap4 = store.snapshot_in(alloc).expect("snap4");
        let stored4 = crate::engine_answer::Stored::new(&root_empty, &snap4, &headers, pages::Check::Store, alloc);
        let res_empty = crate::engine_check::seed_pinned_dep_probes(
            &cx,
            &mut store,
            &stored4,
            &root_empty,
            cert_empty,
            "/empty-payload-pinned",
            crate::engine_check::CheckScope::Whole,
            &SharedCommandProbes::new(),
            frame,
        )
        .expect("empty payload on non-probe MacroModule is valid");
        assert_eq!(res_empty, None);
    }

    #[test]
    fn script_job_binds_deps_digest_to_planned_module_probe_tag_under_live_drift() {
        use blaze_build_script::c_tasks_probe::{CanonicalProbe, ProbeAnswer, ProbeAnswerer};
        use blaze_ids::ContentId;
        use blaze_proc_macro::probe_clang::{CProbeStore, CScriptProbes, MemCProbeStore};

        struct MutableAnswerer(blaze_own::Arc<core::sync::atomic::AtomicU8, SharedAlloc<'static>>);
        impl ProbeAnswerer for MutableAnswerer {
            fn id<'a>(&self, _alloc: blaze_own::Alloc<'a>) -> Result<ContentId, blaze_own::AString<'a>> {
                Ok(ContentId([self.0.load(Ordering::SeqCst); 32]))
            }
            fn answer<'a, 'b>(
                &self,
                _probe: &CanonicalProbe<'_, '_>,
                _input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
                alloc: blaze_own::Alloc<'a>,
            ) -> Result<ProbeAnswer<'a>, blaze_own::AString<'a>> {
                Err(blaze_own::AString::from_str_in("unused", alloc))
            }
        }

        let io = Io::simulated(SimWorld::new(80, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root sim cx");
        let env = sim_env(&io);
        let shared = env.heap.shared();
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);
        let a = erased.handle();

        // Build a minimal 3-unit graph:
        // 0: dep_bs (BuildScript), 1: dep_lib (Lib with build_script=0), 2: app_bs (BuildScript depending on dep_lib).
        let make_u = |pkg: u32, name: &str, kind: blaze_unit_graph::UnitKind<'static>, bs: Option<u32>, dep: Option<(&str, u32)>| {
            let mut deps = blaze_own::Vec::new_in(a);
            if let Some((dn, du)) = dep {
                deps.push(blaze_unit_graph::UnitDep {
                    name: blaze_own::String::from_str_in(dn, a),
                    unit: blaze_unit_graph::UnitId(du),
                });
            }
            blaze_unit_graph::Unit {
                package: blaze_manifest::lock::LockId(pkg),
                name: blaze_own::String::from_str_in(name, a),
                version: blaze_own::String::from_str_in("0.1.0", a),
                kind,
                for_host: true,
                feature_side: blaze_unit_graph::HostFeatureSide::Target,
                edition: edition::PackageEdition::of_manifest("2024").expect("edition"),
                crate_name: blaze_own::String::from_str_in(name, a),
                root: blaze_own::String::from_str_in("build.rs", a),
                features: blaze_own::Vec::new_in(a),
                cfg: blaze_own::Vec::new_in(a),
                deps,
                build_script: bs.map(blaze_unit_graph::UnitId),
                links: None,
            }
        };
        let mut units = blaze_own::Vec::new_in(a);
        units.push(make_u(0, "dep_bs", blaze_unit_graph::UnitKind::BuildScript, None, None));
        units.push(make_u(0, "dep_lib", blaze_unit_graph::UnitKind::Lib, Some(0), None));
        units.push(make_u(1, "app_bs", blaze_unit_graph::UnitKind::BuildScript, None, Some(("dep_lib", 1))));
        let graph = blaze_unit_graph::UnitGraph {
            resolver: blaze_unit_graph::Resolver::V3,
            units,
            meta: blaze_own::Vec::new_in(a),
            left_out: blaze_own::Vec::new_in(a),
        };

        let state = blaze_own::Arc::new_in(core::sync::atomic::AtomicU8::new(0xaa), shared);
        let holder = SharedCommandProbes::new();
        let probes = holder.with_env(&env);
        let _ = probes
            .c_probes_with(&cx, |_| {
                let answerer: blaze_own::Arc<dyn ProbeAnswerer + Send + Sync, SharedAlloc<'static>> =
                    blaze_own::Arc::new_in(MutableAnswerer(blaze_own::Share::share(&state)), shared);
                let records: blaze_own::Arc<dyn CProbeStore, SharedAlloc<'static>> =
                    blaze_own::Arc::new_in(MemCProbeStore::default(), shared);
                Some(CProbeSetting::Ready(CScriptProbes {
                    answerer,
                    #[cfg(target_os = "linux")]
                    hermetic: None,
                    records,
                    sdk_dir: blaze_own::Arc::from_str_in("", shared),
                    sdk_id: blaze_own::Arc::from_str_in("", shared),
                    sdk_digest: [0; 32],
                    clang_sha256: [0x99; 32],
                }))
            })
            .expect("seed c_probes");
        let _ = probes
            .spawn_setting_with(&cx, |_| {
                Ok(SpawnSetting::facts_only(FactPins {
                    clang_version_text: "clang version 23.0.0".into(),
                    macos_product_version: Default::default(),
                    macos_build_version: Default::default(),
                }))
            })
            .expect("seed spawn");

        // Wave-0 planning froze probe tag A for `mod_subj`.
        let hex_a = probes.dep_probe_identity_hex(&cx).expect("hex A");
        let mod_subj = ContentId([0x77; 32]);
        let planned = [(mod_subj, crate::engine_scripts::PlannedProbe::Digest(hex_a))];

        // Before wave-1 `scripts::job` runs, live answerer drifts to B!
        state.store(0xbb, Ordering::SeqCst);
        let hex_b = probes.dep_probe_identity_hex(&cx).expect("hex B");
        assert_ne!(hex_a, hex_b);

        // `fold_wasm_script_dep_probes` MUST bind strictly to planned tag A (never live B!).
        let got = super::fold_wasm_script_dep_probes(
            &cx,
            super::RanAs::Wasm,
            &graph,
            Some(&graph),
            blaze_unit_graph::UnitId(2),
            mod_subj,
            Some(&planned),
            Some("base-deps"),
            probes,
            frame,
        )
        .expect("planned tag must bind cleanly");
        let expected_a = crate::wasm64_compile::fold_dep_probes_opt(Some("base-deps"), hex_a.as_str(), frame);
        let drifted_b = crate::wasm64_compile::fold_dep_probes_opt(Some("base-deps"), hex_b.as_str(), frame);
        assert_eq!(got, Some(expected_a), "Script job deps_digest must stay bound to planned module tag A");
        assert_ne!(got, Some(drifted_b));

        // Contrast: if `planned` does NOT include `mod_subj` for a dep-script-using unit, refuse PlannedDrift.
        let drift_err = super::fold_wasm_script_dep_probes(
            &cx,
            super::RanAs::Wasm,
            &graph,
            Some(&graph),
            blaze_unit_graph::UnitId(2),
            mod_subj,
            Some(&[]),
            Some("base-deps"),
            probes,
            frame,
        )
        .expect_err("unplanned dep-script unit must refuse PlannedDrift");
        assert_eq!(drift_err.kind, ProbeErrorKind::PlannedDrift);
    }

    #[test]
    fn whole_certificate_fallback_resets_and_publishes_whole_probe_snapshot() {
        let io = Io::simulated(SimWorld::new(81, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root sim cx");
        let env = sim_env(&io);

        let hex_scoped_stale = crate::wasm64_compile::Hex64::from_bytes([0xa1; 32]);
        let hex_whole_fresh = crate::wasm64_compile::Hex64::from_bytes([0xb2; 32]);
        let holder = SharedCommandProbes::new();
        let cert_cell: core::cell::Cell<Option<crate::wasm64_compile::Hex64>> = core::cell::Cell::new(None);

        // 1. Stale scoped sweep observed `hex_scoped_stale`, then missed.
        cert_cell.set(Some(hex_scoped_stale));
        // 2. Own-whole fallback resets `cert_cell` before its sweep and validates `hex_whole_fresh`.
        cert_cell.set(None);
        cert_cell.set(Some(hex_whole_fresh));
        crate::engine_check::apply_cert_dep_probe_snapshot(&holder, cert_cell.get());
        let start_hex = holder
            .with_env(&env)
            .with_checked_snapshot()
            .dep_probe_identity_hex(&cx)
            .expect("whole fallback snapshot");
        assert_eq!(start_hex, hex_whole_fresh, "own-whole certificate fallback must publish whole cert tag");

        // 3. If the own-whole certificate has NO `DepProbes` leaf, reset + apply clears any prior tag.
        cert_cell.set(None);
        crate::engine_check::apply_cert_dep_probe_snapshot(&holder, cert_cell.get());
        assert_eq!(holder.checked_dep_probe_hex.load(), None);
    }
}
