//! Tests for `src/native/lark/mod.rs`.

use super::*;
use alloc::format;

#[test]
fn grammar_error_displays_its_message() {
    let err = GrammarError::new("Rule 'x' used but not defined");
    assert_eq!(format!("{err}"), "Rule 'x' used but not defined");
}
