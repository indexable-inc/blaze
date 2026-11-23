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
use blaze_proc_macro::native_unit::{NativeSetting, placed};
use blaze_proc_macro::script::ScriptRuntime;
use blaze_proc_macro::script_sandbox::{ScriptRoot, Tree, dir_tree, mem_tree};
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

/// How the run of build-script unit `script` is placed: D112 / D175 zero-Wasm choice point,
/// made before anything runs; the same function the job asks (`job`), so the key and the run agree.
pub(crate) fn placement(home: Option<&str>, graph: &UnitGraph<'_>, script: UnitId, alloc: Alloc<'_>) -> RanAs {
    match home {
        Some(home) => {
            let store = Path::new(home).join(STORE_UNDER_HOME);
            RanAs::of(&blaze_proc_macro::native_unit::placed_in_store(&store, graph, script, MODE, crate::graph::TRIPLE, alloc))
        }
        None => RanAs::of(&placed(graph, script, MODE, alloc)),
    }
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
    /// The last record read: its bytes are lent for `'m` and answered from here.
    pub(crate) held: Option<crate::store::Held<'m>>,
}

impl blaze_world::lib_features_record::FeatureRecords for CommandFeatures<'_, '_, '_> {
    type Error = CliError;

    fn get(&mut self, key: ContentId) -> Result<Option<&[u8]>, CliError> {
        self.held = self.store.get(self.cx, blaze_drive::Key(key))?;
        if self.held.is_some() {
            self.store.keep_with_toolchain(blaze_drive::Key(key))?;
        }
        Ok(self.held.as_ref().map(|held| held.bytes))
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
        Self::finish_open(cx, env, names.graph(), None, by_subject, graph_record, lock, members, frame)
    }

    /// Lightweight scripts setup when no `Names` task has built the full world yet: decodes the
    /// unit graph and opens only the toolchain shelf (for `ScriptSetup::open_recorded`) and
    /// script-reading crates' `cfg`s, without mapping or laying out any fetched package.
    pub(crate) fn open_light(
        cx: &Cx,
        env: &FetchEnv<'_>,
        names: &crate::names::Names<'f>,
        graph_record: &'f [u8],
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
        Self::finish_open(cx, env, graph, Some(sw), by_subject, graph_record, lock, members, frame)
    }

    #[expect(clippy::too_many_arguments, reason = "shared Scripts constructor")]
    fn finish_open(
        cx: &Cx,
        env: &FetchEnv<'_>,
        graph: &'f UnitGraph<'f>,
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
        Ok(Scripts { shared: None, graph_record, graph, script_world, by_subject, sources, lock, members, files: std::vec::Vec::new_in(Global), setups_opened: 0 })
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
    fn shared(&mut self, cx: &Cx, env: &FetchEnv<'_>, store: &mut dyn Store<'_>, tool: blaze_drive::Tool, workers: u32, world: &blaze_world::World<'_>, frame: &'f Frame) -> Result<&'f Shared<'f>, &'f str> {
        if let Some(opened) = self.shared {
            return opened;
        }
        let _cx = cx.span("scripts: the shared setup opened (the runtime, each side's probe answers, the native runner)");
        self.setups_opened = self.setups_opened.saturating_add(1);
        let opened = Scripts::open_shared(cx, env, store, tool, workers, world, self.graph_record, frame).map_err(|error| said(frame, format_args!("{error}")));
        self.shared = Some(opened);
        opened
    }

    /// [`Scripts::shared`]'s one opening. Each side's library-features listing is read from
    /// `records` (the command's store) by its key, or walked once and put there ([`CommandFeatures`]); `tool` is the
    /// `Script` kind's tool, so a change of the walk's code moves the key (rule 52).
    #[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or fact of it")]
    fn open_shared(cx: &Cx, env: &FetchEnv<'_>, records: &mut dyn Store<'_>, tool: blaze_drive::Tool, workers: u32, world: &blaze_world::World<'_>, graph_record: &'f [u8], frame: &'f Frame) -> Result<&'f Shared<'f>, SetupError<'f>> {
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
        let setup = {
            let _cx = cx.span("scripts: each side's probe answers (library features listed) and the native runner");
            let mut features = CommandFeatures { cx, store: records, held: None };
            blaze_world::script_setting::ScriptSetup::open_recorded(
                blaze_world::script_setting::ScriptSetupOpts { world, target: host, macro_target: MACRO_TARGET, native: native_opts, alloc: alloc.clone() },
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
        let shared: &'f Shared<'f> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(Shared { result_alloc: env.heap.shared(), runtime, store, checkout, table, macro_table, spawn, native, no_sdk, graph_record }, frame));
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
        Files::Member { dir } => match (ScriptRoot::member(dir), checkout) {
            (Ok(root), crate::engine_script_reads::Served::Root(checkout)) => Ok((dir_tree(checkout), root)),
            (Ok(_), crate::engine_script_reads::Served::Simulated) => {
                Err(failed("a member's script with no served checkout", format_args!("the build script of {package}: its member folder `{dir}`: a simulated world serves no checkout on disk"), false))
            }
            (Err(error), _) => Err(failed("a member folder a script cannot see", format_args!("the build script of {package}: its member folder `{dir}`: {error}"), false)),
        },
        Files::External { dir } => match (ScriptRoot::member(dir), checkout) {
            (Ok(_), crate::engine_script_reads::Served::Root(checkout)) => {
                let pkg_path = if dir.is_empty() { checkout.path().to_path_buf() } else { checkout.path().join(dir) };
                match blaze_proc_macro::script_sandbox::RealRoot::resolve(&pkg_path) {
                    Ok(root) => Ok((dir_tree(&root), ScriptRoot::package())),
                    Err(error) => Err(failed("an external package folder a script cannot see", format_args!("the build script of {package}: its external folder `{dir}`: {error}"), false)),
                }
            }
            (Ok(_), crate::engine_script_reads::Served::Simulated) => {
                Err(failed("an external package's script with no served checkout", format_args!("the build script of {package}: its external folder `{dir}`: a simulated world serves no checkout on disk"), false))
            }
            (Err(error), _) => Err(failed("an external package folder a script cannot see", format_args!("the build script of {package}: its external folder `{dir}`: {error}"), false)),
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
        // The world (the crate list's rows, both sides): made once a command, here when no
        // `CrateScope` task made it first, as engine_units.rs does.
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
            let Some(listed) = names.as_ref() else { return Err(CliError::Text { what: "the names world, made above, is gone" }) };
            // Avoid building the full 1,200-crate world (mapping all fetched packages) when no
            // `Names` task has needed it yet: `open_light` opens only the toolchain shelf and
            // unit graph for script execution.
            let opened = match listed.made().filter(|made| made.has_open_packs()) {
                Some(made) => Scripts::open(&cx, env, made, bytes, lock, members, frame),
                None => Scripts::open_light(&cx, env, listed, bytes, lock, members, frame),
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
    let Some(module) = names.as_ref().and_then(|n| n.script_readers(task.name).next().and_then(|at| n.script_module_of(at))) else {
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
    let shared = match scripts.shared(&cx, env, store, tool, workers, world, frame) {
        Ok(shared) => shared,
        Err(words) => return Ok(Prepared::Said { what: NOT_SET_UP, words: said(jobs, format_args!("{words}")), hole: true }),
    };
    Ok(Prepared::Ready(ScriptJob { name: task.name, shared, script, side, cfg, files, producers: producers.into_bump_slice(), checkout: CheckoutReads::Hole }))
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
    // The one choice point (D112 / D175 zero-Wasm), before anything runs: a script placed native
    // needs the command's native setting, or fails naming why it has none.
    let runner = blaze_proc_macro::native_unit::placed_in_store(&shared.store, graph, job.script, MODE, crate::graph::TRIPLE, alloc);
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
    let checkout_prefix = match job.files {
        Files::Member { .. } => Some(""),
        Files::External { dir } => Some(dir),
        Files::Pack { .. } => None,
    };
    let is_checkout = checkout_prefix.is_some();
    // A kept checkout-backed run's failure is a hole: the reads it made are not handed back with
    // its error (`ScriptError`), so no read list makes it stand, and standing on its drive inputs
    // alone would keep it failed past the edit that fixes it (rule 21). Under
    // [`CheckoutReads::Hole`] its failure is filed.
    let failed_hole = is_checkout && matches!(job.checkout, CheckoutReads::Kept { .. });
    let ran = match script_unit::outputs_given(&shared.runtime, &setting, graph, job.script, side_cfg.as_ref().map(|atoms| atoms.as_slice()), tree, root, &given, alloc) {
        Ok(ran) => ran,
        Err(error) => return failed("a build script failed", format_args!("the build script of {package} failed: {error}"), failed_hole),
    };
    if let Some(error) = ran.outputs.errors.first() {
        return failed("a build script printed `cargo::error`", format_args!("the build script of {package} failed: cargo::error={}", error.as_str()), failed_hole);
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
        CheckoutReads::Kept { plant, status } => match admit_reads(&ran.reads, ran_as, &shared.spawn, checkout_prefix, plant, status, checkout, alloc) {
            Ok(()) => {
                for read in checkout.iter() {
                    reads.push((read.name(), read.answer));
                }
            }
            Err(no_cell) => *unkept = Some(no_cell),
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
    use crate::engine_script_reads::{self as reads, NoCell, READS_CAP, Subject};

    let result_alloc = *cells.allocator();
    if let Some(extra) = observed.get(READS_CAP) {
        let subject = extra.subject.strip_prefix("read-limit:").unwrap_or(&extra.subject);
        return Err(NoCell::TooMany { reads: READS_CAP + 1, subject: Subject::from_str_in(subject, result_alloc) });
    }
    let image_subject = blaze_proc_macro::native_facts::IMAGE_SUBJECT;
    let image = observed.iter().find(|read| read.subject == image_subject).and_then(|read| read.digest);
    if ran_as == RanAs::Native && image.is_none() {
        let missing = observed.iter().find(|read| read.subject.starts_with("pkg-config\n")).map_or(image_subject, |read| read.subject.as_str());
        return Err(NoCell::Unobserved { subject: Subject::from_str_in(missing, result_alloc) });
    }
    let mut receipts = blaze_own::BTreeMap::new_in(alloc);
    let mut file = |path: Subject, answer| cells.push(reads::CheckoutRead { kind: reads::ReadKind::File, path, answer });
    for read in observed {
        let subject = read.subject.as_str();
        let rejected = || NoCell::Unobserved { subject: Subject::from_str_in(subject, result_alloc) };
        if ran_as == RanAs::Wasm && let Some((grant, key)) = blaze_build_script::host_tools::spawn_receipt(subject) {
            let pair = receipts.entry(key).or_insert([None; 2]);
            let digest = read.digest.ok_or_else(rejected)?;
            if pair[usize::from(grant)].replace(digest).is_some_and(|was| was != digest) {
                return Err(NoCell::Conflicting { subject: Subject::from_str_in(subject, result_alloc) });
            }
        } else if subject == image_subject && ran_as == RanAs::Native {
            let digest = read.digest.ok_or_else(rejected)?;
            if image != Some(digest) {
                return Err(NoCell::Conflicting { subject: Subject::from_str_in(subject, result_alloc) });
            }
            file(Subject::from_str_in(crate::sweep::SHIM_FILE, result_alloc), digest);
            file(Subject::from_str_in(crate::sweep::PINS_FILE, result_alloc), blaze_proc_macro::native_facts::pins_digest(setting.pins.as_ref(), alloc));
        } else if blaze_proc_macro::spawn_route::is_fact_reply(subject, alloc) {
            // A derived fact (pkg-config, sw_vers, cc-version): its reply is a function of the
            // pins, so it is a keyed read of a reply file the next command answers again.
            let digest = read.digest.ok_or_else(rejected)?;
            if !blaze_proc_macro::spawn_route::fact_reply_matches(subject, Some(digest), setting.pins.as_ref(), alloc) {
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
                return Err(NoCell::Unobserved { subject });
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
        Some("") => reads::collect_reads(tree_reads, plant, status, cells),
        Some(prefix) => {
            let mut rebased: AVec<'_, (blaze_own::AString<'_>, Option<ContentId>)> = AVec::new_in(alloc);
            for (subject, digest) in tree_reads {
                let text = rebase_read_subject(prefix, subject, alloc).unwrap_or_else(|| blaze_own::AString::from_str_in(subject, alloc));
                rebased.push((text, digest));
            }
            reads::collect_reads(rebased.iter().map(|(s, d)| (s.as_str(), *d)), plant, status, cells)
        }
        None => reads::pinned_reads(tree_reads, result_alloc),
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
