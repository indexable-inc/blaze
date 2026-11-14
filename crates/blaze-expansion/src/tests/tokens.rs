//! The token hash's properties: a stream hashes the same wherever it sits and however the caller
//! numbers its anchors and contexts, and any change to a token changes the hash.

use blaze_sim::{Broke, Config, Tape};

use super::support::CONTROL;
use crate::{
    Digest,
    tokens::{
        AbsoluteOffset, AnchorId, CtxId, InSpan, LitKind, Numbering, RelativeRange, Role, Token,
        TokenHasher, TokenKind, TokenPlant,
    },
};

const KINDS: [TokenKind; 5] = [
    TokenKind::Ident,
    TokenKind::Punct { joint: false },
    TokenKind::Punct { joint: true },
    TokenKind::Literal(LitKind::Integer),
    TokenKind::Lifetime,
];

/// A token as drawn: its kind's index, its text, its start and length from its anchor, its
/// anchor's index and its context's index.
#[derive(Clone, Debug)]
struct Spec {
    kind: usize,
    text: String,
    start: u32,
    len: u32,
    anchor: u64,
    ctx: u32,
}

fn stream(tape: &Tape) -> Vec<Spec> {
    (0..tape.int_in(1..=12u64, "tokens"))
        .map(|_| Spec {
            kind: tape.index(KINDS.len(), "kind"),
            text: format!("t{}", tape.int_in(0..=5u64, "text")),
            start: tape.int_in(0..=200u32, "start"),
            len: tape.int_in(0..=8u32, "len"),
            anchor: tape.int_in(0..=2u64, "anchor"),
            ctx: tape.int_in(0..=2u32, "ctx"),
        })
        .collect()
}

/// How the caller places and numbers a stream: where anchor `a` starts in its file, and the
/// ids it gives anchors and contexts.
#[derive(Clone, Copy)]
struct Placing {
    shift: u32,
    anchor_base: u64,
    ctx_base: u32,
}

fn hash(specs: &[Spec], placing: Placing, plant: TokenPlant) -> Digest {
    let anchor_start = |a: u64| AbsoluteOffset(1000 * a as u32 + placing.shift);
    let span = |a: u64, start: u32, len: u32, c: u32| {
        let begin = anchor_start(a).0 + start;
        InSpan {
            anchor: AnchorId(placing.anchor_base + a * 7),
            range: RelativeRange::from_absolute(AbsoluteOffset(begin), AbsoluteOffset(begin + len), anchor_start(a))
                .expect("a token starts after its anchor"),
            ctx: CtxId(placing.ctx_base + c * 3),
        }
    };
    let mut hasher = TokenHasher::new(Role::Input, Numbering::default());
    hasher.plant = plant;
    let site = span(0, 0, 0, 0);
    hasher.sites([site, site, site]);
    for it in specs {
        hasher.token(&Token { kind: KINDS[it.kind], text: &it.text, span: span(it.anchor, it.start, it.len, it.ctx) });
    }
    hasher.finish().0
}

fn other_placing(tape: &Tape) -> Placing {
    Placing {
        shift: tape.int_in(1..=50_000u32, "shift"),
        anchor_base: tape.int_in(1..=1u64 << 40, "anchor base"),
        ctx_base: tape.int_in(1..=1u32 << 20, "ctx base"),
    }
}

const HERE: Placing = Placing { shift: 0, anchor_base: 0, ctx_base: 0 };

/// A stream, placed here and elsewhere: the first property's oracle, under `plant`.
fn placed(tape: &Tape, plant: TokenPlant) -> Result<(), Broke> {
    let specs = stream(tape);
    let there = other_placing(tape);
    if hash(&specs, HERE, plant) != hash(&specs, there, plant) {
        return Err(Broke::Oracle(format!("a stream moved and renumbered hashes otherwise: {specs:?}")));
    }
    Ok(())
}

/// The invariant: a stream moved in its file (an edit above the call) and numbered with
/// other ids (another database, another revision) hashes as it did.
#[test]
fn equal_streams_hash_equal_wherever_they_sit() {
    blaze_sim::check(Config::cases(300), |tape| placed(tape, TokenPlant::None));
}

/// The invariant: any change to one token (its kind, text, start or length) changes the hash.
#[test]
fn any_token_change_changes_the_hash() {
    blaze_sim::check(Config::cases(300), |tape| {
        let specs = stream(tape);
        let mut changed = specs.clone();
        let at = tape.index(changed.len(), "changed token");
        match tape.int_in(0..=3u8, "change") {
            0 => {
                changed[at].kind = (changed[at].kind + 1 + tape.index(KINDS.len() - 1, "other kind")) % KINDS.len()
            }
            1 => changed[at].text.push('x'),
            2 => changed[at].start += 1,
            _ => changed[at].len += 1,
        }
        assert_ne!(
            hash(&specs, HERE, TokenPlant::None),
            hash(&changed, HERE, TokenPlant::None),
            "{specs:?} -> {changed:?}"
        );
    });
}

/// The control: a hash of the caller's anchor ids is caught by the first property.
#[test]
fn the_property_catches_anchor_ids_hashed() {
    blaze_sim::control(CONTROL, TokenPlant::None, TokenPlant::HashAnchorIds, "hashes otherwise", placed);
}
