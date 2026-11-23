//! Names across crates (docs/v2/plan/design/bottom.md, section 6): resolves a graph of real
//! crates end to end through `blaze_scope::resolve::resolve_crates`, each crate after its
//! dependencies, a lookup that leaves a crate reading the other crate's finished `ModuleScope`
//! records (never resolving it again).
//!
//! `crates [--threads 1,18] [--examples N] [--show NAME] [--verbose] [--first prefetch|alone]
//! <set>...`, a set being one of:
//!
//! - `core`: library/core alone;
//! - `alloc`: library/alloc on core;
//! - `std`: library/std and what it is built from (library/'s crates and its vendored copies),
//!   on core and alloc;
//! - `bstd-deps`: the crates bstd's default build depends on (read through blaze-manifest's
//!   `Layout`), on core, alloc and std (most of them use std);
//! - `bstd`: those and bstd;
//! - `hello`: those and the owner's package (/Volumes/Projects/tmp/hello-world, read, not
//!   changed; its manifest's path to bstd is ignored: bstd is the blaze edition's `std`);
//!   `hello=<dir>`: the same with the package in `<dir>`.
//!
//! `crates --links-agree [--threads 1] <core|alloc|std|bstd-deps|bstd|hello|hello=DIR>...`:
//! each crate's records at module grain checked against its written scopes (D116 point 7):
//! one sorted line a crate, the total, then a `DISAGREES` line for each crate that does not
//! agree, and exit 1 if any does.
//!
//! This file is the adapter: it reads manifests from disk into each crate's `CrateInput`
//! (its features, dependencies and cfg), answers `SourceTexts` from disk, runs the library's
//! fill across threads, and prints. The names themselves (walk, lowering, records, the fill
//! and the expansions it asks for) are the library's.
//!
//! Printed per crate: its fill line and its scopes' digest, modules, declarations, imports and
//! how many do not resolve, macro paths unresolved, the ten most frequent causes; the total
//! time on each thread count; for `hello`, what each name of the owner's `main` resolves to and
//! the items of bstd, core and alloc reachable from them by names alone. A measurement and a
//! smoke run (rule 82), not a test.

#![feature(allocator_api)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Component, PathBuf};
use std::sync::{OnceLock, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::time::Instant;

use blaze_expand::Diagnostic;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{
    CrateInput, CrateNames, ExpandNote, Lexed, MAX_MADE_MODULES, MadeNote, Names, NamesNote, RecordError,
    SourceTexts, resolve_crates_with,
};
use blaze_scope::task::{First, ItemText, Loc, WalkNote};
use blaze_surface::{BuiltinDerive, CallShape, DeclsBuilder, Edition, Namespace, Path, PathKind, ScopeError};

/// Every name is a slice of a text leaked for the run: a source file or an expansion.
type N = &'static str;

/// The one allocator of the run: the global heap's handle, named at the program's top.
fn g() -> Alloc<'static> {
    Alloc::global()
}

/// A value that crosses threads although its type is not `Send` or `Sync` only because it
/// holds `Alloc` handles: the fill's task closures and what they return (`Outline`,
/// `DeclsBuilder`, `Resolved`), all made in [`g`].
///
/// Every `OnGlobal` is made around values whose every allocator handle is [`g`]'s, the
/// process's global heap, which any thread may allocate from and free to at once; `Alloc` is
/// neither `Send` nor `Sync` only because another handle may stand for a frame one thread owns
/// (blaze-own's `alloc_ref`). The types wrapped hold no `Rc`, `Cell` or other thread-bound
/// state (blaze-surface, blaze-scope, blaze-file-items and blaze-expand have none outside
/// their tests; what blaze-scope's resolution shares between tasks is behind `std::sync`
/// locks), so moving one to another thread, or reading one from several, is sound.
struct OnGlobal<T>(T);

// SAFETY: see `OnGlobal`: every allocator inside is the global heap's, and nothing else in
// the wrapped values is bound to a thread.
unsafe impl<T> Send for OnGlobal<T> {}
// SAFETY: as for `Send`; a shared read clones through the global heap's handle at most.
unsafe impl<T> Sync for OnGlobal<T> {}

impl<T> std::ops::Deref for OnGlobal<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}


fn main() {
    let usage = "usage: crates [--threads 1,18] [--examples N] [--show NAME] [--verbose] [--first prefetch|alone] [--d116-decide] [--links-agree] [--per-crate] <core|alloc|std|bstd-deps|bstd|hello|hello=DIR>...";
    let most = std::thread::available_parallelism().map_or(1, |it| it.get());
    let mut counts: Vec<usize> = vec![1];
    let mut sets: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--threads" => {
                let list = args.next().unwrap_or_default();
                counts = if list == "all" {
                    let mut c: Vec<usize> = [1usize, 2, 4, 8].into_iter().filter(|it| *it <= most).collect();
                    if !c.contains(&most) {
                        c.push(most);
                    }
                    c
                } else {
                    list.split(',').filter_map(|it| it.parse().ok()).collect()
                };
            }
            "--verbose" => VERBOSE.store(true, Ordering::Relaxed),
            "--d116-decide" => D116_DECIDE.store(true, Ordering::Relaxed),
            "--links-agree" => {
                LINKS_AGREE.store(true, Ordering::Relaxed);
                D116_DECIDE.store(true, Ordering::Relaxed);
            }
            "--per-crate" => PER_CRATE.store(true, Ordering::Relaxed),
            "--show" => {
                let _ = SHOW.set(args.next().unwrap_or_default());
            }
            "--first" => match args.next().as_deref() {
                Some("prefetch") => ALONE.store(false, Ordering::Relaxed),
                Some("alone") => ALONE.store(true, Ordering::Relaxed),
                other => {
                    eprintln!("--first takes prefetch or alone, not {other:?}");
                    std::process::exit(2);
                }
            },
            "--examples" => EXAMPLES.store(args.next().and_then(|it| it.parse().ok()).unwrap_or(3), Ordering::Relaxed),
            _ => sets.push(arg),
        }
    }
    if sets.is_empty() || counts.is_empty() {
        eprintln!("{usage}");
        std::process::exit(2);
    }
    for set in &sets {
        let (set, hello) = match set.split_once('=') {
            Some(("hello", dir)) => ("hello", PathBuf::from(dir)),
            Some(_) => {
                eprintln!("{usage}");
                std::process::exit(2);
            }
            None => (set.as_str(), PathBuf::from(HELLO)),
        };
        run_set(set, &hello, &counts);
    }
    println!("\npeak memory (getrusage max RSS): {:.1} MB", peak_rss_mb());
}

/// Examples kept per failure kind (`--examples N`).
static EXAMPLES: AtomicUsize = AtomicUsize::new(3);

/// `--d116-decide`: each crate's cells decided again on demand and compared (D116).
static D116_DECIDE: AtomicBool = AtomicBool::new(false);

/// `--links-agree`: each crate's records at module grain checked against its written scopes
/// (D116 point 7, `blaze_scope::links::Agreed`); implies `--d116-decide`, whose hook runs it.
static LINKS_AGREE: AtomicBool = AtomicBool::new(false);

/// What `--links-agree` gave each crate of the current run: its totals, or the first
/// disagreement as printed.
static LINKS: std::sync::Mutex<Vec<(String, Result<blaze_scope::links::Agreed, String>)>> = std::sync::Mutex::new(Vec::new());

/// The D116 check's clock: nanoseconds since its first call.
fn decide_clock() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}
/// The fill's first tasks one module each (`--first alone`), else from the prefetch.
static ALONE: AtomicBool = AtomicBool::new(false);
/// A name whose bindings in every scope are printed (`--show NAME`).
static SHOW: std::sync::OnceLock<String> = std::sync::OnceLock::new();
/// `--verbose`: each crate's own ten largest causes.
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// `--per-crate`: also resolve crate by crate through `CrateScope` records and compare.
static PER_CRATE: AtomicBool = AtomicBool::new(false);

/// Crate by crate through records (`crate_scope::resolve_by_records`) at each thread count:
/// every module scope equal to the whole-set call's, every record's bytes equal across thread
/// counts, the time, and the records' sizes.
fn per_crate(whole: &Names<'_>, inputs: &[CrateInput<'static>], sysroot: Sysroot, thread_counts: &[usize]) {
    let scopes = |c: &CrateNames<'_>| {
        let mut keys: Vec<_> = c.done.keys().copied().collect();
        keys.sort();
        keys.into_iter().map(|k| (k, format!("{:?}", c.done[&k].scope))).collect::<Vec<_>>()
    };
    let mut first: Option<Vec<Vec<u8>>> = None;
    for &threads in thread_counts {
        let start = Instant::now();
        let (all, needs) = blaze_scope::crate_scope::resolve_by_records(
            inputs,
            sysroot,
            &Disk,
            &Threads(threads),
            &blaze_scope::crate_scope::Ready { workers: threads, rank: None, wait: std::time::Duration::from_secs(600), hooks: &blaze_scope::crate_scope::NoHooks },
            blaze_own::testing::bump(),
        ).unwrap_or_else(|(at, e)| {
            eprintln!("per crate: crate `{}`: {e:?}", inputs[at].name);
            std::process::exit(2);
        });
        let ms = start.elapsed().as_secs_f64() * 1e3;
        let mut differ = 0;
        let mut modules = 0;
        for (i, c) in whole.crates().iter().enumerate() {
            let (w, p) = (scopes(c), scopes(all[i].names));
            modules += w.len();
            if w != p {
                differ += 1;
                let at = w.iter().zip(p.iter()).position(|(x, y)| x != y).unwrap_or(w.len().min(p.len()));
                println!("per crate: crate `{}` differs at module {at} of {}: whole {:?}\n  per {:?}", c.name, w.len(), w.get(at).map(|x| &x.1), p.get(at).map(|x| &x.1));
            }
        }
        let bytes: Vec<Vec<u8>> = all.iter().map(|c| c.record.to_vec()).collect();
        let total: usize = bytes.iter().map(Vec::len).sum();
        println!(
            "per crate ({threads} threads): {} crates in {ms:.1} ms, {needs} Need re-entries; {modules} module scopes, crates differing from the whole set: {differ}; records {total} bytes",
            all.len()
        );
        match &first {
            None => {
                let mut sizes: Vec<(usize, &str)> = bytes.iter().enumerate().map(|(i, b)| (b.len(), inputs[i].name)).collect();
                sizes.sort_by(|a, b| b.cmp(a));
                println!("per crate: ten largest records: {}", sizes.iter().take(10).map(|(n, c)| format!("{c} {n}")).collect::<Vec<_>>().join(", "));
                first = Some(bytes);
            }
            Some(before) => {
                let unequal: Vec<&str> = before.iter().zip(bytes.iter()).enumerate().filter(|(_, (a, b))| a != b).map(|(i, _)| inputs[i].name).collect();
                println!("per crate: records unequal to the first thread count's: {} {:?}", unequal.len(), unequal);
            }
        }
    }
}
/// The cause of what a crate that uses Rust's std names of it: std is not in the graph.
const NO_STD_IN_GRAPH: &str = "Rust's std is not in the graph";

fn example_cap() -> usize {
    EXAMPLES.load(Ordering::Relaxed)
}

/// `f` over `0..n` on `threads` threads, each taking the next index from one counter; the
/// results in index order.
fn par_map<T: Send>(threads: usize, n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let f = &f;
    let next = &next;
    let parts: Vec<Vec<(usize, T)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(move || {
                    let mut mine = Vec::new();
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        if at >= n {
                            break;
                        }
                        mine.push((at, f(at)));
                    }
                    mine
                })
            })
            .collect();
        handles.into_iter().map(|it| it.join().expect("a worker thread panicked")).collect()
    });
    let mut all: Vec<Option<T>> = (0..n).map(|_| None).collect();
    for part in parts {
        for (at, value) in part {
            all[at] = Some(value);
        }
    }
    all.into_iter().map(|it| it.expect("every index is taken once")).collect()
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

#[repr(C)]
struct RUsage {
    utime: [i64; 2],
    stime: [i64; 2],
    maxrss: i64,
    rest: [i64; 13],
}

unsafe extern "C" {
    fn getrusage(who: i32, usage: *mut RUsage) -> i32;
}

/// The process's peak resident set (`getrusage(RUSAGE_SELF)`): bytes on macOS, KiB on Linux.
fn peak_rss_mb() -> f64 {
    let mut usage = RUsage { utime: [0; 2], stime: [0; 2], maxrss: 0, rest: [0; 13] };
    // SAFETY: `RUsage` has the layout of `struct rusage` on 64-bit macOS and Linux (two
    // 16-byte `timeval`s, then fourteen `long`s); `RUSAGE_SELF` is 0 on both.
    let status = unsafe { getrusage(0, &mut usage) };
    if status != 0 {
        return f64::NAN;
    }
    let bytes = if cfg!(target_os = "macos") { usage.maxrss } else { usage.maxrss * 1024 };
    bytes as f64 / 1e6
}

type Target = blaze_scope::task::Target<'static>;

// ---------------------------------------------------------------------------------------------
// Paths.

fn show_path(path: &Path<'_, N>) -> String {
    let start = match path.kind {
        PathKind::Plain => String::new(),
        PathKind::Crate => "crate::".to_string(),
        PathKind::SelfMod => "self::".to_string(),
        PathKind::Super(n) => "super::".repeat(usize::from(n)),
        PathKind::Abs => "::".to_string(),
        PathKind::DollarCrate(_) => "$crate::".to_string(),
    };
    format!("{start}{}", path.segments.iter().copied().collect::<Vec<_>>().join("::"))
}

// ---------------------------------------------------------------------------------------------
// The crate's input.

/// `path` with `.` and `..` taken out by text alone.
fn normalize(path: &std::path::Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The files of every crate, read from disk as the library asks for them (any thread), each
/// text leaked for the run.
struct Disk;

impl<'a> SourceTexts<'a> for Disk {
    fn read(&self, _krate: usize, path: &str) -> Option<&'a str> {
        std::fs::read_to_string(path).ok().map(leak)
    }
}

// ---------------------------------------------------------------------------------------------
// What a crate's names came to.

/// The digest of a crate's finished scopes: each module's `ModuleScope` record and read list,
/// in key order; and how many there are.
fn scopes_digest(c: &CrateNames<'static>) -> (ContentId, usize) {
    let mut modules: Vec<&ItemKey> = c.done.keys().collect();
    modules.sort_unstable();
    let mut bytes: Vec<u8> = Vec::new();
    for m in &modules {
        let r = &c.done[*m];
        bytes.extend_from_slice(&m.0.0);
        match r.scope.write(g()) {
            Ok(record) => {
                bytes.extend_from_slice(&(record.len() as u64).to_le_bytes());
                bytes.extend_from_slice(&record);
            }
            Err(error) => bytes.extend_from_slice(format!("refused: {error}").as_bytes()),
        }
        bytes.extend_from_slice(format!("{:?}", r.reads).as_bytes());
    }
    (ContentId::of("blaze crates example scopes digest 1", &bytes), modules.len())
}

fn hex8(id: ContentId) -> String {
    id.0[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// `loc` of `c` as `shown/path.rs:line`.
fn at(c: &CrateNames<'static>, loc: Loc) -> String {
    c.line_of(loc).map_or_else(|| format!("{}: file {} (not read)", c.name, loc.file), |(file, line)| format!("{}:{line}", file.shown))
}

/// Runs the fill's waves across `threads`.
struct Threads(usize);

impl blaze_scope::task::Par for Threads {
    fn decide_check(&self) -> Option<&dyn Fn() -> u64> {
        if D116_DECIDE.load(Ordering::Relaxed) { Some(&decide_clock as &dyn Fn() -> u64) } else { None }
    }

    fn decided(&self, krate: &str, check: &blaze_scope::DecideCheck<'_, &str>) {
        println!("d116 decide {krate}: {check}");
        if LINKS_AGREE.load(Ordering::Relaxed)
            && let Some(links) = check.links()
        {
            let got = match links {
                Ok(a) => Ok(*a),
                Err(d) => Err(d.to_string()),
            };
            LINKS.lock().expect("no thread panicked holding the `--links-agree` list").push((krate.to_string(), got));
        }
    }

    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        let f = OnGlobal(f);
        // Through `Deref`, so the closure takes the whole wrapper (which is `Sync`), not its field.
        for OnGlobal(it) in par_map(self.0, n, |i| OnGlobal((*f)(i))) {
            out.push(it);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The adapter: manifests from disk, into each crate's input.

const FRESH: &str = "/Volumes/Projects/blaze-fresh-1115";
const HELLO: &str = "/Volumes/Projects/tmp/hello-world";

/// The packages a set leaves out, and why: printed with the graph.
const LEFT_OUT: &[(&str, &str)] = &[
    ("blaze-http", "being moved behind a feature (the brief: leave it out)"),
    ("compiler_builtins", "symbols only; no path names it"),
];

/// A workspace's layout, its manifests and listings read from disk under `anchor`.
fn layout_of(anchor: &std::path::Path, start: &str) -> blaze_manifest::layout::Units<'static> {
    use blaze_manifest::layout::{Entry, Layout, Need, Pass};
    let mut layout = Layout::new(start, g()).unwrap_or_else(|e| panic!("{}: {e}", anchor.display()));
    for _ in 0..64 {
        let needs = match layout.pass() {
            Ok(Pass::Done(units)) => return units,
            Ok(Pass::Need(needs)) => needs,
            Err(e) => {
                eprintln!("the layout of {start} under {}: {e}", anchor.display());
                std::process::exit(2);
            }
        };
        for need in needs.iter() {
            match need {
                Need::Manifest(path) => {
                    let text = std::fs::read_to_string(anchor.join(path.as_str())).ok();
                    if let Err(e) = layout.manifest(path.as_str(), text.as_deref()) {
                        eprintln!("{}: {e}", anchor.join(path.as_str()).display());
                        std::process::exit(2);
                    }
                }
                Need::Listing(dir) => {
                    let mut names: Vec<(String, bool)> = Vec::new();
                    match std::fs::read_dir(anchor.join(dir.as_str())) {
                        Ok(entries) => {
                            for e in entries.flatten() {
                                names.push((e.file_name().to_string_lossy().into_owned(), e.path().is_dir()));
                            }
                            let entries: Vec<Entry<'_>> = names.iter().map(|(name, is_dir)| Entry { name, is_dir: *is_dir }).collect();
                            layout.listing(dir.as_str(), Some(&entries));
                        }
                        Err(_) => layout.listing(dir.as_str(), None),
                    }
                }
            }
        }
    }
    eprintln!("the layout of {start}: more than 64 passes");
    std::process::exit(2);
}

/// One package to resolve, as the adapter read it.
struct Package {
    /// Its library name (what a dependent calls it unless renamed), or the binary's.
    name: String,
    dir: PathBuf,
    root: PathBuf,
    edition: edition::PackageEdition,
    features: Vec<String>,
    /// (extern name, package index).
    deps: Vec<(String, usize)>,
}

/// The packages of a set, in the order of their crate ids, which of them are the sysroot's,
/// and what was left out.
struct Plan {
    packages: Vec<Package>,
    sysroot: Sysroot,
    notes: Vec<String>,
}

fn lib_name(name: &str) -> String {
    name.replace('-', "_")
}

/// The packages of `set` (`hello_dir`: the owner's package for `hello`): core and alloc from
/// library/, bstd's closure from blaze's workspace, the owner's package; features each
/// package's defaults and what its dependents ask, closed over its `[features]` table.
fn plan_of(set: &str, hello_dir: &std::path::Path, target: &Target) -> Plan {
    use blaze_manifest::layout::DepKind;
    let fresh = PathBuf::from(FRESH);
    let library = fresh.join("library");
    let mut packs = Packages::default();
    let mut notes = Vec::new();
    let core = packs.at(&library.join("core"));
    let mut roots = vec![core];
    let alloc = (set != "core").then(|| packs.at(&library.join("alloc")));
    roots.extend(alloc);
    let with_std = !matches!(set, "core" | "alloc");
    let std = with_std.then(|| packs.at(&library.join("std")));
    roots.extend(std);
    let bstd_dir = fresh.join("crates/blaze-std");
    let bstd = matches!(set, "bstd" | "hello").then(|| packs.at(&bstd_dir));
    roots.extend(bstd);
    if set == "bstd-deps" {
        // bstd's own dependencies, each with what bstd asks of it: the closure of bstd's
        // dependency lines without bstd.
        let b = packs.at(&bstd_dir);
        roots.push(b);
    }
    let hello = (set == "hello").then(|| packs.at(hello_dir));
    roots.extend(hello);
    // A dependency line's `cfg(..)` target, read under the target alone (a predicate that does
    // not read is false).
    let cfg = blaze_scope::task::Cfg::of(target, &[], g());
    // Features: (package, feature), closed over each package's `[features]` table, defaults
    // asked by the roots and by each dependency line that does not turn them off.
    let mut on: HashSet<(usize, String)> = HashSet::new();
    let mut defaults: HashSet<usize> = roots.iter().copied().collect();
    let mut reached: Vec<usize> = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    let mut stack = roots.clone();
    // Each enabled dependency: (dependent, extern name, dependency).
    let mut edges: BTreeSet<(usize, String, usize)> = BTreeSet::new();
    for _ in 0..64 {
        let before = (on.len(), defaults.len(), seen.len(), edges.len());
        while let Some(p) = stack.pop() {
            if seen.insert(p) {
                reached.push(p);
            }
        }
        for at in 0..reached.len() {
            let p = reached[at];
            let member = packs.member(p);
            let member_name = member.name.as_str().to_string();
            let table: Vec<(String, Vec<String>)> = member.features.iter().map(|f| (f.name.as_str().to_string(), f.enables.iter().map(|e| e.as_str().to_string()).collect())).collect();
            let dependencies: Vec<blaze_manifest::layout::Dependency<'static>> = member.dependencies.iter().cloned().collect();
            let mut asked: Vec<String> = on.iter().filter(|(q, _)| *q == p).map(|(_, f)| f.clone()).collect();
            if defaults.contains(&p) {
                asked.push("default".to_string());
            }
            while let Some(f) = asked.pop() {
                on.insert((p, f.clone()));
                let Some((_, enables)) = table.iter().find(|it| it.0 == f) else { continue };
                for e in enables {
                    let e = e.as_str();
                    if let Some(dep) = e.strip_prefix("dep:") {
                        on.insert((p, format!("dep:{dep}")));
                    } else if let Some((dep, feat)) = e.split_once('/') {
                        let weak = dep.ends_with('?');
                        let dep = dep.trim_end_matches('?');
                        if !weak {
                            on.insert((p, format!("dep:{dep}")));
                        }
                        on.insert((p, format!("feature-of:{dep}/{feat}")));
                    } else if !on.contains(&(p, e.to_string())) {
                        asked.push(e.to_string());
                    }
                }
            }
            for d in dependencies.iter() {
                // The owner's lines are not read: bstd is the blaze edition's std.
                if d.kind != DepKind::Normal || Some(p) == hello {
                    continue;
                }
                if let Some(t) = &d.target {
                    let pred = t.as_str().strip_prefix("cfg(").and_then(|it| it.strip_suffix(')'));
                    if !pred.is_some_and(|pred| cfg.eval(pred).unwrap_or(false)) {
                        continue;
                    }
                }
                let name = d.name.as_str();
                // An optional dependency is on by `dep:name`, or by a feature of its name.
                if d.optional && !on.contains(&(p, format!("dep:{name}"))) && !on.contains(&(p, name.to_string())) {
                    continue;
                }
                let package = d.package.as_str();
                if LEFT_OUT.iter().any(|(left, _)| *left == package || lib_name(left) == lib_name(package)) {
                    continue;
                }
                let Some(t) = packs.dep_of(p, &d.source, package, &library) else {
                    let note = format!("{member_name}: dependency `{name}` left out: not in blaze's workspace or library/ (third-party)");
                    if !notes.contains(&note) {
                        notes.push(note);
                    }
                    continue;
                };
                if d.default_features {
                    defaults.insert(t);
                }
                for f in d.features.iter() {
                    on.insert((t, f.as_str().to_string()));
                }
                let asked_of: Vec<String> = on
                    .iter()
                    .filter_map(|(q, f)| (*q == p).then_some(f).and_then(|f| f.strip_prefix("feature-of:")).and_then(|f| f.split_once('/')).filter(|(dn, _)| *dn == name).map(|(_, f)| f.to_string()))
                    .collect();
                for f in asked_of {
                    on.insert((t, f));
                }
                let renamed = name != package;
                let extern_name = if renamed { lib_name(name) } else { packs.lib_of(t) };
                edges.insert((p, extern_name, t));
                stack.push(t);
            }
        }
        if (on.len(), defaults.len(), seen.len(), edges.len()) == before && stack.is_empty() {
            break;
        }
    }
    // `bstd-deps`: the closure without bstd itself.
    if set == "bstd-deps" {
        let b = packs.at(&bstd_dir);
        reached.retain(|p| *p != b);
        edges.retain(|(p, _, _)| *p != b);
    }
    reached.sort_by_key(|p| packs.dir(*p).to_path_buf());
    // core, alloc, std, bstd first, in that order: their ids are fixed.
    let mut ordered: Vec<usize> = [Some(core), alloc, std, bstd].into_iter().flatten().collect();
    let rest: Vec<usize> = reached.iter().copied().filter(|p| !ordered.contains(p)).collect();
    ordered.extend(rest);
    let index: HashMap<usize, usize> = ordered.iter().enumerate().map(|(i, p)| (*p, i)).collect();
    let mut packages = Vec::new();
    for &p in &ordered {
        let member = packs.member(p);
        let (name, root) = match &member.lib {
            Some(lib) => (lib_name(lib.name.as_str()), packs.anchor_of(p).join(lib.root.as_str())),
            None => {
                let bin = member.bins.first().unwrap_or_else(|| panic!("{}: no lib and no binary", packs.dir(p).display()));
                (lib_name(bin.name.as_str()), packs.anchor_of(p).join(bin.root.as_str()))
            }
        };
        let mut features: Vec<String> = on.iter().filter(|(q, f)| *q == p && !f.contains(':') && f != "default").map(|(_, f)| f.clone()).collect();
        features.sort();
        let deps: Vec<(String, usize)> = edges.iter().filter(|(q, _, _)| *q == p).filter_map(|(_, n, t)| index.get(t).map(|at| (n.clone(), *at))).collect();
        packages.push(Package { name, dir: packs.dir(p).to_path_buf(), root, edition: member.edition, features, deps });
    }
    if let Some(h) = hello {
        let member = packs.member(h);
        for d in member.dependencies.iter() {
            notes.push(format!("{}: its dependency `{}` ({}) is not read: bstd is the blaze edition's std", member.name.as_str(), d.name.as_str(), d.package.as_str()));
        }
    }
    for (name, why) in LEFT_OUT {
        notes.push(format!("left out: {name} ({why})"));
    }
    let id = |p: Option<usize>| p.map(|p| CrateId(u32::try_from(index[&p]).expect("a set's crates are counted in u32")));
    let sysroot = Sysroot { core: id(Some(core)), alloc: id(alloc), std: id(std), bstd: id(bstd) };
    Plan { packages, sysroot, notes }
}

/// Each package's `CrateInput`: its name, key (of its directory), edition, what it is given
/// implicitly, root file, the directory `file!()` paths start past, features, dependencies and
/// the cfg it is lowered under (the target's and its features').
fn inputs_of(plan: &Plan, target: &Target) -> Vec<CrateInput<'static>> {
    let leak_all = |list: Vec<&'static str>| -> &'static [&'static str] { list.leak() };
    plan.packages
        .iter()
        .map(|p| {
            let features = leak_all(p.features.iter().map(|f| leak(f.clone())).collect());
            let deps: Vec<(&'static str, usize)> = p.deps.iter().map(|(name, at)| (leak(name.clone()), *at)).collect();
            let cfg = blaze_scope::task::Cfg::of(target, features, g());
            let shown_from = p.dir.parent().unwrap_or(&p.dir);
            CrateInput {
                name: leak(p.name.clone()),
                key: CrateItemKey(ContentId::of("blaze crates example crate 1", p.dir.to_string_lossy().as_bytes())),
                edition: edition_of_syntax(p.edition.syntax),
                implicit: blaze_unit_graph::implicit(p.edition, &blaze_unit_graph::UnitKind::Lib),
                root: leak(normalize(&p.root).to_string_lossy().into_owned()),
                shown_from: leak(shown_from.to_string_lossy().into_owned()),
                features,
                deps: deps.leak(),
                cfg: cfg.atoms().to_vec().leak(),
                script: None,
            }
        })
        .collect()
}

/// The packages read so far, each by its directory: the workspace or registry package it is a
/// member of, read through `Layout`, and the anchor that layout's paths are relative to.
#[derive(Default)]
struct Packages {
    layouts: Vec<(PathBuf, blaze_manifest::layout::Units<'static>)>,
    /// Package id to (layout, member).
    ids: Vec<(usize, usize)>,
    by_dir: HashMap<PathBuf, usize>,
}

impl Packages {
    /// The package in `dir`, its workspace read when it is not yet.
    fn at(&mut self, dir: &std::path::Path) -> usize {
        self.read(dir, false)
    }

    /// The package in `dir`; `registry`: it stands for a registry package (vendored, or
    /// `[patch.crates-io]`), which is its own root as cargo reads one, in no workspace.
    fn read(&mut self, dir: &std::path::Path, registry: bool) -> usize {
        let dir = normalize(dir);
        if let Some(&id) = self.by_dir.get(&dir) {
            return id;
        }
        // A path package is read from `/`, so its workspace is found above it and its path
        // dependencies are inside the anchor.
        let (anchor, start) = if registry {
            (dir.clone(), String::new())
        } else {
            (PathBuf::from("/"), dir.strip_prefix("/").expect("an absolute directory").to_string_lossy().into_owned())
        };
        let units = layout_of(&anchor, &start);
        let layout = self.layouts.len();
        for (member, m) in units.members.iter().enumerate() {
            let at = normalize(&anchor.join(m.dir.as_str()));
            if !self.by_dir.contains_key(&at) {
                self.by_dir.insert(at, self.ids.len());
                self.ids.push((layout, member));
            }
        }
        self.layouts.push((anchor, units));
        *self.by_dir.get(&dir).unwrap_or_else(|| panic!("{}: no package there", dir.display()))
    }

    fn member(&self, id: usize) -> &blaze_manifest::layout::Member<'static> {
        let (layout, member) = self.ids[id];
        &self.layouts[layout].1.members[member]
    }

    fn anchor_of(&self, id: usize) -> &std::path::Path {
        &self.layouts[self.ids[id].0].0
    }

    fn dir(&self, id: usize) -> std::path::PathBuf {
        normalize(&self.anchor_of(id).join(self.member(id).dir.as_str()))
    }

    /// What a dependent calls the package unless it renames it: its library's name.
    fn lib_of(&self, id: usize) -> String {
        let m = self.member(id);
        m.lib.as_ref().map_or_else(|| lib_name(m.name.as_str()), |lib| lib_name(lib.name.as_str()))
    }

    /// The package a dependency line of `of` names: a member or path, or for a package under
    /// library/ a registry line, which is library's `[patch.crates-io]` or its vendored copy.
    /// `None` for a registry or git package elsewhere: third-party, not read.
    fn dep_of(&mut self, of: usize, source: &blaze_manifest::layout::Source<'static>, package: &str, library: &std::path::Path) -> Option<usize> {
        use blaze_manifest::layout::Source;
        let layout = self.ids[of].0;
        let dir = match source {
            Source::Member(t) => {
                let anchor = &self.layouts[layout].0;
                anchor.join(self.layouts[layout].1.members[*t].dir.as_str())
            }
            Source::Path(path) => self.layouts[layout].0.join(path.as_str()),
            Source::Outside(path) => self.dir(of).join(path.as_str()),
            Source::Registry { .. } => {
                if !self.dir(of).starts_with(library) {
                    return None;
                }
                let patched = library.join(package);
                let dir = if package.starts_with("rustc-std-workspace-") && patched.is_dir() {
                    patched
                } else {
                    let prefix = format!("{package}-");
                    let mut found: Vec<PathBuf> = std::fs::read_dir(library.join("vendor"))
                        .ok()?
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix(&prefix)).is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit())))
                        .collect();
                    found.sort();
                    match found.as_slice() {
                        [one] => one.clone(),
                        [] => return None,
                        more => panic!("library/vendor holds {} versions of `{package}`: {more:?}", more.len()),
                    }
                };
                return Some(self.read(&dir, true));
            }
            Source::Git { .. } => return None,
        };
        Some(self.at(&dir))
    }
}

fn edition_of_syntax(e: edition::Edition) -> Edition {
    match e {
        edition::Edition::Edition2015 => Edition::E2015,
        edition::Edition::Edition2018 => Edition::E2018,
        edition::Edition::Edition2021 => Edition::E2021,
        edition::Edition::Edition2024 => Edition::E2024,
    }
}

// ---------------------------------------------------------------------------------------------
// One crate's numbers.

/// One crate's numbers.
#[derive(Default, Clone)]
struct Counts {
    modules: usize,
    decls: usize,
    imports: usize,
    unresolved: usize,
    macro_paths: usize,
    macro_unresolved: usize,
    /// Of `unresolved` and `macro_unresolved`: names of Rust's std, which the graph lacks.
    std_absent: usize,
    /// Unresolved imports and macro paths by cause, with an example.
    causes: BTreeMap<String, (usize, Vec<String>)>,
    /// What the resolution could not read or do (its notes, the walk's, the failed tasks).
    fails: BTreeMap<String, (usize, Vec<String>)>,
}

fn shape_name(shape: CallShape) -> &'static str {
    match shape {
        CallShape::FnLike => "function-like",
        CallShape::Attr => "attribute",
        CallShape::Derive => "derive",
    }
}

/// A note's kind (what failures are counted by) and its example.
fn note_text(c: &CrateNames<'static>, note: &NamesNote<'static>) -> (String, String) {
    let module = |m: ItemKey| c.path_of(m).map_or_else(|| format!("{:x}", m.0), str::to_string);
    match note {
        NamesNote::Lower(n) => {
            let at = at(c, n.at);
            let example = if n.in_expansion { format!("{at} (in an expansion): {}", n.what) } else { format!("{at}: {}", n.what) };
            (n.kind.to_string(), example)
        }
        NamesNote::Root(n) => (n.to_string(), format!("{n:?}")),
        NamesNote::PreludeImportOutsideRoot { module: m } => ("#[prelude_import] outside the crate root".to_string(), module(*m)),
        NamesNote::DeclsRecord { module: m, error: RecordError::Refused(error) } => ("ModuleDecls record refused".to_string(), format!("{}: {error:?}", module(*m))),
        NamesNote::DeclsRecord { module: m, error: RecordError::NotRead(error) } => ("ModuleDecls record does not read back".to_string(), format!("{}: {error:?}", module(*m))),
        NamesNote::Expand { call, at: loc, note } => {
            let at = loc.map_or_else(|| format!("{:x}", call.0), |loc| at(c, loc));
            match note {
                ExpandNote::UnknownCall => ("expansion asked for a call the resolution does not know".to_string(), at),
                ExpandNote::ItemAttrs { macro_name, error } => ("an item's attributes leave no input".to_string(), format!("{at}: {macro_name}: {error}")),
                ExpandNote::AttrNotFound { name } => ("the attribute or derive is not on the item as blaze-expand walks it".to_string(), format!("{at}: {name}")),
                ExpandNote::NoDefinition { shape, name } => (format!("{} call of a macro with no definition here", shape_name(*shape)), format!("{at}: {name}!")),
                ExpandNote::Lex { macro_name, what, error } => {
                    let what = match what {
                        Lexed::Body => "macro body",
                        Lexed::Parameters => "macro parameters",
                        Lexed::Input => "call's input",
                        Lexed::Path => "attribute's path",
                    };
                    (format!("lex refused the {what}"), format!("{at}: {macro_name}!: {error}"))
                }
                ExpandNote::BuiltinUnknown { name } => (format!("builtin macro blaze-expand has no form of: {name}"), at),
                ExpandNote::BuiltinOtherCalled { shape, macro_name } => (format!("builtin {} macro called function-like", shape_name(*shape)), format!("{at}: {macro_name}")),
                ExpandNote::Eager { macro_name, nested, why } => (format!("eager macro: nested call not expanded: {why:?}"), format!("{at}: {macro_name}!({nested}!)")),
                ExpandNote::Expand { macro_name, error } => {
                    let kind = format!("{error:?}");
                    let kind = kind.split([' ', '{', '(']).next().unwrap_or("").to_string();
                    (format!("expand error: {kind}"), format!("{at}: {macro_name}!: {error}"))
                }
                ExpandNote::Diagnostic { macro_name, diagnostic } => {
                    let what = match diagnostic {
                        Diagnostic::NoRuleMatched { .. } => "no rule matched",
                        _ => "another diagnostic",
                    };
                    (format!("macro diagnostic: {what}"), format!("{at}: {macro_name}!"))
                }
                ExpandNote::ForeignDollarCrate { macro_name, krate } => ("an expansion writes a macro definition holding another crate's `$crate` (token-mark-3)".to_string(), format!("{at}: {macro_name}!: crate {krate:?}")),
                ExpandNote::RootsLost { macro_name, lost } => ("an expansion writes another crate's root with no marks".to_string(), format!("{at}: {macro_name}!: {lost}")),
                ExpandNote::ProcModule { macro_name, why } => ("a proc macro whose crate's module cannot run it".to_string(), format!("{at}: {macro_name}: {why:?}")),
        ExpandNote::NestedInput { macro_name, error } => ("a nested call's input is no range of its expansion's tokens".to_string(), format!("{at}: {macro_name}!: {error}")),
                ExpandNote::NotItems { macro_name, message } => ("an expansion that does not read as items (syntax errors)".to_string(), format!("{at}: {macro_name}!: {message}")),
                ExpandNote::Failed { path, implicit, failed } => {
                    let what = match failed.diagnostic {
                        Diagnostic::NoRuleMatched { .. } => "no rule matched",
                        _ => "another error",
                    };
                    let implicit = if *implicit { " (implicit, D104)" } else { "" };
                    (format!("failed call: {what}"), format!("{at}: {path}{implicit}: {failed}"))
                }
            }
        }
        NamesNote::Made { name, at: loc, note } => {
            let at = at(c, *loc);
            match note {
                MadeNote::TooMany => (format!("more than {MAX_MADE_MODULES} modules written by macros: refused"), format!("{at}: mod {name}")),
                MadeNote::NoPlace => ("module written by a macro: its parent module has no place".to_string(), format!("{at}: mod {name}")),
                MadeNote::FileNotFound { path } => ("module written by a macro: its file not found".to_string(), format!("{at}: mod {name}: {path}")),
                MadeNote::CfgOff => ("module written by a macro: cfg'd out by its file's #![cfg]".to_string(), format!("{at}: mod {name}")),
            }
        }
        NamesNote::ExternDropped { krate, name, why } => (format!("implicit crate `{name}` not given: {why}"), format!("crate {krate}")),
    }
}

/// The counts of a finished crate; `std_absent`: it uses Rust's std, which the graph lacks.
fn count(c: &CrateNames<'static>, std_absent: bool) -> Counts {
    let mut n = Counts { modules: c.tree.modules.len() + c.made.len(), ..Counts::default() };
    let mut tally = |d: &DeclsBuilder<'_, N>| {
        n.decls += d.items.len() + d.mods.len() + d.extern_crates.len() + d.macro_rules.len();
        n.imports += d.uses.len();
        n.macro_paths += d.calls.len();
    };
    for (_, d) in c.decls.values() {
        tally(d);
    }
    for e in c.expansions.values() {
        tally(&e.decls);
    }
    for m in c.made.values() {
        tally(&m.decls);
    }
    let use_loc = |fragment: ItemKey, index: u32| c.tables.use_locs.get(&fragment).and_then(|locs| locs.get(index as usize).copied());
    for r in c.done.values() {
        let mpath = c.path_of(r.scope.module).unwrap_or_default().to_string();
        for error in r.scope.errors.iter() {
            match error {
                ScopeError::Import { site, path, segment, why } => {
                    n.unresolved += 1;
                    let fragment = site.expansion.unwrap_or(r.scope.module);
                    let at = use_loc(fragment, site.index).map_or_else(|| mpath.clone(), |loc| at(c, loc));
                    let seg = path.segments.get(*segment as usize).copied().unwrap_or("");
                    let first = path.segments.first().copied().unwrap_or("");
                    let cause = if std_absent && first == "std" {
                        n.std_absent += 1;
                        format!("{NO_STD_IN_GRAPH}: `std::..` imported")
                    } else if *segment == 0 {
                        format!("import {why:?}: first segment `{seg}`")
                    } else {
                        format!("import {why:?}: `{seg}` in `{first}::..`")
                    };
                    add_cause(&mut n.causes, cause, || format!("{at}: use {}", show_path(path)));
                }
                ScopeError::Macro { call, path, why, .. } => {
                    n.macro_unresolved += 1;
                    let src = c.tables.calls.get(call);
                    let at = src.map_or_else(|| mpath.clone(), |src| at(c, src.loc));
                    let last = path.segments.last().copied().unwrap_or("");
                    let first = path.segments.first().copied().unwrap_or("");
                    let cause = if std_absent && (path.segments.len() == 1 || first == "std") {
                        n.std_absent += 1;
                        format!("{NO_STD_IN_GRAPH}: macro or derive `{last}` (std's prelude or path)")
                    } else if src.map(|it| it.kind) == Some(CallShape::Derive) && !is_builtin_derive(last) {
                        // A derive no crate of the graph defines by `macro_rules!` or as a
                        // built-in: a procedural macro's (serde's), which the proc-macro lane runs.
                        format!("needs a procedural macro: derive `{last}`")
                    } else {
                        format!("macro {why:?}: `{last}`")
                    };
                    add_cause(&mut n.causes, cause, || format!("{at}: {}!", show_path(path)));
                }
                ScopeError::ExternCrate { name } => add_cause(&mut n.causes, format!("extern crate `{name}` not found"), || mpath.clone()),
                ScopeError::Duplicate { ns, name, .. } => add_cause(&mut n.fails, format!("duplicate definition ({})", ns.name()), || format!("{mpath}: {name}")),
                other => {
                    let kind = format!("{other:?}");
                    let kind = kind.split([' ', '{']).next().unwrap_or("").to_string();
                    add_cause(&mut n.fails, format!("scope error: {kind}"), || format!("{mpath}: {other:?}"));
                }
            }
        }
    }
    for note in c.tree.notes.iter() {
        let example = match note {
            WalkNote::BothFiles { at: loc } | WalkNote::TooManyModules { at: loc } => at(c, *loc),
            WalkNote::NeitherFile { at: loc, name, .. } =>format!("{}: mod {name}", at(c, *loc)),
            WalkNote::PathFileNotFound { at: loc, path } => format!("{}: {path}", at(c, *loc)),
            WalkNote::PathValueRefused { at: loc, refusal } => format!("{}: {refusal}", at(c, *loc)),
        };
        add_cause(&mut n.fails, note.to_string(), || example);
    }
    for note in c.notes.iter() {
        let (kind, example) = note_text(c, note);
        add_cause(&mut n.fails, kind, || example);
    }
    for (module, error) in c.failed.iter() {
        let path = c.path_of(*module).map_or_else(|| format!("{:x}", module.0), str::to_string);
        let why = error.to_string();
        let kind = why.split(':').next().unwrap_or("").to_string();
        add_cause(&mut n.fails, format!("resolver refused: {kind}"), || format!("{path}: {why}"));
    }
    n
}

fn add_cause(map: &mut BTreeMap<String, (usize, Vec<String>)>, cause: String, example: impl FnOnce() -> String) {
    let entry = map.entry(cause).or_default();
    entry.0 += 1;
    if entry.1.len() < example_cap() {
        entry.1.push(example());
    }
}

/// Whether `name` is one of the toolchain's built-in derives (`Clone`, `Debug`, ...).
fn is_builtin_derive(name: &str) -> bool {
    BuiltinDerive::ALL.into_iter().any(|it| it.name() == name)
}

/// Prints the ten largest of `map`.
fn print_top(title: &str, map: &BTreeMap<String, (usize, Vec<String>)>) {
    let mut rows: Vec<_> = map.iter().collect();
    rows.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
    if rows.is_empty() {
        return;
    }
    println!("    {title}:");
    for (cause, (n, examples)) in rows.iter().take(10) {
        println!("      {n:>6}  {cause}");
        for e in examples {
            println!("              e.g. {e}");
        }
    }
}

/// Every crate of `set`, resolved once per thread count; the counts and the `hello` report of
/// the last run.
fn run_set(set: &str, hello_dir: &std::path::Path, thread_counts: &[usize]) {
    let target = Target::AARCH64_APPLE_DARWIN;
    println!("\n=== set `{set}` ===");
    println!("  target: {target}");
    let plan = plan_of(set, hello_dir, &target);
    for note in &plan.notes {
        println!("  note: {note}");
    }
    let inputs = inputs_of(&plan, &target);
    let first = if ALONE.load(Ordering::Relaxed) { First::Alone } else { First::Prefetch };
    let bump = blaze_own::testing::bump();
    let mut last: Option<Names<'static>> = None;
    let mut times = Vec::new();
    for &threads in thread_counts {
        // The last run's names are dropped first: each run's memory is its own.
        drop(last.take());
        LINKS.lock().expect("no thread panicked holding the `--links-agree` list").clear();
        let start = Instant::now();
        let resolved = resolve_crates_with(&inputs, plan.sysroot, &Disk, &Threads(threads), first, bump);
        let ms = start.elapsed().as_secs_f64() * 1e3;
        let names = resolved.unwrap_or_else(|e| {
            eprintln!("the names of `{set}`: {e}");
            std::process::exit(2);
        });
        if times.is_empty() {
            print_graph(&names);
        }
        times.push((threads, ms));
        for wave in names.graph().waves() {
            for id in wave {
                let c = names.krate(id.index());
                let r = c.report;
                let rate = if r.entries == 0 { 0.0 } else { r.needs as f64 / r.entries as f64 };
                let step_rate = if r.steps == 0 { 0.0 } else { r.wasted_steps as f64 / r.steps as f64 };
                println!(
                    "fill {}: {} entries, {} ended in Need (R = {rate:.3}), {} steps, {} redone (R by steps = {step_rate:.3}), {} waves, {} groups (largest {}), {} failed; prefetch {} components (largest {}), {} edges",
                    c.name,
                    r.entries,
                    r.needs,
                    r.steps,
                    r.wasted_steps,
                    r.waves,
                    r.groups,
                    r.largest,
                    r.failed,
                    r.prefetch_components,
                    r.prefetch_largest,
                    r.prefetch_edges
                );
                let (digest, scopes) = scopes_digest(c);
                println!("names digest {}: {} over {scopes} scopes", c.name, hex8(digest));
            }
        }
        last = Some(names);
    }
    // `--links-agree`: the last run's crates, sorted, their total, then each disagreement.
    let mut disagrees = false;
    if LINKS_AGREE.load(Ordering::Relaxed) {
        let mut links = std::mem::take(&mut *LINKS.lock().expect("no thread panicked holding the `--links-agree` list"));
        links.sort_by(|a, b| a.0.cmp(&b.0));
        println!("\n  links agree (names at module grain against the written scopes, D116):");
        let mut total = blaze_scope::links::Agreed::default();
        for (krate, got) in &links {
            if let Ok(a) = got {
                println!("{krate}: {a}");
                total.add(a);
            }
        }
        println!("total: {total}");
        for (krate, got) in &links {
            if let Err(text) = got {
                println!("{krate}: DISAGREES: {text}");
                disagrees = true;
            }
        }
    }
    let Some(names) = last else { return };
    if PER_CRATE.load(Ordering::Relaxed) {
        per_crate(&names, &inputs, plan.sysroot, thread_counts);
    }
    let files: usize = names.crates().iter().map(|c| c.files.len()).sum();
    let bytes: usize = names.crates().iter().map(|c| c.files.iter().map(|f| f.text.len()).sum::<usize>()).sum();
    println!("\n  {} crates, {files} files read, {:.1} MB of source", names.crates().len(), bytes as f64 / 1e6);
    println!("\n  per crate (last run):");
    println!("    {:<22} {:>7} {:>7} {:>7} {:>10} {:>7} {:>9} {:>8}", "crate", "modules", "decls", "imports", "unresolved", "macros", "macro-unr", "of-std");
    let mut total = Counts::default();
    let std_in_graph = names.graph().sysroot().std.is_some();
    for wave in names.graph().waves() {
        for id in wave {
            let f = names.krate(id.index());
            let name = f.name;
            let std_absent = !f.root_attrs.no_std && !f.root_attrs.no_core && !std_in_graph && matches!(f.implicit.std, blaze_unit_graph::StdIs::Rust);
            let c = count(f, std_absent);
            println!(
                "    {:<22} {:>7} {:>7} {:>7} {:>10} {:>7} {:>9} {:>8}",
                name, c.modules, c.decls, c.imports, c.unresolved, c.macro_paths, c.macro_unresolved, c.std_absent
            );
            total.modules += c.modules;
            total.decls += c.decls;
            total.imports += c.imports;
            total.unresolved += c.unresolved;
            total.macro_paths += c.macro_paths;
            total.macro_unresolved += c.macro_unresolved;
            total.std_absent += c.std_absent;
            for (cause, (n, ex)) in &c.causes {
                // Across crates by cause; the example names the crate.
                let e = total.causes.entry(cause.clone()).or_default();
                e.0 += n;
                if e.1.len() < example_cap() {
                    e.1.extend(ex.iter().take(1).map(|it| format!("{name}: {it}")));
                }
            }
            if VERBOSE.load(Ordering::Relaxed) && !c.causes.is_empty() {
                print_top(&format!("{name}: its causes"), &c.causes);
            }
            for (cause, (n, ex)) in &c.fails {
                let e = total.fails.entry(cause.clone()).or_default();
                e.0 += n;
                if e.1.len() < example_cap() {
                    e.1.extend(ex.iter().take(1).map(|it| format!("{name}: {it}")));
                }
            }
        }
    }
    println!(
        "    {:<22} {:>7} {:>7} {:>7} {:>10} {:>7} {:>9} {:>8}",
        "total", total.modules, total.decls, total.imports, total.unresolved, total.macro_paths, total.macro_unresolved, total.std_absent
    );
    println!("    (of-std: of the unresolved imports and macro paths, those naming Rust's std from a crate that is not no_std; std is not in this graph)");
    let times: Vec<String> = times.iter().map(|(t, ms)| format!("{t} thread{}: {ms:.0} ms", if *t == 1 { "" } else { "s" })).collect();
    println!("  total time (every crate, files read to names): {}", times.join(", "));
    print_top("the ten most frequent causes of what does not resolve (crate: cause)", &total.causes);
    print_top("what the resolution could not read or do, ten largest", &total.fails);
    if let Some(show) = SHOW.get() {
        for f in names.crates() {
            for r in f.done.values() {
                for e in r.scope.bindings.iter().filter(|e| e.name == show.as_str()) {
                    println!("    show {show}: {} {:?} {:?}", f.path_of(r.scope.module).unwrap_or_default(), e.ns, e.binding);
                }
            }
        }
    }
    if set == "hello" {
        hello_report(&names);
    }
    println!("  peak RSS so far {:.1} MB", peak_rss_mb());
    if disagrees {
        eprintln!("`{set}`: names at module grain disagree with the written scopes (the DISAGREES lines above)");
        std::process::exit(1);
    }
}

/// The graph, by wave: each crate, its features and the names of its dependencies.
fn print_graph(names: &Names<'static>) {
    println!("  graph (dependency order, by wave):");
    for (w, wave) in names.graph().waves().enumerate() {
        let list: Vec<String> = wave
            .iter()
            .map(|id| {
                let c = names.graph().get(*id);
                let features = if c.features.is_empty() { String::new() } else { format!(" [{}]", c.features.join(", ")) };
                let deps: Vec<&str> = c.deps.iter().map(|d| d.name).collect();
                format!("{}{features} <- {{{}}}", c.name, deps.join(", "))
            })
            .collect();
        println!("    wave {w}: {}", list.join("; "));
    }
}

// ---------------------------------------------------------------------------------------------
// `hello`: what the owner's `main` reaches by names.

/// The bound on the items the reach walk visits.
const MAX_REACH: usize = 1 << 18;

/// The crate `key` is of.
fn crate_of<'n>(names: &'n Names<'static>, key: ItemKey) -> Option<&'n CrateNames<'static>> {
    names.crate_of(key).map(|at| names.krate(at))
}

/// A module's path, or an item's: its module's path and its name (`core::result::Result`), or
/// its impl header.
fn text_path(f: &CrateNames<'static>, key: ItemKey) -> Option<String> {
    if let Some(path) = f.path_of(key) {
        return Some(path.to_string());
    }
    let (t, _) = f.tables.texts.get(&key)?;
    let module = f.path_of(t.module).unwrap_or_default();
    Some(match t.name {
        Some(name) => format!("{module}::{name}"),
        None => format!("{module}::{{{}}}", t.impl_header.unwrap_or("item").split_whitespace().collect::<Vec<_>>().join(" ")),
    })
}

/// Where a definition is: its crate, its module's path and its name; a variant by its enum's.
fn describe(names: &Names<'static>, key: ItemKey) -> String {
    let Some(f) = crate_of(names, key) else { return format!("{key:?} (no crate)") };
    if let Some(path) = text_path(f, key) {
        return path;
    }
    for (owner, variants) in f.tables.enums.iter() {
        if let Some(v) = variants.iter().find(|v| v.key == key) {
            let owner = text_path(f, *owner).unwrap_or_else(|| format!("{}::<an enum of an expansion>", f.name));
            return format!("{owner}::{}", v.name);
        }
    }
    format!("{}::<an item of an expansion>", f.name)
}

/// `name` in `ns` as written in `module` of `f`: the module's scope (own and globs), its
/// crate's extern prelude (types), then the standard prelude; `crate`, `self` and `super`.
fn name_in(names: &Names<'static>, f: &CrateNames<'static>, module: ItemKey, ns: Namespace, name: N) -> Option<ItemKey> {
    match name {
        "crate" => return Some(f.root),
        "self" => return Some(module),
        "super" => return f.chains.get(&module).and_then(|c| c.get(1)).copied(),
        _ => {}
    }
    if let Some(b) = names.lookup(module, ns, name) {
        return Some(b.def.key);
    }
    if ns == Namespace::Types {
        if let Some((_, root)) = f.crate_root.extern_prelude.iter().find(|(n, _)| *n == name) {
            return Some(*root);
        }
    }
    let prelude = f.crate_root.head.prelude?;
    names.lookup(prelude, ns, name).map(|b| b.def.key)
}

/// A path written in `module` of `f`, resolved segment by segment through the finished
/// scopes: the definitions of its segments, in order, as far as they resolve.
fn resolve_written(names: &Names<'static>, f: &CrateNames<'static>, module: ItemKey, segments: &[N], bang: bool) -> Vec<ItemKey> {
    let mut out = Vec::new();
    let last = segments.len() - 1;
    let ns_of = |at: usize| if at < last { vec![Namespace::Types] } else if bang { vec![Namespace::Macros] } else { vec![Namespace::Types, Namespace::Values] };
    let mut current: Option<ItemKey> = None;
    for (at, segment) in segments.iter().enumerate() {
        let mut found = None;
        for ns in ns_of(at) {
            found = match current {
                None => name_in(names, f, module, ns, segment),
                Some(m) => match *segment {
                    "super" => crate_of(names, m).and_then(|o| o.chains.get(&m)).and_then(|c| c.get(1)).copied(),
                    _ => names.lookup(m, ns, segment).map(|b| b.def.key),
                },
            };
            if found.is_some() {
                break;
            }
        }
        let Some(def) = found else { break };
        out.push(def);
        current = Some(def);
    }
    out
}

/// What the owner's `main` names resolve to, and the modules and items of bstd, core and
/// alloc reachable from them by the names their texts write.
fn hello_report(names: &Names<'static>) {
    let Some(hello) = names.crates().iter().copied().find(|f| f.name == "hello_world") else {
        println!("  hello: the owner's crate is not in the set");
        return;
    };
    println!("\n  hello: the names of `main` (src/main.rs), resolved from its root module:");
    let mut start = Vec::new();
    for (name, ns) in [("Cx", Namespace::Types), ("Io", Namespace::Types), ("Result", Namespace::Types), ("Error", Namespace::Types), ("Ok", Namespace::Values)] {
        match name_in(names, hello, hello.root, ns, name) {
            Some(def) => {
                println!("    {name:<7} -> {}", describe(names, def));
                start.push(def);
            }
            None => println!("    {name:<7} -> not found"),
        }
    }
    match hello.crate_root.head.prelude {
        Some(p) => println!("    (its standard prelude: {})", describe(names, p)),
        None => println!("    (it has no standard prelude)"),
    }
    // The walk: each reached item's text, its paths resolved in its module; a module reached
    // is listed, an item's text read in turn.
    let mut seen: HashSet<ItemKey> = start.iter().copied().collect();
    let mut queue: std::collections::VecDeque<ItemKey> = start.iter().copied().collect();
    let mut modules: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut items: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut no_text = 0usize;
    // The modules the walk touches: each one named, and each one holding a reached item.
    let mut holding: HashSet<ItemKey> = HashSet::new();
    // Each module's impls with the last segments of the paths their headers write.
    let mut impls: HashMap<ItemKey, Vec<(Vec<&str>, ItemText<'static>)>> = HashMap::new();
    for f in names.crates() {
        for (t, _) in f.tables.texts.values() {
            if let Some(header) = t.impl_header {
                let heads: Vec<&str> = paths_in(header).iter().filter_map(|(s, _)| s.last().copied()).collect();
                impls.entry(t.module).or_default().push((heads, *t));
            }
        }
    }
    while let Some(key) = queue.pop_front() {
        if seen.len() > MAX_REACH {
            println!("    the walk stopped at {MAX_REACH} items");
            break;
        }
        let Some(f) = crate_of(names, key) else { continue };
        let path = describe(names, key);
        if f.tree.by_key.contains_key(&key) || f.made.contains_key(&key) {
            modules.entry(f.name.to_string()).or_default().insert(path);
            holding.insert(key);
            continue;
        }
        items.entry(f.name.to_string()).or_default().insert(path);
        let Some((t, _)) = f.tables.texts.get(&key) else {
            no_text += 1;
            continue;
        };
        holding.insert(t.module);
        let mut texts = vec![*t];
        // An inherent or trait impl whose header names a reached type is read with it: an
        // impl names its self type, so by names alone it is reached from that type.
        if let Some(name) = t.name {
            for (header_names, other) in impls.get(&t.module).into_iter().flatten() {
                if header_names.contains(&name) {
                    texts.push(*other);
                }
            }
        }
        for text in texts {
            if text.key != key && seen.insert(text.key) {
                items.entry(f.name.to_string()).or_default().insert(describe(names, text.key));
            }
            for (segments, bang) in paths_in(text.text) {
                for def in resolve_written(names, f, text.module, &segments, bang) {
                    if seen.insert(def) {
                        queue.push_back(def);
                    }
                }
            }
        }
    }
    println!("\n  hello: reachable by names from those five (item texts, and the impls in a reached type's module that name it):");
    for (krate, list) in &modules {
        println!("    {krate}: {} modules: {}", list.len(), list.iter().cloned().collect::<Vec<_>>().join(", "));
    }
    for (krate, list) in &items {
        // Named items first: an impl's header is long and tells less.
        let shown: Vec<String> = list.iter().filter(|it| !it.contains('{')).take(12).cloned().collect();
        let impls = list.iter().filter(|it| it.contains('{')).count();
        println!("    {krate}: {} items ({impls} of them impls), e.g. {}, ..", list.len(), shown.join(", "));
    }
    if no_text > 0 {
        println!("    ({no_text} items reached have no text here: a variant, or written by a derive or a builtin)");
    }
    // What `run` would check: the crates and modules `main` reaches by name.
    let all_modules: usize = names.crates().iter().map(|f| f.tree.modules.len() + f.made.len()).sum();
    let mut by_crate: BTreeMap<String, usize> = BTreeMap::new();
    for &m in &holding {
        if let Some(f) = crate_of(names, m) {
            *by_crate.entry(f.name.to_string()).or_default() += 1;
        }
    }
    let crates_reached = by_crate.len() + 1; // and hello_world itself
    println!(
        "\n  hello: `main` reaches {crates_reached} of {} crates and {} of {all_modules} modules by name (hello's own root included in neither module count)",
        names.crates().len(),
        holding.len()
    );
    let per: Vec<String> = by_crate.iter().map(|(k, n)| format!("{k} {n}")).collect();
    println!("    modules reached per crate: {}", per.join(", "));
    macro_report(names, hello);
}

/// What item-position macros made: the program entry (`main` as the entry attribute rewrote
/// it, with `bstd_hosted_start` in it), bstd's `Placement` and `Reach` `Clone` impls, and each
/// derive applied (expanded with text) against its uses, by name, across bstd.
fn macro_report(names: &Names<'static>, hello: &CrateNames<'static>) {
    println!("\n  macros: what item-position calls made");
    match name_in(names, hello, hello.root, Namespace::Values, "main") {
        Some(main) => match hello.tables.texts.get(&main) {
            Some((t, at)) => {
                let made = matches!(at, blaze_scope::resolve::At::Expansion(_));
                let hosted = t.text.contains("bstd_hosted_start");
                println!("    hello `main`: made by an expansion {made}; holds `bstd_hosted_start` {hosted}");
            }
            None => println!("    hello `main`: {} has no text", describe(names, main)),
        },
        None => println!("    hello `main`: not found"),
    }
    let Some(bstd) = names.crates().iter().copied().find(|f| f.name == "bstd") else {
        println!("    bstd: not in the set");
        return;
    };
    for ty in ["Placement", "Reach"] {
        let impls: Vec<&str> = bstd.tables.impls.iter().map(|i| i.header).filter(|h| h.contains("Clone") && h.split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == ty)).collect();
        println!("    bstd `{ty}`: {} Clone impls in tables.impls{}", impls.len(), impls.first().map_or(String::new(), |h| format!(" ({})", h.trim())));
    }
    derive_counts("bstd", &[bstd]);
    let all: Vec<&CrateNames<'static>> = names.crates().iter().copied().collect();
    derive_counts("every crate of the set", &all);
}

/// Derive calls by the derive's last segment: applied (expanded with text) against used.
fn derive_counts(what: &str, crates: &[&CrateNames<'static>]) {
    let mut by_name: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for f in crates {
        for (key, call) in f.tables.calls.iter() {
            if call.kind != CallShape::Derive {
                continue;
            }
            let last = call.name.rsplit("::").next().unwrap_or(call.name).trim();
            let row = by_name.entry(last).or_default();
            row.0 += 1;
            if f.expansions.get(key).is_some_and(|e| e.made.is_some()) {
                row.1 += 1;
            }
        }
    }
    let mut rows: Vec<(&str, (usize, usize))> = by_name.into_iter().collect();
    rows.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
    let shown: Vec<String> = rows.iter().take(14).map(|(n, (used, applied))| format!("{n} {applied}/{used}")).collect();
    println!("    {what}: derives applied/used: {}", shown.join(", "));
}

/// The paths written in `text`: runs of identifiers joined by `::`, and whether a `!` follows
/// (a macro). Comments, strings and lifetimes are skipped.
fn paths_in(text: &str) -> Vec<(Vec<&str>, bool)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while at < b.len() {
        let c = b[at];
        if c == b'/' && b.get(at + 1) == Some(&b'/') {
            while at < b.len() && b[at] != b'\n' {
                at += 1;
            }
        } else if c == b'/' && b.get(at + 1) == Some(&b'*') {
            at += 2;
            while at + 1 < b.len() && !(b[at] == b'*' && b[at + 1] == b'/') {
                at += 1;
            }
            at += 2;
        } else if c == b'"' {
            at += 1;
            while at < b.len() && b[at] != b'"' {
                at += if b[at] == b'\\' { 2 } else { 1 };
            }
            at += 1;
        } else if c == b'\'' {
            // A lifetime or a char: skip the quote and what an identifier would read.
            at += 1;
            if b.get(at + 1) == Some(&b'\'') {
                at += 2;
            } else {
                while at < b.len() && ident(b[at]) {
                    at += 1;
                }
            }
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let mut segments = Vec::new();
            loop {
                let start = at;
                while at < b.len() && ident(b[at]) {
                    at += 1;
                }
                segments.push(&text[start..at]);
                if text[at..].starts_with("::") && text[at + 2..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
                    at += 2;
                } else {
                    break;
                }
            }
            let bang = text[at..].starts_with('!') && !text[at..].starts_with("!=");
            out.push((segments, bang));
        } else if c.is_ascii_digit() {
            while at < b.len() && ident(b[at]) {
                at += 1;
            }
        } else {
            at += 1;
        }
    }
    out
}
