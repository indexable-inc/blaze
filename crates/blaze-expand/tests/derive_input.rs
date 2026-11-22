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

/// tempo-chainspec t1545 (18-30 tests): reth-ethereum-forks' `ChainHardforks`, `#[derive(Default,
/// Clone, PartialEq, Eq)]` over two fields named `map` under opposite `cfg`s, each a map type
/// whose arguments hold a comma. The Reference, "Structs": a field is `IDENTIFIER : Type`, and
/// the comma inside `<..>` is the type's, not the field list's (`<` and `>` delimit no token
/// tree). The false `cfg` removed only `map: BTreeMap<&'static str,` and left
/// `ForkCondition>,` in the derive's input, which adt.rs refused, so no derive of the type
/// wrote its impl and `ChainHardforks: Default` met core's `impl<T: ArbitraryInt> Default for
/// T` alone ("`ChainHardforks: ArbitraryInt` because `ChainHardforks: Default`").
/// The site (the false `cfg` second), its twin (the false `cfg` first), and the CONTROL: a
/// removed field whose type holds no comma inside `<..>`, which the walk counting groups only
/// removed whole before the fix too (the cause removed, the effect gone: rule 23).
#[test]
fn a_cfg_false_field_whose_type_holds_a_comma_inside_angles_goes_whole() {
    let site = input(
        "#[derive(Default, Clone, PartialEq, Eq)]
        pub struct ChainHardforks {
            forks: Vec<(Box<dyn Hardfork>, ForkCondition)>,
            #[cfg(unix)]
            map: FxHashMap<&'static str, ForkCondition>,
            #[cfg(not(unix))]
            map: alloc::collections::BTreeMap<&'static str, ForkCondition>,
        }",
    );
    assert_eq!(
        site,
        "pub struct ChainHardforks { forks : Vec < ( Box < dyn Hardfork > , ForkCondition ) > , map : FxHashMap < & ' static str , ForkCondition > , }"
    );
    let twin = input(
        "#[derive(Default)] struct S {
            #[cfg(not(unix))]
            map: alloc::collections::BTreeMap<&'static str, ForkCondition>,
            #[cfg(unix)]
            map: FxHashMap<&'static str, ForkCondition>,
        }",
    );
    assert_eq!(twin, "struct S { map : FxHashMap < & ' static str , ForkCondition > , }");
    let control = input("#[derive(Default)] struct S { #[cfg(not(unix))] map: Single<u8>, g: u8 }");
    assert_eq!(control, "struct S { g : u8 }");
    std::println!("derive input: a removed `map: BTreeMap<&'static str, ForkCondition>,` goes whole; site {site:?}");
}

/// What else the element's end is read through, each as adt.rs `split` reads the same pieces:
/// a `->` closes no angle (`fn(u8) -> HashMap<u8, u16>`); after a lone `=` the element is an
/// expression whose `<` is a shift (`A = 1 << 2`), counted as no angle; a `>` closing no angle
/// of the element closes the generic parameter list it is in.
#[test]
fn an_element_end_reads_arrows_discriminants_and_generic_lists() {
    assert_eq!(input("#[derive(X)] struct S { #[cfg(windows)] f: fn(u8) -> HashMap<u8, u16>, g: u8 }"), "struct S { g : u8 }");
    assert_eq!(input("#[derive(X)] enum E { #[cfg(windows)] A = 1 << 2, B = 3 }"), "enum E { B = 3 }");
    assert_eq!(input("#[derive(X)] struct S<A, #[cfg(windows)] B: Tr<C, D>> { a: A }"), "struct S < A , > { a : A }");
    assert_eq!(input("#[derive(X)] struct T(#[cfg(windows)] HashMap<u8, u16>, u32);"), "struct T ( u32 ) ;");
}

/// The cases the old walk (`element_end`, deleted: it counted `<` and `>` until a lone `=`,
/// then ended the element at the next comma outside a group) got wrong, each now the parser's
/// element whole. CONTROL, the old walk's answers on the same inputs:
/// - `struct S<#[cfg(windows)] T = HashMap<K, V>, U>`: it ended `T` at `K,`, leaving
///   `struct S < V > , U > ..`.
/// - `struct S { #[cfg(windows)] f: u8 = g::<A, B>(), h: u8 }`: it ended `f` at `A,`, leaving
///   `struct S { B > ( ) , h : u8 }`.
/// - `struct S { #[cfg(windows)] f: fn(u8, u8) -> u8 = |a, b| a, h: u8 }`: it ended `f` at
///   `a,`, leaving `struct S { b | a , h : u8 }`.
/// - a `#[cfg]` after a `;` in a default's block: no element started there for it, so the false
///   `cfg` and its statement stayed.
#[test]
fn an_element_is_the_parsers_whole() {
    assert_eq!(input("#[derive(X)] struct S<#[cfg(windows)] T = HashMap<K, V>, U> { u: U }"), "struct S < U > { u : U }");
    assert_eq!(input("#[derive(X)] struct S { #[cfg(windows)] f: u8 = g::<A, B>(), h: u8 }"), "struct S { h : u8 }");
    assert_eq!(input("#[derive(X)] struct S { #[cfg(windows)] f: fn(u8, u8) -> u8 = |a, b| a, h: u8 }"), "struct S { h : u8 }");
    assert_eq!(
        input("#[derive(X)] struct S { f: u8 = { let a = 1; #[cfg(windows)] let b = 2; a }, h: u8 }"),
        "struct S { f : u8 = { let a = 1 ; a } , h : u8 }"
    );
}

/// The negative twins: an item that does not parse is a typed refusal naming the element and
/// the token, never an element cut short or a derive input with a piece of one left in it.
#[test]
fn a_malformed_element_is_a_typed_refusal() {
    use blaze_expand::{Extent, ExtentError, ExtentWhy, ItemAttrsError};
    let refusal = |text: &str| {
        let mut unix = |name: &str, _: Option<&str>| Ok::<bool, ()>(name == "unix");
        match ItemAttrs::new(tokens(text)).derives(&mut unix) {
            Err(ItemAttrsError::Syntax(error)) => error,
            Err(other) => panic!("{text}: refused, but not as syntax: {other}"),
            Ok(_) => panic!("{text}: read, though it does not parse"),
        }
    };
    let field = refusal("#[derive(X)] struct S { #[cfg(windows)] f: , h: u8 }");
    assert!(matches!(field, ExtentError { what: Extent::Field, why: ExtentWhy::Refused, .. }), "{field}");
    let param = refusal("#[derive(X)] struct S<#[cfg(windows)] T = , U> { u: U }");
    assert!(matches!(param, ExtentError { what: Extent::GenericParam, .. }), "{param}");
    let variant = refusal("#[derive(X)] enum E { #[cfg(windows)] A = , B }");
    assert!(matches!(variant, ExtentError { what: Extent::Variant, .. }), "{variant}");
    let trailing = refusal("#[derive(X)] struct S; struct T;");
    assert!(matches!(trailing, ExtentError { what: Extent::Item, why: ExtentWhy::Trailing, .. }), "{trailing}");
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
