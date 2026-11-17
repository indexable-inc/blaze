//! A member's refused test, example or bench target is a finding on its manifest, and the rest of
//! the workspace is still checked (units.rs `member_units`, [`crate::units::TARGET_REFUSED`]). A
//! child of main.rs's tests: its helpers are theirs.

use blaze_report::frame::words::{Area, wording};

use super::*;

/// A virtual workspace of two members. `a` has `src/lib.rs`, a test found by its file
/// (`tests/a.rs`), and a `[[test]]` table naming `tests/gone.rs`, which is not there (the Cargo
/// Book, "Cargo Targets": "The `path` field specifies where the source for the crate is
/// located"; a missing file is never replaced by a guess). `b` depends on `a` by path and imports
/// `a::g`, which `a` does not declare (the Rust Reference, "Use declarations"); `tests/a.rs`, a
/// crate of its own that depends on `a`, imports `a::nope`, likewise undeclared. The check prints
/// the three findings and fails:
///
/// `a/Cargo.toml:6:1: error[target-refused]: the test target `gone` of package `a` is left out: no file `a/tests/gone.rs` for its root`
/// `a/tests/a.rs:1:8: error[unresolved]: no `nope` in `a``
/// `b/src/lib.rs:1:8: error[unresolved]: no `g` in `a``
///
/// A command that stops at the refused target (the old `LayoutError::Target`) prints only a
/// manifest finding and never reaches `b`'s names; one that drops the refusal quietly prints only
/// `b`'s line: each fails here. The test found by its file (`a`) is not refused.
#[test]
fn a_refused_test_target_is_one_finding_and_the_rest_is_checked() {
    assert!(wording(Area::Cli, crate::units::TARGET_REFUSED).is_some(), "the rule has no words in WORDINGS");
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[test]]\nname = \"gone\"\npath = \"tests/gone.rs\"\n");
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    write(&io, "a/tests/a.rs", "use a::nope;\n#[test]\nfn one() {\n    let _one: u8 = a::f();\n}\n");
    write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    write(&io, "b/src/lib.rs", "use a::g;\nuse a::f;\npub fn h() -> u8 {\n    f()\n}\n");
    let checked = check(&io, work::Sizes::USUAL);
    let out = stdout(&io);
    assert!(checked.failed.is_empty(), "{:?}\n{out}", checked.failed);
    assert_eq!(out.matches("error[target-refused]").count(), 1, "{out}\n{}", checked.profile);
    assert!(out.contains("a/Cargo.toml:6:"), "the finding is on the `[[test]]` table's line:\n{out}");
    assert!(out.contains("the test target `gone` of package `a` is left out: no file `a/tests/gone.rs` for its root"), "{out}");
    assert!(!out.contains("target `a`"), "the test found by its file is not refused:\n{out}");
    assert!(!out.contains("error[manifest-not-read]"), "the manifest is read: one target is refused, not the layout:\n{out}");
    assert_eq!(out.matches("error[unresolved]").count(), 2, "{out}\n{}", checked.profile);
    assert!(out.contains("a/tests/a.rs:1:8: error[unresolved]: no `nope` in `a`"), "the member's other test is still checked:\n{out}");
    assert!(out.contains("b/src/lib.rs:1:8: error[unresolved]: no `g` in `a`"), "the other member is still checked:\n{out}");
    assert_eq!((checked.errors, checked.status), (3, Some(1)), "{out}\n{}", checked.profile);
}
