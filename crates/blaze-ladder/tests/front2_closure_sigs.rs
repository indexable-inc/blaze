//! A closure's signature taken from a higher-ranked `Fn` bound when the bound's input is a trait
//! object (blaze-flow-lower closure.rs `lay_sig_typed`, the `(Dynamic, Dynamic)` arm), and a
//! body's own signature whose parameter is a projection the body has normalized (lib.rs
//! `Lower::lay_over`). Both come from tempo's refusal groups (c2135wave69 refused-by-cause.txt:
//! 192 `ClosureSigUnaligned`, 239 `SigPositionsUnaligned`).
//!
//! Closures. `fn on<F: FnMut(&dyn E)>(f: F)` is `for<'x> FnMut(&'x (dyn E + 'x))`: the object
//! lifetime of `&dyn E` defaults to the reference's own (the Reference, "Default trait object
//! lifetime bounds"), so the binder's region is the object's lifetime too. In the flow a trait
//! object is one position (its object lifetime), so the clause's slots for `dyn E + 'x` are that
//! one slot, then its predicates' (none here).
//! - `user`: `on(|e| { let _ = e; })`: CLEAN.
//! - TWIN `user_keeps` (rule 23): the closure stores `e` in a captured `Option<&dyn E>`: rustc
//!   E0521 "borrowed data escapes outside of closure" (`'x` is the bound's, chosen per call,
//!   and the capture's region outlives the closure). It must be a FINDING: not clean, and not a
//!   refusal. `FnMut`, not `Fn`, in both: an assignment to a capture in an `Fn` closure is E0594
//!   first.
//! Before the arm, `user` was refused `ClosureSigUnaligned { why: Shape { declared: "dyn",
//! closure: "dyn" } }`.
//!
//! Projections. `impl<'a> Tr for Wrap<'a> { type A = Inner<'a>; }`, so `<Wrap<'a> as Tr>::A`
//! is `Inner<'a>` (the Reference, "Associated types").
//! - `peek<'a>(x: <Wrap<'a> as Tr>::A) -> u8 { *x.0 }`: CLEAN (no relation is needed).
//! - `get<'a>(x: <Wrap<'a> as Tr>::A) -> &'a u8 { x.0 }`: rustc ACCEPTS it, because `x.0` is
//!   `&'a u8` once the parameter is normalized. It must be CLEAN: no false "must outlive" note.
//!   UNSURE: `lay_over` gives the normalized positions hidden placeholders, related to nothing.
//!   If the head's column keeps the projection as ONE position, the placeholder at `Inner<'a>`'s
//!   `'a` is not known to outlive `'a`, and this prints a false finding. If it does, that is the
//!   known gap and the assertion names it.
//! - TWIN `get_static<'a>(x: <Wrap<'a> as Tr>::A) -> &'static u8 { x.0 }`: rustc refuses it
//!   ("lifetime may not live long enough"). It must be a FINDING: a placeholder must never be
//!   accepted as outliving what it was not shown to outlive.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowVerdict;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const MAIN: &str = "fn main() {}

pub trait E {}
fn on<F: FnMut(&dyn E)>(f: F) {
    let _ = f;
}

fn user() {
    on(|e| {
        let _ = e;
    });
}

fn user_keeps() {
    let mut keep: Option<&dyn E> = None;
    on(|e| {
        keep = Some(e);
    });
    let _ = keep;
}

pub struct Inner<'a>(pub &'a u8);
pub struct Wrap<'a>(pub &'a u8);
pub trait Tr {
    type A;
}
impl<'a> Tr for Wrap<'a> {
    type A = Inner<'a>;
}

fn peek<'a>(x: <Wrap<'a> as Tr>::A) -> u8 {
    *x.0
}

fn get<'a>(x: <Wrap<'a> as Tr>::A) -> &'a u8 {
    x.0
}

fn get_static<'a>(x: <Wrap<'a> as Tr>::A) -> &'static u8 {
    x.0
}
";

fn shown(item: &str, v: &BorrowVerdict<'_>) -> String {
    let found: Vec<String> = v.findings.iter().map(|d| format!("{} at {:?}: {}", d.code, item.get(d.start as usize..d.end as usize).unwrap_or("<out of range>"), d.text)).collect();
    format!("{found:?} refused {:?}", v.refused)
}

#[test]
fn closure_and_projection_signatures_are_laid_and_checked() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/sigs/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
    let user = UserCrate { name: "sigs", dir: "/sigs/", root: "/sigs/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
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

    // Print every verdict first, so one run shows them all, whichever assertion fails.
    let mut verdicts = Vec::new();
    for n in ["user", "user_keeps", "peek", "get", "get_static"] {
        let b = find(n);
        let (types, v) = world.check_borrows(b, heap.alloc());
        let line = shown(text_of(b), &v);
        println!("borrows {n}: types clean {} {line}", types.is_clean());
        verdicts.push((n, types.is_clean(), v.refused.is_none(), v.findings.len(), line));
    }
    let of = |n: &str| verdicts.iter().find(|row| row.0 == n).expect("checked above");

    for n in ["user", "peek"] {
        let (_, types, checked, found, line) = of(n);
        assert!(*types, "`{n}`: its types are not clean: {line}");
        assert!(*checked, "`{n}` not borrow-checked (refused): {line}");
        assert_eq!(*found, 0, "ORACLE `{n}`: rustc accepts it: {line}");
    }
    for n in ["user_keeps", "get_static"] {
        let (_, types, checked, found, line) = of(n);
        assert!(*types, "CONTROL `{n}`: its types are not clean: {line}");
        assert!(*checked, "CONTROL `{n}` not borrow-checked (refused): {line}");
        assert!(*found > 0, "CONTROL `{n}`: rustc refuses it (a borrow escapes / may not live long enough), yet it checked clean: {line}");
    }
    let (_, types, checked, found, line) = of("get");
    assert!(*types && *checked, "`get` not checked: {line}");
    assert_eq!(
        *found, 0,
        "KNOWN GAP if this fails: `get` relies on `<Wrap<'a> as Tr>::A` being `Inner<'a>`; the normalized position is a hidden placeholder (blaze-flow-lower lib.rs `lay_over`), not `'a`: {line}"
    );
}
