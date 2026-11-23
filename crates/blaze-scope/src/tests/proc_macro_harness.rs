//! D175 (docs/v2/plan/design/comptime-native.md, "The proc_macro route", blocker 2): a
//! proc-macro crate's harness static. rustc's proc_macro_harness.rs writes, for a crate whose
//! manifest says `proc-macro = true`, one `#[rustc_proc_macro_decls] static _DECLS:
//! &[proc_macro::bridge::client::Client]` listing each `#[proc_macro*]` fn of the root in
//! declaration order (the pinned library/proc_macro/src/bridge/client.rs: `Client::expand1` for
//! a derive and a function-like macro, `Client::expand2` for an attribute macro). blaze makes it
//! as the expansion of the root's harness call (task/lower.rs `Lower::proc_macro_harness`),
//! keyed by `proc_macro_decls_key`, declaring no name.

use std::format;
use std::vec::Vec;

use blaze_own::Alloc;
use blaze_surface::{Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::{Texts, input};
use crate::graph::Sysroot;
use crate::names::Written;
use crate::resolve::{CrateInput, Names, resolve_crates};
use crate::task::{PROC_MACRO_DECLS, Serial, proc_macro_decls_key};

/// One macro of each kind, the attribute's fn named by a raw identifier, an ordinary fn between
/// them that is in no list.
const MACROS: &str = "#![no_core]
#[proc_macro_derive(Hello, attributes(hi))]
pub fn derive_hello() {}
pub fn helper() {}
#[proc_macro_attribute]
pub fn r#trace() {}
#[proc_macro]
pub fn make() {}
";

/// An ordinary crate with the same fns' text: no harness.
const PLAIN: &str = "#![no_core]
pub fn derive_hello() {}
";

static TEXTS: Texts = Texts(&[&[("/plain/lib.rs", PLAIN)], &[("/macros/lib.rs", MACROS)]]);

fn resolved(proc_macro: bool) -> (Names<'static>, [CrateInput<'static>; 2]) {
    let macros = input("macros", "/macros/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro, entry: false };
    let crates = [input("plain", "/plain/lib.rs", &[]), CrateInput { implicit, ..macros }];
    let names = resolve_crates(&crates, Sysroot::default(), &TEXTS, &Serial, super::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    (names, crates)
}

/// The text of the item keyed `key` in crate `krate`, if it has one.
fn text_of(names: &Names<'static>, krate: usize, key: blaze_id::ItemKey) -> Option<(Option<&'static str>, &'static str)> {
    names.krate(krate).tables.texts.get(&key).map(|(t, _)| (t.name, t.text))
}

/// Rule: a proc-macro crate has the `_DECLS` static under `proc_macro_decls_key`, its list the
/// root's three macros in declaration order (`expand1` derive, `expand2` attribute, `expand1`
/// function-like; the raw name kept raw; the plain fn left out), bound to no name; an ordinary
/// crate has none.
///
/// Prints `harness of macros: <the static's text>` and
/// `test tests::proc_macro_harness::a_proc_macro_crate_lists_its_macros_in_one_unnamed_static ... ok`.
/// CONTROL: the same crate resolved with `proc_macro: false` (the cause, `Implicit::proc_macro`,
/// removed) has no such item: the test asserts it, so a harness made for every crate fails here.
#[test]
fn a_proc_macro_crate_lists_its_macros_in_one_unnamed_static() {
    let (names, crates) = resolved(true);
    let notes = || format!("notes {:?}", names.notes(1));
    let key = proc_macro_decls_key(crates[1].key);
    let (name, text) = text_of(&names, 1, key).unwrap_or_else(|| panic!("the macro crate has no `_DECLS` under its key: {}", notes()));
    assert_eq!(name, Some(PROC_MACRO_DECLS));
    assert!(text.starts_with("#[rustc_proc_macro_decls]"), "the static keeps rustc's attribute: {text}");
    assert!(text.contains("static _DECLS: &[::proc_macro::bridge::client::Client] = &["), "{text}");
    let client = "::proc_macro::bridge::client::Client::";
    let wanted = [format!("{client}expand1(crate::derive_hello)"), format!("{client}expand2(crate::r#trace)"), format!("{client}expand1(crate::make)")];
    let at: Vec<usize> = wanted.iter().map(|w| text.find(w.as_str()).unwrap_or_else(|| panic!("`{w}` in {text}"))).collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "declaration order: {at:?} in {text}");
    assert_eq!(text.matches(client).count(), 3, "three macros, the plain fn in no list: {text}");
    // Bound to no name, as rustc's def-site `_DECLS`.
    let g = Alloc::global();
    let bound = names.item_at(names.root(1), Namespace::Values, Written { kind: PathKind::Plain, segments: &["_DECLS"] }, g);
    assert!(bound.is_err(), "`_DECLS` must not be nameable from the crate");
    // An ordinary crate has none.
    assert!(text_of(&names, 0, proc_macro_decls_key(crates[0].key)).is_none(), "an ordinary crate made a harness");
    assert!(names.krate(0).tables.texts.values().all(|(t, _)| t.name != Some(PROC_MACRO_DECLS)));
    std::println!("harness of macros: {text}");

    // CONTROL: the cause removed, the item goes.
    let (plain, crates) = resolved(false);
    assert!(text_of(&plain, 1, proc_macro_decls_key(crates[1].key)).is_none(), "a crate that is no proc-macro crate made a harness");
}
