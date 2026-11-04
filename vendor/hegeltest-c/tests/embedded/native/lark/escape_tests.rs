//! Direct-equivalent tests for `src/native/lark/escape.rs`. Expected
//! values come from running Lark's `eval_escaping`, `_literal_to_pattern`
//! and Python's `re.escape` on the same inputs.

use super::*;

#[test]
fn eval_escaping_interprets_the_python_escapes() {
    assert_eq!(eval_escaping(r"a\nb\tc\fd\re").unwrap(), "a\nb\tc\x0cd\re");
    assert_eq!(eval_escaping(r"\x41é\U0001F600").unwrap(), "Aé😀");
}

#[test]
fn eval_escaping_keeps_other_backslashes() {
    assert_eq!(eval_escaping(r"\d\.\/\\").unwrap(), r"\d\.\/\\");
    assert_eq!(eval_escaping(r"\\n").unwrap(), r"\\n");
}

#[test]
fn eval_escaping_unescapes_quotes_like_lark() {
    assert_eq!(eval_escaping(r#"a\"b"#).unwrap(), r#"a"b"#);
    assert_eq!(eval_escaping(r#"\\""#).unwrap(), r#"\""#);
    assert_eq!(eval_escaping("it's").unwrap(), "it's");
    assert_eq!(string_literal(r#"\""#, "").unwrap(), Pattern::str("\""));
}

#[test]
fn eval_escaping_rejects_a_trailing_backslash() {
    assert!(
        eval_escaping("abc\\")
            .unwrap_err()
            .message
            .contains("Literal ended unexpectedly")
    );
}

#[test]
fn eval_escaping_rejects_bad_hex() {
    assert!(
        eval_escaping(r"\x4")
            .unwrap_err()
            .message
            .contains("Truncated")
    );
    for literal in [
        r"\xzz",
        r"\x+1",
        r"\x-1",
        r"\u+123",
        r"\U+123456a",
        r"\U 1234567",
    ] {
        assert!(
            eval_escaping(literal)
                .unwrap_err()
                .message
                .contains("Truncated"),
            "{literal}"
        );
    }
    assert_eq!(eval_escaping(r"\x41\u0042\U00000043").unwrap(), "ABC");
    assert!(
        eval_escaping(r"\ud800")
            .unwrap_err()
            .message
            .contains("Invalid")
    );
}

#[test]
fn string_literal_collapses_double_backslashes() {
    assert_eq!(string_literal(r"a\\b", "").unwrap(), Pattern::str(r"a\b"));
    assert_eq!(string_literal(r"\\n", "").unwrap(), Pattern::str(r"\n"));
}

#[test]
fn string_literal_keeps_flags() {
    assert_eq!(
        string_literal("ab", "i").unwrap(),
        Pattern::str("ab").with_flags("i")
    );
}

#[test]
fn string_literal_rejects_newlines_and_empty() {
    assert!(
        string_literal("a\nb", "")
            .unwrap_err()
            .message
            .contains("newlines")
    );
    assert!(
        string_literal("", "")
            .unwrap_err()
            .message
            .contains("Empty terminals")
    );
}

#[test]
fn regex_literal_keeps_escapes_and_sorts_flags() {
    assert_eq!(
        regex_literal(r"\d+\/", "si").unwrap(),
        Pattern::re(r"\d+\/").with_flags("is")
    );
    assert_eq!(
        regex_literal("a\nb", "x").unwrap(),
        Pattern::re("a\nb").with_flags("x")
    );
}

#[test]
fn regex_literal_rejects_newlines_without_x_and_empty() {
    assert!(
        regex_literal("a\nb", "")
            .unwrap_err()
            .message
            .contains("verbose")
    );
    assert!(
        regex_literal("", "")
            .unwrap_err()
            .message
            .contains("Empty terminals")
    );
}

#[test]
fn range_literal_uses_raw_text() {
    assert_eq!(range_literal("a", "z").unwrap(), Pattern::re("[a-z]"));
    assert_eq!(
        range_literal(r"\x00", r"\x7f").unwrap(),
        Pattern::re(r"[\x00-\x7f]")
    );
}

#[test]
fn range_literal_requires_single_characters() {
    assert!(
        range_literal("ab", "z")
            .unwrap_err()
            .message
            .contains("single characters")
    );
    assert!(
        range_literal("a", "")
            .unwrap_err()
            .message
            .contains("single characters")
    );
}

#[test]
fn re_escape_matches_python() {
    assert_eq!(
        re_escape(" \t\n\r\x0b\x0c-#&~azAZ09_\x00é/:;,<>=@!\"'%"),
        "\\ \\\t\\\n\\\r\\\x0b\\\x0c\\-\\#\\&\\~azAZ09_\x00é/:;,<>=@!\"'%"
    );
    assert_eq!(
        re_escape(".^$*+?{}[]\\|()"),
        r"\.\^\$\*\+\?\{\}\[\]\\\|\(\)"
    );
}

#[test]
fn pattern_to_regexp_wraps_each_flag() {
    assert_eq!(pattern_to_regexp(&Pattern::str("a.b")), r"a\.b");
    assert_eq!(
        pattern_to_regexp(&Pattern::str("ab").with_flags("i")),
        "(?i:ab)"
    );
    assert_eq!(
        pattern_to_regexp(&Pattern::re("x").with_flags("si")),
        "(?s:(?i:x))"
    );
}

#[test]
fn eval_escaping_handles_quotes_and_four_digit_unicode_escapes() {
    assert_eq!(eval_escaping(r"it\'s").unwrap(), r"it\'s");
    assert_eq!(eval_escaping(r"\u00e9\u0041").unwrap(), "éA");
}
