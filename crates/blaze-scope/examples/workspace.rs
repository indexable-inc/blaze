//! A cargo workspace's names: `workspace <workspace dir> <blaze-fetch store folder>`.
//!
//! The IO around `blaze_scope::units::unit_crates` and `resolve_crates`:
//! - the unit graph as blaze-unit-graph's `units` example builds it: the lock, the members'
//!   manifests through `Layout`, every other package's manifest from the store folder's records
//!   (`StoreSources`), the platform `aarch64-apple-darwin` for target and host;
//! - the installed toolchain (`$HOME/.blaze/v2/toolchain`, the pinned id) and its packs;
//! - `SourceTexts`: a member's files from disk, a registry or git package's from its record
//!   (`StoreSources::files`), a toolchain crate's from its pack;
//! - one `resolve_crates` on one thread (`Serial`).
//!
//! Printed: the crates, the time of each phase, modules, items, and what does not resolve,
//! counted from each finished module's scope errors (`ScopeError::Import` and `::Macro`, the
//! solver's own verdict), its ten largest causes with one example each, the crates the
//! unresolved paths point into, and the peak footprint. A measurement (rule 82), not a test.

use std::collections::BTreeMap;
use std::{path::{Path, PathBuf}, process::ExitCode};
use std::time::Instant;

use blaze_fetch::{PackView, SourceId};
use blaze_manifest::layout::{Entry, Layout, Need, Pass as LayoutPass};
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{AVec, Alloc};
use blaze_scope::resolve::{CrateNames, SourceTexts, resolve_crates};
use blaze_scope::task::{Loc, Serial};
use blaze_scope::units::{Member, Origin, unit_crates};
use blaze_surface::{BuiltinDerive, CallShape, PathKind, ScopeError};
use blaze_toolchain::{Shelf, ToolchainView};
use blaze_unit_graph::{Ask, CfgAtom, Package, Pass, Records, Resolution, Resolver, StoreSources, TargetDesc};

const TRIPLE: &str = "aarch64-apple-darwin";

/// `rustc --print cfg` for the target, as blaze-unit-graph's `units` example passes it.
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
    TargetDesc::new(TRIPLE, cfg, alloc)
}

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

fn leak_str(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

/// Runs a `Layout` anchored at `anchor` to its end: the members it read.
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

/// A file mapped read-only and private, for the rest of the run (never unmapped).
fn map(path: &Path) -> std::io::Result<&'static [u8]> {
    use std::os::fd::AsRawFd as _;
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(&[]);
    }
    // SAFETY: a read-only private mapping of `len` bytes of an open file, which the store only
    // ever replaces by a rename (the bytes under it never change); never unmapped, so the
    // slice lives as long as the process.
    let ptr = unsafe { mmap(core::ptr::null_mut(), len, 1, 2, file.as_raw_fd(), 0) };
    if ptr as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: mapped above, `len` bytes, readable for the rest of the process.
    Ok(unsafe { core::slice::from_raw_parts(ptr, len) })
}

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
}

/// The lifetime peak `phys_footprint` (`rusage_info_v4`'s `ri_lifetime_max_phys_footprint`).
#[cfg(target_os = "macos")]
fn max_footprint() -> u64 {
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
    }
    let mut info = [0u64; 40];
    // SAFETY: flavor 4 writes one rusage_info_v4 (296 bytes), which the buffer outsizes.
    let ok = unsafe { proc_pid_rusage(std::process::id() as i32, 4, info.as_mut_ptr()) };
    if ok != 0 {
        panic!("proc_pid_rusage of this process: {}", std::io::Error::last_os_error());
    }
    info[2 + 28]
}

/// The lifetime peak resident set, in bytes: `getrusage(RUSAGE_SELF)`'s `ru_maxrss`, which
/// Linux counts in kibibytes (as `blaze_core::process` reads a child's).
#[cfg(target_os = "linux")]
fn max_footprint() -> u64 {
    /// `struct rusage` on 64-bit Linux: two 16-byte `timeval`s, then fourteen `long`s,
    /// `ru_maxrss` first.
    #[repr(C)]
    struct Rusage {
        _times: [i64; 4],
        maxrss: i64,
        _rest: [i64; 13],
    }
    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    }
    let mut usage = Rusage { _times: [0; 4], maxrss: 0, _rest: [0; 13] };
    // SAFETY: `Rusage` has `struct rusage`'s layout; `RUSAGE_SELF` is 0. The call writes it
    // and does not keep it.
    if unsafe { getrusage(0, &mut usage) } != 0 {
        panic!("getrusage of this process: {}", std::io::Error::last_os_error());
    }
    usage.maxrss as u64 * 1024
}

/// blaze-fetch's store folder: one file a record, `records/<hex of the key>`.
struct Folder {
    records: PathBuf,
}

impl Records<'static> for Folder {
    fn record(&mut self, source: &SourceId<'_>) -> Option<&'static [u8]> {
        let name: String = source.key().0.iter().map(|b| format!("{b:02x}")).collect();
        let path = self.records.join(name);
        match map(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
}

/// The installed toolchain's files: `<home>/.blaze/v2/toolchain/<64 hex digits>`.
struct DirShelf {
    place: String,
}

impl Shelf for DirShelf {
    type Error = std::io::Error;
    type Bytes = Vec<u8>;

    fn place(&self) -> &str {
        &self.place
    }

    fn size(&mut self, name: &str) -> Result<Option<u64>, std::io::Error> {
        match std::fs::metadata(format!("{}/{name}", self.place)) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read(&mut self, name: &str, _size: u64) -> Result<Vec<u8>, std::io::Error> {
        std::fs::read(format!("{}/{name}", self.place))
    }
}

/// Where one crate's files are read from.
enum Place {
    /// A member's folder on disk.
    Disk(PathBuf),
    /// A package's record (its folder in it, `""` or ending in `/`), or a toolchain pack.
    Pack(PackView<'static>, String),
}

/// Each crate's path prefix (its `shown_from`) and where its files are.
struct Texts {
    places: Vec<(String, Place)>,
}

impl<'a> SourceTexts<'a> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        let (prefix, place) = self.places.get(krate)?;
        let rest = path.strip_prefix(prefix.as_str())?;
        match place {
            Place::Disk(dir) => std::fs::read_to_string(dir.join(rest)).ok().map(leak_str),
            Place::Pack(view, dir) => {
                let file = view.find(&format!("{dir}{rest}"))?;
                core::str::from_utf8(file.bytes).ok()
            }
        }
    }
}

fn add(map: &mut BTreeMap<String, (usize, String)>, cause: String, example: impl FnOnce() -> String) {
    let entry = map.entry(cause).or_insert_with(|| (0, example()));
    entry.0 += 1;
}

fn at(c: &CrateNames<'_>, loc: Loc) -> String {
    c.line_of(loc).map_or_else(|| format!("file {} (not read)", loc.file), |(file, line)| format!("{}:{line}", file.shown))
}

fn show_path(kind: PathKind, segments: &[&str]) -> String {
    let start = match kind {
        PathKind::Plain => String::new(),
        PathKind::Crate => "crate::".to_string(),
        PathKind::SelfMod => "self::".to_string(),
        PathKind::Super(n) => "super::".repeat(usize::from(n)),
        PathKind::Abs => "::".to_string(),
        PathKind::DollarCrate(_) => "$crate::".to_string(),
    };
    format!("{start}{}", segments.join("::"))
}

fn print_top(title: &str, map: &BTreeMap<String, (usize, String)>, n: usize) {
    let mut rows: Vec<_> = map.iter().collect();
    rows.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
    println!("{title} ({} kinds):", rows.len());
    for (cause, (count, example)) in rows.iter().take(n) {
        println!("  {count:>6}  {cause}");
        if !example.is_empty() {
            println!("          e.g. {example}");
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (Some(root), Some(store)) = (args.get(1).map(PathBuf::from), args.get(2).map(PathBuf::from)) else {
        eprintln!("usage: workspace <workspace dir> <blaze-fetch store folder>");
        std::process::exit(2);
    };
    let alloc = Alloc::global();

    // Phase 1: the unit graph.
    let t_graph = Instant::now();
    let bytes: &'static [u8] = Box::leak(std::fs::read(root.join("Cargo.lock")).unwrap_or_else(|e| panic!("{}: {e}", root.join("Cargo.lock").display())).into_boxed_slice());
    let lock: &'static Lock<'static> = leak(Lock::read(bytes, alloc).unwrap_or_else(|e| panic!("Cargo.lock: {e}")));
    let ws = layout(&root, alloc).unwrap_or_else(|e| panic!("workspace layout: {e}"));
    let written = ws.resolver.as_ref().map(|(r, _)| r.as_str());
    let resolver = Resolver::of(written, ws.root_edition).unwrap_or_else(|_| panic!("unknown resolver {written:?}"));
    let mut roots = AVec::new_in(alloc);
    let mut member_ids: Vec<LockId> = Vec::new();
    let mut members: Vec<Member<'static>> = Vec::new();
    let mut member_dirs: BTreeMap<u32, PathBuf> = BTreeMap::new();
    let mut facts = Vec::new();
    for m in ws.members.iter() {
        let Some(id) = lock.named(&m.name).find(|id| matches!(lock.package(*id).source, LockSource::Path)) else {
            println!("member {} is not in Cargo.lock as a path package", m.name.as_str());
            continue;
        };
        roots.push(id);
        member_ids.push(id);
        let dir: &'static str = Box::leak(m.dir.as_str().to_owned().into_boxed_str());
        members.push(Member::new(id, dir).unwrap_or_else(|e| panic!("{e}")));
        member_dirs.insert(id.0, root.join(m.dir.as_str()));
        facts.push((id, Package::from_member(m, alloc)));
    }
    let ask: &'static Ask<'static> = leak(Ask { roots, features: AVec::new_in(alloc), all_features: false, no_default_features: false, tests: false });
    let target: &'static TargetDesc<'static> = leak(mac(alloc));
    let mut res = Resolution::new(lock, target, target, ask, resolver, alloc);
    for (id, p) in facts {
        res.member(id, p).unwrap_or_else(|e| panic!("{e}"));
    }
    let folder = Folder { records: store.join("records") };
    let mut sources = StoreSources::new(lock, folder, PackView::open_source_put, alloc).unwrap_or_else(|e| panic!("{e}"));
    let mut passes = 0;
    let graph = loop {
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
    };
    let graph = leak(graph);
    let graph_ms = t_graph.elapsed().as_secs_f64() * 1e3;
    println!("workspace     {}", root.display());
    println!("unit graph    {} units, {} members, {passes} passes", graph.units.len(), member_ids.len());

    // Phase 2: the toolchain and the crate list.
    let t_tc = Instant::now();
    let home = std::env::var("HOME").unwrap_or_else(|_| panic!("HOME is not set"));
    let mut shelf = DirShelf { place: format!("{home}/.blaze/v2/toolchain") };
    let id = blaze_toolchain::PINNED;
    let manifest: &'static [u8] = Box::leak(
        blaze_toolchain::shelf::open_manifest(&mut shelf, id).unwrap_or_else(|e| panic!("the toolchain at {}: {e}", shelf.place)).into_boxed_slice(),
    );
    let view = ToolchainView::open(manifest, id).unwrap_or_else(|e| panic!("the toolchain manifest: {e}"));
    let crates = leak(unit_crates(graph, lock, &members, view, TRIPLE, alloc).unwrap_or_else(|e| {
        eprintln!("the workspace's crates: {e}");
        std::process::exit(2);
    }));
    let mut places = Vec::with_capacity(crates.crates.len());
    let mut packs: BTreeMap<u32, PackView<'static>> = BTreeMap::new();
    for c in crates.crates.iter() {
        let place = match c.origin {
            Origin::Toolchain { krate, .. } => {
                let view = *packs.entry(krate).or_insert_with(|| {
                    let row = view.krate(krate);
                    let bytes: &'static [u8] =
                        Box::leak(blaze_toolchain::shelf::open_pack(&mut shelf, id, &row).unwrap_or_else(|e| panic!("{e}")).into_boxed_slice());
                    PackView::open_verified(bytes).unwrap_or_else(|_| panic!("the toolchain's pack of `{}` is no pack", row.name))
                });
                Place::Pack(view, String::new())
            }
            Origin::Unit(u) => {
                let package = graph.units[u.index()].package;
                match member_dirs.get(&package.0) {
                    Some(dir) => Place::Disk(dir.clone()),
                    None => {
                        let (view, dir) = sources.files(package).unwrap_or_else(|e| panic!("{e}"));
                        let dir = if dir.is_empty() { String::new() } else { format!("{}/", dir.as_str()) };
                        Place::Pack(view, dir)
                    }
                }
            }
        };
        places.push((c.shown_from.as_str().to_string(), place));
    }
    let texts = Texts { places };
    // A refused crate (a member proc macro the shelf has no wasm32-wasip1 side for) is not
    // resolved with no dependencies: the run stops, naming it (rule 3).
    let inputs = match crates.checked_inputs() {
        Ok(inputs) => inputs,
        Err(refused) => {
            eprintln!("the workspace's crates: {}", refused.why);
            return ExitCode::from(2);
        }
    };
    let tc_ms = t_tc.elapsed().as_secs_f64() * 1e3;
    let procs = crates.crates.iter().filter(|c| c.implicit.proc_macro).count();
    println!(
        "crates        {} ({} toolchain, {} workspace, of which {procs} proc macros); {} with a build script whose cfg is not joined",
        crates.crates.len(),
        crates.toolchain,
        crates.crates.len() - crates.toolchain,
        crates.build_scripts
    );

    // Phase 3: the names.
    let Ok(frame) = blaze_mem::Frame::new(blaze_mem::Tag::Nameres) else {
        return ExitCode::from(2);
    };
    let erased = blaze_own::Erased::new(&frame);
    let t_res = Instant::now();
    let names = resolve_crates(&inputs, crates.sysroot, &texts, &Serial, erased.bump()).unwrap_or_else(|e| {
        eprintln!("the names: {e}");
        std::process::exit(2);
    });
    let res_ms = t_res.elapsed().as_secs_f64() * 1e3;
    println!("time          graph {graph_ms:.0} ms, toolchain {tc_ms:.0} ms, resolve {res_ms:.0} ms (1 thread)");

    // What it came to, from each finished module's scope errors.
    let (mut modules, mut items, mut imports_bad, mut macros_bad, mut failed, mut notes, mut externs_bad) = (0, 0, 0, 0, 0, 0, 0);
    let mut ws_imports_bad = 0;
    let mut ws_macros_bad = 0;
    let mut causes: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut into: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut fails: BTreeMap<String, (usize, String)> = BTreeMap::new();
    // A proc macro's host dependencies (syn, quote, proc_macro2, proc_macro) are not in the graph
    // by design: what its paths name there is counted apart, out of the tables below.
    let (mut pm_imports, mut pm_macros) = (0, 0);
    let mut pm_causes: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut pm_into: BTreeMap<String, (usize, String)> = BTreeMap::new();
    for (k, c) in names.crates().iter().enumerate() {
        let workspace = k >= crates.toolchain;
        let pm = c.implicit.proc_macro;
        let (causes, into) = if pm { (&mut pm_causes, &mut pm_into) } else { (&mut causes, &mut into) };
        modules += names.modules(k).count();
        items += names.items(k).count();
        failed += c.failed.len();
        notes += c.notes.len();
        let deps = &crates.crates[k].deps;
        // The crate a path written in `c` points into, by its first segment.
        let target = |kind: PathKind, first: &str| -> String {
            match kind {
                PathKind::Crate | PathKind::SelfMod | PathKind::Super(_) => format!("{} (itself)", c.name),
                PathKind::DollarCrate(_) => "$crate (a macro's crate)".to_string(),
                PathKind::Plain | PathKind::Abs => match deps.iter().find(|(n, _)| *n == first) {
                    Some((_, d)) => crates.crates[*d].name.to_string(),
                    None if matches!(first, "std" | "core" | "alloc" | "proc_macro") => format!("{first} (toolchain)"),
                    None if kind == PathKind::Abs => format!("`::{first}`: no such crate"),
                    None => format!("{} (itself, or a name not found)", c.name),
                },
            }
        };
        let use_loc = |fragment, index: u32| c.tables.use_locs.get(&fragment).and_then(|locs| locs.get(index as usize).copied());
        for r in c.done.values() {
            let mpath = c.path_of(r.scope.module).unwrap_or_default().to_string();
            for error in r.scope.errors.iter() {
                match error {
                    ScopeError::Import { site, path, segment, why } => {
                        imports_bad += 1;
                        ws_imports_bad += usize::from(workspace);
                        let segs: Vec<&str> = path.segments.iter().copied().collect();
                        let seg = segs.get(*segment as usize).copied().unwrap_or("");
                        let first = segs.first().copied().unwrap_or("");
                        let cause = if *segment == 0 { format!("import {why:?}: first segment `{seg}`") } else { format!("import {why:?}: `{seg}` in `{first}::..`") };
                        let fragment = site.expansion.unwrap_or(r.scope.module);
                        let example = || {
                            let place = use_loc(fragment, site.index).map_or_else(|| mpath.clone(), |loc| at(c, loc));
                            format!("{}: {place}: use {}", c.name, show_path(path.kind, &segs))
                        };
                        pm_imports += usize::from(pm);
                        add(causes, cause, example);
                        add(into, format!("import into {}", target(path.kind, first)), || format!("{}: {}", c.name, show_path(path.kind, &segs)));
                    }
                    ScopeError::Macro { call, path, why, .. } => {
                        macros_bad += 1;
                        ws_macros_bad += usize::from(workspace);
                        let segs: Vec<&str> = path.segments.iter().copied().collect();
                        let last = segs.last().copied().unwrap_or("");
                        let first = segs.first().copied().unwrap_or("");
                        let src = c.tables.calls.get(call);
                        let cause = if src.map(|it| it.kind) == Some(CallShape::Derive) && !BuiltinDerive::ALL.into_iter().any(|it| it.name() == last) {
                            format!("needs a procedural macro: derive `{last}`")
                        } else {
                            format!("macro {why:?}: `{last}`")
                        };
                        let example = || {
                            let place = src.map_or_else(|| mpath.clone(), |src| at(c, src.loc));
                            format!("{}: {place}: {}!", c.name, show_path(path.kind, &segs))
                        };
                        pm_macros += usize::from(pm);
                        add(causes, cause, example);
                        let whence = if segs.len() == 1 { format!("{} (a bare name)", c.name) } else { target(path.kind, first) };
                        add(into, format!("macro path into {whence}"), || format!("{}: {}", c.name, show_path(path.kind, &segs)));
                    }
                    ScopeError::ExternCrate { name } => {
                        externs_bad += 1;
                        add(causes, format!("extern crate `{name}` not found"), || format!("{}: {mpath}", c.name));
                    }
                    _ => {}
                }
            }
        }
        for (module, error) in c.failed.iter() {
            let path = c.path_of(*module).unwrap_or_default().to_string();
            let why = error.to_string();
            let kind = why.split(':').next().unwrap_or("").to_string();
            add(&mut fails, format!("resolver refused: {kind}"), || format!("{}: {path}: {why}", c.name));
        }
    }
    println!("modules       {modules}");
    println!("items         {items}");
    println!("unresolved    {imports_bad} imports ({ws_imports_bad} in workspace units), {macros_bad} macro paths ({ws_macros_bad} in workspace units), {externs_bad} extern crates");
    println!("              counted from each finished module's ModuleScope errors (ScopeError::Import, ::Macro, ::ExternCrate)");
    println!(
        "              of these, in proc-macro crates (their host deps not in the graph by design): {pm_imports} imports, {pm_macros} macro paths; the rest: {} imports, {} macro paths",
        imports_bad - pm_imports,
        macros_bad - pm_macros
    );
    println!("failed        {failed} modules the resolver refused; {notes} notes");
    print_top("\nthe ten most common causes, outside proc-macro crates", &causes, 10);
    print_top("\nwhere the unresolved paths point (by first segment), outside proc-macro crates", &into, 20);
    print_top("\nin proc-macro crates, the five most common causes", &pm_causes, 5);
    if !fails.is_empty() {
        print_top("\nmodules refused", &fails, 5);
    }
    println!("\nmemory        lifetime peak phys_footprint {:.1} MiB", max_footprint() as f64 / (1 << 20) as f64);
    ExitCode::SUCCESS
}
