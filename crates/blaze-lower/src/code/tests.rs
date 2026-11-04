//! [`code_text_of`] on files written here: which comment edits a crate's code can observe.

use span::Edition;
use syntax::{AstNode, SourceFile};

use super::{CodeText, code_text_of};

fn code(text: &str) -> CodeText {
    let parse = SourceFile::parse(text, Edition::CURRENT);
    code_text_of(parse.tree().syntax(), text)
}

/// An edit of a doc comment's text no macro sees (one doctest example of a library) leaves its
/// code text as it was, with the prelude's derives taken as inert or not.
#[test]
fn an_unseen_doc_edit_is_masked() {
    let before =
        code("/// ```\n/// assert_eq!(f(), 1);\n/// ```\n#[inline]\npub fn f() -> u8 { 1 }\n");
    let after =
        code("/// ```\n/// assert_eq!(f(), 2);\n/// ```\n#[inline]\npub fn f() -> u8 { 1 }\n");
    assert_eq!(before, after);
    let plain = code("// one\nfn f() {}\n");
    assert_eq!(plain, code("// two\nfn f() {}\n"), "a `//` comment is never seen");
    let derived = code("#[derive(Debug, Clone)]\nstruct S {\n    /// one\n    f: u8,\n}\n");
    assert_eq!(
        derived.inert,
        code("#[derive(Debug, Clone)]\nstruct S {\n    /// two\n    f: u8,\n}\n").inert
    );
    assert_ne!(
        derived.strict,
        code("#[derive(Debug, Clone)]\nstruct S {\n    /// two\n    f: u8,\n}\n").strict
    );
    assert!(!derived.shadows && !derived.whole);
}

/// A comment's shape stays: an edit that moves the code after it (another line, a block
/// comment's last line longer) changes the code text, for `line!()`, panics and debug info.
#[test]
fn a_comments_shape_is_kept() {
    let one = code("/// a\nfn f() { line!(); }\n");
    assert_ne!(one, code("/// a\n/// b\nfn f() { line!(); }\n"), "a line more");
    let block = code("/* ab */ fn f() { column!(); }\n");
    assert_eq!(block, code("/* cd */ fn f() { column!(); }\n"));
    assert_ne!(block, code("/* abc */ fn f() { column!(); }\n"), "a column more");
    assert_ne!(
        code("/* é */ fn f() {}\n"),
        code("/* ee */ fn f() {}\n"),
        "as many bytes, fewer characters"
    );
    assert_ne!(code("/// a\nfn f() { 1 }\n"), code("/// a\nfn f() { 2 }\n"), "the code itself");
}

/// A doc comment a macro is given keeps its text: in a macro call, a `macro_rules!` body, or
/// on an item (or under one) given to a derive or attribute macro of another crate.
#[test]
fn a_doc_a_macro_sees_is_kept() {
    let pairs = [
        ("m! {\n    /// one\n    fn f() {}\n}\n", "m! {\n    /// two\n    fn f() {}\n}\n"),
        (
            "macro_rules! m {\n    () => {\n        /// one\n        fn f() {}\n    };\n}\n",
            "macro_rules! m {\n    () => {\n        /// two\n        fn f() {}\n    };\n}\n",
        ),
        (
            "#[derive(Serialize)]\nstruct S {\n    /// one\n    f: u8,\n}\n",
            "#[derive(Serialize)]\nstruct S {\n    /// two\n    f: u8,\n}\n",
        ),
        (
            "/// one\n#[tokio::main]\nasync fn main() {}\n",
            "/// two\n#[tokio::main]\nasync fn main() {}\n",
        ),
        (
            "#[my_attr]\nmod m {\n    /// one\n    fn f() {}\n}\n",
            "#[my_attr]\nmod m {\n    /// two\n    fn f() {}\n}\n",
        ),
        (
            "mod m {\n    #![my_attr]\n    /// one\n    fn f() {}\n}\n",
            "mod m {\n    #![my_attr]\n    /// two\n    fn f() {}\n}\n",
        ),
        (
            "/// one\n#[cfg_attr(feature = \"x\", my_attr)]\nfn f() {}\n",
            "/// two\n#[cfg_attr(feature = \"x\", my_attr)]\nfn f() {}\n",
        ),
    ];
    for (one, two) in pairs {
        let (one_code, two_code) = (code(one), code(two));
        assert_ne!(one_code.inert, two_code.inert, "{one}");
        assert_ne!(one_code.strict, two_code.strict, "{one}");
    }
    let inert = (
        "/// one\n#[cfg_attr(test, inline, rustfmt::skip)]\nfn f() {}\n",
        "/// two\n#[cfg_attr(test, inline, rustfmt::skip)]\nfn f() {}\n",
    );
    assert_eq!(code(inert.0), code(inert.1), "a `cfg_attr` of inert attributes");
}

/// A file that could shadow a standard derive, `test` or `bench` says so, so its crate keys the
/// items those see by their docs too; an import from the standard library or the crate can't.
#[test]
fn a_shadowing_file_is_caught() {
    for text in [
        "use serde::Debug;\n",
        "use other::{Clone, Debug as Shown};\n",
        "use other::thing::Hash as Clone;\n",
        "use other::*;\n",
        "use other::{a::*, b};\n",
        "#[macro_use]\nextern crate other;\n",
        "macro_rules! test { () => {} }\n",
    ] {
        assert!(code(text).shadows, "{text}");
    }
    for text in [
        "use core::hash::{Hash, Hasher};\n",
        "use std::fmt::Debug;\n",
        "use crate::*;\n",
        "use super::*;\n",
        "use other::{Thing, More};\n",
        "use other::Thing as Other;\n",
        "#[macro_use]\nextern crate alloc;\n",
    ] {
        assert!(!code(text).shadows, "{text}");
    }
}

/// A file that reads bytes that may be Rust (`include_str!` of a `.rs` file or of a path that
/// is not one literal), or gives an out-of-line module to an attribute, makes its crate keep
/// its whole text; reading a README or an inert attribute on `mod x;` does not.
#[test]
fn a_file_its_code_may_read_whole_keeps_its_text() {
    for text in [
        "const S: &str = include_str!(\"lib.rs\");\n",
        "const S: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/x.txt\"));\n",
        "const S: &str = include_str!(PATH);\n",
        "#[my_attr]\nmod other;\n",
    ] {
        assert!(code(text).whole, "{text}");
    }
    for text in [
        "#![doc = include_str!(\"../README.md\")]\n",
        "const S: &[u8] = include_bytes!(\"data.bin\");\n",
        "#[cfg(test)]\nmod tests;\n",
    ] {
        assert!(!code(text).whole, "{text}");
    }
}
