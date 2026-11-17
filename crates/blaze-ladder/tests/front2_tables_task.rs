//! The body task over the read-only view (front2 tables/, engine.md E2): each body of a small
//! no_core program checked by `check_body` over `Tables`, against today's path
//! (`ProjectWorld::from_records`, one world for every body), in two orders, on 1 and 8
//! workers, and with a planted bug in the recorder that an edit must catch.
//!
//! The program: a trait with two impls, a generic fn, a closure called in place, an `impl Fn`
//! return, and two type errors (E0308).

#![feature(allocator_api)]

use std::collections::HashMap;

use blaze_drive::key::{ABSENT, Key, Kind, Name};
use blaze_engine::{CellId, Made, Phase, Planned, SVec, Stage, Task};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::buckets::{TableSet, write_impl_tables};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::source::{Load, Log, NamesSource, Records};
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts};
use blaze_ladder::front2::tables::task::{BodyFrame, BodyOut, Facts, Make, MakeRefused, check_body, verdict_bytes};
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_own::Alloc;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::names::Written;
use blaze_scope::names_cells::NameCell;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const CORE: &str = "#![no_core]
#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
pub mod marker {
    #[lang = \"pointee_sized\"]
    pub trait PointeeSized {}
    #[lang = \"meta_sized\"]
    pub trait MetaSized: PointeeSized {}
    #[lang = \"sized\"]
    pub trait Sized: MetaSized {}
    #[lang = \"copy\"]
    pub trait Copy: crate::clone::Clone {}
    #[lang = \"tuple_trait\"]
    pub trait Tuple {}
    impl Copy for u32 {}
}
pub mod clone {
    #[lang = \"clone\"]
    pub trait Clone: crate::marker::Sized {
        fn clone(&self) -> Self;
    }
    impl Clone for u32 {
        fn clone(&self) -> u32 {
            *self
        }
    }
}
pub mod ops {
    #[lang = \"drop\"]
    pub trait Drop {
        fn drop(&mut self);
    }
    #[lang = \"fn_once\"]
    #[rustc_paren_sugar]
    pub trait FnOnce<Args: crate::marker::Tuple> {
        #[lang = \"fn_once_output\"]
        type Output;
        extern \"rust-call\" fn call_once(self, args: Args) -> Self::Output;
    }
    #[lang = \"fn_mut\"]
    #[rustc_paren_sugar]
    pub trait FnMut<Args: crate::marker::Tuple>: FnOnce<Args> {
        extern \"rust-call\" fn call_mut(&mut self, args: Args) -> Self::Output;
    }
    #[lang = \"fn\"]
    #[rustc_paren_sugar]
    pub trait Fn<Args: crate::marker::Tuple>: FnMut<Args> {
        extern \"rust-call\" fn call(&self, args: Args) -> Self::Output;
    }
}
pub mod prelude {
    pub mod v1 {
        pub use crate::clone::Clone;
        pub use crate::marker::{Copy, Sized};
        pub use crate::ops::{Fn, FnMut, FnOnce};
    }
    pub mod rust_2015 {
        pub use super::v1::*;
    }
    pub mod rust_2018 {
        pub use super::v1::*;
    }
    pub mod rust_2021 {
        pub use super::v1::*;
    }
    pub mod rust_2024 {
        pub use super::v1::*;
    }
}
";

const APP: &str = "#![no_std]
pub trait Shape {
    fn sides(&self) -> u32;
}
pub struct Tri;
pub struct Sq;
impl Shape for Tri {
    fn sides(&self) -> u32 {
        3
    }
}
impl Shape for Sq {
    fn sides(&self) -> u32 {
        4
    }
}
pub fn sides_of<T: Shape>(x: &T) -> u32 {
    x.sides()
}
pub fn pick<T: Clone>(x: &T) -> T {
    x.clone()
}
pub fn tri_sides() -> u32 {
    sides_of(&Tri)
}
pub fn call_in_place() -> u32 {
    let f = |x: u32| x;
    f(3)
}
pub fn maker() -> impl Fn(u32) -> u32 {
    |x: u32| x
}
pub fn wrong_one() -> u32 {
    Tri
}
pub fn wrong_two() -> Sq {
    5u32
}
";

const APP_AT: usize = 0;
const CORE_AT: usize = 1;
const LANG: &[usize] = &[APP_AT, CORE_AT];

struct Texts;

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (APP_AT, "/app/lib.rs") => Some(APP),
            (CORE_AT, "/core/lib.rs") => Some(CORE),
            _ => None,
        }
    }
}

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-ladder tables task crate", name.as_bytes())),
        edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

/// Every crate's records as bytes: what workers share (immutable, `Sync`).
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<ItemKey, &'static str>>,
    keys: Vec<CrateItemKey>,
    crate_names: Vec<&'static str>,
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

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<&'static str> {
        self.expansions.get(krate)?.get(&call).copied()
    }

    fn expansion_origins(&self, _krate: usize, _call: ItemKey) -> Option<&'static [u8]> {
        None
    }
}

fn kept() -> &'static Kept {
    let crates = [input("app", "/app/lib.rs", &[("core", CORE_AT)]), input("core", "/core/lib.rs", &[])];
    let sysroot = Sysroot { core: Some(CrateId(CORE_AT as u32)), alloc: None, std: None, bstd: None };
    let names: &'static Names<'static> = Box::leak(Box::new(resolve_crates(&crates, sysroot, &Texts, &Serial, a()).unwrap_or_else(|e| panic!("resolve_crates: {e}"))));
    let g = a();
    let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new(), keys: Vec::new(), crate_names: Vec::new() };
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), g));
        kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), g));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), f.text)).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, &*Box::leak(m.text.to_string().into_boxed_str())))).collect());
        kept.keys.push(c.key);
        kept.crate_names.push(c.name);
    }
    Box::leak(Box::new(kept))
}

/// One worker's reader over the shared bytes (`Records` opens lazily: never shared).
fn reader(kept: &'static Kept) -> &'static Records<'static> {
    let g = a();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(Vec::leak(kept.crate_names.clone()), g)));
    Box::leak(Box::new(Records::new(Vec::leak(kept.keys.clone()), kept, log, frame, g)))
}

/// Every crate's impl tables (lane BK's E3), written from the reader's rows.
fn impl_tables(records: &'static Records<'static>) -> &'static TableSet<'static> {
    let g = a();
    let mut tables = TableSet::new(g);
    for krate in [APP_AT, CORE_AT] {
        let mut rows = Vec::new();
        assert!(NamesSource::each_impl_row(records, krate, &mut |r| rows.push(r)), "crate {krate} has impl rows");
        tables.put(write_impl_tables(records, krate, &rows, g).unwrap_or_else(|e| panic!("crate {krate}'s impl tables: {e}")));
    }
    Box::leak(Box::new(tables))
}

/// The program's bodies, by their item keys (a member by its own key), in key order.
fn bodies(records: &'static Records<'static>) -> Vec<(ItemKey, BodyRef<'static>)> {
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the listing's frame")));
    let world = ProjectWorld::from_records(records, frame, LANG, a()).unwrap_or_else(|e| panic!("the world did not start: {e}"));
    let mut out = Vec::new();
    for (body, _) in world.bodies(a()).iter() {
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
        out.push((key, *body));
    }
    out.sort_by_key(|(k, _)| *k);
    out
}

fn task_of(key: ItemKey) -> Task {
    let planned = Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) };
    Task { planned, key: Key(ABSENT) }
}

/// The test's `Make`: every cell's digest from a map, made on first ask; `edited` holds the
/// cells an edit moved.
struct MapMake {
    edited: Vec<CellId>,
}

impl<'a> Make<'a> for MapMake {
    fn make(&self, want: &CellId, _made: &mut SVec<'a, Made<'a>>) -> Result<ContentId, MakeRefused> {
        let mut text = format!("{want:?}");
        if self.edited.contains(want) {
            text.push_str(" edited");
        }
        Ok(ContentId::of("blaze-ladder tables task test cell", text.as_bytes()))
    }
}

/// One body task over `records`.
fn run(records: &'static Records<'static>, impls: Option<&'static TableSet<'static>>, key: ItemKey, frame: &BodyFrame<'static>, make: &MapMake) -> BodyOut<'static> {
    let g = a();
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let tables = Tables::new(TablesOpts { records, impls, lang_crates: LANG, reads: &unused });
    check_body(tables, &task_of(key), frame, make, blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("the body task of {key:x}: {e}"))
}

fn body_frame(plant: Option<NameCell>) -> BodyFrame<'static> {
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the body's frame")));
    BodyFrame { frame, alloc: a(), plant_drop: plant, cost: None, needs: blaze_ladder::front2::heads::NeedsMet::All }
}

/// What two runs are compared by: each body's whole result, by key.
fn shown(outs: &[(ItemKey, BodyOut<'static>)]) -> Vec<String> {
    let mut lines: Vec<String> = outs.iter().map(|(k, o)| format!("{k:x} {:?} {}", o.done, String::from_utf8_lossy(&o.bytes))).collect();
    lines.sort();
    lines
}

/// TODAY'S PATH vs THE TASK. Every body's verdict over `Tables` equals today's (one world
/// over `&Records` for all bodies), byte for byte, its facts' digest included, over the same
/// impl index; over the impl tables the answers are equal too (their facts read the tables'
/// buckets, so the digest is left out of that comparison). Each task gives one result.
#[test]
fn the_task_answers_as_todays_path() {
    let kept = kept();
    let records = reader(kept);
    let list = bodies(records);
    assert!(list.len() >= 8, "the program's bodies: {list:?}");
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("today's frame")));
    let mut today = ProjectWorld::from_records(records, frame, LANG, a()).unwrap_or_else(|e| panic!("the world did not start: {e}"));
    let tables = impl_tables(reader(kept));
    let make = MapMake { edited: Vec::new() };
    let mut e0308 = 0;
    let mut entries = 0usize;
    for (key, body) in list.iter() {
        let (verdict, borrows) = today.check_borrows(*body, a());
        let want = verdict_bytes(&verdict, &borrows, Facts::Digest, a()).expect("today's bytes");
        let want_left = verdict_bytes(&verdict, &borrows, Facts::Left, a()).expect("today's bytes");
        let got = run(records, None, *key, &body_frame(None), &make);
        entries += 1;
        let label = NamesSource::name_of(records, *key).unwrap_or("?");
        println!("BODY {label}: {} reads, failed {}, findings {}\n{}", got.done.reads.len(), got.done.failed, got.done.findings, String::from_utf8_lossy(&got.bytes));
        assert_eq!(String::from_utf8_lossy(&got.bytes), String::from_utf8_lossy(&want), "the body `{label}` answers otherwise over the view");
        if verdict.diagnostics.iter().any(|d| d.code == "E0308") {
            e0308 += 1;
        }
        let over_tables = run(records, Some(tables), *key, &body_frame(None), &make);
        let got_left = without_reads(&over_tables.bytes);
        assert_eq!(got_left, String::from_utf8_lossy(&want_left), "the body `{label}` answers otherwise over the impl tables");
    }
    assert_eq!(e0308, 2, "the two type errors are E0308");
    assert_eq!(entries, list.len(), "entries per result: 1");
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "the records filed {errors:?}");
}

/// A verdict's bytes (`Facts::Digest`) without the facts' digest: its last line.
fn without_reads(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let cut = text.rfind("reads ").unwrap_or_else(|| panic!("no facts digest in {text}"));
    text[..cut].to_string()
}

/// FORWARD vs REVERSED, 1 vs 8 WORKERS: every body's result (its `Done`: out, reads with
/// digests, defines, findings, failed; and its verdict's bytes) is equal bytes whatever the
/// order and the number of workers, each worker with its own reader over the shared bytes.
#[test]
fn order_and_workers_give_equal_bytes() {
    let kept = kept();
    let records = reader(kept);
    let tables = impl_tables(records);
    let list = bodies(records);
    let make = MapMake { edited: Vec::new() };
    let forward: Vec<(ItemKey, BodyOut<'static>)> = list.iter().map(|(k, _)| (*k, run(records, Some(tables), *k, &body_frame(None), &make))).collect();
    let reversed: Vec<(ItemKey, BodyOut<'static>)> = list.iter().rev().map(|(k, _)| (*k, run(records, Some(tables), *k, &body_frame(None), &make))).collect();
    assert_eq!(shown(&forward), shown(&reversed), "reversed order answers otherwise");
    let keys: Vec<ItemKey> = list.iter().map(|(k, _)| *k).collect();
    // Each worker gives its results as lines (a result lives in its worker's allocator).
    let mut eight: Vec<String> = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|w| {
                let keys = &keys;
                s.spawn(move || {
                    let records = reader(kept);
                    let tables = impl_tables(records);
                    let make = MapMake { edited: Vec::new() };
                    let mine: Vec<(ItemKey, BodyOut<'static>)> = keys.iter().enumerate().filter(|(i, _)| i % 8 == w).map(|(_, k)| (*k, run(records, Some(tables), *k, &body_frame(None), &make))).collect();
                    shown(&mine)
                })
            })
            .collect();
        for h in handles {
            eight.extend(h.join().expect("a worker panicked"));
        }
    });
    eight.sort();
    assert_eq!(eight.len(), keys.len(), "entries per result: 1");
    assert_eq!(shown(&forward), eight, "8 workers answer otherwise than 1");
}

/// THE READ LIST IS WHAT MOVES THE ANSWER. `wrong_one` returns `Tri`: its answer is a
/// function of `Tri`'s item cell. An edit that moves that cell dirties the body (a read of it
/// saw another digest). PLANTED BUG: a recorder that drops that one read leaves the body
/// standing under the same edit; the check catches it.
#[test]
fn an_edit_dirties_the_body_and_a_dropped_read_is_caught() {
    let kept = kept();
    let records = reader(kept);
    let tables = impl_tables(records);
    let segments = ["Tri"];
    let tri = NamesSource::walk(records,NamesSource::root_module(records, APP_AT).expect("app's root"), Namespace::Types, Written { kind: PathKind::Plain, segments: &segments[..] }).unwrap_or_else(|e| panic!("`Tri`: {e:?}"));
    let tri = tri.whole(1).expect("`Tri` names an item").key;
    let segments = ["wrong_one"];
    let body = NamesSource::walk(records, NamesSource::root_module(records, APP_AT).expect("app's root"), Namespace::Values, Written { kind: PathKind::Plain, segments: &segments[..] }).unwrap_or_else(|e| panic!("`wrong_one`: {e:?}"));
    let body = body.whole(1).expect("`wrong_one` names a fn").key;
    let cell = NameCell::Item { key: tri };

    let before = run(records, Some(tables), body, &body_frame(None), &MapMake { edited: Vec::new() });
    let read = before.done.reads.iter().find(|(id, _)| id.cell.0 == cell.key()).map(|(id, _)| *id);
    let read = read.unwrap_or_else(|| panic!("`wrong_one` did not read `Tri`'s cell; its reads: {:?}", before.reads));
    let edit = MapMake { edited: vec![read] };
    assert!(dirtied(&before, &edit), "an edit of `Tri`'s cell leaves `wrong_one` standing");

    let planted = run(records, Some(tables), body, &body_frame(Some(cell)), &MapMake { edited: Vec::new() });
    assert!(!dirtied(&planted, &edit), "CONTROL: the planted dropped read must leave the body standing under the edit");
    assert_eq!(planted.bytes.as_slice(), before.bytes.as_slice(), "the planted bug changes only the read list");
}

/// Whether a result's row is not today's under `edit`: a read whose digest moved.
fn dirtied(out: &BodyOut<'_>, edit: &MapMake) -> bool {
    let mut made = SVec::new_in(blaze_own::testing::heap().shared());
    out.done.reads.iter().any(|(id, saw)| edit.make(id, &mut made).expect("a digest") != *saw)
}
