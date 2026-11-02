---
status: plan
summary: lane db's speed of light: the bound each component's work and the hardware set, the arithmetic, how the design reaches 90% of it, and what stands in the way where it doesn't yet.
date: 2026-10-29
---
# Lane db: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component of lane db has a section here,
written by the agent that owns it. Add yours as a new `##` section; don't rewrite another's.

The machine figures, for one core of the target machines (EPYC Turin, Graviton 5) unless noted:
about 3.5 GHz, an L1 hit in 4 to 5 cycles, L2 in about 14, L3 in about 50 (14 ns), DRAM in
about 110 ns, and branchy pointer code at about 2 instructions per cycle. A core keeps about 20
cache misses in flight, so independent misses cost about 110 / 20 = 5.5 ns each in throughput.
DRAM bandwidth: Turin's 12 channels of DDR5-6400 give 614 GB/s peak per socket, about 480
achievable; one core streams about 30 GB/s. Turin has up to 192 cores and 32 MB of L3 per CCD.
Figures in ns use 3.5 GHz.

## vendor/salsa: the front end's query database (speed-salsa)

Costs are counted from the code as of 10-29 20:40, not measured: S1–S4, S5's edges and
claim-free verify, the 24-byte memo header and memos as columns are in the tree, none of it built
yet. The before/after and benchmarks are in [speed-salsa.md](speed-salsa.md). "Off" is today's
cost over the bound. A "line" is a dependent cache-line load: about 4.5 cycles from L1, 110 ns
from DRAM.

### Memo hit (`fetch`: value present, verified this revision)

The most frequent operation in a warm build.

- **Work:** find the memo of (function, key), compare `verified_at` with the current
  revision, take the value, and record the read.
- **Bound:** the memo's address is computable from the key, so the work is 1 line for the row
  (its base cached per task, since rows never move), 1 for the memo, and about 20 instructions
  (address arithmetic, the revision compare, a value-present test, the read's record: a compare
  with the last edge and a 4-byte store). Hot: 2 × 4.5 + 20 / 2 ≈ 19 cycles, **5.4 ns**. Cold:
  independent callers' misses overlap, about 20 in flight, so **≈ 5.5 ns** per hit per core in
  throughput, 220 ns latency.
- **Today (rows by page, 20:40):** 4 lines on the address chain, 4 deep:
  1. the `boxcar` bucket;
  2. the `Page`, which keeps its ingredient's column slice and its first memo row
     (`memo_rows`, 128 rows taken from the ingredient's `MemoTableTypes` when the page is made)
     beside its `Arc`;
  3. the `Column`. Its first line holds the rows region's base and committed length and the
     type id (`repr(C, align(64))`, held by a `const` assertion);
  4. the row pointer, at `rows.base + 8 * (memo_rows + slot)`: computed from the page line and
     the `Id`, no load of the slot;
  5. the memo, at `slab.base + size * slot`.

  The slot is no longer on the chain. An input's hit doesn't touch it. A tracked struct's read
  lock and an interned value's validation still load it, in parallel with the column, and
  nothing waits on it but the branch. About 42 instructions. Hot: 4 × 4.5 + 21 ≈ 39 cycles,
  11 ns, **2× off**. Cold: 4 dependent misses, 440 ns latency, **2× off**; but a hit now takes
  one line fewer (4 against 5), so it takes one in-flight miss fewer, ≈ 5.5 ns less in
  throughput per cold hit, and it keeps one line fewer in L1. Before this change the slot was
  loaded beside the column for its 4-byte row number: 5 lines, the same depth. With columns on
  boxed pages it was 6 lines, 5 deep; before columns, 7 lines, 7 deep (ingredient cache,
  bucket, `Page`, slot, the slot's memo table array, the `MemoEntry` pointer, the boxed memo),
  18 ns hot.
- **What stands in the way, and the change that removes it:** the bucket and the `Page`, two
  lines found again on every hit though neither moves once published. The change: a per-task
  direct-mapped cache of `table page → (column slice, memo_rows)`, 64 entries of 24 bytes beside
  the node cache, filled on a miss. A hit on a page the task has seen is then the cache entry
  (L1), the column, the row pointer and the memo. The column line is the same for every key of
  a function, so it stays in L1 across a task's hits. That leaves 2 lines that miss, the row
  and the memo, which is the bound. A page's entry can't go stale: a page and its
  `memo_rows` never change once published, and the database outlives the task. It changes
  `Zalsa::memo_table_for`'s callers (fetch, maybe_changed_after, execute, specify, function.rs:
  about 15 sites) to pass the task's `ZalsaLocal`. Owner: speed-salsa, next.
- **Today (10-30, sol-salsa, written blind):** the cache is in (`table/page_cache.rs`, 64
  entries of 48 bytes in `ZalsaLocal`). An entry copies the page's slots pointer, slot vtable
  entry, column slice, first row and allocated count, so it needs no `TypeId` and no change to
  the generated `memo_table`: the slot is validated through the page's vtable as `dyn_memos`
  does, and the memo's type is still checked by its column. The hit path (`fetch_hot`,
  `fetch_unclaimed`, `maybe_changed_after`) is the entry, the column, the row and the memo:
  **at the bound** for a task that stays on 64 pages, plus one page read per entry miss. The
  other `memo_table_for` callers (execute, specify, outputs, LRU) are off the hit path and
  unchanged. Measured by `benches/memo_hit.rs` once it builds.

### Recording a read

- **Work:** append the dependency to the task's frame, once per distinct key.
- **Bound:** a compare with the last edge (S4), else an L1-resident probe of the frame's set
  (about 10 cycles), and a 4-byte store of the dependency's node. **≈ 3 ns** for a new key, 1 ns
  for a repeat.
- **Today (S5):** the frame holds 12-byte `QueryEdge`s. When the query finishes, each edge
  becomes a node through a probe of the task's 1024-entry node cache (L1 on a repeat key) or of
  the shared node table (a lock-free probe: 1 to 2 lines, plus the key columns on a tag match).
  That is about 15 ns per distinct edge on a cache miss, **≈ 5× off** for keys the task hasn't
  seen.
- **What stands in the way:** the read doesn't know the dependency's node. The 24-byte header
  has no room for a 4-byte node (the origin word's span uses 61 bits). The change: a node
  region beside each column's rows (4 bytes per row at `4 * row`, the row computed from the page
  as for the row pointer), set when the row's first memo is published, plus a node column
  beside input, interned and tracked struct slots. Then the frame records the node directly and
  finishing a query copies words with no probe. Owner: speed-salsa, with the per-task page cache.
- **Today (10-30, sol-salsa, written blind):** `u32` revisions freed 4 bytes in the header, so
  the node lives in the memo (`MemoHeader::node`, set at publish) instead of a region beside the
  rows: a fetch already holds the memo, so the read records `QueryEdge::input_at(key, node)` and
  `Graph::encode` writes the word without a probe. The frame's set is rustc's dedup
  (`active_query::EdgeSet`: a scan of up to 8 edges, then a hash index), not an `IndexSet`
  insert per read. A read is a compare with the last edge, a scan of at most 8 edges (2 lines)
  and a 16-byte push; **≈ 1.3× off** the 4-byte store (the edge still carries its key for
  cycles, persistence and `Eq`). Reads of inputs, tracked fields and interned values still find
  their node at finish (they have no memo); they are mostly repeats in the task's node cache.
  Measured by `benches/record_read.rs` once it builds.

### Edges at rest

- **Work:** remember, for every memo, the ordered list of what it read.
- **Bound:** an edge names one of N nodes plus its kind: ⌈log2 N⌉ + 1 bits. Nushell (#734) has
  about 100M edges; at N ≈ 10M nodes that is 25 bits, **≈ 312 MB**.
- **Today (S5):** 4-byte words in one paged array, 400 MB, **1.28× off**. Before S5 it was 8
  bytes an edge plus one allocation per memo, about 0.8 GB. With the node table (0.2 to 0.27 GB
  at 10M keys) the graph is 0.6 to 0.67 GB.
- **The change (in, 10-30, sol-salsa-pack, written blind):** compaction (`Graph::compact`, with
  `&mut` at a new revision once the edges are twice what is live) writes every live range into
  a settled region bit-packed at ⌈log2 nodes⌉ + 1 bits, one width per region; the fresh array
  starts past it, so a span's start says its form and `EdgeSpan` stays 8 bytes. Every reader
  goes through `EdgeWords`: `get` is one 8-byte load, a shift, a mask and a rotate; `iter`
  unpacks 8 at a time with AVX2 or NEON for spans of 32 or more. Fresh chunks stay 4 bytes until
  the next compaction. Settled edges are within 1 bit of the bound (25 bits at 10M nodes, 4%
  over the 24-bit node); fresh words since the last compaction are at most as many again as
  what is live, so the store between compactions is 25 bits an edge settled plus 4 bytes for
  each fresh word. Bench: `benches/edges_at_rest.rs` (bytes an edge at 10M nodes; settled
  against word iteration, target within 10%), not yet run.

### Memo at rest

- **Work:** what a memo must keep: `verified_at`, `changed_at`, durability and flags, its value,
  its edge range.
- **Bound:** with revisions as `u32` (enough for the daemon's lifetime) and the edge length in
  16 bits: 4 + 4 + 1 + 4 + 2 ≈ **16 bytes** plus the value. The 16-byte value fingerprint
  belongs in a side table, only for memos that have one.
- **Today (19:55):** the 24-byte header and the value in its column's slab, no box; an 8-byte
  row pointer in its own column only; nothing in the slot, since the row is the page's first row
  plus the slot index (20:40; the 4-byte row number it replaced sat in padding, so no struct
  shrank). For an 8-byte value that is 40 bytes against 24, **1.67× off**. Before:
  48 bytes boxed, 8 per sibling function and a 16-byte table per slot, 2.3× and more.
- **What stands in the way:**
  1. `Revision` is a `usize`, 8 bytes in `verified_at` and in the stamp. Narrowing it to `u32`
     touches every revision compare in salsa; it saves 8 bytes per memo.
  2. The row pointer. A replaced memo can't be overwritten in place because `&'db` references
     to it live for the whole revision, so the new memo takes a new slot and the row points at
     it. Removing the pointer needs memos written in place, which needs readers to copy small
     values out instead of borrowing them: a change to every `returns(ref)` caller in the front
     end. Not worth it while the pointer is 8 bytes against values that are mostly larger.

  Owner of 1: speed-salsa, after the first compile.
- **Today (10-30, sol-salsa, written blind):** 1 is in: `Revision` is a `NonZeroU32`
  (`revision.rs`; `next()` stops before `u32::MAX`, 4 billion revisions). `verified_at` is 4
  bytes and the stamp's changed-at fits its field, but the header stays 24 bytes: the freed 4
  bytes now hold the memo's node, which recording a read and painting its color need. So the 8
  bytes saved went to removing a node lookup per read, not to the header; against a bound that
  counts the node (16 + 4 + value) an 8-byte value is 40 against 28, **1.43× off**, the row
  pointer (2 above) being what remains. Inputs and tracked structs did shrink: their per-field
  revisions are 4 bytes (`tests/memory-usage.rs`: `MyInput` 48 to 24, `MyTracked` 64 to 32).

### Deep verify (a warm build after an edit)

- **Work:** for each memo that may have changed, walk its edges in read order and check each
  dependency's color; stop at the first that changed.
- **Bound:** per edge, one sequential 4-byte edge load and one random load of the dependency's
  color. An edge list's colors are independent loads, so all can be issued before they are
  checked in order. All of nushell's 100M edges touch 100M random lines: 6.4 GB / 480 GB/s =
  **13 ms** on all of Turin's cores if the colors are in DRAM. With a dense color column of 8
  bytes per node, 10M nodes are 80 MB, much of it in L3; the bound is then about 100M × 6
  instructions / (2 IPC × 192 × 3.5 GHz) ≈ **0.5 ms**. An edit's real walk is only what is
  reachable from the change.
- **Today (S5, claim-free verify, columns):** per edge, the node's key (a page load and two
  column loads: 3 lines), then the dependency's hit path (5 deep), all dependent: ≈ 8 × 110 ns
  = 880 ns per edge cold. All 100M edges on 192 cores take ≈ 460 ms, **≈ 35× off** the DRAM
  bound.
- **What stands in the way:** the color lives in the memo, which is found through the key.
  rustc keeps it in `DepNodeColorMap`, a dense array by node. The change: a node-indexed column
  of `verified_at: AtomicU32` and `changed_at_and_durability: AtomicU32`, written wherever the
  memo's are (publish, `mark_as_verified`, an input's setter, interning, backdating). Deep
  verify then reads edge words and colors only, prefetching the next 8 edges' colors as it checks
  one, and goes to a memo only for a node not verified this revision. That is 1 random load per
  edge, the bound. A color written late or missed is a wrong incremental result, not a slow
  one, so it is tested differentially against the memo path (every verify run both ways in
  debug builds) before the memo path is dropped. Owner: speed-salsa, after the first compile.
- **Today (10-30, sol-salsa, written blind):** the color column is in (`graph/colors.rs`): one
  `u64` per node beside its key columns, verified-at, an accumulated bit and changed-at, raised
  only by `fetch_max` at publish, at `mark_as_verified`, for input fields by the walk itself, and
  to "unknown" for provisional memos and when a slot's memos are taken. A color is trusted only
  to say "unchanged" in the current revision; anything else goes to the memo, so a missed write
  is a slower check, not a wrong result, and the memo path stays as the decider (debug builds
  check every trusted color against it, `check_color`). The walk reads edge words, prefetches
  colors 8 edges ahead, and finds a memo only for a node whose color can't decide. An edge to a
  memo already verified this revision is **1 random load: the bound**. The first edge to each
  memo in a revision still finds it through its key, and so does each input-field read the first
  time (≈ 8 loads); an edit's walk pays that once per distinct dependency instead of once per
  edge. Measured by `benches/deep_verify.rs` once it builds.

### Interning a hit (`intern_id`, every multi-argument query call)

- **Work:** hash the key and find its value.
- **Bound:** FxHash of a 2- to 3-word key (≈ 3 cycles), then 1 line for the group (control word
  and pointers together) and 1 line for the value, to compare the key. **≈ 2 lines, ≈ 15 cycles
  hot.**
- **Today (S2):** a table pointer (L1), one 64-byte group line with the control word and seven
  pointers, and the value line. **At the bound.**

### LRU eviction (S1, once per revision per `lru` function)

- **Work:** rank the memos that hold values by `verified_at`, and drop the oldest beyond the
  cap.
- **Bound:** 4 to 8 bytes of `verified_at` per row: 1M rows are 8 MB, 0.27 ms on one core, or
  **17 µs** at all-core bandwidth.
- **Before (10-29):** one walk of the struct's slots in page and slot order (the row number,
  sequential), the row entry (8 bytes, mostly sequential) and the memo's line (64 bytes, in slab
  order): about 100 bytes per row, 100 MB per 1M rows, 3.3 ms on one core, then a serial
  `select_nth_unstable` over the collected revisions (several ms more on 1M) and a second walk.
  It runs once per revision with `&mut`, so no reader waits.
- **Today (10-30, sol-salsa-lru, written blind):** the walk is split by table page across
  workers (`table/scan.rs`: contiguous page ranges, at least 64 pages a worker, so a database
  under 8K memos of the function starts no thread), each collecting its range's `verified_at`
  into a buffer kept between revisions. The cutoff comes from a radix select over the buffers on
  the same workers (8 bits a round over `verified_at - min`: one round, one read of 4 bytes a row,
  when the revisions span under 256, at most four), and each range drops its share of the
  at-cutoff ties from a prefix sum of the ranges' counts, so the values dropped are the serial
  walk's (`function/lru.rs`, tested against the serial ranking on random tables and worker
  counts). 100 MB / 480 GB/s ≈ 0.2 ms per walk on all cores, plus ≈ 10 µs to start each worker
  (`std::thread::scope`; the plane's pool would remove it). Measured by `benches/lru_scan.rs`
  once it builds.
- **What stands in the way:** reading the slab directly would cut it to the header's 8 bytes,
  but the slab's order depends on scheduling, and the LRU's ties must break the same way in every
  run (determinism). The walk is still 12× the bound in bytes, which stays: reaching it needs
  `verified_at` in a column of its own, a second line on every hit, and the hit runs millions of
  times per revision against this scan's once. Owner: speed-salsa.

### Claims (single flight, once per execution)

- **Work:** make sure one task runs a key at a time.
- **Bound:** one CAS on the key's own word to claim, one store to release, plus a wake only if
  someone waits: about 2 × 20 cycles uncontended, **≈ 12 ns**.
- **Today (claim words, 10-30, written, not built):** a claim word per row (`ClaimWord`, a
  `usize`) in a third region of each memo `Column` (vendor/salsa/src/table/memo.rs), at
  `row * 8`, kernel-zeroed and committed as rows are, found from the page exactly as the row
  pointer is. It holds `FREE`, the claiming task's token (its `ZalsaLocal`'s address) or `SLOW`
  (the key's state is its shard-table entry). The uncontended claim is one CAS `FREE → token`
  and the release one CAS back (vendor/salsa/src/function/sync.rs): no mutex, no hash. Finding
  the word is the page line and the column's second line, both loaded by the memo lookup just
  before, plus a 4-entry per-function cache of struct ingredient → column, about 15
  instructions: 2 × 20 + 15 / 2 ≈ 48 cycles, **≈ 14 ns, 1.15×**, within the 10% once the word's
  address is taken from the memo lookup's own row instead of recomputed (the per-task page
  cache gives both). The shard table now holds only contended, peeked, transferred and
  twice-claimed keys: a contender turns the owner's token into `SLOW` with a CAS under the shard
  lock and makes the entry, so the owner's release CAS fails and it takes the lock; every
  change but the two fast CASes is made under the lock, so a `SLOW` word always has its entry.
  Blocking, cycle detection, transfer and panics then run through the entry and the dependency
  graph as before. Bench: `vendor/salsa/benches/claim.rs`. Before (S3): a shard `Mutex` at 4
  times the cores plus a `hashbrown` insert and remove, about 4 atomic operations and two
  probes, ≈ 40 to 60 ns, 4× off for an accessor-sized query.
- **What stands in the way:** recomputing the word's address from the `Id` (page, cache,
  column) after the memo lookup already did; it goes with the per-task page cache. Owner:
  speed-salsa; db-salsa owns cycle handling, which reads claims.

### Unaffected memos, walk order, the graph in the image (lane graph, 10-30, written blind)

Design, exactness arguments and arithmetic: [graph.md](graph.md).

- **Re-verifying a memo no edit reached.** The work is to prove it unchanged. The bound is a
  constant per memo, not a walk of its reachable edges. The change is a 64-bit input-closure
  filter per node, checked against a log of what changed by durability: two loads and an AND,
  ≈ 20 ns with the memo lookup. Today, before it, the memo pays the walk at 1 to 5.5 ns an
  edge. With it, a memo whose filter is not all ones is at the bound. Interned arguments that
  can be reused make the filter all ones (no shortcut) for most front-end memos above
  multi-argument calls (graph.md, "Where it stops paying").
- **The walk's layout.** The bound is ≈ 0.7 to 1 ns an edge, compute-bound, with colors read in
  walk order. Today they are read in key-discovery order: ≈ 5.5 ns an edge from DRAM. The
  change is DFS preorder numbering (`graph/order.rs`), applied to the frozen graph now; the live
  graph's turn is with compaction.
- **The graph at rest in the image.** The bound is ≈ 1 to 1.5 bytes an edge, mapped with no
  decode, at ≈ 2.5 G edges/s read. It is written as StreamVByte deltas in read order, ≈ 1.59
  bytes an edge computed (the count byte is the 0.1 over), and read in place.

### Summary

| component | bound | today | off | removes the gap |
|---|---|---|---|---|
| memo hit | 2 lines, 5 ns hot | the per-task page cache, written blind: cache entry (L1), column (L1), row, memo; 2 lines that miss (was 4 lines, 11 ns) | 1× (a page read per cache miss) | — |
| record a read | 4-byte store, 3 ns | the memo's node rides on the edge, no probe at finish; rustc's scan-then-hash dedup; a 16-byte push, written blind (was a node probe at finish, ≈ 15 ns) | ≈ 1.3× | frame edges as 4-byte node words, which needs cycles, captures and persistence to decode keys from the graph |
| edges at rest | 25 bits, 312 MB (nushell) | settled 25 bits, 312 MB, plus 4 bytes a fresh word until the next compaction (written blind, bench not run) | 1× settled | bit-packed compaction: in |
| memo at rest | 16 + value (+ 4 for the node) | `u32` revisions in, written blind: 24 + value + 8, the freed 4 bytes holding the node | 1.43× (8-byte value, against 20 + value) | the row pointer: memos written in place, which needs readers to copy small values |
| deep verify | 1 random load per edge; 13 ms for all of nushell | the color column with an 8-ahead prefetch, written blind: 1 random load per edge to a memo already verified this revision, ≈ 8 loads for the first edge to each (was ≈ 8 dependent loads every edge, 460 ms) | ≈ 1× per edge; the first visit of each dependency | colors written by input setters (needs the setter to reach the graph), so first visits of inputs are one load too |
| re-verify an unaffected memo | O(1): 2 loads and an AND, ≈ 20 ns with the lookup | input-closure filters written blind (graph.md); before them, a walk of every reachable edge | 1× where the filter isn't all ones | reusable interned arguments make most front-end filters all ones: `revisions = usize::MAX` on those functions (a reuse log is unsound: `data()` asserts the walk validated the value) |
| verify walk layout | ≈ 0.7 to 1 ns an edge (colors in walk order) | DFS preorder from the roots at every compaction, written blind: key columns, key table, colors, edge words, memo nodes and the filter column renumbered in the `&mut` pass, tasks' node caches dropped by epoch; colors in walk order from the first compaction on (`benches/verify_order.rs`, not run; was key-discovery order, ≈ 5.5 ns an edge from DRAM) | ≈ 1× after a compaction; nodes made since sit past the walk order until the next | renumber at every new revision, not only at compaction: O(nodes + live edges) each time |
| graph in the image | 1 to 1.5 B an edge, mapped, no decode | StreamVByte deltas in read order, written blind: ≈ 1.59 B an edge computed | ≈ 1.06× | drop the per-node count byte (offsets in values) |
| intern hit | 2 lines | 2 lines | 1× | — |
| LRU scan | 17 µs per 1M rows (all cores) | split by page over all cores with a parallel radix select, written blind: ≈ 0.2 ms plus ≈ 10 µs per worker started (was 3.3 ms on one core plus a serial select) | 12× in bytes, once per revision | `verified_at` in a column of its own, which costs every hit a line: not taken |
| claim | 12 ns | a claim word per row, one CAS each way, ≈ 14 ns (was 40 to 60 ns) | 1.15× | the word's address from the memo lookup's row (per-task page cache) |

As of 10-30 (sol-salsa and its children, all written blind, building.md section 19), interning,
the memo hit and deep verify's edge check are at their bounds; the claim word, the node on the
edge, `u32` revisions and the parallel LRU scan are in, with the gaps left named above.
Bit-packed compaction is sol-salsa-pack's. Rows by page are in (20:40): the row comes from the
page, not the slot.

## hir-ty's trait selection and trait-error leaf search (db-ty-port-infer-errors)

Files: `crates/hir-ty/src/next_solver/infer/{select,errors,traits}.rs`. Nothing was run: the
costs below are counted from the code and from `vendor/rustc_type_ir/src/search_graph/mod.rs`,
not measured. Unit costs used: a global solver-cache hit (canonicalize a small goal, about 60
instructions, plus one table probe of 2 lines) is about 30 + 9 = **≈ 40 cycles hot**. Unifying
an impl header with a goal is about 200 instructions, **≈ 100 cycles**. A small heap
allocation plus its free is **≈ 20 ns**.

### Obligations (`traits.rs`)

- **Work:** carry a goal plus its span and depth through fulfillment, one per nested goal.
- **Bound:** the information is a `Span` (3-bit tag + `u32` index), a `ParamEnv` and a
  `Predicate` (one 4-byte interned handle each), and a depth no larger than the crate's `u32`
  `recursion_limit`. Kept as separately addressable 4-byte fields: 8 + 4 + 4 + 4 = **20 bytes**.
  Packing the tag into the depth word would reach 17 but caps `recursion_limit` at 2^29, which
  rustc doesn't. So 20 bytes is the bound.
- **Today:** was 24 (`recursion_depth: usize`, 8 bytes plus alignment), **20% off**. Now `u32`
  and `const`-asserted at 20: **at the bound**. fulfill.rs's SoA depth column goes from 8 to 4
  bytes per pending obligation. This needs the fulfill.rs edit requested in
  `v2/requests/db-ty.md`.

### `select` (`select.rs`)

`find_matching_impl` calls it behind the `lookup_impl_method` salsa query, so it runs once per
distinct (method, args) pair. It also runs from method-probe ambiguity reporting, which is cold.

- **Work:** find which candidate proves `T: Trait` and the impl's args.
- **Bound:** the solver's global cache keeps a goal's result, not its candidates. So the root
  goal's candidate assembly must run once: one simplified-type impl lookup (2 lines). Then, for
  each of k candidates, one header unification plus n nested goals that hit the global cache.
  The proof-tree builder is noop below the root (`eval_ctxt/mod.rs`, `evaluate_goal` passes
  `ProofTreeBuilder::new_noop()`), so these do hit it. The chosen impl's args are rematched
  once. With typical k = 2, n = 3: 2 × 100 + 6 × 40 + 100 ≈ 540 cycles, **≈ 0.15 µs**.
- **Today:** exactly that evaluation (`visit_proof_tree` makes one uncached root evaluation),
  plus the proof-tree recording. That is a `Vec` of probe steps per probe, a `ThinVec` of
  original values, and in `inspect.rs`'s `candidates_recur` a `steps.clone()` per candidate plus
  the candidates `Vec`. That makes about 6 to 8 allocations, ≈ 0.15 µs, so **≈ 2× off**.
- **What stands in the way:** allocation in proof-tree recording, not the search. To remove it:
  - `InspectCandidate` keeps a range into one shared step buffer instead of a cloned `Vec<&ProbeStep>`.
  - The builder's step vectors come from a per-evaluation bump arena.

  These belong to inspect.rs (db-ty-port-solver) and the vendored builder (solver-sync). The
  first is requested. With both: ≈ 2 allocations, ≈ 0.19 µs total, **within 10%**. A fast path that
  first asks the cached `evaluate_root_goal` was considered and rejected: `select`'s caller is
  success-dominated, and on success it would evaluate the root twice.

### Trait-error leaf search (`errors.rs`, `BestObligation`)

This is cold: it runs only for an obligation that is reported as an error. A build with no
errors never runs it.

- **Work:** walk from the failing root goal to the leaf obligation to blame, at most
  `max_depth` = 10 levels.
- **Bound:** a level's nested trees are not recorded, so each level costs one proof-tree root
  evaluation of the goal (≈ 0.15 µs as above). Picking among the c candidates needs the results
  of only the nested goals whose source is `Normalization` (rustc's rule). Following the chosen
  candidate needs its n nested results. So per level: (1 + n + #normalization) evaluations. For
  c = 3, n = 3 and 1 normalization goal per candidate, that is 7 × 0.15 ≈ 1 µs a level, **≤ 10 µs
  per error**.
- **Today:** two gaps.
  - `non_trivial_candidates` builds the proof tree of every nested goal of every candidate when
    c > 1: (1 + c × n + n) = 13 evaluations a level, **≈ 1.9× off**.
  - `InferenceTable::has_trait_errors` (infer/unify.rs) runs the whole search for each pending
    error and `emit_trait_errors` runs it again, at least **2× more**.
- **What removes it:** both were requested in `v2/requests/db-ty.md`.
  - `InspectCandidate::instantiate_nested_goals_from(span, keep)` (inspect.rs) still advances
    every step's canonical state but builds trees only where `keep(source)`. errors.rs then
    passes `|s| s == GoalSource::Normalization`.
  - unify.rs converts each error once.

  With both, the search is at the bound.

| component | bound | today | off | removes the gap |
|---|---|---|---|---|
| obligation | 20 bytes | 20 bytes (was 24) | 1× | done; fulfill.rs column to `u32` requested |
| `select` | ≈ 0.15 µs (one root evaluation) | ≈ 0.3 µs | 2× | step ranges in inspect.rs, arena for the proof-tree builder |
| leaf search | ≤ 10 µs per error | ≈ 2× that (10-30) | ≈ 2× (cold path) | the second search per error, which is not a cache (below) |

Recounted 10-30 by sol-all-ty, nothing built:
- **Leaf search.** `InspectCandidate::instantiate_nested_goals_from` had landed.
  `non_trivial_candidates` (`next_solver/infer/errors.rs`) now uses it with
  `|source| source == GoalSource::Normalization`. It read only those goals' results, so the
  answer is the same, and the trees of the other nested goals are no longer built. That is
  1 + c × 1 + n = 7 evaluations a level, at the bound.
- **The second search** is still there. `has_trait_errors` (from `tainted_by_errors`, during
  inference) runs `SolverDiagnostic::from_fulfillment_error` and `emit_trait_errors` runs it
  again at the end. Each error is already converted to a `FulfillmentError` once. Keeping the
  first answer would not be exact, because the search reads the inference state, which
  changes between the two. So the row stays ≈ 2× on a cold path. Removing it needs rustc's
  order (taint from the error's existence, report at the end), which changes what
  `tainted_by_errors` returns mid-body, and that is a semantics decision.
- **`select` 2×.** The step ranges and the proof-tree builder's arena are in
  `vendor/rustc_next_trait_solver`, which is the db lane's, not sol-all-ty's. Not done.
- **Cached root goal ≈ 10×** needs a measurement, as the brief says. Not touched.

State of the art: rustc's new solver (`rustc_trait_selection::solve::select`,
`fulfill/derive_errors.rs`) does the same work with the same per-candidate `Vec`s. The changes
above beat it on allocation and on the error path.

## hir-def signatures: `signatures.rs` (speed-hir-defs-sigs)

What this file does:
- lowers every item's signature (`StructSignature` through `TypeAliasSignature`),
  `VariantFields` and `EnumVariants` from the item's own outline tree (`SignatureSyntax`);
- moves the resulting source map back to the file on request;
- answers every read of a signature.

Costs below are estimates from the code as of 10-29 20:00, not measurements. `proof`'s budgets
measure them once things compile. The store and generic params it lowers into are
speed-hir-store's, and the per-query costs are salsa's (section above). Only what this file adds
on top of those is counted against it.

The sizes used, for a typical item:
- **A fn's outline** (signature, body left out): 40 elements and 120 bytes of text. Its
  flat-tree words are 4 bytes each, so 160 B of words plus 120 B of text: **280 B**.
- **A 20-method impl:** about 820 elements and 4 KB of text, of which the impl's own outline is
  about 15 elements.
- **A file:** 150 items.

### Reading a signature (`X::of`, a memo hit)

- **Work:** return the memo's value.
- **Bound:** one salsa memo hit, 5.4 ns hot (above). This file adds nothing to it.
- **Today:** `returns(deref)` hands out `&X` with no `Arc` clone, so this file's part is at the
  bound. The hit itself is 18 ns, all of it salsa's (MemoRows and the row-base cache take it to
  the bound).

### `signature_syntax`: once per item per edit of its file

- **Work:** decide whether the item's outline changed, and keep a copy of it when it did.
- **Bound:**
  - read the outline from the file's tree (280 B) and write a copy (280 B);
  - compare it with the old copy when salsa compares the values (read 2 × 280 B);
  - that is 1.1 KB through L1/L2 at about 50 GB/s: **22 ns**;
  - one allocation for the copy and its maps, about 10 ns;
  - the outline's few ast ids and span anchors, about 10 ns;
  - salsa's share of one execution at its bound: claim 12 ns, 4 reads × 3 ns, publish about 10
    ns, so about 35 ns.

  Total **≈ 80 ns per fn**.
- **Before this pass, ≈ 1.0 µs, 13× off:**
  - three memo hits (span map, ast id map, source) plus a walk to the node: 85 ns;
  - finding the anchor: 20 ns;
  - `clone_subtree`, which allocates about 10 columns and copies the whole item: 110 ns, plus
    the whole parsed body when the fn's body was lowered (`subtree` copies parsed lazy parts),
    and an impl's or trait's whole item list;
  - `replace_subtree` repacking the copy, with its own 10 columns: 185 ns;
  - the empty node's tree and the text copy: 65 ns;
  - **two SipHash-1-3 passes over the tree** (`tree_hash`): 2 × (300 B × 1.5 cycles + 50) ≈
    1,000 cycles, **285 ns**;
  - `relative_without` walk: 77 ns;
  - `within`: 30 ns;
  - salsa's execution today: 140 ns.
- **Applied now:**
  - The hash is gone. `SignatureSyntax` compares its tree by content (`FlatTree: PartialEq`),
    which reads each byte once, about 15 ns. It runs only when salsa compares a re-run's value,
    and it is exact where a 128-bit hash could collide and reuse a stale signature.
  - Result: ≈ 0.75 µs, **9.5× off**.
- **Applied since** (over frontend-text's `clone_subtree_without`, `v2/requests/frontend-text.md`):
  - the copy is made in one pass over the elements outside the elided node, never reading or
    copying the elided part;
  - that replaces the clone, the repack, the empty tree and the text copy (360 ns) with about
    120 ns, giving ≈ 0.5 µs, **6.5× off**;
  - for a fn whose body was lowered, or a large impl, it also removes the cost that grows with
    the body or the item list (about 1.5 µs for a 20-method impl);
  - and it now runs only when the item's part of the file outline changed (next section), not
    on every edit of its file.
- **What stands in the way after that:**
  - **About 25 allocations per `SignatureSyntax`, ≈ 250 ns:** the tree's 10 columns, the ast
    id map's and span map's vectors, and the `Arc`. Reaching the bound's one allocation means
    building all three into one block. That is a layout change in `FlatTree`, `AstIdMap` and
    `RealSpanMap` (frontend-text, span), or a per-revision bump arena for memo values (salsa).
  - **Salsa's execution cost, 140 ns → 35 ns:** speed-salsa's claim word and node-in-row
    changes.
  - **The structural gap, below:** every item of an edited file runs at all.

### An edit of a file: what signatures re-run

- **Work:** a body edit changes no signature, and an outline edit changes one item.
- **Bound:** for a body edit, establish that no outline changed. That is one pass comparing the
  file's outline with the old one: 150 × 280 B = 42 KB read twice, 84 KB / 50 GB/s =
  **1.7 µs**, plus one salsa execution, so **≈ 2 µs per file**. For an outline edit, add one
  item's `relative` lowering.
- **Before the relative job:** every signature, `VariantFields` and `EnumVariants` of the file
  lowered again: 150 × about 2 to 3 µs of lowering, the signature, its fields and its
  variants each re-run and backdated, ≈ **300 to 450 µs**, about 200× off.
- **Before `file_outline`:** each item's `signature_syntax` read the file's parse, so all 150
  ran and backdated on any edit of the file: 150 × 0.75 µs ≈ **110 µs**, 55× off.
- **Applied: `file_outline(db, EditionedFileId)`** in this file.
  - It holds the file's tree with the body of every fn, const and static that is its own
    anchor left out, its ast id map and span map, and where each body was cut.
  - It is made over the list forms of `clone_subtree_without`, `AstIdMap::relative_without` and
    `RealSpanMap::without`:
    - one walk of the file's items, about 300 nodes, ≈ 1 µs;
    - the copy, one pass over the outline's elements and text, 42 KB in and out, ≈ 3 µs;
    - the ast id map's walk of those elements, about 6,000 at ≈ 1.5 ns, ≈ 9 µs;
    - the span map's one pass, under 1 µs.
  - It is compared by content, so it backdates on every body edit. Positions in it don't move
    when a body's length changes, because bodies aren't in it. The comparison is 84 KB read,
    ≈ 1.7 µs.
  - `signature_syntax` and `variants_syntax` read it and not the parse. On a body edit, this
    one query runs. Each item's memo then costs a deep verify of one edge: ≈ 5 ns at salsa's
    bound, 0.75 µs for the file.
- **Now:** ≈ 15 µs per body edit of a 150-item file for the syntax this file reads.
  - The signature queries also read `AttrFlags::query`, `repr` and `repr_assume_has`, which
    read the item off the live parse and ast id map. So every item's attribute query re-runs
    and backdates on a body edit: 150 × 300 ns ≈ **45 µs more**, found by speed-hir-sol-defs.
  - That makes ≈ 60 µs against a bound of ≈ 2 µs: **≈ 30× off**, from 55×.
  - The attribute part goes when attributes are read off the outline (`v2/requests/db-expand.md`
    item 12, with the arithmetic in speed-hir-sol-defs' section of this file). Then this
    file's ≈ 15 µs is all that's left: ≈ 7× off.
  - **Applied (10-29 and 10-30):** attributes are read off the outline and its flags column,
    so a body edit verifies them in ≈ 1.7 µs; with this file's ≈ 15 µs, ≈ 17 µs, **≈ 8× off**
    (counted, not built).
- **What stands in the way:**
  - The outline is made again from the whole file on each edit: three passes (copy, ast ids,
    spans), and the ast id walk visits every element. Reaching the bound means making the
    outline incremental. The reparse already knows which lazy bodies an edit touched
    (`replace_subtree` carries the rest), so it could hand over the old outline with the
    changed bodies swapped, and the three maps fused into the copy's one pass. That is a change
    to `syntax-tree`'s reparse and `span`'s maps (frontend-text), to ask for if `proof`
    measures this above 10% of an edit.
  - Bodies of items anchored at an enclosing item stay in the outline: items of an impl, trait
    or module with attributes, or of a module nested in a module. Cutting them would move their
    neighbours' anchor-relative offsets. An edit of one of these re-runs the file's syntax
    queries, as every edit did before.

### `X::relative`: lowering a signature (cold, or when its outline changed)

- **Work:** one pass over the outline's elements, writing the signature, its store and its
  source map.
- **Bound:**
  - about 10 instructions per element to dispatch and record: 40 × 10 = 400 instructions;
  - writing about 400 B of output;
  - 1 to 2 allocations;
  - so about 250 cycles, **≈ 70 ns per fn**, plus salsa's 35 ns.
- **Today, ≈ 1 to 2 µs, ≈ 15× off:**
  - this file's part, the flags and keywords, is at its bound: branch-free shifts and masks,
    and one pass over the kind column;
  - the rest is speed-hir-store's: the collector's setup (two memo hits for the def map, a
    pooled builder), the store's tables and their roughly 10 allocations;
  - plus salsa's 140 ns, and the `AttrFlags` and lang-item memo hits (2 to 3 × 18 ns).
- **What stands in the way:**
  - the store's allocation count and setup (speed-hir-store's sorted tables);
  - salsa's execution cost.

  Neither is in this file.

### `X::with_source_map`: moving a map to the file (per edit, for maps someone asked for)

- **Work:** rewrite each pointer of the map by a shift.
- **Bound:** a map of N entries of 16 B, read and written: for 30 entries, 1 KB, about 20 ns,
  plus 1 allocation.
- **Before:** `loc.source(db)` walked the file's tree to the item's node to read its range,
  about 30 ns plus a memo hit. Then `Move` called a `dyn Fn` once per pointer and built about 6
  new vectors: ≈ 150 to 200 ns, about 8× off.
- **Applied now:** the item's file range is read off its ast id pointer (`file_range`), with no
  walk to the node.
- **What stands in the way:**
  - `Move`'s per-pointer `dyn Fn` and per-table allocations (speed-hir-store's type). A shift
    is the same for every pointer outside the elided node, so `moved` could take
    `(at, removed, base)` and rewrite each table with one SIMD add over its ranges. That is a
    change to `Move`, to be asked of speed-hir-store if `proof` measures this above 10% of an
    edit's signature cost.
  - This query runs only for maps someone holds (`lru = 1024`), so it is not on the cold path.

### Summary

| component | bound | before | today | off today | removes the rest |
|---|---|---|---|---|---|
| read a signature | 1 memo hit | 1 memo hit | 1 memo hit | 1× (this file's part) | salsa's hit path |
| `signature_syntax` per item (runs only when its outline changed) | ≈ 80 ns | ≈ 1.0 µs | ≈ 0.5 µs | 6.5× | one-block layout, salsa execution cost |
| signatures re-run per body edit, 150-item file | ≈ 2 µs | ≈ 300–450 µs (all lowered) | ≈ 15 µs (`file_outline` alone runs; attributes read the outline and its flags column, applied, ≈ 1.7 µs of verify) | ≈ 8× | an incremental outline from the reparse, maps fused into the copy (frontend-text, `span`) |
| lowering one fn signature | ≈ 70 ns | ≈ 1–2 µs | same | ≈ 15× | store allocations (speed-hir-store), salsa |
| moving a source map | ≈ 20 ns | ≈ 200 ns | ≈ 150 ns | 7× | `Move` as one shift (speed-hir-store), only if measured |

The benchmark for the whole is rustc's `collect` queries plus its red-green re-verification.
rustc re-verifies an unchanged item's `fn_sig` by one color check per dependency. It needs no
per-item re-run because its HIR is hashed per owner in one pass, which is what `file_outline`
is.

### Memory ([memory.md](../../design/memory.md)) for these files

`blaze-mem` does not exist yet, so nothing here is written against it. Where each remaining
general-purpose allocation goes once it does:

- `select`'s nested obligations (`to_selection`) and `ImplSource::map`'s buffers never escape the
  task: `find_matching_impl` registers them in an `ObligationCtxt` and keeps only the impl and its
  args. They go to the **frame allocator** (`ImplSource<'db, N>` takes a frame-backed vector).
- The leaf search's `parent_trait_obligations` and `FulfillmentError` live until
  `SolverDiagnostic::from_fulfillment_error` converts them within the same inference task. They
  go to the frame allocator too. An edit that keeps an error re-runs this search, so it is part of
  the warm loop, not exempt as an error path.
- Proof-tree step vectors and per-candidate step lists (inspect.rs and the vendored builder) are
  the bulk of `select`'s allocations; with the step-range change above they become one frame
  buffer per evaluation.
- `Obligation` itself is a 20-byte `Copy` value with no heap part.

## blaze-query: the v2 query database (db-query)

Files: `crates/blaze-query/src/{lib,memo,table,segment,image}.rs`. Nothing was run. Costs are
counted from the code as of 10-29 against the machine figures at the top of this file, not
measured; `blaze-query`'s benchmarks check them once the tree builds. The design and the
benchmark each piece is held to (rustc's red-green graph, salsa, papaya, folly's
`ConcurrentHashMap`, Click's `NonBlockingHashMap`, bump arenas, mimalloc) are in
[query-database.md](query-database.md). Rows below are 64-byte lines; "dependent" means a load
whose address comes from the one before it.

### Warm hit (`Cx::get` or `Pinned::get` of a key the view resolved)

The most frequent operation: every read of a query inside a query, on every warm build.

- **Work:** from a `QueryKey` (a content hash, so it is never hashed again), find the memo the
  view resolved and the address of its value's bytes.
- **Bound:** a value is shared by every view and moved by eviction, refill and cold writes, so
  where it lives must be in one place (the memo), not copied into each view's result. Readers
  hold `&[u8]` for their whole task, so rows can't move, and a result must be found through a
  table of places. That leaves 3 dependent lines: the key's place, the view's result row, and
  the memo's line. About 35 instructions (tag, probe, a 34-byte key compare, the value check).
  - Hot: 3 × 4.5 + 35 / 2 ≈ 31 cycles, **≈ 9 ns**.
  - Cold: 3 × 110 = **330 ns** latency, 3 × 5.5 = 16.5 ns throughput when callers' hits overlap.
- **Before this pass:** 4 dependent lines (plus the value's row, read for its address), each row
  load after a dependent load of its chunk's base, and the shard's table header. Cold:
  4 × 110 + 14 ≈ 455 ns, **1.4× off**. Hot: 8 dependent L1 loads + ~45 instructions ≈ 58
  cycles, 17 ns, **1.8× off**.
- **Applied now:**
  - The memo's line (`MemoHot`, 64 bytes, line-aligned) holds a seqlocked copy of the value's
    address and length, rewritten after every swap of its value word. A hit reads the memo's
    line and never the value's row. That is 3 lines, the bound.
  - A shard's table pointer carries log2 of the table's length in its top 6 bits, so a probe
    goes from the pointer straight to the place, with no header line.
  - Rows of a multiple of 64 bytes are line-aligned, so a result or memo is one line, never two.
  - Cold: 3 × 110 + 14 (the shard's pointer, in L2) ≈ 344 ns, **1.04× off: within 10%**.
  - Hot: 6 dependent L1 loads (shard pointer, place, the result chunk's base, result, the memo
    chunk's base, memo) + ~45 instructions ≈ 50 cycles, 14 ns, **1.6× off**.
- **What stood in the way (hot only):** the two chunk-base loads (each `Rows` kept 27
  chunks, found by the row's index) and the kind's `FxMap` probe (`kind_of`, about 15 cycles).
- **Applied second (10-29, with `blaze-mem`):**
  - Every `Rows` is one reserved region (`blaze_mem::Region`, `Tag::Query`: 64 GiB of address
    space, committed as it grows, never moved), so a row's address is base + index × size. The
    base is a field, loaded off the critical path. The memo lines, the database's slots and a
    view's results are on huge pages (`Rows::hot`, `Map::hot`), so a hit's three lines need
    no page walk past the first touch. Generations are a region too.
  - The kinds are a dense table by `QueryKind` (`Kinds`, a `Rows` of 2^16): a kind's live flag
    and row are two independent L1 loads, with no hash probed.
  - Hot: 4 dependent L1 loads (shard pointer, place, result, memo) + ~40 instructions ≈ 38
    cycles, 11 ns, **1.2× off**. The remaining shard pointer can be folded in only by giving
    up sharded writers, which making a memo needs; that is the one thing left in the way.
- `Q::input(key)` (hashing a typed key into its `QueryKey`) is the query's own cost and is
  counted with it. A query that asks with a key it already has as a `QueryKey` pays nothing.

### Recording a read (`Cx::record`)

- **Work:** append a 56-byte edge (the key and the fingerprint it saw) to the running task's
  edge stack.
- **Bound:** one 56-byte store into L1, ≈ 4 cycles.
- **Today:** a thread-local access, a `RefCell` flag set and cleared, and a `Vec` push: about
  12 instructions, ≈ 8 cycles. The `RefCell` flag is about 3 of them. A record always comes
  with a hit or a run, so against the whole read (a 31-cycle hit plus the record) that is
  **≈ 8% over**, within the bar.

### Making a memo (a miss's bookkeeping, apart from the query's own work)

- **Work:** copy the edges (n × 56 bytes) and the value (v bytes) once, write the memo's rows
  (64 hot, 48 cold, a 64-byte value row and a 4-byte generation), and publish into the slot
  map, the view's results and `born`.
- **Bound:** for n = 8 and v = 256, 884 bytes written to L1 at 64 bytes a cycle ≈ 14 cycles;
  3 publishes, each at least one atomic read-modify-write to install lock-free (≈ 20 cycles
  uncontended); ≈ 100 instructions at 2 IPC. Total ≈ 14 + 60 + 50 ≈ **125 cycles, 36 ns**.
- **Before this pass:** a 4 KiB `Writer` allocation per run (≈ 20 ns, plus first-touch page
  faults), the store's writer lock (a CAS and a release), a `fetch_add` on each segment's hold
  count (2), and a `fetch_add` on the row's generation. That is ≈ 330 cycles, **2.6× off**.
- **Applied now:**
  - Each thread reuses up to 8 value buffers (`Writer::with_buffer`), so a run allocates
    nothing once they have grown.
  - Each lane's free lists are its own: no lock. Retired value rows are reclaimed by the lane
    that retired them, as it writes; `&mut` reclaims every lane's.
  - An open segment's hold count carries a bias (`OPEN`); its lane counts the spans it hands out
    in a plain field and settles the count once, at seal. A copy does no atomic operation.
  - A row's generation is a load and a store by its one claimer.
  - So `Store::make` does no atomic read-modify-write. The 3 publishes are each a shard lock
    (a CAS) and a release store. Today ≈ 14 + 4 (the value copied once more, from the
    buffer) + 3 × 25 + 150 / 2 ≈ 170 cycles, 48 ns, **1.35× off**.
- **What stands in the way:**
  - `born`, a whole map published per memo only so `collect` can tell a key added while it
    walked. Stamping the generation in the memo's cold row instead (48 bytes with 6 free) drops
    a publish and its ~40 instructions. A key that resolves to an equal memo it already had
    must restamp that memo. With that: ≈ 130 cycles, **1.04×**. This is the next change to this
    crate.
  - The value is written into a buffer and then copied into the segment. Writing straight into
    the segment would need `blaze_format::Writer` over a borrowed `&mut [u8]` that can grow in
    place. That is 4 cycles of 170, under 3%, so it is not worth a new `Writer`.
  - `add_memo` compares the new memo with the key's existing ones (`Store::same`). That is work
    the bound needs (dedup), proportional to the memos kept for the key.
- **Applied (10-30, sol-all-base):** `born` is `MemoCold::born`, 4 of the cold row's 6 free
  bytes (still 48). A new memo is stamped with a plain store into the cold row this thread just
  wrote (in L1), before it is published; an equal memo the key already had is raised with one
  `fetch_max`, only when it is lower. The `born` map, its publish and `collect`'s `retain` of it
  are gone, and the generation is a `u32`. Today ≈ 14 + 4 + 2 × 25 + 110 / 2 ≈ **125–130
  cycles, ≈ 1.04×** (counted). `a_memo_is_born_in_the_generation_its_key_last_resolved_to_it`.

### Verifying a memo (a warm build after an edit)

- **Work:** walk the memo's edges in order; for each, find the dependency's current fingerprint
  in the view and compare; stop at the first that differs.
- **Bound:** per edge, the record streams in sequentially (56 bytes at 30 GB/s ≈ 1.9 ns, and
  prefetched by the hardware). The lookup is 2 dependent lines (place, result row): a
  dependency is named by its content key, so it must be found in a table. Edges are
  independent, so lookups overlap up to the core's 20 misses in flight: 2 × 5.5 ≈ **11 ns per
  edge** from DRAM. Hot: ≈ 30 instructions, **≈ 15 cycles** an edge.
- **Before this pass:** 3 dependent lines per edge (the result's memo ID, then the memo's line
  for its fingerprint). Out-of-order execution overlaps only as many edges as the reorder buffer
  holds, about 8 at ~55 instructions an edge: 3 × 110 / 8 ≈ 41 ns, **3.7× off**.
- **Applied now:**
  - A view's result (`Resolved`) holds the memo's fingerprint beside its ID: the row is
    exactly one line (34-byte key, 4-byte ID, 16-byte fingerprint), and an edge's check reads
    only it.
  - In a database past `PREFETCH_FROM` memos (2^16, about 8 MB of rows: past L2), verification
    prefetches the place of the edge 8 ahead and the result row of the edge 4 ahead, so about
    16 misses are in flight instead of 8.
  - Today: 2 × 110 / 16 ≈ 14 ns per edge, **1.25× off**; hot (a small database) ≈ 15 cycles,
    at the bound.
- **What stands in the way:**
  - The window (8 and 4) is set from the figures above, not measured. With 20 misses in flight
    and 2 per edge, a window of 10 reaches the bound; `proof`'s budget sets it. Set to 10 and 5
    (10-30, sol-all-base) by that count: 2 × 110 / 20 ≈ **11 ns an edge, ≈ 1×** if the core
    keeps 20 misses in flight. A prefetch is a hint, so no answer changes; only a build can say
    whether the window is right.
  - The stronger bound needs dense key IDs: a view's results become a column indexed by a
    `u32` key ID, so an edge is 1 line (≈ 5.5 ns) and 24 bytes. That is the redesign in
    query-database.md ("To match: intern query keys"). It is 2× today's bound and changes the
    image's layout, so it is a change of its own, after the tree builds and this one is
    measured.

### Collection (`collect`, between edits, concurrent with readers)

- **Work:** mark every memo reachable from the open views and the kept keys (each reached memo's
  edges, and each edge's slot), sweep every slot, and free what is past its grace.
- **Bound:** memory-bound over what it touches. For nushell's scale (M = 10M reached memos,
  E = 100M edges): 64 M + 56 E for the memos and edges plus 2 random lines per edge for its
  slot. Random lines at all of Turin's cores: 200M × 110 ns / (20 × 192) ≈ **6 ms**. On one
  core: 200M × 5.5 ns ≈ 1.1 s.
- **Before this pass:** single-threaded, with `reached` and `hot` as hash sets of 10M entries
  (≈ 160 MB each, so every insert and test a DRAM miss): ≈ 3 random lines per edge-step and 2
  more per memo. ≈ 1.8 s, **300× off** the all-core bound.
- **Applied now:** the marks are bitsets indexed by memo ID (`Marks`, 1 bit a memo: 1.25 MB
  for 10M memos, which stays in L2/L3), so marking costs no miss. ≈ 1.1 s on one core, at the
  one-core bound but **180× off** the all-core one.
- **What stands in the way:** it runs on one thread. Marking parallelizes as rustc's and every
  tracing collector's does: split the roots, and have each worker set bits with `fetch_or` on
  the shared bitset, so a memo is taken once. The sweep over slots splits by shard. The
  workers must be the plane's (no thread of the database's own), and `collect` is called from
  the daemon outside any task. Asked of the owner of `blaze_graph`'s plane: a way to run a batch
  of closures as plane tasks and wait for them from outside a task
  (`v2/requests/db-query.md`, 4). `collect` never blocks a reader (reads take no lock) and runs
  between edits, so it is off an edit's critical path while it is slow.
- **Applied (10-30, sol-all-base), on `Pool::scope`:** from 2^16 memos (`MARK_PARALLEL_FROM`)
  every pass of `collect` but the inputs' runs on the plane's workers (`on_workers`, one job a
  worker). Counted, not measured:
  - **Marking** (`Database::mark_parallel`): the marks are atomic words in the collection's
    frame, set with a load and one `fetch_or`, so a memo is walked once. Workers take 1,024
    kept roots, or one shard of one view's results (setting its `hot` bits), at a time, walk
    depth first, and hand half their stack to an idle worker once it holds 2,048. The views'
    results walk was serial: 10M results × 64 bytes streamed on one core, ≈ 21 ms, now split by
    shard. Marking: 200M lines / (20 × 192) ≈ 6 ms, plus the jobs' start and join (≈ 10 µs).
  - **Sweep** (`Map::edit_all_parallel`, `Database::sweep_slot`): shards split among the
    workers. A slot whose memos were all reached is kept on its row and the marks alone, so a
    slot costs ≈ 1 random line: 10M × 110 ns / (20 × 192) ≈ 0.3 ms. Before: 3 lines a slot,
    one core, ≈ 330 ms (2 of them the `born` map's probe, gone with the map: see "Making a
    memo").
  - **Image keys:** every key of the image is given a slot once per image (`image_slotted`),
    not once per collection: a sweep never removes an image key's slot. Before: 10M lookups a
    collection, ≈ 0.1–0.2 s on one core; the one pass left runs on the workers by table group.
  - Today ≈ 6 + 0.3 ≈ **6.5 ms all-core, ≈ 1.1×**. What a build must confirm: the hand-off size
    (1,024) and the threshold, and that the last shards finish together (≈ 256 shards over 192
    workers is 1.3 shards a worker, so the sweep's tail may be up to 2× its mean, ≈ 0.6 ms).
  - Equivalence: `parallel_marking_reaches_what_serial_marking_does` (random roots, with and
    without views, batch 1 to 1,024, marks too small for some memos),
    `a_parallel_edit_of_every_entry_is_the_serial_one`.

### Cold pack (`write_cold`, then the mapped completion)

- **Work:** copy V bytes of cold values into one file, write it, map it back, check what was
  mapped, and swap each value's word to the mapping.
- **Bound:** the write, V / 7 GB/s on NVMe. One write and one map, each a single I/O op through
  the core. Each value's content ID (`Stored::Disk` names it) is one hash pass: V at about
  4 GB/s a core. The mapped check is a `memcmp` against the copy still in memory, at 30 GB/s.
- **Before this pass:** the check hashed every value again: a second pass over V at 4 GB/s,
  **≈ 2× off** in CPU.
- **Applied now:** a value whose word still names the copy written is compared byte for byte.
  Only one swapped out since (evicted, or moved by another write) is hashed again.
- **What stands in the way:** the check reads the whole mapping back, page by page, faulting in
  4 KiB pages: at ~1 µs a fault, V / 4 KiB µs (≈ 260 ms per GB) against 33 ms for the
  `memcmp` itself. A map that populates its pages (`MAP_POPULATE`, or huge pages) would remove
  it. That is the core's `Effect::Map`, and it is to be measured before it is asked for.

### Writing the image (`write_image`, once per cold build, before `settle`)

- **Work:** every key's memos, sorted by key and by memo, with their edges and values, written
  as one file.
- **Bound:** the bytes, M × (192 + 56 × edges + value) at 7 GB/s; for 10M memos of 10 edges and
  256-byte values, 10 GB, ≈ 1.4 s. Before it, the sort of the keys: as a radix sort on the
  key's leading 8 bytes (uniform, since keys are content hashes), ≈ 0.3 s on one core. Total
  **≈ 1.7 s**.
- **Before this pass:** a `FxSet` of every key (10M random inserts, ≈ 0.3 s), a comparison sort
  of the keys (≈ 10M × 23 × 3 ns ≈ 0.7 s), and a lock of the store's code table per memo for its
  schema and producer (≈ 0.2 s). ≈ 2.6 s, **1.5× off**.
- **Applied now:** the keys are pushed into a `Vec`, sorted and deduplicated (no set), and the
  code table is snapshotted once (taken again only when reading a key in from the image adds a
  code). ≈ 0.7 + 1.4 ≈ 2.1 s, **1.24× off**.
- **What stands in the way:** the comparison sort. A radix sort on the keys' leading bytes
  takes it to the bound. It runs once per cold build, off the warm loop, so it comes after the
  hit, verify and miss paths are measured.
- **Applied (10-30, sol-all-base):** `sort_keys` radix-sorts `(prefix, index)` pairs of 12
  bytes on the plane (`blaze_ids::radix::on`), the prefix being the kind and the input's first
  6 bytes, then sorts each run of equal prefixes by comparison (about one pair in 10M content
  hashes, plus the keys the slots and the image share). Six 11-bit passes over 120 MB of pairs
  and one permutation of the keys: ≈ 0.15 s on one core, ≈ 0.05 s on the plane. Today ≈ 1.4 +
  0.05 ≈ **1.45 s, at the bound** (the bound's 0.3 s was a one-core sort). The order is
  `sort_unstable`'s: `keys_sort_for_the_image_as_by_comparison`.
- **On `blaze-image` (10-29):** the sections are written once each into the daemon's
  `Builder`, each record built on the stack, with no file-sized buffer of the database's own.
  The image's CRC32Cs are `blaze-image`'s, at about 28 GB/s a core, spread over the pool
  (≈ 10 GB / (28 GB/s × 16) ≈ 22 ms). Marking symbols walks each value once through its
  kind's `Persist::symbols`, which reads nothing for a value that names none (every value
  today). A start's reads add one CRC32C per 64 KiB group the first time it is touched
  (≈ 2.3 µs at 28 GB/s), against ≈ 16 page faults of the same group: within the fault cost.

### Summary

| component | bound | before | today | off today | removes the rest |
|---|---|---|---|---|---|
| warm hit, cold | 3 lines, 330 ns | 4 lines + headers, 455 ns | 344 ns | 1.04× | — |
| warm hit, hot | ≈ 31 cycles | ≈ 58 cycles | ≈ 38 cycles | 1.2× | only unsharded writers would drop the shard pointer |
| record a read | 4 cycles | 8 | 8 | 8% of the read | — |
| make a memo | ≈ 125 cycles | ≈ 330 | ≈ 125–130 (counted) | 1.04× | — (`born` in the cold row, 10-30) |
| verify, per edge | 11 ns (2 lines, overlapped) | 41 ns | ≈ 11 ns (counted, window 10) | ≈ 1× if 20 misses fly | a build to confirm the window; dense key IDs for the 1-line bound |
| collect | 6 ms all-core (nushell) | 1.8 s | ≈ 6.5 ms (counted) | ≈ 1.1× (off the edit path) | a build to confirm the hand-off size and the sweep's tail |
| cold pack check | `memcmp`, 30 GB/s | a second hash | `memcmp` | ≈ 1× in CPU; faults dominate | populated maps, once measured |
| image write | ≈ 1.7 s (10 GB) | ≈ 2.6 s | ≈ 1.45 s (counted) | ≈ 1× (once per cold build) | — (radix sort of the keys, 10-30) |

### Salsa's interned values in the image (`vendor/salsa/src/image.rs`, speed-salsa, 10-29)

Not yet built or measured. These are counts from the code:

- **Write:** one pass over every slot of every kept struct, plus one mark-and-name walk of
  the values the roots reach. A kept slot costs a 24-byte record and the codec's bytes; a
  slot of a page that isn't kept costs nothing but its page's 4-byte owner word. The GC bit
  set is 1 bit per slot of the pages in use: \(\frac{n}{8}\) bytes for \(n\) slots, so 128 KB
  for 1 M slots.
- **Restore:** one decode per kept value into a `Vec` (validate, then apply), then a slot
  write, one hash and one key-map insert per value. Its bound is that hash and insert, about
  20 ns a value, so 1 M interned values take about 20 ms on one core. The apply is
  single-threaded under `&mut`; shards are independent, so splitting it across the pool is
  what removes the rest. Unlike `intern::Mapped`, values are copied into salsa's slots and
  not read in place: salsa's slots own their fields, and reading them in place needs fields
  laid out as `blaze-format` records (a change to the slot type, not taken).

## hir-def: name resolution (speed-hir-sol-nameres)

What this covers:
- `crate_def_map` and `block_def_map`: collection, imports and globs to a fixed point, round
  by round;
- a module's scope (`ItemScope`) and its per-name fact (`ModuleScope::name`);
- `resolve_path` of an \(s\)-segment path;
- the `Resolver` a body walk builds;
- `find_path`, at the edge.

Name resolution is being rebuilt as these lines are written. frontend-nameres moves the collector
onto per-module facts (`nameres/facts/`, design in
[frontend/nameres.md](../frontend/nameres.md), contract in
[frontend/nameres-core.md](../frontend/nameres-core.md)), and db-expand-names adds records to the
def map. Costs are counted from the code and those notes as of 10-29 20:30, not measured.
`facts/queries.rs` exists, but `unit.rs`, `collect.rs`, `macros.rs` and `driver.rs` are not
written yet, so the facts engine is costed from its contract. The old collector
(`collector.rs`) is still what `crate_local_def_map` runs.

nameres.md §6 has its own "Speed of light": the per-edit floor of the facts engine, as a
perfect oracle of the changed facts would pay. This section takes that floor as given and adds
four things:
- the cold build;
- the edit's validation term, which nameres.md accepts and this bar does not;
- path resolution;
- the resolver.

The figures used, for one crate:
- \(m = 1000\) modules;
- 20k declarations;
- 4k imports, 300 of them globs, bringing in about 18k names;
- \(k = 4\) expansion rounds;
- 150 items per file;
- paths of 2.5 segments on average.

Salsa costs are speed-salsa's, above:
- a memo hit is 11 ns today and 5.4 ns at its bound;
- an intern hit is about 15 cycles (4.3 ns), already at its bound;
- an execution's bookkeeping is about 140 ns today and about 35 ns at its bound;
- a read of an immortal interned id's field (`module.krate(db)`) is one line, about 2 ns.

### `ItemScope::get` (every name looked up in a scope being built, and every fact's execution)

- **Work:** find the name's entry in each of the three namespaces.
- **Bound:** 3 dependent lines and the unpacking, about 30 cycles, **≈ 9 ns hot**:
  - the scope;
  - the index slot;
  - the entries, which are independent of each other. One hash.
- **Today:** one FxHash, one probe run over `u32` slots (6 tag bits reject 63 in 64 other
  names) and the entries, loaded in parallel. That is the bound's lines, so it is **at the
  bound**. A scope of 8 names or fewer is a scan of at most 8 entries a column. Nothing is
  allocated.
- The only way to save a line is to keep a name's three entries together. That costs 40 bytes
  for each namespace a name lacks, for a line saved only on names in two namespaces.

### `resolve_path` of an \(s\)-segment path (every path a body or signature names)

- **Work:** per segment, the meaning of one name in one module (the fact), and a visibility
  check.
- **Bound:** the fact is one salsa read keyed by (module, name): one intern and one memo hit.
  That is 4.3 + 5.4 ≈ **10 ns per segment**, plus one builtin-table probe for the first segment
  (about 5 ns). The read has to be recorded, because it is the edge that lets an edit elsewhere
  in the module leave this reader alone. For \(s = 3\): **≈ 35 ns**.
- **Today, for a segment in a collected map (`resolve_remaining_segments`), ≈ 37 ns:**
  - `module.krate(db)` and `module.block(db)`, about 4 ns;
  - `self.module(db, module)`, which is `Modules::module`: an intern of (`Modules`, module) and
    a memo hit, 15 ns;
  - `name_in_scope`, which is `ModuleScope::name`: an intern of (scope, name), a memo hit and a
    128-byte `PerNs` copied out, 17 ns;
  - the visibility filter: one tag compare for `Public`, one field read per level for a
    restricted one.

  A three-segment path is ≈ 42 + 2 × 37 ≈ **115 ns, 3.3× off**. It allocates nothing
  (`nameres/tests/paths.rs`, `resolved_path_allocates_nothing`, and below).
- **Inside a unit** (a scope still `Building`), a segment is `ItemScope::get` directly, about
  10 ns: at the bound.
- **What stands in the way:**
  1. **The round trip through `Modules::module` (15 ns a segment):** it fetches a
     `ModuleData` only to read its scope's id. The change is one tracked function for collected
     maps, `scope_name(db, module: ModuleId, name: Name) -> PerNs`. On a miss it runs
     `module.def_map(db).module(db, module).scope(db).get(&name)`. It has the same
     dependencies as today's two reads and backdates the same way (on `PerNs` equality).
     `resolve_remaining_segments` and `resolve_name_in_module` call it when the module's map is
     collected (`ModulesSlot::Collected`, or a map other than `self`). Inside a unit they keep
     today's path, because the facts engine's edges are `ModuleScope::name` of a round's
     handle. It replaces one memo kind with the other for collected reads, so the memo count is
     unchanged. A segment then costs one intern and one memo hit: ≈ 15 ns today and ≈ 10 ns at
     salsa's bound, which is **the bound**. Requested of db-expand-names
     (`v2/requests/db-expand.md`). **Applied** as `Modules::name(module, name)`, read through
     `DefMap::name_in_module` by `resolve_remaining_segments`, `resolve_name_in_module` and the
     prelude and root lookups: a segment is ≈ 4 + 4.3 + 11 ≈ 20 ns, a three-segment path
     ≈ 27 + 2 × 20 ≈ **70 ns, 2×**, ≈ 45 ns at salsa's bound (counted, not built).
  2. **The memo hit itself (11 → 5.4 ns):** salsa's rows by page (speed-salsa).

### A def-map memo hit (`crate_def_map`, `ModuleId::def_map`, `block_def_map`)

- **Work:** return the map.
- **Bound:** two salsa reads, the `crate_local_def_map` memo and the `DefMapPair::def_map`
  field: **≈ 11 ns**. The pair's split is what lets readers of `local` (every block map, the
  resolver) leave a crate map's edit alone, so it is worth its read. A block map is one memo
  hit, ≈ 5.4 ns.
- **Today:**
  - **≈ 22 ns** for the two reads;
  - plus the `block` and `krate` field reads in `ModuleId::def_map`, so **≈ 26 ns**;
  - a block map is 11 ns.

  That is **2× off, all of it salsa's hit path**. The `tracing` span in `crate_local_def_map`
  runs only on execution. Nothing is allocated (a budget test is requested, below).

### The `Resolver` a body walk builds, a scope push and a lookup

- **Work:** the module's maps, the item's generic parameters and the body's expression scopes,
  pushed as a stack of `Copy` scopes (24 bytes each, asserted).
- **Bound:** 4 salsa reads at 5.4 ns: the def map (2), `GenericParams::of` and
  `ExprScopes::of`. Plus a 24-byte store per scope and no allocation: **≈ 25 ns**.
- **Today, for a fn with no block (`FunctionId::resolver`, then `resolver_for_scope`), ≈ 85 ns
  and 1 allocation:**
  - the fn's location, 2 ns;
  - `local_def_map`, 3 reads, 33 ns;
  - `GenericParams::of`, 11 ns;
  - the first push grows `scopes` from empty, about 20 ns;
  - `ExprScopes::of`, 11 ns;
  - the pushes.

  A chain of more than 3 scopes reallocates once more at `reserve`. Each block adds a
  `block_def_map` hit. That is **≈ 3.5× off, and allocates where the rule says none**. It runs
  once per body walk (inference, lowering, diagnostics), so it is under 5% of a body's time,
  but every body pays it.
- **A push** (`update_to_inner_scope`) into reserved capacity is a 24-byte store; leaving is a
  truncate.
- **A lookup** of a local is a reverse scan of the innermost scopes' entries: a name and a
  hygiene compare each. A body's scopes hold a few bindings, so the scan beats a probe (rustc's
  `Rib` is a hash map per rib). A lookup of an item is the path cost above. Both are at the
  bound and allocate nothing.
- **What stands in the way:** the heap `Vec`. The change is `scopes: SmallVec<[Scope<'db>; 8]>`:
  192 bytes inline. The builder moves a `Resolver` by value (`push_scope(mut self)`) two or
  three times per build, about 5 ns of copying. With it, a resolver of up to 8 scopes allocates
  nothing; a deeper body grows once per walk and is reused by `update_to_inner_scope`. A
  `blaze_mem::Frame` vector would be leaner, but `Resolver<'db>` outlives the call that builds
  it (it is stored in `InferenceContext`), so a frame would need a second lifetime on every
  holder. Requested of db-expand-names. **Applied** (`Resolver::scopes`): the first push's
  ≈ 20 ns allocation is gone, so **≈ 65 ns, 0 allocations, 2.6×**, the rest being salsa's five
  reads at 11 ns against 5.4 (counted, not built).

### `crate_def_map`, cold: the fixed point over the crate's items

- **Work:** write every (module, name) slot and resolve every import until nothing changes,
  round by round. Expanding macros is hir-expand's work and lowering item trees is format's;
  neither is counted here.
- **Bound:**
  - **Slots written:** 20k declarations, 3.7k single-import bindings and 18k names brought in by
    globs, ≈ 42k.
  - **Lookups:** each import resolves at least once, and the worklist's measured retry ratio is
    about 2 per import (round 1: 127 calls for 64 imports). At 2.5 probes each:
    4k × 2 × 2.5 ≈ 20k.
  - **Cost per probe or write:** a hash, a slot line and an entry line. The crate's scopes are
    42k × 45 B ≈ 1.9 MB, so these are L2/L3 lines. Independent imports overlap about 4 in
    flight: ≈ 30 cycles, 9 ns.
  - **Reading the declarations:** 20k × 64 B ≈ 1.3 MB at 30 GB/s, ≈ 45 µs.
  - Total: 62k × 9 ns + 45 µs ≈ **0.6 ms** for this crate.
- **Today on the build's path (the old collector, one query):** round 1 brought the import loop
  to the worklist bound. An import is retried only when a slot it read is written, and a glob
  step allocates nothing. So its operation count is near the bound. What it adds on top:
  - the columns' doubling growth, about log2(names) reallocations per column;
  - one `ModuleScope` tracked struct per module at hand-over, ≈ 50 ns each;
  - the `Modules` table.

  No profile exists, so this is not put in ns.
- **The facts engine (what replaces it):** the same resolution work, scheduled per module and
  per component, plus the cost of each query execution. Counted from `facts/queries.rs` and
  nameres-core.md §2:
  - round 0, per module: `module_decls`, `decl_handle`, `child_key`, and `legacy_exports` and
    `subtree_macro_exports` per child, ≈ 5m;
  - every round, per module: `module_round`, `fact_handle`, `macro_step` and
    `unresolved_named`;
  - every round, per component: `scc_state` and `latest`, ≈ 4m + 2c;
  - plus an intern per key.

  With \(c \approx 900\) (most modules are their own component) and \(k = 4\), that is
  5k + 4 × 5.8k ≈ **28k executions**. That is ≈ 3.9 ms at today's 140 ns and ≈ 1.0 ms at
  salsa's bound, against a 0.6 ms bound: **≈ 7.5× off today, ≈ 2.6× at salsa's bound, from
  bookkeeping alone**. Clones come on top:
  - a component that changes at a round clones its `UnitState` and hands new `ModuleScope`s
    (a copy of each member's scope);
  - a macro step clones its module's `ItemScope` for each round it has a pending directive,
    about 1.7 KB and 4 allocations.
- **What stands in the way of 90%:** per-(module, round) queries for modules the round doesn't
  touch. After round 0, most modules have no macro call left, gain no op, and sit in a component
  with no changed input, yet every round runs four queries for each of them. The changes, all
  in frontend-nameres-core's files (requested in `v2/requests/frontend-nameres.md`):
  1. `RoundModules` carries the round's active modules as a sorted `u32` column beside `order`.
     A module is active if it has a pending directive, gained an op, or is a member of a
     component whose input changed. It also carries, per module, the round its component last
     changed (`since`).
  2. Readers ask `fact_handle(since[M], M)`, so an untouched module has one handle query for
     the whole build instead of one per round. `scc_state` runs only for components with an
     active member.
  3. `macro_step`, `module_round` and `unresolved_named` run only for active modules, and
     `round_progress` folds over the active list.
  4. Round 0's firewalls become conditional. `legacy_exports` and `subtree_macro_exports` are
     asked only of a child whose `ModuleDecls` says it has `macro_rules!` or
     `#[macro_export]` (a flag bit). `child_key` becomes an index into
     `ModuleDecls::children`, since the key already travels in the child's `Splice` op.

  Then executions are ≈ 2m + 4·A + 2·(active components). Here \(A\) is the number of
  (module, round) pairs that are active. With 300 modules holding macro calls over 4 rounds,
  \(A\) ≈ 1.2k, giving ≈ 9k executions: ≈ 0.3 ms at salsa's bound, **≈ 0.9 ms in all, 1.5×**.
  What is left is the firewalls' own price and the clones:
  - two executions per module, ≈ 70 ns, against ≈ 600 ns of resolution per module: 12%;
  - copy-on-write rows for the clones, which nameres-core.md §7 already names as the next cut.

  A component built of `use super::*` cycles reruns from its carried state each round it
  changes, which is what the old collector paid for it. It is no worse than before.

### An edit that changes no name (a signature, a field's type, a private body)

- **Work:** collect the edited file's module again and show its declarations are equal.
- **Bound:** about **3 µs**:
  - one pass over the file's 150 items: read each item-tree entry (64 B), write its op (32 B),
    compare with the old log (32 B). That is ≈ 20 KB through L2 plus about 10 ns of dispatch per
    item, ≈ 2.5 µs;
  - one salsa execution.
- **Today (the old collector):**
  - the whole crate is collected again, which is the cold cost, milliseconds for this crate;
  - the `DefMap` is compared: 42k entries × 40 B ≈ 1.7 MB, ≈ 60 µs;
  - it then backdates, and `nameres/tests/backdate.rs` holds dependents to zero runs.

  That is **≈ 1000× off**.
- **The facts engine:**
  - `module_decls` of the edited module runs (≈ 3 µs) and backdates on an equal `OpLog`;
  - before `crate_local_def_map` can be green again, salsa verifies every other query of the
    crate's resolver graph, because every reader asks for the map whole (`ModuleId::def_map`).
    The first read after the edit walks every `fact_handle`, `scc_state` and `macro_step` once.
  - In this crate that is ≈ 28k nodes and ≈ 80k edges: ≈ 1.6 ms at salsa's in-cache verify
    (≈ 20 ns an edge);
  - ≈ 0.5 ms with the active-set change above (≈ 27k edges);
  - ≈ 0.12 ms at blaze-query's hot verify bound (15 cycles an edge).

  Against 3 µs that is **≥ 40× off at the best of these**. nameres.md §6 counts this term (≈ 200
  µs for 1000 modules) and accepts it.
- **What stands in the way:** pull-based verification costs O(graph) per edit. Today only
  durability bounds it: an edit in a workspace crate leaves the HIGH-durability crates' graphs
  unwalked. To reach the bound, verification has to start from what changed. The change is
  pushed forward over reverse edges from the edited input, stopping at the first memo that
  backdates. That is Adapton's dirty propagation, and how Jane Street's Incremental and Skip
  work. Here the backdated `module_decls` stops it at once, so nothing beyond the file is
  touched. That is a change to the query engine (salsa now, blaze-query later), not to name
  resolution. It goes to speed-salsa and db-query through speed-hir.

### `find_path` (the edge: diagnostics and the IDE)

- **Work:** the shortest path to an item visible from a module, by rust-analyzer's choice rules
  (prelude first, `std` over `core`, the fewest segments).
- **Bound, for those rules:**
  - one probe per (module visited, candidate);
  - one `ImportMap::of` memo hit per dependency it looks in;
  - one allocation, the answer.
- **Today:** round 1 builds one `ModPath` per call and compares candidates as (kind, name,
  length) with no path built (`FindPathScratch`). That is **at the bound for these rules**, in
  allocations and probes. rustc's `visible_parent_map` answers a narrower question (rustc's
  path, not the shortest one visible from a given module) in O(path length); taking it would
  change every printed path.
- `find_path` is not on a build's path, so no further change is asked. Its hot_lint findings are
  `Name` copies written as `.clone()` (below).

### Summary

| operation | bound | today | off | removes the rest |
|---|---|---|---|---|
| `ItemScope::get` | 3 lines, ≈ 9 ns | same lines | 1× | — |
| `resolve_path`, 3 segments, collected map | ≈ 35 ns (1 intern + 1 hit a segment) | ≈ 70 ns (`Modules::name(module, name)` applied, `DefMap::name_in_module`; was ≈ 115 ns) | 2× (≈ 1.3× at salsa's bound) | salsa's intern and hit |
| def-map memo hit | 2 reads, ≈ 11 ns | ≈ 26 ns | 2× | salsa's hit path |
| `Resolver` per body | 4 reads, 0 allocations, ≈ 25 ns | ≈ 65 ns, 0 allocations (`SmallVec<[Scope; 8]>` applied; was ≈ 85 ns, 1–2) | 2.6× | salsa's hit (5 reads at 11 ns) |
| scope push, lookup | a store; a scan or a path | same | 1× | — |
| `crate_def_map` cold, 1000 modules | ≈ 0.6 ms | old collector: near it in operations, not profiled; facts as designed ≈ 1.6 ms at salsa's bound, ≈ 4.5 ms today | 2.6× at salsa's bound (facts) | active-set rounds (frontend-nameres-core): ≈ 1.5×, then copy-on-write rows |
| edit that changes no name | ≈ 3 µs | old: ms (whole crate); facts: ≈ 0.12–1.6 ms of verify | ≥ 40× | forward invalidation in the query engine (speed-salsa, db-query) |
| `find_path` | probes per candidate, 1 allocation | same | 1× | — |

The benchmark for the whole is rustc's `rustc_resolve`, which re-resolves the whole crate on
every build. It is not incremental below the crate (its incremental system starts after
resolution), and per lookup it does one `resolve_ident_in_module` probe per segment. Per lookup,
this code matches it inside a unit and comes within the salsa read of it after collection. Per
edit, the facts engine beats it by the share of modules an edit leaves alone, and a forward
invalidating query engine is what would reach the per-file bound. Hit-path and verify costs are
speed-salsa's section's.

## hir-ty-matchck: match checking (db-matchck)

Files: `crates/hir-ty-matchck/src/{lib,visit,lower,cx,print,typo}.rs` and
`crates/hir/src/match_lints.rs`. Nothing was run: the costs below are counted from the code, with
the unit costs above, a small allocation plus its free at ≈ 20 ns, and a `Resolver` at the
≈ 85 ns that the name resolution section gives. The reference is rustc's own `check_match`, which
does this work for every body on every build. Here it is one salsa query per body
(`match_check`, `returns(ref)`, `Hash`/`Eq` result), so a warm build pays one memo hit for each
body that didn't change.

### A body with no `match`, no `let` expression or `let`-`else`, and no refutable pattern (most bodies)

- **Work:** find that there is nothing to check, and lint `bindings_with_variant_name`, the only
  thing that can fire. It needs no type context beyond the inference result that type checking
  already made.
- **Bound:** one read of each expression's kind and each binding pattern's type. For a body of
  n = 200 expressions and b = 20 bindings that is 200 tag reads plus 20 type-kind reads, about
  200 + 20 × 10 ≈ 400 cycles, **≈ 0.1 µs**. No allocation, no inference context, no layout.
- **Today:** `needs_analysis` scans the expression column once (`exprs()` decodes each row,
  ≈ 10 to 20 cycles a row), then a `Visitor` with no lowerer walks the tree for the lint:
  ≈ 200 × 15 + 200 × 15 + 200 ≈ 6 000 cycles, **≈ 1.7 µs, ≈ 15× off**. No inference context,
  layout or type is built, and nothing allocates unless a lint fires (the visitor's `Vec`s start
  empty).
- **What stands in the way:** two full decodes of the body where a tag scan would do.
  - A kind-only scan from hir-def (one byte per expression, no row decode) would make the test
    ≈ 200 cycles.
  - Folding the lint into the same scan of the pattern column would remove the walk. Every binding
    site of such a body is irrefutable, so `suggest` is just "the enum has one variant". The lints
    would then come out in pattern-id order, not THIR order (parameters last). That is the same
    set, but the emitter would have to put them back in rustc's order.
  - With both, ≈ 400 cycles, at the bound.
- **Applied 10-30 (sol-all-ty), counted, nothing built:**
  - `needs_analysis` answers from `ExpressionStore::kinds_present` when the body has a `match`
    or a `let` expression, one load instead of the scan.
  - The walk is replaced by `may_name_a_variant`, a scan of the b bindings' types that is a
    superset of what the lint can report (it skips the binding-mode test). When it finds
    nothing, which is every body without an enum-typed binding named after a unit variant,
    `check_body` returns the empty result. That is exact, and needs no reordering: only when
    a candidate exists does the THIR-order walk run. Cost: 20 × ≈ 10 cycles ≈ 200 cycles.
  - The scan reads hir-def's kind-only columns (queue 00795, landed): `let_statements` decodes
    only the Let rows, and closures and assignments are found by `expr_kind` (one byte per
    expression), only when `kinds_present` has either. No other row is decoded.
  - Today, n = 200 with 20 lets and 20 bindings, no closure or assignment: Let rows 20 × ≈ 10
    plus the binding scan ≈ 200, **≈ 400 cycles ≈ 0.12 µs, ≈ 1.2×** (was 1.7 µs, 15×). A body
    with closures or assignments adds the 200-byte kind scan, ≈ 200 cycles (≈ 0.18 µs, ≈ 1.8×).
  - Tests: `shortcuts_match_the_walk` in `tests.rs` compares the new `needs_analysis` with the
    old scan, and the skipped result with the walk, on bodies with and without candidates. It
    also asserts 0 allocations for a cleared body.

### A body with matches (per `match` of m arms, p pattern nodes in all)

- **Work:** lower the p nodes to `DeconstructedPat`s, run rustc's usefulness algorithm, and turn
  what it reports into lints and errors. Only an error prints a witness or allocates text.
- **Bound:**
  - The usefulness algorithm is rustc's, ported unchanged (`ra-ap-rustc_pattern_analysis`), so its
    cost is rustc's and is the floor.
  - Lowering is one node per pattern node with its fields in an arena: p × ≈ 30 cycles. For
    m = 4 arms of `Some(x)`-like patterns (p = 8) that is ≈ 240 cycles, under the analysis's own
    cost (matrix rows, a `split` per column, ≈ 1 to 2 µs for such a match).
  - The bound is the analysis plus ≈ 0.1 µs.
- **Today:**
  - Each non-leaf node is a `Vec` of fields, because `DeconstructedPat::new` takes one (leaves
    pass an empty `Vec`, which doesn't allocate). That is about 4 allocations for this match,
    ≈ 80 ns.
  - The lowered patterns live in one `Vec` per match, which the report borrows.
  - The scrutinee validity check (`is_known_valid_scrutinee`) builds a `Resolver` for a path
    scrutinee, to see whether it names a static: ≈ 85 ns.
  - A path pattern that is not a variant (a constant) builds one too. It also asks the trait
    solver once per ADT in the constant's value (`StructuralPartialEq`, unless the ADT's module
    records a builtin `PartialEq` derive, which is a scan of that scope's derives) and once
    for an ADT-typed constant (`PartialEq`), each ≈ 0.15 µs. Constant patterns are few, so this
    is off the common path.
  - In all, analysis + ≈ 0.2 µs, **within 10% for a match of more than a few arms** and ≈ 1.15×
    for the smallest.
  - The guard-free second analysis (`would_be_exhaustive_without_guards`) runs only for a
    non-exhaustive match with a guard, which is an error.
- **What stands in the way:**
  - The `Vec` per node is `rustc_pattern_analysis`'s API; an arena there would remove it.
  - The `Resolver` per path scrutinee could go: type checking already resolved the path, so a
    per-expression "names a static" bit in `InferenceResult` makes it a lookup.

### A failed or erroneous body

- A body that type checking found an error in (`has_errors`, a type mismatch, or `is_erroneous`)
  returns `not_checked` before reading the body, as rustc's `thir_body` does: one memo read of
  the inference result, ≈ 10 ns.
- A pattern that fails to lower (an erroneous type, a constant that does not evaluate) stops that
  match and marks the body `failed`, without reporting more.

### Summary

| operation | bound | today | off | removes the rest |
|---|---|---|---|---|
| body with nothing to check, n = 200 | ≈ 0.1 µs | ≈ 0.12 µs, 0 allocations (was 1.7 µs) | ≈ 1.2× | at bound: kind-only reads (queue 00795) and no lint walk |
| `match`, 4 arms | analysis + ≈ 0.1 µs | analysis + ≈ 0.2 µs, ≈ 4 + 1 allocations | ≈ 1.15× | arena in `rustc_pattern_analysis`, a "names a static" bit in inference |
| erroneous body | 1 memo read | same | 1× | — |
| warm body, unchanged | salsa memo hit | same | speed-salsa's | — |

Witness text, typo search (a walk of the crate's module scopes, only for an unreachable catch-all
binding) and source-map conversion in `hir` run only for a diagnostic that fires, so they are on
no build's path.

## hir-def: attributes, interned locs, lang items (speed-hir-sol-defs)

What this covers, all in `crates/hir-def/src/`:
- `AttrFlags::query` of an item (`attrs.rs`), cold and as a hit;
- a value query gated by a flag (`export_name`, `repr`, `lang_item`);
- docs (`attrs/docs.rs`), cold;
- interning a loc (`lib.rs`);
- `crate_lang_items` and `lang_items` of a crate (`lang_item.rs`);
- `upvars_mentioned` of a body (`upvars.rs`);
- the read path of `deps/serve.rs`.

Costs are counted from the code as of 10-29 21:00, not measured. Salsa costs are speed-salsa's,
above: a memo hit is 11 ns today and 5.4 ns at its bound, and an execution's bookkeeping is
140 ns today and 35 ns at its bound. A file has 150 items. Zero-allocation budgets for the hit
paths are in `alloc_budgets.rs` (the crate's counting allocator is requested from db-expand, in
`v2/requests/db-expand.md`).

### `AttrFlags::query`: an item's attribute flags

- **Work:** find the item's attributes, which are the first children of its node, read each
  one's name, and set one bit per known name. A typical item has one attribute of about 20 bytes.
- **Bound, cold, one attribute:**
  - locate the item's first element: one line if the ast id gives the element index, ≈ 4 ns;
  - walk the attribute children: 1 to 2 lines, ≈ 5 ns;
  - per attribute: the name's bytes, already in those lines, then one hash and one compare in
    the `name_set!` perfect hash, ≈ 30 cycles, ≈ 9 ns;
  - the salsa execution, 35 ns;
  - **≈ 55 ns per item**, ≈ 8 µs for a file of 150 items.
- **Today, cold, one attribute (`attrs_source` → `collect_attrs_in` → `match_attr_flags`):**
  - `lookup` of the loc, ≈ 2 ns;
  - `loc.source(db)`: memo hits on `ast_id_map` and `parse`, 2 × 11 ns;
  - `parse_or_expand` makes a new root (`SyntaxNode::new_flat_root`): one `Box`, an atomic
    increment and decrement of the file tree's `Arc`, and a free, ≈ 30 ns. That count's line is
    shared by every worker reading items of the same file, so under contention it costs more;
  - `AstPtr::to_node` → `FlatTree::node_at_range`: a binary search over the start column of
    the file's ≈ 30k elements, which is 15 dependent loads, mostly L2 hits, ≈ 210 cycles,
    ≈ 60 ns, plus 1 to 3 parent steps;
  - `loc.krate(db)`, ≈ 2 ns;
  - the walk of the item's children and the inner-attribute check, ≈ 17 ns;
  - per attribute: `meta`, `path`, `segment` and `name_ref` cursor steps, then the perfect
    hash, ≈ 80 cycles, ≈ 23 ns;
  - the salsa execution, 140 ns;
  - **≈ 300 ns and 1 allocation per item, ≈ 5.5× off**; ≈ 45 µs per file.
- **Applied (sol-all-hirdef, 10-30; counted, not built):** changes 1, 2 and 4 below. The flags
  of a file's items are one column, `file_attr_flags(file, krate)` in `attrs.rs`, built in one
  walk of the outline's items (the file's, each inline module's, impl's, trait's and extern
  block's, and each enum's variants), and `AttrFlags::query` is one binary search of it. Modules,
  items in bodies and items a macro wrote are walked as before. Cold, per item:
  - the column's share: a walk step ≈ 5 ns, the item's ast id (`ast_id_for_ptr`, one probe)
    ≈ 10 ns, its attributes and the perfect hash ≈ 14 ns, a push and its share of the sort
    ≈ 8 ns: ≈ 37 ns, ≈ 5.5 µs and 2 allocations per 150-item file;
  - the query: the loc ≈ 2 ns, its crate ≈ 2 ns, the column's memo hit 11 ns, a binary search
    of 150 ids (600 B, 8 steps in L1) ≈ 10 ns, and the execution, 140 ns;
  - **≈ 200 ns and 0 allocations per item, ≈ 3.6× off**; ≈ 95 ns at salsa's execution bound,
    ≈ 1.7×. What is left: the column's memo hit and the search (a dense index of the item in
    its outline, which change 3's element index gives), and the id probe.
- **Hit:** one memo hit and a 16-byte copy, 11 ns today against 5.4 ns at the bound. That is
  salsa's 2×, and allocates nothing (`attribute_flag_reads_allocate_nothing`).
- **A body edit, the larger gap:**
  - `AttrFlags::query` reads the item off the live `parse` and `ast_id_map`, and both change
    on every edit of the file;
  - the signature queries read it (`signatures.rs` lines 612, 714, 822, 929, 1024, and through
    `repr` and `repr_assume_has`), so verifying them after a body edit re-runs the flag query
    of every item of the file. `fields_attrs` and `query_generic_params` re-run the same way;
  - each result backdates, so nothing downstream runs, but the edit pays ≈ 150 × 300 ns
    ≈ **45 µs per file** for items, plus one run per variant's fields and per generic def's
    params;
  - at the bound, the queries read only `file_outline`, which a body edit leaves the same, so
    each is one deep-verify edge: 150 × 5.4 ns ≈ 0.8 µs. That is ≈ 50× off. The signatures
    section above counts these 45 µs in its body-edit total: ≈ 60 µs per file today, ≈ 15 µs
    once this change lands.
  - **Applied:** change 1 (db-expand-attrs, 10-29), then the column (sol-all-hirdef, 10-30).
    A body edit leaves `file_outline` the same, so `file_attr_flags` verifies by one edge and
    each item's query by one: ≈ 150 × 11 ns ≈ 1.7 µs today, the bound at salsa's 5.4 ns, so
    **2×, salsa's** (counted, not built; `a_body_edit_runs_no_attribute_query` checks that
    nothing runs).
- **What stands in the way, in order:**
  1. **Read attributes off `file_outline`, not the parse** (`attrs_source` through
     `item_syntax`'s outline lookup in `signatures.rs`). This removes the per-edit re-runs.
     A cut body loses any inner attributes it starts with (`fn f() { #![allow(x)] ... }`), so
     `bodies_under` must leave uncut a body whose text, after trivia, starts with `#!`. Items
     not in the outline, those inside bodies or written by macros, keep reading their file:
     an edit there changes what they are read from anyway.
  2. **Walk the borrowed view (`syntax::view`, `Parse::view`) instead of owned cursors.** This
     removes the root `Box` and the shared `Arc` write per item. It needs a view form of
     `hir_expand`'s `expand_cfg_attr`.
  3. **Keep the element index in the ast id map** (`span`'s `AstIdMap`), so `to_node` is one
     load, not a 15-step search. This helps every `loc.source` in the front end.
  4. **One pass per file:** a `file_attr_flags(db, file)` column of every item's flags, built
     in one walk of the outline (≈ 150 × 20 ns ≈ 3 µs), with `AttrFlags::query` as a tracked
     read of one entry, so an attribute edit still changes only its own item's result. Cold,
     each item then costs one execution and one load.
  5. Salsa's execution cost, 140 ns → 35 ns, is speed-salsa's.

  1, 2 and 4 are applied (above; 4 keyed by file and crate, since cfg is the crate's, and
  read by a binary search until 3 lands). 3 is format's (`span`).

### A value query gated by a flag (`export_name`, `link_section`, `repr`, `lang_item`)

- **Without the attribute** (most items): the flag hit, then one bit test that returns `None`.
  That is 11 ns today against 5.4 ns at the bound, with 0 allocations
  (`gated_value_reads_allocate_nothing`). At salsa's bound.
- **With the attribute, cold**, once per item that has one (1 to 5% of items):
  - **Bound:** the value's bytes are in the lines the flag walk read. Locating them takes
    ≈ 15 ns, interning a name (`Symbol::intern`: one hash and one probe) ≈ 25 ns, parsing a
    `repr` hint ≈ 20 ns, and the execution 35 ns, so **≈ 75 ns**.
  - **Today:** a second `attrs_source` and walk (≈ 160 ns, including the root `Box`), the
    value (`symbol_attr` for a literal, `parse_repr_tt` for `repr`,
    `LangItemEnum::from_symbol`, which is one load through `KnownNames`), and a 140 ns
    execution, so **≈ 330 ns, ≈ 4.5× off**. `parse_repr_tt` also clones a token per `packed`
    or `align` hint; requested from db-expand.
  - **What stands in the way:** changes 1 to 3 of `AttrFlags::query`. The second walk itself
    stays: only items with the attribute pay it, and storing every value with the flags would
    widen the 16-byte flag memo that every item reads.
  - **Now (changes 1 and 2 applied; counted, not built):** the walk reads the outline's view,
    so the root `Box` and the shared count are gone: the outline's memo hit 11 ns, the id probe
    ≈ 10 ns, `to_node`'s search of the outline's elements ≈ 50 ns, the walk and the attribute
    ≈ 40 ns, the value, and the execution, **≈ 260 ns, ≈ 3.5× off**; ≈ 155 ns at salsa's
    execution bound, ≈ 2×, the rest being change 3's search (format's).

### Docs, cold

- Read only by `blaze-doc` and `hir` (hover), never by a build.
- **Bound:** copy the doc's bytes once into a buffer sized from the sum of the line lengths,
  then strip the `///` and the indent of each line. A 10-line doc of 500 bytes is ≈ 20 ns of
  copy, 10 × 5 ns of stripping, one allocation and an execution, so **≈ 130 ns**.
- **Today:** `attrs_source` (≈ 140 ns of the cost above), a `String` grown line by line (≈ 7
  reallocations for 500 bytes), line vectors per block comment in `get_vertical_trim` and
  `remove_indent`, and a source-map entry per line. That is about ten allocations and
  **≈ 1 µs, ≈ 8× off**.
- Not pursued: it is on no build's path. If `blaze-doc` makes it matter, the change is a
  sizing pass before the copy and one scratch buffer per worker for the line vectors.

### Interning a loc (a hit)

- **Work:** hash the loc's packed words, probe the intern table, compare the key.
  `ItemLoc` is 24 bytes (3 Fx rounds through `hash_ast_id`'s packing), `AssocItemLoc` 28 bytes
  (3 rounds), and `EnumVariantLoc` and `ProcMacroLoc` 32 bytes (4 rounds).
- **Bound:** 3 dependent Fx rounds at ≈ 4 cycles each, ≈ 12 cycles, then salsa's probe: the
  control group line and the value line, ≈ 15 cycles hot, with the compare in the value line.
  So **≈ 27 cycles, ≈ 8 ns**.
- **Today:** the same. Locs are immortal (`revisions = usize::MAX` in `impl_intern_key`), so
  `intern_id` takes no lock, records no revision, and reports no read
  (`report_tracked_read_if_reusable` skips a value that can't be reused). **At the bound**,
  with 0 allocations (`interning_a_known_loc_allocates_nothing`).

### `crate_lang_items` and `lang_items` of a crate

- **A crate without `#![feature(lang_items)]`** (all but a few): one `crate_def_map` memo hit
  and the feature test, then `None`. ≈ 20 ns, at the bound.
- **`core` from its image** (every build after the toolchain's first): `served_lang_items`
  decodes ≈ 0.7 KB (182 entries of 4 bytes), ≈ 1 µs. At the bound.
- **`core` resolved, cold** (≈ 25k items, counting associated items and variants, ≈ 400 of them
  lang items):
  - **Bound:** a scan of the items' flags as columns (25k × 16 bytes = 400 KB at 30 GB/s,
    ≈ 13 µs), plus ≈ 400 value reads at ≈ 75 ns, ≈ 30 µs. So **≈ 45 µs**, and the columns'
    cost is the one `AttrFlags` already pays, spread across workers.
  - **Today:** one pass over the modules that calls `AttrFlags::lang_item` per item, with memo
    hits on `trait_items`, `impl_items` and `enum_variants` per container. With every flag
    warm, ≈ 25k × 14 ns ≈ 0.35 ms, ≈ 8× off. If this pass is the first reader of `core`'s
    flags, it runs them all on one thread: 25k × 300 ns ≈ **7.5 ms serial**.
  - **What stands in the way:** change 4 of `AttrFlags::query` (flags as per-file columns,
    built in parallel), then a pass that reads the columns' `LANG_ITEM` bits.
  - **Applied (sol-all-hirdef, 10-30; counted, not built):** the pass reads each item's flags
    off its file's column through one cursor (`CrateAttrFlags`, which keeps the last file's
    column), and runs no item's `AttrFlags::query`. Per item: the loc and its crate ≈ 4 ns, the
    file compare ≈ 1 ns, the binary search ≈ 10 ns, so 25k × 15 ns ≈ 0.38 ms, plus ≈ 400 value
    reads. With the columns warm: **≈ 0.4 ms, ≈ 9× off**. Before the pass it builds the
    columns of the crate's module files, one task a file on every core (`build_columns`, over
    `Db::run_all`): 25k × 37 ns ≈ 0.9 ms of work, ≈ 60 µs on 16 cores. As the columns' first
    reader, with 400 × 260 ns of values: **≈ 0.55 ms, ≈ 12× off**, from 7.5 ms. A built
    column is a memo hit, so a re-run pays ≈ 400 file hits and the pool's dispatch.
  - **What is left:** the bound scans the columns' bits and maps only the ≈ 400 hits back to
    items. That needs the def of an ast id (no map has it: an item reaches its ast id through
    its loc, not the other way), either kept beside each column or in the def map. A per-file
    list of the lang-item entries plus that reverse map gives ≈ 2.6k file reads + 400 lookups,
    near the bound; it is a def-map layout change, not made here.
- **`lang_items(db, krate)`**, once per crate: a copy of the crate's own ≈ 0.7 KB, then for
  each of \(d\) crates in its extern prelude, a memo hit and `merge_prefer_self` over 182
  entries (≈ 100 ns). For \(d = 10\), ≈ 1.2 µs plus an execution. That is the work itself, at
  the bound.
- **A lookup** (`lang_items(db, krate).Sized`, `LangItemEnum::from_symbol`): one memo hit and
  one load, 11 ns today against 5.4 ns at the bound, with 0 allocations
  (`lang_item_lookups_allocate_nothing`). Salsa's 2×.

### `upvars_mentioned` of a body

Figures: a body of \(n = 200\) expressions, 20 bindings, one closure of 30 expressions with 5
single-segment paths that may name a local.

- **A body with no closure:**
  - **Bound:** one bit, `has closures`, kept in the store at lowering: one load and an
    execution, ≈ 40 ns.
  - **Before:** an `ExpressionStore::of` hit, then a scan of the 200 `Expr`s (≈ 8 KB at
    30 GB/s, ≈ 0.3 µs), then the execution: ≈ 0.45 µs, ≈ 11× off.
  - **Now:** the bit is in the store (`ExpressionStore::has_closures`), so the query is the
    store's hit, the root check and the bit: 11 + 2 + 1 ns and the execution, **≈ 155 ns,
    ≈ 3.9× off; at salsa's bound ≈ 50 ns, ≈ 1.25×**, the rest being the store's memo hit
    (counted, not built).
- **A body with a closure:**
  - **Bound:** walk the closure's own expressions (30 × 3 ns). For each candidate path, one
    load of the local it resolves to if lowering kept it (5 × 5 ns), and a compare of that
    local's owner. Then one allocation for the result and the execution: **≈ 170 ns**.
  - **Today:**
    - the scan, ≈ 0.3 µs (now the store's bit, ≈ 1 ns);
    - `owner.resolver(db)`: several memo hits and the scope chain's `Vec`, ≈ 0.15 µs (the
      chain is now inline, `SmallVec<[Scope; 8]>`, ≈ 0.1 µs);
    - the `binding_names` set, 20 inserts and 1 to 2 allocations, ≈ 0.15 µs (now in the frame);
    - the visitor over all 200 expressions, ≈ 1 µs;
    - per candidate, `update_to_inner_scope` and `resolve_path_in_value_ns_fully`, ≈ 150 ns,
      so 0.75 µs;
    - the result map, its `Box` and each closure's `Box<[BindingId]>`, 3 to 4 allocations;
    - **≈ 2.4 µs, ≈ 14× off** before; with the bit, the inline chain and the frame set,
      **≈ 2 µs, ≈ 12× off** now (counted, not built): the walk of the whole body and the
      resolver per candidate are what is left.
  - **What stands in the way:**
    - The walk covers the whole body. A closure's expressions are lowered before it, so they
      sit in a contiguous range ending at the closure, and the walk can cover just those.
    - The `Resolver` is built only to find locals. `ExprScopes`' lookup from the expression's
      scope, with the path's hygiene, is exactly the local part of the resolver's answer:
      locals shadow items in the value namespace, so a name that a scope holds is that local.
      rustc's `upvars_mentioned` reads `Res::Local` off HIR, already resolved. The same
      result here is the local each path names, recorded when the store is lowered.
- **A read** (`upvars_mentioned(db, owner)` then `get(closure)`): a memo hit and one FxHash
  probe, ≈ 15 ns, with 0 allocations (`upvar_reads_allocate_nothing`).

### `deps/serve.rs`: serving dependency images, once per session

Figures: 300 dependency crates whose images total \(S\) bytes. \(S = 50\) MB is assumed here;
the image sizes are not measured.

- **Work:** key each crate (BLAKE3 over its content, source root and dependencies' keys, about
  200 bytes each), fetch each image, check its bytes against its key, open its header and
  parts, check that its files are the crate's, and set the `DepImages` input.
- **Bound:** the bytes are hashed once, in parallel. BLAKE3 at ≈ 6 GB/s per core on 16 cores
  takes 50 MB in ≈ 0.5 ms. The parts are used in place, with no copy. Keys take 300 × 0.2 µs
  ≈ 60 µs, and the opens and checks ≈ 300 × 0.2 µs. So **≈ 0.6 ms**.
- **Today:** the loop in `serve` is serial. Per crate, it calls `images.get` (the store's read
  and its BLAKE3 check, on the calling thread), then `ImageRef::open`, then
  `frozen::unreadable`. Then `def_map.into()` and the lang-items part copy each part into a
  new `Arc<[u8]>`. That is 50 MB / 6 GB/s ≈ 8.3 ms of hashing plus ≈ 3 ms of copy at 15 GB/s,
  not counting the store's own read: **≈ 12 ms, ≈ 20× off.**
- **What stands in the way:**
  - A batched store read. `ImageStore::get_many(&[ContentId])` would let the store read and
    verify the images in parallel; this is frontend-deps' interface and the store adapter's
    implementation.
  - The copy. `Frozen` could hold the image's `Arc<Aligned>` and the part's range in place of
    an `Arc<[u8]>` copy, which also keeps the image's alignment.
  - `hits` is not sized ahead; requested from frontend-deps, it is minor.
- **Now (counted, not built):** both are applied in `serve`. Each crate's `fetch` (the store's
  read and BLAKE3 check, `ImageRef::open`, `frozen::unreadable`) is one task of
  `ImageStore::for_each`, on the driver's cores, and the frozen def map, lang items and image
  hold the fetched buffer with a part's range (`PartBytes::In`), with no copy. Hashing is
  50 MB / (6 GB/s × 16) ≈ 0.52 ms, keys ≈ 60 µs on the calling thread, and the serial merge
  of 300 results (map inserts, ≈ 50 ns each) ≈ 15 µs: **≈ 0.6 ms, at the bound**, if the
  driver's `for_each` runs on all cores (its default runs in order, ≈ 8.5 ms; the adapter's
  pool is the driver's). Not measured: the image sizes are assumed.

### Summary

| operation | bound | today | off | removes the rest |
|---|---|---|---|---|
| `AttrFlags::query`, cold, per item | ≈ 55 ns | ≈ 200 ns, 0 allocations (column applied; was ≈ 300 ns, 1) | ≈ 3.6× (≈ 1.7× at salsa's bound) | element index (format's `span`), salsa's execution |
| `AttrFlags` per file after a body edit | ≈ 0.8 µs | ≈ 1.7 µs (attributes off the outline, applied; was ≈ 45 µs) | 2× | salsa's verify edge |
| `AttrFlags` hit | 5.4 ns | 11 ns, 0 allocations | 2× | salsa's |
| gated value, no attribute | 5.4 ns | 11 ns, 0 allocations | 2× | salsa's |
| gated value with the attribute, cold | ≈ 75 ns | ≈ 260 ns (outline view applied; was ≈ 330 ns) | ≈ 3.5× (≈ 2× at salsa's bound) | element index (format's), salsa's execution |
| docs, 10 lines, cold | ≈ 130 ns | ≈ 1 µs, ≈ 10 allocations | ≈ 8× | not on a build's path |
| loc intern hit | ≈ 8 ns | ≈ 8 ns, 0 allocations | 1× | — |
| `crate_lang_items`, no lang feature | ≈ 20 ns | ≈ 20 ns | 1× | — |
| `crate_lang_items` of `core`, cold | ≈ 45 µs | ≈ 0.4 ms warm columns, ≈ 0.55 ms cold (columns applied, built on every core; was 0.35 / 7.5 ms) | 9× to 12× | the def of an ast id, to map only the column's lang-item entries back (a def-map layout change) |
| lang-item lookup | 5.4 ns | 11 ns, 0 allocations | 2× | salsa's |
| `upvars`, no closure | ≈ 40 ns | ≈ 155 ns (the bit, applied; was ≈ 0.45 µs) | ≈ 3.9× (≈ 1.25× at salsa's bound) | salsa's |
| `upvars`, one closure | ≈ 170 ns | ≈ 2 µs (was ≈ 2.4 µs) | ≈ 12× | the closure's range, locals kept at lowering |
| `serve`, 300 crates, 50 MB | ≈ 0.6 ms | ≈ 0.6 ms (fetch on the driver's pool, parts in place, applied; was ≈ 12 ms) | ≈ 1× | — |

## hir-expand: macro expansion (speed-hir-sol-expand)

What this covers, per macro call:
- `macro_arg`: the argument's syntax to a token tree;
- a `macro_rules` expansion, apart from `mbe`'s matcher and transcriber (format's, whose cost
  this section takes as given and calls \(M\));
- `parse_macro_expansion`: the expansion's tokens to a flat tree and its span map;
- a builtin derive of a struct of \(f\) fields;
- a memo hit, a `Name` and a `ModPath`, a span lookup, a hygiene mark;
- and the critical path of one expansion round.

Costs are counted from the code as of 10-29 (after frontend-db-def's `&Db` migration), not
measured; "today" includes the changes requested of db-expand-macros in
`v2/requests/db-expand-macros.md` (`## speed-hir-sol-expand`) only where it says "after".
Machine figures are the lane's: 3.5 GHz, IPC about 2 on this branchy code (so 1 instruction is
about 0.14 ns), L1 4–5 cycles, L2 about 14. Salsa's figures are speed-salsa's, above: a memo hit
is 11 ns today and 5.4 ns at its bound, an intern hit about 4.3 ns, an execution's bookkeeping
about 140 ns today and 35 ns at its bound. An allocation and its free are taken as about 20 ns.

### A memo hit (`parse_macro_expansion`, `macro_arg_considering_derives`, `span_map`, `ast_id_map`, `expand_proc_macro`)

- **Work:** one salsa fetch. Every one of these is `returns(ref)`, keyed by an id.
- **Bound:** salsa's hit, **5.4 ns**; hir-expand's own part is nothing, and 0 allocations.
- **Today:** salsa's 11 ns, nothing added: values come back by reference, `span_map` is an enum
  of a reference. A derive's argument is its `#[derive]` attribute's, one memo for all its
  derives. An eager call (`concat!`, `include_str!`, `env!` and the rest) today runs `macro_arg`
  once to copy its already-expanded argument out of its location into a memo: \(t \times 16\) B
  and about 5 allocations, kept until evicted. **After** request block 1 it reads the location
  in place (one interned read, about 2 ns, no memo), and only eager calls pay for it, because
  the test is on the `kind` the caller already holds.
- **Gap:** speed-salsa's 2×. Budget test: `macro_expansion_tests/budgets.rs` (requested), 0
  allocations for all four hits.

### A `Name`, a `ModPath`, a span lookup

- **`Name`** is a `Symbol` (4 bytes, `Copy`). From a symbol that exists: a move, 0 instructions
  beyond it. From text (`Name::new_root`): one symbol-interner hit, hashing the text; nothing
  hir-expand can remove. Tuple-field names 0 to 15 are pre-interned. **At the bound**, 0
  allocations. Request block 4 drops a `(*self.name).clone()` and the doc comment's "8 bytes".
- **`ModPath`** is 16 bytes, `Copy`: a kind and an interned slice of segments, gathered on the
  stack (8 inline). From segments whose list exists, or `pop_segment`/`push_segment` onto one: an
  intern hit hashing \(4s\) bytes, about 15–20 ns for \(s \le 4\). `from_kind` is a constant.
  **At the bound**, 0 allocations.
- **Span lookup:** in order (the `InOrder` cursor `macro_arg` uses): two compares, about 2 ns.
  Scattered: a binary search of the file's anchors (about 150 a file, so 8 dependent probes,
  mostly L1, about 15–20 ns) or of an expansion's 4-byte `ends` column (16 to a line, so the last
  4 probes share one). **At the bound**, 0 allocations. What is left is span size: an entry is
  24 B (a 20-byte `Span` against rustc's 8), speed-span's.
- Budget tests: `hir-expand/src/budget_tests.rs` (requested), 0 allocations each.

### `macro_arg`: a call's argument, syntax to tokens (\(t\) tokens)

- **Work:** find the call's node from its `AstId`, then per token: its kind and range from the
  flat tree, intern its text if an identifier or literal, its span by the in-order cursor, and
  write a node (8 B) and a packed span (8 B).
- **Bound:**
  - Per token, about 40 instructions: 10 to walk, about 15 to intern (an intern hit of about 30,
    on half the tokens), 5 for the span and 10 to write. That is 20 cycles, **≈ 5.7 ns a token**.
    It writes 16 B a token, so bandwidth is not the limit.
  - Fixed: salsa's execution (35 ns), the pointer to node descent (about 10 levels, 15 ns), the
    call path's span (one binary search, 20 ns), and one allocation for the output (20 ns):
    **≈ 0.1 µs**.
  - A typical call of \(t = 20\): **≈ 0.2 µs**, **1 allocation**.
- **Today:** the per-token loop is the bound's (syntax-bridge's `convert_tokens`, with the
  builder sized by the subtree's length, so it does not grow). The fixed part is salsa's 140 ns
  plus about 10 allocations: the builder's columns, then `TopSubtree`'s five boxed columns
  (nodes, spans, wide, parts, symbols) packed exact by `finish`. That is about 0.2 µs of
  allocation, so **≈ 0.5 µs for \(t = 20\), about 2.5× off**, all in the fixed part and none of
  it in hir-expand's code.
  - Attribute and derive inputs add `cfg_process` (a memo hit on the file's syntax-error ranges,
    then the item's attributes); after request block 5 an answer that keeps or drops a node
    allocates nothing, and an expanded `cfg_attr` costs 2 small allocations (the bridge's
    callback returns a `Vec<tt::Leaf>`).
  - `fixup_syntax` runs only when a syntax error touches the item, so it is off the build path.
- **Gap:** format's `TopSubtree` as one allocation (the five columns in one block) and a
  reused per-thread builder (as the parser's input already is), with salsa's execution bound:
  then within 10% of the bound.
- **Now (10-30, counted, not built):** both are in `tt` (`from_columns_in` writes one exact
  block; `TopSubtreeBuilder::with_capacity` takes the thread's scratch columns, which
  `syntax-bridge` sizes by the subtree's length). The fixed part is salsa's 140 ns, the
  descent, the span and the one block: \(20 \times 5.7\) + 195 ns **≈ 0.31 µs, 1 allocation,
  ≈ 1.55×**, at the bound at salsa's execution cost.
- **Budget:** a stated constant, today at most 12 allocations a call and the same for 16
  tokens as for 1024 (`an_argument_costs_a_constant_number_of_allocations`, requested);
  1 at the bound.

### A `macro_rules` expansion (\(n\) output tokens)

- **Work:** \(M\), plus the definition (`decl_macro_expander`, a memo hit) and a hygiene mark on
  each transcribed token's span.
- **Bound:** \(M\) + 5.4 ns + per token one look in a 4-entry cache (about 5 instructions, under
  1 ns) + per distinct (context, call) pair one mark: 1 to 3 `SyntaxContext` interns, about 15 ns
  on hits. A body carries 1 or 2 contexts, so **\(M\) + ≈ 0.75 ns × \(n\) + ≈ 40 ns**.
- **Today:** that is what `declarative.rs` does (`MarkCache`, round 1). **At the bound** in
  hir-expand's part. A new context is an execution (salsa's 140 ns today) the first time it
  appears in the build, once per call and context, not per token.

### `parse_macro_expansion` (\(n\) output tokens)

- **Work:** the tokens to parser input, the parse, the flat tree and its span map (one 24 B entry
  a token), then for an expression whose expansion ends in `;`, that `;` dropped.
- **Bound:** the grammar's cost a token (format's) + about 10 cycles a token for the input and
  tree building (≈ 3 ns) + the span map's 24 B write; a constant number of allocations (the tree's
  columns and the map's two). Editions are asked once per context (`Editions`, the last one and
  a few near it), not per token.
- **Today:** that, plus `without_last_semicolon`'s scan of the top-level tokens of an expression
  expansion (2 or 3 instructions a token). When the expansion does end in `;` (rare: a
  statement-like body called as an expression) it copies the tree without it (\(n \times 16\) B,
  about 5 allocations). **At the bound** but for that rare copy; the rest is format's.

### A builtin derive of a struct of \(f\) fields

- **Work:** read the item's name, generics and \(f\) field names (and, for some derives, types),
  and write each output token once. `Clone` writes about 18 tokens a field
  (`f0: ::core::clone::Clone::clone(&self.f0),`) and about 40 more.
- **Bound:** the item's shape is read from syntax that already exists: the file's own tree,
  or, when `cfg_attr` changed the item, one parse per `#[derive]` attribute shared by its
  derives. Per field about 18 tokens × 10 instructions, **≈ 26 ns a field**, plus salsa's
  execution and one output allocation, **≈ 0.1 µs**. `#[derive(Clone)]` of 16 fields:
  **≈ 0.5 µs**, a constant number of allocations independent of \(f\).
- **Today:**
  - Each derive reparses the item from its token tree (`to_adt_syntax`, a full
    `token_tree_to_syntax_node`): for an item of \(t\) tokens at roughly 12 ns a token, 0.9 µs
    for a 16-field struct (about 75 tokens), where the bound parses nothing.
  - Each generic parameter and bound is converted back to a token tree of its own (about 5
    allocations each).
  - The output builder starts empty and doubles, about 4 columns × \(\log_2(18f)\) allocations.
  - After request block 6, the field list is reserved once; no allocation is per field
    (`a_derive_allocates_nothing_per_field`, requested: at most 64 for 16 fields, and 256 fields
    adds at most 48).
  - In all, about 35 allocations (0.7 µs) for `Clone` of 16 fields: **≈ 2 µs a derive, about 4×
    off**, and 5 derives on one item spend about 4.5 µs reparsing next to 2.5 µs of output.
- **Gap, in order of size:**
  1. No reparse: `BasicAdtInfo` from a query keyed by the `#[derive]` attribute's call, read by
     each derive, built from the file's syntax (or from one parse when `cfg_attr` changed the
     item).
  2. The builder sized from \(f\) (each derive knows its tokens a field).
  3. Generic parameters as ranges of the argument tree, not copies.
- **Now (10-30, counted, not built):** 2 is moot: every derive writes through
  `TopSubtreeBuilder`, whose columns are a per-thread scratch kept between uses (`tt`'s
  `Scratch`), and its output goes to the task's frame (`build_skip_top_subtree_in`), so the
  doubling is paid once per thread, not per derive. That leaves the reparse (≈ 0.9 µs), the
  output (≈ 0.4 µs) and the execution: **≈ 1.5 µs, ≈ 3× off**. 1 is still open: its
  `BasicAdtInfo` must carry the spans the reparse's token map gives today (each token's span
  is the file's, through the argument's span map), so built from the file's syntax it reads
  them off the file's span map, and a derive's output must stay byte-identical; that wants a
  test comparing every builtin derive's output with the reparse path's before it replaces it.

### One expansion round: the critical path

- **Work:** every call collected in the round expands (argument, expansion, parse), then name
  resolution reads the results.
- **Bound:** \(\max(\text{longest single expansion}, W / c)\) for the round's work \(W\) on
  \(c\) cores. Expansions in a round don't depend on each other: each reads its own argument
  and an immortal definition. For a crate of a few thousand calls of 0.5–2 µs, \(W\) is a few
  ms, so \(W / c\) dominates unless one call is a large `include!` or a deep recursive
  `macro_rules`, whose own chain is then the floor.
- **Today:** the collector expands a round's calls one after another on one thread
  (speed-hir-sol-nameres's section, "`crate_def_map`, cold"), so the round costs \(W\), **\(c\)
  times the bound**. hir-expand's queries take `&Db` and share nothing mutable across calls (the
  mark cache is per expansion), so they are ready for a parallel round. The order is fixed by
  the call's position, not by which finishes first. What stands in the way is the collector,
  name resolution's.

### Summary

| operation | bound | today | off | removes the rest |
|---|---|---|---|---|
| memo hit | 5.4 ns, 0 allocations | 11 ns, 0 allocations | 2× | speed-salsa's |
| `Name` / `ModPath` / span lookup | a move / an intern hit / 2 compares | same | 1× | — (span size: speed-span) |
| `macro_arg`, \(t = 20\) | ≈ 0.2 µs, 1 allocation | ≈ 0.31 µs, 1 allocation (one-block `TopSubtree` and the per-thread builder, applied by format; was ≈ 0.5 µs, ≈ 10) | ≈ 1.55× (at the bound at salsa's) | salsa's execution |
| eager call's argument | one interned read | a copy and a memo | — | request block 1 |
| `macro_rules`, \(n\) tokens | \(M\) + 0.75 ns × \(n\) | same | 1× | — |
| `parse_macro_expansion` | grammar + 3 ns × \(n\) | same, a rare copy | ≈ 1× | — |
| `#[derive(Clone)]`, 16 fields | ≈ 0.5 µs | ≈ 1.5 µs, a constant few allocations (output in the frame through the per-thread builder, applied; was ≈ 2 µs, ≈ 35) | ≈ 3× | no reparse per derive (≈ 0.9 µs), generics as ranges |
| a round | \(\max(\text{longest}, W / c)\) | \(W\) | \(c\)× | a parallel collector (nameres) |

## base-db: inputs and views (speed-hir-sol-basedb)

Files: `crates/base-db/src/{files,table,view,lib,editioned_file_id,input,content}.rs`. Costs are
counted from the code as of 10-29 20:40, not measured. Round 1's before/after (no locks on reads,
nothing done per unchanged file of a view, batched BLAKE3) is in
[speed-hir-basedb.md](speed-hir-basedb.md). The exact edits named "block N" below are in
`v2/requests/frontend-db-base.md`, under "speed-hir-sol-basedb"; frontend-db-base applied them.
A second pass (sol-all-base, 10-30) made the root's content ID a trie, edits a root's file set
in place, and compares a crate graph before hashing it; its costs are counted from the code too,
with nothing built. The budget tests are in `crates/base-db/src/budget_tests.rs`. A "line" is a dependent cache-line load, as in
speed-salsa's section above. salsa's input-field read is counted like the first lines of its memo
hit: bucket, page, slot.

### A file-text read (`db.file_text(file).text(db)` in a query)

- **Work:** map the file's dense ID to its `FileText` input, read the `text` field with its
  revision and durability, and record the read.
- **Bound:** the ID is an index, so the field's address is arithmetic.
  - That is 1 line (the input's slot) plus recording the read (3 ns for a new key, 1 ns for a
    repeat, at salsa's bound).
  - Hot: 4.5 cycles + about 15 instructions / 2 ≈ 12 cycles ≈ 3.5 ns, so **≈ 5 ns** with the
    record.
  - Cold: 1 miss, 110 ns latency, **≈ 5.5 ns** per read in throughput.
- **Today:** 0 allocations, 0 locks, nothing written (round 1). 8 dependent loads:
  - base-db's `Column`: the `Arc<Files>` pointer, the column's page directory, the page's
    `OnceLock` (one line shared by 4,096 files), and the page's word for the file;
  - salsa's input read: the ingredient cache, the table bucket, the page, and the slot.

  Six of the eight are lines shared by thousands of files and stay in L1. The per-file ones are
  the column word and salsa's slot. Hot: 8 × 4.5 + 15 ≈ 51 cycles, 15 ns, **3× off**. Cold: 2
  misses, 11 ns in throughput, **2× off**.
- **What stands in the way:** the file ID becomes the input's ID twice: once in base-db's
  column (file ID to salsa ID), then in salsa's table (salsa ID to slot).
  - The change: a salsa input whose slots are indexed by a dense key its creator gives, in a
    reserved region of `MAX_FILE_ID + 1` slots, with the slot at `base + size × index`. Then
    `FileText` of file `i` is slot `i`, and base-db's three `Column`s (text, source roots, file
    source roots) go. A read is the slot's line: the bound.
  - It rewrites how salsa allocates input IDs, so it waits on S5's first compile like speed-salsa's
    other changes. Owner: speed-salsa, then base-db.

### A source-root read

`db.file_source_root(file).source_root_id(db)`, then `db.source_root(root).source_root_ref(db)`.
- **Bound:** the file's input slot (1 line, per file) and the root's (1 line, shared by all the
  root's files, so hot): ≈ **8 ns hot**, 1 miss cold.
- **Today:** two reads of the file-text read's shape. Hot ≈ 30 ns, **3.5× off**. Cold: 2 per-file
  misses (the column word, the `FileSourceRootInput` slot), **2× off**. 0 allocations, and no
  `Arc` clone (`source_root_ref`, round 1).
- **What stands in the way:** the dense-input change above.

### `relevant_crates` (a memo hit)

- **Work:** the file's source root, then the memo of the crates rooted in it.
- **Bound:** the source-root read (1 per-file line), then one memo hit whose lines every file of
  the root shares: 5 + 5.4 ≈ **10 ns hot**, 1 miss cold.
- **Before block 3:** ≈ 30 ns hot, made of the source-root ID read (15 ns), an `info_span!`
  (≈ 100 ns with an info subscriber), interning `InternedSourceRootId` (an FxHash, 2 lines, and
  the first time each root is read in a revision a CAS on the interned value: a shared write on
  a read path, made by every worker at the start of a revision) and salsa's memo hit (11 ns).
- **Today (block 3 applied):** no span, and the query keyed by the root's `SourceRootInput`,
  read from its column: L1 loads, no hash, nothing written, 0 allocations
  (`reads_allocate_nothing`). The column read costs what the intern hit did, so it is still
  15 + 4 + 11 ≈ **30 ns hot, 3× off**; what it removed is the shared write and the span.
- **What stands in the way:** the dense-input change (the 15 ns read becomes 5) and salsa's memo
  hit (11 ns to 5.4, speed-salsa's rows by page). Both are outside base-db; base-db's own part
  of the hit is one column load.

### `EditionedFileId::parse` (a memo hit)

- **Bound:** one salsa memo hit: 5.4 ns hot, 5.5 ns in throughput cold.
- **Today:** base-db's part is at the bound:
  - `returns(ref)`, so no `Parse` clone (round 1);
  - `EditionedFileId` is interned immortal (`revisions = usize::MAX`), so the key is never
    validated;
  - the span and the reparse run only on execution;
  - 0 allocations (`a_cache_hit_of_parse_allocates_nothing`, `reads_allocate_nothing`).

  The hit itself is salsa's 11 ns, 2× off, closed by rows by page.

### `set_view_root`: a view of \(n\) files, \(k\) of them changed

- **Work:**
  - for each of the \(n\) files, tell whether it is what it was: its blaze-vfs ID against the
    one it was last set from;
  - for the \(k\) changed files, check the given ID against the bytes, set the text input, and
    update the source root's content ID;
  - find the files under the base that the view lacks.
- **Bound:**
  - **Unchanged file:**
    - one probe of the base's index by the path: an FxHash of a path of about 40 bytes (about 10
      cycles), the index slot and the entry (holding the file, its last ID and its mark), and a
      32-byte compare;
    - streaming the view's own entry: about 100 bytes (path, ID, slice) at 30 GB/s, ≈ 3 ns.
    - Files are independent, so their misses overlap: 2 × 5.5 ≈ **11 ns** from DRAM, ≈ 3 ns when
      the index sits in L3. At about 100 bytes per path, 100k paths are 10 MB, inside a 32 MB
      L3.
    - So \(n\) = 100k unchanged files take **≈ 0.4 ms (L3) to 1.4 ms (DRAM)**, with 0 allocations
      and 0 system calls.
  - **Removed files:** a pass over \(n\) 4-byte marks: 400 KB / 30 GB/s ≈ **13 µs** at 100k.
  - **Changed file:**
    - its bytes through BLAKE3 at the SIMD rate: about 0.5 cycles a byte with AVX-512 on Turin,
      about 1 on Graviton's NEON, so a 10 KB file is ≈ 1.5 µs;
    - its text ID: one compression in an 8-lane batch, ≈ 10 ns;
    - three input writes, ≈ 60 ns;
    - the root's content ID: \(O(\log n)\) BLAKE3. With 16-way nodes of 512 B, 100k files are 5
      levels, 2.5 KB, ≈ 0.4 µs.
    - Total **≈ 2 µs per changed 10 KB file**, and a constant number of allocations.
  - **System calls:** base-db needs none. The bytes are blaze-vfs's mapping.
- **Today:**
  - **Unchanged file:** 0 allocations, 0 locks (one `Mutex` per call), no BLAKE3 (round 1).
    Before blocks 1 and 2:
    - a `check_path` scan, about 1 cycle a byte;
    - the path's FxHash and three probes: the base's `FxIndexMap` (index slot, then entry, then
      the key's own `Box<str>`: 3 dependent lines), then `in_roots` and `view_ids`, two
      `FxHashMap`s by `FileId` (a control word and a bucket each, after the entry gives the
      file).
    - That was 7 random lines, 5 deep: ≈ 7 × 5.5 ≈ 38 ns from DRAM, 3.5× off (≈ 2.5× in L3).
    - On top of that, each call allocated 1 to 2 times, for the base's `VfsPath`.
  - **Today (blocks 1 and 2 applied):**
    - `in_roots` and `view_ids` are direct indexes by file ID (`ByFile`), a line each with no
      hash;
    - view bases are keyed by `AbsPathBuf` and looked up by `&AbsPath`, so a call allocates
      nothing (`an_unchanged_view_allocates_nothing`: 0 for 8 and 512 files, with and without
      built directories).
    - An unchanged file is the index slot (random) plus four lines that follow the view's
      order: the entry, the key, `in_roots[file]` and `view_ids[file]`. File IDs and index
      entries are made in the order paths are first named, and blaze-vfs lists a view sorted,
      so in the usual case the four stream and only the slot misses. That is ≈ 5.5 + 4 ≈
      **10 ns, at the bound**. A view whose order differs from the order its paths were first
      named makes the four random: ≈ 27 ns, 2.5×.
    - The fix for that case is to keep the file, its first root and its last ID in the index
      entry. That is a second copy of `view_ids` and `in_roots` to keep in step, so it waits
      for `proof` to show that out-of-order views matter.
  - **Removed files:** a pass over the marks, at the bound.
  - **Changed file:**
    - **Bytes:** hashed once, in one `ContentId::of_many` batch, which is now SIMD across
      inputs (blaze-ids `many.rs`: 16 lanes on AVX-512 for full blocks, 8 for last blocks). A
      file over 1 KiB goes through blake3's own SIMD over its chunks. That is at the bound for
      one file. On a branch switch, though, \(k\) files of 1 to 16 KiB (most source files) are
      each hashed alone, and a 4 KiB file fills 4 of 16 lanes: up to **4× off** the SIMD rate.
      The change: `of_many` puts chunks of different inputs into one `hash_many` and merges each
      input's chunk values after. Owner: lane format (`crates/blaze-ids`).
    - **Text:** the text ID in one batch (at the bound), and a copy into an `Arc<str>`, 0.3 µs
      per 10 KB. With the reparse slot that is 2 allocations per file, a constant
      (`a_changed_file_costs_a_constant`). Reading the text in place from blaze-vfs's mapping
      is frontend-text's design, and not base-db's alone.
    - **Root content ID, before:** the dirty buckets of the root's 64 were hashed again whole,
      after a walk of each bucket's `BTreeMap`. A 100k-file root has 1,560 entries a bucket:
      140 KB hashed (≈ 20 µs) plus about 3,000 dependent lines (≈ 15 µs), ≈ 35 µs a changed
      file, ≈ 80× the bound.
    - **Root content ID, today:** a trie of 16 ways by the hex digits of each path's hash
      (`RootContent` in `files.rs`; `SourceRoot::DOMAIN` is version 2, with `LEAF_DOMAIN` and
      `NODE_DOMAIN`). A node is a leaf while it has at most 16 files, so the shape, and with it
      the ID, is a function of the files alone, however they were edited in. A changed file
      marks its path; the refresh hashes every dirty leaf in one batch, then the dirty inner
      nodes a batch a depth, deepest first.
      - 100k random path hashes: depth 3 has 4,096 nodes of ≈ 24 files (split), depth 4 has
        65,536 of ≈ 1.5 (leaves), so a path is 4 inner nodes of 512 B and a leaf of ≈ 100 B.
      - One changed file: 4 dependent hashes of 512 B, each 8 compressions in one lane at about
        3 cycles a byte, ≈ 0.4 µs each, so ≈ 1.6 µs; the leaf and the root's 33 bytes ≈ 0.1 µs;
        the walk 5 dependent lines, ≈ 0.5 µs from DRAM. **≈ 2 µs at 100k files, against
        ≈ 35 µs: 17× less**, and 0 allocations (`an_edit_of_a_large_root_hashes_its_path_and_allocates_nothing`).
      - Against the 0.4 µs bound that is ≈ 5× for one file, because a path's nodes are
        dependent and each fills one lane. \(k\) files changed together share each level's batch,
        so from \(k \approx 8\) the levels hash at the SIMD rate, at the bound.
      - Tests: `a_root_edited_in_place_is_the_root_built_from_its_files` (4 seeds × 2,400 random
        inserts, removes and text edits, growing to several hundred files and shrinking back; the ID equals
        building from the files, and the shape is checked node by node),
        `a_root_set_again_keeps_the_hashes_of_what_is_unchanged`; the view property test
        `a_view_set_over_others_is_the_view_set_alone` compares the IDs with a fresh database's.
    - **A file added or removed, before:** the root's `FileSet` was rebuilt, a clone of every
      path, about 50 ns each, so 5 ms and 100k allocations for a 100k-file root.
    - **Today:** the root's set is taken out of its input (salsa's setter `.to()` hands back the
      old `Arc<SourceRoot>`, an empty spare holding the input meanwhile), edited where it is
      (`Arc::make_mut`, which copies only for a reader that still holds it; readers hold it by
      reference, blocks 6 and 7) and set back. The content trie takes the file in or out in
      \(O(\log n)\).
      - Added: an append to the set and the trie's path, ≈ 2 µs and at most 12 allocations a call
        (the joined path, its key, the set's two copies of the path, the lists returned and the
        batches), a constant: `a_file_added_or_removed_costs_a_constant` holds 6 for a removal
        and 12 for an addition, and 2,048 files no more than 64.
      - Removed: `FileSet::remove` (lane vfs, `crates/vfs`) is `IndexMap::shift_remove`, which
        keeps the other files' order, as the rebuild did: the entries after it move, ≈ 48 B
        each. At 100k files that is ≈ 2.4 MB moved on average plus a pass to fix the indexes,
        ≈ 0.2 ms, 0 allocations: **25× less than before, ≈ 100× the \(O(\log n)\) bound**.
      - What stands in the way: the set's order is the order its files were put in, and the
        rebuild kept it, so the in-place edit keeps it too. `swap_remove` would make a removal
        \(O(1)\) but changes the order `SourceRoot::iter` gives; whether anything may depend on
        that order is a decision, not a speed change.
      - A set that names a path for two files or a file at two paths (no `vfs::Vfs` partition
        makes one: `RootContent::one_to_one`) is still made again from its files, since there
        which file a path names after a removal depends on all of them.
      - Test: `a_view_edits_its_roots_files_in_place_as_making_them_again_does` (60 random views:
        the set equals the old one's files in order, the removed out and the added after, and
        the `SourceRoot` is the same allocation, not a copy).
  - **Per call:** about 15 to 25 allocations for the batches, a constant
    (`a_changed_file_costs_a_constant` holds 64 a call and 6 a changed file).

### A crate-graph set (`CrateGraphBuilder::set_in_db`, \(N\) crates, nothing changed)

- **Work:** compare each crate's fields with what is set; set and rehash only what changed, and
  the crates that depend on it.
- **Bound:** \(N\) field compares (about 1 KB a crate of dependencies, cfg and env: 35 ns at 30
  GB/s) and one `CratesMap` probe (about 20 ns): ≈ 60 ns a crate, **≈ 30 µs for 500 crates**, 0
  allocations. A changed crate adds its own BLAKE3 (about 2 KB, 0.5 µs) and its dependents'.
- **Before:** every crate's data was built again (a `Vec` of dependencies, clones of `origin`,
  the attributes and `proc_macro_cwd`) and its content ID computed before it was compared (a
  clone of the root path, a path key, the cfg encoded into two sorted `Vec`s, the env sorted,
  1 to 2 KB through BLAKE3): ≈ 12 allocations and ≈ 3 µs a crate, ≈ 1.5 ms for 500 crates,
  ≈ 50× off.
- **Today:** compare first (`Walk` in `input.rs`). Dependencies are walked first, so a crate
  knows whether any of them changed; its fields are compared with the input's in place
  (`same_data`, which destructures every field so a new one cannot be missed), and only a
  differing field is set. A crate is hashed again only if a field changed, a dependency's
  content changed after it was last hashed, or its root file now has another path: each crate
  keeps a stamp (when it was hashed, when its content last changed, its root path) in
  `CratesMap`, so a crate dropped from the graph and put back after its dependency changed is
  hashed again, not taken as fresh.
  - An unchanged crate is the field compare (≈ 35 ns), the `CratesMap` probe and the stamp's
    (≈ 40 ns), its root path (two input reads and a set probe, ≈ 50 ns, no clone), the
    attribute files' check (a lock and a read, ≈ 30 ns) and three sets the call returns or
    keeps (≈ 60 ns): ≈ 220 ns a crate, so 500 crates ≈ 110 µs, plus the graph's own ID over
    500 × 32 B (≈ 5 µs) and the dropped-crates pass (≈ 10 µs). **≈ 125 µs: 12× less than
    before, ≈ 4× off**, and 0 allocations a crate: `an_unchanged_crate_graph_allocates_a_constant`
    holds at most 4 a call for 8 and for 256 crates.
  - What stands in the way: five hash probes a crate (`CratesMap`, the stamps, the visited map
    the call returns, the crate list, the root files). Dense crate IDs would make them indexes.
    It runs on load and when a manifest or a build script's output changes, never in a query or
    on a keystroke, so that waits.
  - Tests: `a_crate_graph_set_keeps_every_content_id_what_hashing_gives` (16 seeds × 60 random
    graphs: cfg, env, dependencies, a second crate on one root, root files moved to other paths;
    every crate's
    ID equals hashing its fields), `an_equal_crate_graph_hashes_no_crate`,
    `a_crate_back_after_its_dependency_changed_is_hashed_again`.

### What queries note (`included_file`, `include_source`, `env_is_read`, `env_is_unset`)

Already noted, each is one FxHash and a probe of a lock-free `Table`: 0 locks, 0 allocations
(round 1). That is the bound for a lookup by a key the caller holds.

### Summary

| operation | bound | today | off | removes the rest |
|---|---|---|---|---|
| file-text read | 1 line + the read's record, 5 ns hot | 8 loads (2 per file), 15 ns, 0 allocations | 3× hot, 2× cold | salsa inputs indexed by a dense key (speed-salsa) |
| source-root read | 1 per-file line, 8 ns | 2 per-file lines, 30 ns | 3.5× hot, 2× cold | as above |
| `relevant_crates` hit | 10 ns | 30 ns, no span, no shared write, 0 allocations (block 3 applied) | 3× | the two above and salsa's hit, outside base-db |
| `parse` hit | 1 memo hit | 1 memo hit, 0 allocations | 1× (base-db's part) | salsa's hit path |
| `set_view_root`, unchanged file | 1 probe, 11 ns, 0 allocations | ≈ 10 ns in view order (≈ 27 ns out of it), 0 allocations a file or a call (blocks 1–2 applied) | 1× (2.5× out of order) | keeping the file and its IDs in the index entry, if out-of-order views matter |
| `set_view_root`, changed file (10 KB) | ≈ 2 µs, a constant of allocations | ≈ 2 µs for a small root, ≈ 4 µs for a 100k-file root (the root's trie ≈ 2 µs of it, was 35); 2 allocations | 1× to 2× by root size | one lane a level for one file; \(k \ge 8\) files fill the lanes |
| file added or removed | \(O(\log n)\) | added ≈ 2 µs, ≤ 12 allocations; removed \(O(n)\) memmove, ≈ 0.2 ms at 100k (was 5 ms), 0 allocations a file | 1× added, ≈ 100× removed at 100k | `swap_remove`, if a root's file order may change (a decision) |
| crate-graph set, 500 crates unchanged | 30 µs | ≈ 125 µs, ≤ 4 allocations a call (was 1.5 ms) | 4× (setup) | dense crate IDs for the five probes a crate |

The benchmarks:
- **rustc's `SourceMap`:** an append-only vector behind `FreezeLock` and `RwLock`, which are real
  locks in the parallel front end. base-db's reads take none. They are within salsa's two-level
  lookup of the bound.
- **Unison:** an unchanged definition is compared by hash, never rehashed. `set_view_root`
  already compares an unchanged file and never hashes it.
- **git's index:** a stat compare per file, then a hash only of what changed. The bound above is
  that shape with no stat, since blaze-vfs hands the IDs over. Like git's index, the root's
  content ID needs a tree (git's `cache-tree`) to be \(O(\log n)\) per change. base-db's root
  trie is that tree, keyed by path hash rather than by directory, so its shape does not follow
  how deep the directories are.

## blaze_query::db: the dense engine (query-unify-engine)

Counted from `crates/blaze-query/src/db/` as of 10-30 16:50, written blind (no lane runs cargo
since 15:21); measured by `query_hit`'s `db/<case>` cases, below, once the coordinator runs them.
A "line" is as in the salsa section: a dependent load, 4.5 cycles from L1, 110 ns from DRAM.

### Warm hit (`fetch`, verified this view, value present)

- **Work:** find the row of (function, key), compare its word with the view, return the value
  in place, and record the read.
- **Bound:** as the salsa section's: 2 lines (the row's page, the row), about 20 instructions,
  **≈ 5.4 ns** hot.
- **Today:** the clock (one word, read-mostly, L1), the function's page directory
  (`storage.dir(index)`: an array indexed by the ingredient's static number, L1 across a task's
  hits), the page pointer at `dir[id.page()]`, then the row at `page + size × slot`: its 32-byte
  header and the value beside it share a line for values up to 32 bytes. The test is one compare
  of the 32-bit word with the clock (`view | VALUE`; a stopped storage or a running row fails
  it). 2 lines that can miss, 2 that stay in L1, about 15 instructions: **at the bound**. No
  hash, no lock, no `Arc`, no allocation (`warm_paths_allocate_nothing`).
- **Inside a query:** plus the read's record: the thread-local's frame count, a compare with
  the last edge, and a 4-byte push onto the edge stack, which grows once per thread to its
  high-water mark. About 8 instructions and L1 lines only, ≈ 2 ns: **≈ 7.5 ns**, at the bound
  of the salsa section's "Recording a read".

### Verifying a memo (the first fetch in a new view)

- **Work:** prove the memo's inputs unchanged since it was verified, or find the first edge
  that changed.
- **Bound, by durability:** one compare of the memo's durability kind's `last_changed` with its
  verified view: the header's line, already loaded by the failed hit, **≈ 1 ns**.
- **Bound, by summary:** the memo's 16-byte summary (a column by node: 1 line) ANDed with
  `written[now − verified − 1]` (64 words of 16 bytes, L1): **1 line, ≈ 3 ns** hot, one miss
  cold. This is what `db/verify/deep_unrelated` measures: independent of the cone's 1M edges.
- **Bound, by edges:** per edge a 4-byte load from the edge array (sequential, streamed) and the
  dependency's 8-byte color (`(verified << 32) | changed`, a column by node: random). The colors
  are prefetched four edges ahead, so their misses overlap: **≈ 5.5 ns an edge** cold in
  throughput, about 1.5 ns hot (two loads and a compare).
- **Today:** that order (durability, summary, edges), claim-free: no flight, no lock. A walk
  that proves a row ORs its dependencies' summaries into its own (one `fetch_or` of 16 bytes)
  and compare-and-swaps its word; a dependency not verified in this view is pushed on the
  thread's walk stack, so the walk has no recursion and allocates nothing once warm. **At the
  bound** on each of the three paths.

### Renumbering the views (at view 2^28 − 1, request 01217)

- **Work:** views are 28 bits of a row's word. At the last one, the next write maps every view
  stamp in use (the view, `last_changed`, each node's color, each function row's verified view
  and `changed`) to its rank among them, which keeps every order between two stamps, so every
  answer stays the same. The written-field history becomes all ones, so each row verified
  before the renumbering walks its edges once.
- **Cost:** with \(r\) function rows and \(n\) nodes, two passes collect and rewrite up to
  \(2n + 2r\) stamps, and a sort and dedup costs \(O(s \log s)\) for \(s\) stamps. At 1M nodes and
  rows that is about 4M stamps, ≈ 16 MB and ≈ 50 ms on one core. It happens once per 2^28
  writes, under `&mut`, so ≈ 0.2 ns per write amortized; the hit path doesn't change. Each row
  then walks once: one verify per row in the next view.

### Interning a hit (`intern`, `intern_by`)

- **Work:** hash the value, find its row, compare.
- **Bound:** FxHash of the key (a few cycles a word), one probe group of the shard's index
  (1 line), the row's fields for the compare (1 line): **≈ 2 lines, ≈ 6 ns** hot for a small key.
- **Today:** 64 shards of `blaze_mem::swiss::Index`, chosen by the hash's top bits. A find takes
  no lock (an insert takes the shard's mutex), and a hit records no edge and allocates nothing.
  **At the bound.**

## Measured: salsa fork vs blaze_query::db (query-unify-bench)

`crates/blaze-bench/benches/query_hit.rs` runs the same query shapes on both engines
([query-unify.md](query-unify.md), "How it is measured"); one database type each, functions take
it concretely (`&Db`), as the front end's do. Run with `cargo bench -p blaze-bench --bench
query_hit -- solo -t 8` (blaze-bench's paired runner, which replaced tango with the same
CLI, in solo mode). The cases:

- **hit**: a tracked function of one interned key (`unsafe(no_lifetime), revisions =
  usize::MAX`, the front end's form) returning `u64`, 64K keys, warm; one hit an iteration, the
  keys in order or by stride 40 503 (scattered).
- **hit in query**: an iteration writes the low-durability input of one outer query, which
  runs again and makes the same 64K hits from inside, each recording its edge. Per hit is the
  iteration over 65 536; it includes the write, the outer query's run, and each inner memo's
  first check in the new revision (passed by durability: the keys are interned outside a query).
- **verify**: 16 384 readers of 16 edges each over 4096 leaves, readers and leaves of medium
  durability. An iteration writes an unrelated input and fetches every reader again. *Other
  durability*: the input is low, so each reader passes on its durability. *Same durability*:
  the input is medium, so each reader verifies its edges (or the fork's bloom check,
  `graph::bloom`, passes it first). Per edge is over 262 144.
- **interned hit**: `Key::new` of a value already interned, scattered over 64K.
- **cycle**: `c0 → c1 → … → c7 → c0`, `c0` with `cycle_initial`; an iteration moves the
  input's cap between 3 and 4, so the cycle runs again from 0 to its fixpoint, two iterations
  of eight queries.

The machine is shared and was loaded (load average 75 to 113 on 18 cores), so the means are
inflated by contention and vary run to run; the minimum sample is the one that repeats (two runs
of 3 s and 8 s a case agree within 20%). Both are given; compare minimums.

**salsa fork**, 2026-10-30 14:51, Apple M5 Max (18 cores), commit `a6adfd68aeb1` plus the
working tree (the fork as in `vendor/salsa` at that time):

| case | min | mean (8 s run) | per op (min) |
|---|---|---|---|
| hit, sequential | 12.5 ns | 42.7 ns | 12.5 ns a hit |
| hit, scattered | 14.6 ns | 55.9 ns | 14.6 ns a hit |
| hit in query, sequential | 1.5 ms | 4.1 ms | 22.9 ns a hit |
| hit in query, scattered | 1.8 ms | 5.5 ms | 27.5 ns a hit |
| verify, other durability | 479 µs | 798 µs | 29.2 ns a reader |
| verify, same durability | 1.4 ms | 1.8 ms | 5.3 ns an edge (87 ns a reader) |
| interned hit | 9.5 ns | 17.4 ns | 9.5 ns |
| cycle of 8 to fixpoint | 6.2 µs | 7.3 µs | ≈ 390 ns a query run (16 runs) |

Against the bounds above ("Memo hit", ≈ 5.4 ns hot), the fork's warm hit is 2.3× off in order
and 2.7× scattered; a hit from inside a query costs about 10 ns more for its edge and the
revision's first check.

A ninth case, **deep unrelated** (Andrew, 15:56), added after this run: the memo on top of
64 layers of 1024 queries of 16 edges (1M edges) over 4 leaf inputs, all of medium durability;
an iteration writes a medium input outside its closure and asks the memo again. The engine's
transitive-input summary ([query-unify.md](query-unify.md), "Transitive-input summaries") makes
it green with no walk; the fork has its 62-bit closure filter (`graph::bloom`) or walks the 1M
edges. Its closure holds few inputs so that 128 bits stay sparse: with 1024 leaves every bit
would be set and every check a walk.

**Both engines**, one binary (`db/<case>` beside `salsa/<case>`), the coordinator's round 3b,
2026-10-30 18:30, commit `d79d33bf1c`, the same Mac, loaded (load average about 100):

| case | salsa min | salsa mean | db min | db mean | db / salsa (mean) |
|---|---|---|---|---|---|
| hit, sequential | 10.7 ns | 12.4 ns | 2.9 ns | 3.5 ns | 0.28 |
| hit, scattered | 13.6 ns | 16.8 ns | 3.4 ns | 5.5 ns | 0.33 |
| hit in query, sequential (64K hits) | 1.2 ms | 1.5 ms | 673 µs | 833 µs | 0.56 |
| hit in query, scattered (64K hits) | 1.6 ms | 1.9 ms | 778 µs | 1.7 ms | 0.89 |
| verify, other durability | 371 µs | 453 µs | 139 µs | 160 µs | 0.35 |
| verify, same durability | 907 µs | 1.1 ms | 466 µs | 602 µs | 0.55 |
| deep unrelated | 500 ns | 769 ns | not measured | | |
| interned hit | not measured | | not measured | | |
| cycle of 8 to fixpoint | not measured | | not measured | | |

Per op, the engine's hit is 3.5 ns in order and 5.5 ns scattered (the bound above is ≈ 5.4 ns
hot); a hit from inside a query is 12.7 ns in order and 26 ns scattered (its minimum, 11.9 ns,
says the scattered mean is mostly the load); verify is 9.8 ns a reader by durability and
2.3 ns an edge by walk.

The run stopped at `db/verify/deep_unrelated`: the engine spent all 2^28 views (one a write)
within tango's 8 s, about 30 ns an iteration, and `Storage::next_view` panicked, so the last
three rows are missing on both sides. The bench then rebuilt the case's database every 2^24
writes until the engine renumbered its views at the last one (`Storage::next_view`, 01215);
with that in, the bench asks the engine as it is.

**All nine cases**, the port's run (`qu-bench1`, worktree blaze-v2-qu at `b7c951d585`,
2026-10-31, the same Mac, loaded): the engine is faster than the fork on every case, by
minimum and by mean.

| case | salsa min | salsa mean | db min | db mean | db / salsa (min) |
|---|---|---|---|---|---|
| hit, sequential | 13.3 ns | 29.4 ns | 3.8 ns | 8.6 ns | 0.29 |
| hit, scattered | 18.5 ns | 39.9 ns | 4.4 ns | 12.6 ns | 0.24 |
| hit in query, sequential (64K hits) | 1.5 ms | 4.0 ms | 830 µs | 2.0 ms | 0.55 |
| hit in query, scattered (64K hits) | 2.1 ms | 5.4 ms | 967 µs | 4.8 ms | 0.46 |
| verify, other durability | 511 µs | 1.2 ms | 176 µs | 374 µs | 0.34 |
| verify, same durability | 1.2 ms | 2.0 ms | 628 µs | 808 µs | 0.52 |
| deep unrelated (1M edges) | 500 ns | 1.3 µs | 28.9 ns | 43.7 ns | 0.06 |
| interned hit | 10.3 ns | 63.6 ns | 6.1 ns | 16.2 ns | 0.59 |
| cycle of 8 to fixpoint | 7.4 µs | 18.1 µs | 2.0 µs | 7.0 µs | 0.27 |

The deep case is the transitive-input summary at work: the engine answers a memo over 1M
edges in 29 ns, one summary test against the view's mask, where the fork's 62-bit closure
filter takes 500 ns. The fork's half of the bench and its `salsa` dev-dependency are gone
(2026-10-31, before `vendor/salsa` is deleted); these tables are its record.

**The engine alone**, the main tree's round 7c (`adc8bbbf04`, 2026-10-31 07:00, the same Mac,
load average 67), minimum / mean: hit 3.4 / 6.2 ns in order and 3.9 / 13.1 ns scattered; hit in
query 768 µs / 1.4 ms and 1.0 / 3.6 ms; verify 165 / 271 µs by durability and 580 / 866 µs by
walk; deep unrelated 28.6 / 37.0 ns; interned hit 6.5 / 11.5 ns; cycle 2.5 / 4.8 µs. Every
minimum is within noise of the port's run above and below every minimum the fork recorded.
