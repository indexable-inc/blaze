//! A macro call an EXPANSION wrote inside a block it also wrote, whose path's first segment is
//! bound by an item of that block. This is proptest 1.11.0's `prop_assert!` (src/sugar.rs:754-757:
//! `if !$cond { extern crate alloc; let message = alloc::format!($($fmt)*); .. }`), the 21
//! bodies of tempo's check refused with "segment 0: not found" on `alloc :: format!`.
//!
//! The Reference, "Statements", "Item declarations": "Declaring an item within a statement
//! block restricts its scope to the block containing the statement". "Macros By Example",
//! "Hygiene": `macro_rules!` have mixed-site hygiene: "loop labels, block labels, and local
//! variables are looked up at the macro definition site while other symbols are looked up at
//! the macro invocation site". An item name is not hygienic: the `extern crate` an expansion
//! writes in its block binds its name for every path in that block, whoever wrote the path.
//! The path is read at the block's module, which the names library filed for it
//! (blaze-scope expanded_blocks.rs), and never at the block of the call written by hand.
//!
//! The bodies, each checked by the body task over the view (`check_body`) and by today's path,
//! with equal answers:
//! - `direct`: `check!(x)` writes `if x { extern crate dep as __d; let _m = __d::unit!(); }`.
//!   Checked.
//! - `nested`: `outer!(x)` writes `{ let left = x; check!(left); }`, so the block is two
//!   expansions deep (`prop_assert_eq!` calling `prop_assert!`). Checked.
//! - `twin` (negative): `leaky!(x)` writes the same `extern crate` in its `if` block, and
//!   `__d::unit!()` AFTER the block, where `__d` is not bound. Refused, naming `unit`.
//! - `control` (the cause removed): `bare!(x)` is `check!` without the `extern crate`.
//!   Refused, naming `unit`.

#![feature(allocator_api)]

use std::collections::HashMap;

use blaze_drive::key::{ABSENT, Key, Kind, Name};
use blaze_engine::{CellId, Made, Phase, Planned, SVec, Stage, Task};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::project_records::{Listed, Listing, list_with};
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
pub mod ops {
    #[lang = \"drop\"]
    pub trait Drop {
        fn drop(&mut self);
    }
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
macro_rules! unit {
    () => { () };
}
";

const APP: &str = "#![no_std]
macro_rules! check {
    ($c:expr) => {
        if $c {
            extern crate dep as __d;
            let _m = __d::unit!();
        }
    };
}
macro_rules! outer {
    ($c:expr) => {{
        let left = $c;
        check!(left);
    }};
}
macro_rules! leaky {
    ($c:expr) => {
        if $c {
            extern crate dep as __d;
        }
        let _m = __d::unit!();
    };
}
macro_rules! bare {
    ($c:expr) => {
        if $c {
            let _m = __d::unit!();
        }
    };
}
pub fn direct(x: bool) {
    check!(x);
}
pub fn nested(x: bool) {
    outer!(x);
}
pub fn twin(x: bool) {
    leaky!(x);
}
pub fn control(x: bool) {
    bare!(x);
}
";

/// evm2's `spec_to_generic!` and `for_each_spec!` (crates/evm2/src/spec_id.rs:185-229 of the
/// copy tempo builds), with two specs and `$crate::never!()` for core's `unreachable!()` (the
/// fake core has none): a call in the `match`'s braces, as there.
const EVM2: &str = "#![no_std]
pub enum SpecId {
    FRONTIER = 0,
    HOMESTEAD = 1,
}
#[macro_export]
macro_rules! never {
    () => { loop {} };
}
#[macro_export]
macro_rules! spec_to_generic {
    (@spec $spec_id:ident, |$spec_const:ident| $e:expr) => {{
        const $spec_const: u32 = $crate::SpecId::$spec_id as u32;
        $e
    }};
    ([@match $spec_id:expr, $spec_const:ident, $e:expr] $($spec:ident $name:ident,)*) => {{
        match $spec_id {
            $(
                $crate::SpecId::$spec => {
                    $crate::spec_to_generic!(@spec $spec, |$spec_const| $e)
                }
            )*
            #[allow(unreachable_patterns)]
            _ => $crate::never!(),
        }
    }};
    ($spec_id:expr, |$spec_const:ident| $e:expr) => {{
        $crate::for_each_spec!([@match $spec_id, $spec_const, $e] $crate::spec_to_generic)
    }};
}
#[macro_export]
macro_rules! for_each_spec {
    ([$($extra:tt)*] $($m:tt)+) => {
        $($m)+! {
            [$($extra)*]
            FRONTIER frontier,
            HOMESTEAD homestead,
        }
    };
}
";

/// tempo's `execution_config` (crates/evm/src/handler/config.rs:79-84) and
/// `zone_execution_config` (crates/zones/evm/src/zone_evm/contract_creation.rs:28-37), reduced:
/// `SPEC_ID` in the turbofish is the `const` the innermost expansion declares in its own
/// block. `twin` names `SPEC_ID` after the call, outside every block that declares it.
/// `control` writes the innermost expansion's block by hand.
const TEMPO: &str = "#![no_std]
pub struct Config<const S: u32>;
pub struct Exec;
impl Exec {
    pub fn for_config<C>() -> Exec {
        Exec
    }
}
pub fn execution_config(spec: evm2::SpecId) -> Exec {
    evm2::spec_to_generic!(spec, |SPEC_ID| Exec::for_config::<
        Config<SPEC_ID>,
    >())
}
pub fn twin(spec: evm2::SpecId) -> Exec {
    let _e = evm2::spec_to_generic!(spec, |SPEC_ID| Exec::for_config::<Config<SPEC_ID>>());
    Exec::for_config::<Config<SPEC_ID>>()
}
pub fn control() -> Exec {
    {
        const SPEC_ID: u32 = evm2::SpecId::FRONTIER as u32;
        Exec::for_config::<Config<SPEC_ID>>()
    }
}
";

const CFG_IF: &str = "#![no_std]
#[macro_export]
macro_rules! cfg_if {
    (
        if #[cfg( $($i_meta:tt)+ )] { $( $i_tokens:tt )* }
        $(
            else if #[cfg( $($ei_meta:tt)+ )] { $( $ei_tokens:tt )* }
        )*
        $(
            else { $( $e_tokens:tt )* }
        )?
    ) => {
        $crate::cfg_if! {
            @__items () ;
            (( $($i_meta)+ ) ( $( $i_tokens )* )),
            $(
                (( $($ei_meta)+ ) ( $( $ei_tokens )* )),
            )*
            $(
                (() ( $( $e_tokens )* )),
            )?
        }
    };
    (@__items ( $( ($($_:tt)*) , )* ) ; ) => {};
    (
        @__items ( $( ($($no:tt)+) , )* ) ;
        (( $( $($yes:tt)+ )? ) ( $( $tokens:tt )* )),
        $( $rest:tt , )*
    ) => {
        #[cfg(all(
            $( $($yes)+ , )?
            not(any( $( $($no)+ ),* ))
        ))]
        $crate::cfg_if! { @__temp_group $( $tokens )* }

        $crate::cfg_if! {
            @__items ( $( ($($no)+) , )* $( ($($yes)+) , )? ) ;
            $( $rest , )*
        }
    };
    (@__temp_group $( $tokens:tt )* ) => {
        $( $tokens )*
    };
}
#[macro_export]
macro_rules! check_true {
    ($cond:expr) => {
        if !$cond { loop {} }
    };
}
";

/// nybbles 0.4.8's `Nibbles::as_array` (`cfg_if!` expanding to `#[cfg(all(..))] $crate::cfg_if!
/// { @__temp_group const { assert!(..); } .. }`), plus the `else` branch (a false `#[cfg]` call
/// before the live one), a hand-written attributed statement call, and an `extern crate` inside
/// the `cfg_if!`-expanded block whose macro call must resolve in that exact block module.
const NYBBLES: &str = "#![no_std]
use cfg_if::{cfg_if, check_true};
pub enum Opt<T> { Some(T), None }
pub struct AsArray([u64; 5]);
pub struct Nibbles([u64; 5]);
impl Nibbles {
    pub const fn as_array(&self) -> Opt<&AsArray> {
        cfg_if! {
            if #[cfg(all())] {
                const {
                    check_true!(true);
                    check_true!(1 == 1);
                }
                Opt::None
            } else {
                Opt::None
            }
        }
    }
    pub const fn as_array_else(&self) -> Opt<&AsArray> {
        cfg_if! {
            if #[cfg(any())] {
                Opt::None
            } else {
                const {
                    check_true!(true);
                }
                Opt::None
            }
        }
    }
}
pub fn attr_on_stmt_call() {
    #[cfg(all())]
    cfg_if! {
        if #[cfg(all())] {
            {
                struct Local;
                check_true!(true);
                let _ = Local;
            }
        }
    }
}
pub fn block_extern_crate_in_cfg_if() {
    cfg_if! {
        if #[cfg(all())] {
            {
                extern crate cfg_if as __c;
                __c::check_true!(true);
            }
        }
    }
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
            (APP_AT, "/tempo/lib.rs") => Some(TEMPO),
            (DEP_AT, "/evm2/lib.rs") => Some(EVM2),
            (APP_AT, "/nybbles/lib.rs") => Some(NYBBLES),
            (DEP_AT, "/cfg_if/lib.rs") => Some(CFG_IF),
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
        key: CrateItemKey(ContentId::of("blaze-ladder expansion block macro path", name.as_bytes())),
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

/// The records of the app, its dependency and core, solved and encoded as the command keeps
/// them.
fn kept() -> &'static Kept {
    kept_of(&[
        input("app", "/app/lib.rs", &[("dep", DEP_AT), ("core", CORE_AT)]),
        input("dep", "/dep/lib.rs", &[("core", CORE_AT)]),
        input("core", "/core/lib.rs", &[]),
    ])
}

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

/// The app's bodies as the command lists them, by their item keys.
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
        Ok(ContentId::of("blaze-ladder expansion block macro path cell", format!("{want:?}").as_bytes()))
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

/// `direct` and `nested` are checked: `__d::unit!` is read at the block the expansion wrote.
/// `twin` (the path after the block) and `control` (no `extern crate`) are the two refused,
/// each naming the call. Every body answers over the view as today's path does.
#[test]
fn a_path_in_a_block_an_expansion_wrote_reads_that_blocks_extern_crate() {
    let kept = kept();
    let records = reader(kept);
    let list = bodies(records);
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("today's frame")));
    let mut today = ProjectWorld::from_records(records, frame, LANG, a()).unwrap_or_else(|e| panic!("the world did not start: {e}"));
    let mut lines = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    for (key, body) in list.iter() {
        let root = Listed { body: *body, at: None, entry: None };
        let reached = today.each_from(&[root], a(), |w, b| w.check_borrows(b, a()));
        let mut want = blaze_own::Vec::new_in(a());
        for one in reached.iter() {
            let (verdict, borrows) = &one.got;
            want.extend_from_slice(&verdict_bytes(verdict, borrows, Facts::Digest, a()).expect("today's bytes"));
        }
        let got = run(records, *key);
        for said in got.said.iter() {
            match &said.what {
                SaidWhat::NotChecked { cause } => refused.push(format!("{key:x} {body:?} NOT CHECKED {cause}: {}", said.words)),
                other => lines.push(format!("{key:x} {body:?} {other:?}: {}", said.words)),
            }
        }
        println!("BODY {key:x} {body:?}\nTASK {}\nTODAY {}", String::from_utf8_lossy(&got.bytes), String::from_utf8_lossy(&want));
        assert_eq!(String::from_utf8_lossy(&got.bytes), String::from_utf8_lossy(&want), "the body {key:x} {body:?} answers otherwise over the view");
    }
    println!("{}\n{}", refused.join("\n"), lines.join("\n"));
    assert_eq!(list.len(), 4, "the four fns are listed: {list:?}");
    assert_eq!(refused.len(), 2, "only the twin and the control are refused: {refused:#?}");
    assert!(refused.iter().all(|r| r.contains("unit")), "each refusal names its call: {refused:#?}");
}

/// Cause (3) of tempo's check, the repro: `spec_to_generic!` reaches its `const SPEC_ID` four
/// expansions deep (the call, `for_each_spec!`, the `@match` rule's `match`, the `@spec`
/// rule's block). Rust's rule ("Statements", "Item declarations"): "Declaring an item within a
/// statement block restricts its scope to the block containing the statement", and the item
/// name is not hygienic, so `SPEC_ID` in `$e` reads the `const` of its block.
/// `execution_config` and `control` must be checked with no refusal, `twin` refused naming
/// `SPEC_ID` (outside the block). It was refused with "the names scope of the block containing
/// the path" (`execution_config`, and the twin's first call): the scope table answers a block
/// module the names library did not file. The library skipped `for_each_spec!`, whose
/// transcriber `$($m)+! { .. }` calls a macro through a repeated path that
/// blaze-file-items `may_declare_through` did not read as a call, so no block of
/// `spec_to_generic!`'s `@match` expansion was filed. `control` said "cannot find type
/// `SPEC_ID`": a one-name argument of a written type (`Config<SPEC_ID>`) was not looked up among
/// the body's items (types.rs `path_args`; a body path's own argument is, body.rs `name_arg`).
/// The fake core declares `Drop`, as the pinned core does: without its lang item
/// `Exec::for_config`'s borrows are not checked (`FactUnknown { fact: "Drop", why: NoLangItem
/// }`), which is no fact of this program.
#[test]
fn a_const_an_expansion_declares_in_its_block_names_a_const_argument() {
    let kept = kept_of(&[
        input("tempo", "/tempo/lib.rs", &[("evm2", DEP_AT), ("core", CORE_AT)]),
        input("evm2", "/evm2/lib.rs", &[("core", CORE_AT)]),
        input("core", "/core/lib.rs", &[]),
    ]);
    let records = reader(kept);
    let list = bodies(records);
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("today's frame")));
    let mut today = ProjectWorld::from_records(records, frame, LANG, a()).unwrap_or_else(|e| panic!("the world did not start: {e}"));
    // Per body: what it said. The twin is the one body that says anything, and it names
    // `SPEC_ID` (a refusal or a resolution error, whichever the checker gives).
    let mut saying: Vec<Vec<String>> = Vec::new();
    for (key, body) in list.iter() {
        let root = Listed { body: *body, at: None, entry: None };
        let reached = today.each_from(&[root], a(), |w, b| w.check_borrows(b, a()));
        let mut want = blaze_own::Vec::new_in(a());
        for one in reached.iter() {
            let (verdict, borrows) = &one.got;
            want.extend_from_slice(&verdict_bytes(verdict, borrows, Facts::Digest, a()).expect("today's bytes"));
        }
        let got = run(records, *key);
        let said: Vec<String> = got
            .said
            .iter()
            .map(|said| match &said.what {
                SaidWhat::NotChecked { cause } => format!("{key:x} {body:?} NOT CHECKED {cause}: {}", said.words),
                other => format!("{key:x} {body:?} {other:?}: {}", said.words),
            })
            .collect();
        println!("BODY {key:x} {body:?}\n{}", said.join("\n"));
        saying.push(said);
        assert_eq!(String::from_utf8_lossy(&got.bytes), String::from_utf8_lossy(&want), "the body {key:x} {body:?} answers otherwise over the view");
    }
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    println!("records filed: {errors:#?}");
    let all: Vec<&String> = saying.iter().flatten().collect();
    assert_eq!(list.len(), 4, "`Exec::for_config` and the three fns are listed: {list:?}");
    assert!(!all.iter().any(|r| r.contains("the names scope of the block containing the path")), "no block scope is a gap: {all:#?}");
    assert_eq!(saying.iter().filter(|s| !s.is_empty()).count(), 1, "only the twin says anything: {all:#?}");
    assert!(all.iter().any(|r| r.contains("SPEC_ID")), "the twin names `SPEC_ID`: {all:#?}");
}

/// nybbles 0.4.8's `Nibbles::as_array`: `cfg_if!` expands via `#[cfg(all(..))] $crate::cfg_if!
/// { @__temp_group const { assert!(..); } .. }`. In body lowering the `MACRO_CALL` starts at
/// `$crate`, after `#[cfg(..)]`, while `nested::scan` previously put `#[cfg(..)]` inside
/// `NestedCall::path`, so `statement_call` missed the attributed call and left the expanded
/// `const { .. }` block unfiled (`UNFILED_BLOCK`). Every body here must check cleanly with zero
/// refusals, and `block_extern_crate_in_cfg_if` proves `__c::check_true!` resolves in the exact
/// expansion block module that declares `extern crate cfg_if as __c;`.
#[test]
fn an_attributed_macro_call_in_an_expansion_files_its_block_modules() {
    let kept = kept_of(&[
        input("nybbles", "/nybbles/lib.rs", &[("cfg_if", DEP_AT), ("core", CORE_AT)]),
        input("cfg_if", "/cfg_if/lib.rs", &[("core", CORE_AT)]),
        input("core", "/core/lib.rs", &[]),
    ]);
    let records = reader(kept);
    let list = bodies(records);
    let frame = blaze_own::testing::bump().alloc_val(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("today's frame"));
    let mut today = ProjectWorld::from_records(records, frame, LANG, a()).unwrap_or_else(|e| panic!("the world did not start: {e}"));
    for (key, body) in list.iter() {
        let root = Listed { body: *body, at: None, entry: None };
        let reached = today.each_from(&[root], a(), |w, b| w.check_borrows(b, a()));
        let mut want = blaze_own::Vec::new_in(a());
        for one in reached.iter() {
            let (verdict, borrows) = &one.got;
            want.extend_from_slice(&verdict_bytes(verdict, borrows, Facts::Digest, a()).expect("today's bytes"));
        }
        let got = run(records, *key);
        assert!(got.said.is_empty(), "body {key:x} {body:?} under attributed macro calls was not clean: {:#?}", got.said);
        assert_eq!(core::str::from_utf8(&got.bytes), core::str::from_utf8(&want), "the body {key:x} {body:?} answers otherwise over the view");
    }
    assert_eq!(list.len(), 4, "the four fns are listed: {list:?}");
}
