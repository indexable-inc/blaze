//! [`crate::offer`] against random call graphs of at most ten nodes, with cycles, self edges,
//! ties in heat and bodies missing. The oracle is a plain O(n^2) rerun of the same greedy (a
//! linear scan for the best candidate instead of a heap, a linear `contains` instead of sorted
//! lists); the invariants are the brief's: sorted, unique, root absent, each member at most
//! `one`, the sum at most `total`, each member reachable from the root through members within
//! `depth`. The graph's edges listed reversed give the same set and the same digest.
//!
//! Expected: `test offer_tests::offers_match_the_plain_greedy ... ok`, and for the control
//! `test offer_tests::the_oracle_catches_an_offer_over_the_total ... ok` with stderr
//! "plant IgnoreTotal: caught at case k of 512".

use std::collections::VecDeque as StdDeque;

use blaze_own::{AVec, Alloc};
use blaze_sim::{Broke, Config, Tape, check, control};

use crate::offer::{Body, Edge, Graph, OFFER, OfferBound, OfferError, offer, offer_digest, offer_planted};
use crate::sim::{key, oracle};
use crate::tier::Callee;
use crate::{CodeId, InstanceKey, Plant};

/// A call graph over nodes `0..n`; node `i` is `InstanceKey(key(i))`.
struct Nodes {
    bodies: Vec<Option<Body>>,
    edges: Vec<Vec<Edge>>,
    /// List each node's edges back to front: the order the offer must not depend on.
    reversed: bool,
}

fn instance(n: usize) -> InstanceKey {
    InstanceKey(key(n as u64))
}

fn index(instance: InstanceKey) -> usize {
    let mut low = [0u8; 8];
    low.copy_from_slice(&instance.0[..8]);
    usize::try_from(u64::from_le_bytes(low)).expect("a test node index fits usize")
}

impl Graph for Nodes {
    fn body(&self, instance: InstanceKey) -> Option<Body> {
        self.bodies.get(index(instance)).copied().flatten()
    }

    fn callees(&self, instance: InstanceKey, out: &mut AVec<'_, Edge>) {
        let edges = &self.edges[index(instance)];
        if self.reversed {
            edges.iter().rev().for_each(|edge| out.push(*edge));
        } else {
            edges.iter().for_each(|edge| out.push(*edge));
        }
    }
}

fn draw_graph(tape: &Tape) -> Nodes {
    let n = usize::try_from(tape.range(1..11, "nodes")).expect("small");
    let bodies = (0..n)
        .map(|i| {
            if tape.one_in(8, "no body") {
                None
            } else {
                let size = u32::try_from(tape.below(25, "size")).expect("small");
                Some(Body { code: CodeId(key(1000 + i as u64)), size })
            }
        })
        .collect();
    let edges = (0..n)
        .map(|_| {
            let count = tape.below(4, "edges");
            (0..count)
                .map(|_| Edge {
                    callee: instance(usize::try_from(tape.below(n as u64, "callee")).expect("small")),
                    heat: tape.below(4, "heat"),
                })
                .collect()
        })
        .collect();
    Nodes { bodies, edges, reversed: false }
}

fn draw_bound(tape: &Tape) -> OfferBound {
    OfferBound {
        one: u32::try_from(tape.below(30, "one")).expect("small"),
        total: u32::try_from(tape.below(60, "total")).expect("small"),
        depth: u8::try_from(tape.below(4, "depth")).expect("small"),
    }
}

/// The same greedy written plainly: every candidate in a list, the best found by a scan.
fn plain_greedy(root: InstanceKey, graph: &Nodes, bound: OfferBound) -> Result<Vec<Callee>, OfferError> {
    // (heat, size, instance, depth, code)
    type Row = (u64, u32, InstanceKey, u8, CodeId);
    fn read(graph: &Nodes, from: InstanceKey, depth: u8, rows: &mut Vec<Row>) -> Result<(), OfferError> {
        let mut edges = graph.edges[index(from)].clone();
        edges.sort_by(|a, b| a.callee.cmp(&b.callee).then(b.heat.cmp(&a.heat)));
        for edge in edges {
            let body = graph.body(edge.callee).ok_or(OfferError::NoBody { instance: edge.callee })?;
            rows.push((edge.heat, body.size, edge.callee, depth, body.code));
        }
        Ok(())
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut decided: Vec<InstanceKey> = Vec::new();
    let mut taken: Vec<Callee> = Vec::new();
    let mut running: u64 = 0;
    if bound.depth >= 1 {
        read(graph, root, 1, &mut rows)?;
    }
    while !rows.is_empty() {
        let mut best = 0;
        for at in 1..rows.len() {
            let (h, s, i, d, _) = rows[at];
            let (bh, bs, bi, bd, _) = rows[best];
            if (std::cmp::Reverse(h), s, i, d) < (std::cmp::Reverse(bh), bs, bi, bd) {
                best = at;
            }
        }
        let (_, size, inst, depth, code) = rows.remove(best);
        if inst == root || decided.contains(&inst) {
            continue;
        }
        decided.push(inst);
        if size > bound.one || running + u64::from(size) > u64::from(bound.total) {
            continue;
        }
        taken.push(Callee { instance: inst, code });
        running += u64::from(size);
        if depth < bound.depth {
            read(graph, inst, depth + 1, &mut rows)?;
        }
    }
    taken.sort_by(|a, b| a.instance.cmp(&b.instance));
    Ok(taken)
}

/// The invariants of an offer set `got` for `root` in `graph` under `bound`. The total is
/// checked first: the planted bug breaks it before anything else.
fn invariants(root: InstanceKey, graph: &Nodes, bound: OfferBound, got: &[Callee]) -> Result<(), Broke> {
    let size = |row: &Callee| graph.body(row.instance).map_or(0, |body| body.size);
    let sum: u64 = got.iter().map(|row| u64::from(size(row))).sum();
    oracle(sum <= u64::from(bound.total), || {
        format!("over the bound: the offer holds {sum} instructions, the total is {}", bound.total)
    })?;
    oracle(got.iter().all(|row| size(row) <= bound.one), || {
        format!("over the bound: a member is larger than one = {}", bound.one)
    })?;
    oracle(got.windows(2).all(|w| w[0].instance < w[1].instance), || {
        "the offer is not sorted by instance with each instance once".to_string()
    })?;
    oracle(got.iter().all(|row| row.instance != root), || "the root is offered".to_string())?;
    oracle(got.iter().all(|row| graph.body(row.instance).map(|b| b.code) == Some(row.code)), || {
        "a member's code is not its body's".to_string()
    })?;
    // Breadth-first from the root through members only.
    let member = |i: InstanceKey| got.iter().any(|row| row.instance == i);
    let mut hops: Vec<Option<u8>> = vec![None; graph.bodies.len()];
    hops[index(root)] = Some(0);
    let mut queue = StdDeque::from([root]);
    while let Some(from) = queue.pop_front() {
        let at = hops[index(from)].expect("queued nodes have hops");
        if at >= bound.depth {
            continue;
        }
        for edge in &graph.edges[index(from)] {
            if member(edge.callee) && hops[index(edge.callee)].is_none() {
                hops[index(edge.callee)] = Some(at + 1);
                queue.push_back(edge.callee);
            }
        }
    }
    oracle(got.iter().all(|row| hops[index(row.instance)].is_some()), || {
        format!("a member is not reachable from the root through members within depth {}", bound.depth)
    })
}

fn offers_agree(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut graph = draw_graph(tape);
    let bound = draw_bound(tape);
    let root = instance(0);
    let got = offer_planted(root, &graph, bound, Alloc::global(), plant);
    if let Ok(got) = &got {
        invariants(root, &graph, bound, got.as_slice())?;
    }
    let want = plain_greedy(root, &graph, bound);
    let same = match (&got, &want) {
        (Ok(got), Ok(want)) => got.as_slice() == want.as_slice(),
        (Err(got), Err(want)) => got == want,
        _ => false,
    };
    oracle(same, || format!("the offer gave {got:?} where the plain greedy gives {want:?}"))?;
    graph.reversed = true;
    let flipped = offer_planted(root, &graph, bound, Alloc::global(), plant);
    let same_order = match (&got, &flipped) {
        (Ok(got), Ok(flipped)) => {
            got.as_slice() == flipped.as_slice() && offer_digest(got) == offer_digest(flipped)
        }
        (Err(got), Err(flipped)) => got == flipped,
        _ => false,
    };
    oracle(same_order, || "the graph's edges listed reversed gave another offer".to_string())
}

/// The refinement: `offer` matches the plain O(n^2) greedy reference on random call graphs while respecting per-member, total-size, and depth bounds.
#[test]
fn offers_match_the_plain_greedy() {
    check(Config::cases(512), |tape| offers_agree(tape, Plant::None));
}

#[test]
fn the_oracle_catches_an_offer_over_the_total() {
    control(Config::cases(512), Plant::None, Plant::IgnoreTotal, "over the bound", offers_agree);
}

/// A fixed graph: 0 calls 1 (hot), 2 and 3; 1 calls 0 (a cycle), 4 and itself; 3 is too large
/// for `one`. Under [`OFFER`] the set is {1, 2, 4}.
fn fixed(reversed: bool, code_of_four: u64) -> Nodes {
    let body = |code: u64, size: u32| Some(Body { code: CodeId(key(code)), size });
    let edge = |callee: usize, heat: u64| Edge { callee: instance(callee), heat };
    Nodes {
        bodies: vec![body(1000, 50), body(1001, 120), body(1002, 30), body(1003, 900), body(code_of_four, 10)],
        edges: vec![
            vec![edge(1, 90), edge(2, 5), edge(3, 40)],
            vec![edge(0, 7), edge(4, 60), edge(1, 3)],
            vec![],
            vec![],
            vec![],
        ],
        reversed,
    }
}

/// D100 point 8: the digest is a function of the set, not of the order edges were listed in,
/// and a changed code identity of a member changes it.
#[test]
fn the_digest_is_one_for_two_orders_and_moves_with_a_code_id() {
    let root = instance(0);
    let forward = offer(root, &fixed(false, 1004), OFFER, Alloc::global()).expect("all bodies present");
    let backward = offer(root, &fixed(true, 1004), OFFER, Alloc::global()).expect("all bodies present");
    let members: Vec<InstanceKey> = forward.iter().map(|row| row.instance).collect();
    assert_eq!(members, vec![instance(1), instance(2), instance(4)], "the fixed graph's offer");
    assert_eq!(offer_digest(&forward), offer_digest(&backward), "two edge orders, one digest");
    let edited = offer(root, &fixed(false, 2004), OFFER, Alloc::global()).expect("all bodies present");
    let members_edited: Vec<InstanceKey> = edited.iter().map(|row| row.instance).collect();
    assert_eq!(members, members_edited, "an edit of a body's code keeps the members");
    assert_ne!(offer_digest(&forward), offer_digest(&edited), "a changed CodeId changes the digest");
}

/// An edge to an instance without a body is refused by name, never skipped.
#[test]
fn an_edge_without_a_body_is_refused() {
    let mut graph = fixed(false, 1004);
    graph.bodies[2] = None;
    let got = offer(instance(0), &graph, OFFER, Alloc::global());
    assert_eq!(got.err(), Some(OfferError::NoBody { instance: instance(2) }));
}
