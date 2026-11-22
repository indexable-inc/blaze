//! Runs the new item layer (docs/v2/plan/design/bottom.md) end to end on real crates and times
//! each stage: `pipeline <crate dir>...`, each directory one crate (its `src/lib.rs`, `lib.rs`
//! or `src/main.rs` the root). The numbers bottom.md section 17 lists as unmeasured come from
//! here: the whole first fill of a large crate, the sizes of the import cycles, the rounds.
//!
//! The stages, each on 1, 2, 4, 8 and the machine's thread count where it runs in parallel
//! (an atomic next index shared by the threads), best of 3:
//!
//! 1. outline every `.rs` file under the directory to a `FileItems` record;
//! 2. walk the modules from the root (`mod x { .. }`, `x.rs`, `x/mod.rs`, `#[path]` on both)
//!    and lower each module's items to its `ModuleDecls`;
//! 3. the crate's `CrateRoot`;
//! 4. the names first fill: every module's `ModuleScope` through `blaze_scope::Resolver`, the
//!    records read back from a `MemSurface`, in rounds (each round steps every waiting module
//!    against the scopes the earlier rounds finished, in parallel; a round that finishes
//!    nothing groups the cycles of the wait graph); item macros expanded by blaze-expand;
//! 5. the impl index and the lang items.
//!
//! What it cannot do it counts, by kind, with the first three examples. A measurement and a
//! smoke run (rule 82), not a test: nothing here is a reference for the layer's semantics.

#![feature(allocator_api)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Component, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use blaze_expand::{
    Call, CallSite, Ctx, DefAt, DefSite, Delim, Diagnostic, ExpandCx, Expanded, Export,
    HostAsk, HostError, HostWhy, IncludeAs, Included, LitKind, LocalInner, MacroDefinition, Place,
    PlaceOf, ProcCall, Produced, Raw, Spacing, TokenKind, TokenStream, expand, lex,
};
use blaze_file_items::{Outline, outline, syntax_kind};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_scope::{
    Answer, ExpandRequest, FillStats, Group, NameAnswer, Node, Passed, Read, RecordFacts, Reply, ResolveError,
    Resolved, Resolver, ScopeFacts, Sources, Step, Touched, through,
};
use blaze_surface::testing::{Fact, MemSurface};
use blaze_surface::{
    AttrKind, BindingState, BuiltinAttr, BuiltinDerive, BuiltinFn, CallShape, ChildModDecl,
    Binding, CrateRootBuilder, CrateRootHead, DeclaredItem, DeclsBuilder, Def, DefKind, Edition,
    ExternCrateDecl, ExternTarget, FloatWidth, GlobEdge, ImplFlags, ImplRow, IntWidth, Item,
    ItemDetail, ItemIndex, LangName, MacroCallDecl, MacroRulesDecl, MacroShapes, ModuleInfo, Mutability,
    Namespace, Path, PathKind, Rename, ScopeError, SelfShape, Shape, SurfaceError, TextualDef,
    UintWidth, UseDecl, UseForm, Visibility, WrittenVis, build_impl_index, build_lang_items,
    globs_digest, textual_digest,
};

/// Every name is a slice of a text leaked for the run: a source file or an expansion.
type N = &'static str;

/// Steps one machine may take in one round before the driver calls it stuck.
const STEP_CAP: usize = 1_000_000;

/// The one allocator of the run: the global heap's handle, named at the program's top.
fn g() -> Alloc<'static> {
    Alloc::global()
}

/// A value that crosses threads although its type is not `Send` or `Sync` only because it
/// holds `Alloc` handles (`Outline`, `DeclsBuilder`, `Resolver`, `Group`, `Resolved`).
///
/// Every `OnGlobal` is made around values whose every allocator handle is [`g`]'s, the
/// process's global heap, which any thread may allocate from and free to at once; `Alloc` is
/// neither `Send` nor `Sync` only because another handle may stand for a frame one thread owns
/// (blaze-own's `alloc_ref`). The types wrapped hold no `Rc`, `Cell` or other thread-bound
/// state (blaze-surface, blaze-scope, blaze-file-items and blaze-expand have none outside
/// their tests), so moving one to another thread, or reading one from several, is sound.
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

impl<T: Default> Default for OnGlobal<T> {
    fn default() -> OnGlobal<T> {
        OnGlobal(T::default())
    }
}

impl<T> std::ops::DerefMut for OnGlobal<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

fn main() {
    let usage = "usage: pipeline [--threads 1,8,18|all] [--repeat N] [--examples N] <crate dir>...  (each directory one crate; default: one thread, one run)";
    let most = std::thread::available_parallelism().map_or(1, |it| it.get());
    let mut counts: Vec<usize> = vec![1];
    let mut dirs: Vec<PathBuf> = Vec::new();
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
            "--repeat" => REPEAT.store(args.next().and_then(|it| it.parse().ok()).unwrap_or(1), Ordering::Relaxed),
            "--show" => {
                let _ = SHOW.set(args.next().unwrap_or_default());
            }
            "--edit" => EDIT.store(true, Ordering::Relaxed),
            "--examples" => EXAMPLES.store(args.next().and_then(|it| it.parse().ok()).unwrap_or(3), Ordering::Relaxed),
            "--dump" => {
                let dir = PathBuf::from(args.next().unwrap_or_default());
                std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
                let _ = DUMP.set(dir);
            }
            _ => dirs.push(PathBuf::from(arg)),
        }
    }
    if dirs.is_empty() || counts.is_empty() {
        eprintln!("{usage}");
        std::process::exit(2);
    }
    for dir in &dirs {
        run_crate(dir, &counts);
    }
    println!("\npeak memory (getrusage max RSS): {:.1} MB", peak_rss_mb());
}

// ---------------------------------------------------------------------------------------------
// Failures, locations, timing.

/// Failures by kind: a count and the first three examples.
#[derive(Default)]
struct Failures {
    by: BTreeMap<String, (usize, Vec<String>)>,
}

impl Failures {
    fn add(&mut self, kind: impl Into<String>, example: impl FnOnce() -> String) {
        let entry = self.by.entry(kind.into()).or_default();
        entry.0 += 1;
        if entry.1.len() < example_cap() {
            entry.1.push(example());
        }
    }

    fn merge(&mut self, other: Failures) {
        for (kind, (count, examples)) in other.by {
            let entry = self.by.entry(kind).or_default();
            entry.0 += count;
            for example in examples {
                if entry.1.len() < example_cap() {
                    entry.1.push(example);
                }
            }
        }
    }

    fn total(&self) -> usize {
        self.by.values().map(|it| it.0).sum()
    }

    fn print(&self) {
        if self.by.is_empty() {
            println!("    failures: none");
            return;
        }
        println!("    failures: {} in {} kinds", self.total(), self.by.len());
        let mut kinds: Vec<_> = self.by.iter().collect();
        kinds.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
        for (kind, (count, examples)) in kinds {
            println!("      {count:>7}  {kind}");
            for example in examples {
                println!("               e.g. {example}");
            }
        }
    }
}

/// A place in a source file: the file's index and a byte offset. Turned into `file:line` only
/// when printed.
#[derive(Clone, Copy, Debug)]
struct Loc {
    file: u32,
    offset: u32,
}

/// How many times each stage runs at each thread count (`--repeat`, default 1); the best
/// wall time is printed.
static REPEAT: AtomicUsize = AtomicUsize::new(1);
/// Examples kept per failure kind (`--examples N`).
static EXAMPLES: AtomicUsize = AtomicUsize::new(3);
/// A name whose bindings in every scope are printed (`--show NAME`).
static SHOW: std::sync::OnceLock<String> = std::sync::OnceLock::new();
/// A directory each record written is appended to (`--dump DIR`): `<kind>.recs`, each record a
/// little-endian `u32` length and its bytes, for blaze-columns' `squeeze` measurement.
static DUMP: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Appends `bytes` to `DIR/<kind>.recs` when `--dump DIR` was given.
fn dump(kind: &str, bytes: &[u8]) {
    use std::io::Write as _;
    let Some(dir) = DUMP.get() else { return };
    let path = dir.join(format!("{kind}.recs"));
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let len = u32::try_from(bytes.len()).expect("a record under 4 GiB");
    file.write_all(&len.to_le_bytes()).and_then(|()| file.write_all(bytes)).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

fn example_cap() -> usize { EXAMPLES.load(Ordering::Relaxed) }

/// Runs `f` `REPEAT` times; the best wall time in milliseconds and the last result.
fn best_of_3<T>(mut f: impl FnMut() -> T) -> (f64, T) {
    let mut best = f64::MAX;
    let mut last = None;
    for _ in 0..REPEAT.load(Ordering::Relaxed).max(1) {
        // The last result is dropped before the next run starts, so a repeat's peak is its own.
        drop(last.take());
        let start = Instant::now();
        let value = f();
        best = best.min(start.elapsed().as_secs_f64() * 1e3);
        last = Some(value);
    }
    (best, last.expect("one run at least"))
}

// The counting allocator: the system heap with three counters, so a stage's bytes are its
// own and not the process's (`getrusage`'s peak RSS counts every stage before it too).

/// Bytes allocated and not yet freed.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// The most `LIVE` has been since the last `Meter::start`.
static PEAK: AtomicUsize = AtomicUsize::new(0);
/// Bytes ever allocated (a realloc counts its new size).
static TOTAL: AtomicUsize = AtomicUsize::new(0);

struct Counting;

impl Counting {
    fn up(size: usize) {
        TOTAL.fetch_add(size, Ordering::Relaxed);
        let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}

// SAFETY: every call is forwarded to `System` unchanged; the counters are only atomics.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc`, passed on.
        let p = unsafe { std::alloc::System.alloc(layout) };
        if !p.is_null() {
            Counting::up(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc_zeroed`, passed on.
        let p = unsafe { std::alloc::System.alloc_zeroed(layout) };
        if !p.is_null() {
            Counting::up(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller's contract for `dealloc`, passed on.
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new: usize) -> *mut u8 {
        // SAFETY: the caller's contract for `realloc`, passed on.
        let p = unsafe { std::alloc::System.realloc(ptr, layout, new) };
        if !p.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            Counting::up(new);
        }
        p
    }
}

#[global_allocator]
static HEAP: Counting = Counting;

/// One stage's memory: what it allocated, the most it held at once above what was live when
/// it started, and what it left live.
struct Meter {
    total0: usize,
    live0: usize,
}

impl Meter {
    fn start() -> Meter {
        let live0 = LIVE.load(Ordering::Relaxed);
        PEAK.store(live0, Ordering::Relaxed);
        Meter { total0: TOTAL.load(Ordering::Relaxed), live0 }
    }

    fn print(&self) {
        let mb = |b: usize| b as f64 / 1e6;
        let live = LIVE.load(Ordering::Relaxed);
        println!(
            "    memory: allocated {:.1} MB, peak live {:.1} MB above the {:.1} MB live at the start, left live {:+.1} MB",
            mb(TOTAL.load(Ordering::Relaxed) - self.total0),
            mb(PEAK.load(Ordering::Relaxed).saturating_sub(self.live0)),
            mb(self.live0),
            (live as f64 - self.live0 as f64) / 1e6
        );
    }
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

fn print_times(times: &[(usize, f64)]) {
    let one = times.first().map_or(1.0, |it| it.1);
    for &(threads, ms) in times {
        println!(
            "    threads {threads:>3}: {ms:>9.1} ms best of {}   {:.2}x of the first count",
            REPEAT.load(Ordering::Relaxed).max(1),
            one / ms
        );
    }
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

// ---------------------------------------------------------------------------------------------
// Keys.

use blaze_scope::task::{key_of, make_key, root_key};

// ---------------------------------------------------------------------------------------------
// cfg: one stated configuration.

/// Names a predicate may hold that this run takes as false on purpose (not counted as unknown).
const KNOWN_FALSE: &[&str] = &[
    "test", "debug_assertions", "doc", "doctest", "miri", "bootstrap", "rustdoc", "clippy",
    "no_global_oom_handling", "no_rc", "no_sync", "overflow_checks", "ub_checks", "fuzzing",
    "kani", "emscripten_wasm_eh", "windows", "target_has_reliable_f16",
    "target_has_reliable_f16_math", "target_has_reliable_f128", "target_has_reliable_f128_math",
    "sanitize", "panic_immediate_abort", "optimize_for_size", "no_fp_fmt_parse", "feature",
];

struct Cfg {
    names: HashSet<String>,
    pairs: HashSet<(String, String)>,
    keys: HashSet<String>,
    /// Names and keys a predicate asked about that the configuration does not set, with how
    /// often: each was taken as false.
    unknown: Mutex<BTreeMap<String, usize>>,
    unparsed: AtomicUsize,
}

impl Cfg {
    fn host(features: &[String]) -> Cfg {
        let mut cfg = Cfg {
            names: HashSet::new(),
            pairs: HashSet::new(),
            keys: HashSet::new(),
            unknown: Mutex::new(BTreeMap::new()),
            unparsed: AtomicUsize::new(0),
        };
        let family = std::env::consts::FAMILY;
        cfg.name(family);
        cfg.name("target_thread_local");
        cfg.pair("target_os", std::env::consts::OS);
        cfg.pair("target_arch", std::env::consts::ARCH);
        cfg.pair("target_family", family);
        cfg.pair("target_pointer_width", if cfg!(target_pointer_width = "64") { "64" } else { "32" });
        cfg.pair("target_endian", if cfg!(target_endian = "little") { "little" } else { "big" });
        let env = if cfg!(target_env = "gnu") {
            "gnu"
        } else if cfg!(target_env = "musl") {
            "musl"
        } else {
            ""
        };
        cfg.pair("target_env", env);
        cfg.pair("target_vendor", if cfg!(target_vendor = "apple") { "apple" } else { "unknown" });
        cfg.pair("target_abi", "");
        cfg.pair("panic", "unwind");
        cfg.pair("fmt_debug", "full");
        cfg.pair("relocation_model", "pic");
        let widths: [(&str, bool); 6] = [
            ("8", cfg!(target_has_atomic = "8")),
            ("16", cfg!(target_has_atomic = "16")),
            ("32", cfg!(target_has_atomic = "32")),
            ("64", cfg!(target_has_atomic = "64")),
            ("128", false),
            ("ptr", cfg!(target_has_atomic = "ptr")),
        ];
        for (width, on) in widths {
            if on {
                cfg.pair("target_has_atomic", width);
                cfg.pair("target_has_atomic_load_store", width);
                cfg.pair("target_has_atomic_equal_alignment", width);
            }
        }
        cfg.name("target_has_atomic");
        cfg.name("target_has_atomic_load_store");
        cfg.name("target_has_atomic_equal_alignment");
        let features_on: [(&str, bool); 14] = [
            ("neon", cfg!(target_feature = "neon")),
            ("fp-armv8", cfg!(target_feature = "fp-armv8")),
            ("aes", cfg!(target_feature = "aes")),
            ("sha2", cfg!(target_feature = "sha2")),
            ("crc", cfg!(target_feature = "crc")),
            ("lse", cfg!(target_feature = "lse")),
            ("sse", cfg!(target_feature = "sse")),
            ("sse2", cfg!(target_feature = "sse2")),
            ("sse3", cfg!(target_feature = "sse3")),
            ("ssse3", cfg!(target_feature = "ssse3")),
            ("sse4.1", cfg!(target_feature = "sse4.1")),
            ("sse4.2", cfg!(target_feature = "sse4.2")),
            ("avx", cfg!(target_feature = "avx")),
            ("avx2", cfg!(target_feature = "avx2")),
        ];
        for (feature, on) in features_on {
            if on {
                cfg.pair("target_feature", feature);
            }
        }
        cfg.keys.insert("target_feature".to_string());
        cfg.keys.insert("target_has_atomic".to_string());
        cfg.keys.insert("target_has_atomic_load_store".to_string());
        cfg.keys.insert("target_has_atomic_equal_alignment".to_string());
        for feature in features {
            cfg.pair("feature", feature);
        }
        cfg.keys.insert("feature".to_string());
        cfg
    }

    fn name(&mut self, name: &str) {
        self.names.insert(name.to_string());
    }

    fn pair(&mut self, key: &str, value: &str) {
        self.keys.insert(key.to_string());
        self.pairs.insert((key.to_string(), value.to_string()));
    }

    fn describe(&self) -> String {
        let mut names: Vec<&String> = self.names.iter().collect();
        names.sort();
        let mut pairs: Vec<String> = self.pairs.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
        pairs.sort();
        format!(
            "names [{}]; pairs [{}]; false on purpose: [{}]; any other name or key is false and counted",
            names.iter().map(|it| it.as_str()).collect::<Vec<_>>().join(", "),
            pairs.join(", "),
            KNOWN_FALSE.join(", ")
        )
    }

    fn has_name(&self, name: &str) -> bool {
        match name {
            "true" => return true,
            "false" => return false,
            _ => {}
        }
        if self.names.contains(name) {
            return true;
        }
        if !KNOWN_FALSE.contains(&name) {
            *self.unknown.lock().expect("the cfg counts' lock").entry(name.to_string()).or_default() += 1;
        }
        false
    }

    fn has_pair(&self, key: &str, value: &str) -> bool {
        if self.pairs.contains(&(key.to_string(), value.to_string())) {
            return true;
        }
        if !self.keys.contains(key) && !KNOWN_FALSE.contains(&key) {
            *self
                .unknown
                .lock()
                .expect("the cfg counts' lock")
                .entry(format!("{key} = ..."))
                .or_default() += 1;
        }
        false
    }

    /// The predicate's value; a predicate this parser does not read is false and counted.
    fn eval(&self, text: &str) -> bool {
        let tokens = cfg_tokens(text);
        let mut at = 0;
        match tokens.as_deref().and_then(|t| self.pred(t, &mut at, "").filter(|_| at == t.len())) {
            Some(value) => value,
            None => {
                self.unparsed.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    fn pred(&self, t: &[CTok<'_>], at: &mut usize, prefix: &str) -> Option<bool> {
        let mut stack: Vec<(&str, &str, Vec<bool>)> = Vec::new();
        let mut cur_prefix = prefix;
        loop {
            let CTok::Ident(name) = *t.get(*at)? else { return None };
            *at += 1;
            let mut val = match t.get(*at) {
                Some(CTok::Open) => {
                    *at += 1;
                    let inner = if name == "target" { "target_" } else { cur_prefix };
                    if t.get(*at) == Some(&CTok::Close) {
                        *at += 1;
                        match name {
                            "all" | "target" => true,
                            "any" => false,
                            _ => return None,
                        }
                    } else {
                        stack.push((name, inner, Default::default()));
                        cur_prefix = inner;
                        continue;
                    }
                }
                Some(CTok::Eq) => {
                    *at += 1;
                    let Some(CTok::Str(value)) = t.get(*at).copied() else { return None };
                    *at += 1;
                    self.has_pair(&format!("{cur_prefix}{name}"), value)
                }
                _ => self.has_name(&format!("{cur_prefix}{name}")),
            };
            loop {
                let Some((_, inner_prefix, values)) = stack.last_mut() else {
                    return Some(val);
                };
                values.push(val);
                match t.get(*at) {
                    Some(CTok::Comma) => {
                        *at += 1;
                        if t.get(*at) == Some(&CTok::Close) {
                            *at += 1;
                        } else {
                            cur_prefix = *inner_prefix;
                            break;
                        }
                    }
                    Some(CTok::Close) => {
                        *at += 1;
                    }
                    _ => return None,
                }
                let Some((op, _, values)) = stack.pop() else { return None };
                val = match op {
                    "all" | "target" => values.iter().all(|it| *it),
                    "any" => values.iter().any(|it| *it),
                    "not" if values.len() == 1 => !values[0],
                    _ => return None,
                };
            }
        }
    }

    /// Reads one attribute's content (`cfg(..)`, `path = ".."`, `derive(..)`, ...) into
    /// `attrs`, a true `cfg_attr`'s attributes in its place.
    fn attr(&self, attrs: &mut Attrs, content: &'static str) {
        let mut work = split_top("");
        work.clear();
        work.push(content);
        while let Some(content) = work.pop() {
            let name = head(content);
            match name {
                "cfg" => {
                    if !self.eval(args(content)) {
                        attrs.cfg_ok = false;
                    }
                }
                "cfg_attr" => {
                    attrs.cfg_attrs += 1;
                    let parts = split_top(args(content));
                    if let Some((predicate, rest)) = parts.split_first() {
                        if self.eval(predicate) {
                            for part in rest.iter().rev() {
                                let part = part.trim();
                                if !part.is_empty() {
                                    work.push(part);
                                }
                            }
                        }
                    }
                }
                "unsafe" => work.push(args(content).trim()),
                "path" => attrs.path = string_value(content),
                "derive" | "derive_const" => {
                    for path in split_top(args(content)) {
                        let path = path.trim();
                        if !path.is_empty() {
                            attrs.derives.push(path);
                        }
                    }
                }
                "macro_export" => {
                    attrs.macro_export = true;
                    attrs.local_inner |= args(content).contains("local_inner_macros");
                }
                "macro_use" => attrs.macro_use = true,
                "lang" => attrs.lang = string_value(content),
                "rustc_builtin_macro" => {
                    attrs.builtin = true;
                    let arg = split_top(args(content)).first().map(|it| it.trim()).unwrap_or("");
                    if !arg.is_empty() {
                        attrs.builtin_name = Some(arg);
                    }
                }
                "prelude_import" => attrs.prelude_import = true,
                "no_implicit_prelude" => attrs.no_implicit_prelude = true,
                "no_std" => attrs.no_std = true,
                "no_core" => attrs.no_core = true,
                "register_tool" => {
                    for tool in split_top(args(content)) {
                        let tool = tool.trim();
                        if !tool.is_empty() {
                            attrs.tools.push(tool);
                        }
                    }
                }
                _ => {
                    attrs.arbitrary_ints |=
                        name.split_once("::").is_some_and(|(tool, rest)| tool.trim() == "blaze" && rest.trim() == "arbitrary_ints");
                    if !is_builtin_attr(name) {
                        attrs.others.push(name);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum CTok<'a> {
    Ident(&'a str),
    Str(&'a str),
    Open,
    Close,
    Comma,
    Eq,
}

fn cfg_tokens(text: &str) -> Option<Vec<CTok<'_>>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let c = bytes[at];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => at += 1,
            b'(' => {
                out.push(CTok::Open);
                at += 1;
            }
            b')' => {
                out.push(CTok::Close);
                at += 1;
            }
            b',' => {
                out.push(CTok::Comma);
                at += 1;
            }
            b'=' => {
                out.push(CTok::Eq);
                at += 1;
            }
            b'"' => {
                let start = at + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end] != b'"' {
                    end += if bytes[end] == b'\\' { 2 } else { 1 };
                }
                if end >= bytes.len() {
                    return None;
                }
                out.push(CTok::Str(&text[start..end]));
                at = end + 1;
            }
            _ if c == b'_' || c.is_ascii_alphanumeric() => {
                let start = at;
                while at < bytes.len() && (bytes[at] == b'_' || bytes[at].is_ascii_alphanumeric()) {
                    at += 1;
                }
                out.push(CTok::Ident(&text[start..at]));
            }
            _ => return None,
        }
    }
    Some(out)
}

/// What one item's attributes say, a true `cfg_attr`'s applied.
struct Attrs {
    cfg_ok: bool,
    cfg_attrs: u32,
    path: Option<&'static str>,
    derives: Vec<&'static str>,
    macro_export: bool,
    local_inner: bool,
    macro_use: bool,
    lang: Option<&'static str>,
    builtin: bool,
    builtin_name: Option<&'static str>,
    prelude_import: bool,
    no_implicit_prelude: bool,
    no_std: bool,
    no_core: bool,
    /// `#![blaze::arbitrary_ints]`.
    arbitrary_ints: bool,
    tools: Vec<&'static str>,
    /// Attribute names that are neither built in nor a tool's: attribute macros, not lowered.
    others: Vec<&'static str>,
}

impl Attrs {
    fn new() -> Attrs {
        Attrs {
            cfg_ok: true,
            cfg_attrs: 0,
            path: None,
            derives: Vec::new(),
            macro_export: false,
            local_inner: false,
            macro_use: false,
            lang: None,
            builtin: false,
            builtin_name: None,
            prelude_import: false,
            no_implicit_prelude: false,
            no_std: false,
            no_core: false,
            arbitrary_ints: false,
            tools: Vec::new(),
            others: Vec::new(),
        }
    }
}

const BUILTIN_ATTRS: &[&str] = &[
    "allow", "automatically_derived", "cfg", "cfg_attr", "cfg_eval", "cold", "collapse_debuginfo",
    "coverage", "crate_name", "crate_type", "debugger_visualizer", "default", "deny", "deprecated",
    "derive", "diagnostic", "doc", "expect", "export_name", "feature", "forbid", "global_allocator",
    "ignore", "inline", "instruction_set", "lang", "link", "link_name", "link_ordinal",
    "link_section", "linkage", "macro_escape", "macro_export", "macro_use", "must_not_suspend",
    "must_use", "naked", "no_builtins", "no_core", "no_implicit_prelude", "no_link", "no_main",
    "no_mangle", "no_std", "non_exhaustive", "optimize", "panic_handler", "path", "prelude_import",
    "proc_macro", "proc_macro_attribute", "proc_macro_derive", "recursion_limit", "register_tool",
    "repr", "should_panic", "stable", "target_feature", "test", "thread_local", "track_caller",
    "type_length_limit", "unsafe", "unstable", "used", "warn", "windows_subsystem", "const_trait",
    "derive_const", "fundamental", "marker", "may_dangle", "ffi_const", "ffi_pure",
    "omit_gdb_pretty_printer_section", "allow_internal_unstable", "allow_internal_unsafe",
    "pointee", "unstable_feature_bound", "align", "sanitize", "loop_match", "const_continue",
    "patchable_function_entry", "define_opaque", "type_const", "coroutine", "custom_mir",
    "do_not_recommend", "on_unimplemented", "rustfmt", "clippy",
];

fn is_builtin_attr(name: &str) -> bool {
    name.starts_with("rustc_") || name.contains("::") || BUILTIN_ATTRS.contains(&name)
}

/// The attribute's content: `#[a(b)]` and `#![a(b)]` give `a(b)`.
fn strip_attr(raw: &'static str) -> &'static str {
    let s = raw.trim();
    let s = s.strip_prefix('#').unwrap_or(s).trim_start();
    let s = s.strip_prefix('!').unwrap_or(s).trim_start();
    let s = s.strip_prefix('[').unwrap_or(s);
    let s = s.strip_suffix(']').unwrap_or(s);
    s.trim()
}

/// The path an attribute's content starts with.
fn head(content: &str) -> &str {
    let end = content
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
        .unwrap_or(content.len());
    &content[..end]
}

/// What is inside an attribute's outer parentheses, or nothing.
fn args(content: &'static str) -> &'static str {
    match (content.find('('), content.rfind(')')) {
        (Some(open), Some(close)) if close > open => &content[open + 1..close],
        _ => "",
    }
}

/// The first string literal's inside.
fn string_value(content: &'static str) -> Option<&'static str> {
    let open = content.find('"')?;
    let rest = &content[open + 1..];
    let close = rest.find('"')?;
    Some(&rest[..close])
}

/// `text` split at the commas outside brackets and strings.
fn split_top(text: &'static str) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let c = bytes[at];
        if in_str {
            if c == b'\\' {
                at += 1;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'(' | b'[' | b'{' | b'<' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                b'>' if at == 0 || bytes[at - 1] != b'-' => depth -= 1,
                b',' if depth == 0 => {
                    out.push(&text[start..at]);
                    start = at + 1;
                }
                _ => {}
            }
        }
        at += 1;
    }
    if start < text.len() && !text[start..].trim().is_empty() {
        out.push(&text[start..]);
    }
    out
}

fn unraw(name: &'static str) -> &'static str {
    name.strip_prefix("r#").unwrap_or(name)
}

// ---------------------------------------------------------------------------------------------
// Paths and `use` trees.

#[derive(Clone, Copy, PartialEq, Debug)]
enum PTok {
    Ident(&'static str),
    Colons,
    Open,
    Close,
    Comma,
    Star,
}

fn path_tokens(text: &'static str) -> Option<Vec<PTok>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let c = bytes[at];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => at += 1,
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                let end = text[at + 2..].find("*/")?;
                at += end + 4;
            }
            b':' if bytes.get(at + 1) == Some(&b':') => {
                out.push(PTok::Colons);
                at += 2;
            }
            b'{' => {
                out.push(PTok::Open);
                at += 1;
            }
            b'}' => {
                out.push(PTok::Close);
                at += 1;
            }
            b',' => {
                out.push(PTok::Comma);
                at += 1;
            }
            b'*' => {
                out.push(PTok::Star);
                at += 1;
            }
            _ if c == b'$' || c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80 => {
                let start = at;
                at += 1;
                if c == b'r' && bytes.get(at) == Some(&b'#') {
                    at += 1;
                }
                while at < bytes.len() && (bytes[at] == b'_' || bytes[at].is_ascii_alphanumeric() || bytes[at] >= 0x80) {
                    at += 1;
                }
                out.push(PTok::Ident(unraw(&text[start..at])));
            }
            _ => return None,
        }
    }
    Some(out)
}

fn to_path(abs: bool, segments: &[N], krate: CrateItemKey) -> Path<'static, N> {
    let own = |it: &[N]| AVec::from_slice_in(it, g());
    if abs {
        return Path { kind: PathKind::Abs, segments: own(segments) };
    }
    match segments.first().copied() {
        Some("crate") => Path { kind: PathKind::Crate, segments: own(&segments[1..]) },
        Some("$crate") => Path { kind: PathKind::DollarCrate(krate), segments: own(&segments[1..]) },
        Some("self") => Path { kind: PathKind::SelfMod, segments: own(&segments[1..]) },
        Some("super") => {
            let n = segments.iter().take_while(|it| **it == "super").count();
            Path { kind: PathKind::Super(u8::try_from(n).unwrap_or(u8::MAX)), segments: own(&segments[n..]) }
        }
        _ => Path { kind: PathKind::Plain, segments: own(segments) },
    }
}

/// A simple path (`a::b`, `crate::a`, `$crate::a`, `::a`); `None` for anything else.
fn parse_path(text: &'static str, krate: CrateItemKey) -> Option<Path<'static, N>> {
    let tokens = path_tokens(text)?;
    let mut abs = false;
    let mut segments = Vec::new();
    let mut at = 0;
    if tokens.first() == Some(&PTok::Colons) {
        abs = true;
        at = 1;
    }
    loop {
        let PTok::Ident(name) = *tokens.get(at)? else { return None };
        segments.push(name);
        at += 1;
        match tokens.get(at) {
            None => break,
            Some(PTok::Colons) => at += 1,
            Some(_) => return None,
        }
    }
    Some(to_path(abs, &segments, krate))
}

/// One leaf of a `use` tree: absolute, its segments, what it imports.
type UseLeaf = (bool, Vec<N>, UseForm<N>);

fn use_leaves(text: &'static str) -> Option<Vec<UseLeaf>> {
    let tokens = path_tokens(text)?;
    let mut out = Vec::new();
    let mut at = 0;
    use_tree(&tokens, &mut at, &[], false, &mut out)?;
    (at == tokens.len()).then_some(out)
}

fn use_tree(t: &[PTok], at: &mut usize, prefix: &[N], abs: bool, out: &mut Vec<UseLeaf>) -> Option<()> {
    let mut segments = prefix.to_vec();
    let mut cur_abs = abs;
    let mut stack: blaze_own::FixedVec<(usize, bool), 64> = blaze_own::FixedVec::new();
    'child: loop {
        if segments.is_empty() && t.get(*at) == Some(&PTok::Colons) {
            cur_abs = true;
            *at += 1;
        }
        let mut emitted = false;
        loop {
            match t.get(*at).copied() {
                Some(PTok::Ident(name)) => {
                    segments.push(name);
                    *at += 1;
                    if t.get(*at) == Some(&PTok::Colons) {
                        *at += 1;
                        continue;
                    }
                    break;
                }
                Some(PTok::Open) => {
                    *at += 1;
                    if t.get(*at) == Some(&PTok::Close) {
                        *at += 1;
                        emitted = true;
                        break;
                    }
                    stack.push((segments.len(), cur_abs)).ok()?;
                    continue 'child;
                }
                Some(PTok::Star) => {
                    *at += 1;
                    out.push((cur_abs, segments.to_vec(), UseForm::Glob));
                    emitted = true;
                    break;
                }
                _ => return None,
            }
        }
        if !emitted {
            let mut rename = Rename::Same;
            if t.get(*at) == Some(&PTok::Ident("as")) {
                *at += 1;
                rename = match t.get(*at).copied() {
                    Some(PTok::Ident("_")) => Rename::Underscore,
                    Some(PTok::Ident(name)) => Rename::To(name),
                    _ => return None,
                };
                *at += 1;
            }
            let mut types_only = false;
            if segments.len() > 1 && segments.last() == Some(&"self") {
                segments.pop();
                types_only = true;
            }
            out.push((cur_abs, segments.to_vec(), UseForm::Named { rename, types_only }));
        }
        loop {
            let Some(&(saved_len, saved_abs)) = stack.last() else {
                return Some(());
            };
            match t.get(*at) {
                Some(PTok::Comma) => {
                    *at += 1;
                    if t.get(*at) == Some(&PTok::Close) {
                        *at += 1;
                        stack.pop();
                    } else {
                        segments.truncate(saved_len);
                        cur_abs = saved_abs;
                        continue 'child;
                    }
                }
                Some(PTok::Close) => {
                    *at += 1;
                    stack.pop();
                }
                _ => return None,
            }
        }
    }
}

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

struct File {
    path: PathBuf,
    rel: String,
    text: &'static str,
    id: ContentId,
    in_dir: bool,
}

struct Krate {
    name: String,
    key: CrateItemKey,
    edition: Edition,
    cfg: Cfg,
    root: usize,
    files: Vec<File>,
    outlines: OnGlobal<Vec<Outline<'static>>>,
    index: HashMap<PathBuf, usize>,
}

impl Krate {
    fn at(&self, loc: Loc) -> String {
        let file = &self.files[loc.file as usize];
        let offset = (loc.offset as usize).min(file.text.len());
        let line = file.text.as_bytes()[..offset].iter().filter(|it| **it == b'\n').count() + 1;
        format!("{}:{line}", file.rel)
    }

    fn item_attrs(&self, o: &Outline<'_>, item: &Item, text: &'static str) -> Attrs {
        let mut attrs = Attrs::new();
        for attr in o.attrs(item) {
            if attr.kind == AttrKind::Doc {
                continue;
            }
            self.cfg.attr(&mut attrs, strip_attr(item.text(text, attr.range)));
        }
        attrs
    }

    fn file_attrs(&self, file: usize) -> Attrs {
        let mut attrs = Attrs::new();
        let text = self.files[file].text;
        for attr in self.outlines[file].file_attrs() {
            if attr.kind == AttrKind::Doc {
                continue;
            }
            self.cfg.attr(&mut attrs, strip_attr(attr.range.of(text)));
        }
        attrs
    }

    fn load(&mut self, path: PathBuf, rel: String, in_dir: bool) -> Result<usize, String> {
        let text = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let text = leak(text);
        let id = ContentId::of("blaze pipeline example file 1", text.as_bytes());
        let at = self.files.len();
        self.outlines.push(outline(text, self.edition, g()));
        self.files.push(File { path: path.clone(), rel, text, id, in_dir });
        self.index.insert(path, at);
        Ok(at)
    }
}

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

/// `name`, edition and default features from the nearest `Cargo.toml`s.
fn manifest(dir: &std::path::Path) -> (String, Option<Edition>, Vec<String>) {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
    let mut name = None;
    let mut edition = None;
    let mut features = Vec::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line.to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        match (section.as_str(), key) {
            ("[package]", "name") => name = Some(value.trim_matches('"').to_string()),
            ("[package]", "edition") => edition = edition_of(value.trim_matches('"')),
            ("[features]", "default") => {
                features = value
                    .trim_matches(|c| c == '[' || c == ']')
                    .split(',')
                    .map(|it| it.trim().trim_matches('"').to_string())
                    .filter(|it| !it.is_empty())
                    .collect();
            }
            _ => {}
        }
    }
    if edition.is_none() {
        // `edition.workspace = true`: the nearest `[workspace.package]` above says.
        let mut up = dir.parent();
        while let Some(at) = up {
            if let Ok(text) = std::fs::read_to_string(at.join("Cargo.toml")) {
                let mut in_package = false;
                for line in text.lines() {
                    let line = line.trim();
                    if line.starts_with('[') {
                        in_package = line == "[workspace.package]";
                    } else if in_package && line.starts_with("edition") {
                        if let Some((_, value)) = line.split_once('=') {
                            edition = edition_of(value.trim().trim_matches('"'));
                        }
                    }
                }
                if edition.is_some() {
                    break;
                }
            }
            up = at.parent();
        }
    }
    let name = name.unwrap_or_else(|| {
        dir.file_name().map_or_else(|| "crate".to_string(), |it| it.to_string_lossy().replace('-', "_"))
    });
    (name.replace('-', "_"), edition, features)
}

fn edition_of(text: &str) -> Option<Edition> {
    match text {
        "2015" => Some(Edition::E2015),
        "2018" => Some(Edition::E2018),
        "2021" => Some(Edition::E2021),
        "2024" => Some(Edition::E2024),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Stage 2: the module tree, and lowering a fragment's items to its declarations.

struct Module {
    key: ItemKey,
    parent: Option<usize>,
    name: Option<N>,
    file: usize,
    /// The inline `mod` item it is, in `file`; `None` for a file's own items.
    item: Option<ItemIndex>,
    /// Where its `mod x;` children's files are.
    dir: PathBuf,
    /// What its children's `#[path]` is relative to.
    path_dir: PathBuf,
    no_implicit_prelude: bool,
    path: String,
}

struct Tree {
    modules: Vec<Module>,
    by_key: HashMap<ItemKey, usize>,
    forms: BTreeMap<&'static str, usize>,
    fails: Failures,
    missing: Vec<PathBuf>,
}

fn walk(k: &Krate) -> Tree {
    let mut tree = Tree {
        modules: Vec::new(),
        by_key: HashMap::new(),
        forms: BTreeMap::new(),
        fails: Failures::default(),
        missing: Vec::new(),
    };
    let root_dir = k.files[k.root].path.parent().map(|it| it.to_path_buf()).unwrap_or_default();
    let root_attrs = k.file_attrs(k.root);
    tree.modules.push(Module {
        key: root_key(k.key),
        parent: None,
        name: None,
        file: k.root,
        item: None,
        dir: root_dir.clone(),
        path_dir: root_dir,
        no_implicit_prelude: root_attrs.no_implicit_prelude,
        path: k.name.clone(),
    });
    let mut at = 0;
    while at < tree.modules.len() && tree.modules.len() < 1_000_000 {
        let m = &tree.modules[at];
        let (file, item, key) = (m.file, m.item, m.key);
        let (dir, path_dir, mpath) = (m.dir.clone(), m.path_dir.clone(), m.path.clone());
        let o = &k.outlines[file];
        let text = k.files[file].text;
        for (idx, child) in o.children(item) {
            let ItemDetail::Mod { inline, .. } = child.detail else { continue };
            let attrs = k.item_attrs(o, child, text);
            if !attrs.cfg_ok {
                *tree.forms.entry("not walked: cfg'd out").or_default() += 1;
                continue;
            }
            let Some(name) = child.name.map(|r| unraw(child.text(text, r))) else { continue };
            let loc = Loc { file: file as u32, offset: child.range.start };
            let ckey = key_of(k.key, key, "mod", name, 0);
            let cpath = format!("{mpath}::{name}");
            if inline {
                let sub = attrs.path.unwrap_or(name);
                let d = normalize(&dir.join(sub));
                let form = if attrs.path.is_some() { "inline, #[path]" } else { "inline" };
                *tree.forms.entry(form).or_default() += 1;
                tree.modules.push(Module {
                    key: ckey,
                    parent: Some(at),
                    name: Some(name),
                    file,
                    item: Some(idx),
                    dir: d.clone(),
                    path_dir: d,
                    no_implicit_prelude: attrs.no_implicit_prelude,
                    path: cpath,
                });
                continue;
            }
            let (target, cdir, cpdir, form) = if let Some(p) = attrs.path {
                let target = normalize(&path_dir.join(p));
                let parent = target.parent().map(|it| it.to_path_buf()).unwrap_or_default();
                (target, parent.clone(), parent, "file, #[path]")
            } else {
                let a = normalize(&dir.join(format!("{name}.rs")));
                let b = normalize(&dir.join(name).join("mod.rs"));
                let sub = normalize(&dir.join(name));
                match (k.index.contains_key(&a), k.index.contains_key(&b)) {
                    (true, true) => {
                        tree.fails.add("module: both x.rs and x/mod.rs exist (x.rs taken)", || k.at(loc));
                        (a, sub, dir.clone(), "file x.rs")
                    }
                    (true, false) => {
                        tree.missing.push(b);
                        (a, sub, dir.clone(), "file x.rs")
                    }
                    (false, true) => {
                        tree.missing.push(a);
                        (b, sub.clone(), sub, "file x/mod.rs")
                    }
                    (false, false) => {
                        tree.missing.push(a);
                        tree.missing.push(b);
                        tree.fails.add("module: neither x.rs nor x/mod.rs exists", || format!("{}: mod {name}", k.at(loc)));
                        continue;
                    }
                }
            };
            let Some(&cfile) = k.index.get(&target) else {
                tree.fails.add("module: #[path] file not found", || format!("{}: {}", k.at(loc), target.display()));
                tree.missing.push(target);
                continue;
            };
            let fattrs = k.file_attrs(cfile);
            if !fattrs.cfg_ok {
                *tree.forms.entry("not walked: cfg'd out by its file's #![cfg]").or_default() += 1;
                continue;
            }
            *tree.forms.entry(form).or_default() += 1;
            tree.modules.push(Module {
                key: ckey,
                parent: Some(at),
                name: Some(name),
                file: cfile,
                item: None,
                dir: cdir,
                path_dir: cpdir,
                no_implicit_prelude: attrs.no_implicit_prelude || fattrs.no_implicit_prelude,
                path: cpath,
            });
        }
        at += 1;
    }
    for (i, m) in tree.modules.iter().enumerate() {
        tree.by_key.insert(m.key, i);
    }
    tree
}

#[derive(Clone, Copy)]
enum MacroBody {
    Rules(&'static str),
    Macro2 { params: Option<&'static str>, body: &'static str },
    BuiltinFn(&'static str),
    /// A builtin derive or attribute (`#[rustc_builtin_macro] macro Clone`): not run here.
    BuiltinOther(CallShape),
}

#[derive(Clone, Copy)]
struct MacroSrc {
    name: N,
    body: MacroBody,
    module: ItemKey,
    export: bool,
    local_inner: bool,
}

#[derive(Clone, Copy)]
struct CallSrc {
    module: ItemKey,
    name: &'static str,
    input: &'static str,
    kind: CallShape,
    loc: Loc,
    depth: u16,
}

#[derive(Clone, Copy)]
struct Variant {
    name: N,
    key: ItemKey,
    record: bool,
}

#[derive(Clone, Copy)]
struct ImplSrc {
    module: ItemKey,
    key: ItemKey,
    header: &'static str,
    loc: Loc,
}

#[derive(Clone, Copy)]
struct LangSrc {
    name: &'static str,
    key: ItemKey,
    loc: Loc,
}

/// One fragment lowered: a module's own text, or one call's expansion.
struct Lowered {
    decls: DeclsBuilder<'static, N>,
    macros: Vec<(ItemKey, MacroSrc)>,
    calls: Vec<(ItemKey, CallSrc)>,
    enums: Vec<(ItemKey, Vec<Variant>)>,
    impls: Vec<ImplSrc>,
    langs: Vec<LangSrc>,
    /// Where each of `decls.uses` is written, by index.
    use_locs: Vec<Loc>,
    prelude: Option<(bool, Vec<N>, Loc)>,
    fails: Failures,
}

impl Lowered {
    fn entries(&self) -> usize {
        let d = &self.decls;
        d.items.len() + d.mods.len() + d.uses.len() + d.extern_crates.len() + d.macro_rules.len() + d.calls.len()
    }
}

/// Where a fragment's text is: a source file, or an expansion (whose places are its call's).
#[derive(Clone, Copy)]
enum Where {
    File(u32),
    Expansion(Loc),
}

struct Lower<'a> {
    k: &'a Krate,
    modules: &'a HashMap<ItemKey, usize>,
    module: ItemKey,
    /// The fragment's key: the module's, or the call's.
    owner: ItemKey,
    text: &'static str,
    outline: &'a Outline<'static>,
    at: Where,
    depth: u16,
    pos: u32,
    out: Lowered,
}

impl<'a> Lower<'a> {
    fn new(
        k: &'a Krate,
        modules: &'a HashMap<ItemKey, usize>,
        module: ItemKey,
        owner: ItemKey,
        text: &'static str,
        outline: &'a Outline<'static>,
        at: Where,
        depth: u16,
    ) -> Lower<'a> {
        Lower {
            k,
            modules,
            module,
            owner,
            text,
            outline,
            at,
            depth,
            pos: 0,
            out: Lowered {
                decls: DeclsBuilder::new_in(g()),
                macros: Vec::new(),
                calls: Vec::new(),
                enums: Vec::new(),
                impls: Vec::new(),
                langs: Vec::new(),
                use_locs: Vec::new(),
                prelude: None,
                fails: Failures::default(),
            },
        }
    }

    fn loc(&self, offset: u32) -> Loc {
        match self.at {
            Where::File(file) => Loc { file, offset },
            Where::Expansion(loc) => loc,
        }
    }

    fn fail(&mut self, kind: impl Into<String>, offset: u32, what: impl FnOnce() -> String) {
        let at = self.k.at(self.loc(offset));
        let expansion = matches!(self.at, Where::Expansion(_));
        self.out.fails.add(kind, || {
            let what = what();
            if expansion { format!("{at} (in an expansion): {what}") } else { format!("{at}: {what}") }
        });
    }

    fn next_pos(&mut self) -> u32 {
        self.pos += 1;
        self.pos
    }

    fn vis(&self, item: &Item) -> WrittenVis<Path<'static, N>> {
        let path = |r| {
            parse_path(item.text(self.text, r), self.k.key)
                .unwrap_or_else(|| to_path(false, &[item.text(self.text, r)], self.k.key))
        };
        match item.visibility {
            WrittenVis::Private => WrittenVis::Private,
            WrittenVis::Pub => WrittenVis::Pub,
            WrittenVis::Crate => WrittenVis::Crate,
            WrittenVis::Super => WrittenVis::Super,
            WrittenVis::SelfOnly => WrittenVis::SelfOnly,
            WrittenVis::In(r) => WrittenVis::In(path(r)),
            WrittenVis::WithoutIn(r) => WrittenVis::WithoutIn(path(r)),
        }
    }

    fn items(&mut self, parent: Option<ItemIndex>) {
        let outline = self.outline;
        for (idx, item) in outline.children(parent) {
            self.item(idx, item);
        }
    }

    fn declare(&mut self, name: N, ns: Namespace, key: ItemKey, kind: DefKind, vis: WrittenVis<Path<'static, N>>) {
        self.out.decls.items.push(DeclaredItem { name, ns, def: Def { key, kind }, vis });
    }

    fn item(&mut self, idx: ItemIndex, item: &Item) {
        let text = self.text;
        let attrs = self.k.item_attrs(self.outline, item, text);
        if !attrs.cfg_ok {
            return;
        }
        let at = item.range.start;
        let name = item.name.map(|r| unraw(item.text(text, r)));
        let vis = self.vis(item);
        let tag = format!("{:?}", item.kind());
        let key = key_of(self.k.key, self.owner, &tag, name.unwrap_or(""), idx.0);
        for other in &attrs.others {
            self.fail(format!("attribute macro not lowered: #[{other}]"), at, || name.unwrap_or("_").to_string());
        }
        for path in &attrs.derives {
            let call = key_of(self.k.key, self.owner, "derive", path, idx.0);
            let Some(parsed) = parse_path(path, self.k.key) else {
                self.fail("derive path not read", at, || path.to_string());
                continue;
            };
            let pos = self.next_pos();
            self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::Derive, pos, item: Some(idx.0), role: blaze_surface::CallRole::Call });
            let src = CallSrc { module: self.module, name: path, input: "", kind: CallShape::Derive, loc: self.loc(at), depth: self.depth };
            self.out.calls.push((call, src));
        }
        if let Some(lang) = attrs.lang {
            self.out.langs.push(LangSrc { name: lang, key, loc: self.loc(at) });
        }
        match item.detail {
            ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. } => {
                if let Some(name) = name.filter(|it| *it != "_") {
                    self.declare(name, Namespace::Values, key, DefKind::Other, vis);
                }
            }
            ItemDetail::Struct => {
                let Some(name) = name else { return };
                let shape = item.body.map(|r| item.text(text, r).trim_start().as_bytes().first().copied());
                let value = matches!(shape, None | Some(Some(b'(')));
                if value {
                    self.declare(name, Namespace::Values, key, DefKind::Other, vis.clone());
                }
                self.declare(name, Namespace::Types, key, DefKind::Other, vis);
            }
            ItemDetail::Union | ItemDetail::TypeAlias | ItemDetail::TraitAlias => {
                if let Some(name) = name {
                    self.declare(name, Namespace::Types, key, DefKind::Other, vis);
                }
            }
            ItemDetail::Enum => {
                let Some(name) = name else { return };
                self.declare(name, Namespace::Types, key, DefKind::Enum, vis);
                let mut variants = Vec::new();
                let outline = self.outline;
                for (vidx, v) in outline.children(Some(idx)) {
                    if !matches!(v.detail, ItemDetail::Variant { .. }) {
                        continue;
                    }
                    if !self.k.item_attrs(outline, v, text).cfg_ok {
                        continue;
                    }
                    let Some(vname) = v.name.map(|r| unraw(v.text(text, r))) else { continue };
                    let record = v.body.is_some_and(|r| v.text(text, r).trim_start().starts_with('{'));
                    variants.push(Variant { name: vname, key: key_of(self.k.key, key, "variant", vname, vidx.0), record });
                }
                self.out.enums.push((key, variants));
            }
            ItemDetail::Trait => {
                if let Some(name) = name {
                    self.declare(name, Namespace::Types, key, DefKind::Trait, vis);
                }
                self.assoc_langs(idx, key);
            }
            ItemDetail::Impl => {
                let header = self.impl_header(item);
                self.out.impls.push(ImplSrc { module: self.module, key, header, loc: self.loc(at) });
                self.assoc_langs(idx, key);
            }
            ItemDetail::Mod { .. } => {
                let Some(name) = name else { return };
                if matches!(self.at, Where::Expansion(_)) {
                    self.fail("module written by a macro: not walked", at, || name.to_string());
                    return;
                }
                let module = key_of(self.k.key, self.module, "mod", name, 0);
                if !self.modules.contains_key(&module) {
                    // Not walked: its file is missing or cfg'd out; counted in the walk.
                    return;
                }
                let pos = self.next_pos();
                self.out.decls.mods.push(ChildModDecl { name, module, vis, macro_use: attrs.macro_use, pos });
            }
            ItemDetail::Use { tree } => {
                let Some(tree) = tree else {
                    self.fail("use without a tree", at, String::new);
                    return;
                };
                let tree_text = item.text(text, tree);
                let Some(leaves) = use_leaves(tree_text) else {
                    self.fail("use tree not read", at, || tree_text.to_string());
                    return;
                };
                if attrs.prelude_import {
                    match leaves.as_slice() {
                        [(abs, segments, UseForm::Glob)] => self.out.prelude = Some((*abs, segments.clone(), self.loc(at))),
                        _ => self.fail("#[prelude_import] that is not one glob", at, || tree_text.to_string()),
                    }
                    return;
                }
                for (abs, segments, leaf) in leaves {
                    let pos = self.next_pos();
                    let path = to_path(abs, &segments, self.k.key);
                    self.out.decls.uses.push(UseDecl { path, leaf, vis: vis.clone(), pos });
                    self.out.use_locs.push(self.loc(at));
                }
            }
            ItemDetail::ExternCrate { rename } => {
                let Some(name) = name else { return };
                let target = if name == "self" { ExternTarget::SelfCrate } else { ExternTarget::Dep(name) };
                let rename = rename.map(|r| unraw(item.text(text, r)));
                let rename = match rename {
                    Rename::To("_") => Rename::Underscore,
                    other => other,
                };
                self.out.decls.extern_crates.push(ExternCrateDecl { target, rename, vis });
            }
            ItemDetail::ExternBlock { .. } => self.items(Some(idx)),
            ItemDetail::MacroRules { exported } => {
                let Some(name) = name else { return };
                let Some(body) = item.body.map(|r| inside(item.text(text, r))) else {
                    self.fail("macro_rules! without a body", at, || name.to_string());
                    return;
                };
                let pos = self.next_pos();
                let export = exported || attrs.macro_export;
                let shapes = if attrs.builtin { MacroShapes::FN_LIKE } else { blaze_scope::task::rules_shapes(body, self.k.edition, g()) };
                self.out.decls.macro_rules.push(MacroRulesDecl { name, def: key, pos, export, shapes });
                let body = if attrs.builtin { MacroBody::BuiltinFn(attrs.builtin_name.unwrap_or(name)) } else { MacroBody::Rules(body) };
                let src = MacroSrc { name, body, module: self.module, export, local_inner: attrs.local_inner };
                self.out.macros.push((key, src));
            }
            ItemDetail::MacroDef => {
                let Some(name) = name else { return };
                let Some(body_range) = item.body else {
                    self.fail("macro without a body", at, || name.to_string());
                    return;
                };
                let (shape, body) = if attrs.builtin {
                    let which = attrs.builtin_name.unwrap_or(name);
                    if BuiltinDerive::ALL.into_iter().any(|it| it.name() == which) {
                        (MacroShapes::DERIVE, MacroBody::BuiltinOther(CallShape::Derive))
                    } else if BuiltinAttr::ALL.into_iter().any(|it| it.name() == which) {
                        (MacroShapes::ATTR, MacroBody::BuiltinOther(CallShape::Attr))
                    } else {
                        (MacroShapes::FN_LIKE, MacroBody::BuiltinFn(which))
                    }
                } else {
                    let params = item.name.map(|r| {
                        let between = blaze_surface::Range::new(r.end, body_range.start);
                        item.text(text, between).trim()
                    });
                    let params = params.filter(|it| it.starts_with('(')).map(inside);
                    let body = inside(item.text(text, body_range));
                    let shapes = match params {
                        Some(_) => MacroShapes::FN_LIKE,
                        None => blaze_scope::task::rules_shapes(body, self.k.edition, g()),
                    };
                    (shapes, MacroBody::Macro2 { params, body })
                };
                self.declare(name, Namespace::Macros, key, DefKind::Macro(shape), vis);
                let src = MacroSrc { name, body, module: self.module, export: false, local_inner: false };
                self.out.macros.push((key, src));
            }
            ItemDetail::MacroCall { path, input } => {
                let (Some(path), Some(input)) = (path, input) else {
                    self.fail("macro call without a path or an input", at, String::new);
                    return;
                };
                let path_text = item.text(text, path);
                let Some(parsed) = parse_path(path_text, self.k.key) else {
                    self.fail("macro call path not read", at, || path_text.to_string());
                    return;
                };
                let call = key;
                let pos = self.next_pos();
                self.out.decls.calls.push(MacroCallDecl { call, path: parsed, kind: CallShape::FnLike, pos, item: None, role: blaze_surface::CallRole::Call });
                let src = CallSrc {
                    module: self.module,
                    name: path_text,
                    input: inside(item.text(text, input)),
                    kind: CallShape::FnLike,
                    loc: self.loc(at),
                    depth: self.depth,
                };
                self.out.calls.push((call, src));
            }
            ItemDetail::Variant { .. } | ItemDetail::Field { .. } => {}
            ItemDetail::Error => self.fail("an item the outline could not read", at, || first_line(item.text(text, blaze_surface::Range::new(0, item.range.end - item.range.start)))),
            ItemDetail::Unhandled { syntax } => {
                self.fail(format!("unhandled item form: {:?}", syntax_kind(syntax)), at, || name.unwrap_or("_").to_string());
            }
        }
    }

    /// The lang items among a trait's or an impl's associated items.
    fn assoc_langs(&mut self, parent: ItemIndex, parent_key: ItemKey) {
        let outline = self.outline;
        let text = self.text;
        for (idx, item) in outline.children(Some(parent)) {
            let attrs = self.k.item_attrs(outline, item, text);
            if !attrs.cfg_ok {
                continue;
            }
            if let Some(lang) = attrs.lang {
                let name = item.name.map_or("", |r| item.text(text, r));
                let key = key_of(self.k.key, parent_key, "assoc", name, idx.0);
                self.out.langs.push(LangSrc { name: lang, key, loc: self.loc(item.range.start) });
            }
        }
    }

    /// An impl's header without its outer attributes: `unsafe impl<T> Tr for S<T> where ..`.
    fn impl_header(&self, item: &Item) -> &'static str {
        let outer_end = self
            .outline
            .attrs(item)
            .iter()
            .filter(|it| !it.inner)
            .map(|it| it.range.end)
            .max()
            .unwrap_or(0);
        let start = outer_end.min(item.signature.end);
        item.text(self.text, blaze_surface::Range::new(start, item.signature.end)).trim()
    }
}

/// The inside of a delimited token tree's text: `{ a }` gives ` a `.
fn inside(text: &'static str) -> &'static str {
    let t = text.trim();
    if t.len() >= 2 && matches!(t.as_bytes()[0], b'(' | b'[' | b'{') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().find(|it| !it.trim().is_empty()).unwrap_or("").trim();
    line.chars().take(80).collect()
}

/// Every module's lowered text, merged.
struct Lowering {
    /// By module index.
    decls: OnGlobal<Vec<DeclsBuilder<'static, N>>>,
    use_locs: HashMap<ItemKey, Vec<Loc>>,
    macros: HashMap<ItemKey, MacroSrc>,
    calls: HashMap<ItemKey, CallSrc>,
    enums: HashMap<ItemKey, Vec<Variant>>,
    impls: Vec<ImplSrc>,
    langs: Vec<LangSrc>,
    prelude: Option<(bool, Vec<N>, Loc)>,
    macro_exports: Vec<(N, ItemKey, MacroShapes, Loc)>,
    entries: usize,
    items_in: usize,
    fails: Failures,
}

fn lower_modules(k: &Krate, tree: &Tree, threads: usize) -> Lowering {
    let parts = par_map(threads, tree.modules.len(), |i| {
        let m = &tree.modules[i];
        let file = &k.files[m.file];
        let mut low = Lower::new(k, &tree.by_key, m.key, m.key, file.text, &k.outlines[m.file], Where::File(m.file as u32), 0);
        low.items(m.item);
        let seen = k.outlines[m.file].children(m.item).count();
        OnGlobal((low.out, seen))
    });
    let mut out = Lowering {
        decls: OnGlobal(Vec::with_capacity(parts.len())),
        use_locs: HashMap::new(),
        macros: HashMap::new(),
        calls: HashMap::new(),
        enums: HashMap::new(),
        impls: Vec::new(),
        langs: Vec::new(),
        prelude: None,
        macro_exports: Vec::new(),
        entries: 0,
        items_in: 0,
        fails: Failures::default(),
    };
    for (i, OnGlobal((part, seen))) in parts.into_iter().enumerate() {
        let key = tree.modules[i].key;
        out.entries += part.entries();
        out.items_in += seen;
        for rules in part.decls.macro_rules.iter() {
            if rules.export {
                let loc = part.macros.iter().find(|(def, _)| *def == rules.def).map_or(Loc { file: 0, offset: 0 }, |_| Loc { file: tree.modules[i].file as u32, offset: 0 });
                out.macro_exports.push((rules.name, rules.def, rules.shapes, loc));
            }
        }
        out.use_locs.insert(key, part.use_locs);
        out.macros.extend(part.macros);
        out.calls.extend(part.calls);
        out.enums.extend(part.enums);
        out.impls.extend(part.impls);
        out.langs.extend(part.langs);
        if i == 0 {
            out.prelude = part.prelude;
        } else if part.prelude.is_some() {
            out.fails.add("#[prelude_import] outside the crate root", || tree.modules[i].path.clone());
        }
        out.fails.merge(part.fails);
        out.decls.push(part.decls);
    }
    out
}

fn module_info(k: &Krate, tree: &Tree, m: &Module) -> ModuleInfo<N> {
    ModuleInfo {
        krate: k.key,
        parent: m.parent.map(|p| tree.modules[p].key),
        name: m.name,
        no_implicit_prelude: m.no_implicit_prelude,
    }
}

// ---------------------------------------------------------------------------------------------
// Stage 3: the crate root.

/// The tool namespaces, `blaze` among them: one table, blaze-surface's.
const TOOLS: &[&str] = &blaze_surface::tool_attrs::TOOLS;

fn crate_root(k: &Krate, tree: &Tree, low: &Lowering, fails: &mut Failures) -> CrateRootBuilder<'static, N> {
    let attrs = k.file_attrs(k.root);
    let root = tree.modules[0].key;
    let prelude = match &low.prelude {
        Some((false, segments, loc)) => {
            // `use prelude::rust_2024::*` at the root: a path of modules from the root.
            let mut at = 0usize;
            let mut found = true;
            for segment in segments {
                let start = if *segment == "self" || *segment == "crate" { continue } else { at };
                match tree.modules.iter().position(|m| m.parent == Some(start) && m.name == Some(*segment)) {
                    Some(next) => at = next,
                    None => {
                        found = false;
                        break;
                    }
                }
            }
            if found {
                Some(tree.modules[at].key)
            } else {
                fails.add("#[prelude_import] path is not a path of modules", || format!("{}: {}", k.at(*loc), segments.join("::")));
                None
            }
        }
        Some((true, segments, loc)) => {
            fails.add("#[prelude_import] of another crate (no dependencies in this run)", || format!("{}: ::{}", k.at(*loc), segments.join("::")));
            None
        }
        None => {
            if !attrs.no_core {
                fails.add("no prelude: the injected std/core prelude needs a dependency this run does not have", || k.name.clone());
            }
            None
        }
    };
    let head = CrateRootHead {
        krate: k.key,
        root_module: root,
        root_file: k.files[k.root].id,
        edition: k.edition,
        no_std: attrs.no_std,
        no_core: attrs.no_core,
        // The example's attribute reader does not read `#![blaze::arbitrary_ints]`; no crate
        // it is run on (tempo's graph) writes it.
        arbitrary_ints: false,
        prelude,
    };
    let mut cr = CrateRootBuilder::new_in(head, g());
    // `extern crate self as name;` at the root is the one extern-prelude entry this run has.
    let mut externs: Vec<(N, ItemKey)> = Vec::new();
    for e in low.decls[0].extern_crates.iter() {
        match (e.target, e.rename) {
            (ExternTarget::SelfCrate, Rename::To(name)) => externs.push((name, root)),
            (ExternTarget::Dep(name), _) => fails.add("extern crate: no dependencies in this run", || name.to_string()),
            _ => {}
        }
    }
    externs.sort();
    externs.dedup_by_key(|it| it.0);
    cr.extern_prelude.extend_from_slice(&externs);
    let mut exports: Vec<(N, ItemKey, MacroShapes)> = Vec::new();
    let mut seen: HashSet<N> = HashSet::new();
    for (name, def, shapes, _) in &low.macro_exports {
        if seen.insert(name) {
            exports.push((name, *def, *shapes));
        } else {
            fails.add("#[macro_export] name exported twice (first kept)", || name.to_string());
        }
    }
    exports.sort();
    cr.macro_exports.extend_from_slice(&exports);
    let mut tools: Vec<N> = TOOLS.to_vec();
    tools.extend(attrs.tools.iter().copied());
    tools.sort();
    tools.dedup();
    cr.tools.extend_from_slice(&tools);
    let mut builtin: Vec<N> = BUILTIN_ATTRS.to_vec();
    builtin.sort();
    builtin.dedup();
    cr.builtin_attrs.extend_from_slice(&builtin);
    cr
}

// ---------------------------------------------------------------------------------------------
// Stage 4: names, in rounds.

/// What every run of stage 4 reads: the records read back from the `MemSurface` once.
struct Base<'k> {
    k: &'k Krate,
    tree: &'k Tree,
    low: &'k Lowering,
    decls: OnGlobal<HashMap<ItemKey, (ModuleInfo<N>, DeclsBuilder<'static, N>)>>,
    root: OnGlobal<CrateRootBuilder<'static, N>>,
    parent: HashMap<ItemKey, Option<ItemKey>>,
    /// Each module's chain of modules up to the crate's root, itself first.
    chains: HashMap<ItemKey, Vec<ItemKey>>,
}

/// What one run of stage 4 adds: the expansions and what they declare.
#[derive(Default)]
struct Run {
    expansions: Mutex<OnGlobal<HashMap<ItemKey, DeclsBuilder<'static, N>>>>,
    calls: RwLock<HashMap<ItemKey, CallSrc>>,
    macros: RwLock<HashMap<ItemKey, MacroSrc>>,
    enums: RwLock<HashMap<ItemKey, Vec<Variant>>>,
    use_locs: Mutex<HashMap<ItemKey, Vec<Loc>>>,
    impls: Mutex<Vec<ImplSrc>>,
    langs: Mutex<Vec<LangSrc>>,
    fails: Mutex<Failures>,
    expanded: Mutex<BTreeMap<String, usize>>,
    expansion_bytes: AtomicUsize,
    /// The rendered text of every expansion, kept once and freed with the run.
    texts: Mutex<Vec<Box<str>>>,
}

impl Run {
    /// Keeps `text` for the run and lends it as `&'static str`, the example's one name type.
    fn keep(&self, text: String) -> &'static str {
        let boxed = text.into_boxed_str();
        // SAFETY: the box's heap bytes do not move when the box is moved into `texts`, and
        // `texts` frees them only when the `Run` drops. Every name lent from them lives in
        // the run's own tables or in the `Resolution` that owns the run, whose `done` field
        // is declared (and dropped) before `run`; nothing reads a name after the
        // `Resolution` is dropped.
        let lent: &'static str = unsafe { &*std::ptr::from_ref::<str>(&boxed) };
        self.texts.lock().expect("the texts' lock").push(boxed);
        lent
    }
}

impl Run {
    fn fail(&self, kind: impl Into<String>, example: impl FnOnce() -> String) {
        self.fails.lock().expect("the run's failures' lock").add(kind, example);
    }
}

struct Facts<'r> {
    base: &'r Base<'r>,
    run: &'r Run,
}

impl ScopeFacts for Facts<'_> {
    type Name = N;

    fn module(&self, module: ItemKey) -> Result<ModuleInfo<N>, SurfaceError> {
        self.base.decls.get(&module).map(|it| it.0).ok_or(SurfaceError::NoRecord { fact: "ModuleDecls", item: module })
    }

    fn decls(&self, module: ItemKey) -> Result<DeclsBuilder<'_, N>, SurfaceError> {
        self.base.decls.get(&module).map(|it| it.1.clone()).ok_or(SurfaceError::NoRecord { fact: "ModuleDecls", item: module })
    }

    fn crate_root(&self, krate: CrateItemKey) -> Result<CrateRootBuilder<'_, N>, SurfaceError> {
        if krate == self.base.k.key {
            Ok((*self.base.root).clone())
        } else {
            Err(SurfaceError::NoCrateRecord { fact: "CrateRoot", krate })
        }
    }

    fn expansion(&self, call: ItemKey) -> Result<Option<DeclsBuilder<'_, N>>, SurfaceError> {
        Ok(self.run.expansions.lock().expect("the expansions' lock").get(&call).cloned())
    }
}

/// The outside of an expansion: the stated cfg, no environment, no files, no proc macros.
struct Host<'r> {
    cfg: &'r Cfg,
    file: &'r str,
}

const REFUSED: HostError = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };

impl ExpandCx for Host<'_> {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Ok(None)
    }

    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        Ok(self.cfg.eval(&render(predicate)))
    }

    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }

    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(match what {
            PlaceOf::Line | PlaceOf::Column => Place::Number(1),
            PlaceOf::File => Place::Text(self.file),
            PlaceOf::ModulePath => Place::Text(""),
        })
    }

    fn run_proc_macro<'a>(
        &self,
        _: &MacroDefinition<'_>,
        _: ProcCall<'_>,
        _: Alloc<'a>,
    ) -> Result<Produced<'a>, HostError> {
        Err(REFUSED)
    }
}

/// A token stream as source text: a space between tokens unless the first is joint; `$crate`
/// written `crate` (one crate per run: they are the same); invisible groups dropped.
fn render(stream: &TokenStream<'_>) -> String {
    let tokens = stream.tokens();
    let mut out = String::new();
    let mut glued = true;
    let mut at = 0;
    while at < tokens.len() {
        let token = &tokens[at];
        let text = stream.text_of(token);
        let piece: String = match token.kind {
            TokenKind::Punct { char: '$', .. }
                if tokens.get(at + 1).is_some_and(|next| stream.text_of(next) == "crate") =>
            {
                at += 1;
                "crate".to_string()
            }
            TokenKind::Ident { .. } if text == "$crate" => "crate".to_string(),
            TokenKind::Ident { raw: Raw::Yes, .. } => format!("r#{text}"),
            TokenKind::Ident { raw: Raw::No, .. } => text.to_string(),
            TokenKind::Punct { char, .. } => char.to_string(),
            TokenKind::Literal { kind, suffix_len, .. } => {
                let split = text.len().saturating_sub(suffix_len as usize);
                let (body, suffix) = text.split_at(split);
                let hashes = |n: u8| "#".repeat(usize::from(n));
                match kind {
                    LitKind::Str => format!("\"{body}\"{suffix}"),
                    LitKind::StrRaw(n) => format!("r{0}\"{body}\"{0}{suffix}", hashes(n)),
                    LitKind::ByteStr => format!("b\"{body}\"{suffix}"),
                    LitKind::ByteStrRaw(n) => format!("br{0}\"{body}\"{0}{suffix}", hashes(n)),
                    LitKind::CStr => format!("c\"{body}\"{suffix}"),
                    LitKind::CStrRaw(n) => format!("cr{0}\"{body}\"{0}{suffix}", hashes(n)),
                    LitKind::Char => format!("'{body}'{suffix}"),
                    LitKind::Byte => format!("b'{body}'{suffix}"),
                    LitKind::Integer | LitKind::Float | LitKind::Err => text.to_string(),
                }
            }
            TokenKind::Open(Delim::Paren) => "(".to_string(),
            TokenKind::Open(Delim::Brace) => "{".to_string(),
            TokenKind::Open(Delim::Bracket) => "[".to_string(),
            TokenKind::Close(Delim::Paren) => ")".to_string(),
            TokenKind::Close(Delim::Brace) => "}".to_string(),
            TokenKind::Close(Delim::Bracket) => "]".to_string(),
            TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible) => {
                at += 1;
                continue;
            }
        };
        if !glued {
            out.push(' ');
        }
        out.push_str(&piece);
        glued = !matches!(token.kind.spacing(), Spacing::Alone);
        at += 1;
    }
    out
}

fn shape_name(shape: CallShape) -> &'static str {
    match shape {
        CallShape::FnLike => "function-like",
        CallShape::Attr => "attribute",
        CallShape::Derive => "derive",
    }
}

/// Expands one call the resolver asked for and files its declarations (empty when it cannot).
/// Time spent expanding calls and in the solver's steps, summed over every thread and run.
static EXPAND_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SOLVE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn expand_call(base: &Base<'_>, run: &Run, request: ExpandRequest) {
    let start = Instant::now();
    expand_call_untimed(base, run, request);
    EXPAND_NS.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

fn expand_call_untimed(base: &Base<'_>, run: &Run, request: ExpandRequest) {
    let decls = expand_decls(base, run, request).unwrap_or_else(|| DeclsBuilder::new_in(g()));
    run.expansions.lock().expect("the expansions' lock").insert(request.call, decls);
}

fn expand_decls(base: &Base<'_>, run: &Run, request: ExpandRequest) -> Option<DeclsBuilder<'static, N>> {
    let k = base.k;
    let src = base.low.calls.get(&request.call).copied().or_else(|| run.calls.read().expect("the calls' lock").get(&request.call).copied());
    let Some(src) = src else {
        run.fail("expansion asked for a call the driver does not know", || format!("{:x}", request.call.0));
        return None;
    };
    let at = k.at(src.loc);
    if src.kind == CallShape::Derive {
        run.fail(format!("derive not run (it writes impls, no names): {}", src.name), || at.clone());
        return Some(DeclsBuilder::new_in(g()));
    }
    let mac = base.low.macros.get(&request.def.key).copied().or_else(|| run.macros.read().expect("the macros' lock").get(&request.def.key).copied());
    let Some(mac) = mac else {
        run.fail(format!("{} call of a macro with no definition here", shape_name(src.kind)), || format!("{at}: {}!", src.name));
        return None;
    };
    let edition = k.edition;
    let module = matches!(mac.body, MacroBody::Macro2 { .. }).then_some(mac.module);
    let def_at = DefAt {
        site: DefSite { krate: k.key, module },
        edition,
        export: if mac.export { Export::Exported } else { Export::Local },
        local_inner: if mac.local_inner { LocalInner::Yes } else { LocalInner::No },
        transparency: None,
    };
    let lexed = |text: &str, what: &str| match lex(text, edition, g()) {
        Ok(stream) => Some(stream),
        Err(error) => {
            run.fail(format!("lex refused the {what}"), || format!("{at}: {}!: {error}", mac.name));
            None
        }
    };
    let def = match mac.body {
        MacroBody::Rules(body) => MacroDefinition::rules(lexed(body, "macro body")?, def_at),
        MacroBody::Macro2 { params, body } => {
            let params = match params {
                Some(p) => Some(lexed(p, "macro parameters")?),
                None => None,
            };
            MacroDefinition::macro2(params, lexed(body, "macro body")?, def_at)
        }
        MacroBody::BuiltinFn(name) => match BuiltinFn::of_library(name) {
            Some(which) => MacroDefinition::builtin_fn(which, def_at, g()),
            None => {
                run.fail(format!("builtin macro blaze-expand has no form of: {name}"), || at.clone());
                return None;
            }
        },
        MacroBody::BuiltinOther(shape) => {
            run.fail(format!("builtin {} macro called function-like", shape_name(shape)), || format!("{at}: {}", mac.name));
            return None;
        }
    };
    let input = lexed(src.input, "call's input")?;
    let site = CallSite { edition, panic_edition: edition, ctx: Ctx::root(edition) };
    let host = Host { cfg: &k.cfg, file: &k.files[src.loc.file as usize].rel };
    let expansion = match expand(&def, &Call::fn_like(&input, site), &host, g()) {
        Ok(Expanded::Expansion(expansion)) => expansion,
        Ok(Expanded::NeedsExpanded(nested)) => {
            run.fail(format!("eager macro: needs {} nested calls expanded first (not driven here)", nested.len().min(9)), || format!("{at}: {}!", mac.name));
            return None;
        }
        Err(error) => {
            let kind = format!("{error:?}");
            let kind = kind.split(|c: char| c == ' ' || c == '{' || c == '(').next().unwrap_or("").to_string();
            run.fail(format!("expand error: {kind}"), || format!("{at}: {}!: {error}", mac.name));
            return None;
        }
    };
    for diagnostic in expansion.diagnostics.iter() {
        let what = match diagnostic {
            Diagnostic::NoRuleMatched { .. } => "no rule matched",
            _ => "another diagnostic",
        };
        run.fail(format!("macro diagnostic: {what}"), || format!("{at}: {}!", mac.name));
    }
    *run.expanded.lock().expect("the expanded counts' lock").entry(mac.name.to_string()).or_default() += 1;
    let text = run.keep(render(&expansion.tokens));
    run.expansion_bytes.fetch_add(text.len(), Ordering::Relaxed);
    let made = outline(text, edition, g());
    if !made.errors().is_empty() {
        run.fail("an expansion that does not read as items (syntax errors)", || {
            format!("{at}: {}!: {}", mac.name, made.message(&made.errors()[0]))
        });
    }
    let mut low = Lower::new(k, &base.tree.by_key, src.module, request.call, text, &made, Where::Expansion(src.loc), src.depth + 1);
    low.items(None);
    let out = low.out;
    {
        let mut calls = run.calls.write().expect("the calls' lock");
        calls.extend(out.calls);
    }
    run.macros.write().expect("the macros' lock").extend(out.macros);
    run.enums.write().expect("the enums' lock").extend(out.enums);
    run.use_locs.lock().expect("the use places' lock").insert(request.call, out.use_locs);
    run.impls.lock().expect("the impls' lock").extend(out.impls);
    run.langs.lock().expect("the langs' lock").extend(out.langs);
    run.fails.lock().expect("the run's failures' lock").merge(out.fails);
    Some(out.decls)
}

/// An answer as the driver holds it.
enum Own {
    Name(NameAnswer),
    Globs { edges: Vec<GlobEdge>, krate: CrateItemKey, chain: Vec<ItemKey>, digest: ContentId },
    Textual(Vec<TextualDef<N>>, ContentId),
    Helpers(ContentId),
    InProgress(ItemKey),
}

impl Own {
    fn answer(&self) -> Answer<'_, N> {
        match self {
            Own::Name(a) => Answer::Name(*a),
            Own::Globs { edges, krate, chain, digest } => Answer::Globs { edges, krate: *krate, chain, digest: *digest },
            Own::Textual(defs, digest) => Answer::Textual { defs, digest: *digest },
            Own::Helpers(digest) => Answer::Helpers { names: &[], digest: *digest },
            Own::InProgress(module) => Answer::InProgress { module: *module },
        }
    }
}

fn digest_of<T: Hash>(tag: u8, items: &[T]) -> ContentId {
    let mut h = DefaultHasher::new();
    tag.hash(&mut h);
    items.hash(&mut h);
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&h.finish().to_le_bytes());
    ContentId::of("blaze pipeline example digest 1", &bytes)
}

type Done = OnGlobal<HashMap<ItemKey, Resolved<'static, N>>>;

struct DriveCx<'r> {
    base: &'r Base<'r>,
    run: &'r Run,
    facts: &'r Facts<'r>,
    done: &'r Done,
    failed: &'r HashSet<ItemKey>,
    module_slot: &'r HashMap<ItemKey, usize>,
}

impl DriveCx<'_> {
    fn variants(&self, owner: ItemKey) -> Option<Vec<Variant>> {
        if let Some(v) = self.base.low.enums.get(&owner) {
            return Some(v.clone());
        }
        self.run.enums.read().expect("the enums' lock").get(&owner).cloned()
    }

    /// The macros visible at `module`'s `mod` line; `Err` names the ancestor to wait on.
    fn entry(&self, module: ItemKey) -> Result<Vec<TextualDef<N>>, ItemKey> {
        let Some(&Some(parent)) = self.base.parent.get(&module) else { return Ok(Vec::new()) };
        if self.failed.contains(&parent) {
            return Ok(Vec::new());
        }
        let Some(p) = self.done.get(&parent) else { return Err(parent) };
        // One level: the parent's own definitions before the `mod` line; the resolver walks up.
        let mut out = Vec::new();
        let len = p.scope.mark(module).unwrap_or(0) as usize;
        out.extend_from_slice(&p.scope.textual[..len.min(p.scope.textual.len())]);
        Ok(out)
    }

    /// What the finished scopes answer `read`; `Err` names the module to wait on.
    fn answer(&self, read: Read<N>) -> Result<Own, ItemKey> {
        let owner = read.module();
        if let Read::DeriveHelpers { .. } = read {
            return Ok(Own::Helpers(digest_of::<u8>(4, &[])));
        }
        if let Read::TextualEntry { module } = read {
            let defs = self.entry(module)?;
            let digest = textual_digest(&defs, g());
            return Ok(Own::Textual(defs, digest));
        }
        if let Some(done) = self.done.get(&owner) {
            let scope = &done.scope;
            return Ok(match read {
                Read::Name { ns, name, .. } => Own::Name(NameAnswer::from(scope.get(ns, name).copied())),
                Read::Globs { module } => self.globs(module, scope.globs.to_vec()),
                Read::TextualOut { .. } => {
                    let defs = scope.textual.to_vec();
                    let digest = textual_digest(&defs, g());
                    Own::Textual(defs, digest)
                }
                Read::TextualEntry { .. } | Read::DeriveHelpers { .. } => unreachable!("answered above"),
            });
        }
        if let Some(variants) = self.variants(owner) {
            return Ok(match read {
                Read::Name { ns, name, .. } => {
                    let hit = variants.iter().find(|v| v.name == name && (ns == Namespace::Types || (ns == Namespace::Values && !v.record)));
                    Own::Name(match hit {
                        Some(v) => NameAnswer::Found {
                            def: Def { key: v.key, kind: DefKind::Other },
                            vis: Visibility::Public,
                            state: BindingState::Determined,
                            origin: blaze_surface::Origin::Declared,
                        },
                        None => NameAnswer::Absent,
                    })
                }
                // An enum has no glob edges: its variants are its own names.
                Read::Globs { module } => self.globs(module, Vec::new()),
                _ => Own::Textual(Vec::new(), textual_digest::<N>(&[], g())),
            });
        }
        if self.base.parent.contains_key(&owner) && !self.failed.contains(&owner) {
            return Err(owner);
        }
        let why = if self.failed.contains(&owner) { "read of a module whose resolution failed: answered empty" } else { "read of a key with no scope here (another crate's?): answered empty" };
        self.run.fail(why, || format!("{read:?}"));
        Ok(match read {
            Read::Name { .. } => Own::Name(NameAnswer::Absent),
            Read::Globs { module } => self.globs(module, Vec::new()),
            _ => Own::Textual(Vec::new(), textual_digest::<N>(&[], g())),
        })
    }

    /// A `Globs` answer of `module`: its edges, this crate, its chain of modules up to the root.
    fn globs(&self, module: ItemKey, edges: Vec<GlobEdge>) -> Own {
        let krate = self.base.k.key;
        let chain = self.base.chains.get(&module).cloned().unwrap_or_else(|| vec![module]);
        let digest = globs_digest(&edges, krate, &chain, g());
        Own::Globs { edges, krate, chain, digest }
    }
}

enum Machine {
    One(Resolver<'static, N>),
    Many(Group<'static, N>),
}

// SAFETY: as for `OnGlobal`: a machine made in `g()` holds only the global heap's handles.
unsafe impl Send for Machine {}

impl Machine {
    fn modules(&self) -> Vec<ItemKey> {
        match self {
            Machine::One(r) => vec![r.module()],
            Machine::Many(g) => g.modules().collect(),
        }
    }
}

enum Out {
    Ask(Vec<Read<N>>),
    Expand(Vec<ExpandRequest>),
    Cycle(Vec<ItemKey>),
    Done(Vec<Resolved<'static, N>>),
}

fn step(m: &mut Machine, facts: &Facts<'_>, replies: &[Reply<'_, N>]) -> Result<Out, ResolveError> {
    Ok(match m {
        Machine::One(r) => match r.step(facts, replies)? {
            Step::Ask(x) => Out::Ask(x.to_vec()),
            Step::Expand(x) => Out::Expand(x.to_vec()),
            Step::Cycle(x) => Out::Cycle(x.to_vec()),
            Step::Done(d) => Out::Done(vec![d]),
        },
        Machine::Many(g) => match g.step(facts, replies)? {
            Step::Ask(x) => Out::Ask(x.to_vec()),
            Step::Expand(x) => Out::Expand(x.to_vec()),
            Step::Cycle(x) => Out::Cycle(x.to_vec()),
            Step::Done(d) => Out::Done(d.into_iter().collect()),
        },
    })
    .inspect(|out| {
        if let Out::Done(d) = out {
            let stats = match m {
                Machine::One(r) => r.stats(),
                Machine::Many(g) => g.stats(),
            };
            FILL_WORK.lock().expect("the fill work's lock").push((d.len(), stats));
        }
    })
}

/// Each finished machine's work: its module count and its counts.
static FILL_WORK: std::sync::Mutex<Vec<(usize, FillStats)>> = std::sync::Mutex::new(Vec::new());

enum State {
    Fresh(ItemKey),
    /// Steps with no replies next (new, or just grouped).
    Ready,
    /// Waits on reads no finished scope answers yet.
    Waiting,
    /// Reported a cycle through these modules: waits to be grouped.
    Blocked(Vec<ItemKey>),
    Done,
    Failed,
    /// Moved into another slot's group.
    Merged,
}

struct Slot {
    machine: Option<Machine>,
    asks: Vec<Read<N>>,
    state: State,
}

#[derive(Default)]
struct RoundOut {
    done: Vec<Resolved<'static, N>>,
    failed: Vec<(ItemKey, String)>,
    steps: usize,
}

// SAFETY: as for `OnGlobal`: the scopes were made in `g()`, the global heap's handle.
unsafe impl Send for RoundOut {}

/// Steps the slot's machine as far as the finished scopes take it.
fn drive(slot: &mut Slot, me: usize, cx: &DriveCx<'_>) -> RoundOut {
    let mut out = RoundOut::default();
    if let State::Fresh(module) = slot.state {
        match Resolver::new_in(module, cx.facts, g()) {
            Ok(r) => {
                slot.machine = Some(Machine::One(r));
                slot.state = State::Ready;
            }
            Err(error) => {
                slot.state = State::Failed;
                out.failed.push((module, error.to_string()));
                return out;
            }
        }
    }
    let mut must_step = matches!(slot.state, State::Ready);
    for _ in 0..STEP_CAP {
        let asks = std::mem::take(&mut slot.asks);
        let mut owned: Vec<(Read<N>, Own)> = Vec::new();
        let mut waiting = Vec::new();
        for read in asks {
            match cx.answer(read) {
                Ok(own) => owned.push((read, own)),
                Err(owner) if cx.module_slot.get(&owner) == Some(&me) => owned.push((read, Own::InProgress(owner))),
                Err(_) => waiting.push(read),
            }
        }
        if owned.is_empty() && !must_step {
            slot.asks = waiting;
            slot.state = State::Waiting;
            return out;
        }
        must_step = false;
        let replies: Vec<Reply<'_, N>> = owned.iter().map(|(read, own)| Reply { read: *read, answer: own.answer() }).collect();
        let machine = slot.machine.as_mut().expect("a live slot holds its machine");
        out.steps += 1;
        let start = Instant::now();
        let stepped = step(machine, cx.facts, &replies);
        SOLVE_NS.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        match stepped {
            Err(error) => {
                for module in machine.modules() {
                    out.failed.push((module, error.to_string()));
                }
                slot.machine = None;
                slot.state = State::Failed;
                return out;
            }
            Ok(Out::Ask(reads)) => {
                must_step = reads.is_empty();
                slot.asks = reads;
            }
            Ok(Out::Expand(requests)) => {
                for request in requests {
                    expand_call(cx.base, cx.run, request);
                }
                must_step = true;
            }
            Ok(Out::Cycle(modules)) => {
                slot.state = State::Blocked(modules);
                return out;
            }
            Ok(Out::Done(all)) => {
                slot.machine = None;
                slot.state = State::Done;
                out.done = all;
                return out;
            }
        }
    }
    let machine = slot.machine.take().expect("a live slot holds its machine");
    for module in machine.modules() {
        out.failed.push((module, format!("more than {STEP_CAP} steps in one round")));
    }
    slot.state = State::Failed;
    out
}

/// The strongly connected components of `nodes` under `edges` (Tarjan's).
fn sccs(nodes: &[usize], edges: &HashMap<usize, Vec<usize>>) -> Vec<Vec<usize>> {
    struct T<'a> {
        edges: &'a HashMap<usize, Vec<usize>>,
        index: HashMap<usize, usize>,
        low: HashMap<usize, usize>,
        on: HashSet<usize>,
        stack: Vec<usize>, frames: Vec<(usize, usize)>,
        next: usize,
        out: Vec<Vec<usize>>,
    }
    fn visit(t: &mut T<'_>, start: usize) {
        t.index.insert(start, t.next);
        t.low.insert(start, t.next);
        t.next += 1;
        t.stack.push(start);
        t.on.insert(start);
        t.frames.push((start, 0));
        while let Some((v, edge_idx)) = t.frames.last_mut() {
            let v = *v;
            let idx = *edge_idx;
            if let Some(&w) = t.edges.get(&v).and_then(|es| es.get(idx)) {
                *edge_idx = idx + 1;
                if !t.index.contains_key(&w) {
                    t.index.insert(w, t.next);
                    t.low.insert(w, t.next);
                    t.next += 1;
                    t.stack.push(w);
                    t.on.insert(w);
                    t.frames.push((w, 0));
                } else if t.on.contains(&w) {
                    let iw = t.index[&w];
                    let lv = t.low.get_mut(&v).expect("visited");
                    *lv = (*lv).min(iw);
                }
            } else {
                t.frames.pop();
                if t.low[&v] == t.index[&v] {
                    let mut component = Vec::new();
                    loop {
                        let w = t.stack.pop().expect("v is on the stack");
                        t.on.remove(&w);
                        component.push(w);
                        if w == v {
                            break;
                        }
                    }
                    t.out.push(component);
                }
                if let Some(&(parent, _)) = t.frames.last() {
                    let lv = t.low[&v];
                    let lp = t.low.get_mut(&parent).expect("visited");
                    *lp = (*lp).min(lv);
                }
            }
        }
    }
    let mut t = T { edges, index: HashMap::new(), low: HashMap::new(), on: HashSet::new(), stack: Vec::new(), frames: Vec::new(), next: 0, out: Vec::new() };
    for &v in nodes {
        if !t.index.contains_key(&v) {
            visit(&mut t, v);
        }
    }
    t.out
}

struct Resolution {
    done: Done,
    run: Run,
    rounds: usize,
    steps: usize,
    groups: usize,
    largest: Vec<ItemKey>,
    /// Every module solved in a group of more than one (the run's, not the records').
    grouped: HashSet<ItemKey>,
    fails: Failures,
    /// Per round: modules finished, import bindings in them, expansions made, wall ms.
    per_round: Vec<(usize, usize, usize, f64)>,
}

fn resolve(base: &Base<'_>, threads: usize) -> Resolution {
    let all: Vec<ItemKey> = base.tree.modules.iter().map(|m| m.key).collect();
    resolve_some(base, threads, &all, OnGlobal(HashMap::new()), Run::default())
}

/// Resolves `modules` (in that order), every other module answered by its record in `done`;
/// `run` holds the expansions made so far, kept (an expansion is a record by its call's key).
fn resolve_some(base: &Base<'_>, threads: usize, modules: &[ItemKey], done: Done, run: Run) -> Resolution {
    let mut fails = Failures::default();
    let mut slots: Vec<Mutex<Slot>> = Vec::new();
    let mut module_slot: HashMap<ItemKey, usize> = HashMap::new();
    for &m in modules {
        module_slot.insert(m, slots.len());
        slots.push(Mutex::new(Slot { machine: None, asks: Vec::new(), state: State::Fresh(m) }));
    }
    let mut done = done;
    let mut failed: HashSet<ItemKey> = HashSet::new();
    let (mut rounds, mut steps, mut groups) = (0usize, 0usize, 0usize);
    let mut largest: Vec<ItemKey> = Vec::new();
    let mut in_groups: HashSet<ItemKey> = HashSet::new();
    let mut per_round = Vec::new();
    loop {
        let active: Vec<usize> = (0..slots.len())
            .filter(|&i| {
                let s = slots[i].get_mut().expect("no lock is poisoned");
                matches!(s.state, State::Fresh(_) | State::Ready | State::Waiting)
            })
            .collect();
        let blocked = slots.iter_mut().any(|s| matches!(s.get_mut().expect("no lock is poisoned").state, State::Blocked(_)));
        if active.is_empty() && !blocked {
            break;
        }
        rounds += 1;
        let before = done.len() + failed.len();
        let expansions_before = run.expansions.lock().expect("the expansions' lock").len();
        let round_start = Instant::now();
        let results = {
            let facts = Facts { base, run: &run };
            let cx = DriveCx { base, run: &run, facts: &facts, done: &done, failed: &failed, module_slot: &module_slot };
            let slots = &slots;
            par_map(threads, active.len(), |j| {
                let i = active[j];
                let mut slot = slots[i].lock().expect("no lock is poisoned");
                drive(&mut slot, i, &cx)
            })
        };
        let (mut finished, mut imported) = (0usize, 0usize);
        for r in results {
            steps += r.steps;
            for resolved in r.done {
                finished += 1;
                imported += resolved.scope.bindings.iter().filter(|b| b.binding.origin == blaze_surface::Origin::Import).count();
                done.insert(resolved.scope.module, resolved);
            }
            for (module, why) in r.failed {
                failed.insert(module);
                let path = base.tree.by_key.get(&module).map_or_else(|| format!("{:x}", module.0), |&i| base.tree.modules[i].path.clone());
                let kind = why.split(':').next().unwrap_or("").to_string();
                fails.add(format!("resolver refused: {kind}"), || format!("{path}: {why}"));
            }
        }
        let expansions_now = run.expansions.lock().expect("the expansions' lock").len();
        per_round.push((finished, imported, expansions_now - expansions_before, round_start.elapsed().as_secs_f64() * 1e3));
        if done.len() + failed.len() > before {
            continue;
        }
        // Nothing finished: group the cycles of the wait graph that wait on nothing outside.
        let pending: Vec<usize> = (0..slots.len())
            .filter(|&i| matches!(slots[i].get_mut().expect("no lock is poisoned").state, State::Waiting | State::Blocked(_)))
            .collect();
        if pending.is_empty() {
            continue;
        }
        let pending_set: HashSet<usize> = pending.iter().copied().collect();
        let mut edges: HashMap<usize, Vec<usize>> = HashMap::new();
        {
            let facts = Facts { base, run: &run };
            let cx = DriveCx { base, run: &run, facts: &facts, done: &done, failed: &failed, module_slot: &module_slot };
            for &i in &pending {
                let s = slots[i].lock().expect("no lock is poisoned");
                let owners: Vec<ItemKey> = match &s.state {
                    State::Blocked(modules) => modules.clone(),
                    _ => s.asks.iter().filter_map(|read| cx.answer(*read).err()).collect(),
                };
                let mut targets: Vec<usize> = owners.iter().filter_map(|o| module_slot.get(o).copied()).filter(|t| pending_set.contains(t)).collect();
                targets.sort_unstable();
                targets.dedup();
                edges.insert(i, targets);
            }
        }
        let components = sccs(&pending, &edges);
        let mut grouped = false;
        for component in components {
            let set: HashSet<usize> = component.iter().copied().collect();
            let sink = component.iter().all(|i| edges[i].iter().all(|t| set.contains(t)));
            if !sink {
                continue;
            }
            let lone_waiting = component.len() == 1 && edges[&component[0]].is_empty();
            if lone_waiting {
                let s = slots[component[0]].get_mut().expect("no lock is poisoned");
                let modules = s.machine.as_ref().map(Machine::modules).unwrap_or_default();
                s.machine = None;
                s.state = State::Failed;
                for module in modules {
                    failed.insert(module);
                    fails.add("driver: a module waits on nothing it can be answered", || format!("{:x}", module.0));
                }
                grouped = true;
                continue;
            }
            let mut members: AVec<'static, Resolver<'static, N>> = AVec::new_in(g());
            for &i in &component {
                let s = slots[i].get_mut().expect("no lock is poisoned");
                match s.machine.take() {
                    Some(Machine::One(r)) => members.push(r),
                    Some(Machine::Many(group)) => members.extend(group.into_members()),
                    None => {}
                }
                s.state = State::Merged;
                s.asks.clear();
            }
            let first = component[0];
            match Group::new_in(members, g()) {
                Ok(group) => {
                    let modules: Vec<ItemKey> = group.modules().collect();
                    for m in &modules {
                        module_slot.insert(*m, first);
                    }
                    if modules.len() > 1 {
                        in_groups.extend(modules.iter().copied());
                    }
                    if modules.len() > largest.len() {
                        largest = modules;
                    }
                    let s = slots[first].get_mut().expect("no lock is poisoned");
                    s.machine = Some(Machine::Many(group));
                    s.state = State::Ready;
                    groups += 1;
                }
                Err(error) => fails.add("Group::new_in refused", || error.to_string()),
            }
            grouped = true;
        }
        if !grouped {
            fails.add("driver: stalled with no group to form", || format!("{} modules waiting", pending.len()));
            break;
        }
    }
    Resolution { done, run, rounds, steps, groups, largest, grouped: in_groups, fails, per_round }
}

// ---------------------------------------------------------------------------------------------
// Stage 5: impls and lang items.

/// `name` in the type namespace of `m` as a reader of the finished scopes finds it: its own
/// binding, else through its glob edges.
fn scope_type(base: &Base<'_>, done: &Done, m: ItemKey, name: N) -> Option<Binding> {
    struct Records<'r> {
        krate: CrateItemKey,
        chains: &'r HashMap<ItemKey, Vec<ItemKey>>,
        done: &'r Done,
        name: N,
    }
    impl<'r> Sources<'r> for Records<'r> {
        type Stop = ();
        fn node(&mut self, source: Def) -> Result<Node<'r>, ()> {
            let Some(r) = self.done.get(&source.key) else { return Ok(Node::Value(None)) };
            if let Some(b) = r.scope.get(Namespace::Types, self.name) {
                return Ok(Node::Value(Some(Passed { def: b.def, vis: b.vis, state: b.state })));
            }
            let Some(chain) = self.chains.get(&source.key) else { return Err(()) };
            Ok(Node::Edges { edges: blaze_surface::Globs::of(&r.scope.globs), krate: self.krate, chain })
        }
    }
    if let Some(b) = done.get(&m).and_then(|r| r.scope.get(Namespace::Types, name).copied()) {
        return Some(b);
    }
    let mut sources = Records { krate: base.k.key, chains: &base.chains, done, name };
    let p = through(&mut sources, Def::module(m), g()).ok()??;
    Some(Binding { def: p.def, vis: p.vis, origin: blaze_surface::Origin::Glob, expansion: 0, state: p.state })
}

/// The definition `text` (a type path) names from `module`, read through the finished scopes.
fn resolve_type(base: &Base<'_>, done: &Done, module: ItemKey, text: &'static str) -> Option<ItemKey> {
    let path = parse_path(text, base.k.key)?;
    let root = base.tree.modules[0].key;
    let scope_get = |m: ItemKey, name: N| scope_type(base, done, m, name);
    let mut segments = path.segments.iter().copied();
    let mut cur = match path.kind {
        PathKind::Crate | PathKind::Abs | PathKind::DollarCrate(_) => root,
        PathKind::SelfMod => module,
        PathKind::Super(n) => {
            let mut at = module;
            for _ in 0..n {
                at = (*base.parent.get(&at)?)?;
            }
            at
        }
        PathKind::Plain => {
            let first = segments.next()?;
            // The lexical layers a type path's first segment goes through: the module, the
            // extern prelude (an `extern crate` at the root joins it: core's `extern crate self
            // as core`), the standard prelude.
            let extern_prelude = || {
                scope_get(root, first).filter(|b| b.origin == blaze_surface::Origin::ExternCrate)
            };
            let binding = scope_get(module, first)
                .or_else(extern_prelude)
                .or_else(|| base.root.head.prelude.and_then(|p| scope_get(p, first)))?;
            binding.def.key
        }
    };
    if path.kind != PathKind::Plain {
        let first = segments.next()?;
        cur = scope_get(cur, first)?.def.key;
    }
    for segment in segments {
        cur = scope_get(cur, segment)?.def.key;
    }
    Some(cur)
}

/// `text` without generic arguments, bounds' sugar and qualifiers: `const ops::Add<u8>` gives
/// `ops::Add`.
fn bare_path(text: &'static str) -> &'static str {
    let mut t = text.trim();
    for prefix in ["?", "!", "~const ", "[const] ", "const ", "unsafe ", "dyn "] {
        t = t.strip_prefix(prefix).unwrap_or(t).trim_start();
    }
    if let Some(rest) = t.strip_prefix("for<") {
        if let Some(end) = rest.find('>') {
            t = rest[end + 1..].trim_start();
        }
    }
    let end = t.find(|c: char| c == '<' || c == '(' || c == ' ' || c == '+').unwrap_or(t.len());
    t[..end].trim_end_matches("::")
}

struct Header {
    flags: ImplFlags,
    params: Vec<&'static str>,
    trait_: Option<&'static str>,
    self_ty: &'static str,
}

/// An impl's header, read by `blaze_file_items::impl_header` (by tokens: `const impl`, line
/// breaks and nightly forms included); `Err` says why it did not read.
fn parse_header(text: &'static str, edition: Edition) -> Result<Header, blaze_file_items::HeaderError> {
    let read = blaze_file_items::impl_header(text, edition, g())?;
    Ok(Header { flags: read.flags, params: read.params.iter().copied().collect(), trait_: read.trait_, self_ty: read.self_ty })
}

/// The first `needle` outside brackets.
fn find_top(text: &str, needle: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    for i in 0..bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'>' if i == 0 || bytes[i - 1] != b'-' => depth -= 1,
            _ => {}
        }
        if depth == 0 && text[i..].starts_with(needle) {
            return Some(i);
        }
    }
    None
}

fn shape_of(base: &Base<'_>, done: &Done, src: &ImplSrc, header: &Header) -> Result<Shape, String> {
    let t = header.self_ty.trim();
    if let Some(rest) = t.strip_prefix('&') {
        let mut rest = rest.trim_start();
        if rest.starts_with('\'') {
            rest = rest.find(' ').map_or("", |at| rest[at..].trim_start());
        }
        return Ok(Shape::Ref(if rest.starts_with("mut ") { Mutability::Mut } else { Mutability::Not }));
    }
    if t.starts_with("*const") {
        return Ok(Shape::Ptr(Mutability::Not));
    }
    if t.starts_with("*mut") {
        return Ok(Shape::Ptr(Mutability::Mut));
    }
    if t.starts_with('[') {
        return Ok(if find_top(&t[1..], ";").is_some() { Shape::Array } else { Shape::Slice });
    }
    if t.starts_with('(') {
        let inner = inside(t);
        return Ok(Shape::Tuple(split_top(inner).iter().filter(|it| !it.trim().is_empty()).count() as u32));
    }
    if t == "!" {
        return Ok(Shape::Never);
    }
    if t.starts_with("fn") || t.starts_with("unsafe ") || t.starts_with("extern ") {
        let open = t.find('(').ok_or("a fn pointer without arguments")?;
        let close = t.rfind(')').ok_or("a fn pointer without arguments")?;
        let args = &t[open + 1..close];
        return Ok(Shape::FnPtr(split_top(args).iter().filter(|it| !it.trim().is_empty()).count() as u32));
    }
    let dyn_ = t.starts_with("dyn ");
    let bare = bare_path(t);
    let prim = match bare {
        "bool" => Some(Shape::Bool),
        "char" => Some(Shape::Char),
        "str" => Some(Shape::Str),
        "isize" => Some(Shape::Int(IntWidth::Isize)),
        "i8" => Some(Shape::Int(IntWidth::I8)),
        "i16" => Some(Shape::Int(IntWidth::I16)),
        "i32" => Some(Shape::Int(IntWidth::I32)),
        "i64" => Some(Shape::Int(IntWidth::I64)),
        "i128" => Some(Shape::Int(IntWidth::I128)),
        "usize" => Some(Shape::Uint(UintWidth::Usize)),
        "u8" => Some(Shape::Uint(UintWidth::U8)),
        "u16" => Some(Shape::Uint(UintWidth::U16)),
        "u32" => Some(Shape::Uint(UintWidth::U32)),
        "u64" => Some(Shape::Uint(UintWidth::U64)),
        "u128" => Some(Shape::Uint(UintWidth::U128)),
        "f16" => Some(Shape::Float(FloatWidth::F16)),
        "f32" => Some(Shape::Float(FloatWidth::F32)),
        "f64" => Some(Shape::Float(FloatWidth::F64)),
        "f128" => Some(Shape::Float(FloatWidth::F128)),
        _ => None,
    };
    if let (Some(prim), false) = (prim, dyn_) {
        return Ok(prim);
    }
    if !dyn_ && header.params.contains(&bare) {
        return Ok(Shape::Blanket);
    }
    match resolve_type(base, done, src.module, bare) {
        Some(key) => Ok(Shape::Def(key)),
        None if dyn_ && matches!(bare, "Send" | "Sync" | "Unpin") => Ok(Shape::DynMarker),
        None => Err(format!("self type not resolved: {t}")),
    }
}

struct Indexed {
    rows: usize,
    buckets: usize,
    bytes: usize,
    langs: usize,
    fails: Failures,
}

fn index(base: &Base<'_>, res: &Resolution) -> Indexed {
    let k = base.k;
    let mut fails = Failures::default();
    let mut impls: Vec<ImplSrc> = base.low.impls.clone();
    impls.extend(res.run.impls.lock().expect("the impls' lock").iter().copied());
    let mut rows = Vec::with_capacity(impls.len());
    for src in &impls {
        let header = match parse_header(src.header, k.edition) {
            Ok(header) => header,
            Err(why) => {
                fails.add(format!("impl header not read: {why}"), || format!("{}: {}", k.at(src.loc), first_line(src.header)));
                continue;
            }
        };
        let trait_ = match header.trait_ {
            Some(text) => match resolve_type(base, &res.done, src.module, bare_path(text)) {
                Some(key) => Some(key),
                None => {
                    fails.add("impl's trait not resolved", || format!("{}: {}", k.at(src.loc), bare_path(text)));
                    continue;
                }
            },
            None => None,
        };
        let shape = match shape_of(base, &res.done, src, &header) {
            Ok(shape) => shape,
            Err(why) => {
                let kind = if trait_.is_some() { "trait impl's self type not resolved" } else { "inherent impl's self type not resolved" };
                fails.add(kind, || format!("{}: {why}", k.at(src.loc)));
                continue;
            }
        };
        let shape = match SelfShape::new(shape) {
            Ok(shape) => shape,
            Err(error) => {
                fails.add("self shape refused", || format!("{}: {error}", k.at(src.loc)));
                continue;
            }
        };
        let header_id = ContentId::of("blaze pipeline example impl header 1", src.header.as_bytes());
        rows.push(ImplRow { impl_: src.key, trait_, shape, flags: header.flags, header: header_id });
    }
    let (row_count, buckets, mut bytes) = match build_impl_index(k.key, &rows, g()) {
        Ok(index) => {
            let bytes = index.directory.len() + index.no_impls_of.len() + index.buckets.iter().map(|b| b.bytes.len()).sum::<usize>();
            for bucket in index.buckets.iter() {
                dump("impl_bucket", &bucket.bytes);
            }
            (rows.len(), index.buckets.len(), bytes)
        }
        Err(error) => {
            fails.add("build_impl_index refused", || error.to_string());
            (0, 0, 0)
        }
    };
    let mut langs: Vec<LangSrc> = base.low.langs.clone();
    langs.extend(res.run.langs.lock().expect("the langs' lock").iter().copied());
    let mut entries: Vec<(LangName, ItemKey)> = Vec::new();
    let mut seen: HashSet<&'static str> = HashSet::new();
    for lang in &langs {
        if !seen.insert(lang.name) {
            fails.add("lang item named twice (first kept)", || format!("{}: {}", k.at(lang.loc), lang.name));
            continue;
        }
        match LangName::new(lang.name) {
            Ok(name) => entries.push((name, lang.key)),
            Err(error) => fails.add("lang name refused", || format!("{}: {}: {error}", k.at(lang.loc), lang.name)),
        }
    }
    let lang_count = entries.len();
    match build_lang_items(k.key, &entries, g()) {
        Ok(record) => bytes += record.len(),
        Err(error) => fails.add("build_lang_items refused", || error.to_string()),
    }
    Indexed { rows: row_count, buckets, bytes, langs: lang_count, fails }
}

// ---------------------------------------------------------------------------------------------
// One crate, every stage.

fn run_crate(dir: &std::path::Path, counts: &[usize]) {
    let dir = normalize(&std::env::current_dir().map(|it| it.join(dir)).unwrap_or_else(|_| dir.to_path_buf()));
    let (name, edition, features) = manifest(&dir);
    let edition_assumed = edition.is_none();
    let edition = edition.unwrap_or(Edition::E2024);
    let root_path = ["src/lib.rs", "lib.rs", "src/main.rs"].iter().map(|it| dir.join(it)).find(|it| it.is_file());
    let Some(root_path) = root_path else {
        eprintln!("{}: no src/lib.rs, lib.rs or src/main.rs", dir.display());
        std::process::exit(2);
    };
    println!("\n=== crate `{name}` ({}) ===", dir.display());
    println!(
        "  edition {}{}; cfg for this host: {}",
        edition.name(),
        if edition_assumed { " (assumed: no edition in Cargo.toml)" } else { " (Cargo.toml)" },
        Cfg::host(&features).describe()
    );
    println!("  features: [{}] (Cargo.toml [features] default; nothing else on)", features.join(", "));
    let mut k = Krate {
        key: CrateItemKey(ContentId::of("blaze pipeline example crate 1", name.as_bytes())),
        name,
        edition,
        cfg: Cfg::host(&features),
        root: 0,
        files: Vec::new(),
        outlines: OnGlobal(Vec::new()),
        index: HashMap::new(),
    };
    let mut paths = Vec::new();
    let mut stack = vec![dir.clone()];
    while let Some(at) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|it| it != "target" && it != ".git") {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|it| it == "rs") {
                paths.push(normalize(&path));
            }
        }
    }
    paths.sort();
    let shown = |path: &std::path::Path| {
        let base = dir.parent().unwrap_or(&dir);
        path.strip_prefix(base).map_or_else(|_| path.display().to_string(), |it| it.display().to_string())
    };
    for path in paths {
        let rel = shown(&path);
        if let Err(error) = k.load(path, rel, true) {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
    let root_path = normalize(&root_path);
    k.root = k.index[&root_path];
    // Files outside the directory a `#[path]` reaches (core's `../../stdarch/..`): found by
    // walking until nothing new turns up, untimed, so each stage below times one fixed set.
    loop {
        let tree = walk(&k);
        let mut added = false;
        for path in tree.missing {
            if !k.index.contains_key(&path) && path.is_file() {
                let rel = shown(&path);
                if k.load(path, rel, false).is_ok() {
                    added = true;
                }
            }
        }
        if !added {
            break;
        }
    }
    let outside = k.files.iter().filter(|f| !f.in_dir).count();
    let bytes_in: usize = k.files.iter().map(|f| f.text.len()).sum();

    // Stage 1.
    println!("\n  stage 1: outline every file to its FileItems record");
    let mut times = Vec::new();
    let mut stage1 = None;
    let meter = Meter::start();
    for &threads in counts {
        let k = &k;
        stage1 = None;
        let (ms, made) = best_of_3(|| {
            par_map(threads, k.files.len(), |i| {
                let file = &k.files[i];
                let o = outline(file.text, k.edition, g());
                let bytes = o.to_file_items(file.text, file.id, g()).map(|it| it.len());
                OnGlobal((o, bytes))
            })
        });
        times.push((threads, ms));
        stage1 = Some(made);
    }
    print_times(&times);
    meter.print();
    let mut fails = Failures::default();
    let (mut items, mut record_bytes) = (0usize, 0usize);
    let made = stage1.expect("one thread count at least");
    let mut outlines = Vec::with_capacity(made.len());
    for (i, OnGlobal((o, bytes))) in made.into_iter().enumerate() {
        items += o.items().len();
        match bytes {
            Ok(bytes) => record_bytes += bytes,
            Err(error) => fails.add("FileItems record refused", || format!("{}: {error}", k.files[i].rel)),
        }
        if DUMP.get().is_some()
            && let Ok(record) = o.to_file_items(k.files[i].text, k.files[i].id, g())
        {
            dump("file_items", &record);
        }
        for error in o.errors() {
            let message: String = o.message(error).chars().take(60).collect();
            fails.add(format!("syntax: {message}"), || k.at(Loc { file: i as u32, offset: error.range.start }));
        }
        for item in o.items() {
            match item.detail {
                ItemDetail::Unhandled { syntax } => fails.add(format!("unhandled form: {:?}", syntax_kind(syntax)), || k.at(Loc { file: i as u32, offset: item.range.start })),
                ItemDetail::Error => fails.add("error entry", || k.at(Loc { file: i as u32, offset: item.range.start })),
                _ => {}
            }
        }
        outlines.push(o);
    }
    k.outlines = OnGlobal(outlines);
    println!(
        "    in: {} files ({} outside the directory, reached by #[path]), {:.1} MB; out: {items} entries; records {:.1} MB",
        k.files.len(),
        outside,
        bytes_in as f64 / 1e6,
        record_bytes as f64 / 1e6
    );
    fails.print();
    println!("    peak RSS so far {:.1} MB", peak_rss_mb());

    // Stage 2.
    println!("\n  stage 2: walk the modules from the root; lower each to its ModuleDecls");
    println!("    forms handled: inline `mod x {{}}`, `x.rs`, `x/mod.rs`, `#[path]` on a file or an inline module (a true cfg_attr's too); a module written by a macro, `include!` of items and a missing file are counted, not walked");
    let mut times = Vec::new();
    let mut stage2 = None;
    let meter = Meter::start();
    for &threads in counts {
        let k = &k;
        stage2 = None;
        let (ms, made) = best_of_3(|| {
            let tree = walk(k);
            let low = lower_modules(k, &tree, threads);
            (tree, low)
        });
        times.push((threads, ms));
        stage2 = Some(made);
    }
    print_times(&times);
    meter.print();
    let (mut tree, mut low) = stage2.expect("one thread count at least");
    let mut decl_bytes = 0usize;
    let mut records: Vec<std::vec::Vec<u8>> = Vec::with_capacity(tree.modules.len());
    for (i, m) in tree.modules.iter().enumerate() {
        let info = module_info(&k, &tree, m);
        match low.decls[i].write(m.key, &info, g()) {
            Ok(bytes) => {
                decl_bytes += bytes.len();
                dump("module_decls", &bytes);
                records.push(bytes.to_vec());
            }
            Err(error) => low.fails.add("ModuleDecls record refused", || format!("{}: {error}", m.path)),
        }
    }
    let forms: Vec<String> = tree.forms.iter().map(|(form, n)| format!("{form} {n}")).collect();
    println!(
        "    in: {} entries of the walked fragments; out: {} modules ({}), {} declarations; records {:.2} MB",
        low.items_in,
        tree.modules.len(),
        forms.join(", "),
        low.entries,
        decl_bytes as f64 / 1e6
    );
    let mut stage2_fails = std::mem::take(&mut tree.fails);
    stage2_fails.merge(std::mem::take(&mut low.fails));
    stage2_fails.print();

    // Stage 3.
    println!("\n  stage 3: the crate root");
    let mut root_fails = Failures::default();
    let (ms, cr) = best_of_3(|| {
        let mut fails = Failures::default();
        let cr = crate_root(&k, &tree, &low, &mut fails);
        root_fails = fails;
        cr
    });
    print_times(&[(1, ms)]);
    let root_bytes = match cr.write(g()) {
        Ok(bytes) => {
            let len = bytes.len();
            records.push(bytes.to_vec());
            len
        }
        Err(error) => {
            root_fails.add("CrateRoot record refused", || error.to_string());
            0
        }
    };
    println!(
        "    in: {} modules; out: 1 CrateRoot (no_std {}, no_core {}, prelude {}, {} macro exports, {} tools), {root_bytes} bytes",
        tree.modules.len(),
        cr.head.no_std,
        cr.head.no_core,
        cr.head.prelude.map_or("none".to_string(), |p| tree.by_key.get(&p).map_or("?".to_string(), |&i| tree.modules[i].path.clone())),
        cr.macro_exports.len(),
        cr.tools.len()
    );
    root_fails.print();

    // The records into a MemSurface, and back as the resolver's working forms, once.
    let load = Instant::now();
    let mut surface = MemSurface::new_in(k.key, g());
    let root_record = records.pop();
    for (i, bytes) in records.iter().enumerate() {
        if let Err(error) = surface.insert(Fact::ModuleDecls, bytes) {
            eprintln!("module {}: the ModuleDecls record does not read back: {error}", tree.modules[i].path);
        }
    }
    if let Some(bytes) = &root_record {
        if let Err(error) = surface.insert(Fact::CrateRoot, bytes) {
            eprintln!("the CrateRoot record does not read back: {error}");
        }
    }
    // Leaked, with the facts over them, so the names read from the records are `&'static`.
    let surface: &'static MemSurface<'static> = Box::leak(Box::new(surface));
    let record_facts: &'static RecordFacts<'static, 'static, MemSurface<'static>> =
        Box::leak(Box::new(RecordFacts::new_in(surface, g())));
    let mut decls = HashMap::new();
    let mut parent = HashMap::new();
    for m in &tree.modules {
        match (record_facts.module(m.key), record_facts.decls(m.key)) {
            (Ok(info), Ok(d)) => {
                parent.insert(m.key, info.parent);
                decls.insert(m.key, (info, d));
            }
            (Err(error), _) | (_, Err(error)) => eprintln!("module {}: {error}", m.path),
        }
    }
    let root = match record_facts.crate_root(k.key) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("the crate root: {error}");
            return;
        }
    };
    let load_ms = load.elapsed().as_secs_f64() * 1e3;
    let chains = parent
        .keys()
        .map(|&m| {
            let mut chain = vec![m];
            while let Some(&Some(p)) = parent.get(chain.last().expect("the chain starts with the module")) {
                chain.push(p);
            }
            (m, chain)
        })
        .collect();
    let base = Base { k: &k, tree: &tree, low: &low, decls: OnGlobal(decls), root: OnGlobal(root), parent, chains };

    // Stage 4.
    println!("\n  stage 4: names, first fill (Resolver over the MemSurface's records, in rounds)");
    println!("    records into a MemSurface and read back once: {load_ms:.1} ms (not in the times below)");
    let mut times = Vec::new();
    let mut stage4 = None;
    let mut shapes = Vec::new();
    let meter = Meter::start();
    for &threads in counts {
        stage4 = None;
        EXPAND_NS.store(0, Ordering::Relaxed);
        SOLVE_NS.store(0, Ordering::Relaxed);
        let (ms, res) = best_of_3(|| resolve(&base, threads));
        let runs = REPEAT.load(Ordering::Relaxed).max(1) as f64;
        println!(
            "    split at {threads} threads, per run, summed over threads: expansion {:.1} ms, solver steps {:.1} ms, of {ms:.1} ms wall",
            EXPAND_NS.load(Ordering::Relaxed) as f64 / 1e6 / runs,
            SOLVE_NS.load(Ordering::Relaxed) as f64 / 1e6 / runs
        );
        times.push((threads, ms));
        shapes.push((res.rounds, res.groups, res.largest.len()));
        stage4 = Some(res);
    }
    print_times(&times);
    meter.print();
    let res = stage4.expect("one thread count at least");
    let same = shapes.windows(2).all(|it| it[0] == it[1]);
    let mut scope_bytes = 0usize;
    let mut bindings = 0usize;
    let mut fails = Failures::default();
    let mut unresolved: BTreeMap<String, (usize, BTreeMap<String, usize>, Vec<String>)> = BTreeMap::new();
    let use_loc = |fragment: ItemKey, index: u32| -> Option<Loc> {
        if let Some(locs) = low.use_locs.get(&fragment) {
            return locs.get(index as usize).copied();
        }
        res.run.use_locs.lock().expect("the use places' lock").get(&fragment).and_then(|locs| locs.get(index as usize).copied())
    };
    let call_loc = |call: ItemKey| -> Option<Loc> {
        low.calls.get(&call).map(|c| c.loc).or_else(|| res.run.calls.read().expect("the calls' lock").get(&call).map(|c| c.loc))
    };
    let mut modules_sorted: Vec<&Resolved<'static, N>> = res.done.values().collect();
    modules_sorted.sort_by_key(|r| tree.by_key.get(&r.scope.module).copied().unwrap_or(usize::MAX));
    for r in modules_sorted {
        bindings += r.scope.bindings.len();
        let mpath = tree.by_key.get(&r.scope.module).map_or_else(String::new, |&i| tree.modules[i].path.clone());
        match r.scope.write(g()) {
            Ok(bytes) => {
                scope_bytes += bytes.len();
                dump("module_scope", &bytes);
            }
            Err(error) => fails.add("ModuleScope record refused", || format!("{mpath}: {error}")),
        }
        for error in r.scope.errors.iter() {
            match error {
                ScopeError::Import { site, path, segment, why } => {
                    let fragment = site.expansion.unwrap_or(r.scope.module);
                    let at = use_loc(fragment, site.index).map_or_else(|| mpath.clone(), |loc| k.at(loc));
                    let seg = path.segments.get(*segment as usize).copied().unwrap_or("");
                    let entry = unresolved.entry(format!("{why:?}")).or_default();
                    entry.0 += 1;
                    *entry.1.entry(seg.to_string()).or_default() += 1;
                    if entry.2.len() < example_cap() {
                        entry.2.push(format!("{at}: use {} (segment `{seg}`)", show_path(path)));
                    }
                }
                ScopeError::Macro { call, path, why, .. } => {
                    let at = call_loc(*call).map_or_else(|| mpath.clone(), |loc| k.at(loc));
                    fails.add(format!("macro path unresolved: {why:?}"), || format!("{at}: {}!", show_path(path)));
                }
                ScopeError::ExternCrate { name } => fails.add("extern crate not found (no dependencies in this run)", || format!("{mpath}: {name}")),
                ScopeError::Duplicate { ns, name, .. } => fails.add(format!("duplicate definition ({})", ns.name()), || format!("{mpath}: {name}")),
                other => {
                    let kind = format!("{other:?}");
                    let kind = kind.split(|c: char| c == ' ' || c == '{').next().unwrap_or("").to_string();
                    fails.add(format!("scope error: {kind}"), || format!("{mpath}: {other:?}"));
                }
            }
        }
    }
    fails.merge(std::mem::take(&mut *res.run.fails.lock().expect("the run's failures' lock")));
    let mut res = res;
    fails.merge(std::mem::take(&mut res.fails));
    let expanded = res.run.expanded.lock().expect("the expanded counts' lock").clone();
    let expansions: usize = expanded.values().sum();
    let expansion_count = res.run.expansions.lock().expect("the expansions' lock").len();
    {
        // The event counts of the floor (bottom.md, section 6): what a forward fill does once.
        let mut origin = BTreeMap::<String, usize>::new();
        // A record holds glob edges, not names copied through them.
        let (mut glob_names, mut glob_ambiguous) = (0usize, 0usize);
        for r in res.done.values() {
            glob_names += r.scope.globs.len();
            for e in r.scope.bindings.iter() {
                *origin.entry(format!("{:?}", e.binding.origin)).or_default() += 1;
                if e.binding.state != BindingState::Determined {
                    glob_ambiguous += 1;
                }
            }
        }
        let (mut imports, mut segments, mut globs, mut items, mut calls) = (0usize, 0usize, 0usize, 0usize, 0usize);
        let mut count = |d: &DeclsBuilder<'_, N>| {
            items += d.items.len() + d.mods.len() + d.extern_crates.len() + d.macro_rules.len();
            calls += d.calls.len();
            for u in d.uses.iter() {
                segments += u.path.segments.len();
                match u.leaf {
                    UseForm::Glob => globs += 1,
                    UseForm::Named { .. } => imports += 1,
                }
            }
        };
        for (_, d) in base.decls.values() {
            count(d);
        }
        for d in res.run.expansions.lock().expect("the expansions' lock").values() {
            count(d);
        }
        println!(
            "    events: declarations {items} (with expansions'), named imports {imports}, globs {globs}, path segments {segments}, macro calls {calls}; bindings by origin {origin:?}; glob edges {glob_names}; own bindings not determined {glob_ambiguous}"
        );
    }
    {
        let mut work = FILL_WORK.lock().expect("the fill work's lock");
        work.sort_by(|a, b| b.1.lookups.cmp(&a.1.lookups));
        let mut total = FillStats::default();
        for (_, s) in work.iter() {
            total.add(s);
        }
        let line = |s: &FillStats| {
            format!(
                "passes {}, relax rounds {}, items planned {}, lookups {}, finals {}, events {} (wakes {}), member steps {} ({} edge bytes), via-glob re-solves {} (spared {})",
                s.passes, s.relax_rounds, s.plans, s.lookups, s.finals, s.events, s.wakes, s.member_nodes, s.member_edge_bytes, s.agains, s.agains_spared
            )
        };
        println!("    work (all machines, every run): {}", line(&total));
        for (n, s) in work.iter().take(3) {
            println!("      a machine of {n} modules: {}", line(s));
        }
        work.clear();
    }
    println!(
        "    rounds {} ({} steps), groups {} (largest: {} modules{}), {}",
        res.rounds,
        res.steps,
        res.groups,
        res.largest.len(),
        if res.largest.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = res.largest.iter().take(8).map(|m| tree.by_key.get(m).map_or("?".to_string(), |&i| tree.modules[i].path.clone())).collect();
            format!(": {}{}", names.join(", "), if res.largest.len() > 8 { ", ..." } else { "" })
        },
        if same { "the same at every thread count" } else { "NOT the same at every thread count" }
    );
    println!(
        "    in: {} modules, {} macro calls in source; out: {} scopes ({} bindings), {} expansions asked ({} expanded, {:.2} MB of text); scope records {:.2} MB",
        tree.modules.len(),
        low.calls.len(),
        res.done.len(),
        bindings,
        expansion_count,
        expansions,
        res.run.expansion_bytes.load(Ordering::Relaxed) as f64 / 1e6,
        scope_bytes as f64 / 1e6
    );
    let read_entries: usize = res.done.values().map(|r| r.reads.entries.len()).sum();
    let read_capacity: usize = res.done.values().map(|r| r.reads.entries.capacity()).sum();
    println!(
        "    read lists: {read_entries} entries ({:.2} MB held for {read_capacity} slots)",
        (read_capacity * size_of::<blaze_scope::ReadEntry<N>>()) as f64 / 1e6
    );
    println!("    per round (last run): modules finished / import bindings in them / expansions made / ms");
    for (i, (finished, imported, expanded, ms)) in res.per_round.iter().enumerate() {
        println!("      round {:>3}: {finished:>5} / {imported:>6} / {expanded:>5} / {ms:>8.1}", i + 1);
    }
    let mut top: Vec<(&String, &usize)> = expanded.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let top: Vec<String> = top.iter().take(10).map(|(name, n)| format!("{name}! {n}")).collect();
    println!("    expanded most: {}", top.join(", "));
    let mut groups: Vec<(&String, &(usize, BTreeMap<String, usize>, Vec<String>))> = unresolved.iter().collect();
    groups.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
    let total: usize = groups.iter().map(|g| g.1.0).sum();
    println!("    unresolved imports: {total}, by reason, largest first");
    for (why, (count, segments, examples)) in groups {
        let mut segs: Vec<(&String, &usize)> = segments.iter().collect();
        segs.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let segs: Vec<String> = segs.iter().take(6).map(|(s, n)| format!("`{s}` {n}")).collect();
        println!("      {count:>7}  {why}; at segment {}", segs.join(", "));
        for example in examples {
            println!("               e.g. {example}");
        }
    }
    fails.print();
    if let Some(show) = SHOW.get() {
        // `--show NAME`: every scope's binding of the name, to see where a chain breaks.
        for r in res.done.values() {
            for e in r.scope.bindings.iter().filter(|e| e.name == show.as_str()) {
                let mpath = tree.by_key.get(&r.scope.module).map_or_else(String::new, |&i| tree.modules[i].path.clone());
                println!("    show {show}: {mpath} {:?} {:?}", e.ns, e.binding);
            }
        }
    }
    let unknown = k.cfg.unknown.lock().expect("the cfg counts' lock").clone();
    let mut unknown: Vec<(String, usize)> = unknown.into_iter().collect();
    unknown.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let unknown: Vec<String> = unknown.iter().take(20).map(|(name, n)| format!("{name} {n}")).collect();
    println!(
        "    cfg names and keys the configuration does not set (taken false), most asked first: {}; predicates not parsed: {}",
        if unknown.is_empty() { "none".to_string() } else { unknown.join(", ") },
        k.cfg.unparsed.load(Ordering::Relaxed)
    );
    println!("    peak RSS so far {:.1} MB", peak_rss_mb());

    // Stage 5.
    println!("\n  stage 5: the impl index and the lang items");
    let (ms, indexed) = best_of_3(|| index(&base, &res));
    print_times(&[(1, ms)]);
    println!(
        "    in: {} impls, {} lang attributes; out: {} impl rows in {} buckets, {} lang items; records {:.2} MB",
        low.impls.len() + res.run.impls.lock().expect("the impls' lock").len(),
        low.langs.len() + res.run.langs.lock().expect("the langs' lock").len(),
        indexed.rows,
        indexed.buckets,
        indexed.langs,
        indexed.bytes as f64 / 1e6
    );
    indexed.fails.print();
    println!("    peak RSS so far {:.1} MB", peak_rss_mb());
    if EDIT.load(Ordering::Relaxed) {
        drop(indexed);
        edits(&base, res, counts.last().copied().unwrap_or(1));
    }
}

// ---------------------------------------------------------------------------------------------
// `--edit`: the edit loop over the first fill's records.

/// After the first fill, six edits, each resolved again from the fill's records (`--edit`).
static EDIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What a read sees in an answer, as a read list holds it.
fn own_saw(own: &Own) -> blaze_scope::Saw {
    match own {
        Own::Name(a) => blaze_scope::Saw::Name(*a),
        Own::Globs { digest, .. } | Own::Textual(_, digest) | Own::Helpers(digest) => blaze_scope::Saw::Digest(*digest),
        Own::InProgress(_) => blaze_scope::Saw::Name(NameAnswer::Absent),
    }
}

type DeclMap = HashMap<ItemKey, (ModuleInfo<N>, DeclsBuilder<'static, N>)>;

/// The first fill's inputs with one edit made to the records.
fn edited_base<'k>(
    base: &Base<'k>,
    edit: impl FnOnce(&mut DeclMap, &mut HashMap<ItemKey, Option<ItemKey>>, &mut HashMap<ItemKey, Vec<ItemKey>>),
) -> Base<'k> {
    let mut decls: DeclMap = base.decls.iter().map(|(k, (info, d))| (*k, (*info, d.clone()))).collect();
    let mut parent = base.parent.clone();
    let mut chains = base.chains.clone();
    edit(&mut decls, &mut parent, &mut chains);
    Base { k: base.k, tree: base.tree, low: base.low, decls: OnGlobal(decls), root: OnGlobal((*base.root).clone()), parent, chains }
}

/// One edit's numbers.
struct EditRow {
    recomputed: usize,
    /// Kept records solved again because a rerun's derivation may go around them.
    widened: usize,
    waves: usize,
    plans: u64,
    events: u64,
    ms: f64,
    changed: usize,
    equal: bool,
}

/// Resolves again after an edit of `before`'s records into `base`'s, the records of `edited`
/// changed: they first, widened by [`blaze_scope::Again::suspects`] to the kept records their
/// derivation may go around (solved again with them, so a cycle the edit closes is solved
/// from text), then every module whose read list no longer holds against today's answers,
/// until every read list holds.
fn incremental(before: &Base<'_>, base: &Base<'_>, threads: usize, orig: &Done, run: Run, edited: &[ItemKey]) -> (Done, Run, EditRow) {
    let order = |set: &HashSet<ItemKey>| -> Vec<ItemKey> {
        let mut v: Vec<ItemKey> = set.iter().copied().collect();
        v.sort_by_key(|m| (base.tree.by_key.get(m).copied().unwrap_or(usize::MAX), m.0));
        v
    };
    let work_before = FILL_WORK.lock().expect("the fill work's lock").len();
    let start = Instant::now();
    let parent = |m: ItemKey| base.parent.get(&m).copied().flatten();
    let again = blaze_scope::Again::new_in(orig.iter().map(|(m, r)| (*m, &r.reads)), |m| before.parent.get(&m).copied().flatten(), g());
    let touched_by_text = |m: ItemKey| match (before.decls.get(&m), base.decls.get(&m)) {
        (Some((_, a)), Some((_, b))) => Touched::of_text(a, b, g()),
        _ => Touched::wild(g()),
    };
    let mut touched: HashMap<ItemKey, Touched<'static, N>> = HashMap::new();
    let mut done: Done = OnGlobal(orig.iter().map(|(k, v)| (*k, v.clone())).collect());
    let mut run = run;
    let mut first = HashSet::new();
    for &m in edited {
        first.insert(m);
        touched.insert(m, touched_by_text(m));
    }
    let mut dirty = order(&first);
    let mut recomputed: HashSet<ItemKey> = HashSet::new();
    let mut waves = 0usize;
    let mut widened = 0usize;
    while !dirty.is_empty() {
        // One wave: the dirty modules from their text, widened until no kept record they read
        // reaches back to what they derive anew.
        loop {
            waves += 1;
            for m in &dirty {
                done.remove(m);
                recomputed.insert(*m);
            }
            let r = resolve_some(base, threads, &dirty, done, run);
            let Resolution { done: d, run: rn, .. } = r;
            done = d;
            run = rn;
            for m in &dirty {
                if let (Some(o), Some(n)) = (orig.get(m), done.get(m)) {
                    touched.get_mut(m).expect("every dirty module is touched").add_changed(&o.scope, &n.scope);
                }
            }
            let rerun: Vec<(ItemKey, &blaze_scope::ReadList<'static, N>, &Touched<'static, N>)> =
                dirty.iter().filter_map(|m| Some((*m, &done.get(m)?.reads, touched.get(m)?))).collect();
            let suspects = again.suspects(&rerun, |m| !recomputed.contains(&m) && done.contains_key(&m), parent);
            if suspects.is_empty() {
                break;
            }
            widened += suspects.len();
            for &s in suspects.iter() {
                touched.insert(s, Touched::wild(g()));
            }
            let mut wave: HashSet<ItemKey> = dirty.iter().copied().collect();
            wave.extend(suspects.iter().copied());
            dirty = order(&wave);
        }
        let mut next = HashSet::new();
        {
            let facts = Facts { base, run: &run };
            let failed = HashSet::new();
            let module_slot = HashMap::new();
            let cx = DriveCx { base, run: &run, facts: &facts, done: &done, failed: &failed, module_slot: &module_slot };
            for (m, rec) in done.iter() {
                let holds = rec.reads.holds(|read| match cx.answer(*read) {
                    Ok(own) => own_saw(&own),
                    Err(_) => blaze_scope::Saw::Name(NameAnswer::Absent),
                });
                if !holds {
                    next.insert(*m);
                }
            }
        }
        for &m in &next {
            touched.insert(m, Touched::wild(g()));
        }
        dirty = order(&next);
    }
    let ms = start.elapsed().as_secs_f64() * 1e3;
    let (plans, events) = FILL_WORK.lock().expect("the fill work's lock")[work_before..]
        .iter()
        .fold((0u64, 0u64), |(p, e), (_, s)| (p + s.plans, e + s.events));
    let changed = done.iter().filter(|(m, r)| orig.get(m).is_none_or(|o| o.scope != r.scope)).count();
    // Every module of the edited records, a new one too.
    let all: HashSet<ItemKey> = base.decls.keys().copied().collect();
    let cold = resolve_some(base, threads, &order(&all), OnGlobal(HashMap::new()), Run::default());
    let equal = cold.done.len() == done.len()
        && done.iter().all(|(m, r)| cold.done.get(m).is_some_and(|c| c.scope == r.scope && c.reads == r.reads));
    if !equal {
        println!("      cold {} records, incremental {}", cold.done.len(), done.len());
        let mut shown = 0;
        for (m, r) in done.iter() {
            let Some(c) = cold.done.get(m) else {
                println!("      {:x}: not in the cold fill", m.0);
                continue;
            };
            if c.scope == r.scope && c.reads == r.reads {
                continue;
            }
            shown += 1;
            if shown > 2 {
                break;
            }
            println!("      {:x}: scope equal {}, reads equal {}", m.0, c.scope == r.scope, c.reads == r.reads);
            if c.scope != r.scope {
                let (cs, rs) = (format!("{:?}", c.scope), format!("{:?}", r.scope));
                let at = cs.bytes().zip(rs.bytes()).position(|(a, b)| a != b).unwrap_or(0);
                println!("        cold: …{}", &cs[at.saturating_sub(120)..(at + 200).min(cs.len())]);
                println!("        incr: …{}", &rs[at.saturating_sub(120)..(at + 200).min(rs.len())]);
            } else {
                let only_cold: Vec<_> = c.reads.entries.iter().filter(|e| !r.reads.entries.contains(e)).take(3).collect();
                let only_incr: Vec<_> = r.reads.entries.iter().filter(|e| !c.reads.entries.contains(e)).take(3).collect();
                println!("        reads only cold: {only_cold:?}\n        reads only incr: {only_incr:?}");
            }
        }
    }
    drop(cold);
    (done, run, EditRow { recomputed: recomputed.len(), widened, waves, plans, events, ms, changed, equal })
}

/// The six edits of the edit loop, each the smallest of its kind on `core`'s real modules.
fn edits(base: &Base<'_>, res: Resolution, threads: usize) {
    println!("\n  edit loop: each edit resolved again from the first fill's records ({threads} threads)");
    let Resolution { done: orig, run, largest, grouped, .. } = res;
    let big: HashSet<ItemKey> = if largest.len() >= 16 { largest.iter().copied().collect() } else { HashSet::new() };
    let path = |m: ItemKey| base.tree.by_key.get(&m).map_or_else(|| format!("{:x}", m.0), |&i| base.tree.modules[i].path.clone());
    let in_order: Vec<ItemKey> = base.tree.modules.iter().map(|m| m.key).collect();
    let krate = base.k.key;
    let private_fn = |name: N, tag: &[u8]| DeclaredItem {
        name,
        ns: Namespace::Values,
        def: Def { key: make_key(krate, &[b"blaze edit", tag]), kind: DefKind::Other },
        vis: WrittenVis::Private,
    };
    let print = |label: &str, row: &EditRow| {
        println!(
            "    {label}\n      records recomputed {} ({} as suspects) in {} waves, items evaluated {}, events {}, {:.1} ms; records changed {}; equal to a cold fill of the edited records: {}",
            row.recomputed, row.widened, row.waves, row.plans, row.events, row.ms, row.changed, if row.equal { "yes" } else { "NO" }
        );
    };
    let mut run = run;
    let alone = |m: &ItemKey| !grouped.contains(m);

    // (a) A private fn added to a leaf module.
    let leaf = in_order.iter().copied().find(|m| alone(m) && base.decls.get(m).is_some_and(|(_, d)| d.mods.is_empty()));
    if let Some(leaf) = leaf {
        let b = edited_base(base, |decls, _, _| {
            decls.get_mut(&leaf).expect("the leaf's record").1.items.push(private_fn("blaze_edit_a", b"a"));
        });
        let (_, rn, row) = incremental(base, &b, threads, &orig, run, &[leaf]);
        run = rn;
        print(&format!("(a) private fn added to leaf {}", path(leaf)), &row);
    }

    // (b) A private fn's body changed: the outline is equal, so no record is edited at all.
    {
        let b = edited_base(base, |_, _, _| {});
        let (_, rn, row) = incremental(base, &b, threads, &orig, run, &[]);
        run = rn;
        print("(b) a fn body changed (outline equal: no name record edited)", &row);
    }

    // (c) A `pub fn` added to the module most glob-imported (outside the big group if one is).
    let mut importers: HashMap<ItemKey, Vec<ItemKey>> = HashMap::new();
    for (m, r) in orig.iter() {
        for e in r.scope.globs.iter() {
            importers.entry(e.source.key).or_default().push(*m);
        }
    }
    let source = importers
        .iter()
        .filter(|(s, _)| base.decls.contains_key(*s))
        .max_by_key(|(s, i)| (!big.contains(*s), i.len(), s.0))
        .map(|(s, i)| (*s, i.clone()));
    if let Some((s, imps)) = source {
        let b = edited_base(base, |decls, _, _| {
            let mut item = private_fn("blaze_edit_c", b"c");
            item.vis = WrittenVis::Pub;
            decls.get_mut(&s).expect("the source's record").1.items.push(item);
        });
        let (_, rn, row) = incremental(base, &b, threads, &orig, run, &[s]);
        run = rn;
        let inside = imps.iter().filter(|m| big.contains(m)).count();
        print(
            &format!(
                "(c) pub fn added to {} (glob-imported by {} modules, {inside} of them in the big group; it is {}in it)",
                path(s),
                imps.len(),
                if big.contains(&s) { "" } else { "not " }
            ),
            &row,
        );
    }

    // (d) A `pub use` added inside the big group: an existing named `pub use` again, renamed.
    let member = in_order.iter().copied().find(|m| {
        big.contains(m)
            && base.decls.get(m).is_some_and(|(_, d)| {
                d.uses.iter().any(|u| matches!(u.vis, WrittenVis::Pub) && matches!(u.leaf, UseForm::Named { .. }))
            })
    });
    if let Some(member) = member {
        let b = edited_base(base, |decls, _, _| {
            let d = &mut decls.get_mut(&member).expect("the member's record").1;
            let at = d.uses.iter().position(|u| matches!(u.vis, WrittenVis::Pub) && matches!(u.leaf, UseForm::Named { .. }));
            let mut u = d.uses[at.expect("found above")].clone();
            if let UseForm::Named { types_only, .. } = u.leaf {
                u.leaf = UseForm::Named { rename: Rename::To("blaze_edit_d"), types_only };
            }
            u.pos = 1 << 30;
            d.uses.push(u);
        });
        let (_, rn, row) = incremental(base, &b, threads, &orig, run, &[member]);
        run = rn;
        print(&format!("(d) pub use added in {} (a member of the {}-module group)", path(member), big.len()), &row);
    }

    // (e) A declared name removed that another module reads and found (outside the big group
    // if one is).
    let mut readers: HashMap<(ItemKey, N), usize> = HashMap::new();
    for (m, r) in orig.iter() {
        for e in r.reads.entries.iter() {
            if let (Read::Name { module, name, .. }, blaze_scope::Saw::Name(NameAnswer::Found { .. })) = (e.read, e.saw) {
                if module != *m {
                    *readers.entry((module, name)).or_default() += 1;
                }
            }
        }
    }
    let removed = readers
        .iter()
        .filter(|((m, name), _)| base.decls.get(m).is_some_and(|(_, d)| d.items.iter().any(|it| it.name == *name)))
        .max_by_key(|((m, name), n)| (!big.contains(m), std::cmp::Reverse(**n), m.0, *name))
        .map(|(k, n)| (*k, *n));
    if let Some(((m, name), n)) = removed {
        let b = edited_base(base, |decls, _, _| {
            decls.get_mut(&m).expect("the module's record").1.items.retain(|it| it.name != name);
        });
        let (_, rn, row) = incremental(base, &b, threads, &orig, run, &[m]);
        run = rn;
        print(&format!("(e) `{name}` removed from {} (read and found by {n} other modules)", path(m)), &row);
    }

    // (f) A new `mod blaze_edit_f;` with one private fn, under the leaf of (a).
    if let Some(leaf) = leaf {
        let child = make_key(krate, &[b"blaze edit", b"f"]);
        let b = edited_base(base, |decls, parent, chains| {
            let (info, d) = decls.get_mut(&leaf).expect("the leaf's record");
            let no_prelude = info.no_implicit_prelude;
            d.mods.push(ChildModDecl { name: "blaze_edit_f", module: child, vis: WrittenVis::Private, macro_use: false, pos: 1 << 30 });
            let mut own = DeclsBuilder::new_in(g());
            own.items.push(private_fn("blaze_edit_f_fn", b"f fn"));
            decls.insert(child, (ModuleInfo { krate, parent: Some(leaf), name: Some("blaze_edit_f"), no_implicit_prelude: no_prelude }, own));
            parent.insert(child, Some(leaf));
            let mut chain = vec![child];
            chain.extend(chains.get(&leaf).cloned().unwrap_or_default());
            chains.insert(child, chain);
        });
        let (_, rn, row) = incremental(base, &b, threads, &orig, run, &[leaf, child]);
        run = rn;
        print(&format!("(f) new mod blaze_edit_f (one private fn) under {}", path(leaf)), &row);
    }
    // The fill's records lend names from the run's texts: dropped first.
    drop(orig);
    drop(run);
}
