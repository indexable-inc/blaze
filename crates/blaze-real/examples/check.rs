//! The checker at scale: every body of real crates through the ladder's front end
//! (crates/blaze-ladder/src/front.rs: outline, `lower_signature`, `lower_body`, `to_typeck`, its
//! names and its `World`) and blaze-typeck's `check_logged`, with what stops each body, by
//! cause, and what each check costs (typeck.md, "Checker at scale (2026-11-16)").
//!
//! `check <crate dir>...`: each crate is one program of `src/**/*.rs`, rooted at `src/lib.rs`
//! (or `src/main.rs`). The front end stops a program at its first refusal, so the harness
//! takes the census by elimination: it blanks the refused item (its bytes become spaces, its
//! newlines stay, in this process's copy of the text: the checkout is read, never written),
//! notes the refusal as the cause of every body inside that item, and builds the program
//! again, until the front end takes what is left. Then every body left is checked in the
//! front end's world. A refusal that names an item blanked before is a cascade, counted apart.
//!
//! A body's causes: the front's refusal of its item (one, the first: the front stops there),
//! or the checker's error, or each distinct diagnostic code of its record. Every crate given
//! compiles with rustc, so every diagnostic is a wrong answer of the front end's stand-in or of
//! the checker; the table says which kind each cause is.
//!
//! Env: `CHECK_THREADS` (default 18) for the parallel pass; `CHECK_SAMPLES` (default 30) for
//! how many snippets per cause are printed; `CHECK_REPEAT` (default 1) repeats the one-thread
//! check pass, for a sampling profiler to attach to.
//!
//! Run with `examplerun.sh blaze-real check <dirs>`. A measurement, not a test.

#![feature(allocator_api)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use blaze_ladder::front::{BlazeAttrs, Front};
use blaze_ladder::refusal::{Refusal, SourceFile};
use blaze_own::Alloc;
use blaze_surface::{Edition, ItemDetail};
use blaze_typeck::{
    AssocItem, AskLog, CheckError, Clock, Def, Fetch, FnSig, Hidden, ImplHeader, InferCtx, ItemKey, LangItem, Name,
    Malformed, Needs, Order, Outcome, PathIdx, Predicates, Question, ScopeId, Seal, Shape, TraitDef, TypeAttrs, World,
    restarts,
};

// ---------------------------------------------------------------------------------------------
// A counting allocator: the heap's live bytes and its peak, for the check pass's footprint.

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const EDITION: Edition = Edition::E2024;

// ---------------------------------------------------------------------------------------------
// The world, timed: every question the checker asks, counted and timed by kind, then answered
// by the front end's world. Harness counters only; the checker is unchanged.

const KINDS: usize = 26;

fn kind_of(q: &Question) -> usize {
    match q {
        Question::FnSig(_) => 0,
        Question::Predicates(_) => 1,
        Question::Adt(_) => 2,
        Question::ImplsOf(..) => 3,
        Question::ImplHeader(_) => 4,
        Question::Inherent(..) => 5,
        Question::AssocItem(..) => 6,
        Question::TraitsInScope(_) => 7,
        Question::Path(..) => 8,
        Question::Lang(_) => 9,
        Question::LangPath(_) => 10,
        Question::Const(_) => 11,
        Question::Opaque(_) => 12,
        Question::Attrs(_) => 13,
        Question::TraitDef(_) => 14,
        Question::AssocParent(_) => 15,
        Question::ConstHolds(..) => 16,
        Question::ConstParams(_) => 17,
        Question::Closure(..) => 19,
        Question::AssocGenerics(..) => 20,
        Question::Coroutine(..) => 21,
        Question::TypeAlias(_) => 22,
        Question::FieldDefaults(_) => 23,
        Question::MemberVisible(..) => 24,
        Question::MemberUnstable(..) => 25,
    }
}

const KIND_NAMES: [&str; KINDS] = [
    "FnSig", "Predicates", "Adt", "ImplsOf", "ImplHeader", "Inherent", "AssocItem", "TraitsInScope", "Path", "Lang",
    "LangPath", "Const", "Opaque", "Attrs", "TraitDef", "AssocParent", "ConstHolds", "ConstParams", "prefetch", "Closure", "AssocGenerics",
    "Coroutine", "TypeAlias", "FieldDefaults", "MemberVisible", "MemberUnstable",
];

struct Tally {
    calls: [AtomicU64; KINDS],
    ns: [AtomicU64; KINDS],
}

static WORLD: Tally = Tally { calls: [const { AtomicU64::new(0) }; KINDS], ns: [const { AtomicU64::new(0) }; KINDS] };

struct Timed<'w, W> {
    inner: &'w W,
    clock: Instant,
}

impl<W> Timed<'_, W> {
    fn time<T>(&self, k: usize, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let out = f();
        WORLD.calls[k].fetch_add(1, Ordering::Relaxed);
        WORLD.ns[k].fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        out
    }
}

impl<W: World> World for Timed<'_, W> {
    fn fn_sig(&self, seal: Seal, item: ItemKey) -> Fetch<Option<FnSig<'_>>> {
        self.time(0, || self.inner.fn_sig(seal, item))
    }
    fn predicates(&self, seal: Seal, item: ItemKey) -> Fetch<Predicates<'_>> {
        self.time(1, || self.inner.predicates(seal, item))
    }
    fn adt(&self, seal: Seal, item: ItemKey) -> Fetch<Option<blaze_typeck::Adt<'_>>> {
        self.time(2, || self.inner.adt(seal, item))
    }
    fn impls_of(&self, seal: Seal, trait_: ItemKey, shape: Shape) -> Fetch<&[ItemKey]> {
        self.time(3, || self.inner.impls_of(seal, trait_, shape))
    }
    fn impl_header(&self, seal: Seal, item: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        self.time(4, || self.inner.impl_header(seal, item))
    }
    fn inherent(&self, seal: Seal, shape: Shape, name: Name) -> Fetch<&[AssocItem]> {
        self.time(5, || self.inner.inherent(seal, shape, name))
    }
    fn assoc_item(&self, seal: Seal, container: ItemKey, name: Name) -> Fetch<Option<AssocItem>> {
        self.time(6, || self.inner.assoc_item(seal, container, name))
    }
    fn traits_in_scope(&self, seal: Seal, scope: ScopeId) -> Fetch<&[ItemKey]> {
        self.time(7, || self.inner.traits_in_scope(seal, scope))
    }
    fn resolve_path(&self, seal: Seal, scope: ScopeId, path: PathIdx, segments: &[Name]) -> Fetch<blaze_typeck::PathAnswer> {
        self.time(8, || self.inner.resolve_path(seal, scope, path, segments))
    }
    fn lang_item(&self, seal: Seal, item: LangItem) -> Fetch<Option<ItemKey>> {
        self.time(9, || self.inner.lang_item(seal, item))
    }
    fn lang_path(&self, seal: Seal, item: LangItem) -> Fetch<Option<Def>> {
        self.time(10, || self.inner.lang_path(seal, item))
    }
    fn const_value(&self, seal: Seal, item: ItemKey) -> Fetch<Option<u64>> {
        self.time(11, || self.inner.const_value(seal, item))
    }
    fn opaque_hidden(&self, seal: Seal, item: ItemKey) -> Fetch<Option<Hidden<'_>>> {
        self.time(12, || self.inner.opaque_hidden(seal, item))
    }
    fn closure(&self, seal: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<blaze_typeck::ClosureFacts<'_>>> {
        self.time(19, || self.inner.closure(seal, owner, ordinal))
    }
    fn coroutine(&self, seal: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<blaze_typeck::CoroutineFacts<'_>>> {
        self.time(21, || self.inner.coroutine(seal, owner, ordinal))
    }
    fn attrs(&self, seal: Seal, item: ItemKey) -> Fetch<TypeAttrs> {
        self.time(13, || self.inner.attrs(seal, item))
    }
    fn trait_def(&self, seal: Seal, trait_: ItemKey) -> Fetch<Option<TraitDef<'_>>> {
        self.time(14, || self.inner.trait_def(seal, trait_))
    }
    fn assoc_parent(&self, seal: Seal, item: ItemKey) -> Fetch<Option<blaze_typeck::AssocParent>> {
        self.time(15, || self.inner.assoc_parent(seal, item))
    }
    fn const_holds(
        &self,
        seal: Seal,
        cond: ItemKey,
        args: &[blaze_typeck::PTy],
        types: blaze_typeck::PTys<'_>,
    ) -> Fetch<blaze_typeck::ConstHolds> {
        self.time(16, || self.inner.const_holds(seal, cond, args, types))
    }
    fn const_params(&self, seal: Seal, item: ItemKey) -> Fetch<blaze_typeck::ConstParams<'_>> {
        self.time(17, || self.inner.const_params(seal, item))
    }
    fn assoc_generics(&self, seal: Seal, trait_: ItemKey, name: blaze_typeck::Name) -> Fetch<Option<blaze_typeck::AssocGenerics>> {
        self.time(20, || self.inner.assoc_generics(seal, trait_, name))
    }
    fn type_alias(&self, seal: Seal, item: ItemKey) -> Fetch<Option<blaze_typeck::AliasDef<'_>>> {
        self.time(22, || self.inner.type_alias(seal, item))
    }
    fn field_defaults(&self, seal: Seal, adt: ItemKey) -> Fetch<&[Option<ItemKey>]> {
        self.time(23, || self.inner.field_defaults(seal, adt))
    }
    fn member_visible(&self, seal: Seal, container: ItemKey, name: blaze_typeck::Name) -> Fetch<bool> {
        self.time(24, || self.inner.member_visible(seal, container, name))
    }
    fn member_unstable(&self, seal: Seal, container: ItemKey, name: blaze_typeck::Name) -> Fetch<bool> {
        self.time(25, || self.inner.member_unstable(seal, container, name))
    }
    fn prefetch(&self, seal: Seal, questions: &[Question]) {
        self.time(18,|| self.inner.prefetch(seal, questions))
    }
}

impl<W> Clock for Timed<'_, W> {
    fn now_ns(&self) -> u64 {
        self.clock.elapsed().as_nanos() as u64
    }
}

// ---------------------------------------------------------------------------------------------
// Causes.

#[derive(Default)]
struct Cause {
    /// Bodies it stops (a body under each of its causes).
    bodies: usize,
    /// Bodies it alone stops.
    alone: usize,
    /// Items it refused (front causes).
    items: usize,
    /// Up to `samples` places with their snippets.
    samples: Vec<String>,
}

#[derive(Default)]
struct Census {
    causes: BTreeMap<String, Cause>,
    bodies: usize,
    clean: usize,
    checked: usize,
}

fn samples_wanted() -> usize {
    std::env::var("CHECK_SAMPLES").ok().and_then(|s| s.parse().ok()).unwrap_or(30)
}

impl Census {
    fn note_item(&mut self, cause: &str) {
        self.causes.entry(cause.to_string()).or_default().items += 1;
    }

    /// One body with its causes (distinct), each with a sample.
    fn body(&mut self, causes: &[(String, String)]) {
        self.bodies += 1;
        if causes.is_empty() {
            self.clean += 1;
            return;
        }
        let want = samples_wanted();
        let mut seen: Vec<&str> = Vec::new();
        for (cause, sample) in causes {
            if seen.contains(&cause.as_str()) {
                continue;
            }
            seen.push(cause);
            let c = self.causes.entry(cause.clone()).or_default();
            c.bodies += 1;
            if c.samples.len() < want {
                c.samples.push(sample.clone());
            }
        }
        if seen.len() == 1 {
            self.causes.get_mut(seen[0]).map(|c| c.alone += 1);
        }
    }
}

/// A refusal's words with the names in backticks and the numbers taken out, so one kind of
/// refusal is one cause.
fn normalize(message: &str) -> String {
    let mut out = String::new();
    let mut tick = false;
    for ch in message.chars() {
        if ch == '`' {
            tick = !tick;
            out.push_str(if tick { "`_" } else { "`" });
            continue;
        }
        if tick {
            continue;
        }
        if ch.is_ascii_digit() {
            if !out.ends_with('N') {
                out.push('N');
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// The names in backticks of a message.
fn ticked(message: &str) -> Vec<&str> {
    message.split('`').skip(1).step_by(2).collect()
}

fn one_line(s: &str, most: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > most { format!("{}…", flat.chars().take(most).collect::<String>()) } else { flat }
}

fn line_of(text: &str, at: usize) -> usize {
    text.as_bytes()[..at.min(text.len())].iter().filter(|b| **b == b'\n').count() + 1
}

// ---------------------------------------------------------------------------------------------
// One crate.

struct CrateFile {
    rel: String,
    text: String,
    /// The original text's body items: (start, end) of each fn, const or static with a body.
    bodies: Vec<(u32, u32)>,
    /// The original outline's items that can be blanked whole: (start, end, name).
    items: Vec<(u32, u32, Option<String>)>,
    /// Blanked ranges with their cause.
    blanked: Vec<(u32, u32, String)>,
}

fn rs_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut todo = vec![dir.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let Ok(listed) = std::fs::read_dir(&dir) else { continue };
        for entry in listed.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                todo.push(entry.path());
            } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "rs") {
                out.push(entry.path());
            }
        }
    }
    out.sort();
}

fn read_crate(dir: &Path) -> (Vec<CrateFile>, Option<String>) {
    let mut paths = Vec::new();
    rs_under(&dir.join("src"), &mut paths);
    let mut files = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let rel = path.strip_prefix(dir).map_or_else(|_| path.display().to_string(), |p| p.display().to_string());
        let made = blaze_file_items::outline(&text, EDITION, Alloc::global());
        let mut bodies = Vec::new();
        let mut items = Vec::new();
        for item in made.items() {
            let has_body = matches!(item.detail, ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. });
            if has_body && item.body.is_some() {
                bodies.push((item.range.start, item.range.end));
            }
            if !matches!(item.detail, ItemDetail::Variant { .. } | ItemDetail::Field { .. }) {
                let name = item.name.and_then(|r| text.get(r.start as usize..r.end as usize)).map(str::to_string);
                items.push((item.range.start, item.range.end, name));
            }
        }
        files.push(CrateFile { rel, text, bodies, items, blanked: Vec::new() });
    }
    let root = ["src/lib.rs", "src/main.rs"].into_iter().find(|r| files.iter().any(|f| f.rel == *r)).map(str::to_string);
    (files, root)
}

/// The front's cause for a refusal: its construct, or its words normalized; a cascade when it
/// names an item blanked before.
fn front_cause(r: &Refusal, blanked_names: &[String]) -> String {
    let rule = r.rule.name();
    match r.construct {
        Some(c) => format!("front {rule}: {c}"),
        None => {
            if ticked(&r.message).iter().any(|n| {
                let last = n.rsplit("::").next().unwrap_or(n);
                blanked_names.iter().any(|b| b == last)
            }) {
                format!("front {rule}: names an item refused before (cascade)")
            } else {
                format!("front {rule}: {}", normalize(&r.message))
            }
        }
    }
}

struct Body {
    file: usize,
    start: u32,
    end: u32,
}

struct Cost {
    ns: u64,
    /// The floor (typeck's examples/cost.rs `floor_one`): writing this record and nothing else,
    /// best of three.
    floor_ns: u64,
    asks: u32,
    steps: u32,
    nodes: usize,
    rec_bytes: usize,
    label: String,
}

#[derive(Default)]
struct Restart {
    wasted: [u64; 3],
    total: [u64; 3],
    entries: [u64; 3],
}

/// Blanks the smallest whole item of `file` that holds byte `place`; false when none does or it
/// is blank already.
fn blank(file: &mut CrateFile, place: u32, cause: &str) -> Option<Option<String>> {
    let best = file
        .items
        .iter()
        .filter(|(s, e, _)| *s <= place && place < *e)
        .min_by_key(|(s, e, _)| e - s)
        .cloned()?;
    let (s, e, name) = best;
    if file.blanked.iter().any(|(bs, be, _)| *bs <= s && e <= *be) {
        return None;
    }
    // SAFETY of the edit: only ASCII spaces replace bytes, newlines stay, so the text stays
    // UTF-8 and every other item keeps its offsets.
    let mut bytes = std::mem::take(&mut file.text).into_bytes();
    for b in &mut bytes[s as usize..e as usize] {
        if *b != b'\n' {
            *b = b' ';
        }
    }
    file.text = String::from_utf8(bytes).unwrap_or_default();
    file.blanked.push((s, e, cause.to_string()));
    Some(name)
}

fn run_crate(dir: &Path, census: &mut Census, costs: &mut Vec<Cost>, restart: &mut Restart, threads: usize, repeat: usize) {
    let (mut files, root) = read_crate(dir);
    let name = dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned());
    let total_bodies: usize = files.iter().map(|f| f.bodies.len()).sum();
    let Some(root) = root else {
        println!("  {name}: no src/lib.rs or src/main.rs");
        return;
    };
    let mut blanked_names: Vec<String> = Vec::new();
    let mut rounds = 0usize;
    let started = Instant::now();
    let mut stuck: Option<String> = None;
    let mut local = Census::default();
    // Elimination: build, blank the refused item, build again.
    loop {
        rounds += 1;
        let cap = std::env::var("CHECK_CAP_S").ok().and_then(|s| s.parse().ok()).unwrap_or(240u64);
        if rounds > 50_000 || started.elapsed().as_secs() > cap {
            stuck = Some(format!("gave up after {rounds} rounds"));
            break;
        }
        let refusal = {
            let sources: Vec<SourceFile<'_>> = files.iter().map(|f| SourceFile { path: &f.rel, text: &f.text }).collect();
            let mut attrs = BlazeAttrs::default();
            match Front::new(&sources, &root, &mut attrs, Alloc::global()) {
                Ok(_) => None,
                Err(r) => Some(r),
            }
        };
        let Some(r) = refusal else { break };
        let cause = front_cause(&r, &blanked_names);
        let Some(fi) = r.file.map(|f| f as usize) else {
            stuck = Some(format!("a refusal of no file: {}", r.message));
            break;
        };
        // A syntax refusal is at the file's offset, not an item's.
        let at = if r.place == 0 { r.start } else { r.place };
        let snippet = {
            let t = &files[fi].text;
            let s = (r.place + r.start) as usize;
            let e = ((r.place + r.end) as usize).max(s + 1).min(t.len());
            format!("{}:{}: {}", files[fi].rel, line_of(t, s), one_line(t.get(s..e).unwrap_or(""), 100))
        };
        match blank(&mut files[fi], at, &cause) {
            Some(n) => {
                if let Some(n) = n {
                    blanked_names.push(n);
                }
                local.note_item(&cause);
                let c = local.causes.entry(cause.clone()).or_default();
                if c.samples.len() < samples_wanted() {
                    c.samples.push(snippet);
                }
            }
            None => {
                stuck = Some(format!("no item to blank for {cause} at {snippet}"));
                break;
            }
        }
    }
    let front_ms = started.elapsed().as_secs_f64() * 1e3;
    // Bodies inside a blanked item take its cause.
    let mut live: Vec<Body> = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        for &(s, e) in &f.bodies {
            match f.blanked.iter().find(|(bs, be, _)| *bs <= s && e <= *be) {
                Some((_, _, cause)) => {
                    local.body(&[(cause.clone(), String::new())]);
                }
                None => live.push(Body { file: fi, start: s, end: e }),
            }
        }
    }
    if let Some(why) = &stuck {
        for _ in &live {
            local.body(&[(format!("front: elimination stopped ({})", one_line(why, 60)), String::new())]);
        }
        println!("  {name}: elimination stopped: {why}");
        merge(census, local);
        return;
    }
    let sources: Vec<SourceFile<'_>> = files.iter().map(|f| SourceFile { path: &f.rel, text: &f.text }).collect();
    let mut attrs = BlazeAttrs::default();
    let Ok(front) = Front::new(&sources, &root, &mut attrs, Alloc::global()) else {
        println!("  {name}: the front refused the program it took a moment ago");
        return;
    };
    // The front's fns with bodies, by (file, place).
    let mut by_place: HashMap<(u32, u32), usize> = HashMap::new();
    for (i, f) in front.fns.iter().enumerate() {
        if f.body.is_some() {
            by_place.insert((f.at.file, f.at.place), i);
        }
    }
    let types = front.tys.view();
    let mut checked_here = 0usize;
    let check_started = Instant::now();
    let base_live = LIVE.load(Ordering::Relaxed);
    PEAK.store(base_live, Ordering::Relaxed);
    for round in 0..repeat {
        for b in &live {
            let Some(&i) = by_place.get(&(b.file as u32, b.start)) else {
                if round == 0 {
                    local.body(&[("front: a body the front keeps no row for".to_string(), format!("{}:{}", files[b.file].rel, line_of(&files[b.file].text, b.start as usize)))]);
                }
                continue;
            };
            let f = &front.fns[i];
            let Some(row) = f.body.as_ref() else { continue };
            let view = row.body.view(types);
            let inner = front.world(i);
            let world = Timed { inner: &inner, clock: Instant::now() };
            let mut cx = InferCtx::new_in(Alloc::global());
            let t = Instant::now();
            let result = blaze_typeck::check_logged(&view, &world, &mut cx, &Needs::new_in(Alloc::global()), &world, Alloc::global());
            let ns = t.elapsed().as_nanos() as u64;
            if round > 0 {
                continue;
            }
            checked_here += 1;
            let place = format!("{}:{} {}", files[b.file].rel, line_of(&files[b.file].text, b.start as usize), f.path);
            let snip = |r: blaze_typeck::Range| one_line(f.text.get(r.start as usize..r.end as usize).unwrap_or(""), 90);
            let mut causes: Vec<(String, String)> = Vec::new();
            let nodes = row.body.expr_ranges.len();
            match result {
                Err(e) => {
                    let label = match e {
                        CheckError::Unported(w) => format!("check: unported: {w}"),
                        CheckError::FutureWitness { .. } | CheckError::ForeignWitness { .. } | CheckError::FutureWitnessGoal { .. } => e.to_string(),
                        CheckError::DynUndecided { why, .. } => format!("check: dyn undecided {}", variant(&format!("{why:?}"))),
                        CheckError::UnknownInCleanBody { at } => format!("check: unknown in a clean body {}", variant(&format!("{at:?}"))),
                        CheckError::DiagWithoutPlace { code, .. } => format!("check: diagnostic without a place {}", code.code()),
                        CheckError::ImplTraitArity { .. } => "check: an impl header's trait arguments miscounted (Self first?)".to_string(),
                        CheckError::OpaqueHiddenUnknown { .. } | CheckError::OpaqueAutoUndecided { .. } => e.to_string(),
                        CheckError::TraitDefaultsNotKnown { .. } => "check: a qualified path's trait defaults not read".to_string(),
                        CheckError::Limit(l) => format!("check: limit {l:?}"),
                        CheckError::Malformed(m) if future_record_error(m) => e.to_string(),
                        CheckError::Malformed(m) => format!("check: malformed {}", variant(&format!("{m:?}"))),
                        CheckError::Solve(s) => format!("check: solver {}", variant(&format!("{s:?}"))),
                    };
                    causes.push((label.clone(), place.clone()));
                    costs.push(Cost { ns, floor_ns: 0, asks: 0, steps: 0, nodes, rec_bytes: 0, label });
                }
                Ok((outcome, log)) => {
                    record_restarts(&log, restart);
                    match outcome {
                        Outcome::Need(needs) => {
                            let q = needs.questions.last().map_or("none".to_string(), |q| variant(&format!("{q:?}")));
                            causes.push((format!("check: world missing {q}"), place.clone()));
                        }
                        Outcome::Done(done) => {
                            for d in done.diags.iter() {
                                let name = d.name.map(|n| format!("{n:?}")).unwrap_or_default();
                                causes.push((
                                    format!("diag {} {:?}", d.code.code(), d.code),
                                    format!("{place}: `{}` {}", snip(d.at), one_line(&name, 40)),
                                ));
                            }
                            let rec_bytes = record_bytes(&done);
                            costs.push(Cost { ns, floor_ns: floor_ns(&done), asks: log.asks.len() as u32, steps: log.end_step,
                            nodes,
                            rec_bytes,
                            label: place.clone(), });
                        }
                    }
                }
            }
            local.body(&causes);
        }
    }
    let check_ms = check_started.elapsed().as_secs_f64() * 1e3 / repeat as f64;
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base_live);
    // The same bodies on `threads` threads.
    let par_ms = {
        let todo: Vec<usize> = live.iter().filter_map(|b| by_place.get(&(b.file as u32, b.start)).copied()).collect();
        // The front's table is not `Sync`: each thread builds its own front (untimed), then all
        // start together at the barrier.
        let next = AtomicUsize::new(0);
        let gate = std::sync::Barrier::new(threads);
        let longest = AtomicU64::new(0);
        let (sources, root) = (&sources, &root);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut attrs = BlazeAttrs::default();
                    let own = Front::new(sources, root, &mut attrs, Alloc::global()).ok();
                    gate.wait();
                    let t = Instant::now();
                    if let Some(front) = own {
                        loop {
                            let k = next.fetch_add(1, Ordering::Relaxed);
                            let Some(&i) = todo.get(k) else { break };
                            let Some(row) = front.fns[i].body.as_ref() else { continue };
                            let view = row.body.view(front.tys.view());
                            let world = front.world(i);
                            let mut cx = InferCtx::new_in(Alloc::global());
                            let _ = blaze_typeck::check(&view, &world, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global());
                        }
                    }
                    longest.fetch_max(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                });
            }
        });
        longest.load(Ordering::Relaxed) as f64 / 1e6
    };
    println!(
        "  {name}: {} files, {total_bodies} bodies; elimination {rounds} rounds in {front_ms:.0} ms; {checked_here} checked in {check_ms:.1} ms on 1 thread, {par_ms:.1} ms on {threads}; check heap peak {:.1} MB",
        files.len(),
        peak as f64 / 1e6
    );
    local.checked += checked_here;
    merge(census, local);
}

fn future_record_error(m: Malformed) -> bool {
    matches!(m, Malformed::CoroutineFactsMissing { .. } | Malformed::FutureIdentity { .. } |
        Malformed::FutureOutsideOwner { .. } | Malformed::NestedFutureIdentity { .. } |
        Malformed::CoroutineSubstitution { .. } | Malformed::BodyRecordCycle { .. })
}

fn variant(debug: &str) -> String {
    debug.split(|c: char| c == '(' || c == ' ' || c == '{').next().unwrap_or(debug).to_string()
}

fn record_restarts(log: &AskLog<'_>, out: &mut Restart) {
    for (k, order) in [Order::Pull, Order::Kind, Order::KindWithGoals].into_iter().enumerate() {
        let r = restarts(log, order, Alloc::global());
        out.wasted[k] += r.wasted_steps;
        out.total[k] += r.total_steps;
        out.entries[k] += r.entries as u64;
    }
}

/// The floor of one check, as blaze-typeck's examples/cost.rs states it: every type the record
/// names interned once into a fresh table and the expression column copied. Best of three.
fn floor_ns(c: &blaze_typeck::Checked<'_>) -> u64 {
    let mut best = u64::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        let view = c.types.view();
        let mut out = blaze_typeck::PortableTys::new_in(Alloc::global());
        for &n in view.nodes() {
            let _ = out.intern(n);
        }
        let col: Vec<blaze_typeck::PTy> = c.expr_ty.iter().copied().collect();
        std::hint::black_box((col.len(), out.view().len()));
        best = best.min(t.elapsed().as_nanos() as u64);
    }
    best
}

fn print_floor(costs: &[Cost]) {
    let done: Vec<&Cost> = costs.iter().filter(|c| c.floor_ns > 0).collect();
    let check: u64 = done.iter().map(|c| c.ns).sum();
    let floor: u64 = done.iter().map(|c| c.floor_ns).sum();
    println!(
        "floor (write the record only, typeck's cost.rs): {:.1} ms over {} finished checks, {:.0} ns/body; the checks {:.1} ms: ratio {:.1}x",
        floor as f64 / 1e6,
        done.len(),
        floor as f64 / done.len().max(1) as f64,
        check as f64 / 1e6,
        check as f64 / floor.max(1) as f64
    );
}

/// The record's bytes as columns: each column's length times its element size.
fn record_bytes(done: &blaze_typeck::Checked<'_>) -> usize {
    let tys = done.types.view();
    tys.len() * std::mem::size_of::<blaze_typeck::PNode>()
        + tys.lists_len() * 4
        + done.expr_ty.len() * 4
        + done.pat_ty.len() * 4
        + done.local_ty.len() * 4
        + done.diags.len() * std::mem::size_of::<blaze_typeck::Diag>()
        + done.reads.questions().len() * (std::mem::size_of::<Question>() + 8)
}

fn merge(into: &mut Census, from: Census) {
    into.bodies += from.bodies;
    into.clean += from.clean;
    into.checked += from.checked;
    let want = samples_wanted();
    for (k, c) in from.causes {
        let t = into.causes.entry(k).or_default();
        t.bodies += c.bodies;
        t.alone += c.alone;
        t.items += c.items;
        for s in c.samples {
            if t.samples.len() < want && !s.is_empty() {
                t.samples.push(s);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The forms pass (`check --forms <library> <dir>...`): every body of every `.rs` file under
// each dir, lowered as stage h of `stages` lowers it (`lower_body_expanding`, std's and the
// dirs' `macro_rules!` by name; `to_typeck` with every written type left to inference), then
// checked in a world that knows no item: every path names nothing, no lang item, no impl.
// What it measures is the walk alone over real bodies: the forms the checker refuses, its
// errors and limits, and its cost per body where nothing outside the body is read.

struct HostCfg;

impl blaze_lower::Cfg for HostCfg {
    fn atom(&mut self, name: &str, value: Option<&str>) -> blaze_lower::CfgAnswer {
        let yes = match (name, value) {
            ("unix" | "debug_assertions", None) => true,
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
        if yes { blaze_lower::CfgAnswer::Yes } else { blaze_lower::CfgAnswer::No }
    }
}

struct Unresolved;

impl blaze_lower::typeck::Types for Unresolved {
    fn ty(
        &mut self,
        _: &blaze_lower::View<'_>,
        _: blaze_lower::TypeId,
        tys: &mut blaze_typeck::PortableTys<'_>,
    ) -> Result<blaze_typeck::PTy, blaze_lower::typeck::Refused> {
        tys.intern(blaze_typeck::PNode::Infer).map_err(blaze_lower::typeck::Refused::Check)
    }
    fn repeat_len(&mut self, _: &blaze_lower::View<'_>, _: blaze_lower::ExprId) -> Result<u64, blaze_lower::typeck::Refused> {
        Ok(0)
    }

    fn const_arg(&mut self, view: &blaze_lower::View<'_>, c: blaze_lower::ConstIdx, _: &mut blaze_typeck::PortableTys<'_>) -> Result<blaze_typeck::PTy, blaze_lower::typeck::Refused> {
        Err(blaze_lower::typeck::no_const_arg(view, c))
    }
}

struct BodyHost;

impl blaze_expand::ExpandCx for BodyHost {
    fn env(&self, _: &str) -> Result<Option<&str>, blaze_expand::HostError> {
        Err(blaze_expand::HostError { ask: blaze_expand::HostAsk::Env, why: blaze_expand::HostWhy::Refused })
    }
    fn cfg(&self, predicate: &blaze_expand::TokenStream<'_>) -> Result<bool, blaze_expand::HostError> {
        use blaze_lower::Cfg;
        let refused = blaze_expand::HostError { ask: blaze_expand::HostAsk::Cfg, why: blaze_expand::HostWhy::Refused };
        let mut atom = |name: &str, value: Option<&str>| match HostCfg.atom(name, value) {
            blaze_lower::CfgAnswer::Yes => Ok(true),
            blaze_lower::CfgAnswer::No => Ok(false),
            blaze_lower::CfgAnswer::Need => Err(refused),
        };
        blaze_expand::cfg_predicate(predicate, Alloc::global(), &mut atom).map_err(|_| refused)
    }
    fn include(&self, _: &str, _: blaze_expand::IncludeAs) -> Result<blaze_expand::Included<'_>, blaze_expand::HostError> {
        Err(blaze_expand::HostError { ask: blaze_expand::HostAsk::Include, why: blaze_expand::HostWhy::Refused })
    }
    fn place(&self, _: blaze_expand::PlaceOf) -> Result<blaze_expand::Place<'_>, blaze_expand::HostError> {
        Err(blaze_expand::HostError { ask: blaze_expand::HostAsk::Place, why: blaze_expand::HostWhy::Refused })
    }
    fn run_proc_macro<'a>(
        &self,
        _: &blaze_expand::MacroDefinition<'_>,
        _: blaze_expand::ProcCall<'_>,
        _: Alloc<'a>,
    ) -> Result<blaze_expand::Produced<'a>, blaze_expand::HostError> {
        Err(blaze_expand::HostError { ask: blaze_expand::HostAsk::ProcMacro, why: blaze_expand::HostWhy::Refused })
    }
}

fn body_def_at() -> blaze_expand::DefAt {
    blaze_expand::DefAt {
        site: blaze_expand::DefSite { krate: blaze_id::CrateItemKey(blaze_id::ContentId([1; 32])), module: None },
        edition: blaze_expand::Edition::E2024,
        export: blaze_expand::Export::Exported,
        local_inner: blaze_expand::LocalInner::No,
        transparency: None,
    }
}

/// Every `macro_rules!` and `macro` definition of `texts`, prepared, leaked for the run.
fn macro_rules_of(texts: &[&'static str]) -> &'static [(&'static str, blaze_expand::Prepared<'static>)] {
    use blaze_expand::{MacroDefinition, NestedKind, TokenStream};
    let mut defs = Vec::new();
    for text in texts {
        let Ok(stream) = blaze_expand::lex(text, blaze_expand::Edition::E2024, Alloc::global()) else { continue };
        let stream: &'static TokenStream<'static> = Box::leak(Box::new(stream));
        let Ok(found) = blaze_expand::find_calls(stream, Alloc::global()) else { continue };
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
            if let Ok(prepared) = blaze_expand::prepare(def) {
                defs.push((stream.text_at(name_at as usize), prepared));
            }
        }
    }
    Box::leak(defs.into_boxed_slice())
}

/// A world that knows no item.
struct Empty;

impl World for Empty {
    fn fn_sig(&self, _: Seal, _: ItemKey) -> Fetch<Option<FnSig<'_>>> {
        Ok(None)
    }
    fn predicates(&self, _: Seal, _: ItemKey) -> Fetch<Predicates<'_>> {
        Ok(Predicates { clauses: &[], types: blaze_typeck::PTys::EMPTY })
    }
    fn adt(&self, _: Seal, _: ItemKey) -> Fetch<Option<blaze_typeck::Adt<'_>>> {
        Ok(None)
    }
    fn impls_of(&self, _: Seal, _: ItemKey, _: Shape) -> Fetch<&[ItemKey]> {
        Ok(&[])
    }
    fn impl_header(&self, _: Seal, _: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        Ok(None)
    }
    fn inherent(&self, _: Seal, _: Shape, _: Name) -> Fetch<&[AssocItem]> {
        Ok(&[])
    }
    fn assoc_item(&self, _: Seal, _: ItemKey, _: Name) -> Fetch<Option<AssocItem>> {
        Ok(None)
    }
    fn traits_in_scope(&self, _: Seal, _: ScopeId) -> Fetch<&[ItemKey]> {
        Ok(&[])
    }
    /// A world with no names: every path's first segment names nothing.
    fn resolve_path(&self, _: Seal, _: ScopeId, _: PathIdx, _: &[Name]) -> Fetch<blaze_typeck::PathAnswer> {
        Ok(Err(blaze_typeck::PathMiss::FIRST))
    }
    fn lang_item(&self, _: Seal, _: LangItem) -> Fetch<Option<ItemKey>> {
        Ok(None)
    }
    fn lang_path(&self, _: Seal, _: LangItem) -> Fetch<Option<Def>> {
        Ok(None)
    }
    fn const_value(&self, _: Seal, _: ItemKey) -> Fetch<Option<u64>> {
        Ok(None)
    }
    fn opaque_hidden(&self, _: Seal, _: ItemKey) -> Fetch<Option<Hidden<'_>>> {
        Ok(None)
    }
    fn closure(&self, _: Seal, _: ItemKey, _: u32) -> Fetch<Option<blaze_typeck::ClosureFacts<'_>>> {
        Ok(None)
    }    fn attrs(&self, _: Seal, _: ItemKey) -> Fetch<TypeAttrs> {
        Ok(TypeAttrs::default())
    }
    fn trait_def(&self, _: Seal, _: ItemKey) -> Fetch<Option<TraitDef<'_>>> {
        Ok(None)
    }
    fn assoc_parent(&self, _: Seal, _: ItemKey) -> Fetch<Option<blaze_typeck::AssocParent>> {
        Ok(None)
    }
    /// The empty world declares no condition: one asked about is an evaluation error.
    fn const_holds(
        &self,
        _: Seal,
        _: ItemKey,
        _: &[blaze_typeck::PTy],
        _: blaze_typeck::PTys<'_>,
    ) -> Fetch<blaze_typeck::ConstHolds> {
        Ok(blaze_typeck::ConstHolds::Error)
    }
    /// The empty world knows no item, so no const parameter.
    fn const_params(&self, _: Seal, _: ItemKey) -> Fetch<blaze_typeck::ConstParams<'_>> {
        Ok(blaze_typeck::ConstParams { params: &[], types: blaze_typeck::PTys::EMPTY })
    }
}

struct FormsOut {
    census: Census,
    costs: Vec<Cost>,
    check_ns: u64,
}

/// One file's bodies through the forms pass.
fn forms_file(rel: &str, text: &str, rules: &'static [(&'static str, blaze_expand::Prepared<'static>)], out: &mut FormsOut) {
    use blaze_lower::typeck::{Refused, to_typeck};
    let alloc = Alloc::global();
    let made = blaze_file_items::outline(text, EDITION, alloc);
    for item in made.items() {
        let has_body = matches!(item.detail, ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. });
        if !has_body || item.body.is_none() {
            continue;
        }
        let base = item.range.start as usize;
        let Some(item_text) = text.get(base..item.range.end as usize) else { continue };
        let place = format!("{rel}:{}", line_of(text, base));
        let mut by_name = blaze_lower::by_name::ByName::new(rules, body_def_at(), BodyHost, alloc);
        let Ok(lowered) = blaze_lower::lower_body_expanding(item_text, EDITION, &mut HostCfg, &mut blaze_lower::NoModule, &mut by_name, alloc) else {
            out.census.body(&[("lower: a cfg atom left unanswered".to_string(), place)]);
            continue;
        };
        let view = lowered.view();
        if view.cfg_out() {
            continue;
        }
        let offset_of = |piece: &str| (piece.as_ptr() as usize).checked_sub(item_text.as_ptr() as usize).filter(|o| *o <= item_text.len()).unwrap_or(0);
        let at = |o: usize| format!("{rel}:{}", line_of(text, base + o));
        let mut causes: Vec<(String, String)> = Vec::new();
        for u in view.unhandled() {
            let piece = view.text_at(u.range).unwrap_or("");
            let kind = format!("{:?}", blaze_file_items::syntax_kind(u.syntax));
            let cause = if kind.starts_with("MACRO_CALL") {
                "lower: a macro call, not expanded".to_string()
            } else {
                format!("lower: unhandled {kind} ({:?})", u.why)
            };
            causes.push((cause, format!("{}: {}", at(offset_of(piece)), one_line(piece, 80))));
        }
        for d in view.diags().flatten() {
            let piece = view.text_at(d.range).unwrap_or("");
            causes.push((format!("lower: {:?}", d.kind), format!("{}: {}", at(offset_of(piece)), one_line(piece, 80))));
        }
        let mut tys = blaze_typeck::PortableTys::new_in(alloc);
        let body = match to_typeck(&view, ItemKey(0), &mut tys, &mut Unresolved, alloc) {
            Ok(b) => Some(b),
            Err(Refused::NoBody) => continue,
            Err(Refused::Form { what, range }) => {
                let piece = item_text.get(range.start as usize..range.end as usize).unwrap_or("");
                causes.push((format!("to_typeck: {what}"), format!("{}: {}", at(range.start as usize), one_line(piece, 80))));
                None
            }
            Err(e) => {
                causes.push((format!("to_typeck: {}", variant(&format!("{e:?}"))), place.clone()));
                None
            }
        };
        if let Some(body) = body.filter(|_| causes.is_empty()) {
            let bv = body.view(tys.view());
            let mut cx = InferCtx::new_in(alloc);
            let world = Timed { inner: &Empty, clock: Instant::now() };
            let t = Instant::now();
            let result = blaze_typeck::check_logged(&bv, &world, &mut cx, &Needs::new_in(alloc), &world, alloc);
            let ns = t.elapsed().as_nanos() as u64;
            out.check_ns += ns;
            out.census.checked += 1;
            let nodes = body.expr_ranges.len();
            let snip = |r: blaze_typeck::Range| one_line(item_text.get(r.start as usize..r.end as usize).unwrap_or(""), 90);
            match result {
                Err(e) => {
                    let label = match e {
                        CheckError::Unported(w) => format!("check: unported: {w}"),
                        CheckError::FutureWitness { .. } | CheckError::ForeignWitness { .. } | CheckError::FutureWitnessGoal { .. } => e.to_string(),
                        CheckError::DynUndecided { why, .. } => format!("check: dyn undecided {}", variant(&format!("{why:?}"))),
                        CheckError::UnknownInCleanBody { at } => format!("check: unknown in a clean body {}", variant(&format!("{at:?}"))),
                        CheckError::DiagWithoutPlace { code, .. } => format!("check: diagnostic without a place {}", code.code()),
                        CheckError::ImplTraitArity { .. } => "check: an impl header's trait arguments miscounted (Self first?)".to_string(),
                        CheckError::OpaqueHiddenUnknown { .. } | CheckError::OpaqueAutoUndecided { .. } => e.to_string(),
                        CheckError::TraitDefaultsNotKnown { .. } => "check: a qualified path's trait defaults not read".to_string(),
                        CheckError::Limit(l) => format!("check: limit {l:?}"),
                        CheckError::Malformed(m) if future_record_error(m) => e.to_string(),
                        CheckError::Malformed(m) => format!("check: malformed {}", variant(&format!("{m:?}"))),
                        CheckError::Solve(s) => format!("check: solver {}", variant(&format!("{s:?}"))),
                    };
                    causes.push((label, place.clone()));
                    out.costs.push(Cost { ns, floor_ns: 0, asks: 0, steps: 0, nodes, rec_bytes: 0, label: place.clone() });
                }
                Ok((Outcome::Need(_), _)) => causes.push(("check: world missing (none expected)".to_string(), place.clone())),
                Ok((Outcome::Done(done), log)) => {
                    for d in done.diags.iter() {
                        // With no item known, an unresolved path or a missing lang item is the
                        // world's, by construction; every other code is the walk's to explain.
                        let kind = match d.code {
                            blaze_typeck::DiagCode::UnresolvedPath | blaze_typeck::DiagCode::MissingLangItem => "no world",
                            _ => "walk",
                        };
                        causes.push((format!("diag ({kind}) {} {:?}", d.code.code(), d.code), format!("{place}: `{}`", snip(d.at))));
                    }
                    out.costs.push(Cost { ns, floor_ns: floor_ns(&done), asks: log.asks.len() as u32, steps: log.end_step,
                    nodes,
                    rec_bytes: record_bytes(&done),
                    label: place.clone(), });
                }
            }
        }
        out.census.body(&causes);
    }
}

fn forms(args: &[String]) {
    let Some(library) = args.first().map(PathBuf::from) else {
        eprintln!("usage: check --forms <rust-src library> <dir>...");
        std::process::exit(2);
    };
    let threads = std::env::var("CHECK_THREADS").ok().and_then(|s| s.parse().ok()).unwrap_or(18);
    let mut files: Vec<(String, &'static str)> = Vec::new();
    for dir in args.iter().skip(1) {
        let mut paths = Vec::new();
        rs_under(Path::new(dir), &mut paths);
        for p in paths {
            if let Ok(t) = std::fs::read_to_string(&p) {
                files.push((p.display().to_string(), Box::leak(t.into_boxed_str())));
            }
        }
    }
    let mut texts: Vec<&'static str> = Vec::new();
    for krate in ["core", "alloc", "std"] {
        let mut paths = Vec::new();
        rs_under(&library.join(krate).join("src"), &mut paths);
        for p in paths {
            if let Ok(t) = std::fs::read_to_string(&p)
                && t.contains("macro")
            {
                texts.push(Box::leak(t.into_boxed_str()));
            }
        }
    }
    for (_, t) in &files {
        if t.contains("macro_rules!") || t.contains("macro ") {
            texts.push(t);
        }
    }
    let rules = macro_rules_of(&texts);
    println!("forms: {} files under {} dirs; {} macro definitions", files.len(), args.len() - 1, rules.len());
    // On `threads` threads first: lowering and checking, files shared out; the rules are
    // prepared per thread (a stream's handle is not shared). Its world tally is then cleared.
    let next = AtomicUsize::new(0);
    let check_all = AtomicU64::new(0);
    let t = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let rules = macro_rules_of(&texts);
                let mut mine = FormsOut { census: Census::default(), costs: Vec::new(), check_ns: 0 };
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    let Some((rel, text)) = files.get(k) else { break };
                    forms_file(rel, text, rules, &mut mine);
                }
                check_all.fetch_add(mine.check_ns, Ordering::Relaxed);
            });
        }
    });
    let par = t.elapsed().as_secs_f64();
    for k in 0..KINDS {
        WORLD.calls[k].store(0, Ordering::Relaxed);
        WORLD.ns[k].store(0, Ordering::Relaxed);
    }
    let base_live = LIVE.load(Ordering::Relaxed);
    PEAK.store(base_live, Ordering::Relaxed);
    let t = Instant::now();
    let mut out = FormsOut { census: Census::default(), costs: Vec::new(), check_ns: 0 };
    for (rel, text) in &files {
        forms_file(rel, text, rules, &mut out);
    }
    let one = t.elapsed().as_secs_f64();
    // More rounds, for a sampling profiler to attach to; their results are dropped.
    let repeat: usize = std::env::var("CHECK_REPEAT").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    for _ in 1..repeat {
        let mut spare = FormsOut { census: Census::default(), costs: Vec::new(), check_ns: 0 };
        for (rel, text) in &files {
            forms_file(rel, text, rules, &mut spare);
        }
    }
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base_live);
    let c = &out.census;
    let pct = |n: usize| if c.bodies == 0 { 0.0 } else { 100.0 * n as f64 / c.bodies as f64 };
    println!(
        "bodies: {}; reached the checker: {} ({:.1}%); clean in a world that knows no item: {} ({:.1}%)",
        c.bodies,
        c.checked,
        pct(c.checked),
        c.clean,
        pct(c.clean)
    );
    println!(
        "time: lower+check {:.1} ms on 1 thread (check alone {:.1} ms: {:.0} bodies/s), {:.1} ms on {threads} threads (rules prepared per thread included); heap peak of the 1-thread pass {:.1} MB",
        one * 1e3,
        out.check_ns as f64 / 1e6,
        c.checked as f64 / (out.check_ns as f64 / 1e9),
        par * 1e3,
        peak as f64 / 1e6
    );
    report(&out.census, &out.costs);
}

fn report(census: &Census, costs: &[Cost]) {
    let mut rows: Vec<(&String, &Cause)> = census.causes.iter().collect();
    rows.sort_by(|a, b| b.1.alone.cmp(&a.1.alone).then(b.1.bodies.cmp(&a.1.bodies)));
    println!("| alone | bodies | items | cause | first sample |");
    println!("|---|---|---|---|---|");
    for (k, c) in &rows {
        println!("| {} | {} | {} | {} | {} |", c.alone, c.bodies, c.items, k, c.samples.first().map_or("", |s| s.as_str()));
    }
    println!("samples by cause:");
    for (k, c) in &rows {
        if c.samples.len() > 1 {
            println!("  {k}:");
            for s in c.samples.iter().skip(1) {
                println!("    {s}");
            }
        }
    }
    let total_ns: u64 = costs.iter().map(|c| c.ns).sum();
    let nodes: usize = costs.iter().map(|c| c.nodes).sum();
    let done: Vec<&Cost> = costs.iter().filter(|c| c.steps > 0).collect();
    let rec: usize = done.iter().map(|c| c.rec_bytes).sum();
    let asks: u64 = done.iter().map(|c| c.asks as u64).sum();
    if !costs.is_empty() {
        let mut ns: Vec<u64> = costs.iter().map(|c| c.ns).collect();
        ns.sort_unstable();
        println!(
            "cost: {} checks, {:.1} ms: {:.0} bodies/s on 1 thread; {:.0} ns/body mean, p50 {} ns, p99 {} ns; {:.1} ns/expression ({} expressions); {:.1} asks/body; record ~{:.0} B/body",
            costs.len(),
            total_ns as f64 / 1e6,
            costs.len() as f64 / (total_ns as f64 / 1e9),
            total_ns as f64 / costs.len() as f64,
            ns[ns.len() / 2],
            ns[ns.len() * 99 / 100],
            total_ns as f64 / nodes.max(1) as f64,
            nodes,
            asks as f64 / done.len().max(1) as f64,
            rec as f64 / done.len().max(1) as f64
        );
    }
    print_floor(costs);
    let world_ns: u64 = WORLD.ns.iter().map(|n| n.load(Ordering::Relaxed)).sum();
    println!("world answers: {:.1} ms", world_ns as f64 / 1e6);
    for k in 0..KINDS {
        let calls = WORLD.calls[k].load(Ordering::Relaxed);
        if calls > 0 {
            println!("  {:<14} {:>9} calls {:>9.2} ms", KIND_NAMES[k], calls, WORLD.ns[k].load(Ordering::Relaxed) as f64 / 1e6);
        }
    }
    let mut slow: Vec<&Cost> = costs.iter().collect();
    slow.sort_by(|a, b| b.ns.cmp(&a.ns));
    println!("slowest 100 bodies (ns, expressions, asks, steps, record bytes, where):");
    for c in slow.iter().take(100) {
        println!("  {:>10} {:>6} {:>6} {:>7} {:>8}  {}", c.ns, c.nodes, c.asks, c.steps, c.rec_bytes, c.label);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--forms") {
        forms(&args[1..]);
        return;
    }
    let dirs: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();
    if dirs.is_empty() {
        eprintln!("usage: check <crate dir>... | check --forms <rust-src library> <dir>...");
        std::process::exit(2);
    }
    let threads = std::env::var("CHECK_THREADS").ok().and_then(|s| s.parse().ok()).unwrap_or(18);
    let repeat = std::env::var("CHECK_REPEAT").ok().and_then(|s| s.parse().ok()).unwrap_or(1).max(1);
    let mut census = Census::default();
    let mut costs = Vec::new();
    let mut restart = Restart::default();
    println!("check: {} crates through the ladder front and blaze-typeck", dirs.len());
    for dir in &dirs {
        run_crate(dir, &mut census, &mut costs, &mut restart, threads, repeat);
    }
    let pct = |n: usize| if census.bodies == 0 { 0.0 } else { 100.0 * n as f64 / census.bodies as f64 };
    println!(
        "bodies: {}; reached the checker: {} ({:.1}%); clean: {} ({:.1}%)",
        census.bodies,
        census.checked,
        pct(census.checked),
        census.clean,
        pct(census.clean)
    );
    let mut rows: Vec<(&String, &Cause)> = census.causes.iter().collect();
    rows.sort_by(|a, b| b.1.alone.cmp(&a.1.alone).then(b.1.bodies.cmp(&a.1.bodies)));
    println!("causes (bodies: under each of its causes; alone: its only cause; items: front refusals):");
    println!("| alone | bodies | items | cause | first sample |");
    println!("|---|---|---|---|---|");
    for (k, c) in &rows {
        println!("| {} | {} | {} | {} | {} |", c.alone, c.bodies, c.items, k, c.samples.first().map_or("", |s| s.as_str()));
    }
    println!("samples by cause:");
    for (k, c) in &rows {
        if c.samples.len() > 1 {
            println!("  {k}:");
            for s in c.samples.iter().skip(1) {
                println!("    {s}");
            }
        }
    }
    // Cost.
    let done: Vec<&Cost> = costs.iter().filter(|c| c.asks > 0 || c.steps > 0).collect();
    let total_ns: u64 = costs.iter().map(|c| c.ns).sum();
    let nodes: usize = costs.iter().map(|c| c.nodes).sum();
    let rec: usize = done.iter().map(|c| c.rec_bytes).sum();
    let asks: u64 = done.iter().map(|c| c.asks as u64).sum();
    if !costs.is_empty() {
        println!(
            "cost: {} bodies checked in {:.1} ms on 1 thread: {:.0} bodies/s, {:.0} ns/body, {:.1} ns/expression ({} expressions); {:.1} asks/body; record ~{:.0} B/body",
            costs.len(),
            total_ns as f64 / 1e6,
            costs.len() as f64 / (total_ns as f64 / 1e9),
            total_ns as f64 / costs.len() as f64,
            total_ns as f64 / nodes.max(1) as f64,
            nodes,
            asks as f64 / done.len().max(1) as f64,
            rec as f64 / done.len().max(1) as f64
        );
    }
    print_floor(&costs);
    let world_ns: u64 = WORLD.ns.iter().map(|n| n.load(Ordering::Relaxed)).sum();
    println!("world answers: {:.1} ms of the checks' time", world_ns as f64 / 1e6);
    for k in 0..KINDS {
        let calls = WORLD.calls[k].load(Ordering::Relaxed);
        if calls > 0 {
            println!("  {:<14} {:>9} calls {:>9.2} ms", KIND_NAMES[k], calls, WORLD.ns[k].load(Ordering::Relaxed) as f64 / 1e6);
        }
    }
    for (k, name) in ["Pull", "Kind", "KindWithGoals"].iter().enumerate() {
        println!(
            "restarts {name}: R = {:.3} ({} wasted of {} steps), {} entries",
            restart.wasted[k] as f64 / restart.total[k].max(1) as f64,
            restart.wasted[k],
            restart.total[k],
            restart.entries[k]
        );
    }
    let mut slow: Vec<&Cost> = costs.iter().collect();
    slow.sort_by(|a, b| b.ns.cmp(&a.ns));
    println!("slowest 100 bodies (ns, expressions, asks, steps, record bytes, where):");
    for c in slow.iter().take(100) {
        println!("  {:>10} {:>6} {:>6} {:>7} {:>8}  {}", c.ns, c.nodes, c.asks, c.steps, c.rec_bytes, c.label);
    }
}
