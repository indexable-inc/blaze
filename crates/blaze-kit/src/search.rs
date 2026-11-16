//! [`StaticSearch<K, A>`]: a sorted set built once and searched many times (a symbol table's
//! sorted hashes, a line table, a sorted id list a lookup probes), laid out as an implicit
//! static B+ tree of 16 keys a node: Slotin's S+ tree ("Static B-Trees", Algorithmica for
//! Programmers, 2021), after Khuong and Morin, "Array layouts for comparison-based searching"
//! (2017). The tree is searched at every size: it beat `partition_point` at every size
//! measured, 1,000 keys included (see below).
//!
//! # Why a 16-key B+ tree and not Eytzinger order
//!
//! Both put the keys a search reads near each other. Eytzinger order (a binary heap's layout)
//! still reads `log2 n` keys, one line each once past the top levels, and hides the latency by
//! prefetching four levels ahead, spending memory bandwidth on lines it does not use. A node of
//! 16 `u32` keys is one 64-byte line, and a search reads `log17 n` of them: 6 lines for ten
//! million keys where Eytzinger and `binary_search` touch 23 keys, about 20 of them on lines
//! of their own past the cache. Each node is decided by counting its keys below the probe, a
//! branch-free compare of 16 lanes. The B+ form keeps every key once, in sorted order, in the
//! leaves, so a leaf's position is the key's rank and no rank is stored.
//!
//! # Layout
//!
//! The leaves are the sorted keys in blocks of 16, the last padded with `K::MAX` (after every
//! real key in order, so a lower bound never lands on padding while a real key would do; the
//! rank is capped at the length). Above them, layers of nodes of 17 children: node `k` of a
//! layer has children `17k ..= 17k + 16` in the layer below, and its key `j` is the least key
//! under child `17k + j + 1` (`K::MAX` where that child is absent). All layers are in one
//! vector, the top first, so a descent reads forward. A search counts the keys of each node
//! below the probe, which is the child to take, and at the leaf adds the leaf's count to its
//! start: the rank.
//!
//! # Floor
//!
//! `size_of::<K>()` bytes a key in the leaves (the sorted array itself, padded to 16) plus the
//! inner layers, a 17th of that again: about 1.07 times a sorted slice. A tree search reads
//! `ceil(log17 (n / 16)) + 1` nodes, one line each for 4-byte keys and two for 8-byte keys.
//! Building writes each leaf slot once and each inner key once.
//!
//! # Measured (`examples/static_search.rs`, release, one Mac run, 2026-11-15)
//!
//! Before (an S-tree, B-tree order with a rank stored beside each key): 30.3 ns a lower bound at
//! 10,000 keys against `partition_point`'s 11.3, 94.7 against 144.3 at 4M, 159.9 against 368.4
//! at 64M; twice a sorted slice's bytes at every size. Causes: the stored ranks (4 bytes a key,
//! an extra dependent load a search) and, at 10,000 keys, where every line is in cache already,
//! a fixed cost of 16 compares a level that binary search's 14 compares of one key do not pay
//! (read from the code, not profiled). Fix: the B+ form, whose leaves are the sorted keys (no
//! ranks).
//!
//! After (2026-11-15, one run, this machine), ns a lower bound, tree against
//! `partition_point`: 1,000 keys 2.44 against 3.96; 10,000 3.41 against 6.20; 29,999 3.51
//! against 7.20; 99,999 5.04 against 9.50; 299,990 5.47 against 13.01; 999,873 6.21 against
//! 18.66; 3,998,133 14.33 against 56.93; 63,525,980 43.97 against 209.35. 1.6 to 4.8 times
//! faster at every size, in about 1.07 times the sorted slice's bytes. A binary search over
//! the leaves was tried below a threshold and was slower than `partition_point` at every size
//! (9.39 at 1,000 keys, 322.08 at 63.5M; not profiled), so it was
//! removed: the tree is the one path.

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

/// Children of an inner node.
const FAN: usize = B + 1;

/// Inner layers at most: `17^7` leaf blocks of 16 exceed `2^32` keys.
const LAYERS: usize = 8;

/// A key a [`StaticSearch`] holds: ordered, with a greatest value to pad with. Keys of 4 and 8
/// bytes, so a node is one or two whole lines with no padding.
pub trait SearchKey: Copy + Ord {
    const MAX: Self;
}

macro_rules! search_keys {
    ($($t:ty),*) => { $( impl SearchKey for $t { const MAX: Self = <$t>::MAX; } )* };
}

search_keys!(u32, u64, i32, i64);

/// One node: 16 keys, aligned so a node of 4-byte keys is one line.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Node<K>([K; B]);

impl<K: SearchKey> Node<K> {
    /// Keys below `x`: the child to take, or the place in a leaf.
    #[inline(always)]
    fn below(&self, x: K) -> usize {
        self.0.iter().map(|&key| usize::from(key < x)).sum()
    }
}

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

/// A sorted set of `K` searched by an implicit B+ tree, in `A` (see the module).
pub struct StaticSearch<K: SearchKey, A: Allocator> {
    /// The inner layers, top first, then the leaves.
    nodes: Vec<Node<K>, A>,
    /// Where each inner layer starts, top first.
    starts: [usize; LAYERS],
    /// Inner layers.
    depth: usize,
    /// Where the leaves start.
    leaves: usize,
    len: usize,
}

impl<K: SearchKey, A: Allocator> StaticSearch<K, A> {
    /// The set of `keys`, which must be strictly increasing; refused with nothing made.
    pub fn try_new_in(keys: &[K], alloc: A) -> Result<StaticSearch<K, A>, SearchError> {
        if let Some(at) = keys.windows(2).position(|pair| pair[0] >= pair[1]) {
            return Err(SearchError::Unsorted { at: at + 1 });
        }
        if !matches!(keys.len().narrow::<u32>(), Ok(len) if len < MOST) {
            return Err(SearchError::Alloc(overflow("blaze_kit::StaticSearch", keys.len(), 0)));
        }
        let blocks = keys.len().div_ceil(B);
        // Layer sizes bottom up, above the leaves: each a 17th of the one below, to one node.
        let mut sizes = [0usize; LAYERS];
        let mut depth = 0;
        let mut below = blocks;
        while below > 1 {
            below = below.div_ceil(FAN);
            sizes[depth] = below;
            depth += 1;
        }
        let inner: usize = sizes[..depth].iter().sum();
        let mut nodes = Vec::try_with_capacity_in(inner + blocks, alloc)?;
        nodes.resize(inner + blocks, Node([K::MAX; B]));
        // Top first: the layer `depth - 1` (bottom-up numbering) is at 0.
        let mut starts = [0usize; LAYERS];
        let mut at = 0;
        for top in 0..depth {
            starts[top] = at;
            at += sizes[depth - 1 - top];
        }
        let leaves = at;
        for (k, chunk) in keys.chunks(B).enumerate() {
            nodes[leaves + k].0[..chunk.len()].copy_from_slice(chunk);
        }
        // Inner layer `l` (1 is just above the leaves) spans `17^l` leaf blocks a node and
        // `17^(l-1)` a child; a child's least key is the first of its first leaf block.
        let mut span = 1usize;
        for l in 1..=depth {
            let child_span = span;
            span = span.saturating_mul(FAN);
            let start = starts[depth - l];
            for k in 0..sizes[l - 1] {
                for j in 0..B {
                    let block = (k * FAN + j + 1).saturating_mul(child_span);
                    if block < blocks {
                        nodes[start + k].0[j] = keys[block * B];
                    }
                }
            }
        }
        Ok(StaticSearch { nodes, starts, depth, leaves, len: keys.len() })
    }

    /// Keys in.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The key of rank `i < len`.
    #[inline(always)]
    fn key(&self, i: usize) -> K {
        self.nodes[self.leaves + i / B].0[i % B]
    }

    /// The rank of the least key at or above `x`: its index in the sorted keys, [`len`] when
    /// every key is below `x` (`partition_point(|k| k < x)` of the sorted keys), by a descent
    /// of the tree at every size.
    ///
    /// [`len`]: StaticSearch::len
    #[inline]
    pub fn lower_bound(&self, x: K) -> usize {
        if self.len == 0 {
            return 0;
        }
        let left = plant::on(Plant::SearchChildLeft);
        let mut k = 0usize;
        for top in 0..self.depth {
            let i = self.nodes[self.starts[top] + k].below(x);
            k = k * FAN + if left && i > 0 { i - 1 } else { i };
        }
        (k * B + self.nodes[self.leaves + k].below(x)).min(self.len)
    }

    /// The lower bounds of `N` probes at once, each as [`lower_bound`] gives it. The `N`
    /// descents go level by level together, so each level issues `N` independent loads, and each
    /// probe's next node is prefetched as soon as its index is known. For a caller with many
    /// probes in hand (8 is the measured choice; see the module's "Batched").
    ///
    /// [`lower_bound`]: StaticSearch::lower_bound
    #[inline]
    pub fn lower_bounds<const N: usize>(&self, xs: &[K; N]) -> [usize; N] {
        let mut k = [0usize; N];
        if self.len == 0 {
            return k;
        }
        let left = plant::on(Plant::SearchChildLeft);
        let base = self.nodes.as_ptr();
        for top in 0..self.depth {
            let next = if top + 1 < self.depth { self.starts[top + 1] } else { self.leaves };
            let start = self.starts[top];
            for s in 0..N {
                let i = self.nodes[start + k[s]].below(xs[s]);
                k[s] = k[s] * FAN + if left && i > 0 { i - 1 } else { i };
                // A hint, never a read: the node at `next + k[s]` is in `nodes` (the child of a
                // node of this layer), and a prefetch of any address does not fault.
                core::intrinsics::prefetch_read_data::<Node<K>, 3>(base.wrapping_add(next + k[s]));
            }
        }
        for s in 0..N {
            k[s] = (k[s] * B + self.nodes[self.leaves + k[s]].below(xs[s])).min(self.len);
        }
        k
    }

    /// Whether `x` is in.
    #[inline]
    pub fn contains(&self, x: K) -> bool {
        self.rank(x).is_some()
    }

    /// The rank of `x` when it is in.
    #[inline]
    pub fn rank(&self, x: K) -> Option<usize> {
        let at = self.lower_bound(x);
        (at < self.len && self.key(at) == x).then_some(at)
    }

    /// The bytes it holds (for a cost check).
    pub fn bytes(&self) -> usize {
        self.nodes.len() * size_of::<Node<K>>()
    }
}

/// The ranks fit 32 bits (checked when made), so a rank is also an id.
impl<K: SearchKey, A: Allocator> StaticSearch<K, A> {
    /// [`StaticSearch::rank`] as a `u32`.
    pub fn rank32(&self, x: K) -> Option<u32> {
        // Below `MOST`, checked when made.
        self.rank(x).map(|at| at.truncate_to::<u32>())
    }
}
