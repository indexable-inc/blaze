# Requests of mem-db-checks: borrowck and wfcheck onto blaze-mem's frame

For lane `db` (owner of `crates/hir-ty-borrowck/` and `crates/hir-ty-wfcheck/`) and, while it
works in borrowck, `db-ty-d-borrowck`. Design and the speed-of-light bound:
`docs/v2/plan/lanes/mem/db-checks.md`. Contract programmed against:
`docs/v2/plan/lanes/mem/db.md` ("The contract"). `blaze_mem::frame::current` is mem-db's request 1
of lane mem, `pub fn current<R>(f: impl FnOnce(&Frame) -> R) -> R`.

Line numbers are of the tree as of 2026-10-29 20:00. Every change keeps what the code computes,
and the order it computes it in, exactly; only where the memory comes from changes. Parts:

- B0-B4 (this file, below): setup, the frame module, the core containers, the entry points.
- B5: the analyses (`liveness.rs`, `loans.rs`, `moves.rs`, `polonius.rs`, `inference.rs`,
  `constraints.rs`, `scan.rs`, `qualifs.rs`, `timings.rs`): this file, below.
- B6: `regions.rs`: `v2/requests/mem-db-checks-regions.md` (child `mem-db-checks-regions`).
- B7: `check.rs`, `check/explain.rs`: `v2/requests/mem-db-checks-check.md` (child
  `mem-db-checks-check`).
- W0-W3: wfcheck: this file, below.
- T: the budget tests: this file, below.

The rule each part follows: a per-run temporary is a frame value (`Vec<T, &'f Frame>`, a frame
slice, a frame `hashbrown` map); the memoized result (`BorrowckResult`, `WfResult`,
`DropConstraint`, `InferredOutlives`, `is_copy_query`'s `bool`) is owned, made once at its exact
size from the frame values at the end. Tags: borrowck `Tag::Mir`, wfcheck `Tag::Infer`.

---

## B0. Setup (borrowck)

`crates/hir-ty-borrowck/Cargo.toml`, `[dependencies]`, after `smallvec.workspace = true`:

```toml
blaze-mem.workspace = true
hashbrown.workspace = true
```

`[dev-dependencies]`, after `hir-ide.workspace = true`:

```toml
blaze-alloc-count.workspace = true
```

Both are root workspace dependencies. hashbrown's `nightly` feature (its `A` parameter is then
`core::alloc::Allocator`, which `&Frame` implements) is mem-db's root request 3.

`crates/hir-ty-borrowck/src/lib.rs` line 14, before:

```rust
#![forbid(unsafe_code)]
```

after:

```rust
#![forbid(unsafe_code)]
#![feature(allocator_api)]
```

Line 33 (`mod regions;`) gets a sibling, in order: `mod mem;` after `mod loans;` (line 28).
After line 36 (`pub mod timings;`):

```rust
#[cfg(test)]
mod budget;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
```

`crates/hir-ty-borrowck/src/scan.rs` line 21, before `mod tests;` after `pub(crate) mod tests;`
(under its `#[cfg(test)]`), and `crates/hir-ty-borrowck/src/scan/tests.rs` line 5, before
`mod db;` after `pub(crate) mod db;`, so the budget tests use the same test database.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: Cargo.toml and lib.rs as written; `blaze-alloc-count` goes after `test-fixture` (there is no `hir-ide` dev dependency). `scan.rs`/`scan/tests.rs` (`pub(crate) mod tests`, `pub(crate) mod db`) are not mine: not applied. lib.rs also declares `mod decode;`, `mod places;`, `mod rows;` and `#[cfg(test)] mod differential;` for frontend-fast-borrowck.

## B1. `crates/hir-ty-borrowck/src/mem.rs` (new)

```rust
//! The run's frame: every temporary of a borrow check is in it, and goes in one store when the
//! task that runs the check ends. What a check returns to the database is owned instead.

pub(crate) use blaze_mem::frame::Frame;
use rustc_hash::FxBuildHasher;

pub(crate) type FVec<'f, T> = Vec<T, &'f Frame>;
pub(crate) type FMap<'f, K, V> = hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>;
pub(crate) type FSet<'f, T> = hashbrown::HashSet<T, FxBuildHasher, &'f Frame>;
pub(crate) type FDeque<'f, T> = std::collections::VecDeque<T, &'f Frame>;

#[inline]
pub(crate) fn map<'f, K, V>(frame: &'f Frame) -> FMap<'f, K, V> {
    hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame)
}

#[inline]
pub(crate) fn set<'f, T>(frame: &'f Frame) -> FSet<'f, T> {
    hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame)
}

/// `len` copies of `value` in one allocation of exactly `len`.
#[inline]
pub(crate) fn filled<'f, T: Clone>(frame: &'f Frame, len: usize, value: T) -> FVec<'f, T> {
    let mut out = Vec::with_capacity_in(len, frame);
    out.resize(len, value);
    out
}

/// The items of `iter` in a frame vector, its room the iterator's lower bound to start.
#[inline]
pub(crate) fn collect<'f, T>(frame: &'f Frame, iter: impl IntoIterator<Item = T>) -> FVec<'f, T> {
    let iter = iter.into_iter();
    let mut out = Vec::with_capacity_in(iter.size_hint().0, frame);
    out.extend(iter);
    out
}

/// The items of `iter`, which says its length, as an owned vector of exactly that length: a
/// memoized result built from frame values.
#[inline]
pub(crate) fn owned<T>(iter: impl ExactSizeIterator<Item = T>) -> Vec<T> {
    let mut out = Vec::with_capacity(iter.len()); // alloc: the memoized result, owned
    out.extend(iter);
    out
}

#[cfg(test)]
mod tests {
    use blaze_mem::{Tag, frame};

    #[test]
    fn frame_collections_hold_what_global_ones_do() {
        frame::task(Tag::Mir, |frame| {
            let mut map = super::map(frame);
            let mut set = super::set(frame);
            for it in 0..1000u32 {
                map.insert(it, it * 2);
                set.insert(it % 37);
            }
            assert_eq!(map.len(), 1000);
            assert_eq!(map[&999], 1998);
            assert_eq!(set.len(), 37);
            let filled = super::filled(frame, 70, 7u64);
            assert_eq!((filled.len(), filled.capacity()), (70, 70));
            let owned = super::owned(filled.iter().copied());
            assert_eq!((owned.len(), owned.capacity()), (70, 70));
            assert!(frame.contains(filled.as_ptr().cast()));
            assert!(!frame.contains(owned.as_ptr().cast()));
        });
    }
}
```

Why `hashbrown` and not `std::collections::HashMap`: std's map takes no allocator. Iteration
order: hashbrown 0.17 and std's copy of it may probe differently, so every map whose iteration
order reaches the output is named in its part and turned into a vector in insertion order (with
a frame map of indices where it is looked up), which is rustc's own `FxIndexMap` order; a map
only looked up changes nothing.

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/mem.rs (every alias and helper, plus `ExactLen`).

## B2. The core containers

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:33): B2.1-B2.5 applied below, each with its own line.

### B2.1 `crates/hir-ty-borrowck/src/bitset.rs`

Bitsets become frame slices of `u64`: fixed size, so no capacity word and no growth.

Lines 1-10, before:

```rust
use std::ops::Range;

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(super) struct BitSet {
    words: Vec<u64>,
}
```

after:

```rust
use std::ops::Range;

use crate::mem::Frame;

#[derive(PartialEq, Eq, Debug, Default)]
pub(super) struct BitSet<'f> {
    words: &'f mut [u64],
}
```

Lines 55-65 (`impl BitSet` `new`, `filled`), before:

```rust
impl BitSet {
    pub(super) fn new(size: usize) -> Self {
        BitSet { words: vec![0; words_for(size)] }
    }

    /// The set of every bit below `size`.
    pub(super) fn filled(size: usize) -> Self {
        let mut out = BitSet::new(size);
        out.gen_range(0..size);
        out
    }
```

after:

```rust
impl<'f> BitSet<'f> {
    pub(super) fn new(frame: &'f Frame, size: usize) -> Self {
        BitSet { words: frame.alloc_slice_fill_with(words_for(size), |_| 0) }
    }

    /// An empty set of a row's size of `matrix`, to [`BitSet::load`] its rows into.
    pub(super) fn for_rows_of(frame: &'f Frame, matrix: &BitMatrix<'_>) -> Self {
        BitSet { words: frame.alloc_slice_fill_with(matrix.words, |_| 0) }
    }

    /// The set of every bit below `size`.
    pub(super) fn filled(frame: &'f Frame, size: usize) -> Self {
        let mut out = BitSet::new(frame, size);
        out.gen_range(0..size);
        out
    }

    /// A copy in `frame`.
    pub(super) fn clone_in(&self, frame: &'f Frame) -> Self {
        BitSet { words: frame.alloc_slice_copy(self.words) }
    }
```

Line 83 `pub(super) fn union(&mut self, other: &BitSet) -> bool {` becomes
`pub(super) fn union(&mut self, other: &BitSet<'_>) -> bool {`; lines 109-121, before:

```rust
    pub(super) fn words(&self) -> &[u64] {
        &self.words
    }

    pub(super) fn words_mut(&mut self) -> &mut [u64] {
        &mut self.words
    }

    /// Makes this the set whose words are `words`, reusing its allocation.
    pub(super) fn load(&mut self, words: &[u64]) {
        self.words.clear();
        self.words.extend_from_slice(words);
    }
}

impl Transfer for BitSet {
```

after:

```rust
    pub(super) fn words(&self) -> &[u64] {
        self.words
    }

    pub(super) fn words_mut(&mut self) -> &mut [u64] {
        self.words
    }

    /// Makes this the set whose words are `words`, which are as many as its own (a row of a
    /// matrix of its size).
    pub(super) fn load(&mut self, words: &[u64]) {
        self.words.copy_from_slice(words);
    }
}

impl Transfer for BitSet<'_> {
```

The `BitSet::default()` + `load` pattern (`check.rs` lines 160, 161, 177 `uninit`, `ever`,
`in_scope`) becomes `BitSet::for_rows_of(frame, &matrix)` of the matrix it loads from (B7 does
this); a set loaded from a row of another size was a bug, and `copy_from_slice` now stops on it.

Lines 160-172, before:

```rust
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(super) struct BitMatrix {
    words: usize,
    data: Vec<u64>,
}

impl BitMatrix {
    pub(super) fn new(rows: usize, columns: usize) -> Self {
        let words = words_for(columns);
        BitMatrix { words, data: vec![0; rows * words] }
    }
```

after:

```rust
#[derive(PartialEq, Eq, Debug, Default)]
pub(super) struct BitMatrix<'f> {
    words: usize,
    data: &'f mut [u64],
}

impl<'f> BitMatrix<'f> {
    pub(super) fn new(frame: &'f Frame, rows: usize, columns: usize) -> Self {
        let words = words_for(columns);
        BitMatrix { words, data: frame.alloc_slice_fill_with(rows * words, |_| 0) }
    }
```

Tests (lines 207-281): each test body runs in `blaze_mem::frame::task(blaze_mem::Tag::Mir,
|frame| { .. })`; `BitSet::filled(size)` -> `BitSet::filled(frame, size)`, `BitSet::new(n)` ->
`BitSet::new(frame, n)`, `BitMatrix::new(3, 70)` -> `BitMatrix::new(frame, 3, 70)`, and line
263 `let mut c = a.clone();` -> `let mut c = a.clone_in(frame);`. Add, at the end of the module:

```rust
    #[test]
    fn a_set_and_a_matrix_take_no_global_memory() {
        blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
            let _ = BitSet::new(frame, 1);
            let _ = BitMatrix::new(frame, 1, 1);
            blaze_alloc_count::assert_at_most(0, "a bitset and a matrix in the frame", || {
                let mut set = BitSet::new(frame, 1000);
                let mut matrix = BitMatrix::new(frame, 64, 1000);
                set.gen_range(3..900);
                matrix.insert(63, 999);
                set.load(matrix.row(63));
                assert!(set.contains(999));
            });
        });
    }
```

(The first two allocations warm the frame's reservation, made once per thread and depth.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: as written.

### B2.2 `crates/hir-ty-borrowck/src/relation.rs`

`Csr` is appended to (`with_capacity`, `push`, `end_node`), so its two arrays are frame vectors;
a frozen `Relation` is only read, so it is three frame slices and `Copy`.

Lines 16-29, before:

```rust
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Csr<T = u32> {
    start: Vec<u32>,
    items: Vec<T>,
}

impl<T> Default for Csr<T> {
    fn default() -> Self {
        Csr { start: vec![0], items: Vec::new() }
    }
}
```

after:

```rust
#[derive(PartialEq, Eq, Debug)]
pub(super) struct Csr<'f, T = u32> {
    start: FVec<'f, u32>,
    items: FVec<'f, T>,
}

impl<'f, T> Csr<'f, T> {
    /// No nodes.
    pub(super) fn empty(frame: &'f Frame) -> Self {
        let mut start = Vec::with_capacity_in(1, frame);
        start.push(0);
        Csr { start, items: Vec::new_in(frame) }
    }
}
```

with, at the top, `use crate::mem::{FVec, Frame, filled};`.

Lines 52-83 (`impl<T: Copy> Csr<T>`, `build`, `with_capacity`), before:

```rust
impl<T: Copy> Csr<T> {
    ...
    pub(super) fn build<I>(nodes: usize, pairs: I) -> Self
    where
        I: IntoIterator<Item = (u32, T)>,
        I::IntoIter: Clone,
    {
        let pairs = pairs.into_iter();
        let mut start = vec![0u32; nodes + 1];
        let total = starts(&mut start, pairs.clone().map(|(node, _)| node as usize));
        let Some((_, filler)) = pairs.clone().next() else {
            return Csr { start, items: Vec::new() };
        };
        let mut items = vec![filler; total];
        ...
    }

    pub(super) fn with_capacity(nodes: usize, items: usize) -> Self {
        let mut start = Vec::with_capacity(nodes + 1);
        start.push(0);
        Csr { start, items: Vec::with_capacity(items) }
    }
```

after:

```rust
impl<'f, T: Copy> Csr<'f, T> {
    ...
    pub(super) fn build<I>(frame: &'f Frame, nodes: usize, pairs: I) -> Self
    where
        I: IntoIterator<Item = (u32, T)>,
        I::IntoIter: Clone,
    {
        let pairs = pairs.into_iter();
        let mut start = filled(frame, nodes + 1, 0u32);
        let total = starts(&mut start, pairs.clone().map(|(node, _)| node as usize));
        let Some((_, filler)) = pairs.clone().next() else {
            return Csr { start, items: Vec::new_in(frame) };
        };
        let mut items = filled(frame, total, filler);
        ...
    }

    pub(super) fn with_capacity(frame: &'f Frame, nodes: usize, items: usize) -> Self {
        let mut start = Vec::with_capacity_in(nodes + 1, frame);
        start.push(0);
        Csr { start, items: Vec::with_capacity_in(items, frame) }
    }
```

(the scatter loop between them unchanged). `Csr::rebuild`, new, for `constraints.rs`'s graph that is
rebuilt after constraints are added (B5.6), reusing both arrays' room:

```rust
    /// [`Csr::build`] into this CSR's arrays, which keep their room.
    pub(super) fn rebuild<I>(&mut self, nodes: usize, pairs: I)
    where
        I: IntoIterator<Item = (u32, T)>,
        I::IntoIter: Clone,
    {
        let pairs = pairs.into_iter();
        self.start.clear();
        self.start.resize(nodes + 1, 0);
        let total = starts(&mut self.start, pairs.clone().map(|(node, _)| node as usize));
        self.items.clear();
        let Some((_, filler)) = pairs.clone().next() else { return };
        self.items.resize(total, filler);
        for (node, item) in pairs {
            let at = &mut self.start[node as usize];
            self.items[*at as usize] = item;
            *at += 1;
        }
        rewind(&mut self.start);
    }
```

Lines 132-169 (`Facts`), before:

```rust
#[derive(Default)]
pub(super) struct Facts {
    rows: Vec<Fact>,
}

impl Facts {
    pub(super) fn with_capacity(rows: usize) -> Self {
        Facts { rows: Vec::with_capacity(rows) }
    }
    ...
    pub(super) fn freeze(self, domains: &[usize]) -> Relation {
        let mut base = Vec::with_capacity(domains.len() + 1);
        let mut nodes = 0u32;
        for &domain in domains {
            base.push(nodes);
            nodes += domain as u32;
        }
        base.push(nodes);
        let node = |fact: &Fact| (base[fact.kind as usize] + fact.a) as usize;
        let mut start = vec![0u32; nodes as usize + 1];
        let total = starts(&mut start, self.rows.iter().map(node));
        let mut items = vec![0u32; total];
        for fact in &self.rows {
            let at = &mut start[node(fact)];
            items[*at as usize] = fact.b;
            *at += 1;
        }
        rewind(&mut start);
        Relation { base, start, items }
    }
}
```

after:

```rust
pub(super) struct Facts<'f> {
    rows: FVec<'f, Fact>,
}

impl<'f> Facts<'f> {
    pub(super) fn new(frame: &'f Frame) -> Self {
        Facts { rows: Vec::new_in(frame) }
    }

    pub(super) fn with_capacity(frame: &'f Frame, rows: usize) -> Self {
        Facts { rows: Vec::with_capacity_in(rows, frame) }
    }
    ...
    pub(super) fn freeze(self, domains: &[usize]) -> Relation<'f> {
        let frame = *self.rows.allocator();
        let mut nodes = 0u32;
        let base = frame.alloc_slice_fill_with(domains.len() + 1, |at| {
            let here = nodes;
            if let Some(&domain) = domains.get(at) {
                nodes += domain as u32;
            }
            here
        });
        let base = &*base;
        let node = |fact: &Fact| (base[fact.kind as usize] + fact.a) as usize;
        let start = frame.alloc_slice_fill_with(nodes as usize + 1, |_| 0u32);
        let total = starts(start, self.rows.iter().map(node));
        let items = frame.alloc_slice_fill_with(total, |_| 0u32);
        for fact in &self.rows {
            let at = &mut start[node(fact)];
            items[*at as usize] = fact.b;
            *at += 1;
        }
        rewind(start);
        Relation { base, start, items }
    }
}
```

Lines 171-184 (`Relation`), before:

```rust
#[derive(Clone, Debug)]
pub(super) struct Relation {
    base: Vec<u32>,
    start: Vec<u32>,
    items: Vec<u32>,
}

impl Default for Relation {
    fn default() -> Self {
        Relation { base: vec![0], start: vec![0], items: Vec::new() }
    }
}

impl Relation {
```

after (an empty `base` already makes every `get` empty: `base.get(kind)` is `None`):

```rust
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Relation<'f> {
    base: &'f [u32],
    start: &'f [u32],
    items: &'f [u32],
}

impl<'f> Relation<'f> {
```

and `pub(super) fn get(&self, kind: u8, a: usize) -> &[u32]` becomes
`pub(super) fn get(&self, kind: u8, a: usize) -> &'f [u32]` (the body returns `&[]` and
`&self.items[..]`, both `&'f`; write `let items = self.items; &items[..]` for the second).

Tests: in `frame::task(Tag::Mir, |frame| ..)`; `Facts::default()` -> `Facts::new(frame)`,
`Csr::build(4, ..)` -> `Csr::build(frame, 4, ..)`, `Csr::<u32>::default()` ->
`Csr::<u32>::empty(frame)`, `Csr::<u32>::build(3, ..)` -> `Csr::<u32>::build(frame, 3, ..)`. Add:

```rust
    #[test]
    fn rebuild_gives_what_build_does() {
        blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
            let pairs = [(2u32, 1u32), (0, 2), (2, 3), (3, 4), (2, 5)];
            let mut csr = Csr::build(frame, 4, pairs.iter().copied());
            csr.rebuild(5, pairs.iter().copied().chain([(4, 6)]));
            let want = Csr::build(frame, 5, pairs.iter().copied().chain([(4, 6)]));
            assert_eq!(csr, want);
            csr.rebuild(2, std::iter::empty());
            assert_eq!((csr.nodes(), csr.of(1)), (2, &[] as &[u32]));
        });
    }
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: `relation.rs` had moved to `blaze_ids::radix::Csr`, whose `group_into` takes global `Vec`s and no allocator, so B2.2's own frame counting sort replaces it in `build`, `rebuild` and `freeze`; the `starts`/`rewind` helpers the text calls were gone and are written again (`starts` asserts each node is below the count). Extra test: `Csr::build` equals `blaze_radix::Csr::build` on random pairs.

### B2.3 `crates/hir-ty-borrowck/src/dataflow.rs`

Lines 17-29, before:

```rust
pub(super) struct GenKill {
    words: usize,
    data: Vec<[u64; 2]>,
}

impl GenKill {
    /// The identity effect for `rows` rows over sets of `domain` bits.
    pub(super) fn new(rows: usize, domain: usize) -> Self {
        let words = words_for(domain);
        GenKill { words, data: vec![[0; 2]; rows * words] }
    }
```

after:

```rust
pub(super) struct GenKill<'f> {
    words: usize,
    data: &'f mut [[u64; 2]],
}

impl<'f> GenKill<'f> {
    /// The identity effect for `rows` rows over sets of `domain` bits.
    pub(super) fn new(frame: &'f Frame, rows: usize, domain: usize) -> Self {
        let words = words_for(domain);
        GenKill { words, data: frame.alloc_slice_fill_with(rows * words, |_| [0; 2]) }
    }
```

Lines 69-96 (`WorkQueue`), before:

```rust
struct WorkQueue {
    queue: VecDeque<BasicBlockId>,
    queued: BitSet,
}

impl WorkQueue {
    fn new(blocks: usize, initial: impl Iterator<Item = BasicBlockId>) -> Self {
        let mut out =
            WorkQueue { queue: VecDeque::with_capacity(blocks), queued: BitSet::new(blocks) };
```

after (each block at most once in the queue, so the ring never grows past `blocks`):

```rust
struct WorkQueue<'f> {
    queue: FDeque<'f, BasicBlockId>,
    queued: BitSet<'f>,
}

impl<'f> WorkQueue<'f> {
    fn new(
        frame: &'f Frame,
        blocks: usize,
        initial: impl Iterator<Item = BasicBlockId>,
    ) -> Self {
        let mut out = WorkQueue {
            queue: VecDeque::with_capacity_in(blocks, frame),
            queued: BitSet::new(frame, blocks),
        };
```

Imports, line 7: `use std::collections::VecDeque;` stays; add `use crate::mem::{FDeque, Frame};`.

Lines 101-131 (`forward`), before:

```rust
pub(super) fn forward(
    cfg: &Cfg,
    blocks: &GenKill,
    start: BasicBlockId,
    boundary: &BitSet,
    mut edge: impl FnMut(BasicBlockId, BasicBlockId, &mut [u64]),
) -> BitMatrix {
    let count = cfg.reachable.len();
    let mut entry = BitMatrix::new(count, blocks.words * 64);
    ...
    let mut out = vec![0u64; blocks.words];
    let mut along = vec![0u64; blocks.words];
    let mut queue = WorkQueue::new(count, cfg.order.iter().copied());
```

after:

```rust
pub(super) fn forward<'f>(
    frame: &'f Frame,
    cfg: &Cfg<'_>,
    blocks: &GenKill<'_>,
    start: BasicBlockId,
    boundary: &BitSet<'_>,
    mut edge: impl FnMut(BasicBlockId, BasicBlockId, &mut [u64]),
) -> BitMatrix<'f> {
    let count = cfg.reachable.len();
    let mut entry = BitMatrix::new(frame, count, blocks.words * 64);
    ...
    let out = frame.alloc_slice_fill_with(blocks.words, |_| 0u64);
    let along = frame.alloc_slice_fill_with(blocks.words, |_| 0u64);
    let mut queue = WorkQueue::new(frame, count, cfg.order.iter().copied());
```

and in its loop `blocks.apply(index(at), &mut out);` -> `blocks.apply(index(at), out);`,
`along.copy_from_slice(&out);` -> `along.copy_from_slice(out);`, `edge(at, to, &mut along);` ->
`edge(at, to, along);`, `union_words(entry.row_mut(index(to)), &along)` ->
`union_words(entry.row_mut(index(to)), along)`.

Lines 136-164 (`backward`) the same: `frame: &'f Frame` first, `cfg: &Cfg<'_>`,
`blocks: &GenKill<'_>`, `-> BitMatrix<'f>`, `BitMatrix::new(frame, ..)`,
`let exit = frame.alloc_slice_fill_with(blocks.words, |_| 0u64);`,
`let along = frame.alloc_slice_fill_with(blocks.words, |_| 0u64);`,
`WorkQueue::new(frame, ..)`; in the loop `&mut along` -> `along`, `&mut exit` -> `exit`,
`exit.as_slice()` -> `&*exit`, `copy_from_slice(&exit)` -> `copy_from_slice(exit)`.

Every caller passes the frame of its `Ctx` (`ctx.frame`, B4): `liveness.rs` 204, 375;
`loans.rs` 790; `moves.rs` (the two `forward` calls after line 607).

Tests (lines 166-429): each test in `frame::task(Tag::Mir, |frame| ..)`; `GenKill::new(..)` ->
`GenKill::new(frame, ..)`, `BitSet::new(..)` -> `BitSet::new(frame, ..)`,
`Cfg::from_edges(&lens, &edges)` -> `Cfg::from_edges(frame, &lens, &edges)`,
`forward(&case.cfg, ..)` -> `forward(frame, &case.cfg, ..)`, `backward(&case.cfg, ..)` ->
`backward(frame, &case.cfg, ..)`, `fn rows(entry: &crate::bitset::BitMatrix<'_>, ..)`,
`Case { cfg: Cfg<'f>, .. }` with `struct Case<'f>` and `fn case<'f>(frame: &'f Frame, rng: &mut
Rng) -> Case<'f>`, `fn composed<'f>(frame: &'f Frame, case: &Case<'_>, backward: bool) ->
GenKill<'f>`. The naive reference versions keep their global `Vec`s: they are the test's
oracle, not the code under test.

> forwarded to db (from requests, pass 16; told by tmux).

> applied by frontend-fast-borrowck, differently: `dataflow.rs` is rewritten as a difference
> fixpoint over frame row stores (`rows.rs`: `Effects<'f>` replaces `GenKill`, `Rows<'f>`
> replaces the `BitMatrix` result, `Edge` replaces the edge callback); `forward(frame, cfg,
> &Effects, start, boundary: &[u64], edge: FnMut(from, to) -> Edge) -> Rows<'f>`,
> `backward(frame, cfg, &Effects, edge) -> Rows<'f>`. All of it allocates from the frame. See
> `docs/v2/plan/lanes/frontend/fast-borrowck.md` 3.1-3.2.

### B2.4 `crates/hir-ty-borrowck/src/cfg.rs`

Lines 21-30, before:

```rust
pub(super) struct Cfg {
    starts: Vec<usize>,
    pub(super) points: usize,
    successors: Csr<BasicBlockId>,
    predecessors: Csr<BasicBlockId>,
    pub(super) order: Vec<BasicBlockId>,
    pub(super) reachable: Vec<bool>,
}
```

after (the doc comments kept):

```rust
pub(super) struct Cfg<'f> {
    starts: FVec<'f, usize>,
    pub(super) points: usize,
    successors: Csr<'f, BasicBlockId>,
    predecessors: Csr<'f, BasicBlockId>,
    pub(super) order: FVec<'f, BasicBlockId>,
    pub(super) reachable: FVec<'f, bool>,
}
```

Lines 54-85 (`Cfg::new`), before / after:

```rust
impl Cfg {
    pub(super) fn new(body: &MirBody<'_>) -> Self {
        let count = body.basic_blocks.len();
        let mut starts = Vec::with_capacity(count);
        let mut points = 0;
        let mut edges: Vec<(u32, BasicBlockId)> = Vec::with_capacity(count * 2);
        ...
        let mut unwinds: FxHashSet<(u32, BasicBlockId)> = FxHashSet::default();
        ...
        Self::of_edges(starts, points, &edges, body.start_block)
    }
```

```rust
impl<'f> Cfg<'f> {
    pub(super) fn new(frame: &'f Frame, body: &MirBody<'_>) -> Self {
        let count = body.basic_blocks.len();
        let mut starts = Vec::with_capacity_in(count, frame);
        let mut points = 0;
        let mut edges: FVec<'_, (u32, BasicBlockId)> = Vec::with_capacity_in(count * 2, frame);
        ...
        let mut unwinds: FSet<'_, (u32, BasicBlockId)> = set(frame);
        ...
        Self::of_edges(frame, starts, points, &edges, body.start_block)
    }
```

`of_edges` (lines 89-103): `fn of_edges(frame: &'f Frame, starts: FVec<'f, usize>, points:
usize, edges: &[(u32, BasicBlockId)], start: BasicBlockId) -> Self`, its two `Csr::build(count,
..)` -> `Csr::build(frame, count, ..)`, `reverse_postorder(&successors, start)` ->
`reverse_postorder(frame, &successors, start)`. `from_edges` (lines 107-122, tests only):
`frame: &'f Frame` first; `Vec::with_capacity(lens.len())` -> `Vec::with_capacity_in(lens.len(),
frame)`; `let mut unique: Vec<(u32, BasicBlockId)> = Vec::new();` ->
`let mut unique: FVec<'_, (u32, BasicBlockId)> = Vec::new_in(frame);`;
`Self::of_edges(frame, starts, points, &unique, block(0))`.

Lines 174-199 (`reverse_postorder`), before / after:

```rust
fn reverse_postorder(
    successors: &Csr<BasicBlockId>,
    start: BasicBlockId,
) -> (Vec<BasicBlockId>, Vec<bool>) {
    let count = successors.nodes();
    let mut reachable = vec![false; count];
    let mut postorder = Vec::with_capacity(count);
    let mut stack = vec![(start, 0usize)];
```

```rust
fn reverse_postorder<'f>(
    frame: &'f Frame,
    successors: &Csr<'_, BasicBlockId>,
    start: BasicBlockId,
) -> (FVec<'f, BasicBlockId>, FVec<'f, bool>) {
    let count = successors.nodes();
    let mut reachable = filled(frame, count, false);
    let mut postorder = Vec::with_capacity_in(count, frame);
    let mut stack: FVec<'_, (BasicBlockId, usize)> = Vec::with_capacity_in(count, frame);
    stack.push((start, 0usize));
```

(`stack` holds each reachable block at most once, so `count` is its bound and it never grows.)

Lines 206-308 (`Dominators`), before / after:

```rust
pub(super) struct Dominators {
    enter: Vec<usize>,
    exit: Vec<usize>,
}
...
impl Dominators {
    pub(super) fn new(cfg: &Cfg) -> Self {
        ...
        Self::of_graph(
            cfg.reachable.len(),
            ...
    fn of_graph<'g>(
        count: usize,
        ...
    ) -> Self {
        let mut enter = vec![NONE; count];
        let mut exit = vec![NONE; count];
        ...
        let mut pre = vec![NONE; count];
        let mut vertex = Vec::new();
        let mut parent = Vec::new();
        let mut stack = vec![(start, NONE)];
        ...
        let mut semi: Vec<usize> = (0..n).collect();
        let mut label: Vec<usize> = (0..n).collect();
        let mut ancestor = vec![NONE; n];
        let mut path = Vec::new();
        ...
        let children = Csr::build(n, (1..n).map(|w| (idom[w] as u32, w as u32)));
        let mut time = 0;
        let mut stack = vec![(0, false)];
```

```rust
pub(super) struct Dominators<'f> {
    enter: FVec<'f, usize>,
    exit: FVec<'f, usize>,
}
...
impl<'f> Dominators<'f> {
    pub(super) fn new(frame: &'f Frame, cfg: &Cfg<'_>) -> Self {
        ...
        Self::of_graph(
            frame,
            cfg.reachable.len(),
            ...
    fn of_graph<'g>(
        frame: &'f Frame,
        count: usize,
        ...
    ) -> Self {
        let mut enter = filled(frame, count, NONE);
        let mut exit = filled(frame, count, NONE);
        ...
        let mut pre = filled(frame, count, NONE);
        let mut vertex = Vec::with_capacity_in(count, frame);
        let mut parent = Vec::with_capacity_in(count, frame);
        let mut stack: FVec<'_, (usize, usize)> = Vec::with_capacity_in(count, frame);
        stack.push((start, NONE));
        ...
        let mut semi = collect(frame, 0..n);
        let mut label = collect(frame, 0..n);
        let mut ancestor = filled(frame, n, NONE);
        let mut path: FVec<'_, usize> = Vec::with_capacity_in(n, frame);
        ...
        let children = Csr::build(frame, n, (1..n).map(|w| (idom[w] as u32, w as u32)));
        let mut time = 0;
        let mut stack: FVec<'_, (usize, bool)> = Vec::with_capacity_in(2 * n, frame);
        stack.push((0, false));
```

(The preorder `stack` can hold an edge per successor; `with_capacity_in(count, ..)` is its
start, and it grows in place while it is the frame's last allocation, which it is not here:
growth past `count` copies once in the frame. The tree walk's holds at most `2 * n`.) `semi`,
`label` and `idom` (`parent` renamed) keep their element type `usize`.

`cfg.rs` imports, line 5: `use rustc_hash::FxHashSet;` -> `use crate::mem::{FVec, Frame,
collect, filled, set, FSet};`. Tests: `Dominators::of_graph(frame, count, ..)` inside
`frame::task`; the naive oracle keeps its `Vec`s.

`predecessor_locations` (lines 155-168) keeps its `SmallVec<[Location; 4]>`: four inline, it is
asked only while an error is explained.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: as written, kept lane db's `terminator.kind.all_successors()` in `Cfg::new` (its `successors` helper is gone).

### B2.5 `crates/hir-ty-borrowck/src/intervals.rs`

A region's value is one `IntervalSet` per region variable: the most numerous temporary of a
check. Its `SmallVec<[(u32, u32); 2]>` spills to the global heap at a third run; it becomes a
frame vector, and the two merges build into the worker's scratch and copy back into the set's
own room, so a set that grows keeps one block and the frame gets no garbage per merge.

Lines 1-11, before:

```rust
use smallvec::SmallVec;

/// Sorted, disjoint, non-adjacent inclusive ranges of points.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub(super) struct IntervalSet {
    ranges: SmallVec<[(u32, u32); 2]>,
}

impl IntervalSet {
```

after:

```rust
use smallvec::SmallVec;

use crate::mem::{FVec, Frame};

/// Sorted, disjoint, non-adjacent inclusive ranges of points.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct IntervalSet<'f> {
    ranges: FVec<'f, (u32, u32)>,
}

fn pack((start, end): (u32, u32)) -> u64 {
    (u64::from(start) << 32) | u64::from(end)
}

fn unpack(it: u64) -> (u32, u32) {
    ((it >> 32) as u32, it as u32)
}

impl<'f> IntervalSet<'f> {
    pub(super) fn new(frame: &'f Frame) -> Self {
        IntervalSet { ranges: Vec::new_in(frame) }
    }
```

Lines 67-116 (`union`), before / after:

```rust
    pub(super) fn union(&mut self, other: &IntervalSet) -> bool {
        if other.ranges.is_empty() {
            return false;
        }
        if self.ranges.is_empty() {
            self.ranges = other.ranges.clone();
            return true;
        }
        ...
        // A merge of the two sorted lists.
        let mut out: SmallVec<[(u32, u32); 2]> =
            SmallVec::with_capacity(self.ranges.len() + other.ranges.len());
        let (mut a, mut b) = (self.ranges.iter().peekable(), other.ranges.iter().peekable());
        loop {
            ...
            match out.last_mut() {
                Some(last) if next.0 <= last.1.saturating_add(1) => last.1 = last.1.max(next.1),
                _ => out.push(next),
            }
        }
        let changed = out != self.ranges;
        self.ranges = out;
        changed
    }
```

```rust
    pub(super) fn union(&mut self, other: &IntervalSet<'_>) -> bool {
        if other.ranges.is_empty() {
            return false;
        }
        if self.ranges.is_empty() {
            self.ranges.extend_from_slice(&other.ranges);
            return true;
        }
        ...
        // A merge of the two sorted lists.
        blaze_mem::scratch::with_u64s(|out| {
            out.reserve(self.ranges.len() + other.ranges.len());
            let (mut a, mut b) = (self.ranges.iter().peekable(), other.ranges.iter().peekable());
            loop {
                ... (the same `next` match)
                match out.last_mut() {
                    Some(last) if next.0 <= unpack(*last).1.saturating_add(1) => {
                        let (start, end) = unpack(*last);
                        *last = pack((start, end.max(next.1)));
                    }
                    _ => out.push(pack(next)),
                }
            }
            let changed = out.len() != self.ranges.len()
                || out.iter().zip(&self.ranges).any(|(&it, &range)| unpack(it) != range);
            if changed {
                self.ranges.clear();
                self.ranges.extend(out.iter().map(|&it| unpack(it)));
            }
            changed
        })
    }
```

Lines 118-150 (`subtract`), before:

```rust
    pub(super) fn subtract(&mut self, other: &IntervalSet) {
        ...
        let mut out: SmallVec<[(u32, u32); 2]> = SmallVec::new();
        let mut cut = other.ranges.iter().peekable();
        for &(mut start, end) in &self.ranges {
            ...
                if cs > start {
                    out.push((start, cs - 1));
                }
            ...
            if left {
                out.push((start, end));
            }
        }
        self.ranges = out;
    }
```

after:

```rust
    pub(super) fn subtract(&mut self, other: &IntervalSet<'_>) {
        ...
        blaze_mem::scratch::with_u64s(|out| {
            let mut cut = other.ranges.iter().peekable();
            for &(mut start, end) in &self.ranges {
                ... (unchanged, with `out.push(pack((start, cs - 1)))` and
                     `out.push(pack((start, end)))`)
            }
            self.ranges.clear();
            self.ranges.extend(out.iter().map(|&it| unpack(it)));
        });
    }
```

`insert_new` (lines 176-193) returns a `SmallVec<[(usize, usize); 2]>`, which spills to the
global heap at a third run; it fills its caller's buffer instead (its caller, `polonius.rs`
B5.4, keeps one for the whole walk):

```rust
    /// Adds `start..=end`, and puts in `new` (emptied first) the runs of it that were not in
    /// the set before.
    pub(super) fn insert_new(&mut self, start: usize, end: usize, new: &mut FVec<'_, (usize, usize)>) {
        new.clear();
        let mut next = start;
        for (s, e) in self.runs_in(start, end) {
            if s > next {
                new.push((next, s - 1));
            }
            next = e + 1;
        }
        if next <= end {
            new.push((next, end));
        }
        if !new.is_empty() {
            self.insert_range(start, end);
        }
    }
```

After this `smallvec` is unused in the file: line 5 `use smallvec::SmallVec;` goes.

Tests (lines 204-276): `fn set<'f>(frame: &'f Frame, points: &[usize]) -> IntervalSet<'f>` with
`IntervalSet::new(frame)`; each test in `frame::task(Tag::Mir, |frame| ..)`; `set(&..)` ->
`set(frame, &..)`; `s.ranges.as_slice()` unchanged; the `insert_new` assertion reads the buffer:

```rust
                        let mut runs = Vec::new_in(frame);
                        grown.insert_new(start, end, &mut runs);
                        let new: Vec<usize> = runs.iter().flat_map(|&(s, e)| s..=e).collect();
```

Add the budget test:

```rust
    #[test]
    fn merging_reuses_the_sets_room() {
        blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
            let mut a = set(frame, &[0, 2, 4, 6, 8, 10]);
            let b = set(frame, &[1, 5, 6, 7, 12]);
            a.union(&b);
            a.subtract(&b);
            blaze_alloc_count::assert_at_most(0, "a union and a subtraction", || {
                let used = frame.used();
                a.union(&b);
                a.subtract(&b);
                assert!(frame.used() <= used + 64);
            });
        });
    }
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-borrowck (2026-10-29 21:55): as written. The `smallvec` import goes; each
> test runs in `frame::task(Tag::Mir, ..)`.

## B3. Lifetimes

Every per-run struct above takes `'f`, the frame's lifetime; the structs of B5-B7 that hold
them take it too (`Cfg<'f>`, `MoveData<'f>`, `Loans<'f>`, `Regions<'f, 'db>`, `RegionValues<'f>`,
`Scan<'f, 'db>`, `Checker<'f, ..>`). `Ctx<'a, 'db>` does not get a new parameter: it holds
`frame: &'a Frame`, and everything it builds borrows it for `'a`, which the store, the inference
result and the scan it holds already bound. Existing lifetimes named `'f` (`liveness.rs`'s
`Effects<'f>`, line 50) are renamed `'e`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: for bitset.rs, relation.rs, cfg.rs, lib.rs, polonius.rs. `Ctx.scan` (and `Ctx::new`'s `scan`) is `&'a scan::Scan<'a, 'db>`, which B4.4 does not say; `Scan` is covariant in its frame lifetime, so the owner's scan passes.

## B4. The entry points: `crates/hir-ty-borrowck/src/lib.rs`

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:33): B4.1-B4.7 applied below by frontend-fast-borrowck-frame.

### B4.1 The queries take the run's frame

Lines 264-311, before:

```rust
#[salsa::tracked(returns(ref))]
pub fn borrowck<'db>(db: &'db Db, def: DefWithBodyId) -> BorrowckResult {
    borrowck_in_mode(db, def, mode())
}

/// [`borrowck`] in `mode`, not cached: for measuring the checker in each mode.
pub fn borrowck_in_mode(db: &Db, def: DefWithBodyId, mode: Mode) -> BorrowckResult {
    let mut result = BorrowckResult::default();
    ...
    check_owner(db, owner, Some(def), mode)
}
```

after:

```rust
#[salsa::tracked(returns(ref))]
pub fn borrowck<'db>(db: &'db Db, def: DefWithBodyId) -> BorrowckResult {
    borrowck_in_mode(db, def, mode())
}

/// [`borrowck`] in `mode`, not cached: for measuring the checker in each mode.
pub fn borrowck_in_mode(db: &Db, def: DefWithBodyId, mode: Mode) -> BorrowckResult {
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| check_def(db, def, mode, frame))
}

fn check_def(db: &Db, def: DefWithBodyId, mode: Mode, frame: &Frame) -> BorrowckResult {
    let mut result = BorrowckResult::default();
    ... (lines 272-309 unchanged)
    check_owner(db, owner, Some(def), mode, frame)
}
```

`borrowck_anon_const` (lines 323-345): its body wrapped as
`blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| { .. check_owner(db, owner, None, mode(),
frame) })`.

Why `task`, not `current`: salsa's tracked functions do not run in a frame task today, and
`frame::current` outside one stops (mem-db's request 1 asserts). A check opens its own task,
tagged `Mir`, which works alone and, once salsa runs each query in a task of its tag
(mem-db-salsa; mem-db's request 5 does it for blaze-query), is a nested task: a frame off the
thread's spares, two thread-local accesses per run, against a run that checks a body. At that
point a checker's entry may become `current`; it is not needed for the budget (T1 measures
`borrowck_in_mode`, the same task either way).

`use crate::mem::Frame;` with the other imports.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: as written, but `borrowck_anon_const`'s two early returns that allocate nothing (inline const, `Mode::Off`) stay outside the task.

### B4.2 `check_owner`: one task per body, what crosses bodies in the owner's frame

`check_owner` (lines 336-470) checks the owner's body and each closure, coroutine and inline
const in it, the nested ones first. Each body gets a task of its own (a frame one level
deeper), so its MIR analyses' memory goes when it is done and the next body reuses the same
warm pages: the peak is the largest body, not their sum. What one body hands the next
(`requirements`, `mut_upvars`, the diagnostics, the move errors) is built in the owner's frame,
`frame` below; a body's task cannot return anything that borrows its own frame.

Before (lines 336-470, abridged to the allocations):

```rust
fn check_owner<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    def: Option<DefWithBodyId>,
    mode: Mode,
) -> BorrowckResult {
    let mut result = BorrowckResult::default();
    ...
    let mut bodies = vec![(body, None, None)];
    let mut written = vec![None];
    let mut seen = rustc_hash::FxHashSet::default();
    let mut index = 0;
    while let Some(&(body, _, _)) = bodies.get(index) {
        ...
        let mut nested = Vec::new();
        nested.extend(body.closures.iter().map(|&it| Nested::Closure(it)));
        nested.extend(coroutines(body).into_iter().map(Nested::Coroutine));
        nested.extend(inline_consts(db, body).into_iter().map(Nested::Const));
        nested.extend(nested_bodies_written(db, owner, store, infer, at));
        ...
    }
    let mut requirements = FxHashMap::default();
    let mut mut_upvars = FxHashMap::default();
    let mut diagnostics: Vec<Vec<Option<BorrowckDiagnostic>>> = Vec::new();
    let mut move_errors: FxHashMap<Vec<usize>, (usize, (LocalId, Vec<PlaceElem>), usize)> =
        FxHashMap::default();
    let scan = timings::phase(timings::Phase::Setup, || {
        scan::Scan::new(store, infer, DbInterner::new_with(db, owner.krate(db)))
    });
    for (body, closure, inline_const) in bodies.into_iter().rev() {
        let mut ctx = timings::phase(timings::Phase::Setup, || {
            Ctx::new(db, owner, store, infer, &scan, body, closure, mode)
        });
        ctx.inline_const = inline_const.is_some();
        if closure.is_none() && inline_const.is_none() && drops_in_const(&ctx, def, body) {
            result.not_checked = Some(NotChecked::DropInConst);
            return result;
        }
        let mut found = Vec::new();
        let mut moved = Vec::new();
        let (count, needs, used_mut_upvars) =
            check::check(&ctx, body, &requirements, &mut_upvars, &mut found, &mut moved);
        if ctx.tainted.get() {
            result.not_checked = Some(NotChecked::Unproven);
            return result;
        }
        let this = diagnostics.len();
        let mut found: Vec<_> = found.into_iter().map(Some).collect();
        for error in moved {
            ...
            move_errors.insert(key, (this, error.place, error.at));
        }
        diagnostics.push(found);
        if let Some(nested) = closure.or(inline_const) {
            requirements.insert(nested, (count, needs));
            mut_upvars.insert(nested, used_mut_upvars);
        }
    }
    result.diagnostics.extend(diagnostics.into_iter().rev().flatten().flatten());
    ...
}
```

After:

```rust
fn check_owner<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    def: Option<DefWithBodyId>,
    mode: Mode,
    frame: &Frame,
) -> BorrowckResult {
    let mut result = BorrowckResult::default();
    ... (the MIR of the owner, unchanged)
    let mut bodies: FVec<'_, _> = Vec::new_in(frame);
    bodies.push((body, None, None));
    let mut written: FVec<'_, Option<ExprId>> = Vec::new_in(frame);
    written.push(None);
    let mut seen = mem::set(frame);
    let mut nested: FVec<'_, Nested<'db>> = Vec::new_in(frame);
    let mut index = 0;
    while let Some(&(body, _, _)) = bodies.get(index) {
        let at = written[index];
        index += 1;
        nested.clear();
        nested.extend(body.closures.iter().map(|&it| Nested::Closure(it)));
        coroutines(body, |it| nested.push(Nested::Coroutine(it)));
        inline_consts(db, body, |it| nested.push(Nested::Const(it)));
        nested_bodies_written(db, owner, store, infer, at, frame, &mut nested);
        for &it in nested.iter() {
            ... (unchanged)
        }
    }
    // What a closure needs of its creator, and which of its captures it uses mutably: by the
    // closure's expression, in the owner's frame.
    let mut requirements: FMap<'_, ExprId, (usize, FVec<'_, regions::Requirement<'db>>)> =
        mem::map(frame);
    let mut mut_upvars: FMap<'_, ExprId, FVec<'_, usize>> = mem::map(frame);
    let mut diagnostics: FVec<'_, FVec<'_, Option<BorrowckDiagnostic>>> = Vec::new_in(frame);
    let mut move_errors: FMap<'_, FVec<'_, usize>, (usize, (LocalId, FVec<'_, PlaceElem>), usize)> =
        mem::map(frame);
    let scan = timings::phase(timings::Phase::Setup, || {
        scan::Scan::new(frame, store, infer, DbInterner::new_with(db, owner.krate(db)))
    });
    for &(body, closure, inline_const) in bodies.iter().rev() {
        let checked = blaze_mem::frame::task(blaze_mem::Tag::Mir, |body_frame| {
            let mut ctx = timings::phase(timings::Phase::Setup, || {
                Ctx::new(db, owner, store, infer, &scan, body, closure, mode, body_frame)
            });
            ctx.inline_const = inline_const.is_some();
            if closure.is_none() && inline_const.is_none() && drops_in_const(&ctx, def, body) {
                return Err(NotChecked::DropInConst);
            }
            let mut found = Vec::new_in(frame);
            let mut moved = Vec::new_in(frame);
            let (count, needs, used_mut_upvars) = check::check(
                &ctx,
                frame,
                body,
                &requirements,
                &mut_upvars,
                &mut found,
                &mut moved,
            );
            if ctx.tainted.get() {
                return Err(NotChecked::Unproven);
            }
            Ok((found, moved, count, needs, used_mut_upvars))
        });
        let (found, moved, count, needs, used_mut_upvars) = match checked {
            Ok(it) => it,
            Err(why) => {
                result.not_checked = Some(why);
                return result;
            }
        };
        let this = diagnostics.len();
        let mut found = mem::collect(frame, found.into_iter().map(Some));
        for error in moved {
            ... (unchanged: `move_errors.get(&key)`, `check::PlaceRef { local: place.0,
                 projection: &place.1 }`, `move_errors.insert(key, (this, error.place,
                 error.at))`)
        }
        diagnostics.push(found);
        if let Some(nested) = closure.or(inline_const) {
            requirements.insert(nested, (count, needs));
            mut_upvars.insert(nested, used_mut_upvars);
        }
    }
    let reported = diagnostics.iter().flatten().flatten().count();
    result.diagnostics = mem::owned(
        ExactLen::new(diagnostics.into_iter().rev().flatten().flatten(), reported),
    );
    ... (the debug print, unchanged)
    result
}
```

The task's closure returns `found` and `moved`, frame vectors of the owner's frame (not the
body's), which the closure may return: they borrow `frame`, not `body_frame`. Inside the body's
task `check::check` takes both frames: `ctx.frame` (the body's) for its analyses and `frame`
(the owner's) for `needs`, `used_mut_upvars` and the two output vectors (B7).
`check::UseOfMoved` holds `rustc_key: Option<FVec<'o, usize>>` and
`place: (LocalId, FVec<'o, PlaceElem>)` in the owner's frame `'o` (B7).

`ExactLen`, in `mem.rs` (B1), an iterator of a length counted once, for the flattened result:

```rust
/// `iter`, of `len` items, as an iterator that says so.
pub(crate) struct ExactLen<I> {
    iter: I,
    len: usize,
}

impl<I> ExactLen<I> {
    pub(crate) fn new(iter: I, len: usize) -> Self {
        ExactLen { iter, len }
    }
}

impl<I: Iterator> Iterator for ExactLen<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        let it = self.iter.next()?;
        self.len -= 1;
        Some(it)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<I: Iterator> ExactSizeIterator for ExactLen<I> {}
```

`result.skipped` (lines 348, 399) keeps its `format!`: it is part of the owned result and is
made once, when MIR lowering refuses a body.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: as written; `check::check` is called with B7's signature, `scan::Scan::new(frame, ..)` with B5.7's.

### B4.3 `coroutines`, `inline_consts`, `nested_bodies_written`

They return a fresh `Vec` per body checked. Lines 514-532, before / after:

```rust
fn coroutines<'db>(body: &MirBody<'db>) -> Vec<InternedCoroutineId<'db>> {
    ...
    let mut out = Vec::new();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let ... && !out.contains(&id.0)
            {
                out.push(id.0);
            }
        }
    }
    out
}
```

```rust
fn coroutines<'db>(body: &MirBody<'db>, mut found: impl FnMut(InternedCoroutineId<'db>)) {
    ...
    let mut last = None;
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(_, Rvalue::Aggregate(AggregateKind::Closure(ty), _)) =
                &statement.kind
                && let TyKind::Coroutine(id, _) = ty.as_ref().kind()
                && last != Some(id.0)
            {
                last = Some(id.0);
                found(id.0);
            }
        }
    }
}
```

The dedup moves out: `check_owner`'s `seen` set already drops a nested body seen before (line
373, `if !seen.insert(it) { continue; }`), and the order of first sight is kept, so dropping
`!out.contains(..)` (quadratic in the coroutines of a body) changes nothing it returns; `last`
keeps the common case, one aggregate statement per coroutine repeated, from reaching the set.

`inline_consts` (lines 593-615) the same: `fn inline_consts<'db>(db: &'db Db, body:
&MirBody<'db>, mut found: impl FnMut(hir_ty::db::AnonConstId<'db>))`, `found(id)` for
`out.push(id)`, the `!out.contains(&id)` test replaced by `last != Some(id)` with `last` as
above.

`nested_bodies_written` (lines 544-590), before / after:

```rust
fn nested_bodies_written<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    store: &ExpressionStore,
    infer: &InferenceResult<'db>,
    at: Option<ExprId>,
) -> Vec<Nested<'db>> {
    ...
    let mut stack = Vec::new();
    ...
    let mut out = Vec::new();
    while let Some(expr) = stack.pop() {
        ... out.push(..) ...
    }
    out
}
```

```rust
fn nested_bodies_written<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    store: &ExpressionStore,
    infer: &InferenceResult<'db>,
    at: Option<ExprId>,
    frame: &Frame,
    out: &mut FVec<'_, Nested<'db>>,
) {
    ...
    let mut stack: FVec<'_, ExprId> = Vec::new_in(frame);
    ...
    while let Some(expr) = stack.pop() {
        ... out.push(..) ...
    }
}
```

(`stack` is the only allocation per call; one per body, in the owner's frame.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: as written.

### B4.4 `Ctx` (lines 617-726)

Fields, before:

```rust
    pub(crate) bindings: FxHashMap<LocalId, BindingId>,
    pub(crate) upvars: Vec<Upvar>,
    ...
    copy: std::cell::RefCell<FxHashMap<Ty<'db>, bool>>,
    glue: std::cell::RefCell<FxHashMap<Ty<'db>, hir_ty::drop::DropGlue>>,
}
```

after:

```rust
    pub(crate) bindings: FMap<'a, LocalId, BindingId>,
    pub(crate) upvars: FVec<'a, Upvar>,
    ...
    copy: std::cell::RefCell<FMap<'a, Ty<'db>, bool>>,
    glue: std::cell::RefCell<FMap<'a, Ty<'db>, hir_ty::drop::DropGlue>>,
    /// The body's task's frame: what the checks of this body build.
    pub(crate) frame: &'a Frame,
}
```

`Ctx::new` (lines 662-725) gets `frame: &'a Frame` last, and:

- line 676-677, before `let bindings = body.binding_locals.iter().map(|(binding, &local)|
  (local, binding)).collect();` after:

  ```rust
        let mut bindings = mem::map(frame);
        bindings.reserve(body.binding_locals.len());
        bindings.extend(body.binding_locals.iter().map(|(binding, &local)| (local, binding)));
  ```

- line 678 `let mut upvars = Vec::new();` -> `let mut upvars = Vec::new_in(frame);`
- lines 685-686, before `let mut locals: Vec<LocalId> = body.upvar_locals.values().flatten()
  .map(|(local, _)| *local).collect();` after
  `let mut locals = mem::collect(frame, body.upvar_locals.values().flatten().map(|(local, _)| *local));`
  (`radix::sort_copy_by_key(&mut locals, ..)` takes `&mut [T]`, which the frame vector
  derefs to).
- line 700 `place: capture.clone(),` stays: `CapturedPlace` is hir-ty's (its projections a
  `Vec`), one clone per capture per closure body; its frame form is mem-db-infer's.
- lines 722-723, before `copy: Default::default(), glue: Default::default(),` after
  `copy: std::cell::RefCell::new(mem::map(frame)), glue: std::cell::RefCell::new(mem::map(frame)),
  frame,`

`self.copy.borrow().get(&ty)`, `.insert(ty, known)` and the `bindings.get(&local)` reads are the
same on a hashbrown map.

`infcx` (line 675): `interner.infer_ctxt().build(..)` builds hir-ty's `InferCtxt`, unchanged
here: its tables do not come from the body's frame but are recycled per worker
(`v2/requests/mem-db-solver.md` request 5), so nothing is threaded into it.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: `ArenaMap` has no `len()`, so `bindings.reserve(body.binding_locals.values().count())`. `Ctx` also gets `places: RefCell<places::Places<'a, 'db>>` (`Places::new(frame, body)`), and `place_ty` goes through `Ctx::place`, for frontend-fast-borrowck.

### B4.5 `normalized_env` (lines 889-912)

Line 911, before:

```rust
    ParamEnv::new(Clauses::new_from_iter(clauses.collect::<Vec<_>>()))
```

after (`env.clauses().iter()` is a copied slice iterator mapped, so it says its length):

```rust
    blaze_mem::frame::current(|frame| {
        ParamEnv::new(Clauses::new_from_slice(frame.alloc_slice_from_iter(clauses)))
    })
```

It runs in the body's task (from `Ctx::new`), so `current` is the body's frame.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: as written.

### B4.6 `debugging` (lines 90-98)

`std::env::var` per call takes the environment's lock (and allocates when the variable is
set); it is asked at the end of every `check_owner` and per body in `check.rs`. Before:

```rust
pub(crate) fn debugging(db: &Db, owner: InferBodyId<'_>) -> bool {
    let Ok(wanted) = std::env::var("BLAZE_BORROWCK_DEBUG") else { return false };
```

after:

```rust
pub(crate) fn debugging(db: &Db, owner: InferBodyId<'_>) -> bool {
    static WANTED: LazyLock<Option<String>> =
        LazyLock::new(|| std::env::var("BLAZE_BORROWCK_DEBUG").ok()); // alloc: once per process
    let Some(wanted) = WANTED.as_deref() else { return false };
```

(the `match` after it compares `wanted` as a `&str` already: `wanted != "1"`, `== wanted`).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: as written.

### B4.7 `drops_in_const` (lines 475-512)

Line 489 `let mut moved = rustc_hash::FxHashSet::default();` -> a bitset of locals:

```rust
    let mut moved = BitSet::new(ctx.frame, body.locals.len());
    let mut visit = |operand: &hir_ty_mir::mir::Operand| {
        if let OperandKind::Move(place) = &operand.kind {
            moved.insert(place.local.into_raw().into_u32() as usize);
        }
    };
```

and line 508 `!moved.contains(&place.local)` ->
`!moved.contains(place.local.into_raw().into_u32() as usize)`; `use crate::bitset::BitSet;`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: as written.

## B5. The analyses

Signatures B6 (regions.rs) and B7 (check.rs) call, after this part:

| function | new signature (frame from) |
|---|---|
| `Cfg::new` | `Cfg::new(ctx.frame, body)` |
| `Dominators::new` | `Dominators::new(ctx.frame, &cfg)` |
| `MoveData::gather` | `MoveData::gather(ctx, body, &cfg) -> MoveData<'a>` (frame `ctx.frame`) |
| `InitFlow::compute` | `InitFlow::compute(ctx.frame, body, &cfg, &moves) -> InitFlow<'f>` |
| `liveness::region_liveness` | `(ctx, body, &cfg, &moves, &regions, count) -> FVec<'a, IntervalSet<'a>>` |
| `liveness::live_regions` | `(ctx, &regions, ty) -> FVec<'a, Rv>` |
| `inference::infer` | `infer(ctx.frame, &regions, count, points, &live) -> RegionValues<'f>` |
| `polonius::live_loans` | `(ctx, body, &cfg, &regions, count, &live, &loans) -> FVec<'a, IntervalSet<'a>>` |
| `Loans::gather` | `(ctx, body, &cfg, &regions, unique) -> Loans<'a>` |
| `Loans::compute_scopes` | `(&mut self, ctx, body, &cfg, &values)` |
| `Loans::compute_polonius_scopes` | `(&mut self, ctx, body, &cfg, &live_loans)` |
| `Loans::entry_sets` | `(&self, ctx, body, &cfg) -> BitMatrix<'a>` (check.rs line 112 passes `body, &cfg` today; B7.12 adds `ctx` to it and to lines 108 and 110) |
| `loans::static_locals` | `(ctx.frame, body) -> FSet<'f, LocalId>` |
| `scan::Scan::new` | `Scan::new(frame, store, infer, interner) -> Scan<'f, 'db>` (the owner's frame, B4.2) |
| `Constraints` | `Constraints::new(frame)`, no `Default` |

`'a` is `Ctx<'a, 'db>`'s: the body's task's frame.

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:33): B5.1-B5.9 applied below, each with its own line.

### B5.1 `liveness.rs`

Per body: `region_liveness` makes the facts of every point (two per point), two backward
analyses over all locals, a value per region, the order of points and the drop components.

- Line 50, `struct Effects<'f> { facts: &'f mut Facts, point }` -> `struct Effects<'e, 'f> {
  facts: &'e mut Facts<'f>, point: usize }`; `impl Effects<'_, '_>`; the free functions'
  `effects: &mut Effects<'_>` -> `&mut Effects<'_, '_>`.
- Line 119 `drops_initialized: &BitSet` -> `&BitSet<'_>`.
- `live_ranges` (lines 184-249), before:

  ```rust
  fn live_ranges(
      body: &MirBody<'_>,
      cfg: &Cfg,
      effects: &Relation,
      uses: u8,
      edge: impl Fn(BasicBlockId, BasicBlockId, &mut [u64]),
  ) -> Vec<IntervalSet> {
      let locals = body.locals.len();
      let mut transfer = GenKill::new(body.basic_blocks.len(), locals);
      ...
      let entry = backward(cfg, &transfer, &edge);
      let mut ranges = vec![IntervalSet::default(); locals];
      let mut run_end = vec![0usize; locals];
      let mut live = BitSet::new(locals);
      let mut along = vec![0u64; live.words().len()];
  ```

  after:

  ```rust
  fn live_ranges<'f>(
      frame: &'f Frame,
      body: &MirBody<'_>,
      cfg: &Cfg<'_>,
      effects: &Relation<'_>,
      uses: u8,
      edge: impl Fn(BasicBlockId, BasicBlockId, &mut [u64]),
  ) -> FVec<'f, IntervalSet<'f>> {
      let locals = body.locals.len();
      let mut transfer = GenKill::new(frame, body.basic_blocks.len(), locals);
      ...
      let entry = backward(frame, cfg, &transfer, &edge);
      let mut ranges = filled(frame, locals, IntervalSet::new(frame));
      let run_end = frame.alloc_slice_fill_with(locals, |_| 0usize);
      let mut live = BitSet::new(frame, locals);
      let along = frame.alloc_slice_fill_with(live.words().len(), |_| 0u64);
  ```

  and in the loop `along.copy_from_slice(..)` unchanged, `edge(block, successor, &mut along)`
  -> `edge(block, successor, along)`, `union_words(live.words_mut(), &along)` ->
  `union_words(live.words_mut(), along)`.
- `region_liveness` (lines 252-341), before / after:

  ```rust
  pub(super) fn region_liveness<'db>(
      ctx: &Ctx<'_, 'db>,
      body: &MirBody<'db>,
      cfg: &Cfg,
      moves: &MoveData,
      regions: &Regions<'db>,
      count: usize,
  ) -> Vec<IntervalSet> {
      let mut values = vec![IntervalSet::default(); count];
      let (drops_initialized, init_at_exit) = maybe_initialized_at_drops(body, cfg, moves);
      let mut facts = Facts::with_capacity(cfg.points * 2);
      ...
      let mut local_regions = Csr::with_capacity(regions.local_tys.len(), regions.local_tys.len());
      let mut scratch = Vec::new();
      ...
      let use_live = live_ranges(body, cfg, &effects, USE, |_, _, _| ());
      ...
      let mut drop_live = live_ranges(
          body,
          ...
      let mut dropped = Vec::new();
      ...
      let mut rank = vec![usize::MAX; cfg.points];
  ```

  ```rust
  pub(super) fn region_liveness<'a, 'db>(
      ctx: &Ctx<'a, 'db>,
      body: &MirBody<'db>,
      cfg: &Cfg<'_>,
      moves: &MoveData<'_>,
      regions: &Regions<'_, 'db>,
      count: usize,
  ) -> FVec<'a, IntervalSet<'a>> {
      let frame = ctx.frame;
      let mut values = filled(frame, count, IntervalSet::new(frame));
      let (drops_initialized, init_at_exit) = maybe_initialized_at_drops(frame, body, cfg, moves);
      let mut facts = Facts::with_capacity(frame, cfg.points * 2);
      ...
      let mut local_regions =
          Csr::with_capacity(frame, regions.local_tys.len(), regions.local_tys.len());
      let mut scratch: FVec<'_, Rv> = Vec::new_in(frame);
      ...
      let use_live = live_ranges(frame, body, cfg, &effects, USE, |_, _, _| ());
      ...
      let mut drop_live = live_ranges(
          frame,
          body,
          ...
      let mut dropped: FVec<'_, usize> = Vec::new_in(frame);
      ...
      let rank = frame.alloc_slice_fill_with(cfg.points, |_| usize::MAX);
  ```

  (`radix::sort_by_key(&mut dropped, ..)` takes `&mut [T]`.) `DropComponents::new(ctx,
  regions)` unchanged in its call.
- `maybe_initialized_at_drops` (lines 346-408): `frame: &'f Frame` first, `cfg: &Cfg<'_>`,
  `moves: &MoveData<'_>`, `-> (BitSet<'f>, InitAtExit<'f>)`; `GenKill::new(frame, ..)` twice,
  `let mut returns_to = filled(frame, blocks, None::<BasicBlockId>);`, `BitSet::new(frame, ..)`
  for `start`, `reached`, `found`, `state`, `BitMatrix::new(frame, ..)` for `exit`, `returned`,
  `forward(frame, cfg, ..)`.
- `locals_in` (line 435): `state: &BitSet<'_>`, `moves: &MoveData<'_>`.
- `InitAtExit` (lines 446-452): `struct InitAtExit<'f> { exit: BitMatrix<'f>, returned:
  BitMatrix<'f>, returns_to: FVec<'f, Option<BasicBlockId>>, reached: BitSet<'f> }`,
  `impl InitAtExit<'_>`.
- `DropComponents` (lines 482-512), before / after:

  ```rust
  struct DropComponents<'c, 'a, 'db> {
      ctx: &'c Ctx<'a, 'db>,
      regions: &'c Regions<'db>,
      done: FxHashMap<Ty<'db>, (u32, u32)>,
      pool: Vec<Rv>,
      open: FxHashMap<Ty<'db>, usize>,
      stack: Vec<Ty<'db>>,
      found: Vec<Rv>,
      children: Vec<Ty<'db>>,
      next: usize,
  }
  ...
      fn new(ctx: &'c Ctx<'a, 'db>, regions: &'c Regions<'db>) -> Self {
          DropComponents {
              ctx,
              regions,
              done: FxHashMap::default(),
              pool: Vec::new(),
              open: FxHashMap::default(),
              stack: Vec::new(),
              found: Vec::new(),
              children: Vec::new(),
              next: 0,
          }
      }
  ```

  ```rust
  struct DropComponents<'c, 'a, 'db> {
      ctx: &'c Ctx<'a, 'db>,
      regions: &'c Regions<'c, 'db>,
      done: FMap<'a, Ty<'db>, (u32, u32)>,
      pool: FVec<'a, Rv>,
      open: FMap<'a, Ty<'db>, usize>,
      stack: FVec<'a, Ty<'db>>,
      found: FVec<'a, Rv>,
      children: FVec<'a, Ty<'db>>,
      next: usize,
  }
  ...
      fn new(ctx: &'c Ctx<'a, 'db>, regions: &'c Regions<'c, 'db>) -> Self {
          let frame = ctx.frame;
          DropComponents {
              ctx,
              regions,
              done: mem::map(frame),
              pool: Vec::new_in(frame),
              open: mem::map(frame),
              stack: Vec::new_in(frame),
              found: Vec::new_in(frame),
              children: Vec::new_in(frame),
              next: 0,
          }
      }
  ```

  `radix::sort(all)` (line 553) takes `&mut [T]`; `self.stack.drain(at..)` is the same on a
  frame vector. Neither map is iterated, so their order reaches nothing.
- `expand` (lines 571-638): `regions: &Regions<'_, 'db>`, `found: &mut FVec<'_, Rv>`,
  `children: &mut FVec<'_, Ty<'db>>`.
- `drop_constraint` (lines 657-696), a query of its own whose value is owned. Before:

  ```rust
  fn drop_constraint(db: &Db, adt: AdtId) -> DropConstraint {
      let interner = DbInterner::new_no_crate(db);
      let mut regions = Vec::new();
      let mut whole = Vec::new();
      let mut tys = Vec::new();
      if let Some(drop_impl) = hir_ty::drop::destructor(db, adt) {
          let args = GenericArgs::identity_for_item(interner, adt.into());
          let dangling = may_dangle(db, *drop_impl, args.len());
          ...
      }
      let variants: Vec<hir_def::VariantId> = match adt {
          AdtId::StructId(it) => vec![it.into()],
          AdtId::UnionId(_) => Vec::new(),
          AdtId::EnumId(it) => {
              it.enum_variants(db).variants.values().map(|&(it, _)| it.into()).collect()
          }
      };
      for variant in variants {
          ...
      }
      let mut seen = FxHashSet::default();
      tys.retain(|it| seen.insert(*it));
      DropConstraint {
          regions: regions.into_iter().map(|it| it.store()).collect(),
          whole: whole.into_iter().map(|it| it.store()).collect(),
          tys: tys.into_iter().map(|it| it.store()).collect(),
      }
  }
  ```

  after:

  ```rust
  fn drop_constraint(db: &Db, adt: AdtId) -> DropConstraint {
      blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
          let interner = DbInterner::new_no_crate(db);
          let mut regions: FVec<'_, Region<'_>> = Vec::new_in(frame);
          let mut whole: FVec<'_, Ty<'_>> = Vec::new_in(frame);
          let mut tys: FVec<'_, Ty<'_>> = Vec::new_in(frame);
          if let Some(drop_impl) = hir_ty::drop::destructor(db, adt) {
              let args = GenericArgs::identity_for_item(interner, adt.into());
              let dangling = may_dangle(frame, db, *drop_impl, args.len());
              ... (unchanged)
          }
          let mut fields = |variant: hir_def::VariantId| {
              for (_, field) in db.field_types(variant).iter() {
                  let field = field.ty().instantiate_identity().skip_norm_wip();
                  take_apart(db, interner, field, &mut regions, &mut whole, &mut tys);
              }
          };
          match adt {
              AdtId::StructId(it) => fields(it.into()),
              AdtId::UnionId(_) => (),
              AdtId::EnumId(it) => {
                  for &(variant, _) in it.enum_variants(db).variants.values() {
                      fields(variant.into());
                  }
              }
          }
          let mut seen = mem::set(frame);
          tys.retain(|it| seen.insert(*it));
          DropConstraint {
              regions: mem::owned(regions.iter().map(|it| it.store())),
              whole: mem::owned(whole.iter().map(|it| it.store())),
              tys: mem::owned(tys.iter().map(|it| it.store())),
          }
      })
  }
  ```

  The three owned vectors are made once at their exact length; `.collect()` over a
  `Vec::into_iter().map(..)` could reuse the doubled source allocation in place, so its capacity
  was not the length.
- `take_apart` (lines 703-750): `regions: &mut FVec<'_, Region<'db>>`, `whole: &mut FVec<'_,
  Ty<'db>>`, `tys: &mut FVec<'_, Ty<'db>>`.
- `may_dangle` (lines 755-776): `fn may_dangle<'f>(frame: &'f Frame, db: &Db, drop_impl:
  hir_def::ImplId, count: usize) -> FVec<'f, bool>`; `return vec![false; count]` ->
  `return filled(frame, count, false)`; the final `.collect()` -> `mem::collect(frame, ..)`
  around the same `args.iter().map(..)`.
- `live_regions_of` (lines 781-826): `regions: &Regions<'_, 'db>`, `out: &mut FVec<'_, Rv>`;
  `struct Visit<'c, 'a, 'db> { ctx: &'c Ctx<'a, 'db>, regions: &'c Regions<'c, 'db>, out: &'c
  mut FVec<'a, Rv> }`.
- `live_regions` (lines 829-837): `-> FVec<'a, Rv>` with `ctx: &Ctx<'a, 'db>`,
  `let mut out = Vec::new_in(ctx.frame);`.
- `alias_bound` (lines 842-906), asked for every alias in every local's type. Lines 856-862,
  before:

  ```rust
      let item_bounds: Vec<Clause<'db>> =
          rustc_type_ir::Interner::item_bounds(interner, super::regions::alias_def_id(alias.kind))
              .iter_instantiated(interner, alias.args)
              .map(|it| it.skip_norm_wip())
              .collect();
      let mut found: Option<Option<Rv>> = None;
      for clause in item_bounds.into_iter().chain(ctx.env.caller_bounds()) {
  ```

  after:

  ```rust
      let item_bounds = mem::collect(
          ctx.frame,
          rustc_type_ir::Interner::item_bounds(interner, super::regions::alias_def_id(alias.kind))
              .iter_instantiated(interner, alias.args)
              .map(|it| it.skip_norm_wip()),
      );
      let mut found: Option<Option<Rv>> = None;
      for clause in item_bounds.iter().copied().chain(ctx.env.caller_bounds()) {
  ```

  and `regions_in` (lines 877-884) `let mut found = Vec::new();` ->
  `let mut found: FVec<'_, Region<'db>> = Vec::new_in(ctx.frame);`.
- Imports: line 7 `use rustc_hash::{FxHashMap, FxHashSet};` ->
  `use crate::mem::{self, FMap, FVec, Frame, filled};`.

> forwarded to db (from requests, pass 16; told by tmux).

> applied by frontend-fast-borrowck-facts: liveness.rs on the frame as above, with these
> differences. `region_liveness(ctx, body, &cfg, &decoded, &moves, &regions, count)` takes the
> decode (`decode.rs`); the uses and defs of each point are its columns, and the `Facts` here
> hold only the callee reads and the drops of what may be initialized (`READ`, `DROP`), so
> `Effects`/`effects_at`/`place_uses`/`operand_uses`/`assigned` are gone. `live_ranges` and
> `maybe_initialized_at_drops` run on `rows::Effects` and `dataflow::backward`/`forward` (not
> `GenKill`), their edges `rows::Edge`; `InitAtExit::restrict` is `InitAtExit::edge` (an
> `Edge::Mask` of the exit or return row, `Edge::Nothing` from a block not reached), its
> `returns_to` the decode's `returns: &'f [u32]`. `DropComponents` has a fourth lifetime for
> `Regions<'r, 'db>`; `live_regions_of`'s `Visit` one for its output vector. Calls
> `regions_of(ty, &mut FVec)` and `opaque_captures(frame, ..)` of R8.

### B5.2 `loans.rs`

Per body: the loans, their projections, three relations; per loan, `out_of_scope`'s walk; per
point of a body with a two-phase borrow, a list of used locals.

- `Loans` (lines 69-81): `pub(super) struct Loans<'f> { pub(super) loans: FVec<'f, Loan>,
  projections: FVec<'f, PlaceElem>, index: Relation<'f>, scopes: Relation<'f>, kills:
  Relation<'f>, pub(super) promoted: FSet<'f, Location> }`.
- `ignored` (line 109): `statics: &FSet<'_, LocalId>`.
- `plain_value` (line 164): `consts: &Default::default(),` ->
  `consts: &std::cell::RefCell::new(mem::map(ctx.frame)),`.
- `static_locals` (lines 170-183): `pub(super) fn static_locals<'f>(frame: &'f Frame, body:
  &MirBody<'_>) -> FSet<'f, LocalId>`, `let mut found = mem::set(frame);`.
- `extended_temporaries` (lines 189-235): `-> FSet<'a, LocalId>` with `ctx: &Ctx<'a, '_>`;
  `let mut refs = mem::set(ctx.frame);`, line 195 `let mut stack: Vec<_> =
  root.into_iter().collect();` -> `let mut stack = mem::collect(ctx.frame, root);`, line 222
  `let mut found = mem::set(ctx.frame);`.
- `const_promoted` (lines 244-277): `-> FSet<'a, LocalId>`; `calls` (line 247) runs per borrow
  of a const item: `let mut stack = vec![expr];` -> `let mut stack = Vec::new_in(ctx.frame);
  stack.push(expr);`; line 256 `let mut found = mem::set(ctx.frame);`.
- `certain_blocks` (lines 282-296): `fn certain_blocks<'f>(frame: &'f Frame, body:
  &MirBody<'_>) -> FSet<'f, BasicBlockId>`, `let mut found = mem::set(frame);`; its caller
  line 245 `certain_blocks(ctx.frame, body)`.
- `promoted_borrows` (lines 304-383): `-> FSet<'a, Location>`; `define`'s `defs: &mut
  FMap<'_, LocalId, Option<&'b Rvalue>>`, line 312 `let mut defs = mem::map(ctx.frame);`,
  `promotable`'s `defs: &FMap<'_, LocalId, Option<&Rvalue>>`, line 367
  `let mut found = mem::set(ctx.frame);`. The `entry(..).and_modify(..).or_insert(..)` is the
  same on hashbrown.
- `const_promotable` (lines 385-467), per element of a const item's value: `all` takes the
  children as an iterator, not a fresh `Vec` per aggregate. Lines 396-398, before:

  ```rust
      let all = |exprs: Vec<hir_def::hir::ExprId>| {
          exprs.into_iter().all(|it| const_promotable(ctx, body, it, depth + 1))
      };
  ```

  after:

  ```rust
      let promotable = |it: hir_def::hir::ExprId| const_promotable(ctx, body, it, depth + 1);
  ```

  (it captures two shared references and a `usize`, so it is `Copy` and each site passes it by
  value: no `dyn`, no collect). The call sites, before -> after: `all(elements.to_vec())` ->
  `elements.iter().copied().all(promotable)` (line 422), `all(exprs.to_vec())` ->
  `exprs.iter().copied().all(promotable)` (426), `all(fields.iter().map(|it|
  it.expr).collect())` -> `fields.iter().map(|it| it.expr).all(promotable)` (428),
  `all(args.to_vec())` -> `args.iter().copied().all(promotable)` (448),
  `all(std::iter::once(receiver).chain(args.to_vec()).collect())` ->
  `std::iter::once(receiver).chain(args.iter().copied()).all(promotable)` (458). The children
  are tested in the same order and the first failing one stops the walk, as before.
- `Loans::gather` (lines 471-640), before / after of the allocations:

  ```rust
  impl Loans {
      pub(super) fn gather<'db>(
          ctx: &Ctx<'_, 'db>,
          body: &MirBody<'db>,
          cfg: &Cfg,
          regions: &Regions<'_>,
          unique: impl Fn(LocalId, LocalId, &[PlaceElem]) -> bool,
      ) -> Self {
          let mut statics = static_locals(body);
          ...
          let (promoted, rejected): (Vec<_>, Vec<_>) =
              body.promoted_locals.iter().partition(|&&(local, expr)| { .. });
          let rejected: FxHashSet<hir_def::hir::ExprId> =
              rejected.into_iter().map(|&(_, expr)| expr).collect();
          let borrows_rejected = |expr: hir_def::hir::ExprId| {
              let mut stack = vec![expr];
              ...
          };
          ...
          let mut loans = Loans {
              loans: Vec::new(),
              projections: Vec::new(),
              index: Relation::default(),
              scopes: Relation::default(),
              kills: Relation::default(),
              promoted: promoted_borrows(ctx, body),
          };
          let mut facts = Facts::default();
          let mut two_phase: FxHashMap<LocalId, usize> = FxHashMap::default();
          ...
                      let mut used = Vec::new();
  ```

  ```rust
  impl<'a> Loans<'a> {
      pub(super) fn gather<'db>(
          ctx: &Ctx<'a, 'db>,
          body: &MirBody<'db>,
          cfg: &Cfg<'_>,
          regions: &Regions<'_, '_>,
          unique: impl Fn(LocalId, LocalId, &[PlaceElem]) -> bool,
      ) -> Self {
          let frame = ctx.frame;
          let mut statics = static_locals(frame, body);
          ...
          let promotes = |&(local, expr): &(LocalId, hir_def::hir::ExprId)| {
              item || (!is_call(ctx, expr)
                  && (ctx.is_plain(body.locals[local].ty.as_ref())
                      || plain_value(ctx, body, expr)))
          };
          let mut promoted: FVec<'_, &(LocalId, hir_def::hir::ExprId)> = Vec::new_in(frame);
          let mut rejected = mem::set(frame);
          for it in body.promoted_locals.iter() {
              match promotes(it) {
                  true => promoted.push(it),
                  false => {
                      rejected.insert(it.1);
                  }
              }
          }
          let mut stack: FVec<'_, hir_def::hir::ExprId> = Vec::new_in(frame);
          let mut borrows_rejected = |expr: hir_def::hir::ExprId| {
              stack.clear();
              stack.push(expr);
              while let Some(expr) = stack.pop() {
                  if rejected.contains(&expr) {
                      return true;
                  }
                  ctx.store.walk_child_exprs(expr, |it| stack.push(it));
              }
              false
          };
          statics.extend(
              promoted
                  .iter()
                  .filter(|&&&(_, expr)| rejected.is_empty() || !borrows_rejected(expr))
                  .map(|&&(local, _)| local),
          );
          ...
          let mut loans = Loans {
              loans: Vec::new_in(frame),
              projections: Vec::new_in(frame),
              index: Relation::default(),
              scopes: Relation::default(),
              kills: Relation::default(),
              promoted: promoted_borrows(ctx, body),
          };
          let mut facts = Facts::new(frame);
          let mut two_phase: FMap<'_, LocalId, usize> = mem::map(frame);
          let mut used: FVec<'_, LocalId> = Vec::new_in(frame);
          ...
                      used.clear();
  ```

  `partition` into two vectors and a set from one of them is now one pass into the vector it
  keeps and the set; the order of `promoted` (the order of `body.promoted_locals`) is the
  same, and `rejected` is only looked up. In the two-phase loop, `for local in used` ->
  `for &local in used.iter()` (`used` is reused per point instead of made per point).
- `find_kills` (line 643): `fn find_kills<'db>(&self, ctx: &Ctx<'a, 'db>, body: &MirBody<'db>,
  cfg: &Cfg<'_>) -> Relation<'a>`, `let mut facts = Facts::new(ctx.frame);`.
- `projection`, `made_at`, `of_local`, `activated_at`: `of_local` and `activated_at` return
  `&'a [u32]` (`Relation::get` is `&'f` now), which lets `kill_on_place` iterate them while
  calling `kill`.
- `compute_scopes` (lines 732-744) and `compute_polonius_scopes` (748-771): `out_of_scope`
  per loan made a `BitSet` of the blocks, a work list and a `SmallVec` of ends: per loan, a
  zeroed bitset of the whole body. They now share one of each for all loans, the bitset
  cleared by the blocks the walk entered (kept in `entered_list`), not by the whole width.
  Before:

  ```rust
      pub(super) fn compute_scopes(&mut self, body: &MirBody<'_>, cfg: &Cfg, values: &RegionValues) {
          let mut facts = Facts::default();
          for id in 0..self.loans.len() {
              let points = values.points_of(self.loans[id].region);
              let ends = out_of_scope(body, cfg, self.loans[id].location, |start, end| {
                  points.first_absent_in(start, end)
              });
              for point in ends {
                  facts.push(ENDS_AT, point, id);
              }
          }
          self.scopes = facts.freeze(&[cfg.points]);
      }
  ```

  after:

  ```rust
      pub(super) fn compute_scopes(
          &mut self,
          ctx: &Ctx<'a, '_>,
          body: &MirBody<'_>,
          cfg: &Cfg<'_>,
          values: &RegionValues<'_>,
      ) {
          let mut facts = Facts::new(ctx.frame);
          let mut walk = ScopeWalk::new(ctx.frame, body.basic_blocks.len());
          for id in 0..self.loans.len() {
              let points = values.points_of(self.loans[id].region);
              walk.out_of_scope(
                  body,
                  cfg,
                  self.loans[id].location,
                  |start, end| points.first_absent_in(start, end),
                  |point| facts.push(ENDS_AT, point, id),
              );
          }
          self.scopes = facts.freeze(&[cfg.points]);
      }
  ```

  `compute_polonius_scopes` the same: `(&mut self, ctx: &Ctx<'a, '_>, body, cfg: &Cfg<'_>,
  live: &[IntervalSet<'_>])`, `Facts::new(ctx.frame)`, one `ScopeWalk`, its `first_absent`
  closure unchanged and `|point| facts.push(ENDS_AT, point, id)` for the ends.

  `out_of_scope` (lines 844-882) becomes `ScopeWalk::out_of_scope`; before:

  ```rust
  fn out_of_scope(
      body: &MirBody<'_>,
      cfg: &Cfg,
      issued: Location,
      mut first_absent: impl FnMut(usize, usize) -> Option<usize>,
  ) -> SmallVec<[usize; 2]> {
      let mut out = SmallVec::new();
      let last = |block: BasicBlockId| { .. };
      let mut entered = BitSet::new(body.basic_blocks.len());
      let mut work: Vec<BasicBlockId> = Vec::new();
      ...
          Some(point) => out.push(point),
      ...
      while let Some(block) = work.pop() {
          if !entered.insert(index(block)) {
              continue;
          }
          ...
              Some(point) => out.push(point),
      ...
      out
  }
  ```

  after:

  ```rust
  /// What [`ScopeWalk::out_of_scope`] keeps from one loan to the next: the blocks a walk
  /// entered, as a set and as a list to clear it by.
  struct ScopeWalk<'f> {
      entered: BitSet<'f>,
      entered_list: FVec<'f, BasicBlockId>,
      work: FVec<'f, BasicBlockId>,
  }

  impl<'f> ScopeWalk<'f> {
      fn new(frame: &'f Frame, blocks: usize) -> Self {
          ScopeWalk {
              entered: BitSet::new(frame, blocks),
              entered_list: Vec::new_in(frame),
              work: Vec::new_in(frame),
          }
      }

      /// Gives `end` the first points reachable from just past `issued` that are not in
      /// scope, given `first_absent(start, end)`, the first point of `start..=end` not in
      /// scope. It walks whole blocks: a block's points are consecutive, so one lookup finds
      /// where a loan ends in it.
      fn out_of_scope(
          &mut self,
          body: &MirBody<'_>,
          cfg: &Cfg<'_>,
          issued: Location,
          mut first_absent: impl FnMut(usize, usize) -> Option<usize>,
          mut end_at: impl FnMut(usize),
      ) {
          for block in self.entered_list.drain(..) {
              self.entered.remove(index(block));
          }
          self.work.clear();
          let work = &mut self.work;
          let last = |block: BasicBlockId| { .. (unchanged) };
          ... (unchanged, with `out.push(point)` -> `end_at(point)`)
          while let Some(block) = work.pop() {
              if !self.entered.insert(index(block)) {
                  continue;
              }
              self.entered_list.push(block);
              ...
          }
      }
  }
  ```

  The points go to `end_at` in the order they were pushed to `out`, so the facts are pushed in
  the same order. `smallvec::SmallVec` (line 12) goes from the imports.
- `entry_sets` (lines 774-791): `pub(super) fn entry_sets(&self, ctx: &Ctx<'a, '_>, body:
  &MirBody<'_>, cfg: &Cfg<'_>) -> BitMatrix<'a>`; `BitMatrix::new(ctx.frame, blocks, 0)`,
  `GenKill::new(ctx.frame, blocks, count)`, `forward(ctx.frame, cfg, &transfer,
  body.start_block, &BitSet::new(ctx.frame, count), |_, _, _| ())`.
- `before`, `after` (lines 794-810): `cfg: &Cfg<'_>`.
- `places_conflict` (line 896): `loans: &Loans<'_>`.
- Imports: line 10 `use rustc_hash::{FxHashMap, FxHashSet};` ->
  `use crate::mem::{self, FMap, FSet, FVec, Frame};`.

> forwarded to db (from requests, pass 16; told by tmux).

> applied by frontend-fast-borrowck-facts: loans.rs on the frame as above, with these
> differences. `Loans::gather(ctx, body, &cfg, &decoded, &regions, unique)` reads the decode's
> borrows, constants, temporaries' definitions, writes, `StorageDead`s and two-phase reads
> instead of the statements (same order: reverse postorder, statement order, operand order);
> `static_locals(frame, &decoded)`, `extended_temporaries`, `const_promoted`, `certain_blocks`
> and `promoted_borrows` take `&decoded` too (`promoted_borrows`' `defs` map is the decode's
> `temp_of`). `Loans` keeps beside each loan's projection the `PlaceId` of each prefix
> (`prefixes`), so `ignored` and `places_conflict` read the place-type column instead of
> projecting again. `entry_sets` returns `rows::Rows<'a>` (`Dense` of no words when there is no
> loan) from `dataflow::forward` over `rows::Effects`. `ScopeWalk`, `promotes`, `plain_value`
> and `const_promotable` as above.

### B5.3 `moves.rs`

Per body: the move-path tree's six columns, its map of steps, the moves and inits, the
preorder columns and the facts.

- `MoveData` (lines 90-104): `pub(super) struct MoveData<'f> { local: FVec<'f, LocalId>,
  parent: FVec<'f, u32>, end: FVec<'f, u32>, projections: Csr<'f, PlaceElem>, locals: &'f [u32],
  steps: FMap<'f, (u32, PlaceElem), u32>, pub(super) moves: FVec<'f, MoveOut>, pub(super) inits:
  FVec<'f, Init>, facts: Relation<'f> }`.
- `Tree` (lines 108-121): `struct Tree<'f>` with each `Vec` an `FVec<'f, _>` and `steps:
  FMap<'f, (u32, PlaceElem), u32>`.
- `MoveData::gather` (lines 124-138), before:

  ```rust
  impl MoveData {
      pub(super) fn gather<'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, cfg: &Cfg) -> Self {
          let roots = body.locals.len();
          let mut data = Tree {
              local: Vec::with_capacity(roots * 2),
              parent: Vec::with_capacity(roots * 2),
              depth: Vec::with_capacity(roots * 2),
              first_child: Vec::with_capacity(roots * 2),
              last_child: Vec::with_capacity(roots * 2),
              next_sibling: Vec::with_capacity(roots * 2),
              step: Vec::with_capacity(roots),
              roots,
              steps: FxHashMap::default(),
              moves: Vec::with_capacity(cfg.points / 2),
              inits: Vec::with_capacity(cfg.points / 2),
          };
  ```

  after:

  ```rust
  impl<'f> MoveData<'f> {
      pub(super) fn gather<'db>(ctx: &Ctx<'f, 'db>, body: &MirBody<'db>, cfg: &Cfg<'_>) -> Self {
          let frame = ctx.frame;
          let roots = body.locals.len();
          let mut data = Tree {
              local: Vec::with_capacity_in(roots * 2, frame),
              parent: Vec::with_capacity_in(roots * 2, frame),
              depth: Vec::with_capacity_in(roots * 2, frame),
              first_child: Vec::with_capacity_in(roots * 2, frame),
              last_child: Vec::with_capacity_in(roots * 2, frame),
              next_sibling: Vec::with_capacity_in(roots * 2, frame),
              step: Vec::with_capacity_in(roots, frame),
              roots,
              steps: mem::map(frame),
              moves: Vec::with_capacity_in(cfg.points / 2, frame),
              inits: Vec::with_capacity_in(cfg.points / 2, frame),
          };
  ```

  (seven columns grown in turn: once the tree passes `roots * 2` paths each grows by a copy in
  the frame, as each is not the frame's last allocation; the copies are bounded by the final
  sizes, as with doubling on the global heap.)
- `first_in` (line 258): `set: &BitSet<'_>`.
- `Tree::freeze` (lines 439-513), before:

  ```rust
      fn freeze(self, cfg: &Cfg) -> MoveData {
          let count = self.local.len();
          let mut order: Vec<u32> = Vec::with_capacity(count);
          let mut renumbered = vec![NONE; count];
          ...
          let mut local = Vec::with_capacity(count);
          let mut parent = Vec::with_capacity(count);
          let mut end: Vec<u32> = (1..=count as u32).collect();
          let mut projections = Csr::with_capacity(count, depths);
          ...
          let mut steps = FxHashMap::with_capacity_and_hasher(count - self.roots, Default::default());
          ...
          let locals = renumbered[..self.roots].to_vec();
          ...
          let mut facts = Facts::with_capacity(moves.len() * 2 + inits.len());
  ```

  after:

  ```rust
      fn freeze(self, cfg: &Cfg<'_>) -> MoveData<'f> {
          let frame = *self.local.allocator();
          let count = self.local.len();
          let mut order: FVec<'_, u32> = Vec::with_capacity_in(count, frame);
          let renumbered = frame.alloc_slice_fill_with(count, |_| NONE);
          ...
          let mut local = Vec::with_capacity_in(count, frame);
          let mut parent = Vec::with_capacity_in(count, frame);
          let mut end = mem::collect(frame, 1..=count as u32);
          let mut projections = Csr::with_capacity(frame, count, depths);
          ...
          let mut steps = mem::map(frame);
          steps.reserve(count - self.roots);
          ...
          let locals = &renumbered[..self.roots];
          ...
          let mut facts = Facts::with_capacity(frame, moves.len() * 2 + inits.len());
  ```

  (`locals` was a copy of the first `roots` entries of `renumbered`; it is that slice of the
  frame slice now, which lives as long as the frame.) `impl Tree<'f>` (the `add`, `child`,
  `find`, `gather_*` methods) unchanged but for its header, `impl<'f> Tree<'f>`.
- `InitFlow` (lines 559-637): `pub(super) struct InitFlow<'f> { pub(super) uninit_entry:
  BitMatrix<'f>, pub(super) ever_entry: BitMatrix<'f> }`;
  `pub(super) fn compute(frame: &'f Frame, body: &MirBody<'_>, cfg: &Cfg<'_>, moves:
  &MoveData<'_>) -> Self` in `impl<'f> InitFlow<'f>`; `GenKill::new(frame, ..)` four times,
  `let mut returns_to = filled(frame, blocks, None::<(BasicBlockId, usize)>);`,
  `BitSet::filled(frame, paths)`, `BitSet::new(frame, locals)`, `forward(frame, cfg, ..)` twice.
- `apply`, `call_returned` (lines 643-688): `moves: &MoveData<'_>`, `cfg: &Cfg<'_>`.
- Imports: line 19 `use rustc_hash::FxHashMap;` -> `use crate::mem::{self, FMap, FVec, Frame,
  filled};`.

> forwarded to db (from requests, pass 16; told by tmux).

> applied by frontend-fast-borrowck-facts: moves.rs on the frame as above, with these
> differences. `MoveData::gather(ctx, body, &cfg, &decoded)` reads the decode's moves, writes
> and `StorageDead`s, a point's moves then its `StorageDead` then its writes (rustc's move
> numbering); whether an operand moves (`is_copy`, `moves_out_of_guard`, the cast) is asked here
> as before. The tree steps through `ctx.places` (parent, element, type per `PlaceId`) with a
> memo of each place's `Lookup`, kept after `freeze` for `find`, which goes through
> `ctx.place(local, projection)`. `MoveData` also keeps the decode's `dead`, `returns`,
> `terminated`. `InitFlow<'f>`'s rows are `rows::Rows<'f>`, from `dataflow::forward` over
> `rows::Effects`, a call's return an `Edge::Effect` of a row per call; `returns_to` is
> replaced by a per-block row index (`call_of`). `freeze` takes `frame`, keeps `renumbered` a
> frame vector and leaks it for `locals`.

### B5.4 `polonius.rs`

Per body in `Polonius` mode: a direction per region, two CSRs over the constraints, and per
loan a walk with a visited set per region it reaches.

- `live_loans` (lines 33-202), before / after of the allocations:

  ```rust
  pub(super) fn live_loans<'db>(
      ctx: &Ctx<'_, 'db>,
      body: &MirBody<'db>,
      cfg: &Cfg,
      regions: &Regions<'db>,
      count: usize,
      liveness: &[IntervalSet],
      loans: &Loans,
  ) -> Vec<IntervalSet> {
      let mut directions: Vec<Option<Direction>> = vec![None; count];
      ...
      let mut universal = BitSet::new(count);
      ...
      let all = {
          let mut all = IntervalSet::default();
          ...
      };
      let empty = IntervalSet::default();
      ...
      let mut localized: Csr<(u32, u32)> = Csr::with_capacity(count, constraints.len());
      let mut logical: Csr = Csr::with_capacity(count, constraints.len());
      ...
      let mut live = Vec::with_capacity(loans.loans.len());
      let mut visited: Vec<IntervalSet> = vec![IntervalSet::default(); count];
      let mut touched: Vec<Rv> = Vec::new();
      let mut stack: Vec<(Rv, usize, usize)> = Vec::new();
      for loan in &loans.loans {
          let mut points = IntervalSet::default();
          ...
              for (start, end) in visited[region].insert_new(start, end) {
  ```

  ```rust
  pub(super) fn live_loans<'a, 'db>(
      ctx: &Ctx<'a, 'db>,
      body: &MirBody<'db>,
      cfg: &Cfg<'_>,
      regions: &Regions<'_, 'db>,
      count: usize,
      liveness: &[IntervalSet<'_>],
      loans: &Loans<'_>,
  ) -> FVec<'a, IntervalSet<'a>> {
      let frame = ctx.frame;
      let mut directions = filled(frame, count, None::<Direction>);
      ...
      let mut universal = BitSet::new(frame, count);
      ...
      let all = {
          let mut all = IntervalSet::new(frame);
          ...
      };
      let empty = IntervalSet::new(frame);
      ...
      let mut localized: Csr<'_, (u32, u32)> = Csr::with_capacity(frame, count, constraints.len());
      let mut logical: Csr<'_> = Csr::with_capacity(frame, count, constraints.len());
      ...
      let mut live = Vec::with_capacity_in(loans.loans.len(), frame);
      let mut visited = filled(frame, count, IntervalSet::new(frame));
      let mut touched: FVec<'_, Rv> = Vec::new_in(frame);
      let mut stack: FVec<'_, (Rv, usize, usize)> = Vec::new_in(frame);
      let mut runs: FVec<'_, (usize, usize)> = Vec::new_in(frame);
      for loan in &loans.loans {
          let mut points = IntervalSet::new(frame);
          ...
              visited[region].insert_new(start, end, &mut runs);
              for &(start, end) in runs.iter() {
  ```

  `visited[rv].clear()` keeps each region's room for the next loan, as it did.
- `record_variances` and the functions under it (lines 205-350): `directions: &mut
  FVec<'_, Option<Direction>>` for `&mut Vec<Option<Direction>>`.
- Imports: add `use crate::mem::{FVec, filled};`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by frontend-fast-borrowck-frame: as written; `for loan in loans.loans.iter()`; also the opaque captures' variances (`record_variances`) are collected into `ctx.frame` rather than a global `Vec`.

### B5.5 `inference.rs`

Per body: the SCCs (Tarjan's five arrays), a value and a row of universals per SCC, the
members CSR.

- `RegionValues` (lines 15-19): `pub(super) struct RegionValues<'f> { scc: FVec<'f, u32>,
  points: FVec<'f, IntervalSet<'f>>, universals: BitMatrix<'f> }`, `impl<'f> RegionValues<'f>`,
  `points_of(&self, rv) -> &IntervalSet<'f>`.
- `infer` (lines 38-88), before / after:

  ```rust
  pub(super) fn infer(
      regions: &Regions<'_>,
      count: usize,
      points: usize,
      live: &[IntervalSet],
  ) -> RegionValues {
      let mut edges = Csr::with_capacity(count, regions.outlives.len());
      ...
      let (scc, sccs) = strongly_connected(&edges);
      let mut values = vec![IntervalSet::default(); sccs];
      let mut universals = BitMatrix::new(sccs, regions.universals.len());
      ...
      let members = Csr::build(sccs, scc.iter().enumerate().map(|(rv, &at)| (at, rv as u32)));
      let mut seen = vec![u32::MAX; sccs];
  ```

  ```rust
  pub(super) fn infer<'f>(
      frame: &'f Frame,
      regions: &Regions<'_, '_>,
      count: usize,
      points: usize,
      live: &[IntervalSet<'_>],
  ) -> RegionValues<'f> {
      let mut edges = Csr::with_capacity(frame, count, regions.outlives.len());
      ...
      let (scc, sccs) = strongly_connected(frame, &edges);
      let mut values = filled(frame, sccs, IntervalSet::new(frame));
      let mut universals = BitMatrix::new(frame, sccs, regions.universals.len());
      ...
      let members =
          Csr::build(frame, sccs, scc.iter().enumerate().map(|(rv, &at)| (at, rv as u32)));
      let seen = frame.alloc_slice_fill_with(sccs, |_| u32::MAX);
  ```
- `strongly_connected` (lines 93-150), before:

  ```rust
  fn strongly_connected(edges: &Csr) -> (Vec<u32>, usize) {
      let count = edges.nodes();
      const NONE: u32 = u32::MAX;
      let mut index = vec![NONE; count];
      let mut low = vec![0u32; count];
      let mut on_stack = vec![false; count];
      let mut scc = vec![NONE; count];
      let mut stack: Vec<u32> = Vec::new();
      let mut next_index = 0u32;
      let mut sccs = 0u32;
      let mut calls: Vec<(u32, usize)> = Vec::new();
  ```

  after (each node is on each stack at most once, so `count` bounds both and neither grows):

  ```rust
  fn strongly_connected<'f>(frame: &'f Frame, edges: &Csr<'_>) -> (FVec<'f, u32>, usize) {
      let count = edges.nodes();
      const NONE: u32 = u32::MAX;
      let index = frame.alloc_slice_fill_with(count, |_| NONE);
      let low = frame.alloc_slice_fill_with(count, |_| 0u32);
      let mut on_stack = BitSet::new(frame, count);
      let mut scc = filled(frame, count, NONE);
      let mut stack: FVec<'_, u32> = Vec::with_capacity_in(count, frame);
      let mut next_index = 0u32;
      let mut sccs = 0u32;
      let mut calls: FVec<'_, (u32, usize)> = Vec::with_capacity_in(count, frame);
  ```

  with `on_stack[root as usize] = true;` -> `on_stack.insert(root as usize);`,
  `on_stack[t] = true;` -> `on_stack.insert(t);`, `else if on_stack[t]` ->
  `else if on_stack.contains(t)`, `on_stack[member as usize] = false;` ->
  `on_stack.remove(member as usize);` (a bit per node, 1/8 of the bytes, the same answers).
- Imports: `use super::bitset::BitSet; use crate::mem::{FVec, Frame, filled};`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-borrowck (21:36): as written, on -frame's `BitSet`/`BitMatrix`/`Csr` (`Csr::build(frame, ..)`, `Csr::with_capacity(frame, ..)`). Callers in regions.rs (B6) pass the frame.

### B5.6 `constraints.rs`

Per body: six columns of the constraints, and the graph by `sup`, rebuilt after constraints
are added once it was asked (regions.rs asks it while it still adds).

Lines 22-52, before / after:

```rust
#[derive(Default)]
pub(super) struct Constraints {
    category: Vec<Category>,
    sup: Vec<u32>,
    sub: Vec<u32>,
    location: Vec<[u32; 2]>,
    span: Vec<MirSpan>,
    from_closure: Vec<u64>,
    by_sup: OnceCell<Csr>,
}

impl Constraints {
    pub(super) fn push(&mut self, it: Outlives) {
        ...
        self.by_sup.take();
    }
```

```rust
pub(super) struct Constraints<'f> {
    category: FVec<'f, Category>,
    sup: FVec<'f, u32>,
    sub: FVec<'f, u32>,
    location: FVec<'f, [u32; 2]>,
    span: FVec<'f, MirSpan>,
    from_closure: FVec<'f, u64>,
    by_sup: OnceCell<Csr<'f>>,
    /// The graph last built, whose arrays the next build reuses.
    spare: Cell<Option<Csr<'f>>>,
}

impl<'f> Constraints<'f> {
    pub(super) fn new(frame: &'f Frame) -> Self {
        Constraints {
            category: Vec::new_in(frame),
            sup: Vec::new_in(frame),
            sub: Vec::new_in(frame),
            location: Vec::new_in(frame),
            span: Vec::new_in(frame),
            from_closure: Vec::new_in(frame),
            by_sup: OnceCell::new(),
            spare: Cell::new(None),
        }
    }

    pub(super) fn push(&mut self, it: Outlives) {
        ...
        if let Some(graph) = self.by_sup.take() {
            *self.spare.get_mut() = Some(graph);
        }
    }
```

`of_sup` (lines 96-103), before / after:

```rust
    pub(super) fn of_sup(&self, sup: Rv) -> &[u32] {
        self.by_sup
            .get_or_init(|| {
                let regions = self.sup.iter().map(|&it| it as usize + 1).max().unwrap_or(0);
                Csr::build(regions, (0..self.len()).map(|at| (self.sup[at], at as u32)))
            })
            .of(sup)
    }
```

```rust
    pub(super) fn of_sup(&self, sup: Rv) -> &[u32] {
        self.by_sup
            .get_or_init(|| {
                let regions = self.sup.iter().map(|&it| it as usize + 1).max().unwrap_or(0);
                let edges = (0..self.len()).map(|at| (self.sup[at], at as u32));
                match self.spare.take() {
                    Some(mut graph) => {
                        graph.rebuild(regions, edges);
                        graph
                    }
                    None => Csr::build(*self.sup.allocator(), regions, edges),
                }
            })
            .of(sup)
    }
```

`impl Extend<Outlives> for Constraints<'_>`, `pub(super) struct Iter<'a> { constraints: &'a
Constraints<'a>, .. }` -> `Iter<'a, 'f> { constraints: &'a Constraints<'f>, .. }` with `impl
Iterator for Iter<'_, '_>`, `impl<'a, 'f> IntoIterator for &'a Constraints<'f> { type IntoIter =
Iter<'a, 'f>; .. }`, `iter(&self) -> Iter<'_, 'f>`. Line 6 `use std::cell::OnceCell;` ->
`use std::cell::{Cell, OnceCell};`, and `use crate::mem::{FVec, Frame};`. Test: in
`frame::task(Tag::Mir, |frame| ..)`, `Constraints::default()` -> `Constraints::new(frame)`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-borrowck (21:36): as written; `of_sup` rebuilds the spare graph with -frame's `Csr::rebuild(&mut self, nodes, pairs)` and builds a new one with `Csr::build(*self.sup.allocator(), ..)`. The test's body moved into a helper `a_constraint_reads_back_in(frame)` that the `frame::task` calls.

### B5.7 `scan.rs`

Once per owner, in the owner's frame (B4.2).

- `Scan<'db>` (lines 26-59) -> `Scan<'f, 'db>`: `matched: BitSet<'f>`, `tested_at: FVec<'f,
  u32>`, `union_literals: BitSet<'f>`, `guarded: GuardedMatches<'f>`, `scrutinees: FMap<'f,
  BindingId, Option<ExprId>>`, `guard_bindings: FMap<'f, ExprId, FVec<'f, BindingId>>`,
  `statement_values: FMap<'f, ExprId, Option<Ty<'db>>>`, `lets: FVec<'f, Annotation>`,
  `spelled_patterns: FVec<'f, (PatId, Option<ExprId>)>`, `spelled_paths: FVec<'f, (ExprId,
  Option<ExprId>)>`.
- `GuardedMatches` (lines 63-74): `pub(crate) struct GuardedMatches<'f>`, no `Default`,
  `tested_by: FMap<'f, PatId, usize>`, `bound_by: FMap<'f, PatId, (usize, Option<usize>)>`,
  `guards: FVec<'f, (usize, ExprId)>`.
- `Scan::new` (lines 96-257), before / after of the allocations:

  ```rust
  impl<'db> Scan<'db> {
      pub(crate) fn new(
          store: &ExpressionStore,
          infer: &InferenceResult<'db>,
          interner: DbInterner<'db>,
      ) -> Self {
          ...
          let mut matched = BitSet::new(count);
          let mut union_literals = BitSet::new(count);
          let mut tested_at = Vec::new();
          let mut guarded = GuardedMatches::default();
          let mut scrutinees = FxHashMap::default();
          let mut guard_bindings: FxHashMap<ExprId, Vec<BindingId>> = FxHashMap::default();
          let mut statement_values = FxHashMap::default();
          let (mut lets, mut spelled_patterns, mut spelled_paths) =
              (Vec::new(), Vec::new(), Vec::new());
          let (mut bindings, mut stack) = (Vec::new(), Vec::new());
          ...
                                  guard_bindings.entry(expr).or_default().extend(&bindings);
          ...
              let mut parent = vec![NONE; count];
  ```

  ```rust
  impl<'f, 'db> Scan<'f, 'db> {
      pub(crate) fn new(
          frame: &'f Frame,
          store: &ExpressionStore,
          infer: &InferenceResult<'db>,
          interner: DbInterner<'db>,
      ) -> Self {
          ...
          let mut matched = BitSet::new(frame, count);
          let mut union_literals = BitSet::new(frame, count);
          let mut tested_at = Vec::new_in(frame);
          let mut guarded = GuardedMatches {
              matches: 0,
              tested_by: mem::map(frame),
              bound_by: mem::map(frame),
              guards: Vec::new_in(frame),
          };
          let mut scrutinees = mem::map(frame);
          let mut guard_bindings: FMap<'f, ExprId, FVec<'f, BindingId>> = mem::map(frame);
          let mut statement_values = mem::map(frame);
          let (mut lets, mut spelled_patterns, mut spelled_paths) =
              (Vec::new_in(frame), Vec::new_in(frame), Vec::new_in(frame));
          let mut bindings: FVec<'_, BindingId> = Vec::new_in(frame);
          let mut stack: FVec<'_, ExprId> = Vec::new_in(frame);
          ...
                                  guard_bindings
                                      .entry(expr)
                                      .or_insert_with(|| Vec::new_in(frame))
                                      .extend_from_slice(&bindings);
          ...
              let parent = frame.alloc_slice_fill_with(count, |_| NONE);
  ```

  `guard_bindings(&self, expr) -> &[BindingId]`: `.map_or(&[], Vec::as_slice)` ->
  `.map_or(&[], |it| it.as_slice())`. The maps are only looked up, never iterated in the
  checks: order reaches nothing.
- `scan/tests.rs` `check` (lines 272-348): its body in `blaze_mem::frame::task(blaze_mem::Tag::Mir,
  |frame| { .. })`, `Scan::new(frame, store, infer, interner)`; the map comparisons
  (`scan.guarded.tested_by == tested_by`, `bound_by`, `scrutinees`, `guard_bindings`,
  `statement_values`, an `FMap` against the oracle's `FxHashMap`) through:

  ```rust
  fn same<K: Eq + std::hash::Hash, V: PartialEq, W>(
      got: &crate::mem::FMap<'_, K, V>,
      want: &FxHashMap<K, W>,
      eq: impl Fn(&V, &W) -> bool,
  ) -> bool {
      got.len() == want.len() && want.iter().all(|(k, w)| got.get(k).is_some_and(|v| eq(v, w)))
  }
  ```

  as `assert!(same(&scan.guarded.tested_by, &tested_by, PartialEq::eq), "{def:?}: tested_by");`
  and, for `guard_bindings`, `|got, want: &Vec<BindingId>| got.as_slice() == want.as_slice()`.
  The vector comparisons (`scan.lets == want`) hold across allocators as they are.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-borrowck (21:36): as written, with B0's `pub(crate) mod tests;` in scan.rs and `pub(crate) mod db;` in scan/tests.rs. `check`'s body is a helper `check_in(frame, db, def, seen)` that the `frame::task` calls.

### B5.8 `qualifs.rs`

- `ValueCx` (line 63): `pub(crate) consts: &'a RefCell<FMap<'a, ConstId, Qualifs>>`; line 17
  `use rustc_hash::FxHashMap;` -> `use crate::mem::FMap;`. Its other builder (line 258) passes
  `self.consts` through.
- `value`'s record literal (line 124), per record literal of a constant: `let parts: Vec<_> =
  fields.iter().map(|it| it.expr).collect(); self.aggregate(ty, &parts, depth)` ->
  `self.aggregate(ty, fields.iter().map(|it| it.expr), depth)`; line 111 `self.aggregate(ty,
  &[], depth)` -> `self.aggregate(ty, std::iter::empty(), depth)`; line 131
  `self.aggregate(ty, args, depth)` -> `self.aggregate(ty, args.iter().copied(), depth)`.
  `aggregate` (lines 159-179), before:

  ```rust
      fn aggregate(&self, ty: Ty<'db>, parts: &[ExprId], depth: usize) -> Qualifs {
          let parts =
              || parts.iter().fold(Qualifs::default(), |acc, &it| acc.or(self.value(it, depth + 1)));
  ```

  after:

  ```rust
      fn aggregate(
          &self,
          ty: Ty<'db>,
          parts: impl Iterator<Item = ExprId>,
          depth: usize,
      ) -> Qualifs {
          let parts =
              || parts.fold(Qualifs::default(), |acc, it| acc.or(self.value(it, depth + 1)));
  ```

  (`parts` is called once on each of the three paths that call it, each returning, so an
  `FnOnce` serves.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-borrowck (21:36): as written. The map's other maker, loans.rs line 164 `consts: &Default::default()`, has no `Default` for an `FMap`: it is B5.3's (frontend-fast-borrowck), told.

### B5.9 `timings.rs`

Nothing per run: its thread-local `RefCell<Option<Timings>>` is a measurement's, set up by the
caller that asks for timings.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-borrowck (21:36): nothing to change, as it says.

## W. `crates/hir-ty-wfcheck/`

Tag `Infer`. `wfcheck` and `inferred_outlives` are queries; each opens a `frame::task` of
`Tag::Infer` for its run, as borrowck's do (B4.1 says why `task` and not `current`), and the
code under them takes it through the `Check` and `Wfcx` that hold it.

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:33): W0-W4 applied below by db-mem-borrowck.

### W0. Setup

- `Cargo.toml` `[dependencies]`: `blaze-mem.workspace = true` and `hashbrown.workspace = true`
  after `base-db.workspace = true` / with the others in order; `[dev-dependencies]`:
  `blaze-alloc-count.workspace = true`.
- `src/lib.rs` after `#![forbid(unsafe_code)]`: `#![feature(allocator_api)]`; after
  `mod compare;`: `mod mem;`; and

  ```rust
  #[cfg(test)]
  mod budget;

  #[cfg(test)]
  #[global_allocator]
  static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
  ```
- `src/mem.rs` (new): B1's file with `FVec`, `FMap`, `FSet`, `map`, `set`, `collect`, `owned`
  and its test, without `FDeque`, `filled` and `ExactLen` (unused here).
- `src/outlives.rs` line 6 `mod tests;` -> `pub(crate) mod tests;`, and `outlives/tests.rs`
  line 5 `mod db;` -> `pub(crate) mod db;` (the budget tests use its `TestDB`).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-borrowck (2026-10-29 21:40): as written; `hashbrown` first in
> `[dependencies]`, `blaze-mem` after `blaze-format`. `mem.rs`'s test makes its 70-item frame
> vector with `collect(frame, repeat_n(7, 70))`, as `filled` is left out, and runs under `Tag::Infer`.

### W1. `lib.rs`

`wfcheck` (lines 97-111), before:

```rust
#[salsa::tracked(returns(ref))]
pub fn wfcheck(db: &Db, def: GenericDefId) -> WfResult {
    let mut result = WfResult::default();
    if off() {
        return result;
    }
    let mut check = Check { db, inferred: Inferred::default(), found: &mut result.diagnostics };
    match def {
        ...
    }
    result
}
```

after:

```rust
#[salsa::tracked(returns(ref))]
pub fn wfcheck(db: &Db, def: GenericDefId) -> WfResult {
    check_item(db, def)
}

/// [`wfcheck`] of `def`, not cached: what it reads is asked through the database as before.
fn check_item(db: &Db, def: GenericDefId) -> WfResult {
    if off() {
        return WfResult::default();
    }
    blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
        let mut found = Vec::new_in(frame);
        let mut check = Check { db, frame, inferred: Inferred::default(), found: &mut found };
        match def {
            ... (unchanged)
        }
        WfResult { diagnostics: mem::owned(found.drain(..)) }
    })
}
```

`Check` (lines 121-125): `struct Check<'a, 'f, 'db> { db: &'db Db, frame: &'f Frame, inferred:
Inferred<'db>, found: &'a mut FVec<'f, WfDiagnostic> }`, `impl<'f, 'db> Check<'_, 'f, 'db>`.

Per item, in `impl Check`, each before -> after (`Needs`, `Site` pairs are `FVec<'f, (Needs<'db>,
Site)>` throughout):

| line | before | after |
|---|---|---|
| 135 | `Known::new(self.clauses(adt.into()))` | `Known::new(self.frame, self.clauses(adt.into()))` |
| 146 | `let mut needs = Vec::new();` | `let mut needs = Vec::new_in(self.frame);` |
| 150 | `let mut found = Vec::new();` | `let mut found = Vec::new_in(self.frame);` |
| 173 | `Wfcx::new(self.db, adt.into())` | `Wfcx::new(self.frame, self.db, adt.into())` |
| 184 | `fn header(&self, impl_: ImplId) -> Option<Vec<Ty<'db>>>` | `-> Option<FVec<'f, Ty<'db>>>` |
| 186 | `let mut header = vec![self_ty];` | `let mut header = Vec::new_in(self.frame); header.push(self_ty);` |
| 203 | `fn plain_header(..) -> Option<Vec<Ty<'db>>>` | `-> Option<FVec<'f, Ty<'db>>>` |
| 208 | `fn known(&mut self, def, assumed: &[Ty<'db>]) -> Known<'db>` | `-> Known<'f, 'db>` |
| 209 | `Known::new(self.clauses(def))` | `Known::new(self.frame, self.clauses(def))` |
| 210 | `let mut implied = Vec::new();` | `let mut implied = Vec::new_in(self.frame);` |
| 224 | `Wfcx::new(db, impl_.into())` | `Wfcx::new(self.frame, db, impl_.into())` |
| 225, 238, 266 | `let mut found = Vec::new();` | `let mut found = Vec::new_in(self.frame);` |
| 268-269 | `let mut needs: Vec<_> = found.into_iter().map(..).collect();` | `let mut needs = mem::collect(self.frame, found.into_iter().map(\|it\| (it, Site::AssocType(alias))));` |
| 313 | `let inputs: Vec<Ty<'db>> = sig..collect();` | `let inputs = mem::collect(self.frame, sig..);` (the same chain) |
| 317 | `Wfcx::new(db, function.into())` | `Wfcx::new(self.frame, db, function.into())` |
| 320-321 | `let mut assumed: Vec<Ty<'db>> = inputs.iter().map(\|&ty\| wfcx.normalize(ty, &mut Vec::new())).collect();` | `let mut dropped = Vec::new_in(self.frame);` then `let mut assumed = mem::collect(self.frame, inputs.iter().map(\|&ty\| { dropped.clear(); wfcx.normalize(ty, &mut dropped) }));` |
| 324 | `wfcx.normalize(ty, &mut Vec::new())` | `{ dropped.clear(); wfcx.normalize(ty, &mut dropped) }` |
| 344 | `wfcx.normalize(output, &mut Vec::new())` | `{ dropped.clear(); wfcx.normalize(output, &mut dropped) }` |
| 355 | `let mut found = Vec::new();` (per input) | hoisted above the `for`: `let mut found = Vec::new_in(self.frame);`, and `found.clear();` first in the loop body, `needs.extend(found.drain(..).map(\|it\| (it, site)));` for line 363 |
| 368 | `elided_output(db, function, &inputs)` | `elided_output(self.frame, db, function, &inputs)` |
| 382 | `known: &Known<'db>` | `known: &Known<'f, 'db>` (also `trait_opaques` 437, `resolve` 607) |
| 388 | `let hidden: Vec<_> = infer.return_position_impl_trait_types(db).collect();` | `let hidden = mem::collect(self.frame, infer.return_position_impl_trait_types(db));` |
| 389, 440 | `let mut opaques = Vec::new();` | `let mut opaques = Vec::new_in(self.frame);` |
| 405-406 | `let mut needs = Vec::new(); let mut found = Vec::new();` (per opaque) | hoisted above `for alias in opaques`: one `found` (`Vec::new_in(self.frame)`), `found.clear()` per opaque; `needs` per opaque as `mem::collect(self.frame, found.drain(..).map(\|it\| (it, site)))` for 426 |
| 450 | `let mut found = Vec::new();` (per opaque) | hoisted, `found.clear()` per opaque |
| 475-482 | `let needs = found.into_iter().map(..).map(..).collect();` | `let needs = mem::collect(self.frame, found.drain(..).map(..).map(..));` (the same closures) |
| 489 | `fn where_needs(&mut self, def) -> Vec<(Needs<'db>, Site)>` | `-> FVec<'f, (Needs<'db>, Site)>` |
| 494-496 | `Vec::new()` three times | `Vec::new_in(self.frame)` three times |
| 607 | `needs: Vec<(Needs<'db>, Site)>` | `needs: FVec<'f, (Needs<'db>, Site)>` |

`resolve` is called once per field, impl item, opaque and function; the borrows of `needs`
in it are unchanged. Its diagnostics are made only when they are new: today each message is
made (`to_owned`, `format!`) before `contains` throws a duplicate away. Lines 618-669, before:

```rust
        let mut concrete = Vec::new();
        for &(need, site) in &needs {
            ...
            let message = "the type does not fulfill the required lifetime".to_owned();
            let diagnostic = WfDiagnostic { code: "E0477", message, site };
            if !concrete.contains(&diagnostic) {
                concrete.push(diagnostic);
            }
        }
        if !concrete.is_empty() {
            for diagnostic in concrete {
                if !self.found.contains(&diagnostic) {
                    self.found.push(diagnostic);
                }
            }
            return;
        }
        for (need, site) in needs {
            ...
            for _ in known.failing_aliases(self.db, ty, region) {
                let message = "the associated type may not live long enough".to_owned();
                let diagnostic = WfDiagnostic { code, message, site };
                if !self.found.contains(&diagnostic) {
                    self.found.push(diagnostic);
                }
            }
            for param in known.failing_params(ty, region) {
                ...
                let message = format!("the parameter type `{name}` may not live long enough");
                let diagnostic = WfDiagnostic { code, message, site };
                if !self.found.contains(&diagnostic) {
                    self.found.push(diagnostic);
                }
            }
        }
```

after:

```rust
        let mut concrete: FVec<'_, Site> = Vec::new_in(self.frame);
        for &(need, site) in &needs {
            ...
            if !concrete.contains(&site) {
                concrete.push(site);
            }
        }
        if !concrete.is_empty() {
            for &site in concrete.iter() {
                push_new(
                    self.found,
                    "E0477",
                    format_args!("the type does not fulfill the required lifetime"),
                    site,
                );
            }
            return;
        }
        for (need, site) in needs {
            ...
            for _ in known.failing_aliases(self.db, ty, region) {
                push_new(
                    self.found,
                    code,
                    format_args!("the associated type may not live long enough"),
                    site,
                );
            }
            for param in known.failing_params(ty, region) {
                ...
                push_new(
                    self.found,
                    code,
                    format_args!("the parameter type `{name}` may not live long enough"),
                    site,
                );
            }
        }
```

with, after `impl Check`:

```rust
/// Adds the diagnostic unless `found` has it, making its message only then.
fn push_new(
    found: &mut FVec<'_, WfDiagnostic>,
    code: &'static str,
    message: std::fmt::Arguments<'_>,
    site: Site,
) {
    if found
        .iter()
        .any(|it| it.code == code && it.site == site && formats_to(&it.message, message))
    {
        return;
    }
    // alloc: the result's own text, once per diagnostic reported
    found.push(WfDiagnostic { code, message: message.to_string(), site });
}

/// Whether `message` formats to `text`, compared piece by piece as it formats.
fn formats_to(text: &str, message: std::fmt::Arguments<'_>) -> bool {
    struct Rest<'t>(&'t str);
    impl std::fmt::Write for Rest<'_> {
        fn write_str(&mut self, piece: &str) -> std::fmt::Result {
            self.0 = self.0.strip_prefix(piece).ok_or(std::fmt::Error)?;
            Ok(())
        }
    }
    let mut rest = Rest(text);
    std::fmt::write(&mut rest, message).is_ok() && rest.0.is_empty()
}
```

The `concrete` list held only E0477s of one message, so a list of their sites with the same
duplicates dropped keeps the same ones in the same order; `found` gets the same diagnostics in
the same order, as a diagnostic's equality is its code, message and site.

`elided_output` (lines 676-698): `fn elided_output<'db>(frame: &Frame, db: &'db Db, function,
inputs: &[Ty<'db>])`, line 685 `let mut regions = Vec::new();` ->
`let mut regions: FVec<'_, Region<'db>> = Vec::new_in(frame);`.

What stays: `errors` (line 294, `vec![Vec::new(); n]`, a `Vec<Vec<Region>>` per function)
goes to `hir_ty::elided_lifetimes(..).regions(&errors)`, which takes `&[Vec<Region<'db>>]`
and returns a `Vec<Option<Region>>`: hir-ty's `lower.rs` line 3945 (asked below). Until it
takes `&[impl AsRef<[Region<'db>]>]`, the line gets `// alloc: hir-ty's
ElidedLifetimes::regions takes vectors (asked of mem-db)`; after it, `errors` is
`FVec<'f, FVec<'f, Region<'db>>>` made by `mem::collect(self.frame, (0..n).map(|_|
Vec::new_in(self.frame)))`. `bound_site`'s `trait_name` clone (line 577, a `Name` per where
predicate checked) is a `Symbol` clone, a reference count, not an allocation: unchanged.

Imports: `use crate::mem::{self, FVec}; use blaze_mem::frame::Frame;`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-borrowck (2026-10-29 21:40): every row, at lines about 10 below those
> given. `errors` stays a global `Vec<Vec<Region>>` with the `// alloc:` line, since hir-ty's
> `regions` still takes `&[Vec<Region>]`. The `function` per-input `found` is hoisted
> once above the `for` and drained into `needs`. `bound_site`'s `Name` clone has a `// clone:` line.

### W2. `outlives.rs`

The output buffers become generic over their allocator, as std's collections are: the checks
pass frame vectors, the tests keep theirs (`outlives/tests.rs` unchanged). Each `out: &mut
Vec<Needs<'db>>` -> `out: &mut Vec<Needs<'db>, A>` with `<.., A: Allocator>` on the function,
in `adt_needs` (187), `predicate_needs` (215), `clause_needs` (228), `wf` (250), `wf_into`
(288, and its `args` closure's parameter, 294), `projection_needs` (352), `trait_needs` (383),
`Wfcx::needs_since` (475), `Wfcx::normalize` (550), `prove_needs` (578); `opaques` (76) `out:
&mut Vec<AliasTy<'db>, A>` with `struct Find<'a, 'db, A: Allocator>(&'a mut Vec<AliasTy<'db>,
A>)`; `walk` (759) and `walk_with` (773) `out: &mut Vec<Ty<'db>, A>` with their `Walk`
structs the same; `required` (707) `found: &mut Vec<OutlivesClause<..>, A>` with `A: Allocator
+ Copy`. `use std::alloc::Allocator;`.

- `Wfcx` (lines 434-448) holds the frame its temporaries go in: `pub(crate) struct Wfcx<'f,
  'db> { frame: &'f Frame, db, def, interner, solver }`, `pub(crate) fn new(frame: &'f Frame,
  db: &'db Db, def) -> Self`, `impl<'f, 'db> Wfcx<'f, 'db>`; `projection_needs`, `trait_needs`
  and `prove_needs` take `wfcx: &Wfcx<'_, 'db>`.
- `projection_needs` (lines 355-370): `let mut aliases = Vec::new();` ->
  `let mut aliases = Vec::new_in(wfcx.frame);`; `let predicates = aliases.into_iter()..
  .collect();` -> `let predicates = mem::collect(wfcx.frame, aliases.into_iter()..);`;
  `prove_needs(wfcx, predicates, out)` -> `prove_needs(wfcx, &predicates, out)`.
- `trait_needs` (lines 387-389): `Vec::new()` twice -> `Vec::new_in(wfcx.frame)`;
  `prove_needs(wfcx, &predicates, out)`.
- `prove_needs` (lines 575-602): `predicates: &[hir_ty::next_solver::Predicate<'db>]`,
  `for &predicate in predicates`.
- `needs_since` (lines 475-543), once per normalization and proof: lines 479-491, before:

  ```rust
          let obligations: Vec<_> = inner.region_obligations()[mark.0..]
              .iter()
              .map(|it| (it.sup_type, it.sub_region))
              .collect();
          let constraints: Vec<Constraint<'db>> = inner
              .unwrap_region_constraints()
              .region_constraint_data()
              .constraints
              .iter_from(mark.1)
              .collect();
          drop(inner);
          // The impl's lifetimes are inference variables the solver equates with the item's.
          let mut equal: FxHashMap<rustc_type_ir::RegionVid, Region<'db>> = FxHashMap::default();
  ```

  after:

  ```rust
          let obligations = mem::collect(
              self.frame,
              inner.region_obligations()[mark.0..].iter().map(|it| (it.sup_type, it.sub_region)),
          );
          let constraints: FVec<'_, Constraint<'db>> = mem::collect(
              self.frame,
              inner.unwrap_region_constraints().region_constraint_data().constraints.iter_from(mark.1),
          );
          drop(inner);
          // The impl's lifetimes are inference variables the solver equates with the item's.
          let mut equal: FMap<'_, rustc_type_ir::RegionVid, Region<'db>> = mem::map(self.frame);
  ```

  `equal` is only looked up; the loops over `obligations` and `constraints` keep their order.
- `inferred_outlives` (lines 653-680), a query whose value is owned. Before / after:

  ```rust
  fn inferred_outlives(db: &Db, adt: AdtId) -> InferredOutlives {
      let mut found = Vec::new();
      let mut subtypes = Vec::new();
      ...
      let mut seen = FxHashSet::default();
      found.retain(|it| seen.insert(*it));
      InferredOutlives(
          found
              .into_iter()
              .filter_map(|OutlivesClause(arg, region)| { .. })
              .collect(),
      )
  }
  ```

  ```rust
  fn inferred_outlives(db: &Db, adt: AdtId) -> InferredOutlives {
      blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
          let mut found = Vec::new_in(frame);
          let mut subtypes = Vec::new_in(frame);
          ... (unchanged)
          let mut seen = mem::set(frame);
          found.retain(|it| seen.insert(*it));
          let kept = mem::collect(
              frame,
              found.into_iter().filter_map(|OutlivesClause(arg, region)| { .. (unchanged) }),
          );
          InferredOutlives(mem::owned(kept.into_iter()))
      })
  }
  ```

  The memo is made once at its length; `.collect()` of a `filter_map` grew it by doubling.
- `required` (line 713): `let mut needs = Vec::new();` -> `let mut needs =
  Vec::new_in(*found.allocator());` (the caller's allocator: the frame in the query, the
  global heap in the tests).
- `variants` (lines 788-796): a `SmallVec<[VariantId; 4]>` per ADT, on the heap for an enum of
  five variants or more; it becomes an iterator over the enum's own table. Before / after:

  ```rust
  pub(crate) fn variants(db: &Db, adt: AdtId) -> SmallVec<[VariantId; 4]> {
      match adt {
          AdtId::StructId(it) => smallvec![it.into()],
          AdtId::UnionId(it) => smallvec![it.into()],
          AdtId::EnumId(it) => {
              it.enum_variants(db).variants.values().map(|&(it, _)| it.into()).collect()
          }
      }
  }
  ```

  ```rust
  pub(crate) fn variants(db: &Db, adt: AdtId) -> impl Iterator<Item = VariantId> + '_ {
      let (one, all): (Option<VariantId>, _) = match adt {
          AdtId::StructId(it) => (Some(it.into()), None),
          AdtId::UnionId(it) => (Some(it.into()), None),
          AdtId::EnumId(it) => (None, Some(it.enum_variants(db).variants.values())),
      };
      one.into_iter().chain(all.into_iter().flatten().map(|&(it, _)| it.into()))
  }
  ```

  Its callers (`lib.rs` line 151, `inferred_outlives` line 656, the tests' `field_tys`) iterate
  it with `for`, in the same order.
- `Known` (lines 800-812): `pub(crate) struct Known<'f, 'db> { regions: FVec<'f, (Region<'db>,
  Region<'db>)>, types: FVec<'f, (Ty<'db>, Region<'db>)> }`, `impl<'f, 'db> Known<'f, 'db>`;
  `pub(crate) fn new(frame: &'f Frame, clauses: &[Clause<'db>]) -> Self` with `Known { regions:
  Vec::new_in(frame), types: Vec::new_in(frame) }` and `let mut needs = Vec::new_in(frame);`.
- `region_outlives` (lines 854-868), called per need against what is known: two `SmallVec`s of
  eight, on the heap past eight regions. One frame vector serves as both (what is reachable
  does not depend on the order it is found in, and the answer is whether `shorter` or
  `'static` is), and it is the frame's last allocation while it lives, so it grows and is
  freed in place. Before:

  ```rust
          let mut seen: SmallVec<[Region<'db>; 8]> = smallvec![longer];
          let mut stack: SmallVec<[Region<'db>; 8]> = smallvec![longer];
          while let Some(region) = stack.pop() {
              for &(from, to) in &self.regions {
                  if from != region || seen.contains(&to) {
                      continue;
                  }
                  if to == shorter || matches!(to.kind(), RegionKind::ReStatic) {
                      return true;
                  }
                  seen.push(to);
                  stack.push(to);
              }
          }
          false
  ```

  after:

  ```rust
          let mut seen = Vec::new_in(*self.regions.allocator());
          seen.push(longer);
          let mut next = 0;
          while let Some(&region) = seen.get(next) {
              next += 1;
              for &(from, to) in self.regions.iter() {
                  if from != region || seen.contains(&to) {
                      continue;
                  }
                  if to == shorter || matches!(to.kind(), RegionKind::ReStatic) {
                      return true;
                  }
                  seen.push(to);
              }
          }
          false
  ```
- `failing_aliases` (line 879) and `failing_params` (965): `-> FVec<'f, Ty<'db>>`, `let mut
  failing = Vec::new_in(*self.regions.allocator());`.
- `alias_outlives` (lines 918-929): `let declared: Vec<Region<'db>> = ...collect();` ->
  `let declared = mem::collect(*self.regions.allocator(), ...);` (the same chain).
- Imports: line 10 `use rustc_hash::{FxHashMap, FxHashSet};` -> `use crate::mem::{self, FMap,
  FVec}; use blaze_mem::frame::Frame;`; line 11 `use smallvec::{SmallVec, smallvec};` ->
  `use smallvec::SmallVec;` (`components` keeps its `SmallVec<[Component; 4]>`: below).

What stays: `components` returns a `SmallVec<[Component; 4]>` by value to callers that iterate
it while calling back into `Known`; a type of five or more components puts it on the heap.
Turning it into an out-parameter on the frame frees nothing in place (the callers allocate in
between), so the frame would keep one per call. It stays: a type of five or more components
is one global allocation per `components` call on it, which T2's count shows (`Many` and
`both` have at most four, so T2 passes without it; a fixture type with five would fail T2 by
the calls on it). The long-term fix is a per-worker `Scratch<Component>` that `components`
fills and its callers read before calling it again; it is listed in the notes as open.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-borrowck (2026-10-29 21:40): as written. The `Find`/`Walk` structs declare
> their own `A: Allocator`, since an inner item can't name the function's.

### W3. `compare.rs`

- `own` (lines 140-146) and the kinds test (152-153): `GenericArgs::iter(self)` is an exact
  `Copied<slice::Iter<'db, _>>`, so nothing is collected:

  ```rust
      let own = |def: FunctionId, parent: SolverDefId<'db>| {
          let all = GenericArgs::identity_for_item(interner, SolverDefId::FunctionId(def));
          let parent = GenericArgs::identity_for_item(interner, parent).len();
          all.iter().skip(parent)
      };
      ...
      if !own(trait_method, SolverDefId::TraitId(trait_ref.def_id.0))
          .map(|it| kind(it.kind()))
          .eq(own(impl_method, SolverDefId::ImplId(impl_)).map(|it| kind(it.kind())))
      {
          return None;
      }
  ```

  (equal lengths and equal kinds at each place is equal sequences of kinds.) `kind` moves above
  `own`'s use.
- `synthetic` (lines 162-176): `GenericParams::of` returns `&'db GenericParams`, so the closure
  returns the iterator: `.collect::<SmallVec<[bool; 4]>>()` goes, and `if synthetic(impl_method)
  != synthetic(trait_method)` -> `if !synthetic(impl_method).eq(synthetic(trait_method))`.
- `hybrid` (lines 199-211): `elaborate` takes any `IntoIterator`, so the chain goes to it
  without the `SmallVec<[Clause; 8]>` (on the heap past eight where clauses):

  ```rust
      let hybrid = GenericPredicates::query_all(db, impl_.into())
          .iter_identity()
          .map(|it| it.skip_norm_wip())
          .chain(
              GenericPredicates::query_own_explicit(db, trait_method.into())
                  .iter_instantiated(interner, trait_to_impl_args.as_slice())
                  .map(|it| it.skip_norm_wip()),
          );
      let param_env = ParamEnv::new(Clauses::new_from_iter(rustc_type_ir::elaborate::elaborate(
          interner, hybrid,
      )));
  ```
- Line 20 `use smallvec::SmallVec;` goes if nothing else in the file uses it (it does not).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-borrowck (2026-10-29 21:40): as written. The now-unused `Clause` import
> goes as well.

### W4. `persist.rs`

The edge (the memo written to the store and read back): `to_result` (line 73) is exact; `write`
(lines 93-115) collects the message offsets and rows per memo written, `// alloc: the memo's
persisted form, once per write` on each. Unchanged otherwise.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-borrowck (2026-10-29 21:40): the two `// alloc:` lines on `write`'s
> collects, plus one on `to_result`'s `to_owned` (the edge), which the hot lint flags.

## S. Per-call buffers, reused

The hit paths (a memo read: `borrowck`, `wfcheck`, `inferred_outlives` through
`Inferred::of`, `gate::checked` on a passing body) make no buffer: they read the memo in
place (T1, T2 check it). The buffers made per call inside a run, each now made once per run
and cleared per use:

| where | per | now |
|---|---|---|
| `loans.rs` `out_of_scope` (844-882) | loan: a bitset of all blocks, a work list, a `SmallVec` of ends | one `ScopeWalk` per body, cleared by the blocks entered (B5.2) |
| `loans.rs` two-phase scan (596) | point: `let mut used = Vec::new()` | one `used`, cleared per point (B5.2) |
| `loans.rs` `borrows_rejected` (496) | promoted local: `vec![expr]` | one stack, cleared per call (B5.2) |
| `loans.rs` `const_promotable` (422-458) | aggregate: a copy of its children | iterators, nothing (B5.2) |
| `qualifs.rs` `value` (124) | record literal: its fields | an iterator, nothing (B5.8) |
| `intervals.rs` `insert_new` | walk step: a `SmallVec` of runs | the caller's buffer (B2.5, B5.4) |
| `intervals.rs` `union`, `subtract` | call: a merged copy | `scratch::with_u64s`, copied back only when changed (B2.5) |
| `liveness.rs` `alias_bound` (856) | alias: its bounds | the frame (B5.1) |
| `inference.rs` `strongly_connected` | body: vectors grown by doubling | frame arrays at their final size (B5.5) |
| `constraints.rs` `of_sup` | rebuild: a new graph | the last graph's arrays (B5.6) |
| wfcheck `function` (320-355) | input: `&mut Vec::new()`, `found` | one `dropped`, one `found` (W1) |
| wfcheck `opaques`, `trait_opaques` (405, 450) | opaque: `found` | one, cleared per opaque (W1) |
| wfcheck `resolve` (627-667) | need: a message before the duplicate test | made only when new (W1) |
| wfcheck `needs_since` (479-491) | normalization: two vectors and a map | the frame (W2) |
| wfcheck `region_outlives` (854) | call: two `SmallVec`s, heap past eight | one frame vector at the top, freed in place (W2) |
| wfcheck `variants`, compare `own`/`synthetic`/`hybrid` | call: a `SmallVec` | iterators (W2, W3) |

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:33): a table of the B2/B5/W rows, each applied with its section; e.g. `ScopeWalk` at crates/hir-ty-borrowck/src/loans.rs:772.

## T. Budget tests

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:33): T1 and T2 below.

### T1. `crates/hir-ty-borrowck/src/budget.rs` (new)

```rust
//! What the borrow checker allocates on the global heap: the result it keeps and nothing else,
//! and nothing at all to answer again.

use blaze_alloc_count::measure;
use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map};
use test_fixture::WithFixture;

use crate::{
    BorrowckResult, PartSpan, borrowck, borrowck_in_mode, mode, scan::tests::db::TestDB,
};

/// The global heap blocks `result` owns: each vector and string with room.
fn owned_blocks(result: &BorrowckResult) -> u64 {
    let text = |it: &String| u64::from(it.capacity() > 0);
    let room = |capacity: usize| u64::from(capacity > 0);
    let labels = |it: &Vec<(PartSpan, String)>| {
        room(it.capacity()) + it.iter().map(|(_, it)| text(it)).sum::<u64>()
    };
    room(result.diagnostics.capacity())
        + result.skipped.as_ref().map_or(0, text)
        + result
            .diagnostics
            .iter()
            .map(|it| {
                text(&it.message)
                    + labels(&it.labels)
                    + room(it.children.capacity())
                    + it.children
                        .iter()
                        .map(|it| text(&it.message) + room(it.spans.capacity()) + labels(&it.labels))
                        .sum::<u64>()
                    + room(it.suggestions.capacity())
                    + it.suggestions.iter().map(|it| text(&it.message) + labels(&it.parts)).sum::<u64>()
            })
            .sum::<u64>()
}

fn functions(db: &TestDB) -> Vec<DefWithBodyId> {
    let mut found = Vec::new();
    for &krate in base_db::all_crates(db).iter() {
        for (_, module) in crate_def_map(db, krate).modules(db) {
            found.extend(module.scope(db).declarations().filter_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it.into()),
                _ => None,
            }));
        }
    }
    found
}

const CLEAN: &str = r#"
//- minicore: sized, copy, clone, option
struct Pair { a: u32, b: Option<u32> }
fn sum(pair: &Pair) -> u32 { pair.a + pair.b.unwrap_or(0) }
fn grow(pair: &mut Pair, by: u32) { pair.a += by; if let Some(b) = &mut pair.b { *b += by; } }
fn closures(mut pair: Pair) -> u32 {
    let mut add = |by| grow(&mut pair, by);
    add(1);
    add(2);
    sum(&pair)
}
fn loops(items: &[u32]) -> u32 {
    let mut total = 0;
    for at in 0..items.len() { if items[at] > 3 { total += items[at]; } else { break; } }
    total
}
"#;

const ERRORS: &str = r#"
//- minicore: sized, copy, clone, option
struct Text;
fn take(_: Text) {}
fn moved_twice() { let text = Text; take(text); take(text); }
fn two_mut() { let mut n = 0; let a = &mut n; let b = &mut n; *a += 1; *b += 1; }
fn unused_mut() { let mut n = 1; let _ = n; }
"#;

/// Checking a body again, with all it reads (its MIR, its types) already asked, allocates the
/// result's own blocks and nothing else: every temporary is on the run's frame.
fn allocates_its_result_alone(fixture: &str, errors: bool) {
    let (db, _) = TestDB::with_single_file(fixture);
    hir_ty::attach_db(&db, || {
        let defs = functions(&db);
        assert!(defs.len() >= 3, "{}", defs.len());
        for &def in &defs {
            borrowck(&db, def);
        }
        let mut reported = 0;
        for &def in &defs {
            let (result, used) = measure(|| borrowck_in_mode(&db, def, mode()));
            assert_eq!(result, *borrowck(&db, def), "{def:?}");
            assert_eq!(used.allocations, owned_blocks(&result), "{def:?}: {result:?}");
            reported += result.diagnostics.len();
        }
        assert_eq!(reported > 0, errors, "{reported}");
    });
}

#[test]
fn a_clean_body_allocates_nothing() {
    allocates_its_result_alone(CLEAN, false);
}

#[test]
fn a_body_with_errors_allocates_its_diagnostics_alone() {
    allocates_its_result_alone(ERRORS, true);
}

#[test]
fn asking_again_allocates_nothing() {
    let (db, _) = TestDB::with_single_file(ERRORS);
    hir_ty::attach_db(&db, || {
        for def in functions(&db) {
            let first: *const BorrowckResult = borrowck(&db, def);
            let (again, used) = measure(|| borrowck(&db, def) as *const BorrowckResult);
            assert_eq!(again, first, "{def:?}");
            assert_eq!(used.allocations, 0, "{def:?}");
        }
    });
}

#[test]
fn an_edit_to_another_file_allocates_nothing_to_answer_again() {
    let (mut db, files) = TestDB::with_many_files(
        r#"
//- /main.rs
mod other;
struct Text;
fn take(_: Text) {}
fn moved_twice() { let text = Text; take(text); take(text); }
fn fine(n: &mut u32) { *n += 1; }
//- /other.rs
pub fn elsewhere() -> u32 { 1 }
"#,
    );
    let other = files[1];
    let defs = hir_ty::attach_db(&db, || {
        let defs: Vec<_> = functions(&db)
            .into_iter()
            .filter(|&def| {
                matches!(def, DefWithBodyId::FunctionId(it)
                    if hir_def::signatures::FunctionSignature::of(&db, it).name.as_str() != "elsewhere")
            })
            .collect();
        for &def in &defs {
            borrowck(&db, def);
        }
        defs
    });
    db.set_file_text(other.file_id(&db), "pub fn elsewhere() -> u32 { 2 }\n");
    hir_ty::attach_db(&db, || {
        for &def in &defs {
            let (_, used) = measure(|| borrowck(&db, def));
            assert_eq!(used.allocations, 0, "{def:?}");
        }
    });
}

#[test]
fn a_passing_gate_allocates_nothing() {
    let (db, _) = TestDB::with_single_file(CLEAN);
    hir_ty::attach_db(&db, || {
        for def in functions(&db) {
            assert!(crate::gate::checked(&db, def).is_ok(), "{def:?}: the clean fixture passes");
            let (passed, used) = measure(|| crate::gate::checked(&db, def).is_ok());
            assert!(passed, "{def:?}");
            assert_eq!(used.allocations, 0, "{def:?}");
        }
    });
}
```

What each needs before it passes, besides this request: the first two, that what a
body's check builds through hir-ty allocates nothing once warm (`InferCtxt`'s tables, recycled
per worker by mem-db-solver's request 5, and the solver's caches: mem-db-infer, mem-db-solver)
and B6/B7's parts (regions.rs, check.rs, explain.rs) with this rule for B7:
**each owned `String` of a diagnostic is made in one allocation** (formatted into a frame
buffer, then copied out at its length: `format!` estimates and may grow, and a growth counts);
the last three, salsa's hit and revalidation paths allocating nothing (mem-db-salsa).
`gate::checked(db, def) -> Result<Checked<'_>, Refused>` (`gate.rs` line 108) on a passing
body reads the memo and makes the `Checked` view; a refused body's `Refused` clones the
errors, its error path.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-audit-mem (22:33): crates/hir-ty-borrowck/src/budget.rs had frontend-fast-borrowck's tests (clean, large, errors, asking again, the fixpoint) but not T1's last two: added `an_edit_to_another_file_allocates_nothing_to_answer_again` and `a_passing_gate_allocates_nothing`, on that file's `Db` and `functions -> (name, def)`, the gate test on its `TYPICAL` fixture in place of `CLEAN`.

### T2. `crates/hir-ty-wfcheck/src/budget.rs` (new)

```rust
//! What item well-formedness allocates on the global heap: its diagnostics alone, and
//! nothing to answer again.

use blaze_alloc_count::measure;
use hir_def::{GenericDefId, ModuleDefId, nameres::crate_def_map};
use test_fixture::WithFixture;

use crate::{WfResult, outlives::tests::db::TestDB, wfcheck};

fn items(db: &TestDB) -> Vec<GenericDefId> {
    let mut found = Vec::new();
    for &krate in base_db::all_crates(db).iter() {
        for (_, module) in crate_def_map(db, krate).modules(db) {
            found.extend(module.scope(db).declarations().filter_map(|def| match def {
                ModuleDefId::AdtId(it) => Some(it.into()),
                ModuleDefId::FunctionId(it) => Some(it.into()),
                ModuleDefId::TraitId(it) => Some(it.into()),
                _ => None,
            }));
            found.extend(module.scope(db).impls().map(GenericDefId::from));
        }
    }
    found
}

const FIXTURE: &str = r#"
//- minicore: sized, option
struct Wrap<'a, T: 'a>(&'a T);
struct Static<T> { x: &'static T }
enum Many<'a, T> { A(&'a T), B(Option<Wrap<'a, T>>), C, D(u8), E(&'a u8) }
trait Tr<'a> { type Out; }
impl<'a, T: 'a> Tr<'a> for Wrap<'a, T> { type Out = &'a T; }
fn both<'a, T>(x: &'a T, y: Static<T>) -> &'a T where T: 'a { let _ = y; x }
struct Twice<T> { x: &'static T, y: &'static T }
"#;

/// The diagnostics' own blocks: the vector and each message.
fn owned_blocks(result: &WfResult) -> u64 {
    u64::from(result.diagnostics.capacity() > 0)
        + result.diagnostics.iter().map(|it| u64::from(it.message.capacity() > 0)).sum::<u64>()
}

#[test]
fn checking_an_item_allocates_its_diagnostics_alone() {
    let (db, _) = TestDB::with_single_file(FIXTURE);
    hir_ty::attach_db(&db, || {
        let defs = items(&db);
        let mut reported = 0;
        for &def in &defs {
            let result = wfcheck(&db, def);
            reported += result.diagnostics.len();
        }
        assert!(reported > 0, "the fixture's `Static` and `Twice` need `T: 'static`");
        for &def in &defs {
            let (result, used) = measure(|| crate::check_item(&db, def));
            assert_eq!(&result, wfcheck(&db, def), "{def:?}");
            assert_eq!(used.allocations, owned_blocks(&result), "{def:?}: {result:?}");
        }
    });
}

#[test]
fn asking_again_allocates_nothing() {
    let (db, _) = TestDB::with_single_file(FIXTURE);
    hir_ty::attach_db(&db, || {
        for def in items(&db) {
            wfcheck(&db, def);
            let (_, used) = measure(|| wfcheck(&db, def));
            assert_eq!(used.allocations, 0, "{def:?}");
        }
    });
}
```

`check_item` (W1) is `wfcheck`'s body as a plain function, as `borrowck_in_mode` is
`borrowck`'s, so the test runs it again with what it reads already asked. `Twice` has two E0310s of different sites (`push_new`
keeps both); the second pass also checks `push_new` adds nothing of its own (its comparison
allocates nothing). What the first test needs besides this request: `Wfcx`'s `InferCtxt`
and solver allocating nothing once warm (tables recycled per worker, mem-db-solver's request
5); the second, salsa's hit path
(mem-db-salsa).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-borrowck (2026-10-29 21:40): `crates/hir-ty-wfcheck/src/budget.rs` as
> written. The test's own `Vec` gets an `// alloc:` line for the hot lint.

## Asks of others

- **mem lane, through mem-db** (mem-db's request 1): `blaze_mem::frame::current`, which
  B4.4-B4.5 use under a check's task.
- **mem-db-salsa**: salsa's hit and revalidation paths allocating nothing (T1's last three
  tests, T2's second). The checks open their own tasks (B4.1, W1), so they need no tag from
  salsa; `is_copy_query` and `drop_glue_query` (borrowck `lib.rs` 880-905) build an
  `InferCtxt` per miss, which is mem-db-infer's.
- **hir-ty `lower.rs` line 3945** (`ElidedLifetimes::regions`): `errors: &[impl
  AsRef<[Region<'db>]>]` for `&[Vec<Region<'db>>]` (`.map(Vec::as_slice)` ->
  `.map(AsRef::as_ref)`), so wfcheck's per-input lists can be frame vectors (W1).
- **mem-db-infer / mem-db-solver**: `InferCtxt`'s tables recycled per worker (mem-db-solver
  request 5; nothing is threaded from the checks) and the solver's per-run state allocating
  nothing once warm; both crates' "owned result alone" tests (T1, T2) count what they allocate.
- **Settled with `dedup-bits-db`** (B7.13 in `v2/requests/mem-db-checks-check.md`):
  `v2/requests/db.md` "## dedup: bits: hir-ty-borrowck on frame words" lands after B2 and
  keeps B2.1's frame words, so T1's zero global allocations hold.

> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-mem (22:33): in the tree: `frame::current` (blaze-mem), salsa M1/M2 (`vendor/salsa/src/function/execute.rs` `frame::task(C::MEM_TAG, ..)`), `ElidedLifetimes::regions_into<E: AsRef<[Region]>>` at crates/hir-ty/src/lower.rs:4169, dedup bits applied (db.md); the rest are notes.

## frontend-fast-bodies-cache-bodies, for db-mem-borrowck: a served verdict in the gate, and the served tests (2026-10-29)

Items 6 and 7 of `v2/requests/db.md` / `db-ty.md` "frontend-fast-bodies-cache-bodies: served
dependency bodies, relocated ids and two hooks"; db-ty says hir-ty-borrowck is yours. hir-ty's
side (`pub mod check_cache`, the persist edits, the `infer_body_query` hook) is in the tree or
being applied by db-apply-late.

### 1. `crates/hir-ty-borrowck/src/gate.rs`

Old:

```rust
pub fn checked(db: &Db, def: DefWithBodyId) -> Result<Checked<'_>, Refused> {
    verdict(def, borrowck(db, def))?;
    Ok(Checked { owner: def, _revision: PhantomData })
}
```

New:

```rust
pub fn checked(db: &Db, def: DefWithBodyId) -> Result<Checked<'_>, Refused> {
    if !hir_ty::check_cache::served_passed(db, def) {
        verdict(def, borrowck(db, def))?;
    }
    Ok(Checked { owner: def, _revision: PhantomData })
}
```

Why: a dependency's body served from its image carries the verdict the driver's borrow check
gave when the part was written (`check_cache::encode`'s `passed`); `served_passed` is `false`
with nothing served (one `try_get` of a singleton input) and for any body not recorded as passed.

### 2. `crates/hir-ty-borrowck/src/lib.rs`

Old:

```rust
#[cfg(test)]
mod differential;
```

New:

```rust
#[cfg(test)]
mod differential;
#[cfg(test)]
mod served;
```

`src/served.rs` is written (mine): a served body's MIR equals the MIR of its own inference, and
`two_mut` (a borrow error) fails `checked` unserved, passes served as passed, fails served as not.

### 3. `crates/hir-ty-borrowck/Cargo.toml`, `[dev-dependencies]`

Old:

```toml
[dev-dependencies]
test-fixture.workspace = true
blaze-alloc-count.workspace = true
```

New:

```toml
[dev-dependencies]
test-fixture.workspace = true
blaze-alloc-count.workspace = true
blaze-format.workspace = true
```

Why: the test hands a part over as an `Aligned`, the one `Held`.

Status: applied
> applied by db-mem-borrowck (2026-10-29 22:44): 1, 2 and 3 as written, together. db-ty says `hir_ty::check_cache` is in the tree and the borrowck lanes wait on it; everything they name resolves (`served_passed`, `encode`, `set_served`, `hir_ty::infer_query_with_inspect`, `blaze-format` in the workspace). db.md's parts 1-5, the `infer_body_query` hook among them, are all in as of 22:45 (db-apply-late). Manifest lint and hot_lint clean. Not compiled (applier rules).
> (earlier) not applied by db-mem-borrowck (2026-10-29 22:41): the same edits as db.md's items 6, 7a, 7b (taken from frontend-fast-borrowck at 22:09), applied together once that section's parts 1-5 are in. Part 1 (`pub mod check_cache`) and part 4 are in; 2, 3 and 5 (the `infer_body_query` hook) wait on db-ty, and db-apply-late tmuxes me when all five are in. Everything `served.rs` names (`encode`, `set_served`, `served_passed`) is in `check_cache.rs` already.

> forwarded to db-mem-borrowck (its window); told by tmux (requests, pass 32).

## mem-db-checks (db-mem-borrowck; from db-ty, 2026-10-29 23:58): take E0741's const-parameter check into `hir-ty-wfcheck`

What: db-ty applied rustc's `check_param_wf` for const parameters (E0741, for diagnostics window 67) as `hir_ide::const_param_ty::const_param_ty_errors(db, GenericDefId)`, in the new `crates/hir-ty/src/const_param_ty.rs` (query kind `CONST_PARAM_TY_ERRORS = 0x243a` in `hir-ty/src/db.rs`). Its test is `hir-ty/src/tests/const_param_ty.rs`. The requester places it in `hir-ty-wfcheck`, beside `wfcheck`, since it is part of rustc's `check_well_formed`. That crate is yours, so I put it in hir-ty for now.

To move it:
- Copy the module into `hir-ty-wfcheck`. It reads only public hir-ty API, but for `crate::lower::const_param_types`; use `db.const_param_types(def)` (`HirDatabase`) instead.
- Copy its test too.
- Repoint hir's one caller, `DiagnosticsCollector::collect_const_param_tys` in `crates/hir/src/diagnostics.rs`. It reads `error.param`, `error.ty` and `error.reason`, and re-exports `ConstParamTyReason` from `hir_ide::const_param_ty`. Change both paths.
- Tell db-ty. Then db-ty deletes the hir-ty module, its `pub mod`, its test and its kind, since a kind number is never reused.

Alternatively, fold it into `wfcheck`'s `WfResult::diagnostics` as a new `Site`. Why: rule 8 (semantics-in-front-end, in a crate of its own where possible). The check is item well-formedness.

> forwarded to db-mem-borrowck (requests, pass 43: `hir-ty-wfcheck` and hir are db's; told by tmux)
> not applied by db-mem-borrowck (2026-10-29 23:58): agreed (the check is `hir-ty-wfcheck`'s), waits on the coordinator's freeze (23:48); db-ty keeps the hir-ty copy until it is moved.
