//! A body waits for and reads only its file's owners' scopes (drive.md, "A body's owners, and
//! what it waits for", ruled 20:07 to 20:31): never every member's. A child of main.rs's tests:
//! its helpers are theirs. Ported to the engine's check on 2026-11-19 (the old checker's
//! `BodyCheck` made counts and file jobs became the engine's `Bodies` stage entries).

use super::*;

/// A workspace of two members that do not depend on each other, `a` and `b`, one body each.
/// Then `a` gains an item with no body (`pub struct Added`), which moves `a`'s names: `a`'s body
/// may be entered again, and `b`'s is not, since `b`'s file's owners are `b`'s root alone and
/// `b`'s check scope (its own dependency closure and the sysroot) holds no `a`. Expected: the
/// edit enters at most one body (`a`'s), and a repeat stands on its certificate.
///
/// CONTROL: the same edit made in BOTH members enters two bodies at most and at least the one
/// the first edit entered, so the count is of the members edited, not a constant.
#[test]
fn an_export_edit_in_one_member_reruns_no_body_of_another() {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "b/src/lib.rs", "pub fn h() -> u8 {\n    2\n}\n");
    let cold = check(&io);
    let out = stdout(&io);
    assert_eq!((cold.errors, cold.failures), (0, 0), "{out}\n{}", cold.profile);
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\npub struct Added;\n");
    let edit = check(&io);
    let out = stdout(&io);
    assert_eq!((edit.errors, edit.failures), (0, 0), "{out}\n{}", edit.profile);
    let one = edit.entered(blaze_engine::Stage::Bodies);
    assert!(one <= 1, "an edit of `a` entered {one} bodies: `b`'s body reads no scope of `a`\n{}", edit.profile);
    let again = check(&io);
    assert!(again.stood(), "nothing moved: the certificate\n{}", again.profile);
    // The control: both members edited.
    write(&io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\npub struct Added;\npub struct More;\n");
    write(&io, "b/src/lib.rs", "pub fn h() -> u8 {\n    2\n}\npub struct Added;\n");
    let both = check(&io);
    let two = both.entered(blaze_engine::Stage::Bodies);
    assert!(two >= one && two <= 2, "both members edited entered {two} bodies (one member: {one})\n{}", both.profile);
}
