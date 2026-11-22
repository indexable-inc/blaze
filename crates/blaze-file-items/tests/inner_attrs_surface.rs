//! names-attr-4 (AGENTS.md rule 21, keys-cover-every-input): a file's surface holds what name
//! resolution reads of the file's own inner attributes.
//!
//! The crate's names (the drive's `CrateScope`) are kept by a read list of its files' `Surface`s
//! (blaze-cli work.rs: `Outline::surface` is each file's `Surface` part). Every module's
//! lowering reads the crate root's `#![register_tool(..)]` (names-attr-3: `crate_tools`,
//! `LowerCx::tools`, which decides whether `#[x::y]` takes its item), and the root's lowering
//! reads `#![no_std]`, `#![no_core]`, `#![blaze::arbitrary_ints]`; a module file's own
//! `#![cfg(..)]`, `#![macro_use]` and `#![no_implicit_prelude]` are read too (blaze-scope
//! task/lower.rs `file_attrs`: every inner attribute but a doc comment). Before names-attr-4 the
//! surface hashed the file's entries only, so an edit to any of these left every key equal and
//! the crate's names stale.
//!
//! The grain is the crate's: the lowered module records are written and read back inside the
//! one `CrateScope` task, under its read list, so no module record has a key of its own. What
//! the design allows is per FILE: the root's surface moves, a module file's does not.

use blaze_file_items::{Outline, outline};
use blaze_surface::Edition;

fn parse(text: &str) -> Outline<'static> {
    outline(text, Edition::E2024, blaze_own::testing::heap().alloc())
}

/// Rule: an edit to an inner attribute the name solver reads moves the file's surface; an edit to
/// a doc comment or a body does not (the Reference, "Attributes": `attributes.tool`, a tool is
/// registered at the crate root; `#![no_std]` changes the crate's prelude).
///
/// Prints `inner attributes: 4 edits moved the root's surface, 3 kept it`. Control: without
/// names-attr-4's `file_names` part in `Outline::surface` the first `assert_ne` fails ("adding
/// `#![register_tool(x)]` left the surface equal").
#[test]
fn an_inner_attribute_the_solver_reads_moves_the_surface() {
    let root = "#![no_core]\n#[x::y]\npub fn uses() {}\nmod other;\n";
    let moves: [(&str, &str, &str); 4] = [
        ("adding `#![register_tool(x)]`", root, "#![no_core]\n#![register_tool(x)]\n#[x::y]\npub fn uses() {}\nmod other;\n"),
        (
            "renaming the registered tool",
            "#![register_tool(x)]\n#[x::y]\npub fn uses() {}\n",
            "#![register_tool(z)]\n#[x::y]\npub fn uses() {}\n",
        ),
        ("adding `#![no_std]`", "pub fn f() {}\n", "#![no_std]\npub fn f() {}\n"),
        ("a module file's `#![cfg(..)]`", "pub fn f() {}\n", "#![cfg(test)]\npub fn f() {}\n"),
    ];
    let keeps: [(&str, &str, &str); 3] = [
        ("a crate doc comment", "//! one\n#![register_tool(x)]\npub fn f() {}\n", "//! two\n#![register_tool(x)]\npub fn f() {}\n"),
        ("a body in a root that registers a tool", "#![register_tool(x)]\npub fn f() { a() }\n", "#![register_tool(x)]\npub fn f() { b() }\n"),
        ("blank lines between inner attributes", "#![no_std]\n#![register_tool(x)]\n", "#![no_std]\n\n#![register_tool(x)]\n"),
    ];
    for (what, old, new) in moves {
        assert_ne!(parse(old).surface(), parse(new).surface(), "{what} left the surface equal:\n{old}\nto\n{new}");
    }
    for (what, old, new) in keeps {
        assert_eq!(parse(old).surface(), parse(new).surface(), "{what} moved the surface:\n{old}\nto\n{new}");
    }
    // A module file's own inner attributes count the same; a file with none has one fixed part,
    // so two files that differ only in a doc comment of the file are equal.
    assert_eq!(parse("//! a\npub fn f() {}\n").surface(), parse("pub fn f() {}\n").surface(), "a file doc comment moved the surface");
    println!("inner attributes: {} edits moved the root's surface, {} kept it", moves.len(), keeps.len());
}
