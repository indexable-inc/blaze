//! [`through_in`]'s promises: the same answer as the walk it replaced, from the same reads in
//! the same order (a property test against a reference copy of the old `through`, with a
//! planted-bug control); a second walk in the same scratch neither differs nor grows it; and
//! each edge's source is looked up once, however many fixpoint rounds the walk takes.

use std::format;

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc, Vec};
use blaze_sim::{Broke, Config, Tape};

use super::{ThroughScratch, through_in};
use crate::kind::{AmbiguityKind, BindingState, Def, Visibility};
use crate::module_scope::{GlobEdge, Globs};
use crate::through::{Asker, Node, Passed, SeenFrom, Sources, Through, merge};

pub(super) const K1: CrateItemKey = CrateItemKey(ContentId([1; 32]));
const K2: CrateItemKey = CrateItemKey(ContentId([2; 32]));

/// The item numbered `n`: the crate's 8 bytes, then `n` little-endian, then zeros.
pub(super) fn item(n: u32) -> ItemKey {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&K1.prefix());
    bytes[8..12].copy_from_slice(&n.to_le_bytes());
    ItemKey(ContentId(bytes))
}

/// The number [`item`] put in `key`.
fn number(key: ItemKey) -> usize {
    let b = key.0.0;
    u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as usize
}

pub(super) fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// Why a test source cannot be read: `Open` gives no value and no edge, `Closed` stops the walk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Halt {
    Open,
    Closed,
}

/// One source of a test graph.
pub(super) enum Spec {
    Value(Option<Passed>),
    Halt(Halt),
    Edges { edges: AVec<'static, GlobEdge>, krate: CrateItemKey, chain: AVec<'static, ItemKey> },
}

/// The test graph's sources, read by number, every read logged in order.
pub(super) struct Reader<'g> {
    graph: &'g [Spec],
    calls: AVec<'static, ItemKey>,
}

impl<'g> Reader<'g> {
    pub(super) fn new(graph: &'g [Spec]) -> Reader<'g> {
        Reader { graph, calls: Vec::new_in(heap()) }
    }
}

impl<'g> Sources<'g> for Reader<'g> {
    type Stop = Halt;

    fn open(stop: &Halt) -> bool {
        *stop == Halt::Open
    }

    fn node(&mut self, source: Def) -> Result<Node<'g>, Halt> {
        self.calls.push(source.key);
        let graph: &'g [Spec] = self.graph;
        match &graph[number(source.key)] {
            Spec::Value(v) => Ok(Node::Value(*v)),
            Spec::Halt(h) => Err(*h),
            Spec::Edges { edges, krate, chain } => {
                Ok(Node::Edges { edges: Globs::of(edges.as_slice()), krate: *krate, chain: chain.as_slice() })
            }
        }
    }
}

/// A bug planted in the reference: the oracle must see that the two walks differ.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Plant {
    None,
    /// Each node's last edge (of two or more) is left out of its merge.
    LastEdgeIgnored,
}

/// The old `through` (blaze-surface through.rs before GLOB-THROUGH-1), kept verbatim but for
/// the plant: the reference the new walk is compared with.
pub(super) fn reference<'w, W: Sources<'w>>(
    sources: &mut W,
    source: Def,
    alloc: Alloc<'_>,
    plant: Plant,
) -> Result<Option<Passed>, Through<W::Stop>> {
    let first = sources.node(source).map_err(Through::Stop)?;
    let Node::Edges { .. } = first else {
        let Node::Value(v) = first else { unreachable!("matched above") };
        return Ok(v);
    };
    let mut keys: AVec<'_, ItemKey> = Vec::new_in(alloc.clone());
    let mut nodes: AVec<'_, Node<'w>> = Vec::new_in(alloc.clone());
    keys.push(source.key);
    nodes.push(first);
    let mut open: Option<W::Stop> = None;
    let mut at = 0;
    while at < nodes.len() {
        if let Node::Edges { edges, .. } = nodes[at] {
            for e in edges.iter() {
                if keys.contains(&e.source.key) {
                    continue;
                }
                if keys.len() >= crate::through::MAX_REACHED {
                    return Err(Through::Unbounded);
                }
                let node = match sources.node(e.source) {
                    Ok(node) => node,
                    Err(stop) if W::open(&stop) => {
                        if open.is_none() {
                            open = Some(stop);
                        }
                        Node::Value(None)
                    }
                    Err(stop) => return Err(Through::Stop(stop)),
                };
                keys.push(e.source.key);
                nodes.push(node);
            }
        }
        at += 1;
    }
    let mut vals: AVec<'_, Option<Passed>> = Vec::with_capacity_in(nodes.len(), alloc);
    vals.extend(nodes.iter().map(|n| match n {
        Node::Value(v) => *v,
        Node::Edges { .. } => None,
    }));
    let bound = 4 * nodes.len() + 2;
    for _ in 0..bound {
        let mut changed = false;
        for i in 0..nodes.len() {
            let Node::Edges { edges, krate, chain } = nodes[i] else { continue };
            let asker = Asker { krate, chain };
            let mut acc = None;
            for (ei, e) in edges.iter().enumerate() {
                if plant == Plant::LastEdgeIgnored && edges.len() > 1 && ei + 1 == edges.len() {
                    continue;
                }
                let Some(j) = keys.iter().position(|k| *k == e.source.key) else { continue };
                let Some(p) = vals[j] else { continue };
                if !p.vis.visible_from(asker) {
                    continue;
                }
                let p = Passed { vis: Visibility::narrower(e.vis, p.vis, asker), ..p };
                acc = Some(merge(acc, p, asker));
            }
            if acc != vals[i] {
                vals[i] = acc;
                changed = true;
            }
        }
        if !changed {
            return match (vals[0], open) {
                (None, Some(stop)) => Err(Through::Stop(stop)),
                (v, _) => Ok(v),
            };
        }
    }
    Err(Through::Unbounded)
}

/// The modules a chain and a restricted visibility draw from: few, so they meet.
const MODULES: [u32; 4] = [100_000, 100_001, 100_002, 100_003];

fn vis(tape: &Tape) -> Visibility {
    match tape.below(4, "visibility") {
        0 => Visibility::Public,
        1 => Visibility::Crate(K1),
        2 => Visibility::Crate(K2),
        _ => Visibility::Restricted(item(MODULES[tape.below(4, "restricted to") as usize])),
    }
}

/// A random glob graph of up to 300 sources (source 0 is asked, and has edges): values,
/// sources that cannot be read (open or not), and nodes with up to five edges to any source,
/// so cycles and shared targets are common; visibilities of every form, two crates.
pub(super) fn draw(tape: &Tape) -> AVec<'static, Spec> {
    let n = 1 + tape.below(300, "sources") as u32;
    let mut graph: AVec<'static, Spec> = Vec::new_in(heap());
    for i in 0..n {
        let kind = if i == 0 { 9 } else { tape.below(10, "kind") };
        let spec = match kind {
            0 => Spec::Value(None),
            1 | 2 => {
                let def = Def::module(item(200_000 + tape.below(3, "def") as u32));
                let state = if tape.coin("ambiguous") {
                    BindingState::Ambiguous(AmbiguityKind::GlobVsGlob)
                } else {
                    BindingState::Determined
                };
                Spec::Value(Some(Passed { def, vis: vis(tape), state }))
            }
            3 => Spec::Halt(if tape.below(8, "closed") == 0 { Halt::Closed } else { Halt::Open }),
            _ => {
                let mut edges = Vec::new_in(heap());
                for _ in 0..tape.below(6, "edges") {
                    let to = tape.below(u64::from(n), "edge to") as u32;
                    edges.push(GlobEdge { source: Def::module(item(to)), vis: vis(tape), expansion: 0 });
                }
                let mut chain = Vec::new_in(heap());
                chain.push(item(i));
                for m in MODULES {
                    if tape.coin("in chain") {
                        chain.push(item(m));
                    }
                }
                let krate = if tape.coin("second crate") { K2 } else { K1 };
                Spec::Edges { edges, krate, chain }
            }
        };
        graph.push(spec);
    }
    graph
}

/// One case: the new walk against the reference (with `plant` in the reference), then a
/// second walk of the same graph in the same scratch.
fn agrees(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let graph = draw(tape);
    let root = Def::module(item(0));
    let mut old = Reader::new(&graph);
    let want = reference(&mut old, root, heap(), plant);
    let mut scratch = ThroughScratch::new_in(heap());
    let mut new = Reader::new(&graph);
    let got = through_in(&mut new, root, &mut scratch);
    if got != want {
        return Err(Broke::Oracle(format!("through_in differs from the reference: {got:?} against {want:?}")));
    }
    if new.calls.as_slice() != old.calls.as_slice() {
        return Err(Broke::Oracle(format!(
            "through_in read the sources in another order: {} reads against {}",
            new.calls.len(),
            old.calls.len()
        )));
    }
    let caps = scratch.capacities();
    let mut again = Reader::new(&graph);
    let second = through_in(&mut again, root, &mut scratch);
    if second != got {
        return Err(Broke::Oracle(format!("a second walk in the same scratch gave {second:?}, the first {got:?}")));
    }
    if scratch.capacities() != caps {
        return Err(Broke::Oracle(format!(
            "the scratch grew on a second walk of the same graph: {:?} after {caps:?}",
            scratch.capacities()
        )));
    }
    Ok(())
}

/// The new walk equals the old on random glob graphs, reads the same sources in the same
/// order, and reuses its scratch without growing it; the oracle catches a reference that
/// leaves each node's last edge out. Expected: `test through_scratch::tests::through_in_is_the_old_walk ... ok`
/// with the control's observe block naming `LastEdgeIgnored` caught.
#[test]
fn through_in_is_the_old_walk() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::LastEdgeIgnored, "differs from the reference", agrees);
}

/// The counter test: a ring of 40 glob nodes, a value entering at its far end, needs about 40
/// fixpoint rounds to reach the asked source. Each of the 41 edges' sources is looked up once
/// (41 finds); the old walk searched the reached keys for every edge in every round (about
/// 41 x 41), and a fix that still looks up in the fixpoint counts more than 41 here.
/// Expected: `test through_scratch::tests::each_edge_is_found_once ... ok`.
#[test]
fn each_edge_is_found_once() {
    const RING: u32 = 40;
    let value = Passed { def: Def::module(item(300_000)), vis: Visibility::Public, state: BindingState::Determined };
    let mut graph: AVec<'static, Spec> = Vec::new_in(heap());
    for i in 0..RING {
        let mut edges = Vec::new_in(heap());
        edges.push(GlobEdge { source: Def::module(item((i + 1) % RING)), vis: Visibility::Public, expansion: 0 });
        if i == RING - 1 {
            edges.push(GlobEdge { source: Def::module(item(RING)), vis: Visibility::Public, expansion: 0 });
        }
        let mut chain = Vec::new_in(heap());
        chain.push(item(i));
        graph.push(Spec::Edges { edges, krate: K1, chain });
    }
    graph.push(Spec::Value(Some(value)));
    let root = Def::module(item(0));
    let mut scratch = ThroughScratch::new_in(heap());
    let got = through_in(&mut Reader::new(&graph), root, &mut scratch);
    assert_eq!(got, Ok(Some(value)), "the value reaches the asked source round the ring");
    assert_eq!(got, reference(&mut Reader::new(&graph), root, heap(), Plant::None), "the same answer as the old walk");
    assert_eq!(scratch.finds(), u64::from(RING) + 1, "each edge's source is looked up once, not once per round");
}
