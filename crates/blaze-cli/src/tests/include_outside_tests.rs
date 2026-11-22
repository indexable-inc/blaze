//! Decision D111 (2026-11-16, amending D60; the owner: "sure allow it but give a warning for
//! include str"): a workspace member's include may read under the workspace root; one that
//! leaves its own package is allowed and reported as a warning finding at the call
//! (names/include_outside.rs), never an error; one that leaves the workspace is refused by its
//! path and the rule. The dependency's bound (its own package) is blaze-scope's
//! `tests::include_roots::a_dependency_reads_only_inside_its_package` (a registry package
//! cannot be made in this simulation). A child of main.rs's tests: its helpers are theirs.
//!
//! The crates are `#![no_core]` and declare `include_str!` as core does, so the names step
//! expands the calls (statement calls in a body, where tempo's are).

use super::*;

/// A member at `crates/a` whose body reads `text` (statement calls), beside `other-dir/file.txt`
/// at the workspace root and `crates/a/assets/own.json` in its package.
fn workspace(seed: u64, body: &str) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"crates/a\"]\nresolver = \"2\"\n");
    write(&io, "crates/a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "crates/a/src/lib.rs", &format!("#![no_core]\n#[rustc_builtin_macro]\nmacro_rules! include_str {{ () => {{}} }}\npub fn f() {{\n{body}}}\n"));
    write(&io, "crates/a/assets/own.json", "{\"own\": true}\n");
    write(&io, "other-dir/file.txt", "outside the package, inside the workspace\n");
    io
}

const WARNED: &str = "outside its package";

/// tempo's shape (`crates/precompiles/src/zone_verifier/attestation.rs` reading
/// `../../../nitro-attestation/..`): `crates/a/src/lib.rs` reads `../../../other-dir/file.txt`.
/// Allowed: the check answers, nothing fails, no error; exactly one finding, a warning, says
/// "`include_str!` reads `other-dir/file.txt`, outside its package `crates/a`". A repeat
/// check runs nothing and shows it again from the record. Without D111's member arm the read
/// is `Outside` (refused); without `left_package` there is no warning.
///
/// Prints `warned: <the finding's line>`.
#[test]
fn a_member_reading_outside_its_package_inside_the_workspace_is_allowed_and_warned() {
    let io = workspace(51, "    include_str!(\"../../../other-dir/file.txt\");\n");
    let first = check(&io);
    let out = stdout(&io);
    assert_eq!(first.failures, 0, "{}", first.profile);
    assert_eq!(first.errors, 0, "a warning is no error:\n{out}");
    assert_eq!(out.matches(WARNED).count(), 1, "exactly one finding:\n{out}");
    let line = out.lines().find(|line| line.contains(WARNED)).unwrap_or("");
    assert!(line.contains("include_str!") && line.contains("other-dir/file.txt") && line.contains("crates/a"), "{line}");
    let seen = out.len();
    let repeat = check(&io);
    let again = &stdout(&io)[seen..];
    assert!(repeat.stood(), "the repeat stands on its certificate:\n{}", repeat.profile);
    assert_eq!(again.matches(WARNED).count(), 1, "the repeat shows it from the record:\n{again}");
    println!("warned: {line}");
}

/// An include inside its own package: read, no finding.
///
/// Prints nothing; passes as `.. an_include_inside_its_own_package_is_not_warned ... ok`.
#[test]
fn an_include_inside_its_own_package_is_not_warned() {
    let io = workspace(52, "    include_str!(\"../assets/own.json\");\n");
    let checked = check(&io);
    let out = stdout(&io);
    assert_eq!(checked.failures, 0, "{}", checked.profile);
    assert_eq!((checked.errors, out.matches(WARNED).count()), (0, 0), "{out}");
}

/// An include that leaves the workspace root (`../../../../x.txt` from `crates/a/src`) is
/// refused by its path and the rule (the expansion's `IncludeFailure::Outside`, "decision
/// D60"): an error naming `x.txt`, never read, never a warning.
///
/// Prints `refused: <the line naming the path>`.
#[test]
fn an_include_leaving_the_workspace_is_refused_by_name() {
    let io = workspace(53, "    include_str!(\"../../../../x.txt\");\n");
    let checked = check(&io);
    let out = stdout(&io);
    assert!(checked.errors > 0, "a path leaving the workspace is an error:\n{out}\n{}", checked.profile);
    assert_eq!(out.matches(WARNED).count(), 0, "{out}");
    let line = out.lines().find(|line| line.contains("x.txt")).unwrap_or_else(|| panic!("no line names the path:\n{out}"));
    println!("refused: {line}");
}
