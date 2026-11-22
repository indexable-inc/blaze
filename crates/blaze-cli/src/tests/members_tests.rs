//! Names in a workspace of more than one member (drive.md, "End state", piece 1, MEMBERS):
//! each path package is placed at its own member's directory. A child of main.rs's tests: its
//! helpers are theirs.

use super::*;

/// A virtual workspace of two members, `a` and `b`, each with its own `src/lib.rs` (different
/// text: a reading of every member's files from the root, or of one member's from another's
/// directory, loses `a::f`); `b` depends on `a` by path and imports `a::f`, which `a` declares,
/// and `a::g`, which it does not (the Rust Reference, "Use declarations": a path that names no
/// item is an error). The check names exactly one unresolved path, at `b`'s first line:
///
/// `b/src/lib.rs:1:8: error[unresolved]: no `g` in `a``
///
/// The failing `use` is the file's first item: two unnamed `use` items share an item id today
/// (the Driver/CLI lane's open bug, handed on 12:52), so a second one would be drawn at the
/// first's line; this test is about the members' placement, not that bug.
#[test]
fn two_members_each_read_under_its_own_directory() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    write(&io, "b/src/lib.rs", "use a::g;\nuse a::f;\npub fn h() -> u8 {\n    f()\n}\n");
    let checked = check(&io);
    let out = stdout(&io);
    assert_eq!(checked.failures, 0, "{out}\n{}", checked.profile);
    assert!(!out.contains("names not resolved"), "the names world is made for two members:\n{out}");
    assert!(!out.contains("no `f` in `a`"), "`a::f` is declared in a/src/lib.rs:\n{out}");
    assert_eq!(out.matches("error[unresolved]").count(), 1, "{out}\n{}", checked.profile);
    assert!(out.contains("b/src/lib.rs:1:8: error[unresolved]: no `g` in `a`"), "{out}");
    assert_eq!((checked.errors, checked.status), (1, Some(1)), "{out}\n{}", checked.profile);
}
