//! A few end-to-end checks: source to LLBC through Charon's own passes, read off its pretty
//! printer.

use base_db::{Crate, Db, all_crates};
use charon_lib::ast::{RefKind, Region, TyKind};
use hir_expand::fixture;
// Links the crate that provides the MIR queries.
use hir_ty_mir as _;

use crate::{Error, Unsupported, llbc_json, to_llbc, translate_crate};

const HEADER: &str = "//- minicore: sized, copy, add, ord, eq, fn, builtin_impls\n//- /lib.rs crate:main cfg:overflow_checks\n";

fn main_crate(db: &Db) -> Crate {
    *all_crates(db)
        .iter()
        .find(|krate| crate::names::crate_name(db, **krate) == "main")
        .expect("the fixture has a crate `main`")
}

fn db(source: &str) -> Db {
    fixture::db(&format!("{HEADER}{source}")).into_db()
}

fn llbc(source: &str) -> String {
    let db = db(source);
    match to_llbc(&db, main_crate(&db)) {
        Ok(krate) => krate.to_string(),
        Err(err) => panic!("no LLBC: {err}"),
    }
}

fn unsupported(source: &str) -> Vec<Unsupported> {
    let db = db(source);
    match to_llbc(&db, main_crate(&db)) {
        Err(Error::Unsupported(all)) => all,
        Err(err) => panic!("expected refusals, got: {err}"),
        Ok(krate) => panic!("expected refusals, got LLBC:\n{krate}"),
    }
}

#[test]
fn checked_add_is_a_panicking_add() {
    let out = llbc("pub fn incr(x: u32) -> u32 { x + 1 }\n");
    assert!(out.contains("panic.+"), "{out}");
    assert!(!out.contains("assert"), "{out}");
}

#[test]
fn while_is_a_loop() {
    let out = llbc("pub fn count(n: u32) -> u32 { let mut i = 0; while i < n { i += 1; } i }\n");
    assert!(out.contains("loop {"), "{out}");
}

#[test]
fn enum_match_is_a_match() {
    let out = llbc(
        "pub struct P { pub x: u32, pub y: u32 }
pub enum E { A, B(u32) }
pub fn get(e: E, p: P) -> u32 { match e { E::A => p.x, E::B(v) => v } }
",
    );
    assert!(out.contains("match "), "{out}");
}

#[test]
fn mut_ref_param_is_region_bound() {
    let db = db("pub fn bump(x: &mut u32) { *x += 1; }\n");
    let krate = match to_llbc(&db, main_crate(&db)) {
        Ok(krate) => krate,
        Err(err) => panic!("no LLBC: {err}"),
    };
    let bump = krate
        .fun_decls
        .iter()
        .find(|decl| crate::names::display(&decl.item_meta.name).ends_with("::bump"))
        .expect("`bump` is declared");
    assert_eq!(bump.generics.regions.len(), 1);
    let input = &bump.signature.inputs[0];
    assert!(
        matches!(input.kind(), TyKind::Ref(Region::Var(_) | Region::Body(_), _, RefKind::Mut)),
        "{input:?}"
    );
}

#[test]
fn closure_is_refused_by_name() {
    let all = unsupported("pub fn f() -> u32 { let g = |x: u32| x; g(1) }\n");
    assert!(
        all.iter().any(|it| it.function == "main::f" && it.construct.contains("closure")),
        "{all:?}"
    );
    let all = unsupported("pub fn p(x: *const u32) -> *const u32 { x }\n");
    assert!(
        all.iter().any(|it| it.function == "main::p" && it.construct.contains("raw pointer")),
        "{all:?}"
    );
}

#[test]
fn json_is_deterministic() {
    let source =
        "pub fn incr(x: u32) -> u32 { x + 1 }\npub fn twice(x: u32) -> u32 { incr(incr(x)) }\n";
    let first = {
        let db = db(source);
        llbc_json(&to_llbc(&db, main_crate(&db)).expect("LLBC"))
    };
    let second = {
        let db = db(source);
        llbc_json(&to_llbc(&db, main_crate(&db)).expect("LLBC"))
    };
    same_json(&first, &second);
    // The ULLBC alone is the same twice too.
    let db = db(source);
    let a = translate_crate(&db, main_crate(&db)).expect("ULLBC");
    let b = translate_crate(&db, main_crate(&db)).expect("ULLBC");
    same_json(&llbc_json(&a), &llbc_json(&b));
}

/// `a` and `b` are the same bytes; when not, the failure shows where they first part.
fn same_json(a: &[u8], b: &[u8]) {
    if a == b {
        return;
    }
    let at = a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
    let window = |it: &[u8]| {
        String::from_utf8_lossy(&it[at.saturating_sub(300)..(at + 300).min(it.len())]).into_owned()
    };
    panic!(
        "the JSON differs at byte {at} (lengths {} and {}):\n  first: {}\n second: {}",
        a.len(),
        b.len(),
        window(a),
        window(b)
    );
}
