//! Direct-equivalent tests for `src/native/lark/lexer.rs`.

use super::*;
use alloc::string::ToString;
use alloc::vec;

fn toks(text: &str) -> Vec<Tok> {
    tokenize(text).unwrap().into_iter().map(|t| t.tok).collect()
}

fn err(text: &str) -> String {
    tokenize(text).unwrap_err().message
}

fn rule(s: &str) -> Tok {
    Tok::Rule(s.to_string())
}

fn term(s: &str) -> Tok {
    Tok::Terminal(s.to_string())
}

fn string(body: &str) -> Tok {
    Tok::Str {
        body: body.to_string(),
        flags: String::new(),
    }
}

#[test]
fn simple_rule() {
    assert_eq!(
        toks("start: a B \"x\""),
        vec![
            rule("start"),
            Tok::Colon,
            rule("a"),
            term("B"),
            string("x"),
            Tok::Nl
        ]
    );
}

#[test]
fn punctuation_tokens() {
    assert_eq!(
        toks("a: ( b ) [ c ] d{e, f} | g ~ 1..2 -> h"),
        vec![
            rule("a"),
            Tok::Colon,
            Tok::LPar,
            rule("b"),
            Tok::RPar,
            Tok::LBra,
            rule("c"),
            Tok::RBra,
            rule("d"),
            Tok::LBrace,
            rule("e"),
            Tok::Comma,
            rule("f"),
            Tok::RBrace,
            Tok::Or,
            rule("g"),
            Tok::Tilde,
            Tok::Number(1),
            Tok::DotDot,
            Tok::Number(2),
            Tok::To,
            rule("h"),
            Tok::Nl,
        ]
    );
}

#[test]
fn operators_and_modifiers() {
    assert_eq!(
        toks("?a: b? c* d+\n!e: f\n?!g: h\n!?i: j"),
        vec![
            Tok::RuleModifiers("?".to_string()),
            rule("a"),
            Tok::Colon,
            rule("b"),
            Tok::Op('?'),
            rule("c"),
            Tok::Op('*'),
            rule("d"),
            Tok::Op('+'),
            Tok::Nl,
            Tok::RuleModifiers("!".to_string()),
            rule("e"),
            Tok::Colon,
            rule("f"),
            Tok::Nl,
            Tok::RuleModifiers("?!".to_string()),
            rule("g"),
            Tok::Colon,
            rule("h"),
            Tok::Nl,
            Tok::RuleModifiers("!?".to_string()),
            rule("i"),
            Tok::Colon,
            rule("j"),
            Tok::Nl,
        ]
    );
}

#[test]
fn question_mark_before_underscore_name_is_a_modifier() {
    assert_eq!(toks("?_a: b")[0], Tok::RuleModifiers("?".to_string()));
}

#[test]
fn bang_not_followed_by_name_is_an_error() {
    assert!(err("a: b!").contains("Unexpected input"));
}

#[test]
fn priorities_and_dots() {
    assert_eq!(
        toks("a.2: b\nA.-1: c"),
        vec![
            rule("a"),
            Tok::Dot,
            Tok::Number(2),
            Tok::Colon,
            rule("b"),
            Tok::Nl,
            term("A"),
            Tok::Dot,
            Tok::Number(-1),
            Tok::Colon,
            rule("c"),
            Tok::Nl,
        ]
    );
}

#[test]
fn plus_before_digit_is_a_number() {
    assert_eq!(
        toks("a: b ~ +3"),
        vec![
            rule("a"),
            Tok::Colon,
            rule("b"),
            Tok::Tilde,
            Tok::Number(3),
            Tok::Nl
        ]
    );
}

#[test]
fn minus_alone_is_an_error() {
    assert!(err("a: -").contains("Unexpected input"));
}

#[test]
fn number_too_large() {
    assert!(err("a: b ~ 99999999999999999999").contains("too large"));
}

#[test]
fn strings_with_escapes_and_flags() {
    assert_eq!(
        toks(r#"A: "a\"b\\" "c"i"#),
        vec![
            term("A"),
            Tok::Colon,
            string(r#"a\"b\\"#),
            Tok::Str {
                body: "c".to_string(),
                flags: "i".to_string()
            },
            Tok::Nl,
        ]
    );
}

#[test]
fn string_backslash_before_other_char_is_kept() {
    assert_eq!(toks(r#"A: "\d""#)[2], string(r"\d"));
}

#[test]
fn unterminated_string() {
    assert!(err("A: \"abc").contains("Unterminated string"));
    assert!(err("A: \"abc\ndef\"").contains("Unterminated string"));
}

#[test]
fn regexps_with_escapes_and_flags() {
    assert_eq!(
        toks(r"A: /a\/b\\c/ims /d/"),
        vec![
            term("A"),
            Tok::Colon,
            Tok::Regexp {
                body: r"a\/b\\c".to_string(),
                flags: "ims".to_string()
            },
            Tok::Regexp {
                body: "d".to_string(),
                flags: String::new()
            },
            Tok::Nl,
        ]
    );
}

#[test]
fn regexp_may_span_lines() {
    assert_eq!(
        toks("A: /a\nb/x")[2],
        Tok::Regexp {
            body: "a\nb".to_string(),
            flags: "x".to_string()
        }
    );
}

#[test]
fn unterminated_regexp() {
    assert!(err("A: /abc").contains("Unterminated regular expression"));
}

#[test]
fn double_slash_is_a_comment_not_a_regexp() {
    assert_eq!(
        toks("a: b // c\nd: e"),
        vec![
            rule("a"),
            Tok::Colon,
            rule("b"),
            Tok::Nl,
            rule("d"),
            Tok::Colon,
            rule("e"),
            Tok::Nl
        ]
    );
}

#[test]
fn hash_comments_and_comment_only_lines() {
    assert_eq!(
        toks("# heading\na: b # trailing\n   // indented comment\n  | c\n"),
        vec![
            Tok::Nl,
            rule("a"),
            Tok::Colon,
            rule("b"),
            Tok::NlOr,
            rule("c"),
            Tok::Nl
        ]
    );
}

#[test]
fn a_newline_swallows_any_unicode_whitespace_after_it() {
    for space in ['\u{a0}', '\u{3000}', '\u{1c}', '\u{85}'] {
        assert_eq!(
            toks(&format!("a: b\n{space}| c\n{space}# x\nd: e")),
            vec![
                rule("a"),
                Tok::Colon,
                rule("b"),
                Tok::NlOr,
                rule("c"),
                Tok::Nl,
                rule("d"),
                Tok::Colon,
                rule("e"),
                Tok::Nl
            ]
        );
        assert!(err(&format!("a: b{space}| c")).contains("Unexpected input"));
    }
}

#[test]
fn newline_or_continuation() {
    assert_eq!(
        toks("a: b\n  | c\n\n  | d\n"),
        vec![
            rule("a"),
            Tok::Colon,
            rule("b"),
            Tok::NlOr,
            rule("c"),
            Tok::NlOr,
            rule("d"),
            Tok::Nl
        ]
    );
}

#[test]
fn backslash_continues_a_line() {
    assert_eq!(
        toks("a: b \\  \n   c"),
        vec![rule("a"), Tok::Colon, rule("b"), rule("c"), Tok::Nl]
    );
}

#[test]
fn backslash_not_before_newline_is_an_error() {
    assert!(err("a: b \\ c").contains("Unexpected input"));
}

#[test]
fn directives() {
    assert_eq!(
        toks("%ignore WS\n%import common.INT\n%declare A\n%override a: b\n%extend a: c"),
        vec![
            Tok::Ignore,
            term("WS"),
            Tok::Nl,
            Tok::Import,
            rule("common"),
            Tok::Dot,
            term("INT"),
            Tok::Nl,
            Tok::Declare,
            term("A"),
            Tok::Nl,
            Tok::Override,
            rule("a"),
            Tok::Colon,
            rule("b"),
            Tok::Nl,
            Tok::Extend,
            rule("a"),
            Tok::Colon,
            rule("c"),
            Tok::Nl,
        ]
    );
}

#[test]
fn unknown_directive() {
    assert!(err("%frobnicate x").contains("Unknown directive %frobnicate"));
}

#[test]
fn names_with_underscores_and_digits() {
    assert_eq!(toks("_a1: _B2")[0], rule("_a1"));
    assert_eq!(toks("_a1: _B2")[2], term("_B2"));
}

#[test]
fn mixed_case_name_is_illegal() {
    assert!(err("Aa: b").contains("Illegal name for rules or terminals"));
    assert!(err("aA: b").contains("Illegal name for rules or terminals"));
}

#[test]
fn lone_underscore_is_an_error() {
    assert!(err("_: b").contains("Expecting a name"));
}

#[test]
fn unexpected_character() {
    let message = err("a: b @");
    assert!(
        message.contains("Unexpected input at line 1 column 6"),
        "{message}"
    );
    assert!(message.contains("a: b @"), "{message}");
}

#[test]
fn positions_track_lines_and_columns() {
    let tokens = tokenize("a: b\n  c: d").unwrap();
    let positions: Vec<(u32, u32)> = tokens.iter().map(|t| (t.pos.line, t.pos.column)).collect();
    assert_eq!(
        positions,
        vec![
            (1, 1),
            (1, 2),
            (1, 4),
            (1, 5),
            (2, 3),
            (2, 4),
            (2, 6),
            (2, 7)
        ]
    );
}

#[test]
fn empty_input_is_a_single_newline() {
    assert_eq!(toks(""), vec![Tok::Nl]);
    assert_eq!(toks("\n\n  \n"), vec![Tok::Nl]);
}

#[test]
fn carriage_returns_are_newlines() {
    assert_eq!(
        toks("a: b\r\nc: d"),
        vec![
            rule("a"),
            Tok::Colon,
            rule("b"),
            Tok::Nl,
            rule("c"),
            Tok::Colon,
            rule("d"),
            Tok::Nl
        ]
    );
}
