//! How fast front2 checks real code with the world: `check_speed <crate> [--threads N] [--cap M]`.
//!
//! For ONE crate of the toolchain (as tests/front2_census.rs lists its bodies: file index, offset,
//! expansion offset, key), every phase timed apart from the outside, each once:
//! 1. names resolved (printed, not the subject); `Front2::new`; every head of the crate made
//!    (tests/front2_heads.rs's wants), by kind;
//! 2. pass A: every body lowered then checked over the records the heads left (time each apart);
//! 3. pass B (warm): the same bodies again, the records already made;
//! 4. the ten slowest bodies of pass A, each again from EMPTY records (time, heads it made);
//! 5. one small and one median body alone from empty records (with and without the impl
//!    buckets prebuilt; `Buckets::new` timed alone): what one edit pays in a fresh process;
//! 6. with `--threads N`: every body from empty records per thread, on 1 thread and on N;
//! then the peak footprint by phase (getrusage) and the ratio to the no-world floor.
//! Every line is printed in a fixed order (no map order), so two runs compare line by line.
//! Inside one body's check, blaze-typeck's tally (asks.rs `Tally`, through
//! `check_body_counted`) counts rounds, questions by kind, table conversions and their entries,
//! and splits the time between the world's answers, hashing them and the rest; each pass prints
//! the sums. Fills per round (the records front2 makes between rounds) are not counted.

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn, lower_member};
use blaze_ladder::front2::buckets::Buckets;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body_counted, eval_len};
use blaze_typeck::{Clock, QUESTION_KINDS, Tally};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::resolve::{At, Names};
use blaze_surface::ItemDetail;

const EDITION: &str = "blaze-2026.11";
const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}
";
const MOST_LENS: u32 = 64;
/// The no-world rate of docs/v2/plan/design/typeck.md (bstd, every name unknown): the floor.
const FLOOR_BODIES_PER_S: f64 = 71_747.0;

// Send/Sync probes by inherent-over-trait resolution (the `impls` crate's method): a type's
// inherent const exists only when the bound holds, else the trait's default `false` is read.
struct SyncP<T: ?Sized>(core::marker::PhantomData<T>);
struct SendP<T: ?Sized>(core::marker::PhantomData<T>);
trait NoSync {
    const YES: bool = false;
}
trait NoSend {
    const YES: bool = false;
}
impl<T: ?Sized> NoSync for SyncP<T> {}
impl<T: ?Sized> NoSend for SendP<T> {}
impl<T: ?Sized + Sync> SyncP<T> {
    const YES: bool = true;
}
impl<T: ?Sized + Send> SendP<T> {
    const YES: bool = true;
}

#[derive(Clone, Copy)]
enum Target<'n> {
    Free(blaze_id::ItemKey),
    Member { container: blaze_id::ItemKey, name: &'n str },
}

struct Entry<'n> {
    order: (u32, u32, u32, u64),
    target: Target<'n>,
    path: String,
    bytes: usize,
}

/// One set of records: what a process keeps between bodies.
struct St<'n> {
    store: Store<'n>,
    buckets: Option<Buckets>,
    consts: Consts<'n>,
    containers: BTreeSet<blaze_id::ItemKey>,
    failed_containers: BTreeSet<blaze_id::ItemKey>,
}

impl<'n> St<'n> {
    fn new(sized: Option<blaze_id::ItemKey>, frame: &'n blaze_mem::Frame, tables: blaze_own::Alloc<'static>) -> St<'n> { St { store: Store::new(sized, frame, tables), buckets: None, consts: Consts::default(), containers: BTreeSet::new(), failed_containers: BTreeSet::new() } }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    Clean,
    CheckFail,
    LowerFail,
    NoBody,
    ContainerFail,
    Panic,
}

const OUTCOMES: [(Outcome, &str); 6] = [
    (Outcome::Clean, "clean"),
    (Outcome::CheckFail, "check refused (diagnostic, gap or check error)"),
    (Outcome::LowerFail, "lowering refused"),
    (Outcome::NoBody, "no body (required trait fn)"),
    (Outcome::ContainerFail, "container head refused"),
    (Outcome::Panic, "panic"),
];

#[derive(Clone, Copy)]
struct Rec {
    container_ns: u64,
    lower_ns: u64,
    check_ns: u64,
    outcome: Outcome,
    /// Heads (ADT, impl, fn, trait) the store gained over this body.
    heads_made: usize,
    /// Type-table nodes the store gained over this body.
    nodes_made: usize,
    /// What the body's check asked and what the answers cost (blaze-typeck's `Tally`).
    tally: Tally,
}

/// The tally's clock (rule 75: the example gives it): nanoseconds since the run started.
struct Mono(Instant);

impl Clock for Mono {
    fn now_ns(&self) -> u64 {
        self.0.elapsed().as_nanos() as u64
    }
}

/// The counters of `recs` summed: rounds, questions by kind, conversions, and where the
/// check's time went.
fn tally_lines(label: &str, recs: &[&Rec], out: &mut String) {
    let mut t = Tally::default();
    for r in recs {
        t.add(&r.tally);
    }
    let rounds: Vec<u64> = recs.iter().map(|r| r.tally.checks).collect();
    let mut hist = [0usize; 6];
    for &n in &rounds {
        hist[(n.max(1) as usize - 1).min(5)] += 1;
    }
    out.push_str(&format!(
        "{label} rounds: {} checks for {} bodies; bodies by rounds 1:{} 2:{} 3:{} 4:{} 5:{} 6+:{}\n",
        t.checks,
        recs.len(),
        hist[0],
        hist[1],
        hist[2],
        hist[3],
        hist[4],
        hist[5]
    ));
    out.push_str(&format!("{label} speculative needs dropped {} (fills past a hole that could not be made)\n", t.dropped));
    let mut late = String::new();
    for (i, name) in QUESTION_KINDS.iter().enumerate() {
        if t.late_holes[i] > 0 {
            late.push_str(&format!(" {name} {};", t.late_holes[i]));
        }
    }
    out.push_str(&format!("{label} holes met by second and later runs (what makes a third round):{late}\n"));
    let mut asks = String::new();
    let mut total = 0;
    for (i, name) in QUESTION_KINDS.iter().enumerate() {
        total += t.asks[i];
        if t.asks[i] > 0 {
            asks.push_str(&format!(" {name} {};", t.asks[i]));
        }
    }
    out.push_str(&format!("{label} questions: {total} ({} missing), {:.1} per check:{asks}\n", t.missing, total as f64 / t.checks.max(1) as f64));
    out.push_str(&format!(
        "{label} table conversions {}: entries converted {}; entries reached {}; entries offered {} ({:.0} per conversion)\n",
        t.conversions,
        t.entries_converted,
        t.entries_reached,
        t.entries_offered,
        t.entries_offered as f64 / t.conversions.max(1) as f64
    ));
    out.push_str(&format!(
        "{label} time in checks {}: world answers {}, hashing answers {}, the rest (walk, solver, conversions) {}; of all, facts fills (their asks included) {}\n",
        ms(t.check_ns),
        ms(t.world_ns),
        ms(t.hash_ns),
        ms(t.check_ns.saturating_sub(t.world_ns + t.hash_ns)),
        ms(t.convert_ns)
    ));
    let s = t.solve;
    out.push_str(&format!(
        "{label} solver: {} calls in {} ({:.1} us each), {} repeated after a fact fill; frames {} ({:.1} per call); cache hits {}; ground fast path {}; impl candidates {} ({:.2} per frame); cycle reruns {}\n",
        s.calls,
        ms(s.ns),
        s.ns as f64 / 1000.0 / s.calls.max(1) as f64,
        s.refills,
        s.stats.frames,
        s.stats.frames as f64 / s.calls.max(1) as f64,
        s.stats.cache_hits,
        s.stats.fast_path,
        s.stats.candidates,
        s.stats.candidates as f64 / s.stats.frames.max(1) as f64,
        s.stats.reruns
    ));
    out.push_str(&format!(
        "{label} checker phases: walk and rules (all less world answers, hashing, solver calls and fact fills; a fill's asks are taken off twice, so a lower bound) {}\n",
        ms(t.check_ns.saturating_sub(t.world_ns + t.hash_ns + s.ns + t.convert_ns))
    ));
}

fn heads_of(s: &Store<'_>) -> usize {
    s.adts.len() + s.impls.len() + s.fns.len() + s.maps.traits.len()
}

fn kinds(s: &Store<'_>) -> String {
    format!(
        "adts {}, impls {}, fns {}, traits {}, trait members {}, preds {}, generics {}, aliases {}, ctors {}, fn gaps {}, table nodes {}",
        s.adts.len(),
        s.impls.len(),
        s.fns.len(),
        s.maps.traits.len(),
        s.trait_defs.len(),
        s.preds.len(),
        s.maps.generics.len(),
        s.maps.aliases.len(),
        s.ctors.len(),
        s.fn_gaps.len(),
        s.tys.len()
    )
}

fn ns(t: Instant) -> u64 {
    t.elapsed().as_nanos() as u64
}

fn ms(n: u64) -> String {
    format!("{:.1} ms", n as f64 / 1e6)
}

fn us(n: u64) -> String {
    format!("{:.1} us", n as f64 / 1e3)
}

fn rate(count: usize, total_ns: u64) -> f64 {
    if total_ns == 0 { 0.0 } else { count as f64 / (total_ns as f64 / 1e9) }
}

/// Peak resident set so far, in MB (macOS reports `ru_maxrss` in bytes).
fn maxrss_mb() -> f64 {
    let Ok(ru) = blaze_sys::posix::thread::process_rusage() else { return -1.0 };
    #[cfg(target_os = "macos")]
    let b = ru.max_rss_bytes as f64;
    #[cfg(target_os = "linux")]
    let b = (ru.max_rss_kb as f64) * 1000.0;
    if cfg!(target_os = "macos") { b / 1e6 } else { b / 1e3 }
}

fn load() -> f64 {
    let mut l = [0f64; 3];
    // SAFETY: getloadavg writes at most 3 doubles into the array.
    #[cfg(target_os = "macos")]
    let n = unsafe { blaze_sys::posix::proc::getloadavg(l.as_mut_ptr(), 3) };
    #[cfg(not(target_os = "macos"))]
    let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    if n < 1 { -1.0 } else { l[0] }
}

/// p-th percentile (nearest rank) of sorted values.
fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[i.clamp(1, sorted.len()) - 1]
}

fn dist(label: &str, mut v: Vec<u64>) -> String {
    v.sort_unstable();
    let sum: u64 = v.iter().sum();
    format!(
        "{label}: n {}, total {}, mean {}, p50 {}, p90 {}, p99 {}, max {}",
        v.len(),
        ms(sum),
        us(if v.is_empty() { 0 } else { sum / v.len() as u64 }),
        us(pct(&v, 50.0)),
        us(pct(&v, 90.0)),
        us(pct(&v, 99.0)),
        us(pct(&v, 100.0))
    )
}

fn make<'n>(r: &mut Records<'_, 'n>, w: Want) -> bool {
    let mut lens = 0;
    loop {
        match r.store.ensure(r.names, r.keys, w) {
            Ok(()) => return true,
            Err(HeadError::Item(_, Why::NeedConst(c, _))) if lens < MOST_LENS => {
                lens += 1;
                if eval_len(r, c).is_err() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

/// One body over `st`: its container's head (once per container in `st`), lowered, checked.
fn one<'n>(names: &'n Names<'n>, front: &mut Front2<'n>, st: &mut St<'n>, prepared: &Prepares<'n>, t: Target<'_>) -> Rec {
    let resolver = front.resolver;
    let (h0, n0) = (heads_of(&st.store), st.store.tys.len());
    let mut rec = Rec { container_ns: 0, lower_ns: 0, check_ns: 0, outcome: Outcome::Panic, heads_made: 0, nodes_made: 0, tally: Tally::default() };
    let clock = Mono(Instant::now());
    let St { store, buckets, consts, containers, failed_containers } = st;
    let mut r = Records { names, keys: &mut front.keys, index: &front.index, store, buckets, order: BuildOrder::AsAsked, consts: Some((consts, prepared)) };
    let got = catch_unwind(AssertUnwindSafe(|| -> Outcome {
        if let Target::Member { container, .. } = t {
            if failed_containers.contains(&container) {
                return Outcome::ContainerFail;
            }
            if !containers.contains(&container) {
                let c0 = Instant::now();
                let is_trait = matches!(resolver.detail(container), Some(ItemDetail::Trait));
                let ok = if is_trait { make(&mut r, Want::Head(container)) && make(&mut r, Want::Members(container)) } else { make(&mut r, Want::Impl(container)) };
                rec.container_ns = ns(c0);
                containers.insert(container);
                if !ok {
                    failed_containers.insert(container);
                    return Outcome::ContainerFail;
                }
            }
        }
        let l0 = Instant::now();
        let mut lens = 0;
        let lowered = loop {
            let lowered = match t {
                Target::Free(lib) => lower_fn(r.names, prepared, r.keys, r.store, lib),
                Target::Member { container, name } => match r.keys.filed_member(checker_of(container), MemberKind::Fn, name) {
                    Some(k) if !r.store.fn_gaps.contains_key(&k) => lower_member(r.names, prepared, r.keys, r.store, k),
                    _ => {
                        rec.lower_ns = ns(l0);
                        return Outcome::LowerFail;
                    }
                },
            };
            let c = match &lowered {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => *c,
                _ => break lowered,
            };
            lens += 1;
            if eval_len(&mut r, c).is_err() {
                rec.lower_ns = ns(l0);
                return Outcome::LowerFail;
            }
        };
        let body = match lowered {
            Ok(b) => b,
            Err(BodyError::NoBody(_)) => {
                rec.lower_ns = ns(l0);
                return Outcome::NoBody;
            }
            Err(_) => {
                rec.lower_ns = ns(l0);
                return Outcome::LowerFail;
            }
        };
        let view = body.lowered.view();
        let Ok(spell) = Spelling::of(&view) else {
            rec.lower_ns = ns(l0);
            return Outcome::LowerFail;
        };
        let scope = body.scope(&spell);
        rec.lower_ns = ns(l0);
        let k0 = Instant::now();
        let got = check_body_counted(&mut r, &body, &scope, Some((&mut rec.tally, &clock)));
        rec.check_ns = ns(k0);
        match got {
            Ok(done) if done.diags.is_empty() => Outcome::Clean,
            _ => Outcome::CheckFail,
        }
    }));
    rec.outcome = got.unwrap_or(Outcome::Panic);
    rec.heads_made = heads_of(&st.store) - h0;
    rec.nodes_made = st.store.tys.len().saturating_sub(n0);
    rec
}

fn outcome_line(label: &str, recs: &[Rec]) -> String {
    let mut s = format!("{label} outcomes:");
    for (o, name) in OUTCOMES {
        s.push_str(&format!(" {name} {};", recs.iter().filter(|r| r.outcome == o).count()));
    }
    s
}

/// One pass's lines: totals and rates of lowering and checking (bodies = those lowered).
fn pass_lines(label: &str, recs: &[Rec], out: &mut String) {
    let lowered: Vec<&Rec> = recs.iter().filter(|r| !matches!(r.outcome, Outcome::ContainerFail | Outcome::NoBody)).collect();
    let checked: Vec<&Rec> = recs.iter().filter(|r| matches!(r.outcome, Outcome::Clean | Outcome::CheckFail)).collect();
    let cont: u64 = recs.iter().map(|r| r.container_ns).sum();
    let low: u64 = lowered.iter().map(|r| r.lower_ns).sum();
    let chk: u64 = checked.iter().map(|r| r.check_ns).sum();
    out.push_str(&format!("{}\n", outcome_line(label, recs)));
    out.push_str(&format!("{label} container heads: {}\n", ms(cont)));
    out.push_str(&format!("{label} lower: {} bodies in {}: {:.0} bodies/s\n", lowered.len(), ms(low), rate(lowered.len(), low)));
    out.push_str(&format!("{label} check: {} bodies in {}: {:.0} bodies/s (floor {FLOOR_BODIES_PER_S:.0}/s: ratio {:.1}x)\n", checked.len(), ms(chk), rate(checked.len(), chk), FLOOR_BODIES_PER_S / rate(checked.len(), chk).max(1e-9)));
    out.push_str(&format!("{}\n", dist(&format!("{label} check per body"), checked.iter().map(|r| r.check_ns).collect())));
    out.push_str(&format!("{}\n", dist(&format!("{label} check per clean body"), checked.iter().filter(|r| r.outcome == Outcome::Clean).map(|r| r.check_ns).collect())));
    out.push_str(&format!("{}\n", dist(&format!("{label} lower per body"), lowered.iter().map(|r| r.lower_ns).collect())));
    tally_lines(label, &checked, out);
    let grew = recs.iter().filter(|r| r.heads_made > 0).count();
    out.push_str(&format!(
        "{label} records made: heads {} by {grew} bodies, table nodes {}\n",
        recs.iter().map(|r| r.heads_made).sum::<usize>(),
        recs.iter().map(|r| r.nodes_made).sum::<usize>()
    ));
}

fn main() {
    // The program's one allocator, made at its top and handed down: a frame, whose memory comes
    // from the OS (no global heap). Each worker thread makes its own (below).
    let held = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the program's frame");
    let held_erased = blaze_own::Erased::new(&held);
    let alloc = held_erased.handle();
    // The store's tables are `'static` (front2 heads.rs `Store::new`), which a frame made here is
    // not: until a tool's main is handed its heap, the global allocator is the one it can name.
    let tables = blaze_own::Alloc::global();
    let start = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    let cname = args.get(1).cloned().unwrap_or_else(|| "bstd".to_string());
    let mut threads = 0usize;
    let mut cap = usize::MAX;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--threads" => threads = args.get(i + 1).and_then(|s| s.parse().ok()).expect("--threads N"),
            "--cap" => cap = args.get(i + 1).and_then(|s| s.parse().ok()).expect("--cap M"),
            other => panic!("unknown argument {other:?}: check_speed <crate> [--threads N] [--cap M]"),
        }
        i += 2;
    }
    let mut out = String::new();
    let threads_avail = std::thread::available_parallelism().map_or(0, |n| n.get());
    out.push_str(&format!("check_speed {cname}: cap {}, threads {threads}, cores {threads_avail}, load before {:.2}\n", if cap == usize::MAX { "none".to_string() } else { cap.to_string() }, load()));

    // Phase 0: names (not the subject).
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let t0 = Instant::now();
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names: Names<'_> = inputs.resolve(&crates, held_erased.bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    out.push_str(&format!("phase names: toolchain opened and names resolved in {} (not the subject)\n", ms(ns(t0))));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let t0 = Instant::now();
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    out.push_str(&format!("phase front2: Front2::new (keys filed, index) in {}\n", ms(ns(t0))));
    out.push_str(&format!(
        "sync: Names {} send {}; Front2 {} send {}; Store {} send {}; Buckets {} send {}; Consts {} send {}; Prepares {} send {}; Alloc {} send {}\n",
        SyncP::<Names<'static>>::YES,
        SendP::<Names<'static>>::YES,
        SyncP::<Front2<'static>>::YES,
        SendP::<Front2<'static>>::YES,
        SyncP::<Store<'static>>::YES,
        SendP::<Store<'static>>::YES,
        SyncP::<Buckets>::YES,
        SendP::<Buckets>::YES,
        SyncP::<Consts<'static>>::YES,
        SendP::<Consts<'static>>::YES,
        SyncP::<Prepares<'static>>::YES,
        SendP::<Prepares<'static>>::YES,
        SyncP::<Alloc<'static>>::YES,
        SendP::<Alloc<'static>>::YES,
    ));
    let crate_names: Vec<&str> = crates.iter().map(|c| c.name).collect();
    let Some(krate) = crate_names.iter().position(|n| *n == cname) else { panic!("no crate named {cname:?} among {crate_names:?}") };
    let resolver = front.resolver;
    let cn = names.krate(krate);
    std::panic::set_hook(Box::new(|_| {}));

    // The crate's items, in file order (as the censuses).
    let pos = |key: blaze_id::ItemKey, text: &str| -> (u32, u32, u32) {
        let mut k = key;
        let mut inner: Option<u32> = None;
        let mut sub_text = text.as_ptr() as usize;
        for _ in 0..16 {
            let Some(p) = names.place(k) else { break };
            let Some((base, _)) = cn.outline_at(p.at) else { break };
            let b = base.as_ptr() as usize;
            let off = if sub_text >= b && sub_text <= b + base.len() { (sub_text - b) as u32 } else { 0 };
            match p.at {
                At::File(f) => return (f, off, inner.map_or(0, |i| i + 1)),
                At::Expansion(call) => {
                    inner.get_or_insert(off);
                    k = call;
                    sub_text = names.item_text(call).map_or(0, |t| t.text.as_ptr() as usize);
                }
            }
        }
        (u32::MAX, 0, inner.map_or(0, |i| i + 1))
    };
    let key_low = |k: blaze_id::ItemKey| -> u64 {
        let s = format!("{k:x}");
        u64::from_str_radix(&s[s.len().saturating_sub(16)..], 16).unwrap_or(0)
    };
    let items: Vec<(blaze_id::ItemKey, ItemDetail)> = names.items(krate).filter_map(|(k, _, _)| resolver.detail(k).map(|d| (k, d))).collect();
    let mut member_keys: BTreeSet<blaze_id::ItemKey> = BTreeSet::new();
    for (k, d) in &items {
        if matches!(d, ItemDetail::Impl | ItemDetail::Trait) {
            for m in names.members(*k).unwrap_or(&[]) {
                member_keys.insert(m.key);
            }
        }
    }
    let mut entries: Vec<Entry<'_>> = Vec::new();
    let mut head_items: Vec<((u32, u32, u32, u64), blaze_id::ItemKey, ItemDetail)> = Vec::new();
    for (k, d) in &items {
        if member_keys.contains(k) {
            continue;
        }
        let Some(it) = names.item_text(*k) else { continue };
        let (f, o, s) = pos(*k, it.text);
        if matches!(d, ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union | ItemDetail::Trait | ItemDetail::TraitAlias | ItemDetail::Impl | ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. } | ItemDetail::TypeAlias) {
            head_items.push(((f, o, s, key_low(*k)), *k, *d));
        }
        let module = cn.path_of(it.module).unwrap_or("?");
        match d {
            ItemDetail::Fn { .. } if !it.text.trim_end().ends_with(';') => {
                entries.push(Entry { order: (f, o, s, key_low(*k)), target: Target::Free(*k), path: format!("{module}::{}", it.name.unwrap_or("?")), bytes: it.text.len() });
            }
            ItemDetail::Impl | ItemDetail::Trait => {
                let head = it.impl_header.map(|h| h.chars().take(60).map(|c| if c == '\n' { ' ' } else { c }).collect::<String>()).unwrap_or_else(|| format!("trait {}", it.name.unwrap_or("?")));
                for m in names.members(*k).unwrap_or(&[]) {
                    let (ItemDetail::Fn { .. }, Some(name)) = (m.detail, m.name) else { continue };
                    let (f, o, s) = pos(*k, m.text);
                    entries.push(Entry { order: (f, o, s, key_low(m.key)), target: Target::Member { container: *k, name }, path: format!("{module}::<{head}>::{name}"), bytes: m.text.len() });
                }
            }
            _ => {}
        }
    }
    entries.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.path.cmp(&b.path)));
    head_items.sort_by_key(|h| h.0);
    let listed = entries.len();
    entries.truncate(cap);
    out.push_str(&format!("crate {cname}: {listed} fns with a body listed, {} taken (file order); {} head items\n", entries.len(), head_items.len()));
    out.push_str(&format!("rss after names+front2: {:.0} MB\n", maxrss_mb()));
    print!("{out}");
    out.clear();

    // Phase 1: every head of the crate, one shared store.
    let prepared = Prepares::new_in(held_erased.bump());
    let sized = front.index.lang("sized");
    let mut st = St::new(sized, front.frame, tables);
    let kinds_order = ["struct/enum/union", "trait", "trait alias", "impl", "fn", "const", "static", "type alias"];
    let mut by_kind: Vec<(u64, usize, usize)> = vec![(0, 0, 0); kinds_order.len()];
    let t0 = Instant::now();
    for (_, k, d) in &head_items {
        let k = *k;
        let St { store, buckets, consts, .. } = &mut st;
        let mut r = Records { names: &names, keys: &mut front.keys, index: &front.index, store, buckets, order: BuildOrder::AsAsked, consts: Some((consts, &prepared)) };
        let h0 = Instant::now();
        let (slot, ok) = match catch_unwind(AssertUnwindSafe(|| match d {
            ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union => (0, make(&mut r, Want::Adt(k))),
            ItemDetail::Trait => (1, make(&mut r, Want::Head(k)) && make(&mut r, Want::Members(k))),
            ItemDetail::TraitAlias => (2, make(&mut r, Want::Head(k))),
            ItemDetail::Impl => (3, make(&mut r, Want::Impl(k))),
            ItemDetail::Fn { .. } => (4, make(&mut r, Want::Fn(k))),
            ItemDetail::Const { .. } => (5, make(&mut r, Want::Fn(k))),
            ItemDetail::Static { .. } => (6, make(&mut r, Want::Fn(k))),
            _ => (7, make(&mut r, Want::Head(k))),
        })) {
            Ok(x) => x,
            Err(_) => (7, false),
        };
        let e = &mut by_kind[slot];
        e.0 += ns(h0);
        e.1 += 1;
        e.2 += usize::from(ok);
        if matches!(d, ItemDetail::Impl | ItemDetail::Trait) {
            st.containers.insert(k);
            if !ok {
                st.failed_containers.insert(k);
            }
        }
    }
    let heads_ns = ns(t0);
    let made = heads_of(&st.store);
    out.push_str(&format!(
        "phase heads: {} items asked in {}: {:.0} items/s; store holds {made} heads ({:.0} heads/s); impl buckets built: {}\n",
        head_items.len(),
        ms(heads_ns),
        rate(head_items.len(), heads_ns),
        rate(made, heads_ns),
        st.buckets.is_some()
    ));
    for (i, name) in kinds_order.iter().enumerate() {
        let (t, n, ok) = by_kind[i];
        out.push_str(&format!("heads {name}: {n} asked, {ok} made, {}\n", ms(t)));
    }
    out.push_str(&format!("store after heads: {}\n", kinds(&st.store)));
    let rss_heads = maxrss_mb();
    out.push_str(&format!("rss after heads: {rss_heads:.0} MB\n"));
    print!("{out}");
    out.clear();

    // Phase 2: pass A, every body over the heads' records.
    let tys_before = st.store.tys.len();
    let t0 = Instant::now();
    let pass_a: Vec<Rec> = entries.iter().map(|e| one(&names, &mut front, &mut st, &prepared, e.target)).collect();
    out.push_str(&format!("phase pass A (records from the heads phase, kept between bodies): wall {}\n", ms(ns(t0))));
    pass_lines("A", &pass_a, &mut out);
    let rss_a = maxrss_mb();
    let nb = pass_a.len().max(1);
    out.push_str(&format!(
        "rss after A: {rss_a:.0} MB; table nodes made per body {:.1} ({} -> {}); store after A: {}\n",
        (st.store.tys.len() - tys_before) as f64 / nb as f64,
        tys_before,
        st.store.tys.len(),
        kinds(&st.store)
    ));
    print!("{out}");
    out.clear();

    // Phase 3: pass B, warm.
    let t0 = Instant::now();
    let pass_b: Vec<Rec> = entries.iter().map(|e| one(&names, &mut front, &mut st, &prepared, e.target)).collect();
    out.push_str(&format!("phase pass B (warm: every record of pass A kept): wall {}\n", ms(ns(t0))));
    pass_lines("B", &pass_b, &mut out);
    let both = |r: &Rec| matches!(r.outcome, Outcome::Clean | Outcome::CheckFail);
    let a_chk: u64 = pass_a.iter().filter(|r| both(r)).map(|r| r.check_ns).sum();
    let b_chk: u64 = pass_b.iter().filter(|r| both(r)).map(|r| r.check_ns).sum();
    let changed = pass_a.iter().zip(&pass_b).filter(|(a, b)| a.outcome != b.outcome).count();
    out.push_str(&format!(
        "split from outside: A check {} = B (records present: checker + world answers read) {} ({:.1}%) + first-pass extra (records made, extra rounds) {} ({:.1}%); outcomes changed A->B: {changed}\n",
        ms(a_chk),
        ms(b_chk),
        100.0 * b_chk as f64 / a_chk.max(1) as f64,
        ms(a_chk.saturating_sub(b_chk)),
        100.0 * a_chk.saturating_sub(b_chk) as f64 / a_chk.max(1) as f64
    ));
    print!("{out}");
    out.clear();

    // Phase 4: the ten slowest of pass A, again from empty records.
    let mut idx: Vec<usize> = (0..pass_a.len()).collect();
    idx.sort_by(|&x, &y| pass_a[y].check_ns.cmp(&pass_a[x].check_ns).then(x.cmp(&y)));
    out.push_str("ten slowest checks of pass A (A check, B warm check, bytes, heads made in A; then alone from empty records: lower, check, heads in the store after):\n");
    for &i in idx.iter().take(10) {
        let e = &entries[i];
        let mut fresh = St::new(sized, front.frame, tables);
        let f0 = Instant::now();
        let r = one(&names, &mut front, &mut fresh, &prepared, e.target);
        let all = ns(f0);
        // Again over its own small records: warm, but a table of only what it needed. Against B
        // (warm over the whole crate's table) this tests whether the store table's size is a cost.
        let w = one(&names, &mut front, &mut fresh, &prepared, e.target);
        out.push_str(&format!(
            "  {} | warm over own records ({} table nodes) {} vs B over the crate's ({} nodes) {}\n",
            e.path,
            fresh.store.tys.len(),
            us(w.check_ns),
            st.store.tys.len(),
            us(pass_b[i].check_ns)
        ));
        out.push_str(&format!(
            "  {} | A {} | B {} | {} B | A made {} | fresh: lower {} check {} all {} heads {} ({}) | {:?}\n",
            e.path,
            us(pass_a[i].check_ns),
            us(pass_b[i].check_ns),
            e.bytes,
            pass_a[i].heads_made,
            us(r.lower_ns),
            us(r.check_ns),
            us(all),
            heads_of(&fresh.store),
            fresh.buckets.is_some(),
            OUTCOMES.iter().find(|o| o.0 == r.outcome).map_or("?", |o| o.1)
        ));
    }
    print!("{out}");
    out.clear();

    // Phase 5: one small body and one median body alone, from empty records.
    let mut clean: Vec<usize> = (0..pass_a.len()).filter(|&i| pass_a[i].outcome == Outcome::Clean).collect();
    clean.sort_by_key(|&i| (entries[i].bytes, i));
    let t0 = Instant::now();
    let _b = Buckets::new(&names, &front.index);
    let buckets_ns = ns(t0);
    out.push_str(&format!("Buckets::new alone (every impl header of the program read): {}\n", ms(buckets_ns)));
    let picks: Vec<(&str, usize)> = [("smallest clean", clean.first().copied()), ("median clean", clean.get(clean.len() / 2).copied())].into_iter().filter_map(|(l, i)| i.map(|i| (l, i))).collect();
    for (label, i) in picks {
        let e = &entries[i];
        let mut fresh = St::new(sized, front.frame, tables);
        let f0 = Instant::now();
        let r = one(&names, &mut front, &mut fresh, &prepared, e.target);
        let all = ns(f0);
        out.push_str(&format!(
            "cold {label}: {} ({} B): from empty records: container {} lower {} check {} all {}; heads made {}; buckets built {}; {}\n",
            e.path,
            e.bytes,
            us(r.container_ns),
            us(r.lower_ns),
            us(r.check_ns),
            us(all),
            heads_of(&fresh.store),
            fresh.buckets.is_some(),
            kinds(&fresh.store)
        ));
        let mut pre = St::new(sized, front.frame, tables);
        pre.buckets = Some(Buckets::new(&names, &front.index));
        let f0 = Instant::now();
        let r2 = one(&names, &mut front, &mut pre, &prepared, e.target);
        out.push_str(&format!(
            "cold {label} with buckets prebuilt: container {} lower {} check {} all {}; heads made {}\n",
            us(r2.container_ns),
            us(r2.lower_ns),
            us(r2.check_ns),
            us(ns(f0)),
            heads_of(&pre.store)
        ));
        let w = one(&names, &mut front, &mut pre, &prepared, e.target);
        out.push_str(&format!(
            "edit-loop floor for {label}: its warm check over its own records ({} table nodes) {} lower {}; over the crate's (B, {} nodes) check {} lower {}; cold/warm-own check ratio {:.0}x\n",
            pre.store.tys.len(),
            us(w.check_ns),
            us(w.lower_ns),
            st.store.tys.len(),
            us(pass_b[i].check_ns),
            us(pass_b[i].lower_ns),
            r.check_ns as f64 / w.check_ns.max(1) as f64
        ));
    }
    print!("{out}");
    out.clear();

    // Phase 6: threads, everything per thread. `Names` is not `Sync` (it holds
    // `blaze_own::Alloc`, the thread-bound handle; the first build of this example stopped on
    // `RawBuf<&str, Alloc<'_>>` cannot be shared between threads), so each thread opens the
    // toolchain, resolves names and makes its Front2 itself, then checks its share of the bodies
    // (index mod n) from empty records. The setup is timed apart from the work.
    if threads > 0 {
        let owned: Vec<(blaze_id::ItemKey, Option<String>)> = entries
            .iter()
            .map(|e| match e.target {
                Target::Free(k) => (k, None),
                Target::Member { container, name } => (container, Some(name.to_string())),
            })
            .collect();
        for n in [1usize, threads] {
            let t0 = Instant::now();
            let entries_r = &owned;
            let shelf_r = &shelf;
            let per: Vec<(u64, u64, u64, usize, usize)> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..n)
                    .map(|t| {
                        s.spawn(move || {
                            let f0 = Instant::now();
                            // This thread's own allocator, at its top: a frame is not `Sync`, so
                            // main's cannot be shared.
                            let held = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the thread's frame");
                            let held_erased = blaze_own::Erased::new(&held);
                            let alloc = held_erased.handle();
                            let tc = Toolchain::open(&mut DirShelf::new(shelf_r.clone())).unwrap_or_else(|e| panic!("the toolchain: {e}"));
                            let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
                            let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
                            let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
                            let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
                            let crates = inputs.crates();
                            let names: Names<'_> = inputs.resolve(&crates, held_erased.bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
                            let srcs = [SourceFile { path: "src/main.rs", text: MAIN }];
                            let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
                                let mut fr = Front2::new(&names, &frame, &srcs, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
                            let front_ns = ns(f0);
                            let prep = Prepares::new_in(held_erased.bump());
                            // The handle is not `Send`: each thread names its own, as main does.
                            let mut st = St::new(fr.index.lang("sized"), fr.frame, blaze_own::Alloc::global());
                            let (mut low, mut chk, mut done) = (0u64, 0u64, 0usize);
                            for (i, (k, name)) in entries_r.iter().enumerate() {
                                if i % n != t {
                                    continue;
                                }
                                let target = match name {
                                    None => Target::Free(*k),
                                    Some(nm) => Target::Member { container: *k, name: nm.as_str() },
                                };
                                let r = one(&names, &mut fr, &mut st, &prep, target);
                                low += r.lower_ns + r.container_ns;
                                chk += r.check_ns;
                                done += 1;
                            }
                            (front_ns, low, chk, done, heads_of(&st.store))
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().expect("a thread")).collect()
            });
            let wall = ns(t0);
            let done: usize = per.iter().map(|p| p.3).sum();
            let busiest = per.iter().map(|p| p.0 + p.1 + p.2).max().unwrap_or(0);
            let work_max = per.iter().map(|p| p.1 + p.2).max().unwrap_or(0);
            out.push_str(&format!(
                "threads {n} work only (no setup): busiest thread lower+check {}: {:.0} bodies/s\n",
                ms(work_max),
                rate(per.iter().map(|p| p.3).sum(), work_max)
            ));
            let heads: usize = per.iter().map(|p| p.4).sum();
            out.push_str(&format!(
                "threads {n}: {done} bodies, wall {} ({:.0} bodies/s incl. setup); per thread sums: setup (toolchain+names+front2) {}, container+lower {}, check {}; busiest thread {}; heads made in all threads {heads}; rss now {:.0} MB\n",
                ms(wall),
                rate(done, wall),
                ms(per.iter().map(|p| p.0).sum()),
                ms(per.iter().map(|p| p.1).sum()),
                ms(per.iter().map(|p| p.2).sum()),
                ms(busiest),
                maxrss_mb()
            ));
            print!("{out}");
            out.clear();
        }
    }

    out.push_str("not counted (no counter on this path): fills per round (the records front2 makes between rounds)\n");
    out.push_str(&format!("check_speed {cname}: done in {}; load after {:.2}; peak rss {:.0} MB\n", ms(ns(start)), load(), maxrss_mb()));
    print!("{out}");
}
