//! A closure's KIND from its captured places (W4; blaze-typeck capture_places.rs `settle_uses`).
//! Per the Reference, "Closure types", "Call traits and coercions", a closure that moves a
//! captured value out is `FnOnce` only, and one that mutates a capture is `FnMut`. The kind is
//! the maximum over the uses, taken BEFORE `move` turns every capture into a by-value one
//! (rustc upvar.rs `compute_min_captures`). Each rule has its twin (rule 23); every answer is
//! rustc's. The tempo sites are blaze-int notes reports/tempo-correct-1120/c0255wave87/
//! borrow-notes.txt.
//!
//! 1. A closure moving a captured `String` is `FnOnce`.
//!    - TWIN `call_twice` (`move || drop(s)`, called twice): E0382, the second call uses the
//!      closure after the first moved it.
//!    - `call_twice_ref` (`move || s.len()`, called twice): `Fn`, clean.
//!    - TWIN `need_fn_once` (`move || drop(s)` where `Fn` is required): an error naming `Fn`.
//!      rustc says E0525; blaze reports an unmet `Fn` bound as E0277 (UNSURE: no E0525 exists
//!      in blaze-typeck today), so the code is printed and not asserted.
//!    - `need_fn_ref`: `move || s.len()` where `Fn` is required: clean.
//! 2. A match on an upvar that binds its payload by value moves it, so `FnOnce` (tempo
//!    bin/tempo tempo_cmd.rs:1071; RFC 2229 "patterns": a binding by value of a place is a move
//!    of that place).
//!    - TWIN `match_moves`: an error naming `Fn`.
//!    - `match_refs` (`Some(ref s)`): `Fn`, clean.
//! 3. A struct update `W { n: 1, ..w }` moves each field it takes from the base (the Reference,
//!    "Struct expressions", functional update syntax). A non-`Copy` field makes the closure
//!    `FnOnce` (alloy rpc reth_compat.rs:83).
//!    - TWIN `fru_moves`: an error naming `Fn`.
//!    - `fru_copies`: every field taken is `Copy`: `Fn`, clean.
//! 4. A child `async move` block's by-value capture is a by-value use in its parent (rustc
//!    replays a nested closure's captures in the parent: `walk_captures`). So the parent is
//!    `FnOnce` (tempo full_ceremony.rs:41, p2p runtime.rs:409).
//!    - TWIN `child_moves`: an error naming `Fn`.
//!    - `child_copies`: the child copies a `Copy` value, so the parent only reads it: `Fn`,
//!      clean (tempo network_chaos_e2e.rs:318).
//!
//! NOT YET RUN (written 2026-11-21 by a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
fn need_fn<F: Fn() -> R, R>(_: F) {}

fn call_twice(s: String) { let f = move || drop(s); f(); f(); }
fn call_twice_ref(s: String) { let f = move || s.len(); f(); f(); }
fn need_fn_once(s: String) { need_fn(move || drop(s)); }
fn need_fn_ref(s: String) { need_fn(move || s.len()); }

fn match_moves(o: Option<String>) { need_fn(|| match o { Some(s) => drop(s), None => {} }); }
fn match_refs(o: Option<String>) { need_fn(|| match o { Some(ref s) => s.len(), None => 0 }); }

pub struct W { pub inner: String, pub n: u8 }
fn fru_moves(w: W) { need_fn(|| W { n: 1, ..w }); }
#[derive(Clone, Copy)]
pub struct C { pub a: u8, pub b: u8 }
fn fru_copies(c: C) { need_fn(|| C { a: 1, ..c }); }

fn child_moves(s: String) { need_fn(|| async move { s.len() }); }
fn child_copies(n: u8) { need_fn(|| async move { n + 1 }); }

fn main() {}
"#;

/// What one body's check said, kept past the checker: the type check's codes and texts, the
/// borrow check's codes (only for `borrows`), and whether either stopped short.
#[derive(Debug)]
struct Said {
    name: String,
    types_clean: bool,
    codes: Vec<&'static str>,
    texts: Vec<String>,
    borrow_codes: Vec<&'static str>,
    stopped: bool,
}

fn verdicts(wanted: &[&str], borrows: bool) -> Vec<Said> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/kinds/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "kinds", dir: "/kinds/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2021").expect("Rust 2021"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let mut out = Vec::new();
    for &name in wanted {
        let key = names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
        let said = if borrows {
            let (v, b) = world.check_borrows(BodyRef::Free(key), alloc);
            Said {
                name: name.to_owned(),
                types_clean: v.is_clean(),
                codes: v.diagnostics.iter().map(|d| d.code).collect(),
                texts: v.diagnostics.iter().map(|d| d.text.to_string()).collect(),
                borrow_codes: b.findings.iter().map(|d| d.code).collect(),
                stopped: !(v.gaps.is_empty() && v.undecided.is_empty() && v.refused.is_none()) || b.refused.is_some(),
            }
        } else {
            let v = world.check(BodyRef::Free(key), alloc);
            Said {
                name: name.to_owned(),
                types_clean: v.is_clean() && v.reads.is_some(),
                codes: v.diagnostics.iter().map(|d| d.code).collect(),
                texts: v.diagnostics.iter().map(|d| d.text.to_string()).collect(),
                borrow_codes: Vec::new(),
                stopped: !(v.gaps.is_empty() && v.undecided.is_empty() && v.refused.is_none()),
            }
        };
        // The verdicts first, every one, before any assertion.
        println!("{}: types clean {} codes {:?} borrow codes {:?} stopped {} {:?}", said.name, said.types_clean, said.codes, said.borrow_codes, said.stopped, said.texts);
        out.push(said);
    }
    out
}

fn of<'r>(rows: &'r [Said], name: &str) -> &'r Said {
    rows.iter().find(|s| s.name == name).expect("checked above")
}

#[test]
fn a_moving_closure_called_twice_is_a_use_after_move() {
    let rows = verdicts(&["call_twice", "call_twice_ref"], true);
    let twice = of(&rows, "call_twice");
    assert!(twice.types_clean && !twice.stopped, "TWIN `call_twice` types check: {twice:?}");
    assert!(twice.borrow_codes.contains(&"E0382"), "TWIN `call_twice`: rustc reports E0382: {twice:?}");
    let ok = of(&rows, "call_twice_ref");
    assert!(ok.types_clean && !ok.stopped && !ok.borrow_codes.contains(&"E0382"), "`call_twice_ref`: an `Fn` called twice: {ok:?}");
}

#[test]
fn the_kind_is_the_strongest_use_of_a_place() {
    let rows = verdicts(&[
        "need_fn_once", "need_fn_ref", "match_moves", "match_refs", "fru_moves", "fru_copies", "child_moves", "child_copies",
    ], false);
    for n in ["need_fn_ref", "match_refs", "fru_copies", "child_copies"] {
        let s = of(&rows, n);
        assert!(s.types_clean && !s.stopped, "`{n}`: rustc accepts it (`Fn`): {s:?}");
    }
    for n in ["need_fn_once", "match_moves", "fru_moves", "child_moves"] {
        let s = of(&rows, n);
        assert!(!s.stopped, "TWIN `{n}`: a gap where rustc decides E0525: {s:?}");
        assert_eq!(s.codes.len(), 1, "TWIN `{n}`: rustc reports one error (E0525): {s:?}");
        assert!(s.texts[0].contains("Fn"), "TWIN `{n}`: the error names the `Fn` the closure is not: {s:?}");
    }
}
