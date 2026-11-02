//! Source text, through the lexer (`parser::LexedStr::new`, whose byte classes are made by
//! vector code) and the parser (`syntax::SourceFile::parse`, an outline whose bodies are
//! parsed when walked). The first byte picks the edition; the rest is the text, made UTF-8 by
//! replacement as a file read as text is.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - neither panics nor hangs, on any text;
//! - the tokens tile the text: each starts where the last ended, the last ends at its end,
//!   and each one's text is the text's bytes there;
//! - the fast path lexes as `rustc_lexer` alone (`LexedStr::lexes_alike`);
//! - the tree is lossless: its tokens, every lazy body's included, tile the text as the
//!   lexer's do, and every error (every body's included) is inside it; in the current edition
//!   its blocks are balanced (`syntax::fuzz::check_parser`);
//! - neither asks for more than `blaze_fuzz::allowed(len)` bytes at once.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_fuzzer::Target;
use syntax::{Edition, LexedStr, NodeOrToken, SourceFile, TextRange, TextSize};

#[path = "syntax_shared.rs"]
mod shared;

const TARGET: Target = Target {
    name: "syntax_parse",
    check,
    seeds,
    dictionary: &[
        b"fn", b"let", b"match", b"impl", b"macro_rules!", b"r#", b"r\"", b"br\"", b"c\"",
        b"b'", b"'a", b"#\"", b"##", b"///", b"//!", b"/*", b"*/", b"#!", b"---\n", b"1e10",
        b"0x", b"\\u{", b"::", b"=>", b"..=", b"(", b")", b"{", b"}", b"[", b"]", b"<", b">",
        b"\xc2\x85", b"\xe2\x80\xa8",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

fn check(data: &[u8]) {
    let Some((&first, rest)) = data.split_first() else { return };
    let editions = Edition::iter().count();
    let edition = Edition::iter().nth(first as usize % editions).unwrap();
    let text = String::from_utf8_lossy(rest);
    let bound = blaze_fuzz::allowed(text.len());

    let (largest, lexed) = blaze_fuzz::largest_allocation(|| LexedStr::new(edition, &text));
    assert!(largest <= bound, "lexing {} bytes asked for {largest} at once", text.len());
    let mut end = 0;
    for i in 0..lexed.len() {
        let range = lexed.text_range(i);
        assert_eq!(range.start, end, "token {i} starts where the last ended");
        assert_eq!(lexed.text(i), &text[range.clone()], "token {i} is the text's bytes");
        end = range.end;
    }
    assert_eq!(end, text.len(), "the tokens end at the text's end");
    assert!(LexedStr::lexes_alike(edition, &text), "the fast path lexes as rustc_lexer alone");

    let (largest, parse) = blaze_fuzz::largest_allocation(|| SourceFile::parse(&text, edition));
    assert!(largest <= bound, "parsing {} bytes asked for {largest} at once", text.len());
    let (largest, errors) = blaze_fuzz::largest_allocation(|| parse.errors_with_bodies());
    assert!(largest <= bound, "parsing bodies of {} bytes asked for {largest} at once", text.len());
    let whole = TextRange::up_to(TextSize::of(&*text));
    for error in &errors {
        assert!(whole.contains_range(error.range()), "an error at {:?}", error.range());
    }
    let mut end = TextSize::new(0);
    for (element, range) in parse.root().descendants_with_ranges() {
        let NodeOrToken::Token(token) = element else { continue };
        assert_eq!(range.start(), end, "the tree's token at {range:?} starts where the last ended");
        assert_eq!(token.text(), &text[range], "the tree's token at {range:?} is the text's bytes");
        end = range.end();
    }
    assert_eq!(end, whole.end(), "the tree's tokens end at the text's end");
    if edition == Edition::CURRENT {
        syntax::fuzz::check_parser(&text);
    }
}

/// The parser's own test files (every token, every grammar entry, their error cases), and
/// texts at the lexer's edges: deep nesting, a long string, a shebang and frontmatter, bytes
/// that are not UTF-8, Unicode whitespace and identifiers.
fn seeds() -> Vec<Vec<u8>> {
    let texts: &[&str] = &[
        "",
        "fn main() { let x: Vec<u8> = vec![1, 2]; match x[0] { 0..=9 => {} _ => () } }",
        "#!/usr/bin/env cargo\n---\n[package]\n---\nfn main() {}\n",
        "r#\"raw\"# br\"b\" c\"c\" cr#\"c\"# b'x' '\\u{1F600}' 'a 'r#a #\"guarded\"# ##",
        "/* /* nested */ */ /// doc\n//! inner\n/** block doc */ x",
        "1e10 0x1f_u8 1.0f32 1. 1..2 1.e3 0b2 \"unterminated",
        "fn f() { \u{2028} é名🦀 \u{85} }",
        "impl<T: ?Sized + 'static> Tr for &mut [T; N] where for<'a> T: 'a { type A = (); }",
    ];
    let mut seeds: Vec<Vec<u8>> =
        texts.iter().map(|it| [&[3u8][..], it.as_bytes()].concat()).collect();
    seeds.push([&[0u8][..], "(".repeat(5000).as_bytes()].concat());
    seeds.push([&[3u8][..], "{".repeat(3000).as_bytes(), "}".repeat(3000).as_bytes()].concat());
    seeds.push([&[3u8][..], b"fn f() { \"", "s".repeat(1 << 15).as_bytes(), b"\" }"].concat());
    seeds.push(vec![3, b'f', b'n', 0xff, 0xc0, b' ', 0xe2, 0x80]);
    for (n, path) in shared::rust_files("crates/parser/test_data").iter().enumerate() {
        seeds.push([&[(n % 4) as u8][..], &shared::read(path)].concat());
    }
    seeds
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
