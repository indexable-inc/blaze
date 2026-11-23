//! Runs real build scripts of a workspace's graph in the sandbox, each once: its outputs (cfgs,
//! env pairs, files with sizes, C requests, reads) and time. Nothing is kept: the command's
//! `Script` task record is the only cache of a run (D115).
//!
//! `tempo_scripts <checkout> <blaze-fetch store> <blaze store> <package>...`: the graph is
//! blaze-unit-graph's for a dev check on aarch64-apple-darwin (sources from the fetch store's
//! records, never cargo's caches); each named package's script modules are the bootstrap
//! builder's records in `<blaze store>/build-script/modules`. A dependency's files are served
//! from its record; a member's from the checkout, read-only. The checkout is never written.
//!
//! Every run is made as `blaze check`'s `Script` task makes it, through the same two shared
//! functions (rule 30), so the census's records never drift from the command's:
//! - the setting: blaze-world `script_setting::ScriptSetup::open_recorded` over the world of
//!   the graph, its library-features records held for this process alone ([`ONE_RUN_TOOL`])
//!   (the toolchain from `$HOME/.blaze/v2/toolchain`, `blaze_toolchain::PINNED`): each side's
//!   probe table (pinned rows, ships facts, the toolchain's library-feature listings), the spawn
//!   setting and the native runner;
//! - the run: `script_unit::outputs_given`, served the records of its `links` closure's runs
//!   made earlier in this process (`DEP_<LINKS>_*` and their files, wasm or native alike).
//!
//! With `native <shelf> <sdk> <fact shim>` first, a script placed native for `check` (D112:
//! toolchain.rs `NATIVE_EVERY_MODE`, chosen by `native_unit::placed` alone) runs in the
//! Seatbelt native runner with the pinned clang unpacked on `<shelf>` and the macOS SDK
//! `<sdk>`, its host binary from the interim builder's `native` mode in
//! `<blaze store>/build-script/modules`, its work under `<blaze store>/native-work/tempo_scripts`;
//! without it such a script is refused by name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use blaze_build_script::git;
use blaze_build_script::key::CMode;
use blaze_build_script::native::Runner;
use blaze_build_script::outputs::OutputsRef;
use blaze_fetch::{PackView, SourceId};
use blaze_manifest::layout::{Entry, Layout, Need, Pass as LayoutPass};
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{Alloc, Vec as AVec};
use blaze_proc_macro::{
    native_setting::{NativeRunOpts, native_setting},
    native_unit::placed,
    script::ScriptRuntime,
    script_sandbox::{ScriptRoot, Tree, dir_tree, mem_tree},
    script_unit::{GivenRun, Setting, links_closure, links_order, outputs_given},
};
use blaze_unit_graph::{Ask, CfgAtom, Package, Pass, Records, Resolution, Resolver, StoreSources, TargetDesc, UnitGraph, UnitId, UnitKind};
use blaze_world::script_setting::{ScriptSetup, ScriptSetupOpts};

const TARGET: &str = "aarch64-apple-darwin";
/// The macro side's triple (D89 (a)), as blaze-cli's scripts.rs names it.
const MACRO_TARGET: &str = "wasm32-wasip1";

/// The tool in the library-features records' keys: they live in a `FeatureMemory` made for this
/// process and dropped with it, which holds no record of another build of the walk, so one
/// fixed value tells nothing apart that needs telling (blaze-world lib_features_record.rs).
const ONE_RUN_TOOL: blaze_ids::ContentId = blaze_ids::ContentId([0; 32]);

/// The runtime over the folder `store` (its compiled modules).
fn script_runtime(store: &Path) -> ScriptRuntime {
    ScriptRuntime::new(store).unwrap_or_else(|error| panic!("the script runtime: {error}"))
}

/// The installed toolchain's files (`$HOME/.blaze/v2/toolchain`, by 64-hex name), read whole
/// and kept for the run, as blaze-world's `names` example reads them.
struct DirShelf(String);

impl blaze_toolchain::Shelf for DirShelf {
    type Error = std::io::Error;
    type Bytes = &'static [u8];
    fn place(&self) -> &str {
        &self.0
    }
    fn size(&mut self, name: &str) -> Result<Option<u64>, std::io::Error> {
        match std::fs::metadata(format!("{}/{name}", self.0)) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn read(&mut self, name: &str, _size: u64) -> Result<&'static [u8], std::io::Error> {
        std::fs::read(format!("{}/{name}", self.0)).map(|bytes| &*Box::leak(bytes.into_boxed_slice()))
    }
}

/// `rustc --print cfg` for aarch64-apple-darwin, as blaze-unit-graph's example passes it.
const MAC_CFG: &[(&str, Option<&str>)] = &[
    ("panic", Some("unwind")),
    ("target_abi", Some("")),
    ("target_arch", Some("aarch64")),
    ("target_endian", Some("little")),
    ("target_env", Some("")),
    ("target_family", Some("unix")),
    ("target_feature", Some("aes")),
    ("target_feature", Some("crc")),
    ("target_feature", Some("dit")),
    ("target_feature", Some("dotprod")),
    ("target_feature", Some("dpb")),
    ("target_feature", Some("dpb2")),
    ("target_feature", Some("fcma")),
    ("target_feature", Some("fhm")),
    ("target_feature", Some("flagm")),
    ("target_feature", Some("fp16")),
    ("target_feature", Some("frintts")),
    ("target_feature", Some("jsconv")),
    ("target_feature", Some("lor")),
    ("target_feature", Some("lse")),
    ("target_feature", Some("neon")),
    ("target_feature", Some("paca")),
    ("target_feature", Some("pacg")),
    ("target_feature", Some("pan")),
    ("target_feature", Some("pmuv3")),
    ("target_feature", Some("ras")),
    ("target_feature", Some("rcpc")),
    ("target_feature", Some("rcpc2")),
    ("target_feature", Some("rdm")),
    ("target_feature", Some("sb")),
    ("target_feature", Some("sha2")),
    ("target_feature", Some("sha3")),
    ("target_feature", Some("ssbs")),
    ("target_feature", Some("vh")),
    ("target_has_atomic", Some("128")),
    ("target_has_atomic", Some("16")),
    ("target_has_atomic", Some("32")),
    ("target_has_atomic", Some("64")),
    ("target_has_atomic", Some("8")),
    ("target_has_atomic", Some("ptr")),
    ("target_os", Some("macos")),
    ("target_pointer_width", Some("64")),
    ("target_vendor", Some("apple")),
    ("unix", None),
];

fn mac(alloc: Alloc<'static>) -> TargetDesc<'static> {
    let mut cfg = AVec::new_in(alloc);
    for (name, value) in MAC_CFG {
        cfg.push(match value {
            Some(v) => CfgAtom::pair(name, v, alloc),
            None => CfgAtom::flag(name, alloc),
        });
    }
    TargetDesc::new(TARGET, cfg, alloc)
}

/// The workspace's layout, read from the checkout.
fn layout(anchor: &Path, alloc: Alloc<'static>) -> Result<blaze_manifest::layout::Units<'static>, String> {
    let mut reader = Layout::new("", alloc).map_err(|e| e.to_string())?;
    for _ in 0..64 {
        let needs = match reader.pass() {
            Ok(LayoutPass::Done(units)) => return Ok(units),
            Ok(LayoutPass::Need(needs)) => needs,
            Err(e) => return Err(e.to_string()),
        };
        for need in needs.iter() {
            match need {
                Need::Manifest(path) => {
                    let text = std::fs::read_to_string(anchor.join(path.as_str())).ok();
                    reader.manifest(path, text.as_deref()).map_err(|e| format!("{}: {e}", path.as_str()))?;
                }
                Need::Listing(dir) => {
                    let listed: Option<Vec<(String, bool)>> = std::fs::read_dir(anchor.join(dir.as_str())).ok().map(|listed| {
                        let mut entries: Vec<(String, bool)> = listed
                            .flatten()
                            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.file_type().is_ok_and(|t| t.is_dir())))
                            .collect();
                        entries.sort();
                        entries
                    });
                    let entries: Option<Vec<Entry<'_>>> = listed.as_ref().map(|l| l.iter().map(|(name, is_dir)| Entry { name, is_dir: *is_dir }).collect());
                    reader.listing(dir, entries.as_deref());
                }
            }
        }
    }
    Err("no layout after 64 passes".to_string())
}

/// blaze-fetch's store folder: one file a record, `records/<hex of the key>`, read whole and
/// kept for the run.
#[derive(Clone)]
struct Folder {
    records: PathBuf,
}

impl Records<'static> for Folder {
    fn record(&mut self, source: &SourceId<'_>) -> Option<&'static [u8]> {
        let mut name = String::with_capacity(64);
        for b in source.key().0 {
            name.push_str(&format!("{b:02x}"));
        }
        let path = self.records.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => Some(Box::leak(bytes.into_boxed_slice())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
}

/// The module record of `name version`'s script in `store`: its folder.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, checkout, fetch_store, store, wanted @ ..] = &args[..] else {
        eprintln!(
            "usage: tempo_scripts <checkout> <blaze-fetch store> <blaze store> [native <shelf> <sdk> <fact shim>] [dependents] [export <dir>] [<package>...] (no package: every script)"
        );
        std::process::exit(2);
    };
    let (native_args, wanted) = match wanted {
        [first, shelf, sdk, shim, rest @ ..] if first == "native" => (Some((shelf.as_str(), sdk.as_str(), shim.as_str())), rest),
        _ => (None, wanted),
    };
    // `export <dir>`: each script's outputs record written as `<dir>/<name>@<version>@<side>.outputs`
    // (side `host` or `target`, the served unit's), what blaze-world's `names --scripts` reads.
    let (dependents, wanted) = match wanted {
        [first, rest @ ..] if first == "dependents" => (true, rest),
        _ => (false, wanted),
    };
    let (export, wanted) = match wanted {
        [first, dir, rest @ ..] if first == "export" => (Some(PathBuf::from(dir)), rest),
        _ => (None, wanted),
    };
    let (root, store) = (PathBuf::from(checkout), PathBuf::from(store));
    // The served root, resolved once (the argument may be relative: made absolute here).
    let real_root = blaze_proc_macro::script_sandbox::RealRoot::resolve(&std::path::absolute(&root).expect("the checkout's absolute path")).expect("the checkout resolves");
    // An entry that takes no grant yet (a plain `fn main`): the run's memory is one frame
    // taken from the OS here, kept for the process's life since the lock and the resolution
    // below are `'static`, and handed down.
    let frame: &'static blaze_mem::Frame =
        Box::leak(Box::new(blaze_mem::Frame::with_reserve(1 << 36, blaze_mem::Tag::Usercode).expect("the entry's frame")));
    let erased: &'static blaze_own::Erased<&'static blaze_mem::Frame> = Box::leak(Box::new(blaze_own::Erased::new(frame)));
    let alloc: Alloc<'static> = erased.handle();
    let bump: blaze_own::Bump<'static> = erased.bump();
    let bytes = std::fs::read(root.join("Cargo.lock")).unwrap_or_else(|e| panic!("{}: {e}", root.join("Cargo.lock").display()));
    let lock: &'static Lock<'static> = match Lock::read(&bytes, alloc) {
        Ok(lock) => Box::leak(Box::new(lock)),
        Err(e) => panic!("Cargo.lock: {e}"),
    };
    let ws = layout(&root, alloc).unwrap_or_else(|e| panic!("workspace layout: {e}"));
    let written = ws.resolver.as_ref().map(|(r, _)| r.as_str());
    let resolver = Resolver::of(written, ws.root_edition).unwrap_or_else(|_| panic!("unknown resolver {written:?}"));
    let mut member_dirs: BTreeMap<LockId, String> = BTreeMap::new();
    let mut roots = AVec::new_in(alloc);
    let mut members = Vec::new();
    // Each member's folder, checkout-relative: where the world finds a path package's files.
    let mut paths: Vec<blaze_world::CheckoutPath<'static>> = Vec::new();
    for m in ws.members.iter() {
        if let Some(id) = lock.named(&m.name).find(|id| matches!(lock.package(*id).source, LockSource::Path)) {
            roots.push(id);
            member_dirs.insert(id, m.dir.as_str().to_owned());
            members.push((id, Package::from_member(m, alloc)));
            paths.push(blaze_world::CheckoutPath { id, dir: Box::leak(m.dir.as_str().to_owned().into_boxed_str()), kind: blaze_world::PathKind::Member });
        }
    }
    let ask: &'static Ask<'static> =
        Box::leak(Box::new(Ask { roots, features: AVec::new_in(alloc), all_features: false, no_default_features: false, tests: false }));
    let target: &'static TargetDesc<'static> = Box::leak(Box::new(mac(alloc)));
    let mut res = Resolution::new(lock, target, target, ask, resolver, alloc);
    for (id, p) in members {
        res.member(id, p).unwrap_or_else(|e| panic!("{e}"));
    }
    let folder = Folder { records: PathBuf::from(fetch_store).join("records") };
    let mut sources = StoreSources::new(lock, folder, PackView::open_source_put, alloc).unwrap_or_else(|e| panic!("{e}"));
    let mut passes = 0;
    let graph: &'static UnitGraph<'static> = Box::leak(Box::new(loop {
        passes += 1;
        assert!(passes <= 64, "no graph after 64 passes");
        match res.pass().unwrap_or_else(|e| panic!("{e}")) {
            Pass::Done(graph) => break graph,
            Pass::Partial(partial) => panic!("no unit graph: the manifests of {:?} are missing", partial.missing()),
            Pass::Need(needs) => {
                for id in needs.iter() {
                    let facts = sources.package(*id).unwrap_or_else(|e| panic!("{e}"));
                    res.package(*id, Some(facts)).unwrap_or_else(|e| panic!("{e}"));
                }
            }
        }
    }));

    // The world of the graph, as `blaze check` makes it (blaze-world `open_list` and `world`):
    // the crate list, both sides' toolchain crates from the pinned toolchain, and their texts,
    // over which the probe tables are made.
    let home = std::env::var("HOME").unwrap_or_else(|_| panic!("HOME is not set: the pinned toolchain is read from $HOME/.blaze/v2/toolchain"));
    let checkout: &'static blaze_world::Checkout = Box::leak(Box::new(blaze_world::Checkout::new(root.clone())));
    let world: &'static blaze_world::World<'static> = {
        let mut packages = blaze_world::Fetched { store: &mut sources, paths: &paths, lock, alloc: bump };
        let mut shelf = DirShelf(format!("{home}/.blaze/v2/toolchain"));
        let listed = blaze_world::open_list(&mut shelf, blaze_toolchain::PINNED, TARGET, graph, lock, &mut packages, bump).unwrap_or_else(|e| panic!("the crate list: {e}"));
        let listed: &'static blaze_world::Listed<'static, &'static [u8]> = Box::leak(Box::new(listed));
        Box::leak(Box::new(blaze_world::world(graph, lock, listed, &mut packages, checkout, bump).unwrap_or_else(|e| panic!("the world: {e}"))))
    };

    let runtime = script_runtime(&store);
    // `check`'s placement: C requests deferred; the bindgen class (D69's NATIVE_EVERY_MODE)
    // runs native with `native <shelf> <sdk> <fact shim>` (D112), and is refused by name
    // without it. The runner (the shim placed, the jobserver with a token for each core but the
    // script's own) is opened by the shared setup, as `blaze` opens it.
    let work = store.join("native-work/tempo_scripts");
    let work = work.to_str().unwrap_or_else(|| panic!("{}: not UTF-8", work.display())).to_owned();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let workers = u32::try_from(cores).ok().and_then(core::num::NonZeroU32::new).expect("a core count of 1 to 2^32 - 1");
    let native = match native_args {
        None => Err("no `native <shelf> <sdk> <fact shim>` was given"),
        Some((shelf, sdk, shim)) => {
            let owned = native_setting(shelf, sdk, &work, TARGET, alloc).unwrap_or_else(|e| panic!("{e}"));
            println!("native: shelf {}, SDK {} (version {}), work {}", owned.shelf.as_str(), owned.sdk.as_str(), owned.sdk_version.as_str(), owned.work.as_str());
            Ok(NativeRunOpts { setting: owned, shim, workers })
        }
    };
    let mut features = blaze_world::lib_features_record::FeatureMemory::new(alloc);
    let setup = ScriptSetup::open_recorded(ScriptSetupOpts { world, target: TARGET, macro_target: MACRO_TARGET, native, alloc }, ONE_RUN_TOOL, &mut features).unwrap_or_else(|e| panic!("the scripts' setting: {e}"));
    println!("probe table: {} rows for {TARGET}, {} for {MACRO_TARGET}", setup.table.len(), setup.macro_table.len());
    match &setup.native {
        Ok(run) => println!("native: fact shim {}", run.shim),
        Err(why) => println!("native: none ({why}): a script placed native is refused by name"),
    }
    let setting = Setting {
        store: &store,
        target: TARGET,
        host: TARGET,
        table: Arc::clone(&setup.table),
        probes: None,
        c_mode: CMode::Deferred,
        mode: blaze_build_script::toolchain::Mode::Check,
        native: setup.native.as_ref().ok().map(|run| run.setting()),
        spawn_setting: Arc::clone(&setup.spawn),
    };
    let units = &graph.units;
    let mut trees: BTreeMap<LockId, (Tree, ScriptRoot)> = BTreeMap::new();
    // `dependents`: for each package named, how many library units of the graph depend on its
    // library, directly or through others (a work list over the reversed edges).
    if dependents {
        for name in wanted {
            let mut seen = vec![false; units.len()];
            let mut work: Vec<usize> = (0..units.len()).filter(|i| units[*i].name.as_str() == name && matches!(units[*i].kind, UnitKind::Lib)).collect();
            while let Some(at) = work.pop() {
                for (i, unit) in units.iter().enumerate() {
                    if !seen[i] && unit.deps.iter().any(|d| d.unit.index() == at) {
                        seen[i] = true;
                        work.push(i);
                    }
                }
            }
            let libs = (0..units.len()).filter(|i| seen[*i] && matches!(units[*i].kind, UnitKind::Lib | UnitKind::ProcMacro)).count();
            let all = seen.iter().filter(|s| **s).count();
            println!("{name}: {libs} library units depend on it ({all} units of any kind)");
        }
        return;
    }
    // No package named: every build script of the graph.
    let every: Vec<String> = if wanted.is_empty() {
        let mut names: Vec<String> = units.iter().filter(|u| u.kind == UnitKind::BuildScript).map(|u| u.name.as_str().to_owned()).collect();
        names.sort_unstable();
        names.dedup();
        names
    } else {
        wanted.to_vec()
    };
    // Every script in `links` order: each run of a script's `links` closure is made before it,
    // and kept as its outputs RECORD with the tree its package was served, which is what the
    // command's `Script` task is handed of it.
    let order = links_order(graph).unwrap_or_else(|e| panic!("{e}"));
    let mut runs: BTreeMap<UnitId, (Vec<u8>, Tree, ScriptRoot)> = BTreeMap::new();
    for name in &every {
        if !order.iter().any(|s| units[s.index()].name.as_str() == name) {
            println!("{name}: no build-script unit in the graph");
        }
    }
    let (mut ran_count, mut asked) = (0usize, 0usize);
    for script in order.iter().copied().filter(|s| every.iter().any(|n| n == units[s.index()].name.as_str())) {
        asked += 1;
        let unit = &units[script.index()];
        let package = format!("{} {}", unit.name.as_str(), unit.version.as_str());
        let Some(served) = units.iter().find(|u| u.build_script == Some(script)) else {
            println!("{package}: its script serves no unit");
            continue;
        };
        let features: Vec<&str> = unit.features.iter().map(|f| f.as_str()).collect();
        println!("== {package} ({} side, features [{}])", if served.for_host { "host" } else { "target" }, features.join(","));
        let (tree, folder) = trees
            .entry(unit.package)
            .or_insert_with(|| match member_dirs.get(&unit.package) {
                Some(dir) => (dir_tree(&real_root), ScriptRoot::member(dir).unwrap_or_else(|e| panic!("{package}: {e}"))),
                None => {
                    let (view, dir) = sources.files(unit.package).unwrap_or_else(|e| panic!("{e}"));
                    // A fetched git source answers `git` from the lock's commit (D54).
                    let git = match &lock.packages[unit.package.index()].source {
                        LockSource::Git { commit, .. } => {
                            Some(git::Oid::parse_hex(commit.as_bytes()).unwrap_or_else(|| panic!("{package}: the lock's commit `{commit}` is not 40 hex digits")))
                        }
                        LockSource::Path | LockSource::Registry { .. } => None,
                    };
                    (mem_tree(view.files().map(|f| (f.path, f.bytes)), dir.as_str(), git), ScriptRoot::package())
                }
            })
            .clone();
        // The runs of its `links` closure made so far, read from their records in place; one
        // that gave no outputs is missing, and `outputs_given` refuses a direct one by name.
        let mut given: Vec<GivenRun<'_>> = Vec::new();
        for (input, links, direct) in links_closure(graph, script) {
            let Some((record, tree, root)) = runs.get(&input) else { continue };
            let outputs = OutputsRef::open(record).unwrap_or_else(|why| panic!("{package}: the record of its `links = \"{links}\"` input does not read back: {why:?}"));
            if direct {
                for (key, value) in outputs.metadata() {
                    println!("    given DEP_{}_{}={value:?}", links.to_uppercase().replace('-', "_"), key.to_uppercase().replace('-', "_"));
                }
            }
            given.push(GivenRun { script: input, links, outputs, tree: blaze_own::Share::share(tree), root: blaze_own::Share::share(root), direct });
        }
        // The one choice point (D112), made by the run itself: said here for the census.
        if let Runner::Native { because } = placed(graph, script, setting.mode, alloc) {
            let because = &units[because.index()];
            println!("  native (its closure holds {} {}, D112)", because.name.as_str(), because.version.as_str());
        }
        let started = Instant::now();
        let ran = match outputs_given(&runtime, &setting, graph, script, None, blaze_own::Share::share(&tree), blaze_own::Share::share(&folder), &given, alloc) {
            Ok(ran) => ran,
            Err(error) => {
                println!("  run: {error}");
                continue;
            }
        };
        drop(given);
        ran_count += 1;
        let o = &ran.outputs;
        println!("  run: {:?}, fuel {}, peak memory {}, spawns {}", started.elapsed(), ran.fuel, ran.peak_memory, ran.spawns);
        for (key, value) in o.metadata.iter() {
            println!("    metadata {}={:?}", key.as_str(), value.as_str());
        }
        for (cfg, value) in o.cfgs.iter() {
            println!("    cfg {}{}", cfg.as_str(), value.as_ref().map_or(String::new(), |v| format!("={:?}", v.as_str())));
        }
        for name in o.check_cfgs.iter() {
            println!("    check-cfg {}", name.as_str());
        }
        for (key, value) in o.env.iter() {
            println!("    env {}={:?}", key.as_str(), value.as_str());
        }
        for file in o.files.iter() {
            println!("    file {} ({} bytes)", file.name.as_str(), file.bytes.len());
        }
        for file in o.stored.iter() {
            println!("    stored file {} ({} bytes)", file.name.as_str(), file.len);
        }
        for request in o.c_requests.iter() {
            println!("    c request {:?} (OUT_DIR state {})", request.argv.as_str().replace('\0', " "), request.state);
        }
        for warning in o.warnings.iter() {
            println!("    warning {:?}", warning.as_str());
        }
        println!("    reads {}: {}", ran.reads.len(), ran.reads.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>().join(" "));
        if served.links.is_some() {
            let mut record = Vec::new();
            {
                let mut encoded = AVec::new_in(alloc);
                o.encode(&mut encoded);
                record.extend_from_slice(&encoded);
            }
            runs.insert(script, (record, tree, folder));
        }
        if let Some(dir) = &export {
            export_record(dir, &store, unit, served.for_host, o, alloc);
        }
    }
    println!("scripts: {ran_count} of {asked} ran");
    if let Ok(run) = &setup.native
        && let Err(error) = run.end()
    {
        println!("native: {error}");
    }
}

/// `export <dir>`: the run's outputs record written as `<dir>/<name>@<version>@<side>.outputs`
/// (what blaze-world's `ScriptRecords` readers load), and each of its STORED files (a native
/// script's `OUT_DIR` files, which the record names by digest and does not hold) put beside it
/// in `<dir>/files/<sha256 hex>`, cloned from the run's file area
/// (`<blaze store>/build-script/files`, where the native runner renamed them) by
/// `std::fs::copy`, which clones on APFS; a digest already there is left. A stored file the
/// area lacks stops the export naming it.
fn export_record(dir: &Path, store: &Path, unit: &blaze_unit_graph::Unit<'_>, for_host: bool, outputs: &blaze_build_script::Outputs<'_>, alloc: Alloc<'_>) {
    let mut record = AVec::new_in(alloc);
    outputs.encode(&mut record);
    let side = if for_host { "host" } else { "target" };
    let path = dir.join(format!("{}@{}@{side}.outputs", unit.name.as_str(), unit.version.as_str()));
    std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    std::fs::write(&path, &record[..]).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    println!("  exported {} ({} bytes)", path.display(), record.len());
    if outputs.stored.is_empty() {
        return;
    }
    let area = store.join(blaze_build_script::outputs::FILE_AREA);
    let files = dir.join("files");
    std::fs::create_dir_all(&files).unwrap_or_else(|e| panic!("{}: {e}", files.display()));
    for file in outputs.stored.iter() {
        let hex: String = file.sha256.iter().map(|byte| format!("{byte:02x}")).collect();
        let to = files.join(&hex);
        if !to.exists() {
            let from = area.join(&hex);
            std::fs::copy(&from, &to).unwrap_or_else(|e| panic!("{}: stored file `{}` of {}: {e}", from.display(), file.name.as_str(), path.display()));
        }
        println!("  exported stored file {} ({} bytes) as {}", file.name.as_str(), file.len, to.display());
    }
}

