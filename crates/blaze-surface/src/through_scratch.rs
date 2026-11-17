//! A lookup through glob edges ([`through_in`]) with its working memory owned by the caller
//! ([`ThroughScratch`]): the same walk and the same least fixpoint as [`crate::through`], with
//! two changes of cost and none of answer.
//!
//! - Slots, not searches. A source reached is numbered (its slot) once, when the walk first
//!   meets it; each edge is resolved to its target's slot once, when its node is walked; the
//!   fixpoint reads `vals[slot]`. The old walk searched the reached keys linearly for every
//!   edge in the walk and again for every edge in every round (O(nodes x edges x rounds)):
//!   10.5% of a hello world's `blaze run` (port27/hello-1116 TABLES-NOTES, GLOB-THROUGH-1).
//! - A scratch reused. The vectors live in the caller's [`ThroughScratch`] (one per pass, held
//!   by blaze-scope's `Sink`), cleared per call with their capacity kept: the old walk grew two
//!   vectors from empty on every call and freed them at return (1.8% of the same run).
//!
//! Output is the old function's exactly: the sources are read in the same order (breadth
//! first, each once, by key), each node's edges are merged in the same order, and the rounds
//! and their bound are the same. A map's order never reaches an answer: the key index is a
//! lookup only. The memo lives for one call; the scratch holds no answer between calls.

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc, Vec};

use crate::kind::{Def, Visibility};
use crate::module_scope::GlobEdge;
use crate::through::{Asker, MAX_REACHED, Node, Passed, SeenFrom, Sources, Through, merge};

/// Up to this many reached sources a key is found by a linear scan of them (a few cache lines,
/// no hashing); past it the index map is filled once and used from then on.
const LINEAR: usize = 16;

/// A reached source as the fixpoint reads it: a known value (in `vals`), or edges and who it is.
/// Ranges index the scratch's flat `edges`/`edge_slots` and `chains` arrays.
#[derive(Clone, Copy, Debug)]
enum Slot {
    Value,
    Edges { krate: CrateItemKey, edges: (u32, u32), chain: (u32, u32) },
}

/// The working memory of [`through_in`], owned by its caller and passed in by `&mut`: cleared
/// at the start of every call, its capacity kept, so a pass that looks up many names through
/// globs allocates only while its largest walk grows. Holds nothing between calls that an
/// answer depends on.
pub struct ThroughScratch<'a> {
    /// Slot -> the source's key, in the order the walk reached them (slot 0: the asked source).
    keys: AVec<'a, ItemKey>,
    /// Key -> slot, filled only once more than [`LINEAR`] sources are reached; a lookup only.
    index: Map<ItemKey, u32, FixedState, Alloc<'a>>,
    /// Slot -> what the source is.
    slots: AVec<'a, Slot>,
    /// Every walked node's glob edges, copied once when the node is reached, node after node.
    edges: AVec<'a, GlobEdge>,
    /// Beside `edges`: the slot of each edge's source, resolved once when its node is walked.
    edge_slots: AVec<'a, u32>,
    /// Every walked node's chain of modules (itself first), node after node.
    chains: AVec<'a, ItemKey>,
    /// Slot -> its value so far in the fixpoint.
    vals: AVec<'a, Option<Passed>>,
    /// How many times a key was looked up among the reached sources, over the scratch's life:
    /// once per edge walked, never in the fixpoint (the counter test reads it).
    finds: u64,
}

impl<'a> ThroughScratch<'a> {
    /// An empty scratch in `alloc`. It allocates nothing until its first walk.
    pub fn new_in(alloc: Alloc<'a>) -> ThroughScratch<'a> {
        ThroughScratch {
            keys: Vec::new_in(alloc),
            index: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            slots: Vec::new_in(alloc),
            edges: Vec::new_in(alloc),
            edge_slots: Vec::new_in(alloc),
            chains: Vec::new_in(alloc),
            vals: Vec::new_in(alloc),
            finds: 0,
        }
    }

    /// How many key lookups among reached sources this scratch has served: one per edge of
    /// every node walked, whatever the number of fixpoint rounds.
    pub fn finds(&self) -> u64 {
        self.finds
    }

    fn clear(&mut self) {
        self.keys.clear();
        self.index.clear();
        self.slots.clear();
        self.edges.clear();
        self.edge_slots.clear();
        self.chains.clear();
        self.vals.clear();
    }

    /// The slot of `key`, if the walk has reached it.
    fn find(&mut self, key: ItemKey) -> Option<u32> {
        self.finds += 1;
        if self.keys.len() <= LINEAR {
            let at = self.keys.iter().position(|k| *k == key)?;
            // Below MAX_REACHED, so it fits.
            return u32::try_from(at).ok();
        }
        self.index.get(&key).copied()
    }

    /// Numbers `key` as the next slot and records what its source is; `None` past u32 (never
    /// reached: the walk stops at MAX_REACHED first).
    fn add(&mut self, key: ItemKey, node: Node<'_>) -> Option<u32> {
        let slot = u32::try_from(self.keys.len()).ok()?;
        self.keys.push(key);
        if self.keys.len() == LINEAR + 1 {
            // Past the linear scan's reach: index every key reached so far, once.
            self.index.reserve(self.keys.len());
            for (i, k) in self.keys.iter().enumerate() {
                self.index.insert(*k, u32::try_from(i).ok()?);
            }
        } else if self.keys.len() > LINEAR + 1 {
            self.index.insert(key, slot);
        }
        match node {
            Node::Value(v) => {
                self.slots.push(Slot::Value);
                self.vals.push(v);
            }
            Node::Edges { edges, krate, chain } => {
                let e0 = u32::try_from(self.edges.len()).ok()?;
                self.edges.extend(edges.iter());
                let e1 = u32::try_from(self.edges.len()).ok()?;
                let c0 = u32::try_from(self.chains.len()).ok()?;
                self.chains.extend_from_slice(chain);
                let c1 = u32::try_from(self.chains.len()).ok()?;
                self.slots.push(Slot::Edges { krate, edges: (e0, e1), chain: (c0, c1) });
                self.vals.push(None);
            }
        }
        Some(slot)
    }

    /// The capacities of every buffer: a second walk of a graph no larger than the first does
    /// not grow them (the reuse test reads it).
    pub fn capacities(&self) -> [usize; 7] {
        [
            self.keys.capacity(),
            self.index.capacity(),
            self.slots.capacity(),
            self.edges.capacity(),
            self.edge_slots.capacity(),
            self.chains.capacity(),
            self.vals.capacity(),
        ]
    }
}

/// `source`'s value for the name `sources` reads, through its glob edges, walked in `scratch`.
/// The same answer as [`crate::through`] (which makes a scratch and calls this), from the same
/// reads in the same order.
pub fn through_in<'w, W: Sources<'w>>(
    sources: &mut W,
    source: Def,
    scratch: &mut ThroughScratch<'_>,
) -> Result<Option<Passed>, Through<W::Stop>> {
    let first = sources.node(source).map_err(Through::Stop)?;
    if let Node::Value(v) = first {
        return Ok(v);
    }
    // A source with no binding and no glob edge holds nothing under the name: the walk below
    // would read no other node and answer `None`. Answered here, the scratch is not touched, so a
    // miss in a glob-free module (a lexical layer passed through) allocates nothing (D114 rule 1).
    if matches!(first, Node::Edges { edges, .. } if edges.is_empty()) {
        return Ok(None);
    }
    scratch.clear();
    scratch.add(source.key, first).ok_or(Through::Unbounded)?;
    // The first source that could not be read, when open nodes are allowed.
    let mut open: Option<W::Stop> = None;
    // Breadth first, as the old walk: node `at`'s edges are resolved to slots in order, a
    // source not reached before is read and numbered now (its edges copied after the others).
    let mut at = 0;
    while at < scratch.slots.len() {
        if let Slot::Edges { edges: (e0, e1), .. } = scratch.slots[at] {
            for k in e0..e1 {
                let e = scratch.edges[k as usize];
                if let Some(slot) = scratch.find(e.source.key) {
                    scratch.edge_slots.push(slot);
                    continue;
                }
                if scratch.keys.len() >= MAX_REACHED {
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
                let slot = scratch.add(e.source.key, node).ok_or(Through::Unbounded)?;
                scratch.edge_slots.push(slot);
            }
        }
        at += 1;
    }
    // Every edge was walked once, in the order the edges are stored: `edge_slots[k]` is edge k's.
    debug_assert_eq!(scratch.edge_slots.len(), scratch.edges.len());
    let ThroughScratch { slots, edges, edge_slots, chains, vals, .. } = scratch;
    let n = slots.len();
    // Every round either settles or raises some value one step in a lattice of height 4 per
    // source (none, one item, wider, ambiguous), so the bound is never reached by a sound walk.
    let bound = 4 * n + 2;
    for _ in 0..bound {
        let mut changed = false;
        for i in 0..n {
            let Slot::Edges { krate, edges: (e0, e1), chain: (c0, c1) } = slots[i] else { continue };
            let asker = Asker { krate, chain: &chains[c0 as usize..c1 as usize] };
            let mut acc = None;
            for k in e0 as usize..e1 as usize {
                let Some(p) = vals[edge_slots[k] as usize] else { continue };
                if !p.vis.visible_from(asker) {
                    continue;
                }
                let p = Passed { vis: Visibility::narrower(edges[k].vis, p.vis, asker), ..p };
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

#[cfg(test)]
mod tests;
