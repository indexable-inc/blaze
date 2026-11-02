//! Expected strings are rustc's `tts_to_string` at the ported commit: for a stream lexed from
//! source, the source's own spacing kept (`print_tts`'s `Alone` rule); for a stream a client
//! built (as `quote!` builds it: every token `Alone` but the first characters of an operator),
//! the spaced form rustc's `TokenStream::to_string` is known for (`a :: b`, `& mut x`,
//! `& 'a T`, `x : u8`, `Vec < u8 >`), each from `space_between`'s rules as commented there.

use std::cell::Cell;

use span::{EditionedFileId, SpanAnchor, SyntaxContext, TextRange};

use super::*;
use crate::lex;
use crate::stream::{DelimSpan, push_given};

fn site() -> Span {
    let file_id = EditionedFileId::new(span::FileId::from_raw(3), Edition::CURRENT);
    Span {
        range: TextRange::new(0.into(), 0.into()),
        anchor: SpanAnchor { file_id, ast_id: span::ROOT_ERASED_FILE_AST_ID },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

fn print_in(trees: &[Tree], edition: Edition) -> String {
    let mut out = String::new();
    to_string(trees, &mut out, |_| edition);
    out
}

fn print(trees: &[Tree]) -> String {
    print_in(trees, Edition::Edition2021)
}

fn lex(text: &str) -> Stream {
    let mut emitted = Vec::new();
    let stream = lex::stream(text, site(), Edition::Edition2021, false, &mut emitted, &mut Vec::new()).expect("lexes");
    assert!(emitted.is_empty(), "lexing {text:?} reported {emitted:?}");
    stream
}

/// `text` printed after lexing it: what `ts_to_string` of `text.parse()` gives.
fn lexed(text: &str) -> String {
    print(lex(text).trees())
}

/// The trees as a client gives them back (rustc's `ToInternal`), groups too.
fn given(trees: &[Tree]) -> Vec<Tree> {
    let mut out = Vec::new();
    for tree in trees {
        let tree = match tree {
            Tree::Group { delimiter, stream, span, open, spacing } => Tree::Group {
                delimiter: *delimiter,
                stream: Stream::of(given(stream.trees())),
                span: *span,
                open: *open,
                spacing: *spacing,
            },
            other => other.clone(),
        };
        push_given(tree, &mut out);
    }
    out
}

/// `text` as `quote!` would build it, printed.
fn quoted(text: &str) -> String {
    print(&given(lex(text).trees()))
}

#[test]
fn paths_keep_the_sources_spacing() {
    assert_eq!(lexed("a::b"), "a::b");
    assert_eq!(lexed("a :: b"), "a :: b");
    assert_eq!(quoted("a::b"), "a :: b");
    assert_eq!(quoted("::std::vec::Vec"), ":: std :: vec :: Vec");
}

#[test]
fn method_calls_and_fields_print_tight() {
    assert_eq!(lexed("x.y()"), "x.y()");
    assert_eq!(quoted("x.y()"), "x.y()");
    assert_eq!(quoted("x . y ()"), "x.y()");
    assert_eq!(quoted("tup.0"), "tup.0");
    assert_eq!(quoted("a, b; c.d"), "a, b; c.d");
}

#[test]
fn attributes_and_references() {
    assert_eq!(lexed("#[attr]"), "#[attr]");
    assert_eq!(quoted("#[attr]"), "#[attr]");
    // The lexer makes `#` `Joint` before the `!`, and a client's `Joint` is kept.
    assert_eq!(quoted("#![attr]"), "#! [attr]");
    assert_eq!(lexed("&mut x"), "&mut x");
    assert_eq!(quoted("&mut x"), "& mut x");
}

#[test]
fn lifetimes_glue_their_quote() {
    assert_eq!(lexed("'a"), "'a");
    assert_eq!(lexed("&'a T"), "&'a T");
    assert_eq!(quoted("&'a T"), "& 'a T");
    assert_eq!(quoted("'static"), "'static");
    assert_eq!(lexed("'r#a"), "'r#a");
}

#[test]
fn dollar_and_dollar_crate() {
    assert_eq!(lexed("$x"), "$x");
    assert_eq!(quoted("$x"), "$x");
    let span = site();
    let dollar_crate = [
        Tree::Ident { sym: Symbol::intern("$crate"), kind: IdentKind::Normal, span, spacing: Spacing::Alone },
        Tree::Punct { ch: b':', spacing: Spacing::Joint, span },
        Tree::Punct { ch: b':', spacing: Spacing::Alone, span },
        Tree::Ident { sym: Symbol::intern("x"), kind: IdentKind::Normal, span, spacing: Spacing::Alone },
    ];
    // `convert_dollar_crate` is false for `ts_to_string`.
    assert_eq!(print(&dollar_crate), "$crate :: x");
}

#[test]
fn raw_and_forced_keyword_idents() {
    assert_eq!(lexed("r#type"), "r#type");
    assert_eq!(quoted("r#type"), "r#type");
    let span = site();
    let forced = [Tree::Ident { sym: Symbol::intern("fn"), kind: IdentKind::ForcedKeyword, span, spacing: Spacing::Alone }];
    assert_eq!(print(&forced), "k#fn");
}

#[test]
fn idents_before_bang_and_parens() {
    assert_eq!(lexed("println!(\"{}\", x)"), "println!(\"{}\", x)");
    assert_eq!(quoted("println!(\"{}\", x)"), "println! (\"{}\", x)");
    assert_eq!(quoted("if !x {}"), "if ! x {}");
    assert_eq!(quoted("r#if!()"), "r#if! ()");
    assert_eq!(quoted("let (a, b) = (1, 2);"), "let (a, b) = (1, 2);");
    assert_eq!(quoted("pub(crate) fn f(x: u8) -> Self {}"), "pub(crate) fn f(x : u8) -> Self {}");
    assert_eq!(quoted("fn(u8) Self()"), "fn(u8) Self()");
    assert_eq!(quoted("Vec<u8>"), "Vec < u8 >");
    assert_eq!(quoted("x..=y"), "x ..= y");
}

#[test]
fn keywords_by_edition_are_asked_only_when_they_matter() {
    let trees = given(lex("dyn(x)").trees());
    assert_eq!(print_in(&trees, Edition::Edition2015), "dyn(x)");
    assert_eq!(print_in(&trees, Edition::Edition2018), "dyn (x)");
    let gen_bang = given(lex("gen!()").trees());
    assert_eq!(print_in(&gen_bang, Edition::Edition2021), "gen! ()");
    assert_eq!(print_in(&gen_bang, Edition::Edition2024), "gen ! ()");

    let asked = Cell::new(0);
    let mut out = String::new();
    to_string(&given(lex("a!(b) c(d) e f").trees()), &mut out, |_| {
        asked.set(asked.get() + 1);
        Edition::Edition2021
    });
    assert_eq!(out, "a! (b) c(d) e f");
    assert_eq!(asked.get(), 0);
}

#[test]
fn every_literal_kind() {
    let text = "'a' b'a' \"s\" r#\"s\"# b\"s\" br##\"s\"## c\"s\" cr#\"s\"# 1u8 1.5e3f64 0x1F 'a'_x \"\\n\"";
    assert_eq!(lexed(text), text);
    assert_eq!(quoted(text), text);
    let span = site();
    let negative = [
        Tree::Ident { sym: Symbol::intern("x"), kind: IdentKind::Normal, span, spacing: Spacing::Alone },
        Tree::Punct { ch: b'-', spacing: Spacing::Alone, span },
        Tree::Literal { kind: LitKind::Integer, symbol: Symbol::intern("-1"), suffix: Some(Symbol::intern("i8")), span, spacing: Spacing::Alone },
    ];
    assert_eq!(print(&negative), "x - -1i8");
    assert_eq!(print(&given(&negative)), "x - -1i8");
}

#[test]
fn braces_space_as_their_opening_delimiter() {
    assert_eq!(lexed("{x}"), "{x}");
    assert_eq!(lexed("{ x }"), "{ x }");
    assert_eq!(lexed("{ x}"), "{ x }");
    assert_eq!(lexed("{x }"), "{x}");
    assert_eq!(lexed("{}"), "{}");
    assert_eq!(lexed("{ a { b } c }"), "{ a { b } c }");
    assert_eq!(quoted("{x}"), "{ x }");
    assert_eq!(quoted("( x )"), "(x)");
}

#[test]
fn a_brace_group_longer_than_the_margin_breaks_and_indents() {
    let words = ["abcd"; 30].join(" ");
    let line = ["abcd"; 15].join(" ");
    assert_eq!(lexed(&format!("{{ {words} }}")), format!("{{\n    {line}\n    {line}\n}}"));
    assert_eq!(quoted(&format!("{{ {words} }}")), format!("{{\n    {line}\n    {line}\n}}"));
    // A parenthesized group has no breaks of its own: only its tokens' spaces break.
    assert_eq!(lexed(&format!("({words})")), format!("({line}\n{line})"));
}

#[test]
fn an_invisible_group_prints_only_its_tokens() {
    let span = site();
    let inner = given(lex("a + b").trees());
    let mut trees = vec![Tree::Group {
        delimiter: Delimiter::Invisible,
        stream: Stream::of(inner),
        span: DelimSpan { open: span, close: span, entire: span },
        open: Spacing::Alone,
        spacing: Spacing::Alone,
    }];
    trees.extend(given(lex("* c").trees()));
    assert_eq!(print(&trees), "a + b * c");
}

#[test]
fn doc_comments_print_as_written_and_end_the_line() {
    assert_eq!(lexed("/// doc\nfn f() {}"), "/// doc\nfn f() {}");
    assert_eq!(lexed("//! inner\nx"), "//! inner\nx");
    // A block doc comment is no line comment, so `space_between` adds its space after the
    // hard break.
    assert_eq!(lexed("/** block */ x"), "/** block */\n x");
}

#[test]
fn a_doc_comment_taken_apart_prints_as_its_attribute() {
    let stream = lex("/// a \"b\"\n");
    let [Tree::DocComment { kind, inner, data, span, .. }] = stream.trees() else {
        panic!("one doc comment: {:?}", stream.trees());
    };
    let mut trees = Vec::new();
    lex::desugar_doc(*kind, *inner, data, *span, &mut trees);
    assert_eq!(print(&given(&trees)), "#[doc = \" a \\\"b\\\"\"]");

    let stream = lex("//! it's\n");
    let [Tree::DocComment { kind, inner, data, span, .. }] = stream.trees() else {
        panic!("one doc comment: {:?}", stream.trees());
    };
    let mut trees = Vec::new();
    lex::desugar_doc(*kind, *inner, data, *span, &mut trees);
    assert_eq!(print(&given(&trees)), "# ! [doc = \" it\\'s\"]");
}

#[test]
fn a_round_trip_through_the_lexer_keeps_the_source() {
    for text in [
        "fn main() { let x = a::b(&mut y, 'a', \"s\"); x.y()?; }",
        "impl<'a, T: Clone> Foo<'a> for Bar<T> where T: Copy {}",
        "match x { Some(v) => v + 1, None => 0, }",
        "#[derive(Debug)] struct S(u8, [u8; 4]);",
        "macro_rules! m { ($x:expr) => { $x }; }",
        "let r = 0..=10; let s = &x[..]; a <<= 2; b >>= 3; c != d && e || f",
    ] {
        assert_eq!(lexed(text), text);
    }
}

#[test]
fn a_second_print_allocates_only_the_growth_of_out() {
    let words = ["abcd"; 40].join(" ");
    let text = format!("/// doc\nfn f(x: &'a u8) -> u8 {{ {words} ({words}) [x; 3] }}");
    let stream = given(lex(&text).trees());
    let mut out = String::new();
    to_string(&stream, &mut out, |_| Edition::Edition2021);
    let first = out.clone();
    out.clear();
    let ((), used) =
        blaze_alloc_count::measure(|| to_string(&stream, &mut out, |_| Edition::Edition2021));
    assert_eq!(used.allocations, 0, "a second print of one stream into a cleared `out`");
    assert_eq!(out, first);
}
