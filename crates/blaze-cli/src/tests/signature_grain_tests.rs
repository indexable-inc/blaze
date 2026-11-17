//! A body's check reads the HEADS it uses, at signature grain (D114, drive.md "Check times: the
//! end state in numbers", rule 4 and piece (d)). A child of main.rs's tests: its helpers are
//! theirs.
//!
//! First, the stale check (correctness): a signature edited in ANOTHER file or crate that leaves
//! the crates' exports equal must still reach every body that calls it. Until a body's read list
//! names each head it used, a body's key held its own item's text and its file's text around
//! bodies, and its reads were the crates' `CrateScope` outs (exports digests): `f(x: u8)` made
//! `f(x: u16)` in `a.rs` moved none of them for a caller in `b.rs`, and the caller's old
//! "checked" record stood (body_types.rs's header, gap (a)).
//!
//! The rule the expected error rests on (the Rust Reference, "Call expressions": "the arguments
//! [..] are coerced to the parameter types"; "Type coercions": no coercion site turns `u8` into
//! `u16`, numeric widening is an `as` cast only): `f(1u8)` against `fn f(_x: u16)` is one type
//! error at the call.

use super::*;

/// The library root of a crate whose two modules are one file each: the callee's and the
/// caller's.
const TWO_FILES: &str = "mod a;\nmod b;\n";
/// The callee as first written, then with its one parameter's type edited.
const CALLEE_U8: &str = "pub fn f(_x: u8) {}\n";
const CALLEE_U16: &str = "pub fn f(_x: u16) {}\n";
/// The caller: a `u8` literal by its suffix, so no inference can make it a `u16`.
const CALLER: &str = "pub fn g() {\n    crate::a::f(1u8);\n}\n";

/// The errors of three checks of one workspace: as written, after the callee's edit, after the
/// edit is undone. Right: `(0, 1, 0)`.
struct ThreeChecks {
    errors: (u32, u32, u32),
    /// The edited check's findings, (code, id) and words: what a failing assertion quotes.
    after: std::string::String,
}

/// Checks the workspace as written, edits the file `callee` to `CALLEE_U16` and checks, puts
/// `CALLEE_U8` back and checks.
fn three_checks(io: &Io, callee: &str, sizes: work::Sizes) -> ThreeChecks {
    let first = check(io, sizes);
    assert!(first.failed.is_empty(), "{:?}", first.failed);
    write(io, callee, CALLEE_U16);
    let edited = check(io, sizes);
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    let after = std::format!("{:?}\n{:?}\n{}", edited.ids, edited.words, edited.profile);
    write(io, callee, CALLEE_U8);
    let undone = check(io, sizes);
    assert!(undone.failed.is_empty(), "{:?}", undone.failed);
    ThreeChecks { errors: (first.errors, edited.errors, undone.errors), after }
}

/// The two-file workspace, written into a fresh simulated home.
fn two_files() -> Io {
    let io = Io::simulated(SimWorld::new(21, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", TWO_FILES);
    write(&io, "src/a.rs", CALLEE_U8);
    write(&io, "src/b.rs", CALLER);
    io
}

/// Two members, `a` (the callee) and `b` (the caller, depending on `a` by path).
fn two_crates() -> Io {
    let io = Io::simulated(SimWorld::new(22, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "a/src/lib.rs", CALLEE_U8);
    write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    write(&io, "b/src/lib.rs", "pub fn g() {\n    a::f(1u8);\n}\n");
    io
}

/// THE STALE CHECK, across files. `fn f(_x: u8)` in `a.rs`, its caller `g` in `b.rs`; `f` made
/// to take `u16`: the second check reports the caller's `types` error, and the undo clears it.
/// Before the fix the counts are `(0, 0, 0)`: `b.rs`'s body key (its item's text and its file
/// around bodies) and its reads (the crate's `CrateScope`, whose out is the exports' digest,
/// which a parameter's type leaves equal) all stand. Prints
/// `stale check, two files: errors (0, 1, 0)`.
#[test]
fn a_signature_edit_in_another_file_reaches_its_caller() {
    let io = two_files();
    let seen = three_checks(&io, "src/a.rs", work::Sizes::USUAL);
    assert_eq!(seen.errors, (0, 1, 0), "the caller in b.rs must see f's new parameter type:\n{}", seen.after);
    assert!(seen.after.contains("\"types\""), "the error is the caller's type error:\n{}", seen.after);
    std::println!("stale check, two files: errors {:?}", seen.errors);
}

/// THE STALE CHECK, across crates: the same edit in member `a`, the caller in member `b`.
/// Before the fix `(0, 0, 0)`, for the same reason (`a`'s `CrateScope` out is its exports'
/// digest). Prints `stale check, two crates: errors (0, 1, 0)`.
#[test]
fn a_signature_edit_in_another_crate_reaches_its_caller() {
    let io = two_crates();
    let seen = three_checks(&io, "a/src/lib.rs", work::Sizes::USUAL);
    assert_eq!(seen.errors, (0, 1, 0), "the caller in member b must see a::f's new parameter type:\n{}", seen.after);
    assert!(seen.after.contains("\"types\""), "the error is the caller's type error:\n{}", seen.after);
    std::println!("stale check, two crates: errors {:?}", seen.errors);
}
