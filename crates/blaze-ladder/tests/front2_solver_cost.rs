//! THE TRAIT SOLVER'S COST ON REAL BODIES: the solver's counters (blaze-solve `Stats`, summed by
//! blaze-typeck's `Tally`) per body of a real `#![no_std]` root over the installed toolchain's
//! core, the same program and loader as tests/front2_tables_core.rs: every fn of the root
//! lowered and checked over the records alone, each with its own tally.
//!
//! It prints, per body and summed:
//! `BODY <fn>: goals <evaluations> frames <f> hits <h> fast <fp> distinct <d> terms <t> adt_fields
//! <a> summaries <s> prematch_rejected <r> impl_candidates <i> canon <c> (<w> words) decoded <dc>
//! fills <fl> (<m> missed) rows <conversions>/<entries> entries <rec> kept <k> solver_us <us>
//! check_us <us>`
//! and `GROUND <n> ground root goals over all bodies, <d> distinct per body summed, <u> distinct
//! across bodies`: the share of a body's ground goals another body asks too (what a cache of
//! proved goals across bodies could save).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_start_rule.rs.

#![feature(allocator_api)]

use std::alloc::Global;
use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::buckets::Buckets;
use blaze_ladder::front2::check::{self, BuildOrder, check_body_metered};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::front2::spell::Spelling;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_typeck::{Clock, Tally};

/// The root: `#![no_std]`, so the graph is core's (tests/front2_tables_core.rs's program).
const MAIN: &str = "#![no_std]
use core::ops::Add;

#[derive(Clone, Copy, PartialEq, Default)]
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

fn main() {}
";

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve: the test's effect layer.
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<ItemKey, &'static str>>,
    origins: Vec<HashMap<ItemKey, &'static [u8]>>,
}

/// An expansion's origin column as a loader serves it (tests/front2_records.rs `made_origins`).
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

/// The tally's clock (rule 75: the test gives it): nanoseconds since the run started.
struct Mono(Instant);

impl Clock for Mono {
    fn now_ns(&self) -> u64 {
        self.0.elapsed().as_nanos() as u64
    }
}

fn line(name: &str, t: &Tally) -> String {
    let s = &t.solve;
    let st = &s.stats;
    format!(
        "{name}: goals {} frames {} hits {} fast {} distinct {} terms {} adt_fields {} summaries {} summary_fields {} prematch_rejected {} impl_candidates {} header_mismatch {} reruns {} provisional {} need_stops {} abandoned {} pulls {} canon {} ({} words) canon_skipped {} decoded {} fills {} ({} missed) rows {}/{} entries {} kept {} rounds {} solver_calls {} solver_us {} check_us {}",
        st.evaluations,
        st.frames,
        st.cache_hits,
        st.fast_path,
        st.distinct_goals,
        st.terms_imported,
        st.adt_fields,
        st.auto_summaries,
        st.summary_fields,
        st.impl_prematch_rejected,
        st.impl_candidates,
        st.impl_header_mismatch,
        st.reruns,
        st.provisional,
        st.need_stops,
        st.frames_abandoned,
        st.pulls,
        st.canonicalized,
        st.canon_words,
        st.canon_skipped,
        st.decoded,
        s.fills,
        s.fill_misses,
        t.conversions,
        t.entries_converted,
        s.entries_recorded,
        s.entries_kept,
        t.checks,
        s.calls,
        s.ns / 1000,
        t.check_ns / 1000,
    )
}

#[test]
fn the_solver_s_cost_per_real_body() {
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
    let lang_crates = [0, core];
    let g = Alloc::global();

    // The solve, the records written from it, the root's fns (with their names).
    let (kept, fns) = {
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new(), origins: Vec::new() };
        for c in names.crates().iter() {
            kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.items.push(aligned(&items_record::encode_with(c, &names, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
            kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
            kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
        }
        let mut fns: Vec<ItemKey> = names.items(0).map(|(key, _, _)| key).filter(|key| (&names).kind_of_item(*key) == Some(ItemKind::Function)).collect();
        fns.sort_unstable();
        (kept, fns)
    };
    println!("SOLVED {} crates, {} fns of the root", crate_names.len(), fns.len());
    assert!(fns.len() >= 10, "the root has its fns: {}", fns.len());

    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, g)));
    let load: &'static Kept = Box::leak(Box::new(kept));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(keys, load, log, frame, g)));
    let mut front = Front2::from_records(records, frame, &lang_crates).unwrap_or_else(|e| panic!("front2 over records: {e}"));

    let clock = Mono(Instant::now());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), frame, g);
    let mut buckets: Option<Buckets> = None;
    let mut consts = Consts::default();
    let mut total = Tally::default();
    let mut lines = String::new();
    let (mut clean, mut ground_sum, mut ground_per_body) = (0usize, 0usize, 0usize);
    let mut ground_all: BTreeSet<u64> = BTreeSet::new();
    let mut root_keys: Vec<u64, Global> = Vec::new_in(Global);
    for (i, key) in fns.iter().enumerate() {
        let body = match lower_fn(records, prepared, &mut front.keys, &mut store, *key) {
            Ok(body) => body,
            Err(e) => panic!("{key:x} not lowered: {e}"),
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("{key:x} not spelled: {e:?}"));
        let scope = body.scope(&spell);
        let mut r = check::Records { names: records, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let mut tally = Tally::default();
        root_keys.clear();
        match check_body_metered(&mut r, &body, &scope, &mut tally, &clock, &mut root_keys) {
            Ok(_) => clean += 1,
            Err(e) => println!("NOT CLEAN {key:x}: {}", format!("{e:?}").chars().take(300).collect::<String>()),
        }
        ground_sum += root_keys.len();
        let mine: BTreeSet<u64> = root_keys.iter().copied().collect();
        ground_per_body += mine.len();
        ground_all.extend(mine);
        let l = line(&format!("BODY {i} ({key:x})"), &tally);
        println!("{l}");
        lines.push_str(&l);
        lines.push('\n');
        total.add(&tally);
    }
    let l = line(&format!("SUM of {} bodies ({clean} clean)", fns.len()), &total);
    println!("{l}");
    lines.push_str(&l);
    lines.push('\n');
    let by: Vec<String> = blaze_solve::facts::QUESTION_KIND_NAMES
        .iter()
        .zip(total.solve.misses_by_kind.iter())
        .filter(|(_, n)| **n > 0)
        .map(|(k, n)| format!("{k} {n}"))
        .collect();
    let m = format!("MISSES by kind: {}", by.join(", "));
    println!("{m}");
    lines.push_str(&m);
    lines.push('\n');
    let g = format!(
        "GROUND {ground_sum} ground root goals searched over all bodies, {ground_per_body} distinct per body summed, {} distinct across bodies",
        ground_all.len()
    );
    println!("{g}");
    lines.push_str(&g);
    lines.push('\n');
    assert_eq!(clean, fns.len(), "every body checks clean\n{lines}");
    // The counters seen through: a body that asked goals counted them.
    assert!(total.solve.stats.evaluations > 0 && total.solve.stats.canonicalized > 0 && total.solve.fills > 0, "the counters reached the tally\n{lines}");
    // Ceilings a quarter over the counts of 2026-11-17 (green d9773a9f76: frames 3194, 2124 of
    // them abandoned by 1445 need stops, 5780 goals canonicalized, 1692 fills all missed), so
    // the checker's own growth passes and a step back in the solver's cost fails here with the
    // counters in its message. Each fix of a counter lowers its ceiling.
    let s = &total.solve.stats;
    let ceil = |name: &str, got: u64, max: u64| assert!(got <= max, "{name}: {got} over its ceiling {max}\n{lines}");
    // The pull (blaze-solve facts.rs `Source`, typeck infer/mod.rs `Pull`; lane SW): a fact
    // the cache lacks is filled at the step that met it, so each distinct goal is one frame
    // (3194 frames for 1114 goals before; 1114 after) and the 1415 pulls stop nothing. The
    // stops left (30, 44 frames) are typeck's `Need` of the world, which ends the check round
    // (check.rs's rounds: 84), not the fact cache's.
    ceil("frames", s.frames, 1170);
    ceil("frames over distinct goals + abandoned", s.frames, s.distinct_goals + s.frames_abandoned);
    ceil("frames abandoned", s.frames_abandoned, 50);
    ceil("need stops", s.need_stops, 35);
    // One canonical walk per goal asked (frames + cache hits: 2510), less the goals with no
    // inference variable seen before, found by their own identity (blaze-solve eval.rs
    // `Solver::input_of`: 71 skipped, 2439 walked).
    ceil("canonicalized", s.canonicalized, 2560);
    ceil("canonical walks over goals asked", s.canonicalized + s.canon_skipped, s.frames + s.cache_hits + s.frames_abandoned);
    assert!(s.canon_skipped > 0, "no goal was found by its plain key\n{lines}");
    ceil("terms imported", s.terms_imported, 6190);
    ceil("fact cache misses", total.solve.fill_misses, 2115);
}
