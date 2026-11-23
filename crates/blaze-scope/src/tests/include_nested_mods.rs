//! The current file of a call is the file its text is written in, down a chain of `include!`s
//! (wiring icu-include-macros-2: `CallSrc::written`, set by the lowering from
//! `Fragment::written`), for a `mod x;` and for an `include!` written in an inline module of an
//! included file.
//!
//! The rules. core `macros/mod.rs`, `include!`: "The file is located relative to the current
//! file (similarly to how modules are found)". The Reference, "Modules", "Module Source
//! Filenames": a module without a body is loaded from a file found from the current file's
//! folder. An included file is the current file of what it writes (`crate::included`).
//!
//! Each case plants a decoy where the TOPMOST file's folder (`src/`) would find the file, so a
//! read from the wrong folder fails on "Unresolved" (the decoy has no `Leaf` or `Y`), not only on
//! a missing file.

use std::format;
use std::vec::Vec;

use blaze_surface::{Namespace, PathKind};

use super::entry::{Texts, input};
use crate::graph::Sysroot;
use crate::names::Written;
use crate::resolve::{Names, resolve_crates};
use crate::task::Serial;

const ROOT: &str = "#![no_core]
#[rustc_builtin_macro]
macro_rules! include { () => {} }
include!(\"../data/mod.rs\");
pub use inner_leaf::Leaf;
pub use y::Y;
";

static TEXTS: Texts = Texts(&[&[
    ("/d/src/lib.rs", ROOT),
    // `data/mod.rs`: a nested include (whose `mod` is found from `data/inner/`), and an
    // inline module holding an include (read relative to `data/mod.rs`).
    ("/d/data/mod.rs", "include!(\"inner/a.rs\");\npub mod y {\n    include!(\"y.rs\");\n}\n"),
    ("/d/data/inner/a.rs", "#[path = \"leaf.rs\"]\npub mod inner_leaf;\n"),
    ("/d/data/inner/leaf.rs", "pub struct Leaf;\n"),
    ("/d/data/y.rs", "pub struct Y;\n"),
    // The plants: what the topmost file's folder (`src/`) or `data/` alone would give.
    ("/d/src/leaf.rs", "pub struct Decoy;\n"),
    ("/d/data/leaf.rs", "pub struct Decoy;\n"),
    ("/d/src/y.rs", "pub struct Decoy;\n"),
]]);

fn solved() -> Names<'static> {
    let mut d = input("d", "/d/src/lib.rs", &[]);
    d.shown_from = "/d/";
    resolve_crates(&[d], Sysroot::default(), &TEXTS, &Serial, super::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"))
}

fn binds(names: &Names<'static>, name: &'static str) -> bool {
    let segments: &[&str] = &[name];
    names.item_at(names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments }, blaze_own::testing::heap().alloc()).is_ok()
}

fn errors(names: &Names<'static>) -> Vec<std::string::String> {
    names.krate(0).done.values().flat_map(|done| done.scope.errors.iter().map(|e| format!("{e:?}"))).collect()
}

/// (a) `mod x;` (here with `#[path]`) in a file included by an included file is found from
/// THAT file's folder, `data/inner/`. Before icu-include-macros-1 the nested include itself
/// missed (`data/inner/a.rs` was asked for at `src/inner/a.rs`); passes after -1 and after -2.
///
/// Passes as `tests::include_nested_mods::a_mod_in_a_nested_included_file_is_found_from_its_folder ... ok`.
#[test]
fn a_mod_in_a_nested_included_file_is_found_from_its_folder() {
    let names = solved();
    let notes = format!("{:?}", names.notes(0));
    assert!(binds(&names, "Leaf"), "the root binds `Leaf` from data/inner/leaf.rs; notes: {notes}");
    assert!(!notes.contains("FileNotFound"), "every module the included files write is found: {notes}");
}

/// (b) An `include!` in an inline module of an included file (`pub mod y { include!("y.rs"); }`
/// in `data/mod.rs`) reads `data/y.rs`. With -1 alone this FAILS: the calls of a module made from
/// an expansion are lowered in `make_modules`, which -1's `Run::written_in` map never saw, so the
/// host took `src/lib.rs` and read the decoy `src/y.rs` ("the root binds `Y`" fails, an
/// `Unresolved` for `y::Y`). With -2 the made module's fragment carries `Made::written`.
///
/// Passes after -2 as `tests::include_nested_mods::an_include_in_an_inline_module_of_an_included_file_reads_beside_it ... ok`.
#[test]
fn an_include_in_an_inline_module_of_an_included_file_reads_beside_it() {
    let names = solved();
    let notes = format!("{:?}", names.notes(0));
    assert!(binds(&names, "Y"), "the root binds `Y` from data/y.rs; notes: {notes}");
    let errors = errors(&names);
    assert!(errors.is_empty(), "`pub use inner_leaf::Leaf` and `pub use y::Y` resolve: {errors:?}\nnotes: {notes}");
}
