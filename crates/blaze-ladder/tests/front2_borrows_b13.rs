//! Lane B13's pieces on the front end's own facts (front2 borrows.rs; NEVER COMPILED when
//! written, 2026-11-18), each with its positive, its negative (rustc rejects it) and its control
//! (rule 23: the provider turned off, the effect gone):
//!
//! - IMPL REGIONS (D148: an impl's facts are its head record's regions, `RegionFacts::
//!   impl_header` and `impl_assoc`; the text reader of impl sources is deleted): `via(Src(x))`
//!   with `fn via<'de, D: De<'de>>(d: D) -> D::Out` and `impl<'s> De<'s> for Src<'s> { type
//!   Out = &'s u8; .. }` is answered by the impl the type check selected, through its header's
//!   column (`Src<'s>`: `[Param(0)]`) and its `type Out`'s (`&'s u8`: `[Param(0)]`) (serde's
//!   and thiserror's shape). `concrete` CLEAN; `concrete_bad` (returns it as `&'static u8`)
//!   FOUND. CONTROL `NoAliases` (the plant that withholds every head's regions, the impl's
//!   among them): `concrete` REFUSED (`RegionsUnread`), never accepted.
//! - ONE READ AN IMPL A BODY (B12's question): `twice` calls `via(Src(x))` twice and
//!   `Src(x).take()` (a member of the same impl): CLEAN. The old count (`impl_text_reads ==
//!   1`) has no equal: an impl's regions are its head record's columns read in place, never a
//!   text re-read per use, so there is no per-body read to count; what is left to assert is the
//!   verdict.
//! - VARIANCE (variance.rs; ADTs by `Facts::adt_variances`, from lifetimes.rs `variances`):
//!   `co` (`W<'b>` as `W<'a>`, `W` covariant) CLEAN; `co_bad` (the reverse) FOUND; `inv`
//!   (`C<'b>` as `C<'a>`, `C` holds a `Cell<&'w u8>`: invariant) FOUND; `pair` (`(&'b u8,
//!   &'b u8)` as `(&'a u8, &'a u8)`) CLEAN; `mut_short` (`&'a mut &'b u8` as `&'a mut &'a u8`,
//!   the classic unsoundness) FOUND; `ok_de` (B7's finding: `(&'x u8, D::Error)` from a callee's
//!   `(&'de u8, Self::Error)` with `'de: 'x`) CLEAN. CONTROL `NoVariances`: `co` NOT CLEAN
//!   (`W`'s parameter related both ways, as before).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`. The heap is one root of
//! the test's own, kept for the process because the world takes `Alloc<'static>` (as
//! tests/record_bytes.rs does).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::{BorrowVerdict, FactsPlant};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_mem::{Root, RootOpts};

const EDITION: &str = "blaze-2026.11";

const MAIN: &str = "use std::cell::Cell;

fn main() {}

trait De<'de> {
    type Out;
    fn take(self) -> Self::Out;
}

struct Src<'s>(&'s u8);

impl<'s> De<'s> for Src<'s> {
    type Out = &'s u8;
    fn take(self) -> &'s u8 {
        self.0
    }
}

fn via<'de, D: De<'de>>(d: D) -> D::Out {
    d.take()
}

fn concrete<'a>(x: &'a u8) -> &'a u8 {
    via(Src(x))
}

fn concrete_bad<'a>(x: &'a u8) -> &'static u8 {
    via(Src(x))
}

fn twice<'a>(x: &'a u8) -> &'a u8 {
    let _ = via(Src(x));
    let _ = Src(x).take();
    via(Src(x))
}

struct W<'w>(&'w u8);

struct C<'w>(Cell<&'w u8>);

fn co<'a, 'b: 'a>(x: W<'b>) -> W<'a> {
    x
}

fn co_bad<'a, 'b: 'a>(x: W<'a>) -> W<'b> {
    x
}

fn inv<'a, 'b: 'a>(x: C<'b>) -> C<'a> {
    x
}

fn pair<'a, 'b: 'a>(x: (&'b u8, &'b u8)) -> (&'a u8, &'a u8) {
    x
}

fn mut_short<'a, 'b: 'a>(x: &'a mut &'b u8) -> &'a mut &'a u8 {
    x
}

trait Tk<'de> {
    type Error;
    fn take(self) -> (&'de u8, Self::Error);
}

fn ok_de<'de: 'x, 'x, D: Tk<'de>>(d: D) -> (&'x u8, D::Error) {
    d.take()
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
fn b13_impl_sources_variance_and_one_read() {
    let root = root();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/b13/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "b13", dir: "/b13/", root: "/b13/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
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
    // One body checked with one plant: (clean and checked, has a finding, refused, the printed
    // line).
    let mut check = |n: &str, plant: FactsPlant| -> (bool, bool, bool, String) {
        let b = find(n);
        let (types, v) = world.check_borrows_planted(b, plant, root.alloc());
        let line = shown(text_of(b), &v);
        println!("borrows {n} [{plant:?}]: {line}");
        assert!(types.is_clean(), "{n}'s types: {types:?}");
        (v.findings.is_empty() && v.refused.is_none(), !v.findings.is_empty(), v.refused.is_some(), line)
    };

    // IMPL REGIONS.
    let (clean, _, _, line) = check("concrete", FactsPlant::None);
    assert!(clean, "concrete: {line}");
    let (_, found, refused, line) = check("concrete_bad", FactsPlant::None);
    assert!(found && !refused, "concrete_bad: no finding: {line}");
    let (clean, _, refused, line) = check("concrete", FactsPlant::NoAliases);
    assert!(!clean && refused, "CONTROL concrete without head regions is not refused: {line}");
    assert!(line.contains("RegionsUnread"), "CONTROL concrete without head regions is refused for another reason: {line}");

    // ONE IMPL, THREE USES IN ONE BODY: clean (no read count to assert: the module doc).
    let (clean, _, _, line) = check("twice", FactsPlant::None);
    assert!(clean, "twice: {line}");

    // VARIANCE.
    let (clean, _, _, line) = check("co", FactsPlant::None);
    assert!(clean, "co: {line}");
    let (_, found, refused, line) = check("co_bad", FactsPlant::None);
    assert!(found && !refused, "co_bad: no finding: {line}");
    let (_, found, refused, line) = check("inv", FactsPlant::None);
    assert!(found && !refused, "inv (Cell, invariant): no finding: {line}");
    let (clean, _, _, line) = check("pair", FactsPlant::None);
    assert!(clean, "pair: {line}");
    let (_, found, refused, line) = check("mut_short", FactsPlant::None);
    assert!(found && !refused, "mut_short (the classic unsoundness): no finding: {line}");
    let (clean, _, _, line) = check("ok_de", FactsPlant::None);
    assert!(clean, "ok_de (B7's finding): {line}");
    let (clean, _, _, line) = check("co", FactsPlant::NoVariances);
    assert!(!clean, "CONTROL co without ADT variances is still clean: {line}");
}
