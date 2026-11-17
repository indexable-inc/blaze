//! The item a derive sees: a field or variant whose `cfg` is false is removed WITH EVERY
//! ATTRIBUTE IT CARRIES, in either order of the `cfg` and the other attributes.
//!
//! The Reference, "Conditional compilation": "If the predicate is false, the thing is removed
//! from the source code"; "Procedural macros", derive macros: the input is the item the derive
//! is on after `cfg` and `cfg_attr` are applied; "Attributes", doc comments: "Doc comments are
//! interpreted as a special syntax for attributes" (a `#[doc]` of the same field). tempo
//! bin/tempo/src/cli.rs:127-138 (`#[command(flatten)] #[cfg(feature = "pyroscope")] ..` and
//! six doc comments before `#[cfg(feature = "custom-pcrs")] #[arg(..)] ..`) gave clap_derive's
//! `Args` attributes with no field after them ("unexpected end of input, expected
//! identifier") while the attributes before a false `cfg` stayed in the input.
//!
//! `cargo test -p blaze-expand --test derive_input` prints
//! "test a_cfg_false_field_goes_with_every_attribute_it_carries ... ok" and
//! "test a_cfg_attr_leaving_a_false_cfg_drops_the_attributes_before_it ... ok".

#![feature(allocator_api)]

use blaze_expand::{Delim, Edition, ItemAttrs, TokenKind, TokenStream, lex};

fn tokens(text: &str) -> TokenStream<'static> {
    lex(text, Edition::E2021, blaze_own::testing::heap().alloc()).expect("the test's text lexes")
}

/// Each token's text, a space between: punctuation by its character, a literal by its text
/// (a string's inside), a delimiter by its character.
fn show(stream: &TokenStream<'_>) -> String {
    let mut out: Vec<String> = Vec::new();
    for token in stream.tokens() {
        out.push(match token.kind {
            TokenKind::Punct { char, .. } => char.to_string(),
            TokenKind::Open(Delim::Paren) => "(".to_owned(),
            TokenKind::Open(Delim::Brace) => "{".to_owned(),
            TokenKind::Open(Delim::Bracket) => "[".to_owned(),
            TokenKind::Close(Delim::Paren) => ")".to_owned(),
            TokenKind::Close(Delim::Brace) => "}".to_owned(),
            TokenKind::Close(Delim::Bracket) => "]".to_owned(),
            TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible) => "~".to_owned(),
            TokenKind::Ident { .. } | TokenKind::Literal { .. } => stream.text_of(token).to_owned(),
        });
    }
    out.join(" ")
}

/// The derive input of `text` under a cfg set where `unix` holds and nothing else does.
fn input(text: &str) -> String {
    let mut unix = |name: &str, _: Option<&str>| Ok::<bool, ()>(name == "unix");
    let derives = ItemAttrs::new(tokens(text)).derives(&mut unix).expect("the attributes read").expect("the item stays");
    show(&derives.input)
}

/// The brief's shape (`#[a] #[cfg(any())] f`, `#[cfg(any())] #[b] g`), and tempo's: the
/// attributes before a false `cfg` go with the field, a doc comment among them; a kept field
/// keeps the attributes before its true `cfg`.
#[test]
fn a_cfg_false_field_goes_with_every_attribute_it_carries() {
    assert_eq!(input("#[derive(X)] struct S { #[a] #[cfg(any())] f: u8, #[cfg(any())] #[b] g: u8, h: u8 }"), "struct S { h : u8 }");
    assert_eq!(
        input(
            "#[derive(clap::Args)] struct T {
                a: u8,
                #[command(flatten)]
                #[cfg(feature = \"pyroscope\")]
                p: P,
                /// Replace the policy.
                #[cfg(feature = \"custom-pcrs\")]
                #[arg(long)]
                c: Option<C>,
            }"
        ),
        "struct T { a : u8 , }"
    );
    assert_eq!(input("#[derive(X)] struct K { #[serde(y)] #[cfg(unix)] k: u8 }"), "struct K { # [ serde ( y ) ] k : u8 }");
    // A variant and a tuple field alike.
    assert_eq!(input("#[derive(X)] enum E { #[a] #[cfg(windows)] V(u8), W(#[b] #[cfg(windows)] u8, u16) }"), "enum E { W ( u16 ) }");
}

/// A `cfg` a `#[cfg_attr]` leaves removes the field the same way: the attributes written
/// before the `cfg_attr` go too; one that holds leaves them, and its other metas, in place.
#[test]
fn a_cfg_attr_leaving_a_false_cfg_drops_the_attributes_before_it() {
    assert_eq!(
        input("#[derive(X)] struct S { #[serde(x)] #[cfg_attr(unix, cfg(windows))] w: u8, #[serde(z)] #[cfg_attr(unix, cfg(unix), serde(q))] u: u8 }"),
        "struct S { # [ serde ( z ) ] # [ serde ( q ) ] u : u8 }"
    );
}
