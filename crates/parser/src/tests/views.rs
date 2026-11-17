//! `#![blaze::views]`'s view of a receiver, `&self.{a, b}` (`grammar/params.rs`, `self_view`).

use super::{has_run, parse_lines};
use crate::Edition;

/// The fields of a view are `NAME_REF`s in the `SELF_PARAM`, after `self`, its `.` and `{`.
#[test]
fn a_receiver_takes_a_view_of_fields() {
    for text in ["impl S { fn f(&self.{a, b}) {} }", "impl S { fn f(&'a mut self.{a,}, x: u8) {} }"]
    {
        let (tree, errors) = parse_lines(text, Edition::CURRENT);
        assert_eq!(errors, Vec::<String>::new(), "{text}: {tree:#?}");
        assert!(has_run(&tree, &["SELF_KW \"self\"", "DOT \".\"", "L_CURLY \"{\""]), "{tree:#?}");
        assert!(has_run(&tree, &["NAME_REF", "IDENT \"a\""]), "{text}: {tree:#?}");
    }
}

/// A receiver without a view parses as it does in Rust.
#[test]
fn a_receiver_without_a_view_is_unchanged() {
    let (tree, errors) = parse_lines("impl S { fn f(&self) { self.a; } }", Edition::CURRENT);
    assert_eq!(errors, Vec::<String>::new(), "{tree:#?}");
    assert!(!has_run(&tree, &["SELF_KW \"self\"", "DOT \".\"", "L_CURLY \"{\""]), "{tree:#?}");
}
