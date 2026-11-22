//! BODIES A MACRO WROTE AT MODULE LEVEL ARE CHECKED (lane I, 2026-11-17; D122: a macro-written
//! item's place is its expansion's). A module-level macro call, and an item a derive or an
//! attribute takes, is an entry with a `BodyCheck` part of its own (work.rs `expansion_entry`)
//! whose check covers every body the expansions at it write (body_types/covered.rs
//! `written_by`, `Root::Expanded`), its findings at the entry (the call), the expansion's item
//! named in the words. Before, such a body had no outline entry, so no part named it and no
//! check covered it: `blaze check` never type-checked it (the owner's rule: "check is a whole
//! check"), and `blaze run` refused it by name (hello world's `bstd_hosted_start`, which
//! `bstd::main`'s expansion writes).
//!
//! The rule the expected errors rest on (the Rust Reference, "Function body", "Type
//! coercions"): a body's tail expression is coerced to its return type, and no coercion site
//! turns `u8` into `u16`: `fn f(x: u8) -> u16 { x }` is one type error.
//!
//! A child of signature_grain_tests.rs (its module list is main.rs's, which this lane does not
//! edit): its helpers are main.rs's tests'.

use super::super::*;

/// A `macro_rules!` at module level writing a free fn, called once, as written and with the
/// written body's tail made the wrong type.
const WRITES_F: &str = "macro_rules! write_f {\n    () => {\n        pub fn f(x: u8) -> u8 { x }\n    };\n}\nwrite_f!();\n";
const WRITES_BAD_F: &str = "macro_rules! write_f {\n    () => {\n        pub fn f(x: u8) -> u16 { x }\n    };\n}\nwrite_f!();\n";

/// A `macro_rules!` writing an impl of a struct the file declares: its member fn is a body of a
/// container a macro wrote, as a derive's impl fns are (the fixture shelf's std holds no
/// `Clone`, so a builtin derive cannot be expanded in the simulated home).
const WRITES_GET: &str = "pub struct S(pub u8);\nmacro_rules! getter {\n    () => {\n        impl S {\n            pub fn get(&self) -> u8 { self.0 }\n        }\n    };\n}\ngetter!();\n";
const WRITES_BAD_GET: &str = "pub struct S(pub u8);\nmacro_rules! getter {\n    () => {\n        impl S {\n            pub fn get(&self) -> u16 { self.0 }\n        }\n    };\n}\ngetter!();\n";

/// The errors of three checks of a one-module workspace (`src/a.rs` as `good`, then `bad`,
/// then `good`), and the second check's findings on one line: what a failing assertion quotes.
fn three_checks(seed: u64, good: &str, bad: &str) -> ((u32, u32, u32), std::string::String) {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", "mod a;\n");
    write(&io, "src/a.rs", good);
    let first = check(&io);
    assert_eq!(first.failures, 0, "{}", first.profile);
    assert!(first.words.iter().all(|w| !w.contains("not checked") && !w.contains("lists no body")), "the clean workspace notes a body unchecked: {:?}", first.words);
    write(&io, "src/a.rs", bad);
    let edited = check(&io);
    assert_eq!(edited.failures, 0, "{}", edited.profile);
    let after = std::format!("{:?} {:?}", edited.ids, edited.words);
    write(&io, "src/a.rs", good);
    let undone = check(&io);
    assert_eq!(undone.failures, 0, "{}", undone.profile);
    ((first.errors, edited.errors, undone.errors), after)
}

/// `write_f!();` at module level writes `pub fn f(x: u8) -> u8 { x }`: the first check is
/// clean; the macro's body made `-> u16` is ONE `types` error, filed by the call's own
/// `BodyCheck` at the call, its words naming the expansion's item
/// (`{the expansion of `write_f!`}::f`); the undo clears it: `(0, 1, 0)`. CONTROL: with no
/// `BodyCheck` part at the call (work.rs `expansion_entry` false), nothing checks `f` and the
/// counts are `(0, 0, 0)`. Prints
/// `macro-written fn: errors (0, 1, 0), at the call, named {the expansion of `write_f!`}::f`.
#[test]
fn a_fn_a_module_level_macro_wrote_is_checked_at_its_call() {
    let (errors, after) = three_checks(61, WRITES_F, WRITES_BAD_F);
    assert_eq!(errors, (0, 1, 0), "the fn the macro wrote must be type checked:\n{after}");
    assert!(after.contains("\"types\""), "the error is the body's type error:\n{after}");
    assert!(after.contains("{the expansion of `write_f!`}::f"), "the finding names the expansion's item:\n{after}");
    std::println!("macro-written fn: errors {errors:?}, at the call, named {{the expansion of `write_f!`}}::f");
}

/// `getter!();` writes `impl S { pub fn get(&self) -> u8 { self.0 } }`: the member fn of an
/// impl a macro wrote is covered by the call's check as a derive's impl fns are: `(0, 1, 0)`,
/// named `{the expansion of `getter!`}::get`. CONTROL as above: `(0, 0, 0)`. Prints
/// `macro-written impl fn: errors (0, 1, 0)`.
#[test]
fn an_impl_fn_a_macro_wrote_is_checked_at_its_call() {
    let (errors, after) = three_checks(62, WRITES_GET, WRITES_BAD_GET);
    assert_eq!(errors, (0, 1, 0), "the impl fn the macro wrote must be type checked:\n{after}");
    assert!(after.contains("\"types\""), "the error is the body's type error:\n{after}");
    assert!(after.contains("{the expansion of `getter!`}::get"), "the finding names the expansion's item:\n{after}");
    std::println!("macro-written impl fn: errors {errors:?}");
}

/// The macro defined in `lib.rs`, called in `a.rs` (textual scope: a `macro_rules!` is in
/// scope in a module declared after it). An edit of the macro's BODY in `lib.rs` leaves the
/// call's text (its `BodyCheck`'s key), its file's `Surface` and the crate's exports (its
/// `CrateScope` out) equal, and the head it writes too (the call entry's `Signature`); the
/// call's check reruns because it reads `lib.rs`'s `Surface`, which holds the macro's tokens
/// (body_types.rs `rules_reads`): `(0, 1, 0)`. CONTROL: without that read the call's record
/// stands and the counts are `(0, 0, 0)`. Prints
/// `macro edited in another file: errors (0, 1, 0)`.
#[test]
fn an_edit_of_the_macro_in_another_file_reruns_its_calls_check() {
    let good = "macro_rules! write_f {\n    () => {\n        pub fn f(x: u8) -> u8 { x }\n    };\n}\nmod a;\n";
    let bad = "macro_rules! write_f {\n    () => {\n        pub fn f(x: u8) -> u8 { x as u16 }\n    };\n}\nmod a;\n";
    let io = Io::simulated(SimWorld::new(63, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", good);
    write(&io, "src/a.rs", "write_f!();\n");
    let first = check(&io);
    assert_eq!(first.failures, 0, "{}", first.profile);
    write(&io, "src/lib.rs", bad);
    let edited = check(&io);
    assert_eq!(edited.failures, 0, "{}", edited.profile);
    let after = std::format!("{:?} {:?}\n{}", edited.ids, edited.words, edited.profile);
    write(&io, "src/lib.rs", good);
    let undone = check(&io);
    assert_eq!(undone.failures, 0, "{}", undone.profile);
    let errors = (first.errors, edited.errors, undone.errors);
    assert_eq!(errors, (0, 1, 0), "the macro's edit in lib.rs must reach the check of its call in a.rs:\n{after}");
    assert!(after.contains("{the expansion of `write_f!`}::f"), "the finding is the call's, naming the expansion's item:\n{after}");
    std::println!("macro edited in another file: errors {errors:?}");
}
