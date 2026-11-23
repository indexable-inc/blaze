//! Lane B14's pieces on the front end's own facts (front2 borrows.rs; NEVER COMPILED when
//! written, 2026-11-18), each with its positive, its negative (rustc rejects it) and its control
//! (rule 23: the provider turned off, the effect gone). Edition 2024: Rust's std, so `Box` is
//! the `owned_box` lang item with `Box::from_raw`.
//!
//! - ASYNC BLOCK REQUIREMENTS (blaze-flow-lower coroutine.rs `LoweredAsync::externals`,
//!   borrows_nested.rs `nested_inner_first`): the NLL RFC, "Closures" (the creator proves the
//!   relations among the nested body's external regions it needs), applied to async blocks by
//!   the Reference, "Async blocks" ("Async blocks capture variables from their environment
//!   using the same capture modes as closures"), as rustc does (an async block's body is its
//!   own MIR body whose region requirements propagate to its creator).
//!   - `astore_ok<'a>(slot: &mut &'a u8, x: &'a u8)` makes `async { *slot = x; }`: the block
//!     needs `'a: 'a`. Prints `borrows astore_ok [None]: [] refused None`.
//!   - `astore_bad<'a, 'b>` with `x: &'b u8`: the block needs `'b: 'a`, which the owner does
//!     not declare ("Subtyping and Variance": `&'b u8` is a subtype of `&'a u8` only when
//!     `'b: 'a`; `&mut T` is invariant in `T`). Prints one finding at the block, refused None.
//!   - `astatic_ok(x: &'static u8)` makes `async move { need(x) }` with `fn need(_: &'static
//!     u8)`: the block needs its by-value capture's region to be `'static`, and it is. Prints
//!     `[] refused None`.
//!   - `astatic_bad<'a>(x: &'a u8)`: the same needs `'a: 'static`: one finding at the block,
//!     refused None.
//!   - `anest_bad<'a>(x: &'a u8)` makes `async move { let g = || need(x); g(); }`: the
//!     closure's requirement (its captured `x`'s referent `'static`) is the block's, and the
//!     block's is the owner's (innermost first): one finding, refused None.
//!   - CONTROL `NoClosureRequirements` (no externals given, so nothing is mapped): every one of
//!     the five is refused `NestedOutlives` (the form before this piece), so `astore_ok` and
//!     `astatic_ok` are no longer clean.
//! - BOX MOVE (lane B11's owed proof, class H: blaze-flow-lower lib.rs `deref`,
//!   `Task::DerefDone`): the Reference, "Moved and copied types" and "Dereference expression":
//!   `*b` for `b: Box<T>` is a place the value may be moved out of (the box's lang item: a move
//!   out of `*Box` is allowed, unlike out of `*&T`). `unbox_raw` moves out of `*unsafe {
//!   Box::from_raw(p) }`: prints `[] refused None`. CONTROL (no plant; must stay found):
//!   `out_of_ref` moves out of `*r` for `r: &Owned` ("cannot move out of a shared reference"):
//!   prints one `E0507` finding.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`. The heap is one root of
//! the test's own, kept for the process because the world takes `Alloc<'static>` (as
//! front2_borrows_b13.rs does).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::{BorrowVerdict, FactsPlant};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_mem::{Root, RootOpts};

const EDITION: &str = "2024";

const MAIN: &str = "fn main() {}

struct Owned {
    n: u8,
}

fn need(_: &'static u8) {}

fn astore_ok<'a>(slot: &mut &'a u8, x: &'a u8) {
    let _f = async {
        *slot = x;
    };
}

fn astore_bad<'a, 'b>(slot: &mut &'a u8, x: &'b u8) {
    let _f = async {
        *slot = x;
    };
}

fn astatic_ok(x: &'static u8) {
    let _f = async move {
        need(x);
    };
}

fn astatic_bad<'a>(x: &'a u8) {
    let _f = async move {
        need(x);
    };
}

fn anest_bad<'a>(x: &'a u8) {
    let _f = async move {
        let g = || need(x);
        g();
    };
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

/// The test's heap: one root, made here and kept for the process.
fn root() -> &'static Root {
    let root = Root::new(RootOpts::TEST_ARENA).unwrap_or_else(|e| panic!("the test's root: {e}"));
    Root::leak(root).unwrap_or_else(|e| panic!("the test's root, kept for the process: {e}"))
}

#[test]
fn b14_async_requirements_and_box_move() {
    let root = root();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/b14/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "b14", dir: "/b14/", root: "/b14/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
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
    // One body checked with one plant: (clean and checked, has a finding of `code` (any code
    // for ""), refused, the printed line).
    let mut check = |n: &str, plant: FactsPlant, code: &str| -> (bool, bool, bool, String) {
        let b = find(n);
        let (types, v) = world.check_borrows_planted(b, plant, root.alloc());
        let line = shown(text_of(b), &v);
        println!("borrows {n} [{plant:?}]: {line}");
        assert!(types.is_clean(), "{n}'s types: {types:?}");
        let found = v.findings.iter().any(|d| code.is_empty() || d.code == code);
        (v.findings.is_empty() && v.refused.is_none(), found, v.refused.is_some(), line)
    };

    // ASYNC BLOCK REQUIREMENTS.
    for n in ["astore_ok", "astatic_ok"] {
        let (clean, _, _, line) = check(n, FactsPlant::None, "");
        assert!(clean, "{n}: {line}");
    }
    for n in ["astore_bad", "astatic_bad", "anest_bad"] {
        let (_, found, refused, line) = check(n, FactsPlant::None, "");
        assert!(found && !refused, "{n}: no finding at the owner: {line}");
    }
    for n in ["astore_ok", "astore_bad", "astatic_ok", "astatic_bad", "anest_bad"] {
        let (clean, _, _, line) = check(n, FactsPlant::NoClosureRequirements, "");
        assert!(!clean && line.contains("NestedOutlives"), "CONTROL {n} without requirements: {line}");
    }

    // BOX MOVE (class H): the move out of a box is clean; out of a shared reference E0507.
    let (clean, _, _, line) = check("unbox_raw", FactsPlant::None, "");
    assert!(clean, "unbox_raw: {line}");
    let (_, found, _, line) = check("out_of_ref", FactsPlant::None, "E0507");
    assert!(found, "CONTROL out_of_ref: no E0507: {line}");
}
