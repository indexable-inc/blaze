//! The outline lexer ([`LexedStr::new_outline`]): a member `fn`'s body is one `LAZY_BODY`
//! token, and the outline parse of its tokens is the outline parse of the whole lex's, step for
//! step and byte for byte (the lazy body's text the same). A head's braces (const generics,
//! array lengths, `-> impl Fn() -> [u8; {1}]`, where-clause const blocks, macro calls in a head,
//! braces in attributes) are not a body; a body holding a refusal of
//! [`crate::lazy_block_len`] is lexed in full. Two controls: a planted `'}'` taken for a close
//! is caught, and the skip turned off gives the whole lex's tokens.

use core::fmt::Write;

use crate::lexed_str::plant::{self, Plant};
use crate::{Edition, LexedStr, OutlineStrStep, Reparser, SyntaxKind};

/// Every item form, one item a text, as `blaze-lower`'s `lower_signature` is handed them.
pub(crate) const CORPUS: &[&str] = &[
    // A fn with where clauses, impl Trait, a const generic and braces in its head.
    "fn f<T: Clone + Into<u8>, const N: usize>(a: &[T; N], b: impl Fn(u8) -> [u8; { 1 + 2 }]) -> impl Iterator<Item = T> where T: Copy, [(); { N }]: Sized { let _ = '}'; a.iter().copied() }",
    "fn g() -> impl Fn() -> [u8; {1}] { || [0] }",
    "fn h<const N: usize = { 1 }>() where Tr<{ N }>: Copy, for<'a> F: Fn(&'a u8) -> &'a u8 { }",
    "fn s<T: Into<Vec<u8>>>(t: T) -> Vec<Vec<u8>> { vec![t.into()] }",
    // A trait with default bodies and consts.
    "trait Tr<const N: usize> where Self: Sized { const X: [u8; { 2 * 3 }] = [0; 6]; const Y: u8; fn d(&self) -> u8 { '}' as u8 } fn e(); unsafe fn g() -> impl Fn() -> [u8; {1}] { || [0] } type A: Into<u8>; }",
    // An impl with const generics in its header.
    "impl<T, const N: usize> Trait for [T; { N + 1 }] where T: Default, Tr<{ N }>: Copy { const C: u8 = { 3 }; fn h(&self) -> u8 { fn nested() -> u8 { 1 } nested() + Self::C } }",
    "impl X<{ N }> { pub(crate) const fn a(&self) -> u8 { 1 } #[inline] pub async fn b() {} default fn c() {} }",
    "unsafe impl<T: ?Sized> Send for Box<T> where T: Send {}",
    "impl Foo for fn() -> u8 { fn a() {} }",
    "impl !Send for S { }",
    // Nested fns.
    "fn outer() { fn inner() { fn innermost() {} } struct S; impl S { fn m(&self) {} } }",
    // `unsafe extern` and extern fns.
    "unsafe extern \"C\" { pub safe fn a(x: i32) -> i32; pub unsafe fn b(); static C: u8; }",
    "#[cfg(unix)] #[doc = \"{\"] pub(crate) unsafe extern \"C\" fn g(x: *const u8, ...) -> i32 { 0 }",
    "extern \"C\" fn q() {}",
    // Body text that holds a `}` in every token that is not a close.
    "fn t<'a>() -> &'static str { let a = r#\"}\"#; let b = '}'; let c = b'}'; let d: &'a u8; /* } /* } */ } */ let e = \"}\"; // }\n let f = br##\"}}\"##; let g = b\"}\"; 'l: loop { break 'l; } \"x\" }",
    "fn t2() { let s = c\"}\"; let r = r\"}\"; let x = 0x7d_u8; let y = 1.0e3; let z = r#fn; }",
    // Bodies the outline parses in full: an attribute or doc comment right before `}`, a `#`
    // or `<` before a brace, `builtin #`, a body that does not close.
    "fn a1() { let x = 1; #[allow(unused)] }",
    "fn a2() { let x = 1;\n /// doc\n }",
    "fn a3() { let x = a < { 1 }; }",
    "fn a4() { m!(# {}); }",
    "fn a5() { builtin # offset_of(S, a) }",
    "fn a6() { let x = 1;",
    "fn a7() { let é = \"ü\"; }",
    // CR LF text.
    "fn crlf(x: u8)\r\n-> u8\r\n{\r\n    let s = \"a\r\nb\";\r\n    x\r\n}\r\n",
    // Consts and statics: their initializers are not skipped.
    "const C: [u8; 2] = { let a = [0; 2]; a };",
    "static S: fn() -> u8 = { fn f() -> u8 { 1 } f };",
    "const fn cf() -> u8 { 1 }",
    "const _: () = { fn z() {} };",
    // A macro call in a head, attributes with braces, contract clauses.
    "fn m() -> m!{u8} { 0 }",
    "#[doc = \"}\"] #[cfg_attr(unix, doc = \"{\")] fn ga() {}",
    "fn v(x: u8) -> u8 requires x < 3 { x }",
    // Other items, and items holding fns the outline lexes as before.
    "mod m { fn a() { 1 } }",
    "macro_rules! mm { () => { fn x() {} } }",
    "enum E { A = { 1 }, B }",
    "struct S<'a, T: ?Sized + 'a> { a: &'a T, b: (u8, (u16, (u32, (u64, [u8; 4])))) }",
    "type F = fn() -> u8;",
    "fn r#match() { r#fn }",
];

/// A hash of the outline steps of `lexed`'s outline parse, each step's kind and text, and how
/// many there are: no step is kept.
fn outline_steps(lexed: &LexedStr<'_>, edition: Edition) -> (u64, usize) {
    let input = lexed.to_input(edition);
    let output = Reparser::top_level_item().parse_outline(input.tokens());
    let mut hash = Fnv(0xcbf2_9ce4_8422_2325);
    let mut steps = 0usize;
    lexed.intersperse_trivia_outline(&output, &mut |step: OutlineStrStep<'_>| {
        steps += 1;
        write!(hash, "{step:?};").expect("a hash takes every byte");
    });
    (hash.0, steps)
}

/// FNV-1a over what is written to it.
struct Fnv(u64);

impl Write for Fnv {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
        Ok(())
    }
}

fn lazy_bodies(lexed: &LexedStr<'_>) -> usize {
    (0..lexed.len()).filter(|&i| lexed.kind(i) == SyntaxKind::LAZY_BODY).count()
}

/// Whether the two lexes are the same tokens: kinds and lengths.
fn same_tokens(a: &LexedStr<'_>, b: &LexedStr<'_>) -> bool {
    a.len() == b.len() && (0..a.len()).all(|i| a.kind(i) == b.kind(i) && a.text_len(i) == b.text_len(i))
}

/// How many texts of the corpus outline differently from the outline lexer's tokens than from
/// the whole lex's, and the `LAZY_BODY`s made.
fn differing() -> (usize, usize) {
    let edition = Edition::CURRENT;
    let (mut differ, mut lazies) = (0, 0);
    for text in CORPUS {
        let whole = LexedStr::new(edition, text);
        let outline = LexedStr::new_outline(edition, text);
        lazies += lazy_bodies(&outline);
        if outline_steps(&whole, edition) != outline_steps(&outline, edition) {
            differ += 1;
            std::println!("OUTLINE LEX: differs on {text:?}");
        }
    }
    (differ, lazies)
}

/// Expected line: `test tests::outline_lex::the_outline_lex_outlines_as_the_whole_lex ... ok`,
/// printing `OUTLINE LEX: N texts, M lazy bodies, differing 0 (floor 0)`.
#[test]
fn the_outline_lex_outlines_as_the_whole_lex() {
    let edition = Edition::CURRENT;
    let (differ, lazies) = differing();
    for text in CORPUS {
        // The whole lex still parses, in full and in outline.
        let whole = LexedStr::new(edition, text);
        let input = whole.to_input(edition);
        let _ = Reparser::top_level_item().parse(input.tokens());
        // Every byte is a token's, the lazy body's too.
        let outline = LexedStr::new_outline(edition, text);
        let bytes: usize = (0..outline.len()).map(|i| outline.text_len(i)).sum();
        assert_eq!(bytes, text.len(), "the outline lex covers {text:?}");
    }
    std::println!("OUTLINE LEX: {} texts, {lazies} lazy bodies, differing {differ} (floor 0)", CORPUS.len());
    assert_eq!(differ, 0, "an outline lex outlined differently");
    assert!(lazies >= 15, "the corpus's member fn bodies are lazy: {lazies}");
}

/// Which bodies are skipped: a member fn's, never a head's braces, a const's initializer, a
/// body holding a refusal, or a fn in a `mod`, a block or a macro call.
#[test]
fn only_member_fn_bodies_are_one_token() {
    let edition = Edition::CURRENT;
    let expect: &[(&str, usize)] = &[
        ("fn g() -> impl Fn() -> [u8; {1}] { || [0] }", 1),
        ("fn h<const N: usize = { 1 }>() where Tr<{ N }>: Copy, for<'a> F: Fn(&'a u8) -> &'a u8 { }", 1),
        ("impl<T, const N: usize> Trait for [T; { N + 1 }] where T: Default, Tr<{ N }>: Copy { const C: u8 = { 3 }; fn h(&self) -> u8 { fn nested() -> u8 { 1 } nested() + Self::C } }", 1),
        ("impl X<{ N }> { pub(crate) const fn a(&self) -> u8 { 1 } #[inline] pub async fn b() {} default fn c() {} }", 3),
        ("impl Foo for fn() -> u8 { fn a() {} }", 1),
        ("unsafe extern \"C\" { pub safe fn a(x: i32) -> i32; pub unsafe fn b(); static C: u8; }", 0),
        ("fn a1() { let x = 1; #[allow(unused)] }", 0),
        ("fn a2() { let x = 1;\n /// doc\n }", 0),
        ("fn a3() { let x = a < { 1 }; }", 0),
        ("fn a4() { m!(# {}); }", 0),
        ("fn a5() { builtin # offset_of(S, a) }", 0),
        ("fn a6() { let x = 1;", 0),
        ("const C: [u8; 2] = { let a = [0; 2]; a };", 0),
        ("const fn cf() -> u8 { 1 }", 1),
        ("fn m() -> m!{u8} { 0 }", 0),
        ("#[doc = \"}\"] #[cfg_attr(unix, doc = \"{\")] fn ga() {}", 1),
        ("fn v(x: u8) -> u8 requires x < 3 { x }", 0),
        ("mod m { fn a() { 1 } }", 0),
        ("enum E { A = { 1 }, B }", 0),
        ("fn crlf(x: u8)\r\n-> u8\r\n{\r\n    let s = \"a\r\nb\";\r\n    x\r\n}\r\n", 1),
    ];
    for &(text, lazies) in expect {
        assert_eq!(lazy_bodies(&LexedStr::new_outline(edition, text)), lazies, "{text:?}");
    }
}

/// The floors: a fn with a body of 2,000 tokens pushes its head's tokens and ONE more, and its
/// parser input holds no body token.
///
/// Expected line: `test tests::outline_lex::a_long_body_is_one_token ... ok`, printing
/// `OUTLINE LEX: 2000-token body: tokens pushed B -> A (floor F), input tokens I -> J (floor K)`
/// with `A == F` and `J == K`.
#[test]
fn a_long_body_is_one_token() {
    let edition = Edition::CURRENT;
    const HEAD: &str = "pub fn big<T: Copy>(x: u8) -> u8 where T: Clone ";
    // `{ ` then `x + ` 500 times (2,000 tokens with the whitespace) then `x }`.
    let mut buffer = [0u8; HEAD.len() + 2 + 4 * 500 + 3];
    buffer[..HEAD.len()].copy_from_slice(HEAD.as_bytes());
    let mut at = HEAD.len();
    for piece in core::iter::once("{ ").chain(core::iter::repeat_n("x + ", 500)).chain(core::iter::once("x }")) {
        buffer[at..at + piece.len()].copy_from_slice(piece.as_bytes());
        at += piece.len();
    }
    let text = core::str::from_utf8(&buffer[..at]).expect("ASCII");
    let whole = LexedStr::new(edition, text);
    let outline = LexedStr::new_outline(edition, text);
    let head = LexedStr::new(edition, HEAD);
    let (whole_input, outline_input, head_input) =
        (whole.to_input(edition).len(), outline.to_input(edition).len(), head.to_input(edition).len());
    std::println!(
        "OUTLINE LEX: 2000-token body: tokens pushed {} -> {} (floor {}), input tokens {whole_input} -> {outline_input} (floor {})",
        whole.len(),
        outline.len(),
        head.len() + 1,
        head_input + 1,
    );
    assert!(whole.len() > 2000, "the body is 2,000 tokens");
    assert_eq!(outline.len(), head.len() + 1, "the head's tokens and one");
    assert_eq!(outline_input, head_input + 1, "no body token in the input");
    assert_eq!(outline_steps(&whole, edition), outline_steps(&outline, edition));
}

/// Control 1 (rule 23): with the planted bug, a `'}'` char's `}` taken for a close, the
/// comparison fails on the texts holding one; without it, on none.
///
/// Expected line: `test tests::outline_lex::a_planted_char_close_is_caught ... ok`, printing
/// `OUTLINE LEX CONTROL: planted '}' close: differing N (> 0); clean: 0`.
#[test]
fn a_planted_char_close_is_caught() {
    let (planted, _) = plant::with(Plant::CharCloses, differing);
    let (clean, _) = differing();
    std::println!("OUTLINE LEX CONTROL: planted '}}' close: differing {planted} (> 0); clean: {clean}");
    assert!(planted > 0, "the planted bug was not caught");
    assert_eq!(clean, 0);
}

/// Control 2: with the skip turned off the outline lexer makes the whole lex's tokens, every
/// count of the 2,000-token test back to the old one.
///
/// Expected line: `test tests::outline_lex::the_skip_off_lexes_every_token ... ok`, printing
/// `OUTLINE LEX CONTROL: skip off: N texts lex as the whole lex, M tokens, 0 lazy`.
#[test]
fn the_skip_off_lexes_every_token() {
    let edition = Edition::CURRENT;
    let (mut tokens, mut lazies) = (0, 0);
    plant::with(Plant::SkipOff, || {
        for text in CORPUS {
            let whole = LexedStr::new(edition, text);
            let outline = LexedStr::new_outline(edition, text);
            assert!(same_tokens(&whole, &outline), "skip off lexes {text:?} as the whole lex");
            tokens += outline.len();
            lazies += lazy_bodies(&outline);
        }
    });
    std::println!(
        "OUTLINE LEX CONTROL: skip off: {} texts lex as the whole lex, {tokens} tokens, {lazies} lazy",
        CORPUS.len()
    );
    assert_eq!(lazies, 0);
}
