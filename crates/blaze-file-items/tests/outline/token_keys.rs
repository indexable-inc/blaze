//! Keys from tokens (crate docs, "Identity"): an entry's digests are of its tokens, so trivia
//! moves no key and every token, attribute, doc and edition does; an entry a macro may read the
//! layout of keeps its bytes; twin impls are told apart by their ordinal. CONTROL: the planted
//! `ByBytes` feed puts trivia back, and the comment row moves again.
//!
//! Expected lines: `token keys: 4 trivia edits moved no key of 9 entries`, `token keys: a
//! literal, a head token, a joint, a doc, a cfg and the edition each moved their entry`,
//! `token keys: a comment in a macro's fn moved it alone`, `token keys: an impl inserted before
//! its twin moved the twin's ordinal and its member's chain`, `token keys: under ByBytes the
//! comment moved fn f (the control)`.

use blaze_file_items::{ByBytes, ByTokens, Outline, chain_at};
use blaze_surface::{ContentId, Edition, Item, ItemIndex};

use super::{named, parse, parse_as};

/// A file of the forms a key is made of: a doc comment, an attribute, a body, a head, an impl
/// with a member, a trait, a const's initializer and a struct.
const BASE: &str = "/// one
#[inline]
pub fn f(x: u8) -> u8 {
    x + 1
}

struct S {
    a: u8,
}

impl S {
    fn get(&self) -> u8 {
        self.a
    }
}

trait T {
    fn t(&self);
}

const C: u8 = 7;
";

/// Edits of `BASE` that change only trivia: a line comment in a body, a block comment in a
/// head, a blank line above, and every line reformatted.
const TRIVIA: [(&str, &str); 4] = [
    ("a comment in a body", "/// one\n#[inline]\npub fn f(x: u8) -> u8 {\n    // add one\n    x + 1\n}\n\nstruct S {\n    a: u8,\n}\n\nimpl S {\n    fn get(&self) -> u8 {\n        self.a\n    }\n}\n\ntrait T {\n    fn t(&self);\n}\n\nconst C: u8 = 7;\n"),
    ("a comment in a head", "/// one\n#[inline]\npub fn f(x: u8 /* the input */) -> u8 {\n    x + 1\n}\n\nstruct S {\n    a: u8,\n}\n\nimpl S {\n    fn get(&self) -> u8 {\n        self.a\n    }\n}\n\ntrait T {\n    fn t(&self);\n}\n\nconst C: u8 = 7;\n"),
    ("a blank line above", "\n\n/// one\n#[inline]\npub fn f(x: u8) -> u8 {\n    x + 1\n}\n\nstruct S {\n    a: u8,\n}\n\nimpl S {\n    fn get(&self) -> u8 {\n        self.a\n    }\n}\n\ntrait T {\n    fn t(&self);\n}\n\nconst C: u8 = 7;\n"),
    ("a reformat", "/// one\n#[ inline ]\npub fn f( x : u8 ) -> u8 { x+1 }\nstruct S { a : u8 , }\nimpl S { fn get( & self ) -> u8 { self . a } }\ntrait T { fn t( & self ) ; }\nconst C : u8 = 7 ;\n"),
];

/// Each entry's keys: its digest, its signature digest, its ordinal among equals and its chain.
fn keys(outline: &Outline<'_>, text: &str, at: usize) -> (ContentId, ContentId, u32, ContentId) {
    let item: &Item = &outline.items()[at];
    let index = ItemIndex(u32::try_from(at).expect("a small fixture"));
    (item.hash, item.signature_hash, item.disambiguator, chain_at(outline, index, text))
}

/// The first entry whose keys differ between the two outlines, or none; the counts must match.
fn first_moved(old: (&Outline<'_>, &str), new: (&Outline<'_>, &str)) -> Option<usize> {
    assert_eq!(old.0.items().len(), new.0.items().len(), "the edit changed the entries:\n{}\nto\n{}", old.1, new.1);
    (0..old.0.items().len()).find(|&at| keys(old.0, old.1, at) != keys(new.0, new.1, at))
}

/// Whether the entry named `name` has other digests in `new` than in `old`: (its hash moved, its
/// signature hash moved).
fn moved(old: &str, new: &str, name: &str) -> (bool, bool) {
    moved_as(&parse(old), old, &parse(new), new, name)
}

fn moved_as(old_outline: &Outline<'_>, old: &str, new_outline: &Outline<'_>, new: &str, name: &str) -> (bool, bool) {
    let (_, was) = named(old_outline, old, name);
    let (_, now) = named(new_outline, new, name);
    (was.hash != now.hash, was.signature_hash != now.signature_hash)
}

/// THE COMMENT ROW, THE REFORMAT ROW: a comment in a body or a head, a blank line above and a
/// reformat of every line move no entry's digest, signature digest, ordinal or chain.
#[test]
fn trivia_moves_no_key() {
    let base = parse(BASE);
    for (what, text) in TRIVIA {
        let edited = parse(text);
        let at = first_moved((&base, BASE), (&edited, text));
        assert_eq!(at, None, "{what} moved the keys of entry {at:?}:\n{text}");
    }
    println!("token keys: {} trivia edits moved no key of {} entries", TRIVIA.len(), base.items().len());
}

/// RULE 21: every token, attribute, doc and edition is an input. A literal in `f`'s body moves
/// its digest and not its signature digest, nor `get`'s; a head token moves both; `&&` written
/// as `& &` (one joint apart) moves the body; a doc edit, a cfg and the edition move both.
#[test]
fn every_token_doc_attribute_and_edition_moves_its_entry() {
    const SMALL: &str = "/// one\n#[inline]\npub fn f(x: u8) -> u8 {\n    x + 1\n}\nfn get() -> u8 {\n    2\n}\nconst C: u8 = 7;\n";
    const LITERAL: &str = "/// one\n#[inline]\npub fn f(x: u8) -> u8 {\n    x + 2\n}\nfn get() -> u8 {\n    2\n}\nconst C: u8 = 7;\n";
    const HEAD: &str = "/// one\n#[inline]\npub fn f(x: u8) -> u16 {\n    x + 1\n}\nfn get() -> u8 {\n    2\n}\nconst C: u8 = 7;\n";
    const DOC: &str = "/// two\n#[inline]\npub fn f(x: u8) -> u8 {\n    x + 1\n}\nfn get() -> u8 {\n    2\n}\nconst C: u8 = 7;\n";
    const CFG: &str = "/// one\n#[cfg(unix)]\npub fn f(x: u8) -> u8 {\n    x + 1\n}\nfn get() -> u8 {\n    2\n}\nconst C: u8 = 7;\n";
    const INITIALIZER: &str = "/// one\n#[inline]\npub fn f(x: u8) -> u8 {\n    x + 1\n}\nfn get() -> u8 {\n    2\n}\nconst C: u8 = 8;\n";
    assert_eq!(moved(SMALL, LITERAL, "f"), (true, false), "a literal in the body");
    assert_eq!(moved(SMALL, LITERAL, "get"), (false, false), "a literal in `f` moved `get`");
    assert_eq!(moved(SMALL, HEAD, "f"), (true, true), "a head token");
    let and = "fn j(x: bool, y: bool) -> bool {\n    x && y\n}\n";
    let apart = "fn j(x: bool, y: bool) -> bool {\n    x & &y\n}\n";
    assert_eq!(moved(and, apart, "j"), (true, false), "`&&` and `& &` are other tokens joined");
    assert_eq!(moved(SMALL, DOC, "f"), (true, true), "a doc edit");
    assert_eq!(moved(SMALL, CFG, "f"), (true, true), "an attribute changed to a cfg");
    let (old, new) = (parse_as::<ByTokens>(SMALL, Edition::E2024), parse_as::<ByTokens>(SMALL, Edition::E2021));
    assert_eq!(moved_as(&old, SMALL, &new, SMALL, "f"), (true, true), "the edition");
    assert_eq!(moved(SMALL, INITIALIZER, "C"), (true, false), "a const's initializer is its body");
    println!("token keys: a literal, a head token, a joint, a doc, a cfg and the edition each moved their entry");
}

/// RULE 21, THE EXCEPTION: `stringify!`, `line!` and a proc macro read the layout, so an entry
/// whose own tokens call a macro keeps its bytes: a comment in `m`'s body moves `m`, and leaves
/// the plain `p` beside it; a comment inside a derived struct moves the struct.
#[test]
fn an_entry_that_calls_a_macro_keeps_its_bytes() {
    let old = "fn m() -> &'static str {\n    stringify!(a + b)\n}\nfn p() -> u8 {\n    1\n}\n#[derive(Debug)]\nstruct D {\n    x: u8,\n}\n";
    let new = "fn m() -> &'static str {\n    // what it says\n    stringify!(a + b)\n}\nfn p() -> u8 {\n    // one\n    1\n}\n#[derive(Debug)]\nstruct D {\n    // the one field\n    x: u8,\n}\n";
    assert_eq!(moved(old, new, "m").0, true, "a comment in a macro's fn left it: `stringify!` may print the layout");
    assert_eq!(moved(old, new, "p"), (false, false), "a comment in a plain fn moved it");
    assert_eq!(moved(old, new, "D").0, true, "a comment in a derived struct left it: the derive reads its spans");
    println!("token keys: a comment in a macro's fn moved it alone");
}

/// TWINS BY ORDINAL: two `impl S` blocks are told apart by their ordinal, no offset. An `impl S`
/// inserted before the second moves the second's ordinal from 1 to 2, and with it its member's
/// chain; the first keeps both. CONTROL: an `impl U` inserted there (another head) moves neither.
#[test]
fn an_impl_inserted_before_its_twin_moves_the_twin() {
    let old = "struct S;\nstruct U;\nimpl S {\n    fn a() {}\n}\nimpl S {\n    fn b() {}\n}\n";
    let twin = "struct S;\nstruct U;\nimpl S {\n    fn a() {}\n}\nimpl S {\n    fn c() {}\n}\nimpl S {\n    fn b() {}\n}\n";
    let other = "struct S;\nstruct U;\nimpl S {\n    fn a() {}\n}\nimpl U {\n    fn c() {}\n}\nimpl S {\n    fn b() {}\n}\n";
    let (old_outline, twin_outline, other_outline) = (parse(old), parse(twin), parse(other));
    let impl_of = |outline: &Outline<'_>, text: &str, member: &str| -> (u32, ContentId, ContentId) {
        let (at, item) = named(outline, text, member);
        let parent = item.parent.expect("a member is in its impl");
        (outline.item(parent).disambiguator, chain_at(outline, parent, text), chain_at(outline, at, text))
    };
    let (first, second) = (impl_of(&old_outline, old, "a"), impl_of(&old_outline, old, "b"));
    assert_eq!((first.0, second.0), (0, 1), "the twins' ordinals");
    assert_ne!(first.1, second.1, "the twins share a chain");
    let inserted = impl_of(&twin_outline, twin, "b");
    assert_eq!(inserted.0, 2, "an `impl S` inserted before the twin left its ordinal");
    assert_ne!(inserted.2, second.2, "the twin's member kept its chain though its impl's ordinal moved");
    assert_eq!(impl_of(&twin_outline, twin, "a"), first, "the first twin moved");
    assert_eq!(impl_of(&other_outline, other, "b"), second, "an `impl U` inserted there moved the twin");
    println!("token keys: an impl inserted before its twin moved the twin's ordinal and its member's chain");
}

/// CONTROL: under the planted `ByBytes` feed (every entry by its bytes, the keys before tokens)
/// the comment row moves `f`'s digest again, so `trivia_moves_no_key` is not passing for a key
/// that holds nothing.
#[test]
fn under_bytes_a_comment_moves_its_entry() {
    let (_, text) = TRIVIA[0];
    let (old, new) = (parse_as::<ByBytes>(BASE, Edition::E2024), parse_as::<ByBytes>(text, Edition::E2024));
    assert_eq!(moved_as(&old, BASE, &new, text, "f"), (true, false), "under ByBytes the comment in `f`'s body");
    assert_eq!(moved_as(&old, BASE, &new, text, "get"), (false, false), "under ByBytes the comment moved `get`");
    println!("token keys: under ByBytes the comment moved fn f (the control)");
}
