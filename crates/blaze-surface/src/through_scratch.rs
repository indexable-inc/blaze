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
//! - Borrowed, not copied. A reached node's glob edges and chain stay where the world holds
//!   them (a record's rows read in place, or the working form's slices) for the length of one
//!   walk: the walk keeps the [`Node`] the world handed it, never a copy of its edges or its
//!   chain (the walk before copied every node's edges and chain into the scratch: 0 bytes per
//!   node now). The node list borrows the world for one walk ([`ThroughScratch::walk`] lends
//!   its buffer as a [`Walk`] of that world's lifetime and takes it back, cleared, when the walk
//!   ends), so a scratch is kept across walks, sources and worlds with no lifetime of a world
//!   in its type.
//!
//! Output is the old function's exactly: the sources are read in the same order (breadth
//! first, each once, by key), each node's edges are merged in the same order, and the rounds
//! and their bound are the same. A map's order never reaches an answer: the key index is a
//! lookup only. The memo lives for one call; the scratch holds no answer between calls.

use core::mem;

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc, Vec};

use crate::kind::{Def, Visibility};
use crate::through::{Asker, MAX_REACHED, Node, Passed, SeenFrom, Sources, Through, merge};

/// Up to this many reached sources a key is found by a linear scan of them (a few cache lines,
/// no hashing); past it the index map is filled once and used from then on.
const LINEAR: usize = 16;

/// A reached source as one walk holds it: the node the world handed (a known value, or its
/// edges and chain, borrowed), and where its edges' slots begin in the scratch's `edge_slots`.
#[derive(Clone, Copy, Debug)]
struct Reached<'w> {
    node: Node<'w>,
    first_edge: u32,
}

/// The working memory of [`through_in`], owned by its caller and passed in by `&mut`: cleared
/// at the start of every call, its capacity kept, so a pass that looks up many names through
/// globs allocates only while its largest walk grows. Holds nothing between calls that an
/// answer depends on, and no borrow of any world: its node list is lent to one walk at a time.
pub struct ThroughScratch<'a> {
    /// The allocator every buffer is in: the caller's, handed to [`ThroughScratch::new_in`].
    alloc: Alloc<'a>,
    /// Slot -> the source's key, in the order the walk reached them (slot 0: the asked source).
    keys: AVec<'a, ItemKey>,
    /// Key -> slot, filled only once more than [`LINEAR`] sources are reached; a lookup only.
    index: Map<ItemKey, u32, FixedState, Alloc<'a>>,
    /// Slot -> the node the world handed: EMPTY between walks. Its block is lent to each walk
    /// as the node list of that walk's world ([`Walk`]) and given back empty, so the lifetime
    /// here is never a world's (`'static` names no borrow: the vector holds no value).
    reached: AVec<'a, Reached<'static>>,
    /// The slot of each walked edge's source, node after node, resolved once when its node is
    /// walked (node `i`'s edges from `reached[i].first_edge`).
    edge_slots: AVec<'a, u32>,
    /// Slot -> its value so far in the fixpoint.
    vals: AVec<'a, Option<Passed>>,
    /// How many times a key was looked up among the reached sources, over the scratch's life:
    /// once per edge walked, never in the fixpoint (the counter test reads it).
    finds: u64,
    /// How many walks used this scratch (lookups answered before any walk do not count).
    walks: u64,
    /// PLANTED BUG ([`ThroughScratch::plant_stale_values`]): the values are not cleared between
    /// walks.
    stale_values: bool,
}

impl<'a> ThroughScratch<'a> {
    /// An empty scratch in `alloc`. It allocates nothing until its first walk.
    pub fn new_in(alloc: Alloc<'a>) -> ThroughScratch<'a> {
        ThroughScratch {
            alloc,
            keys: Vec::new_in(alloc),
            index: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            reached: Vec::new_in(alloc),
            edge_slots: Vec::new_in(alloc),
            vals: Vec::new_in(alloc),
            finds: 0,
            walks: 0,
            stale_values: false,
        }
    }

    /// How many key lookups among reached sources this scratch has served: one per edge of
    /// every node walked, whatever the number of fixpoint rounds.
    pub fn finds(&self) -> u64 {
        self.finds
    }

    /// How many walks through globs this scratch served (a lookup answered by its first node,
    /// a value or a node with no edge, walks nothing and is not counted).
    pub fn walks(&self) -> u64 {
        self.walks
    }

    /// PLANTED BUG, for the tests of a held scratch only: from now on the fixpoint's values are
    /// not cleared between walks, so a walk reads, at a slot, the value an earlier walk left
    /// there. A scratch kept across lookups must then give answers a fresh scratch per lookup
    /// does not: the control that the clear is what keeps a held scratch's answers equal.
    #[doc(hidden)]
    pub fn plant_stale_values(&mut self) {
        self.stale_values = true;
    }

    fn clear(&mut self) {
        self.keys.clear();
        self.index.clear();
        self.edge_slots.clear();
        if !self.stale_values {
            self.vals.clear();
        }
    }

    /// The node list lent to one walk of a world `'w`, the rest cleared: the walk borrows the
    /// world's edges and chains through it, and gives the block back, empty, when it ends.
    fn walk<'s, 'w>(&'s mut self) -> Walk<'s, 'a, 'w> {
        self.clear();
        self.walks += 1;
        let lent = mem::replace(&mut self.reached, Vec::new_in(self.alloc));
        Walk { reached: relifetime(lent), edges: 0, scratch: self }
    }

    /// The capacities of every buffer: a second walk of a graph no larger than the first does
    /// not grow them (the reuse test reads it).
    pub fn capacities(&self) -> [usize; 5] {
        [
            self.keys.capacity(),
            self.index.capacity(),
            self.reached.capacity(),
            self.edge_slots.capacity(),
            self.vals.capacity(),
        ]
    }
}

/// `v`'s block as an empty vector of the same values under another lifetime: what lends the
/// scratch's node list to one walk and takes it back.
fn relifetime<'a, 'x, 'y>(mut v: AVec<'a, Reached<'x>>) -> AVec<'a, Reached<'y>> {
    v.clear();
    let (ptr, _, capacity, alloc) = v.into_raw_parts_with_alloc();
    // SAFETY: `Reached<'x>` and `Reached<'y>` differ only in a lifetime, which no layout depends
    // on: one size, one alignment. So the block `ptr` of `capacity` values, which `alloc` made for
    // `Reached<'x>` (or, at capacity 0, the dangling aligned pointer of an empty vector), is
    // exactly a block of `capacity` `Reached<'y>`, freed later by the same `alloc` with the same
    // layout. The length is 0: no value of lifetime `'x` is ever read as one of `'y`.
    unsafe { Vec::from_raw_parts_in(ptr.cast::<Reached<'y>>(), 0, capacity, alloc) }
}

/// One walk's view of a [`ThroughScratch`]: the scratch's buffers, and its node list as the
/// nodes of the world `'w`, borrowed for this walk alone. Dropped (at the walk's end, or on an
/// early return), it gives the node list back to the scratch, empty.
struct Walk<'s, 'a, 'w> {
    scratch: &'s mut ThroughScratch<'a>,
    reached: AVec<'a, Reached<'w>>,
    /// The edges of the nodes reached so far: where the next node's edge slots begin.
    edges: u32,
}

impl Drop for Walk<'_, '_, '_> {
    fn drop(&mut self) {
        let lent = mem::replace(&mut self.reached, Vec::new_in(self.scratch.alloc));
        self.scratch.reached = relifetime(lent);
    }
}

impl<'w> Walk<'_, '_, 'w> {
    /// The slot of `key`, if the walk has reached it.
    fn find(&mut self, key: ItemKey) -> Option<u32> {
        let scratch = &mut *self.scratch;
        scratch.finds += 1;
        if scratch.keys.len() <= LINEAR {
            let at = scratch.keys.iter().position(|k| *k == key)?;
            // Below MAX_REACHED, so it fits.
            return u32::try_from(at).ok();
        }
        scratch.index.get(&key).copied()
    }

    /// Numbers `key` as the next slot and keeps its node as the world handed it (nothing of it
    /// copied); `None` past u32 (never reached: the walk stops at MAX_REACHED first).
    fn add(&mut self, key: ItemKey, node: Node<'w>) -> Option<u32> {
        let scratch = &mut *self.scratch;
        let slot = u32::try_from(scratch.keys.len()).ok()?;
        scratch.keys.push(key);
        if scratch.keys.len() == LINEAR + 1 {
            // Past the linear scan's reach: index every key reached so far, once.
            scratch.index.reserve(scratch.keys.len());
            for (i, k) in scratch.keys.iter().enumerate() {
                scratch.index.insert(*k, u32::try_from(i).ok()?);
            }
        } else if scratch.keys.len() > LINEAR + 1 {
            scratch.index.insert(key, slot);
        }
        let first_edge = self.edges;
        match node {
            Node::Value(v) => scratch.vals.push(v),
            Node::Edges { edges, .. } => {
                self.edges = self.edges.checked_add(u32::try_from(edges.len()).ok()?)?;
                scratch.vals.push(None);
            }
        }
        self.reached.push(Reached { node, first_edge });
        Some(slot)
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
    let mut walk: Walk<'_, '_, 'w> = scratch.walk();
    walk.add(source.key, first).ok_or(Through::Unbounded)?;
    // The first source that could not be read, when open nodes are allowed.
    let mut open: Option<W::Stop> = None;
    // Breadth first, as the old walk: node `at`'s edges are resolved to slots in order, a
    // source not reached before is read and numbered now (its edge slots after the others').
    let mut at = 0;
    while at < walk.reached.len() {
        if let Node::Edges { edges, .. } = walk.reached[at].node {
            for e in edges.iter() {
                if let Some(slot) = walk.find(e.source.key) {
                    walk.scratch.edge_slots.push(slot);
                    continue;
                }
                if walk.scratch.keys.len() >= MAX_REACHED {
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
                let slot = walk.add(e.source.key, node).ok_or(Through::Unbounded)?;
                walk.scratch.edge_slots.push(slot);
            }
        }
        at += 1;
    }
    // Every edge was walked once, in the order the nodes and their edges were reached:
    // `edge_slots[reached[i].first_edge + j]` is node i's edge j's.
    debug_assert_eq!(walk.scratch.edge_slots.len(), walk.edges as usize);
    let reached = &walk.reached;
    let ThroughScratch { edge_slots, vals, .. } = &mut *walk.scratch;
    let n = reached.len();
    // Every round either settles or raises some value one step in a lattice of height 4 per
    // source (none, one item, wider, ambiguous), so the bound is never reached by a sound walk.
    let bound = 4 * n + 2;
    for _ in 0..bound {
        let mut changed = false;
        for i in 0..n {
            let Reached { node: Node::Edges { edges, krate, chain }, first_edge } = reached[i] else { continue };
            let asker = Asker { krate, chain };
            let mut acc = None;
            // Each edge read where the world holds it (a record's row decodes its 48 bytes).
            for (j, e) in edges.iter().enumerate() {
                let Some(p) = vals[edge_slots[first_edge as usize + j] as usize] else { continue };
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

#[cfg(test)]
mod tests;
#[cfg(test)]
mod borrow_tests;
