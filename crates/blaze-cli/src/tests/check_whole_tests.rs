//! `blaze check` checks everything asked (the owner: a check that prints a pass for what it did
//! not check is worse than none). A child of main.rs's tests: its helpers are theirs.
//!
//! TEST CODE IS ASKED. `check` makes a library's plain unit and its test unit (names_records_
//! tests.rs). Both own the library's files by their module trees as written (blaze-drive
//! owners.rs), and each crate is that tree under its own cfg (the Reference, "Conditional
//! compilation": an item whose predicate is false is not compiled; `attributes.testing.test.
//! enabled`: a `#[test]` fn only in test mode). So `#[cfg(test)] mod tests { .. }` and
//! `#[cfg(test)] mod t;` are the test unit's to check and not the library's: the library's
//! owner has nothing there, and must say nothing there. Before the fix it said "front2 lists no
//! body for it" for each fn of an inline test module and "the owner's items record no longer
//! lists this file" for each item of a test-only file, and the check closed incomplete.

use super::*;

/// A library whose test code holds one type error in each place test code is written: an inline
/// `#[cfg(test)]` module, a `#[cfg(test)]` module of its own file, a top-level `#[test]` fn.
/// The Reference, "Literal expressions": `true` is a `bool`; "Let statements": the initializer
/// is checked against the written type, so `let _x: u8 = true;` is one type error (E0308).
const LIB: &str = "pub fn lib_fn() -> u8 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn inline() {\n        let _x: u8 = true;\n    }\n}\n\n#[cfg(test)]\nmod in_file;\n\n#[test]\nfn top() {\n    let _y: u8 = true;\n}\n";
const IN_FILE: &str = "#[test]\nfn in_file() {\n    let _z: u8 = true;\n}\n";

/// The library's test code is checked, by its test unit, and nothing of it is a note of the
/// plain library's: three type errors (one a test fn), no "front2 lists no body", no "no longer
/// lists this file", no item left unchecked. CONTROL (run 2026-11-17 05:01, with the library's
/// owner checking cfg-off entries and noting an unlisted file again): the same 3 errors, then
/// "not checked because front2 lists no body for it: `tests::inline`, `top`" and "its owner's
/// front end over records could not be made: `in_file`", "the check is incomplete: blaze could
/// not check 3 items" (tempo's check of 04:38 had 163 and 24 of these).
/// Prints `test code: errors 3, notes 0`.
#[test]
fn test_code_is_checked_by_its_test_unit_alone() {
    let io = Io::simulated(SimWorld::new(61, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", LIB);
    write(&io, "src/in_file.rs", IN_FILE);
    let seen = check(&io);
    let out = stdout(&io);
    std::println!("failures {}; errors {}; words {:?}", seen.failures, seen.errors, seen.words);
    assert_eq!(seen.failures, 0, "{out}\n{}", seen.profile);
    assert!(!out.contains("front2 lists no body"), "the library's owner named a test fn it does not hold:\n{out}");
    assert!(!out.contains("no longer lists this file"), "the library's owner named a test-only file it does not hold:\n{out}");
    assert!(!out.contains("could not check"), "an item was left unchecked:\n{out}");
    assert_eq!(seen.errors, 3, "each test fn's type error is reported, once:\n{out}");
    let notes = seen.ids.iter().filter(|(code, _)| code.contains("not-checked")).count();
    std::println!("test code: errors {}, notes {notes}", seen.errors);
}

/// INITIALIZERS ARE BODIES. The Reference, "Constant items": "The constant expression may only
/// be omitted in a trait definition"; "Static items": a static has an initializer of its type;
/// "Associated constants": an impl's and a trait's `const` with a value. Each initializer is an
/// expression checked against the declared type (a coercion site, "Type coercions"), so
/// `= true` where `u8` is declared is one type error (E0308), at the initializer. One line each:
/// a free `const`, a `static`, an impl's `const`, a trait's provided `const`; and one of each
/// that is well typed, which says nothing.
const VALUES: &str = "pub const C: u8 = true;\npub static S: u8 = true;\npub struct T;\nimpl T {\n    pub const A: u8 = true;\n    pub const A_OK: u8 = 1;\n}\npub trait Tr {\n    const B: u8 = true;\n    const NONE: u8;\n}\npub const C_OK: u8 = 2;\npub static S_OK: u8 = 3;\n";

/// Each wrong initializer is an error at its own line, every initializer is checked (no
/// "a `const` or `static` body" note, nothing left unchecked), and the well-typed ones and the
/// trait's `const` with no value say nothing. CONTROL (run 2026-11-17 05:09, with fn entries
/// alone given a root and the old note back): "errors 0", the seven initializers "not checked
/// because a `const` or `static` body: front2 checks fn bodies only", "the check is incomplete:
/// blaze could not check 7 items"; the edit test fails at its cold check the same way.
/// Prints `initializers: errors 4 at [1, 2, 5, 9]`.
#[test]
fn a_wrong_typed_initializer_is_an_error_at_its_item() {
    let io = Io::simulated(SimWorld::new(62, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", VALUES);
    let seen = check(&io);
    let out = stdout(&io);
    std::println!("failures {}; errors {}; words {:?}", seen.failures, seen.errors, seen.words);
    assert_eq!(seen.failures, 0, "{out}\n{}", seen.profile);
    assert!(!out.contains("front2 checks fn bodies only"), "an initializer was left to a note:\n{out}");
    assert!(!out.contains("could not check"), "an item was left unchecked:\n{out}");
    let lines: std::vec::Vec<u32> =
        out.lines().filter(|l| l.contains("error[types]")).filter_map(|l| l.strip_prefix("src/lib.rs:")?.split(':').next()?.parse().ok()).collect();
    std::println!("initializers: errors {} at {lines:?}", seen.errors);
    assert_eq!(lines, [1, 2, 5, 9], "one error at each wrong initializer's line, none at the others:\n{out}");
    assert_eq!(seen.errors, 4, "{out}");
}

/// A BLOCK'S `use` A MACRO WROTE. The Reference, "Use declarations": a `use` may be in a block;
/// "Method call expressions": a method of a trait in scope there is found; "Macros By Example",
/// "Hygiene": `$crate` names the crate that defines the macro. `d::go!` writes `{ use
/// $crate::m::Tr as _; $e.go() }`, as tracing's `debug!` writes `use $crate::__macro_support::
/// Callsite as _;` (60 tempo items in check-g045852 were "a use declaration in a block whose path
/// the names library does not resolve"). So `a`'s `f` is checked, and its one type error found.
/// CONTROL (run 2026-11-17 05:22, block_traits.rs reading the path with no mark, as before):
/// "errors 0", "not checked because a question the front end cannot answer yet: `f`", its first
/// `LocalGap` "a use declaration in a block whose path the names library does not resolve", as
/// in tempo. Prints `block use: errors 1`.
#[test]
fn a_block_use_a_macro_wrote_names_the_macros_crate() {
    let io = Io::simulated(SimWorld::new(64, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"d\"]\nresolver = \"2\"\n");
    write(&io, "d/Cargo.toml", "[package]\nname = \"d\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(
        &io,
        "d/src/lib.rs",
        "pub mod m {\n    pub trait Tr {\n        fn go(&self) -> u8 {\n            1\n        }\n    }\n    impl Tr for u8 {}\n}\n\n#[macro_export]\nmacro_rules! go {\n    ($e:expr) => {{\n        use $crate::m::Tr as _;\n        $e.go()\n    }};\n}\n",
    );
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nd = { path = \"../d\" }\n");
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    let _x: u8 = true;\n    d::go!(3u8)\n}\n");
    let seen = check(&io);
    let out = stdout(&io);
    std::println!("failures {}; errors {}; words {:?}", seen.failures, seen.errors, seen.words);
    assert_eq!(seen.failures, 0, "{out}\n{}", seen.profile);
    assert!(!out.contains("cannot answer yet"), "the block's `use` was not read:\n{out}");
    assert!(!out.contains("could not check"), "an item was left unchecked:\n{out}");
    std::println!("block use: errors {}", seen.errors);
    assert_eq!(seen.errors, 1, "`f`'s one type error, and nothing of the macro's block:\n{out}");
}

/// An initializer is its own `BodyCheck`, keyed by its item's text like a fn's: an edit of one
/// initializer's value checks that one body again, and no other (not the fn, not the other
/// initializer). The Reference, "Constant evaluation": a constant's value is read where it is
/// used; nothing here uses `C`, so nothing else reads it. Prints `initializer edit: BodyCheck
/// made <cold> then <edit>`, expected 3 then 1.
#[test]
fn an_initializer_edit_rechecks_that_initializer_alone() {
    let io = Io::simulated(SimWorld::new(63, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", "pub fn f() -> u8 {\n    1\n}\npub const C: u8 = 1;\npub static S: u8 = 3;\n");
    let cold = check(&io);
    let out = stdout(&io);
    assert_eq!(cold.failures, 0, "{out}\n{}", cold.profile);
    assert!(!out.contains("could not check"), "cold, every body is checked:\n{out}");
    write(&io, "src/lib.rs", "pub fn f() -> u8 {\n    1\n}\npub const C: u8 = 2;\npub static S: u8 = 3;\n");
    let edit = check(&io);
    let out = stdout(&io);
    assert_eq!(edit.failures, 0, "{out}\n{}", edit.profile);
    // The engine's `Bodies` entries (the old checker's `BodyCheck` made rows until 2026-11-19).
    let (before, after) = (cold.entered(blaze_engine::Stage::Bodies), edit.entered(blaze_engine::Stage::Bodies));
    std::println!("initializer edit: Bodies entered {before} then {after}");
    // D191: every unit that holds a body checks it, a row each; the library and its test build
    // hold all three, so a body is 2 rows.
    assert_eq!((before, after), (6, 2), "cold: f, C and S; after C's value moved: C alone; each body in its 2 units' rows (D191)\n{}\n{out}", edit.profile);
}
