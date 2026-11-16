//! Runs real build scripts of a workspace's graph in the sandbox, each twice: the first run's
//! outputs (cfgs, env pairs, files with sizes, C requests, reads) and time, then the second
//! answered from the record.
//!
//! `tempo_scripts <checkout> <blaze-fetch store> <blaze store> <package>...`: the graph is
//! blaze-unit-graph's for a dev check on aarch64-apple-darwin (sources from the fetch store's
//! records, never cargo's caches); each named package's script modules are the bootstrap
//! builder's records in `<blaze store>/build-script/modules`. A dependency's files are served
//! from its record; a member's from the checkout, read-only. The checkout is never written.
//! With `discover` before the packages, each script is run until it asks no try-compile the
//! pinned table lacks, and every such question is printed whole: the rows to write.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use blaze_build_script::{
    env::PackageFacts,
    key::CMode,
    probe::{Known, Row, pinned_rows},
};
use blaze_build_script::cx::ScriptCx;
use blaze_build_script::git;
use blaze_expand::{DefAt, DefSite, Export, ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, LocalInner, MacroDefinition, Place, PlaceOf, ProcCall, Produced, TokenStream};
use blaze_fetch::{PackView, SourceId};
use blaze_id::CrateItemKey;
use blaze_lower::{Cfg, CfgAnswer, ItemRef, Outcome, Sink, Source, What, by_name::ByName, lower_file};
use blaze_manifest::layout::{Entry, Layout, Need, Pass as LayoutPass};
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{Alloc, Vec as AVec};
use blaze_proc_macro::{
    record::{DirStore, native_world},
    script::{ScriptError, ScriptRuntime, ScriptWhy},
    script_sandbox::{Refusal, Script, SpawnRefusal, Tree, dir_tree, mem_tree},
    script_unit::{Prepared, Setting, links_inputs, links_order, prepare},
};
use blaze_build_script::env::DepMetadata;
use blaze_unit_graph::{Ask, CfgAtom, Package, Pass, Records, Resolution, Resolver, StoreSources, TargetDesc, UnitId, UnitKind};

const TARGET: &str = "aarch64-apple-darwin";

/// The runtime over the folder `store`, its records through a world this program opens there.
fn script_runtime(store: &Path) -> ScriptRuntime {
    let (io, root) = native_world(store).unwrap_or_else(|error| panic!("the records at {}: {error}", store.display()));
    ScriptRuntime::new(store, DirStore::build_scripts(&io, &root)).unwrap_or_else(|error| panic!("the script runtime: {error}"))
}

/// The try-compiles the class C scripts make: none. Class A's rows come with their reasons.
const KNOWN: &[Known] = blaze_build_script::pinned::TEMPO;

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
        eprintln!("usage: tempo_scripts <checkout> <blaze-fetch store> <blaze store> [discover] <package>...");
        std::process::exit(2);
    };
    let (discover, wanted) = match wanted {
        [first, rest @ ..] if first == "discover" => (true, rest),
        _ => (false, wanted),
    };
    let (outline, wanted) = match wanted {
        [first, rest @ ..] if first == "outline" => (true, rest),
        _ => (false, wanted),
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
    let alloc = Alloc::global();
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
    for m in ws.members.iter() {
        if let Some(id) = lock.named(&m.name).find(|id| matches!(lock.package(*id).source, LockSource::Path)) {
            roots.push(id);
            member_dirs.insert(id, m.dir.as_str().to_owned());
            members.push((id, Package::from_member(m, alloc)));
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
    let graph = loop {
        passes += 1;
        assert!(passes <= 64, "no graph after 64 passes");
        match res.pass().unwrap_or_else(|e| panic!("{e}")) {
            Pass::Done(graph) => break graph,
            Pass::Need(needs) => {
                for id in needs.iter() {
                    let facts = sources.package(*id).unwrap_or_else(|e| panic!("{e}"));
                    res.package(*id, Some(facts)).unwrap_or_else(|e| panic!("{e}"));
                }
            }
        }
    };

    let runtime = script_runtime(&store);
    let table = Arc::new(pinned_rows(TARGET, KNOWN, alloc).to_vec());
    // `check`'s placement: C requests deferred; the bindgen class (D69's NATIVE_EVERY_MODE)
    // would be refused by name here, as no shelf is given (this example calls `prepare`, the
    // wasm step, directly).
    let setting = Setting {
        store: &store,
        target: TARGET,
        host: TARGET,
        table: Arc::clone(&table),
        c_mode: CMode::Deferred,
        mode: blaze_build_script::toolchain::Mode::Check,
        native: None,
    };
    let units = &graph.units;
    let mut trees: BTreeMap<LockId, (Tree, String)> = BTreeMap::new();
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
    // Every script in `links` order: a dependency's metadata (`DEP_<LINKS>_*`) is known before
    // the scripts of its direct dependents run. Each first run's metadata, by script unit.
    let order = links_order(&graph).unwrap_or_else(|e| panic!("{e}"));
    let mut metadata: BTreeMap<UnitId, Vec<(String, String)>> = BTreeMap::new();
    for name in &every {
        if !order.iter().any(|s| units[s.index()].name.as_str() == name) {
            println!("{name}: no build-script unit in the graph");
        }
    }
    {
        let scripts = order.iter().map(|s| s.index()).filter(|i| every.iter().any(|n| n == units[*i].name.as_str()));
        for at in scripts {
            let unit = &units[at];
            let package = format!("{} {}", unit.name.as_str(), unit.version.as_str());
            let Some(served) = units.iter().find(|u| u.build_script.is_some_and(|s| s.index() == at)) else {
                println!("{package}: its script serves no unit");
                continue;
            };
            let features: Vec<&str> = unit.features.iter().map(|f| f.as_str()).collect();
            println!("== {package} ({} side, features [{}])", if served.for_host { "host" } else { "target" }, features.join(","));
            let (tree, folder) = trees
                .entry(unit.package)
                .or_insert_with(|| match member_dirs.get(&unit.package) {
                    Some(dir) => (dir_tree(&root), dir.clone()),
                    None => {
                        let (view, dir) = sources.files(unit.package).unwrap_or_else(|e| panic!("{e}"));
                        // A fetched git source answers `git` from the lock's commit (D54).
                        let git = match &lock.packages[unit.package.index()].source {
                            LockSource::Git { commit, .. } => {
                                Some(git::Oid::parse_hex(commit.as_bytes()).unwrap_or_else(|| panic!("{package}: the lock's commit `{commit}` is not 40 hex digits")))
                            }
                            LockSource::Path | LockSource::Registry { .. } => None,
                        };
                        (mem_tree(view.files().map(|f| (f.path, f.bytes)), dir.as_str(), git), String::new())
                    }
                })
                .clone();
            let inputs = links_inputs(&graph, UnitId(at as u32));
            let mut deps = Vec::new();
            let mut lacking = None;
            for (input, links) in &inputs {
                match metadata.get(input) {
                    Some(pairs) => deps.extend(pairs.iter().map(|(key, value)| DepMetadata { links: *links, key: key.as_str(), value: value.as_str() })),
                    None => lacking = Some(format!("{} {} (links {links})", units[input.index()].name.as_str(), units[input.index()].version.as_str())),
                }
            }
            if let Some(dependency) = lacking {
                println!("  not run: needs the metadata of {dependency}, whose build script gave no outputs in this run");
                continue;
            }
            for dep in &deps {
                println!("    given DEP_{}_{}={:?}", dep.links.to_uppercase().replace('-', "_"), dep.key.to_uppercase().replace('-', "_"), dep.value);
            }
            let started = Instant::now();
            let Prepared { loaded, script, env } = match prepare(&runtime, &setting, &graph, UnitId(at as u32), tree, folder, &deps, alloc) {
                Ok(prepared) => prepared,
                Err(error) => {
                    println!("  {error}");
                    continue;
                }
            };
            println!("  load {:?} (compiled now: {})", started.elapsed(), loaded.compiled);
            if discover {
                // Every try-compile the script would make: each unanswered one is printed and
                // then answered "refused" in this run's own table only (its id is in the
                // record's key, so these runs never answer a real one), up to 64.
                let mut rows = table.to_vec();
                for _ in 0..64 {
                    let asked = Arc::new(Script { table: Arc::new(rows.clone()), ..Script::clone(&script) });
                    match runtime.run(&loaded, asked, &env, alloc) {
                        Err(ScriptError { why: ScriptWhy::Refused(Refusal::Spawn(SpawnRefusal::CannotAnswerYet { key, args, snippet })), .. }) => {
                            println!("  question {:02x?}: rustc {}\n    snippet {snippet:?}", &key.0[..8], args.join(" "));
                            rows.push(Row { key, accepted: false });
                            rows.sort_by(|a, b| a.key.cmp(&b.key));
                        }
                        Err(error) => {
                            println!("  discovery ended: {error}");
                            break;
                        }
                        Ok(_) => {
                            println!("  discovery done");
                            break;
                        }
                    }
                }
                continue;
            }
            for pass in ["first", "second"] {
                let started = Instant::now();
                match runtime.run(&loaded, Arc::clone(&script), &env, alloc) {
                    Ok(ran) => {
                        let o = &ran.outputs;
                        println!(
                            "  {pass}: {:?}, from the record: {}, fuel {}, peak memory {}, spawns {}",
                            started.elapsed(),
                            ran.from_record,
                            ran.fuel,
                            ran.peak_memory,
                            ran.spawns
                        );
                        if pass == "second" {
                            continue;
                        }
                        metadata.insert(UnitId(at as u32), o.metadata.iter().map(|(k, v)| (k.as_str().to_owned(), v.as_str().to_owned())).collect());
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
                        for request in o.c_requests.iter() {
                            println!("    c request {:?}", request.as_str().replace('\0', " "));
                        }
                        for warning in o.warnings.iter() {
                            println!("    warning {:?}", warning.as_str());
                        }
                        println!("    reads {}: {}", ran.reads.len(), ran.reads.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>().join(" "));
                    }
                    Err(error) => println!("  {pass}: {error}"),
                }
            }
            if outline {
                let Ok(ran) = runtime.run(&loaded, Arc::clone(&script), &env, alloc) else { continue };
                outline_root(&script, served, &ran.outputs);
            }
            if let Some(dir) = &export {
                let Ok(ran) = runtime.run(&loaded, Arc::clone(&script), &env, alloc) else { continue };
                let mut record = AVec::new_in(Alloc::global());
                ran.outputs.encode(&mut record);
                let side = if served.for_host { "host" } else { "target" };
                let path = dir.join(format!("{}@{}@{side}.outputs", unit.name.as_str(), unit.version.as_str()));
                std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
                std::fs::write(&path, &record[..]).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                println!("  exported {} ({} bytes)", path.display(), record.len());
            }
        }
    }
}

/// The served unit's root file lowered with the script's outputs laid over its host
/// ([`ScriptCx`]) and its cfg joined with the script's: every item an expansion made (an
/// `include!` of `OUT_DIR` among them) is printed with its first line.
fn outline_root(script: &Script, served: &blaze_unit_graph::Unit<'_>, out: &blaze_build_script::Outputs<'_>) {
    let root = served.root.as_str();
    let rel = if script.folder.is_empty() { root.to_owned() } else { format!("{}/{root}", script.folder) };
    let Some(bytes) = script.tree.file(&rel) else {
        println!("  outline: no file {rel}");
        return;
    };
    let Ok(text) = String::from_utf8(bytes) else {
        println!("  outline: {rel} is not UTF-8");
        return;
    };
    let mut cfg = UnitCfg { atoms: Vec::new() };
    for atom in served.cfg.iter() {
        cfg.atoms.push((atom.name.as_str().to_owned(), atom.value.as_ref().map(|v| v.as_str().to_owned())));
    }
    for feature in served.features.iter() {
        cfg.atoms.push(("feature".to_owned(), Some(feature.as_str().to_owned())));
    }
    for (name, value) in out.cfgs.iter() {
        cfg.atoms.push((name.as_str().to_owned(), value.as_ref().map(|v| v.as_str().to_owned())));
    }
    let host = Bare { env: script.env.clone() };
    // `edition::Edition` is `repr(u8)` from 0 in edition order, as blaze-surface's is.
    let edition = match served.edition.syntax as u8 {
        0 => blaze_surface::Edition::E2015,
        1 => blaze_surface::Edition::E2018,
        2 => blaze_surface::Edition::E2021,
        _ => blaze_surface::Edition::E2024,
    };
    let at = DefAt {
        site: DefSite { krate: CrateItemKey(blaze_ids::ContentId([1; 32])), module: None },
        edition,
        export: Export::Exported,
        local_inner: LocalInner::No,
        transparency: None,
    };
    let rules: &[(&str, blaze_expand::Prepared<'_>)] = &[];
    // The outputs as their record's bytes, read in place: what a reader of the store is given.
    let mut record = AVec::new_in(Alloc::global());
    out.encode(&mut record);
    let opened = match blaze_build_script::outputs::OutputsRef::open(&record) {
        Ok(opened) => opened,
        Err(why) => {
            println!("  outline: the outputs record does not read back: {why:?}");
            return;
        }
    };
    let mut by_name = ByName::new(rules, at, ScriptCx { inner: host, out: opened }, Alloc::global());
    let mut sink = Made(0, true);
    let stats = lower_file(&text, edition, &mut cfg, &mut blaze_lower::NoModule, &mut by_name, &mut sink, Alloc::global());
    println!("  outline of {rel}: {} items, {} made by expansions", stats.items, sink.0);
    // The control: the same file over the same host with no outputs laid over it.
    let mut empty = AVec::new_in(Alloc::global());
    blaze_build_script::Outputs::new_in(script.out_dir(), Alloc::global()).encode(&mut empty);
    let Ok(none) = blaze_build_script::outputs::OutputsRef::open(&empty) else {
        println!("  outline: an empty outputs record does not read back");
        return;
    };
    let mut by_name = ByName::new(rules, at, ScriptCx { inner: Bare { env: script.env.clone() }, out: none }, Alloc::global());
    let mut quiet = Made(0, false);
    let stats = lower_file(&text, edition, &mut cfg, &mut blaze_lower::NoModule, &mut by_name, &mut quiet, Alloc::global());
    println!("  control without the outputs: {} items, {} made by expansions", stats.items, quiet.0);
}

/// A unit's cfg set: its atoms, its features and its script's cfgs.
struct UnitCfg {
    atoms: Vec<(String, Option<String>)>,
}

impl Cfg for UnitCfg {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        if self.atoms.iter().any(|(n, v)| n == name && v.as_deref() == value) { CfgAnswer::Yes } else { CfgAnswer::No }
    }
}

/// A host with the package's cargo variables and nothing else.
struct Bare {
    env: Vec<(String, String)>,
}

impl ExpandCx for Bare {
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        Ok(self.env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()))
    }

    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Err(HostError { ask: HostAsk::Cfg, why: HostWhy::Refused })
    }

    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }

    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(match what {
            PlaceOf::Line | PlaceOf::Column => Place::Number(1),
            PlaceOf::File | PlaceOf::ModulePath => Place::Text(""),
        })
    }

    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused })
    }
}

/// Counts each item an expansion made, printing its first line when `.1`.
struct Made(u64, bool);

impl Sink for Made {
    fn refused(&mut self, item: &ItemRef<'_>, refusal: blaze_lower::Refusal<'_>) {
        println!("    refused: {} on {}: {}", refusal.path, item.item_text().lines().next().unwrap_or(""), refusal.bound);
    }

    fn record(&mut self, item: &ItemRef<'_>, what: What, _: blaze_ids::ContentId, _: Outcome<'_, '_>) {
        if matches!(item.source, Source::Expansion { .. }) && matches!(what, What::Signature) {
            self.0 += 1;
            if self.1 && self.0 <= 3 {
                let first = item.item_text().lines().next().unwrap_or("");
                println!("    made: {}", first.chars().take(100).collect::<String>());
            }
        }
    }
}
