//! A macro call inside an unnamed constant's block, through an `extern crate .. as` the block
//! itself declares: the shape every serde derive writes (`const _: () = { extern crate serde as
//! _serde; _serde::__require_serde_not_serde_core!(); impl _serde::Serialize for T { .. } };`).
//!
//! The Reference, "Statements", "Item declarations": "Declaring an item within a statement block
//! restricts its scope to the block containing the statement", and "Extern crate declarations":
//! an `extern crate` binds its name in the declaring scope. So `_dep::the_macro!` reads `_dep` in
//! the block, never at the module. Each body (the constant's initializer and the impl's fn) is
//! checked by the body task over the view (`check_body`) and by today's path, equal, and none is
//! refused; a type error planted in a second such impl's fn is reported (the control).

#![feature(allocator_api)]

use std::collections::HashMap;

use blaze_drive::key::{ABSENT, Key, Kind, Name};
use blaze_engine::{CellId, Made, Phase, Planned, SVec, Stage, Task};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::project_records::{Listing, list_with};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts};
use blaze_ladder::front2::tables::task::{BodyFrame, BodyOut, Facts, Make, MakeRefused, SaidWhat, check_body, verdict_bytes};
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_own::Alloc;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const CORE: &str = "#![no_core]
#![feature(no_core, lang_items)]
pub mod marker {
    #[lang = \"pointee_sized\"]
    pub trait PointeeSized {}
    #[lang = \"meta_sized\"]
    pub trait MetaSized: PointeeSized {}
    #[lang = \"sized\"]
    pub trait Sized: MetaSized {}
    #[lang = \"copy\"]
    pub trait Copy {}
}
pub mod prelude {
    pub mod v1 {
        pub use crate::marker::{Copy, Sized};
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

const DEP: &str = "#![no_std]
#[macro_export]
macro_rules! the_macro {
    () => {};
}
pub trait Trait {
    fn f(&self) -> u32;
}
";

const APP: &str = "#![no_std]
pub struct T;
pub struct U;
const _: () = {
    extern crate dep as _dep;
    _dep::the_macro!();
    impl _dep::Trait for T {
        fn f(&self) -> u32 {
            7
        }
    }
};
const _: () = {
    extern crate dep as _dep;
    _dep::the_macro!();
    impl _dep::Trait for U {
        fn f(&self) -> u32 {
            T
        }
    }
};
";

/// The textual scope of a body's macro call (the regression of 2026-11-17: 557 functions of the
/// 1,100-crate run, core's `Argument::new_debug` calling `argument_new!` among them, refused
/// with "segment 0: not found" once a call was read at its innermost block). The Reference,
/// "Macros by example", "Textual scope": a `macro_rules!` is in scope from its definition "to
/// the end of the enclosing module or block", and a one-segment call is looked up there first,
/// then as a path from the innermost block outward ("Path-based scope").
const MACROS: &str = "#![no_std]
pub struct T;
pub trait Tr {
    fn f(&self) -> u32;
}
macro_rules! seven {
    () => { 7 };
}
macro_rules! shadowed {
    () => { false };
}
pub fn by_module() -> u32 {
    seven!()
}
pub fn in_nested_block() -> u32 {
    let x = { seven!() };
    x
}
pub fn in_closure() -> u32 {
    let _f = || { seven!() };
    7
}
pub fn by_block_shadow() -> u32 {
    macro_rules! shadowed {
        () => { 7 };
    }
    shadowed!()
}
pub fn by_block_use() -> u32 {
    use mdep::from_dep;
    from_dep!()
}
const _: () = {
    extern crate mdep as _mdep;
    _mdep::marker!();
    impl Tr for T {
        fn f(&self) -> u32 {
            seven!()
        }
    }
};
pub fn by_nothing() -> u32 {
    nothing_binds!()
}
";

const MDEP: &str = "#![no_std]
#[macro_export]
macro_rules! from_dep {
    () => { 7 };
}
#[macro_export]
macro_rules! marker {
    () => {};
}
";

const APP_AT: usize = 0;
const DEP_AT: usize = 1;
const CORE_AT: usize = 2;
const LANG: &[usize] = &[APP_AT, DEP_AT, CORE_AT];

struct Texts;

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (APP_AT, "/app/lib.rs") => Some(APP),
            (DEP_AT, "/dep/lib.rs") => Some(DEP),
            (CORE_AT, "/core/lib.rs") => Some(CORE),
            (APP_AT, "/macros/lib.rs") => Some(MACROS),
            (DEP_AT, "/mdep/lib.rs") => Some(MDEP),
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
        key: CrateItemKey(ContentId::of("blaze-ladder block extern crate", name.as_bytes())),
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
    kept_of(&[
        input("app", "/app/lib.rs", &[("dep", DEP_AT), ("core", CORE_AT)]),
        input("dep", "/dep/lib.rs", &[("core", CORE_AT)]),
        input("core", "/core/lib.rs", &[]),
    ])
}

/// The records of `crates` (an app at [`APP_AT`], its dependency at [`DEP_AT`], core at
/// [`CORE_AT`]), solved and encoded as the command keeps them.
fn kept_of(crates: &[CrateInput<'static>]) -> &'static Kept {
    let sysroot = Sysroot { core: Some(CrateId(CORE_AT as u32)), alloc: None, std: None, bstd: None };
    let names: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, &Texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"))));
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

fn reader(kept: &'static Kept) -> &'static Records<'static> {
    let g = a();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(Vec::leak(kept.crate_names.clone()), g)));
    Box::leak(Box::new(Records::new(Vec::leak(kept.keys.clone()), kept, log, frame, g)))
}

/// The app's bodies as the command lists them (fns and initializers), by their item keys.
fn bodies(records: &'static Records<'static>) -> Vec<(ItemKey, BodyRef<'static>)> {
    let mut out = Vec::new();
    for listed in list_with(records, Listing::FnsAndInitializers, a()).iter() {
        let key = match listed.body {
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
        out.push((key, listed.body));
    }
    out.sort_by_key(|(k, _)| *k);
    out
}

struct NoEdit;

impl<'a> Make<'a> for NoEdit {
    fn make(&self, want: &CellId, _made: &mut SVec<'a, Made<'a>>) -> Result<ContentId, MakeRefused> {
        Ok(ContentId::of("blaze-ladder block extern crate cell", format!("{want:?}").as_bytes()))
    }
}

fn run(records: &'static Records<'static>, key: ItemKey) -> BodyOut<'static> {
    let g = a();
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: LANG, reads: &unused });
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the body's frame")));
    let body = BodyFrame { frame, alloc: a(), plant_drop: None, cost: None, needs: blaze_ladder::front2::heads::NeedsMet::All, heads: None, sources: None, goals: None, prepared: None };
    let task = Task { planned: Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) }, key: Key(ABSENT) };
    check_body(tables, &task, &body, &NoEdit, blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("the body task of {key:x}: {e}"))
}

/// Every body of the app, the two constants' initializers and the two impls' fns, is checked
/// by the task as today's path checks it; none is refused; the planted E0308 is reported.
#[test]
fn a_call_through_a_blocks_extern_crate_is_checked() {
    let kept = kept();
    let records = reader(kept);
    let list = bodies(records);
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("today's frame")));
    let mut today = ProjectWorld::from_records(records, frame, LANG, a()).unwrap_or_else(|e| panic!("the world did not start: {e}"));
    let mut lines = Vec::new();
    let (mut refused, mut e0308, mut fns, mut consts) = (0, 0, 0, 0);
    for (key, body) in list.iter() {
        let (verdict, borrows) = today.check_borrows(*body, a());
        let want = verdict_bytes(&verdict, &borrows, Facts::Digest, a()).expect("today's bytes");
        let got = run(records, *key);
        match body {
            BodyRef::Member { .. } => fns += 1,
            BodyRef::Free(_) => consts += 1,
        }
        for said in got.said.iter() {
            match &said.what {
                SaidWhat::NotChecked { cause } => {
                    refused += 1;
                    lines.push(format!("{key:x} NOT CHECKED {cause}: {}", said.words));
                }
                SaidWhat::Type { code } => {
                    if *code == "E0308" {
                        e0308 += 1;
                    }
                    lines.push(format!("{key:x} {code}: {}", said.words));
                }
                other => lines.push(format!("{key:x} {other:?}: {}", said.words)),
            }
        }
        println!("BODY {key:x} {body:?}\nTASK {}\nTODAY {}", String::from_utf8_lossy(&got.bytes), String::from_utf8_lossy(&want));
        assert_eq!(String::from_utf8_lossy(&got.bytes), String::from_utf8_lossy(&want), "the body {key:x} answers otherwise over the view");
    }
    println!("{}", lines.join("\n"));
    assert_eq!((consts, fns), (2, 2), "the two initializers and the two impls' fns are listed: {list:?}");
    assert_eq!(refused, 0, "no body is refused: {lines:#?}");
    assert_eq!(e0308, 1, "the planted type error is reported once: {lines:#?}");
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "the records filed {errors:?}");
}

/// [`MACROS`]: every body calling a `macro_rules!` written above it in the module is checked
/// (at the body's block, an inner block, a closure's block, an impl's fn inside a derive's
/// `const _` block), the block's own `macro_rules! shadowed` is the one expanded (`7`; the
/// module's `false` would be E0308 in a `u32` fn), the block's `use mdep::from_dep` names the
/// dependency's macro, the derive's `_mdep::marker!()` reads the block's `extern crate`, each
/// body answering over the view as today's path does; the control `nothing_binds!()` is the
/// one body refused, and its refusal names the call.
#[test]
fn a_body_reads_the_textual_scope_of_its_module_first() {
    let kept = kept_of(&[
        input("macros", "/macros/lib.rs", &[("mdep", DEP_AT), ("core", CORE_AT)]),
        input("mdep", "/mdep/lib.rs", &[("core", CORE_AT)]),
        input("core", "/core/lib.rs", &[]),
    ]);
    let records = reader(kept);
    let list = bodies(records);
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("today's frame")));
    let mut today = ProjectWorld::from_records(records, frame, LANG, a()).unwrap_or_else(|e| panic!("the world did not start: {e}"));
    let mut lines = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    let mut e0308 = 0;
    for (key, body) in list.iter() {
        let (verdict, borrows) = today.check_borrows(*body, a());
        let want = verdict_bytes(&verdict, &borrows, Facts::Digest, a()).expect("today's bytes");
        let got = run(records, *key);
        for said in got.said.iter() {
            match &said.what {
                SaidWhat::NotChecked { cause } => refused.push(format!("{key:x} {body:?} NOT CHECKED {cause}: {}", said.words)),
                SaidWhat::Type { code } => {
                    if *code == "E0308" {
                        e0308 += 1;
                    }
                    lines.push(format!("{key:x} {body:?} {code}: {}", said.words));
                }
                other => lines.push(format!("{key:x} {body:?} {other:?}: {}", said.words)),
            }
        }
        assert_eq!(String::from_utf8_lossy(&got.bytes), String::from_utf8_lossy(&want), "the body {key:x} {body:?} answers otherwise over the view");
    }
    println!("{}\n{}", refused.join("\n"), lines.join("\n"));
    assert!(list.len() >= 8, "the six fns, the impl's fn and the constant's initializer are listed: {list:?}");
    // `Tr::f` is declared with no default body: no body of the crate, so never listed (front2
    // project_records.rs `list_in`), and never answered "no body" (was a known hole of the body
    // list, set aside here by its words until 2026-11-20).
    assert!(
        !list.iter().any(|(_, body)| matches!(body, BodyRef::Member { is_trait: true, name: "f", .. })),
        "a trait's fn with no default body is listed as a body: {list:?}"
    );
    assert_eq!(refused.len(), 1, "only the control is refused: {refused:#?}");
    assert!(refused[0].contains("nothing_binds"), "the control's refusal names its call: {refused:#?}");
    assert_eq!(e0308, 0, "the block's own `shadowed` is expanded, not the module's: {lines:#?}");
}
