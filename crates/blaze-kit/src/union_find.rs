//! [`UnionFind<K, V, A>`]: disjoint sets over typed ids with a value per root, union by rank,
//! path halving, and snapshots that roll back exactly what happened since: what type inference
//! needs (one variable table, probes that undo), in place of the `ena` crate and of each
//! checker's own (crates/blaze-typeck/src/toy.rs keeps a parent, a rank and an undo log by hand;
//! this is the one home for it).
//!
//! # Floor
//!
//! A key is one node: a 4-byte parent, a 1-byte rank and its value, in one vector, so
//! `size_of::<V>() + 8` bytes rounded to `V`'s alignment (12 for a 4-byte value). Union by rank
//! keeps every tree at most `log2 n` deep; path halving makes a run of finds near-constant
//! (inverse Ackermann). A find touches one line per step of the path, usually one or two; a
//! unify touches the two roots. Outside a snapshot nothing is logged; inside one, each write
//! (a parent, a rank, a root's value, a new key) logs one record, so a rollback costs what was
//! done since the snapshot, never the table's size.
//!
//! # Snapshots
//!
//! [`UnionFind::snapshot`] opens one; snapshots nest and close in reverse order of opening,
//! each by [`UnionFind::rollback_to`] (undo everything since, newest first, keys made since
//! included) or [`UnionFind::commit`] (keep it; the outermost commit drops the log). A
//! [`Snapshot`] is not `Copy` and is used up by either, and one closed out of order is a bug
//! and panics naming both depths.
//!
//! # Measured (`examples/union_find.rs`, release, one Mac run, 2026-11-15)
//!
//! ns per unify and find against a naive union-find (no rank, no halving): 20.1 against 66.8; 31.0
//! inside a snapshot that is rolled back.

use core::{alloc::Allocator, any::type_name, marker::PhantomData};

use blaze_int::Int;
use blaze_own::{AllocFailure, Refused, Vec};

use crate::{
    error::{fail, overflow},
    id::Id,
    plant::{self, Plant},
    table::MOST,
};

/// One key: its parent (itself at a root), its rank (an upper bound on its tree's height, read
/// at roots) and its value (read at roots; a non-root's is the one it had as a root, kept for a
/// rollback that makes it a root again).
struct Node<V> {
    parent: u32,
    rank: u8,
    value: V,
}

/// One undoable write.
enum Undo<V> {
    /// A key was made: a rollback removes the newest key.
    NewKey,
    /// `at`'s parent was `was`.
    Parent { at: u32, was: u32 },
    /// `at`'s rank was `was`.
    Rank { at: u32, was: u8 },
    /// `at`'s value was `was`.
    Value { at: u32, was: V },
}

/// An open snapshot (see the module): closed by [`UnionFind::rollback_to`] or
/// [`UnionFind::commit`].
#[must_use = "a snapshot is closed by rollback_to or commit"]
#[derive(Debug)]
pub struct Snapshot {
    log: usize,
    depth: u32,
}

/// Disjoint sets over the ids `K`, a `V` per set, in `A` (see the module).
pub struct UnionFind<K: Id, V, A: Allocator> {
    nodes: Vec<Node<V>, A>,
    log: Vec<Undo<V>, A>,
    /// Snapshots open.
    open: u32,
    _key: PhantomData<fn(K) -> K>,
}

impl<K: Id, V, A: Allocator + Clone> UnionFind<K, V, A> {
    /// An empty table; allocates nothing until a key is made.
    pub fn new_in(alloc: A) -> UnionFind<K, V, A> {
        UnionFind { nodes: Vec::new_in(alloc.clone()), log: Vec::new_in(alloc), open: 0, _key: PhantomData }
    }

    /// An empty table with room for `keys` keys.
    pub fn try_with_capacity_in(keys: usize, alloc: A) -> Result<UnionFind<K, V, A>, AllocFailure> {
        Ok(UnionFind {
            nodes: Vec::try_with_capacity_in(keys, alloc.clone())?,
            log: Vec::new_in(alloc),
            open: 0,
            _key: PhantomData,
        })
    }
}

impl<K: Id, V, A: Allocator> UnionFind<K, V, A> {
    /// How many keys there are.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Snapshots open now.
    pub fn open_snapshots(&self) -> u32 {
        self.open
    }

    /// A new key in a set of its own with `value`; refused with the value back and nothing
    /// changed when there is no room (or no id left).
    pub fn try_new_key(&mut self, value: V) -> Result<K, Refused<V>> {
        let len = self.nodes.len();
        let raw = match len.narrow::<u32>() {
            Ok(raw) if raw < MOST => raw,
            _ => return Err(Refused { value, why: overflow(type_name::<Self>(), len, 1) }),
        };
        if self.open > 0
            && let Err(why) = self.log.try_reserve(1)
        {
            return Err(Refused { value, why });
        }
        self.nodes.try_push(Node { parent: raw, rank: 0, value }).map_err(|r| Refused { value: r.value.value, why: r.why })?;
        if self.open > 0 {
            self.log.push(Undo::NewKey);
        }
        Ok(K::from_u32(raw))
    }

    /// [`UnionFind::try_new_key`], panicking with the refusal.
    pub fn new_key(&mut self, value: V) -> K {
        match self.try_new_key(value) {
            Ok(key) => key,
            Err(refused) => fail(refused.why),
        }
    }

    /// The node of `raw`; a key of another table past this one's end panics naming both.
    #[inline]
    #[track_caller]
    fn node(&self, raw: u32) -> &Node<V> {
        match self.nodes.get(blaze_int::idx(raw)) {
            Some(node) => node,
            None => panic!("blaze_kit: key {raw} is not a key of `{}`, which has {}", type_name::<Self>(), self.nodes.len()),
        }
    }

    /// The root of `key`'s set, with no writes: for a shared borrow.
    #[inline]
    #[track_caller]
    pub fn find_root(&self, key: K) -> K {
        let mut at = key.to_u32();
        loop {
            let parent = self.node(at).parent;
            if parent == at {
                return K::from_u32(at);
            }
            at = parent;
        }
    }

    /// The root of `key`'s set, halving the path on the way (each node on it then points to its
    /// grandparent). Inside a snapshot each halving logs one record, which may grow the log.
    #[inline]
    #[track_caller]
    pub fn find(&mut self, key: K) -> K {
        match self.try_find(key) {
            Ok(root) => root,
            Err(failure) => fail(failure),
        }
    }

    /// [`UnionFind::find`], refused with nothing changed when the log has no room.
    #[track_caller]
    pub fn try_find(&mut self, key: K) -> Result<K, AllocFailure> {
        let start = key.to_u32();
        if self.open > 0 {
            // Room for one record per step first, so the halving below cannot fail half way.
            let mut steps = 0usize;
            let mut at = start;
            loop {
                let parent = self.node(at).parent;
                if parent == at {
                    break;
                }
                steps += 1;
                at = parent;
            }
            self.log.try_reserve(steps)?;
        }
        let logged = self.open > 0 && !plant::on(Plant::HalvingUnlogged);
        let mut at = start;
        loop {
            let parent = self.node(at).parent;
            if parent == at {
                return Ok(K::from_u32(at));
            }
            let grand = self.node(parent).parent;
            if grand != parent {
                if logged {
                    self.log.push(Undo::Parent { at, was: parent });
                }
                self.nodes[blaze_int::idx(at)].parent = grand;
            }
            at = grand;
        }
    }

    /// Whether `a` and `b` are in one set.
    pub fn same(&mut self, a: K, b: K) -> bool {
        self.find(a) == self.find(b)
    }

    /// The value of `key`'s set.
    pub fn value(&mut self, key: K) -> &V {
        let root = self.find(key);
        &self.nodes[root.index()].value
    }

    /// The value of `key`'s set, with no writes.
    pub fn value_of(&self, key: K) -> &V {
        &self.nodes[self.find_root(key).index()].value
    }

    /// Sets the value of `key`'s set; refused with the value back and nothing changed.
    pub fn try_set_value(&mut self, key: K, value: V) -> Result<(), Refused<V>> {
        // The find's halving changes no set, so its writes are no change to undo on a refusal.
        let root = match self.try_find(key) {
            Ok(root) => root.to_u32(),
            Err(why) => return Err(Refused { value, why }),
        };
        if self.open > 0
            && let Err(why) = self.log.try_reserve(1)
        {
            return Err(Refused { value, why });
        }
        let was = core::mem::replace(&mut self.nodes[blaze_int::idx(root)].value, value);
        if self.open > 0 {
            self.log.push(Undo::Value { at: root, was });
        }
        Ok(())
    }

    /// [`UnionFind::try_set_value`], panicking with the refusal.
    pub fn set_value(&mut self, key: K, value: V) {
        if let Err(refused) = self.try_set_value(key, value) {
            fail(refused.why);
        }
    }

    /// Joins the sets of `a` and `b`, their value `merge(a's, b's)`; the new root. When `merge`
    /// refuses, nothing changes and its error is given back. Panics when memory for the log
    /// cannot be had; [`UnionFind::try_unify`] returns that instead.
    pub fn unify<E>(&mut self, a: K, b: K, merge: impl FnOnce(&V, &V) -> Result<V, E>) -> Result<K, E> {
        match self.try_unify(a, b, merge) {
            Ok(done) => done,
            Err(failure) => fail(failure),
        }
    }

    /// [`UnionFind::unify`], with the log's allocation refused as the outer error, nothing
    /// changed then either.
    pub fn try_unify<E>(
        &mut self,
        a: K,
        b: K,
        merge: impl FnOnce(&V, &V) -> Result<V, E>,
    ) -> Result<Result<K, E>, AllocFailure> {
        let (ra, rb) = (self.try_find(a)?.to_u32(), self.try_find(b)?.to_u32());
        if ra == rb {
            return Ok(Ok(K::from_u32(ra)));
        }
        if self.open > 0 {
            // A parent, a rank and a value at most.
            self.log.try_reserve(3)?;
        }
        let merged = match merge(&self.nodes[blaze_int::idx(ra)].value, &self.nodes[blaze_int::idx(rb)].value) {
            Ok(merged) => merged,
            Err(refused) => return Ok(Err(refused)),
        };
        let (rank_a, rank_b) = (self.nodes[blaze_int::idx(ra)].rank, self.nodes[blaze_int::idx(rb)].rank);
        // The lower tree goes under the higher; on a tie `b`'s under `a`'s, whose rank rises.
        let (root, child) = if rank_a < rank_b { (rb, ra) } else { (ra, rb) };
        let logged = self.open > 0;
        if logged {
            self.log.push(Undo::Parent { at: child, was: child });
        }
        self.nodes[blaze_int::idx(child)].parent = root;
        if rank_a == rank_b {
            if logged {
                self.log.push(Undo::Rank { at: root, was: rank_a });
            }
            self.nodes[blaze_int::idx(root)].rank = rank_a.saturating_add(1);
        }
        let was = core::mem::replace(&mut self.nodes[blaze_int::idx(root)].value, merged);
        if logged {
            self.log.push(Undo::Value { at: root, was });
        }
        Ok(Ok(K::from_u32(root)))
    }

    /// Opens a snapshot (see the module).
    pub fn snapshot(&mut self) -> Snapshot {
        self.open += 1;
        Snapshot { log: self.log.len(), depth: self.open }
    }

    #[track_caller]
    fn close(&mut self, snapshot: &Snapshot) {
        if snapshot.depth != self.open {
            panic!(
                "blaze_kit: a snapshot of depth {} closed while {} are open in `{}`: snapshots close newest first",
                snapshot.depth,
                self.open,
                type_name::<Self>()
            );
        }
        self.open -= 1;
    }

    /// Undoes everything since `snapshot` was opened, newest first, and closes it.
    #[track_caller]
    pub fn rollback_to(&mut self, snapshot: Snapshot) {
        self.close(&snapshot);
        while self.log.len() > snapshot.log {
            match self.log.pop() {
                Some(Undo::NewKey) => {
                    self.nodes.pop();
                }
                Some(Undo::Parent { at, was }) => self.nodes[blaze_int::idx(at)].parent = was,
                Some(Undo::Rank { at, was }) => self.nodes[blaze_int::idx(at)].rank = was,
                Some(Undo::Value { at, was }) => self.nodes[blaze_int::idx(at)].value = was,
                None => break,
            }
        }
    }

    /// Keeps everything since `snapshot` and closes it; the outermost commit drops the log.
    #[track_caller]
    pub fn commit(&mut self, snapshot: Snapshot) {
        self.close(&snapshot);
        if self.open == 0 {
            self.log.clear();
        }
    }

    /// Records in the log now (for a cost check: zero outside snapshots).
    pub fn logged(&self) -> usize {
        self.log.len()
    }
}
