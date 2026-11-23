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
use blaze_ladder::front2::buckets::{ImplShelf, ImplShelfOpts, ImplTables, ShelfError, ShelfSource, ShelvedTwice, TableSet, check_framed, framed, impl_tables_name, impl_tables_task, write_impl_tables};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
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
pub enum Light {
    Red,
    Green(u32),
}
pub fn go() -> Light {
    Light::Green(1)
}
pub fn longer<'a, 'b: 'a>(x: &'a u8, _y: &'b u8) -> &'a u8 {
    x
}
pub fn pick_longer(x: &u8) -> &u8 {
    longer(x, x)
}
";

const APP_AT: usize = 0;
const CORE_AT: usize = 1;
const LANG: &[usize] = &[APP_AT, CORE_AT];

struct Texts(&'static str);

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (APP_AT, "/app/lib.rs") => Some(self.0),
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

    /// The texts, with no origins kept.
    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        Some(LoadedExpansion { text, origins: None })
    }
}

fn kept() -> &'static Kept {
    kept_with_app(APP)
}

fn kept_with_app(app: &'static str) -> &'static Kept {
    let crates = [input("app", "/app/lib.rs", &[("core", CORE_AT)]), input("core", "/core/lib.rs", &[])];
    let sysroot = Sysroot { core: Some(CrateId(CORE_AT as u32)), alloc: None, std: None, bstd: None };
    let names: &'static Names<'static> = Box::leak(Box::new(resolve_crates(&crates, sysroot, &Texts(app), &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"))));
    let g = a();
    let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new(), keys: Vec::new(), crate_names: Vec::new() };
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
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
fn run(records: &'static Records<'static>, impls: Option<&'static TableSet<'static>>, key: ItemKey, frame: &BodyFrame<'static>, make: &dyn Make<'static>) -> BodyOut<'static> {
    let g = a();
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let tables = Tables::new(TablesOpts { records, impls, lang_crates: LANG, reads: &unused });
    check_body(tables, &task_of(key), frame, make, blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("the body task of {key:x}: {e}"))
}

fn body_frame(plant: Option<NameCell>) -> BodyFrame<'static> {
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the body's frame")));
    BodyFrame { frame, alloc: a(), plant_drop: plant, cost: None, needs: blaze_ladder::front2::heads::NeedsMet::All, heads: None, sources: None, goals: None, prepared: None }
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

/// ONE SHELF, MANY RUNS (engine.md E6, the Heads stage): each crate's tables made ONCE by the
/// Heads task (`impl_tables_task`), framed as the store holds them and put on one `ImplShelf`;
/// 8 runs on 8 threads, each with its own reader and its own `TableSet::lent` over that one
/// shelf, give every body the same result, reads included, as one reader's own tables. A crate
/// put twice is refused by name.
#[test]
fn several_runs_read_one_shelf() {
    let kept = kept();
    let records = reader(kept);
    let list = bodies(records);
    let make = MapMake { edited: Vec::new() };
    let own = impl_tables(records);
    let want: Vec<(ItemKey, BodyOut<'static>)> = list.iter().map(|(k, _)| (*k, run(records, Some(own), *k, &body_frame(None), &make))).collect();
    let mut shelf = ImplShelf::new(ImplShelfOpts {});
    for krate in [APP_AT, CORE_AT] {
        let mut rows = Vec::new();
        assert!(NamesSource::each_impl_row(records, krate, &mut |r| rows.push(r)), "crate {krate} has impl rows");
        let done = impl_tables_task(records, krate, &rows, a());
        assert!(done.failed.is_none(), "crate {krate}'s tables task failed: {:?}", done.failed);
        let key = NamesSource::crate_key(records, krate).expect("the crate's key");
        assert_eq!(done.name, impl_tables_name(key), "the task's name is its crate's");
        let tables = done.records.expect("the task's records");
        shelf.put_framed(key, &framed(&tables, std::alloc::Global)).unwrap_or_else(|e| panic!("crate {krate}'s framed tables: {e}"));
        assert!(shelf.holds(key));
        assert_eq!(shelf.put(key, &tables), Err(ShelvedTwice { krate: key }), "a crate put twice is refused");
    }
    assert_eq!(shelf.len(), 2);
    // The form's magic (one aligned word), then a length word cut short.
    assert!(matches!(shelf.put_framed(CrateItemKey(ContentId([7; 32])), b"bzimpl2\0\0\0\0\0\0\0\0\0\x05"), Err(ShelfError::Malformed { .. })), "a cut payload is refused");
    // The form before the parts were aligned (`bzimpl1`) is another form, refused by its magic.
    assert!(matches!(shelf.put_framed(CrateItemKey(ContentId([7; 32])), b"bzimpl1\0\0\0\0\0\0\0\0\0"), Err(ShelfError::Malformed { at: 0, .. })), "a form-1 payload is refused by name");
    let shelf: &'static ImplShelf = Box::leak(Box::new(shelf));
    let keys: Vec<ItemKey> = list.iter().map(|(k, _)| *k).collect();
    let mut eight: Vec<String> = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|w| {
                let keys = &keys;
                s.spawn(move || {
                    let records = reader(kept);
                    let lent: &'static TableSet<'static> = Box::leak(Box::new(TableSet::lent(shelf, a())));
                    let make = MapMake { edited: Vec::new() };
                    let mine: Vec<(ItemKey, BodyOut<'static>)> = keys.iter().enumerate().filter(|(i, _)| i % 8 == w).map(|(_, k)| (*k, run(records, Some(lent), *k, &body_frame(None), &make))).collect();
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
    assert_eq!(shown(&want), eight, "runs over one shelf answer otherwise than one reader's own tables");
}

/// The test's [`ShelfSource`]: each crate's framed tables by key, at an address aligned as the
/// store aligns a payload (blaze-drive record.rs `PAYLOAD_ALIGN`), and how many asks it had.
struct Framed {
    by_key: Vec<(CrateItemKey, &'static [u8])>,
    asks: std::sync::atomic::AtomicUsize,
}

impl ShelfSource for Framed {
    fn framed(&self, krate: CrateItemKey) -> Option<&[u8]> {
        self.asks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.by_key.iter().find(|(k, _)| *k == krate).map(|(_, b)| *b)
    }
}

/// THE PAGED SHELF IS THE WHOLE ONE (rule 78: one seam, two sources). Every body checked over a
/// `TableSet::paged` (an empty shelf and a source holding each crate's framed tables; then a
/// shelf holding one crate and the source the other) gives the result it gives over a
/// `TableSet::lent` whose shelf holds every crate: the verdict's bytes and the `Done`, its read
/// list included. Each reader asks the source at most once a crate, and none of a crate the
/// shelf holds. CONTROL (rule 23): a source that holds nothing moves some body's result, so the
/// comparison sees the tables; a lookup of a crate it lacks is `NoTables`.
#[test]
fn a_paged_shelf_answers_as_the_whole_one() {
    use blaze_impls::ImplRecords;
    let kept = kept();
    let records = reader(kept);
    let list = bodies(records);
    let make = MapMake { edited: Vec::new() };
    let mut whole = ImplShelf::new(ImplShelfOpts {});
    let mut half = ImplShelf::new(ImplShelfOpts {});
    let mut by_key: Vec<(CrateItemKey, &'static [u8])> = Vec::new();
    for krate in [APP_AT, CORE_AT] {
        let mut rows = Vec::new();
        assert!(NamesSource::each_impl_row(records, krate, &mut |r| rows.push(r)), "crate {krate} has impl rows");
        let done = impl_tables_task(records, krate, &rows, a());
        let tables = done.records.expect("the task's records");
        let key = NamesSource::crate_key(records, krate).expect("the crate's key");
        let bytes: &'static [u8] = aligned(&framed(&tables, std::alloc::Global), blaze_own::testing::bump());
        check_framed(key, bytes).unwrap_or_else(|e| panic!("crate {krate}'s framed tables: {e}"));
        // THE CAUSE'S CONTROL (rule 23): the same bytes one byte off an aligned address are
        // refused by name, never handed on to be read in place (where every record's reader
        // refuses its address: blaze-format-core `header`, and the paged answers part).
        let mut shifted: Vec<u8> = vec![0; bytes.len() + 16];
        let off = (1..16).find(|k| !(shifted.as_ptr().addr() + k).is_multiple_of(16)).expect("an unaligned offset");
        shifted[off..off + bytes.len()].copy_from_slice(bytes);
        assert_eq!(check_framed(key, &shifted[off..off + bytes.len()]), Err(ShelfError::Unaligned { krate: key }), "crate {krate}'s tables off an aligned address are refused by their address");
        whole.put_framed(key, bytes).unwrap_or_else(|e| panic!("crate {krate}'s framed tables: {e}"));
        if krate == APP_AT {
            half.put_framed(key, bytes).unwrap_or_else(|e| panic!("crate {krate}'s framed tables: {e}"));
        }
        by_key.push((key, bytes));
    }
    assert!(matches!(check_framed(CrateItemKey(ContentId([7; 32])), b"bzimpl2\0\0\0\0\0\0\0\0\0\x05"), Err(ShelfError::Malformed { .. })), "a cut payload is refused");
    let crates = by_key.len();
    let whole: &'static ImplShelf = Box::leak(Box::new(whole));
    let half: &'static ImplShelf = Box::leak(Box::new(half));
    let empty: &'static ImplShelf = Box::leak(Box::new(ImplShelf::new(ImplShelfOpts {})));
    let source: &'static Framed = Box::leak(Box::new(Framed { by_key, asks: std::sync::atomic::AtomicUsize::new(0) }));
    let lent: &'static TableSet<'static> = Box::leak(Box::new(TableSet::lent(whole, a())));
    let want: Vec<(ItemKey, BodyOut<'static>)> = list.iter().map(|(k, _)| (*k, run(records, Some(lent), *k, &body_frame(None), &make))).collect();
    for (label, shelf, most) in [("an empty shelf", empty, crates), ("a shelf of one crate", half, crates - 1)] {
        source.asks.store(0, std::sync::atomic::Ordering::Relaxed);
        let paged: &'static TableSet<'static> = Box::leak(Box::new(TableSet::paged(shelf, source, a())));
        let got: Vec<(ItemKey, BodyOut<'static>)> = list.iter().map(|(k, _)| (*k, run(records, Some(paged), *k, &body_frame(None), &make))).collect();
        assert_eq!(shown(&got), shown(&want), "over {label} and the source, the bodies answer otherwise than over the whole shelf");
        let asks = source.asks.load(std::sync::atomic::Ordering::Relaxed);
        assert!(asks <= most, "over {label}: {asks} asks of the source, more than the {most} crate(s) it alone holds");
        // CONTROL: the paged path ran (an empty shelf answers nothing by itself).
        assert!(shelf.len() == crates || asks >= 1, "over {label}: the source was never asked, so the paged path did not run");
    }
    let nothing: &'static Framed = Box::leak(Box::new(Framed { by_key: Vec::new(), asks: std::sync::atomic::AtomicUsize::new(0) }));
    let bare: &'static TableSet<'static> = Box::leak(Box::new(TableSet::paged(empty, nothing, a())));
    let core_key = NamesSource::crate_key(records, CORE_AT).expect("the crate's key");
    assert!(bare.directory(core_key).is_err() && bare.no_impls_of(core_key).is_err(), "CONTROL: a crate no source holds has no tables");
    assert!(lent.directory(core_key).is_ok(), "the whole shelf holds it");
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

// Use the writer's real impl definitions in both revisions. Holding every other table
// fixed isolates whether the body's impl reads alone carry the change.
struct AppImpls(&'static ImplTables<'static>);

impl<'a> Make<'a> for AppImpls {
    fn make(&self, want: &CellId, _: &mut SVec<'a, Made<'a>>) -> Result<ContentId, MakeRefused> {
        if want.of != impl_tables_name(self.0.krate) {
            return Ok(ABSENT);
        }
        let defs = &self.0.defines;
        Ok(defs.binary_search_by_key(&want.cell.0, |(cell, _)| *cell).map_or(ABSENT, |at| defs[at].1))
    }
}

fn side_revision(source: &'static str) -> (AppImpls, CellId, BodyOut<'static>) {
    use blaze_drive::cells::Cell;
    use blaze_ladder::front2::buckets::bucket_cell;
    use blaze_surface::{BucketKey, SelfShape, Shape};

    let records = reader(kept_with_app(source));
    let tables = impl_tables(records);
    let root = NamesSource::root_module(records, APP_AT).expect("app's root");
    let item = |ns, name: &str| {
        let segments = [name];
        NamesSource::walk(records, root, ns, Written { kind: PathKind::Plain, segments: &segments })
            .expect("a fixture path").whole(1).expect("a fixture item").key
    };
    let app = tables.of(NamesSource::crate_key(records, APP_AT).expect("app's key")).expect("app's impls");
    let shape = SelfShape::new(Shape::Def(item(Namespace::Types, "Tri"))).expect("nominal self type");
    let bucket = BucketKey::Trait { trait_: item(Namespace::Types, "Shape"), shape };
    let read = CellId { of: impl_tables_name(app.krate), cell: Cell(bucket_cell(bucket.hash())) };
    let make = AppImpls(app);
    let body = run(records, Some(tables), item(Namespace::Values, "side"), &body_frame(None), &make);
    (make, read, body)
}

#[test]
fn adding_an_impl_to_a_consulted_empty_bucket_rechecks_the_body() {
    use blaze_ladder::front2::tables::task::SaidWhat;

    // Sq keeps the trait present on both revisions. Only the Shape-for-Tri
    // bucket changes from absent to present among the body's impl reads.
    const SOURCE: &str = "#![no_std]
pub trait Shape { fn sides(&self) -> u32; }
pub struct Tri;
pub struct Sq;
impl Shape for Sq { fn sides(&self) -> u32 { 4 } }
pub fn side(t: &Tri) -> u32 { t.sides() }
";
    let edited = Box::leak(format!("{SOURCE}impl Shape for Tri {{ fn sides(&self) -> u32 {{ 3 }} }}\n").into_boxed_str());

    let (old_defs, cell, mut before) = side_revision(SOURCE);
    let (new_defs, edited_cell, after) = side_revision(edited);
    assert_eq!(cell, edited_cell, "adding an impl preserves the identity of its bucket");
    assert!(before.said.iter().any(|s| matches!(s.what, SaidWhat::Type { code: "E0599" })), "Tri has no sides method: {:?}", before.said);
    assert!(after.said.is_empty(), "Tri implements Shape and its sides method returns u32: {:?}", after.said);
    assert!(before.done.reads.contains(&(cell, ABSENT)), "a missing impl is a read of its absent bucket");
    assert!(after.done.reads.iter().any(|(id, digest)| *id == cell && *digest != ABSENT), "the added impl defines the bucket");
    assert!(!dirtied(&before, &old_defs), "the original definitions validate the read list");
    assert!(dirtied(&before, &new_defs), "the new impl must invalidate the missing-method diagnostic");
    before.done.reads.retain(|(id, _)| *id != cell);
    assert!(!dirtied(&before, &new_defs), "CONTROL: dropping the negative read accepts the stale E0599");
    println!("adding Shape for Tri rechecks side: E0599 becomes clean; dropping the negative read leaves E0599 stale");
}

#[test]
fn the_first_unread_trait_impl_invalidates_a_rejected_trait_lookup() {
    use blaze_ladder::front2::buckets::TRAIT_HELD;
    use blaze_ladder::front2::tables::task::SaidWhat;
    use blaze_surface::NoImplsOfView;

    const SOURCE: &str = "#![no_std]
pub trait Shape { fn sides(&self) -> u32; }
pub struct Tri;
pub fn side(t: &Tri) -> u32 { t.sides() }
";
    let edited = Box::leak(format!("{SOURCE}impl gone::Gone for Tri {{}}\n").into_boxed_str());
    let (old_defs, exact, mut before) = side_revision(SOURCE);
    let (new_defs, edited_exact, after) = side_revision(edited);
    assert_eq!(before.done.name, after.done.name, "the edit preserves the body");
    assert_eq!(exact, edited_exact);
    assert_eq!(NoImplsOfView::read(&old_defs.0.records.no_impls_of).expect("original filter").unread(), (0, 0));
    assert_eq!(NoImplsOfView::read(&new_defs.0.records.no_impls_of).expect("edited filter").unread(), (1, 0));
    assert!(!before.done.failed && before.said.iter().any(|s| matches!(s.what, SaidWhat::Type { code: "E0599" })), "Tri has no sides method: {:?}", before.said);
    // The gap is said by its words (said.rs `gap_words`: the gap's `Display`, rule 19), never by
    // its `Debug` name: "(unread as written)" is what `Gap::UnreadImpl` alone prints
    // (world_items.rs), so this names the same gap the variant's name did.
    assert!(
        after.done.failed && after.said.iter().any(|s| matches!(s.what, SaidWhat::NotChecked { .. }) && s.words.contains("(unread as written)") && s.words.contains("`gone::Gone`")),
        "resolving sides depends on the unread impl for Tri: {:?}",
        after.said
    );
    assert!(!before.done.reads.iter().any(|(id, _)| *id == exact), "the rejected trait lookup stopped before its shaped bucket");
    assert!(after.done.reads.contains(&(exact, ABSENT)), "the filter now admits Shape but its named bucket is still empty");
    assert!(!dirtied(&before, &old_defs));
    assert!(dirtied(&before, &new_defs), "the first unread impl must invalidate the completed verdict");
    assert!(dirtied(&after, &old_defs), "removing the last unread impl must invalidate the incomplete verdict");
    // Discover the changed dependency from the two real read lists, without teaching the
    // fixture the writer's cell name. The old named-trait-only scheme fails above.
    let mut changed = before.done.reads.iter().filter(|(id, saw)| *saw == ABSENT && after.done.reads.contains(&(*id, TRAIT_HELD)));
    let &(guard, _) = changed.next().expect("the crate's missing unread-impl flag became present");
    assert_eq!(guard.of, exact.of);
    assert_eq!(changed.next(), None);
    before.done.reads.retain(|(id, _)| *id != guard);
    assert!(!dirtied(&before, &new_defs), "CONTROL: without the unread-impl cell, the stale E0599 stands");
    println!("first unread impl for Tri rechecks side: E0599 becomes an unread-impl gap; dropping the crate flag leaves E0599 stale");
}

/// A FACT'S CELL IS THE CELL THAT HOLDS IT, READ WHOLE. `go` calls one constructor of `Light`:
/// the constructor's facts are held by `Light`'s head cell (an edit of the variant list moves
/// that cell), so `go` reads it, part 0; `sides_of` calls a trait member, held by `Shape`'s
/// cell. Before (lane FI's find): the read named the constructor's and the member's own keys,
/// cells nothing writes, with the fact's kind as the part, so the edit left `go` standing.
#[test]
fn a_fact_reads_the_cell_that_holds_it() {
    use blaze_ladder::front2::keys::checker_of;
    use blaze_ladder::front2::tables::head::signature_name;
    let kept = kept();
    let records = reader(kept);
    let tables = impl_tables(records);
    let root = NamesSource::root_module(records, APP_AT).expect("app's root");
    let item = |ns: Namespace, name: &str| -> ItemKey {
        let segments = [name];
        let found = NamesSource::walk(records, root, ns, Written { kind: PathKind::Plain, segments: &segments[..] }).unwrap_or_else(|e| panic!("`{name}`: {e:?}"));
        found.whole(1).unwrap_or_else(|| panic!("`{name}` names an item")).key
    };
    // `pick_longer` calls `longer<'a, 'b: 'a>` and reads `longer`'s head cell (its frozen
    // meaning holds the lifetime bounds, lane FK's `a_lifetime_bound_alone_moves_the_head`),
    // so dropping only `'b: 'a` dirties it. (Lane VB's open find was the fixture's: it named
    // two fns `pick`, and the walk found the generic one, which calls `clone`, not `longer`.)
    for (body, holder, ns) in [("go", "Light", Namespace::Types), ("sides_of", "Shape", Namespace::Types), ("pick_longer", "longer", Namespace::Values)] {
        let out = run(records, Some(tables), item(Namespace::Values, body), &body_frame(None), &MapMake { edited: Vec::new() });
        let want = CellId { of: signature_name(checker_of(item(ns, holder)).0.to_be_bytes()), cell: blaze_drive::cells::Cell([0; 16]) };
        assert!(out.done.reads.iter().any(|(id, _)| *id == want), "`{body}` does not read `{holder}`'s head cell {want:?}; its head reads: {:?}", out.done.reads.iter().filter(|(id, _)| id.of.kind == Kind::Signature).collect::<Vec<_>>());
        assert!(dirtied(&out, &MapMake { edited: vec![want] }), "an edit of `{holder}`'s head leaves `{body}` standing");
    }
    // ONE PART: every head read of every body is its cell's part 0.
    for (key, _) in bodies(records) {
        let out = run(records, Some(tables), key, &body_frame(None), &MapMake { edited: Vec::new() });
        for (id, _) in out.done.reads.iter().filter(|(id, _)| id.of.kind == Kind::Signature) {
            assert_eq!(id.cell.0, [0; 16], "{key:x} reads part {:?} of {:?}: a head cell is read whole", id.cell, id.of);
        }
    }
}

/// THE RESULT SAYS WHAT `check` PRINTS: `wrong_one` returns `Tri` where `u32` is declared, a
/// type error by its code at an item-relative range, its words the verdict's; a clean body
/// says nothing.
#[test]
fn the_result_carries_the_verdicts_lines() {
    use blaze_ladder::front2::tables::task::SaidWhat;
    let kept = kept();
    let records = reader(kept);
    let tables = impl_tables(records);
    let root = NamesSource::root_module(records, APP_AT).expect("app's root");
    let item = |name: &str| -> ItemKey {
        let segments = [name];
        let found = NamesSource::walk(records, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &segments[..] }).unwrap_or_else(|e| panic!("`{name}`: {e:?}"));
        found.whole(1).unwrap_or_else(|| panic!("`{name}` names an item")).key
    };
    let wrong = run(records, Some(tables), item("wrong_one"), &body_frame(None), &MapMake { edited: Vec::new() });
    assert!(wrong.done.findings > 0, "`wrong_one` has findings");
    assert_eq!(wrong.said.len(), wrong.done.findings as usize, "one line a finding: {:?}", wrong.said);
    let first = wrong.said.first().expect("a line");
    assert!(matches!(first.what, SaidWhat::Type { .. }) && first.end > first.start && !first.words.is_empty(), "a type error with its range and words: {first:?}");
    let clean = run(records, Some(tables), item("go"), &body_frame(None), &MapMake { edited: Vec::new() });
    assert!(clean.said.is_empty(), "a clean body says nothing: {:?}", clean.said);
}

/// EVERY HEAD A BODY'S CHECK TOUCHES IS A READ (rule 21), for every body of the fixture: a
/// body's own store makes a head only when the check asks for it, so each head it holds after
/// the check (`BodyOut::held`, by the store's own keys) must have its cell in the read list.
/// A head answered without its fact recorded (a cache that answers a later asker, a lookup
/// beside the recorded question) is a held head with no read, and fails here by name.
#[test]
fn every_head_the_check_touches_is_read() {
    let kept = kept();
    let records = reader(kept);
    let tables = impl_tables(records);
    let mut held = 0usize;
    let mut missing: Vec<String> = Vec::new();
    for (key, body) in bodies(records) {
        let out = run(records, Some(tables), key, &body_frame(None), &MapMake { edited: Vec::new() });
        held += out.held.len();
        // The program's crate's heads (its checker keys share the body's first 8 bytes): a
        // `core` head the store holds is made as a NEED of another head (the `Sized` chain a
        // trait's head needs), which no check reads; whether a head's cell covers its needs is
        // the head's own read list (tables/head.rs), not the body's.
        let krate = u128::from(u64::from_be_bytes(key.0.0[..8].try_into().expect("8 bytes")));
        let lost: Vec<_> = out.held.iter().filter(|(h, k)| k >> 64 == krate && !out.reads.contains(h)).map(|(_, k)| format!("{k:032x}")).collect();
        if !lost.is_empty() {
            missing.push(format!("body {key:x} ({body:?}): held, not read {lost:?}; its verdict {}", String::from_utf8_lossy(&out.bytes)));
        }
    }
    assert!(held > 0, "CONTROL: the fixture's bodies hold heads (the instrument saw none)");
    assert!(missing.is_empty(), "{} bodies hold heads they do not read:\n{}", missing.len(), missing.join("\n"));
}

/// Whether a result's row is not today's under `edit`: a read whose digest moved.
fn dirtied(out: &BodyOut<'_>, edit: &dyn Make<'static>) -> bool {
    let mut made = SVec::new_in(blaze_own::testing::heap().shared());
    out.done.reads.iter().any(|(id, saw)| edit.make(id, &mut made).expect("a digest") != *saw)
}
