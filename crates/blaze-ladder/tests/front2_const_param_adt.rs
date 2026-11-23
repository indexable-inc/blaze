//! A const parameter of a struct type and of `&'static str` has a head, on front2 with the real
//! core (front2/const_param_ty.rs `admits`, asked by heads.rs `own_generics`). Hello-world path,
//! walk AJ P12.
//!
//! The evidence: the walk of hello world left out 208 functions of alloc's `Vec` (`drop`,
//! `as_slice`, `push`, `len`, ..) with "check: Head: Item: Unsupported(\"a const parameter
//! whose type is not an integer, `char` or `bool` (`adt_const_params`)\" ..)", the item at bytes
//! 3317..3359 of library/core/src/mem/transmutability.rs, line 91:
//!
//! ```text
//! pub unsafe trait TransmuteFrom<Src, const ASSUME: Assume = { Assume::NOTHING }>
//! ```
//!
//! where `Assume` is a STRUCT (line 148-151, and line 293 `impl ConstParamTy_ for Assume {}`):
//!
//! ```text
//! #[lang = "transmute_opts"]
//! #[derive(PartialEq, Eq, Clone, Copy, Debug)]
//! pub struct Assume {
//! ```
//!
//! The rule, from the pinned library (library/core/src/marker.rs): "A marker for types which
//! can be used as types of `const` generic parameters." `pub trait ConstParamTy_:
//! StructuralPartialEq + Eq {}`, implemented for the integers, `bool`, `char`, `()`, `[T; N]`,
//! and under `unsized_const_params` for `str`, `[T]` and `&T`; a struct or enum by its own impl
//! (`#[derive(ConstParamTy)]`, "Derive macro generating an impl of the trait `ConstParamTy`").
//! A float is not among them, and `&mut T` is not (only `&T`). A const parameter's type is
//! `Sized`, so `str` alone is no such type.
//!
//! What is right, and what each case here must say:
//!
//! ```text
//! struct_const_param_has_head:
//!   head T: built        pub trait T<const P: A> {}            (A derives ConstParamTy)
//!   head W: built        pub struct W<const P: A>;
//!   head g: built        fn g<const P: A>() {}
//!   ln: clean            fn ln(v: &[u8]) -> usize { v.len() }  (a method call: every trait
//!                        in scope, `T` too, is asked for an item named `len`)
//!   le: clean            fn le(a: usize, b: usize) -> bool { a <= b }
//!   fp: no head, "floating-point"     fn fp<const P: f32>() {}
//!   mr: no head, "`&mut`"             fn mr<const P: &'static mut u8>() {}
//!   wv: no head, "unit variant"       fn wv(w: W<{ A { x: true } }>) {}: a struct's VALUE has
//!                                     no node yet: refused where it is read, by name
//! str_const_param_has_head:
//!   head R: built        pub struct R<const F: &'static str, T>(pub T);
//!   lr: clean            a <= b over &R<F, u8>: selects `impl<const F: &'static str, T:
//!                        PartialOrd> PartialOrd for R<F, T>`, whose head has the parameter
//!   su: no head, "not behind `&`"     fn su<const P: str>() {}
//!   rv: no head, "value tree"         fn rv(r: R<{ "k" }, u8>) {}: a `&str` VALUE has no node
//! struct_without_const_param_ty_is_e0741 (IGNORED: OWED, the item-level check):
//!   st: refused, "E0741"              fn st<const V: S>() {} for `struct S;` (no impl)
//! ```
//!
//! and the check script's line: `test blaze-ladder: exit 0; 2 passed, 0 failed` (1 ignored).
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_params.rs.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const STRUCT_MAIN: &str = "#![feature(adt_const_params)]
use core::marker::ConstParamTy;
fn main(cx: &Cx) -> Result<(), Error> {
    Ok(())
}
#[derive(PartialEq, Eq, ConstParamTy)]
pub struct A { pub x: bool }
pub trait T<const P: A> {}
pub struct W<const P: A>;
fn g<const P: A>() {}
fn ln(v: &[u8]) -> usize { v.len() }
fn le(a: usize, b: usize) -> bool { a <= b }
fn fp<const P: f32>() {}
fn mr<const P: &'static mut u8>() {}
fn wv(w: W<{ A { x: true } }>) {}
";

const STR_MAIN: &str = "#![feature(adt_const_params, unsized_const_params)]
fn main(cx: &Cx) -> Result<(), Error> {
    Ok(())
}
pub struct R<const F: &'static str, T>(pub T);
impl<const F: &'static str, T: PartialEq> PartialEq for R<F, T> {
    fn eq(&self, other: &Self) -> bool { self.0 == other.0 }
}
impl<const F: &'static str, T: PartialOrd> PartialOrd for R<F, T> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> { self.0.partial_cmp(&other.0) }
}
fn lr<const F: &'static str>(a: &R<F, u8>, b: &R<F, u8>) -> bool { a <= b }
fn su<const P: str>() {}
fn rv(r: R<{ \"k\" }, u8>) {}
";

const E0741_MAIN: &str = "#![feature(adt_const_params)]
fn main(cx: &Cx) -> Result<(), Error> {
    Ok(())
}
struct S;
fn st<const V: S>() {}
";

/// One case of a crate.
enum Case {
    /// The head of the type-namespace item (a trait or a struct) builds.
    TypeHead(&'static str),
    /// The head of the fn builds.
    FnHead(&'static str),
    /// The fn's head is refused with a text holding `.1`.
    NoHead(&'static str, &'static str),
    /// The fn's head builds, it lowers, and its body checks with no diagnostic.
    Clean(&'static str),
}

/// Runs every case over the one-file crate `main`; the cases that did not say what is stated,
/// and one note per case.
fn run(main: &'static str, cases: &[Case]) -> (Vec<String>, Vec<String>) {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/cpa/src/main.rs".to_string(), main.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "cpa", dir: "/cpa/", root: "/cpa/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: main }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let key_in = |ns: Namespace, name: &str| {
        names::path(&names, names.root(0), ns, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate's root"))
    };
    for case in cases {
        match *case {
            Case::TypeHead(name) | Case::FnHead(name) => {
                let want = match case {
                    Case::TypeHead(_) => Want::Head(key_in(Namespace::Types, name)),
                    _ => Want::Fn(key_in(Namespace::Values, name)),
                };
                match store.ensure(&names, &mut front.keys, want) {
                    Ok(_) => notes.push(format!("head {name}: built")),
                    Err(e) => {
                        let brief: String = format!("{e:?}").chars().take(240).collect();
                        notes.push(format!("head {name}: {brief}"));
                        failed.push(format!("head {name}"));
                    }
                }
            }
            Case::NoHead(name, holds) => {
                let said = match store.ensure(&names, &mut front.keys, Want::Fn(key_in(Namespace::Values, name))) {
                    Ok(_) => "built".to_string(),
                    Err(e) => format!("{e:?}"),
                };
                if said.contains(holds) {
                    notes.push(format!("{name}: no head, {holds:?}"));
                } else {
                    let brief: String = said.chars().take(240).collect();
                    notes.push(format!("{name}: wanted a refusal holding {holds:?}: {brief}"));
                    failed.push(name.to_string());
                }
            }
            Case::Clean(name) => {
                let key = key_in(Namespace::Values, name);
                if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
                    let brief: String = format!("{e:?}").chars().take(240).collect();
                    notes.push(format!("{name}: no head: {brief}"));
                    failed.push(name.to_string());
                    continue;
                }
                let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
                    Ok(b) => b,
                    Err(e) => {
                        let brief: String = format!("{e}").chars().take(240).collect();
                        notes.push(format!("{name}: does not lower: {brief}"));
                        failed.push(name.to_string());
                        continue;
                    }
                };
                let view = body.lowered.view();
                let spell = Spelling::of(&view).expect("spelling");
                let scope = body.scope(&spell);
                let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
                let said = match check_body(&mut records, &body, &scope) {
                    Ok(done) if done.diags.is_empty() => "clean".to_string(),
                    Ok(done) => {
                        let codes: Vec<String> = done.diags.iter().map(|d| format!("{:?}", d.code)).collect();
                        format!("diagnostics {codes:?}")
                    }
                    Err(CheckFail::Diags { diags, .. }) => {
                        let codes: Vec<String> = diags.iter().map(|d| format!("{:?}", d.0)).collect();
                        format!("diagnostics {codes:?}")
                    }
                    Err(other) => format!("{other:?}"),
                };
                let brief: String = said.chars().take(240).collect();
                notes.push(format!("{name}: {brief} (wanted clean)"));
                if said != "clean" {
                    failed.push(name.to_string());
                }
            }
        }
    }
    (failed, notes)
}

/// The FIRST case (walk AJ P12): a const parameter of a struct type deriving `ConstParamTy`,
/// as core's `TransmuteFrom<Src, const ASSUME: Assume>`. Fails without the fix: every head
/// with `const P: A` was refused "a const parameter whose type is not an integer, `char` or
/// `bool`", and so was `ln`, whose method call asks every trait in scope (`T` among them).
#[test]
fn struct_const_param_has_head() {
    let cases = [
        Case::TypeHead("T"),
        Case::TypeHead("W"),
        Case::FnHead("g"),
        Case::Clean("ln"),
        Case::Clean("le"),
        Case::NoHead("fp", "floating-point"),
        Case::NoHead("mr", "`&mut`"),
        Case::NoHead("wv", "unit variant"),
    ];
    let (failed, notes) = run(STRUCT_MAIN, &cases);
    assert!(failed.is_empty(), "cases that did not say what is stated: {failed:?}\n{}", notes.join("\n"));
}

/// The second case: `&'static str` (core's `ConstParamTy_ for &T` and `for str` under
/// `unsized_const_params`), its parameter on a struct and on a trait impl selected by `<=`.
#[test]
fn str_const_param_has_head() {
    let cases = [
        Case::TypeHead("R"),
        Case::Clean("lr"),
        Case::NoHead("su", "not behind `&`"),
        Case::NoHead("rv", "value tree"),
    ];
    let (failed, notes) = run(STR_MAIN, &cases);
    assert!(failed.is_empty(), "cases that did not say what is stated: {failed:?}\n{}", notes.join("\n"));
}

/// What is right for a struct with no `ConstParamTy_` impl: the pinned nightly refuses `fn
/// st<const V: S>()` (E0741, "`S` must implement `ConstParamTy` to be used as the type of a
/// const generic parameter"). The head admits the type by shape (const_param_ty.rs); the goal
/// `S: ConstParamTy_` belongs to the item-level check, which the rebuilt layer does not have
/// yet (blaze-typeck rules/control.rs). Ignored until that check exists; it states the right
/// answer, not today's.
#[test]
#[ignore = "OWED: the item-level check proving `S: ConstParamTy_` (E0741)"]
fn struct_without_const_param_ty_is_e0741() {
    let (failed, notes) = run(E0741_MAIN, &[Case::NoHead("st", "E0741")]);
    assert!(failed.is_empty(), "cases that did not say what is stated: {failed:?}\n{}", notes.join("\n"));
}
