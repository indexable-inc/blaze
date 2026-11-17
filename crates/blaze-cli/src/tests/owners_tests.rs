//! A body waits for and reads only its file's owners' scopes (drive.md, "A body's owners, and
//! what it waits for", ruled 20:07 to 20:31): never every member's. A child of main.rs's tests:
//! its helpers are theirs.

use super::*;

/// A workspace of two members that do not depend on each other, `a` and `b`, one body each.
/// Cold, both bodies are checked. Then `a` gains an item with no body (`pub struct Added`), which moves `a`'s
/// `CrateScope` out and `a`'s file's text around its bodies: `a`'s body is checked again, and
/// `b`'s is not, since `b`'s file's owners are `b`'s root alone and `b`'s check scope (its own
/// dependency closure and the sysroot) holds no `a`. Expected: "BodyCheck made 1, in 1 file
/// jobs": `a`'s one body, `b`'s none.
///
/// Control (run once, 2026-11-16 21:24, before the drive's wait on the moved `Surface`, when
/// the fix read (2, 2)): with every member taken as an owner of every file (the "members" read
/// list this replaced), `b`'s body reruns: "left: (3, 3)".
#[test]
fn an_export_edit_in_one_member_reruns_no_body_of_another() {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "b/src/lib.rs", "pub fn h() -> u8 {\n    2\n}\n");
    let cold = check(&io, work::Sizes::USUAL);
    let out = stdout(&io);
    assert!(cold.failed.is_empty(), "{:?}\n{out}", cold.failed);
    assert_eq!(made(&cold.profile, "BodyCheck"), 2, "cold, both members' bodies are checked:\n{out}\n{}", cold.profile);
    assert!(!out.contains("front2 lists no body"), "each file has one owner, its own crate:\n{out}");
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\npub struct Added;\n");
    let edit = check(&io, work::Sizes::USUAL);
    let out = stdout(&io);
    assert!(edit.failed.is_empty(), "{:?}\n{out}", edit.failed);
    let bodies = made(&edit.profile, "BodyCheck");
    eprintln!("BodyCheck made {bodies}, in {} file jobs", edit.body_jobs);
    // `a`'s one body, entered once: it waits on its file's moved `Surface`, so its reads are
    // `a`'s remade scopes (DRIVER runner 19, 21:35; before it, (2, 2): entered over its last
    // reads first). `b`'s file is never prepared (RUNNER 7, 21:23).
    assert_eq!((bodies, edit.body_jobs), (1, 1), "only `a`'s body is checked again; `b` reads no scope of `a`:\n{out}\n{}", edit.profile);
}
