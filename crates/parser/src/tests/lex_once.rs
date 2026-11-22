//! The lexer's per-text questions asked once per text (lane RP4, floor C2): a text is scanned
//! for a CR once ([`crate::line_break::has_cr`]), and a text with none asks no string's or
//! comment's range whether it holds a bare CR ([`crate::line_break::has_bare_cr`]). A text with
//! CRs still gets every answer it got before: a CR LF in a doc comment is a break, a bare CR an
//! error.

use crate::line_break::counts::{BARE_CR_ASKS, CR_SCANS};
use crate::{Edition, LexedStr};

/// Strings, raw strings, byte strings, chars, line, block and doc comments: every token kind
/// that asked the bare-CR question per token.
const NO_CR: &str = r####"
/// An outer doc comment.
//! An inner doc comment.
/** A block doc comment. */
/*! An inner block doc comment, /* nested */ still. */
// A plain comment.
fn f() -> &'static str {
    let a = "a string with \" an escape";
    let b = r#"a raw "string""#;
    let c = br##"a raw byte string"##;
    let d = b"bytes";
    let e = '}';
    let g = b'x';
    let h = "plain";
    "end"
}
"####;

fn counts() -> (u64, u64) {
    (CR_SCANS.with(|n| n.get()), BARE_CR_ASKS.with(|n| n.get()))
}

/// Expected line: `test tests::lex_once::a_text_with_no_cr_asks_no_token ... ok`, printing
/// `LEX ONCE: N tokens, CR scans 1, bare-CR questions 0 (floor 1, 0)`.
#[test]
fn a_text_with_no_cr_asks_no_token() {
    let before = counts();
    let lexed = LexedStr::new(Edition::Edition2021, NO_CR);
    let after = counts();
    let (scans, asks) = (after.0 - before.0, after.1 - before.1);
    std::println!("LEX ONCE: {} tokens, CR scans {scans}, bare-CR questions {asks} (floor 1, 0)", lexed.len());
    assert_eq!(lexed.errors().count(), 0, "the text lexes with errors");
    assert_eq!(scans, 1, "the text was scanned for a CR more than once");
    assert_eq!(asks, 0, "a token of a text with no CR asked the bare-CR question");
}

/// Expected line: `test tests::lex_once::a_text_with_crs_keeps_every_answer ... ok`. A CR LF
/// in a doc comment and a string is a break (no error); a bare CR in a doc comment and in a
/// string is an error, as before.
#[test]
fn a_text_with_crs_keeps_every_answer() {
    let fine = LexedStr::new(Edition::Edition2021, "/// doc\r\nfn f() { \"a\r\nb\"; }\r\n");
    assert_eq!(fine.errors().count(), 0, "a CR LF was taken as a bare CR");
    let before = counts();
    let bare_doc = LexedStr::new(Edition::Edition2021, "/// doc\rmore\nfn f() {}\n");
    assert!(counts().1 > before.1, "a text with a CR asked no token");
    assert!(bare_doc.errors().any(|(_, m)| m.contains("bare CR")), "a bare CR in a doc comment is no error");
    let bare_str = LexedStr::new(Edition::Edition2021, "fn f() { \"a\rb\"; }\n");
    assert!(bare_str.errors().count() > 0, "a bare CR in a string is no error");
}
