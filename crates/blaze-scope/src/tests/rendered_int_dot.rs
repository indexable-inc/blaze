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
";

static TEXTS: Texts = Texts(&[&[("/m/lib.rs", LIB)]]);

#[test]
fn an_integer_before_a_dot_in_an_expansion_stays_an_integer() {
    let crates = [input("m", "/m/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    let key = names
        .item_at(names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["f"] }, Alloc::global())
        .unwrap_or_else(|e| panic!("`f`, written by `write_f!`: {e}"))
        .def
        .key;
    let text = names.item_text(key).unwrap_or_else(|| panic!("`f` has no text")).text;
    let stream = blaze_expand::lex(text, crate::Edition::E2021, Alloc::global()).unwrap_or_else(|e| panic!("`{text}` lexes: {e:?}"));
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
