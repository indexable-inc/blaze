//! A head's tokens digest ([`unencoded`]) by `token_digest` (docs/v2/plan/design/token-keys.md):
//! `1 >> 2` and `1 > > 2` were one digest when only lexemes were hashed (two programs under one
//! key, rule 21); a comment or a re-indent inside a signature moves nothing.

use blaze_surface::Edition;

use super::{piece_digest, unencoded};

/// [`unencoded`] of the first top-level item of `text`.
fn first_head(text: &str) -> blaze_ids::ContentId {
    let outline = blaze_file_items::outline(text, Edition::E2021, blaze_own::testing::heap().alloc());
    let (at, _) = outline.children(None).next().expect("the text has an item");
    unencoded(&outline, at, text, Edition::E2021)
}

#[test]
fn a_split_shift_moves_a_head() {
    assert_ne!(
        piece_digest("[u8; 1 >> 2]", Edition::E2021),
        piece_digest("[u8; 1 > > 2]", Edition::E2021),
        "a piece: `>>` and `> >`"
    );
    assert_ne!(
        first_head("pub struct S { a: [u8; 1 >> 2] }\n"),
        first_head("pub struct S { a: [u8; 1 > > 2] }\n"),
        "a struct's head: `>>` and `> >`"
    );
}

#[test]
fn a_comment_or_reindent_in_a_head_moves_nothing() {
    assert_eq!(
        first_head("pub struct S { a: [u8; 1 >> 2] }\n"),
        first_head("pub struct S {\n    a: [u8; 1 >> /* two */ 2] // why\n}\n"),
        "a struct's head re-indented, with two comments"
    );
    assert_eq!(
        first_head("pub fn f(a: u8) -> u8 { a }\n"),
        first_head("pub fn f(\n    a: u8 // the one\n) -> u8 {\n    a\n}\n"),
        "a fn's signature re-indented, with a comment"
    );
}

#[test]
fn the_edition_is_in_a_piece() {
    assert_ne!(piece_digest("async fn f()", Edition::E2015), piece_digest("async fn f()", Edition::E2021));
}
