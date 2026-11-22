//! The rule of crate::line_break, generated: a source in its LF form and with some or all of
//! its line breaks CR LF lexes to the same tokens, errors and values, token for token, with
//! every position mapped by the offset map; and the hand-stated cases.

use blaze_sim::{Broke, Config, Tape};

use super::{doc_comment_value, has_bare_cr, is_bare_cr, line_break_len, line_comment_text, without_break_crs};
use crate::unescape::{
    EscapeError, check_raw_byte_str, check_raw_c_str, check_raw_str, unescape_byte, unescape_byte_str,
    unescape_c_str, unescape_char, unescape_str_breaking,
};
use crate::{Edition, LexedStr, SyntaxKind};

/// Pieces of Rust in their LF form; every `\n` in them is a line break of the file.
const PIECES: &[&str] = &[
    "a",
    ";",
    "\n",
    "  \n\t",
    // A string's continuation, over one line and over two.
    "\"x\\\n   y\"",
    "\"x\\\n\n  y\"",
    "\"\\\n\"",
    // A line break inside a string, a raw string, a byte string and a C string.
    "\"a\nb\"",
    "r\"a\nb\"",
    "r#\"a\n\"b\"#",
    "b\"x\\\n  y\\n\"",
    "br\"a\nb\"",
    "c\"x\\\n  y\"",
    "cr\"a\nb\"",
    "\"\\t\\\\\\u{41}\\x41\\\"\"",
    // Characters and bytes, and a line break where a character must be escaped.
    "'a'",
    "'\\n'",
    "b'\\r'",
    "'\n'",
    // Comments, doc comments of both kinds and styles.
    "// c\n",
    "/// d\n",
    "//! d e\n",
    "/* a\n b */",
    "/** a\n b */",
    "/*! a\n */",
];

/// Frontmatters a drawn source may open with (after its shebang), in their LF form.
const FRONTMATTERS: &[&str] = &["---\n---\n", "---cargo\n[package]\nedition = \"2024\"\n---\n", "----  x.toml \n\n----\n"];

/// Which rules the values are read by: the tree's, or a planted bug.
#[derive(Clone, Copy, Debug)]
enum Rule {
    /// [`line_break_len`] and [`doc_comment_value`].
    Clean,
    /// The continuation takes only LF, as a lexer that never met a CR LF would.
    ContinuationLfOnly,
    /// A block doc comment's value is its raw text, the CR of each CR LF kept, as
    /// `syntax`'s `text()` returns it.
    BlockDocRaw,
}

fn lf_only(rest: &[u8]) -> Option<usize> {
    (rest.first() == Some(&b'\n')).then_some(1)
}

/// The normalised position of the raw position `p`: `p` less the CRs of CR LFs before it.
fn offset_map(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut map = Vec::with_capacity(bytes.len() + 1);
    let mut crs = 0;
    for at in 0..=bytes.len() {
        map.push(at - crs);
        if at < bytes.len() && line_break_len(&bytes[at..]) == Some(2) {
            crs += 1;
        }
    }
    map
}

/// One token as the oracle compares it: kind, normalised range, error, and its value (a
/// literal's units or errors with normalised ranges, a doc comment's text).
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    kind: SyntaxKind,
    range: core::ops::Range<usize>,
    error: Option<String>,
    value: Vec<String>,
}

/// The value of the literal or doc comment `token`, at `start` in `text`: each unit with its
/// normalised range.
fn value(text: &str, start: usize, token: &str, kind: SyntaxKind, map: &[usize], rule: Rule) -> Vec<String> {
    let mut out = Vec::new();
    let at = |r: core::ops::Range<usize>, inner: usize| map[start + inner + r.start]..map[start + inner + r.end];
    let Some(lit) = rustc_lexer::tokenize(token, rustc_lexer::FrontmatterAllowed::No).next() else {
        return out;
    };
    use rustc_lexer::LiteralKind as L;
    match lit.kind {
        rustc_lexer::TokenKind::Literal { kind: lk, suffix_start } => {
            let quoted = &token[..suffix_start as usize];
            let hashes = |n: Option<u8>| usize::from(n.unwrap_or(0));
            let (open, close) = match lk {
                L::Str { .. } | L::Char { .. } => (1, 1),
                L::ByteStr { .. } | L::CStr { .. } | L::Byte { .. } => (2, 1),
                L::RawStr { n_hashes } => (2 + hashes(n_hashes), 1 + hashes(n_hashes)),
                L::RawByteStr { n_hashes } | L::RawCStr { n_hashes } => {
                    (3 + hashes(n_hashes), 1 + hashes(n_hashes))
                }
                L::Int { .. } | L::Float { .. } => return out,
            };
            let Some(inside) = quoted.get(open..quoted.len().saturating_sub(close)) else { return out };
            let mut push = |r: core::ops::Range<usize>, v: Result<String, EscapeError>| {
                out.push(format!("{:?} {v:?}", at(r, open)));
            };
            match lk {
                L::Str { .. } => {
                    let line_break = match rule {
                        Rule::Clean | Rule::BlockDocRaw => line_break_len,
                        Rule::ContinuationLfOnly => lf_only,
                    };
                    unescape_str_breaking(inside, line_break, |r, v| push(r, v.map(String::from)));
                }
                L::ByteStr { .. } => unescape_byte_str(inside, |r, v| push(r, v.map(|b| format!("{b}")))),
                L::CStr { .. } => unescape_c_str(inside, |r, v| push(r, v.map(|u| format!("{u:?}")))),
                L::RawStr { .. } => check_raw_str(inside, |r, v| push(r, v.map(String::from))),
                L::RawByteStr { .. } => check_raw_byte_str(inside, |r, v| push(r, v.map(|b| format!("{b}")))),
                L::RawCStr { .. } => check_raw_c_str(inside, |r, v| push(r, v.map(|c| format!("{c:?}")))),
                L::Char { .. } => push(0..inside.len(), unescape_char(inside).map(String::from)),
                L::Byte { .. } => push(0..inside.len(), unescape_byte(inside).map(|b| format!("{b}"))),
                L::Int { .. } | L::Float { .. } => {}
            }
        }
        _ if kind == SyntaxKind::INNER_DOC_COMMENT || kind == SyntaxKind::OUTER_DOC_COMMENT => {
            let followed_by_lf = text.as_bytes().get(start + token.len()) == Some(&b'\n');
            match rule {
                Rule::BlockDocRaw if token.starts_with("/*") => out.push(token[3..token.len() - 2].to_owned()),
                _ => out.push(doc_comment_value(token, followed_by_lf).expect("a doc comment").collect()),
            }
        }
        _ => {}
    }
    out
}

/// What the parser's lexer makes of `text`, every position normalised.
fn seen(text: &str, rule: Rule) -> Vec<Seen> {
    let map = offset_map(text);
    let lexed = LexedStr::new(Edition::Edition2024, text);
    (0..lexed.len())
        .map(|i| {
            let range = lexed.text_range(i);
            let kind = lexed.kind(i);
            Seen {
                kind,
                range: map[range.start]..map[range.end],
                error: lexed.error(i).map(String::from),
                value: value(text, range.start, lexed.text(i), kind, &map, rule),
            }
        })
        .collect()
}

/// A drawn source: its LF form, and the same with each line break CR LF where the tape says.
fn drawn(tape: &Tape) -> (String, String) {
    let shebang = tape.one_in(4, "shebang");
    let frontmatter = match tape.one_in(4, "frontmatter") {
        true => Some(*tape.pick(FRONTMATTERS, "which frontmatter").expect("frontmatters")),
        false => None,
    };
    let pieces = tape.vec(1..=12, "pieces", |tape| *tape.pick(PIECES, "piece").expect("pieces"));
    let mut lf = String::new();
    if shebang {
        lf.push_str("#!/usr/bin/env run\n");
    }
    lf.push_str(frontmatter.unwrap_or(""));
    lf.push_str(&pieces.join(" "));
    let mut mixed = String::with_capacity(lf.len() * 2);
    for c in lf.chars() {
        if c == '\n' && !tape.one_in(4, "this break stays LF") {
            mixed.push('\r');
        }
        mixed.push(c);
    }
    (lf, mixed)
}

/// The property: the CR LF form's tokens, errors and values equal the LF form's, and its
/// shebang ends at the same normalised position.
fn same(tape: &Tape, rule: Rule) -> Result<(), Broke> {
    let (lf, mixed) = drawn(tape);
    let (want, got) = (seen(&lf, rule), seen(&mixed, rule));
    if want != got {
        let at = want.iter().zip(&got).position(|(a, b)| a != b).unwrap_or(want.len().min(got.len()));
        return Err(Broke::Oracle(format!(
            "the CR LF form lexes otherwise than the LF form at token {at}: {:?} against {:?}\nLF: {lf:?}\nCR LF: {mixed:?}",
            want.get(at),
            got.get(at)
        )));
    }
    let map = offset_map(&mixed);
    let shebang = rustc_lexer::strip_shebang(&mixed).map(|end| map[end]);
    if shebang != rustc_lexer::strip_shebang(&lf) {
        return Err(Broke::Oracle(format!("the shebang ends otherwise: {mixed:?}")));
    }
    Ok(())
}

/// The refinement: any source with CRLF line breaks (including line continuations and block doc comments) lexes to the exact same tokens, errors, and values as its LF form with mapped offsets.
#[test]
fn crlf_lexes_as_its_lf_form() {
    blaze_sim::check(Config::cases(2000), |tape| same(tape, Rule::Clean));
}

#[test]
fn the_oracle_catches_a_continuation_that_takes_only_lf() {
    blaze_sim::control(Config::cases(2000), Rule::Clean, Rule::ContinuationLfOnly, "lexes otherwise", |tape, rule| {
        same(tape, rule)
    });
}

#[test]
fn the_oracle_catches_a_block_doc_comment_that_keeps_its_crs() {
    blaze_sim::control(Config::cases(2000), Rule::Clean, Rule::BlockDocRaw, "lexes otherwise", |tape, rule| {
        same(tape, rule)
    });
}

/// rustc_lexer, which never meets a CR in rustc (rustc normalises first), calls the CR of a
/// CR LF after an opening fence an infostring; the parser's frontmatter (crate::frontmatter)
/// reads it as the break it is, so the CR LF form lexes as the LF form with no error.
#[test]
fn a_frontmatter_fence_before_cr_lf_is_no_error() {
    let text = "---\r\n[package]\r\n---\r\nfn f() {}\r\n";
    let token = rustc_lexer::tokenize(text, rustc_lexer::FrontmatterAllowed::Yes).next().expect("a token");
    assert!(
        matches!(token.kind, rustc_lexer::TokenKind::Frontmatter { invalid_infostring: true, .. }),
        "rustc_lexer's flag, which the parser does not read: {token:?}"
    );
    assert!(lexed(text).iter().all(|(_, e)| e.is_none()), "{:?}", lexed(text));
    assert_eq!(lexed(text)[0].0, SyntaxKind::FRONTMATTER);
    assert_eq!(seen(text, Rule::Clean), seen(&text.replace("\r\n", "\n"), Rule::Clean));
}

/// The plant shrinks to the least source that shows it: one continuation before one CR LF.
#[test]
fn the_plant_shrinks_to_one_continuation() {
    let verdict = blaze_sim::search(Config::cases(2000), |tape| same(tape, Rule::ContinuationLfOnly));
    let blaze_sim::Verdict::Failed(failed) = &verdict else { panic!("the plant was not caught: {verdict:?}") };
    let text = &failed.failure.text;
    assert!(text.ends_with("LF: \"\\\"x\\\\\\n   y\\\"\"\nCR LF: \"\\\"x\\\\\\r\\n   y\\\"\""), "shrunk to: {text}");
}

/// The errors and kinds of `text` as the parser's lexer gives them.
fn lexed(text: &str) -> Vec<(SyntaxKind, Option<String>)> {
    let lexed = LexedStr::new(Edition::Edition2024, text);
    (0..lexed.len()).map(|i| (lexed.kind(i), lexed.error(i).map(String::from))).collect()
}

#[test]
fn a_continuation_before_cr_lf_is_no_error() {
    // crossterm-0.29.0/src/macros.rs:341's form.
    let text = "let s = \"a \\\r\n    b\";\r\n";
    assert!(lexed(text).iter().all(|(_, e)| e.is_none()), "{:?}", lexed(text));
    let mut value = String::new();
    unescape_str_breaking("a \\\r\n    b", line_break_len, |_, c| value.push(c.expect("no error")));
    assert_eq!(value, "a b");
}

#[test]
fn a_bare_cr_in_a_string_is_an_error_named_as_such() {
    let errors: Vec<_> = lexed("\"a\rb\"").into_iter().filter_map(|(_, e)| e).collect();
    assert_eq!(errors, ["bare CR not allowed in string, use `\\r` instead"]);
    let errors: Vec<_> = lexed("r\"a\rb\"").into_iter().filter_map(|(_, e)| e).collect();
    assert_eq!(errors, ["bare CR not allowed in raw string"]);
}

#[test]
fn a_raw_string_with_cr_lf_holds_lf() {
    let mut value = String::new();
    check_raw_str("a\r\nb", |r, c| {
        if c == Ok('\n') {
            assert_eq!(r, 1..3, "the break's value comes from both its bytes");
        }
        value.push(c.expect("no error"));
    });
    assert_eq!(value, "a\nb");
    assert!(lexed("r\"a\r\nb\"").iter().all(|(_, e)| e.is_none()));
}

#[test]
fn a_doc_comment_ending_in_cr_lf_has_no_cr() {
    let text = "/// doc\r\nfn f() {}\r\n";
    assert!(lexed(text).iter().all(|(_, e)| e.is_none()), "{:?}", lexed(text));
    let lexed = LexedStr::new(Edition::Edition2024, text);
    assert_eq!(lexed.kind(0), SyntaxKind::OUTER_DOC_COMMENT);
    assert_eq!(lexed.text(0), "/// doc\r", "positions stay raw: the token ends before the LF");
    assert_eq!(line_comment_text(lexed.text(0), true), "/// doc");
    // A CR the comment ends with at the end of the text is bare.
    assert_eq!(line_comment_text("/// doc\r", false), "/// doc\r");
    let errors: Vec<_> = self::lexed("/// a\rb\n").into_iter().filter_map(|(_, e)| e).collect();
    assert_eq!(errors, ["bare CR not allowed in doc-comment"]);
}

#[test]
fn a_file_mixing_both_endings_lexes_as_its_lf_form() {
    let mixed = "//! m\r\nconst A: &str = \"x\\\n  y\";\r\nconst B: &str = \"x\\\r\n  y\";\n/** b\r\n */\r\n";
    let lf = mixed.replace("\r\n", "\n");
    assert_eq!(seen(mixed, Rule::Clean), seen(&lf, Rule::Clean));
    assert!(lexed(mixed).iter().all(|(_, e)| e.is_none()), "{:?}", lexed(mixed));
}

#[test]
fn the_rule_s_pieces() {
    assert_eq!(line_break_len(b"\n"), Some(1));
    assert_eq!(line_break_len(b"\r\nx"), Some(2));
    assert_eq!(line_break_len(b"\rx"), None);
    assert!(is_bare_cr(b"\r") && is_bare_cr(b"\rx") && !is_bare_cr(b"\r\n") && !is_bare_cr(b"x"));
    assert!(!has_bare_cr(b"a\r\n", 0..2), "a CR at the range's end looks at the byte after it");
    assert!(has_bare_cr(b"a\rb", 0..3));
    assert_eq!(without_break_crs("a\r\nb\rc\r\n").collect::<String>(), "a\nb\rc\n");
}
