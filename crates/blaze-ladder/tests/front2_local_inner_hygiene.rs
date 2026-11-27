//! End-to-end tests for `#[macro_export(local_inner_macros)]` token hygiene (`Mark::local_inner` /
//! `TextMark::local_inner`) across `blaze-expand`, `blaze-scope`, `blaze-lower`, and `blaze-ladder`
//! (`front2::scoped::ScopedBy` + `check_body`).
//!
//! Covers:
//! - Positive 1 (`nix::ioctl_*` shape): `dep`'s `#[macro_export(local_inner_macros)]` item-level
//!   macro emits a `fn` whose body calls `wrapper!(inner_code!($x))` (nested inside macro args,
//!   forwarded through non-`local_inner_macros` `wrapper!`).
//! - Positive 2 (`prometheus::register_int_gauge!` shape): `dep`'s `#[macro_export(local_inner_macros)]`
//!   body-level macro calls `reg_inner!(opts!($x))` directly inside a caller `fn`, even when the
//!   caller defines a shadowing body-local `macro_rules! opts`.
//! - Negative control 1: the same macros WITHOUT `local_inner_macros` fail to resolve `wrapper` /
//!   `inner_code` / `opts` in the caller crate.
//! - Negative control 2: `::core::stringify!(opts!(1))` inside `local_inner_macros` still yields
//!   `"opts!(1)"` (no token rewriting to `$crate::opts!(1)`), and a caller-supplied argument
//!   `dep::pass_arg!(caller_mac!())` resolves `caller_mac!` in the caller crate (`TokenOrigin::Input`,
//!   `local_inner == false`), not `dep`.
//! - Negative control 3: a non-macro value/type path (`CallerTy`, `CALLER_VAL`) emitted by a
//!   `local_inner_macros` macro resolves in the caller crate, never `$crate::CallerTy` /
//!   `$crate::CALLER_VAL`.

use std::collections::HashMap;

use blaze_drive::key::{ABSENT, Key, Kind, Name};
use blaze_engine::{CellId, Made, Phase, Planned, SVec, Stage, Task};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::project::BodyRef;
use blaze_ladder::front2::project_records::{Listing, list_with};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, Records};
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts};
use blaze_ladder::front2::tables::task::{BodyFrame, BodyOut, Make, MakeRefused, SaidWhat, check_body};
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_own::Alloc;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const CORE: &str = r#"#![no_core]
#![feature(no_core, lang_items, rustc_attrs, decl_macro)]
pub mod marker {
    #[lang = "pointee_sized"]
    pub trait PointeeSized {}
    #[lang = "meta_sized"]
    pub trait MetaSized: PointeeSized {}
    #[lang = "sized"]
    pub trait Sized: MetaSized {}
    #[lang = "copy"]
    pub trait Copy {}
}
#[rustc_builtin_macro]
pub macro stringify($($t:tt)*) {
    /* compiler built-in */
}
pub mod prelude {
    pub mod v1 {
        pub use crate::marker::{Copy, Sized};
        pub use crate::stringify;
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
"#;

const DEP: &str = r#"#![no_std]

// Types/consts in `dep` that MUST NOT capture non-macro paths emitted by `local_inner_macros`.
pub type CallerTy = bool;
pub const CALLER_VAL: bool = false;

// Incompatible `caller_mac` in `dep` (bool) vs `app` (u32) to prove caller arguments resolve in `app`.
#[macro_export]
macro_rules! caller_mac {
    () => {
        false
    };
}

// Positive 1 helpers (`nix::ioctl_*` -> `convert_ioctl_res!` -> `request_code_*`).
#[macro_export]
macro_rules! wrapper {
    ($e:expr) => {
        $e + 1u32
    };
}

#[macro_export]
macro_rules! inner_code {
    ($x:expr) => {
        $x
    };
}

#[macro_export(local_inner_macros)]
macro_rules! ioctl_like {
    ($name:ident, $x:expr) => {
        pub fn $name() -> u32 {
            wrapper!(inner_code!($x))
        }
    };
}

// Negative control 1a: identical to `ioctl_like!` WITHOUT `local_inner_macros`.
#[macro_export]
macro_rules! ioctl_no_local {
    ($name:ident, $x:expr) => {
        pub fn $name() -> u32 {
            wrapper!(inner_code!($x))
        }
    };
}

// Positive 2 helpers (`prometheus::register_int_gauge!` -> `opts!`).
#[macro_export]
macro_rules! opts {
    ($x:expr) => {
        $x
    };
}

#[macro_export]
macro_rules! reg_inner {
    ($v:expr) => {
        $v + 2u32
    };
}

#[macro_export(local_inner_macros)]
macro_rules! reg {
    ($x:expr) => {
        reg_inner!(opts!($x))
    };
}

// Negative control 1b: identical to `reg!` WITHOUT `local_inner_macros`.
#[macro_export]
macro_rules! reg_no_local {
    ($x:expr) => {
        reg_inner!(opts!($x))
    };
}

// Negative control 2a: `stringify!(opts!(1))` inside `local_inner_macros` must yield `"opts!(1)"`.
#[macro_export(local_inner_macros)]
macro_rules! str_check {
    () => {
        ::core::stringify!(opts!(1))
    };
}

// Negative control 2b: caller-supplied argument forwarded into `wrapper!($e)`.
#[macro_export(local_inner_macros)]
macro_rules! pass_arg {
    ($e:expr) => {
        wrapper!($e)
    };
}

// Negative control 3: non-macro value/type paths must resolve in caller, NOT `$crate`.
#[macro_export(local_inner_macros)]
macro_rules! emit_val {
    ($name:ident) => {
        pub fn $name() -> CallerTy {
            CALLER_VAL
        }
    };
}
"#;

const APP: &str = r#"#![no_std]

pub type CallerTy = u32;
pub const CALLER_VAL: u32 = 5u32;

macro_rules! caller_mac {
    () => {
        99u32
    };
}

// Positive 1: item-level `local_inner_macros` macro emitting fn body calling `wrapper!(inner_code!(41))`.
dep::ioctl_like!(pos1_ioctl, 41u32);

// Positive 2: direct body-level `local_inner_macros` macro calling `reg_inner!(opts!(7u32))`,
// even with a body-local `macro_rules! opts` of incompatible type (`bool`).
pub fn pos2_reg() -> u32 {
    macro_rules! opts {
        ($x:expr) => {
            true
        };
    }
    dep::reg!(7u32)
}

// Negative control 1a: item-level macro WITHOUT `local_inner_macros` cannot resolve `wrapper!`.
dep::ioctl_no_local!(neg1_ioctl_no_local, 41u32);

// Negative control 1b: body-level macro WITHOUT `local_inner_macros` cannot resolve `reg_inner!` / `opts!`.
pub fn neg1_reg_no_local() -> u32 {
    dep::reg_no_local!(7u32)
}

// Negative control 2a: `stringify!(opts!(1))` inside `local_inner_macros` yields `"opts!(1)"`.
pub fn neg2_stringify() -> &'static str {
    dep::str_check!()
}

// Negative control 2b: caller-supplied `caller_mac!()` resolves in `app` (`u32`), not `dep` (`bool`).
pub fn neg2_caller_arg() -> u32 {
    dep::pass_arg!(caller_mac!())
}

// Negative control 3: non-macro paths (`CallerTy`, `CALLER_VAL`) resolve in `app` (`u32`), not `dep` (`bool`).
dep::emit_val!(neg3_non_macro_paths);
"#;

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
        key: CrateItemKey(ContentId::of("blaze-ladder local_inner hygiene", name.as_bytes())),
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
    expansions: Vec<HashMap<ItemKey, (&'static str, Option<&'static [u8]>)>>,
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

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'static>> {
        let &(text, origins) = self.expansions.get(krate)?.get(&call)?;
        let origins = origins.and_then(|b| blaze_scope::expansion_origins::split(b).ok());
        Some(LoadedExpansion { text, origins })
    }
}

fn build_records() -> (&'static Names<'static>, &'static Records<'static>) {
    let crates = [
        input("app", "/app/lib.rs", &[("dep", DEP_AT), ("core", CORE_AT)]),
        input("dep", "/dep/lib.rs", &[("core", CORE_AT)]),
        input("core", "/core/lib.rs", &[]),
    ];
    let sysroot = Sysroot { core: Some(CrateId(CORE_AT as u32)), alloc: None, std: None, bstd: None };
    let names: &'static Names<'static> = Box::leak(Box::new(
        resolve_crates(&crates, sysroot, &Texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}")),
    ));
    let g = a();
    let mut kept = Kept {
        names: Vec::new(),
        items: Vec::new(),
        files: Vec::new(),
        expansions: Vec::new(),
        keys: Vec::new(),
        crate_names: Vec::new(),
    };
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), f.text)).collect());
        kept.expansions.push(
            c.expansions
                .iter()
                .filter_map(|(call, e)| {
                    e.made.map(|m| {
                        let text: &'static str = Box::leak(m.text.to_string().into_boxed_str());
                        let origins = m.origins.ok().map(|b| &*Box::leak(b.to_vec().into_boxed_slice()));
                        (*call, (text, origins))
                    })
                })
                .collect(),
        );
        kept.keys.push(c.key);
        kept.crate_names.push(c.name);
    }
    let kept: &'static Kept = Box::leak(Box::new(kept));
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(Vec::leak(kept.crate_names.clone()), g)));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(Vec::leak(kept.keys.clone()), kept, log, frame, g)));
    (names, records)
}

struct NoEdit;

impl<'a> Make<'a> for NoEdit {
    fn make(&self, want: &CellId, _made: &mut SVec<'a, Made<'a>>) -> Result<ContentId, MakeRefused> {
        Ok(ContentId::of("blaze-ladder local_inner cell", format!("{want:?}").as_bytes()))
    }
}

fn run_body(records: &'static Records<'static>, key: ItemKey) -> BodyOut<'static> {
    let g = a();
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: LANG, reads: &unused });
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("body frame")));
    let body = BodyFrame {
        frame,
        alloc: a(),
        plant_drop: None,
        cost: None,
        needs: blaze_ladder::front2::heads::NeedsMet::All,
        heads: None,
        sources: None,
        goals: None,
        prepared: None,
        front: None,
    };
    let task = Task {
        planned: Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) },
        key: Key(ABSENT),
    };
    check_body(tables, &task, &body, &NoEdit, blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("body task {key:x}: {e}"))
}

fn fn_key(names: &'static Names<'static>, records: &'static Records<'static>, name: &str) -> ItemKey {
    for listed in list_with(records, Listing::FnsAndInitializers, a()).iter() {
        if let BodyRef::Free(k) = listed.body
            && let Some(item) = names.item_text(k)
            && item.name == Some(name)
        {
            return k;
        }
    }
    panic!("function `{name}` not found in app");
}

fn issues(out: &BodyOut<'_>) -> Vec<String> {
    out.said
        .iter()
        .map(|s| format!("{:?}: {}", s.what, s.words.as_str()))
        .collect()
}

struct NoCfg;
impl blaze_lower::Cfg for NoCfg {
    fn atom(&mut self, _: &str, _: Option<&str>) -> blaze_lower::CfgAnswer {
        blaze_lower::CfgAnswer::No
    }
}

struct NoVals;
impl blaze_lower::ValueNames for NoVals {
    fn value_name(&mut self, _: &str) -> blaze_lower::ValueName {
        blaze_lower::ValueName::Nothing
    }
}

struct NoHost;
impl blaze_expand::ExpandCx for NoHost {
    fn env(&self, _: &str) -> Result<Option<&str>, blaze_expand::HostError> {
        Ok(None)
    }
    fn cfg(&self, _: &blaze_expand::TokenStream<'_>) -> Result<bool, blaze_expand::HostError> {
        Ok(false)
    }
    fn include(&self, _: &str, _: blaze_expand::IncludeAs) -> Result<blaze_expand::Included<'_>, blaze_expand::HostError> {
        Ok(blaze_expand::Included::Missing)
    }
    fn place(&self, _: blaze_expand::PlaceOf) -> Result<blaze_expand::Place<'_>, blaze_expand::HostError> {
        Ok(blaze_expand::Place::Number(1))
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

#[test]
fn local_inner_macros_token_hygiene_positives_and_negative_controls() {
    let (names, records) = build_records();
    assert!(names.notes(APP_AT).is_empty(), "app resolve notes: {:?}", names.notes(APP_AT));
    assert!(names.notes(DEP_AT).is_empty(), "dep resolve notes: {:?}", names.notes(DEP_AT));

    // Positive 1 (`nix::ioctl_*` -> `wrapper!(inner_code!(41u32))`): clean!
    let pos1 = run_body(records, fn_key(names, records, "pos1_ioctl"));
    assert!(issues(&pos1).is_empty(), "pos1_ioctl must check clean, got: {:?}", issues(&pos1));

    // Positive 2 (`prometheus::register_int_gauge!` -> `reg_inner!(opts!(7u32))`): clean even with caller local `opts!`!
    let pos2 = run_body(records, fn_key(names, records, "pos2_reg"));
    assert!(issues(&pos2).is_empty(), "pos2_reg must check clean, got: {:?}", issues(&pos2));

    // Negative control 1a & 1b: WITHOUT `local_inner_macros`, bare calls to `dep`'s macros MUST fail to resolve!
    let neg1a = run_body(records, fn_key(names, records, "neg1_ioctl_no_local"));
    assert!(
        neg1a.said.iter().any(|s| matches!(s.what, SaidWhat::NotChecked { .. })),
        "neg1_ioctl_no_local must fail without local_inner_macros, got: {:?}",
        issues(&neg1a)
    );
    let neg1b = run_body(records, fn_key(names, records, "neg1_reg_no_local"));
    assert!(
        neg1b.said.iter().any(|s| matches!(s.what, SaidWhat::NotChecked { .. })),
        "neg1_reg_no_local must fail without local_inner_macros, got: {:?}",
        issues(&neg1b)
    );

    // Negative control 2a: `::core::stringify!(opts!(1))` inside `local_inner_macros` checks clean
    // AND produces `"opts!(1)"` (never `"$crate::opts!(1)"` or `"crate::opts!(1)"`).
    let neg2a_key = fn_key(names, records, "neg2_stringify");
    let neg2a = run_body(records, neg2a_key);
    assert!(issues(&neg2a).is_empty(), "neg2_stringify must check clean, got: {:?}", issues(&neg2a));
    // Verify the exact stringified literal text through `blaze-lower` + `ScopedBy`.
    {
        let alloc = a();
        let item = names.item_text(neg2a_key).expect("neg2_stringify item text");
        let scoped = blaze_ladder::front2::scoped::ScopedBy { names: records, module: item.module, prepared: None, blocks: None };
        let mut expand = blaze_lower::by_name::ByName::with_names(&scoped, NoHost, alloc);
        let lowered = blaze_lower::lower_body_expanding(item.text, Edition::E2021, &mut NoCfg, &mut NoVals, &mut expand, alloc)
            .unwrap_or_else(|e| panic!("neg2_stringify lower: {e:?}"));
        assert!(expand.findings().is_empty(), "neg2_stringify findings: {:?}", expand.findings());
        let view = lowered.view();
        let mut lits = Vec::new();
        for i in 0..u32::try_from(view.exprs.len()).expect("exprs fit u32") {
            let id = blaze_lower::ExprId(i);
            if matches!(view.expr(id), Ok(blaze_lower::Expr::Lit(blaze_lower::Lit::Str))) {
                let r = view.expr_range(id).expect("expr range");
                lits.push(view.text_at(r).expect("str lit text").to_string());
            }
        }
        assert_eq!(lits, vec!["\"opts! (1)\""], "stringify! must yield \"opts! (1)\" (not $crate::opts!(1) or crate::opts!(1))");
    }

    // Negative control 2b: caller argument `dep::pass_arg!(caller_mac!())` resolves `caller_mac!`
    // in `app` (`u32`), NOT `dep` (`bool`)!
    let neg2b = run_body(records, fn_key(names, records, "neg2_caller_arg"));
    assert!(issues(&neg2b).is_empty(), "neg2_caller_arg must resolve caller_mac! in app (u32), got: {:?}", issues(&neg2b));

    // Negative control 3: non-macro paths (`CallerTy`, `CALLER_VAL`) resolve in `app` (`u32`), NOT `dep` (`bool`)!
    let neg3 = run_body(records, fn_key(names, records, "neg3_non_macro_paths"));
    assert!(issues(&neg3).is_empty(), "neg3_non_macro_paths must resolve CallerTy/CALLER_VAL in app (u32), got: {:?}", issues(&neg3));
}
