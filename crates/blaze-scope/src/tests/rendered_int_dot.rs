//! An item a `macro_rules!` writes is rendered to text (resolve.rs `render_with`) that lexes
//! back to the same tokens. The pinned core's `_fmt_inner` (library/core/src/fmt/num.rs)
//! writes `999.try_into()` in a macro's body: `999` touches the `.`, so by spacing alone the two
//! are glued and the `.` stands apart before `try_into`, giving `999. try_into ( )`, which the
//! lexer reads as the float `999.` and a name (the Reference, "Tokens", "Floating-point
//! literals": `DEC_LITERAL .` not followed by `.`, `_` or an identifier is a float, and the
//! renderer's space puts no identifier right after it). `blaze_expand::glued_in_text` keeps an
//! unsuffixed integer apart from a following `.`. CONTROL: with the spacing-only rule back in
//! `render_with`, `999` lexes back as a `Float` and this fails.
//! Expected: `test tests::rendered_int_dot::an_integer_before_a_dot_in_an_expansion_stays_an_integer ... ok`.

use blaze_expand::{LitKind, TokenKind};
use blaze_own::Alloc;
use blaze_surface::{Namespace, PathKind};

use super::entry::{Texts, input, resolved};
use crate::names::Written;

const LIB: &str = "#![no_core]
macro_rules! write_f { () => { pub fn f() -> u32 { 999.try_into() } } }
write_f!();
macro_rules! pass_item { ($($t:tt)*) => { $($t)* }; }
pass_item! {
    /// \"###############################################################################################################################################################################################################################################################
    pub fn g() {}
}
macro_rules! gen_mac {
    () => {
        macro_rules! with_doc_comment {
            (
                /// line doc inside generated matcher
                /** multi
                line block doc inside generated matcher */
                $name:ident
            ) => { pub fn $name() {} };
        }
        macro_rules! with_explicit_doc_attr {
            (#[doc = r\" attr\"] $name:ident) => { pub fn $name() {} };
        }
    };
}
gen_mac!();
with_doc_comment!(from_doc_comment);
with_explicit_doc_attr!(
    /// attr
    from_explicit_attr
);
with_explicit_doc_attr!(missing_attr);
";

static TEXTS: Texts = Texts(&[&[("/m/lib.rs", LIB)]]);

fn item_stream(name: &str) -> (&'static str, blaze_expand::TokenStream<'static>) {
    let alloc = Alloc::global();
    let crates = [input("m", "/m/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    let key = names
        .item_at(names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, alloc)
        .unwrap_or_else(|e| panic!("`{name}`: {e}"))
        .def
        .key;
    let text = names.item_text(key).unwrap_or_else(|| panic!("`{name}` has no text")).text;
    let stream = blaze_expand::lex(text, crate::Edition::E2021, alloc).unwrap_or_else(|e| panic!("`{text}` lexes: {e:?}"));
    (text, stream)
}

#[test]
fn an_integer_before_a_dot_in_an_expansion_stays_an_integer() {
    let (text, stream) = item_stream("f");
    let tokens = stream.tokens();
    let at = tokens
        .iter()
        .position(|t| stream.text_of(t).starts_with("999"))
        .unwrap_or_else(|| panic!("no `999` in the rendered `{text}`"));
    assert!(
        matches!(tokens[at].kind, TokenKind::Literal { kind: LitKind::Integer, .. }),
        "the integer lexed back as {:?} in the rendered `{text}`",
        tokens[at].kind
    );
    // A punctuation token's text is empty (blaze-expand `TokenKind::Punct`): its char says it.
    assert!(
        matches!(tokens.get(at + 1).map(|t| t.kind), Some(TokenKind::Punct { char: '.', .. })),
        "a `.` follows `999` in the rendered `{text}`: {:?}",
        tokens.get(at + 1).map(|t| t.kind)
    );
}

#[test]
fn a_saturated_255_hash_doc_comment_renders_as_lexable_string() {
    let (text, stream) = item_stream("g");
    let tokens = stream.tokens();
    assert!(
        matches!(tokens.get(4).map(|t| t.kind), Some(TokenKind::Literal { kind: LitKind::Str, suffix_len: 0, .. })),
        "saturated 255-hash doc literal should render as cooked Str in `{text}`, got {:?}",
        tokens.get(4).map(|t| t.kind)
    );
    let alloc = stream.allocator();
    let mut rendered_lit = blaze_own::AString::new_in(alloc);
    blaze_expand::write_token(&stream, &tokens[4], &mut rendered_lit);
    let mut decoded = blaze_own::AString::new_in(alloc);
    assert!(blaze_expand::decode_str_literal(rendered_lit.as_str(), |ch| decoded.push(ch)), "rendered literal decodes");
    let mut want = blaze_own::AString::new_in(alloc);
    want.push_str(" \"");
    for _ in 0..255 {
        want.push('#');
    }
    assert_eq!(decoded.as_str(), want.as_str());
}

#[test]
fn generated_macro_rules_preserves_doc_comment_provenance_in_matcher() {
    let (_, stream) = item_stream("from_doc_comment");
    let (_, _) = item_stream("from_explicit_attr");
    let crates = [input("m", "/m/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    assert!(
        names
            .item_at(
                names.root(0),
                Namespace::Values,
                Written { kind: PathKind::Plain, segments: &["missing_attr"] },
                stream.allocator(),
            )
            .is_err(),
        "explicit #[doc = r\" attr\"] inside generated macro_rules! must still require matching doc input"
    );
}
