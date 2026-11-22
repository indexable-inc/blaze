//! The unit graph of a real workspace: `units <workspace dir> <store folder> [--tests] [--list]`.
//!
//! Reads the workspace's `Cargo.lock` with blaze-manifest's `lock`, the members' manifests from
//! the checkout through `Layout`, and every other package's manifest from its sources' record
//! in blaze-fetch's store folder (`records/<key>`, mapped and read in place through
//! `StoreSources`); never from cargo's caches. A package whose record is absent stops the run
//! with "source not fetched", naming it. Prints the graph's size, each member's features, the
//! ten packages with the most features, every proc-macro unit with its features and every unit
//! with a build script (`--list`), the time of each stage and the peak footprint.
//!
//! The platform is `aarch64-apple-darwin` for both target and host, its cfgs as `rustc --print
//! cfg` lists them for that target (a value the driver passes; nothing here asks a compiler).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use blaze_fetch::{PackView, SourceId};
use blaze_manifest::layout::{Entry, Layout, Need, Pass as LayoutPass};
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{Alloc, Vec as AVec};
use blaze_unit_graph::record::Source;
use blaze_unit_graph::{Ask, CfgAtom, Package, Pass, Records, Resolution, Resolver, StoreSources, TargetDesc, UnitKind};

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
    TargetDesc::new("aarch64-apple-darwin", cfg, alloc)
}

/// Runs a `Layout` anchored at `anchor` from `start` to its end; the members it read.
fn layout(anchor: &Path, start: &str, source: &mut Source, alloc: Alloc<'static>) -> Result<blaze_manifest::layout::Units<'static>, String> {
    let mut reader = Layout::new(start, alloc).map_err(|e| e.to_string())?;
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
                    source.manifest(path, text.as_deref());
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
                    source.listing(dir, entries.as_deref());
                    reader.listing(dir, entries.as_deref());
                }
            }
        }
    }
    Err("no layout after 64 passes".to_string())
}

/// A file mapped read-only and private, for the rest of the run (the example never unmaps).
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
    mapped: u64,
}

impl Records<'static> for Folder {
    fn record(&mut self, source: &SourceId<'_>) -> Option<&'static [u8]> {
        let key = source.key().0;
        let mut name = String::with_capacity(64);
        for b in key {
            name.push_str(&format!("{b:02x}"));
        }
        let path = self.records.join(name);
        match map(&path) {
            Ok(bytes) => {
                self.mapped += bytes.len() as u64;
                Some(bytes)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
}

fn main() {
    let heap = blaze_own::Heap::process();
    let args: Vec<String> = std::env::args().collect();
    let (Some(root), Some(store)) = (args.get(1).map(PathBuf::from), args.get(2).map(PathBuf::from)) else {
        eprintln!("usage: units <workspace dir> <blaze-fetch store folder> [--tests] [--list]");
        std::process::exit(2);
    };
    let tests = args.iter().any(|a| a == "--tests");
    let alloc = heap.alloc();

    let t0 = Instant::now();
    let bytes = std::fs::read(root.join("Cargo.lock")).unwrap_or_else(|e| panic!("{}: {e}", root.join("Cargo.lock").display()));
    let t_lock = Instant::now();
    let lock: &'static Lock<'static> = match Lock::read(&bytes, alloc) {
        Ok(lock) => Box::leak(Box::new(lock)),
        Err(e) => panic!("Cargo.lock: {e}"),
    };
    let lock_time = t_lock.elapsed();
    if args.iter().any(|a| a == "--lock-profile") {
        // Best of 5 each: the whole read, and the TOML parse alone.
        let text = std::str::from_utf8(&bytes).unwrap_or("");
        let best = |f: &dyn Fn()| {
            (0..5)
                .map(|_| {
                    let t = Instant::now();
                    f();
                    t.elapsed().as_secs_f64() * 1e3
                })
                .fold(f64::MAX, f64::min)
        };
        let whole = best(&|| drop(Lock::read(&bytes, alloc)));
        let toml = best(&|| drop(blaze_toml::Document::parse_in(text, blaze_toml::Limits::CARGO, alloc)));
        println!("lock profile  read {whole:.2} ms, of which the TOML parse {toml:.2} ms ({} bytes)", bytes.len());
    }
    let mut source = Source::new();
    source.lock(&bytes);
    let t_ws = Instant::now();
    let ws = layout(&root, "", &mut source, alloc).unwrap_or_else(|e| panic!("workspace layout: {e}"));
    let ws_time = t_ws.elapsed();
    let written = ws.resolver.as_ref().map(|(r, _)| r.as_str());
    let resolver = Resolver::of(written, ws.root_edition).unwrap_or_else(|_| panic!("unknown resolver {written:?}"));

    let mut roots = AVec::new_in(alloc);
    let mut members = Vec::new();
    for m in ws.members.iter() {
        let id = lock.named(&m.name).find(|id| matches!(lock.package(*id).source, LockSource::Path));
        match id {
            Some(id) => {
                roots.push(id);
                members.push((id, Package::from_member(m, alloc)));
            }
            None => println!("member {} is not in Cargo.lock as a path package", m.name.as_str()),
        }
    }
    let ask: &'static Ask<'static> =
        Box::leak(Box::new(Ask { roots, features: AVec::new_in(alloc), all_features: false, no_default_features: false, tests }));
    let target: &'static TargetDesc<'static> = Box::leak(Box::new(mac(alloc)));
    source.platform("target", target).platform("host", target).ask(ask);
    let mut res = Resolution::new(lock, target, target, ask, resolver, alloc);
    for (id, p) in members {
        res.member(id, p).unwrap_or_else(|e| panic!("{e}"));
    }

    let t_plan = Instant::now();
    let folder = Folder { records: store.join("records"), mapped: 0 };
    // Records this machine's fetch put: their digests were checked then.
    let mut sources = StoreSources::new(lock, folder, PackView::open_source_put, alloc).unwrap_or_else(|e| panic!("{e}"));
    let mut read_time = t_plan.elapsed();
    let mut solve_time = std::time::Duration::ZERO;
    let mut read_packages = 0u32;
    let mut passes = 0;
    let graph = loop {
        passes += 1;
        assert!(passes <= 64, "no graph after 64 passes");
        let t = Instant::now();
        let pass = res.pass().unwrap_or_else(|e| panic!("{e}"));
        solve_time += t.elapsed();
        match pass {
            Pass::Done(graph) => break graph,
            Pass::Partial(partial) => panic!("no unit graph: the manifests of {:?} are missing", partial.missing()),
            Pass::Need(needs) => {
                let t = Instant::now();
                for id in needs.iter() {
                    // A source the store lacks stops the run, naming it: never guessed.
                    let facts = sources.package(*id).unwrap_or_else(|e| panic!("{e}"));
                    read_packages += 1;
                    res.package(*id, Some(facts)).unwrap_or_else(|e| panic!("{e}"));
                }
                read_time += t.elapsed();
            }
        }
    };
    let t_enc = Instant::now();
    let record = blaze_unit_graph::record::encode(&graph, target, target, alloc).unwrap_or_else(|e| panic!("{e}"));
    let enc_time = t_enc.elapsed();
    let total = t0.elapsed();
    let t_dec = Instant::now();
    let back = blaze_unit_graph::record::decode(&record, alloc).unwrap_or_else(|e| panic!("{e}"));
    let dec_time = t_dec.elapsed();
    assert!(blaze_unit_graph::record::encode(&back.graph, &back.target, &back.host, alloc).ok() == Some(record.clone()), "the record does not round-trip");
    let hex = |b: &[u8]| b.iter().map(|b| format!("{b:02x}")).collect::<String>();

    let units = &graph.units;
    let mut packages: Vec<LockId> = units.iter().map(|u| u.package).collect();
    packages.sort();
    packages.dedup();
    let procs = units.iter().filter(|u| u.kind == UnitKind::ProcMacro).count();
    let builds = units.iter().filter(|u| u.kind == UnitKind::BuildScript).count();
    let host = units.iter().filter(|u| u.for_host).count();
    println!("workspace     {}", root.display());
    println!("resolver      {:?} (written: {written:?})", graph.resolver);
    println!("lock          {} packages, format {:?}", lock.packages.len(), lock.format.0);
    println!("members       {}", ws.members.len());
    let of = |git: bool| packages.iter().filter(|id| matches!(lock.package(**id).source, LockSource::Git { .. }) == git).count();
    let path = packages.iter().filter(|id| matches!(lock.package(**id).source, LockSource::Path)).count();
    println!("packages      {} in the graph ({} git, {} path, {} registry)", packages.len(), of(true), path, of(false) - path);
    println!("units         {} ({host} for the host)", units.len());
    println!("proc macros   {procs} units");
    println!("build scripts {builds} units");
    // `files`: each store package's directory in its record holds its manifest.
    let t_files = Instant::now();
    let mut located = 0usize;
    for id in packages.iter().filter(|id| !matches!(lock.package(**id).source, LockSource::Path)) {
        let (view, dir) = sources.files(*id).unwrap_or_else(|e| panic!("{e}"));
        let manifest = match dir.as_str() {
            "" => "Cargo.toml".to_owned(),
            dir => format!("{dir}/Cargo.toml"),
        };
        assert!(view.find(&manifest).is_some(), "{}: no `{manifest}` in its record", lock.package(*id).name);
        located += 1;
    }
    println!("files         {located} store packages located, each directory's Cargo.toml present ({:.1} ms)", t_files.elapsed().as_secs_f64() * 1e3);
    println!("passes        {passes}");
    println!(
        "sources       {read_packages} dependency packages read from {:.1} MiB of records mapped in place",
        sources.records().mapped as f64 / (1 << 20) as f64
    );
    println!(
        "time          {:.1} ms total: lock {:.1} ms, workspace layout {:.1} ms, dependency manifests {:.1} ms, resolution {:.1} ms",
        total.as_secs_f64() * 1e3,
        lock_time.as_secs_f64() * 1e3,
        ws_time.as_secs_f64() * 1e3,
        read_time.as_secs_f64() * 1e3,
        solve_time.as_secs_f64() * 1e3
    );
    println!(
        "record        {} bytes, digest {} (encoded in {:.2} ms, decoded in {:.2} ms, round-trips); source {}",
        record.len(),
        &hex(&blaze_ids::ContentId::of("blaze-unit-graph example: a record's digest", &record).0)[..16],
        enc_time.as_secs_f64() * 1e3,
        dec_time.as_secs_f64() * 1e3,
        &hex(&source.finish().0)[..16]
    );
    println!("memory        lifetime peak phys_footprint {:.1} MiB", max_footprint() as f64 / (1 << 20) as f64);

    println!("\nmember features (target lib, or first unit):");
    for m in ws.members.iter() {
        let Some(u) = units.iter().find(|u| u.name.as_str() == m.name.as_str() && !u.for_host).or_else(|| units.iter().find(|u| u.name.as_str() == m.name.as_str()))
        else {
            continue;
        };
        let fs: Vec<&str> = u.features.iter().map(|f| f.as_str()).collect();
        println!("  {:<40} [{}]", m.name.as_str(), fs.join(", "));
    }

    println!("\ntop 10 packages by features enabled (any unit):");
    let mut by: BTreeMap<String, usize> = BTreeMap::new();
    for u in units.iter() {
        let e = by.entry(format!("{} {}", u.name, u.version)).or_default();
        *e = (*e).max(u.features.len());
    }
    let mut ranked: Vec<(String, usize)> = by.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (name, n) in ranked.iter().take(10) {
        println!("  {n:>3}  {name}");
    }

    if args.iter().any(|a| a == "--list") {
        // Proc macros are built for the host: their features are the host side's.
        println!("\nproc-macro units (unit id, package, version, host features, crate name, root, deps):");
        for (at, u) in units.iter().enumerate().filter(|(_, u)| u.kind == UnitKind::ProcMacro) {
            let fs: Vec<&str> = u.features.iter().map(|f| f.as_str()).collect();
            let deps: Vec<String> = u.deps.iter().map(|d| format!("{}=u{}", d.name, d.unit.0)).collect();
            println!("  proc-macro u{at} {} {} [{}] {} {} deps [{}]", u.name, u.version, fs.join(","), u.crate_name, u.root, deps.join(","));
        }
        // A build script serves each unit naming it: its `CARGO_FEATURE_*` and `CARGO_CFG_*` are
        // that unit's features and cfg (its side's platform), `TARGET` that side's triple.
        println!("\nbuild-script units (script unit, package, version, root, links; served unit, its side, its features):");
        for (at, u) in units.iter().enumerate().filter(|(_, u)| u.kind == UnitKind::BuildScript) {
            let p = lock.package(u.package);
            let links = u.links.as_deref().unwrap_or("-");
            for (served_at, s) in units.iter().enumerate().filter(|(_, s)| s.build_script == Some(blaze_unit_graph::UnitId(at as u32))) {
                let side = if s.for_host { "host" } else { "target" };
                let fs: Vec<&str> = s.features.iter().map(|f| f.as_str()).collect();
                println!(
                    "  build-script u{at} {} {} {} links={links} serves u{served_at} {:?} {side} [{}] ({})",
                    u.name,
                    u.version,
                    u.root,
                    s.kind,
                    fs.join(","),
                    p.source_text
                );
            }
        }
    }
}
