//! The borrow checker's algorithms as they were before the fixpoint and the analyses were
//! redone, copied here as the reference, run against the new ones on the same inputs: the
//! random graphs of the old tests, generated large bodies, and the borrowck fixtures. What
//! they compute must agree bit for bit.
//!
//! This file holds the fixpoint's half; [`analyses`] the moves, liveness and loans.

use std::ops::Range;

use blaze_hash::HashMap;
use blaze_ids::rng::SplitMix64;
use blaze_mem::{Tag, frame};
use hir_ty_mir::mir::BasicBlockId;

use crate::{
    bitset::{Transfer, iter_words, words_for},
    cfg::{Cfg, block, index},
    dataflow,
    mem::Frame,
    rows::{DENSE_WORDS, Dense, Edge, Effects, LINE, Lined, Rows, Store},
};

mod analyses;
mod old;

/// A gen (`true`) or a kill of a range of bits.
type Op = (bool, Range<usize>);

/// What an edge other than [`Edge::Same`] does.
enum Kind {
    Effect(Vec<Op>),
    /// A row's length of words.
    Mask(Vec<u64>),
    Nothing,
}

pub(super) struct Case<'f> {
    cfg: Cfg<'f>,
    domain: usize,
    /// Each block's effect, the ops in the order they compose.
    blocks: Vec<Vec<Op>>,
    /// Each edge that is not [`Edge::Same`], once.
    kinds: Vec<((usize, usize), Kind)>,
    by_edge: HashMap<(usize, usize), usize>,
    /// What the start block begins with, a row's length of words.
    boundary: Vec<u64>,
}

impl<'f> Case<'f> {
    /// The case of `blocks.len()` blocks with these edges, from block 0, an edge's kind given
    /// by `kind` for each distinct edge (`None` for [`Edge::Same`]).
    fn new(
        frame: &'f Frame,
        domain: usize,
        blocks: Vec<Vec<Op>>,
        edges: &[(usize, usize)],
        boundary: Vec<u64>,
        mut kind: impl FnMut() -> Option<Kind>,
    ) -> Self {
        let lens = vec![1; blocks.len()];
        let cfg = Cfg::from_edges(frame, &lens, edges);
        let mut kinds = Vec::new();
        let mut by_edge = HashMap::default();
        let mut seen = HashMap::default();
        for &edge in edges {
            if seen.insert(edge, ()).is_some() {
                continue;
            }
            if let Some(it) = kind() {
                by_edge.insert(edge, kinds.len());
                kinds.push((edge, it));
            }
        }
        Case { cfg, domain, blocks, kinds, by_edge, boundary }
    }

    pub(super) fn words(&self) -> usize {
        words_for(self.domain)
    }
}

fn compose(row: &mut impl Transfer, ops: &[Op]) {
    for (gen_, range) in ops {
        if *gen_ {
            row.gen_range(range.clone());
        } else {
            row.kill_range(range.clone());
        }
    }
}

fn old_edge(
    case: &Case<'_>,
    effects: &old::GenKill,
    from: BasicBlockId,
    to: BasicBlockId,
    state: &mut [u64],
) {
    let Some(&at) = case.by_edge.get(&(index(from), index(to))) else { return };
    match &case.kinds[at].1 {
        Kind::Effect(_) => effects.apply(at, state),
        Kind::Mask(mask) => {
            for (word, mask) in state.iter_mut().zip(mask) {
                *word &= mask;
            }
        }
        Kind::Nothing => state.fill(0),
    }
}

fn new_edge<'a>(
    case: &'a Case<'_>,
    effects: &'a Effects<'_>,
    from: BasicBlockId,
    to: BasicBlockId,
) -> Edge<'a> {
    match case.by_edge.get(&(index(from), index(to))) {
        None => Edge::Same,
        Some(&at) => match &case.kinds[at].1 {
            Kind::Effect(_) => effects.edge(at),
            Kind::Mask(mask) => Edge::Mask(mask),
            Kind::Nothing => Edge::Nothing,
        },
    }
}

/// Each row of `load` against the old matrix, bit for bit.
fn same_rows(
    what: &str,
    want: &old::BitMatrix,
    case: &Case<'_>,
    load: impl Fn(usize, &mut [u64]),
) {
    let mut row = vec![0u64; case.words()];
    for b in 0..case.blocks.len() {
        row.fill(!0);
        load(b, &mut row);
        let want = want.row(b);
        if row != want {
            let word = row.iter().zip(want).position(|(a, b)| a != b).unwrap_or(0);
            panic!(
                "{what}: block {b} of {}, domain {}: word {word} is {:#x}, the old fixpoint's {:#x}",
                case.blocks.len(),
                case.domain,
                row[word],
                want[word],
            );
        }
    }
}

/// The old fixpoint and the new one, on both stores and through the pick by size, forward
/// and backward, on `case`.
fn agree(frame: &Frame, what: &str, case: &Case<'_>) {
    let count = case.blocks.len();
    let mut old_blocks = old::GenKill::new(count, case.domain);
    let mut blocks = Effects::new(frame, count, case.domain);
    for (b, ops) in case.blocks.iter().enumerate() {
        if !ops.is_empty() {
            compose(&mut old_blocks.row(b), ops);
            compose(&mut blocks.row(b), ops);
        }
    }
    let mut old_edges = old::GenKill::new(case.kinds.len(), case.domain);
    let mut edges = Effects::new(frame, case.kinds.len(), case.domain);
    for (at, (_, kind)) in case.kinds.iter().enumerate() {
        if let Kind::Effect(ops) = kind {
            compose(&mut old_edges.row(at), ops);
            compose(&mut edges.row(at), ops);
        }
    }
    let mut boundary = old::BitSet::new(case.domain);
    for bit in iter_words(&case.boundary) {
        boundary.insert(bit);
    }
    let start = block(0);
    let edges = &edges;

    let want = old::forward(&case.cfg, &old_blocks, start, &boundary, |from, to, state| {
        old_edge(case, &old_edges, from, to, state)
    });
    let dense: Dense<'_> =
        dataflow::forward_in(frame, &case.cfg, &blocks, start, &case.boundary, |from, to| {
            new_edge(case, edges, from, to)
        });
    same_rows(&format!("{what}, forward, dense"), &want, case, |b, out| dense.load(b, out));
    let lined: Lined<'_> =
        dataflow::forward_in(frame, &case.cfg, &blocks, start, &case.boundary, |from, to| {
            new_edge(case, edges, from, to)
        });
    same_rows(&format!("{what}, forward, lined"), &want, case, |b, out| lined.load(b, out));
    let picked = dataflow::forward(frame, &case.cfg, &blocks, start, &case.boundary, |from, to| {
        new_edge(case, edges, from, to)
    });
    picks_by_size(what, case, &picked);
    same_rows(&format!("{what}, forward, picked"), &want, case, |b, out| picked.load(b, out));

    let want = old::backward(&case.cfg, &old_blocks, |from, to, state| {
        old_edge(case, &old_edges, from, to, state)
    });
    let dense: Dense<'_> = dataflow::backward_in(frame, &case.cfg, &blocks, |from, to| {
        new_edge(case, edges, from, to)
    });
    same_rows(&format!("{what}, backward, dense"), &want, case, |b, out| dense.load(b, out));
    let lined: Lined<'_> = dataflow::backward_in(frame, &case.cfg, &blocks, |from, to| {
        new_edge(case, edges, from, to)
    });
    same_rows(&format!("{what}, backward, lined"), &want, case, |b, out| lined.load(b, out));
    let picked = dataflow::backward(frame, &case.cfg, &blocks, |from, to| {
        new_edge(case, edges, from, to)
    });
    picks_by_size(what, case, &picked);
    same_rows(&format!("{what}, backward, picked"), &want, case, |b, out| picked.load(b, out));
}

/// What an analysis does with the fixpoint, on `case`: composes the blocks' and the edges'
/// effects, solves forward and backward through the pick by size, and reads every entry row
/// into `row` (a row's length), as the walk does. Gives the bits it read, so the budget test
/// can check the work was done.
pub(super) fn solve(frame: &Frame, case: &Case<'_>, row: &mut [u64]) -> usize {
    let mut blocks = Effects::new(frame, case.blocks.len(), case.domain);
    for (b, ops) in case.blocks.iter().enumerate() {
        if !ops.is_empty() {
            compose(&mut blocks.row(b), ops);
        }
    }
    let mut edges = Effects::new(frame, case.kinds.len(), case.domain);
    for (at, (_, kind)) in case.kinds.iter().enumerate() {
        if let Kind::Effect(ops) = kind {
            compose(&mut edges.row(at), ops);
        }
    }
    let edges = &edges;
    let mut bits = 0;
    let mut read = |rows: &Rows<'_>| {
        for b in 0..case.blocks.len() {
            rows.load(b, row);
            bits += row.iter().map(|word| word.count_ones() as usize).sum::<usize>();
        }
    };
    read(&dataflow::forward(frame, &case.cfg, &blocks, block(0), &case.boundary, |from, to| {
        new_edge(case, edges, from, to)
    }));
    read(&dataflow::backward(frame, &case.cfg, &blocks, |from, to| {
        new_edge(case, edges, from, to)
    }));
    bits
}

fn picks_by_size(what: &str, case: &Case<'_>, rows: &Rows<'_>) {
    assert_eq!(rows.words(), case.words(), "{what}");
    let dense = matches!(rows, Rows::Dense(_));
    assert_eq!(dense, case.words() <= DENSE_WORDS, "{what}: {} words", case.words());
}

/// A gen or kill: mostly a single bit, as a statement's; sometimes a run of whole lines, as a
/// storage-dead of a large place kills or a call's return initializes; sometimes any run.
fn op(rng: &mut SplitMix64, domain: usize) -> Op {
    let gen_ = rng.coin();
    let line_bits = LINE * 64;
    let range = match rng.index(8) {
        0 | 1 if domain > line_bits => {
            let lines = domain.div_ceil(line_bits);
            let from = rng.index(lines) * line_bits;
            let len = (1 + rng.index(4)) * line_bits;
            from..(from + len).min(domain)
        }
        2 => {
            let from = rng.index(domain);
            from..from + rng.index((domain - from).min(300) + 1)
        }
        3 if rng.one_in(8) => {
            let from = rng.index(domain);
            from..from + rng.index(domain - from + 1)
        }
        _ => {
            let bit = rng.index(domain);
            bit..bit + 1
        }
    };
    (gen_, range)
}

fn ops(rng: &mut SplitMix64, domain: usize, max: usize) -> Vec<Op> {
    (0..rng.index(max + 1)).map(|_| op(rng, domain)).collect()
}

/// An edge's kind: mostly [`Edge::Same`], as most edges of a body are.
fn kind(rng: &mut SplitMix64, domain: usize) -> Option<Kind> {
    let words = words_for(domain);
    match rng.index(10) {
        0 | 1 => Some(Kind::Effect(ops(rng, domain, 4))),
        2 | 3 => {
            // Each line kept whole, dropped whole, or mixed.
            let mut mask = vec![0u64; words];
            for line in mask.chunks_mut(LINE) {
                match rng.index(3) {
                    0 => line.fill(!0),
                    1 => (),
                    _ => line.iter_mut().for_each(|word| *word = rng.next_u64()),
                }
            }
            if domain % 64 != 0 {
                mask[words - 1] &= (1 << (domain % 64)) - 1;
            }
            Some(Kind::Mask(mask))
        }
        4 => Some(Kind::Nothing),
        _ => None,
    }
}

/// What the start block begins with: nothing, a few bits, every bit (every path uninitialized
/// but the arguments), or a prefix.
fn boundary(rng: &mut SplitMix64, domain: usize) -> Vec<u64> {
    let mut out = vec![0u64; words_for(domain)];
    match rng.index(4) {
        0 => (),
        1 => {
            for _ in 0..1 + rng.index(8) {
                let bit = rng.index(domain);
                out[bit / 64] |= 1 << (bit % 64);
            }
        }
        2 => crate::bitset::masks(0..domain, |word, mask| out[word] |= mask),
        _ => crate::bitset::masks(0..rng.index(domain + 1), |word, mask| out[word] |= mask),
    }
    out
}

/// The old tests' random graphs: up to `max_blocks` blocks, a chain from the start most
/// blocks are on, and random edges besides, over `domain` bits.
fn random_case<'f>(
    frame: &'f Frame,
    rng: &mut SplitMix64,
    max_blocks: usize,
    domain: usize,
) -> Case<'f> {
    let count = 1 + rng.index(max_blocks);
    let mut edges = Vec::new();
    for b in 1..count {
        if rng.index(5) != 0 {
            edges.push((rng.index(b), b));
        }
    }
    for _ in 0..rng.index(count * 2 + 1) {
        edges.push((rng.index(count), rng.index(count)));
    }
    let blocks = (0..count).map(|_| ops(rng, domain, 5)).collect();
    let boundary = boundary(rng, domain);
    let mut kinds = SplitMix64::new(rng.next_u64());
    Case::new(frame, domain, blocks, &edges, boundary, || kind(&mut kinds, domain))
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Shape {
    Chain,
    Diamonds,
    Loops,
    Fans,
    Tangle,
    Mixed,
}

const SHAPES: [Shape; 6] =
    [Shape::Chain, Shape::Diamonds, Shape::Loops, Shape::Fans, Shape::Tangle, Shape::Mixed];

/// Edges over blocks `lo..hi`, every one reachable from `lo` and `hi - 1` reachable from all
/// of them that reach anything.
fn shaped(rng: &mut SplitMix64, shape: Shape, lo: usize, hi: usize, edges: &mut Vec<(usize, usize)>) {
    let chain = |from: usize, edges: &mut Vec<(usize, usize)>| {
        edges.extend((from..hi.saturating_sub(1)).map(|b| (b, b + 1)));
    };
    match shape {
        Shape::Chain => chain(lo, edges),
        Shape::Diamonds => {
            let mut at = lo;
            while at + 3 < hi {
                edges.extend([(at, at + 1), (at, at + 2), (at + 1, at + 3), (at + 2, at + 3)]);
                at += 3;
            }
            chain(at, edges);
        }
        Shape::Loops => {
            chain(lo, edges);
            nest(rng, lo, hi, hi, 8, edges);
        }
        Shape::Fans => {
            let mut at = lo;
            while at + 3 < hi {
                let room = hi - at - 2;
                let arms = 2 + rng.index(room.min(64) - 1);
                let join = at + arms + 1;
                for arm in at + 1..join {
                    edges.extend([(at, arm), (arm, join)]);
                }
                if rng.one_in(4) {
                    edges.push((join, at));
                }
                at = join;
            }
            chain(at, edges);
        }
        Shape::Tangle => {
            chain(lo, edges);
            for _ in 0..(hi - lo) / 2 {
                let from = lo + rng.index(hi - lo);
                let reach = 1 + rng.index(64);
                let to = if rng.coin() {
                    from.saturating_sub(reach).max(lo)
                } else {
                    (from + reach).min(hi - 1)
                };
                edges.push((from, to));
            }
        }
        Shape::Mixed => {
            let mut at = lo;
            while at < hi {
                let end = (at + 200 + rng.index(600)).min(hi);
                let shape = SHAPES[rng.index(SHAPES.len() - 1)];
                shaped(rng, shape, at, end, edges);
                if end < hi {
                    edges.push((end - 1, end));
                }
                at = end;
            }
        }
    }
}

/// Loops nested in `lo..hi` down to `depth` levels: a back edge from the last block to the
/// first and an exit from the header past the loop, then up to three loops inside.
fn nest(
    rng: &mut SplitMix64,
    lo: usize,
    hi: usize,
    end: usize,
    depth: usize,
    edges: &mut Vec<(usize, usize)>,
) {
    if depth == 0 || hi - lo < 4 {
        return;
    }
    edges.push((hi - 1, lo));
    if hi < end {
        edges.push((lo, hi));
    }
    let (mut at, inner) = (lo + 1, hi - 1);
    for _ in 0..1 + rng.index(3) {
        if at + 4 > inner {
            break;
        }
        let to = at + 4 + rng.index(inner - at - 3);
        nest(rng, at, to, end, depth - 1, edges);
        at = to;
    }
}

/// A large body of `shape`: 2 000 to 6 000 blocks and a few unreachable after them, 1 000 to
/// 12 000 bits.
pub(super) fn large_case<'f>(frame: &'f Frame, rng: &mut SplitMix64, shape: Shape) -> Case<'f> {
    let reachable = 2000 + rng.index(4001);
    let domain = 1000 + rng.index(11_001);
    let mut edges = Vec::new();
    shaped(rng, shape, 0, reachable, &mut edges);
    let unreachable = rng.index(9);
    for b in reachable..reachable + unreachable {
        if b + 1 < reachable + unreachable {
            edges.push((b, b + 1));
        }
        edges.push((b, rng.index(reachable)));
    }
    let count = reachable + unreachable;
    let blocks = (0..count).map(|_| ops(rng, domain, 4)).collect();
    let boundary = boundary(rng, domain);
    let mut kinds = SplitMix64::new(rng.next_u64());
    Case::new(frame, domain, blocks, &edges, boundary, || kind(&mut kinds, domain))
}

/// The old tests' graphs: up to 16 blocks, up to 150 bits.
#[test]
fn the_fixpoints_agree_on_small_random_graphs() {
    for (seed, rounds) in [(0x2545_f491_4f6c_dd1d, 400), (0x853c_49e6_748f_ea9b, 400)] {
        let mut rng = SplitMix64::new(seed);
        for round in 0..rounds {
            frame::task(Tag::Mir, |frame| {
                let domain = 1 + rng.index(150);
                let case = random_case(frame, &mut rng, 16, domain);
                agree(frame, &format!("seed {seed:#x}, round {round}"), &case);
            });
        }
    }
}

/// Up to 40 blocks over up to 3 000 bits: rows of many lines, a few of them each.
#[test]
fn the_fixpoints_agree_on_random_graphs_of_many_lines() {
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    for round in 0..80 {
        frame::task(Tag::Mir, |frame| {
            let domain = 1 + rng.index(3000);
            let case = random_case(frame, &mut rng, 40, domain);
            agree(frame, &format!("round {round}"), &case);
        });
    }
}

/// Rows of exactly the dense limit and one word past it, where the pick changes store.
#[test]
fn the_fixpoints_agree_at_the_dense_limit() {
    let mut rng = SplitMix64::new(0xd1b5_4a32_d192_ed03);
    for round in 0..60 {
        let domain = match round % 4 {
            0 => DENSE_WORDS * 64,
            1 => DENSE_WORDS * 64 - 1,
            2 => DENSE_WORDS * 64 + 1,
            _ => (DENSE_WORDS + 1) * 64,
        };
        frame::task(Tag::Mir, |frame| {
            let case = random_case(frame, &mut rng, 24, domain);
            agree(frame, &format!("round {round}, domain {domain}"), &case);
        });
    }
}

/// Bodies of thousands of blocks and bits, of each shape: chains, diamonds, nested loops,
/// switch fans, irreducible tangles and all of them strung together.
#[test]
fn the_fixpoints_agree_on_large_bodies() {
    let mut rng = SplitMix64::new(0x6a09_e667_f3bc_c908);
    for shape in SHAPES {
        frame::task(Tag::Mir, |frame| {
            let case = large_case(frame, &mut rng, shape);
            let what = format!(
                "{shape:?}: {} blocks, {} bits, {} edges not the same",
                case.blocks.len(),
                case.domain,
                case.kinds.len(),
            );
            agree(frame, &what, &case);
        });
    }
}
