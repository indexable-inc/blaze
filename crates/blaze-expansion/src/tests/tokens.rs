//! The token hash's properties (Hegel): a stream hashes the same wherever it sits and however
//! the caller numbers its anchors and contexts, and any change to a token changes the hash.

use blaze_rng::SplitMix64;
use hegel::TestCase;

use super::support::{CONTROL_SEEDS, Draw, Drawn};
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

fn stream(d: &mut impl Draw) -> Vec<Spec> {
    (0..d.int(1, 12))
        .map(|_| Spec {
            kind: d.under(KINDS.len()),
            text: format!("t{}", d.int(0, 5)),
            start: d.int(0, 200) as u32,
            len: d.int(0, 8) as u32,
            anchor: d.int(0, 2),
            ctx: d.int(0, 2) as u32,
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

fn other_placing(d: &mut impl Draw) -> Placing {
    Placing { shift: d.int(1, 50_000) as u32, anchor_base: d.int(1, 1 << 40), ctx_base: d.int(1, 1 << 20) as u32 }
}

const HERE: Placing = Placing { shift: 0, anchor_base: 0, ctx_base: 0 };

/// The invariant: a stream moved in its file (an edit above the call) and numbered with
/// other ids (another database, another revision) hashes as it did.
#[hegel::test(test_cases = 300)]
fn equal_streams_hash_equal_wherever_they_sit(tc: TestCase) {
    let mut d = Drawn(&tc);
    let specs = stream(&mut d);
    let there = other_placing(&mut d);
    assert_eq!(hash(&specs, HERE, TokenPlant::None), hash(&specs, there, TokenPlant::None), "{specs:?}");
}

/// The invariant: any change to one token (its kind, text, start or length) changes the hash.
#[hegel::test(test_cases = 300)]
fn any_token_change_changes_the_hash(tc: TestCase) {
    let mut d = Drawn(&tc);
    let specs = stream(&mut d);
    let mut changed = specs.clone();
    let at = d.under(changed.len());
    match d.int(0, 3) {
        0 => changed[at].kind = (changed[at].kind + 1 + d.under(KINDS.len() - 1)) % KINDS.len(),
        1 => changed[at].text.push('x'),
        2 => changed[at].start += 1,
        _ => changed[at].len += 1,
    }
    assert_ne!(hash(&specs, HERE, TokenPlant::None), hash(&changed, HERE, TokenPlant::None), "{specs:?} -> {changed:?}");
}

/// The control: a hash of the caller's anchor ids is caught by the first property.
#[test]
fn the_property_catches_anchor_ids_hashed() {
    for seed in 0..CONTROL_SEEDS {
        let mut d = SplitMix64::new(seed);
        let specs = stream(&mut d);
        let there = other_placing(&mut d);
        if hash(&specs, HERE, TokenPlant::HashAnchorIds) != hash(&specs, there, TokenPlant::HashAnchorIds) {
            return;
        }
    }
    panic!("hashing the caller's anchor ids went unseen at seeds 0..{CONTROL_SEEDS}");
}
