//! A member's integration tests, examples and benches are crate roots the check reads, each a
//! part of its member's units (units.rs `member_units`, the roots of `Member::extras`), their
//! `mod` files found beside them as for any crate root. A child of main.rs's tests: its helpers
//! are theirs.

use super::*;

/// One member `a` with `src/lib.rs`, an integration test `tests/a.rs` that declares `mod common;`
/// whose file is `tests/common/mod.rs`, an example `examples/e.rs` and a bench `benches/x.rs`
/// (the Cargo Book, "Target auto-discovery": each `.rs` file in `tests/`, `examples/` and
/// `benches/` is a target of its own). The module's file is by the Rust Reference, "Modules",
/// "Module source filenames": "a module without a `path` attribute [..] is determined by the
/// crate root file's directory" (for a crate root `tests/a.rs`, the directory `tests/`): the
/// file is `tests/common.rs` or `tests/common/mod.rs`, never `tests/a/common.rs`. Each of the three
/// files imports a name `a` does not declare (the Rust Reference, "Use declarations"); `a` is the
/// package's library, which each of these crates may name. The check answers (no
/// `DriveError::NoPart` for a root no units record listed), reads all four roots and the module,
/// and prints exactly:
///
/// `a/benches/x.rs:1:8: error[unresolved]: no `in_bench` in `a``
/// `a/examples/e.rs:1:8: error[unresolved]: no `in_example` in `a``
/// `a/tests/common/mod.rs:1:8: error[unresolved]: no `in_common` in `a``
///
/// A command that lists only the library's and binaries' roots fails at its first want (the
/// wrong answer this guards); one that reads a test root's modules as if `tests/a.rs` were a
/// `mod a;` file looks for `tests/a/common.rs` and prints a module-file finding instead.
#[test]
fn tests_examples_and_benches_are_checked_with_their_modules() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    write(&io, "a/tests/a.rs", "mod common;\n#[test]\nfn one() {\n    let _one: u8 = a::f();\n}\n");
    write(&io, "a/tests/common/mod.rs", "use a::in_common;\npub fn helper() -> u8 {\n    a::f()\n}\n");
    write(&io, "a/examples/e.rs", "use a::in_example;\nfn main() {\n    a::f();\n}\n");
    write(&io, "a/benches/x.rs", "use a::in_bench;\nfn main() {\n    a::f();\n}\n");
    let checked = check(&io);
    let out = stdout(&io);
    assert_eq!(checked.failures, 0, "{out}\n{}", checked.profile);
    assert!(!out.contains("NoPart"), "every root the graph makes a unit of is a part:\n{out}");
    assert!(!out.contains("error[module-file-missing]"), "`mod common;` in `tests/a.rs` is `tests/common/mod.rs`:\n{out}");
    assert_eq!(out.matches("error[unresolved]").count(), 3, "{out}\n{}", checked.profile);
    assert!(out.contains("a/benches/x.rs:1:8: error[unresolved]: no `in_bench` in `a`"), "{out}");
    assert!(out.contains("a/examples/e.rs:1:8: error[unresolved]: no `in_example` in `a`"), "{out}");
    assert!(out.contains("a/tests/common/mod.rs:1:8: error[unresolved]: no `in_common` in `a`"), "{out}");
    assert_eq!((checked.errors, checked.status), (3, Some(1)), "{out}\n{}", checked.profile);
}
