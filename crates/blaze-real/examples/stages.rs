//! Runs every stage of the new blaze that can run alone over a real checkout and prints, per
//! stage, what it covered, what stopped it (the ten most frequent causes, one place each), the
//! time on one thread and on every core, and the heap's peak while it ran:
//!
//! - a: manifests through blaze-manifest's `Layout`, and the lock file through its `lock`;
//! - b: which dependency sources the lock file names are on this disk (cargo's caches, read only);
//! - c: the outline of every `.rs` file (blaze-file-items);
//! - d: the macro census (blaze-real's `census`) and the item-position `macro_rules!` calls
//!   expanded by blaze-expand against the workspace's own definitions;
//! - f: the token lints over every item (blaze-lint), and again after a one-line edit;
//! - g: stages a, c and d over each dependency package on disk, in place: its manifest, the
//!   outline of its files in its own edition, and its own `macro_rules!` definitions prepared
//!   and its calls of them (at any position) expanded;
//! - h: every body of the workspace lowered by blaze-lower (`lower_body_expanding`, its calls
//!   expanded by name over std's and the workspace's macros) and converted to blaze-typeck's
//!   form (`to_typeck`): clean, or the first form that stops it.
//!
//! Names (stage e) are blaze-scope's, whose driver is still its examples': see tempo.md.
//!
//! `examplerun.sh blaze-real stages <checkout> [cargo home] [<rust-src>/library]`. The checkout
//! is read, never written; nothing of it is run. A measurement and a smoke run (rule 82), not a
//! test.

#![feature(allocator_api)]

use std::alloc::{GlobalAlloc, Layout as MemLayout, System};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use blaze_expand::{
    CallSite, Ctx, DefAt, DefSite, ExpandCx, Expanded, Export, HostAsk, HostError, HostWhy, IncludeAs, Included,
    LocalInner, MacroDefinition, NestedKind, Place, PlaceOf, Prepared, ProcCall, Produced, TokenStream, expand_prepared,
    find_calls, prepare,
};
use blaze_file_items::outline;
use blaze_lower::typeck::{Refused, Types, to_typeck};
use blaze_lower::by_name::ByName;
use blaze_lower::scoped::Scoped;
use blaze_lower::{Cfg, CfgAnswer, Expand, ExprId, TypeId, View, lower_body_expanding};
use blaze_typeck::{ItemKey, PNode, PTy, PortableTys};
use blaze_id::{ContentId, CrateItemKey};
use blaze_lint::{Findings, Rule, RuleSet, Scope, Step, TestBits, Units};
use blaze_manifest::layout::{DepKind, Entry, Layout, Need, Pass, Source};
use blaze_manifest::lock::Lock;
use blaze_own::Alloc;
use blaze_real::census::{self, Event, ExportKind};
use blaze_surface::{Edition, ItemDetail};

mod names_load;

// ---------------------------------------------------------------------------------------------
// The heap's peak, per stage: every allocation counted, the peak kept only while a stage runs.

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ON: AtomicBool = AtomicBool::new(false);

// SAFETY: every call forwards to `System` with the caller's own layout and pointer, unchanged;
// the counters are plain atomics beside it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: MemLayout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        if ON.load(Ordering::Relaxed) {
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        // SAFETY: the caller's layout, as `GlobalAlloc::alloc` requires.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: MemLayout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` came from `System` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: MemLayout, new_size: usize) -> *mut u8 {
        if new_size >= layout.size() {
            let live = LIVE.fetch_add(new_size - layout.size(), Ordering::Relaxed) + new_size - layout.size();
            if ON.load(Ordering::Relaxed) {
                PEAK.fetch_max(live, Ordering::Relaxed);
            }
        } else {
            LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
        }
        // SAFETY: `ptr` came from `System` with `layout`; the new size is the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static HEAP: Counting = Counting;

/// The heap's peak from now until [`Watch::peak`], above what was live at the start.
struct Watch {
    base: usize,
}

impl Watch {
    fn start() -> Watch {
        let base = LIVE.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);
        ON.store(true, Ordering::Relaxed);
        Watch { base }
    }

    fn peak(self) -> String {
        ON.store(false, Ordering::Relaxed);
        mb(PEAK.load(Ordering::Relaxed).saturating_sub(self.base))
    }
}

fn mb(bytes: usize) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

fn ms(seconds: f64) -> String {
    format!("{:.1} ms", seconds * 1e3)
}

/// Best of three of `work`.
fn best<T>(mut work: impl FnMut() -> T) -> (f64, T) {
    let mut fastest = f64::MAX;
    let mut last = None;
    for _ in 0..3 {
        let start = Instant::now();
        let out = work();
        fastest = fastest.min(start.elapsed().as_secs_f64());
        last = Some(out);
    }
    match last {
        Some(out) => (fastest, out),
        None => unreachable!("three rounds ran"),
    }
}

/// Runs `work` over `0..n` on `threads` threads taking the next index from one counter.
fn spread(n: usize, threads: usize, work: &(dyn Fn(usize) -> usize + Sync)) -> usize {
    let next = AtomicUsize::new(0);
    let total = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut mine = 0;
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    if at >= n {
                        break;
                    }
                    mine += work(at);
                }
                total.fetch_add(mine, Ordering::Relaxed);
            });
        }
    });
    total.load(Ordering::Relaxed)
}

/// A tally of causes: each with its count and its first place.
#[derive(Default)]
struct Causes(BTreeMap<String, (usize, String)>);

impl Causes {
    fn add(&mut self, cause: impl Into<String>, place: impl FnOnce() -> String) {
        let entry = self.0.entry(cause.into()).or_insert_with(|| (0, place()));
        entry.0 += 1;
    }

    /// `other`'s counts added; a cause's first place stays the one seen first.
    fn merge(&mut self, other: Causes) {
        for (cause, (n, place)) in other.0 {
            self.0.entry(cause).or_insert((0, place)).0 += n;
        }
    }

    fn total(&self) -> usize {
        self.0.values().map(|(n, _)| n).sum()
    }

    fn print(&self, title: &str, most: usize) {
        let mut rows: Vec<_> = self.0.iter().collect();
        rows.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
        println!("  {title}: {} in {} kinds{}", self.total(), rows.len(), if rows.len() > most { format!(", the {most} most frequent:") } else { String::new() });
        for (cause, (n, place)) in rows.iter().take(most) {
            println!("    {n:>6}  {cause}  (first: {place})");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The checkout's files.

struct File {
    /// Checkout-relative, `/`-separated.
    rel: String,
    text: String,
}

fn walk(root: &Path) -> Vec<File> {
    let mut out = Vec::new();
    let mut todo = vec![(root.to_path_buf(), String::new())];
    // Bounded: each directory is listed once; links are not followed.
    while let Some((dir, rel)) = todo.pop() {
        let Ok(listed) = std::fs::read_dir(&dir) else { continue };
        for entry in listed.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let sub = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() && name != "target" && name != ".git" && name != ".blaze" {
                todo.push((entry.path(), sub));
            } else if kind.is_file() && name.ends_with(".rs") {
                match std::fs::read(entry.path()) {
                    Ok(bytes) => out.push(File { rel: sub, text: String::from_utf8_lossy(&bytes).into_owned() }),
                    Err(error) => println!("  unreadable: {sub}: {error}"),
                }
            }
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

fn line_of(text: &str, at: usize) -> usize {
    text.as_bytes()[..at.min(text.len())].iter().filter(|b| **b == b'\n').count() + 1
}

// ---------------------------------------------------------------------------------------------
// Stage a: manifests.

struct Member {
    dir: String,
    name: String,
    /// The edition its syntax is read in.
    edition: Edition,
    proc_macro: bool,
    /// Its dependencies' package names.
    deps: Vec<String>,
}

/// The answer to one need, read from disk.
enum Answer {
    Manifest(String, Option<String>),
    Listing(String, Option<Vec<(String, bool)>>),
}

/// A need, as plain strings.
enum Asked {
    Manifest(String),
    Listing(String),
}

fn answer(root: &Path, need: &Asked) -> Answer {
    match need {
        Asked::Manifest(path) => Answer::Manifest(path.clone(), std::fs::read_to_string(root.join(path)).ok()),
        Asked::Listing(dir) => {
            let listed = std::fs::read_dir(root.join(dir)).ok().map(|listed| {
                let mut entries: Vec<(String, bool)> = listed
                    .flatten()
                    .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.file_type().is_ok_and(|t| t.is_dir())))
                    .collect();
                entries.sort();
                entries
            });
            Answer::Listing(dir.clone(), listed)
        }
    }
}

/// Why `Layout` gave no members: its own error's words, kept for the tally.
enum LayoutStop {
    Start(String),
    Pass(String),
    Manifest { path: String, why: String },
    Passes,
}

impl std::fmt::Display for LayoutStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutStop::Start(why) => write!(f, "refused at the start: {why}"),
            LayoutStop::Pass(why) => write!(f, "refused: {why}"),
            LayoutStop::Manifest { path, why } => write!(f, "refused {path}: {why}"),
            LayoutStop::Passes => write!(f, "no layout after 64 passes"),
        }
    }
}

/// The front end's edition of a manifest's.
fn surface_edition(edition: edition::Edition) -> Edition {
    match edition {
        edition::Edition::Edition2015 => Edition::E2015,
        edition::Edition::Edition2018 => Edition::E2018,
        edition::Edition::Edition2021 => Edition::E2021,
        edition::Edition::Edition2024 => Edition::E2024,
    }
}

/// The expander's edition of the front end's.
fn expand_edition(edition: Edition) -> blaze_expand::Edition {
    match edition {
        Edition::E2015 => blaze_expand::Edition::E2015,
        Edition::E2018 => blaze_expand::Edition::E2018,
        Edition::E2021 => blaze_expand::Edition::E2021,
        Edition::E2024 => blaze_expand::Edition::E2024,
    }
}

/// Reads the layout, answering each pass's needs on `threads` threads; prints only when `show`.
fn layout(root: &Path, threads: usize, show: bool) -> Result<Vec<Member>, LayoutStop> {
    let alloc = Alloc::global();
    let mut reader = Layout::new("", alloc).map_err(|error| LayoutStop::Start(error.to_string()))?;
    let mut passes = 0;
    // Bounded: the number of passes is the depth of the search; 64 is far past any workspace.
    let units = loop {
        passes += 1;
        if passes > 64 {
            return Err(LayoutStop::Passes);
        }
        let needs = match reader.pass() {
            Ok(Pass::Done(units)) => break units,
            Ok(Pass::Need(needs)) => needs,
            Err(error) => return Err(LayoutStop::Pass(error.to_string())),
        };
        // As plain strings, which threads may share (an `Alloc` handle may not cross).
        let needs: Vec<Asked> = needs
            .iter()
            .map(|need| match need {
                Need::Manifest(path) => Asked::Manifest(path.as_str().to_string()),
                Need::Listing(dir) => Asked::Listing(dir.as_str().to_string()),
            })
            .collect();
        let answers: Vec<Answer> = if threads <= 1 {
            needs.iter().map(|need| answer(root, need)).collect()
        } else {
            let slots: Vec<std::sync::Mutex<Option<Answer>>> = needs.iter().map(|_| std::sync::Mutex::new(None)).collect();
            spread(needs.len(), threads, &|at| {
                let made = answer(root, &needs[at]);
                if let Ok(mut slot) = slots[at].lock() {
                    *slot = Some(made);
                }
                0
            });
            slots.into_iter().filter_map(|slot| slot.into_inner().ok().flatten()).collect()
        };
        for made in answers {
            match made {
                Answer::Manifest(path, text) => {
                    if let Err(error) = reader.manifest(&path, text.as_deref()) {
                        return Err(LayoutStop::Manifest { path, why: error.to_string() });
                    }
                }
                Answer::Listing(dir, entries) => {
                    let entries: Option<Vec<Entry<'_>>> =
                        entries.as_ref().map(|list| list.iter().map(|(name, is_dir)| Entry { name, is_dir: *is_dir }).collect());
                    reader.listing(&dir, entries.as_deref());
                }
            }
        }
    };
    let members: Vec<Member> = units
        .members
        .iter()
        .map(|m| Member {
            dir: m.dir.as_str().to_string(),
            name: m.name.as_str().to_string(),
            edition: surface_edition(m.edition.syntax),
            proc_macro: m.lib.as_ref().is_some_and(|lib| lib.proc_macro),
            deps: m.dependencies.iter().map(|d| d.package.as_str().to_string()).collect(),
        })
        .collect();
    if show {
        let (mut libs, mut bins, mut procs, mut features) = (0, 0, 0, 0);
        let mut by_source: BTreeMap<&str, usize> = BTreeMap::new();
        let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
        let mut registry_names: HashSet<String> = HashSet::new();
        let mut git_urls: HashSet<String> = HashSet::new();
        let mut editions: BTreeMap<String, usize> = BTreeMap::new();
        for member in units.members.iter() {
            libs += usize::from(member.lib.is_some());
            procs += usize::from(member.lib.as_ref().is_some_and(|lib| lib.proc_macro));
            bins += member.bins.len();
            features += member.features.len();
            *editions.entry(format!("{:?}", member.edition)).or_default() += 1;
            for dep in member.dependencies.iter() {
                let source = match &dep.source {
                    Source::Member(_) => "member",
                    Source::Path(_) => "path",
                    Source::Registry { .. } => {
                        registry_names.insert(dep.package.as_str().to_string());
                        "registry"
                    }
                    Source::Git { url, .. } => {
                        git_urls.insert(url.as_str().to_string());
                        "git"
                    }
                    Source::Outside(_) => "outside",
                };
                *by_source.entry(source).or_default() += 1;
                let kind = match dep.kind {
                    DepKind::Normal => "normal",
                    DepKind::Dev => "dev",
                    DepKind::Build => "build",
                };
                *by_kind.entry(kind).or_default() += 1;
            }
        }
        println!(
            "  layout: {} manifests read, {} members, {libs} libraries ({procs} proc-macro), {bins} binaries, {features} features, {passes} passes",
            units.manifests.len(),
            units.members.len()
        );
        println!("  editions: {editions:?}");
        println!(
            "  dependency lines by source: {by_source:?}; by kind: {by_kind:?}; {} registry packages and {} git repositories named",
            registry_names.len(),
            git_urls.len()
        );
    }
    Ok(members)
}

// ---------------------------------------------------------------------------------------------
// Stage b: the lock file and the dependency sources on this disk.

struct Locked {
    name: String,
    version: String,
    /// `registry+..`, `git+..`, or empty for a path package.
    source: String,
}

fn lock(root: &Path) -> Vec<Locked> {
    let text = match std::fs::read_to_string(root.join("Cargo.lock")) {
        Ok(text) => text,
        Err(error) => {
            println!("  Cargo.lock: {error}");
            return Vec::new();
        }
    };
    let (took, parsed) = best(|| Lock::read(text.as_bytes(), Alloc::global()));
    match parsed {
        Ok(lock) => {
            let packages: Vec<Locked> = lock
                .packages
                .iter()
                .map(|p| Locked {
                    name: p.name.as_str().to_string(),
                    version: p.version.to_string(),
                    source: p.source_text.as_str().to_string(),
                })
                .collect();
            let (mut path, mut registry, mut git) = (0, 0, 0);
            for p in &packages {
                match p.source.split_once('+').map(|(kind, _)| kind) {
                    None => path += 1,
                    Some("git") => git += 1,
                    Some(_) => registry += 1,
                }
            }
            println!(
                "  Cargo.lock (blaze_manifest::lock): format {:?}, {} packages ({path} path, {registry} registry, {git} git), {:.1} KB read in {}",
                lock.format,
                packages.len(),
                text.len() as f64 / 1e3,
                ms(took)
            );
            packages
        }
        Err(error) => {
            println!("  Cargo.lock refused: {error}");
            Vec::new()
        }
    }
}

/// Every `.rs` file under `dir`: count and bytes.
fn rs_under(dir: &Path, each: &mut impl FnMut(&Path, u64)) {
    let mut todo = vec![dir.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let Ok(listed) = std::fs::read_dir(&dir) else { continue };
        for entry in listed.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name();
            if kind.is_dir() && name != "target" && name != ".git" {
                todo.push(entry.path());
            } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "rs") {
                each(&entry.path(), entry.metadata().map_or(0, |m| m.len()));
            }
        }
    }
}

/// Where each locked dependency's source would be in cargo's caches, and whether it is there.
fn sources(packages: &[Locked], cargo_home: &Path) -> Vec<PathBuf> {
    let registries: Vec<PathBuf> =
        std::fs::read_dir(cargo_home.join("registry/src")).map(|l| l.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    let checkouts: Vec<(String, PathBuf)> = std::fs::read_dir(cargo_home.join("git/checkouts"))
        .map(|l| l.flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), e.path())).collect())
        .unwrap_or_default();
    let mut present = Vec::new();
    let mut seen_git: HashSet<PathBuf> = HashSet::new();
    let (mut reg_have, mut reg_miss, mut git_have, mut git_miss) = (0, 0, 0, 0);
    let mut missing = Causes::default();
    for p in packages {
        if p.source.starts_with("git+") {
            // `git+https://host/owner/repo?rev=..#<sha>`: the checkout is `<repo>-<hash>/<sha:7>`.
            let url = &p.source[4..];
            let (url, sha) = url.split_once('#').unwrap_or((url, ""));
            let url = url.split('?').next().unwrap_or(url);
            let repo = url.trim_end_matches('/').trim_end_matches(".git").rsplit('/').next().unwrap_or("");
            let short = &sha[..sha.len().min(7)];
            let found = checkouts
                .iter()
                .filter(|(name, _)| name.rsplit_once('-').is_some_and(|(r, _)| r == repo))
                .map(|(_, path)| path.join(short))
                .find(|dir| dir.is_dir());
            match found {
                Some(dir) => {
                    git_have += 1;
                    if seen_git.insert(dir.clone()) {
                        present.push(dir);
                    }
                }
                None => {
                    git_miss += 1;
                    missing.add(format!("git {repo} at {short}"), || p.name.clone());
                }
            }
        } else if !p.source.is_empty() {
            let dir = registries.iter().map(|r| r.join(format!("{}-{}", p.name, p.version))).find(|d| d.is_dir());
            match dir {
                Some(dir) => {
                    reg_have += 1;
                    present.push(dir);
                }
                None => {
                    reg_miss += 1;
                    missing.add("registry package not unpacked", || format!("{} {}", p.name, p.version));
                }
            }
        }
    }
    let (mut files, mut bytes) = (0u64, 0u64);
    for dir in &present {
        rs_under(dir, &mut |_, len| {
            files += 1;
            bytes += len;
        });
    }
    println!(
        "  on disk: registry {reg_have} of {} packages, git {git_have} of {} packages ({} checkouts); {files} .rs files, {:.1} MB",
        reg_have + reg_miss,
        git_have + git_miss,
        seen_git.len(),
        bytes as f64 / 1e6
    );
    missing.print("not on disk", 10);
    present
}

/// What the dependencies on disk define, by name: their procedural macros and helpers, and their
/// `macro_rules!` names.
#[derive(Default)]
struct Defined {
    /// By name: each kind and package (`name-version`) that exports it.
    exports: HashMap<String, Vec<(ExportKind, String)>>,
    helpers: HashSet<String>,
    /// By name: each package with a `macro_rules!` of it.
    rules: HashMap<String, Vec<String>>,
    /// The packages the workspace's members depend on directly.
    direct: HashSet<String>,
}

/// Which package a use names, by name and by its path's first segment.
enum Found<'d> {
    In(&'d str),
    /// The path's first segment names a crate none of whose like-named packages is on disk.
    CrateNotOnDisk(String),
    Nowhere,
}

/// The path's first segment when it names a crate (`serde::Serialize`), not `crate::`, `self::`..
fn first_segment(path: &str) -> Option<&str> {
    let (first, _) = path.trim_start_matches("::").split_once("::")?;
    let first = first.trim();
    (!matches!(first, "crate" | "self" | "super" | "$crate")).then_some(first)
}

/// A package (`serde_derive-1.0.229`) whose name holds the crate name `segment` (`serde`).
fn package_has(package: &str, segment: &str) -> bool {
    package.rsplit_once('-').map_or(package, |(name, _)| name).replace('-', "_").contains(segment)
}

impl Defined {
    fn export(&self, path: &str, name: &str, kind: ExportKind) -> Found<'_> {
        let list: Vec<&str> = self.exports.get(name).into_iter().flatten().filter(|e| e.0 == kind).map(|e| e.1.as_str()).collect();
        pick(&list, path, &self.direct)
    }

    fn rules_of(&self, path: &str, name: &str) -> Found<'_> {
        let list: Vec<&str> = self.rules.get(name).into_iter().flatten().map(String::as_str).collect();
        pick(&list, path, &self.direct)
    }
}

/// The package a name is from: the one its path's first segment names; for a bare name, one
/// the workspace depends on directly (`info!` is tracing's, not bindgen's), else any.
fn pick<'d>(list: &[&'d str], path: &str, direct: &HashSet<String>) -> Found<'d> {
    match first_segment(path) {
        Some(segment) => match list.iter().find(|p| package_has(p, segment)) {
            Some(package) => Found::In(package),
            None => Found::CrateNotOnDisk(segment.to_string()),
        },
        None => {
            let named = |p: &&&str| direct.contains(p.rsplit_once('-').map_or(**p, |(name, _)| name));
            list.iter().find(named).or(list.first()).map_or(Found::Nowhere, |package| Found::In(package))
        }
    }
}

fn defined_by(dirs: &[PathBuf], threads: usize, direct: HashSet<String>) -> Defined {
    let mut files = Vec::new();
    for dir in dirs {
        let package = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        rs_under(dir, &mut |path, _| files.push((path.to_path_buf(), package.clone())));
    }
    let found = std::sync::Mutex::new(Defined::default());
    spread(files.len(), threads, &|at| {
        let (path, package) = &files[at];
        let Ok(text) = std::fs::read_to_string(path) else { return 0 };
        let mut mine = Defined::default();
        census::scan(&text, blaze_lex::Cpu::BASELINE, Alloc::global(), &mut |event| match event {
            Event::Export { kind, name, .. } => mine.exports.entry(name.to_string()).or_default().push((kind, package.clone())),
            Event::Helper { name, .. } => {
                mine.helpers.insert(name.to_string());
            }
            Event::RulesDef { name, .. } => mine.rules.entry(name.to_string()).or_default().push(package.clone()),
            _ => {}
        });
        if let Ok(mut all) = found.lock() {
            for (name, list) in mine.exports {
                all.exports.entry(name).or_default().extend(list);
            }
            all.helpers.extend(mine.helpers);
            for (name, list) in mine.rules {
                all.rules.entry(name).or_default().extend(list);
            }
        }
        1
    });
    let mut defined = found.into_inner().unwrap_or_default();
    defined.direct = direct;
    for list in defined.exports.values_mut() {
        list.sort();
        list.dedup();
    }
    for list in defined.rules.values_mut() {
        list.sort();
        list.dedup();
    }
    let count = |kind: ExportKind| defined.exports.values().flatten().filter(|e| e.0 == kind).count();
    println!(
        "  read from {} dependency files: procedural macros {} derives, {} attributes, {} function-like; {} derive helpers; {} macro_rules names",
        files.len(),
        count(ExportKind::Derive),
        count(ExportKind::Attr),
        count(ExportKind::FnLike),
        defined.helpers.len(),
        defined.rules.len()
    );
    defined
}

// ---------------------------------------------------------------------------------------------
// Stage d: the expander's outside, for `macro_rules!` calls (which ask it nothing but `cfg`
// inside their own expansions' eager calls).

struct NoHost;

const NO_HOST: HostError = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed };

impl ExpandCx for NoHost {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Ok(None)
    }

    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Ok(false)
    }

    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }

    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(Place::Number(1))
    }

    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(NO_HOST)
    }
}

fn inner(text: &str) -> &str {
    let t = text.trim();
    match (t.chars().next(), t.chars().last()) {
        (Some('(' | '[' | '{'), Some(')' | ']' | '}')) if t.len() >= 2 => &t[1..t.len() - 1],
        _ => t,
    }
}

/// The member a file is in: the longest member directory it is under.
fn member_of<'m>(members: &'m [Member], rel: &str) -> Option<(usize, &'m Member)> {
    members
        .iter()
        .enumerate()
        .filter(|(_, m)| m.dir.is_empty() || rel.starts_with(&format!("{}/", m.dir)))
        .max_by_key(|(_, m)| m.dir.len())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(root) = args.get(1).map(PathBuf::from) else {
        eprintln!("usage: stages <checkout> [cargo home] [<rust-src>/library] [fetch store]");
        std::process::exit(2);
    };
    let cargo_home = args.get(2).map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")));
    // The pinned toolchain's library sources (pins/toolchain), whose macros stage h's
    // body calls are expanded with.
    let library = args.get(3).map(PathBuf::from);
    // A folder without `core/src/lib.rs` is a wrong argument, never "no library": read
    // quietly, it left stage h with 28 definitions and 0 library files (hprime-6.txt).
    if let Some(library) = &library
        && !library.join("core").join("src").join("lib.rs").is_file()
    {
        eprintln!("stages: {}: not a <rust-src>/library folder (no core/src/lib.rs)", library.display());
        std::process::exit(2);
    }
    // blaze-fetch's store folder (the one holding `records/`): when given, stage h runs again
    // with every body call resolved through the workspace's names.
    let store = args.get(4).map(PathBuf::from);
    // blaze-proc-macro's module store (holding `proc-macro/modules/`) and a folder of this
    // run's for compiled modules and expansion records: when given, stage h''s proc-macro
    // calls run in the sandbox.
    let modules = args.get(5).map(PathBuf::from);
    let modules_out = args.get(6).map(PathBuf::from);
    // A folder of build-script outputs records (`<name>@<version>@<host|target>.outputs`):
    // stage h''s `env!`/`include*!` see each crate's script's variables and files.
    let scripts = args.get(7).map(PathBuf::from);
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!("stages on {} ({cores} threads)", root.display());

    // a: manifests.
    println!("a. manifests");
    let watch = Watch::start();
    let (one, members) = best(|| layout(&root, 1, false));
    let peak = watch.peak();
    let (all, _) = best(|| layout(&root, cores, false));
    let members = match (members, layout(&root, 1, true)) {
        (Ok(members), Ok(_)) => members,
        (Err(stop), _) | (_, Err(stop)) => {
            println!("  {stop}");
            Vec::new()
        }
    };
    println!("  time: {} on 1 thread, {} on {cores} (reads answered in parallel); heap peak {peak}", ms(one), ms(all));
    let packages = lock(&root);

    // b: dependency sources.
    println!("b. dependency sources on this disk");
    let present = match &cargo_home {
        Some(home) => sources(&packages, home),
        None => Vec::new(),
    };
    let start = Instant::now();
    let direct: HashSet<String> = members.iter().flat_map(|m| m.deps.iter().cloned()).collect();
    let defined = defined_by(&present, cores, direct);
    println!("  census of the dependencies on disk: {} on {cores} threads", ms(start.elapsed().as_secs_f64()));

    // c: outline.
    println!("c. outline");
    let files = walk(&root);
    let bytes: usize = files.iter().map(|f| f.text.len()).sum();
    let lines: usize = files.iter().map(|f| f.text.lines().count()).sum();
    let outside = files.iter().filter(|f| member_of(&members, &f.rel).is_none()).count();
    println!("  {} .rs files, {lines} lines, {:.1} MB; {outside} files under no member's directory", files.len(), bytes as f64 / 1e6);
    let watch = Watch::start();
    let (one, items) = best(|| spread(files.len(), 1, &|at| outline(&files[at].text, Edition::E2024, Alloc::global()).items().len()));
    let peak = watch.peak();
    let (all, _) = best(|| spread(files.len(), cores, &|at| outline(&files[at].text, Edition::E2024, Alloc::global()).items().len()));
    let mut errors = Causes::default();
    let mut unhandled = Causes::default();
    for file in &files {
        let made = outline(&file.text, Edition::E2024, Alloc::global());
        for error in made.errors() {
            errors.add(made.message(error).to_string(), || file.rel.clone());
        }
        for item in made.items() {
            if let ItemDetail::Unhandled { syntax } = item.detail {
                unhandled.add(format!("{:?}", blaze_file_items::syntax_kind(syntax)), || {
                    format!("{}:{}", file.rel, line_of(&file.text, item.range.start as usize))
                });
            }
        }
    }
    println!("  {items} items; time {} on 1 thread, {} on {cores}; heap peak {peak} (one thread, outlines dropped as made)", ms(one), ms(all));
    errors.print("syntax errors", 10);
    unhandled.print("unhandled forms", 10);

    // d: macros.
    println!("d. macros");
    let start = Instant::now();
    let mut workspace_rules: HashMap<String, Vec<usize>> = HashMap::new();
    let mut workspace_exports: HashMap<String, ExportKind> = HashMap::new();
    let mut workspace_helpers: HashSet<String> = HashSet::new();
    let mut events: Vec<(usize, String, String, u32, u8)> = Vec::new();
    for (at, file) in files.iter().enumerate() {
        census::scan(&file.text, blaze_lex::Cpu::BASELINE, Alloc::global(), &mut |event| match event {
            Event::RulesDef { name, .. } => workspace_rules.entry(name.to_string()).or_default().push(at),
            Event::Export { kind, name, .. } => {
                workspace_exports.insert(name.to_string(), kind);
            }
            Event::Helper { name, .. } => {
                workspace_helpers.insert(name.to_string());
            }
            Event::Derive { path, name, at: pos } => events.push((at, path.to_string(), name.to_string(), pos, 0)),
            Event::Attr { path, name, at: pos, .. } => events.push((at, path.to_string(), name.to_string(), pos, 1)),
            Event::Call { path, name, at: pos } => events.push((at, path.to_string(), name.to_string(), pos, 2)),
        });
    }
    let census_took = start.elapsed().as_secs_f64();
    let place = |file: usize, pos: u32| format!("{}:{}", files[file].rel, line_of(&files[file].text, pos as usize));
    // `sol!` calls (alloy's, and any wrapper of that name): inline Solidity, or a path to a file.
    let mut sol = Causes::default();
    for (file, path, name, pos, kind) in &events {
        if *kind != 2 || name != "sol" {
            continue;
        }
        let text = &files[*file].text[*pos as usize..];
        let input = text.find('!').map_or("", |bang| text[bang + 1..].trim_start());
        let input = input.get(1..).unwrap_or("").trim_start();
        // Past the outer attributes (`#[sol(rpc)]`), each bracket-balanced.
        let mut rest = input;
        while let Some(after) = rest.strip_prefix('#') {
            let after = after.trim_start();
            let mut depth = 0usize;
            let mut end = after.len();
            for (i, c) in after.char_indices() {
                match c {
                    '[' => depth += 1,
                    ']' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            end = i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            rest = after[end..].trim_start();
        }
        let form = if rest.starts_with('"') { "a path to a file" } else { "inline Solidity" };
        sol.add(format!("{path}!: {form}"), || place(*file, *pos));
    }
    sol.print("sol! calls by form", 10);
    let (mut derives, mut attrs, mut helpers, mut unknown_attrs) = (Causes::default(), Causes::default(), Causes::default(), Causes::default());
    let (mut proc_calls, mut dep_rules_calls, mut unknown_calls, mut own_calls) =
        (Causes::default(), Causes::default(), Causes::default(), Causes::default());
    let (mut builtin_derives, mut builtin_attrs, mut std_calls) = (0, 0, 0);
    // A use is the workspace's own when its path names no crate, or names a member.
    let member_crates: Vec<String> = members.iter().map(|m| m.name.replace('-', "_")).collect();
    let own_path = |path: &str| first_segment(path).is_none_or(|s| member_crates.iter().any(|m| m == s));
    let own_export = |path: &str, name: &str, kind: ExportKind| own_path(path) && workspace_exports.get(name) == Some(&kind);
    for (file, path, name, pos, kind) in &events {
        let at = || place(*file, *pos);
        let (path, name) = (path.as_str(), name.as_str());
        match kind {
            0 if census::builtin_derive(path, name) => builtin_derives += 1,
            0 if own_export(path, name, ExportKind::Derive) => derives.add(format!("{path} (workspace)"), at),
            0 => match defined.export(path, name, ExportKind::Derive) {
                Found::In(package) => derives.add(format!("{path} ({package})"), at),
                Found::CrateNotOnDisk(segment) => derives.add(format!("{path} ({segment}: not on disk)"), at),
                Found::Nowhere => derives.add(format!("{path} (not on disk)"), at),
            },
            1 if census::builtin_attr(path) => builtin_attrs += 1,
            1 if own_export(path, name, ExportKind::Attr) => attrs.add(format!("{path} (workspace)"), at),
            1 => match defined.export(path, name, ExportKind::Attr) {
                Found::In(package) => attrs.add(format!("{path} ({package})"), at),
                _ if first_segment(path).is_none() && (defined.helpers.contains(name) || workspace_helpers.contains(name)) => {
                    helpers.add(path.to_string(), at)
                }
                Found::CrateNotOnDisk(segment) => attrs.add(format!("{path} ({segment}: not on disk)"), at),
                Found::Nowhere => unknown_attrs.add(path.to_string(), at),
            },
            _ if census::std_macro(path, name) => std_calls += 1,
            _ if own_path(path) && workspace_rules.contains_key(name) => own_calls.add(path.to_string(), at),
            _ if own_export(path, name, ExportKind::FnLike) => proc_calls.add(format!("{path} (workspace)"), at),
            _ => match (defined.rules_of(path, name), defined.export(path, name, ExportKind::FnLike)) {
                (Found::In(package), _) => dep_rules_calls.add(format!("{path} ({package})"), at),
                (_, Found::In(package)) => proc_calls.add(format!("{path} ({package})"), at),
                (Found::CrateNotOnDisk(segment), _) => unknown_calls.add(format!("{path} ({segment}: not on disk)"), at),
                _ => unknown_calls.add(path.to_string(), at),
            },
        }
    }
    println!(
        "  census: {} uses read in {} on 1 thread; {} macro_rules! definitions in the workspace; {} procedural macros its proc-macro crates export",
        events.len(),
        ms(census_took),
        workspace_rules.values().map(Vec::len).sum::<usize>(),
        workspace_exports.len()
    );
    println!("  built in (blaze-expand has them): {builtin_derives} derives, {std_calls} std macro calls; {builtin_attrs} attributes rustc reads itself");
    derives.print("derives that are procedural macros", 25);
    attrs.print("attribute macros", 15);
    proc_calls.print("function-like procedural macro calls", 15);
    helpers.print("derive helper attributes (inert once their derive runs)", 10);
    unknown_attrs.print("attributes neither built in nor found in a dependency on disk", 10);
    dep_rules_calls.print("calls of dependencies' macro_rules! (blaze-expand's engine, once the dependency is read)", 15);
    unknown_calls.print("calls of macros found nowhere on disk", 10);
    own_calls.print("calls of the workspace's own macro_rules!", 10);

    // Item-position calls of the workspace's own `macro_rules!`, expanded.
    let mut outcomes = Causes::default();
    let mut skipped = Causes::default();
    let site_key = |member: usize| DefSite { krate: CrateItemKey(ContentId([member as u8; 32])), module: None };
    let call_site = CallSite { edition: blaze_expand::Edition::E2021, panic_edition: blaze_expand::Edition::E2021, ctx: Ctx::root(blaze_expand::Edition::E2021) };
    // Every definition in the workspace by name, with its member and body.
    let mut defs: HashMap<String, Vec<(usize, Option<usize>, String)>> = HashMap::new();
    let mut calls: Vec<(usize, Option<usize>, String, String, u32)> = Vec::new();
    for (at, file) in files.iter().enumerate() {
        let made = outline(&file.text, Edition::E2024, Alloc::global());
        let member = member_of(&members, &file.rel).map(|(i, _)| i);
        for item in made.items() {
            let base = item.range.start as usize;
            let slice = |r: blaze_surface::Range| file.text.get(base + r.start as usize..base + r.end as usize).unwrap_or("");
            match item.detail {
                ItemDetail::MacroRules { .. } => {
                    if let (Some(name), Some(body)) = (item.name, item.body) {
                        defs.entry(slice(name).trim_start_matches("r#").to_string()).or_default().push((at, member, inner(slice(body)).to_string()));
                    }
                }
                ItemDetail::MacroCall { path: Some(path), input } => {
                    calls.push((at, member, slice(path).to_string(), input.map(|i| inner(slice(i)).to_string()).unwrap_or_default(), item.range.start));
                }
                _ => {}
            }
        }
    }
    // The time and heap of the expansions alone, the outlines above left out.
    let watch = Watch::start();
    let start = Instant::now();
    let mut expanded = 0;
    for (file, member, path, input, pos) in &calls {
        let at = || place(*file, *pos);
        let name = path.rsplit("::").next().unwrap_or(path).trim();
        let Some(found) = defs.get(name) else {
            let why = if census::std_macro(path, name) { "a std macro" } else { "not the workspace's macro_rules!" };
            skipped.add(why, at);
            continue;
        };
        // The definition in the call's own member first, else the only one in the workspace.
        let def = found.iter().find(|d| d.1 == *member).or_else(|| (found.len() == 1).then(|| &found[0]));
        let Some((_, def_member, body)) = def else {
            skipped.add("several definitions of the name in other members (needs names)", at);
            continue;
        };
        let def_at = DefAt {
            site: site_key(def_member.unwrap_or(255)),
            edition: blaze_expand::Edition::E2021,
            export: Export::Local,
            local_inner: LocalInner::No,
            transparency: None,
        };
        let body = match blaze_expand::lex(body, blaze_expand::Edition::E2021, Alloc::global()) {
            Ok(tokens) => tokens,
            Err(error) => {
                outcomes.add(format!("definition does not lex: {error:?}"), at);
                continue;
            }
        };
        let args = match blaze_expand::lex(input, blaze_expand::Edition::E2021, Alloc::global()) {
            Ok(tokens) => tokens,
            Err(error) => {
                outcomes.add(format!("call does not lex: {error:?}"), at);
                continue;
            }
        };
        let def = MacroDefinition::rules(body, def_at);
        let call = blaze_expand::Call::fn_like(&args, call_site);
        match blaze_expand::expand(&def, &call, &NoHost, Alloc::global()) {
            Ok(Expanded::Expansion(_)) => {
                expanded += 1;
                outcomes.add("expanded", at);
            }
            Ok(Expanded::NeedsExpanded(_)) => outcomes.add("needs its nested eager calls first", at),
            Err(error) => {
                let text = format!("{error:?}");
                let cause = text.split(['(', '{', ' ']).next().unwrap_or(&text).to_string();
                outcomes.add(format!("refused: {cause}"), at);
            }
        }
    }
    println!(
        "  item-position macro calls: {} in the outlines; {expanded} of the workspace's own expanded in {} on 1 thread; heap peak {}",
        calls.len(),
        ms(start.elapsed().as_secs_f64()),
        watch.peak()
    );
    outcomes.print("outcomes of the workspace's own", 10);
    skipped.print("not expanded here", 10);

    // f: token lints.
    println!("f. token lints (every rule, as a measurement; a plain project's default set is in lint.md)");
    let scopes: Vec<Scope> = files.iter().map(|f| file_scope(&f.rel)).collect();
    let lint_one = |at: usize, rules: &RuleSet, tally: Option<&std::sync::Mutex<BTreeMap<(Rule, &'static str), usize>>>| -> usize {
        let alloc = Alloc::global();
        let file = &files[at];
        let made = outline(&file.text, Edition::E2024, alloc);
        let Ok(units) = Units::plan(made.items(), file.text.len(), alloc) else { return 0 };
        let mut out = Findings::new_in(alloc);
        let mut scratch = blaze_own::AVec::new_in(alloc);
        if let Err(error) = units.lint_all(rules, &file.text, scopes[at], alloc, &mut out, &mut scratch, |_| {}) {
            println!("  lint refused {}: {error:?}", file.rel);
        }
        if let Some(tally) = tally
            && let Ok(mut tally) = tally.lock()
        {
            for finding in out.list.iter() {
                *tally.entry((finding.rule(), finding.template.kind())).or_default() += 1;
            }
        }
        out.list.len()
    };
    let tally = std::sync::Mutex::new(BTreeMap::new());
    let found = spread(files.len(), cores, &|at| lint_one(at, &RuleSet::ALL, Some(&tally)));
    let watch = Watch::start();
    let (one, _) = best(|| spread(files.len(), 1, &|at| lint_one(at, &RuleSet::ALL, None)));
    let peak = watch.peak();
    let (all, _) = best(|| spread(files.len(), cores, &|at| lint_one(at, &RuleSet::ALL, None)));
    let (none_one, _) = best(|| spread(files.len(), 1, &|at| lint_one(at, &RuleSet::only(&[]), None)));
    let plan_only = |at: usize| {
        let alloc = Alloc::global();
        let made = outline(&files[at].text, Edition::E2024, alloc);
        Units::plan(made.items(), files[at].text.len(), alloc).map_or(0, |units| units.list.len())
    };
    let (plan_one, units) = best(|| spread(files.len(), 1, &plan_only));
    let (plan_all, _) = best(|| spread(files.len(), cores, &plan_only));
    let (lex_one, _) = best(|| spread(files.len(), 1, &|at| blaze_lint::lex_floor(&files[at].text)));
    println!(
        "  {found} findings in {units} units; lint alone (outline + plan + lint, less outline + plan) {} on 1 thread, {} on {cores}; the pass with no rule {}; one lex of every byte (the floor) {}; heap peak {peak}",
        ms(one - plan_one),
        ms(all - plan_all),
        ms(none_one - plan_one),
        ms(lex_one)
    );
    let tally = tally.into_inner().unwrap_or_default();
    let mut per_rule: BTreeMap<&str, usize> = BTreeMap::new();
    for ((rule, kind), n) in &tally {
        *per_rule.entry(rule.name()).or_default() += n;
        println!("    {:<15} {kind:<18} {n:>6}", rule.name());
    }
    println!("  per rule: {per_rule:?}");
    for step in [Step::Rust, Step::Suggest, Step::Edition, Step::Strict] {
        let rules = step.rules();
        let n: usize = tally.iter().filter(|((rule, _), _)| rules.runs(*rule)).map(|(_, n)| n).sum();
        println!("  adoption step {}: {n} findings", step.name());
    }
    one_edit(&files, &scopes);

    bodies(&files, &members, cores, library.as_deref());
    if let Some(store) = &store {
        let modules = match (&modules, &modules_out) {
            (Some(m), Some(o)) => Some((m.as_path(), o.as_path())),
            _ => None,
        };
        // `--tests` (after the positional arguments): the members' test units too.
        bodies_named(&root, store, modules, scripts.as_deref(), args.iter().any(|a| a == "--tests"), blaze_own::Heap::process());
    }
    dependencies(&present, cores);
}

/// A file's root scope from its path alone: test code under `tests/` or `benches/` or named
/// `tests.rs`/`test.rs`; a binary root; a build script.
fn file_scope(rel: &str) -> Scope {
    let parts: Vec<&str> = rel.split('/').collect();
    let name = parts.last().copied().unwrap_or("");
    let dirs = &parts[..parts.len().saturating_sub(1)];
    let test = dirs.iter().any(|d| *d == "tests" || *d == "benches") || name == "tests.rs" || name == "test.rs";
    let build_script = name == "build.rs" && dirs.last().is_some_and(|d| *d != "src");
    let bin_root = match parts.as_slice() {
        [.., "src", "main.rs"] => true,
        [.., "bin" | "examples", _, "main.rs"] => true,
        [.., "bin" | "examples", _] => true,
        _ => false,
    };
    Scope::file(if test { TestBits::CFG } else { TestBits::NONE }, bin_root, build_script)
}

/// A one-line edit to a leaf item in the middle of the largest file: the file outlined again,
/// and only the units whose key (item hash, scope key) the first pass did not see linted again.
fn one_edit(files: &[File], scopes: &[Scope]) {
    let Some((at, file)) = files.iter().enumerate().max_by_key(|(_, f)| f.text.len()) else { return };
    let alloc = Alloc::global();
    let before = outline(&file.text, Edition::E2024, alloc);
    let Ok(units) = Units::plan(before.items(), file.text.len(), alloc) else { return };
    let mut kept: HashSet<(ContentId, u64)> = HashSet::new();
    let mut out = Findings::new_in(alloc);
    let mut scratch = blaze_own::AVec::new_in(alloc);
    let mut unit_scopes = vec![scopes[at]; units.list.len()];
    let lint_ok = units.lint_all(&RuleSet::ALL, &file.text, scopes[at], alloc, &mut out, &mut scratch, |done| {
        unit_scopes[done.unit] = done.scope;
    });
    if lint_ok.is_err() {
        return;
    }
    for (i, item) in before.items().iter().enumerate() {
        kept.insert((item.hash, unit_scopes[i + 1].key()));
    }
    // The edit adds a statement before a leaf fn's closing `}`: only a fn with a body, since a
    // line spliced into `fn f();` (a trait's or an extern block's) changes the items, and the
    // relint below reuses the scopes by unit number.
    let leaves: Vec<usize> = (1..units.list.len())
        .filter(|u| {
            let item = &before.items()[u - 1];
            units.children_of(*u).is_empty()
                && matches!(item.detail, ItemDetail::Fn { .. })
                && item.body.is_some()
                && file.text.as_bytes().get(units.list[*u].end as usize - 1) == Some(&b'}')
        })
        .collect();
    let Some(&target) = leaves.get(leaves.len() / 2) else { return };
    let end = units.list[target].end as usize - 1;
    let edited = format!("{}    let edit = 1;\n{}", &file.text[..end], &file.text[end..]);
    let start = Instant::now();
    let after = outline(&edited, Edition::E2024, alloc);
    let Ok(after_units) = Units::plan(after.items(), edited.len(), alloc) else { return };
    let outlined = start.elapsed().as_secs_f64();
    if after_units.list.len() != units.list.len() {
        println!(
            "  one edit in {} at line {}: the units went from {} to {}, so the scopes kept by unit number do not apply; not relinted",
            file.rel,
            line_of(&file.text, units.list[target].base as usize),
            units.list.len(),
            after_units.list.len()
        );
        return;
    }
    let mut relinted = 0;
    let mut out = Findings::new_in(alloc);
    let start = Instant::now();
    for u in 0..after_units.list.len() {
        if u > 0 && kept.contains(&(after.items()[u - 1].hash, unit_scopes[u].key())) {
            continue;
        }
        relinted += 1;
        match blaze_lint::lint(&RuleSet::ALL, &after_units.unit(u, &edited, unit_scopes[u]), alloc, &mut out) {
            Ok(linted) => {
                for (child, scope) in after_units.children_of(u).iter().zip(linted.children.iter()) {
                    unit_scopes[*child as usize] = *scope;
                }
            }
            Err(error) => println!("  lint refused the edit: {error:?}"),
        }
    }
    println!(
        "  one edit in {} ({} units, a line added to the fn at line {}): outline + plan again {}, {relinted} units linted again in {:.1} us",
        file.rel,
        after_units.list.len(),
        line_of(&file.text, units.list[target].base as usize),
        ms(outlined),
        start.elapsed().as_secs_f64() * 1e6
    );
}

// ---------------------------------------------------------------------------------------------
// Stage g: each dependency package on disk, read in place.

/// One package (a member of a dependency's manifest), in its own edition.
struct Package {
    /// `name-version`, as cargo unpacks it.
    label: String,
    dir: PathBuf,
    edition: Edition,
}

/// One file of a package.
struct DepFile {
    package: usize,
    /// Package-relative.
    rel: String,
    text: String,
}

impl DepFile {
    fn place(&self, packages: &[Package], at: usize) -> String {
        format!("{}/{}:{}", packages[self.package].label, self.rel, line_of(&self.text, at))
    }

    /// Part of the package's library or build script, not a test, bench, example or fixture.
    fn in_target(&self) -> bool {
        self.rel.starts_with("src/") || self.rel == "build.rs"
    }
}

/// What one package's own `macro_rules!` did on its own calls of them.
#[derive(Default)]
struct Expansions {
    defs: usize,
    prepared: usize,
    def_refused: Causes,
    macro2: usize,
    calls: usize,
    own: usize,
    expanded: usize,
    outcomes: Causes,
    not_lexed: Causes,
    /// Packages with any call of their own macros, and those whose every such call expanded.
    with_own: usize,
    all_expanded: usize,
}

impl Expansions {
    fn merge(&mut self, other: Expansions) {
        self.defs += other.defs;
        self.prepared += other.prepared;
        self.def_refused.merge(other.def_refused);
        self.macro2 += other.macro2;
        self.calls += other.calls;
        self.own += other.own;
        self.expanded += other.expanded;
        self.outcomes.merge(other.outcomes);
        self.not_lexed.merge(other.not_lexed);
        self.with_own += other.with_own;
        self.all_expanded += other.all_expanded;
    }
}

/// The first word of a value's `Debug`: its variant.
fn variant(debug: &str) -> &str {
    debug.split(['(', '{', ' ']).next().unwrap_or(debug)
}

/// A package's own `macro_rules!` definitions, prepared, and every outermost call in its files
/// whose path names one of them (a single name, or `crate::`, `$crate::`, `self::`, `super::`)
/// expanded once, nested calls in the expansion not followed.
fn expand_package(at: usize, package: &Package, files: &[&DepFile], packages: &[Package]) -> Expansions {
    let alloc = Alloc::global();
    let edition = expand_edition(package.edition);
    let mut out = Expansions::default();
    let mut streams = Vec::new();
    for file in files {
        match blaze_expand::lex(&file.text, edition, alloc) {
            Ok(stream) => streams.push((*file, stream)),
            Err(error) => out.not_lexed.add(format!("{error:?}"), || file.place(packages, 0)),
        }
    }
    let def_at = DefAt {
        site: DefSite { krate: CrateItemKey(ContentId([(at % 251) as u8; 32])), module: None },
        edition,
        export: Export::Local,
        local_inner: LocalInner::No,
        transparency: None,
    };
    let call_site = CallSite { edition, panic_edition: edition, ctx: Ctx::root(edition) };
    let mut defs: Vec<(String, MacroDefinition<'_>)> = Vec::new();
    let mut calls: Vec<(&DepFile, String, String, TokenStream<'_>)> = Vec::new();
    for (file, stream) in &streams {
        let Ok(found) = find_calls(stream, alloc) else {
            out.not_lexed.add("find_calls refused the stream", || file.place(packages, 0));
            continue;
        };
        for call in found.iter() {
            let inner = call.inner();
            let Ok(group) = stream.copy_in(inner.start as usize..inner.end as usize, alloc) else { continue };
            match call.kind {
                NestedKind::MacroRules => {
                    let Some(name) = call.name else { continue };
                    defs.push((stream.text_at(name as usize).to_string(), MacroDefinition::rules(group, def_at)));
                }
                NestedKind::Macro2 => out.macro2 += 1,
                NestedKind::FnLike => {
                    let path: String = (call.path.start..call.path.end).map(|t| stream.text_at(t as usize)).collect();
                    let name = stream.text_at(call.path.end as usize - 1).to_string();
                    calls.push((*file, path, name, group));
                }
            }
        }
    }
    out.defs = defs.len();
    let prepared: Vec<(&str, Option<Prepared<'_>>)> = defs
        .iter()
        .map(|(name, def)| match prepare(def) {
            Ok(prepared) => (name.as_str(), Some(prepared)),
            Err(error) => {
                out.def_refused.add(variant(&format!("{error:?}")).to_string(), || format!("{} `{name}`", package.label));
                (name.as_str(), None)
            }
        })
        .collect();
    out.prepared = prepared.iter().filter(|(_, p)| p.is_some()).count();
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (name, _)) in prepared.iter().enumerate() {
        by_name.entry(name).or_default().push(i);
    }
    out.calls = calls.len();
    for (file, path, name, args) in &calls {
        let local = !path.contains("::") || ["crate::", "$crate::", "self::", "super::"].iter().any(|p| path.starts_with(p));
        let Some(found) = by_name.get(name.as_str()).filter(|_| local) else { continue };
        out.own += 1;
        let place = || file.place(packages, file.text.find(&format!("{name}!")).unwrap_or(0));
        let [one] = found.as_slice() else {
            out.outcomes.add("several definitions of the name (cfg or scope picks one: needs names)", place);
            continue;
        };
        let Some(def) = &prepared[*one].1 else {
            out.outcomes.add("its definition was refused", place);
            continue;
        };
        let call = blaze_expand::Call::fn_like(args, call_site);
        match expand_prepared(def, &call, &NoHost, alloc) {
            Ok(Expanded::Expansion(_)) => {
                out.expanded += 1;
                out.outcomes.add("expanded", place);
            }
            Ok(Expanded::NeedsExpanded(_)) => out.outcomes.add("needs its nested eager calls first", place),
            Err(error) => out.outcomes.add(format!("refused: {}", variant(&format!("{error:?}"))), place),
        }
    }
    if out.own > 0 {
        out.with_own = 1;
        out.all_expanded = usize::from(out.expanded == out.own);
    }
    out
}

fn dependencies(present: &[PathBuf], threads: usize) {
    println!("g. dependency packages on this disk, read in place");
    // a: each package's manifest through `Layout`, for its members and editions.
    let start = Instant::now();
    let mut packages = Vec::new();
    let mut stops = Causes::default();
    for dir in present {
        let label = dir.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        match layout(dir, 1, false) {
            Ok(members) if members.is_empty() => stops.add("no member", || label.clone()),
            Ok(members) => {
                for member in members {
                    let label = if member.dir.is_empty() { label.clone() } else { format!("{label}/{}", member.dir) };
                    packages.push(Package { label, dir: dir.join(&member.dir), edition: member.edition });
                }
            }
            Err(stop) => stops.add(stop.to_string(), || label.clone()),
        }
    }
    let mut editions: BTreeMap<String, usize> = BTreeMap::new();
    for package in &packages {
        *editions.entry(format!("{:?}", package.edition)).or_default() += 1;
    }
    println!(
        "  manifests: {} of {} read by Layout ({} packages), editions {editions:?}, in {} on 1 thread",
        present.len() - stops.total(),
        present.len(),
        packages.len(),
        ms(start.elapsed().as_secs_f64())
    );
    stops.print("manifests Layout refused", 10);

    // c: every file of every package, in its package's edition.
    let mut files = Vec::new();
    for (at, package) in packages.iter().enumerate() {
        for file in walk(&package.dir) {
            files.push(DepFile { package: at, rel: file.rel, text: file.text });
        }
    }
    let bytes: usize = files.iter().map(|f| f.text.len()).sum();
    let lines: usize = files.iter().map(|f| f.text.lines().count()).sum();
    let in_target = files.iter().filter(|f| f.in_target()).count();
    println!(
        "  {} .rs files ({in_target} under src/ or build.rs), {lines} lines, {:.1} MB",
        files.len(),
        bytes as f64 / 1e6
    );
    let outline_one = |at: usize| {
        let file = &files[at];
        outline(&file.text, packages[file.package].edition, Alloc::global()).items().len()
    };
    let watch = Watch::start();
    let start = Instant::now();
    let items = spread(files.len(), 1, &outline_one);
    let one = start.elapsed().as_secs_f64();
    let peak = watch.peak();
    let start = Instant::now();
    spread(files.len(), threads, &outline_one);
    let all = start.elapsed().as_secs_f64();
    println!("  {items} items; time {} on 1 thread, {} on {threads}; heap peak {peak} (one thread)", ms(one), ms(all));
    let found: std::sync::Mutex<[Causes; 3]> = std::sync::Mutex::new(Default::default());
    spread(files.len(), threads, &|at| {
        let file = &files[at];
        let made = outline(&file.text, packages[file.package].edition, Alloc::global());
        let mut mine: [Causes; 3] = Default::default();
        let slot = usize::from(!file.in_target());
        for error in made.errors() {
            mine[slot].add(made.message(error).to_string(), || file.place(&packages, error.range.start as usize));
        }
        for item in made.items() {
            if let ItemDetail::Unhandled { syntax } = item.detail {
                mine[2].add(format!("{:?}", blaze_file_items::syntax_kind(syntax)), || file.place(&packages, item.range.start as usize));
            }
        }
        if let Ok(mut all) = found.lock() {
            for (into, from) in all.iter_mut().zip(mine) {
                into.merge(from);
            }
        }
        0
    });
    let [in_src, elsewhere, unhandled] = found.into_inner().unwrap_or_default();
    in_src.print("syntax errors in src/ and build.rs (almost surely the outline's)", 10);
    elsewhere.print("syntax errors in tests, benches, examples and fixtures", 10);
    unhandled.print("unhandled forms", 10);

    // d: each package's own `macro_rules!` on its own calls.
    let mut by_package: Vec<Vec<&DepFile>> = packages.iter().map(|_| Vec::new()).collect();
    for file in &files {
        by_package[file.package].push(file);
    }
    let merged = std::sync::Mutex::new(Expansions::default());
    let start = Instant::now();
    spread(packages.len(), threads, &|at| {
        let made = expand_package(at, &packages[at], &by_package[at], &packages);
        if let Ok(mut all) = merged.lock() {
            all.merge(made);
        }
        0
    });
    let took = start.elapsed().as_secs_f64();
    let x = merged.into_inner().unwrap_or_default();
    println!(
        "  macro_rules!: {} definitions, {} prepared ({} macro 2.0 left out); {} outermost calls, {} of the package's own, {} expanded ({:.1}%); {} of {} packages with own calls expand them all; {} on {threads} threads",
        x.defs,
        x.prepared,
        x.macro2,
        x.calls,
        x.own,
        x.expanded,
        100.0 * x.expanded as f64 / x.own.max(1) as f64,
        x.all_expanded,
        x.with_own,
        ms(took)
    );
    x.def_refused.print("definitions refused", 10);
    x.outcomes.print("outcomes of own calls", 10);
    x.not_lexed.print("files the expander's lexer refused", 10);
}

// ---------------------------------------------------------------------------------------------
// Stage h: bodies lowered and converted to blaze-typeck's form.

/// A cfg like this host's (macOS on arm64), answering every atom: test code in, every feature
/// off (each crate's feature set is the units lane's, not wired here).
struct HostCfg;

impl Cfg for HostCfg {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        let yes = match (name, value) {
            ("unix" | "debug_assertions" | "test", None) => true,
            ("target_os", Some(os)) => os == "macos",
            ("target_vendor", Some(vendor)) => vendor == "apple",
            ("target_family", Some(family)) => family == "unix",
            ("target_arch", Some(arch)) => arch == "aarch64",
            ("target_pointer_width", Some(width)) => width == "64",
            ("target_endian", Some(endian)) => endian == "little",
            ("panic", Some(panic)) => panic == "unwind",
            ("target_has_atomic", _) => true,
            _ => false,
        };
        if yes { CfgAnswer::Yes } else { CfgAnswer::No }
    }
}

/// Written types are the names stage's: each is left to inference here, so what is counted is
/// the body's forms.
struct Unresolved;

impl Types for Unresolved {
    fn ty(&mut self, _: &View<'_>, _: TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        tys.intern(PNode::Infer).map_err(Refused::Check)
    }

    fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, Refused> {
        Ok(0)
    }

    fn const_arg(&mut self, view: &View<'_>, c: blaze_lower::ConstIdx, _: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        Err(blaze_lower::typeck::no_const_arg(view, c))
    }
}

/// The outside of a body call's expansion: `cfg!` answered by [`HostCfg`] (the body's own cfg
/// set); with a runner ([`names_load::Procs`]), a proc macro run in its sandbox in the body's
/// crate's package and edition, and `env!` answered from that crate's environment set
/// ([`names_load::CARGO_SET`] says which cargo variables it does not know yet: refused; an
/// unset one is `None` only for a crate with no build script, whose `cargo::rustc-env` this
/// run does not read). Every other question (a file, a position) is refused, so a call that
/// asks one is counted as failed, never answered by a guess.
struct BodyHost<'h> {
    procs: Option<&'h names_load::Procs>,
    /// The body's crate, by index.
    krate: usize,
    /// Whether the crate has a build script whose outputs this run was not given (its
    /// variables are unknown, so an unset one is refused, never answered unset). A crate whose
    /// record was given is wrapped in `ScriptCx`, which answers its script's variables first.
    build_script: bool,
    tally: Option<&'h ProcTally>,
    /// The calling file, for `include*!`: its path past the crate's `shown_from` and the
    /// names' loaded texts that read it (`None`: every include refused).
    file: Option<(&'h str, &'h names_load::Loaded)>,
}

impl BodyHost<'_> {
    /// Stage h's host: no runner, no environment.
    const NONE: BodyHost<'static> = BodyHost { procs: None, krate: 0, build_script: true, tally: None, file: None };
}

/// How the body calls of proc macros went: ran (or found in a record), refused, failed.
#[derive(Default)]
struct ProcTally {
    ran: core::cell::Cell<usize>,
    refused: core::cell::Cell<usize>,
    failed: core::cell::Cell<usize>,
}

const BODY_REFUSED: HostError = HostError { ask: HostAsk::Env, why: HostWhy::Refused };

impl ExpandCx for BodyHost<'_> {
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        let Some(procs) = self.procs else { return Err(BODY_REFUSED) };
        let package = &procs.packages[self.krate];
        match package.env.get(name) {
            blaze_proc_macro::package_env::EnvAnswer::Set(value) => return Ok(Some(value)),
            blaze_proc_macro::package_env::EnvAnswer::Unknown => return Err(BODY_REFUSED),
            blaze_proc_macro::package_env::EnvAnswer::Unset => {}
        }
        if self.build_script || names_load::CARGO_SET.contains(&name) {
            return Err(BODY_REFUSED);
        }
        Ok(None)
    }

    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        let refused = HostError { ask: HostAsk::Cfg, why: HostWhy::Refused };
        let mut atom = |name: &str, value: Option<&str>| match HostCfg.atom(name, value) {
            CfgAnswer::Yes => Ok(true),
            CfgAnswer::No => Ok(false),
            CfgAnswer::Need => Err(refused),
        };
        blaze_expand::cfg_predicate(predicate, Alloc::global(), &mut atom).map_err(|_| refused)
    }

    /// A path relative to the calling file's folder (its package-relative place), kept under
    /// the unit's read root the loader gave it (decision D60: `blaze_expand::include_target`
    /// over `Loaded::read_root`), then read from that root's files. A path under the script's
    /// `OUT_DIR` never reaches here: `ScriptCx` answers it from the record.
    fn include(&self, written: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        let refused = HostError { ask: HostAsk::Include, why: HostWhy::Refused };
        let (Some(_), Some((shown, loaded))) = (self.procs, self.file) else { return Err(refused) };
        let Some(root) = loaded.read_root(self.krate) else { return Err(refused) };
        let folder = shown.rsplit_once('/').map_or("", |(folder, _)| folder);
        let Ok(target) = blaze_expand::include_target(root, folder, written, Alloc::global()) else {
            return Ok(Included::Outside);
        };
        Ok(match loaded.root_bytes(self.krate, &target.join("/")) {
            Some(bytes) => Included::Bytes(bytes),
            None => Included::Missing,
        })
    }

    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Err(HostError { ask: HostAsk::Place, why: HostWhy::Refused })
    }

    fn run_proc_macro<'a>(&self, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        let Some(procs) = self.procs else { return Err(refused) };
        let out = procs.procs.run(def, &call, procs.editions[self.krate], &procs.packages[self.krate], alloc);
        if let Some(tally) = self.tally {
            let cell = match &out {
                Ok(_) => &tally.ran,
                Err(HostError { why: HostWhy::Refused, .. }) => &tally.refused,
                Err(_) => &tally.failed,
            };
            cell.set(cell.get() + 1);
        }
        out
    }
}

/// The texts whose `macro_rules!` and `macro` definitions stage h's expander finds by name:
/// the pinned toolchain's `core`, `alloc` and `std` (their exports are every crate's
/// `#[macro_use]` prelude, found by bare name), then the workspace's own files, so a workspace
/// macro of a name shadows std's (`ByName`: the last of a name wins). Until blaze-scope's
/// `Names::macro_path` resolves a call's path, a name is all the lookup has: a dependency's
/// macro (`eyre!`, `info!`) is not defined here and stays a cause.
static BODY_TEXTS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();

thread_local! {
    /// [`BODY_TEXTS`]'s definitions, compiled once a thread (a stream's allocator handle is not
    /// shared across threads), leaked for the run; and how many `prepare` refused.
    static BODY_RULES: std::cell::OnceCell<(&'static [(&'static str, Prepared<'static>)], usize)> =
        const { std::cell::OnceCell::new() };
}

fn body_def_at() -> DefAt {
    DefAt {
        site: DefSite { krate: CrateItemKey(ContentId([1; 32])), module: None },
        edition: blaze_expand::Edition::E2024,
        export: Export::Exported,
        local_inner: LocalInner::No,
        transparency: None,
    }
}

fn body_rules() -> (&'static [(&'static str, Prepared<'static>)], usize) {
    BODY_RULES.with(|rules| {
        *rules.get_or_init(|| {
            let mut defs = Vec::new();
            let mut refused = 0;
            for text in BODY_TEXTS.get().map_or(&[][..], Vec::as_slice) {
                let Ok(stream) = blaze_expand::lex(text, blaze_expand::Edition::E2024, Alloc::global()) else { continue };
                let stream: &'static TokenStream<'static> = Box::leak(Box::new(stream));
                let Ok(found) = find_calls(stream, Alloc::global()) else { continue };
                for call in found.iter() {
                    let Some(name_at) = call.name else { continue };
                    let inner = call.inner();
                    let Ok(body) = stream.copy_in(inner.start as usize..inner.end as usize, Alloc::global()) else { continue };
                    let def = match call.kind {
                        NestedKind::MacroRules => MacroDefinition::rules(body, body_def_at()),
                        NestedKind::Macro2 => {
                            let params = match call.params {
                                None => None,
                                Some(p) => match stream.copy_in(p.start as usize + 1..p.end as usize - 1, Alloc::global()) {
                                    Ok(params) => Some(params),
                                    Err(_) => continue,
                                },
                            };
                            MacroDefinition::macro2(params, body, body_def_at())
                        }
                        _ => continue,
                    };
                    let def: &'static MacroDefinition<'static> = Box::leak(Box::new(def));
                    match prepare(def) {
                        Ok(prepared) => defs.push((stream.text_at(name_at as usize), prepared)),
                        Err(_) => refused += 1,
                    }
                }
            }
            (&*Box::leak(defs.into_boxed_slice()), refused)
        })
    })
}

/// Reads [`BODY_TEXTS`] once: the library's `core`, `alloc` and `std` sources (none when no
/// library is given: then std's macros are not defined, and the line says so), then the
/// workspace's files; only those that define a macro.
fn set_body_texts(library: Option<&Path>, files: &[File]) -> usize {
    let mut texts: Vec<&'static str> = Vec::new();
    let mut library_files = 0;
    if let Some(library) = library {
        let mut paths = Vec::new();
        for krate in ["core", "alloc", "std"] {
            rs_under(&library.join(krate).join("src"), &mut |path, _| paths.push(path.to_path_buf()));
        }
        paths.sort();
        for path in paths {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if text.contains("macro") {
                library_files += 1;
                texts.push(Box::leak(text.into_boxed_str()));
            }
        }
    }
    for file in files {
        if file.text.contains("macro_rules!") || file.text.contains("macro ") {
            texts.push(Box::leak(file.text.clone().into_boxed_str()));
        }
    }
    let _ = BODY_TEXTS.set(texts);
    library_files
}

#[derive(Default)]
struct BodyTally {
    bodies: usize,
    clean: usize,
    /// Body macro calls the expander expanded.
    expanded: usize,
    cfg_out: usize,
    no_body: usize,
    /// Per body, the first thing that stops it.
    first: Causes,
    /// Every form the lowering left unhandled, wherever it is.
    unhandled: Causes,
    /// Every macro call the lowering left in a body, by path, std's marked.
    macros: Causes,
    /// Per cause, the bodies it stops (a body under each of its causes).
    each: Causes,
    /// Bodies whose only cause is macro calls: clean once their calls expand.
    only_macros: usize,
}

impl BodyTally {
    fn merge(&mut self, other: BodyTally) {
        self.bodies += other.bodies;
        self.clean += other.clean;
        self.expanded += other.expanded;
        self.cfg_out += other.cfg_out;
        self.no_body += other.no_body;
        self.first.merge(other.first);
        self.unhandled.merge(other.unhandled);
        self.macros.merge(other.macros);
        self.each.merge(other.each);
        self.only_macros += other.only_macros;
    }
}

/// Every fn, const and static body of `file`.
fn lower_bodies(file: &File, edition: Edition) -> BodyTally {
    let alloc = Alloc::global();
    let made = outline(&file.text, edition, alloc);
    let mut out = BodyTally::default();
    for item in made.items() {
        let has_body = matches!(item.detail, ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. });
        if !has_body || item.body.is_none() {
            continue;
        }
        let base = item.range.start as usize;
        let Some(text) = file.text.get(base..item.range.end as usize) else { continue };
        out.bodies += 1;
        let place = |offset: usize| format!("{}:{}", file.rel, line_of(&file.text, base + offset));
        let mut by_name = ByName::new(body_rules().0, body_def_at(), BodyHost::NONE, alloc);
        lower_one(&mut out, text, edition, &place, &mut by_name);
    }
    out
}

/// One body, the item's text `text`, lowered with `expand`, into `out`: clean, or each cause
/// that stops it (`place` writes where an offset into `text` is).
fn lower_one(out: &mut BodyTally, text: &str, edition: Edition, place: &dyn Fn(usize) -> String, expand: &mut dyn Expand) {
    let alloc = Alloc::global();
    {
        let lowered = match lower_body_expanding(text, edition, &mut HostCfg, &mut blaze_lower::NoModule, expand, alloc) {
            Ok(lowered) => lowered,
            Err(_) => {
                out.first.add("lower_body: a cfg atom left unanswered", || place(0));
                return;
            }
        };
        let view = lowered.view();
        if view.cfg_out() {
            out.cfg_out += 1;
            return;
        }
        out.expanded += view.expansion_calls().count();
        // Where a piece of the view's text sits in the item.
        let offset_of = |piece: &str| (piece.as_ptr() as usize).checked_sub(text.as_ptr() as usize).filter(|o| *o <= text.len()).unwrap_or(0);
        // This body's causes, each with its first place: the lowering's unhandled forms and
        // diagnostics, then the first form `to_typeck` refuses.
        let mut causes: Vec<(String, usize)> = Vec::new();
        let mut note = |cause: String, at: usize| {
            if !causes.iter().any(|(c, _)| *c == cause) {
                causes.push((cause, at));
            }
        };
        for u in view.unhandled() {
            let piece = view.text_at(u.range).unwrap_or("");
            let at = offset_of(piece);
            let kind = format!("{:?} ({:?})", blaze_file_items::syntax_kind(u.syntax), u.why);
            if kind.starts_with("MACRO_CALL") {
                let path = piece.split('!').next().unwrap_or(piece).trim();
                let name = path.rsplit("::").next().unwrap_or(path).trim();
                let label = if census::std_macro(path, name) { format!("{path} (std)") } else { path.to_string() };
                out.macros.add(label, || place(at));
                note("lower_body: a macro call, not expanded".to_string(), at);
            } else {
                note(format!("lower_body: unhandled {kind}"), at);
            }
            out.unhandled.add(kind, || place(at));
        }
        for diag in view.diags() {
            match diag {
                Ok(diag) => note(format!("lower_body: {:?}", diag.kind), view.text_at(diag.range).map_or(0, offset_of)),
                Err(bad) => note(format!("lower_body: bad row {}", variant(&format!("{bad:?}"))), 0),
            }
        }
        let mut tys = PortableTys::new_in(alloc);
        match to_typeck(&view, ItemKey(0), &mut tys, &mut Unresolved, alloc) {
            Ok(_) => {}
            Err(Refused::NoBody) => {
                out.no_body += 1;
                return;
            }
            Err(Refused::Form { what, range }) => {
                let piece = text.get(range.start as usize..range.end as usize).unwrap_or("");
                let cause = match what {
                    "`yield`" if piece.contains(".await") => "to_typeck: `yield` (an `.await`'s desugaring)".to_string(),
                    "a macro call" => "lower_body: a macro call, not expanded".to_string(),
                    _ => format!("to_typeck: {what}"),
                };
                note(cause, range.start as usize);
            }
            Err(Refused::Bad(bad)) => note(format!("to_typeck: bad row {}", variant(&format!("{bad:?}"))), 0),
            Err(Refused::Check(error)) => note(format!("to_typeck: table {}", variant(&format!("{error:?}"))), 0),
            Err(Refused::Text(error)) => note(format!("to_typeck: text {}", variant(&format!("{error:?}"))), 0),
        }
        match causes.first() {
            None => out.clean += 1,
            Some((cause, at)) => {
                out.first.add(cause.clone(), || place(*at));
                if causes.iter().all(|(c, _)| c == "lower_body: a macro call, not expanded") {
                    out.only_macros += 1;
                }
                for (cause, at) in &causes {
                    out.each.add(cause.clone(), || place(*at));
                }
            }
        }
    }
}

fn bodies(files: &[File], members: &[Member], threads: usize, library: Option<&Path>) {
    println!(
        "h. bodies (blaze-lower's lower_body_expanding, then to_typeck; written types left to inference; body calls expanded by name over std's and the workspace's macros)"
    );
    let library_files = set_body_texts(library, files);
    let (rules, refused_rules) = body_rules();
    match library {
        Some(library) => println!(
            "  expander: {} definitions ({refused_rules} refused by `prepare`) from {library_files} files of {} and the workspace's",
            rules.len(),
            library.display()
        ),
        None => println!("  expander: no library given: std's macros are not defined; {} of the workspace's", rules.len()),
    }
    let edition_of = |file: &File| member_of(members, &file.rel).map_or(Edition::E2024, |(_, m)| m.edition);
    let watch = Watch::start();
    let start = Instant::now();
    let mut all = BodyTally::default();
    for file in files {
        all.merge(lower_bodies(file, edition_of(file)));
    }
    let one = start.elapsed().as_secs_f64();
    let peak = watch.peak();
    let start = Instant::now();
    spread(files.len(), threads, &|at| lower_bodies(&files[at], edition_of(&files[at])).bodies);
    let many = start.elapsed().as_secs_f64();
    let refused = all.first.total();
    println!(
        "  {} bodies: {} clean ({:.1}%), {refused} stopped, {} turned off by cfg, {} with none; {} on 1 thread (with the tally), {} on {threads}; heap peak {peak}",
        all.bodies,
        all.clean,
        100.0 * all.clean as f64 / all.bodies.max(1) as f64,
        all.cfg_out,
        all.no_body,
        ms(one),
        ms(many)
    );
    println!("  body macro calls expanded: {}", all.expanded);
    println!("  stopped only by macro calls (clean once body calls expand): {}", all.only_macros);
    all.each.print("bodies each cause stops (a body under each of its causes)", 25);
    all.first.print("what stops each body first", 10);
    all.macros.print("macro calls left in bodies, every one, by path", 20);
    all.unhandled.print("forms the lowering leaves unhandled, every one", 10);
}

/// One body of stage h' lowered with its calls expanded through `names` over the host `cx`
/// (with or without its build script's outputs laid over it), each unresolved call tallied.
fn scoped_body<'d, C: blaze_expand::ExpandCx>(
    all: &mut BodyTally,
    misses: &mut Causes,
    by_macro: &mut Causes,
    text: &str,
    edition: Edition,
    place: &dyn Fn(usize) -> String,
    names: &'d dyn blaze_lower::by_name::MacroNames<'d>,
    cx: C,
    whose: &BodyCrate<'_>,
) {
    let mut by_name = ByName::with_names(names, cx, Alloc::global());
    lower_one(all, text, edition, place, &mut by_name);
    for finding in by_name.findings() {
        let shown: String = finding.path.split([':', ' ']).filter(|it| !it.is_empty()).collect::<Vec<_>>().join("::");
        by_macro.add(format!("{shown}  --  {}  --  {}", miss_cause(&finding.why), whose.reason()), || {
            format!("{} {}", whose.name, place(finding.at.start as usize))
        });
        misses.add(miss_cause(&finding.why), || shown);
    }
}

/// The crate a body is in, for the table: its name and whether it is a procedural-macro crate
/// (whose own dependencies, the host's build, the names set does not hold).
struct BodyCrate<'n> {
    name: &'n str,
    proc_macro: bool,
    /// A test unit (cfg `test` on): only its `#[cfg(test)]` modules' bodies are its rows.
    test: bool,
}

impl BodyCrate<'_> {
    /// Why a call in this crate's body can name nothing: the crate's kind, else a real miss.
    fn reason(&self) -> &'static str {
        match (self.proc_macro, self.test) {
            (true, _) => "its crate is a macro crate: its dependencies are not in the names set",
            (false, true) => "a real miss (test unit)",
            (false, false) => "a real miss",
        }
    }
}

/// A module entry's first bytes (its range starts at its first outer attribute): at most 400,
/// cut at a char boundary.
fn module_head(text: &str, start: usize, end: usize) -> &str {
    let mut end = end.min(start + 400).min(text.len());
    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(start..end).unwrap_or("")
}

/// The cfg written on a module entry (`#[cfg(test)]`), from its outer attributes: the predicate
/// that turned it off, shown.
fn module_cfg(signature: &str) -> String {
    let head = signature.split("mod ").next().unwrap_or(signature);
    head.find("cfg(")
        .and_then(|at| head[at..].lines().next())
        .map(|line| line.trim_end_matches(']').to_string())
        .unwrap_or_else(|| "(no cfg written: the module's file was not read)".into())
}

/// The cause of a [`blaze_lower::by_name::MacroFinding`], without the path: a tally's key.
fn miss_cause(why: &blaze_lower::by_name::NotExpanded<'_>) -> String {
    use blaze_lower::by_name::{NoMacro, NotExpanded};
    use blaze_scope::resolve::MacroDefError;
    let NotExpanded::NoMacro(why) = why else { return format!("{why}") };
    match why {
        NoMacro::Names(MacroDefError::Path(error)) => format!("path: {:?}", error.why),
        NoMacro::Names(MacroDefError::NoSource { .. }) => "no source: a procedural macro, or not read".into(),
        NoMacro::Names(MacroDefError::Refused(note)) => format!("refused: {}", variant(&format!("{note:?}"))),
        other => format!("{other}"),
    }
}


/// Stage h again, over the members' crates of the workspace's names: every fn, const and
/// static of every file each crate's module walk read, lowered with its calls resolved through
/// [`BodyNames`] for its module.
fn bodies_named(root: &Path, store: &Path, modules: Option<(&Path, &Path)>, scripts: Option<&Path>, tests: bool, heap: &'static blaze_own::Heap) {
    use blaze_scope::resolve::At;
    use blaze_surface::ItemIndex;
    println!("h'. bodies, every body call resolved through the workspace's names (blaze-scope `Names::item_at`)");
    let watch = Watch::start();
    let loaded = match names_load::load(root, store, modules, scripts, tests, heap) {
        Ok(loaded) => loaded,
        Err(error) => {
            println!("  stopped: {error}");
            return;
        }
    };
    let names = loaded.names;
    println!("  names: {} crates resolved in {}; heap peak {}", names.crates().len(), ms(loaded.resolve_ms / 1e3), watch.peak());
    let mut misses = Causes::default();
    // Every body call not expanded, keyed by its macro path and typed cause: the lane's list.
    let mut by_macro = Causes::default();
    let mut all = BodyTally::default();
    let (mut files_read, mut files_no_module) = (0usize, 0usize);
    // Bodies in a module the unit's cfg turns off (not part of the unit), and stopped bodies by
    // whether their crate is a macro crate.
    let (mut module_off, mut stopped_macro_crate, mut stopped_other) = (0usize, 0usize, 0usize);
    let mut off_modules = Causes::default();
    // `--tests`: the test units' own rows (bodies under `#[cfg(test)]` modules), and how many are clean.
    let (mut test_bodies, mut test_clean) = (0usize, 0usize);
    let Ok(prep_frame) = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer) else {
        return;
    };
    let prep_erased = blaze_own::Erased::new(&prep_frame);
    let prepared = blaze_lower::scoped::Prepares::new_in(prep_erased.bump());
    let procs_tally = ProcTally::default();
    let start = Instant::now();
    for (k, c) in names.crates().iter().enumerate() {
        if !loaded.member[k] {
            continue;
        }
        // Each file's own module: the walked module whose text is the whole file.
        let mut file_module: HashMap<u32, blaze_id::ItemKey> = HashMap::new();
        for m in names.modules(k) {
            if let (At::File(f), None) = (m.at, m.item) {
                file_module.insert(f, m.key);
            }
        }
        let edition = names.edition(k);
        let unit = &loaded.crates.crates[k];
        let flag = |name: &str| unit.cfg.iter().any(|(atom, value)| *atom == name && value.is_none());
        let whose = BodyCrate { name: unit.name, proc_macro: flag("proc_macro"), test: flag("test") };
        for (f, file) in c.files.iter().enumerate() {
            let f = f as u32;
            let Some(&own) = file_module.get(&f) else {
                files_no_module += 1;
                continue;
            };
            files_read += 1;
            let outline = &file.outline;
            for (i, item) in outline.items().iter().enumerate() {
                let has_body = matches!(item.detail, ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. });
                if !has_body || item.body.is_none() {
                    continue;
                }
                let base = item.range.start as usize;
                let Some(text) = file.text.get(base..item.range.end as usize) else { continue };
                // The module the body is written in: its nearest keyed ancestor's (an inline
                // `mod` is itself the module), else the file's.
                let mut module = own;
                // An enclosing `mod` the walk did not key: the unit's cfg turned it off.
                let mut off: Option<String> = None;
                let mut at = Some(ItemIndex(i as u32));
                while let Some(idx) = at {
                    let entry = outline.item(idx);
                    let keyed = names.key_at(k, At::File(f), idx);
                    if keyed.is_none() && idx.0 != i as u32 && matches!(entry.detail, ItemDetail::Mod { .. }) {
                        off = Some(module_cfg(module_head(file.text, entry.range.start as usize, entry.range.end as usize)));
                        break;
                    }
                    if let Some(key) = keyed {
                        let found = match entry.detail {
                            ItemDetail::Mod { .. } if idx.0 != i as u32 => Some(key),
                            _ => names.module_of(key),
                        };
                        if let Some(found) = found {
                            module = found;
                            break;
                        }
                    }
                    at = entry.parent;
                }
                let place = |offset: usize| format!("{}:{}", file.shown, line_of(file.text, base + offset));
                if let Some(cfg) = off {
                    // Not part of this unit: counted apart, never "stopped" (once: by the library
                    // unit, not again by its test unit).
                    if !whose.test {
                        module_off += 1;
                        off_modules.add(format!("{cfg}  --  {}", whose.reason()), || format!("{} {}", whose.name, place(0)));
                    }
                    continue;
                }
                if whose.test {
                    // A test unit's rows: bodies under a module written `#[cfg(test)]` (the rest
                    // are the library unit's, counted there).
                    let mut under_test = false;
                    let mut up = outline.item(ItemIndex(i as u32)).parent;
                    while let Some(idx) = up {
                        let entry = outline.item(idx);
                        if matches!(entry.detail, ItemDetail::Mod { .. }) {
                            under_test |= module_cfg(module_head(file.text, entry.range.start as usize, entry.range.end as usize)).contains("test");
                        }
                        up = entry.parent;
                    }
                    if !under_test {
                        continue;
                    }
                    test_bodies += 1;
                }
                all.bodies += 1;
                let (stopped_before, clean_before) = (all.first.total(), all.clean);
                let resolver = Scoped { names, module, prepared: Some(prepared) };
                let script = loaded.scripts[k];
                let host = BodyHost {
                    procs: loaded.procs,
                    krate: k,
                    build_script: loaded.crates.crates[k].build_script && script.is_none(),
                    tally: Some(&procs_tally),
                    file: Some((file.shown, &loaded)),
                };
                match script {
                    Some(out) => scoped_body(&mut all, &mut misses, &mut by_macro, text, edition, &place, &resolver, blaze_build_script::cx::ScriptCx { inner: host, out }, &whose),
                    None => scoped_body(&mut all, &mut misses, &mut by_macro, text, edition, &place, &resolver, host, &whose),
                }
                if all.first.total() > stopped_before {
                    if whose.proc_macro { stopped_macro_crate += 1 } else { stopped_other += 1 }
                }
                if whose.test && all.clean > clean_before {
                    test_clean += 1;
                }
            }
        }
    }
    let took = start.elapsed().as_secs_f64();
    let refused = all.first.total();
    println!(
        "  {files_read} files of the members' crates ({files_no_module} not in a walked module); {} bodies: {} clean ({:.1}%), {refused} stopped, {} turned off by cfg, {} with none; {} on 1 thread",
        all.bodies,
        all.clean,
        100.0 * all.clean as f64 / all.bodies.max(1) as f64,
        all.cfg_out,
        all.no_body,
        ms(took)
    );
    // The two rows the owner is told: each over a set fixed by its definition, printed beside it.
    println!(
        "  ROW library and binary targets: {} bodies: {} clean (set: every fn, const and static body with a body, of every file the module walk reaches from the workspace members' lib, bin and proc-macro roots, cfg(test) off, through `Scoped` with all macros)",
        all.bodies, all.clean
    );
    println!(
        "  ROW test, bench and example targets: not measured (set: every body of the files only the members' test harnesses (cfg(test)), tests/, benches/ and examples/ targets reach; missing: blaze-unit-graph has no unit for an integration test, bench or example target (lib.rs `# Not covered`; resolve.rs `Kind` has TestLib and TestBin only), and names_load resolves with `tests: false`)"
    );
    println!(
        "  NO SCOPE: {module_off} bodies turned off by cfg (in a module the unit's cfg turns off: not part of the unit, not counted above); stopped: {stopped_macro_crate} in macro crates (their dependencies are not in the names set), {stopped_other} elsewhere"
    );
    off_modules.print("modules the unit's cfg turns off, by their cfg (bodies in them)", 20);
    if tests {
        println!(
            "  ROW test units (--tests): {test_bodies} bodies: {test_clean} clean (set: the bodies under a `#[cfg(test)]` module of each member's library compiled with `test` on and its dev-dependencies; included in the totals above)"
        );
    }
    println!("  body macro calls expanded: {}", all.expanded);
    println!("  `macro_rules!` and `macro` definitions prepared (each once): {}", prepared.len());
    println!(
        "  body proc-macro calls: {} ran in the sandbox or came from a record, {} refused, {} failed",
        procs_tally.ran.get(),
        procs_tally.refused.get(),
        procs_tally.failed.get()
    );
    if let Some(procs) = loaded.procs {
        let registered = procs.procs.crates().count();
        let with_module = procs.procs.crates().filter(|c| c.2).count();
        println!("  proc-macro crates: {registered} registered, {with_module} with a module");
        // The crates' results tables are written here: without it the next run finds nothing.
        println!("  proc-macro results kept: {:?}", procs.procs.keep_all().map_err(|error| error.to_string()));
        for finding in procs.procs.take_findings() {
            println!("    proc-macro finding: {finding}");
        }
    }
    println!("  stopped only by macro calls: {}", all.only_macros);
    all.each.print("bodies each cause stops (a body under each of its causes)", 25);
    all.first.print("what stops each body first", 10);
    all.macros.print("macro calls left in bodies, every one, by path", 25);
    misses.print("why a call's path named no definition, by cause (every call asked)", 15);
    by_macro.print("BY MACRO AND CAUSE: every body call not expanded, by its path and typed cause", 200);
    all.unhandled.print("forms the lowering leaves unhandled, every one", 10);
}
