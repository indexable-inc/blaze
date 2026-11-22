//! [`BTreeMap`] and [`BTreeSet`]: ordered by key, a B-tree whose nodes keep their keys and their
//! values in two separate arrays (columns), the allocator stored once in the map.
//!
//! # The fan-out
//!
//! `B = 8`: a node holds up to `2B - 1 = 15` entries, an internal node 16 edges, and every node
//! but the root at least `B - 1 = 7`. A node starts with a 4-byte header (its length and its
//! place in its parent) and then its key column, and is aligned to a cache line (128 bytes on
//! Apple's arm64, 64 elsewhere), so header and keys take exactly one 64-byte line for 4-byte
//! keys, 128 bytes for 8-byte keys and 256 for 16-byte keys: a search reads a node's keys from
//! one or two whole lines and touches its value column only for the entry it finds. 15 is the
//! largest count for which that holds with the header; std's 11 leaves a third of the lines
//! empty, and a larger node makes every insert and remove shift more. The values follow the
//! keys, then the parent pointer, then (internal nodes) the edges.
//!
//! # Costs
//!
//! For `n` entries: `get`, `insert` and `remove` are `O(log n)` comparisons (a linear scan of at
//! most 15 keys per level) and `O(B log n)` moves; iteration in order walks the leaves and climbs
//! by parent pointers, `O(1)` amortized per entry; `retain` is `O(n)` plus `O(log n)` per entry
//! removed; `split_off` cuts `O(log n)` nodes and counts the nodes of one side; `append` inserts
//! each entry of the other map. Memory: one node per 7 to 15 entries, no per-entry overhead.
//!
//! # Allocation
//!
//! The allocator needs only [`Allocator`]: no `Clone` bound on the type or on any method but
//! [`BTreeMap::split_off`] (two maps that free into one allocator, `AllocatorClone`) and `Clone`
//! itself. Every insertion first makes every node its splits will need, so a refused
//! allocation ([`BTreeMap::try_insert`], [`VacantEntry::try_insert`]) hands the pair back with
//! the map as it was. Removal never allocates.
//!
//! # Panics in user code
//!
//! Comparisons run before anything moves, and a value's drop runs after the tree is whole again:
//! a panic in `Ord`, `Clone`, `Drop` or a `retain` closure leaves a map that holds every value
//! not yet dropped, and drops each once. A comparison that is not a total order gives wrong
//! answers or a panic, never undefined behavior.

use core::{
    alloc::{Allocator, AllocatorClone, Layout},
    borrow::Borrow,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    iter::FusedIterator,
    marker::PhantomData,
    mem::{self, DropGuard, ManuallyDrop, MaybeUninit},
    ops::{Bound, Index, RangeBounds},
    ptr::{self, NonNull},
};

#[cfg(test)]
use crate::plant::{self, Plant};
use crate::{
    error::{AllocFailure, fail},
    fixed::FixedVec,
    make::CloneIn,
    vec::Refused,
};

/// Half the fan-out: see the module docs for why 8.
pub(crate) const B: usize = 8;
/// The most entries a node holds.
pub(crate) const CAPACITY: usize = 2 * B - 1;
/// The fewest entries a node other than the root holds.
const MIN: usize = B - 1;

/// The alignment of every node: a cache line.
#[cfg(all(target_arch = "aarch64", target_vendor = "apple"))]
const LINE: usize = 128;
#[cfg(not(all(target_arch = "aarch64", target_vendor = "apple")))]
const LINE: usize = 64;

/// More than any tree's height: a tree of height `h` has at least `2 * 8^(h - 1)` leaves of at
/// least 64 bytes each, which passes `isize::MAX` bytes before `h` reaches 20.
const MOST_HEIGHT: usize = 24;
/// The most nodes one insertion or one split makes: one per level and a new root.
const MOST_NODES: usize = MOST_HEIGHT + 1;

const WHAT: &str = "BTreeMap";

/// A leaf, and the first part of every internal node.
///
/// # Invariants
///
/// `keys[..len]` and `vals[..len]` are written, the keys strictly increasing; `parent` is the
/// internal node whose edge `parent_idx` this is, `None` for the root.
#[repr(C)]
struct Leaf<K, V> {
    len: u16,
    parent_idx: u16,
    keys: [MaybeUninit<K>; CAPACITY],
    vals: [MaybeUninit<V>; CAPACITY],
    parent: Option<NonNull<Leaf<K, V>>>,
}

/// An internal node: a leaf's columns and `len + 1` edges, every edge's keys between the keys on
/// either side of it.
#[repr(C)]
struct Internal<K, V> {
    leaf: Leaf<K, V>,
    edges: [MaybeUninit<NonNull<Leaf<K, V>>>; CAPACITY + 1],
}

/// A node of either kind; which one is said by its height (0 for a leaf), kept by whoever
/// holds the pointer.
type Node<K, V> = NonNull<Leaf<K, V>>;

fn node_layout<K, V>(height: usize) -> Layout {
    let plain = if height == 0 { Layout::new::<Leaf<K, V>>() } else { Layout::new::<Internal<K, V>>() };
    match plain.align_to(LINE) {
        Ok(layout) => layout,
        Err(_) => unreachable!("a cache line is a power of two far below isize::MAX"),
    }
}

/// A fresh node with no entries, no parent and (internal) no edges written.
fn new_node<K, V, A: Allocator>(height: usize, alloc: &A) -> Result<Node<K, V>, AllocFailure> {
    let layout = node_layout::<K, V>(height);
    let Ok(block) = alloc.allocate(layout) else { return Err(AllocFailure::Refused { what: WHAT, layout }) };
    let node = block.cast::<Leaf<K, V>>();
    // SAFETY: the block is fresh, sized and aligned for a node of this height (`node_layout`);
    // the header is written before anything reads it.
    unsafe {
        (&raw mut (*node.as_ptr()).len).write(0);
        (&raw mut (*node.as_ptr()).parent_idx).write(0);
        (&raw mut (*node.as_ptr()).parent).write(None);
    }
    Ok(node)
}

// Every function below that takes a `Node` is `unsafe`: the node is live, of the height given
// (or internal, for the edge functions), every index is within its columns, and the slots read
// are written. Each says only what it adds.

/// Frees `node`. The caller has moved or dropped what it held, and nothing reads it after.
unsafe fn free_node<K, V, A: Allocator>(node: Node<K, V>, height: usize, alloc: &A) {
    // SAFETY: the node is `alloc`'s, made by `new_node` with this height's layout.
    unsafe { alloc.deallocate(node.cast(), node_layout::<K, V>(height)) }
}

#[inline]
unsafe fn len<K, V>(node: Node<K, V>) -> usize {
    // SAFETY: the node is live.
    usize::from(unsafe { (*node.as_ptr()).len })
}

#[inline]
unsafe fn set_len<K, V>(node: Node<K, V>, len: usize) {
    debug_assert!(len <= CAPACITY);
    // SAFETY: the node is live; `len <= CAPACITY` fits a `u16`.
    unsafe { (*node.as_ptr()).len = len as u16 };
}

#[inline]
unsafe fn key_at<K, V>(node: Node<K, V>, i: usize) -> *mut K {
    // SAFETY: the node is live and `i` within its key column.
    unsafe { (&raw mut (*node.as_ptr()).keys).cast::<K>().add(i) }
}

#[inline]
unsafe fn val_at<K, V>(node: Node<K, V>, i: usize) -> *mut V {
    // SAFETY: the node is live and `i` within its value column.
    unsafe { (&raw mut (*node.as_ptr()).vals).cast::<V>().add(i) }
}

#[inline]
unsafe fn edges<K, V>(node: Node<K, V>) -> *mut Node<K, V> {
    // SAFETY: the node is internal, so the block is an `Internal`, whose first field is its
    // `Leaf` (`repr(C)`).
    unsafe { (&raw mut (*node.as_ptr().cast::<Internal<K, V>>()).edges).cast::<Node<K, V>>() }
}

#[inline]
unsafe fn edge_at<K, V>(node: Node<K, V>, i: usize) -> Node<K, V> {
    // SAFETY: edge `i <= len` of an internal node is written.
    unsafe { *edges(node).add(i) }
}

/// The node's parent and its edge there.
#[inline]
unsafe fn parent<K, V>(node: Node<K, V>) -> Option<(Node<K, V>, usize)> {
    // SAFETY: the node is live.
    let (up, at) = unsafe { ((*node.as_ptr()).parent, (*node.as_ptr()).parent_idx) };
    up.map(|up| (up, usize::from(at)))
}

/// Makes `node` a root.
#[inline]
unsafe fn orphan<K, V>(node: Node<K, V>) {
    // SAFETY: the node is live.
    unsafe { (*node.as_ptr()).parent = None };
}

/// Writes `child` at edge `i` of `node` and points it back.
#[inline]
unsafe fn set_edge<K, V>(node: Node<K, V>, i: usize, child: Node<K, V>) {
    // SAFETY: the node is internal and `i <= CAPACITY`; the child is live. `i` fits a `u16`.
    unsafe {
        edges(node).add(i).write(child);
        (*child.as_ptr()).parent = Some(node);
        (*child.as_ptr()).parent_idx = i as u16;
    }
}

/// Points edges `from..=to` of `node` back at it, after they moved.
unsafe fn adopt<K, V>(node: Node<K, V>, from: usize, to: usize) {
    for i in from..=to {
        // SAFETY: edges `from..=to` are written.
        unsafe { set_edge(node, i, edge_at(node, i)) };
    }
}

/// Moves `count` entries of `src` from `from` to `dst` at `to`; the ranges may overlap.
unsafe fn move_kv<K, V>(src: Node<K, V>, from: usize, dst: Node<K, V>, to: usize, count: usize) {
    // SAFETY: both ranges are within the columns; the source slots are written and are read
    // once more only after something is written there again.
    unsafe {
        ptr::copy(key_at(src, from), key_at(dst, to), count);
        ptr::copy(val_at(src, from), val_at(dst, to), count);
    }
}

/// Moves `count` edges of `src` from `from` to `dst` at `to`; the ranges may overlap.
unsafe fn move_edges<K, V>(src: Node<K, V>, from: usize, dst: Node<K, V>, to: usize, count: usize) {
    // SAFETY: both nodes are internal and both ranges within `CAPACITY + 1` edges.
    unsafe { ptr::copy(edges(src).add(from), edges(dst).add(to), count) };
}

/// Where `key` is in `node`: `Ok(i)` at entry `i`, `Err(i)` below edge `i`.
unsafe fn search_node<K: Borrow<Q>, V, Q: Ord + ?Sized>(node: Node<K, V>, key: &Q) -> Result<usize, usize> {
    // SAFETY: the node is live.
    let n = unsafe { len(node) };
    for i in 0..n {
        // SAFETY: key `i < len` is written.
        let here: &K = unsafe { &*key_at(node, i) };
        match key.cmp(here.borrow()) {
            Ordering::Greater => {}
            Ordering::Equal => return Ok(i),
            Ordering::Less => return Err(i),
        }
    }
    Err(n)
}

/// How many keys of `node` are below `key`, or (`or_equal`) not above it.
unsafe fn count_below<K: Borrow<Q>, V, Q: Ord + ?Sized>(node: Node<K, V>, key: &Q, or_equal: bool) -> usize {
    // SAFETY: the node is live.
    let n = unsafe { len(node) };
    for i in 0..n {
        // SAFETY: key `i < len` is written.
        let here: &K = unsafe { &*key_at(node, i) };
        match here.borrow().cmp(key) {
            Ordering::Less => {}
            Ordering::Equal if or_equal => {}
            Ordering::Equal | Ordering::Greater => return i,
        }
    }
    n
}

/// The entries in the subtree of `node`.
unsafe fn count_entries<K, V>(node: Node<K, V>, height: usize) -> usize {
    // SAFETY: the node is live, of this height, its edges written.
    unsafe {
        if height == 0 {
            return len(node);
        }
        let mut stack: FixedVec<(Node<K, V>, usize, usize), MOST_NODES> = FixedVec::new();
        let mut total = len(node);
        let _ = stack.push((node, height, 0));
        while let Some((cur, h, edge)) = stack.last_mut() {
            let cur_node = *cur;
            let cur_h = *h;
            let idx = *edge;
            if idx > len(cur_node) {
                stack.pop();
                continue;
            }
            *edge = idx + 1;
            let child = edge_at(cur_node, idx);
            total += len(child);
            if cur_h > 1 {
                let _ = stack.push((child, cur_h - 1, 0));
            }
        }
        total
    }
}

/// An entry: entry `idx` of `node`, at `height`.
struct At<K, V> {
    node: Node<K, V>,
    height: usize,
    idx: usize,
}

impl<K, V> Clone for At<K, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K, V> Copy for At<K, V> {}

/// A place between two entries in order: edge `idx` of a leaf. Each place has exactly one gap,
/// so two gaps are the same place exactly when they are equal.
struct Gap<K, V> {
    leaf: Node<K, V>,
    idx: usize,
}

impl<K, V> Clone for Gap<K, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K, V> Copy for Gap<K, V> {}

impl<K, V> PartialEq for Gap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.leaf == other.leaf && self.idx == other.idx
    }
}

unsafe fn first_gap<K, V>(root: Node<K, V>, height: usize) -> Gap<K, V> {
    let mut node = root;
    for _ in 0..height {
        // SAFETY: the nodes above the leaves are internal.
        node = unsafe { edge_at(node, 0) };
    }
    Gap { leaf: node, idx: 0 }
}

unsafe fn last_gap<K, V>(root: Node<K, V>, height: usize) -> Gap<K, V> {
    let mut node = root;
    for _ in 0..height {
        // SAFETY: the nodes above the leaves are internal.
        node = unsafe { edge_at(node, len(node)) };
    }
    // SAFETY: the leaf is live.
    Gap { leaf: node, idx: unsafe { len(node) } }
}

/// The gap past every key below `key`, or (`or_equal`) not above it.
unsafe fn seek<K: Borrow<Q>, V, Q: Ord + ?Sized>(root: Node<K, V>, height: usize, key: &Q, or_equal: bool) -> Gap<K, V> {
    let (mut node, mut height) = (root, height);
    loop {
        // SAFETY: the node is live; the gap lies below edge `i` of an internal node.
        unsafe {
            let i = count_below(node, key, or_equal);
            if height == 0 {
                return Gap { leaf: node, idx: i };
            }
            node = edge_at(node, i);
        }
        height -= 1;
    }
}

/// `key` found, or the gap where it would go.
unsafe fn search_tree<K: Borrow<Q>, V, Q: Ord + ?Sized>(root: Node<K, V>, height: usize, key: &Q) -> Result<At<K, V>, Gap<K, V>> {
    let (mut node, mut height) = (root, height);
    loop {
        // SAFETY: the node is live; below a leaf there is nothing, above it every node is
        // internal.
        match unsafe { search_node(node, key) } {
            Ok(idx) => return Ok(At { node, height, idx }),
            Err(idx) if height == 0 => return Err(Gap { leaf: node, idx }),
            // SAFETY: as above.
            Err(idx) => node = unsafe { edge_at(node, idx) },
        }
        height -= 1;
    }
}

/// The entry after `gap`. Past the last one only through a comparison that is not a total
/// order (a range whose ends cross), where it stops with a panic.
unsafe fn next_kv<K, V>(gap: Gap<K, V>) -> At<K, V> {
    let (mut node, mut idx, mut height) = (gap.leaf, gap.idx, 0);
    // SAFETY: the nodes climbed are live.
    while idx == unsafe { len(node) } {
        // SAFETY: as above.
        let Some((up, at)) = (unsafe { parent(node) }) else {
            panic!("blaze_own::{WHAT}: an iteration passed the last entry: the keys' `Ord` is not a total order")
        };
        (node, idx, height) = (up, at, height + 1);
    }
    At { node, height, idx }
}

/// The entry before `gap`; as [`next_kv`] past the first.
unsafe fn prev_kv<K, V>(gap: Gap<K, V>) -> At<K, V> {
    let (mut node, mut idx, mut height) = (gap.leaf, gap.idx, 0);
    while idx == 0 {
        // SAFETY: the nodes climbed are live.
        let Some((up, at)) = (unsafe { parent(node) }) else {
            panic!("blaze_own::{WHAT}: an iteration passed the first entry: the keys' `Ord` is not a total order")
        };
        (node, idx, height) = (up, at, height + 1);
    }
    At { node, height, idx: idx - 1 }
}

/// The gap right after the entry `at`.
unsafe fn gap_after<K, V>(at: At<K, V>) -> Gap<K, V> {
    if at.height == 0 {
        return Gap { leaf: at.node, idx: at.idx + 1 };
    }
    // SAFETY: an internal entry has an edge after it, and every node below it to the leaves is
    // internal.
    unsafe {
        let mut node = edge_at(at.node, at.idx + 1);
        for _ in 1..at.height {
            node = edge_at(node, 0);
        }
        Gap { leaf: node, idx: 0 }
    }
}

/// The gap right before the entry `at`.
unsafe fn gap_before<K, V>(at: At<K, V>) -> Gap<K, V> {
    if at.height == 0 {
        return Gap { leaf: at.node, idx: at.idx };
    }
    // SAFETY: as `gap_after`.
    unsafe {
        let mut node = edge_at(at.node, at.idx);
        for _ in 1..at.height {
            node = edge_at(node, len(node));
        }
        Gap { leaf: node, idx: len(node) }
    }
}

/// The entries between two gaps, taken from either end.
struct Span<K, V> {
    front: Gap<K, V>,
    back: Gap<K, V>,
}

impl<K, V> Clone for Span<K, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K, V> Copy for Span<K, V> {}

impl<K, V> Span<K, V> {
    /// Both ends of the tree.
    unsafe fn whole(root: Node<K, V>, height: usize) -> Span<K, V> {
        // SAFETY: the tree is live.
        unsafe { Span { front: first_gap(root, height), back: last_gap(root, height) } }
    }

    unsafe fn next(&mut self) -> Option<At<K, V>> {
        if self.front == self.back {
            return None;
        }
        // SAFETY: an entry lies between the gaps.
        unsafe {
            let at = next_kv(self.front);
            self.front = gap_after(at);
            Some(at)
        }
    }

    unsafe fn next_back(&mut self) -> Option<At<K, V>> {
        if self.front == self.back {
            return None;
        }
        // SAFETY: an entry lies between the gaps.
        unsafe {
            let at = prev_kv(self.back);
            self.back = gap_before(at);
            Some(at)
        }
    }
}

/// The nodes one change makes before it moves anything.
struct Spares<K, V> {
    leaf: Option<Node<K, V>>,
    internals: FixedVec<Node<K, V>, MOST_NODES>,
}

impl<K, V> Spares<K, V> {
    /// A leaf (when `leaf`) and `internals` internal nodes, or none and why.
    fn make<A: Allocator>(leaf: bool, internals: usize, alloc: &A) -> Result<Spares<K, V>, AllocFailure> {
        assert!(internals <= MOST_NODES, "blaze_own::{WHAT}: a tree taller than {MOST_HEIGHT}");
        let mut made = DropGuard::new(Spares { leaf: None, internals: FixedVec::new() }, |made| {
            // SAFETY: the spares are `alloc`'s and hold nothing.
            unsafe { made.free(alloc) }
        });
        if leaf {
            made.leaf = Some(new_node(0, alloc)?);
        }
        for _ in 0..internals {
            let node = new_node(1, alloc)?;
            if made.internals.push(node).is_err() {
                unreachable!("at most {MOST_NODES} internal nodes, checked above");
            }
        }
        Ok(DropGuard::dismiss(made))
    }

    fn leaf(&mut self) -> Node<K, V> {
        match self.leaf.take() {
            Some(leaf) => leaf,
            None => unreachable!("the leaves a change needs were counted before it"),
        }
    }

    fn internal(&mut self) -> Node<K, V> {
        match self.internals.pop() {
            Some(node) => node,
            None => unreachable!("the internal nodes a change needs were counted before it"),
        }
    }

    /// Frees what is left: nothing, after a change that counted right.
    unsafe fn free<A: Allocator>(self, alloc: &A) {
        // SAFETY: each spare is `alloc`'s, of its kind's layout, and holds nothing.
        unsafe {
            if let Some(leaf) = self.leaf {
                free_node(leaf, 0, alloc);
            }
            for &node in self.internals.iter() {
                free_node(node, 1, alloc);
            }
        }
    }
}

/// Puts `(key, value)` in at `idx` of a node with room; for an internal node, `right` becomes
/// the edge after it.
unsafe fn fit<K, V>(node: Node<K, V>, idx: usize, key: K, value: V, right: Option<Node<K, V>>) {
    // SAFETY: the node has room for one more entry and edge; `idx <= len`.
    unsafe {
        let n = len(node);
        move_kv(node, idx, node, idx + 1, n - idx);
        key_at(node, idx).write(key);
        val_at(node, idx).write(value);
        set_len(node, n + 1);
        if let Some(right) = right {
            move_edges(node, idx + 1, node, idx + 2, n - idx);
            set_edge(node, idx + 1, right);
            adopt(node, idx + 2, n + 1);
        }
    }
}

/// Moves the entries after `m` (and their edges) of `node` into the empty `new`, and takes entry
/// `m` out: the median that goes up.
unsafe fn split<K, V>(node: Node<K, V>, new: Node<K, V>, height: usize, m: usize) -> (K, V) {
    // SAFETY: `m < len`; `new` is empty and of `node`'s height.
    unsafe {
        let n = len(node);
        let moved = n - m - 1;
        let median = (key_at(node, m).read(), val_at(node, m).read());
        move_kv(node, m + 1, new, 0, moved);
        if height > 0 {
            move_edges(node, m + 1, new, 0, moved + 1);
            adopt(new, 0, moved);
        }
        set_len(node, m);
        set_len(new, moved);
        median
    }
}

/// Moves one entry from the child at edge `s` of `up` to the child at `s + 1`, through
/// `up`'s entry `s`; the children are of `height`.
unsafe fn rotate_right<K, V>(up: Node<K, V>, s: usize, height: usize) {
    // SAFETY: both children exist; the left has an entry to give, the right room for it.
    unsafe {
        let (left, right) = (edge_at(up, s), edge_at(up, s + 1));
        let (ln, rn) = (len(left), len(right));
        move_kv(right, 0, right, 1, rn);
        key_at(right, 0).write(key_at(up, s).read());
        val_at(right, 0).write(val_at(up, s).read());
        key_at(up, s).write(key_at(left, ln - 1).read());
        val_at(up, s).write(val_at(left, ln - 1).read());
        if height > 0 {
            move_edges(right, 0, right, 1, rn + 1);
            set_edge(right, 0, edge_at(left, ln));
            adopt(right, 1, rn + 1);
        }
        set_len(left, ln - 1);
        set_len(right, rn + 1);
    }
}

/// Moves one entry from the child at edge `s + 1` of `up` to the child at `s`, through `up`'s
/// entry `s`.
unsafe fn rotate_left<K, V>(up: Node<K, V>, s: usize, height: usize) {
    // SAFETY: as `rotate_right`, the other way.
    unsafe {
        let (left, right) = (edge_at(up, s), edge_at(up, s + 1));
        let (ln, rn) = (len(left), len(right));
        key_at(left, ln).write(key_at(up, s).read());
        val_at(left, ln).write(val_at(up, s).read());
        key_at(up, s).write(key_at(right, 0).read());
        val_at(up, s).write(val_at(right, 0).read());
        move_kv(right, 1, right, 0, rn - 1);
        if height > 0 {
            set_edge(left, ln + 1, edge_at(right, 0));
            move_edges(right, 1, right, 0, rn);
            adopt(right, 0, rn - 1);
        }
        set_len(left, ln + 1);
        set_len(right, rn - 1);
    }
}

/// Merges the children at edges `s` and `s + 1` of `up`, with `up`'s entry `s` between them,
/// into the left one, and frees the right; `up` loses an entry and an edge.
unsafe fn merge<K, V, A: Allocator>(up: Node<K, V>, s: usize, height: usize, alloc: &A) {
    // SAFETY: both children exist and `len(left) + 1 + len(right) <= CAPACITY`.
    unsafe {
        let (left, right) = (edge_at(up, s), edge_at(up, s + 1));
        let (ln, rn, un) = (len(left), len(right), len(up));
        key_at(left, ln).write(key_at(up, s).read());
        val_at(left, ln).write(val_at(up, s).read());
        move_kv(up, s + 1, up, s, un - s - 1);
        move_edges(up, s + 2, up, s + 1, un - s - 1);
        adopt(up, s + 1, un - 1);
        set_len(up, un - 1);
        let keep = rn;
        // The planted bug (rule 53): the merge forgets the right child's last entry and its last
        // edge, a whole subtree; the tree keeps its shape, and the entries lost are told to the
        // map so its length stays the count of what it holds.
        #[cfg(test)]
        let keep = if height > 0 && rn > 0 && plant::on(Plant::BTreeMergeForgetsChild) {
            plant::lose(1 + count_entries(edge_at(right, rn), height - 1));
            rn - 1
        } else {
            keep
        };
        move_kv(right, 0, left, ln + 1, keep);
        if height > 0 {
            move_edges(right, 0, left, ln + 1, keep + 1);
            adopt(left, ln + 1, ln + 1 + keep);
        }
        set_len(left, ln + 1 + keep);
        free_node(right, height, alloc);
    }
}

/// Copies the subtree of `src` with `copy`, into nodes of `alloc`: the copy and its entries'
/// count. A failure or a panic in `copy` drops what was copied and frees its nodes.
unsafe fn clone_tree<K, V, K2, V2, B: Allocator>(
    src: Node<K, V>,
    height: usize,
    alloc: &B,
    copy: &mut impl FnMut(&K, &V) -> Result<(K2, V2), AllocFailure>,
) -> Result<(Node<K2, V2>, usize), AllocFailure> {
    enum FrameState<K2, V2> {
        First,
        Child { dst: Node<K2, V2>, made: usize, next_i: usize, pending: (K2, V2) },
    }
    struct Frame<K, V, K2, V2> {
        src: Node<K, V>,
        height: usize,
        state: FrameState<K2, V2>,
    }
    let mut stack = DropGuard::new(
        FixedVec::<Frame<K, V, K2, V2>, MOST_NODES>::new(),
        |mut stack| {
            while let Some(frame) = stack.pop() {
                if let FrameState::Child { dst, made, .. } = frame.state {
                    // SAFETY: the node is `alloc`'s, a root, holding `made` entries below it.
                    drop(unsafe { IntoIter::from_root(Some(dst), frame.height, made, alloc) });
                }
            }
        },
    );
    let mut cur_src = src;
    let mut cur_h = height;
    loop {
        while cur_h > 0 {
            let _ = stack.push(Frame { src: cur_src, height: cur_h, state: FrameState::First });
            // SAFETY: an internal source has edge 0.
            cur_src = unsafe { edge_at(cur_src, 0) };
            cur_h -= 1;
        }
        // SAFETY: the source leaf is live.
        let leaf_n = unsafe { len(cur_src) };
        let leaf = new_node::<K2, V2, B>(0, alloc)?;
        let mut leaf_made = DropGuard::new(0usize, |made| {
            // SAFETY: the leaf is `alloc`'s, a root, holding `made` entries.
            drop(unsafe { IntoIter::from_root(Some(leaf), 0, made, alloc) })
        });
        for i in 0..leaf_n {
            // SAFETY: entry `i < len` of the source is written.
            let (key, value) = unsafe { copy(&*key_at(cur_src, i), &*val_at(cur_src, i))? };
            // SAFETY: slot `i` of the new leaf is free and within it.
            unsafe {
                key_at(leaf, i).write(key);
                val_at(leaf, i).write(value);
                set_len(leaf, i + 1);
            }
            *leaf_made += 1;
        }
        let mut child = leaf;
        let mut child_len = DropGuard::dismiss(leaf_made);
        loop {
            let Some(frame) = stack.pop() else {
                DropGuard::dismiss(stack);
                return Ok((child, child_len));
            };
            let (dst, made, next_i) = match frame.state {
                FrameState::First => {
                    let first_made = DropGuard::new((), |()| {
                        // SAFETY: the copied child is `alloc`'s, a root, holding `child_len` entries.
                        drop(unsafe { IntoIter::from_root(Some(child), frame.height - 1, child_len, alloc) })
                    });
                    let node = new_node::<K2, V2, B>(frame.height, alloc)?;
                    DropGuard::dismiss(first_made);
                    // SAFETY: the node is internal and empty.
                    unsafe { set_edge(node, 0, child) };
                    (node, child_len, 0)
                }
                FrameState::Child { dst, made, next_i, pending: (key, value) } => {
                    // SAFETY: slot `next_i` and edge `next_i + 1` of the node are free and within it.
                    unsafe {
                        key_at(dst, next_i).write(key);
                        val_at(dst, next_i).write(value);
                        set_len(dst, next_i + 1);
                        set_edge(dst, next_i + 1, child);
                    }
                    (dst, made + 1 + child_len, next_i + 1)
                }
            };
            // SAFETY: the source is live.
            let n = unsafe { len(frame.src) };
            if next_i < n {
                let node_guard = DropGuard::new((), |()| {
                    // SAFETY: the node is `alloc`'s, a root, holding `made` entries below it.
                    drop(unsafe { IntoIter::from_root(Some(dst), frame.height, made, alloc) })
                });
                // SAFETY: entry `next_i` and edge `next_i + 1` of the source are written.
                let pending = unsafe { copy(&*key_at(frame.src, next_i), &*val_at(frame.src, next_i))? };
                DropGuard::dismiss(node_guard);
                let _ = stack.push(Frame {
                    src: frame.src,
                    height: frame.height,
                    state: FrameState::Child { dst, made, next_i, pending },
                });
                // SAFETY: edge `next_i + 1` of the source is written.
                cur_src = unsafe { edge_at(frame.src, next_i + 1) };
                cur_h = frame.height - 1;
                break;
            }
            child = dst;
            child_len = made;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The map.

/// A map ordered by key: a B-tree of nodes of up to 15 entries, keys and values in separate
/// columns (see the module docs).
pub struct BTreeMap<K, V, A: Allocator> {
    root: Option<Node<K, V>>,
    /// Edges from the root down to the leaves; 0 for an empty map.
    height: usize,
    len: usize,
    /// Taken by the drop, which hands it to the `IntoIter` that frees the nodes.
    alloc: ManuallyDrop<A>,
    _owns: PhantomData<(K, V)>,
}

// SAFETY: the map owns its keys, values and allocator, reached only through it.
unsafe impl<K: Send, V: Send, A: Allocator + Send> Send for BTreeMap<K, V, A> {}
// SAFETY: `&BTreeMap` gives only `&K`, `&V` and `&A`.
unsafe impl<K: Sync, V: Sync, A: Allocator + Sync> Sync for BTreeMap<K, V, A> {}

impl<K, V, A: Allocator> BTreeMap<K, V, A> {
    /// Empty in `alloc`; nothing is allocated until the first insert.
    pub const fn new_in(alloc: A) -> BTreeMap<K, V, A> {
        BTreeMap { root: None, height: 0, len: 0, alloc: ManuallyDrop::new(alloc), _owns: PhantomData }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn allocator(&self) -> &A {
        &self.alloc
    }

    fn span(&self) -> Option<Span<K, V>> {
        // SAFETY: the tree is live and borrowed with the map.
        self.root.map(|root| unsafe { Span::whole(root, self.height) })
    }

    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter { span: self.span(), left: self.len, _borrow: PhantomData }
    }

    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut { span: self.span(), left: self.len, _borrow: PhantomData }
    }

    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys { inner: self.iter() }
    }

    pub fn values(&self) -> Values<'_, K, V> {
        Values { inner: self.iter() }
    }

    pub fn values_mut(&mut self) -> ValuesMut<'_, K, V> {
        ValuesMut { inner: self.iter_mut() }
    }

    pub fn first_key_value(&self) -> Option<(&K, &V)> {
        let root = self.root?;
        // SAFETY: a non-empty tree's first leaf has an entry 0, borrowed with the map.
        unsafe {
            let gap = first_gap(root, self.height);
            Some((&*key_at(gap.leaf, 0), &*val_at(gap.leaf, 0)))
        }
    }

    pub fn last_key_value(&self) -> Option<(&K, &V)> {
        let root = self.root?;
        // SAFETY: a non-empty tree's last leaf has an entry before its last gap.
        unsafe {
            let gap = last_gap(root, self.height);
            Some((&*key_at(gap.leaf, gap.idx - 1), &*val_at(gap.leaf, gap.idx - 1)))
        }
    }

    /// The first entry, taken out.
    pub fn pop_first(&mut self) -> Option<(K, V)> {
        let root = self.root?;
        // SAFETY: entry 0 of the first leaf is this map's.
        unsafe {
            let gap = first_gap(root, self.height);
            Some(self.remove_at(At { node: gap.leaf, height: 0, idx: 0 }))
        }
    }

    /// The last entry, taken out.
    pub fn pop_last(&mut self) -> Option<(K, V)> {
        let root = self.root?;
        // SAFETY: the entry before the last gap is this map's.
        unsafe {
            let gap = last_gap(root, self.height);
            Some(self.remove_at(At { node: gap.leaf, height: 0, idx: gap.idx - 1 }))
        }
    }

    /// Drops every entry and frees every node; a panicking drop drops the rest.
    pub fn clear(&mut self) {
        let (root, height, len) = (self.root.take(), mem::take(&mut self.height), mem::take(&mut self.len));
        // SAFETY: the nodes were this map's, which no longer holds them; they are `alloc`'s.
        drop(unsafe { IntoIter::from_root(root, height, len, &*self.alloc) });
    }

    /// A copy of every entry, made with `copy`, in a map over `alloc`.
    fn copy_into<K2, V2, B: Allocator>(
        &self,
        alloc: B,
        mut copy: impl FnMut(&K, &V) -> Result<(K2, V2), AllocFailure>,
    ) -> Result<BTreeMap<K2, V2, B>, AllocFailure> {
        let mut out = BTreeMap::new_in(alloc);
        if let Some(root) = self.root {
            // SAFETY: the source tree is live and borrowed for the copy.
            let (copied, len) = unsafe { clone_tree(root, self.height, &*out.alloc, &mut copy)? };
            (out.root, out.height, out.len) = (Some(copied), self.height, len);
        }
        Ok(out)
    }

    /// A deep copy in another allocator, of the same shape: each key and value copied with
    /// [`CloneIn`] into `alloc` too. A refusal drops what was copied.
    pub fn try_clone_in<B: Allocator + Clone>(&self, alloc: B) -> Result<BTreeMap<K::In, V::In, B>, AllocFailure>
    where
        K: CloneIn<B>,
        V: CloneIn<B>,
    {
        let each = alloc.clone();
        self.copy_into(alloc, |k, v| Ok((k.try_clone_in(each.clone())?, v.try_clone_in(each.clone())?)))
    }

    /// [`BTreeMap::try_clone_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn clone_in<B: Allocator + Clone>(&self, alloc: B) -> BTreeMap<K::In, V::In, B>
    where
        K: CloneIn<B>,
        V: CloneIn<B>,
    {
        match self.try_clone_in(alloc) {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }

    /// Takes the entry at `at` out and restores the least fill; runs no user code.
    ///
    /// # Safety
    ///
    /// `at` is an entry of this map.
    unsafe fn remove_at(&mut self, at: At<K, V>) -> (K, V) {
        // SAFETY: `at` is this map's; an internal entry swaps with the one before it, the last
        // of a leaf, which is then taken from the leaf. No comparison runs while the keys are
        // out of order.
        unsafe {
            let (leaf, idx) = if at.height == 0 {
                (at.node, at.idx)
            } else {
                let gap = gap_before(at);
                ptr::swap(key_at(at.node, at.idx), key_at(gap.leaf, gap.idx - 1));
                ptr::swap(val_at(at.node, at.idx), val_at(gap.leaf, gap.idx - 1));
                (gap.leaf, gap.idx - 1)
            };
            let n = len(leaf);
            let entry = (key_at(leaf, idx).read(), val_at(leaf, idx).read());
            move_kv(leaf, idx + 1, leaf, idx, n - idx - 1);
            set_len(leaf, n - 1);
            self.len -= 1;
            self.rebalance(leaf);
            entry
        }
    }

    /// From a leaf that lost an entry up: a node under the least fill borrows from a sibling
    /// that has one to spare, else merges with one, which takes an entry from its parent; the
    /// root goes when it empties.
    unsafe fn rebalance(&mut self, leaf: Node<K, V>) {
        let (mut node, mut height) = (leaf, 0);
        // SAFETY: every node visited is this map's: the leaf, then the parents.
        unsafe {
            loop {
                let n = len(node);
                let Some((up, at)) = parent(node) else {
                    if n == 0 {
                        if height == 0 {
                            free_node(node, 0, &*self.alloc);
                            (self.root, self.height) = (None, 0);
                        } else {
                            let child = edge_at(node, 0);
                            orphan(child);
                            free_node(node, height, &*self.alloc);
                            (self.root, self.height) = (Some(child), self.height - 1);
                        }
                    }
                    return;
                };
                if n >= MIN {
                    return;
                }
                if at > 0 && len(edge_at(up, at - 1)) > MIN {
                    rotate_right(up, at - 1, height);
                    return;
                }
                if at < len(up) && len(edge_at(up, at + 1)) > MIN {
                    rotate_left(up, at, height);
                    return;
                }
                merge(up, if at > 0 { at - 1 } else { at }, height, &*self.alloc);
                #[cfg(test)]
                {
                    self.len -= plant::take_lost();
                }
                (node, height) = (up, height + 1);
            }
        }
    }

    /// Frees roots left with no entries: an internal one gives way to its only child.
    unsafe fn drop_empty_roots(&mut self) {
        // SAFETY: the root is this map's.
        unsafe {
            while let Some(root) = self.root
                && len(root) == 0
            {
                if self.height == 0 {
                    free_node(root, 0, &*self.alloc);
                    (self.root, self.height) = (None, 0);
                } else {
                    let child = edge_at(root, 0);
                    orphan(child);
                    free_node(root, self.height, &*self.alloc);
                    (self.root, self.height) = (Some(child), self.height - 1);
                }
            }
        }
    }

    /// After a cut that left the right border's nodes with any number of entries: from the root
    /// down, each border child takes from or merges with its left sibling until it holds more
    /// than the least fill, so a merge below it leaves it at the least fill or more. A sibling
    /// off the border holds at least the least fill, so a merge (when the two fit one node)
    /// gives more than it, and a theft (when they do not) leaves the sibling at it.
    unsafe fn fix_right_border(&mut self) {
        // SAFETY: the nodes are this map's; the border's nodes but the root are fixed before
        // their children are looked at.
        unsafe {
            self.drop_empty_roots();
            let Some(mut node) = self.root else { return };
            let mut height = self.height;
            while height > 0 {
                let n = len(node);
                let mut child = edge_at(node, n);
                let c = len(child);
                if c <= MIN {
                    let sibling = edge_at(node, n - 1);
                    if len(sibling) + c < CAPACITY {
                        merge(node, n - 1, height - 1, &*self.alloc);
                        child = sibling;
                        if len(node) == 0 {
                            orphan(child);
                            free_node(node, height, &*self.alloc);
                            (self.root, self.height) = (Some(child), self.height - 1);
                        }
                    } else {
                        for _ in c..=MIN {
                            rotate_right(node, n - 1, height - 1);
                        }
                    }
                }
                (node, height) = (child, height - 1);
            }
        }
    }

    /// [`BTreeMap::fix_right_border`], mirrored: the left border, taking from right siblings.
    unsafe fn fix_left_border(&mut self) {
        // SAFETY: as `fix_right_border`.
        unsafe {
            self.drop_empty_roots();
            let Some(mut node) = self.root else { return };
            let mut height = self.height;
            while height > 0 {
                let child = edge_at(node, 0);
                let c = len(child);
                if c <= MIN {
                    let sibling = edge_at(node, 1);
                    if c + len(sibling) < CAPACITY {
                        merge(node, 0, height - 1, &*self.alloc);
                        if len(node) == 0 {
                            orphan(child);
                            free_node(node, height, &*self.alloc);
                            (self.root, self.height) = (Some(child), self.height - 1);
                        }
                    } else {
                        for _ in c..=MIN {
                            rotate_left(node, 0, height - 1);
                        }
                    }
                }
                (node, height) = (child, height - 1);
            }
        }
    }
}

impl<K: Ord, V, A: Allocator> BTreeMap<K, V, A> {
    fn find<Q: Ord + ?Sized>(&self, key: &Q) -> Option<At<K, V>>
    where
        K: Borrow<Q>,
    {
        let root = self.root?;
        // SAFETY: the tree is live.
        unsafe { search_tree(root, self.height, key).ok() }
    }

    pub fn get<Q: Ord + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        let at = self.find(key)?;
        // SAFETY: an entry of this map, borrowed with it.
        Some(unsafe { &*val_at(at.node, at.idx) })
    }

    pub fn get_key_value<Q: Ord + ?Sized>(&self, key: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
    {
        let at = self.find(key)?;
        // SAFETY: as `get`.
        Some(unsafe { (&*key_at(at.node, at.idx), &*val_at(at.node, at.idx)) })
    }

    pub fn get_mut<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        let at = self.find(key)?;
        // SAFETY: an entry of this map, borrowed mutably with it.
        Some(unsafe { &mut *val_at(at.node, at.idx) })
    }

    pub fn contains_key<Q: Ord + ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.find(key).is_some()
    }

    /// The value under `key`; on a miss, `make(key)` gives the owned key and its value, put in
    /// where the one search left off. So the owned key is made only on a miss, where
    /// `map.entry(key.to_owned())` makes it on every hit.
    ///
    /// # Panics
    ///
    /// When `make` gives a key not equal to `key` (it would sit out of order), or there is no
    /// memory.
    #[track_caller]
    pub fn get_or_insert_with<Q: Ord + ?Sized>(&mut self, key: &Q, make: impl FnOnce(&Q) -> (K, V)) -> &mut V
    where
        K: Borrow<Q>,
    {
        match self.try_get_or_insert_with(key, make) {
            Ok(value) => value,
            Err(refused) => fail(refused.why),
        }
    }

    /// [`BTreeMap::get_or_insert_with`], or the made pair handed back when a node could not be
    /// had, with the map as it was.
    ///
    /// # Panics
    ///
    /// When `make` gives a key not equal to `key`.
    #[track_caller]
    pub fn try_get_or_insert_with<Q: Ord + ?Sized>(
        &mut self,
        key: &Q,
        make: impl FnOnce(&Q) -> (K, V),
    ) -> Result<&mut V, Refused<(K, V)>>
    where
        K: Borrow<Q>,
    {
        let gap = match self.root {
            None => None,
            // SAFETY: the tree is live.
            Some(root) => match unsafe { search_tree(root, self.height, key) } {
                // SAFETY: an entry of this map, borrowed mutably with it.
                Ok(at) => return Ok(unsafe { &mut *val_at(at.node, at.idx) }),
                Err(gap) => Some(gap),
            },
        };
        let (owned, value) = make(key);
        if owned.borrow().cmp(key) != Ordering::Equal {
            panic!("BTreeMap::get_or_insert_with: `make` gave a key not equal to the one looked up");
        }
        // The gap is where any key equal to `key` goes, and nothing changed the map since.
        VacantEntry { key: owned, gap, map: self }.try_insert(value)
    }

    /// The entry for `key`, there or not.
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V, A> {
        let Some(root) = self.root else { return Entry::Vacant(VacantEntry { key, gap: None, map: self }) };
        // SAFETY: the tree is live.
        match unsafe { search_tree(root, self.height, &key) } {
            Ok(at) => Entry::Occupied(OccupiedEntry { at, map: self }),
            Err(gap) => Entry::Vacant(VacantEntry { key, gap: Some(gap), map: self }),
        }
    }

    /// Puts `value` under `key`, giving back the value it replaced (the key already there is
    /// kept); or, when a node could not be had, the pair, with the map as it was.
    pub fn try_insert(&mut self, key: K, value: V) -> Result<Option<V>, Refused<(K, V)>> {
        match self.entry(key) {
            Entry::Occupied(mut there) => Ok(Some(there.insert(value))),
            Entry::Vacant(free) => free.try_insert(value).map(|_| None),
        }
    }

    /// [`BTreeMap::try_insert`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        match self.try_insert(key, value) {
            Ok(old) => old,
            Err(refused) => fail(refused.why),
        }
    }

    pub fn remove<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        self.remove_entry(key).map(|(_, value)| value)
    }

    pub fn remove_entry<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
    {
        let at = self.find(key)?;
        // SAFETY: `find` found it in this map.
        Some(unsafe { self.remove_at(at) })
    }

    /// The span of `range`; panics, as std's does, on a range whose start is past its end or
    /// that starts and ends at one excluded key.
    fn span_of<Q: Ord + ?Sized, R: RangeBounds<Q>>(&self, range: &R) -> Option<Span<K, V>>
    where
        K: Borrow<Q>,
    {
        let (start, end) = (range.start_bound(), range.end_bound());
        if let (Bound::Included(s) | Bound::Excluded(s), Bound::Included(e) | Bound::Excluded(e)) = (start, end) {
            match s.cmp(e) {
                Ordering::Greater => panic!("blaze_own::{WHAT}::range: the range starts after it ends"),
                Ordering::Equal if matches!((start, end), (Bound::Excluded(_), Bound::Excluded(_))) => {
                    panic!("blaze_own::{WHAT}::range: the range starts and ends at one key, excluded")
                }
                Ordering::Less | Ordering::Equal => {}
            }
        }
        let root = self.root?;
        // SAFETY: the tree is live; the gaps are found before the span is used.
        unsafe {
            let front = match start {
                Bound::Included(s) => seek(root, self.height, s, false),
                Bound::Excluded(s) => seek(root, self.height, s, true),
                Bound::Unbounded => first_gap(root, self.height),
            };
            let back = match end {
                Bound::Included(e) => seek(root, self.height, e, true),
                Bound::Excluded(e) => seek(root, self.height, e, false),
                Bound::Unbounded => last_gap(root, self.height),
            };
            Some(Span { front, back })
        }
    }

    pub fn range<Q: Ord + ?Sized, R: RangeBounds<Q>>(&self, range: R) -> Range<'_, K, V>
    where
        K: Borrow<Q>,
    {
        Range { span: self.span_of(&range), _borrow: PhantomData }
    }

    pub fn range_mut<Q: Ord + ?Sized, R: RangeBounds<Q>>(&mut self, range: R) -> RangeMut<'_, K, V>
    where
        K: Borrow<Q>,
    {
        RangeMut { span: self.span_of(&range), _borrow: PhantomData }
    }

    /// Keeps the entries `keep` says to, in order. A panic in `keep` or in a dropped value's
    /// drop leaves the map holding the entries not yet taken out.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        let Some(root) = self.root else { return };
        // SAFETY: the tree is live.
        let mut gap = unsafe { first_gap(root, self.height) };
        // Entries after `gap`, each looked at once.
        let mut after = self.len;
        while after > 0 {
            after -= 1;
            // SAFETY: `after + 1` entries follow the gap, in this map.
            let at = unsafe { next_kv(gap) };
            // SAFETY: the entry is live; `keep` borrows it for the call only.
            let (k, v) = unsafe { (&*key_at(at.node, at.idx), &mut *val_at(at.node, at.idx)) };
            if keep(k, v) {
                // SAFETY: as above.
                gap = unsafe { gap_after(at) };
                continue;
            }
            // SAFETY: an entry of this map.
            let (key, value) = unsafe { self.remove_at(at) };
            drop(value);
            let Some(root) = self.root else { return };
            // The rebalance moved entries: the rest follow the gap where the key was.
            // SAFETY: the tree is live.
            gap = unsafe { seek(root, self.height, &key, false) };
        }
    }

    /// Moves every entry of `other` into this map; an entry of `other` replaces the value of one
    /// here with its key. `other` is left empty. Panics, as `insert` does, when a node cannot be
    /// had: the entries not yet moved are dropped.
    pub fn append<B: Allocator>(&mut self, other: &mut BTreeMap<K, V, B>) {
        let (root, height, len) = (other.root.take(), mem::take(&mut other.height), mem::take(&mut other.len));
        // SAFETY: the nodes were `other`'s, which no longer holds them; they are its allocator's.
        let theirs = unsafe { IntoIter::from_root(root, height, len, &*other.alloc) };
        for (key, value) in theirs {
            self.insert(key, value);
        }
    }

    /// The entries from `key` on, moved into a new map over a clone of this map's allocator,
    /// which frees the nodes it is given (`AllocatorClone`). Cuts one node per level and counts
    /// the nodes of the part moved; the nodes the cut makes are made first, so a refusal leaves
    /// the map as it was, and the comparisons run before anything moves.
    pub fn try_split_off<Q: Ord + ?Sized>(&mut self, key: &Q) -> Result<BTreeMap<K, V, A>, AllocFailure>
    where
        K: Borrow<Q>,
        A: AllocatorClone,
    {
        let mut right = BTreeMap::new_in((*self.alloc).clone());
        let Some(root) = self.root else { return Ok(right) };
        // The cut's place on each level, from the root down.
        let mut cuts: FixedVec<usize, MOST_NODES> = FixedVec::new();
        let mut node = root;
        for height in (0..=self.height).rev() {
            // SAFETY: the node is live; above the leaves it is internal.
            let i = unsafe { count_below(node, key, false) };
            if cuts.push(i).is_err() {
                unreachable!("a tree is at most {MOST_HEIGHT} high");
            }
            if height > 0 {
                // SAFETY: as above.
                node = unsafe { edge_at(node, i) };
            }
        }
        let mut spares = Spares::make(true, self.height, &*self.alloc)?;
        // SAFETY: from here on no user code runs. Each level's node keeps the entries before the
        // cut and edge `i`, whose part after the cut is the next level's new node; the new nodes
        // take the rest, so both trees have the old height and their borders are fixed after.
        unsafe {
            let (mut left, mut above, mut top) = (root, None, None);
            for (level, &i) in cuts.iter().enumerate() {
                let height = self.height - level;
                let new = if height == 0 { spares.leaf() } else { spares.internal() };
                let n = len(left);
                move_kv(left, i, new, 0, n - i);
                if height > 0 {
                    move_edges(left, i + 1, new, 1, n - i);
                    adopt(new, 1, n - i);
                }
                set_len(left, i);
                set_len(new, n - i);
                match above {
                    Some(up) => set_edge(up, 0, new),
                    None => top = Some(new),
                }
                above = Some(new);
                if height > 0 {
                    left = edge_at(left, i);
                }
            }
            spares.free(&*self.alloc);
            (right.root, right.height) = (top, self.height);
            self.fix_right_border();
            // A planted merge's lost entries: this side's are taken from its length, the other
            // side's length is counted below.
            #[cfg(test)]
            {
                self.len -= plant::take_lost();
            }
            right.fix_left_border();
            #[cfg(test)]
            {
                plant::take_lost();
            }
            let moved = match right.root {
                Some(root) => count_entries(root, right.height),
                None => 0,
            };
            right.len = moved;
            self.len -= moved;
        }
        Ok(right)
    }

    /// [`BTreeMap::try_split_off`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn split_off<Q: Ord + ?Sized>(&mut self, key: &Q) -> BTreeMap<K, V, A>
    where
        K: Borrow<Q>,
        A: AllocatorClone,
    {
        match self.try_split_off(key) {
            Ok(right) => right,
            Err(failure) => fail(failure),
        }
    }

    /// Puts `(key, value)` in the empty map.
    fn try_insert_first(&mut self, key: K, value: V) -> Result<(Node<K, V>, usize), Refused<(K, V)>> {
        let leaf = match new_node::<K, V, A>(0, &*self.alloc) {
            Ok(leaf) => leaf,
            Err(why) => return Err(Refused { value: (key, value), why }),
        };
        // SAFETY: the leaf is fresh, with room.
        unsafe {
            key_at(leaf, 0).write(key);
            val_at(leaf, 0).write(value);
            set_len(leaf, 1);
        }
        (self.root, self.height, self.len) = (Some(leaf), 0, 1);
        Ok((leaf, 0))
    }

    /// Puts `(key, value)` in at `idx` of `leaf`, splitting full nodes on the way up: where the
    /// entry ended up, or the pair back when the nodes the splits need could not be had (made
    /// before anything moves, so the map is as it was).
    ///
    /// # Safety
    ///
    /// `leaf` is a leaf of this map and `idx` the place of `key` in it.
    unsafe fn insert_at(&mut self, leaf: Node<K, V>, idx: usize, key: K, value: V) -> Result<(Node<K, V>, usize), Refused<(K, V)>> {
        // Count the splits: every full node from the leaf up, and a new root when all are.
        let (mut splits_leaf, mut internals) = (false, 0);
        let (mut node, mut height) = (leaf, 0);
        // SAFETY: the nodes climbed are this map's.
        unsafe {
            while len(node) == CAPACITY {
                if height == 0 {
                    splits_leaf = true;
                } else {
                    internals += 1;
                }
                match parent(node) {
                    Some((up, _)) => (node, height) = (up, height + 1),
                    None => {
                        internals += 1;
                        break;
                    }
                }
            }
        }
        let mut spares = match Spares::make(splits_leaf, internals, &*self.alloc) {
            Ok(spares) => spares,
            Err(why) => return Err(Refused { value: (key, value), why }),
        };
        let (mut node, mut height, mut idx) = (leaf, 0, idx);
        let (mut key, mut value, mut right) = (key, value, None);
        // Where the new entry went, once it is in a node and not the median going up.
        let mut landed: Option<(Node<K, V>, usize)> = None;
        // SAFETY: no user code runs. A full node of 15 and the entry coming in make 16: 7 stay,
        // the 8th goes up and 8 move to the new node. The entry lands in the left half (`idx <
        // 7`), is the median (`idx == 7`), or lands in the right half.
        let at = unsafe {
            loop {
                if len(node) < CAPACITY {
                    fit(node, idx, key, value, right);
                    break landed.unwrap_or((node, idx));
                }
                let new = if height == 0 { spares.leaf() } else { spares.internal() };
                let median = if idx < B - 1 {
                    let median = split(node, new, height, B - 2);
                    fit(node, idx, key, value, right);
                    if landed.is_none() {
                        landed = Some((node, idx));
                    }
                    median
                } else if idx == B - 1 {
                    move_kv(node, B - 1, new, 0, B);
                    if let Some(right) = right {
                        set_edge(new, 0, right);
                        move_edges(node, B, new, 1, B);
                        adopt(new, 1, B);
                    }
                    set_len(node, B - 1);
                    set_len(new, B);
                    (key, value)
                } else {
                    let median = split(node, new, height, B - 1);
                    fit(new, idx - B, key, value, right);
                    if landed.is_none() {
                        landed = Some((new, idx - B));
                    }
                    median
                };
                match parent(node) {
                    Some((up, at)) => {
                        (node, height, idx) = (up, height + 1, at);
                        (key, value) = median;
                        right = Some(new);
                    }
                    None => {
                        let root = spares.internal();
                        set_edge(root, 0, node);
                        fit(root, 0, median.0, median.1, Some(new));
                        (self.root, self.height) = (Some(root), self.height + 1);
                        break landed.unwrap_or((root, 0));
                    }
                }
            }
        };
        // SAFETY: the spares are `alloc`'s and hold nothing (none are left).
        unsafe { spares.free(&*self.alloc) };
        self.len += 1;
        Ok(at)
    }
}

// SAFETY (`may_dangle`): the drop drops the `K`s and `V`s it owns (`PhantomData<(K, V)>` tells
// drop check so) and reads nothing else of them: a map of borrowed keys may be dropped after the
// keys' owner has moved, as std's may.
unsafe impl<#[may_dangle] K, #[may_dangle] V, A: Allocator> Drop for BTreeMap<K, V, A> {
    fn drop(&mut self) {
        // SAFETY: the allocator is taken once, here, and the map is not used after.
        let alloc = unsafe { ManuallyDrop::take(&mut self.alloc) };
        // SAFETY: the nodes are this map's and its allocator's.
        drop(unsafe { IntoIter::from_root(self.root.take(), self.height, self.len, alloc) });
    }
}

impl<K: Clone, V: Clone, A: Allocator + Clone> Clone for BTreeMap<K, V, A> {
    /// Allocates from the collection's own allocator: prefer `clone_in`. Of the same shape.
    fn clone(&self) -> BTreeMap<K, V, A> {
        match self.copy_into((*self.alloc).clone(), |k, v| Ok((k.clone(), v.clone()))) {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug, A: Allocator> fmt::Debug for BTreeMap<K, V, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: PartialEq, V: PartialEq, A: Allocator, B: Allocator> PartialEq<BTreeMap<K, V, B>> for BTreeMap<K, V, A> {
    fn eq(&self, other: &BTreeMap<K, V, B>) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<K: Eq, V: Eq, A: Allocator> Eq for BTreeMap<K, V, A> {}

impl<K: PartialOrd, V: PartialOrd, A: Allocator> PartialOrd for BTreeMap<K, V, A> {
    fn partial_cmp(&self, other: &BTreeMap<K, V, A>) -> Option<Ordering> {
        self.iter().partial_cmp(other.iter())
    }
}

impl<K: Ord, V: Ord, A: Allocator> Ord for BTreeMap<K, V, A> {
    fn cmp(&self, other: &BTreeMap<K, V, A>) -> Ordering {
        self.iter().cmp(other.iter())
    }
}

impl<K: Hash, V: Hash, A: Allocator> Hash for BTreeMap<K, V, A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.len);
        for (k, v) in self.iter() {
            k.hash(state);
            v.hash(state);
        }
    }
}

impl<K: Borrow<Q> + Ord, Q: Ord + ?Sized, V, A: Allocator> Index<&Q> for BTreeMap<K, V, A> {
    type Output = V;

    #[track_caller]
    fn index(&self, key: &Q) -> &V {
        match self.get(key) {
            Some(value) => value,
            None => panic!("blaze_own::{WHAT}: no entry for the key indexed"),
        }
    }
}

impl<K: Ord, V, A: Allocator> Extend<(K, V)> for BTreeMap<K, V, A> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (key, value) in iter {
            self.insert(key, value);
        }
    }
}

impl<'a, K, V, A: Allocator> IntoIterator for &'a BTreeMap<K, V, A> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Iter<'a, K, V> {
        self.iter()
    }
}

impl<'a, K, V, A: Allocator> IntoIterator for &'a mut BTreeMap<K, V, A> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;

    fn into_iter(self) -> IterMut<'a, K, V> {
        self.iter_mut()
    }
}

impl<K, V, A: Allocator> IntoIterator for BTreeMap<K, V, A> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V, A>;

    fn into_iter(self) -> IntoIter<K, V, A> {
        let mut map = ManuallyDrop::new(self);
        // SAFETY: the allocator is taken once and the map is never dropped.
        let alloc = unsafe { ManuallyDrop::take(&mut map.alloc) };
        // SAFETY: the nodes were the map's and are its allocator's.
        unsafe { IntoIter::from_root(map.root, map.height, map.len, alloc) }
    }
}

// ---------------------------------------------------------------------------------------------
// Entries.

/// An entry of a map, there or not.
pub enum Entry<'a, K, V, A: Allocator> {
    Vacant(VacantEntry<'a, K, V, A>),
    Occupied(OccupiedEntry<'a, K, V, A>),
}

/// A key not in the map, and where it would go.
pub struct VacantEntry<'a, K, V, A: Allocator> {
    key: K,
    /// `None` when the map is empty.
    gap: Option<Gap<K, V>>,
    map: &'a mut BTreeMap<K, V, A>,
}

/// An entry in the map.
pub struct OccupiedEntry<'a, K, V, A: Allocator> {
    at: At<K, V>,
    map: &'a mut BTreeMap<K, V, A>,
}

impl<'a, K: Ord, V, A: Allocator> Entry<'a, K, V, A> {
    pub fn key(&self) -> &K {
        match self {
            Entry::Vacant(free) => &free.key,
            Entry::Occupied(there) => there.key(),
        }
    }

    #[track_caller]
    pub fn or_insert(self, value: V) -> &'a mut V {
        match self {
            Entry::Vacant(free) => free.insert(value),
            Entry::Occupied(there) => there.into_mut(),
        }
    }

    #[track_caller]
    pub fn or_insert_with(self, make: impl FnOnce() -> V) -> &'a mut V {
        match self {
            Entry::Vacant(free) => free.insert(make()),
            Entry::Occupied(there) => there.into_mut(),
        }
    }

    #[track_caller]
    pub fn or_insert_with_key(self, make: impl FnOnce(&K) -> V) -> &'a mut V {
        match self {
            Entry::Vacant(free) => {
                let value = make(&free.key);
                free.insert(value)
            }
            Entry::Occupied(there) => there.into_mut(),
        }
    }

    #[track_caller]
    pub fn or_default(self) -> &'a mut V
    where
        V: Default,
    {
        self.or_insert_with(V::default)
    }

    pub fn and_modify(self, change: impl FnOnce(&mut V)) -> Entry<'a, K, V, A> {
        match self {
            Entry::Occupied(mut there) => {
                change(there.get_mut());
                Entry::Occupied(there)
            }
            Entry::Vacant(free) => Entry::Vacant(free),
        }
    }
}

impl<'a, K: Ord, V, A: Allocator> VacantEntry<'a, K, V, A> {
    pub fn key(&self) -> &K {
        &self.key
    }

    pub fn into_key(self) -> K {
        self.key
    }

    /// Puts `value` in under the key: a borrow of it, or the pair back when a node could not be
    /// had, with the map as it was.
    pub fn try_insert(self, value: V) -> Result<&'a mut V, Refused<(K, V)>> {
        let VacantEntry { key, gap, map } = self;
        let (node, idx) = match gap {
            None => map.try_insert_first(key, value)?,
            // SAFETY: the gap was found for the key in this map, unchanged since: the entry
            // borrows it.
            Some(gap) => unsafe { map.insert_at(gap.leaf, gap.idx, key, value)? },
        };
        // SAFETY: the entry just written, in a map borrowed for `'a`.
        Ok(unsafe { &mut *val_at(node, idx) })
    }

    /// [`VacantEntry::try_insert`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn insert(self, value: V) -> &'a mut V {
        match self.try_insert(value) {
            Ok(value) => value,
            Err(refused) => fail(refused.why),
        }
    }
}

impl<'a, K: Ord, V, A: Allocator> OccupiedEntry<'a, K, V, A> {
    pub fn key(&self) -> &K {
        // SAFETY: the entry is the map's, borrowed with it.
        unsafe { &*key_at(self.at.node, self.at.idx) }
    }

    pub fn get(&self) -> &V {
        // SAFETY: as `key`.
        unsafe { &*val_at(self.at.node, self.at.idx) }
    }

    pub fn get_mut(&mut self) -> &mut V {
        // SAFETY: as `key`, borrowed mutably.
        unsafe { &mut *val_at(self.at.node, self.at.idx) }
    }

    pub fn into_mut(self) -> &'a mut V {
        // SAFETY: the map is borrowed mutably for `'a`.
        unsafe { &mut *val_at(self.at.node, self.at.idx) }
    }

    /// Puts `value` in place of the one there, given back.
    pub fn insert(&mut self, value: V) -> V {
        mem::replace(self.get_mut(), value)
    }

    pub fn remove_entry(self) -> (K, V) {
        // SAFETY: the entry is the map's, unchanged since it was found.
        unsafe { self.map.remove_at(self.at) }
    }

    pub fn remove(self) -> V {
        self.remove_entry().1
    }
}

// ---------------------------------------------------------------------------------------------
// Iterators.

/// The entries in order, borrowed; from either end.
pub struct Iter<'a, K, V> {
    span: Option<Span<K, V>>,
    left: usize,
    _borrow: PhantomData<&'a (K, V)>,
}

// SAFETY: it hands out `&K` and `&V` only.
unsafe impl<K: Sync, V: Sync> Send for Iter<'_, K, V> {}
// SAFETY: as above.
unsafe impl<K: Sync, V: Sync> Sync for Iter<'_, K, V> {}

impl<K, V> Clone for Iter<'_, K, V> {
    fn clone(&self) -> Self {
        Iter { span: self.span, left: self.left, _borrow: PhantomData }
    }
}

impl<'a, K: 'a, V: 'a> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<(&'a K, &'a V)> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        // SAFETY: `left + 1` entries are in the span, of a map borrowed for `'a`.
        unsafe {
            let at = self.span.as_mut()?.next()?;
            Some((&*key_at(at.node, at.idx), &*val_at(at.node, at.idx)))
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl<'a, K: 'a, V: 'a> DoubleEndedIterator for Iter<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a K, &'a V)> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        // SAFETY: as `next`.
        unsafe {
            let at = self.span.as_mut()?.next_back()?;
            Some((&*key_at(at.node, at.idx), &*val_at(at.node, at.idx)))
        }
    }
}

impl<'a, K: 'a, V: 'a> ExactSizeIterator for Iter<'a, K, V> {}

// SAFETY: `size_hint` is `left`, the entries left, which each `next` and `next_back` lowers by one.
unsafe impl<K, V> core::iter::TrustedLen for Iter<'_, K, V> {}

impl<'a, K: 'a, V: 'a> FusedIterator for Iter<'a, K, V> {}

/// The entries in order, values borrowed mutably; from either end.
pub struct IterMut<'a, K, V> {
    span: Option<Span<K, V>>,
    left: usize,
    _borrow: PhantomData<(&'a K, &'a mut V)>,
}

// SAFETY: it hands out `&K` and `&mut V`, each entry once.
unsafe impl<K: Sync, V: Send> Send for IterMut<'_, K, V> {}
// SAFETY: `&IterMut` gives nothing.
unsafe impl<K: Sync, V: Sync> Sync for IterMut<'_, K, V> {}

impl<'a, K: 'a, V: 'a> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<(&'a K, &'a mut V)> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        // SAFETY: as `Iter::next`, the map borrowed mutably; each entry is handed out once, the
        // two ends never passing each other.
        unsafe {
            let at = self.span.as_mut()?.next()?;
            Some((&*key_at(at.node, at.idx), &mut *val_at(at.node, at.idx)))
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl<'a, K: 'a, V: 'a> DoubleEndedIterator for IterMut<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a K, &'a mut V)> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        // SAFETY: as `next`.
        unsafe {
            let at = self.span.as_mut()?.next_back()?;
            Some((&*key_at(at.node, at.idx), &mut *val_at(at.node, at.idx)))
        }
    }
}

impl<'a, K: 'a, V: 'a> ExactSizeIterator for IterMut<'a, K, V> {}

// SAFETY: as `Iter`'s.
unsafe impl<K, V> core::iter::TrustedLen for IterMut<'_, K, V> {}

impl<'a, K: 'a, V: 'a> FusedIterator for IterMut<'a, K, V> {}

/// The keys in order.
pub struct Keys<'a, K, V> {
    inner: Iter<'a, K, V>,
}

impl<K, V> Clone for Keys<'_, K, V> {
    fn clone(&self) -> Self {
        Keys { inner: self.inner.clone() }
    }
}

impl<'a, K: 'a, V: 'a> Iterator for Keys<'a, K, V> {
    type Item = &'a K;

    fn next(&mut self) -> Option<&'a K> {
        self.inner.next().map(|(k, _)| k)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'a, K: 'a, V: 'a> DoubleEndedIterator for Keys<'a, K, V> {
    fn next_back(&mut self) -> Option<&'a K> {
        self.inner.next_back().map(|(k, _)| k)
    }
}

impl<'a, K: 'a, V: 'a> ExactSizeIterator for Keys<'a, K, V> {}

// SAFETY: it forwards `Iter`'s exact `size_hint`.
unsafe impl<K, V> core::iter::TrustedLen for Keys<'_, K, V> {}

impl<'a, K: 'a, V: 'a> FusedIterator for Keys<'a, K, V> {}

/// The values in their keys' order.
pub struct Values<'a, K, V> {
    inner: Iter<'a, K, V>,
}

impl<K, V> Clone for Values<'_, K, V> {
    fn clone(&self) -> Self {
        Values { inner: self.inner.clone() }
    }
}

impl<'a, K: 'a, V: 'a> Iterator for Values<'a, K, V> {
    type Item = &'a V;

    fn next(&mut self) -> Option<&'a V> {
        self.inner.next().map(|(_, v)| v)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'a, K: 'a, V: 'a> DoubleEndedIterator for Values<'a, K, V> {
    fn next_back(&mut self) -> Option<&'a V> {
        self.inner.next_back().map(|(_, v)| v)
    }
}

impl<'a, K: 'a, V: 'a> ExactSizeIterator for Values<'a, K, V> {}

// SAFETY: it forwards `Iter`'s exact `size_hint`.
unsafe impl<K, V> core::iter::TrustedLen for Values<'_, K, V> {}

impl<'a, K: 'a, V: 'a> FusedIterator for Values<'a, K, V> {}

/// The values in their keys' order, borrowed mutably.
pub struct ValuesMut<'a, K, V> {
    inner: IterMut<'a, K, V>,
}

impl<'a, K: 'a, V: 'a> Iterator for ValuesMut<'a, K, V> {
    type Item = &'a mut V;

    fn next(&mut self) -> Option<&'a mut V> {
        self.inner.next().map(|(_, v)| v)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'a, K: 'a, V: 'a> DoubleEndedIterator for ValuesMut<'a, K, V> {
    fn next_back(&mut self) -> Option<&'a mut V> {
        self.inner.next_back().map(|(_, v)| v)
    }
}

impl<'a, K: 'a, V: 'a> ExactSizeIterator for ValuesMut<'a, K, V> {}

// SAFETY: it forwards `IterMut`'s exact `size_hint`.
unsafe impl<K, V> core::iter::TrustedLen for ValuesMut<'_, K, V> {}

impl<'a, K: 'a, V: 'a> FusedIterator for ValuesMut<'a, K, V> {}

/// The entries of a range of keys, in order; from either end.
pub struct Range<'a, K, V> {
    span: Option<Span<K, V>>,
    _borrow: PhantomData<&'a (K, V)>,
}

// SAFETY: as `Iter`.
unsafe impl<K: Sync, V: Sync> Send for Range<'_, K, V> {}
// SAFETY: as `Iter`.
unsafe impl<K: Sync, V: Sync> Sync for Range<'_, K, V> {}

impl<K, V> Clone for Range<'_, K, V> {
    fn clone(&self) -> Self {
        Range { span: self.span, _borrow: PhantomData }
    }
}

impl<'a, K: 'a, V: 'a> Iterator for Range<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<(&'a K, &'a V)> {
        // SAFETY: the span's ends are gaps of a map borrowed for `'a`; an entry lies between
        // them when they differ.
        unsafe {
            let at = self.span.as_mut()?.next()?;
            Some((&*key_at(at.node, at.idx), &*val_at(at.node, at.idx)))
        }
    }
}

impl<'a, K: 'a, V: 'a> DoubleEndedIterator for Range<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a K, &'a V)> {
        // SAFETY: as `next`.
        unsafe {
            let at = self.span.as_mut()?.next_back()?;
            Some((&*key_at(at.node, at.idx), &*val_at(at.node, at.idx)))
        }
    }
}

impl<'a, K: 'a, V: 'a> FusedIterator for Range<'a, K, V> {}

/// The entries of a range of keys, values borrowed mutably.
pub struct RangeMut<'a, K, V> {
    span: Option<Span<K, V>>,
    _borrow: PhantomData<(&'a K, &'a mut V)>,
}

// SAFETY: as `IterMut`.
unsafe impl<K: Sync, V: Send> Send for RangeMut<'_, K, V> {}
// SAFETY: as `IterMut`.
unsafe impl<K: Sync, V: Sync> Sync for RangeMut<'_, K, V> {}

impl<'a, K: 'a, V: 'a> Iterator for RangeMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<(&'a K, &'a mut V)> {
        // SAFETY: as `Range::next`, the map borrowed mutably; the ends never pass each other, so
        // each entry is handed out once.
        unsafe {
            let at = self.span.as_mut()?.next()?;
            Some((&*key_at(at.node, at.idx), &mut *val_at(at.node, at.idx)))
        }
    }
}

impl<'a, K: 'a, V: 'a> DoubleEndedIterator for RangeMut<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a K, &'a mut V)> {
        // SAFETY: as `next`.
        unsafe {
            let at = self.span.as_mut()?.next_back()?;
            Some((&*key_at(at.node, at.idx), &mut *val_at(at.node, at.idx)))
        }
    }
}

impl<'a, K: 'a, V: 'a> FusedIterator for RangeMut<'a, K, V> {}

/// The entries in order, owned; from either end. Each node is freed as both ends have left it.
pub struct IntoIter<K, V, A: Allocator> {
    span: Option<Span<K, V>>,
    left: usize,
    alloc: A,
    _owns: PhantomData<(K, V)>,
}

// SAFETY: it owns what it hands out and its allocator.
unsafe impl<K: Send, V: Send, A: Allocator + Send> Send for IntoIter<K, V, A> {}
// SAFETY: `&IntoIter` gives nothing.
unsafe impl<K: Sync, V: Sync, A: Allocator + Sync> Sync for IntoIter<K, V, A> {}

impl<K, V, A: Allocator> IntoIter<K, V, A> {
    /// # Safety
    ///
    /// The caller gives up the tree under `root` (`height`, `len` entries, no parent), whose
    /// nodes are `alloc`'s.
    unsafe fn from_root(root: Option<Node<K, V>>, height: usize, len: usize, alloc: A) -> IntoIter<K, V, A> {
        // SAFETY: the tree is live.
        let span = root.map(|root| unsafe { Span::whole(root, height) });
        IntoIter { span, left: len, alloc, _owns: PhantomData }
    }

    /// The next entry, read out; the nodes the front climbs out of are freed, all their entries
    /// taken (by this end, or by the back before it passed them).
    fn dying_next(&mut self) -> Option<(K, V)> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let span = self.span.as_mut()?;
        // SAFETY: `left + 1` entries are left between the ends. A node the front climbs out of
        // holds none of them and no gap of the back, and is freed once; its parent is read first.
        unsafe {
            let (mut node, mut idx, mut height) = (span.front.leaf, span.front.idx, 0);
            while idx == len(node) {
                let Some((up, at)) = parent(node) else { unreachable!("an entry is left after the front") };
                free_node(node, height, &self.alloc);
                (node, idx, height) = (up, at, height + 1);
            }
            let at = At { node, height, idx };
            let entry = (key_at(node, idx).read(), val_at(node, idx).read());
            span.front = gap_after(at);
            Some(entry)
        }
    }

    /// [`IntoIter::dying_next`] from the back.
    fn dying_next_back(&mut self) -> Option<(K, V)> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let span = self.span.as_mut()?;
        // SAFETY: as `dying_next`, mirrored.
        unsafe {
            let (mut node, mut idx, mut height) = (span.back.leaf, span.back.idx, 0);
            while idx == 0 {
                let Some((up, at)) = parent(node) else { unreachable!("an entry is left before the back") };
                free_node(node, height, &self.alloc);
                (node, idx, height) = (up, at, height + 1);
            }
            let at = At { node, height, idx: idx - 1 };
            let entry = (key_at(node, idx - 1).read(), val_at(node, idx - 1).read());
            span.back = gap_before(at);
            Some(entry)
        }
    }

    /// With every entry taken: frees the front's leaf and its parents, the only nodes left.
    fn free_rest(&mut self) {
        let Some(span) = self.span.take() else { return };
        let (mut node, mut height) = (span.front.leaf, 0);
        // SAFETY: the nodes left are the front's leaf and the nodes above it; none holds an
        // entry and each is freed once, its parent read first.
        unsafe {
            loop {
                let up = parent(node);
                free_node(node, height, &self.alloc);
                match up {
                    Some((up, _)) => (node, height) = (up, height + 1),
                    None => break,
                }
            }
        }
    }
}

impl<K, V, A: Allocator> Iterator for IntoIter<K, V, A> {
    type Item = (K, V);

    fn next(&mut self) -> Option<(K, V)> {
        self.dying_next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl<K, V, A: Allocator> DoubleEndedIterator for IntoIter<K, V, A> {
    fn next_back(&mut self) -> Option<(K, V)> {
        self.dying_next_back()
    }
}

impl<K, V, A: Allocator> ExactSizeIterator for IntoIter<K, V, A> {}

// SAFETY: `size_hint` is `left`, the entries left, which each dying step lowers by one.
unsafe impl<K, V, A: Allocator> core::iter::TrustedLen for IntoIter<K, V, A> {}

impl<K, V, A: Allocator> FusedIterator for IntoIter<K, V, A> {}

// SAFETY (`may_dangle`): as the map's: it drops the `K`s and `V`s it owns and reads nothing else
// of them.
unsafe impl<#[may_dangle] K, #[may_dangle] V, A: Allocator> Drop for IntoIter<K, V, A> {
    fn drop(&mut self) {
        // A panicking drop leaves the guard to drop the rest and free the nodes.
        let mut rest = DropGuard::new(self, |rest: &mut IntoIter<K, V, A>| {
            while let Some(entry) = rest.dying_next() {
                drop(entry);
            }
            rest.free_rest();
        });
        while let Some(entry) = rest.dying_next() {
            drop(entry);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The set.

/// A set ordered by value: a [`BTreeMap`] of the values to `()`, whose value column takes no
/// room.
pub struct BTreeSet<T, A: Allocator> {
    map: BTreeMap<T, (), A>,
}

impl<T, A: Allocator> BTreeSet<T, A> {
    pub const fn new_in(alloc: A) -> BTreeSet<T, A> {
        BTreeSet { map: BTreeMap::new_in(alloc) }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn allocator(&self) -> &A {
        self.map.allocator()
    }

    pub fn iter(&self) -> SetIter<'_, T> {
        SetIter { inner: self.map.keys() }
    }

    pub fn first(&self) -> Option<&T> {
        self.map.first_key_value().map(|(k, ())| k)
    }

    pub fn last(&self) -> Option<&T> {
        self.map.last_key_value().map(|(k, ())| k)
    }

    pub fn pop_first(&mut self) -> Option<T> {
        self.map.pop_first().map(|(k, ())| k)
    }

    pub fn pop_last(&mut self) -> Option<T> {
        self.map.pop_last().map(|(k, ())| k)
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// A deep copy in another allocator, each value copied with [`CloneIn`].
    pub fn try_clone_in<B: Allocator + Clone>(&self, alloc: B) -> Result<BTreeSet<T::In, B>, AllocFailure>
    where
        T: CloneIn<B>,
    {
        let each = alloc.clone();
        let map = self.map.copy_into(alloc, |k, ()| Ok((k.try_clone_in(each.clone())?, ())))?;
        Ok(BTreeSet { map })
    }

    /// [`BTreeSet::try_clone_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn clone_in<B: Allocator + Clone>(&self, alloc: B) -> BTreeSet<T::In, B>
    where
        T: CloneIn<B>,
    {
        match self.try_clone_in(alloc) {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }
}

impl<T: Ord, A: Allocator> BTreeSet<T, A> {
    pub fn contains<Q: Ord + ?Sized>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
    {
        self.map.contains_key(value)
    }

    pub fn get<Q: Ord + ?Sized>(&self, value: &Q) -> Option<&T>
    where
        T: Borrow<Q>,
    {
        self.map.get_key_value(value).map(|(k, ())| k)
    }

    /// Adds `value`: whether it was not there (one already there is kept); or the value back
    /// when a node could not be had, the set as it was.
    pub fn try_insert(&mut self, value: T) -> Result<bool, Refused<T>> {
        match self.map.try_insert(value, ()) {
            Ok(old) => Ok(old.is_none()),
            Err(Refused { value: (value, ()), why }) => Err(Refused { value, why }),
        }
    }

    /// [`BTreeSet::try_insert`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn insert(&mut self, value: T) -> bool {
        self.map.insert(value, ()).is_none()
    }

    pub fn remove<Q: Ord + ?Sized>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
    {
        self.map.remove(value).is_some()
    }

    pub fn take<Q: Ord + ?Sized>(&mut self, value: &Q) -> Option<T>
    where
        T: Borrow<Q>,
    {
        self.map.remove_entry(value).map(|(k, ())| k)
    }

    pub fn range<Q: Ord + ?Sized, R: RangeBounds<Q>>(&self, range: R) -> SetRange<'_, T>
    where
        T: Borrow<Q>,
    {
        SetRange { inner: self.map.range(range) }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.map.retain(|k, ()| keep(k));
    }

    pub fn append<B: Allocator>(&mut self, other: &mut BTreeSet<T, B>) {
        self.map.append(&mut other.map);
    }

    pub fn try_split_off<Q: Ord + ?Sized>(&mut self, value: &Q) -> Result<BTreeSet<T, A>, AllocFailure>
    where
        T: Borrow<Q>,
        A: AllocatorClone,
    {
        Ok(BTreeSet { map: self.map.try_split_off(value)? })
    }

    #[track_caller]
    pub fn split_off<Q: Ord + ?Sized>(&mut self, value: &Q) -> BTreeSet<T, A>
    where
        T: Borrow<Q>,
        A: AllocatorClone,
    {
        BTreeSet { map: self.map.split_off(value) }
    }
}

impl<T: Clone, A: Allocator + Clone> Clone for BTreeSet<T, A> {
    /// Allocates from the collection's own allocator: prefer `clone_in`.
    fn clone(&self) -> BTreeSet<T, A> {
        BTreeSet { map: self.map.clone() }
    }
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for BTreeSet<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<T: PartialEq, A: Allocator, B: Allocator> PartialEq<BTreeSet<T, B>> for BTreeSet<T, A> {
    fn eq(&self, other: &BTreeSet<T, B>) -> bool {
        self.map == other.map
    }
}

impl<T: Eq, A: Allocator> Eq for BTreeSet<T, A> {}

impl<T: PartialOrd, A: Allocator> PartialOrd for BTreeSet<T, A> {
    fn partial_cmp(&self, other: &BTreeSet<T, A>) -> Option<Ordering> {
        self.iter().partial_cmp(other.iter())
    }
}

impl<T: Ord, A: Allocator> Ord for BTreeSet<T, A> {
    fn cmp(&self, other: &BTreeSet<T, A>) -> Ordering {
        self.iter().cmp(other.iter())
    }
}

impl<T: Hash, A: Allocator> Hash for BTreeSet<T, A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.len());
        for value in self.iter() {
            value.hash(state);
        }
    }
}

impl<T: Ord, A: Allocator> Extend<T> for BTreeSet<T, A> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for value in iter {
            self.insert(value);
        }
    }
}

impl<'a, T, A: Allocator> IntoIterator for &'a BTreeSet<T, A> {
    type Item = &'a T;
    type IntoIter = SetIter<'a, T>;

    fn into_iter(self) -> SetIter<'a, T> {
        self.iter()
    }
}

impl<T, A: Allocator> IntoIterator for BTreeSet<T, A> {
    type Item = T;
    type IntoIter = SetIntoIter<T, A>;

    fn into_iter(self) -> SetIntoIter<T, A> {
        SetIntoIter { inner: self.map.into_iter() }
    }
}

/// A set's values in order, borrowed; from either end.
pub struct SetIter<'a, T> {
    inner: Keys<'a, T, ()>,
}

impl<T> Clone for SetIter<'_, T> {
    fn clone(&self) -> Self {
        SetIter { inner: self.inner.clone() }
    }
}

impl<'a, T: 'a> Iterator for SetIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'a, T: 'a> DoubleEndedIterator for SetIter<'a, T> {
    fn next_back(&mut self) -> Option<&'a T> {
        self.inner.next_back()
    }
}

impl<'a, T: 'a> ExactSizeIterator for SetIter<'a, T> {}

// SAFETY: it forwards `Keys`'s exact `size_hint`.
unsafe impl<T> core::iter::TrustedLen for SetIter<'_, T> {}

impl<'a, T: 'a> FusedIterator for SetIter<'a, T> {}

/// A set's values in a range, in order; from either end.
pub struct SetRange<'a, T> {
    inner: Range<'a, T, ()>,
}

impl<T> Clone for SetRange<'_, T> {
    fn clone(&self) -> Self {
        SetRange { inner: self.inner.clone() }
    }
}

impl<'a, T: 'a> Iterator for SetRange<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        self.inner.next().map(|(k, ())| k)
    }
}

impl<'a, T: 'a> DoubleEndedIterator for SetRange<'a, T> {
    fn next_back(&mut self) -> Option<&'a T> {
        self.inner.next_back().map(|(k, ())| k)
    }
}

impl<'a, T: 'a> FusedIterator for SetRange<'a, T> {}

/// A set's values in order, owned; from either end.
pub struct SetIntoIter<T, A: Allocator> {
    inner: IntoIter<T, (), A>,
}

impl<T, A: Allocator> Iterator for SetIntoIter<T, A> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.inner.next().map(|(k, ())| k)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<T, A: Allocator> DoubleEndedIterator for SetIntoIter<T, A> {
    fn next_back(&mut self) -> Option<T> {
        self.inner.next_back().map(|(k, ())| k)
    }
}

impl<T, A: Allocator> ExactSizeIterator for SetIntoIter<T, A> {}

// SAFETY: it forwards `IntoIter`'s exact `size_hint`.
unsafe impl<T, A: Allocator> core::iter::TrustedLen for SetIntoIter<T, A> {}

impl<T, A: Allocator> FusedIterator for SetIntoIter<T, A> {}

// ---------------------------------------------------------------------------------------------
// The shape's invariants, for the tests.

#[cfg(test)]
impl<K: Ord + fmt::Debug, V, A: Allocator> BTreeMap<K, V, A> {
    pub(crate) fn height(&self) -> usize {
        self.height
    }

    /// Every invariant of the tree: leaves at one depth, every node but the root at least the
    /// least fill and the root not empty, keys strictly increasing in order, each child pointing
    /// back at its parent and edge, and the length the count of entries.
    pub(crate) fn invariants(&self) -> Result<(), std::string::String> {
        let Some(root) = self.root else {
            return match (self.len, self.height) {
                (0, 0) => Ok(()),
                (len, height) => Err(std::format!("no root, but a length of {len} and a height of {height}")),
            };
        };
        // SAFETY: the tree is live and borrowed.
        unsafe {
            if parent(root).is_some() {
                return Err(std::string::String::from("the root has a parent"));
            }
            let count = check_node(root, self.height, None, None, true)?;
            if count != self.len {
                return Err(std::format!("{count} entries, but a length of {}", self.len));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
impl<T: Ord + fmt::Debug, A: Allocator> BTreeSet<T, A> {
    /// The map's invariants.
    pub(crate) fn invariants(&self) -> Result<(), std::string::String> {
        self.map.invariants()
    }
}

#[cfg(test)]
unsafe fn check_node<K: Ord + fmt::Debug, V>(
    node: Node<K, V>,
    height: usize,
    low: Option<&K>,
    high: Option<&K>,
    root: bool,
) -> Result<usize, std::string::String> {
    // SAFETY: the node is live, of this height.
    unsafe {
        let n = len(node);
        if root && n == 0 {
            return Err(std::string::String::from("the root is empty"));
        }
        if !root && n < MIN {
            return Err(std::format!("a node at height {height} holds {n}, under {MIN}"));
        }
        let mut before = low;
        for i in 0..n {
            let key = &*key_at(node, i);
            if let Some(before) = before
                && before >= key
            {
                return Err(std::format!("{key:?} at height {height} follows {before:?}"));
            }
            before = Some(key);
        }
        if let (Some(last), Some(high)) = (before, high)
            && last >= high
        {
            return Err(std::format!("{last:?} at height {height} is not below {high:?}"));
        }
        if height == 0 {
            return Ok(n);
        }
        let mut count = n;
        for i in 0..=n {
            let child = edge_at(node, i);
            if parent(child) != Some((node, i)) {
                return Err(std::format!("edge {i} at height {height} points back elsewhere"));
            }
            let low = if i == 0 { low } else { Some(&*key_at(node, i - 1)) };
            let high = if i == n { high } else { Some(&*key_at(node, i)) };
            count += check_node(child, height - 1, low, high, false)?;
        }
        Ok(count)
    }
}
