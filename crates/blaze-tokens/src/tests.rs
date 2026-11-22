//! The record's promises, each on bytes a builder wrote: every column round-trips, two builders
//! write equal bytes, `open` refuses a broken buffer by the column and entry, the digest is a
//! function of the tokens alone, and `shown` is for a person and keeps an integer apart from
//! a `.`. Each passing test prints its one line `test tests::<name> ... ok`.

use std::string::String;
use std::vec::Vec as StdVec;

use blaze_format_core::{FormatError, open_as};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::Alloc;
use blaze_surface::{DefSite, Edition, Transparency};
use parser::SyntaxKind;

use crate::form::TokensRoot;
use crate::{
    Column, Ctx, CtxNode, Mark, MarkOf, NewToken, OpenError, Origin, TOKENS_SCHEMA, TokenRange,
    Tokens, TokensBuilder, TokensBytes,
};

const ED: parser::Edition = parser::Edition::Edition2021;

fn plain(kind: SyntaxKind) -> u32 {
    parser::word(kind, SyntaxKind::EOF, ED)
}

fn ident() -> u32 {
    parser::word(SyntaxKind::IDENT, SyntaxKind::IDENT, ED)
}

fn site(module: bool) -> DefSite {
    DefSite {
        krate: CrateItemKey(ContentId([7; 32])),
        module: module.then_some(ItemKey(ContentId([8; 32]))),
    }
}

fn node(parent: Ctx, of: MarkOf, transparency: Transparency, edition: Edition, module: bool) -> CtxNode {
    CtxNode { parent, mark: Mark { of, transparency, edition, site: site(module) } }
}

/// One token of the round trip: its word, its lexeme, joined, which context (0 the root, 1 and
/// 2 the two nodes), its origin.
struct Want {
    word: u32,
    lexeme: Option<&'static str>,
    joined: bool,
    ctx: usize,
    origin: Origin,
}

/// 75 tokens, across two joined words, of every lexeme kind with lexemes repeated, every
/// origin kind and all three contexts.
fn wanted() -> StdVec<Want> {
    let shapes: [(u32, Option<&'static str>); 9] = [
        (ident(), Some("a")),
        (plain(SyntaxKind::DOT), None),
        (ident(), Some("r#fn")),
        (parser::word(SyntaxKind::LIFETIME_IDENT, SyntaxKind::EOF, ED), Some("'a")),
        (plain(SyntaxKind::L_PAREN), None),
        (parser::word(SyntaxKind::INT_NUMBER, SyntaxKind::EOF, ED), Some("1u8")),
        (parser::word(SyntaxKind::FLOAT_NUMBER, SyntaxKind::EOF, ED) | parser::JOINT, Some("1.5")),
        (plain(SyntaxKind::R_PAREN), None),
        (ident(), Some("a")),
    ];
    (0..75u32)
        .map(|i| {
            let (word, lexeme) = shapes[usize::try_from(i).unwrap() % shapes.len()];
            Want {
                word,
                lexeme,
                joined: i % 3 == 0 || i == 74,
                ctx: usize::try_from(i % 3).unwrap(),
                origin: match i % 5 {
                    0 => Origin::Input(i),
                    1 => Origin::AttrArgs(i + 1),
                    2 => Origin::Def(i * 2),
                    3 => Origin::Params(7),
                    _ => Origin::Made(Transparency::Opaque),
                },
            }
        })
        .collect()
}

const GROUPS: [TokenRange; 3] =
    [TokenRange::new(2, 10), TokenRange::new(2, 5), TokenRange::new(20, 75)];

/// The round trip's record, and its three contexts in [`Want::ctx`] order.
fn round_trip_record(alloc: Alloc<'_>) -> (TokensBytes<'_>, [Ctx; 3]) {
    let mut b = TokensBuilder::new_in(alloc);
    let root = Ctx::root(Edition::E2021);
    let first = b
        .context(node(root, MarkOf::This, Transparency::SemiOpaque, Edition::E2018, false))
        .unwrap();
    let again = b
        .context(node(root, MarkOf::This, Transparency::SemiOpaque, Edition::E2018, false))
        .unwrap();
    assert_eq!(first, again, "an equal node is the context made before");
    let outer = MarkOf::Outer(ContentId([9; 32]));
    let second = b.context(node(first, outer, Transparency::Opaque, Edition::E2024, true)).unwrap();
    let ctxs = [root, first, second];
    for want in wanted() {
        b.push(NewToken {
            word: want.word,
            lexeme: want.lexeme,
            joined: want.joined,
            ctx: ctxs[want.ctx],
            origin: want.origin,
        })
        .unwrap();
    }
    for group in GROUPS {
        b.group(group).unwrap();
    }
    (b.finish().unwrap(), ctxs)
}

/// Every column of every token reads back as it was pushed: word, kind, joined, lexeme (and
/// its first-seen number), text, context, mark, edition and origin, the groups, and a slice's
/// view of all of them. Prints `test tests::every_column_round_trips ... ok`.
#[test]
fn every_column_round_trips() {
    let heap = blaze_own::testing::heap();
    let (bytes, ctxs) = round_trip_record(heap.alloc());
    let t = Tokens::open(&bytes).unwrap();
    let wanted = wanted();
    assert_eq!(t.len(), wanted.len());
    assert_eq!(t.lexeme_count(), 5, "a, r#fn, 'a, 1u8, 1.5: each once");
    let mut first_seen: StdVec<&str> = StdVec::new();
    for (i, want) in wanted.iter().enumerate() {
        assert_eq!(t.word(i), want.word, "token {i}'s word");
        assert_eq!(t.kind(i), parser::word_kind(want.word), "token {i}'s kind");
        assert_eq!(t.is_joined(i), want.joined, "token {i}'s joined bit");
        assert_eq!(t.lexeme(i), want.lexeme, "token {i}'s lexeme");
        if let Some(text) = want.lexeme {
            if !first_seen.contains(&text) {
                first_seen.push(text);
            }
            let number = first_seen.iter().position(|seen| *seen == text).unwrap();
            assert_eq!(t.lexeme_of(i).unwrap().index(), u32::try_from(number).unwrap());
        }
        let text = want.lexeme.unwrap_or_else(|| parser::word_kind(want.word).fixed_text().unwrap());
        assert_eq!(t.text(i), text, "token {i}'s text");
        assert_eq!(t.ctx(i), ctxs[want.ctx], "token {i}'s context");
        assert_eq!(t.origin(i), want.origin, "token {i}'s origin");
        let (mark, edition) = match want.ctx {
            0 => (None, Edition::E2021),
            1 => (Some(Transparency::SemiOpaque), Edition::E2018),
            _ => (Some(Transparency::Opaque), Edition::E2024),
        };
        assert_eq!(t.mark(i).map(|mark| mark.transparency), mark, "token {i}'s mark");
        assert_eq!(t.edition(i), edition, "token {i}'s edition");
    }
    let node = t.node(ctxs[2]).unwrap();
    assert_eq!(node.parent, ctxs[1]);
    assert_eq!(node.mark.of, MarkOf::Outer(ContentId([9; 32])));
    assert_eq!(node.mark.site, site(true));
    assert_eq!(t.node(ctxs[1]).unwrap().mark.site, site(false));
    assert_eq!(t.nodes().count(), 2);
    assert_eq!(t.groups().collect::<StdVec<_>>(), GROUPS);
    assert_eq!(t.words(t.all()).len(), 75);
    assert!(t.parser_tokens().is_some());

    let view = t.slice(TokenRange::new(2, 70));
    assert_eq!(view.offset(), 2);
    assert_eq!(view.len(), 68);
    for i in 0..view.len() {
        assert_eq!(view.word(i), t.word(i + 2));
        assert_eq!(view.is_joined(i), t.is_joined(i + 2), "the view's joined bit {i}");
        assert_eq!(view.lexeme(i), t.lexeme(i + 2));
        assert_eq!(view.ctx(i), t.ctx(i + 2));
        assert_eq!(view.origin(i), t.origin(i + 2));
    }
    assert_eq!(view.lexeme_count(), t.lexeme_count(), "a view shares the lexeme table");
    assert_eq!(
        view.groups().collect::<StdVec<_>>(),
        [TokenRange::new(0, 8), TokenRange::new(0, 3)],
        "the groups inside the view, as its own ranges"
    );
    assert!(view.parser_tokens().is_none(), "a view ending before its record has no pad");
    assert!(t.slice(TokenRange::new(70, 75)).parser_tokens().is_some());
    assert_eq!(t.digest(TokenRange::new(2, 70)), view.digest(view.all()));
}

/// Two builders handed the same tokens write equal bytes (rule 25). Prints
/// `test tests::two_builders_write_equal_bytes ... ok`.
#[test]
fn two_builders_write_equal_bytes() {
    let heap = blaze_own::testing::heap();
    let (one, _) = round_trip_record(heap.alloc());
    let (two, _) = round_trip_record(heap.alloc());
    assert_eq!(&one[..], &two[..]);
}

/// The byte offset in `bytes` of entry `row` of a column's slice.
fn offset_in<T>(bytes: &[u8], column: &[T], row: usize) -> usize {
    column[row..].as_ptr().addr() - bytes.as_ptr().addr()
}

fn two_idents(alloc: Alloc<'_>) -> TokensBytes<'_> {
    let mut b = TokensBuilder::new_in(alloc);
    for (text, at) in [("a", 0), ("b", 1)] {
        b.push(NewToken {
            word: ident(),
            lexeme: Some(text),
            joined: false,
            ctx: Ctx::root(Edition::E2021),
            origin: Origin::Input(at),
        })
        .unwrap();
    }
    b.group(TokenRange::new(0, 2)).unwrap();
    b.finish().unwrap()
}

/// A buffer cut short is refused by blaze-format's header check. Prints
/// `test tests::open_refuses_a_short_payload ... ok`.
#[test]
fn open_refuses_a_short_payload() {
    let heap = blaze_own::testing::heap();
    let bytes = two_idents(heap.alloc());
    let short = &bytes[..bytes.len() - 16];
    assert!(
        matches!(Tokens::open(short), Err(OpenError::Format(FormatError::Header(_)))),
        "{:?}",
        Tokens::open(short)
    );
}

/// A lexeme number past the table is refused as that, naming the token, the number and the
/// table's size. Prints `test tests::open_refuses_a_lexeme_out_of_range ... ok`.
#[test]
fn open_refuses_a_lexeme_out_of_range() {
    let heap = blaze_own::testing::heap();
    let mut bytes = two_idents(heap.alloc());
    let at = {
        let root = open_as::<TokensRoot>(&bytes, TOKENS_SCHEMA).unwrap();
        offset_in(&bytes, root.lexeme_of.follow(&bytes).unwrap(), 1)
    };
    bytes.as_mut_slice()[at..at + 4].copy_from_slice(&5u32.to_le_bytes());
    assert_eq!(
        Tokens::open(&bytes).unwrap_err(),
        OpenError::LexemeOutOfRange { token: 1, index: 5, lexemes: 2 }
    );
}

/// A group ending past the last token is refused as that, naming the column and the row.
/// Prints `test tests::open_refuses_a_range_past_the_end ... ok`.
#[test]
fn open_refuses_a_range_past_the_end() {
    let heap = blaze_own::testing::heap();
    let mut bytes = two_idents(heap.alloc());
    let at = {
        let root = open_as::<TokensRoot>(&bytes, TOKENS_SCHEMA).unwrap();
        offset_in(&bytes, root.groups.follow(&bytes).unwrap(), 0) + 4
    };
    bytes.as_mut_slice()[at..at + 4].copy_from_slice(&9u32.to_le_bytes());
    assert_eq!(
        Tokens::open(&bytes).unwrap_err(),
        OpenError::RangePastEnd { column: Column::Groups, row: 0, end: 9, tokens: 2 }
    );
}

/// A record of identifiers `(text, joined)`, each in one context of `transparency`.
fn idents<'a>(alloc: Alloc<'a>, tokens: &[(&str, bool)], transparency: Transparency) -> TokensBytes<'a> {
    let mut b = TokensBuilder::new_in(alloc);
    let root = Ctx::root(Edition::E2021);
    let ctx = b.context(node(root, MarkOf::This, transparency, Edition::E2021, false)).unwrap();
    for (at, &(text, joined)) in tokens.iter().enumerate() {
        b.push(NewToken {
            word: ident(),
            lexeme: Some(text),
            joined,
            ctx,
            origin: Origin::Input(u32::try_from(at).unwrap()),
        })
        .unwrap();
    }
    b.finish().unwrap()
}

/// `x y x` digests alike whether it is a whole record (lexemes numbered x=0, y=1) or tokens
/// 1..4 of `y x y x` (y=0, x=1), whose last token is joined to one outside the range; one
/// joined flag, one mark or one lexeme changed changes it. Prints
/// `test tests::digest_is_of_the_tokens_alone ... ok`.
#[test]
fn digest_is_of_the_tokens_alone() {
    let heap = blaze_own::testing::heap();
    let semi = Transparency::SemiOpaque;
    let a_bytes = idents(heap.alloc(), &[("x", true), ("y", false), ("x", false)], semi);
    let b_bytes =
        idents(heap.alloc(), &[("y", false), ("x", true), ("y", false), ("x", true)], semi);
    let a = Tokens::open(&a_bytes).unwrap();
    let b = Tokens::open(&b_bytes).unwrap();
    assert_ne!(a.lexeme_of(0), b.lexeme_of(1), "the two tables number x differently");
    let digest = a.digest(a.all());
    assert_eq!(digest, b.digest(TokenRange::new(1, 4)));

    let joined = idents(heap.alloc(), &[("x", false), ("y", false), ("x", false)], semi);
    let marked = idents(heap.alloc(), &[("x", true), ("y", false), ("x", false)], Transparency::Opaque);
    let lexeme = idents(heap.alloc(), &[("x", true), ("z", false), ("x", false)], semi);
    for (what, bytes) in [("a joined flag", &joined), ("a mark", &marked), ("a lexeme", &lexeme)] {
        let other = Tokens::open(bytes).unwrap();
        assert_ne!(other.digest(other.all()), digest, "{what} changed and the digest did not");
    }
}

/// `999` `.` `try_into` `(` `)` stays five words, and `shown` never prints the float `999.`:
/// `999 .try_into()` with the `.` joined, `999 . try_into()` without. Prints
/// `test tests::an_integer_and_a_dot_stay_two_tokens ... ok`.
#[test]
fn an_integer_and_a_dot_stay_two_tokens() {
    let heap = blaze_own::testing::heap();
    for (dot_joined, wanted) in [(true, "999 .try_into()"), (false, "999 . try_into()")] {
        let mut b = TokensBuilder::new_in(heap.alloc());
        let tokens: [(u32, Option<&str>, bool); 5] = [
            (parser::word(SyntaxKind::INT_NUMBER, SyntaxKind::EOF, ED), Some("999"), true),
            (plain(SyntaxKind::DOT), None, dot_joined),
            (ident(), Some("try_into"), true),
            (plain(SyntaxKind::L_PAREN), Some("("), true),
            (plain(SyntaxKind::R_PAREN), None, false),
        ];
        for (at, (word, lexeme, joined)) in tokens.into_iter().enumerate() {
            b.push(NewToken {
                word,
                lexeme,
                joined,
                ctx: Ctx::root(Edition::E2021),
                origin: Origin::Input(u32::try_from(at).unwrap()),
            })
            .unwrap();
        }
        let bytes = b.finish().unwrap();
        let t = Tokens::open(&bytes).unwrap();
        let kinds: StdVec<SyntaxKind> =
            t.words(t.all()).iter().map(|&word| parser::word_kind(word)).collect();
        assert_eq!(
            kinds,
            [
                SyntaxKind::INT_NUMBER,
                SyntaxKind::DOT,
                SyntaxKind::IDENT,
                SyntaxKind::L_PAREN,
                SyntaxKind::R_PAREN
            ]
        );
        let mut shown = String::new();
        t.shown(t.all(), &mut shown).unwrap();
        assert_eq!(shown, wanted);
        assert!(!shown.contains("999."), "a float in {shown:?}");
    }
}
