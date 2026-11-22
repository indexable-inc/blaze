//! A member's refused test, example or bench target is a finding on its manifest, and the rest of
//! the workspace is still checked (units.rs `member_units`, [`crate::units::TARGET_REFUSED`]). A
//! child of main.rs's tests: its helpers are theirs.

use blaze_report::frame::words::{Area, wording};

use super::*;

/// A virtual workspace of two members. `a` has `src/lib.rs`, a test found by its file
/// (`tests/a.rs`), and a `[[test]]` table `gone` with no `path`, whose inferred root
/// `tests/gone.rs` is not there (the Cargo Book, "Target auto-discovery"; a missing file is never
/// replaced by a guess). (A table with an explicit `path` is rooted at exactly that file,
/// layout/extra.rs `written`, and one that is absent stops at the root's read: see
/// [`an_explicit_test_path_is_rooted_at_that_file`].) `b` depends on `a` by path and imports
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
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[test]]\nname = \"gone\"\n");
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    write(&io, "a/tests/a.rs", "use a::nope;\n#[test]\nfn one() {\n    let _one: u8 = a::f();\n}\n");
    write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    write(&io, "b/src/lib.rs", "use a::g;\nuse a::f;\npub fn h() -> u8 {\n    f()\n}\n");
    let checked = check(&io);
    let out = stdout(&io);
    assert_eq!(checked.failures, 0, "{out}\n{}", checked.profile);
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

/// AN EXPLICIT `path` IS THE ROOT, WHEREVER IT IS UNDER THE PACKAGE (turso, 2026-11-20: its member
/// `core_tester` in the folder `tests` names `integration/mod.rs` and was refused "no file ... for
/// its root"; the Cargo Book, "Configuring a target": "The `path` field specifies where the source
/// for the crate is located, relative to the Cargo.toml file"). The member is a folder named
/// `tests`, its library at `lib.rs` and its test at `integration/mod.rs`, a file no listing the
/// layout asks for shows (sweep.rs lists a folder's `lib.rs`, `main.rs` and `build.rs` only, which
/// misled the old check). The test's root is read and checked: its one import of a name the
/// library does not declare is the one finding, and its `mod helper;` is found beside it
/// (`integration/helper.rs`, the Rust Reference, "Module source filenames"). CONTROL: the old
/// lookup in the listing refuses the target, printing `error[target-refused]` and no import line.
#[test]
fn an_explicit_test_path_is_rooted_at_that_file() {
    let io = Io::simulated(SimWorld::new(13, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"tests\"]\nresolver = \"2\"\n");
    write(&io, "tests/Cargo.toml", "[package]\nname = \"core_tester\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"lib.rs\"\n\n[[test]]\nname = \"integration_tests\"\npath = \"integration/mod.rs\"\n");
    write(&io, "tests/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    write(&io, "tests/integration/mod.rs", "use core_tester::nope;\nmod helper;\n#[test]\nfn one() {\n    let _one: u8 = helper::g();\n}\n");
    write(&io, "tests/integration/helper.rs", "pub fn g() -> u8 {\n    core_tester::f()\n}\n");
    let checked = check(&io);
    let out = stdout(&io);
    assert_eq!(checked.failures, 0, "{out}\n{}", checked.profile);
    assert!(!out.contains("error[target-refused]"), "an explicit path is the root, not looked up in a listing:\n{out}");
    assert!(out.contains("tests/integration/mod.rs:1:"), "the test's root is checked:\n{out}");
    assert!(out.contains("error[unresolved]: no `nope` in `core_tester`"), "{out}");
    assert!(!out.contains("helper"), "`mod helper;` is found beside the root and `g` resolves:\n{out}");
    assert_eq!((checked.errors, checked.status), (1, Some(1)), "{out}\n{}", checked.profile);
}

/// A PACKAGE OF ONLY A BENCH IS A PACKAGE (turso, 2026-11-20: a member with only `[[bench]]`
/// tables was refused "the package has no library and no binary"; the Cargo Book, "Cargo
/// Targets": a package's targets are its library, binaries, examples, tests and benches, and
/// only one with none at all is refused, units.rs `member_units`). Member `c` has no library and
/// no binary, and one `[[bench]]` `b` with `harness = false`, its root `benches/b.rs` (inferred
/// from the name), importing a name `a` does not declare. The bench is checked: its one import is
/// the one finding, and no manifest finding is printed. CONTROL: the old rule prints
/// `error[manifest-not-read]` on `c/Cargo.toml` and never reaches the bench.
#[test]
fn a_bench_only_member_is_checked() {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"c\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    write(&io, "c/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n\n[[bench]]\nname = \"b\"\nharness = false\n");
    write(&io, "c/benches/b.rs", "use a::nope;\nfn main() {\n    let _one: u8 = a::f();\n}\n");
    let checked = check(&io);
    let out = stdout(&io);
    assert_eq!(checked.failures, 0, "{out}\n{}", checked.profile);
    assert!(!out.contains("has no target"), "a bench is a target:\n{out}");
    assert!(!out.contains("error[manifest-not-read]") && !out.contains("error[target-refused]"), "{out}");
    assert!(out.contains("c/benches/b.rs:1:8: error[unresolved]: no `nope` in `a`"), "the bench is checked:\n{out}");
    assert_eq!((checked.errors, checked.status), (1, Some(1)), "{out}\n{}", checked.profile);
}
