//! Lane B11: the borrow check's new facts and the closure requirements, each with a control
//! that turns its provider off (rule 23: the cause removed, the effect gone), through
//! `ProjectWorld::check_borrows_planted` (front2/borrows.rs `FactsPlant`).
//!
//! Rules (the Reference): "Subtyping and Variance", "Lifetime bounds", "Type aliases" ("a new
//! name for an existing type"), "Copy" (a type is `Copy` by its impl, whose bounds are on the
//! arguments: `impl<T: Copy, U> Copy for Q<T, U>` makes `Q<u8, String>` `Copy`); the NLL RFC,
//! "Closures" (the closure's creator proves the relations among the closure's external regions
//! its body needs).
//!
//! Expected (each case prints `borrows <name> [<plant>]: [findings] refused ..`):
//! - FIELD REGIONS (`Facts::field_regions`): `pick` reads field `y: &'b u8` of `Two<'a, 'b>`
//!   and returns it as `&'b u8`: CLEAN; `pick_wrong` returns it as `&'a u8` with no `'b: 'a`:
//!   one outlives finding. CONTROL `NoFieldRegions`: `pick` is no longer clean and checked (a
//!   refusal by name, or a false finding: the field's lifetime is not known to be `'b`).
//! - COPY ARGS (`Facts::copy_args`): `Q<u8, String>` under `impl<T: Copy, U> Copy`: `twice`
//!   moves it twice: CLEAN. `twice_p` with `P<String>` under `impl<T: Copy> Copy for P<T>`:
//!   `P<String>` is not `Copy`, the second use is E0382. CONTROL `NoCopyArgs`: `twice` gives
//!   E0382 or a refusal (every argument then must be `Copy`, and `String` is not).
//! - ALIASES (the `SigWorld` the heads are read with): `via<'a>(x: R<'a>) -> R<'a>` with
//!   `type R<'a> = &'a u8`. `pass` returns `via(x)` for an `x: &'a u8` as `&'a u8`: CLEAN;
//!   `leak` returns `via(&local)` as `&'static u8`: one finding (the local does not live long
//!   enough). CONTROL `NoAliases`: `leak` is no longer a finding with refused None (the head's
//!   `R<'a>` is not expanded: the head is refused by name, or its result unrelated).
//! - CLOSURE REQUIREMENTS (borrows_nested.rs `Requirements`): `store_ok<'a>(slot: &mut &'a u8,
//!   x: &'a u8)` runs `|| *slot = x`: CLEAN (the closure's requirement `'a: 'a` holds).
//!   `store_bad<'a, 'b>` with `x: &'b u8`: a finding at the owner (`'b: 'a` is not declared),
//!   refused None. CONTROL `NoClosureRequirements`: both refused `NestedOutlives` (as before).
//! - BOX MOVE (class H, the fix is in blaze-flow-lower lib.rs, not in this lane: see the lane's
//!   report): `unbox_raw` moves out of `*unsafe { Box::from_raw(p) }`: CLEAN once that merges
//!   (E0507 before it). CONTROL (no plant; must stay found): `out_of_ref` moves out of `*r` for
//!   `r: &String`: E0507.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::{BorrowVerdict, FactsPlant};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const EDITION: &str = "blaze-2026.11";

const MAIN: &str = "fn main() {}

struct Two<'a, 'b> {
    x: &'a u8,
    y: &'b u8,
}

fn pick<'a, 'b>(t: Two<'a, 'b>) -> &'b u8 {
    let _ = t.x;
    t.y
}

fn pick_wrong<'a, 'b>(t: Two<'a, 'b>) -> &'a u8 {
    t.y
}

// `U` is held only behind a raw pointer (`Copy` whatever `U` is): no `PhantomData`, whose
// path does not resolve in this fixture's edition (second run, 2026-11-18: `AdtNotMade ..
// Resolve` at the field's type).
struct Q<T, U> {
    t: T,
    u: *const U,
}

impl<T: Copy, U> Clone for Q<T, U> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Copy, U> Copy for Q<T, U> {}

// A type that is not `Copy`, the program's own: in this edition `String` takes an allocator
// argument (first run, 2026-11-18: \"`String` is written with 0 type arguments; it takes 1\").
struct Owned {
    n: u8,
}

fn twice(q: Q<u8, Owned>) -> (Q<u8, Owned>, Q<u8, Owned>) {
    (q, q)
}

struct P<T> {
    t: T,
}

impl<T: Copy> Clone for P<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Copy> Copy for P<T> {}

fn twice_p(p: P<Owned>) -> (P<Owned>, P<Owned>) {
    (p, p)
}

type R<'a> = &'a u8;

fn via<'a>(x: R<'a>) -> R<'a> {
    x
}

fn pass<'a>(x: &'a u8) -> &'a u8 {
    via(x)
}

fn leak() -> &'static u8 {
    let local = 1u8;
    via(&local)
}

fn store_ok<'a>(slot: &mut &'a u8, x: &'a u8) {
    let mut f = || *slot = x;
    f();
}

fn store_bad<'a, 'b>(slot: &mut &'a u8, x: &'b u8) {
    let mut f = || *slot = x;
    f();
}

fn unbox_raw(p: *mut Owned) -> Owned {
    *unsafe { Box::from_raw(p) }
}

fn out_of_ref(r: &Owned) -> Owned {
    *r
}
";

fn shown(item: &str, v: &BorrowVerdict<'_>) -> String {
    let found: Vec<String> = v.findings.iter().map(|d| format!("{} at {:?}", d.code, item.get(d.start as usize..d.end as usize).unwrap_or("<out of range>"))).collect();
    format!("{found:?} refused {:?}", v.refused)
}

#[test]
fn b11_facts_and_closure_requirements() {
    // The tests' heap (blaze_own::testing): the one allocator a test is handed.
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/b11/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "b11", dir: "/b11/", root: "/b11/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(heap.alloc());
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
    // One body checked with one plant: (clean and checked, has a finding of `code` (any code
    // for ""), refused, the printed line).
    let mut check = |n: &str, plant: FactsPlant, code: &str| -> (bool, bool, bool, String) {
        let b = find(n);
        let (types, v) = world.check_borrows_planted(b, plant, heap.alloc());
        let line = shown(text_of(b), &v);
        println!("borrows {n} [{plant:?}]: {line}");
        assert!(types.is_clean(), "{n}'s types: {types:?}");
        let found = v.findings.iter().any(|d| code.is_empty() || d.code == code);
        (v.findings.is_empty() && v.refused.is_none(), found, v.refused.is_some(), line)
    };

    // FIELD REGIONS.
    let (clean, _, _, line) = check("pick", FactsPlant::None, "");
    assert!(clean, "pick: {line}");
    let (_, found, refused, line) = check("pick_wrong", FactsPlant::None, "");
    assert!(found && !refused, "pick_wrong: no finding: {line}");
    let (clean, _, _, line) = check("pick", FactsPlant::NoFieldRegions, "");
    assert!(!clean, "CONTROL pick without field regions is still clean: {line}");

    // COPY ARGS.
    let (clean, _, _, line) = check("twice", FactsPlant::None, "");
    assert!(clean, "twice: {line}");
    let (_, found, refused, line) = check("twice_p", FactsPlant::None, "E0382");
    assert!(found && !refused, "twice_p: no E0382: {line}");
    let (clean, _, _, line) = check("twice", FactsPlant::NoCopyArgs, "");
    assert!(!clean, "CONTROL twice without copy args is still clean: {line}");

    // ALIASES.
    let (clean, _, _, line) = check("pass", FactsPlant::None, "");
    assert!(clean, "pass: {line}");
    let (_, found, refused, line) = check("leak", FactsPlant::None, "");
    assert!(found && !refused, "leak: no finding: {line}");
    let (_, found, refused, line) = check("leak", FactsPlant::NoAliases, "");
    assert!(!(found && !refused), "CONTROL leak without the world is still found: {line}");

    // CLOSURE REQUIREMENTS.
    let (clean, _, _, line) = check("store_ok", FactsPlant::None, "");
    assert!(clean, "store_ok: {line}");
    let (_, found, refused, line) = check("store_bad", FactsPlant::None, "");
    assert!(found && !refused, "store_bad: no finding at the owner: {line}");
    for n in ["store_ok", "store_bad"] {
        let (_, _, _, line) = check(n, FactsPlant::NoClosureRequirements, "");
        assert!(line.contains("NestedOutlives"), "CONTROL {n} without requirements: {line}");
    }

    // BOX MOVE (class H): clean once blaze-flow-lower's fix merges; the control stays E0507.
    // OPEN (third run, 2026-11-18): `unbox_raw` is NOT checked here: in this fixture's edition
    // `Box::from_raw` does not resolve (its types: E0599 "found `Error`, about `from_raw`"), so
    // the body never reaches the borrow check and the class H fix (blaze-flow-lower `deref` and
    // `Task::DerefDone`) is unproven by this test. Owed: a fixture whose `Box` is the lang item
    // with `from_raw`, then `assert!(clean)` on `unbox_raw`.
    let (_, found, _, line) = check("out_of_ref", FactsPlant::None, "E0507");
    assert!(found, "CONTROL out_of_ref: no E0507: {line}");
}
