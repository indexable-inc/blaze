//! How a program crate that exports no entry starts is ONE rule on both fronts (wiring
//! solved-root-start-1.md; front2/program/run_entry.rs `RootStart`, `RootStart::unexported`).
//!
//! D104 (decision row; front2/program/run_entry.rs's module doc): a blaze-edition binary's root
//! `fn main` is the input of `bstd::main`, whose expansion exports bstd's hosted entry
//! (`bstd_hosted_start`); so a blaze-edition program's entry is always an exported fn, and one
//! with none is refused by name, never started at its `main` directly (that would skip the
//! grants `bstd::main` makes). The record walk refused it already (`EntryRefusal::NoHostedStart`);
//! the SOLVED front (program.rs `make_from`) fell to `linked_lang_start`, which answers `None`
//! for a bstd root, and so to `Entry2::Direct(main)`: a quiet other route (rule 3).
//!
//! A Rust-edition root keeps Rust's rule (the pinned std, library/std/src/rt.rs: `#[lang =
//! "start"] fn lang_start<T: crate::process::Termination + 'static>(..)`, found in the crates
//! linked into the binary): with std linked, through `rt::lang_start`; with none linked (a
//! program run with no shelf), its `fn main` directly.
//!
//! The blaze-edition root here is resolved with `Implicit::entry` clear: the names library makes
//! no `bstd::main` expansion, which is the "main expansion missing" case (a bstd root whose
//! expansion exported nothing). No shelf: the root links no crate.
//!
//! CONTROL (rule 8): with make_from's `None =>` arm back to the old one (straight to
//! `linked_lang_start`, else `Entry2::Direct(rust_main)`), the first test fails with
//! "SOLVED ROOT START: started as Some(Direct(..)) (the quiet Direct route)".
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (the third test), as
//! tests/front2_start_rule.rs. Run: `LANECHECK_WAIT=540 bash
//! /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh blaze-ladder test '--test front2_solved_root_start'`
//! prints "SOLVED ROOT START: front2: the program's start: a blaze-edition program exports no
//! `bstd_hosted_start` (D104: its root's `fn main` is expanded by `bstd::main`, which exports it)"
//! and "test blaze-ladder: exit 0; 3 passed, 0 failed".

#![feature(allocator_api)]

use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::program::run_entry::{RootStart, Unhosted};
use blaze_ladder::front2::program::{Front2Program, Front2Refusal};
use blaze_ladder::front2::start::{Entry2, linked_lang_start};
use blaze_ladder::refusal::SourceFile;
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude, StdIs};
use edition::BlazeEdition;

const ROOT: &str = "pub fn main() {}\n";

struct Texts;

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (0, "/app/src/main.rs") => Some(ROOT),
            _ => None,
        }
    }
}

/// A blaze-edition program crate whose `fn main` was given no `bstd::main` expansion.
const BLAZE_NO_EXPANSION: Implicit = Implicit { std: StdIs::Bstd(BlazeEdition::Blaze2026_11), prelude: Prelude::Blaze(BlazeEdition::Blaze2026_11), proc_macro: false, entry: false };

/// One crate, `app`, its root [`ROOT`], resolved over no toolchain crate.
fn names_of(implicit: Implicit) -> &'static Names<'static> {
    let crates = [CrateInput {
        name: "app",
        key: CrateItemKey(ContentId::of("blaze-ladder solved root start test crate", b"app")),
        edition: Edition::E2024,
        implicit,
        // The package's directory: the root is shown as `src/main.rs` (blaze-scope resolve.rs
        // `shown`), the path the program's `SourceFile` gives (front2 texts.rs `Texts::new`).
        root: "/app/src/main.rs",
        shown_from: "/app/",
        features: &[],
        deps: &[],
        cfg: &[],
        script: None,
    }];
    let names = resolve_crates(&crates, Sysroot::default(), &Texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    Box::leak(Box::new(names))
}

/// The blaze-edition root exporting no hosted entry: refused by name on the SOLVED front, the
/// same refusal as the record walk's, never `Entry2::Direct`.
#[test]
fn a_blaze_root_with_no_hosted_entry_is_refused_on_the_solved_front() {
    let names = names_of(BLAZE_NO_EXPANSION);
    let start = RootStart::of_names(names).expect("the names hold crate 0");
    assert_eq!(start, RootStart::Hosted, "a blaze-edition root (bstd, neither `#![no_core]` nor `#![no_std]`) starts hosted");
    assert!(start.unexported().is_err(), "a hosted root that exports nothing has no other start");
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let sources = [SourceFile { path: "src/main.rs", text: ROOT }];
    match Front2Program::new(names, frame, &sources, blaze_own::Alloc::global()) {
        Err(e @ Front2Refusal::NoHostedStart) => {
            let said = format!("{e}");
            println!("SOLVED ROOT START: {said}");
            assert!(said.contains("exports no `bstd_hosted_start`"), "{said}");
        }
        Err(other) => panic!("SOLVED ROOT START: refused, but not as a missing hosted start: {other}"),
        Ok(program) => panic!("SOLVED ROOT START: started as {:?} (the quiet Direct route)", program.start()),
    }
}

/// A Rust-edition root with no std linked (no shelf): `RootStart` says `LangStart`, the solved
/// front refines it by what the root links (none), and the program starts at `main` directly,
/// as before this change.
#[test]
fn a_rust_root_without_std_still_starts_at_main() {
    let rust = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2024), proc_macro: false, entry: false };
    let names = names_of(rust);
    let start = RootStart::of_names(names).expect("the names hold crate 0");
    assert_eq!(start, RootStart::LangStart);
    assert_eq!(start.unexported(), Ok(Unhosted::LangStart));
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let sources = [SourceFile { path: "src/main.rs", text: ROOT }];
    let program = Front2Program::new(names, frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("a std-less Rust program is refused: {e}"));
    assert!(matches!(program.start(), Some(Entry2::Direct(_))), "a program that links no std starts at its `fn main`: {:?}", program.start());
}

/// A Rust-edition `fn main() {}` on the shelf (std linked): `RootStart::LangStart`, not refused
/// as hosted, and the item the solved front then starts through is std's `rt::lang_start`.
#[test]
fn a_rust_root_with_std_starts_through_lang_start() {
    let text = "fn main() {}\n";
    let home = std::env::var("HOME").expect("HOME is set");
    let place = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(place.clone())).unwrap_or_else(|e| panic!("the toolchain at {place}: {e}"));
    let files = [("/hello/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "hello", dir: "/hello/", root: "/hello/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names: Names<'_> = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let start = RootStart::of_names(&names).expect("the names hold crate 0");
    assert_eq!(start, RootStart::LangStart, "an edition-2024 root with std");
    assert_eq!(start.unexported(), Ok(Unhosted::LangStart));
    let linked = linked_lang_start(&names, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the start rule: {e}"));
    let lib = linked.expect("a program with std links `#[lang = \"start\"]`");
    let name = names.item_text(lib).and_then(|t| t.name).expect("lang_start has a name");
    let krate = names.crate_of(lib).expect("lang_start is in a crate");
    let module = names.module_of(lib).and_then(|m| names.krate(krate).path_of(m)).expect("lang_start's module");
    let path = format!("{module}::{name}");
    assert!(path.ends_with("rt::lang_start"), "the `#[lang = \"start\"]` item is `{path}`, not std's `rt::lang_start`");
}
