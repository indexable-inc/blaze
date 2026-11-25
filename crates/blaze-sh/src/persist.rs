//! Persistent, O(1) forkable copy-on-write data structures on `blaze_own::Rc` (`ARc`).
//!
//! - [`PMap`]: Hash Array Mapped Trie (32-way branching via 5-bit hash chunks and `u32` bitmaps)
//!   with deterministic `blaze_own::xxhash64_slice` hashing (fixed seed, rule 25). Cloning /
//!   forking a `PMap` is O(1); updates copy only the O(log_32 N) path touched, via `Rc::make_mut`.
//! - [`PVec`]: Persistent 32-way radix-balanced chunked vector (trie + tail). Cloning / forking
//!   is O(1); push/pop are O(1) amortized; index and CoW set are O(log_32 N).

use core::borrow::Borrow;
use core::hash::{Hash, Hasher};

use blaze_own::{ARc, AVec, Alloc, Rc, Share};

/// 5 bits per trie level -> 32-way branching (`1 << 5`).
const BITS: u32 = 5;
const WIDTH: usize = 1 << BITS;
const MASK: usize = WIDTH - 1;
/// Max HAMT depth before collision leaf (12 levels * 5 bits = 60 bits + 4 remaining bits).
const MAX_SHIFT: u32 = 60;
const HASH_SEED: u64 = 0x517c_c1b7_2722_0a95;

/// Deterministic 64-bit hasher for `PMap` keys (fixed seed, rule 25).
struct FixedHasher {
    state: u64,
}

impl FixedHasher {
    #[inline]
    fn new() -> Self {
        Self { state: HASH_SEED }
    }
}

impl Hasher for FixedHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let h = blaze_own::xxhash64_slice(bytes, self.state);
        self.state = self.state.rotate_left(13) ^ h.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.state
    }
}

#[inline]
fn hash_key<Q: Hash + ?Sized>(key: &Q) -> u64 {
    let mut h = FixedHasher::new();
    key.hash(&mut h);
    h.finish()
}

// =============================================================================
// PMap: Persistent HAMT
// =============================================================================

#[derive(Clone, Debug)]
enum MapNode<'a, K, V> {
    /// Leaf holding a single key-value entry with its full hash.
    Leaf { hash: u64, key: K, val: V },
    /// Collision node when distinct keys share a 64-bit hash.
    Collision { hash: u64, entries: AVec<'a, (K, V)> },
    /// Compressed 32-way internal branch node indexed by popcount of `bitmap`.
    Branch { bitmap: u32, children: AVec<'a, ARc<'a, MapNode<'a, K, V>>> },
}

/// Persistent copy-on-write hash map with O(1) [`PMap::fork`] / [`Clone`].
#[derive(Debug)]
pub struct PMap<'a, K, V> {
    root: Option<ARc<'a, MapNode<'a, K, V>>>,
    len: usize,
    alloc: Alloc<'a>,
}

impl<'a, K, V> Clone for PMap<'a, K, V> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            root: self.root.as_ref().map(Share::share),
            len: self.len,
            alloc: self.alloc,
        }
    }
}

impl<'a, K, V> Share for PMap<'a, K, V> {}

impl<'a, K, V> PMap<'a, K, V> {
    /// Create an empty persistent map in `alloc`.
    #[inline]
    pub fn new(alloc: Alloc<'a>) -> Self {
        Self { root: None, len: 0, alloc }
    }

    /// O(1) structural-sharing fork of this map.
    #[inline]
    pub fn fork(&self) -> Self {
        self.share()
    }

    #[inline]
    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True if both maps point to the exact same root node (O(1) identity check).
    #[inline]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        match (&self.root, &other.root) {
            (None, None) => true,
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl<'a, K: Eq + Hash + Clone, V: Clone> PMap<'a, K, V> {
    /// Look up `key` in O(log_32 N).
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let root = self.root.as_ref()?;
        let hash = hash_key(key);
        let mut node = root.as_ref();
        let mut shift = 0u32;
        loop {
            match node {
                MapNode::Leaf { hash: h, key: k, val } => {
                    return if *h == hash && k.borrow() == key { Some(val) } else { None };
                }
                MapNode::Collision { hash: h, entries } => {
                    if *h != hash {
                        return None;
                    }
                    for (k, v) in entries.iter() {
                        if k.borrow() == key {
                            return Some(v);
                        }
                    }
                    return None;
                }
                MapNode::Branch { bitmap, children } => {
                    let bit = 1u32 << ((hash >> shift) as usize & MASK);
                    if bitmap & bit == 0 {
                        return None;
                    }
                    let idx = (bitmap & (bit - 1)).count_ones() as usize;
                    node = children[idx].as_ref();
                    shift += BITS;
                }
            }
        }
    }

    #[inline]
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get(key).is_some()
    }

    /// Copy-on-write insert (updates `self` in place when uniquely owned, copies only the
    /// touched path when shared). Returns the previous value if `key` already existed.
    pub fn insert(&mut self, key: K, val: V) -> Option<V> {
        let hash = hash_key(&key);
        let alloc = self.alloc;
        match &mut self.root {
            None => {
                self.root = Some(Rc::new_in(MapNode::Leaf { hash, key, val }, alloc));
                self.len = 1;
                None
            }
            Some(root) => {
                let prev = insert_node(root, hash, 0, key, val, alloc);
                if prev.is_none() {
                    self.len += 1;
                }
                prev
            }
        }
    }

    /// Copy-on-write remove. Returns the removed value if present.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let root = self.root.as_mut()?;
        let hash = hash_key(key);
        let (removed, remaining) = remove_node(root, hash, 0, key, self.alloc);
        if removed.is_some() {
            self.len -= 1;
            if let Some(replacement) = remaining {
                match replacement {
                    RemoveState::Empty => self.root = None,
                    RemoveState::Replaced(n) => self.root = Some(n),
                }
            }
        }
        removed
    }

    /// Collect all `(K, V)` pairs into `out` (deterministic by hash/trie traversal order).
    pub fn collect_entries(&self, out: &mut AVec<'a, (K, V)>) {
        if let Some(root) = &self.root {
            collect_map_node(root, out);
        }
    }

    /// Visit every `(&K, &V)` in deterministic hash-trie order.
    pub fn for_each(&self, f: &mut impl FnMut(&K, &V)) {
        if let Some(root) = &self.root {
            visit_map_node(root, f);
        }
    }
}

fn collect_map_node<'a, K: Clone, V: Clone>(node: &MapNode<'a, K, V>, out: &mut AVec<'a, (K, V)>) {
    match node {
        MapNode::Leaf { key, val, .. } => out.push((key.clone(), val.clone())),
        MapNode::Collision { entries, .. } => {
            for (k, v) in entries.iter() {
                out.push((k.clone(), v.clone()));
            }
        }
        MapNode::Branch { children, .. } => {
            for c in children.iter() {
                collect_map_node(c, out);
            }
        }
    }
}

fn visit_map_node<'a, K, V>(node: &MapNode<'a, K, V>, f: &mut impl FnMut(&K, &V)) {
    match node {
        MapNode::Leaf { key, val, .. } => f(key, val),
        MapNode::Collision { entries, .. } => {
            for (k, v) in entries.iter() {
                f(k, v);
            }
        }
        MapNode::Branch { children, .. } => {
            for c in children.iter() {
                visit_map_node(c, f);
            }
        }
    }
}

fn insert_node<'a, K: Eq + Clone, V: Clone>(
    slot: &mut ARc<'a, MapNode<'a, K, V>>,
    hash: u64,
    shift: u32,
    key: K,
    val: V,
    alloc: Alloc<'a>,
) -> Option<V> {
    let node = Rc::make_mut(slot);
    match node {
        MapNode::Leaf { hash: eh, key: ek, val: ev } => {
            if *eh == hash && *ek == key {
                return Some(core::mem::replace(ev, val));
            }
            if *eh == hash || shift >= MAX_SHIFT {
                let mut entries = AVec::with_capacity_in(2, alloc);
                entries.push((ek.clone(), ev.clone()));
                entries.push((key, val));
                *node = MapNode::Collision { hash, entries };
                return None;
            }
            let existing_hash = *eh;
            let existing_leaf = Rc::new_in(
                MapNode::Leaf { hash: existing_hash, key: ek.clone(), val: ev.clone() },
                alloc,
            );
            let new_leaf = Rc::new_in(MapNode::Leaf { hash, key, val }, alloc);
            *node = merge_two_leaves(existing_hash, existing_leaf, hash, new_leaf, shift, alloc);
            None
        }
        MapNode::Collision { hash: eh, entries } => {
            if *eh == hash {
                for (k, v) in entries.iter_mut() {
                    if *k == key {
                        return Some(core::mem::replace(v, val));
                    }
                }
                entries.push((key, val));
                None
            } else {
                let old_hash = *eh;
                let old_node = Rc::new_in(
                    MapNode::Collision { hash: old_hash, entries: entries.clone() },
                    alloc,
                );
                let new_leaf = Rc::new_in(MapNode::Leaf { hash, key, val }, alloc);
                *node = merge_two_leaves(old_hash, old_node, hash, new_leaf, shift, alloc);
                None
            }
        }
        MapNode::Branch { bitmap, children } => {
            let bit = 1u32 << ((hash >> shift) as usize & MASK);
            let idx = (*bitmap & (bit - 1)).count_ones() as usize;
            if *bitmap & bit != 0 {
                insert_node(&mut children[idx], hash, shift + BITS, key, val, alloc)
            } else {
                let leaf = Rc::new_in(MapNode::Leaf { hash, key, val }, alloc);
                children.insert_shift(idx, leaf);
                *bitmap |= bit;
                None
            }
        }
    }
}

fn merge_two_leaves<'a, K: Clone, V: Clone>(
    h1: u64,
    n1: ARc<'a, MapNode<'a, K, V>>,
    h2: u64,
    n2: ARc<'a, MapNode<'a, K, V>>,
    shift: u32,
    alloc: Alloc<'a>,
) -> MapNode<'a, K, V> {
    let i1 = (h1 >> shift) as usize & MASK;
    let i2 = (h2 >> shift) as usize & MASK;
    if i1 != i2 {
        let b1 = 1u32 << i1;
        let b2 = 1u32 << i2;
        let mut children = AVec::with_capacity_in(2, alloc);
        if i1 < i2 {
            children.push(n1);
            children.push(n2);
        } else {
            children.push(n2);
            children.push(n1);
        }
        MapNode::Branch { bitmap: b1 | b2, children }
    } else {
        let sub = merge_two_leaves(h1, n1, h2, n2, shift + BITS, alloc);
        let mut children = AVec::with_capacity_in(1, alloc);
        children.push(Rc::new_in(sub, alloc));
        MapNode::Branch { bitmap: 1u32 << i1, children }
    }
}

enum RemoveState<'a, K, V> {
    Empty,
    Replaced(ARc<'a, MapNode<'a, K, V>>),
}

fn remove_node<'a, K, Q, V>(
    slot: &mut ARc<'a, MapNode<'a, K, V>>,
    hash: u64,
    shift: u32,
    key: &Q,
    alloc: Alloc<'a>,
) -> (Option<V>, Option<RemoveState<'a, K, V>>)
where
    K: Borrow<Q> + Eq + Clone,
    Q: Hash + Eq + ?Sized,
    V: Clone,
{
    match slot.as_ref() {
        MapNode::Leaf { hash: eh, key: ek, val } => {
            if *eh == hash && ek.borrow() == key {
                (Some(val.clone()), Some(RemoveState::Empty))
            } else {
                (None, None)
            }
        }
        MapNode::Collision { hash: eh, entries } => {
            if *eh != hash {
                return (None, None);
            }
            let pos = entries.iter().position(|(k, _)| k.borrow() == key);
            let Some(pos) = pos else {
                return (None, None);
            };
            let node = Rc::make_mut(slot);
            if let MapNode::Collision { hash: ch, entries } = node {
                let (_, v) = entries.remove_shift(pos);
                if entries.is_empty() {
                    return (Some(v), Some(RemoveState::Empty));
                }
                if entries.len() == 1 {
                    let (lk, lv) = entries.pop().unwrap();
                    let leaf = Rc::new_in(MapNode::Leaf { hash: *ch, key: lk, val: lv }, alloc);
                    return (Some(v), Some(RemoveState::Replaced(leaf)));
                }
                (Some(v), None)
            } else {
                (None, None)
            }
        }
        MapNode::Branch { bitmap, .. } => {
            let bit = 1u32 << ((hash >> shift) as usize & MASK);
            if bitmap & bit == 0 {
                return (None, None);
            }
            let idx = (bitmap & (bit - 1)).count_ones() as usize;
            // Quick existence check before calling make_mut on the branch.
            let found = match slot.as_ref() {
                MapNode::Branch { children, .. } => exists_in_node(&children[idx], hash, shift + BITS, key),
                _ => false,
            };
            if !found {
                return (None, None);
            }
            let node = Rc::make_mut(slot);
            if let MapNode::Branch { bitmap, children } = node {
                let (val, state) = remove_node(&mut children[idx], hash, shift + BITS, key, alloc);
                if let Some(st) = state {
                    match st {
                        RemoveState::Empty => {
                            children.remove_shift(idx);
                            *bitmap &= !bit;
                        }
                        RemoveState::Replaced(rep) => {
                            children[idx] = rep;
                        }
                    }
                }
                if children.is_empty() {
                    return (val, Some(RemoveState::Empty));
                }
                if children.len() == 1 {
                    let only = children[0].share();
                    if matches!(only.as_ref(), MapNode::Leaf { .. } | MapNode::Collision { .. }) {
                        return (val, Some(RemoveState::Replaced(only)));
                    }
                }
                (val, None)
            } else {
                (None, None)
            }
        }
    }
}

fn exists_in_node<K, Q, V>(node: &MapNode<'_, K, V>, hash: u64, shift: u32, key: &Q) -> bool
where
    K: Borrow<Q>,
    Q: Eq + ?Sized,
{
    match node {
        MapNode::Leaf { hash: eh, key: ek, .. } => *eh == hash && ek.borrow() == key,
        MapNode::Collision { hash: eh, entries } => {
            *eh == hash && entries.iter().any(|(k, _)| k.borrow() == key)
        }
        MapNode::Branch { bitmap, children } => {
            let bit = 1u32 << ((hash >> shift) as usize & MASK);
            if bitmap & bit == 0 {
                return false;
            }
            let idx = (bitmap & (bit - 1)).count_ones() as usize;
            exists_in_node(&children[idx], hash, shift + BITS, key)
        }
    }
}

impl<'a, K: Eq + Hash + Clone, V: PartialEq + Clone> PartialEq for PMap<'a, K, V> {
    fn eq(&self, other: &Self) -> bool {
        if self.ptr_eq(other) {
            return true;
        }
        if self.len != other.len {
            return false;
        }
        let mut ok = true;
        self.for_each(&mut |k, v| {
            if ok && other.get(k) != Some(v) {
                ok = false;
            }
        });
        ok
    }
}

impl<'a, K: Eq + Hash + Clone, V: Eq + Clone> Eq for PMap<'a, K, V> {}

// =============================================================================
// PVec: Persistent 32-way Radix-Balanced Vector (Clojure-style trie + tail)
// =============================================================================

#[derive(Clone, Debug)]
enum VecNode<'a, T> {
    Internal(AVec<'a, ARc<'a, VecNode<'a, T>>>),
    Leaf(AVec<'a, T>),
}

/// Persistent copy-on-write vector with O(1) [`PVec::fork`] / [`Clone`], O(1) amortized `push`
/// and `pop`, and O(log_32 N) `get` and `set`.
#[derive(Debug)]
pub struct PVec<'a, T> {
    len: usize,
    shift: u32,
    root: Option<ARc<'a, VecNode<'a, T>>>,
    tail: ARc<'a, AVec<'a, T>>,
    alloc: Alloc<'a>,
}

impl<'a, T> Clone for PVec<'a, T> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            len: self.len,
            shift: self.shift,
            root: self.root.as_ref().map(Share::share),
            tail: self.tail.share(),
            alloc: self.alloc,
        }
    }
}

impl<'a, T> Share for PVec<'a, T> {}

impl<'a, T> PVec<'a, T> {
    #[inline]
    pub fn new(alloc: Alloc<'a>) -> Self {
        Self {
            len: 0,
            shift: BITS,
            root: None,
            tail: Rc::new_in(AVec::new_in(alloc), alloc),
            alloc,
        }
    }

    /// O(1) structural-sharing fork of this vector.
    #[inline]
    pub fn fork(&self) -> Self {
        self.share()
    }

    #[inline]
    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn tail_offset(&self) -> usize {
        if self.len < WIDTH {
            0
        } else {
            ((self.len - 1) >> BITS) << BITS
        }
    }

    /// Look up element at `index` in O(log_32 N) (O(1) for elements in the tail).
    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len {
            return None;
        }
        let tail_off = self.tail_offset();
        if index >= tail_off {
            return self.tail.get(index - tail_off);
        }
        let mut node = self.root.as_ref()?.as_ref();
        let mut level = self.shift;
        while level > 0 {
            match node {
                VecNode::Internal(children) => {
                    let sub = (index >> level) & MASK;
                    node = children[sub].as_ref();
                    level -= BITS;
                }
                VecNode::Leaf(_) => return None,
            }
        }
        match node {
            VecNode::Leaf(items) => items.get(index & MASK),
            VecNode::Internal(_) => None,
        }
    }

    #[inline]
    pub fn first(&self) -> Option<&T> {
        self.get(0)
    }

    #[inline]
    pub fn last(&self) -> Option<&T> {
        if self.len == 0 { None } else { self.get(self.len - 1) }
    }

    pub fn iter(&self) -> PVecIter<'_, 'a, T> {
        PVecIter { vec: self, idx: 0 }
    }
}

impl<'a, T: Clone> PVec<'a, T> {
    pub fn from_slice(slice: &[T], alloc: Alloc<'a>) -> Self {
        let mut v = Self::new(alloc);
        for item in slice {
            v.push(item.clone());
        }
        v
    }

    /// Push `val` to the end in O(1) amortized time, copying only shared path nodes.
    pub fn push(&mut self, val: T) {
        let alloc = self.alloc;
        if self.len - self.tail_offset() < WIDTH {
            Rc::make_mut(&mut self.tail).push(val);
            self.len += 1;
            return;
        }
        // Tail is full (WIDTH elements): flush it into the trie.
        let tail_leaf = Rc::new_in(VecNode::Leaf((*self.tail).clone()), alloc);
        let mut new_tail = AVec::with_capacity_in(1, alloc);
        new_tail.push(val);
        self.tail = Rc::new_in(new_tail, alloc);

        if self.root.is_none() {
            // Transition from tail-only (len == WIDTH) to a root branch at shift = BITS.
            let mut children = AVec::with_capacity_in(2, alloc);
            children.push(tail_leaf);
            self.root = Some(Rc::new_in(VecNode::Internal(children), alloc));
            self.shift = BITS;
            self.len += 1;
            return;
        }

        // Check if root overflows: (len >> BITS) > (1 << shift).
        if (self.len >> BITS) > (1usize << self.shift) {
            let old_root = self.root.take().unwrap();
            let path = new_path(self.shift, tail_leaf, alloc);
            let mut children = AVec::with_capacity_in(2, alloc);
            children.push(old_root);
            children.push(path);
            self.root = Some(Rc::new_in(VecNode::Internal(children), alloc));
            self.shift += BITS;
        } else {
            let root = self.root.as_mut().unwrap();
            push_tail(self.shift, root, self.len, tail_leaf, alloc);
        }
        self.len += 1;
    }

    /// Copy-on-write update at `index`. Returns `Some(old)` if in bounds.
    pub fn set(&mut self, index: usize, val: T) -> Option<T> {
        if index >= self.len {
            return None;
        }
        let tail_off = self.tail_offset();
        if index >= tail_off {
            let tail = Rc::make_mut(&mut self.tail);
            return Some(core::mem::replace(&mut tail[index - tail_off], val));
        }
        let root = self.root.as_mut()?;
        Some(set_in_trie(self.shift, root, index, val))
    }

    /// Pop the last element in O(1) amortized time.
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        if self.len == 1 {
            let item = Rc::make_mut(&mut self.tail).pop();
            self.len = 0;
            self.root = None;
            self.shift = BITS;
            return item;
        }
        if self.len - self.tail_offset() > 1 {
            let item = Rc::make_mut(&mut self.tail).pop();
            self.len -= 1;
            return item;
        }
        // Tail has exactly 1 element; pop it and pull the rightmost leaf from trie as new tail.
        let popped = Rc::make_mut(&mut self.tail).pop();
        let new_tail_items = match self.root.as_ref() {
            Some(r) => rightmost_leaf(r, self.shift).clone(),
            None => AVec::new_in(self.alloc),
        };
        self.tail = Rc::new_in(new_tail_items, self.alloc);
        let new_root = if let Some(mut r) = self.root.take() {
            pop_tail(self.shift, &mut r, self.len - 2)
        } else {
            None
        };
        self.len -= 1;
        match new_root {
            None => {
                self.root = None;
                self.shift = BITS;
            }
            Some(r) => {
                if self.shift > BITS
                    && let VecNode::Internal(children) = r.as_ref()
                    && children.len() == 1
                {
                    self.root = Some(children[0].share());
                    self.shift -= BITS;
                } else {
                    self.root = Some(r);
                }
            }
        }
        popped
    }

    pub fn to_avec(&self) -> AVec<'a, T> {
        let mut out = AVec::with_capacity_in(self.len, self.alloc);
        for i in 0..self.len {
            if let Some(v) = self.get(i) {
                out.push(v.clone());
            }
        }
        out
    }
}

fn new_path<'a, T>(level: u32, node: ARc<'a, VecNode<'a, T>>, alloc: Alloc<'a>) -> ARc<'a, VecNode<'a, T>> {
    if level == 0 {
        node
    } else {
        let child = new_path(level - BITS, node, alloc);
        let mut children = AVec::with_capacity_in(1, alloc);
        children.push(child);
        Rc::new_in(VecNode::Internal(children), alloc)
    }
}

fn push_tail<'a, T: Clone>(
    level: u32,
    parent: &mut ARc<'a, VecNode<'a, T>>,
    len: usize,
    tail: ARc<'a, VecNode<'a, T>>,
    alloc: Alloc<'a>,
) {
    let sub = ((len - 1) >> level) & MASK;
    let VecNode::Internal(children) = Rc::make_mut(parent) else {
        return;
    };
    if level == BITS {
        if sub < children.len() {
            children[sub] = tail;
        } else {
            children.push(tail);
        }
    } else if sub < children.len() {
        push_tail(level - BITS, &mut children[sub], len, tail, alloc);
    } else {
        children.push(new_path(level - BITS, tail, alloc));
    }
}

fn set_in_trie<'a, T: Clone>(
    level: u32,
    node: &mut ARc<'a, VecNode<'a, T>>,
    index: usize,
    val: T,
) -> T {
    match Rc::make_mut(node) {
        VecNode::Internal(children) => {
            let sub = (index >> level) & MASK;
            set_in_trie(level - BITS, &mut children[sub], index, val)
        }
        VecNode::Leaf(items) => core::mem::replace(&mut items[index & MASK], val),
    }
}

fn rightmost_leaf<'a, 'b, T>(node: &'b VecNode<'a, T>, level: u32) -> &'b AVec<'a, T> {
    let mut cur = node;
    let mut lvl = level;
    while lvl > 0 {
        match cur {
            VecNode::Internal(children) => {
                cur = children[children.len() - 1].as_ref();
                lvl -= BITS;
            }
            VecNode::Leaf(items) => return items,
        }
    }
    match cur {
        VecNode::Leaf(items) => items,
        VecNode::Internal(_) => unreachable!("leaf expected at level 0"),
    }
}

fn pop_tail<'a, T: Clone>(
    level: u32,
    parent: &mut ARc<'a, VecNode<'a, T>>,
    index: usize,
) -> Option<ARc<'a, VecNode<'a, T>>> {
    let sub = (index >> level) & MASK;
    if level > BITS {
        let child_new = {
            let VecNode::Internal(children) = Rc::make_mut(parent) else {
                return None;
            };
            pop_tail(level - BITS, &mut children[sub], index)
        };
        let VecNode::Internal(children) = Rc::make_mut(parent) else {
            return None;
        };
        match child_new {
            None => {
                children.pop();
                if children.is_empty() { None } else { Some(parent.share()) }
            }
            Some(c) => {
                children[sub] = c;
                Some(parent.share())
            }
        }
    } else {
        let VecNode::Internal(children) = Rc::make_mut(parent) else {
            return None;
        };
        children.pop();
        if children.is_empty() { None } else { Some(parent.share()) }
    }
}

pub struct PVecIter<'b, 'a, T> {
    vec: &'b PVec<'a, T>,
    idx: usize,
}

impl<'b, 'a, T> Iterator for PVecIter<'b, 'a, T> {
    type Item = &'b T;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.vec.get(self.idx)?;
        self.idx += 1;
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.vec.len.saturating_sub(self.idx);
        (rem, Some(rem))
    }
}

impl<'b, 'a, T> ExactSizeIterator for PVecIter<'b, 'a, T> {}

impl<'a, T: PartialEq> PartialEq for PVec<'a, T> {
    fn eq(&self, other: &Self) -> bool {
        if self.len != other.len {
            return false;
        }
        for i in 0..self.len {
            if self.get(i) != other.get(i) {
                return false;
            }
        }
        true
    }
}

impl<'a, T: Eq> Eq for PVec<'a, T> {}
