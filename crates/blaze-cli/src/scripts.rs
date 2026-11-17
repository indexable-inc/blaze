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
//!   tree at `/deps/<links>`; a machine path is refused by name. A script that read a checkout
//!   file (a member's own, or a member producer's tree it was served) is a hole, never stored
//!   (the drive has no name for a file's content yet: `key::content_of`, DRIVER runner 17's,
//!   see [`run`]).
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

use blaze_build_script::env::DepMetadata;
use blaze_build_script::key::CMode;
use blaze_build_script::native::Runner;
use blaze_build_script::outputs::{OutputsRef, StoredBytes, StoredError, StoredRef, StoredWhy};
use blaze_build_script::probe::Row;
use blaze_build_script::toolchain::Mode;
use blaze_drive::{Input, Kind, Name, Task, Want};
use blaze_ids::ContentId;
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{AVec, Alloc, Erased};
use blaze_proc_macro::native_unit::{NativeSetting, placed};
use blaze_proc_macro::script::ScriptRuntime;
use blaze_proc_macro::script_sandbox::dep_mount::DepProducer;
use blaze_proc_macro::script_sandbox::{ScriptRoot, Tree, dir_tree, mem_tree};
use blaze_proc_macro::script_unit::{self, Setting};
use blaze_proc_macro::spawn_route::SpawnSetting;
use blaze_unit_graph::record::Decoded;
use blaze_unit_graph::{CfgAtom, Records, StoreSources, Unit, UnitGraph, UnitId, UnitKind};
use blaze_world::Side;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;

use crate::CliError;
use crate::fetch::FetchEnv;
use crate::store::Store;
use crate::work::Record;

const SUBJECT_DOMAIN: &str = "blaze-cli script unit 1";

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

/// How the run of build-script unit `script` is placed: D112's one choice point, made before
/// anything runs; the same function the job asks (`job`), so the key and the run agree.
pub(crate) fn placement(graph: &UnitGraph<'_>, script: UnitId, alloc: Alloc<'_>) -> RanAs {
    RanAs::of(&placed(graph, script, MODE, alloc))
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

/// A record's stored files as this command reads them (`blaze_build_script::outputs`'s
/// `StoredBytes`): each mapped read-only from the home's file area
/// (`<home>/.blaze/v2/build-script/files/<sha256 hex>`) the first time a reader asks it, and
/// never before. Opening a record maps nothing, so `check` of a crate that includes a native
/// script's `bindings.rs` maps that one file and never the 810 MB archive beside it; a link
/// maps the archive. A mapping reads no page until its bytes are read.
pub(crate) struct FileArea {
    /// `None`: the command has no home, so no stored file can be read (each ask is refused).
    folder: Option<PathBuf>,
    /// One slot per stored file of the record, by digest, sorted and each once.
    #[expect(clippy::disallowed_types, reason = "a slot of one record's file, mapped on its first ask and read by every reader of the record after: per value, not global state")]
    files: std::vec::Vec<([u8; 32], std::sync::OnceLock<Result<blaze_core::Mapping, StoredWhy>>), Global>,
}

impl FileArea {
    /// The area of `outputs`' stored files under `home` (`HOME`, read at the edge): nothing
    /// mapped yet.
    pub(crate) fn of(home: Option<&str>, outputs: &OutputsRef<'_>) -> FileArea {
        let folder = home.map(|home| Path::new(home).join(STORE_UNDER_HOME).join(blaze_build_script::outputs::FILE_AREA));
        #[expect(clippy::disallowed_types, reason = "see the field")]
        let mut files: std::vec::Vec<([u8; 32], std::sync::OnceLock<Result<blaze_core::Mapping, StoredWhy>>), Global> = outputs.stored().map(|file| (*file.sha256, std::sync::OnceLock::new())).collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files.dedup_by(|a, b| a.0 == b.0);
        FileArea { folder, files }
    }
}

impl StoredBytes for FileArea {
    fn bytes(&self, file: &StoredRef<'_>) -> Result<&[u8], StoredError> {
        let refused = |why: StoredWhy| StoredError { sha256: *file.sha256, why };
        let folder = self.folder.as_ref().ok_or(refused(StoredWhy::NoArea))?;
        let at = self.files.binary_search_by(|(digest, _)| digest.cmp(file.sha256)).map_err(|_| refused(StoredWhy::NotThere))?;
        let mapped = self.files[at].1.get_or_init(|| {
            let name: std::string::String = file.sha256.iter().map(|byte| std::format!("{byte:02x}")).collect();
            match blaze_core::Mapping::map_file(&folder.join(name)) {
                Ok(mapping) => Ok(mapping),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(StoredWhy::NotThere),
                Err(_) => Err(StoredWhy::Unreadable),
            }
        });
        mapped.as_ref().map(blaze_core::Mapping::bytes).map_err(|why| refused(*why))
    }
}

/// The build-script module store's folder under blaze's folder in the home (`<home>/.blaze`):
/// `build-script/modules/<key>/{identity,module.wasm,wasm-sha256}`, filled by the bootstrap
/// builder (`toolrun.sh proc_macro_modules`, build-script mode). Its listing is a
/// `HomeListing` row read with the layout (main.rs), so it is in the units' source, the
/// graph's source and every `Script` part's: a module that comes or goes asks again.
pub(crate) const MODULE_LISTING: &str = "v2/build-script/modules";

/// The store `script_unit::Setting::store` names: `<home>/.blaze/v2` (its `build-script/`).
const STORE_UNDER_HOME: &str = ".blaze/v2";

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

/// The identity of the command's macOS SDK (blaze-apple-sdk `Sdk::identity`: its version, never
/// its folder), which a native run's key holds; `None` when the command has none, and then a
/// native run fails naming why ([`run`]).
pub(crate) fn sdk_identity(env: &FetchEnv<'_>) -> Option<blaze_apple_sdk::Version> {
    env.sdk.ok().map(|sdk| sdk.identity())
}

/// What a run's key holds of the macOS SDK (rule 21): a native run's compiles and links are
/// made against it, so its identity ([`sdk_identity`]; a marker for none); a wasm run reads no
/// SDK, so nothing, and its key moves with no SDK change.
fn sdk_bytes(ran_as: RanAs, sdk: Option<blaze_apple_sdk::Version>) -> ([u8; 5], usize) {
    match (ran_as, sdk) {
        (RanAs::Wasm, _) => ([0; 5], 0),
        (RanAs::Native, None) => ([0; 5], 1),
        (RanAs::Native, Some(version)) => {
            let [a, b, c, d] = version.key();
            ([1, a, b, c, d], 5)
        }
    }
}

/// The `Script` parts of a world whose crates read the runs `keys` (`Names::script_keys`: each
/// run's subject and its reading crate's world key, named with the crates' `CrateScope` parts),
/// for a command whose macOS SDK is `sdk` ([`sdk_identity`]): one a subject, sorted and without
/// repeats (two crates of one subject read one run, and have one crate key: ruling D115 (3)).
pub(crate) fn parts(keys: impl Iterator<Item = (ContentId, ContentId, RanAs)>, sdk: Option<blaze_apple_sdk::Version>) -> std::vec::Vec<(Name, ContentId), Global> {
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

/// The probe table of `side`, sorted by key:
/// - the pinned rows (`pinned::TEMPO`) whose `needs` the side's sysroot ships (`shipped`, the
///   world's toolchain crates of that side), each keyed for the side's triple
///   (`probe_ask::Compile::key` holds the target), so a try-compile is answered by a row made
///   for its target, never by another triple's;
/// - one ships fact per shipped crate (`probe_ask::ships_key`), which lets blaze decide a probe of
///   no items itself (`probe_ask::decide`).
///
/// A question neither answers is the runtime's refusal naming the triple
/// (`SpawnRefusal::CannotAnswerYet`).
pub(crate) fn pinned_table<'s>(side: Side, shipped: impl Iterator<Item = &'s str> + Clone, alloc: Alloc<'_>) -> std::vec::Vec<Row> {
    let triple = side_triple(side);
    let mut known = std::vec::Vec::new_in(Global);
    for row in blaze_build_script::pinned::TEMPO {
        if row.needs.iter().all(|needed| shipped.clone().any(|name| name == *needed)) {
            known.push(*row);
        }
    }
    let mut rows = blaze_build_script::probe::pinned_rows(triple, &known, alloc.clone()).to_vec();
    rows.extend_from_slice(&blaze_build_script::probe_ask::ships_rows(triple, shipped, alloc));
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    rows
}

/// The triple a side's scripts are run for: what its probes are keyed by.
fn side_triple(side: Side) -> &'static str {
    match side {
        Side::Target => crate::graph::TRIPLE,
        Side::Macro => MACRO_TARGET,
    }
}

/// The library-feature listings of `side` (`probe_ask::feature_rows`), unsorted: for each crate
/// a probe of no items loads (`core` under `#![no_std]`, else `std`), every toolchain crate of
/// the side it reaches by its dependencies (the crates rustc's `lib_features` reads), each read
/// under its own cfg and features by blaze-scope's `lib_features`. `core`'s closure is read
/// first and `std`'s listing goes on from it (std's closure holds core's). Read when a command
/// first runs a script, from the world's texts in place; the record it is to become is the
/// lane report's spec (keyed by the toolchain and the side's cfg, rule 77).
pub(crate) fn feature_table(world: &blaze_world::World<'_>, side: Side, alloc: Alloc<'_>) -> Result<std::vec::Vec<Row>, SetupError> {
    use blaze_build_script::probe_ask::{Needs, feature_rows};
    use blaze_scope::lib_features::{LibFeatures, LibFeaturesOpts};
    let triple = side_triple(side);
    let mut rows = std::vec::Vec::new_in(Global);
    let mut features = LibFeatures::new(LibFeaturesOpts { alloc: alloc.clone() });
    let mut read = std::vec::Vec::new_in(Global);
    read.resize(world.crates.len(), false);
    for needs in [Needs::Core, Needs::Std] {
        let toolchain = |krate: &blaze_world::Crate<'_>| matches!(krate.origin, blaze_world::Origin::Toolchain(_)) && krate.side == side;
        let Some(root) = world.crates.iter().position(|krate| toolchain(krate) && krate.input.name == needs.krate()) else { continue };
        let mut stack = std::vec::Vec::new_in(Global);
        stack.push(root);
        while let Some(at) = stack.pop() {
            if core::mem::replace(&mut read[at], true) {
                continue;
            }
            let krate = &world.crates[at];
            stack.extend(krate.input.deps.iter().map(|(_, dep)| *dep));
            // Only a crate of `#![feature(staged_api)]` may carry a stability attribute (rustc
            // refuses one elsewhere, E0734): the registry crates beneath std (libc, cfg-if,
            // hashbrown, ...) declare no feature and are not read.
            let staged = blaze_scope::resolve::SourceTexts::read(&world.texts, at, krate.input.root).is_some_and(|text| text.contains("staged_api"));
            if !staged {
                continue;
            }
            let mut atoms = std::vec::Vec::new_in(Global);
            atoms.extend_from_slice(krate.input.cfg);
            atoms.extend(krate.input.features.iter().map(|feature| ("feature", Some(*feature))));
            let cfg = blaze_scope::task::Cfg::from_atoms(&atoms, alloc.clone());
            if let Err(error) = features.scan(&world.texts, at, krate.input.root, &cfg) {
                return Err(SetupError::LibFeatures { triple, krate: krate.input.name.to_owned(), why: std::format!("{error}") });
            }
        }
        let listed = features.listed(alloc.clone());
        rows.extend_from_slice(&feature_rows(triple, needs, &listed.declared, &listed.mentioned, alloc.clone()));
    }
    Ok(rows)
}

/// The toolchain crates the world holds for `side` (its sysroot's), by library name.
pub(crate) fn shipped<'r, 'w: 'r>(world: &'r blaze_world::World<'w>, side: Side) -> impl Iterator<Item = &'w str> + Clone + 'r {
    world.crates.iter().filter(move |krate| matches!(krate.origin, blaze_world::Origin::Toolchain(_)) && krate.side == side).map(|krate| krate.input.name)
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
    runtime: ScriptRuntime,
    /// `<home>/.blaze/v2`.
    store: PathBuf,
    /// The checkout, as a member's folder is reached from where blaze was started.
    checkout: PathBuf,
    /// The probe rows of each side, keyed for its triple ([`pinned_table`]).
    table: Arc<std::vec::Vec<Row>>,
    macro_table: Arc<std::vec::Vec<Row>>,
    spawn: Arc<SpawnSetting>,
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
/// graph decoded once for making jobs, the fetched sources' records).
pub(crate) struct Scripts<'f> {
    shared: &'f Shared<'f>,
    decoded: &'f Decoded<'f>,
    /// (subject, script unit, side), sorted: each build-script unit's run on either side, so
    /// any part the world named is found (a side nobody reaches is never asked).
    by_subject: std::vec::Vec<(ContentId, UnitId, Side), Global>,
    sources: StoreSources<'f, 'f, 'f, SourcesDir<'f>>,
    lock: &'f Lock<'f>,
    /// Each member's lock entry and folder, checkout-relative, copied into the request's frame.
    members: &'f [(LockId, &'f str)],
    /// Each fetched package's files as first located ([`files_of`]), sorted by lock id: a
    /// package's manifests are laid out once a command, not once for each job that runs its
    /// script or is served by it as a `links` producer (rule 77).
    files: std::vec::Vec<(LockId, Files<'f>), Global>,
}

/// Why the command's scripts could not be set up: every `Script` task fails with these
/// words (a hole: a fact outside every key, asked again next command).
#[derive(Debug)]
pub(crate) enum SetupError {
    NoHome,
    NoLock,
    NoMembers,
    Graph(blaze_unit_graph::record::RecordRefused),
    Runtime(std::string::String),
    SpawnSetting(std::string::String),
    Sources(std::string::String),
    /// A toolchain crate's library features could not be listed (blaze-scope `lib_features`'s
    /// `ScanError`, rendered): the crate, and the triple whose cfg it was read under.
    LibFeatures { triple: &'static str, krate: std::string::String, why: std::string::String },
}

impl core::fmt::Display for SetupError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SetupError::NoHome => f.write_str("HOME is not set: the build-script module store and the native setting are under it"),
            SetupError::NoLock => f.write_str("no lock was read this command: a build script's package is pinned by it"),
            SetupError::NoMembers => f.write_str("the workspace's members were not matched to the lock this command: a member's script has no folder"),
            SetupError::Graph(refused) => write!(f, "the unit graph record at byte {}: {}", refused.at, refused.why),
            SetupError::Runtime(words) => write!(f, "the build-script runtime: {words}"),
            SetupError::SpawnSetting(words) => write!(f, "the command's spawn setting (the derived facts' pins): {words}"),
            SetupError::Sources(words) => write!(f, "the fetched sources: {words}"),
            SetupError::LibFeatures { triple, krate, why } => write!(f, "the toolchain crate `{krate}` for {triple}: {why}"),
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
        let mapping: &'f blaze_core::Mapping = blaze_own::Box::leak(blaze_own::Box::new_in(mapping, self.frame));
        Some(mapping.bytes())
    }
}

/// `bytes` copied into `frame`, held as long as it is.
fn held_bytes<'f>(bytes: &[u8], frame: &'f Frame) -> &'f [u8] {
    blaze_own::Vec::from_slice_in(bytes, frame).leak()
}

/// Words held in `frame`.
/// Why the fact shim is not in place for the command's native runs (D112, N7): each names the
/// path it looked at, or why the running blaze's folder is not known.
#[derive(Debug)]
enum FactShimError {
    ExeDir(crate::fetch::ExeDirWhy),
    Place(blaze_proc_macro::native_facts::NativeFactsError),
}

impl core::fmt::Display for FactShimError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FactShimError::ExeDir(why) => write!(f, "{why}"),
            FactShimError::Place(error) => write!(f, "{error}"),
        }
    }
}

fn said<'f>(frame: &'f Frame, words: core::fmt::Arguments<'_>) -> &'f str {
    let text = bstd::mem::format_in!(frame, "{words}");
    crate::work::held_str(text.as_str(), frame)
}

impl<'f> Scripts<'f> {
    /// The command's scripts, made once: the graph record `graph_record` (held in `frame`),
    /// the lock, the members' folders and what `main` read at the edge (`env`: the home, the
    /// SDK folder, the checkout). `world` names each side's sysroot ([`shipped`]). `workers` is
    /// the command's worker count, which its native scripts' children share (the jobserver).
    #[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
    pub(crate) fn open(
        cx: &Cx,
        env: &FetchEnv<'_>,
        workers: u32,
        world: &blaze_world::World<'_>,
        graph_record: &'f [u8],
        lock: Option<&'f Lock<'static>>,
        members: Option<&[(LockId, blaze_unit_graph::Text<'_>)]>,
        frame: &'f Frame,
    ) -> Result<Scripts<'f>, SetupError> {
        let home = env.home.ok_or(SetupError::NoHome)?;
        let lock: &'f Lock<'f> = lock.ok_or(SetupError::NoLock)?;
        let listed = members.ok_or(SetupError::NoMembers)?;
        let mut held = blaze_own::Vec::with_capacity_in(listed.len(), frame);
        for (id, dir) in listed.iter() {
            held.push((*id, crate::work::held_str(dir.as_str(), frame)));
        }
        let members: &'f [(LockId, &'f str)] = held.leak();
        // The process's heap, handed from `main` (`FetchEnv::heap`), never the request's frame:
        // the decoded graph and the setting live to the command's end, and the sources' lookup
        // lays out a package's manifests on each call and frees them when it returns
        // (blaze-unit-graph `StoreSources::locate`). In the request's bump frame, which never
        // frees, those layouts grew with every job until the frame was full (tempo, 1,072
        // crates: 49 MB at the 59th `Script` job, a panic in blaze-toml's parse).
        let alloc: Alloc<'f> = env.heap.alloc();
        let decoded = blaze_unit_graph::record::decode(graph_record, alloc).map_err(SetupError::Graph)?;
        let decoded: &'f Decoded<'f> = blaze_own::Box::leak(blaze_own::Box::new_in(decoded, frame));
        let mut by_subject = std::vec::Vec::new_in(Global);
        for at in 0..decoded.graph.units.len() {
            let script = UnitId(at as u32);
            for side in [Side::Target, Side::Macro] {
                if let Some(subject) = subject(&decoded.graph, script, side, alloc.clone()) {
                    by_subject.push((subject, script, side));
                }
            }
        }
        by_subject.sort_unstable_by_key(|(subject, script, _)| (*subject, *script));
        by_subject.dedup_by_key(|(subject, _, _)| *subject);

        let mut store = PathBuf::from(home);
        store.push(STORE_UNDER_HOME);
        let runtime = ScriptRuntime::new(&store).map_err(|error| SetupError::Runtime(std::format!("{error}")))?;
        let host = crate::graph::TRIPLE;
        let table = {
            let _cx = cx.span("scripts: the target side's probe answers (library features listed)");
            let mut rows = pinned_table(Side::Target, shipped(world, Side::Target), alloc.clone());
            rows.extend_from_slice(&feature_table(world, Side::Target, alloc.clone())?);
            rows.sort_by(|a, b| a.key.cmp(&b.key));
            Arc::new(rows)
        };
        let macro_table = {
            let _cx = cx.span("scripts: the macro side's probe answers (library features listed)");
            let mut rows = pinned_table(Side::Macro, shipped(world, Side::Macro), alloc.clone());
            rows.extend_from_slice(&feature_table(world, Side::Macro, alloc.clone())?);
            rows.sort_by(|a, b| a.key.cmp(&b.key));
            Arc::new(rows)
        };
        let spawn = blaze_proc_macro::fact_pins::command_spawn_setting(host, alloc).map_err(|error| SetupError::SpawnSetting(std::format!("{error}")))?;
        // The native setting: only a script `placed` native needs it; without it that script
        // fails naming why ([`pooled`]), never run as wasm (D112).
        // The SDK was resolved once at the command's edge (sdk.rs); its refusal names what was
        // tried (`SDKROOT`'s value, or xcrun's answer) and how to set `SDKROOT`.
        let workers = core::num::NonZeroU32::new(workers);
        let native = match env.sdk {
            Err(refused) => Err(said(frame, format_args!("{refused}"))),
            Ok(sdk) => match blaze_proc_macro::native_setting::command_native_setting(home, sdk.folder(), NATIVE_WORK, host, alloc) {
                Ok(owned) => {
                    // D112 (N7): the fact shim, beside the running blaze (its folder read once
                    // at the edge, main.rs), copied into the work folder once here, where each
                    // native run finds it. Either failure is typed and names the path.
                    let placed = match env.exe_dir {
                        Err(why) => Err(FactShimError::ExeDir(why)),
                        Ok(dir) => {
                            let from = std::format!("{dir}/{}", blaze_build_script::fact_shim::SHIM_NAME);
                            blaze_proc_macro::native_facts::place_shim(owned.work.as_str(), &from).map_err(FactShimError::Place)
                        }
                    };
                    // The command's jobserver (jobserver.rs): its fifo under the work folder,
                    // `workers - 1` tokens; the native runs' children share the command's
                    // workers through it. Ended, its fifo removed, when `Scripts` drops.
                    let jobs = match workers {
                        None => Err(said(frame, format_args!("the command's worker count is 0: the native runner's jobserver needs at least one (scripts.rs)"))),
                        Some(workers) => blaze_proc_macro::jobserver::Jobserver::new(blaze_proc_macro::jobserver::JobserverOpts { work: owned.work.as_str(), workers, id: std::process::id() })
                            .map_err(|error| said(frame, format_args!("the native runner's jobserver (D112): {error}"))),
                    };
                    match (placed, jobs) {
                        (Ok(_), Ok(jobs)) => {
                            let held = |text: &str| crate::work::held_str(text, frame);
                            let jobs: &'f blaze_proc_macro::jobserver::Jobserver = blaze_own::Box::leak(blaze_own::Box::new_in(jobs, frame));
                            Ok(NativeSetting { shelf: held(owned.shelf.as_str()), sdk: held(owned.sdk.as_str()), work: held(owned.work.as_str()), deadline: owned.deadline, jobs })
                        }
                        (Err(error), jobs) => {
                            if let Ok(jobs) = jobs {
                                end_jobs(&jobs);
                            }
                            Err(said(frame, format_args!("the native runner's fact shim (D112, N7): {error}")))
                        }
                        (Ok(_), Err(why)) => Err(why),
                    }
                }
                Err(error) => Err(said(frame, format_args!("the native runner's setting (D112): {error}"))),
            },
        };
        let checkout = PathBuf::from(if env.checkout.is_empty() { "." } else { env.checkout });
        let no_sdk = env.sdk.is_err();
        let shared: &'f Shared<'f> = blaze_own::Box::leak(blaze_own::Box::new_in(Shared { runtime, store, checkout, table, macro_table, spawn: Arc::new(spawn), native, no_sdk, graph_record }, frame));

        let _cx = cx.span("scripts: the fetched sources");
        let folder = crate::fetch::sources_native(env, frame).ok_or(SetupError::NoHome)?;
        let dir = SourcesDir { folder: crate::work::held_str(folder.as_str(), frame), frame };
        let sources = StoreSources::new(lock, dir, blaze_fetch::pack::PackView::open_source_put, alloc).map_err(|error| SetupError::Sources(std::format!("{error}")))?;
        Ok(Scripts { shared, decoded, by_subject, sources, lock, members, files: std::vec::Vec::new_in(Global) })
    }

    fn unit_of(&self, name: Name) -> Option<(UnitId, Side)> {
        self.by_subject.binary_search_by_key(&name.subject, |(subject, _, _)| *subject).ok().map(|at| (self.by_subject[at].1, self.by_subject[at].2))
    }
}

impl Drop for Scripts<'_> {
    /// The command is over (its scripts' runs all ended): the jobserver's fifo is removed.
    fn drop(&mut self) {
        if let Ok(native) = &self.shared.native {
            end_jobs(native.jobs);
        }
    }
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

/// The scripts whose runs `script` is served of: each of its `links_inputs` and, walked from
/// those, each of theirs (the transitive closure brief S1b serves a forwarded path from), with
/// the `links` each serves and whether it is a direct input. A work list, no recursion; sorted
/// by unit, each once (a script reached directly and upstream is direct).
fn links_closure<'g>(graph: &'g UnitGraph<'_>, script: UnitId) -> std::vec::Vec<(UnitId, &'g str, bool), Global> {
    let mut found: std::vec::Vec<(UnitId, &'g str, bool), Global> = std::vec::Vec::new();
    let mut stack: std::vec::Vec<(UnitId, &'g str, bool), Global> = std::vec::Vec::new();
    stack.extend(script_unit::links_inputs(graph, script).into_iter().map(|(unit, links)| (unit, links, true)));
    while let Some((unit, links, direct)) = stack.pop() {
        match found.iter_mut().find(|(seen, _, _)| *seen == unit) {
            Some(seen) => seen.2 |= direct,
            None => {
                found.push((unit, links, direct));
                stack.extend(script_unit::links_inputs(graph, unit).into_iter().map(|(up, links)| (up, links, false)));
            }
        }
    }
    found.sort_unstable_by_key(|(unit, _, _)| *unit);
    found
}

/// Where the package of `unit` has its files: a member's folder (checkout-relative), else its
/// fetched record, located once a command and kept in `found` (its folder in `frame`). `Err`:
/// the job's refusal, naming the package (a manifest of it refused, the manifest too).
fn files_of<'f: 'j, 'j>(
    sources: &mut StoreSources<'f, 'f, 'f, SourcesDir<'f>>,
    found: &mut std::vec::Vec<(LockId, Files<'f>), Global>,
    members: &'f [(LockId, &'f str)],
    lock: &'f Lock<'f>,
    unit: &Unit<'_>,
    frame: &'f Frame,
    jobs: &'j Frame,
) -> Result<Files<'j>, Prepared<'j>> {
    if let Some((_, dir)) = members.iter().find(|(id, _)| *id == unit.package) {
        return Ok(Files::Member { dir });
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

/// The tree `files` serves a script of `package`, and the root it sees it at. `Err`: the
/// run's failure, naming the package and the folder.
fn tree_of(files: &Files<'_>, checkout: &std::path::Path, package: &str) -> Result<(Tree, ScriptRoot), Ran> {
    match files {
        Files::Member { dir } => match ScriptRoot::member(dir) {
            Ok(root) => Ok((dir_tree(checkout), root)),
            Err(error) => Err(failed("a member folder a script cannot see", format_args!("the build script of {package}: its member folder `{dir}`: {error}"), false)),
        },
        Files::Pack { view, dir, git } => Ok((mem_tree(view.files().map(|f| (f.path, f.bytes)), dir, *git), ScriptRoot::package())),
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

/// The `Script` job of `task`, made on the main thread from record bytes (as a crate scope's
/// is, names/scope.rs): the command's scripts opened at the first task, the unit found by its
/// subject, its `links` inputs' records read into `jobs`, its files located.
#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
pub(crate) fn job<'f: 'j, 'j>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store<'_>,
    scripts: &mut Option<Result<Scripts<'f>, &'f str>>,
    names: &mut Option<crate::names::Names<'f>>,
    env: &FetchEnv<'_>,
    workers: u32,
    lock: Option<&'f Lock<'static>>,
    members: Option<&[(LockId, blaze_unit_graph::Text<'_>)]>,
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
        // The world (the crate list's rows, both sides): made once a command, here when no
        // `CrateScope` task made it first, exactly as names/scope.rs `prepare` does.
        if names.is_none() {
            match crate::names::world(&cx, root, read.unit_graph(graph.name)?.graph, lock, members, env) {
                Ok(made) => {
                    crate::names::tell_public(store, &made);
                    *names = Some(made);
                }
                Err(error) => return Ok(Prepared::Said { what: "the crates' inputs could not be made", words: said(jobs, format_args!("names not resolved: {error}")), hole: false }),
            }
        }
        if scripts.is_none() {
            let bytes: &'f [u8] = held_bytes(read.unit_graph(graph.name)?.graph, frame);
            let world = names.as_ref().expect("made above").world();
            match Scripts::open(&cx, env, workers, world, bytes, lock, members, frame) {
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
    // The crate of the list that reads this run: the world row whose script is this subject,
    // the row its part was named from (ruling D115 (2)). Its cfg is the run's side's.
    let Some(world) = names.as_ref().map(|names| names.world()) else { return Err(CliError::Text { what: "the names world, made above, is gone" }) };
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
    let mut cfg: blaze_own::Vec<(&'j str, Option<&'j str>), &'j Frame> = blaze_own::Vec::with_capacity_in(row.input.cfg.len(), jobs);
    for (name, value) in row.input.cfg.iter() {
        cfg.push((crate::work::held_str(name, jobs), value.map(|value| crate::work::held_str(value, jobs))));
    }
    let cfg: &'j [(&'j str, Option<&'j str>)] = cfg.leak();
    // Copied out of `scripts`, which the sources' lookup below borrows mutably.
    let (decoded, shared, members, lock) = (scripts.decoded, scripts.shared, scripts.members, scripts.lock);
    let graph = &decoded.graph;
    let unit = &graph.units[script.index()];
    // The links closure: each producing script's record (direct and upstream), asked for until
    // handed, and where its package's files are (a path in its metadata may name them).
    let closure = links_closure(graph, script);
    let mut wants = std::vec::Vec::new_in(Global);
    // Its module, looked up by its own task (script_modules.rs, ruling D115 (c)): read, so a
    // rebuilt module reruns this run alone and an equal one moves nothing.
    let at = world.crates.iter().position(|krate| krate.script == wanted).ok_or(CliError::Text { what: "the world row found above is gone" })?;
    let Some(module) = names.as_ref().and_then(|names| names.script_module_of(at)) else {
        return Ok(Prepared::Said { what: "a build script with no module lookup", words: "the build script read here has no unit in the graph to name its module by", hole: false });
    };
    let module = crate::script_modules::name(module.subject);
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
                let read = crate::work::payload(&cx, store, handed, jobs)?;
                let files = match files_of(&mut scripts.sources, &mut scripts.files, members, lock, &graph.units[input.index()], frame, jobs) {
                    Ok(files) => files,
                    Err(refused) => return Ok(refused),
                };
                producers.push(Producer { script: *input, links: crate::work::held_str(links, jobs), name: dep, out: handed.out, bytes: held_bytes(read.payload(), jobs), files, direct: *direct });
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
    Ok(Prepared::Ready(ScriptJob { name: task.name, shared, script, side, cfg, files, producers: producers.leak() }))
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
}

/// `job` on a pool worker, in the worker's task frame: a panic or a full frame fails this
/// script alone (rule 20).
pub(crate) fn pooled(cx: &Cx, job: &ScriptJob<'_>, clock: &bstd::clock::Clock) -> PooledScript {
    let began = clock.now();
    let scratch = cx.frame();
    let ran = crate::work::unwinding(|| run(cx, job, scratch));
    let (used, limit) = (scratch.peak(), scratch.reserved());
    let did = ran.map_err(|crate::work::Panicked| blaze_drive::Why::Panicked { used: used as u64, limit: limit as u64 });
    PooledScript { did, used, ns: clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)) }
}

fn failed(what: &'static str, words: core::fmt::Arguments<'_>, hole: bool) -> Ran {
    let mut held = blaze_own::Vec::new_in(Global);
    let text = std::fmt::format(words);
    held.extend_from_slice(text.as_bytes());
    Ran::Failed { what, words: held, hole }
}

/// The run on the worker.
fn run(cx: &Cx, job: &ScriptJob<'_>, scratch: &Frame) -> Ran {
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
    // The `links` metadata it is given, from each direct producer's record (`DEP_<LINKS>_*`),
    // and what each producer of the closure serves (brief S1): built upstream first, so each
    // takes its own inputs' closures (`DepProducer::closure`, one flatten).
    let order = match script_unit::links_order(graph) {
        Ok(order) => order,
        Err(error) => return failed("a `links` cycle", format_args!("the build script of {package}: {error}"), false),
    };
    let mut deps: AVec<'_, DepMetadata<'_>> = AVec::new_in(alloc);
    let mut served: std::vec::Vec<(UnitId, DepProducer), Global> = std::vec::Vec::new();
    for producer in order.iter().filter_map(|unit| job.producers.iter().find(|p| p.script == *unit)) {
        let (links, dep) = (producer.links, producer.name);
        let opened = match payload_parts(producer.bytes) {
            Some((_, record)) => OutputsRef::open(record),
            None => Err(blaze_build_script::outputs::RecordError::Layout),
        };
        let outputs = match opened {
            Ok(outputs) => outputs,
            Err(why) => return failed("a `links` input's record is not an outputs record", format_args!("the build script of {package}: the record of {dep:?}, whose `links = \"{links}\"` metadata it is served, is not an outputs record: {why:?}"), false),
        };
        if producer.direct {
            deps.extend(outputs.metadata().map(|(key, value)| DepMetadata { links, key, value }));
        }
        let (tree, root) = match tree_of(&producer.files, &shared.checkout, &package) {
            Ok(served) => served,
            Err(ran) => return ran,
        };
        let inputs: std::vec::Vec<(&str, &DepProducer), Global> = script_unit::links_inputs(graph, producer.script)
            .into_iter()
            .filter_map(|(up, links)| served.iter().find(|(s, _)| *s == up).map(|(_, p)| (links, p)))
            .collect();
        let upstream = DepProducer::closure(&inputs);
        let mut out: std::vec::Vec<(std::string::String, std::vec::Vec<u8>)> = outputs.files().map(|(path, bytes)| (path.to_owned(), bytes.to_vec())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        // Its stored files (a native script's, in the file area) are named, never read into
        // `out`: a dependent served that `OUT_DIR` is refused by name (`DepViewError::StoredOut`).
        let stored: std::vec::Vec<std::string::String> = outputs.stored().map(|file| file.name.to_owned()).collect();
        let made = DepProducer { tree, root, out_dir: outputs.out_dir().to_owned(), out: Arc::new(out), stored: Arc::new(stored), upstream: Arc::new(std::vec::Vec::new()) }.with_upstream(upstream);
        served.push((producer.script, made));
    }
    let given: std::vec::Vec<(&str, &DepProducer), Global> = job
        .producers
        .iter()
        .filter(|p| p.direct)
        .filter_map(|p| served.iter().find(|(s, _)| *s == p.script).map(|(_, made)| (p.links, made)))
        .collect();
    let (tree, root) = match tree_of(&job.files, &shared.checkout, &package) {
        Ok(served) => served,
        Err(ran) => return ran,
    };
    // The one choice point (D112), before anything runs: a script placed native needs the
    // command's native setting, or fails naming why it has none.
    let runner = placed(graph, job.script, MODE, alloc);
    let ran_as = RanAs::of(&runner);
    let native = match (runner, &shared.native) {
        // No SDK: a function of the run's key, which holds "no SDK" ([`Shared::no_sdk`]), so
        // filed. An SDK and the setting still refused (its shelf, its fact shim): a fact of the
        // machine no key holds yet, so a hole, asked again next command.
        (Runner::Native { .. }, Err(why)) => return failed("a native build script with no native setting", format_args!("the build script of {package} runs native (D112): {why}"), !shared.no_sdk),
        (_, Ok(native)) => Some(*native),
        (Runner::Wasm, Err(_)) => None,
    };
    let setting = Setting {
        store: &shared.store,
        // Its crates' side's target (ruling D115 (2)); HOST and every other machine fact from
        // the pinned table, never the machine (rule 25).
        target: match job.side {
            Side::Target => crate::graph::TRIPLE,
            Side::Macro => MACRO_TARGET,
        },
        host: crate::graph::TRIPLE,
        table: Arc::clone(match job.side {
            Side::Target => &shared.table,
            Side::Macro => &shared.macro_table,
        }),
        c_mode: C_MODE,
        mode: MODE,
        native,
        spawn_setting: Arc::clone(&shared.spawn),
    };
    // A macro-side run is made with its row's cfg (wasm32-wasip1's atoms); a target-side run
    // with the graph's, which its row copies (and a native target-side script may run).
    let side_cfg = match job.side {
        Side::Target => None,
        Side::Macro => Some(side_atoms(job.cfg, alloc.clone())),
    };
    let ran = match script_unit::outputs_viewing_on(&shared.runtime, &setting, graph, job.script, side_cfg.as_ref().map(|atoms| atoms.as_slice()), tree, root, &deps, &given, alloc) {
        Ok(ran) => ran,
        Err(error) => return failed("a build script failed", format_args!("the build script of {package} failed: {error}"), false),
    };
    let outputs = &ran.outputs;
    if let Some(error) = outputs.errors.first() {
        return failed("a build script printed `cargo::error`", format_args!("the build script of {package} failed: cargo::error={}", error.as_str()), false);
    }
    let mut made = Record::new_in(job.name, Global);
    // A member's tree is the checkout: what it read there is a read of today's file. A fetched
    // package's tree is its record, pinned by the lock in the graph's source: its reads (files,
    // listings, git facts from the lock's commit) are functions of that pin.
    //
    // A member's read has no drive name yet: DRIVER runner 17 is adding `key::content_of(path)`
    // (a name whose value is the sweep's content id of a checkout file). Then each `file:<path>`
    // read becomes a read of that name, listed with the out the drive hands for it (the task
    // enters again with those names as inputs: the runtime's own read digest is another
    // domain and is never listed), a `git:` fact is covered by the `file:` reads it made
    // (script_sandbox.rs `GitView::read`), and a `dir:`/`stat:` read waits for a listing fact.
    // Until then such a run is a hole: its outputs serve nothing and it runs again next command.
    // A member producer's tree, mounted at `/deps/<links>`, is the checkout too: its reads are
    // among `ran.reads`, told apart from the package's own by nothing yet, so the run is a hole.
    let reads_checkout = matches!(job.files, Files::Member { .. }) || job.producers.iter().any(|p| matches!(p.files, Files::Member { .. }));
    if reads_checkout && let Some(read) = ran.reads.first() {
        return failed(
            "a member's build script read a checkout file the drive cannot name yet",
            format_args!("the build script of {package} read `{}` of the checkout: a read of a checkout file is not a drive read yet (`key::content_of`), so its outputs are not kept; it runs again next command", read.subject),
            true,
        );
    }
    // Every producer of the closure, by its out (dep_paths.rs: per producer mounted, its outputs
    // record's id; a fetched tree is under the graph's source, a member's is a hole above).
    for producer in job.producers.iter() {
        made.reads.push((producer.name, producer.out));
    }
    // How it ran, then the outputs record ([`payload_parts`]); the out is the outputs' alone, so
    // a reader moves only when what it reads moves. Encoded on the heap and handed whole to the
    // record, which outlives this worker's frame (rules 11 and 51): never encoded in the frame
    // and copied out.
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
