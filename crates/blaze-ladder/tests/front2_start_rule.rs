//! Which programs start through the library's `#[lang = "start"]` (LADDER.md L1a, wiring
//! ladder-L1a-2.md; front2/start.rs `linked_lang_start`): decided from what the crate graph
//! links, by Rust's rule, never from the edition alone.
//!
//! The rule's sources. The pinned std (nightly-2026-10-27, library/std/src/rt.rs:197) declares
//! `#[lang = "start"] fn lang_start<T: crate::process::Termination + 'static>(..)`; a lang
//! item is found in the crates linked into the binary. The Reference, "Preludes", extern
//! prelude: "If the `no_std` attribute is specified, then the `std` crate is not added to the
//! extern prelude" (core is); so a `#![no_std]` root that writes no `extern crate std` links no
//! std, and a program whose graph holds no std at all (the ladder's rungs run without a shelf)
//! has no `#[lang = "start"]`: both keep the direct call of `fn main` (`Entry2::Direct`).
//!
//! The control: ladder-L1a-1's rule ("a Rust edition without `#![no_std]`/`#![no_core]` starts
//! through lang start") refuses the first test with "the shelf has no `#[lang = \"start\"]`
//! item: hello (/hello/src/main.rs)" (RETURNS.md 18:58: 1-empty, x1-waker-01, ... regressed
//! so); a rule that ignored `#![no_std]` fails the second; one that never asked the shim
//! fails the third.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (the second and third), as
//! tests/rust_start.rs. Run: `LANECHECK_WAIT=540 bash
//! /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh blaze-ladder test '--test front2_start_rule'`
//! prints "test blaze-ladder: exit 0; 3 passed, 0 failed".

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::front2::start::{Entry2, linked_lang_start};
use blaze_ladder::refusal::SourceFile;
use blaze_scope::names::{self, Written};
use blaze_scope::resolve::Names;
use blaze_surface::{Namespace, PathKind};

/// The toolchain: the installed shelf, or none (a program of its own crate only).
fn toolchain(shelf: bool) -> Toolchain {
    if !shelf {
        return Toolchain::none(blaze_own::Alloc::global());
    }
    let home = std::env::var("HOME").expect("HOME is set");
    let place = format!("{home}/.blaze/v2/toolchain");
    Toolchain::open(&mut DirShelf::new(place.clone())).unwrap_or_else(|e| panic!("the toolchain at {place}: {e}"))
}

/// The crate root's `fn main`.
fn main_of(names: &Names<'_>) -> blaze_id::ItemKey {
    let written = Written { kind: PathKind::Plain, segments: &["main"] };
    let found = names::path(names, names.root(0), Namespace::Values, written, blaze_own::testing::heap().alloc()).ok();
    found.and_then(|f| f.whole(1)).expect("the crate root's `fn main` resolves").key
}

/// A core-only Rust-edition program (no shelf: its graph is its own crate, as every ladder rung
/// that runs without one): no `#[lang = "start"]` is linked, so it is NOT refused and starts
/// at its `fn main` directly. Before ladder-L1a-2 this program was refused by name.
#[test]
fn a_rust_program_without_std_starts_at_main() {
    let text = "fn main() {}\n";
    let tc = toolchain(false);
    let files = [("/hello/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "hello", dir: "/hello/", root: "/hello/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names: Names<'_> = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let linked = linked_lang_start(&names, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the start rule: {e}"));
    assert_eq!(linked, None, "a graph with no std links no `#[lang = \"start\"]`");
    let main = main_of(&names);
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let program = Front2Program::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("a core-only Rust program is refused: {e}"));
    assert_eq!(program.start(), Some(Entry2::Direct(main)), "a program without std starts at its `fn main`");
}

/// `#![no_std]` on the shelf: std is in the graph but not linked (the Reference's extern
/// prelude rule), so no lang start: the direct call.
#[test]
fn a_no_std_root_on_the_shelf_starts_at_main() {
    let text = "#![no_std]\nfn main() {}\n";
    let tc = toolchain(true);
    let files = [("/hello/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "hello", dir: "/hello/", root: "/hello/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names: Names<'_> = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let linked = linked_lang_start(&names, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the start rule: {e}"));
    assert_eq!(linked, None, "a `#![no_std]` root links core only: no `#[lang = \"start\"]`");
}

/// The same `fn main() {}` with std linked (the shelf, no `#![no_std]`): through std's
/// `rt::lang_start`.
#[test]
fn a_std_program_starts_through_lang_start() {
    let text = "fn main() {}\n";
    let tc = toolchain(true);
    let files = [("/hello/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "hello", dir: "/hello/", root: "/hello/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names: Names<'_> = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let linked = linked_lang_start(&names, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the start rule: {e}"));
    let lib = linked.expect("a program with std links `#[lang = \"start\"]`");
    let name = names.item_text(lib).and_then(|t| t.name).expect("lang_start has a name");
    let krate = names.crate_of(lib).expect("lang_start is in a crate");
    let module = names.module_of(lib).and_then(|m| names.krate(krate).path_of(m)).expect("lang_start's module");
    let path = format!("{module}::{name}");
    assert!(path.ends_with("rt::lang_start"), "the `#[lang = \"start\"]` item is `{path}`, not std's `rt::lang_start`");
}
