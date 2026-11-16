//! [`StaticSearch<K, A>`]: a sorted set built once and searched many times (a symbol table's
//! sorted hashes, a line table, a sorted id list a lookup probes), laid out as an implicit
//! static B-tree of 16 keys a node: Slotin's S-tree ("Static B-Trees", Algorithmica for
//! Programmers, 2021), after Khuong and Morin, "Array layouts for comparison-based searching"
//! (2017).
//!
//! # Why a 16-key B-tree and not Eytzinger order
//!
//! Both put the keys a search reads near each other. Eytzinger order (a binary heap's layout)
//! still reads `log2 n` keys, one line each once past the top levels, and hides the latency by
//! prefetching four levels ahead, spending memory bandwidth on lines it does not use. A node of
//! 16 `u32` keys is one 64-byte line, and a search reads `log17 n` of them: 6 lines for ten
//! million keys where Eytzinger and `binary_search` touch 23 keys, about 20 of them on lines
//! of their own past the cache. Each node is decided by counting its keys below the probe, a
//! branch-free compare of 16 lanes. Khuong and Morin measure the B-tree layout fastest past
//! the cache, and Slotin's S-tree is several times `binary_search` there.
//!
//! # Layout
//!
//! Node `k`'s children are `k * 17 + i + 1` for `i` in `0..=16`; the keys are placed in order
//! by an in-order walk of that tree, and the last node is padded with `K::MAX` (after every real
//! key in order, so a lower bound never lands on padding while a real key would do). Each slot
//! also keeps its key's rank (its position in the sorted input).
//!
//! # Floor
//!
//! `size_of::<K>()` bytes a key in the nodes plus 4 for its rank, the last node padded: 8 bytes
//! a `u32` key, twice a sorted slice (the rank is what makes [`StaticSearch::lower_bound`] an
//! index; a set that needs only [`StaticSearch::contains`] reads none of it). A search reads
//! `ceil(log17 (n / 16 + 1))` nodes, one line each for 4-byte keys and two for 8-byte keys, and
//! one rank. Building is one in-order pass, writing each slot once.

use core::{alloc::Allocator, fmt};

use blaze_int::Int;
use blaze_own::{AllocFailure, Vec};

use crate::{
    error::overflow,
    plant::{self, Plant},
    table::MOST,
};

/// Keys a node.
pub const B: usize = 16;

/// A key a [`StaticSearch`] holds: ordered, with a greatest value to pad with.
pub trait SearchKey: Copy + Ord {
    const MAX: Self;
}

macro_rules! search_keys {
    ($($t:ty),*) => { $( impl SearchKey for $t { const MAX: Self = <$t>::MAX; } )* };
}

search_keys!(u16, u32, u64, i32, i64);

/// One node: 16 keys, aligned so a node of 4-byte keys is one line.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Node<K>([K; B]);

/// Why a set could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// `keys[at]` is not greater than the key before it: keys are strictly increasing.
    Unsorted { at: usize },
    /// Room could not be had.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for SearchError {
    fn from(failure: AllocFailure) -> SearchError {
        SearchError::Alloc(failure)
    }
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchError::Unsorted { at } => write!(f, "blaze_kit::StaticSearch: key {at} is not above the one before it"),
            SearchError::Alloc(failure) => write!(f, "blaze_kit::StaticSearch: {failure}"),
        }
    }
}

/// A sorted set of `K` searched by an implicit B-tree, in `A` (see the module).
pub struct StaticSearch<K: SearchKey, A: Allocator> {
    nodes: Vec<Node<K>, A>,
    /// The rank of each slot's key, `len` for padding.
    ranks: Vec<u32, A>,
    len: u32,
}

impl<K: SearchKey, A: Allocator + Clone> StaticSearch<K, A> {
    /// The set of `keys`, which must be strictly increasing; refused with nothing made.
    pub fn try_new_in(keys: &[K], alloc: A) -> Result<StaticSearch<K, A>, SearchError> {
        if let Some(at) = keys.windows(2).position(|pair| pair[0] >= pair[1]) {
            return Err(SearchError::Unsorted { at: at + 1 });
        }
        let len = match keys.len().narrow::<u32>() {
            Ok(len) if len < MOST => len,
            _ => return Err(SearchError::Alloc(overflow("blaze_kit::StaticSearch", keys.len(), 0))),
        };
        let blocks = keys.len().div_ceil(B);
        let mut nodes = Vec::try_with_capacity_in(blocks, alloc.clone())?;
        nodes.resize(blocks, Node([K::MAX; B]));
        let mut ranks = Vec::try_with_capacity_in(blocks * B, alloc)?;
        ranks.resize(blocks * B, len);
        let mut next = 0usize;
        fill(0, &mut next, keys, &mut nodes, &mut ranks);
        Ok(StaticSearch { nodes, ranks, len })
    }
}

/// The in-order walk that places the keys: node `k`'s subtrees and keys, left to right.
fn fill<K: SearchKey>(k: usize, next: &mut usize, keys: &[K], nodes: &mut [Node<K>], ranks: &mut [u32]) {
    if k >= nodes.len() {
        return;
    }
    for i in 0..B {
        fill(k * (B + 1) + i + 1, next, keys, nodes, ranks);
        if let Some(&key) = keys.get(*next) {
            nodes[k].0[i] = key;
            // `next < len < MOST`, checked when made.
            ranks[k * B + i] = next.truncate_to::<u32>();
        }
        *next += 1;
    }
    fill(k * (B + 1) + B + 1, next, keys, nodes, ranks);
}

impl<K: SearchKey, A: Allocator> StaticSearch<K, A> {
    /// Keys in.
    pub fn len(&self) -> usize {
        blaze_int::idx(self.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The least key at or above `x` and its slot, by one descent.
    #[inline]
    fn descend(&self, x: K) -> Option<(K, usize)> {
        let mut found = None;
        let mut k = 0usize;
        let left = plant::on(Plant::SearchChildLeft);
        while let Some(node) = self.nodes.get(k) {
            let i: usize = node.0.iter().map(|&key| usize::from(key < x)).sum();
            if i < B {
                found = Some((node.0[i], k * B + i));
            }
            k = k * (B + 1) + i + if left && i > 0 { 0 } else { 1 };
        }
        // Padding is never the least real key at or above `x`, but is found when none is.
        found.filter(|&(_, slot)| self.ranks[slot] < self.len)
    }

    /// The rank of the least key at or above `x`: its index in the sorted keys, [`len`] when
    /// every key is below `x` (`partition_point(|k| k < x)` of the sorted keys).
    ///
    /// [`len`]: StaticSearch::len
    #[inline]
    pub fn lower_bound(&self, x: K) -> usize {
        match self.descend(x) {
            Some((_, slot)) => blaze_int::idx(self.ranks[slot]),
            None => self.len(),
        }
    }

    /// Whether `x` is in; reads no rank.
    #[inline]
    pub fn contains(&self, x: K) -> bool {
        let mut k = 0usize;
        let left = plant::on(Plant::SearchChildLeft);
        let mut hit = false;
        while let Some(node) = self.nodes.get(k) {
            let i: usize = node.0.iter().map(|&key| usize::from(key < x)).sum();
            hit |= i < B && node.0[i] == x;
            k = k * (B + 1) + i + if left && i > 0 { 0 } else { 1 };
        }
        // `K::MAX` matches the padding too: it is in only when it is a real key.
        hit && (x != K::MAX || self.descend(x).is_some())
    }

    /// The rank of `x` when it is in.
    #[inline]
    pub fn rank(&self, x: K) -> Option<usize> {
        match self.descend(x) {
            Some((key, slot)) if key == x => Some(blaze_int::idx(self.ranks[slot])),
            _ => None,
        }
    }

    /// The bytes it holds (for a cost check).
    pub fn bytes(&self) -> usize {
        self.nodes.len() * size_of::<Node<K>>() + self.ranks.len() * 4
    }
}
