//! The lints of rustc's lexer, `keyword_idents` and the identifier lints, against rustc's UI
//! tests.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked_with, compiletest, error, normalized, ui, warning},
};

/// std's items the UI tests use, `println!` (with what it calls) and `Box`, and what std's
/// prelude adds of them.
const STD: (&str, &str) = (
    r#"pub mod io {
    pub fn _print(_args: core::fmt::Arguments<'_>) {}
}

pub mod boxed {
    #[lang = "owned_box"]
    pub struct Box<T: ?Sized>(*mut T);
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        $crate::io::_print(core::format_args_nl!($($arg)*));
    }};
}
"#,
    "pub use crate::{boxed::Box, println};",
);

/// What compiletest gives for `fixture`, each file named `$DIR/<path>`: [`ui`] of a UI test
/// of more than one file.
fn stderr(fixture: &str) -> String {
    let options = compiletest(Mode::Other, &Options::default());
    let stderr = checked_with(fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() });
    normalized(&stderr)
}

/// [`ui`] of a UI test that uses std's items: `std` is a crate of [`STD`] on minicore, with a
/// prelude of core's and of what std's prelude adds.
fn ui_with_std(name: &str, edition: &str, source: &str) -> String {
    let (items, prelude) = STD;
    let preludes: String = ["rust_2015", "rust_2018", "rust_2021", "rust_2024"]
        .iter()
        .map(|it| format!("    pub mod {it} {{ pub use core::prelude::{it}::*; {prelude} }}\n"))
        .collect();
    stderr(&format!(
        "//- minicore: sized, fmt, builtin_impls\n//- /std.rs crate:std\npub mod prelude {{\n{preludes}}}\n{items}\n\
         //- /{name} crate:other:main edition:{edition} deps:std\n{source}"
    ))
}

/// `source` with each `<U+XXXX>` in it written as that codepoint. The UI tests of the text
/// direction lints hold codepoints this file can't: rustc denies them in a literal.
fn codepoints(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(at) = rest.find("<U+") {
        out.push_str(&rest[..at]);
        let end = rest[at..].find('>').map(|it| at + it).expect("a `<U+XXXX>` is closed");
        let code = u32::from_str_radix(&rest[at + 3..end], 16).expect("a `<U+XXXX>` is hexadecimal");
        out.push(char::from_u32(code).expect("a `<U+XXXX>` is a codepoint"));
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[test]
fn markers_are_codepoints() {
    assert_eq!(codepoints("a<U+202E>b<U+2066>"), "a\u{202e}b\u{2066}");
    assert_eq!(codepoints("plain"), "plain");
}

#[test]
fn a_doc_comment_with_codepoints_is_denied() {
    // tests/ui/parser/unicode-control-doc-comment-issue-153096.rs
    let source = r#"//@ edition: 2024

#[allow(unused)]
/// <U+2068>א<U+2069>, <U+2068>ב<U+2069>, <U+2068>ג<U+2069>, <U+2068>ד<U+2069>, <U+2068>ה<U+2069>
//~^ ERROR unicode codepoint changing visible direction of text present in doc comment
fn foo() {}

fn main() {}
"#;
    let expected = r#"error: unicode codepoint changing visible direction of text present in doc comment
  --> $DIR/unicode-control-doc-comment-issue-153096.rs:4:1
   |
LL | /// �א�, �ב�, �ג�, �ד�, �ה�
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^ this doc comment contains invisible unicode text flow control codepoints
   |
   = note: these kind of unicode codepoints change the way text flows on applications that support them, but can cause confusion because they change the order of characters on the screen
   = note: if their presence wasn't intentional, you can remove them
   = note: if you need to keep them and make them explicit in source, rewrite this doc comment as a `#[doc = "..."]` attribute and use Unicode escapes such as '\u{2068}', '\u{2069}', '\u{2068}', '\u{2069}', '\u{2068}', '\u{2069}', '\u{2068}', '\u{2069}', '\u{2068}', '\u{2069}'
   = note: `#[deny(text_direction_codepoint_in_literal)]` on by default

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("unicode-control-doc-comment-issue-153096.rs", "2024", &codepoints(source)), expected);
}

#[test]
fn codepoints_in_comments_and_literals_are_denied() {
    // tests/ui/parser/unicode-control-codepoints.rs
    let source = r###"//@ edition: 2021

fn main() {
    // if access_level != "us<U+202B>e<U+202A>r" { // Check if admin
    //~^ ERROR unicode codepoint changing visible direction of text present in comment
    println!("us\u{202B}e\u{202A}r");
    println!("{:?}", r#"us\u{202B}e\u{202A}r"#);
    println!("{:?}", b"us\u{202B}e\u{202A}r");
    //~^ ERROR unicode escape in byte string
    //~| ERROR unicode escape in byte string
    println!("{:?}", br##"us\u{202B}e\u{202A}r"##);

    println!("{:?}", "/*<U+202E> } <U+2066>if isAdmin<U+2069> <U+2066> begin admins only ");
    //~^ ERROR unicode codepoint changing visible direction of text present in literal

    println!("{:?}", r##"/*<U+202E> } <U+2066>if isAdmin<U+2069> <U+2066> begin admins only "##);
    //~^ ERROR unicode codepoint changing visible direction of text present in literal
    println!("{:?}", b"/*<U+202E> } <U+2066>if isAdmin<U+2069> <U+2066> begin admins only ");
    //~^ ERROR non-ASCII character in byte string literal
    //~| ERROR non-ASCII character in byte string literal
    //~| ERROR non-ASCII character in byte string literal
    //~| ERROR non-ASCII character in byte string literal
    println!("{:?}", br##"/*<U+202E> } <U+2066>if isAdmin<U+2069> <U+2066> begin admins only "##);
    //~^ ERROR non-ASCII character in raw byte string literal
    //~| ERROR non-ASCII character in raw byte string literal
    //~| ERROR non-ASCII character in raw byte string literal
    //~| ERROR non-ASCII character in raw byte string literal
    println!("{:?}", '<U+202E>');
    //~^ ERROR unicode codepoint changing visible direction of text present in literal

    let _ = c"<U+202E>";
    //~^ ERROR unicode codepoint changing visible direction of text present in literal
    let _ = cr#"<U+202E>"#;
    //~^ ERROR unicode codepoint changing visible direction of text present in literal

    println!("{{<U+202E>}}");
    //~^ ERROR unicode codepoint changing visible direction of text present in literal
}

//"/*<U+202E> } <U+2066>if isAdmin<U+2069> <U+2066> begin admins only */"
//~^ ERROR unicode codepoint changing visible direction of text present in comment

/**  '<U+202E>'); */fn foo() {}
//~^ ERROR unicode codepoint changing visible direction of text present in doc comment

/**
 *
 *  '<U+202E>'); */fn bar() {}
//~^^^ ERROR unicode codepoint changing visible direction of text present in doc comment
"###;
    assert_reports(
        &ui_with_std("unicode-control-codepoints.rs", "2021", &codepoints(source)),
        &[
            error(8, &[]),
            error(8, &[]),
            error(18, &[]),
            error(18, &[]),
            error(18, &[]),
            error(18, &[]),
            error(23, &[]),
            error(23, &[]),
            error(23, &[]),
            error(23, &[]),
            error(4, &[]),
            error(13, &[]),
            error(16, &[]),
            error(28, &[]),
            error(31, &[]),
            error(33, &[]),
            error(36, &[]),
            error(40, &[]),
            error(43, &[]),
            error(46, &[]),
        ],
    );
}

#[test]
fn an_allowed_comment_codepoint_is_not_linted() {
    // tests/ui/lint/issue-90614-accept-allow-text-direction-codepoint-in-comment-lint.rs
    let source = r#"//@ check-pass
// Allowing the code lint should work without warning and
// the text flow char in the comment should be ignored.

#![allow(text_direction_codepoint_in_comment)]

fn main() {
    // U+2066 LEFT-TO-RIGHT ISOLATE follows:<U+2066><U+2066>
}
"#;
    let expected = "";
    assert_eq!(ui("issue-90614-accept-allow-text-direction-codepoint-in-comment-lint.rs", "2015", &codepoints(source)), expected);
}

#[test]
fn unknown_prefixes_before_2021_are_linted() {
    // tests/ui/rust-2021/reserved-prefixes-migration.rs
    let source = r#"//@ check-pass
//@ run-rustfix
//@ edition:2018

#![warn(rust_2021_prefixes_incompatible_syntax)]

macro_rules! m2 {
    ($a:tt $b:tt) => {};
}

macro_rules! m3 {
    ($a:tt $b:tt $c:tt) => {};
}

fn main() {
    m2!(z"hey");
    //~^ WARNING prefix `z` is unknown [rust_2021_prefixes_incompatible_syntax]
    //~| WARNING hard error in Rust 2021
    m2!(prefix"hey");
    //~^ WARNING prefix `prefix` is unknown [rust_2021_prefixes_incompatible_syntax]
    //~| WARNING hard error in Rust 2021
    m3!(hey#123);
    //~^ WARNING prefix `hey` is unknown [rust_2021_prefixes_incompatible_syntax]
    //~| WARNING hard error in Rust 2021
    m3!(hey#hey);
    //~^ WARNING prefix `hey` is unknown [rust_2021_prefixes_incompatible_syntax]
    //~| WARNING hard error in Rust 2021
}

macro_rules! quote {
    (# name = # kind # value) => {};
}

quote! {
    #name = #kind#value
    //~^ WARNING prefix `kind` is unknown [rust_2021_prefixes_incompatible_syntax]
    //~| WARNING hard error in Rust 2021
}
"#;
    let expected = r#"warning: prefix `z` is unknown
  --> $DIR/reserved-prefixes-migration.rs:16:9
   |
LL |     m2!(z"hey");
   |         ^ unknown prefix
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2021!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2021/reserving-syntax.html>
note: the lint level is defined here
  --> $DIR/reserved-prefixes-migration.rs:5:9
   |
LL | #![warn(rust_2021_prefixes_incompatible_syntax)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
help: insert whitespace here to avoid this being parsed as a prefix in Rust 2021
   |
LL |     m2!(z "hey");
   |          +

warning: prefix `prefix` is unknown
  --> $DIR/reserved-prefixes-migration.rs:19:9
   |
LL |     m2!(prefix"hey");
   |         ^^^^^^ unknown prefix
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2021!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2021/reserving-syntax.html>
help: insert whitespace here to avoid this being parsed as a prefix in Rust 2021
   |
LL |     m2!(prefix "hey");
   |               +

warning: prefix `hey` is unknown
  --> $DIR/reserved-prefixes-migration.rs:22:9
   |
LL |     m3!(hey#123);
   |         ^^^ unknown prefix
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2021!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2021/reserving-syntax.html>
help: insert whitespace here to avoid this being parsed as a prefix in Rust 2021
   |
LL |     m3!(hey #123);
   |            +

warning: prefix `hey` is unknown
  --> $DIR/reserved-prefixes-migration.rs:25:9
   |
LL |     m3!(hey#hey);
   |         ^^^ unknown prefix
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2021!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2021/reserving-syntax.html>
help: insert whitespace here to avoid this being parsed as a prefix in Rust 2021
   |
LL |     m3!(hey #hey);
   |            +

warning: prefix `kind` is unknown
  --> $DIR/reserved-prefixes-migration.rs:35:14
   |
LL |     #name = #kind#value
   |              ^^^^ unknown prefix
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2021!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2021/reserving-syntax.html>
help: insert whitespace here to avoid this being parsed as a prefix in Rust 2021
   |
LL |     #name = #kind #value
   |                  +

warning: 5 warnings emitted

"#;
    assert_eq!(ui_with_std("reserved-prefixes-migration.rs", "2018", source), expected);
}

#[test]
fn guarded_strings_before_2024_are_linted() {
    // tests/ui/rust-2024/reserved-guarded-strings-migration.rs
    let source = r####"//@ check-pass
//@ run-rustfix
//@ edition:2021

#![warn(rust_2024_guarded_string_incompatible_syntax)]

macro_rules! demo1 {
    ( $a:tt ) => { println!("one tokens") };
}

macro_rules! demo2 {
    ( $a:tt $b:tt ) => { println!("two tokens") };
}

macro_rules! demo3 {
    ( $a:tt $b:tt $c:tt ) => { println!("three tokens") };
}

macro_rules! demo4 {
    ( $a:tt $b:tt $c:tt $d:tt ) => { println!("four tokens") };
}

macro_rules! demo5 {
    ( $a:tt $b:tt $c:tt $d:tt $e:tt ) => { println!("five tokens") };
}

macro_rules! demo6 {
    ( $a:tt $b:tt $c:tt $d:tt $e:tt $f:tt ) => { println!("six tokens") };
}


fn main() {
    demo1!("");
    demo2!(# "");
    demo3!(# ""#);
    demo2!(# "foo");
    demo3!(# "foo"#);
    demo2!("foo"#);

    demo3!(## "foo");
    //~^ WARNING reserved token in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    demo4!(### "foo");
    //~^ WARNING reserved token in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    //~| WARNING reserved token in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    demo4!(## "foo"#);
    //~^ WARNING reserved token in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    demo6!(### "foo"##);
    //~^ WARNING reserved token in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    //~| WARNING reserved token in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    //~| WARNING reserved token in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024

    demo4!("foo"###);
    //~^ WARNING reserved token in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    //~| WARNING reserved token in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024

    demo2!(#"");
    //~^ WARNING parsed as a guarded string in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    demo3!(#""#);
    //~^ WARNING parsed as a guarded string in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    demo3!(##"");
    //~^ WARNING parsed as a guarded string in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    //~| WARNING parsed as a guarded string in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    demo2!(#"foo");
    //~^ WARNING parsed as a guarded string in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    demo3!(##"foo");
    //~^ WARNING parsed as a guarded string in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    //~| WARNING parsed as a guarded string in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    demo3!(#"foo"#);
    //~^ WARNING parsed as a guarded string in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    demo4!(##"foo"#);
    //~^ WARNING parsed as a guarded string in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    //~| WARNING parsed as a guarded string in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    demo5!(##"foo"##);
    //~^ WARNING parsed as a guarded string in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    //~| WARNING parsed as a guarded string in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
    //~| WARNING reserved token in Rust 2024 [rust_2024_guarded_string_incompatible_syntax]
    //~| WARNING hard error in Rust 2024
}
"####;
    assert_reports(
        &ui_with_std("reserved-guarded-strings-migration.rs", "2021", source),
        &[warning(40, &[]), warning(43, &[]), warning(43, &[]), warning(48, &[]), warning(51, &[]), warning(51, &[]), warning(51, &[]), warning(59, &[]), warning(59, &[]), warning(65, &[]), warning(68, &[]), warning(71, &[]), warning(71, &[]), warning(76, &[]), warning(79, &[]), warning(79, &[]), warning(84, &[]), warning(87, &[]), warning(87, &[]), warning(92, &[]), warning(92, &[]), warning(92, &[])],
    );
}

#[test]
fn async_as_an_identifier_in_2015_is_linted() {
    // tests/ui/rust-2018/async-ident.rs
    let source = r#"#![allow(dead_code, unused_variables, unused_macro_rules, bad_style)]
#![deny(keyword_idents)]

//@ edition:2015
//@ run-rustfix

fn async() {} //~ ERROR async
//~^ WARN this is accepted in the current edition

macro_rules! foo {
    ($foo:ident) => {};
    ($async:expr, async) => {};
    //~^ ERROR async
    //~| WARN this is accepted in the current edition
}

foo!(async);
//~^ ERROR async
//~| WARN this is accepted in the current edition

mod dont_lint_raw {
    fn r#async() {}
}

mod async_trait {
    trait async {}
    //~^ ERROR async
    //~| WARN this is accepted in the current edition
    struct MyStruct;
    impl async for MyStruct {}
    //~^ ERROR async
    //~| WARN this is accepted in the current edition
}

mod async_static {
    static async: u32 = 0;
    //~^ ERROR async
    //~| WARN this is accepted in the current edition
}

mod async_const {
    const async: u32 = 0;
    //~^ ERROR async
    //~| WARN this is accepted in the current edition
}

struct Foo;
impl Foo { fn async() {} }
    //~^ ERROR async
    //~| WARN this is accepted in the current edition

fn main() {
    struct async {}
    //~^ ERROR async
    //~| WARN this is accepted in the current edition
    let async: async = async {};
    //~^ ERROR async
    //~| WARN this is accepted in the current edition
    //~| ERROR async
    //~| WARN this is accepted in the current edition
    //~| ERROR async
    //~| WARN this is accepted in the current edition
}

#[macro_export]
macro_rules! produces_async {
    () => (pub fn async() {})
    //~^ ERROR async
    //~| WARN this is accepted in the current edition
}

#[macro_export]
macro_rules! consumes_async {
    (async) => (1)
    //~^ ERROR async
    //~| WARN this is accepted in the current edition
}
"#;
    let expected = r#"error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:7:4
   |
LL | fn async() {}
   |    ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>
note: the lint level is defined here
  --> $DIR/async-ident.rs:2:9
   |
LL | #![deny(keyword_idents)]
   |         ^^^^^^^^^^^^^^
   = note: `#[deny(keyword_idents_2018)]` implied by `#[deny(keyword_idents)]`

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:12:19
   |
LL |     ($async:expr, async) => {};
   |                   ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:17:6
   |
LL | foo!(async);
   |      ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:26:11
   |
LL |     trait async {}
   |           ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:30:10
   |
LL |     impl async for MyStruct {}
   |          ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:36:12
   |
LL |     static async: u32 = 0;
   |            ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:42:11
   |
LL |     const async: u32 = 0;
   |           ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:48:15
   |
LL | impl Foo { fn async() {} }
   |               ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:53:12
   |
LL |     struct async {}
   |            ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:56:9
   |
LL |     let async: async = async {};
   |         ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:56:16
   |
LL |     let async: async = async {};
   |                ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:56:24
   |
LL |     let async: async = async {};
   |                        ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:67:19
   |
LL |     () => (pub fn async() {})
   |                   ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident.rs:74:6
   |
LL |     (async) => (1)
   |      ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

error: aborting due to 14 previous errors

"#;
    assert_eq!(ui("async-ident.rs", "2015", source), expected);
}

#[test]
fn async_is_linted_through_the_2018_group() {
    // tests/ui/rust-2018/async-ident-allowed.rs
    let source = r#"//@ edition:2015

#![deny(rust_2018_compatibility)]

// Don't make a suggestion for a raw identifier replacement unless raw
// identifiers are enabled.

fn main() {
    let async = 3; //~ ERROR: is a keyword
    //~^ WARN this is accepted in the current edition
}
"#;
    let expected = r#"error: `async` is a keyword in the 2018 edition
  --> $DIR/async-ident-allowed.rs:9:9
   |
LL |     let async = 3;
   |         ^^^^^ help: you can use a raw identifier to stay compatible: `r#async`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>
note: the lint level is defined here
  --> $DIR/async-ident-allowed.rs:3:9
   |
LL | #![deny(rust_2018_compatibility)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
   = note: `#[deny(keyword_idents_2018)]` implied by `#[deny(rust_2018_compatibility)]`

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("async-ident-allowed.rs", "2015", source), expected);
}

#[test]
fn dyn_as_an_identifier_in_2015_is_linted() {
    // tests/ui/rust-2018/dyn-keyword.rs
    let source = r#"//@ edition:2015
//@ run-rustfix

#![allow(unused_variables)]
#![deny(keyword_idents)]

fn main() {
    let dyn = (); //~ ERROR dyn
    //~^ WARN this is accepted in the current edition
}
"#;
    let expected = r#"error: `dyn` is a keyword in the 2018 edition
  --> $DIR/dyn-keyword.rs:8:9
   |
LL |     let dyn = ();
   |         ^^^ help: you can use a raw identifier to stay compatible: `r#dyn`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>
note: the lint level is defined here
  --> $DIR/dyn-keyword.rs:5:9
   |
LL | #![deny(keyword_idents)]
   |         ^^^^^^^^^^^^^^
   = note: `#[deny(keyword_idents_2018)]` implied by `#[deny(keyword_idents)]`

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("dyn-keyword.rs", "2015", source), expected);
}

#[test]
fn try_as_an_identifier_in_2015_is_linted() {
    // tests/ui/rust-2018/try-ident.rs
    let source = r#"//@ edition: 2015
//@ run-rustfix
//@ check-pass

#![warn(rust_2018_compatibility)]

fn main() {
    try();
    //~^ WARNING `try` is a keyword in the 2018 edition
    //~| WARNING this is accepted in the current edition
}

fn try() {
    //~^ WARNING `try` is a keyword in the 2018 edition
    //~| WARNING this is accepted in the current edition
}
"#;
    let expected = r#"warning: `try` is a keyword in the 2018 edition
  --> $DIR/try-ident.rs:8:5
   |
LL |     try();
   |     ^^^ help: you can use a raw identifier to stay compatible: `r#try`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>
note: the lint level is defined here
  --> $DIR/try-ident.rs:5:9
   |
LL | #![warn(rust_2018_compatibility)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
   = note: `#[warn(keyword_idents_2018)]` implied by `#[warn(rust_2018_compatibility)]`

warning: `try` is a keyword in the 2018 edition
  --> $DIR/try-ident.rs:13:4
   |
LL | fn try() {
   |    ^^^ help: you can use a raw identifier to stay compatible: `r#try`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2018!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2018/new-keywords.html>

warning: 2 warnings emitted

"#;
    assert_eq!(ui("try-ident.rs", "2015", source), expected);
}

#[test]
fn gen_as_an_identifier_in_2015_is_linted() {
    // tests/ui/rust-2024/gen-kw.rs, revision e2015
    let source = r#"//@ revisions: e2015 e2018
//@[e2015] edition: 2015
//@[e2018] edition: 2018

#![deny(rust_2024_compatibility)]

fn gen() {}
//~^ ERROR `gen` is a keyword in the 2024 edition
//[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
//[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!

fn main() {
    let gen = r#gen;
    //~^ ERROR `gen` is a keyword in the 2024 edition
    //[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
    //[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
}

macro_rules! t {
    () => { mod test { fn gen() {} } }
    //~^ ERROR `gen` is a keyword in the 2024 edition
    //[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
    //[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
}

fn test<'gen>(_: &'gen i32) {}
//~^ ERROR `gen` is a keyword in the 2024 edition
//~| ERROR `gen` is a keyword in the 2024 edition
//[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
//[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
//[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
//[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!

struct Test<'gen>(Box<Test<'gen>>, &'gen ());
//~^ ERROR `gen` is a keyword in the 2024 edition
//~| ERROR `gen` is a keyword in the 2024 edition
//~| ERROR `gen` is a keyword in the 2024 edition
//[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
//[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
//[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
//[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
//[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
//[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!

t!();
"#;
    let expected = r#"error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:7:4
   |
LL | fn gen() {}
   |    ^^^ help: you can use a raw identifier to stay compatible: `r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>
note: the lint level is defined here
  --> $DIR/gen-kw.rs:5:9
   |
LL | #![deny(rust_2024_compatibility)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
   = note: `#[deny(keyword_idents_2024)]` implied by `#[deny(rust_2024_compatibility)]`

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:13:9
   |
LL |     let gen = r#gen;
   |         ^^^ help: you can use a raw identifier to stay compatible: `r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:20:27
   |
LL |     () => { mod test { fn gen() {} } }
   |                           ^^^ help: you can use a raw identifier to stay compatible: `r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:26:9
   |
LL | fn test<'gen>(_: &'gen i32) {}
   |         ^^^^ help: you can use a raw identifier to stay compatible: `'r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:26:19
   |
LL | fn test<'gen>(_: &'gen i32) {}
   |                   ^^^^ help: you can use a raw identifier to stay compatible: `'r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:34:13
   |
LL | struct Test<'gen>(Box<Test<'gen>>, &'gen ());
   |             ^^^^ help: you can use a raw identifier to stay compatible: `'r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:34:28
   |
LL | struct Test<'gen>(Box<Test<'gen>>, &'gen ());
   |                            ^^^^ help: you can use a raw identifier to stay compatible: `'r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:34:37
   |
LL | struct Test<'gen>(Box<Test<'gen>>, &'gen ());
   |                                     ^^^^ help: you can use a raw identifier to stay compatible: `'r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: aborting due to 8 previous errors

"#;
    assert_eq!(ui_with_std("gen-kw.rs", "2015", source), expected);
}

#[test]
fn gen_as_an_identifier_in_2018_is_linted() {
    // tests/ui/rust-2024/gen-kw.rs, revision e2018
    let source = r#"//@ revisions: e2015 e2018
//@[e2015] edition: 2015
//@[e2018] edition: 2018

#![deny(rust_2024_compatibility)]

fn gen() {}
//~^ ERROR `gen` is a keyword in the 2024 edition
//[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
//[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!

fn main() {
    let gen = r#gen;
    //~^ ERROR `gen` is a keyword in the 2024 edition
    //[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
    //[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
}

macro_rules! t {
    () => { mod test { fn gen() {} } }
    //~^ ERROR `gen` is a keyword in the 2024 edition
    //[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
    //[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
}

fn test<'gen>(_: &'gen i32) {}
//~^ ERROR `gen` is a keyword in the 2024 edition
//~| ERROR `gen` is a keyword in the 2024 edition
//[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
//[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
//[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
//[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!

struct Test<'gen>(Box<Test<'gen>>, &'gen ());
//~^ ERROR `gen` is a keyword in the 2024 edition
//~| ERROR `gen` is a keyword in the 2024 edition
//~| ERROR `gen` is a keyword in the 2024 edition
//[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
//[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
//[e2015]~| WARNING this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
//[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
//[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
//[e2018]~| WARNING this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!

t!();
"#;
    let expected = r#"error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:7:4
   |
LL | fn gen() {}
   |    ^^^ help: you can use a raw identifier to stay compatible: `r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>
note: the lint level is defined here
  --> $DIR/gen-kw.rs:5:9
   |
LL | #![deny(rust_2024_compatibility)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
   = note: `#[deny(keyword_idents_2024)]` implied by `#[deny(rust_2024_compatibility)]`

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:13:9
   |
LL |     let gen = r#gen;
   |         ^^^ help: you can use a raw identifier to stay compatible: `r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:20:27
   |
LL |     () => { mod test { fn gen() {} } }
   |                           ^^^ help: you can use a raw identifier to stay compatible: `r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:26:9
   |
LL | fn test<'gen>(_: &'gen i32) {}
   |         ^^^^ help: you can use a raw identifier to stay compatible: `'r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:26:19
   |
LL | fn test<'gen>(_: &'gen i32) {}
   |                   ^^^^ help: you can use a raw identifier to stay compatible: `'r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:34:13
   |
LL | struct Test<'gen>(Box<Test<'gen>>, &'gen ());
   |             ^^^^ help: you can use a raw identifier to stay compatible: `'r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:34:28
   |
LL | struct Test<'gen>(Box<Test<'gen>>, &'gen ());
   |                            ^^^^ help: you can use a raw identifier to stay compatible: `'r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: `gen` is a keyword in the 2024 edition
  --> $DIR/gen-kw.rs:34:37
   |
LL | struct Test<'gen>(Box<Test<'gen>>, &'gen ());
   |                                     ^^^^ help: you can use a raw identifier to stay compatible: `'r#gen`
   |
   = warning: this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/gen-keyword.html>

error: aborting due to 8 previous errors

"#;
    assert_eq!(ui_with_std("gen-kw.rs", "2018", source), expected);
}

#[test]
fn a_metavariable_named_gen_is_not_linted() {
    // tests/ui/rust-2024/gen-kw-in-macro.rs
    let source = r#"//@ check-pass

#![deny(keyword_idents_2024)]

macro_rules! foo {
    ($gen:expr) => {
        $gen
    };
}

fn main() {
    foo!(println!("hello, world"));
}
"#;
    let expected = "";
    assert_eq!(ui_with_std("gen-kw-in-macro.rs", "2015", source), expected);
}

#[test]
fn a_raw_lifetime_named_gen_is_not_linted() {
    // tests/ui/rust-2024/raw-gen-lt.rs
    let source = r#"//@ edition: 2021
//@ check-pass

#![deny(keyword_idents_2024)]

fn foo<'r#gen>() {}

fn main() {}
"#;
    let expected = "";
    assert_eq!(ui("raw-gen-lt.rs", "2021", source), expected);
}

#[test]
fn non_ascii_identifiers_are_linted_where_first_seen() {
    // tests/ui/lint/rfc-2457-non-ascii-idents/lint-non-ascii-idents.rs
    let source = r#"#![deny(non_ascii_idents)]

const חלודה: usize = 2; //~ ERROR identifier contains non-ASCII characters

fn coöperation() {} //~ ERROR identifier contains non-ASCII characters

fn main() {
    let naïveté = 2; //~ ERROR identifier contains non-ASCII characters

    // using the same identifier the second time won't trigger the lint.
    println!("{}", naïveté);
}
"#;
    let expected = r#"error: identifier contains non-ASCII characters
  --> $DIR/lint-non-ascii-idents.rs:3:7
   |
LL | const חלודה: usize = 2;
   |       ^^^^^
   |
note: the lint level is defined here
  --> $DIR/lint-non-ascii-idents.rs:1:9
   |
LL | #![deny(non_ascii_idents)]
   |         ^^^^^^^^^^^^^^^^

error: identifier contains non-ASCII characters
  --> $DIR/lint-non-ascii-idents.rs:5:4
   |
LL | fn coöperation() {}
   |    ^^^^^^^^^^^

error: identifier contains non-ASCII characters
  --> $DIR/lint-non-ascii-idents.rs:8:9
   |
LL |     let naïveté = 2;
   |         ^^^^^^^

error: aborting due to 3 previous errors

"#;
    assert_eq!(ui_with_std("lint-non-ascii-idents.rs", "2015", source), expected);
}

#[test]
fn uncommon_codepoints_are_linted_by_kind() {
    // tests/ui/lint/rfc-2457-non-ascii-idents/lint-uncommon-codepoints.rs
    let source = r#"#![deny(uncommon_codepoints)]

const µ: f64 = 0.000001; //~ ERROR identifier contains a non normalized (NFKC) character: 'µ'
//~| WARNING should have an upper case name

fn dĳkstra() {}
//~^ ERROR identifier contains a non normalized (NFKC) character: 'ĳ'

fn main() {
    let ㇻㇲㇳ = "rust"; //~ ERROR identifier contains uncommon characters: 'ㇻ', 'ㇲ', and 'ㇳ'

    // using the same identifier the second time won't trigger the lint.
    println!("{}", ㇻㇲㇳ);
}
"#;
    let expected = r#"error: identifier contains a non normalized (NFKC) character: 'µ'
  --> $DIR/lint-uncommon-codepoints.rs:3:7
   |
LL | const µ: f64 = 0.000001;
   |       ^
   |
   = note: this character is included in the Not_NFKC Unicode general security profile
note: the lint level is defined here
  --> $DIR/lint-uncommon-codepoints.rs:1:9
   |
LL | #![deny(uncommon_codepoints)]
   |         ^^^^^^^^^^^^^^^^^^^

error: identifier contains a non normalized (NFKC) character: 'ĳ'
  --> $DIR/lint-uncommon-codepoints.rs:6:4
   |
LL | fn dĳkstra() {}
   |    ^^^^^^^
   |
   = note: this character is included in the Not_NFKC Unicode general security profile

error: identifier contains uncommon characters: 'ㇻ', 'ㇲ', and 'ㇳ'
  --> $DIR/lint-uncommon-codepoints.rs:10:9
   |
LL |     let ㇻㇲㇳ = "rust";
   |         ^^^^^^
   |
   = note: these characters are included in the Unicode general security profile

warning: constant `µ` should have an upper case name
  --> $DIR/lint-uncommon-codepoints.rs:3:7
   |
LL | const µ: f64 = 0.000001;
   |       ^ help: convert the identifier to upper case: `Μ`
   |
   = note: `#[warn(non_upper_case_globals)]` (part of `#[warn(nonstandard_style)]`) on by default

error: aborting due to 3 previous errors; 1 warning emitted

"#;
    assert_eq!(ui_with_std("lint-uncommon-codepoints.rs", "2015", source), expected);
}

#[test]
fn confusable_identifiers_are_linted() {
    // tests/ui/lint/rfc-2457-non-ascii-idents/lint-confusable-idents.rs
    let source = r#"#![deny(confusable_idents)]
#![allow(uncommon_codepoints, non_upper_case_globals)]

const ｓ: usize = 42;
const s_s: usize = 42;

fn main() {
    let s = "rust"; //~ ERROR found both
    let ｓ_ｓ = "rust2"; //~ ERROR found both
    not_affected();
}

fn not_affected() {
    let s1 = 1;
    let sl = 'l';
}
"#;
    let expected = r#"error: found both `ｓ` and `s` as identifiers, which look alike
  --> $DIR/lint-confusable-idents.rs:8:9
   |
LL | const ｓ: usize = 42;
   |       -- other identifier used here
...
LL |     let s = "rust";
   |         ^ this identifier can be confused with `ｓ`
   |
note: the lint level is defined here
  --> $DIR/lint-confusable-idents.rs:1:9
   |
LL | #![deny(confusable_idents)]
   |         ^^^^^^^^^^^^^^^^^

error: found both `s_s` and `ｓ_ｓ` as identifiers, which look alike
  --> $DIR/lint-confusable-idents.rs:9:9
   |
LL | const s_s: usize = 42;
   |       --- other identifier used here
...
LL |     let ｓ_ｓ = "rust2";
   |         ^^^^^ this identifier can be confused with `s_s`

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("lint-confusable-idents.rs", "2015", source), expected);
}

#[test]
fn scripts_used_only_by_confusables_are_linted() {
    // tests/ui/lint/rfc-2457-non-ascii-idents/lint-mixed-script-confusables.rs
    let source = r#"#![deny(mixed_script_confusables)]

struct ΑctuallyNotLatin;
//~^ ERROR the usage of Script Group `Greek` in this crate consists solely of

fn main() {
    let v = ΑctuallyNotLatin;
}

mod роре {
//~^ ERROR the usage of Script Group `Cyrillic` in this crate consists solely of
    const エ: &'static str = "アイウ";
    //~^ ERROR the usage of Script Group `Japanese, Katakana` in this crate consists solely of
}
"#;
    let expected = r#"error: the usage of Script Group `Greek` in this crate consists solely of mixed script confusables
  --> $DIR/lint-mixed-script-confusables.rs:3:8
   |
LL | struct ΑctuallyNotLatin;
   |        ^^^^^^^^^^^^^^^^
   |
   = note: the usage includes 'Α' (U+0391)
   = note: please recheck to make sure their usages are indeed what you want
note: the lint level is defined here
  --> $DIR/lint-mixed-script-confusables.rs:1:9
   |
LL | #![deny(mixed_script_confusables)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^

error: the usage of Script Group `Cyrillic` in this crate consists solely of mixed script confusables
  --> $DIR/lint-mixed-script-confusables.rs:10:5
   |
LL | mod роре {
   |     ^^^^
   |
   = note: the usage includes 'е' (U+0435), 'о' (U+043E), 'р' (U+0440)
   = note: please recheck to make sure their usages are indeed what you want

error: the usage of Script Group `Japanese, Katakana` in this crate consists solely of mixed script confusables
  --> $DIR/lint-mixed-script-confusables.rs:12:11
   |
LL |     const エ: &'static str = "アイウ";
   |           ^^
   |
   = note: the usage includes 'エ' (U+30A8)
   = note: please recheck to make sure their usages are indeed what you want

error: aborting due to 3 previous errors

"#;
    assert_eq!(ui("lint-mixed-script-confusables.rs", "2015", source), expected);
}

#[test]
fn scripts_used_otherwise_too_are_not_linted() {
    // tests/ui/lint/rfc-2457-non-ascii-idents/lint-mixed-script-confusables-2.rs
    let source = r#"//@ check-pass
#![deny(mixed_script_confusables)]

struct ΑctuallyNotLatin;

fn main() {
    let λ = 42; // this usage of Greek confirms that Greek is used intentionally.
}

mod роре {
    const エ: &'static str = "アイウ";

    // this usage of Katakana confirms that Katakana is used intentionally.
    fn ニャン() {
        let д: usize = 100; // this usage of Cyrillic confirms that Cyrillic is used intentionally.

        println!("meow!");
    }
}
"#;
    let expected = "";
    assert_eq!(ui_with_std("lint-mixed-script-confusables-2.rs", "2015", source), expected);
}

#[test]
fn a_module_file_does_not_take_the_crate_roots_level_for_keyword_idents() {
    // tests/ui/lint/keyword-idents/multi-file.rs (a known bug of rustc's, #132218: the
    // pre-expansion pass of a module file starts from the command line's levels)
    let fixture = r#"//- /multi-file.rs crate:other:main edition:2015
#![deny(keyword_idents)] // Should affect the submodule, but doesn't.
//@ edition: 2015
//@ known-bug: #132218
//@ check-pass (known bug; should be check-fail)

// Because `keyword_idents_2018` and `keyword_idents_2024` are pre-expansion
// lints, configuring them via lint attributes doesn't propagate to submodules
// in other files.
// <https://github.com/rust-lang/rust/issues/132218>

#[path = "./auxiliary/multi_file_submod.rs"]
mod multi_file_submod;

fn main() {}
//- /auxiliary/multi_file_submod.rs
// Submodule file used by test `../multi-file.rs`.

// Keywords reserved from Rust 2018:
fn async() {}
fn await() {}
fn try() {}
fn dyn() {}

// Keywords reserved from Rust 2024:
fn gen() {}
"#;
    assert_reports(&stderr(fixture), &[]);
}
