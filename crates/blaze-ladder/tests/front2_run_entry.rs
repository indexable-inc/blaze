//! D106 piece Z, item 3: the run's entry key from crate 0's items and the root's text, with no
//! body checked (front2/program/run_entry.rs).
//!
//! The rule tested (the Reference, crates-and-source-files.main: "A crate that contains a `main`
//! function can be compiled to an executable"; a `#![no_core]`/`#![no_std]` root starts at it
//! directly, start.rs `Entry2::Direct`; an exported `bstd_hosted_start` is bstd's hosted entry
//! and wins over `main`, program.rs THE ENTRY RULE; a blaze-edition root exports it through
//! D104's `bstd::main` expansion, so one without it is refused by name).
//!
//! The key is checked against the root's `main` found by PATH resolution (`names::path`, the
//! solve's own walk), not by the rows `run_entry` reads. Planted control: the second program
//! exports `start` as `bstd_hosted_start`: the entry must be `start`, not `main`; a rule that
//! ignored exports (always the root's `main`) fails `exported_wins`.
//!
//! Expected lines:
//!   test entry_key_is_the_roots_main ... ok
//!   test exported_wins ... ok
//!   test blaze_edition_without_export_is_refused ... ok

#![feature(allocator_api)]

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::program::run_entry::{EntryHow, EntryRefusal, RootStart, run_entry};
use blaze_ladder::refusal::SourceFile;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Edition, Namespace, PathKind};

const DIRECT: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

fn helper() -> i32 {
    1
}

fn main() {
    helper();
}
";

const EXPORTED: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

#[export_name = \"bstd_hosted_start\"]
fn start() {}

fn main() {}
";

/// `text`'s names, solved, given to `each` with the key of the root's `main` by path.
fn with_names(text: &'static str, each: impl FnOnce(&blaze_scope::resolve::Names<'_>, blaze_id::ItemKey)) {
    let files = [SourceFile { path: "src/main.rs", text }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let written = Written { kind: PathKind::Plain, segments: &["main"] };
    let found = names::path(&names, names.root(0), Namespace::Values, written, Alloc::global()).unwrap_or_else(|e| panic!("`main` by path: {e:?}"));
    let main = found.whole(1).unwrap_or_else(|| panic!("`main` resolves whole")).key;
    each(&names, main);
}

#[test]
fn entry_key_is_the_roots_main() {
    // The root's own attributes, read from its text: `#![no_core]` starts at `main` directly.
    let start = RootStart::of_root(blaze_unit_graph::StdIs::Rust, DIRECT, Edition::E2024, &[], Alloc::global());
    assert_eq!(start, RootStart::Direct, "a `#![no_core]` root starts at its `main`");
    with_names(DIRECT, |names, main| {
        let entry = run_entry(names, start).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(entry.how, EntryHow::Direct);
        assert_eq!(entry.lib, main, "the entry is the root's `main` by path");
        assert_eq!(entry.key, checker_of(main));
    });
}

#[test]
fn exported_wins() {
    with_names(EXPORTED, |names, main| {
        let entry = run_entry(names, RootStart::Direct).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(entry.how, EntryHow::Hosted, "an exported `bstd_hosted_start` is the entry");
        assert_ne!(entry.lib, main, "not the root's `main`");
        assert_eq!(names.item_text(entry.lib).and_then(|t| t.name), Some("start"));
    });
}

#[test]
fn blaze_edition_without_export_is_refused() {
    with_names(DIRECT, |names, _| match run_entry(names, RootStart::Hosted) {
        Err(EntryRefusal::NoHostedStart) => {}
        other => panic!("a blaze-edition root with no `bstd_hosted_start` is refused by name, got {other:?}"),
    });
    assert_eq!(RootStart::of(blaze_unit_graph::StdIs::Rust, false, false), RootStart::LangStart);
}

/// A Rust program with std is no longer refused (D124): its entry names the root's `main`,
/// run through `lang_start::<T>` (run/special.rs `roots` makes the shim and both instances).
#[test]
fn a_rust_start_names_its_main() {
    with_names(DIRECT, |names, main| {
        let entry = run_entry(names, RootStart::LangStart).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(entry.how, EntryHow::LangStart);
        assert_eq!(entry.lib, main, "the Rust start's `main` is the root's `main` by path");
    });
}
