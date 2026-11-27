//! Rust 2015 and Rust 2018 bare trait objects (trait object types written without `dyn`, e.g.
//! `glob 0.3.3`'s `impl Error for GlobError { fn cause(&self) -> Option<&Error> { Some(&self.error) } }`),
//! verified end-to-end through `Front2Program`, `LazyProgram` (including vtable construction and
//! native execution via `run_with`), and head region laying.
//!
//! Negative control (Rule 23): the exact same bare trait object in Rust 2021 and Rust 2024 is
//! refused (E0782), and `LazyProgram` surfaces the `fn_gaps` head refusal directly instead of
//! swallowing it as an opaque `NoHead`.

use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::crates::{Inputs, Toolchain, syntax_of};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of, member_key_of};
use blaze_ladder::front2::lifetimes::{LifeRefusal, Region};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::front2::program::lazy::LazyProgram;
use blaze_ladder::front2::source::NamesSource;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};
use blaze_ladder::seam::FrontEnd;
use blaze_own::Alloc;
use blaze_scope::graph::Sysroot;
use blaze_scope::names::{self, Written};
use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_surface::{ItemKind, Namespace, PathKind};
use blaze_unit_graph::implicit;
use edition::PackageEdition;

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/twin-prelude.rs");

/// Exact `glob 0.3.3` pattern (`impl Error for GlobError { fn cause(&self) -> Option<&Error> }`)
/// plus broader bare trait object forms: one-name generic arg (`Holder<Error>`), associated type
/// binding (`&Iter<Item = u8>`), trait lifetime (`&Borrowed<'a>` and elided `&Borrowed`),
/// parenthesized sugar (`&Callable(&u8) -> &u8`), HRTB (`&for<'a> Borrowed<'a>`), auto trait
/// (`&Sync`), and body type annotation (`let e: &Error = ...`).
const GLOB_RUN_PROGRAM: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, auto_traits)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

#[lang = \"sync\"]
unsafe auto trait Sync {}

enum MyOption<T> {
    None,
    Some(T),
}

trait Error {
    fn code(&self) -> u8;
    fn cause(&self) -> MyOption<&Error> {
        MyOption::None
    }
}

struct IoError {
    val: u8,
}

impl Error for IoError {
    fn code(&self) -> u8 {
        self.val
    }
}

struct GlobError {
    error: IoError,
}

impl Error for GlobError {
    fn code(&self) -> u8 {
        self.error.code()
    }

    fn cause(&self) -> MyOption<&Error> {
        MyOption::Some(&self.error)
    }
}

struct Holder<T: ?Sized> {
    ptr: *const T,
}

trait Iter {
    type Item;
    fn next_item(&self) -> Self::Item;
}

struct ConstIter {
    v: u8,
}

impl Iter for ConstIter {
    type Item = u8;
    fn next_item(&self) -> u8 {
        self.v
    }
}

trait Borrowed<'a> {
    fn peek(&self) -> &'a u8;
}

struct SliceBorrow<'a> {
    r: &'a u8,
}

impl<'a> Borrowed<'a> for SliceBorrow<'a> {
    fn peek(&self) -> &'a u8 {
        self.r
    }
}

fn read_cause(e: &Error) -> u8 {
    match e.cause() {
        MyOption::Some(inner) => inner.code(),
        MyOption::None => 0,
    }
}

fn read_iter(it: &Iter<Item = u8>) -> u8 {
    it.next_item()
}

fn read_borrowed<'a>(b: &Borrowed<'a>) -> u8 {
    *b.peek()
}

fn read_borrowed_elided(b: &Borrowed) -> u8 {
    *b.peek()
}

fn accept_holder(_h: Holder<Error>) -> u8 {
    5
}

fn accept_sync(_s: &Sync) -> u8 {
    3
}

#[no_mangle]
pub extern \"C\" fn main() -> i32 {
    let g = GlobError { error: IoError { val: 40 } };
    let e: &Error = &g;
    let c = read_cause(e);
    let ci = ConstIter { v: 7 };
    let i = read_iter(&ci);
    let x: u8 = 11;
    let sb = SliceBorrow { r: &x };
    let b1 = read_borrowed(&sb);
    let b2 = read_borrowed_elided(&sb);
    let h = Holder { ptr: e as *const Error };
    let hv = accept_holder(h);
    let sv = accept_sync(&x);
    // 40 + 7 + 11 + 11 + 5 + 3 = 77
    (c + i + b1 + b2 + hv + sv) as i32
}
";

/// Minimal `no_core` program (no prelude needed) covering bare trait objects in signatures,
/// generic args, associated type bindings, parenthesized Fn-style sugar, and HRTBs.
const MINIMAL_PROGRAM: &str = "#![feature(no_core, lang_items, auto_traits)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"unsize\"]
trait Unsize<T: ?Sized> {}

#[lang = \"coerce_unsized\"]
trait CoerceUnsized<T: ?Sized> {}

#[lang = \"sync\"]
unsafe auto trait Sync {}

enum MyOption<T> {
    None,
    Some(T),
}

trait Error {
    fn code(&self) -> u8;
    fn cause(&self) -> MyOption<&Error> {
        MyOption::None
    }
}

struct IoError {
    val: u8,
}

impl Error for IoError {
    fn code(&self) -> u8 {
        self.val
    }
}

struct GlobError {
    error: IoError,
}

impl Error for GlobError {
    fn code(&self) -> u8 {
        self.error.code()
    }

    fn cause(&self) -> MyOption<&Error> {
        MyOption::Some(&self.error)
    }
}

trait Callable<Args> {
    type Output;
}

trait Borrowed<'a> {
    fn peek(&self) -> &'a u8;
}

trait Iter {
    type Item;
}

struct Holder<T: ?Sized> {
    _p: *const T,
}

fn takes_holder(_h: Holder<Error>) {}
fn takes_iter<'a>(_it: &Iter<Item = &'a u8>) {}
fn takes_callable(_f: &Callable(&u8) -> &u8) {}
fn takes_hrtb(_b: &for<'a> Borrowed<'a>) {}
fn takes_sync(_s: &Sync) {}

fn main() {
    let _f1: fn(Holder<Error>) = takes_holder;
    let _f2: fn(&Iter<Item = &u8>) = takes_iter;
    let _f3: fn(&Callable(&u8) -> &u8) = takes_callable;
    let _f4: fn(&for<'a> Borrowed<'a>) = takes_hrtb;
    let _f5: fn(&Sync) = takes_sync;
}
";

fn run_for_edition(edition: &'static str) -> Result<u8, String> {
    let new = NewFront { package: "glob_bare", edition, shelf: None, overflow: blaze_build::OverflowChecks::On };
    let files = [
        SourceFile { path: "src/main.rs", text: GLOB_RUN_PROGRAM },
        SourceFile { path: "src/prelude.rs", text: PRELUDE },
    ];
    run_with(&files, "src/main.rs", None, FrontChoice::New(new), blaze_own::testing::heap().shared())
        .map_err(|e| format!("{e}"))
}

fn with_minimal(edition: &'static str, f: impl FnOnce(&blaze_scope::resolve::Names<'_>, &[SourceFile<'_>])) {
    let files = [SourceFile { path: "src/main.rs", text: MINIMAL_PROGRAM }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "bare_min", edition, shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
        .unwrap_or_else(|e| panic!("ProgramTexts({edition}): {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("Inputs({edition}): {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve({edition}): {e}"));
    f(&names, &files);
}

fn key_of(names: &blaze_scope::resolve::Names<'_>, ns: Namespace, name: &'static str) -> blaze_typeck::ItemKey {
    let segments: &[&str] = &[name];
    let written = Written { kind: PathKind::Plain, segments };
    let found = names::path(names, names.root(0), ns, written, Alloc::global()).unwrap_or_else(|e| panic!("`{name}`: {e:?}"));
    checker_of(found.whole(1).unwrap_or_else(|| panic!("`{name}` resolves whole")).key)
}

/// Finds `<impl Error for GlobError>::cause` in crate 0's solved names.
fn glob_cause_key(names: &blaze_scope::resolve::Names<'_>) -> (blaze_id::ItemKey, blaze_typeck::ItemKey) {
    let mut found = None;
    names.each_item(0, &mut |lib, _module| {
        if names.kind_of_item(lib) == Some(ItemKind::Impl) {
            if let Some(text) = names.item_text(lib)
                && text.text.contains("impl Error for GlobError")
            {
                let ck = checker_of(lib);
                found = Some((lib, member_key_of(ck, MemberKind::Fn, "cause")));
            }
        }
    });
    found.expect("`impl Error for GlobError` exists in crate 0")
}

/// Rust 2015 and Rust 2018 build and execute the exact `glob 0.3.3` `impl Error for GlobError {
/// fn cause(&self) -> Option<&Error> }` pattern (including vtable construction and dynamic
/// dispatch through `&Error`, plus `Holder<Error>`, `&Iter<Item = u8>`, `&Borrowed<'a>`,
/// `&Borrowed`, and `&Sync`).
#[test]
fn glob_error_cause_bare_trait_object_runs_in_2015_and_2018() {
    for edition in ["2015", "2018"] {
        let got = run_for_edition(edition).unwrap_or_else(|e| panic!("edition {edition} failed: {e}"));
        assert_eq!(got, 77, "edition {edition}: dynamic dispatch over bare trait objects");
    }
}

/// Both `Front2Program` and `LazyProgram` (and head region laying) accept all bare trait object
/// forms in Rust 2015 and Rust 2018: `&Error`, `Holder<Error>`, `&Iter<Item = &'a u8>`,
/// `&Callable(&u8) -> &u8`, `&for<'a> Borrowed<'a>`, and `&Sync`, with zero `fn_gaps` and clean
/// head regions on `<impl Error for GlobError>::cause`.
#[test]
fn bare_trait_object_heads_regions_and_lazy_program_succeed_in_2015_and_2018() {
    for edition in ["2015", "2018"] {
        with_minimal(edition, |names, files| {
            let (wf, lf, hf) = (
                blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame"),
                blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame"),
                blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame"),
            );
            let whole = Front2Program::new(names, &wf, files, Alloc::global())
                .unwrap_or_else(|e| panic!("Front2Program ({edition}): {e}"));
            let main = key_of(names, Namespace::Values, "main");
            let error_trait = key_of(names, Namespace::Types, "Error");
            let mut lazy = LazyProgram::new(names, &lf, files, Some(main), blaze_own::testing::heap().alloc())
                .unwrap_or_else(|e| panic!("LazyProgram ({edition}): {e}"));
            lazy.check_one(main).unwrap_or_else(|e| panic!("check_one(main) ({edition}): {e}")).expect("main has a body");
            let (impl_lib, cause_key) = glob_cause_key(names);
            lazy.check_one(cause_key)
                .unwrap_or_else(|e| panic!("check_one(<impl Error for GlobError>::cause) ({edition}): {e}"))
                .expect("cause has a body");
            assert!(whole.object_trait(error_trait).is_some(), "whole built object table for Error ({edition})");
            assert!(lazy.object_trait(error_trait).is_some(), "lazy built object table for Error ({edition})");

            // Head regions of `<impl Error for GlobError>::cause` are laid cleanly (no refusal).
            let mut front = Front2::new(names, &hf, files, Alloc::global()).unwrap_or_else(|e| panic!("front2 ({edition}): {e}"));
            let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());
            store.ensure(names, &mut front.keys, Want::Impl(impl_lib)).unwrap_or_else(|e| panic!("Want::Impl ({edition}): {e:?}"));
            store
                .ensure(names, &mut front.keys, Want::MemberFn(impl_lib, cause_key))
                .unwrap_or_else(|e| panic!("Want::MemberFn(cause) ({edition}): {e:?}"));
            assert!(store.fns.contains_key(&cause_key), "`cause` head filed in store ({edition})");
            assert!(store.fn_gaps.get(&cause_key).is_none(), "`cause` has no fn_gaps entry ({edition}): {:?}", store.fn_gaps.get(&cause_key));
            assert!(store.regions.made(cause_key).is_some(), "`cause` regions laid cleanly ({edition})");
            // Also verify head & region laying for every bare trait object form (one-name generic
            // arg, associated type binding, parenthesized Fn sugar, HRTB, auto trait).
            for fn_name in ["takes_holder", "takes_iter", "takes_callable", "takes_hrtb", "takes_sync"] {
                let fk = key_of(names, Namespace::Values, fn_name);
                let lib = front.keys.library_in(names, fk).unwrap_or_else(|| panic!("lib key of `{fn_name}`"));
                store
                    .ensure(names, &mut front.keys, Want::Fn(lib))
                    .unwrap_or_else(|e| panic!("Want::Fn({fn_name}) ({edition}): {e:?}"));
                assert!(store.fns.contains_key(&fk), "`{fn_name}` head filed ({edition})");
                assert!(store.regions.made(fk).is_some(), "`{fn_name}` regions laid cleanly ({edition})");
            }
        });
    }
}

/// Negative control (Rule 23): in Rust 2021 and Rust 2024, bare trait objects without `dyn` are
/// invalid (E0782). `run_with` must refuse the program, and `LazyProgram` must surface the
/// `fn_gaps` head refusal naming E0782 instead of the opaque `has no head in the store` (`NoHead`).
#[test]
fn bare_trait_object_refused_in_2021_and_2024_with_head_reason() {
    for edition in ["2021", "2024"] {
        let err = run_for_edition(edition).expect_err("bare trait object must be refused in 2021/2024");
        assert!(err.contains("E0782"), "edition {edition} refusal should name E0782, got: {err}");
        assert!(
            !err.contains("has no head in the store"),
            "edition {edition} refusal must surface fn_gaps instead of opaque NoHead, got: {err}"
        );

        with_minimal(edition, |names, files| {
            let lf = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
            let main = key_of(names, Namespace::Values, "main");
            let mut lazy = LazyProgram::new(names, &lf, files, Some(main), blaze_own::testing::heap().alloc())
                .unwrap_or_else(|e| panic!("LazyProgram ({edition}): {e}"));
            let (_, cause_key) = glob_cause_key(names);
            let err = lazy.head_row(cause_key).expect_err("head_row(cause) must fail in 2021/2024");
            let msg = format!("{err}");
            assert!(msg.contains("E0782"), "head_row({edition}) should surface E0782 from fn_gaps, got: {msg}");
            assert!(!msg.contains("has no head in the store"), "head_row({edition}) must not hide fn_gaps behind NoHead, got: {msg}");
        });
    }
}

struct CrossFiles<'t> {
    root: &'t str,
    dep: &'t str,
}

impl<'t> SourceTexts<'t> for CrossFiles<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        match (krate, path) {
            (0, "/root/src/main.rs") => Some(self.root),
            (1, "/dep/src/lib.rs") => Some(self.dep),
            _ => None,
        }
    }
}

fn cross_edition_check(dep_edition: edition::Edition, root_edition: edition::Edition, fn_name: &'static str) -> Result<(), String> {
    let dep = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
pub trait Sized {}

pub trait Err {
    fn code(&self) -> u8;
}

#[macro_export]
macro_rules! make_bare_fn {
    ($name:ident) => {
        pub fn $name(_e: &dep::Err) {}
    };
}
";
    let root = format!(
        "#![feature(no_core)]
#![no_core]

dep::make_bare_fn!({fn_name});
"
    );
    let tc = Toolchain::none(Alloc::global());
    let cfg = tc.program_cfg(blaze_build::Panic::Unwind);
    let dep_pkg = PackageEdition { syntax: dep_edition, blaze: None };
    let root_pkg = PackageEdition { syntax: root_edition, blaze: None };
    let crates = [
        CrateInput {
            name: "root",
            key: CrateItemKey(ContentId::of("front2_bare_trait_objects cross root", fn_name.as_bytes())),
            edition: syntax_of(root_pkg),
            implicit: implicit(root_pkg, &blaze_unit_graph::UnitKind::Lib),
            root: "/root/src/main.rs",
            shown_from: "/root/",
            features: &[],
            deps: &[("dep", 1)],
            cfg: &cfg,
            script: None,
        },
        CrateInput {
            name: "dep",
            key: CrateItemKey(ContentId::of("front2_bare_trait_objects cross dep", fn_name.as_bytes())),
            edition: syntax_of(dep_pkg),
            implicit: implicit(dep_pkg, &blaze_unit_graph::UnitKind::Lib),
            root: "/dep/src/lib.rs",
            shown_from: "/dep/",
            features: &[],
            deps: &[],
            cfg: &cfg,
            script: None,
        },
    ];
    let files = CrossFiles { root: &root, dep };
    let names = resolve_crates(&crates, Sysroot::default(), &files, &Serial, blaze_own::testing::bump())
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let sources = [SourceFile { path: "src/main.rs", text: &root }];
    let mut front = Front2::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());
    let fk = key_of(&names, Namespace::Values, fn_name);
    let lib = front.keys.library_in(&names, fk).unwrap_or_else(|| panic!("lib key of `{fn_name}`"));
    store.ensure(&names, &mut front.keys, Want::Fn(lib)).map_err(|e| format!("{e:?}"))?;
    assert!(store.regions.made(fk).is_some(), "`{fn_name}` regions laid cleanly");
    Ok(())
}

/// Finding 1: cross-edition macros honor the emitted path token's edition mark:
/// - a Rust 2015 macro emitting `&dep::Err` into a Rust 2021 crate succeeds;
/// - a Rust 2021 macro emitting `&dep::Err` into a Rust 2015 crate is rejected with E0782.
#[test]
fn cross_edition_macro_bare_trait_object_follows_token_edition() {
    cross_edition_check(edition::Edition::Edition2015, edition::Edition::Edition2021, "from_2015_in_2021")
        .unwrap_or_else(|e| panic!("2015 macro emitting bare trait object into 2021 crate must succeed: {e}"));
    let err = cross_edition_check(edition::Edition::Edition2021, edition::Edition::Edition2015, "from_2021_in_2015")
        .expect_err("2021 macro emitting bare trait object into 2015 crate must fail with E0782");
    assert!(err.contains("E0782"), "expected E0782 for 2021 macro in 2015 crate, got: {err}");
}

/// Findings 2, 3, 4, 5, 6, and 7:
/// - Ordinary type alias `type Obj<'b> = dyn Tr + 'b` in `&'a Obj<'b>` returns `LifeRefusal::Alias`
///   from `fn_life` instead of overwriting `'b` with `'a`, while `store.regions` lays it cleanly.
/// - HRTB `&for<'a> Borrowed<'a>` and `*const for<'a> Borrowed<'a>` align cleanly in `fn_life` and
///   `adt_life` without `Unaligned`.
/// - Plain bare trait object behind `*const Error` defaults to `'static` in `adt_life` (`Ctx::Other`).
/// - Parenthesized bare `fn elide_fn(x: &Callable0()) -> &u8` preserves unambiguous output elision.
/// - Bare trait alias `trait Alias<'a> = Borrowed<'a>;` lays its lifetime slots cleanly.
/// - Bare trait with omitted defaulted lifetime argument `trait Defaulted<T = &'static u8>` and
///   `trait DefaultedLt<'a, T = &'a u8>` lays its default argument's region slots cleanly.
#[test]
fn bare_trait_object_lifetimes_aliases_hrtb_raw_ptr_fn_sugar_and_defaults() {
    const EDGE_PROGRAM: &str = "#![feature(no_core, lang_items, trait_alias)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

trait Error {
    fn code(&self) -> u8;
}

trait Tr {}
type Obj<'b> = dyn Tr + 'b;

trait Borrowed<'a> {
    fn peek(&self) -> &'a u8;
}

trait Alias<'a> = Borrowed<'a>;

trait Callable0<Args> {
    type Output;
}

trait Defaulted<T = &'static u8> {}
trait DefaultedLt<'a, T = &'a u8> {}

struct RawHolder {
    p: *const Error,
}

struct HrtbHolder {
    p: *const for<'a> Borrowed<'a>,
}

fn alias_ref<'a, 'b>(_x: &'a Obj<'b>) {}
fn hrtb_elide(_b: &for<'a> Borrowed<'a>) -> &u8 { loop {} }
fn fn_sugar_elide(_f: &Callable0()) -> &u8 { loop {} }
fn takes_alias<'a>(_a: &Alias<'a>) {}
fn takes_defaulted(_d: &Defaulted) {}
fn takes_defaulted_lt<'x>(_d: &DefaultedLt<'x>) {}
";
    let files = [SourceFile { path: "src/main.rs", text: EDGE_PROGRAM }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "bare_edges", edition: "2015", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
        .unwrap_or_else(|e| panic!("ProgramTexts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("Inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());

    // Finding 2: ordinary type alias `type Obj<'b> = dyn Tr + 'b` must not enter bare-object arm.
    let alias_fn = key_of(&names, Namespace::Values, "alias_ref");
    let alias_lib = front.keys.library_in(&names, alias_fn).expect("alias_ref lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(alias_lib)).unwrap_or_else(|e| panic!("alias_ref: {e:?}"));
    assert!(store.regions.made(alias_fn).is_some(), "alias_ref regions laid cleanly");
    let alias_life = store.fn_life(&names, &front.keys, alias_fn);
    assert!(
        matches!(alias_life, Err(LifeRefusal::Alias { .. })),
        "ordinary type alias to `dyn Tr + 'b` must return LifeRefusal::Alias, got: {alias_life:?}"
    );

    // Finding 3: `&for<'a> Borrowed<'a>` and `*const for<'a> Borrowed<'a>` align cleanly.
    let hrtb_fn = key_of(&names, Namespace::Values, "hrtb_elide");
    let hrtb_lib = front.keys.library_in(&names, hrtb_fn).expect("hrtb_elide lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(hrtb_lib)).unwrap_or_else(|e| panic!("hrtb_elide: {e:?}"));
    assert!(store.regions.made(hrtb_fn).is_some(), "hrtb_elide regions laid cleanly");
    let hrtb_life = store.fn_life(&names, &front.keys, hrtb_fn).unwrap_or_else(|e| panic!("hrtb_elide fn_life: {e:?}"));
    assert_eq!(
        hrtb_life.regions.as_slice(),
        &[Region::Anon(0), Region::Anon(0), Region::Anon(0)],
        "hrtb_elide: input & + object + elided return all share Anon(0)"
    );
    let hrtb_adt = key_of(&names, Namespace::Types, "HrtbHolder");
    let hrtb_adt_lib = front.keys.library_in(&names, hrtb_adt).expect("HrtbHolder lib key");
    store.ensure(&names, &mut front.keys, Want::Adt(hrtb_adt_lib)).unwrap_or_else(|e| panic!("HrtbHolder: {e:?}"));
    let hrtb_adt_life = store.adt_life(&names, &front.keys, hrtb_adt_lib).unwrap_or_else(|e| panic!("HrtbHolder adt_life: {e:?}"));
    assert_eq!(hrtb_adt_life.fields.as_slice(), &[Region::Static], "HrtbHolder `*const for<'a> Borrowed<'a>` defaults to 'static");

    // Finding 4: plain bare trait object behind `*const Error` defaults to 'static (`Ctx::Other`).
    let raw_adt = key_of(&names, Namespace::Types, "RawHolder");
    let raw_adt_lib = front.keys.library_in(&names, raw_adt).expect("RawHolder lib key");
    store.ensure(&names, &mut front.keys, Want::Adt(raw_adt_lib)).unwrap_or_else(|e| panic!("RawHolder: {e:?}"));
    let raw_adt_life = store.adt_life(&names, &front.keys, raw_adt_lib).unwrap_or_else(|e| panic!("RawHolder adt_life: {e:?}"));
    assert_eq!(raw_adt_life.fields.as_slice(), &[Region::Static], "RawHolder `*const Error` defaults to 'static");

    // Finding 5: `fn fn_sugar_elide(f: &Callable0()) -> &u8` preserves unambiguous output elision.
    let sugar_fn = key_of(&names, Namespace::Values, "fn_sugar_elide");
    let sugar_lib = front.keys.library_in(&names, sugar_fn).expect("fn_sugar_elide lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(sugar_lib)).unwrap_or_else(|e| panic!("fn_sugar_elide: {e:?}"));
    assert!(store.regions.made(sugar_fn).is_some(), "fn_sugar_elide regions laid cleanly");
    let sugar_life = store.fn_life(&names, &front.keys, sugar_fn).unwrap_or_else(|e| panic!("fn_sugar_elide fn_life: {e:?}"));
    assert_eq!(
        sugar_life.regions.as_slice(),
        &[Region::Anon(0), Region::Anon(0), Region::Anon(0)],
        "fn_sugar_elide: `&Callable0()` must not set outside=true; output &u8 elides to Anon(0)"
    );

    // Finding 6: bare trait alias `trait Alias<'a> = Borrowed<'a>; fn takes_alias<'a>(_: &Alias<'a>)`
    let ta_fn = key_of(&names, Namespace::Values, "takes_alias");
    let ta_lib = front.keys.library_in(&names, ta_fn).expect("takes_alias lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(ta_lib)).unwrap_or_else(|e| panic!("takes_alias: {e:?}"));
    assert!(store.regions.made(ta_fn).is_some(), "takes_alias regions laid cleanly");

    // Finding 7: omitted defaulted trait arguments with lifetimes (`Defaulted` and `DefaultedLt<'x>`)
    for fn_name in ["takes_defaulted", "takes_defaulted_lt"] {
        let fk = key_of(&names, Namespace::Values, fn_name);
        let flib = front.keys.library_in(&names, fk).unwrap_or_else(|| panic!("{fn_name} lib key"));
        store.ensure(&names, &mut front.keys, Want::Fn(flib)).unwrap_or_else(|e| panic!("{fn_name}: {e:?}"));
        assert!(store.regions.made(fk).is_some(), "`{fn_name}` regions laid cleanly");
    }
}

/// Round 2 Finding 1: a Rust 2015 macro emitting `&$crate::Tr` and `*const $crate::Tr` into a
/// Rust 2021 crate that shadows `Tr` resolves `$crate::Tr` via token marks in `lifetimes.rs`
/// (`fn_life` and `adt_life`) identically to `types.rs`.
#[test]
fn marked_dollar_crate_bare_object_resolves_in_lifetimes_despite_call_site_shadow() {
    let dep = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
pub trait Sized {}

pub trait Tr {
    fn code(&self) -> u8;
}

#[macro_export]
macro_rules! make_dollar_crate_items {
    () => {
        pub fn via_macro(_e: &$crate::Tr) -> &u8 {
            loop {}
        }
        pub struct ViaMacroPtr {
            pub p: *const $crate::Tr,
        }
    };
}
";
    let root = "#![feature(no_core)]
#![no_core]

// Shadow `Tr` at the invocation site with a struct to prove `$crate::Tr` does not resolve here.
pub struct Tr;

dep::make_dollar_crate_items!();
";
    let tc = Toolchain::none(Alloc::global());
    let cfg = tc.program_cfg(blaze_build::Panic::Unwind);
    let dep_pkg = PackageEdition { syntax: edition::Edition::Edition2015, blaze: None };
    let root_pkg = PackageEdition { syntax: edition::Edition::Edition2021, blaze: None };
    let crates = [
        CrateInput {
            name: "root",
            key: CrateItemKey(ContentId::of("front2_bare_trait_objects r2 root", b"shadow")),
            edition: syntax_of(root_pkg),
            implicit: implicit(root_pkg, &blaze_unit_graph::UnitKind::Lib),
            root: "/root/src/main.rs",
            shown_from: "/root/",
            features: &[],
            deps: &[("dep", 1)],
            cfg: &cfg,
            script: None,
        },
        CrateInput {
            name: "dep",
            key: CrateItemKey(ContentId::of("front2_bare_trait_objects r2 dep", b"shadow")),
            edition: syntax_of(dep_pkg),
            implicit: implicit(dep_pkg, &blaze_unit_graph::UnitKind::Lib),
            root: "/dep/src/lib.rs",
            shown_from: "/dep/",
            features: &[],
            deps: &[],
            cfg: &cfg,
            script: None,
        },
    ];
    let files = CrossFiles { root, dep };
    let names = resolve_crates(&crates, Sysroot::default(), &files, &Serial, blaze_own::testing::bump())
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let sources = [SourceFile { path: "src/main.rs", text: root }];
    let mut front = Front2::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());

    let fn_key = key_of(&names, Namespace::Values, "via_macro");
    let fn_lib = front.keys.library_in(&names, fn_key).expect("via_macro lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(fn_lib)).unwrap_or_else(|e| panic!("via_macro: {e:?}"));
    assert!(store.regions.made(fn_key).is_some(), "via_macro regions laid cleanly");
    let fn_life = store.fn_life(&names, &front.keys, fn_key).unwrap_or_else(|e| panic!("via_macro fn_life: {e:?}"));
    assert_eq!(
        fn_life.regions.as_slice(),
        &[Region::Anon(0), Region::Anon(0), Region::Anon(0)],
        "`&$crate::Tr` resolves to `dep::Tr` and preserves elision"
    );

    let adt_key = key_of(&names, Namespace::Types, "ViaMacroPtr");
    let adt_lib = front.keys.library_in(&names, adt_key).expect("ViaMacroPtr lib key");
    store.ensure(&names, &mut front.keys, Want::Adt(adt_lib)).unwrap_or_else(|e| panic!("ViaMacroPtr: {e:?}"));
    let adt_life = store.adt_life(&names, &front.keys, adt_lib).unwrap_or_else(|e| panic!("ViaMacroPtr adt_life: {e:?}"));
    assert_eq!(adt_life.fields.as_slice(), &[Region::Static], "`*const $crate::Tr` resolves to `dep::Tr` and defaults to 'static");
}

/// Round 2 Findings 2 & 3:
/// - Lifetime-free trait aliases (`trait PlainAlias = PlainTr;`) are quiet inputs so
///   `fn f(x: &PlainAlias) -> &u8` preserves unambiguous output elision, while aliases carrying
///   lifetimes (`trait StaticArgAlias = Borrowed<'static>;`) are not treated as quiet.
/// - HRTB binders carry into nested type arguments (`fn f(x: &for<'a> GenTr<&'a u8>) -> &u8`
///   elides cleanly, while free `'x` in `&for<'a> GenTr<&'x u8>` still blocks elision).
#[test]
fn lifetime_free_trait_alias_and_nested_hrtb_args_preserve_elision() {
    const PROG: &str = "#![feature(no_core, lang_items, trait_alias)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

trait PlainTr {}
trait PlainAlias = PlainTr;

trait Borrowed<'a> {}
trait StaticArgAlias = Borrowed<'static>;

trait GenTr<T> {}

fn alias_elide(_x: &PlainAlias) -> &u8 { loop {} }
fn static_alias_elide(_x: &StaticArgAlias) -> &u8 { loop {} }
fn hrtb_nested_elide(_x: &for<'a> GenTr<&'a u8>) -> &u8 { loop {} }
fn hrtb_free_arg<'x>(_x: &for<'a> GenTr<&'x u8>) -> &u8 { loop {} }
";
    let files = [SourceFile { path: "src/main.rs", text: PROG }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "bare_r2_elide", edition: "2015", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
        .unwrap_or_else(|e| panic!("ProgramTexts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("Inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());

    // Lifetime-free trait alias preserves elision.
    let alias_fn = key_of(&names, Namespace::Values, "alias_elide");
    let alias_lib = front.keys.library_in(&names, alias_fn).expect("alias_elide lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(alias_lib)).unwrap_or_else(|e| panic!("alias_elide: {e:?}"));
    let alias_life = store.fn_life(&names, &front.keys, alias_fn).unwrap_or_else(|e| panic!("alias_elide fn_life: {e:?}"));
    assert_eq!(alias_life.regions.as_slice(), &[Region::Anon(0), Region::Anon(0), Region::Anon(0)]);

    // Trait alias naming `'static` is not quiet (negative control: E0106).
    let static_alias_fn = key_of(&names, Namespace::Values, "static_alias_elide");
    let static_alias_lib = front.keys.library_in(&names, static_alias_fn).expect("static_alias_elide lib key");
    let static_alias_err = store
        .ensure(&names, &mut front.keys, Want::Fn(static_alias_lib))
        .expect_err("`trait StaticArgAlias = Borrowed<'static>` must not be treated as quiet");
    let static_alias_msg = format!("{static_alias_err:?}");
    assert!(
        static_alias_msg.contains("E0106"),
        "`trait StaticArgAlias = Borrowed<'static>` must fail elision with E0106, got: {static_alias_msg}"
    );

    // HRTB binder carried into nested type argument `&'a u8` preserves elision.
    let nested_fn = key_of(&names, Namespace::Values, "hrtb_nested_elide");
    let nested_lib = front.keys.library_in(&names, nested_fn).expect("hrtb_nested_elide lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(nested_lib)).unwrap_or_else(|e| panic!("hrtb_nested_elide: {e:?}"));
    let nested_life = store.fn_life(&names, &front.keys, nested_fn).unwrap_or_else(|e| panic!("hrtb_nested_elide fn_life: {e:?}"));
    assert_eq!(nested_life.regions.as_slice(), &[Region::Anon(0), Region::Anon(0), Region::Anon(0)]);

    // Negative control: free lifetime `'x` inside `for<'a> GenTr<&'x u8>` still blocks elision (E0106).
    let free_fn = key_of(&names, Namespace::Values, "hrtb_free_arg");
    let free_lib = front.keys.library_in(&names, free_fn).expect("hrtb_free_arg lib key");
    let free_err = store
        .ensure(&names, &mut front.keys, Want::Fn(free_lib))
        .expect_err("free `'x` inside `for<'a> GenTr<&'x u8>` must block elision");
    let free_msg = format!("{free_err:?}");
    assert!(
        free_msg.contains("E0106"),
        "free `'x` inside `for<'a> GenTr<&'x u8>` must fail elision with E0106, got: {free_msg}"
    );
}

/// Round 2 Finding 4: `trait_is_self_static` inspects every queued supertrait up to the 64-trait
/// bound (finding `Self: 'static` on the 64th trait in the queue) and refuses with
/// `NotLaidWhy::DynRegion` rather than silently defaulting to `false` when the graph exceeds 64
/// traits without finding `Self: 'static`.
#[test]
fn supertrait_self_static_walk_inspects_all_64_queued_and_refuses_overflow() {
    let mut src = String::from("#![feature(no_core, lang_items)]\n#![no_core]\n#[lang = \"sized\"] trait Sized {}\n");
    for i in 0..64 {
        src.push_str(&format!("trait S{i} {{}}\n"));
    }
    src.push_str("trait StaticTail: 'static {}\n");
    // Wide64 has 62 non-static supertraits + StaticTail = 64 distinct traits total (Wide64 + 63).
    // `work.len()` hits 64 before `StaticTail` is popped, so inspecting all queued items must find `'static`.
    src.push_str("trait Wide64: ");
    for i in 0..62 {
        src.push_str(&format!("S{i} + "));
    }
    src.push_str("StaticTail {}\n");
    // Wide65 has 64 non-static supertraits = 65 distinct traits total (exceeds cap 64 -> must refuse).
    src.push_str("trait Wide65: ");
    for i in 0..64 {
        if i > 0 {
            src.push_str(" + ");
        }
        src.push_str(&format!("S{i}"));
    }
    src.push_str(" {}\n");
    src.push_str("fn takes_wide64<'a>(_x: &'a Wide64) {}\n");
    src.push_str("fn takes_wide65<'a>(_x: &'a Wide65) {}\n");

    let files = [SourceFile { path: "src/main.rs", text: &src }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "bare_r2_static", edition: "2015", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
        .unwrap_or_else(|e| panic!("ProgramTexts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("Inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());

    // Wide64: 64th trait (`StaticTail: 'static`) is found -> object lifetime is `'static`, NOT `'a`.
    let k64 = key_of(&names, Namespace::Values, "takes_wide64");
    let lib64 = front.keys.library_in(&names, k64).expect("takes_wide64 lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(lib64)).unwrap_or_else(|e| panic!("takes_wide64: {e:?}"));
    let r64 = store.regions.made(k64).expect("takes_wide64 regions laid cleanly");
    let sig64 = r64.regions().sig_view().expect("takes_wide64 sig_view");
    let input0 = sig64.col.child(0).expect("takes_wide64 input 0");
    assert_eq!(
        input0,
        &[blaze_typeck::regions::Region::Param(0), blaze_typeck::regions::Region::Static],
        "64th queued supertrait `StaticTail: 'static` must make object lifetime 'static, got: {input0:?}"
    );

    // Wide65: 65 distinct non-static traits — with PR #2812's O((V+E)*S) RFC 1156 resolver over
    // pre-filed `trait_super_lifetimes`, wide hierarchies beyond 64 traits are traversed completely
    // without arbitrary truncation, resolving `&'a Wide65` to `[Param(0), Param(0)]`.
    let k65 = key_of(&names, Namespace::Values, "takes_wide65");
    let lib65 = front.keys.library_in(&names, k65).expect("takes_wide65 lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(lib65)).unwrap_or_else(|e| panic!("takes_wide65: {e:?}"));
    let r65 = store.regions.made(k65).expect("takes_wide65 regions laid cleanly");
    assert_eq!(
        r65.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
        &[blaze_typeck::regions::Region::Param(0), blaze_typeck::regions::Region::Param(0)]
    );
}

/// Round 3 Finding 1: associated item constraints (`Item = u8`) must be rejected on nominal types
/// and type aliases (E0229) instead of silently appending the RHS as a positional argument.
#[test]
fn assoc_binding_on_nominal_type_path_is_rejected() {
    for edition in ["2015", "2021"] {
        const PROG: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

struct S<T>(T);
type Bad = S<Item = u8>;
type Alias<T> = S<T>;
type BadAlias = Alias<Item = u8>;

fn uses_direct(_x: S<Item = u8>) {}
fn uses_bad(_x: Bad) {}
fn uses_bad_alias(_x: BadAlias) {}
";
        let files = [SourceFile { path: "src/main.rs", text: PROG }];
        let tc = Toolchain::none(Alloc::global());
        let new = NewFront { package: "bare_r3_assoc_nominal", edition, shelf: None, overflow: blaze_build::OverflowChecks::On };
        let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
            .unwrap_or_else(|e| panic!("edition {edition} ProgramTexts: {e}"));
        let user = texts.user();
        let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("edition {edition} Inputs: {e}"));
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("edition {edition} resolve: {e}"));
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
        let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("edition {edition} front2: {e}"));
        let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());

        for name in ["uses_direct", "uses_bad", "uses_bad_alias"] {
            let k = key_of(&names, Namespace::Values, name);
            let lib = front.keys.library_in(&names, k).expect("lib key");
            let err = store
                .ensure(&names, &mut front.keys, Want::Fn(lib))
                .expect_err(&format!("edition {edition}: `{name}` with `Item = u8` on nominal/alias must be rejected"));
            let msg = format!("{err:?}");
            assert!(
                msg.contains("E0229") || msg.contains("associated item constraints"),
                "edition {edition}: `{name}` refusal should mention E0229, got: {msg}"
            );
        }
    }
}

/// Round 3 Findings 2 & 3:
/// - Unique trait-declared object-lifetime bound (`trait Tr<'a>: 'a {}`) resolves
///   `struct S<'a> { p: *const Tr<'a> }` to `'a` instead of `ObjectDefault`, while multiple
///   distinct bounds (`trait Two<'a, 'b>: 'a + 'b {}`) still refuse `ObjectDefault`.
/// - Output elision succeeds when a bare object's written lifetime matches the outer reference
///   (`fn same<'a>(x: &'a Tr<'a>) -> &u8`), and stays undecided when they differ
///   (`fn diff<'a, 'b>(x: &'a Tr<'b>) -> &u8`).
#[test]
fn bare_object_unique_trait_bound_and_matching_elision_in_lifetimes() {
    const PROG: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

trait Tr<'a>: 'a {}
trait Plain<'a> {}
trait Two<'a, 'b>: 'a + 'b {}

struct PtrUnique<'a> {
    p: *const Tr<'a>,
}

struct PtrUnbounded<'a> {
    p: *const Plain<'a>,
}

struct PtrAmbig<'a, 'b> {
    p: *const Two<'a, 'b>,
}

fn same_lt<'a>(_x: &'a Tr<'a>) -> &u8 { loop {} }
fn diff_lt<'a, 'b>(_x: &'a Tr<'b>) -> &u8 { loop {} }
";
    let files = [SourceFile { path: "src/main.rs", text: PROG }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "bare_r3_life", edition: "2015", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
        .unwrap_or_else(|e| panic!("ProgramTexts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("Inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());

    // Unique bound `'a` on `Tr<'a>: 'a` resolves `*const Tr<'a>` to `'a` (`Region::Named(0)`).
    let uniq_k = key_of(&names, Namespace::Types, "PtrUnique");
    let uniq_lib = front.keys.library_in(&names, uniq_k).expect("PtrUnique lib key");
    store.ensure(&names, &mut front.keys, Want::Adt(uniq_lib)).unwrap_or_else(|e| panic!("PtrUnique: {e:?}"));
    let uniq_life = store.adt_life(&names, &front.keys, uniq_lib).unwrap_or_else(|e| panic!("PtrUnique adt_life: {e:?}"));
    assert_eq!(uniq_life.fields.as_slice(), &[Region::Named(0)]);

    // Unbounded `trait Plain<'a> {}` resolves `*const Plain<'a>` to `'static`.
    let unb_k = key_of(&names, Namespace::Types, "PtrUnbounded");
    let unb_lib = front.keys.library_in(&names, unb_k).expect("PtrUnbounded lib key");
    store.ensure(&names, &mut front.keys, Want::Adt(unb_lib)).unwrap_or_else(|e| panic!("PtrUnbounded: {e:?}"));
    let unb_life = store.adt_life(&names, &front.keys, unb_lib).unwrap_or_else(|e| panic!("PtrUnbounded adt_life: {e:?}"));
    assert_eq!(unb_life.fields.as_slice(), &[Region::Static]);

    // Ambiguous `trait Two<'a, 'b>: 'a + 'b {}` refuses with `ObjectDefault`.
    let amb_k = key_of(&names, Namespace::Types, "PtrAmbig");
    let amb_lib = front.keys.library_in(&names, amb_k).expect("PtrAmbig lib key");
    store.ensure(&names, &mut front.keys, Want::Adt(amb_lib)).unwrap_or_else(|e| panic!("PtrAmbig: {e:?}"));
    let amb_life = store.adt_life(&names, &front.keys, amb_lib);
    assert!(
        matches!(amb_life, Err(LifeRefusal::ObjectDefault { .. })),
        "two distinct trait lifetime bounds must refuse ObjectDefault, got: {amb_life:?}"
    );

    // Matching outer & object lifetime (`&'a Tr<'a> -> &u8`) elides cleanly to `'a`.
    let same_k = key_of(&names, Namespace::Values, "same_lt");
    let same_lib = front.keys.library_in(&names, same_k).expect("same_lt lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(same_lib)).unwrap_or_else(|e| panic!("same_lt: {e:?}"));
    let same_life = store.fn_life(&names, &front.keys, same_k).unwrap_or_else(|e| panic!("same_lt fn_life: {e:?}"));
    assert_eq!(same_life.regions.as_slice(), &[Region::Named(0), Region::Named(0), Region::Named(0)]);

    // Negative control: distinct lifetimes (`&'a Tr<'b> -> &u8`) must fail elision (E0106).
    let diff_k = key_of(&names, Namespace::Values, "diff_lt");
    let diff_lib = front.keys.library_in(&names, diff_k).expect("diff_lt lib key");
    let diff_err = store
        .ensure(&names, &mut front.keys, Want::Fn(diff_lib))
        .expect_err("`fn diff_lt<'a, 'b>(_x: &'a Tr<'b>) -> &u8` must fail elision");
    let diff_msg = format!("{diff_err:?}");
    assert!(
        diff_msg.contains("E0106"),
        "distinct lifetimes in `&'a Tr<'b> -> &u8` must report E0106, got: {diff_msg}"
    );
}

/// Round 3 Findings 4 & 5:
/// - Trait aliases with nested lifetime-bearing type arguments and projection terms
///   (`trait Alias<'a> = Gen<&'a u8>;`, `trait IterAlias<'a> = Iter<Item = &'a u8>;`) lay their
///   nested regions cleanly.
/// - Bare trait objects with supertrait associated-type projections
///   (`trait Super<'a> { type A; } trait Sub<'a>: Super<'a> {} fn f<'a>(_: &Sub<'a, A = &'a u8>) {}`)
///   populate the owner supertrait's lifetime slots cleanly.
#[test]
fn bare_trait_alias_nested_regions_and_supertrait_proj_slots() {
    const PROG: &str = "#![feature(no_core, lang_items, trait_alias)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

trait Gen<T> {}
trait Alias<'a> = Gen<&'a u8>;
trait AliasParam<'a, U> = Gen<(&'a u8, U)>;

trait Iter {
    type Item;
}
trait IterAlias<'a> = Iter<Item = &'a u8>;

trait Super<'a> {
    type A;
}
trait Sub<'a>: Super<'a> {}

fn takes_nested_alias<'a>(_x: &'a Alias<'a>) {}
fn takes_param_alias<'a, 'b>(_x: &'a AliasParam<'a, &'b u8>) {}
fn takes_iter_alias<'a>(_x: &'a IterAlias<'a>) {}
fn takes_super_proj<'a>(_x: &'a Sub<'a, A = &'a u8>) {}
";
    let files = [SourceFile { path: "src/main.rs", text: PROG }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "bare_r3_regions", edition: "2015", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
        .unwrap_or_else(|e| panic!("ProgramTexts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("Inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());

    use blaze_typeck::regions::Region as R;

    // `Alias<'a> = Gen<&'a u8>`: &[Param(0) (&'a), Param(0) (dyn obj), Param(0) (&'a u8)].
    let k1 = key_of(&names, Namespace::Values, "takes_nested_alias");
    let lib1 = front.keys.library_in(&names, k1).expect("takes_nested_alias lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(lib1)).unwrap_or_else(|e| panic!("takes_nested_alias: {e:?}"));
    let r1 = store.regions.made(k1).expect("takes_nested_alias regions laid cleanly");
    assert_eq!(
        r1.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
        &[R::Param(0), R::Param(0), R::Param(0)]
    );

    // `AliasParam<'a, &'b u8> = Gen<(&'a u8, &'b u8)>`: &[Param(0), Param(0), Param(0), Param(1)].
    let k2 = key_of(&names, Namespace::Values, "takes_param_alias");
    let lib2 = front.keys.library_in(&names, k2).expect("takes_param_alias lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(lib2)).unwrap_or_else(|e| panic!("takes_param_alias: {e:?}"));
    let r2 = store.regions.made(k2).expect("takes_param_alias regions laid cleanly");
    assert_eq!(
        r2.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
        &[R::Param(0), R::Param(0), R::Param(0), R::Param(1)]
    );

    // `IterAlias<'a> = Iter<Item = &'a u8>`: &[Param(0), Param(0), Param(0)].
    let k3 = key_of(&names, Namespace::Values, "takes_iter_alias");
    let lib3 = front.keys.library_in(&names, k3).expect("takes_iter_alias lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(lib3)).unwrap_or_else(|e| panic!("takes_iter_alias: {e:?}"));
    let r3 = store.regions.made(k3).expect("takes_iter_alias regions laid cleanly");
    assert_eq!(
        r3.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
        &[R::Param(0), R::Param(0), R::Param(0)]
    );

    // `&'a Sub<'a, A = &'a u8>`: &[Param(0) (&'a), Param(0) (dyn obj), Param(0) (Sub<'a>), Param(0) (Super<'a> ExProj), Param(0) (&'a u8)].
    let k4 = key_of(&names, Namespace::Values, "takes_super_proj");
    let lib4 = front.keys.library_in(&names, k4).expect("takes_super_proj lib key");
    store.ensure(&names, &mut front.keys, Want::Fn(lib4)).unwrap_or_else(|e| panic!("takes_super_proj: {e:?}"));
    let r4 = store.regions.made(k4).expect("takes_super_proj regions laid cleanly");
    assert_eq!(
        r4.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
        &[R::Param(0), R::Param(0), R::Param(0), R::Param(0), R::Param(0)]
    );
}

/// Round 4 Findings 1, 2, 3, and 4:
/// 1. Omitted lifetime-bearing defaults (`trait Defaulted<T = &'static u8> {}` and defaulted trait
///    aliases) are inspected before declaring an object quiet, so `fn f(x: &Defaulted) -> &u8`
///    does not silently elide in `fn_life`, while explicit `fn g(x: &Defaulted<u8>) -> &u8` and
///    lifetime-free defaults (`trait PlainDef<T = u8> {}`) still elide cleanly.
/// 2. Repeated supertraits with distinct lifetime instantiations
///    (`trait Super<'x>: 'x {} trait Sub<'a, 'b>: Super<'a> + Super<'b> {}`) are deduplicated by
///    `(trait_key, substituted_args)`, refusing ambiguity (`ObjectDefault`) regardless of order
///    in BOTH `adt_life` and `store.regions`.
/// 3. Unique trait-declared object-lifetime bounds (`trait Tr<'a>: 'a {}`) apply in the region
///    column (`store.regions.made(...)`) for `struct S<'a> { p: *const Tr<'a> }`, recording
///    `Param(0)` (`'a`) instead of `'static`.
/// 4. Multiple nested trait aliases (`trait Base = Tr; trait Marker = Send; trait Combo = Base + Marker;`)
///    expand cleanly without refusing on the second nested alias.
#[test]
fn round4_omitted_defaults_repeated_supers_region_col_bound_and_multi_nested_aliases() {
    const PROG: &str = "#![feature(no_core, lang_items, trait_alias, auto_traits)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

auto trait Send {}
auto trait Sync {}

trait Defaulted<T = &'static u8> {}
trait PlainDef<T = u8> {}
trait Gen<T> {}
trait AliasDefault = Defaulted;

fn def_elide(_x: &Defaulted) -> &u8 { loop {} }
fn def_explicit_elide(_x: &Defaulted<u8>) -> &u8 { loop {} }
fn plain_def_elide(_x: &PlainDef) -> &u8 { loop {} }
fn alias_def_elide(_x: &AliasDefault) -> &u8 { loop {} }

trait Super<'x>: 'x {}
trait SubAB<'a, 'b>: Super<'a> + Super<'b> {}
trait SubBA<'a, 'b>: Super<'b> + Super<'a> {}
trait UniqueTr<'a>: 'a {}

struct PtrSubAB<'a, 'b> {
    p: *const SubAB<'a, 'b>,
}

struct PtrSubBA<'a, 'b> {
    p: *const SubBA<'a, 'b>,
}

struct PtrUniqueCol<'a> {
    p: *const UniqueTr<'a>,
}

trait Tr {}
trait Base = Tr;
trait Marker = Send;
trait Markers = Marker + Sync;
trait Combo = Base + Marker;
trait ComboRev = Markers + Base;
trait BaseNested<'a> = Gen<&'a u8>;
trait ComboNested<'a> = BaseNested<'a> + Markers;

fn takes_combo(_x: &Combo) {}
fn takes_combo_rev(_x: &ComboRev) {}
fn takes_combo_nested<'a>(_x: &'a ComboNested<'a>) {}
";
    let files = [SourceFile { path: "src/main.rs", text: PROG }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "bare_r4", edition: "2015", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
        .unwrap_or_else(|e| panic!("ProgramTexts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("Inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());

    use blaze_typeck::regions::Region as R;

    // Finding 1: omitted lifetime-bearing default (`T = &'static u8`) is NOT quiet in `lifetimes.rs`.
    for fn_name in ["def_elide", "alias_def_elide"] {
        let k = key_of(&names, Namespace::Values, fn_name);
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("{fn_name}: {e:?}"));
        let life = store.fn_life(&names, &front.keys, k);
        assert!(
            matches!(life, Err(LifeRefusal::ElisionUndecided { .. })),
            "`{fn_name}` with omitted `&'static u8` default must not silently elide, got: {life:?}"
        );
    }
    // Positive controls: explicit `Defaulted<u8>` and lifetime-free `PlainDef<T = u8>` still elide.
    for fn_name in ["def_explicit_elide", "plain_def_elide"] {
        let k = key_of(&names, Namespace::Values, fn_name);
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("{fn_name}: {e:?}"));
        let life = store.fn_life(&names, &front.keys, k).unwrap_or_else(|e| panic!("{fn_name} fn_life: {e:?}"));
        assert_eq!(life.regions.as_slice(), &[Region::Anon(0), Region::Anon(0), Region::Anon(0)]);
    }

    // Finding 2: repeated supertrait instantiations `Super<'a> + Super<'b>` refuse `ObjectDefault`
    // in BOTH orders (`SubAB` and `SubBA`), in both `adt_life` and `store.regions`.
    for adt_name in ["PtrSubAB", "PtrSubBA"] {
        let k = key_of(&names, Namespace::Types, adt_name);
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Adt(lib)).unwrap_or_else(|e| panic!("{adt_name}: {e:?}"));
        let life = store.adt_life(&names, &front.keys, lib);
        assert!(
            matches!(life, Err(LifeRefusal::ObjectDefault { .. })),
            "`{adt_name}` must refuse ObjectDefault for Super<'a> + Super<'b>, got: {life:?}"
        );
        assert!(
            store.regions.made(k).is_none(),
            "`{adt_name}` region column must refuse ObjectDefault for Super<'a> + Super<'b>"
        );
    }

    // Finding 3: `struct PtrUniqueCol<'a> { p: *const UniqueTr<'a> }` records `Param(0)` (`'a`)
    // for the trait object lifetime in the region column (NOT `'static`), agreeing with `adt_life`.
    let uk = key_of(&names, Namespace::Types, "PtrUniqueCol");
    let ulib = front.keys.library_in(&names, uk).expect("PtrUniqueCol lib key");
    store.ensure(&names, &mut front.keys, Want::Adt(ulib)).unwrap_or_else(|e| panic!("PtrUniqueCol: {e:?}"));
    let ulife = store.adt_life(&names, &front.keys, ulib).unwrap_or_else(|e| panic!("PtrUniqueCol adt_life: {e:?}"));
    assert_eq!(ulife.fields.as_slice(), &[Region::Named(0)]);
    let ureg = store.regions.made(uk).expect("PtrUniqueCol regions laid cleanly");
    let field_slots = ureg.regions().fields.first().expect("PtrUniqueCol field 0").2.as_slice();
    assert_eq!(
        field_slots,
        &[R::Param(0), R::Param(0)],
        "PtrUniqueCol `*const UniqueTr<'a>` must record Param(0) (not Static) for object default, got: {field_slots:?}"
    );

    // Finding 4: multiple nested trait aliases (`Combo = Base + Marker`, `ComboRev = Markers + Base`,
    // `ComboNested<'a> = BaseNested<'a> + Markers`) lay cleanly.
    for (fn_name, expected) in [
        ("takes_combo", &[R::Param(0), R::Param(0)][..]),
        ("takes_combo_rev", &[R::Param(0), R::Param(0)][..]),
        ("takes_combo_nested", &[R::Param(0), R::Param(0), R::Param(0)][..]),
    ] {
        let k = key_of(&names, Namespace::Values, fn_name);
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("{fn_name}: {e:?}"));
        let reg = store.regions.made(k).unwrap_or_else(|| panic!("{fn_name} regions unread: {:?}", store.regions.unread(k)));
        assert_eq!(
            reg.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
            expected,
            "{fn_name} input 0 regions"
        );
    }
}

/// Round 5 Findings 1, 2, and 3:
/// 1. Containing ADT parameter object-lifetime bounds (`Holder<'a, Tr>` -> `'a`,
///    `Unbounded<'a, Tr>` -> `'static`, `Ambig<'a, 'b, Tr>` -> refused).
/// 2. Omitted default type arguments declared by bare trait aliases
///    (`trait AliasU8<T = u8> = Gen<T>;`, `trait AliasRef<T = &'static u8> = Gen<T>;`).
/// 3. Lifetime bounds on bare trait aliases (`trait AliasLt<'a> = Tr + 'a;`) in both
///    `fn f<'a>(_: &'a AliasLt<'a>) {}` and `struct S<'a> { p: *const AliasLt<'a> }`.
#[test]
fn round5_adt_param_bounds_trait_alias_defaults_and_alias_lifetime_bounds() {
    const PROG: &str = "#![feature(no_core, lang_items, trait_alias)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

trait Tr {}
trait Gen<T> {}

struct Holder<'a, T: ?Sized + 'a>(&'a T);
struct HolderWhere<'a, T: ?Sized>(&'a T) where T: 'a;
struct Unbounded<'a, T: ?Sized>(&'a (), *const T);
struct Ambig<'a, 'b, T: ?Sized + 'a + 'b>(&'a (), &'b (), *const T);

fn takes_holder<'a>(_x: Holder<'a, Tr>) {}
fn takes_holder_where<'a>(_x: HolderWhere<'a, Tr>) {}
fn takes_unbounded<'a>(_x: Unbounded<'a, Tr>) {}
fn takes_ambig<'a, 'b>(_x: Ambig<'a, 'b, Tr>) {}

trait AliasU8<T = u8> = Gen<T>;
trait AliasRef<T = &'static u8> = Gen<T>;
trait AliasLt<'a> = Tr + 'a;

fn takes_alias_u8(_x: &AliasU8) {}
fn takes_alias_ref(_x: &AliasRef) {}
fn alias_ref_elide(_x: &AliasRef) -> &u8 { loop {} }
fn takes_alias_lt<'a>(_x: &'a AliasLt<'a>) {}

struct PtrAliasLt<'a> {
    p: *const AliasLt<'a>,
}
";
    let files = [SourceFile { path: "src/main.rs", text: PROG }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "bare_r5", edition: "2015", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
        .unwrap_or_else(|e| panic!("ProgramTexts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("Inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());

    use blaze_typeck::regions::Region as R;

    // Finding 1a: `Holder<'a, Tr>` and `HolderWhere<'a, Tr>` resolve `Tr`'s object default to `'a`.
    for fn_name in ["takes_holder", "takes_holder_where"] {
        let k = key_of(&names, Namespace::Values, fn_name);
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("{fn_name}: {e:?}"));
        let life = store.fn_life(&names, &front.keys, k).unwrap_or_else(|e| panic!("{fn_name} fn_life: {e:?}"));
        assert_eq!(life.regions.as_slice(), &[Region::Named(0), Region::Named(0)], "{fn_name} fn_life");
    }

    // Finding 1b: `Unbounded<'a, Tr>` (no `T: 'a` bound) resolves `Tr` to `'static`.
    {
        let k = key_of(&names, Namespace::Values, "takes_unbounded");
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("takes_unbounded: {e:?}"));
        let life = store.fn_life(&names, &front.keys, k).unwrap_or_else(|e| panic!("takes_unbounded fn_life: {e:?}"));
        assert_eq!(life.regions.as_slice(), &[Region::Named(0), Region::Static]);
    }

    // Finding 1c: `Ambig<'a, 'b, Tr>` refuses in both `fn_life` and `store.regions`.
    {
        let k = key_of(&names, Namespace::Values, "takes_ambig");
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("takes_ambig: {e:?}"));
        let life = store.fn_life(&names, &front.keys, k);
        assert!(
            matches!(life, Err(LifeRefusal::ObjectDefault { .. } | LifeRefusal::TwoLifetimesOneOrigin { .. })),
            "Ambig<'a, 'b, Tr> must be refused, got: {life:?}"
        );
        assert!(store.regions.made(k).is_none(), "Ambig<'a, 'b, Tr> regions must be refused");
    }

    // Finding 2: bare trait aliases with omitted defaults (`AliasU8` and `AliasRef`).
    for (fn_name, expected) in [
        ("takes_alias_u8", &[R::Param(0), R::Param(0)][..]),
        ("takes_alias_ref", &[R::Param(0), R::Param(0), R::Static][..]),
    ] {
        let k = key_of(&names, Namespace::Values, fn_name);
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("{fn_name}: {e:?}"));
        let reg = store.regions.made(k).unwrap_or_else(|| panic!("{fn_name} regions unread: {:?}", store.regions.unread(k)));
        assert_eq!(
            reg.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
            expected,
            "{fn_name} input 0 regions"
        );
    }
    {
        let k = key_of(&names, Namespace::Values, "alias_ref_elide");
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("alias_ref_elide: {e:?}"));
        let life = store.fn_life(&names, &front.keys, k);
        assert!(
            matches!(life, Err(LifeRefusal::ElisionUndecided { .. })),
            "`AliasRef<T = &'static u8>` omitted default must not silently elide in fn_life, got: {life:?}"
        );
    }

    // Finding 3: trait alias with explicit lifetime bound (`trait AliasLt<'a> = Tr + 'a;`).
    {
        let k = key_of(&names, Namespace::Values, "takes_alias_lt");
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("takes_alias_lt: {e:?}"));
        let reg = store.regions.made(k).expect("takes_alias_lt regions laid cleanly");
        assert_eq!(
            reg.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
            &[R::Param(0), R::Param(0)]
        );
        let life = store.fn_life(&names, &front.keys, k).unwrap_or_else(|e| panic!("takes_alias_lt fn_life: {e:?}"));
        assert_eq!(life.regions.as_slice(), &[Region::Named(0), Region::Named(0)]);
    }
    {
        let k = key_of(&names, Namespace::Types, "PtrAliasLt");
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Adt(lib)).unwrap_or_else(|e| panic!("PtrAliasLt: {e:?}"));
        let life = store.adt_life(&names, &front.keys, lib).unwrap_or_else(|e| panic!("PtrAliasLt adt_life: {e:?}"));
        assert_eq!(life.fields.as_slice(), &[Region::Named(0)]);
        let reg = store.regions.made(k).expect("PtrAliasLt regions laid cleanly");
        let field_slots = reg.regions().fields.first().expect("PtrAliasLt field 0").2.as_slice();
        assert_eq!(field_slots, &[R::Param(0)]);
    }
}

/// Round 6 Findings 1, 2, and 3:
/// 1. Defaulted const parameters on bare trait aliases (`trait Alias<const N: usize = 3> = Gen<N>;`)
///    are evaluated and substituted instead of failing as missing defaults.
/// 2. Ambiguous containing-type lifetime bounds mixing a named lifetime and `'static`
///    (`struct Holder<'a, T: ?Sized + 'a + 'static>(&'a T); fn f<'a>(_: Holder<'a, Tr>) {}`)
///    are rejected as `ObjectDefault` (not overridden by `'static`).
/// 3. Associated type bindings written on bare trait aliases (`trait IterAlias = Iter;
///    fn f(_: &IterAlias<Item = u8>) {}`, `fn g<'a>(_: &'a IterAlias<Item = &'a u8>) {}`, and
///    supertrait projections `&SubAlias<'a, A = &'a u8>`) are resolved against the expanded
///    principal trait and laid cleanly.
#[test]
fn round6_const_alias_defaults_ambig_adt_static_and_alias_assoc_bindings() {
    const PROG: &str = "#![feature(no_core, lang_items, trait_alias)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

trait Tr {}
trait GenConst<const N: usize> {}
trait AliasConst<const N: usize = 3> = GenConst<N>;

fn takes_alias_const(_x: &AliasConst) {}

struct HolderStaticAmbig<'a, T: ?Sized + 'a + 'static>(&'a T);
struct HolderStaticOnly<T: ?Sized + 'static>(*const T);

fn takes_ambig_static<'a>(_x: HolderStaticAmbig<'a, Tr>) {}
fn takes_static_only(_x: HolderStaticOnly<Tr>) {}

trait Iter {
    type Item;
}
trait IterAlias = Iter;
trait IterAlias2 = IterAlias;

trait Super<'a> {
    type A;
}
trait Sub<'a>: Super<'a> {}
trait SubAlias<'a> = Sub<'a>;

fn takes_iter_alias_u8(_x: &IterAlias<Item = u8>) {}
fn takes_iter_alias_ref<'a>(_x: &'a IterAlias<Item = &'a u8>) {}
fn takes_iter_alias2_ref<'a>(_x: &'a IterAlias2<Item = &'a u8>) {}
fn takes_sub_alias_proj<'a>(_x: &'a SubAlias<'a, A = &'a u8>) {}
";
    let files = [SourceFile { path: "src/main.rs", text: PROG }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "bare_r6", edition: "2018", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
        .unwrap_or_else(|e| panic!("ProgramTexts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("Inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());

    use blaze_typeck::regions::Region as R;

    // Finding 1: defaulted const parameter on a bare trait alias (`const N: usize = 3`).
    {
        let k = key_of(&names, Namespace::Values, "takes_alias_const");
        let lib = front.keys.library_in(&names, k).expect("takes_alias_const lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("takes_alias_const: {e:?}"));
        let reg = store.regions.made(k).expect("takes_alias_const regions laid cleanly");
        assert_eq!(
            reg.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
            &[R::Param(0), R::Param(0)]
        );
    }

    // Finding 2: `HolderStaticAmbig<'a, Tr>` (`T: 'a + 'static`) must refuse `ObjectDefault`,
    // while `HolderStaticOnly<Tr>` (`T: 'static` alone) resolves to `Region::Static`.
    {
        let k = key_of(&names, Namespace::Values, "takes_ambig_static");
        let lib = front.keys.library_in(&names, k).expect("takes_ambig_static lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("takes_ambig_static: {e:?}"));
        let life = store.fn_life(&names, &front.keys, k);
        assert!(
            matches!(life, Err(LifeRefusal::ObjectDefault { .. })),
            "containing ADT bound `T: 'a + 'static` is ambiguous and must refuse ObjectDefault, got: {life:?}"
        );
    }
    {
        let k = key_of(&names, Namespace::Values, "takes_static_only");
        let lib = front.keys.library_in(&names, k).expect("takes_static_only lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("takes_static_only: {e:?}"));
        let life = store.fn_life(&names, &front.keys, k).unwrap_or_else(|e| panic!("takes_static_only fn_life: {e:?}"));
        assert_eq!(life.regions.as_slice(), &[Region::Static]);
    }

    // Finding 3: associated bindings written on bare trait aliases (`&IterAlias<Item = u8>`,
    // `&'a IterAlias<Item = &'a u8>`, nested `&'a IterAlias2<Item = &'a u8>`, and supertrait
    // projection `&'a SubAlias<'a, A = &'a u8>`).
    for (fn_name, expected) in [
        ("takes_iter_alias_u8", &[R::Param(0), R::Param(0)][..]),
        ("takes_iter_alias_ref", &[R::Param(0), R::Param(0), R::Param(0)][..]),
        ("takes_iter_alias2_ref", &[R::Param(0), R::Param(0), R::Param(0)][..]),
        ("takes_sub_alias_proj", &[R::Param(0), R::Param(0), R::Param(0), R::Param(0), R::Param(0)][..]),
    ] {
        let k = key_of(&names, Namespace::Values, fn_name);
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("{fn_name}: {e:?}"));
        let reg = store.regions.made(k).unwrap_or_else(|| panic!("{fn_name} regions unread: {:?}", store.regions.unread(k)));
        assert_eq!(
            reg.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
            expected,
            "{fn_name} input 0 regions"
        );
    }
}

/// Round 7 Findings 1, 2, and 3:
/// 1. Omitted trait lifetimes on bare trait objects (`trait Tr<'a>: 'a {}; fn f(_: *const Tr) {}`,
///    `fn g(_: *const Tr) -> &u8`, `fn h(_: *const Tr<'_>) -> &u8`) are materialized and shared
///    before deriving the trait-bound object default so `fn_life` and `store.regions` succeed.
/// 2. Nested trait aliases propagating const arguments (`trait Gen<const N: usize> {};
///    trait Inner<const N: usize> = Gen<N>; trait Outer = Inner<3>;
///    trait OuterParam<const M: usize> = Inner<M>;`) lower and lay cleanly.
/// 3. Higher-ranked (`for<'a>`) binders on trait-alias bounds (`trait Tr<'a> {};
///    trait Alias = for<'a> Tr<'a>;` and nested `trait Outer = for<'a> Inner<'a>;` /
///    `trait OuterRef = for<'a> InnerRef<&'a u8>;`) are carried through alias region layout.
#[test]
fn round7_omitted_trait_lt_nested_alias_const_and_alias_hrtb() {
    const PROG: &str = "#![feature(no_core, lang_items, trait_alias)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

trait Tr<'a>: 'a {}
trait TrAlias<'a> = Tr<'a> + 'a;

fn takes_ptr_omitted(_p: *const Tr) {}
fn ptr_omitted_elide(_p: *const Tr) -> &u8 { loop {} }
fn ptr_anon_elide(_p: *const Tr<'_>) -> &u8 { loop {} }
fn takes_ptr_alias_omitted(_p: *const TrAlias) {}
fn ret_ptr_omitted(_x: &u8) -> *const Tr { loop {} }

trait Gen<const N: usize> {}
trait InnerConst<const N: usize> = Gen<N>;
trait OuterConst = InnerConst<3>;
trait OuterConstParam<const M: usize> = InnerConst<M>;

fn takes_outer_const(_x: &OuterConst) {}
fn takes_outer_const_param(_x: &OuterConstParam<5>) {}

trait TrHrtb<'a> {}
trait AliasHrtb = for<'a> TrHrtb<'a>;
trait InnerHrtb<'a> = TrHrtb<'a>;
trait OuterHrtb = for<'a> InnerHrtb<'a>;

trait TrRef<T> {}
trait AliasHrtbRef = for<'a> TrRef<&'a u8>;
trait InnerHrtbRef<T> = TrRef<T>;
trait OuterHrtbRef = for<'a> InnerHrtbRef<&'a u8>;

fn takes_dyn_hrtb(_x: &(dyn for<'a> TrHrtb<'a>)) {}
fn takes_alias_hrtb(_x: &AliasHrtb) {}
fn alias_hrtb_elide(_x: &AliasHrtb) -> &u8 { loop {} }
fn takes_outer_hrtb(_x: &OuterHrtb) {}
fn takes_alias_hrtb_ref(_x: &AliasHrtbRef) {}
fn takes_outer_hrtb_ref(_x: &OuterHrtbRef) {}
";
    let files = [SourceFile { path: "src/main.rs", text: PROG }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "bare_r7", edition: "2018", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
        .unwrap_or_else(|e| panic!("ProgramTexts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("Inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());

    use blaze_typeck::regions::Region as R;

    // Finding 1: omitted / `'_` trait lifetimes on bare trait objects resolve via materialized input region.
    for (fn_name, expected_life, expected_reg) in [
        ("takes_ptr_omitted", &[Region::Anon(0)][..], &[R::Param(0), R::Param(0)][..]),
        ("ptr_omitted_elide", &[Region::Anon(0), Region::Anon(0)][..], &[R::Param(0), R::Param(0)][..]),
        ("ptr_anon_elide", &[Region::Anon(0), Region::Anon(0)][..], &[R::Param(0), R::Param(0)][..]),
        ("takes_ptr_alias_omitted", &[Region::Anon(0)][..], &[R::Param(0), R::Param(0)][..]),
    ] {
        let k = key_of(&names, Namespace::Values, fn_name);
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("{fn_name}: {e:?}"));
        let life = store.fn_life(&names, &front.keys, k).unwrap_or_else(|e| panic!("{fn_name} fn_life: {e:?}"));
        assert_eq!(life.regions.as_slice(), expected_life, "{fn_name} fn_life");
        let reg = store.regions.made(k).unwrap_or_else(|| panic!("{fn_name} regions unread: {:?}", store.regions.unread(k)));
        assert_eq!(
            reg.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
            expected_reg,
            "{fn_name} input 0 regions"
        );
    }
    {
        let k = key_of(&names, Namespace::Values, "ret_ptr_omitted");
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("ret_ptr_omitted: {e:?}"));
        let life = store.fn_life(&names, &front.keys, k).unwrap_or_else(|e| panic!("ret_ptr_omitted fn_life: {e:?}"));
        assert_eq!(life.regions.as_slice(), &[Region::Anon(0), Region::Anon(0)], "ret_ptr_omitted fn_life");
    }

    // Finding 2: nested trait aliases with const arguments (`InnerConst<3>` and `InnerConst<M>`).
    for fn_name in ["takes_outer_const", "takes_outer_const_param"] {
        let k = key_of(&names, Namespace::Values, fn_name);
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("{fn_name}: {e:?}"));
        let reg = store.regions.made(k).unwrap_or_else(|| panic!("{fn_name} regions unread: {:?}", store.regions.unread(k)));
        assert_eq!(
            reg.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
            &[R::Param(0), R::Param(0)],
            "{fn_name} input 0 regions"
        );
    }

    // Finding 3: HRTB binders (`for<'a>`) on direct and nested trait aliases match explicit `dyn for<'a>`.
    for fn_name in ["takes_dyn_hrtb", "takes_alias_hrtb", "takes_outer_hrtb", "takes_alias_hrtb_ref", "takes_outer_hrtb_ref"] {
        let k = key_of(&names, Namespace::Values, fn_name);
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("{fn_name}: {e:?}"));
        let reg = store.regions.made(k).unwrap_or_else(|| panic!("{fn_name} regions unread: {:?}", store.regions.unread(k)));
        assert_eq!(
            reg.regions().sig_view().expect("sig").col.child(0).expect("input 0"),
            &[R::Param(0), R::Param(0), R::Late { binder: 1, var: 0 }],
            "{fn_name} input 0 regions"
        );
    }
    {
        let k = key_of(&names, Namespace::Values, "alias_hrtb_elide");
        let lib = front.keys.library_in(&names, k).expect("lib key");
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("alias_hrtb_elide: {e:?}"));
        let life = store.fn_life(&names, &front.keys, k).unwrap_or_else(|e| panic!("alias_hrtb_elide fn_life: {e:?}"));
        assert_eq!(life.regions.as_slice(), &[Region::Anon(0), Region::Anon(0), Region::Anon(0)], "alias_hrtb_elide fn_life");
    }
}

#[path = "front2_bare_trait_objects/anon_trait_params.rs"]
mod anon_trait_params;
