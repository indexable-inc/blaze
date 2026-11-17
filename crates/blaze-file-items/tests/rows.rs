//! What the outline reads because it reads the text's own tokens, once, with the item grammar
//! deciding what a `fn` body is: three forms the path before it lost, which cut `{ .. }` groups
//! out of the text by a rule of its own (the first `{` after `fn` and a name, when the bytes
//! between the braces held no item keyword and no `!` after an identifier's ASCII byte) before
//! the grammar saw them. Each test states what the text says by the Rust Reference, and that
//! leaving bodies unparsed gives the rows a parse of every body gives.

#![feature(allocator_api)]

use blaze_file_items::{Bodies, Outline, TokenColumns, TokensRefused, outline, outline_tokens, outline_with};
use blaze_surface::{Edition, ErrorOrigin, Item, ItemDetail, ItemFlags, Range};

mod render;

/// The outline, bodies left unparsed.
fn unparsed(text: &str) -> Outline<'static> {
    outline(text, Edition::E2024, blaze_own::testing::heap().alloc())
}

/// The outline with every body parsed with the rest.
fn parsed(text: &str) -> Outline<'static> {
    outline_with(text, Edition::E2024, Bodies::Parsed, blaze_own::testing::heap().alloc())
}

/// The first entry named `name`.
fn named<'o>(outline: &'o Outline<'_>, text: &str, name: &str) -> &'o Item {
    outline
        .items()
        .iter()
        .find(|item| item.name.is_some_and(|it| item.text(text, it) == name))
        .unwrap_or_else(|| panic!("no entry is named `{name}`"))
}

/// Every finding by where it is, whose it is and what it says, in order.
fn said(outline: &Outline<'_>) -> Vec<(Range, ErrorOrigin, String)> {
    outline.errors().iter().map(|error| (error.range, error.origin, outline.message(error).to_owned())).collect()
}

/// Every row with its keys and digests, every attribute and the flags are equal.
fn assert_rows_equal(a: &Outline<'_>, b: &Outline<'_>) {
    assert_eq!(a.items(), b.items(), "the entries");
    assert_eq!(a.file_attrs(), b.file_attrs(), "the file's attributes");
    for (ours, theirs) in a.items().iter().zip(b.items()) {
        assert_eq!(a.attrs(ours), b.attrs(theirs), "an entry's attributes");
    }
    assert_eq!((a.flags(), a.own_flags()), (b.flags(), b.own_flags()), "the flags");
    assert_eq!(a.surface(), b.surface(), "the surface");
}

/// The Rust Reference, "Identifiers": `IDENTIFIER_OR_KEYWORD : XID_Start XID_Continue* | _
/// XID_Continue+`; `é` (U+00E9) is `XID_Start`, so `é` is an identifier. "Macros", macro
/// invocation: `MacroInvocation : SimplePath ! DelimTokenTree`. So `é!()` in a body is a macro
/// call, whose expansion may hold items: the body holds one and the `fn` has the macro-call
/// flag. A scan of the body's bytes that takes only an ASCII letter, digit or `_` before the `!`
/// as a path's end reads no call here.
#[test]
fn a_bang_call_named_in_letters_past_ascii_is_a_call() {
    let text = "fn f() { é!() }\nfn g() { é }\nfn h() { let x = é != 1; }\n";
    let outline = unparsed(text);
    assert!(outline.errors().is_empty(), "{:?}", said(&outline));
    let f = named(&outline, text, "f");
    assert_eq!(f.detail, ItemDetail::Fn { body_holds_items: true });
    assert!(f.flags.contains(ItemFlags::MACRO_CALL), "{:?}", f.flags);
    for plain in ["g", "h"] {
        let item = named(&outline, text, plain);
        assert_eq!(item.detail, ItemDetail::Fn { body_holds_items: false }, "{plain}");
        assert!(item.flags.is_empty(), "{plain}: {:?}", item.flags);
    }
    assert_rows_equal(&outline, &parsed(text));
}

/// The Rust Reference, "Tokens", integer literals: `BIN_LITERAL : 0b ( BIN_DIGIT | _ )*
/// BIN_DIGIT ( BIN_DIGIT | _ )*`: `0b` with no digit is no literal, and the lexer refuses it
/// where it stands, in a body as anywhere. The outline lexes the whole text once, so the finding
/// is its own whether or not the body around the token is parsed: a text's lexer errors do not
/// depend on which bodies were read.
#[test]
fn a_lexer_error_in_a_body_left_unparsed_is_a_finding() {
    let text = "fn f() { let x = 0b; }\n";
    let outline = unparsed(text);
    let at = u32::try_from(text.find("0b").expect("the token")).expect("a small text");
    let findings = said(&outline);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!((findings[0].0, findings[0].1), (Range::new(at, at + 2), ErrorOrigin::Lexer), "{findings:?}");
    assert_eq!(named(&outline, text, "f").detail, ItemDetail::Fn { body_holds_items: false });
    let full = parsed(text);
    assert_rows_equal(&outline, &full);
    assert_eq!(findings, said(&full));
}

/// The Rust Reference, "Keywords", reserved keywords: `gen` is reserved "beginning in the 2024
/// edition", and "Identifiers": an identifier is an `IDENTIFIER_OR_KEYWORD` that is not a strict
/// or reserved keyword. So in 2024 `fn f(gen: u8)` has no parameter there: the grammar ends the
/// `fn` at `gen`, and the braces after the `)` are no `fn` body but a block where an item is
/// expected, which the grammar reads statement by statement. Its tokens are the text's: the
/// `gen` in `a(gen)` is a finding ("expected expression", where a `gen` block's braces are
/// missing), and the rows are those of the text with every body parsed. A reader that cut the
/// group out as "the first `{` after `fn` and a name" gave the grammar `{}` in its place.
///
/// Where this was met: seven files of tempo's locked packages (async-trait 0.1.92, metrics-util
/// 0.20.4, moka 0.12.16 twice, primitive-types 0.12.2, serde_with 3.21.0, sharded-slab 0.1.7),
/// each edition-2018 or 2021 source with a parameter named `gen`, read as 2024 because the
/// lane's oracle (examples/oracle.rs) outlines every file in one edition. In its own edition
/// such a file has no finding and no such block.
#[test]
fn braces_the_grammar_reads_as_no_fn_body_are_read_whole() {
    let text = "impl S {\n    fn f(gen: u8) {\n        a(gen);\n        b();\n    }\n    fn after() {}\n}\n";
    let outline = unparsed(text);
    let inside = u32::try_from(text.find("a(gen)").expect("the call")).expect("a small text") + 2;
    let findings = said(&outline);
    assert!(
        findings.iter().any(|(range, origin, _)| range.start == inside && *origin == ErrorOrigin::Parser),
        "no parser finding at byte {inside}, the `gen` inside the braces: {findings:?}"
    );
    assert_eq!(named(&outline, text, "after").detail, ItemDetail::Fn { body_holds_items: false });
    let full = parsed(text);
    assert_rows_equal(&outline, &full);
    assert_eq!(findings, said(&full));
}

/// The parser's findings of an outline, in order: the lexer's are the text path's alone.
fn parser_said(outline: &Outline<'_>) -> Vec<(Range, String)> {
    said(outline).into_iter().filter(|(_, origin, _)| *origin == ErrorOrigin::Parser).map(|(range, _, message)| (range, message)).collect()
}

/// `text` written again from its tokens (tests/render), then outlined from the tokens handed
/// over and from the text written: the two outlines and the text they are of.
fn both_ways(text: &str) -> (Outline<'static>, Outline<'static>, String) {
    let (mut again, mut written) = (String::new(), Vec::new());
    let mut columns = TokenColumns::new_in(blaze_own::testing::heap().alloc());
    render::rerender(text, Edition::E2024, &mut again, &mut columns, &mut written).expect("the lexer's own tokens are taken");
    assert!(render::lexes_back(&again, Edition::E2024, &written), "the text written lexes to other tokens than those written: {again:?}");
    let from_tokens = outline_tokens(&again, &columns, Edition::E2024, blaze_own::testing::heap().alloc()).expect("tokens inside their text");
    (from_tokens, unparsed(&again), again)
}

/// The Rust Reference, "Tokens", punctuation: `>>`, `>=`, `!=`, `::`, `->`, `..=` and the
/// rest are each ONE token, so `> =` with whitespace between is two tokens and no `>=`
/// ("Whitespace": "any non-empty string containing only characters that have the
/// Pattern_White_Space Unicode property"; it separates tokens). The lexer under the grammar
/// gives single characters and says which are joint; tokens handed over with their text say it
/// by where they stand in the text, one rule for both (`parser::Joiner`). So each text below
/// reads the same from its tokens as from its text: an operator written apart is no operator
/// (a finding, the same finding), one written together is, a float's fraction is the float's
/// (`1.0`, `x.0.1`), and a `!` after a name past ASCII is a call.
///
/// Prints "test the_tokens_handed_over_read_as_their_text ... ok". Without the rule's gap
/// (`Joiner::gap` never called for handed-over tokens) `1 > = 2` reads as `1 >= 2` and the
/// findings differ.
#[test]
fn the_tokens_handed_over_read_as_their_text() {
    let texts = [
        "const A: bool = 1 > = 2;\nconst B: bool = 1 >= 2;\nconst C: bool = a ! = b;\nconst D: bool = a != b;\n",
        "use a : : b;\nuse c::d;\nmod m { use e ::f; use g:: h; }\n",
        "const E: u8 = 1 > > 2;\nconst F: u8 = 1 >> 2 << 3;\ntype G = H<I<J> >;\ntype K = L<M<N>>;\n",
        "const O: f32 = 1.0;\nconst P: f32 = 1. + 2.;\nconst Q: u8 = x.0.1;\nconst R: u8 = y.0 .1;\nconst S: f64 = 1e3.max(2.5);\n",
        "/// doc\n#[cfg(a)] pub(crate) struct S<'a> {\n    /// a field\n    pub f: &'a u8, // not a doc\n    g: u8, /* nor this */\n}\n",
        "macro_rules! m { ($a:tt) => { $a :: b } }\nm!{ a::b }\né!();\nfn f() { é!() }\nfn g() { let x = é != 1; }\n",
        "fn f(gen: u8) { a(gen); }\nenum E { #[x] A = 1, B(u8), C { x: u8 } }\ntrait T = A + B;\nextern \"C\" { fn g(); }\nunsafe impl !Send for X {}\n",
        "fn f() -> u8 { 1 }\nfn g() - > u8 { 1 }\nconst R: core::ops::RangeInclusive<u8> = 1..=2;\nconst W: u8 = 1 . . = 2;\n",
        "  #![a]\n//! inner\nfn f() { let x = 0b; }\nconst Z: u8 = 0b;\n",
        "",
        "   // nothing but a comment\n",
    ];
    for text in texts {
        let (from_tokens, from_text, again) = both_ways(text);
        assert_rows_equal(&from_tokens, &from_text);
        assert_eq!(parser_said(&from_tokens), parser_said(&from_text), "the parser's findings of {again:?}");
        assert!(said(&from_tokens).iter().all(|(_, origin, _)| *origin == ErrorOrigin::Parser), "nothing is lexed: {again:?}");
    }
    // What the first text says, stated and not only compared: two operators written apart are
    // two findings' worth of no operator, and those written together none.
    let (from_tokens, _, again) = both_ways(texts[0]);
    let findings = parser_said(&from_tokens);
    // A name and its `:` are written together, so they are in the text written as they were.
    let (b, c, d) = (again.find("B:").expect("B") as u32, again.find("C:").expect("C") as u32, again.find("D:").expect("D") as u32);
    assert!(findings.iter().any(|(range, _)| range.start < b), "`> =` is no `>=`: {findings:?}");
    assert!(findings.iter().any(|(range, _)| range.start >= c && range.start < d), "`! =` is no `!=`: {findings:?}");
    assert!(findings.iter().all(|(range, _)| range.start < b || (range.start >= c && range.start < d)), "{findings:?}");
}

/// What tokens are refused, each by its name: a token is bytes of the text, in order, sharing
/// none, cut at characters, and its word a token's (`TokensRefused`'s docs). Nothing is added
/// by a refused push: the tokens before it still read.
///
/// Prints "test tokens_that_are_not_a_texts_are_refused_by_name ... ok".
#[test]
fn tokens_that_are_not_a_texts_are_refused_by_name() {
    let ident = parser::word(parser::SyntaxKind::IDENT, parser::SyntaxKind::IDENT, parser::Edition::Edition2024);
    let word = |kind| parser::word(kind, parser::SyntaxKind::EOF, parser::Edition::Edition2024);
    let mut columns = TokenColumns::new_in(blaze_own::testing::heap().alloc());
    assert_eq!(columns.push(ident, 3, 3), Err(TokensRefused::Empty { token: 0, start: 3, end: 3 }));
    assert_eq!(columns.push(word(parser::SyntaxKind::WHITESPACE), 0, 1), Err(TokensRefused::NotAToken { token: 0, syntax: u16::from(parser::SyntaxKind::WHITESPACE) }));
    assert_eq!(columns.push(word(parser::SyntaxKind::EOF), 0, 1), Err(TokensRefused::NotAToken { token: 0, syntax: u16::from(parser::SyntaxKind::EOF) }));
    assert_eq!(columns.push(u32::MAX, 0, 1), Err(TokensRefused::Word { token: 0, word: u32::MAX, why: parser::WordRefused::Kind }));
    assert!(columns.is_empty());
    // `mod é;`: `é` is bytes 4..6.
    let text = "mod é;";
    assert_eq!(columns.push(word(parser::SyntaxKind::MOD_KW), 0, 3), Ok(()));
    assert_eq!(columns.push(ident, 2, 6), Err(TokensRefused::Overlaps { token: 1, start: 2, before_end: 3 }));
    assert_eq!(columns.len(), 1);
    assert_eq!(columns.push(ident | parser::JOINT | parser::FRAGMENT, 4, 6), Ok(()));
    assert_eq!(columns.push(word(parser::SyntaxKind::SEMICOLON), 6, 7), Ok(()));
    let outline = outline_tokens(text, &columns, Edition::E2024, blaze_own::testing::heap().alloc()).expect("the text's tokens");
    assert_eq!(named(&outline, text, "é").detail, ItemDetail::Mod { inline: false, path: None });
    assert_rows_equal(&outline, &unparsed(text));
    assert_eq!(outline_tokens("mod é", &columns, Edition::E2024, blaze_own::testing::heap().alloc()).err(), Some(TokensRefused::OutsideText { token: 2, end: 7, len: 6 }));
    columns.clear();
    assert_eq!(columns.push(word(parser::SyntaxKind::MOD_KW), 0, 3), Ok(()));
    assert_eq!(columns.push(ident, 4, 5), Ok(()));
    assert_eq!(outline_tokens(text, &columns, Edition::E2024, blaze_own::testing::heap().alloc()).err(), Some(TokensRefused::SplitsChar { token: 1, at: 5 }));
}
