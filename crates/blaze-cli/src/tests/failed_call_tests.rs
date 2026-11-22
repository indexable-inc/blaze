//! A failed macro call is an error `blaze check` prints, naming the call and the matcher's
//! reason, placed at its item (implicit-main-reason; names/failed_call.rs; rules 19, 71, 81).
//! The Reference, "Macros By Example", "Transcribing": "When a macro is invoked, the macro
//! expander looks up macro invocations by name, and tries each macro rule in turn. ... If no
//! rule matches, an error is reported." D104's implicit `#[::std::main]` over a root `fn main`
//! is the same kind of call (blaze-scope `tests::failed_call` proves its lookup names it); its
//! finding's words say "the implicit `#[::std::main]` over `fn main`".
//!
//! CONTROL (rule 8): without the failed-call findings (names/scope.rs `answer` filing no
//! `FAILED_CALL`), `check` says nothing about the call: zero errors and no line naming it,
//! today's silence; the test fails with "a failed call is an error".
//!
//! A child of main.rs's tests: its helpers are theirs.

use super::*;

/// The call's words, as names/failed_call.rs writes them.
const CALL: &str = "the call of `make`";

/// A member whose `lib.rs` calls `make!` with input no rule takes.
fn workspace(seed: u64, call: &str) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"crates/a\"]\nresolver = \"2\"\n");
    write(&io, "crates/a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "crates/a/src/lib.rs", &format!("#![no_core]\nmacro_rules! make {{\n    (fn $n:ident) => {{ pub fn $n() {{}} }};\n}}\n{call}\npub fn here() {{}}\n"));
    io
}

/// `make!(struct foo);` against `(fn $n:ident)`: `blaze check` fails with one error naming the
/// call of `make` and the matcher's reason (no rule of 1 matched; the closest, rule 1 of 1,
/// expected `fn`, found `struct`), at `crates/a/src/lib.rs`. A repeat check runs nothing and
/// shows it again from the record.
///
/// Prints `failed call: <the finding's line>`.
#[test]
fn a_failed_macro_call_is_an_error_naming_the_call_and_the_reason() {
    let io = workspace(61, "make!(struct foo);");
    let first = check(&io);
    let out = stdout(&io);
    assert!(first.errors >= 1, "a failed call is an error:\n{out}");
    assert_eq!(out.matches(CALL).count(), 1, "exactly one finding names the call:\n{out}");
    let line = out.lines().find(|line| line.contains(CALL)).unwrap_or("");
    assert!(line.contains("no rule of the macro's 1 matched") && line.contains("rule 1 of 1"), "the matcher's reason: {line}\n{out}");
    assert!(out.contains("crates/a/src/lib.rs"), "the item's file is named:\n{out}");
    let seen = out.len();
    let repeat = check(&io);
    let again = &stdout(&io)[seen..];
    assert!(repeat.stood(), "the repeat stands on its certificate:\n{}", repeat.profile);
    assert_eq!(again.matches(CALL).count(), 1, "the repeat shows it from the record:\n{again}");
    println!("failed call: {line}");
}

/// A call a rule takes is no finding (the control of the finding itself).
///
/// Prints nothing; passes as `.. a_call_a_rule_takes_is_no_finding ... ok`.
#[test]
fn a_call_a_rule_takes_is_no_finding() {
    let io = workspace(62, "make!(fn foo);");
    let checked = check(&io);
    let out = stdout(&io);
    assert_eq!(checked.failures, 0, "{}", checked.profile);
    assert_eq!((checked.errors, out.matches(CALL).count()), (0, 0), "{out}");
}
