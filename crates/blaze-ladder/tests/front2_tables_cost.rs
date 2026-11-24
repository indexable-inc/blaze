//! THE COST OF ONE BODY ON THE NEW PATH (rule 16; engine.md "Floors": about 1 ms of CPU a
//! body). The real 78-crate fixture (std, alloc, core, bstd, serde, ..: `Inputs` over the
//! toolchain) is solved and written as records; every crate's impl tables are written (lane
//! BK); then every body of the program's crate is checked, single-threaded:
//!
//! 1. TODAY'S PATH: one `ProjectWorld::from_records` for all bodies (`check_borrows`), its
//!    store, index and impl buckets shared from body to body;
//! 2. THE TASK: `tables::task::check_body` over the read-only view with the impl tables, a
//!    fresh world per body (engine.md E5), measured by part with the meter (tables/cost.rs).
//!
//! It prints, per body and in sum, the microseconds and heap bytes of each part, the heads and
//! impl questions counted, the solver's frames, and how many of the task's bodies read a whole
//! section (`NameCell::Whole`: every edit to that section dirties the body). `ANSWERS` is a
//! digest of every task's bytes and read list: a change that removes cost must leave it equal.
//! Lines are also written to `front2_tables_cost.out` in the temp directory.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use blaze_drive::key::{ABSENT, Key, Kind, Name};
use blaze_engine::{CellId, Made, Phase, Planned, SVec, Stage, Task};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::buckets::{TableSet, write_impl_tables};
use blaze_ladder::front2::heads::NeedsMet;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::front2::tables::cost::{Cost, Gauge, PARTS, Spent};
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts, TableRead};
use blaze_ladder::front2::tables::head::{ConstValues, SeededHeads, SeededHeadsOpts, StoreConsts};
use blaze_lower::scoped::Prepares;
use blaze_ladder::front2::tables::seeds::{Freezing, FrozenTable, TableView};
use blaze_ladder::front2::tables::task::{BodyFrame, Make, MakeRefused, check_body};
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_own::Alloc;
use blaze_scope::names_cells::NameCell;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};

/// The heap's bytes allocated so far: the meter's byte counter.
static BYTES: AtomicU64 = AtomicU64::new(0);

struct Counting;

// SAFETY: every call is forwarded to `System` unchanged; only a counter is added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        BYTES.fetch_add(new_size.saturating_sub(layout.size()) as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static HEAP: Counting = Counting;

struct Clock(Instant);

impl Gauge for Clock {
    fn now_ns(&self) -> u64 {
        self.0.elapsed().as_nanos() as u64
    }

    fn bytes(&self) -> u64 {
        BYTES.load(Ordering::Relaxed)
    }
}

const MAIN: &str = "#![no_std]
extern crate alloc;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;
use core::ops::Add;

#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub struct P {
    x: i32,
    y: i32,
}

impl Add for P {
    type Output = P;
    fn add(self, o: P) -> P {
        P { x: self.x + o.x, y: self.y + o.y }
    }
}

impl fmt::Display for P {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, \"({}, {})\", self.x, self.y)
    }
}

pub trait Shape {
    fn area(&self) -> f64;
    fn name(&self) -> &'static str {
        \"shape\"
    }
}

pub struct Sq(pub f64);
pub struct Circle {
    pub r: f64,
}

impl Shape for Sq {
    fn area(&self) -> f64 {
        self.0 * self.0
    }
}

impl Shape for Circle {
    fn area(&self) -> f64 {
        3.14 * self.r * self.r
    }
    fn name(&self) -> &'static str {
        \"circle\"
    }
}

#[derive(Debug)]
pub enum E {
    Empty,
    Bad(u32),
}

pub struct Stack<T> {
    items: Vec<T>,
}

impl<T: Clone> Stack<T> {
    pub fn new() -> Self {
        Stack { items: Vec::new() }
    }
    pub fn push(&mut self, t: T) {
        self.items.push(t);
    }
    pub fn pop(&mut self) -> Option<T> {
        self.items.pop()
    }
    pub fn peek(&self) -> Option<&T> {
        self.items.last()
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

pub fn sum(xs: &[u32]) -> u32 {
    let mut t = 0;
    for x in xs {
        t += *x;
    }
    t
}

pub fn iters(xs: &[u32]) -> usize {
    xs.iter().map(|x| x * 2).filter(|x| *x > 3).count()
}

pub fn opts(a: Option<u8>) -> u8 {
    a.map(|v| v + 1).unwrap_or(0)
}

pub fn strs(a: &str, b: &str) -> bool {
    a == b && a.len() < 10
}

pub fn points(a: P, b: P) -> P {
    let c = a + b;
    if c == P::default() { a.clone() } else { c }
}

pub fn res(r: Result<u32, ()>) -> u32 {
    match r {
        Ok(v) => v.wrapping_add(1),
        Err(()) => 0,
    }
}

pub fn chars(s: &str) -> usize {
    s.chars().rev().take(3).count()
}

pub fn floats(a: f64) -> f64 {
    (a * 2.0).max(1.0)
}

pub fn ranges(n: usize) -> usize {
    (0..n).map(|i| i % 3).sum()
}

pub fn slices(xs: &mut [i64]) -> Option<i64> {
    xs.sort_unstable();
    xs.first().copied()
}

pub fn vecs(n: u32) -> Vec<u32> {
    let mut v = Vec::with_capacity(n as usize);
    for i in 0..n {
        v.push(i * i);
    }
    v.retain(|x| x % 2 == 0);
    v
}

pub fn collect_sorted(xs: &[i32]) -> Vec<i32> {
    let mut v: Vec<i32> = xs.iter().copied().filter(|x| *x >= 0).collect();
    v.sort();
    v.dedup();
    v
}

pub fn words(s: &str) -> Vec<String> {
    s.split(' ').filter(|w| !w.is_empty()).map(|w| w.to_string()).collect()
}

pub fn joined(xs: &[&str]) -> String {
    let mut out = String::new();
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(x);
    }
    out
}

pub fn counts(xs: &[u8]) -> BTreeMap<u8, usize> {
    let mut m = BTreeMap::new();
    for x in xs {
        *m.entry(*x).or_insert(0) += 1;
    }
    m
}

pub fn lookup(m: &BTreeMap<String, u32>, k: &str) -> u32 {
    m.get(k).copied().unwrap_or_default()
}

pub fn uniq(xs: &[u32]) -> usize {
    let s: BTreeSet<u32> = xs.iter().copied().collect();
    s.len()
}

pub fn queue(n: usize) -> Option<usize> {
    let mut q = VecDeque::new();
    for i in 0..n {
        q.push_back(i);
    }
    q.pop_front();
    q.back().copied()
}

pub fn parse_all(xs: &[&str]) -> Result<Vec<u32>, core::num::ParseIntError> {
    let mut out = Vec::new();
    for x in xs {
        out.push(x.parse::<u32>()?);
    }
    Ok(out)
}

pub fn first_bad(xs: &[u32]) -> Result<u32, E> {
    let first = xs.first().ok_or(E::Empty)?;
    if *first > 10 {
        return Err(E::Bad(*first));
    }
    Ok(*first)
}

pub fn chain(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    let x = a?;
    let y = b?;
    x.checked_add(y)
}

pub fn shapes() -> Vec<Box<dyn Shape>> {
    let mut v: Vec<Box<dyn Shape>> = Vec::new();
    v.push(Box::new(Sq(2.0)));
    v.push(Box::new(Circle { r: 1.0 }));
    v
}

pub fn total_area(v: &[Box<dyn Shape>]) -> f64 {
    v.iter().map(|s| s.area()).sum()
}

pub fn names(v: &[Box<dyn Shape>]) -> Vec<&'static str> {
    v.iter().map(|s| s.name()).collect()
}

pub fn largest<T: PartialOrd + Copy>(xs: &[T]) -> Option<T> {
    let mut it = xs.iter();
    let mut best = *it.next()?;
    for x in it {
        if *x > best {
            best = *x;
        }
    }
    Some(best)
}

pub fn apply<F: Fn(u32) -> u32>(f: F, x: u32) -> u32 {
    f(f(x))
}

pub fn adder(n: u32) -> impl Fn(u32) -> u32 {
    move |x| x + n
}

pub fn boxed_fn(n: i32) -> Box<dyn Fn(i32) -> i32> {
    Box::new(move |x| x * n)
}

pub fn counter() -> u32 {
    let mut c = 0;
    let mut inc = || c += 1;
    inc();
    inc();
    c
}

pub fn zipped(a: &[u32], b: &[u32]) -> u32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

pub fn windows(xs: &[i32]) -> usize {
    xs.windows(2).filter(|w| w[0] < w[1]).count()
}

pub fn folded(xs: &[u64]) -> u64 {
    xs.iter().fold(0, |acc, x| acc ^ x)
}

pub fn positions(s: &str) -> Option<usize> {
    s.bytes().position(|b| b == b'=')
}

pub fn shown(p: P) -> String {
    alloc::format!(\"{} {:?}\", p, p)
}

pub fn stacked() -> Option<u32> {
    let mut s = Stack::new();
    s.push(1u32);
    s.push(2);
    let top = s.pop()?;
    Some(top + *s.peek()? + s.len() as u32)
}

pub fn matched(e: &E) -> u32 {
    match e {
        E::Empty => 0,
        E::Bad(n) if *n > 3 => *n,
        E::Bad(_) => 1,
    }
}

pub fn nested(v: &Vec<Vec<u8>>) -> usize {
    v.iter().flat_map(|row| row.iter()).filter(|b| **b != 0).count()
}

pub fn maxed(xs: &[(u32, &str)]) -> Option<&str> {
    xs.iter().max_by_key(|(n, _)| *n).map(|(_, s)| *s)
}

pub fn split_sum(s: &str) -> u32 {
    s.split(',').filter_map(|p| p.trim().parse::<u32>().ok()).sum()
}

pub fn rev_string(s: &str) -> String {
    s.chars().rev().collect()
}

fn main() {}
";

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve (tests/front2_tables_core.rs `Kept`).
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<ItemKey, &'static str>>,
    origins: Vec<HashMap<ItemKey, &'static [u8]>>,
}

fn made_origins(call: ItemKey, origins: &Result<&[u8], blaze_scope::expansion_origins::OriginsError>) -> &'static [u8] {
    use blaze_scope::expansion_origins::OriginsError;
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("the origins of expansion {call:x}: {e:?}"),
    }
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        // The fixture keeps the origins in the joined form: its two parts, cut where the column
        // starts (empty: the text keeps none). None kept for the call: none served.
        let origins = self.origins.get(krate).and_then(|by_call| by_call.get(&call)).copied().map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}"))
            }
        });
        Some(LoadedExpansion { text, origins })
    }
}

fn task_of(key: ItemKey) -> Task {
    let planned = Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) };
    Task { planned, key: Key(ABSENT) }
}

/// Every cell's digest from its name (tests/front2_tables_task.rs `MapMake`).
struct NameMake;

impl<'a> Make<'a> for NameMake {
    fn make(&self, want: &CellId, _made: &mut SVec<'a, Made<'a>>) -> Result<ContentId, MakeRefused> {
        Ok(ContentId::of("blaze-ladder tables cost test cell", format!("{want:?}").as_bytes()))
    }
}

fn frame() -> &'static blaze_mem::Frame {
    Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("a frame")))
}

/// One line of parts: µs and KB each, heads and impl questions counted.
fn parts(s: &Spent, n: u64) -> String {
    let n = n.max(1);
    let mut out = String::new();
    for p in PARTS {
        let i = p as usize;
        out.push_str(&format!(" {} {}us/{}KB", p.name(), s.ns[i] / 1000 / n, s.bytes[i] / 1024 / n));
        if matches!(p, blaze_ladder::front2::tables::cost::Part::Heads | blaze_ladder::front2::tables::cost::Part::Impls) {
            out.push_str(&format!("(x{})", s.entries[i] / n));
        }
    }
    out.push_str(&format!(" | head steps {} stopped at a need {} ({} us)", s.steps / n, s.stopped / n, s.stopped_ns / 1000 / n));
    out.push_str(&format!(" | rounds {}.{:02} solver frames {} candidates {} cache hits {}", s.rounds / n, s.rounds * 100 / n % 100, s.solve.stats.frames / n, s.solve.stats.candidates / n, s.solve.stats.cache_hits / n));
    // Each round more, by the kind of its first hole (all n bodies, not per body); then every
    // hole of those rounds by kind.
    let kinds = |counts: &[u64]| -> String {
        let mut by: Vec<(u64, &str)> = counts.iter().zip(blaze_typeck::QUESTION_KINDS).filter(|(c, _)| **c > 0).map(|(c, k)| (*c, k)).collect();
        by.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        by.iter().map(|(c, k)| format!("{k} {c}")).collect::<Vec<_>>().join(", ")
    };
    out.push_str(&format!(" | rounds more for (first hole): [{}] holes: [{}]", kinds(&s.again_for), kinds(&s.holes)));
    out
}

/// Run on the test's own thread and its default stack: `SeededHeads` freezes a cell's needs by
/// an explicit, capped stack of wants (head.rs `MOST_MAKING`), not by recursion, so core's
/// head chains no longer need the 1 GB thread this ran on before.
#[test]
fn the_cost_of_one_body_by_part() {
    cost_of_one_body();
}

fn cost_of_one_body() {
    let mut said = String::new();
    macro_rules! say {
        ($($t:tt)*) => {{
            let line = format!($($t)*);
            println!("{line}");
            said.push_str(&line);
            said.push('\n');
            // Kept as it is said: a crash still leaves the stages reached.
            let _ = std::fs::write(std::env::temp_dir().join("front2_tables_cost.out"), &said);
        }};
    }
    // A line for the file only (the queue cuts a long output before a failure's text).
    macro_rules! note {
        ($($t:tt)*) => {{
            said.push_str(&format!($($t)*));
            said.push('\n');
        }};
    }
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/cost/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "cost", dir: "/cost/", root: "/cost/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let core = crate_names.iter().position(|n| *n == "core").expect("a crate named core");
    let lang: &'static [usize] = Vec::leak(vec![0, core]);
    let g = Alloc::global();

    let start = Instant::now();
    let kept = {
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new(), origins: Vec::new() };
        for c in names.crates().iter() {
            kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.items.push(aligned(&items_record::encode_with(c, &names, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
            kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
            kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
        }
        kept
    };
    let load: &'static Kept = Box::leak(Box::new(kept));
    let reader = || -> &'static Records<'static> {
        let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, g)));
        Box::leak(Box::new(Records::new(keys, load, log, frame(), g)))
    };
    say!("SOLVED {} crates in {} ms", crate_names.len(), start.elapsed().as_millis());

    // Every crate's impl tables.
    let start = Instant::now();
    let records = reader();
    let mut tables = TableSet::new(g);
    for krate in 0..crate_names.len() {
        let mut rows = Vec::new();
        NamesSource::each_impl_row(records, krate, &mut |r| rows.push(r));
        tables.put(write_impl_tables(records, krate, &rows, g).unwrap_or_else(|e| panic!("`{}`'s impl tables: {e}", crate_names[krate])));
    }
    let tables: &'static TableSet<'static> = Box::leak(Box::new(tables));
    say!("IMPL TABLES of {} crates in {} ms", crate_names.len(), start.elapsed().as_millis());

    // The program's bodies by item key (a member by its own key), in key order.
    let listing = ProjectWorld::from_records(records, frame(), lang, g).unwrap_or_else(|e| panic!("the world did not start: {e}"));
    let mut bodies: Vec<(ItemKey, BodyRef<'static>)> = Vec::new();
    for (body, _) in listing.bodies(g).iter() {
        let key = match *body {
            BodyRef::Free(k) => k,
            BodyRef::Member { container, name, .. } => {
                let mut found = None;
                NamesSource::each_member(records, container, &mut |m| {
                    if m.name == Some(name) {
                        found = Some(m.key);
                    }
                });
                found.unwrap_or_else(|| panic!("the member `{name}` of {container:x} has no key"))
            }
        };
        bodies.push((key, *body));
    }
    drop(listing);
    bodies.sort_by_key(|(k, _)| *k);
    assert!(bodies.len() >= 40, "the program's bodies: {}", bodies.len());

    let clock = Clock(Instant::now());
    let cost: &'static Cost<'static> = Box::leak(Box::new(Cost::new(Box::leak(Box::new(Clock(clock.0))))));

    // 1. TODAY'S PATH: one world for every body.
    let today_records = reader();
    let mut today = ProjectWorld::from_records(today_records, frame(), lang, g).unwrap_or_else(|e| panic!("the world did not start: {e}"));
    today.set_cost(Some(cost));
    let mut today_us: HashMap<ItemKey, u64> = HashMap::new();
    let (mut today_clean, mut today_sum) = (0usize, Spent::default());
    let start = Instant::now();
    for (key, body) in bodies.iter() {
        let (before, t0) = (cost.spent(), Instant::now());
        let (verdict, borrows) = today.check_borrows(*body, g);
        today_us.insert(*key, t0.elapsed().as_micros() as u64);
        today_sum.add(&cost.spent().since(&before));
        if verdict.is_clean() && borrows.refused.is_none() && borrows.findings.is_empty() {
            today_clean += 1;
        }
    }
    let today_total = start.elapsed();
    let n = bodies.len() as u64;
    say!("TODAY {} bodies ({today_clean} clean) in {} ms: {} us a body;{}", bodies.len(), today_total.as_millis(), today_total.as_micros() as u64 / n, parts(&today_sum, n));

    // 2. THE TASK: a fresh world per body over the view with the impl tables.
    let task_records = reader();
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let view = Tables::new(TablesOpts { records: task_records, impls: Some(tables), lang_crates: lang, reads: &unused });
    let shared = blaze_own::testing::heap().shared();
    let (mut clean, mut with_whole, mut sum, mut answers) = (0usize, 0usize, Spent::default(), ContentId::of("cost answers", b""));
    let mut whole_sections: HashMap<String, usize> = HashMap::new();
    // The costliest heads (inclusive), each body's four: (bodies, ns in all).
    let mut slow: HashMap<String, (u64, u64)> = HashMap::new();
    cost.take_slowest();
    // Each body's bytes and read list, for the strategies' equality below.
    let mut kept_outs: Vec<(ItemKey, Vec<u8>, String)> = Vec::new();
    let mut all_reads: HashMap<ItemKey, Vec<String>> = HashMap::new();
    let start = Instant::now();
    for (key, _) in bodies.iter() {
        let body_frame = BodyFrame { frame: frame(), alloc: g, plant_drop: None, cost: Some(cost), needs: NeedsMet::All, heads: None, sources: None, goals: None, prepared: None };
        let (before, b0, t0) = (cost.spent(), BYTES.load(Ordering::Relaxed), Instant::now());
        let out = check_body(view, &task_of(*key), &body_frame, &NameMake, shared).unwrap_or_else(|e| panic!("the body task of {key:x}: {e}"));
        let us = t0.elapsed().as_micros() as u64;
        let bytes = BYTES.load(Ordering::Relaxed) - b0;
        let spent = cost.spent().since(&before);
        sum.add(&spent);
        if !out.done.failed && out.done.findings == 0 {
            clean += 1;
        }
        let wholes: Vec<String> = out.reads.iter().filter_map(|r| match r {
            TableRead::Names(NameCell::Whole { section, .. }) => Some(format!("{section:?}")),
            _ => None,
        }).collect();
        if !wholes.is_empty() {
            with_whole += 1;
        }
        for w in wholes.iter() {
            *whole_sections.entry(w.clone()).or_default() += 1;
        }
        let mut digest = Vec::new();
        digest.extend_from_slice(&answers.0);
        digest.extend_from_slice(&out.bytes);
        digest.extend_from_slice(format!("{:?}", out.done.reads).as_bytes());
        answers = ContentId::of("cost answers", &digest);
        kept_outs.push((*key, out.bytes.to_vec(), format!("{:?}", out.done.reads)));
        all_reads.insert(*key, out.reads.iter().map(|r| format!("{r:?}")).collect());
        let label = NamesSource::name_of(task_records, *key).unwrap_or("?");
        for (ns, want) in cost.take_slowest().into_iter().flatten() {
            let entry = slow.entry(format!("{want:?} `{}`", NamesSource::name_of(task_records, want.key()).unwrap_or("?"))).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += ns;
        }
        note!(
            "BODY {label}: task {us} us ({} KB), today {} us; {} reads, {} whole; failed {} findings {};{}",
            bytes / 1024,
            today_us.get(key).copied().unwrap_or(0),
            out.done.reads.len(),
            wholes.len(),
            out.done.failed,
            out.done.findings,
            parts(&spent, 1)
        );
    }
    let total = start.elapsed();
    say!("TASK {} bodies ({clean} clean) in {} ms: {} us a body;{}", bodies.len(), total.as_millis(), total.as_micros() as u64 / n, parts(&sum, n));
    let named: u64 = sum.ns.iter().sum();
    say!("TASK named parts {} us a body of {} us; unnamed {} us a body", named / 1000 / n, total.as_micros() as u64 / n, (total.as_nanos() as u64).saturating_sub(named) / 1000 / n);
    let mut slow: Vec<(String, (u64, u64))> = slow.into_iter().collect();
    slow.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(&b.0)));
    for (want, (bodies_in, ns)) in slow.iter().take(12) {
        say!("SLOW HEAD {want}: among the 4 costliest of {bodies_in} bodies, {} ms in all (inclusive)", ns / 1_000_000);
    }
    let mut sections: Vec<(String, usize)> = whole_sections.into_iter().collect();
    sections.sort();
    say!("WHOLE READS: {with_whole} of {} bodies read a whole section; by section {sections:?}", bodies.len());
    say!("ANSWERS {answers:?}");

    // 3. THE SAME TASKS with a trait made again after each need (heads.rs `NeedsMet::First`,
    //    the form before): equal bytes and read lists, body by body.
    let start = Instant::now();
    let mut differ: Vec<String> = Vec::new();
    let mut reads_differ: Vec<String> = Vec::new();
    for (key, bytes, reads) in kept_outs.iter() {
        let body_frame = BodyFrame { frame: frame(), alloc: g, plant_drop: None, cost: None, needs: NeedsMet::First, heads: None, sources: None, goals: None, prepared: None };
        let out = check_body(view, &task_of(*key), &body_frame, &NameMake, shared).unwrap_or_else(|e| panic!("the body task of {key:x}: {e}"));
        let label = NamesSource::name_of(task_records, *key).unwrap_or("?");
        if out.bytes.as_slice() != bytes.as_slice() {
            differ.push(format!("`{label}` answer"));
            note!("DIFFER `{label}` all:\n{}\nfirst:\n{}", String::from_utf8_lossy(bytes), String::from_utf8_lossy(&out.bytes));
        }
        if &format!("{:?}", out.done.reads) != reads {
            let first: Vec<String> = out.reads.iter().map(|r| format!("{r:?}")).collect();
            let all: Vec<String> = all_reads.get(key).cloned().unwrap_or_default();
            let only_all: Vec<&String> = all.iter().filter(|r| !first.contains(r)).collect();
            let only_first: Vec<&String> = first.iter().filter(|r| !all.contains(r)).collect();
            // OPEN (reported): the interim read list holds the reads of the heads the body
            // made in its own store, so it moves with which heads were made on the way; the
            // answers do not. Counted, not asserted, until frozen heads replace the store.
            reads_differ.push(format!("`{label}` +{} -{}", only_all.len(), only_first.len()));
            note!("READS DIFFER `{label}`: only when all needs are met {only_all:?}; only one at a time {only_first:?}");
        }
    }
    say!("NEEDS ONE AT A TIME: {} bodies in {} ms; answers differ: {differ:?}; read lists differ (reads only with all needs met +, only one at a time -): {reads_differ:?}", kept_outs.len(), start.elapsed().as_millis());

    // 3b. SHARED GOALS (blaze-solve share.rs, plan row A4): the same tasks over ONE request's
    //     table of trait goals, forward and reversed, THE AUDIT ON (every hit solved again in
    //     the body and compared: a difference fails the body's check). Equal bytes body by
    //     body; then THE PLANTED BUG (a hit without its read) must be caught by the audit.
    let shared_pass = |opts: blaze_solve::share::ShareOpts, reversed: bool| -> (Vec<String>, usize, Vec<(String, usize)>) {
        use blaze_ladder::front2::tables::goals::{GoalTable, GoalTableOpts, MOST_GOALS};
        let table: &'static GoalTable = Box::leak(Box::new(GoalTable::new(GoalTableOpts { share: opts, most: MOST_GOALS })));
        let mut order: Vec<&(ItemKey, Vec<u8>, String)> = kept_outs.iter().collect();
        if reversed {
            order.reverse();
        }
        let mut differ: Vec<String> = Vec::new();
        let mut per_body: Vec<(String, usize)> = Vec::new();
        for (key, bytes, _) in order {
            let body_frame = BodyFrame { frame: frame(), alloc: g, plant_drop: None, cost: None, needs: NeedsMet::All, heads: None, sources: None, goals: Some(table), prepared: None };
            let out = check_body(view, &task_of(*key), &body_frame, &NameMake, shared).unwrap_or_else(|e| panic!("the body task of {key:x}: {e}"));
            let label = NamesSource::name_of(task_records, *key).unwrap_or("?").to_string();
            if out.bytes.as_slice() != bytes.as_slice() {
                // (Printed plainly: `note!` holds the said-so-far buffer, which the lines after this closure also write.)
                println!("SHARED DIFFER `{label}`:\n{}\nalone:\n{}", String::from_utf8_lossy(&out.bytes), String::from_utf8_lossy(bytes));
                differ.push(label.clone());
            }
            per_body.push((label, out.goal_hits.len()));
        }
        per_body.sort();
        (differ, table.len(), per_body)
    };
    let audit = blaze_solve::share::ShareOpts { audit: true, ..Default::default() };
    let start = Instant::now();
    let (fwd_differ, fwd_rows, fwd_hits) = shared_pass(audit, false);
    let (rev_differ, rev_rows, rev_hits) = shared_pass(audit, true);
    let hits: usize = fwd_hits.iter().map(|h| h.1).sum();
    say!("SHARED GOALS (audit on): {} bodies forward and reversed in {} ms; {fwd_rows} goals put forward, {rev_rows} reversed; {hits} hits forward, {} reversed",
        kept_outs.len(), start.elapsed().as_millis(), rev_hits.iter().map(|h| h.1).sum::<usize>());
    for (label, n) in fwd_hits.iter().filter(|h| h.1 != 0) {
        say!("SHARED BODY `{label}`: {n} goals taken from the table forward");
    }
    assert!(fwd_differ.is_empty() && rev_differ.is_empty(), "shared goals changed answers: forward {fwd_differ:?}, reversed {rev_differ:?}");
    assert!(hits > 0, "no body took a shared goal: the table is not reached");
    let plant = blaze_solve::share::ShareOpts { audit: true, plant_hit_without_read: true, ..Default::default() };
    let (plant_differ, _, _) = shared_pass(plant, false);
    say!("SHARED PLANT (a hit without its read, audit on): bodies refused {plant_differ:?}");
    assert!(!plant_differ.is_empty(), "the audit missed the planted hit without its read");

    // 4. SEEDED: the request's heads frozen ONCE (lane FJ's `SeededHeads`, each item lowered
    //    once; counted apart), then every body's heads filed from that one READ-ONLY table:
    //    a body lowers no head. Equal answers to the lowering task, body by body.
    let sized = today.front.index.lang("sized");
    let freeze_records = reader();
    // The freeze's constant evaluator: the store path's own (check.rs `eval_len`), over the
    // program's index and no prepared macros, as a `ProjectWorld` sets it up.
    let const_world: &'static ProjectWorld<'static, &'static Records<'static>> =
        Box::leak(Box::new(ProjectWorld::from_records(freeze_records, frame(), lang, g).unwrap_or_else(|e| panic!("the world did not start: {e}"))));
    let prepared: &'static Prepares<'static> = Prepares::new_in(blaze_own::testing::bump());
    let evaluator: &'static mut (dyn ConstValues<'static> + 'static) = Box::leak(Box::new(StoreConsts { names: freeze_records, index: &const_world.front.index, prepared }));
    let start = Instant::now();
    let freezing: &'static Freezing<'static, 'static, &'static Records<'static>> = Box::leak(Box::new(Freezing::new(
        freeze_records,
        SeededHeads::new(SeededHeadsOpts { names: freeze_records, sized, pointers: blaze_ladder::front2::receiver::Pointers::NOT_LOOKED_UP, frame: frame(), scratch: g, alloc: g, consts: Some(evaluator), known: None, owners: None }),
    )));
    // A body whose heads the record form cannot carry yet (FJ's known limits) is REFUSED by
    // type: counted with its reason, and left out of the seeded run.
    let mut refused: HashMap<ItemKey, String> = HashMap::new();
    for (key, _, _) in kept_outs.iter() {
        let body_frame = BodyFrame { frame: frame(), alloc: g, plant_drop: None, cost: None, needs: NeedsMet::All, heads: Some(freezing), sources: None, goals: None, prepared: None };
        if let Err(e) = check_body(view, &task_of(*key), &body_frame, &NameMake, shared) {
            let label = NamesSource::name_of(task_records, *key).unwrap_or("?");
            note!("SEED REFUSED `{label}`: {e}");
            refused.insert(*key, e.to_string());
        }
    }
    let mut reasons: HashMap<String, usize> = HashMap::new();
    for why in refused.values() {
        // The reason without the body's own name and keys: the text after the last `: `.
        *reasons.entry(why.rsplit(": ").next().unwrap_or(why).to_string()).or_default() += 1;
    }
    let mut reasons: Vec<(String, usize)> = reasons.into_iter().collect();
    reasons.sort();
    say!("SEED REFUSED {} of {} bodies (record form limits); by reason {reasons:?}", refused.len(), kept_outs.len());
    let frozen: &'static FrozenTable<'static> = Box::leak(Box::new(FrozenTable::of(freezing, g).unwrap_or_else(|e| panic!("the frozen table: {e:?}"))));
    // Read through the BODIES' own names, not the freezer's: one table of frozen bytes serves
    // every reader (seeds.rs `TableView`).
    let table: &'static TableView<'static, 'static, &'static Records<'static>> = Box::leak(Box::new(frozen.view(task_records)));
    let (froze_lowered, reentered, attempts) = { let h = freezing.heads.borrow(); (h.lowered, h.reentered, h.attempts) };
    say!("FREEZE ONCE: {} cells in {} ms ({} wants lowered in all, {} met up the stack, {attempts} freeze attempts)", frozen.len(), start.elapsed().as_millis(), froze_lowered, reentered);
    let (mut seeded_sum, mut seeded_differ, mut seeded_wrong) = (Spent::default(), Vec::new(), Vec::new());
    let mut seeded_gaps: HashMap<String, usize> = HashMap::new();
    // The seeded bodies that answer as lowered: their cost alone, and today's (one world) for them.
    let (mut equal_sum, mut equal_n, mut equal_us, mut lowered_us) = (Spent::default(), 0u64, 0u64, 0u64);
    let start = Instant::now();
    let seeded_n = (kept_outs.len() - refused.len()) as u64;
    // Per seeded body: its rounds, the heads it asked (imports on the spot and between rounds,
    // `Part::Heads` entries) and its impl questions filed (`Part::Impls` entries), its name.
    let mut per_body: Vec<(u64, u64, u64, String)> = Vec::new();
    for (key, bytes, _) in kept_outs.iter().filter(|(k, _, _)| !refused.contains_key(k)) {
        let body_frame = BodyFrame { frame: frame(), alloc: g, plant_drop: None, cost: Some(cost), needs: NeedsMet::All, heads: Some(table), sources: None, goals: None, prepared: None };
        let (before, t0) = (cost.spent(), Instant::now());
        let out = check_body(view, &task_of(*key), &body_frame, &NameMake, shared).unwrap_or_else(|e| panic!("seeded, the body task of {key:x}: {e}"));
        let spent = cost.spent().since(&before);
        seeded_sum.add(&spent);
        {
            use blaze_ladder::front2::tables::cost::Part;
            let label = NamesSource::name_of(task_records, *key).unwrap_or("?").to_string();
            let (heads, impls) = (spent.entries[Part::Heads as usize], spent.entries[Part::Impls as usize]);
            say!("SEEDED BODY `{label}`: rounds {} heads asked {heads} impl questions {impls}", spent.rounds);
            per_body.push((spent.rounds, heads, impls, label));
        }
        if out.bytes.as_slice() == bytes.as_slice() {
            equal_sum.add(&spent);
            equal_n += 1;
            equal_us += t0.elapsed().as_micros() as u64;
            lowered_us += today_us.get(key).copied().unwrap_or(0);
        }
        if out.bytes.as_slice() != bytes.as_slice() {
            let label = NamesSource::name_of(task_records, *key).unwrap_or("?");
            if !out.done.failed {
                // A DIFFERENT ANSWER: never allowed.
                seeded_wrong.push(format!("`{label}`"));
            }
            // A refusal (a gap of the import, FJ's record form): counted by its first gap.
            let text = String::from_utf8_lossy(&out.bytes).to_string();
            let gap = text.split("gaps [").nth(1).map(|g| g.split(['{', '(', ']']).take(3).collect::<Vec<_>>().join("/")).unwrap_or_default();
            *seeded_gaps.entry(gap).or_default() += 1;
            seeded_differ.push(format!("`{label}`"));
            note!("SEEDED DIFFER `{label}` lowered:\n{}\nseeded:\n{}", String::from_utf8_lossy(bytes), String::from_utf8_lossy(&out.bytes));
        }
    }
    let total = start.elapsed();
    {
        let mut by_rounds: std::collections::BTreeMap<u64, usize> = std::collections::BTreeMap::new();
        for (rounds, _, _, _) in per_body.iter() {
            *by_rounds.entry(*rounds).or_default() += 1;
        }
        let most = |f: fn(&(u64, u64, u64, String)) -> u64| per_body.iter().map(f).max().unwrap_or(0);
        let sum = |f: fn(&(u64, u64, u64, String)) -> u64| per_body.iter().map(f).sum::<u64>();
        let n = per_body.len().max(1) as u64;
        let mut over: Vec<&(u64, u64, u64, String)> = per_body.iter().filter(|b| b.0 > 1).collect();
        over.sort_by(|a, b| b.0.cmp(&a.0).then(a.3.cmp(&b.3)));
        let over: Vec<String> = over.iter().take(8).map(|b| format!("`{}` {} rounds {} heads {} impls", b.3, b.0, b.1, b.2)).collect();
        say!(
            "SEEDED ROUNDS a body (rounds: bodies) {by_rounds:?}; heads asked a body {} (most {}); impl questions a body {} (most {}); most rounds: {over:?}",
            sum(|b| b.1) / n,
            most(|b| b.1),
            sum(|b| b.2) / n,
            most(|b| b.2)
        );
    }
    say!("SEEDED {seeded_n} bodies in {} ms: {} us a body;{}", total.as_millis(), total.as_micros() as u64 / seeded_n.max(1), parts(&seeded_sum, seeded_n));
    say!("SEEDED EQUAL {equal_n} bodies: {} us a body (today's one world: {} us a body for the same);{}", equal_us / equal_n.max(1), lowered_us / equal_n.max(1), parts(&equal_sum, equal_n));
    let mut seeded_gaps: Vec<(String, usize)> = seeded_gaps.into_iter().collect();
    seeded_gaps.sort();
    say!("SEEDED answers differ from the lowering task: {} of {seeded_n} (each a refusal, by first gap {seeded_gaps:?}): {seeded_differ:?}; a different clean answer: {seeded_wrong:?}", seeded_differ.len());

    let out = std::env::temp_dir().join("front2_tables_cost.out");
    std::fs::write(&out, &said).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
    assert!(differ.is_empty(), "answers differ with needs met one at a time: {differ:?}");
    // OPEN (reported): the import's gaps (FJ's record form) refuse bodies; counted above. A
    // seeded body never answers otherwise than the lowering one.
    // KNOWN WRONG (a ratchet: a new one fails, a fixed one must be removed here). Fixed by lane
    // FL: `res` (`match r { Ok(v) => v, Err(()) => 0 }`), whose enum's ctors were not in its
    // frozen record (head_tests.rs `a_seeded_store_answers_every_ctor_lookup_as_lowering`).
    const KNOWN_WRONG: &[&str] = &[];
    let new_wrong: Vec<&String> = seeded_wrong.iter().filter(|w| !KNOWN_WRONG.contains(&w.as_str())).collect();
    let fixed: Vec<&&str> = KNOWN_WRONG.iter().filter(|k| !seeded_wrong.iter().any(|w| w == *k)).collect();
    assert!(new_wrong.is_empty(), "a body seeded from the frozen table answered otherwise than lowering it: {new_wrong:?}");
    assert!(fixed.is_empty(), "known wrong seeded answers now right: remove them from KNOWN_WRONG: {fixed:?}");
    // A SEEDED BODY IS CHECKED IN ONE ROUND (plan row A1, lane RA1): a head it lacks is imported
    // on the spot (world_items.rs `BodyWorld::store_want`) and an impl question answered on the
    // spot (`BodyWorld::impl_question`), so no check restarts from the body's first line. No
    // kind of question is excluded: one the world leaves to a fill (check.rs `NotHere`, or a
    // refusal met on the spot) is a body named here.
    // One body of the 58 runs no round at all (an unnamed body that is refused before its first round: the
    // first admitted run printed `{0: 1, 1: 57}`); what is asserted is that none takes a SECOND round.
    let over: Vec<String> = per_body.iter().filter(|b| b.0 > 1).map(|b| format!("`{}` {} rounds {} heads {} impl questions", b.3, b.0, b.1, b.2)).collect();
    assert!(!per_body.is_empty(), "no seeded body was checked");
    assert!(over.is_empty(), "{} of {} seeded bodies took other than one round: {over:?}", over.len(), per_body.len());
    let errors: Vec<String> = task_records.errors().iter().map(|e| e.to_string()).collect();
    say!("RECORD ERRORS: {errors:?}");
    let out = std::env::temp_dir().join("front2_tables_cost.out");
    std::fs::write(&out, said).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
}
