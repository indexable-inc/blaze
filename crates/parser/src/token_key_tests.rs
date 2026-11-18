//! The token key's promises (docs/v2/plan/design/token-keys.md): the twelve edits of the spec's
//! table each move [`token_digest`] or not as the table says; re-formatting a text three ways
//! (every whitespace run made one space, one newline, or a comment) moves no digest, over this
//! crate's own sources and ten samples; a [`TokenPlace`] round-trips for every token and, after
//! a re-format, maps to the re-formatted token's bytes; the line map moves when a line break is
//! added inside an item and not when one is added above it.

use std::{
    fs,
    path::{Path, PathBuf},
};

use blaze_ids::ContentId;

use super::{PlaceError, TokenPlace, adjacency_digest, key_tokens, line_map_digest, token_digest};
use crate::{Edition, LexedStr, SyntaxKind};

const EDITION: Edition = Edition::CURRENT;

fn digest(text: &str) -> ContentId {
    token_digest(text, EDITION).expect("a test's text fits")
}

fn lines(text: &str) -> ContentId {
    line_map_digest(text, EDITION).expect("a test's text fits")
}

fn adjacency(text: &str) -> ContentId {
    adjacency_digest(text, EDITION).expect("a test's text fits")
}

fn moved(before: &str, after: &str) -> bool {
    digest(before) != digest(after)
}

/// `text` with every whitespace token replaced by `with`; a whitespace token after a line
/// comment (`//`, `///`, `//!`) keeps a line break first, or the comment would run on.
fn reformat(text: &str, with: &str) -> String {
    let lexed = LexedStr::new(EDITION, text);
    let mut out = String::with_capacity(text.len() * 2);
    let (mut at, mut after_line_comment) = (0usize, false);
    for (kind, len) in lexed.kinds_and_lens() {
        let lexeme = &text[at..at + len];
        at += len;
        if kind == SyntaxKind::WHITESPACE {
            if after_line_comment {
                out.push('\n');
            }
            out.push_str(with);
        } else {
            out.push_str(lexeme);
        }
        after_line_comment = is_comment(kind) && lexeme.starts_with("//");
    }
    out
}

/// Whether `kind` is a comment's, plain or doc: the lexer gives doc comments their own kinds,
/// so `COMMENT` alone missed `///` and `//!` lines (the property's first failure: cook.rs's
/// `//!` header glued into one doc comment by a space).
fn is_comment(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::COMMENT | SyntaxKind::INNER_DOC_COMMENT | SyntaxKind::OUTER_DOC_COMMENT)
}

/// Where the tokens of `before` and `after` first differ, for a failing assertion: the index,
/// then each side's token (kind, lexeme, joined) and the 80 bytes around it.
fn first_difference(before: &str, after: &str) -> String {
    fn tokens(text: &str) -> Vec<(usize, SyntaxKind, String, bool)> {
        let mut out = Vec::new();
        key_tokens(text, EDITION, |token| out.push((token.start, token.kind, String::from(token.lexeme), token.joined)));
        out
    }
    fn around(text: &str, at: usize) -> &str {
        let mut lo = at.saturating_sub(40);
        while !text.is_char_boundary(lo) {
            lo -= 1;
        }
        let mut hi = (at + 40).min(text.len());
        while !text.is_char_boundary(hi) {
            hi += 1;
        }
        &text[lo..hi]
    }
    let (a, b) = (tokens(before), tokens(after));
    let Some(i) = (0..a.len().max(b.len())).find(|&i| a.get(i).map(|t| (t.1, &t.2, t.3)) != b.get(i).map(|t| (t.1, &t.2, t.3))) else {
        return String::from("no token differs (the digests differ by the edition or the encoding)");
    };
    let side = |tokens: &[(usize, SyntaxKind, String, bool)], text: &str| match tokens.get(i) {
        Some((start, kind, lexeme, joined)) => {
            std::format!("{kind:?} {lexeme:?} joined {joined}, around: {:?}", around(text, *start))
        }
        None => String::from("no such token"),
    };
    std::format!("first differing token {i}: before {}; after {}", side(&a, before), side(&b, after))
}

/// The three re-formattings of the property: one space, one newline, a comment between spaces
/// (the spaces keep `/` `*` from becoming `//` or `*/`).
const FORMATS: [&str; 3] = [" ", "\n", " /* x */ "];

/// Each counted token's byte range in `text`, in order.
fn token_ranges(text: &str) -> Vec<core::ops::Range<usize>> {
    let mut out = Vec::new();
    key_tokens(text, EDITION, |token| out.push(token.start..token.start + token.lexeme.len()));
    out
}

/// Ten item texts with the shapes a token key must get right: generics closed by `>>`,
/// lifetimes, raw strings, byte strings, nested block comments, doc comments of all four
/// kinds, a `macro_rules!` body, a float `1.` before a method call, `r#` identifiers, compound
/// operators and attributes.
const SAMPLES: [&str; 10] = [
    "fn f<T: Iterator<Item = Vec<u8>>>(x: T) -> Option<Vec<Vec<u8>>> { None }",
    "struct S<'a, 'b: 'a> { x: &'a str, y: &'b mut [u8], z: &'static dyn Fn(&'a u8) -> u8 }",
    "const R: &str = r#\"raw \"quoted\" string\"#; const Q: &str = r##\"a \"# b\"##;",
    "static B: &[u8] = b\"bytes\\x00\\n\"; const C: u8 = b'a'; const D: &core::ffi::CStr = c\"c\";",
    "fn g() { /* outer /* nested */ still a comment */ let x = 1; // line\n x }",
    "/// Outer doc.\n/** Block doc. */\n#[inline]\nfn h() {}\nmod m {\n    //! Inner doc.\n    /*! Inner block. */\n}",
    "macro_rules! m {\n    ($x:expr) => { $x + 1 };\n    ($($t:tt)*) => { $($t)* };\n}",
    "fn k() -> f64 { let a = 1. .max(2.); let b = 2.5.max(1.); let c = x.0.1; a + b + 1e3 }",
    "fn r#try() -> u8 { let r#match = 1_000u32; r#match as u8 }",
    "#[derive(Debug)] #[cfg(test)] impl<T> Tr for W<T> where T: Fn(&u8) -> bool {\n    fn m(&mut self) -> bool { self.0 >>= 2; a && b || !c && x != y }\n}",
];

/// This crate's own `.rs` files: real Rust, about two hundred files.
fn crate_sources() -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs: Vec<PathBuf> = std::vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(dir) = dirs.pop() {
        let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("reading {}: {error}", dir.display()))
            .map(|entry| entry.expect("a directory entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|it| it == "rs") {
                out.push(fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display())));
            }
        }
    }
    assert!(out.len() > 20, "the parser's sources are found ({} files)", out.len());
    out
}

/// The spec's table of twelve edits, each moving the token digest or not as the table says.
#[test]
fn the_twelve_edits_move_as_the_table_says() {
    // 1. Formatting: spaces, line breaks and indentation.
    assert!(!moved("fn f(a:u8)->u8{a+1}", "fn f(a: u8) -> u8 {\n    a + 1\n}"), "formatting");
    // 2. A plain comment added, and one edited.
    assert!(!moved("fn f() {}", "fn f() { // later\n}"), "a plain comment added");
    assert!(!moved("fn f() { // a\n}", "fn f() { // b\n}"), "a plain comment edited");
    // 3. A doc comment edited.
    assert!(moved("/// a\nfn f() {}", "/// b\nfn f() {}"), "a doc comment edited");
    assert!(moved("fn f() {}", "/// a\nfn f() {}"), "a doc comment added");
    // 4. `>>` split.
    assert!(moved("type T = Vec<Vec<u8>>;", "type T = Vec<Vec<u8> >;"), "`>>` split");
    // 5. A literal's spelling.
    assert!(moved("const N: u32 = 1_000;", "const N: u32 = 1000;"), "`1_000` to `1000`");
    // 6. A trailing comma: a token.
    assert!(moved("fn f() { g(a, b) }", "fn f() { g(a, b,) }"), "a trailing comma");
    // 7. Parentheses: tokens.
    assert!(moved("fn f() { a + b }", "fn f() { (a + b) }"), "parentheses");
    // 8. CRLF to LF: not between tokens; yes inside a string literal or a doc comment, whose
    //    lexemes keep the `\r`.
    assert!(!moved("fn f() {\r\n    x\r\n}", "fn f() {\n    x\n}"), "CRLF between tokens");
    assert!(moved("fn f() { \"a\r\nb\" }", "fn f() { \"a\nb\" }"), "CRLF inside a string");
    assert!(moved("/// a\r\nfn f() {}", "/// a\nfn f() {}"), "CRLF ending a doc comment");
    // 9. A blank line above an item.
    assert!(!moved("fn f() {}", "\n\nfn f() {}"), "a blank line above");
    // 10. Attributes reordered.
    assert!(moved("#[a]\n#[b]\nfn f() {}", "#[b]\n#[a]\nfn f() {}"), "attributes reordered");
    // 11. Tabs to spaces.
    assert!(!moved("fn f() {\n\tx\n}", "fn f() {\n    x\n}"), "tabs to spaces");
    // 12. A block comment inside an expression.
    assert!(!moved("fn f() { a + b }", "fn f() { a /* why */ + b }"), "a `/* */` comment in an expression");
}

/// Joined punctuation and lexemes the language tells apart move the digest; the edition is in it.
#[test]
fn joints_lexemes_and_the_edition_count() {
    assert!(moved("fn f() { a - -b }", "fn f() { a --b }"), "`- -` and `--`");
    assert!(moved("fn f() { a && b }", "fn f() { a & & b }"), "`&&` and `& &`");
    assert!(moved("struct S<'a>(&'a u8);", "struct S<'a>(& 'a u8);"), "`&'a` and `& 'a`");
    assert!(moved("fn r#try() {}", "fn try() {}"), "`r#try` and `try`");
    assert!(moved("const S: &str = \"a b\";", "const S: &str = \"a  b\";"), "a string's contents");
    assert!(moved("const N: u64 = 1u64;", "const N: u64 = 1;"), "a literal's suffix");
    assert!(!moved("fn f() {}", "/**/fn f() {}"), "`/**/` is a plain comment");
    assert!(!moved("fn f() {}", "/***/fn f() {}"), "`/***/` is a plain comment");
    assert!(!moved("fn f() {}", "////\nfn f() {}"), "`////` is a plain comment");
    assert!(moved("fn f() {}", "/*! a */fn f() {}"), "`/*!` is a doc comment");
    // Only punctuation against punctuation (or a lifetime) is joined: spacing elsewhere is layout.
    assert!(!moved("fn f() { a + b }", "fn f() { a+b }"), "`+` against an identifier is not joined");
    assert!(!moved("fn f() { g(x)(y) }", "fn f() { g (x) (y) }"), "delimiters are never joined");
    let text = "async fn f() {}";
    assert_ne!(
        token_digest(text, Edition::Edition2015),
        token_digest(text, Edition::Edition2024),
        "one text in two editions is two programs"
    );
}

/// Every whitespace run made one space, one newline or a comment moves no token digest, over
/// this crate's sources and the samples.
#[test]
fn reformatting_three_ways_moves_no_digest() {
    let mut texts = crate_sources();
    texts.extend(SAMPLES.iter().map(|it| String::from(*it)));
    for (at, text) in texts.iter().enumerate() {
        let (before, spacing) = (digest(text), adjacency(text));
        for with in FORMATS {
            let after = reformat(text, with);
            if digest(&after) != before {
                panic!("text {at} re-formatted with {with:?} moved its token digest: {}", first_difference(text, &after));
            }
            // Every whitespace run is still trivia between the same two tokens.
            assert_eq!(adjacency(&after), spacing, "text {at} re-formatted with {with:?}: adjacency");
        }
    }
}

/// The property's first failure, alone: a `//!` header's line break replaced by a space glued
/// its lines into one doc comment, a different program (the doc text grew). `reformat` keeps a
/// break after a doc line comment, and the digest tells the glued text apart.
#[test]
fn a_doc_line_comment_keeps_its_line_break() {
    let text = "//! a\n//! b\nfn f() {}";
    for with in FORMATS {
        let after = reformat(text, with);
        assert_eq!(token_ranges(&after).len(), 8, "two doc comments and six tokens of `fn f() {{}}` in {after:?}");
        assert_eq!(digest(&after), digest(text), "re-formatted with {with:?}: {}", first_difference(text, &after));
    }
    assert!(moved(text, "//! a //! b\nfn f() {}"), "one glued doc comment is another program");
    assert!(moved("/// a\n/// b\nfn f() {}", "/// a /// b\nfn f() {}"), "an outer doc comment glued");
}

/// The adjacency moves when spacing between two tokens appears or goes, and not when trivia
/// that already separated them changes; the line map is the other way round.
#[test]
fn adjacency_is_spacing_and_the_line_map_is_lines() {
    assert_ne!(adjacency("fn f() { g(x) }"), adjacency("fn f() { g (x) }"), "`f(x)` to `f (x)`: adjacency");
    assert_eq!(digest("fn f() { g(x) }"), digest("fn f() { g (x) }"), "`f(x)` to `f (x)`: tokens");
    assert_eq!(adjacency("fn f() { a + b }"), adjacency("fn f() {\n    a + b\n}"), "breaks where spaces were: adjacency");
    assert_ne!(lines("fn f() { a + b }"), lines("fn f() {\n    a + b\n}"), "breaks where spaces were: lines");
    assert_eq!(adjacency("fn f() { a + b }"), adjacency("fn f() { a /* c */ + b }"), "a comment where a space was");
    assert_eq!(adjacency("fn f() {}"), adjacency("fn f() {}\n\n"), "trivia after the last token");
    assert_ne!(adjacency("fn f() { a+b }"), adjacency("fn f() { a + b }"), "`a+b` and `a + b`");
}

/// Each token's place round-trips; so do a whole run of tokens and a stretch inside one; and
/// after a re-format each place maps to the re-formatted token's bytes, which spell the same.
#[test]
fn token_places_round_trip_and_survive_reformatting() {
    for (sample, text) in SAMPLES.iter().enumerate() {
        let ranges = token_ranges(text);
        assert!(ranges.len() > 3, "sample {sample} has tokens");
        for (i, range) in ranges.iter().enumerate() {
            let index = u32::try_from(i).expect("a sample's tokens fit a u32");
            let place = TokenPlace::from_bytes(text, EDITION, range.clone()).expect("a token's bytes have a place");
            assert_eq!(place, TokenPlace { first: index, last: index, inside: None }, "sample {sample} token {i}");
            assert_eq!(place.to_bytes(text, EDITION), Ok(range.clone()), "sample {sample} token {i}");
            if range.len() >= 2 {
                let inner = range.start + 1..range.end;
                let place = TokenPlace::from_bytes(text, EDITION, inner.clone()).expect("a stretch inside a token");
                assert_eq!(place.inside, Some((1, u32::try_from(range.len()).expect("fits"))), "sample {sample} token {i}");
                assert_eq!(place.to_bytes(text, EDITION), Ok(inner), "sample {sample} token {i}");
            }
        }
        let whole = ranges[0].start..ranges[ranges.len() - 1].end;
        let place = TokenPlace::from_bytes(text, EDITION, whole.clone()).expect("the whole item");
        let last = u32::try_from(ranges.len() - 1).expect("fits");
        assert_eq!(place, TokenPlace { first: 0, last, inside: None });
        assert_eq!(place.to_bytes(text, EDITION), Ok(whole));
        for with in FORMATS {
            let after = reformat(text, with);
            let moved_ranges = token_ranges(&after);
            assert_eq!(moved_ranges.len(), ranges.len(), "sample {sample} re-formatted with {with:?}");
            for (i, (range, moved_range)) in ranges.iter().zip(&moved_ranges).enumerate() {
                let place = TokenPlace::from_bytes(text, EDITION, range.clone()).expect("a token's place");
                assert_eq!(place.to_bytes(&after, EDITION), Ok(moved_range.clone()), "sample {sample} token {i} with {with:?}");
                assert_eq!(&after[moved_range.clone()], &text[range.clone()], "sample {sample} token {i} with {with:?}");
            }
        }
    }
}

/// A place the text does not have, or bytes no place names, is a typed refusal, never clamped.
#[test]
fn places_out_of_reach_are_refused() {
    let text = "fn  f() {}";
    let tokens = u32::try_from(token_ranges(text).len()).expect("fits");
    assert_eq!(tokens, 6, "`fn`, `f`, `(`, `)`, `{{` and `}}`");
    assert_eq!(
        TokenPlace { first: 0, last: tokens, inside: None }.to_bytes(text, EDITION),
        Err(PlaceError::PastEnd { last: tokens, tokens })
    );
    assert_eq!(
        TokenPlace { first: 2, last: 1, inside: None }.to_bytes(text, EDITION),
        Err(PlaceError::Backwards { first: 2, last: 1 })
    );
    assert_eq!(
        TokenPlace { first: 0, last: 1, inside: Some((0, 1)) }.to_bytes(text, EDITION),
        Err(PlaceError::InsideSpansTokens { first: 0, last: 1 })
    );
    assert_eq!(
        TokenPlace { first: 0, last: 0, inside: Some((0, 3)) }.to_bytes(text, EDITION),
        Err(PlaceError::InsidePastLexeme { token: 0, inside: (0, 3), lexeme: 2 })
    );
    assert_eq!(TokenPlace::from_bytes(text, EDITION, 2..3), Err(PlaceError::InTrivia { at: 2 }));
    assert_eq!(TokenPlace::from_bytes(text, EDITION, 1..5), Err(PlaceError::NotTokenAligned { at: 1 }));
    assert_eq!(
        TokenPlace::from_bytes(text, EDITION, 3..11),
        Err(PlaceError::RangeOutside { start: 3, end: 11, text: text.len() })
    );
    // An empty range at a token's end is a place inside it; one in whitespace is refused.
    assert_eq!(
        TokenPlace::from_bytes(text, EDITION, 2..2),
        Ok(TokenPlace { first: 0, last: 0, inside: Some((2, 2)) })
    );
    assert_eq!(TokenPlace::from_bytes(text, EDITION, 3..3), Err(PlaceError::InTrivia { at: 3 }));
}

/// The line map moves when a line break is added inside the item or a column changes, and not
/// when a line is added above it or a line ends in CRLF instead of LF.
#[test]
fn the_line_map_moves_inside_the_item_only() {
    let item = "fn f() { a }";
    assert_ne!(lines(item), lines("fn f() {\n a }"), "a line break added inside");
    assert_eq!(lines(item), lines("\nfn f() { a }"), "a line added above");
    assert_eq!(lines(item), lines("\n\n\n// above\nfn f() { a }"), "lines and a comment above");
    assert_ne!(lines(item), lines("fn f() {  a }"), "a column changed");
    assert_eq!(lines("fn f() {\r\n    x\r\n}"), lines("fn f() {\n    x\n}"), "CRLF and LF");
    assert_ne!(lines("fn f() {\n\tx\n}"), lines("fn f() {\n    x\n}"), "a tab and four spaces");
    assert_eq!(digest("fn f() {\n\tx\n}"), digest("fn f() {\n    x\n}"), "while the tokens stay");
}
