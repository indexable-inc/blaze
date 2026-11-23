//! THE IMPL TABLES ON REAL CORE (engine.md E3, D131): the questions a real check asks, answered
//! alike by today's index (`ItemsIndex`: the items records' bucket section) and the impl
//! tables (`write_impl_tables` per crate, read through blaze-impls' lookups: `TableSet`).
//!
//! The program: a `#![no_std]` root on the installed toolchain's shelf, so its graph is the
//! shelf's core (and what core links) and the root; its bodies use core's operators, iterators,
//! `Option`, `Result`, `str`, slices, floats and derives. The graph is solved once, each crate's
//! names and items records written from the solve (as tests/front2_records.rs does), the
//! solved names dropped; then over the records alone:
//!
//! 1. EVERY fn body of the root is lowered and checked (`check::check_body`, the impl index
//!    lazy over the records: `Buckets::lazy`). The questions are the ones that check asked
//!    (`Buckets::asked`), never a hand list.
//! 2. Every crate's impl tables are written from its impl rows; none may be refused.
//! 3. Each question is asked ALONE over both sources (a fresh index each): the candidates, the
//!    unread among them, and the negatives at the question's shape must be equal (the items
//!    records file a trait's negatives with no shape, so the index's are filtered by their table
//!    row's shape: `row_answers`). Over the tables, each (crate, bucket) is read once per
//!    question.
//!
//! It prints the tables' size (buckets written, bytes, the largest bucket), the questions
//! asked, and the probes per question over each source:
//! `TABLES <crate>: <rows> rows, <n> buckets, <b> bytes (largest bucket <b> bytes: <key>), <t> us`
//! `CHECKED <n> bodies (<c> clean) in <t> ms; <q> questions asked`
//! `PROBES tables: <total> bucket reads, <max> at most for one question, <mean> per question; index: ..`
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_start_rule.rs.

#![feature(allocator_api)]

use std::collections::HashMap;
use std::time::Instant;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::buckets::{Ask, BucketSource, Buckets, ItemsIndex, TableSet, impl_tables_name, impl_tables_task, inherent_home, prepare_home, row_answers, rows_key, trait_home, write_impl_tables};
use blaze_ladder::front2::tables::recorder::TableRead;
use blaze_scope::names_cells::NameCell;
use blaze_ladder::front2::check::{self, BuildOrder, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::keys::Keys;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::front2::spell::Spelling;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;

/// The root: `#![no_std]`, so the graph is core's.
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

/// What one question answered: the candidates, the unread among them, the negatives.
#[derive(Debug, PartialEq, Eq)]
struct Answer {
    candidates: Vec<ItemKey>,
    unread: Vec<ItemKey>,
    negatives: Vec<ItemKey>,
}

/// `q` asked alone, in a fresh index over `source`, as the check asks it (`make_once`): its
/// answer and how many (crate, bucket) it read.
fn alone<B: BucketSource>(source: &B, records: &'static Records<'static>, keys: &mut Keys, store: &mut Store<'static>, q: Ask) -> (Answer, u32) {
    let mut b = Buckets::lazy(records);
    prepare_home(records, keys, store, q).unwrap_or_else(|e| panic!("{q:?}: prepare coherence: {e}"));
    let home = match q {
        Ask::Trait(t, shape) => trait_home(records, keys, store, t, shape).unwrap_or_else(|e| panic!("{q:?}: trait home: {e}")),
        Ask::Inherent(shape) => inherent_home(records, keys, store, shape).unwrap_or_else(|e| panic!("{q:?}: inherent home: {e}")),
    };
    source.admit(&mut b, records, keys, home, q, store.tys.alloc()).unwrap_or_else(|e| panic!("{q:?}: {e}"));
    let (mut candidates, mut negatives) = match q {
        Ask::Trait(t, shape) => (b.trait_candidates(t, shape, home).to_vec(), b.negative_of(t, shape, home).to_vec()),
        Ask::Inherent(shape) => (b.inherent_candidates(shape, home).to_vec(), Vec::new()),
    };
    candidates.sort_unstable();
    negatives.sort_unstable();
    let unread = candidates.iter().copied().filter(|k| b.is_unread(*k)).collect();
    (Answer { candidates, unread, negatives }, b.bucket_lookups())
}

#[test]
fn a_real_check_s_questions_answer_alike_over_the_impl_tables() {
    // Each line printed is kept too, and written to `front2_tables_core.out` in the temp
    // directory: the build queue shows a passing test's output to no one.
    let mut said = String::new();
    macro_rules! say {
        ($($t:tt)*) => {{
            let line = format!($($t)*);
            println!("{line}");
            said.push_str(&line);
            said.push('\n');
        }};
    }
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/tables/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "tables", dir: "/tables/", root: "/tables/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let core = crate_names.iter().position(|n| *n == "core").expect("a crate named core");
    let lang_crates = [0, core];
    let g = Alloc::global();

    // The solve, the records written from it, the root's fns. Nothing solved leaves the block.
    let start = Instant::now();
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
    say!("SOLVED {} crates {crate_names:?}, {} fns of the root, records written in {:?}", crate_names.len(), fns.len(), start.elapsed());
    assert!(fns.len() >= 10, "the root has its fns: {}", fns.len());

    // Over the records alone.
    // Keep the synthetic store frame alive for the `'static` records used by this test.
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, g)));
    let load: &'static Kept = Box::leak(Box::new(kept));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(keys, load, log, frame, g)));
    let mut front = Front2::from_records(records, frame, &lang_crates).unwrap_or_else(|e| panic!("front2 over records: {e}"));

    // 1. The real check: every fn of the root.
    let start = Instant::now();
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store: Store<'static> = Store::new(front.index.lang("sized"), frame, g);
    let mut buckets: Option<Buckets> = None;
    let mut consts = Consts::default();
    let mut clean = 0usize;
    for key in fns.iter() {
        let body = match lower_fn(records, prepared, &mut front.keys, &mut store, *key) {
            Ok(body) => body,
            Err(e) => {
                say!("NOT LOWERED {key:x}: {e}");
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("{key:x} not spelled: {e:?}"));
        let scope = body.scope(&spell);
        let mut r = check::Records { names: records, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        match check_body(&mut r, &body, &scope) {
            Ok(_) => clean += 1,
            Err(e) => say!("NOT CLEAN {key:x}: {}", format!("{e:?}").chars().take(300).collect::<String>()),
        }
    }
    let asked = buckets.as_ref().map(|b| b.asked()).unwrap_or_default();
    say!("CHECKED {} bodies ({clean} clean) in {} ms; {} questions asked", fns.len(), start.elapsed().as_millis(), asked.len());
    assert!(asked.len() >= 20, "the check asked {} impl questions", asked.len());
    // 2. Every crate's impl tables.
    let mut tables = TableSet::new(g);
    let mut crate_keys: Vec<CrateItemKey> = Vec::new();
    let (mut task_reads, mut task_wholes) = (0usize, 0usize);
    for krate in 0..crate_names.len() {
        let start = Instant::now();
        let mut rows = Vec::new();
        NamesSource::each_impl_row(records, krate, &mut |r| rows.push(r));
        let written = write_impl_tables(records, krate, &rows, g).unwrap_or_else(|e| panic!("`{}`'s impl tables: {e}", crate_names[krate]));
        assert!(written.records.refused.is_empty(), "`{}`'s impl tables refused rows: {:?}", crate_names[krate], written.records.refused);
        let took = start.elapsed();
        let bytes: usize = written.records.buckets.iter().map(|b| b.bytes.len()).sum::<usize>() + written.records.directory.len() + written.records.no_impls_of.len();
        let largest = written.records.buckets.iter().max_by_key(|b| b.bytes.len());
        say!(
            "TABLES {}: {} rows, {} buckets, {bytes} bytes (directory {}, NoImplsOf {}; largest bucket {} bytes: {:?}), {} cells, {} from rows, {} from text, {} us",
            crate_names[krate],
            rows.len(),
            written.records.buckets.len(),
            written.records.directory.len(),
            written.records.no_impls_of.len(),
            largest.map_or(0, |b| b.bytes.len()),
            largest.map(|b| b.key),
            written.defines.len(),
            written.counts.from_rows,
            written.counts.from_text,
            took.as_micros()
        );
        // THE TASK: the same tables through the read-only view, with what the filing read.
        let done = impl_tables_task(records, krate, &rows, g);
        assert_eq!(done.failed, None, "`{}`'s impl tables task failed", crate_names[krate]);
        let task_records = done.records.as_ref().expect("a task that did not fail has its records");
        assert_eq!(task_records.directory, written.records.directory, "`{}`: the task's directory differs from the writer's", crate_names[krate]);
        assert_eq!(task_records.no_impls_of, written.records.no_impls_of, "`{}`: the task's NoImplsOf differs from the writer's", crate_names[krate]);
        assert_eq!(done.key.0, written.key, "`{}`: the task's key is not the tables' key", crate_names[krate]);
        assert_eq!(done.key.0, rows_key(written.krate, &rows), "`{}`: `rows_key` is not the tables' key", crate_names[krate]);
        assert_eq!(done.name, impl_tables_name(written.krate));
        assert!(done.defines.windows(2).all(|w| w[0].0 < w[1].0), "`{}`: the task's cells are not sorted and once", crate_names[krate]);
        assert!(done.reads.windows(2).all(|w| w[0] < w[1]), "`{}`: the task's reads are not sorted and once", crate_names[krate]);
        let (mut items, mut wholes, mut other_crates, mut other) = (0usize, 0usize, 0usize, 0usize);
        let own = {
            let mut p = [0u8; 8];
            p.copy_from_slice(&written.krate.0.0[..8]);
            p
        };
        for read in done.reads.iter() {
            match read {
                TableRead::Names(NameCell::Whole { .. }) => wholes += 1,
                TableRead::Names(cell) if cell.crate_prefix() != own => other_crates += 1,
                TableRead::Names(NameCell::Item { .. }) => items += 1,
                _ => other += 1,
            }
        }
        if krate == 0 || crate_names[krate] == "core" || wholes > 0 {
            let shown: Vec<String> = done.reads.iter().filter(|r| matches!(r, TableRead::Names(NameCell::Whole { .. }))).take(4).map(|r| format!("{r:?}")).collect();
            say!("TASK {}: {} reads (own items rows {items}, other crates' cells {other_crates}, whole sections {wholes}, other {other}) {shown:?}", crate_names[krate], done.reads.len());
        }
        task_reads += done.reads.len();
        task_wholes += wholes;
        crate_keys.push(written.krate);
        tables.put(written);
    }
    say!("TASKS: {} crates, {task_reads} reads in all, {task_wholes} of a whole section", crate_names.len());

    // A table row of `impl_`, for the negatives' shapes.
    let row_of = |impl_: ItemKey| -> Option<blaze_surface::ImplRow> {
        for c in crate_keys.iter() {
            let Some(t) = tables.of(*c) else { continue };
            for bucket in t.records.buckets.iter() {
                let view = blaze_surface::BucketView::read(&bucket.bytes).unwrap_or_else(|e| panic!("a bucket of {c:x}: {e:?}"));
                if let Some(row) = view.find(impl_) {
                    return Some(row.unwrap_or_else(|e| panic!("the row of {impl_:x}: {e:?}")));
                }
            }
        }
        None
    };

    // 3. Each question alone over both sources.
    let (mut over_tables, mut over_index, mut most, mut most_index) = (0u64, 0u64, 0u32, 0u32);
    let mut candidates = 0usize;
    for q in asked.iter() {
        let (index, index_reads) = alone(&ItemsIndex, records, &mut front.keys, &mut store, *q);
        let (t, reads) = alone(&tables, records, &mut front.keys, &mut store, *q);
        assert_eq!(t.candidates, index.candidates, "{q:?}: the tables' candidates differ from the index's");
        assert_eq!(t.unread, index.unread, "{q:?}: the tables' unread candidates differ from the index's");
        let shape = match q {
            Ask::Trait(_, s) | Ask::Inherent(s) => *s,
        };
        let fitting: Vec<ItemKey> = index.negatives.iter().copied().filter(|k| row_of(*k).is_some_and(|row| row_answers(row.shape, shape))).collect();
        assert_eq!(t.negatives, fitting, "{q:?}: the tables' negatives are not the index's at the question's shape");
        candidates += t.candidates.len();
        over_tables += u64::from(reads);
        over_index += u64::from(index_reads);
        most = most.max(reads);
        most_index = most_index.max(index_reads);
    }
    let n = asked.len().max(1) as u64;
    say!("EQUAL: {} questions, {candidates} candidates in all, equal over both sources", asked.len());
    say!(
        "PROBES tables: {over_tables} bucket reads, {most} at most for one question, {:.1} per question; index: {over_index} bucket lookups, {most_index} at most, {:.1} per question",
        over_tables as f64 / n as f64,
        over_index as f64 / n as f64
    );
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    say!("RECORD ERRORS: {errors:?}");
    let out = std::env::temp_dir().join("front2_tables_core.out");
    std::fs::write(&out, said).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
}
