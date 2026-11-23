//! tracing's event shape ACROSS CRATES: a block written by a 2018 crate's macro keeps the 2018
//! rule for its tail's temporaries inside a 2024 caller's body.
//!
//! The rule. The Reference, "Destructors", "Temporary scopes": before 2024 a block's tail
//! expression's temporaries live to the end of the enclosing temporary scope (here the call
//! statement), so the `ValueSet` the block yields may borrow them (the array, `display(&x)`).
//! From 2024 ("Tail expression temporary scope", RFC 3606) they are dropped at the block's end,
//! and the `ValueSet` used after it is E0716 (blaze: E0597 "goes out of scope", E0505 "is
//! dropped" for a temporary with drop glue). rustc decides by the BLOCK's span's edition
//! (region.rs `resolve_block`: `blk.span.edition()`), and a token a macro wrote has its
//! defining crate's edition. So tracing 0.1.44 (edition 2018) is clean in tempo (2024).
//!
//! The chain under test, end to end: the dependency's edition (its items record) -> the macro's
//! definition (`DefAt::edition`) -> each transcribed word's mark (blaze-expand `body_words`) ->
//! the lowered view's mark column (blaze-lower machine.rs `append_expansion`) -> the
//! expression's edition (typeck.rs `expr_editions`, columns.rs `mark_of_expr`) -> the Block
//! arm's `own_temps` (blaze-flow-lower lib.rs). tempo's c2300wave72 had 816 distinct notes on
//! tracing calls, each with "taken" and "later used" both the whole call.
//!
//! Crates: `t` (2018) and `t24` (2024) have the SAME text, two macros shaped like tracing's
//! `event!` arms (the closure called at once, and `__tracing_log`'s call with `&valueset`).
//! The user crate (2024) calls each. Expected lines:
//! `user closure_2018: .. findings []`, `user call_2018: .. findings []` (clean), and the TWIN
//! `user closure_2024` / `user call_2024` with at least one finding each.
//! NOT YET RUN: written by a write-only lane (2026-11-21). Every verdict is printed first.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Texts, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude};

/// The library, the same text in both editions: tracing-core's `ValueSet` and
/// `FieldSet::value_set_all` (field.rs:944), tracing's `valueset_all!` (macros.rs:2814) and the
/// two places `event!` (macros.rs:833) passes its value set.
const LIB: &str = r#"
pub trait Value {}
pub struct DisplayValue<T>(T);
impl<T> Value for DisplayValue<T> {}
pub fn display<T>(t: T) -> DisplayValue<T> { DisplayValue(t) }
pub struct FieldSet;
pub struct ValueSet<'a> { pub fields: &'a FieldSet, pub values: &'a [Option<&'a (dyn Value + 'a)>] }
impl FieldSet {
    pub fn value_set_all<'v>(&'v self, values: &'v [Option<&'v (dyn Value + 'v)>]) -> ValueSet<'v> {
        ValueSet { fields: self, values }
    }
}
pub static FIELDS: FieldSet = FieldSet;
pub fn dispatch(v: &ValueSet<'_>) { let _ = v.values.len(); }

#[macro_export]
macro_rules! valueset_all {
    ($fields:expr, $($val:expr),*) => {
        {
            $fields.value_set_all(&[ $(Some(&$crate::display(&$val) as &dyn $crate::Value)),* ])
        }
    };
}

#[macro_export]
macro_rules! ev_closure {
    ($($val:expr),*) => {{
        if true {
            (|value_set: $crate::ValueSet| { $crate::dispatch(&value_set); })($crate::valueset_all!($crate::FIELDS, $($val),*));
        }
    }};
}

#[macro_export]
macro_rules! ev_call {
    ($($val:expr),*) => {{
        if true {
            $crate::dispatch(&$crate::valueset_all!($crate::FIELDS, $($val),*));
        }
    }};
}

pub fn dispatch_owned(v: ValueSet<'_>) { let _ = v.values.len(); }

#[macro_export]
macro_rules! ev_owned {
    ($($val:expr),*) => {{
        if true {
            $crate::dispatch_owned($crate::valueset_all!($crate::FIELDS, $($val),*));
        }
    }};
}
"#;

/// The user crate (2024): each macro of each library, called as tempo calls `info!(%x, ..)`.
const USER: &str = r#"
pub fn closure_2018(x: u64) { t::ev_closure!(x); }
pub fn call_2018(x: u64) { t::ev_call!(x); }
pub fn closure_2024(x: u64) { t24::ev_closure!(x); }
pub fn call_2024(x: u64) { t24::ev_call!(x); }
pub fn call_direct(x: u64) { t::dispatch(&t::FIELDS.value_set_all(&[Some(&t::display(&x) as &dyn t::Value)])); }
pub fn owned_2018(x: u64) { t::ev_owned!(x); }
fn main() {}
"#;

/// The texts of the three-crate layout: the libraries' files for crates `t` and `t24`, the rest
/// as [`Inputs`] reads them.
struct WithLibs<'t> {
    base: &'t Texts<'t>,
    first: usize,
    files: &'t [(String, String)],
}

impl<'t> SourceTexts<'t> for WithLibs<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        if krate == self.first || krate == self.first + 1 {
            return self.files.iter().find(|(p, _)| p == path).map(|(_, t)| t.as_str());
        }
        self.base.read(krate, path)
    }
}

/// Each user body's verdict: `Some(n)` checked with `n` findings, `None` refused; each printed.
fn verdicts() -> Vec<(&'static str, Option<usize>)> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let root = "/user/src/main.rs";
    let user_files = [(root.to_owned(), USER.to_owned())];
    let lib_files = [("/t/src/lib.rs".to_owned(), LIB.to_owned()), ("/t24/src/lib.rs".to_owned(), LIB.to_owned())];
    let user = UserCrate {
        name: "user",
        dir: "/user/",
        root,
        files: &user_files,
        edition: edition::PackageEdition::of_manifest("2024").expect("an edition"),
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let mut crates: Vec<CrateInput<'_>> = inputs.crates();
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(i as u32));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
    assert!(sysroot.core.is_some(), "the toolchain at {shelf} has a `core` unit");
    let first = crates.len();
    let user_deps: &'static [(&'static str, usize)] = Box::leak(vec![("t", first), ("t24", first + 1)].into_boxed_slice());
    // The user's own deps (the sysroot's) stay; the two libraries are added.
    let mut deps: Vec<(&str, usize)> = crates[0].deps.to_vec();
    deps.extend_from_slice(user_deps);
    crates[0].deps = Box::leak(deps.into_boxed_slice());
    let (cfg, user_implicit, user_deps_base) = (crates[0].cfg, crates[0].implicit, inputs.crates()[0].deps);
    for (name, root, ed, prelude) in [
        ("t", "/t/src/lib.rs", Edition::E2018, edition::Edition::Edition2018),
        ("t24", "/t24/src/lib.rs", Edition::E2024, edition::Edition::Edition2024),
    ] {
        crates.push(CrateInput {
            name,
            key: CrateItemKey(ContentId::of("blaze-ladder tracing edition test crate", name.as_bytes())),
            edition: ed,
            implicit: Implicit { prelude: Prelude::Rust(prelude), proc_macro: false, entry: false, ..user_implicit },
            root,
            shown_from: if name == "t" { "/t/" } else { "/t24/" },
            features: &[],
            // A library sees the sysroot crates the user crate sees.
            deps: user_deps_base,
            cfg,
            script: None,
        });
    }
    let texts = WithLibs { base: &inputs.texts, first, files: &lib_files };
    let names = resolve_crates(&crates, sysroot, &texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates (three crates): {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: USER }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("the body checker: {e}"));
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    let mut out = Vec::new();
    for name in ["closure_2018", "call_2018", "closure_2024", "call_2024", "call_direct", "owned_2018"] {
        let (types, v) = world.check_borrows(BodyRef::Free(key_of(name)), alloc);
        println!("user {name}: types clean {}, {types:?}; borrows {v:?}", types.is_clean());
        assert!(types.is_clean(), "`{name}`'s types: {types:?}");
        let got = if v.refused.is_some() { None } else { Some(v.findings.len()) };
        out.push((name, got));
    }
    out
}

/// A 2018 macro's block in a 2024 body: its tail's temporaries live to the end of the call
/// statement, so both bodies are clean. TWIN: the same macro from a 2024 crate drops them at the
/// block's end, before the `ValueSet` is used: each body has a finding.
#[test]
fn a_2018_macros_block_keeps_its_tail_temporaries_in_a_2024_body() {
    let got = verdicts();
    println!("verdicts: {got:?}");
    assert_eq!(got[0], ("closure_2018", Some(0)), "the 2018 block passed to the closure is clean");
    assert_eq!(got[1], ("call_2018", Some(0)), "the 2018 block borrowed for a call is clean");
    assert!(matches!(got[2], ("closure_2024", Some(n)) if n > 0), "TWIN: the 2024 block passed to the closure is E0716: {:?}", got[2]);
    assert!(matches!(got[3], ("call_2024", Some(n)) if n > 0), "TWIN: the 2024 block borrowed for a call is E0716: {:?}", got[3]);
}

/// Which part of `call_2018`'s shape (c0010 run: 2 findings, the closure shape 0) the loss is
/// in, each body clean under rustc:
/// - `call_direct`: the same value set borrowed for `dispatch` with NO block and no macro
///   (a statement's temporaries live to its end in every edition). Findings here put the cause
///   in borrowing a `ValueSet` for a call, not in any edition or block rule.
/// - `owned_2018`: the 2018 macro's block passed BY VALUE to a fn (`dispatch_owned`), not
///   borrowed. Findings here but none in `closure_2018` put the cause in the fn call's
///   relation of a `ValueSet` argument; none here and none in `call_direct` put it in a block
///   that is the operand of `&`.
#[test]
fn the_call_shapes_parts_are_each_clean() {
    let got = verdicts();
    println!("verdicts: {got:?}");
    assert_eq!(got[4], ("call_direct", Some(0)), "the value set borrowed for a call, no block, is clean");
    assert_eq!(got[5], ("owned_2018", Some(0)), "the 2018 block passed by value to a fn is clean");
}
