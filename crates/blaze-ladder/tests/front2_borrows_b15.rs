//! Lane B15 on the front end's own facts (front2 borrows.rs; NEVER COMPILED when written,
//! 2026-11-18): a fn-pointer type knows which lifetimes its `for<..>` binds (the head's region
//! record, D148: a bound lifetime is `Region::Late { binder, var }`), and the borrow check uses
//! it. Each case prints
//! `borrows <name> [<plant>]: [findings] refused ..`.
//!
//! - (a) BOUND, INSTANTIATED PER CALL. `call(f: for<'r> fn(&'r u8) -> &'r u8, x: &u8) -> &u8
//!   { f(x) }` and its elided twin `call_elided(f: fn(&u8) -> &u8, ..)` are clean: the
//!   Reference, "Function pointer types" and "Higher-ranked trait bounds" (`for<'r>` binds `'r`
//!   for the pointer: each call picks it), and "Lifetime elision" ("Each elided lifetime in the
//!   parameters becomes a distinct lifetime parameter", in a fn pointer's own parameters, so
//!   `fn(&u8) -> &u8` IS `for<'r> fn(&'r u8) -> &'r u8`); and the enclosing fn's elided output
//!   takes `x`'s lifetime, the only one its parameters use that is not the pointer's. Prints
//!   `borrows call [None]: [] refused None` (and the same for `call_elided`).
//! - (b) A FIELD'S FREE LIFETIME, CONTRAVARIANT. `struct S<'a> { f: fn(&'a u8) }`: "Subtyping
//!   and Variance", the table: `fn(T) -> ()` is contravariant in `T`, so `fn(&'static u8)` is
//!   a subtype of `fn(&'a u8)` only when `'a: 'static`.
//!   - `store_static_bad<'a>(s: &mut S<'a>, g: fn(&'static u8)) { s.f = g; }`: a finding.
//!   - `store_any_ok(t: &mut T, g: fn(&u8))` with `struct T { f: fn(&'static u8) }` (the other
//!     direction: `fn(&'b u8)` where `fn(&'static u8)` is wanted, `'static: 'b` holds): clean.
//!   - the ADT's own variance (computed from its fields by the variance table: `S` is
//!     contravariant in `'a`):
//!     `grow<'a>(s: S<'a>) -> S<'static> { s }` clean, `shrink<'a>(s: S<'static>) -> S<'a> { s }`
//!     a finding (it needs `'a: 'static`).
//! - (c) A FREE OUTPUT LIFETIME THROUGH A FN-POINTER VALUE. `get<'a>(f: fn() -> &'a u8) -> &'a
//!   u8 { f() }` is clean: `'a` is free in the pointer's type ("Function pointer types": only a
//!   `for<..>` binds), so the call's result is the region the parameter's own type holds there.
//!   `get_static<'a>(f: fn() -> &'a u8) -> &'static u8 { f() }` is a finding (`'a: 'static`).
//!   Before B15 both were refused `CalleeValueUnknown` (`CallableRefusal::OuterLifetime`).
//! - (d) CONTROL `FnBindersFree` (rule 23: the binder's reading removed): every bound lifetime
//!   read as free, the body's own a placeholder and a call's taken from the parameter's own
//!   position. `call` and `call_elided` are then no longer clean (a false finding: `x`'s region
//!   must outlive the pointer's one placeholder and back), while `get` (free only) stays clean.
//!
//! NEEDS (LINES ELSEWHERE in lane B15's report): front2/borrow_fields.rs walking a fn
//! pointer's positions and mapping its bound lifetimes (`Region::Late`); without them (b)'s `s.f = g` and `t.f = g`
//! are refused (the field's place has no row for the pointer's position).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`. The heap is one root of
//! the test's own, kept for the process because the world takes `Alloc<'static>` (as
//! front2_borrows_b14.rs does).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::{BorrowVerdict, FactsPlant};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_mem::{Root, RootOpts};

const EDITION: &str = "2024";

const MAIN: &str = "fn main() {}

struct S<'a> {
    f: fn(&'a u8),
}

struct T {
    f: fn(&'static u8),
}

fn call(f: for<'r> fn(&'r u8) -> &'r u8, x: &u8) -> &u8 {
    f(x)
}

fn call_elided(f: fn(&u8) -> &u8, x: &u8) -> &u8 {
    f(x)
}

fn store_static_bad<'a>(s: &mut S<'a>, g: fn(&'static u8)) {
    s.f = g;
}

fn store_any_ok(t: &mut T, g: fn(&u8)) {
    t.f = g;
}

fn grow<'a>(s: S<'a>) -> S<'static> {
    s
}

fn shrink<'a>(s: S<'static>) -> S<'a> {
    s
}

fn get<'a>(f: fn() -> &'a u8) -> &'a u8 {
    f()
}

fn get_static<'a>(f: fn() -> &'a u8) -> &'static u8 {
    f()
}
";

fn shown(item: &str, v: &BorrowVerdict<'_>) -> String {
    let found: Vec<String> = v.findings.iter().map(|d| format!("{} at {:?}", d.code, item.get(d.start as usize..d.end as usize).unwrap_or("<out of range>"))).collect();
    format!("{found:?} refused {:?}", v.refused)
}

/// The test's heap: one root, made here and kept for the process.
fn root() -> &'static Root {
    let root = Root::new(RootOpts::TEST_ARENA).unwrap_or_else(|e| panic!("the test's root: {e}"));
    Root::leak(root).unwrap_or_else(|e| panic!("the test's root, kept for the process: {e}"))
}

#[test]
fn b15_fn_pointer_binders() {
    let root = root();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/b15/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "b15", dir: "/b15/", root: "/b15/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, root.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(root.alloc());
    let name_of = |b: &BodyRef<'_>| -> String {
        match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        }
    };
    let find = |n: &str| bodies.iter().find(|(b, _)| name_of(b) == n).map(|(b, _)| *b).unwrap_or_else(|| panic!("`{n}` listed"));
    let text_of = |b: BodyRef<'_>| {
        let k = match b {
            BodyRef::Free(k) => k,
            BodyRef::Member { container, .. } => container,
        };
        names.item_text(k).expect("the body's item text").text
    };
    // One body checked with one plant: (clean and checked, has a finding, refused, the line).
    let mut check = |n: &str, plant: FactsPlant| -> (bool, bool, bool, String) {
        let b = find(n);
        let (types, v) = world.check_borrows_planted(b, plant, root.alloc());
        let line = shown(text_of(b), &v);
        println!("borrows {n} [{plant:?}]: {line}");
        assert!(types.is_clean(), "{n}'s types: {types:?}");
        (v.findings.is_empty() && v.refused.is_none(), !v.findings.is_empty(), v.refused.is_some(), line)
    };

    // (a), (b)'s clean halves, (c)'s clean half.
    for n in ["call", "call_elided", "store_any_ok", "grow", "get"] {
        let (clean, _, _, line) = check(n, FactsPlant::None);
        assert!(clean, "{n}: {line}");
    }
    // (b) and (c): rustc refuses each (the module's rules); a finding, never a refusal.
    for n in ["store_static_bad", "shrink", "get_static"] {
        let (_, found, refused, line) = check(n, FactsPlant::None);
        assert!(found && !refused, "{n}: no finding: {line}");
    }
    // (d) CONTROL: a reading that knew no binder makes (a) wrong; (c)'s free lifetime is not
    // the binder's and stays clean.
    for n in ["call", "call_elided"] {
        let (clean, _, _, line) = check(n, FactsPlant::FnBindersFree);
        assert!(!clean, "CONTROL {n} with every fn-pointer lifetime free: still clean: {line}");
    }
    let (clean, _, _, line) = check("get", FactsPlant::FnBindersFree);
    assert!(clean, "CONTROL get (free only): {line}");
}
