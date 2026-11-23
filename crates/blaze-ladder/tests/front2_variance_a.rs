#![feature(allocator_api)]
//! THE VARIANCE OF EVERY ADT PARAMETER (D190 point 2, front2 adt_variance.rs and adt_facts.rs):
//! each rule of the Reference's "Subtyping and Variance" table on a reduced program, with its
//! NEGATIVE TWIN (the same shape where Rust refuses), and the control: with no variances filed
//! (`FactsPlant::NoVariances`) the clean ones are no longer clean (rule 23: the cause removed,
//! the effect gone).
//!
//! - `Option<&'static str>` to `Option<&'a str>`: `Option<T>` covariant, from its definition;
//! - `Cell<T>` invariant through `UnsafeCell` (lang item): `set` of a shorter value, and the
//!   value returned at a shorter lifetime, are errors;
//! - a struct of a `fn(&'a str)`: contravariant in `'a`; its twin the other way;
//! - two ADTs that name each other: covariant through the fixed point; the twin pair holds a
//!   `Cell` and is invariant;
//! - a type parameter only under `&'a mut T`: covariant in `'a`, invariant in `T`;
//! - the sampler's case (tempo): iterating `&Vec<(&'static str, u8)>`, `Iter<'_, T>` covariant
//!   in `T` through `NonNull` and `PhantomData`.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::{BorrowVerdict, FactsPlant};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_mem::{Root, RootOpts};

// Plain Rust (the fixtures write `Box<B<'a, T>>` and `Vec<..>`): under a blaze edition `std`
// is bstd, whose `Box<T, A>` and `Vec<T, A>` take their allocator with no default.
const EDITION: &str = "2024";

const MAIN: &str = "use std::cell::Cell;

fn main() {}

fn opt_co<'a>(x: Option<&'static str>) -> Option<&'a str> {
    x
}

fn opt_co_bad<'a>(x: Option<&'a str>) -> Option<&'static str> {
    x
}

fn cell_set<'a>(x: &Cell<&'static str>, s: &'a str) {
    x.set(s)
}

fn cell_set_ok(x: &Cell<&'static str>, s: &'static str) {
    x.set(s)
}

fn cell_out<'a>(x: Cell<&'static str>) -> Cell<&'a str> {
    x
}

fn cell_out_same<'a>(x: Cell<&'a str>) -> Cell<&'a str> {
    x
}

struct F<'a>(fn(&'a str));

fn contra<'a>(f: F<'a>) -> F<'static> {
    f
}

fn contra_bad<'a>(f: F<'static>) -> F<'a> {
    f
}

struct A<'a, T> {
    b: Option<Box<B<'a, T>>>,
    x: &'a T,
}

struct B<'a, T> {
    a: Option<Box<A<'a, T>>>,
}

fn rec<'a, 'b: 'a>(x: A<'b, u8>) -> A<'a, u8> {
    x
}

struct P<'a> {
    q: Option<Box<Q<'a>>>,
}

struct Q<'a> {
    p: Option<Box<P<'a>>>,
    c: Cell<&'a u8>,
}

fn rec_bad<'a, 'b: 'a>(x: P<'b>) -> P<'a> {
    x
}

struct M<'a, T>(&'a mut T);

fn m_co<'a, 'b: 'a>(x: M<'b, u8>) -> M<'a, u8> {
    x
}

fn m_bad<'a, 'b: 'a>(x: M<'a, &'b u8>) -> M<'a, &'a u8> {
    x
}

fn vec_iter(v: &Vec<(&'static str, u8)>) -> usize {
    let mut n = 0;
    for (name, _) in v {
        n += name.len();
    }
    n
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
fn variance_of_every_adt_parameter() {
    let root = root();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/va/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "va", dir: "/va/", root: "/va/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
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
    let clean = |check: &mut dyn FnMut(&str, FactsPlant) -> (bool, bool, bool, String), n: &str| {
        let (clean, _, _, line) = check(n, FactsPlant::None);
        assert!(clean, "{n} is not clean: {line}");
        // The control: the cause (the variances) removed, the effect (a clean check) gone.
        let (clean, _, _, line) = check(n, FactsPlant::NoVariances);
        assert!(!clean, "CONTROL {n} is clean with no variance filed: {line}");
    };
    let error = |check: &mut dyn FnMut(&str, FactsPlant) -> (bool, bool, bool, String), n: &str| {
        let (_, found, refused, line) = check(n, FactsPlant::None);
        assert!(found && !refused, "{n}: no finding, or refused: {line}");
    };

    // `Option<T>` covariant (computed from its definition), and the twin.
    clean(&mut check, "opt_co");
    error(&mut check, "opt_co_bad");
    // `Cell<T>` invariant through `UnsafeCell` (lang item): both twins are errors; the
    // same-lifetime forms are clean.
    error(&mut check, "cell_set");
    error(&mut check, "cell_out");
    let (ok, _, _, line) = check("cell_set_ok", FactsPlant::None);
    assert!(ok, "cell_set_ok: {line}");
    let (ok, _, _, line) = check("cell_out_same", FactsPlant::None);
    assert!(ok, "cell_out_same: {line}");
    // `fn(&'a str)` contravariant in `'a`.
    clean(&mut check, "contra");
    error(&mut check, "contra_bad");
    // A mutually recursive pair: covariant through the fixed point; the pair holding a `Cell`
    // is invariant.
    clean(&mut check, "rec");
    error(&mut check, "rec_bad");
    // Only under `&'a mut T`: covariant in `'a`, invariant in `T`.
    clean(&mut check, "m_co");
    error(&mut check, "m_bad");
    // The sampler's case: `slice::Iter<'_, (&'static str, u8)>` covariant in `T`.
    let (ok, _, _, line) = check("vec_iter", FactsPlant::None);
    assert!(ok, "vec_iter: {line}");
}
