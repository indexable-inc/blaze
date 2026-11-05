# 12 — Batch API

Status: built (2026-10-15). Lanes, their executor and the only software
prefetch are in `bbtree/src/tree/batch.rs`; the API in `bbtree/src/unsync/map/batch.rs`
and `bbtree/src/unsync/set/mod.rs`; batched `extend` in
`bbtree/src/unsync/map/from_entries.rs`; the set operations' batched lookups in
`bbtree/src/unsync/set/lookups.rs`. D53. Prefetch is allowed here and nowhere else:
D46's exception. The shared `BBTreeMap`'s batches, `read_many`,
`get_many_copied`, `insert_many` and `update_or_insert_many`, run lanes of
their own on this executor and `Prefetcher`
(`bbtree/src/tree/latched/batch.rs`, doc 09, D55). File maps' `get_many`
(`bbtree/src/file.rs`, doc 15, D58) runs the unsync lanes on the file's
pages.

## Problem

A lookup is a chain of dependent misses: a child's address comes out of
its parent's search, and a `String` hit then reads the entry's key and its
heap bytes (06, "Lookup"). Consecutive operations don't overlap: between
one operation's first miss and the next one's first load lies the rest of
the first, far more than the 448-entry reorder buffer of Zen 5 reaches. So
each operation pays its chain alone, even when the caller has many
independent keys: a join probing an index, a bulk load in random order,
`extend`.

## API and contract

```rust
// unsync::BBTreeMap<K, V, S, A>
pub fn get_many<Q, const N: usize>(&self, keys: [&Q; N]) -> [Option<&V>; N]
where K: Borrow<Q> + Ord, Q: Ord + ?Sized;
pub fn insert_many<const N: usize>(&mut self, entries: [(K, V); N]) -> [Option<V>; N]
where K: Ord;

// unsync::BBTreeSet<T, S, A>
pub fn contains_many<Q, const N: usize>(&self, values: [&Q; N]) -> [bool; N]
where T: Borrow<Q> + Ord, Q: Ord + ?Sized;
pub fn insert_many<const N: usize>(&mut self, values: [T; N]) -> [bool; N]
where T: Ord;
```

Results match individual calls in input order, repeated keys included.
Replacement keeps the existing key. Empty arrays work. Borrowed queries
obey `get`'s ordering contract. Inserts make the same tree, node by node,
keys and prefix lengths included, as individual inserts. Nothing async
reaches the caller: no future, no executor, nothing to poll.

Comparison order need not match individual calls: the read-only descent
compares later keys before earlier entries apply. User-code panics are not
reconciled with a serial loop (D48); entries pulled into a batch but not
yet applied may be dropped. Ownership and memory safety hold.

Operations whose work is a batch use it without being asked:

- **`extend`**, of maps and sets, owned and by reference: once the tree is
  past the size cutoff (below), sixteen entries at a time through
  `insert_many`.
- **`intersection`, `difference`, `is_subset`, `is_superset`,
  `is_disjoint`** and the `&` and `-` operators, when they look one set's
  elements up in a set at least 16 times larger (std's choice) that is past
  the cutoff: 32 lookups at a time, ahead of the iterator. The iterator
  still hands out one element per `next`, so it prints and hints as std's.

## Lanes

Each key descends in its own lane: an async block that reads like the
single-key search, but, before reading a line it may not have, prefetches
that line and yields. A private executor polls the lanes round-robin, so
one lane's wait for memory overlaps the others' searches.

| Before the lane reads | It prefetches, then yields |
|---|---|
| the root (lookups) | the query's window |
| a chosen edge | that edge's line |
| a child internal node | the child's header line |
| its leaf | the leaf's header line |
| a unique leaf candidate | the candidate's entry |
| a long string candidate's bytes | the first heap line the comparison reads |
| an insertion's shift | the first slot it shifts (hint only, no yield) |

A lane ends early at a separator equal to its key, at a key outside what
every key of the tree shares (D50), or at a leaf whose inline keys rule the
key out. Ties in the inline keys (D18) go to the ordinary leaf search.

Each `.await` names what it fetches (`LaneFetch`) to the lane's watch:
`()` in the map's batches, which compiles to nothing (as many instructions
a key as before, `design/10-visualization.md`), and the visualizer's trace
behind `inspect` (`TreeView::trace_many`). The lane passes its watch, not
itself, to `Prefetcher::watched`: borrowing the whole lane changes what
the async block keeps across awaits.

A lane keeps only its query, one node pointer for the internal levels and
the leaf, a `u16` index for the chosen edge and then the leaf candidate,
and a `u16` prefix length, the root's for the query window and then the
leaf's: the candidate is confirmed without reading the leaf's header again.
The lookup future is 56 bytes. Each writes its result into a slot of the
caller's output array and returns `()`.

The executor keeps up to 64 lanes pinned in a stack array with one bit per
live lane; more lanes each sit in an `Option`, emptied as they finish. It
uses `Waker::noop()`: a lane yields only after prefetching what it reads
next, so every live lane is simply polled again next round. No allocation,
vtable or runtime.

Below the size cutoff, for N < 2, and on a root leaf, each key goes alone
through `find_in_batch`, which has its own call-site marker (below).

### What the compiler makes of a lane

Each rule was measured on 10M hits; reverting it loses what it says.

1. **One flat async block per lane.** Awaiting a nested future for the
   internal levels added a second dispatch to every resume: 124
   instructions and 13 ns per `u64` hit.
2. **Write the result to a slot; return `()`.** LLVM keeps a returned
   `Poll<Option<&V>>` as one aggregate phi and tests its constant tag on
   every pending path; the slot saved 37 instructions per `u64` hit.
3. **A live-bit mask, not `Option` slots, for up to 64 lanes:** no second
   discriminant beside each coroutine's own.
4. **`#[inline(always)]` on the async block** of a `fn -> impl Future`. On
   an `async fn` it reaches only the constructor, not the poll.
5. **Keep the query a lane needs in the lane.** Rebuilding a `SearchKey`
   per stage rereads string metadata; built inside an `if let` or `match`
   scrutinee, it also stays in the future across the await (96 bytes).
6. **Give each caller of a shared search its own call-site marker** (next
   section).

Codex's measured reference lane, built into the same binary, is 4.9 ns
faster on 10M JSON hits and misses (83.6 / 68.6 against 88.5 / 73.5 ns),
with as many instructions and lines from memory. Future size and padding,
cache-line alignment, the order lanes are built in and the outlining of
the leaf search don't explain it; `u64` and `hash32` are equal or faster.
Left open.

## Prefetch policy and cost model

Only `Prefetcher` in `bbtree/src/tree/batch.rs` issues a hint: x86-64
`prefetcht0`, AArch64 `prfm pldl1keep` (through `asm!`, since its
intrinsic isn't stable), nothing on other targets or under Miri. Without
it, an M1 Pro's 10M batched hits took 283 ns a `u64` key against 200 with
it, and 276 against 191 a JSON key (2026-10-16). Single-key
operations never call it. Each hinted address is the next thing its lane
reads; no unchosen edges, whole-node scans, whole heap keys or full shift
spans. Node searches load their inline-key columns themselves.

```text
one key at a time ≈ dependent miss chain + search work
a batch           ≈ max(miss chain / overlap, search work + lane bookkeeping)
```

At 70% occupancy, 10M `u64` entries need about 250K leaf pages and 4 MB
of internal pages; `String` entries about 585K leaves and 11 MB. Leaves
are 1 KiB; inline-key internal nodes 512 B. The 9900X has 48 KiB L1D and
1 MiB L2 per core, and 32 MiB L3 per die: upper nodes stay in L1/L2, and
the last internal level competes with leaves and queries in L3. The query
itself can miss before the cached root, hence the window hint.

The L1D has 64 sets of 12 ways. Aligned 1 KiB leaf headers fall into four
sets: 64 headers exceed their 48 ways. Keeping the leaf prefix in the lane
avoids reloading a header that may have been evicted meanwhile.

Fixed-array queries recover their slice from their reference, so outlined
searches keep constant lengths. Heap strings keep their cached slice, and
their window checks for a query shorter than eight bytes first: long
queries load a whole word, a tail shifts the last word. This also serves
the single-key search: 30 fewer instructions per JSON `get`, 10 per
`hash32` and wiki, and a 10M JSON hit at about 220 ns against 245 without
it.

The size cutoff estimates one L2 of leaf pages from the leaf's plan:

```text
pages_in_l2 = max(1, 1 MiB / leaf_page_bytes)
entries_per_leaf = floor(0.7 * leaf_capacity) + 1  // includes its head
batch if length >= pages_in_l2 * entries_per_leaf
```

That is 39,936 entries for `u64 → u64`, 17,408 for `String` or `hash32`
keys, and between 64K and 128K for a `u64` set. Heap bytes are unknown, so
this is a floor, not a cache-residency test. Fuzzing builds skip the
cutoff to reach the lanes on small trees.

## Insertion and staleness

`insert_many` first tries the remembered rightmost leaf (D45): if the first
key appends, the rest of the array goes one by one, as ascending keys
mostly append. Otherwise each key's lane descends read-only and saves a
destination: its leaf with that leaf's length and search
(`Leaf::search_always_inlined`), a separator equal to the key, or "from the
root" for a key outside what every key shares. Then the entries apply in
input order:

| Earlier change | Later destinations |
|---|---|
| A value replaced | stay valid |
| An insertion that fit | in that leaf, searched again (its length changed) |
| A split, a raise included | that leaf's and every separator are dropped; other leaves stay |
| An insertion from the root | all dropped: the leaf it split is unknown |

A full leaf goes through `insert_with_stack`, which searches again with a
node stack before splitting. A batch starts at an internal root, so a
raise moves no saved leaf. At 10M, insertions from the root and the
destinations they drop touch 0.003% of inputs. The shared prefix shrinks
just before each entry applies: shrinking it for the whole batch first
would change the prefixes an earlier split fits.

## Extension and set operations

`extend` keeps its signature and input order. If the size hint says the
tree stays below the cutoff, it inserts one by one; otherwise it inserts
one by one until the cutoff, then pulls sixteen entries at a time into a
stack array and calls `insert_many`, whose append check decides per
chunk. A partial last chunk goes one by one. Entries above 256 bytes
(`[Option<(K, V)>; 16]` over 4 KiB) never batch. `collect` and `From`
still sort and build (`tree/build.rs`).

The set operations' lookups go through `LookupsAhead`: when it runs out,
it clones the iterator, looks its next 32 elements up with
`contains_many`, and keeps one bit per element; each `next` then advances
the real iterator and takes the next bit. Fewer than 32 left, or a set
below the cutoff, and it looks up one element at a time.

## Single-key compilation

Adding batch callers to shared code changed how LLVM compiled the
single-key paths. These keep them as they were:

- A zero-sized call-site marker passes through `Node::find` and
  `Leaf::find`: single-key callers pass `()`, the batch `BatchSearch`.
  Sharing one instantiation outlined the single-key lookup: 40–43 more
  instructions per 10M hit.
- `Tree::insert`, `Node::search_for_insert` and `rightmost_append` are
  `inline(always)`. Without the attribute on `rightmost_append`, a 10M JSON
  random insert takes 24 more instructions and 79 ns more.
- `shrink_shared_prefix_for` is `inline(always)`: the batch's inserts
  outlined it, which also cost a 10M JSON random insert about 75 ns.

Single-key `get` hit and miss run the same instructions as before on
`u64`, and 10–31 fewer on strings and `hash32` (the window above). Single
inserts match `HEAD`'s time on all four key kinds.

The insert path has a cliff of about one DRAM latency that its inlining
moves around: with `rightmost_append` forced inline, 10M `hash32` random
inserts take 288 ns and JSON 230; without, `hash32` 175 and JSON 310, with
the same instructions and lines from memory. `HEAD` before this work has
the first state too. Which miss stops overlapping is open.

## Measurements

Ryzen 9 9900X; measured thread pinned to CPU 2, setup off CPUs 2 and 14;
mimalloc, native code, fat LTO, `u64` values, seeded inputs. One run per
cell, never averaged; lookup cells time one million probes. Runs whose
control fetched more lines from memory than usual were discarded and run
again once the machine was back (the slow mode after a large process
exits). † marks the other hardware thread on the core being busy at some
point in the cell, setup included.

**10M entries, ns per key.** Lookups take 64 keys a call, 32 for `hash32`;
inserts are random-order `insert` against `extend`. Hashbrown takes one key
a call (it has no batch API), from the consolidation run of the
level-by-level work, same harness.

| Keys | Operation | One key a call | Batch | Speedup | Level by level | hashbrown |
|---|---|---|---|---|---|---|
| u64 | hit | 130.0 | 74.8 | 1.74× | 60.8 | 17.1 |
| u64 | miss | 93.2 | 64.5 | 1.45× | 49.7 | 7.3 |
| u64 | insert | 136.6 | 93.9 | 1.45× | 85.5 | 21.8 |
| JSON | hit | 219.2 | 86.7 | 2.53× | 72.9 | 87.6 |
| JSON | miss | 158.5 | 80.3‡ | 1.97× | 57.8 | 34.7 |
| JSON | insert | 230.5† | 146.1† | 1.58× | 143.4 | 65.4 |
| hash32 | hit | 155.8† | 81.0 | 1.92× | 67.6 | 70.3 |
| hash32 | miss | 109.3 | 61.9† | 1.77× | 52.3 | 39.6 |
| hash32 | insert | 289.9 | 119.9 | 2.42× | 109.4 | 42.9 |
| wiki | hit | 419.4 | 259.3† | 1.62× | 229.1 | 101.4 |
| wiki | miss | 341.7 | 225.6 | 1.51× | 201.9 | 47.4 |
| wiki | insert | 389.4† | 286.8 | 1.36× | 278.3 | 83.3† |

‡ 6.67 lines from memory per key against the usual 5.6: the slow mode.
The previous build, with the same lookup code, measured 72.1 ns at 5.63.

## Level by level: the measured ceiling

The scheduler built first, and measured faster, is level by level: a step
searches every live lane at one depth, then follows every lane's chosen
edge; every leaf is equally deep (D17), so the first live lane's edge
decides the next level's kind for all of them. No executor, no futures:
plain loops over a `[Lane; N]`. It was replaced because a lane that reads
like one lookup is simpler to read and to extend (inserts, ranges, mixed
operations, lanes refilled as they finish), at the cost below. Its code is
not in the repository; this section and D53 describe it.

| 10M, ns per key | Level by level | Async lanes | Gap |
|---|---|---|---|
| u64 hit / miss | 60.8 / 49.7 | 74.8 / 64.5 | +14.0 / +14.8 |
| JSON hit / miss | 72.9 / 57.8 | 86.7 / 72.1§ | +13.8 / +14.3 |
| hash32 hit / miss | 67.6 / 52.3 | 81.0 / 61.9 | +13.4 / +9.6 |
| wiki hit / miss | 229.1 / 201.9 | 259.3 / 225.6 | +30.2 / +23.7 |
| `extend`, u64 / JSON / hash32 / wiki | 85.5 / 143.4 / 109.4 / 278.3 | 93.9 / 146.1 / 119.9 / 286.8 | +8.4 / +2.7 / +10.5 / +8.5 |

§ The previous build's clean measurement of the same lookup code.
At 1M, Codex's reference lane trailed by 7–10 ns per hit (`u64` 27.2
against 36.0).

Where the time goes, on 10M `u64` hits (339 against 416 cycles per key):

- **55.5 cycles more waiting on a load**, 20.0 more retiring instructions,
  1.3 other. Async runs about 150 more instructions per key, but with all
  nodes in cache the gap is 0.16 ns: the instructions overlap with misses.
- **Less overlap, same traffic:** 6.3 misses in flight on average against
  8.1, at the same lines from memory. Instruction samples put the waiting
  on the leaf's loads, which see about 11 open requests against 16.
- **Not the cause:** resume mispredictions (0.0006 per key), the
  coroutines' own state (no L2 misses in the samples), page walks, future
  size, deferring ties to a final pass, an extra leaf stage, more poll
  sites, and 32 lanes instead of 64.
- **Compiler share:** nightly's native coroutines, `yield` in place of the
  awaited `Prefetcher`, cut the gap from 13.8 to 10.4 ns (42 fewer
  instructions, no awaiter flag). The remaining 10 ns is per-lane
  scheduling as built, not a proven floor: level by level shares each
  level's kind decision and its tight loops across the batch, which a
  generic round-robin executor can't know is safe.

What makes the leaf loads enter flight later in async is not known. One
hypothesis to test: a lane branches on leaf data that hasn't arrived, and a
misprediction there flushes the younger lanes' loads.

## Limits and open problems

- Misses stay behind hashbrown: a hash miss is usually rejected by one
  group of control bytes; a tree miss reaches a leaf. At 10M, batched
  misses fetch about 5.6 / 3.1 lines from memory per JSON / `hash32` key,
  hashbrown 1.4 / 0.8.
- `u64` hashing has a short chain and little search work, and independent
  hash lookups already overlap in the reorder buffer. Batching can't remove
  the tree's levels.
- Inserts still pay leaf shifts, splits, allocation and ordered search.
- Wiki titles average about 2.3 full-key comparisons per 10M hit in
  internal nodes, nearly all reading heap bytes; they stay serial inside
  node search.
- The machine has an unexplained slow mode after some large processes
  exit: identical code fetches 0.65–1.0 more lines per key. Controls catch
  it; nothing is averaged away.

## Verification

Tests compare batches with individual calls, and inserted trees node by
node (`tree/batch.rs`, `unsync/map/tests/batch.rs`, `unsync/set/tests.rs`), including
more than 64 lanes, panicking lanes in the executor, and set operations on
a set past the cutoff. The differential fuzz targets
(`bbtree/fuzz/src/map_differential.rs`, `bbtree/fuzz/src/set_algebra.rs`) cover
`get_many`, `insert_many`, `contains_many` and `extend` on every tree size,
the cutoff being off under fuzzing. The bench has `batch_get_hit32/64`,
`batch_get_miss32/64`, `batch_random_insert`, `batch_sequential_insert`
(16 a call), `extend_random` and `extend_sequential`.
