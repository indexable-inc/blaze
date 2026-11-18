//! [`lower_signature`] from the outline lexer ([`parser::LexedStr::new_outline`]: a member
//! `fn`'s body one token) and the word-range column ([`crate::feed::feed_input`]) lowers every
//! item form to the record the whole lex and the lexer-walking feed made
//! ([`crate::lower_signature_whole_lex`]), byte for byte.

use blaze_own::{Erased, Inline};

use crate::expand_tests::Unix;
use crate::{Edition, lower_signature, lower_signature_whole_lex};

/// The bytes one text's two lowerings take, on the test's stack: a named buffer, no heap.
const BUFFER: usize = 256 << 10;

/// Every item form, one item a text: fns with where clauses, impl Trait and braces in their
/// heads, traits with default bodies and consts, impls with const generics in their headers,
/// nested fns, `unsafe extern`, bodies holding a `}` in every token that is not a close,
/// bodies the outline parses in full, CR LF text, consts and statics, macro calls in a head.
const CORPUS: &[&str] = &[
    "fn f<T: Clone + Into<u8>, const N: usize>(a: &[T; N], b: impl Fn(u8) -> [u8; { 1 + 2 }]) -> impl Iterator<Item = T> where T: Copy, [(); { N }]: Sized { let _ = '}'; a.iter().copied() }",
    "fn g() -> impl Fn() -> [u8; {1}] { || [0] }",
    "fn h<const N: usize = { 1 }>() where Tr<{ N }>: Copy, for<'a> F: Fn(&'a u8) -> &'a u8 { }",
    "fn s<T: Into<Vec<u8>>>(t: T) -> Vec<Vec<u8>> { vec![t.into()] }",
    "trait Tr<const N: usize> where Self: Sized { const X: [u8; { 2 * 3 }] = [0; 6]; const Y: u8; fn d(&self) -> u8 { '}' as u8 } fn e(); unsafe fn g() -> impl Fn() -> [u8; {1}] { || [0] } type A: Into<u8>; }",
    "impl<T, const N: usize> Trait for [T; { N + 1 }] where T: Default, Tr<{ N }>: Copy { const C: u8 = { 3 }; fn h(&self) -> u8 { fn nested() -> u8 { 1 } nested() + Self::C } }",
    "impl X<{ N }> { pub(crate) const fn a(&self) -> u8 { 1 } #[inline] pub async fn b() {} default fn c() {} }",
    "unsafe impl<T: ?Sized> Send for Box<T> where T: Send {}",
    "impl Foo for fn() -> u8 { fn a() {} }",
    "fn outer() { fn inner() { fn innermost() {} } struct S; impl S { fn m(&self) {} } }",
    "unsafe extern \"C\" { pub safe fn a(x: i32) -> i32; pub unsafe fn b(); static C: u8; }",
    "#[cfg(unix)] #[doc = \"{\"] pub(crate) unsafe extern \"C\" fn g(x: *const u8, ...) -> i32 { 0 }",
    "#[cfg(windows)] fn w() { 1 }",
    "fn t<'a>() -> &'static str { let a = r#\"}\"#; let b = '}'; let c = b'}'; let d: &'a u8; /* } /* } */ } */ let e = \"}\"; // }\n let f = br##\"}}\"##; let g = b\"}\"; 'l: loop { break 'l; } \"x\" }",
    "fn t2() { let s = c\"}\"; let r = r\"}\"; let x = 0x7d_u8; let y = 1.0e3; let z = r#fn; }",
    "fn a1() { let x = 1; #[allow(unused)] }",
    "fn a2() { let x = 1;\n /// doc\n }",
    "fn a3() { let x = a < { 1 }; }",
    "fn a5() { builtin # offset_of(S, a) }",
    "fn a6() { let x = 1;",
    "fn crlf(x: u8)\r\n-> u8\r\n{\r\n    let s = \"a\r\nb\";\r\n    x\r\n}\r\n",
    "const C: [u8; 2] = { let a = [0; 2]; a };",
    "static S: fn() -> u8 = { fn f() -> u8 { 1 } f };",
    "const fn cf() -> u8 { 1 }",
    "fn m() -> m!{u8} { 0 }",
    "#[doc = \"}\"] #[cfg_attr(unix, doc = \"{\")] fn ga() {}",
    "mod m { fn a() { 1 } }",
    "enum E { A = { 1 }, B }",
    "struct S<'a, T: ?Sized + 'a> { a: &'a T, b: (u8, (u16, (u32, (u64, [u8; 4])))) }",
    "fn r#match() { r#fn }",
    "fn trailing() {}  // a comment after the body\n",
];

/// Expected line: `test outline_sig_tests::the_outline_lexer_lowers_every_signature_as_before
/// ... ok`, printing `OUTLINE SIGNATURE: N texts, records equal N, differing 0 (floor 0)`.
#[test]
fn the_outline_lexer_lowers_every_signature_as_before() {
    let mut equal = 0usize;
    for text in CORPUS {
        let buffer: Inline<BUFFER> = Inline::new();
        let erased = Erased::new(&buffer);
        let alloc = erased.handle();
        let (mut cfg_a, mut cfg_b) = (Unix, Unix);
        let new = lower_signature(text, Edition::E2024, &mut cfg_a, alloc);
        let old = lower_signature_whole_lex(text, Edition::E2024, &mut cfg_b, alloc);
        let (Ok(new), Ok(old)) = (new, old) else {
            panic!("a lowering asked for what the cfg set answers: {text:?}");
        };
        let (new, old) = (new.to_record(alloc), old.to_record(alloc));
        if *new == *old {
            equal += 1;
        } else {
            std::println!("OUTLINE SIGNATURE: differs on {text:?}");
        }
    }
    std::println!(
        "OUTLINE SIGNATURE: {} texts, records equal {equal}, differing {} (floor 0)",
        CORPUS.len(),
        CORPUS.len() - equal
    );
    assert_eq!(equal, CORPUS.len(), "a signature lowered differently from the outline lexer");
}
