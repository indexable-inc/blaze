//! A HEAD A KEPT ITEM HAS IS READ BY ITS ENTRY'S `Signature` (lane KI, 2026-11-17). tempo's
//! `blaze check` (port27/tempo-deps/check-g043851.out) left 132 bodies unchecked, each "a head
//! it reads was written by a macro of a workspace crate at no entry a `Signature` covers": every
//! such head is an ADT written `#[derive(..)] #[helper(..)] struct S { .. }` (serde's
//! `#[serde(..)]`, thiserror's `#[error(..)]`). A helper written on the item after a derive
//! sends the item as after-derive calls, and when none takes it the item is kept by its keep
//! call (blaze-scope resolve.rs `keep_item`), so its head is a slice of that call's text. That
//! text kept no origins, so body_types/covered.rs `callers` could not reach the file entry the
//! call is at, and no `Signature` covered the head. Now the kept text keeps an identity column
//! whose call part names the struct's entry (blaze-scope expansion_origins.rs `kept`).
//!
//! The derive here is a built-in one the crate declares with a helper, as the pinned library
//! declares `Default` (core/src/default.rs `#[rustc_builtin_macro(Default,
//! attributes(default))]`): the fixture shelf runs no proc macro, and its `core` holds no
//! derive. `Copy` writes `impl $crate::marker::Copy for S {}`, `$crate` the declaring crate.
//!
//! The rule the expected error rests on (the Rust Reference, "Struct expressions", "Type
//! coercions"): a field of a struct expression is a coercion site to the field's type, and no
//! coercion turns `u8` into `u16`: `S { a: 1u8 }` against `a: u16` is one type error.
//!
//! A child of signature_grain_tests.rs (main.rs's module list is not this lane's): its helpers
//! are main.rs's tests'.

use super::super::*;
use super::{made_pair, unnamed};

/// The crate root: the two modules and the derive `Copy` declaring the helper `show`, as
/// serde's `Serialize` declares `serde`.
const ROOT: &str = "#![feature(decl_macro, rustc_attrs)]
mod a;
mod b;
pub mod marker {
    #[rustc_builtin_macro(Copy, attributes(show))]
    pub macro Copy($item:item) {}
    pub trait Copy {}
}
";

/// `a.rs`: a fn, then the kept struct `S` (a derive, then its helper on the item).
fn kept_s(other_body: &str, field: &str) -> std::string::String {
    std::format!("pub fn other() -> u8 {other_body}\n#[derive(crate::marker::Copy)]\n#[show(rename = \"s\")]\npub struct S {{\n    pub a: {field},\n}}\n")
}

/// `b.rs`: the reader of `S`'s head, a struct expression with a `u8` by its suffix.
const READER: &str = "pub fn g() {\n    let _s = crate::a::S { a: 1u8 };\n}\n";

/// One check: its errors, its words, what it made (`Signature`, `BodyCheck`), and its findings
/// and profile for a failing assertion.
struct Seen {
    errors: u32,
    words: std::string::String,
    made: (u32, u32),
    all: std::string::String,
}

/// The workspace checked once a state of `a.rs`, in turn on one store; each check's [`Seen`],
/// printed before any assertion.
fn checks_of(seed: u64, states: &[&str]) -> std::vec::Vec<Seen> {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", ROOT);
    write(&io, "src/b.rs", READER);
    let mut seen = std::vec::Vec::new();
    for (i, a) in states.iter().enumerate() {
        write(&io, "src/a.rs", a);
        let got = check(&io);
        let made_counts = made_pair(&got);
        std::println!("check {i}: failures {}; errors {}; made (Heads, Bodies) {made_counts:?}; words {:?}", got.failures, got.errors, got.words);
        assert_eq!(got.failures, 0, "check {i}: {}", got.profile);
        let all = std::format!("{:?}\n{:?}\n{}", got.ids, got.words, got.profile);
        seen.push(Seen { errors: got.errors, words: std::format!("{:?}", got.words), made: made_counts, all });
    }
    seen
}

/// THE KEPT ITEM'S HEAD IS COVERED, AND ITS FIELD EDIT REACHES ITS READER. `S` in `a.rs` is
/// `#[derive(Copy)] #[show(..)]`, its reader `g` in `b.rs`:
/// - the first check is clean, with no note of a head no `Signature` covers (`g` is CHECKED);
/// - `S`'s field made `u16` reports `g`'s `types` error, and the undo clears it: `(0, 1, 0)`.
/// CONTROL: with the kept text's origins `Rewritten` (blaze-scope resolve.rs `keep_item` before
/// KI), the first check filed "at no entry a `Signature` covers" for `g` and the edit moved no
/// read: `(0, 0, 0)`. Prints `kept item head: errors (0, 1, 0); field edit (Signature,
/// BodyCheck) made (S, B)`.
#[test]
fn a_kept_item_s_head_is_read_by_its_entry() {
    let seen = checks_of(91, &[&kept_s("{\n    1\n}", "u8"), &kept_s("{\n    1\n}", "u16"), &kept_s("{\n    1\n}", "u8")]);
    assert!(!unnamed(&seen[0].words), "a kept item's head was read at no entry: {}", seen[0].all);
    assert_eq!((seen[0].errors, seen[1].errors, seen[2].errors), (0, 1, 0), "g in b.rs must see S's new field type:\n{}", seen[1].all);
    assert!(seen[1].all.contains("\"types\""), "the error is the reader's type error:\n{}", seen[1].all);
    assert!(seen[1].made.1 >= 1, "the field edit checks its reader again:\n{}", seen[1].all);
    std::println!("kept item head: errors ({}, {}, {}); field edit (Signature, BodyCheck) made {:?}", seen[0].errors, seen[1].errors, seen[2].errors, seen[1].made);
}

/// A BODY EDIT ELSEWHERE IN THE KEPT ITEM'S FILE does not check its reader again: `other`'s body
/// (written BEFORE `S`, so `S`'s file offset moves) grows a statement. Its own body is checked
/// (1) and no `Signature` is made (a body is no head; the keep call's origins are item-relative,
/// rule 71): `(0, 1)`, and `g` stands. Prints `body edit beside a kept item: made (0, 1)`.
#[test]
fn a_body_edit_beside_a_kept_item_leaves_its_reader() {
    let seen = checks_of(92, &[&kept_s("{\n    1\n}", "u8"), &kept_s("{\n    let _y = 2u8;\n    1\n}", "u8")]);
    assert!(!unnamed(&seen[0].words), "a kept item's head was read at no entry: {}", seen[0].all);
    assert_eq!((seen[0].errors, seen[1].errors), (0, 0), "{}", seen[1].all);
    assert!(!stale_words(&seen[1].words), "a body edit refused its file: {}", seen[1].all);
    assert_eq!(seen[1].made, (0, 2), "a body edit beside a kept item makes no head and that one body, in its 2 units' rows (D191: the library's and its test build's):\n{}", seen[1].all);
    std::println!("body edit beside a kept item: made {:?}", seen[1].made);
}

/// Whether a check's words hold a refusal of a text the records saw changed under them
/// (blaze-scope items_record.rs `Stale`).
fn stale_words(words: &str) -> bool {
    words.contains("Stale {")
}
