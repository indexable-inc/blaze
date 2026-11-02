//! Expected trees are what rustc's `ts_from_str` gives a client at the ported commit
//! (`lex_token_trees`' spacing, then `FromInternal`'s one punctuation mark per character),
//! expected messages the primary messages of rustc's diagnostics there
//! (`rustc_parse::diagnostics`, `lexer/mod.rs`, `proc_macro_server.rs`).

use span::{EditionedFileId, SpanAnchor, SyntaxContext, TextRange};

use super::*;
use crate::stream::IdentKind;

fn site() -> Span {
    let file_id = EditionedFileId::new(span::FileId::from_raw(5), Edition::CURRENT);
    Span {
        range: TextRange::new(10.into(), 14.into()),
        anchor: SpanAnchor { file_id, ast_id: span::ROOT_ERASED_FILE_AST_ID },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

fn lex_in(text: &str, edition: Edition) -> (Result<Stream, String>, Vec<String>) {
    let mut emitted = Vec::new();
    let stream = stream(text, site(), edition, false, &mut emitted, &mut Vec::new());
    (stream, emitted)
}

fn lex(text: &str) -> Stream {
    let (stream, emitted) = lex_in(text, Edition::Edition2021);
    assert!(emitted.is_empty(), "lexing {text:?} reported {emitted:?}");
    stream.expect("lexes")
}

/// Each tree as text and its spacing: `A` alone, `J` joint, `H` joint hidden.
fn shape(trees: &[Tree]) -> Vec<String> {
    let mark = |spacing: Spacing| match spacing {
        Spacing::Alone => 'A',
        Spacing::Joint => 'J',
        Spacing::JointHidden => 'H',
    };
    trees
        .iter()
        .map(|tree| match tree {
            Tree::Punct { ch, spacing, .. } => format!("{}{}", *ch as char, mark(*spacing)),
            Tree::Ident { sym, kind, spacing, .. } => {
                let prefix = match kind {
                    IdentKind::Normal => "",
                    IdentKind::Raw => "r#",
                    IdentKind::ForcedKeyword => "k#",
                };
                format!("{prefix}{}{}", sym.as_str(), mark(*spacing))
            }
            Tree::Literal { kind, symbol, suffix, spacing, .. } => {
                let mut text = String::new();
                crate::print::literal_text(*kind, symbol.as_str(), *suffix, &mut text);
                format!("{text}{}", mark(*spacing))
            }
            Tree::DocComment { data, spacing, .. } => format!("doc{:?}{}", data.as_str(), mark(*spacing)),
            Tree::Group { delimiter, stream, open, spacing, .. } => format!(
                "{delimiter:?}{}[{}]{}",
                mark(*open),
                shape(stream.trees()).join(" "),
                mark(*spacing)
            ),
        })
        .collect()
}

#[test]
fn spacing_is_the_lexers() {
    assert_eq!(shape(lex("a::b").trees()), ["aJ", ":J", ":H", "bA"]);
    assert_eq!(shape(lex("a :: b").trees()), ["aA", ":J", ":A", "bA"]);
    assert_eq!(shape(lex("x.y()").trees()), ["xJ", ".H", "yH", "ParenthesisH[]A"]);
    assert_eq!(shape(lex("#[attr]").trees()), ["#H", "BracketH[attrH]A"]);
    assert_eq!(shape(lex("&mut x").trees()), ["&H", "mutA", "xA"]);
    assert_eq!(shape(lex("$x").trees()), ["$H", "xA"]);
    assert_eq!(shape(lex("a+=-1").trees()), ["aJ", "+J", "=J", "-H", "1A"]);
    assert_eq!(shape(lex("{ x }").trees()), ["BraceA[xA]A"]);
}

#[test]
fn a_lifetime_is_a_joint_quote_and_its_name() {
    assert_eq!(shape(lex("'a").trees()), ["'J", "aA"]);
    assert_eq!(shape(lex("&'a T").trees()), ["&H", "'J", "aA", "TA"]);
    assert_eq!(shape(lex("'r#a").trees()), ["'J", "r#aA"]);
    // Before 2021 `'r#a` is the lifetime `'r`, then `#a`.
    let (before, emitted) = lex_in("'r#a", Edition::Edition2018);
    assert!(emitted.is_empty());
    assert_eq!(shape(before.unwrap().trees()), ["'J", "rJ", "#H", "aA"]);
}

#[test]
fn raw_and_forced_keyword_idents() {
    assert_eq!(shape(lex("r#type").trees()), ["r#typeA"]);
    let (forced, emitted) = lex_in("k#fn", Edition::Edition2021);
    assert_eq!(shape(forced.unwrap().trees()), ["k#fnA"]);
    assert_eq!(emitted, ["forced keywords are experimental"]);
    // A direction-changing codepoint is a warning, not an error.
    let (mut emitted, mut warned) = (Vec::new(), Vec::new());
    let bidi = stream("\"a\u{202E}b\"", site(), Edition::Edition2021, false, &mut emitted, &mut warned);
    assert_eq!(shape(bidi.unwrap().trees()).len(), 1);
    assert!(emitted.is_empty(), "{emitted:?}");
    assert_eq!(warned.len(), 1, "{warned:?}");
    // A crate that enables the feature gets no error.
    let mut emitted = Vec::new();
    let enabled = stream("k#fn", site(), Edition::Edition2021, true, &mut emitted, &mut Vec::new());
    assert_eq!(shape(enabled.unwrap().trees()), ["k#fnA"]);
    assert!(emitted.is_empty());
    // Before 2021 it is `k`, `#`, `fn`.
    let (before, emitted) = lex_in("k#fn", Edition::Edition2018);
    assert!(emitted.is_empty());
    assert_eq!(shape(before.unwrap().trees()), ["kJ", "#H", "fnA"]);
    let (_, emitted) = lex_in("r#self", Edition::Edition2021);
    assert_eq!(emitted, ["`self` cannot be a raw identifier"]);
}

#[test]
fn every_literal_kind() {
    let stream = lex("'a' b'a' \"s\" r#\"s\"# b\"s\" br##\"s\"## c\"s\" cr#\"s\"# 1u8 1.5e3f64 0x1F");
    assert_eq!(
        shape(stream.trees()),
        [
            "'a'A", "b'a'A", "\"s\"A", "r#\"s\"#A", "b\"s\"A", "br##\"s\"##A", "c\"s\"A", "cr#\"s\"#A",
            "1u8A", "1.5e3f64A", "0x1FA",
        ]
    );
    let kinds: Vec<LitKind> = stream
        .trees()
        .iter()
        .map(|tree| match tree {
            Tree::Literal { kind, .. } => *kind,
            other => panic!("not a literal: {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            LitKind::Char,
            LitKind::Byte,
            LitKind::Str,
            LitKind::StrRaw(1),
            LitKind::ByteStr,
            LitKind::ByteStrRaw(2),
            LitKind::CStr,
            LitKind::CStrRaw(1),
            LitKind::Integer,
            LitKind::Float,
            LitKind::Integer,
        ]
    );
    // Before 2021 `c"s"` is the identifier `c` and a string.
    let (before, _) = lex_in("c\"s\"", Edition::Edition2018);
    assert_eq!(shape(before.unwrap().trees()), ["cH", "\"s\"A"]);
}

#[test]
fn every_token_is_at_the_call_site() {
    let span = site();
    let stream = lex("a(b)");
    let [Tree::Ident { span: a, .. }, Tree::Group { span: group, stream: inner, .. }] = stream.trees() else {
        panic!("{:?}", stream.trees());
    };
    assert_eq!(*a, span);
    assert_eq!(*group, DelimSpan { open: span, close: span, entire: span });
    let [Tree::Ident { span: b, .. }] = inner.trees() else { panic!("{:?}", inner.trees()) };
    assert_eq!(*b, span);
}

#[test]
fn an_operator_as_long_as_the_call_site_gets_a_byte_of_it_per_character() {
    let call_site = Span { range: TextRange::new(20.into(), 22.into()), ..site() };
    let mut emitted = Vec::new();
    let path = stream("a::b", call_site, Edition::Edition2021, false, &mut emitted, &mut Vec::new()).unwrap();
    let spans: Vec<TextRange> = path
        .trees()
        .iter()
        .map(|tree| match tree {
            Tree::Punct { span, .. } | Tree::Ident { span, .. } => span.range,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        spans,
        [
            TextRange::new(20.into(), 22.into()),
            TextRange::new(20.into(), 21.into()),
            TextRange::new(21.into(), 22.into()),
            TextRange::new(20.into(), 22.into()),
        ]
    );
    // An operator of another length keeps the whole span.
    let range = stream("..=", call_site, Edition::Edition2021, false, &mut emitted, &mut Vec::new()).unwrap();
    assert!(range.trees().iter().all(|tree| matches!(tree, Tree::Punct { span, .. } if span.range == call_site.range)));
}

#[test]
fn doc_comments_are_kept_as_one_token() {
    let stream = lex("/// outer\n//! inner\n/** block */ /*! inner block */ // plain\n/* plain */");
    let docs: Vec<(DocKind, bool, &str)> = stream
        .trees()
        .iter()
        .map(|tree| match tree {
            Tree::DocComment { kind, inner, data, .. } => (*kind, *inner, data.as_str()),
            other => panic!("not a doc comment: {other:?}"),
        })
        .collect();
    assert_eq!(
        docs,
        [
            (DocKind::Line, false, " outer"),
            (DocKind::Line, true, " inner"),
            (DocKind::Block, false, " block "),
            (DocKind::Block, true, " inner block "),
        ]
    );
    // `////` and `/***/` are not doc comments.
    assert!(lex("//// x\n/***/").is_empty());
}

#[test]
fn a_doc_comment_is_taken_apart_as_rustc_gives_it_to_a_client() {
    let span = site();
    let mut out = Vec::new();
    desugar_doc(DocKind::Block, true, &Symbol::intern(" a\n\"b\" it's\t"), span, &mut out);
    assert_eq!(shape(&out), ["#A", "!A", "BracketA[docA =A \" a\\n\\\"b\\\" it\\'s\\t\"A]A"]);
    let mut out = Vec::new();
    desugar_doc(DocKind::Line, false, &Symbol::intern(" x"), span, &mut out);
    assert_eq!(shape(&out), ["#A", "BracketA[docA =A \" x\"A]A"]);
}

#[test]
fn text_is_normalized_as_a_source_file() {
    let stream = lex("\u{feff}/// x\r\ny");
    assert_eq!(shape(stream.trees()), ["doc\" x\"A", "yA"]);
    // A shebang is stripped, an inner attribute is not.
    assert_eq!(shape(lex("#!/bin/sh\nx").trees()), ["xA"]);
    assert_eq!(shape(lex("#![a]").trees()), ["#J", "!H", "BracketH[aH]A"]);
}

#[test]
fn delimiter_errors_are_the_clients_err() {
    let err = |text: &str| lex_in(text, Edition::Edition2021).0.unwrap_err();
    assert_eq!(err("("), "this file contains an unclosed delimiter");
    assert_eq!(err("a { b"), "this file contains an unclosed delimiter");
    assert_eq!(err(")"), "unexpected closing delimiter: `)`");
    assert_eq!(err("(]"), "mismatched closing delimiter: `]`");
    assert_eq!(err("{ ( }"), "mismatched closing delimiter: `}`");
    assert_eq!(err("[ ( } ]"), "mismatched closing delimiter: `}`");
}

#[test]
fn a_fatal_error_fails_the_parse_and_is_emitted() {
    let (stream, emitted) = lex_in("\"abc", Edition::Edition2021);
    assert_eq!(stream.unwrap_err(), "failed to parse to tokenstream");
    assert_eq!(emitted, ["unterminated double quote string"]);
    let (stream, emitted) = lex_in("x /* y", Edition::Edition2021);
    assert_eq!(stream.unwrap_err(), "failed to parse to tokenstream");
    assert_eq!(emitted, ["unterminated block comment"]);
    let (stream, emitted) = lex_in("r##\"x\"#", Edition::Edition2021);
    assert_eq!(stream.unwrap_err(), "failed to parse to tokenstream");
    assert_eq!(emitted, ["unterminated raw string"]);
}

#[test]
fn other_errors_are_emitted_and_the_stream_is_given() {
    let (stream, emitted) = lex_in("'ab'", Edition::Edition2021);
    assert_eq!(shape(stream.unwrap().trees()), ["'ab'A"]);
    assert_eq!(emitted, ["character literal may only contain one codepoint"]);

    let (_, emitted) = lex_in("\"\\q\"", Edition::Edition2021);
    assert_eq!(emitted, ["unknown character escape: `q`"]);

    let (_, emitted) = lex_in("0b102", Edition::Edition2021);
    assert_eq!(emitted, ["invalid digit for a base 2 literal"]);

    let (stream, emitted) = lex_in("a \u{a1} b", Edition::Edition2021);
    assert_eq!(shape(stream.unwrap().trees()), ["aA", "bA"]);
    assert_eq!(emitted, ["unknown start of token: \\u{a1}"]);

    // A confusable is lexed as what it looks like.
    let (stream, emitted) = lex_in("a \u{2212} b", Edition::Edition2021);
    assert_eq!(shape(stream.unwrap().trees()), ["aA", "-A", "bA"]);
    assert_eq!(emitted, ["unknown start of token: \\u{2212}"]);

    let (_, emitted) = lex_in("foo#bar", Edition::Edition2021);
    assert_eq!(emitted, ["prefix `foo` is unknown"]);
    let (_, emitted) = lex_in("foo#bar", Edition::Edition2018);
    assert!(emitted.is_empty());

    let (_, emitted) = lex_in("#\"x\"#", Edition::Edition2024);
    assert_eq!(emitted, ["invalid string literal"]);
    let (_, emitted) = lex_in("##", Edition::Edition2024);
    assert_eq!(emitted, ["reserved multi-hash token is forbidden"]);

    // Once per identifier, as rustc reports them after expansion.
    let (_, emitted) = lex_in("\u{1f980} \u{1f980}", Edition::Edition2021);
    assert_eq!(emitted, ["Ferris cannot be used as an identifier"]);
}

#[test]
fn emitted_keeps_what_was_there() {
    let mut emitted = vec!["earlier".to_owned()];
    let _ = stream("'ab'", site(), Edition::Edition2021, false, &mut emitted, &mut Vec::new());
    assert_eq!(emitted, ["earlier", "character literal may only contain one codepoint"]);
}

fn literal_of(text: &str) -> Result<(LitKind, String, Option<String>), String> {
    let mut emitted = Vec::new();
    let tree = literal(text, site(), Edition::Edition2021, false, &mut emitted, &mut Vec::new())?;
    let Tree::Literal { kind, symbol, suffix, span, spacing } = tree else {
        panic!("not a literal: {tree:?}");
    };
    assert_eq!(span, site());
    assert_eq!(spacing, Spacing::Alone);
    Ok((kind, symbol.as_str().to_owned(), suffix.map(|s| s.as_str().to_owned())))
}

#[test]
fn a_literal_from_text() {
    assert_eq!(literal_of("1"), Ok((LitKind::Integer, "1".to_owned(), None)));
    assert_eq!(literal_of("-1"), Ok((LitKind::Integer, "-1".to_owned(), None)));
    assert_eq!(literal_of("-1.5f32"), Ok((LitKind::Float, "-1.5".to_owned(), Some("f32".to_owned()))));
    assert_eq!(literal_of("\"s\""), Ok((LitKind::Str, "s".to_owned(), None)));
    assert_eq!(literal_of("r#\"s\"#"), Ok((LitKind::StrRaw(1), "s".to_owned(), None)));
    assert_eq!(literal_of("b'x'"), Ok((LitKind::Byte, "x".to_owned(), None)));
}

#[test]
fn a_literal_from_text_refused_as_rustc_refuses_it() {
    let refused = |text: &str| literal_of(text).unwrap_err();
    assert_eq!(refused(""), "not a literal");
    assert_eq!(refused("x"), "not a literal");
    assert_eq!(refused("true"), "not a literal");
    assert_eq!(refused("-"), "not a literal");
    assert_eq!(refused("-x"), "not a literal");
    assert_eq!(refused("(1)"), "not a literal");
    assert_eq!(refused(" 1"), "comment or whitespace around literal");
    assert_eq!(refused("1 "), "comment or whitespace around literal");
    assert_eq!(refused("1 2"), "comment or whitespace around literal");
    assert_eq!(refused("1/**/"), "comment or whitespace around literal");
    assert_eq!(refused("- 1"), "comment or whitespace after minus");
    assert_eq!(refused("-\"s\""), "non-numeric literal may not be negated");
    assert_eq!(refused("-'c'"), "non-numeric literal may not be negated");
    assert_eq!(refused("(1"), "this file contains an unclosed delimiter");
    assert_eq!(refused("\"s"), "failed to parse to literal");
}

#[test]
fn an_ident_is_normalized_then_checked() {
    assert_eq!(ident("foo").map(|s| s.as_str()), Ok("foo"));
    assert_eq!(ident("_").map(|s| s.as_str()), Ok("_"));
    assert_eq!(ident("e\u{301}").map(|s| s.as_str()), Ok("\u{e9}"));
    assert_eq!(ident(""), Err(()));
    assert_eq!(ident("1a"), Err(()));
    assert_eq!(ident("r#foo"), Err(()));
    assert_eq!(ident("a b"), Err(()));
}

#[test]
fn lexing_twice_gives_the_same_trees() {
    let text = "fn f<'a>(x: &'a [u8]) -> Option<&'a u8> { x.first() } /// d\n";
    assert_eq!(shape(lex(text).trees()), shape(lex(text).trees()));
}
