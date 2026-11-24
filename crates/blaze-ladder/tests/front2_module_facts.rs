//! A body names things THROUGH modules and writes constants as expressions: each is checked,
//! and its read list holds no head cell of a module (front2 by_origin.rs `FactHolder`, tables/
//! task.rs `fact_read`).
//!
//! THE FIND (`blaze engine-check` on the 1,100-crate workspace, g190500): 467 bodies refused
//! with "no head cell holds the body's fact of ItemKey(..): head cell of key ItemKey(..): no
//! want makes it: NoHead(.., Module)". The fact's key and the cell's key differ, which only
//! `fact_read`'s anonymous-constant branch made: an anonymous constant (an array length or a
//! repeat count written as an expression, front2/anon.rs) is keyed under its MODULE, and the
//! branch asked that module for a head cell. Every body whose own store filed one (from its
//! own text, or from a head it lowered) was refused. Each body below that writes such a
//! constant (`head_len` through `m::arr`'s head, `body_len` and `wrong_len` in their own
//! text) panicked in `run` before the fix.
//!
//! The program: `dep` with `pub mod m { f, S, arr }`; `dep2` re-exporting `m` as `shown`; the
//! member `app` naming `m` by `use`, by `use .. as`, through the re-export, by a full path, a
//! module of its own crate by `self::`, `super::` and `crate::`, and `m` inside a
//! `macro_rules!` expansion.
//!
//! NOT RUN by the writer (a write-only lane, uncompiled).

#![feature(allocator_api)]

use std::collections::HashMap;

use blaze_drive::key::{ABSENT, Key, Kind, Name};
use blaze_engine::{CellId, Made, Phase, Planned, SVec, Stage, Task};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::front2::tables::head::signature_name;
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts, TableRead};
use blaze_ladder::front2::tables::task::{BodyFrame, BodyOut, Make, MakeRefused, SaidWhat, check_body};
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_own::Alloc;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::names::Written;
use blaze_scope::names_cells::{Section, crate_table, seen_in};
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

/// The no_core `core` of front2_tables_task.rs, as written there.
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

/// The dependency: a module with a fn, a unit struct and a fn whose head's array length is an
/// anonymous constant of `m`.
const DEP: &str = "#![no_std]
pub mod m {
    pub fn f() -> u8 {
        7
    }
    pub struct S;
    pub fn arr() -> [u8; { 4 }] {
        [1, 2, 3, 4]
    }
}
";

/// [`DEP`] with `m::f`'s signature changed.
const DEP_F_EDITED: &str = "#![no_std]
pub mod m {
    pub fn f() -> u32 {
        7
    }
    pub struct S;
    pub fn arr() -> [u8; { 4 }] {
        [1, 2, 3, 4]
    }
}
";

/// [`DEP`] with an item no body names added at the end of `m`.
const DEP_UNRELATED: &str = "#![no_std]
pub mod m {
    pub fn f() -> u8 {
        7
    }
    pub struct S;
    pub fn arr() -> [u8; { 4 }] {
        [1, 2, 3, 4]
    }
    pub fn unrelated() -> u8 {
        9
    }
}
";

/// The second dependency: `m` re-exported under another name.
const DEP2: &str = "#![no_std]
pub use dep::m as shown;
";

const APP: &str = "#![no_std]
use dep::m;
use dep::m as n;
pub fn a() -> u8 {
    m::f()
}
pub fn b() -> u8 {
    n::f()
}
pub fn through_reexport() -> u8 {
    dep2::shown::f()
}
pub fn c() -> dep::m::S {
    dep::m::S
}
mod local {
    pub fn g() {}
    pub fn seven() -> u8 {
        7
    }
    pub mod deeper {
        pub fn up() -> u8 {
            super::seven()
        }
    }
}
pub fn d() {
    local::g()
}
pub fn via_self() -> u8 {
    self::local::seven()
}
pub fn via_crate() -> u8 {
    crate::local::deeper::up()
}
macro_rules! mk {
    () => {
        pub fn mac() -> u8 {
            m::f()
        }
    };
}
mk!();
pub fn head_len() -> u8 {
    let _x = m::arr();
    0
}
pub fn body_len() -> u8 {
    let _x: [u8; { 2 }] = [1, 2];
    0
}
pub fn wrong_len() -> u8 {
    let _x: [u8; { 3 }] = [1, 2];
    0
}
";

const APP_AT: usize = 0;
const DEP_AT: usize = 1;
const DEP2_AT: usize = 2;
const CORE_AT: usize = 3;
const LANG: &[usize] = &[APP_AT, CORE_AT];

/// Every clean body of `app` by its path from the crate root.
const CLEAN: &[&[&str]] = &[&["a"], &["b"], &["through_reexport"], &["c"], &["d"], &["via_self"], &["via_crate"], &["local", "deeper", "up"], &["mac"], &["head_len"], &["body_len"]];
/// The bodies that name `m::f` through a module: a signature change of `f` must enter each.
const CALLERS_OF_F: &[&str] = &["a", "b", "through_reexport", "mac"];

struct Texts {
    dep: &'static str,
}

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (APP_AT, "/app/lib.rs") => Some(APP),
            (DEP_AT, "/dep/lib.rs") => Some(self.dep),
            (DEP2_AT, "/dep2/lib.rs") => Some(DEP2),
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
        key: CrateItemKey(ContentId::of("blaze-ladder module facts crate", name.as_bytes())),
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

/// Every crate's records as bytes, and each crate's names cell table (blaze-scope
/// names_cells.rs `crate_table`, with the names and items sections as whole cells), by the
/// crate's first 8 key bytes: what an edit's moved cells are judged by.
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<ItemKey, &'static str>>,
    keys: Vec<CrateItemKey>,
    crate_names: Vec<&'static str>,
    cells: HashMap<[u8; 8], Vec<u8>>,
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

/// The four crates solved with `dep`'s text `dep`.
fn kept(dep: &'static str) -> &'static Kept {
    let crates = [
        input("app", "/app/lib.rs", &[("core", CORE_AT), ("dep", DEP_AT), ("dep2", DEP2_AT)]),
        input("dep", "/dep/lib.rs", &[("core", CORE_AT)]),
        input("dep2", "/dep2/lib.rs", &[("core", CORE_AT), ("dep", DEP_AT)]),
        input("core", "/core/lib.rs", &[]),
    ];
    let sysroot = Sysroot { core: Some(CrateId(CORE_AT as u32)), alloc: None, std: None, bstd: None };
    let g = a();
    let bump = blaze_own::testing::bump();
    let names: &'static Names<'static> = Box::leak(Box::new(resolve_crates(&crates, sysroot, &Texts { dep }, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"))));
    let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new(), keys: Vec::new(), crate_names: Vec::new(), cells: HashMap::new() };
    for c in names.crates().iter() {
        let names_bytes = aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), bump);
        let items_bytes = aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), bump);
        let wholes = [(Section::Names, names_bytes), (Section::Items, items_bytes)];
        let table = crate_table(c, names_bytes, items_bytes, &wholes, bump).unwrap_or_else(|e| panic!("the cell table of `{}`: {e:?}", c.name));
        let mut prefix = [0u8; 8];
        prefix.copy_from_slice(&c.key.0.0[..8]);
        kept.cells.insert(prefix, table.to_vec());
        kept.names.push(names_bytes);
        kept.items.push(items_bytes);
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), f.text)).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, &*Box::leak(m.text.to_string().into_boxed_str())))).collect());
        kept.keys.push(c.key);
        kept.crate_names.push(c.name);
    }
    Box::leak(Box::new(kept))
}

/// One worker's reader over the shared bytes.
fn reader(kept: &'static Kept) -> &'static Records<'static> {
    let g = a();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(Vec::leak(kept.crate_names.clone()), g)));
    Box::leak(Box::new(Records::new(Vec::leak(kept.keys.clone()), kept, log, frame, g)))
}

/// The item `path` names from crate `krate`'s root, in `ns` for its last segment.
fn item(records: &'static Records<'static>, krate: usize, ns: Namespace, path: &[&str]) -> ItemKey {
    let root = NamesSource::root_module(records, krate).unwrap_or_else(|| panic!("crate {krate}'s root"));
    let found = NamesSource::walk(records, root, ns, Written { kind: PathKind::Plain, segments: path }).unwrap_or_else(|e| panic!("`{}`: {e:?}", path.join("::")));
    found.whole(path.len()).unwrap_or_else(|| panic!("`{}` names an item", path.join("::"))).key
}

fn task_of(key: ItemKey) -> Task {
    let planned = Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) };
    Task { planned, key: Key(ABSENT) }
}

/// The test's `Make`: every cell's digest from its name, `edited` ones moved.
struct MapMake {
    edited: Vec<CellId>,
}

impl<'a> Make<'a> for MapMake {
    fn make(&self, want: &CellId, _made: &mut SVec<'a, Made<'a>>) -> Result<ContentId, MakeRefused> {
        let mut text = format!("{want:?}");
        if self.edited.contains(want) {
            text.push_str(" edited");
        }
        Ok(ContentId::of("blaze-ladder module facts test cell", text.as_bytes()))
    }
}

fn body_frame() -> BodyFrame<'static> {
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the body's frame")));
    BodyFrame { frame, alloc: a(), plant_drop: None, cost: None, needs: blaze_ladder::front2::heads::NeedsMet::All, heads: None, sources: None, goals: None, prepared: None }
}

/// One body task: a task error (the find's refusal) fails the test by the body's path.
fn run(records: &'static Records<'static>, path: &[&str]) -> BodyOut<'static> {
    let key = item(records, APP_AT, Namespace::Values, path);
    let g = a();
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: LANG, reads: &unused });
    check_body(tables, &task_of(key), &body_frame(), &MapMake { edited: Vec::new() }, blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("the body task of `{}`: {e}", path.join("::")))
}

/// Whether a result is not today's under `edit`: a read whose digest moved.
fn dirtied(out: &BodyOut<'_>, edit: &MapMake) -> bool {
    let mut made = SVec::new_in(blaze_own::testing::heap().shared());
    out.done.reads.iter().any(|(id, saw)| edit.make(id, &mut made).expect("a digest") != *saw)
}

/// The head cell id of library item `lib`.
fn head_cell(lib: ItemKey) -> CellId {
    CellId { of: signature_name(checker_of(lib).0.to_be_bytes()), cell: blaze_drive::cells::Cell([0; 16]) }
}

/// Every module the program's bodies name, or are written in, by its path.
fn modules(records: &'static Records<'static>) -> Vec<(&'static str, ItemKey)> {
    let root = |krate: usize| NamesSource::root_module(records, krate).unwrap_or_else(|| panic!("crate {krate}'s root"));
    vec![
        ("app", root(APP_AT)),
        ("dep", root(DEP_AT)),
        ("dep2", root(DEP2_AT)),
        ("dep::m", item(records, DEP_AT, Namespace::Types, &["m"])),
        ("app::local", item(records, APP_AT, Namespace::Types, &["local"])),
        ("app::local::deeper", item(records, APP_AT, Namespace::Types, &["local", "deeper"])),
    ]
}

/// EVERY BODY NAMING THROUGH A MODULE IS CHECKED, AND READS NO MODULE'S HEAD CELL: each body
/// of [`CLEAN`] gives a result (no task error), checked whole (not failed, no "not checked"
/// line, no type error), and none of its reads is the head cell of a module (a module has no
/// head: no task defines that cell, so such a read is one no edit moves, rule 21). CONTROL:
/// `wrong_len` writes `[u8; { 3 }]` over two elements, so it must be a type error: the
/// anonymous constant had its value, it was no hole that let every length through. Before the
/// fix `head_len`, `body_len` and `wrong_len` were task errors ("no want makes it:
/// NoHead(.., Module)").
#[test]
fn a_body_through_a_module_is_checked_and_reads_no_module_head() {
    let records = reader(kept(DEP));
    let modules = modules(records);
    // The re-exports and aliases name the module itself: `n`, `shown` are `m`.
    assert_eq!(item(records, APP_AT, Namespace::Types, &["n"]), modules[3].1, "`n` names `dep::m`");
    assert_eq!(item(records, DEP2_AT, Namespace::Types, &["shown"]), modules[3].1, "`dep2::shown` names `dep::m`");
    let mut failed: Vec<String> = Vec::new();
    for path in CLEAN {
        let shown = path.join("::");
        let out = run(records, path);
        let refused: Vec<_> = out.said.iter().filter(|s| matches!(s.what, SaidWhat::NotChecked { .. } | SaidWhat::Type { .. })).map(|s| format!("{:?}: {:?}", s.what, s.words)).collect();
        if out.done.failed || !refused.is_empty() {
            failed.push(format!("`{shown}`: failed {}, said {refused:?}; verdict {}", out.done.failed, String::from_utf8_lossy(&out.bytes)));
        }
        for (module, key) in modules.iter() {
            let cell = head_cell(*key);
            if out.done.reads.iter().any(|(id, _)| *id == cell) {
                failed.push(format!("`{shown}` reads the head cell of the module `{module}`"));
            }
            let mut item16 = [0u8; 16];
            item16.copy_from_slice(&checker_of(*key).0.to_be_bytes());
            if out.reads.iter().any(|r| matches!(r, TableRead::Head { item, .. } if *item == item16)) {
                failed.push(format!("`{shown}` records a head read of the module `{module}`"));
            }
        }
    }
    assert!(failed.is_empty(), "bodies through modules:\n{}", failed.join("\n"));
    let wrong = run(records, &["wrong_len"]);
    assert!(wrong.said.iter().any(|s| matches!(s.what, SaidWhat::Type { .. })), "CONTROL: `[u8; {{ 3 }}] = [1, 2]` is a type error: {:?}", wrong.said);
}

/// A SIGNATURE CHANGE OF `m::f` ENTERS EACH BODY THAT NAMES IT THROUGH `m`: `a` (by `use`),
/// `b` (by `use .. as`), `through_reexport` (by `dep2::shown`) and `mac` (inside a
/// `macro_rules!` expansion) each read `f`'s head cell, and an edit of that cell dirties each.
/// CONTROL: `d`, which names only its own crate's `local::g`, does not read `f`'s cell.
#[test]
fn a_signature_change_through_a_module_enters_its_callers() {
    let records = reader(kept(DEP));
    let f = item(records, DEP_AT, Namespace::Values, &["m", "f"]);
    let cell = head_cell(f);
    let edit = MapMake { edited: vec![cell] };
    for &body in CALLERS_OF_F {
        let out = run(records, &[body]);
        assert!(out.done.reads.iter().any(|(id, _)| *id == cell), "`{body}` does not read `dep::m::f`'s head cell; its head reads: {:?}", out.done.reads.iter().filter(|(id, _)| id.of.kind == Kind::Signature).collect::<Vec<_>>());
        assert!(dirtied(&out, &edit), "an edit of `dep::m::f`'s head leaves `{body}` standing");
    }
    let d = run(records, &["d"]);
    assert!(!dirtied(&d, &edit), "CONTROL: `d` names nothing of `dep::m`, and `f`'s edit dirties it");
    // The text edit itself moves `f`'s names cells somewhere (its row's text): the edited
    // world's tables differ from today's, so the next test's comparison is not vacuous.
    let edited = kept(DEP_F_EDITED);
    let today = kept(DEP);
    let prefix = dep_prefix(today);
    assert_ne!(today.cells.get(&prefix), edited.cells.get(&prefix), "CONTROL: `f`'s signature edit moves a cell of `dep`'s table");
}

/// `dep`'s first 8 key bytes.
fn dep_prefix(kept: &Kept) -> [u8; 8] {
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&kept.keys[DEP_AT].0.0[..8]);
    prefix
}

/// The digest names cell read `read` has in `kept`'s tables (`None`: absent), or `None` for a
/// read that is no names cell.
fn names_digest(kept: &Kept, read: TableRead) -> Option<Option<ContentId>> {
    let TableRead::Names(cell) = read else { return None };
    let table = kept.cells.get(&cell.crate_prefix())?;
    Some(seen_in(table, cell.key()).unwrap_or_else(|e| panic!("the cell {cell:?}: {e:?}")))
}

/// AN UNRELATED ITEM ADDED TO `m` MOVES NO READ OF A BODY THAT NAMES THROUGH `m`: every names
/// cell each body of [`CLEAN`] read holds the same digest in the tables of `dep` with
/// `pub fn unrelated` added at the end of `m` as in today's (the module's `Item` cell holds
/// only its head, its scope cells only what a walk consulted, a binding cell only its name),
/// and no body reads a head cell of `unrelated` (a new key). CONTROL: the edit moves `dep`'s
/// table (the new binding, item and the whole sections), so equal digests are a finding.
#[test]
fn an_unrelated_item_added_to_the_module_moves_no_read() {
    let today = kept(DEP);
    let added = kept(DEP_UNRELATED);
    let prefix = dep_prefix(today);
    assert_ne!(today.cells.get(&prefix), added.cells.get(&prefix), "CONTROL: an item added to `m` moves a cell of `dep`'s table");
    let records = reader(today);
    let unrelated_records = reader(added);
    let unrelated = head_cell(item(unrelated_records, DEP_AT, Namespace::Values, &["m", "unrelated"]));
    let mut moved: Vec<String> = Vec::new();
    let mut names_reads = 0usize;
    for path in CLEAN {
        let shown = path.join("::");
        let out = run(records, path);
        for read in out.reads.iter() {
            let (Some(before), Some(after)) = (names_digest(today, *read), names_digest(added, *read)) else { continue };
            names_reads += 1;
            if before != after {
                moved.push(format!("`{shown}` reads {read:?}: {before:?} today, {after:?} with `unrelated` added"));
            }
        }
        if out.done.reads.iter().any(|(id, _)| *id == unrelated) {
            moved.push(format!("`{shown}` reads the head cell of `m::unrelated`"));
        }
    }
    assert!(names_reads > 0, "CONTROL: the bodies read names cells (the instrument saw none)");
    assert!(moved.is_empty(), "an unrelated item added to `m` moves reads:\n{}", moved.join("\n"));
}
