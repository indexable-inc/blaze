//! A body's escaped items (`values::escaped`): the digest is the same in two databases of one
//! source, stays when a statement or a method's body inside the body changes, moves when an
//! impl in the body gains a method or a method changes its signature, and is `None` for a body
//! that declares nothing that escapes.

use blaze_ids::ContentId;

use super::perturb::{body, db};
use crate::values::escaped;

const BODY: &str = r#"
//- /lib.rs crate:main
pub fn f() -> u32 {
    struct Local;
    impl Local {
        pub fn get(&self) -> u32 {
            1
        }
    }
    let x = 2;
    Local.get() + x
}
"#;

/// The escaped digest of the one function named `name` in `fixture`.
#[track_caller]
fn escaped_in(fixture: &str, name: &str) -> Option<ContentId> {
    let db = db(fixture);
    escaped(&db, body(&db, name)).unwrap_or_else(|what| panic!("{what}"))
}

#[track_caller]
fn edited(from: &str, to: &str) -> String {
    let after = BODY.replace(from, to);
    assert_ne!(after, BODY, "the edit `{from}` → `{to}` applies");
    after
}

#[test]
fn two_databases_of_one_source_give_one_digest() {
    let one = escaped_in(BODY, "f");
    assert!(one.is_some(), "an impl in the body escapes");
    assert_eq!(one, escaped_in(BODY, "f"));
}

#[test]
fn an_edit_inside_the_body_no_reader_sees_leaves_it() {
    let before = escaped_in(BODY, "f");
    assert_eq!(before, escaped_in(&edited("let x = 2;", "let x = 3;"), "f"), "a statement");
    assert_eq!(
        before,
        escaped_in(&edited("let x = 2;", "let y = 5;\n    let x = y - 3;"), "f"),
        "a statement added before nothing that escapes"
    );
    assert_eq!(before, escaped_in(&edited("            1\n", "            7\n"), "f"), "a method's body");
}

#[test]
fn an_impl_in_the_body_gaining_a_method_or_changing_a_signature_moves_it() {
    let before = escaped_in(BODY, "f");
    let more = edited(
        "        pub fn get(&self) -> u32 {",
        "        pub fn more(&self) {}\n        pub fn get(&self) -> u32 {",
    );
    assert_ne!(before, escaped_in(&more, "f"), "a method added");
    let signature = edited("pub fn get(&self) -> u32 {", "pub fn get(&self, _n: u8) -> u32 {")
        .replace("Local.get() + x", "Local.get(0) + x");
    assert_ne!(before, escaped_in(&signature, "f"), "a parameter added");
    let renamed = edited("pub fn get(&self)", "pub fn got(&self)").replace("Local.get()", "Local.got()");
    assert_ne!(before, escaped_in(&renamed, "f"), "a method renamed: its name is what a lookup reads");
}

#[test]
fn a_body_that_declares_nothing_that_escapes_has_none() {
    const PLAIN: &str = r#"
//- /lib.rs crate:main
pub fn f() -> u32 {
    macro_rules! two {
        () => {
            2
        };
    }
    let x = two!();
    x + 1
}
pub fn g() -> u32 {
    #[no_mangle]
    extern "C" fn exported() {}
    1
}
"#;
    assert_eq!(escaped_in(PLAIN, "f"), None, "a macro not exported, used inside the body alone");
    // The control: an exported symbol in a body of the same fixture escapes.
    assert!(escaped_in(PLAIN, "g").is_some(), "a `#[no_mangle]` function escapes");
}
