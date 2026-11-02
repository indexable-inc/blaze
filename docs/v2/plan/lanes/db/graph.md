---
status: plan
summary: lane graph's three changes to vendor/salsa's query graph (input-closure Bloom filters, walk-order node numbering, the packed graph read in place from the daemon image), why each is exact, their speed-of-light bounds and arithmetic, and what is written versus still waiting on hooks in sol-salsa's files.
date: 2026-10-30
---
# Lane graph: filters, walk order and the graph at rest

Lane graph works beside sol-salsa (vendor/salsa's graph, colors, memo header). Everything below
was written blind on 10-30: nothing has been compiled or run yet. The code is in
`vendor/salsa/src/graph/{bloom,order,varint,frozen}.rs`, `tests/graph_filters.rs` and
`benches/graph_{bloom,walk,varint,frozen}.rs`. The hooks it needs in sol-salsa's files are
listed at the end; they are in.

## 1. Input-closure filters (`graph/bloom.rs`)

**What it does.** Each node keeps a 64-bit column word. For a function memo, the word is a
filter: the OR of what each edge contributes, made when the memo is published and again after
every deep verify that finds it unchanged. Edges contribute as follows:

- an input field: its own bit (a hash of the key, 1 of 62);
- a function memo: that memo's filter;
- a tracked field: its creator query's filter, but only if the creator's color says it was
  verified in the current revision; otherwise every bit;
- anything else: every bit. That covers interned values that can be reused (salsa records
  edges to those only), accumulators, untracked reads, provisional or cycle memos, and assigned
  memos.

The runtime keeps a change log. For each durability level and each of the last 64 revisions it
stores the filter of the input fields set since then. A write is logged at the field's durability
before the write, at every level up to it.

When the durability check in `shallow_verify_memo_cold` fails, the memo is tested against the
log. If its filter is not all ones and shares no bit with what changed at its durability since
it was verified, the memo is green without a walk. It takes the same path as the durability
check (`ShallowUpdate::HigherDurability`), so its outputs are marked verified as a walk would
mark them.

**Why it is exact.** The argument is induction over revisions. Claim: whenever a memo is
verified in revision r, its filter covers the bit of every input field its edges reach
transitively in r. There are four ways a memo gets verified:

- It executed: its filter is built from its own edges, each verified in r.
- A deep verify succeeded: the filter is rebuilt the same way.
- It passed the durability check: nothing at its durability changed, so its reads are the same.
- It passed the filter check: no input it reached changed, so by induction neither did any
  dependency's edges.

A memo of durability d reads only inputs of durability at least d, and every such write is in
level d of the log. So when the filter shares no bit with the log, no input the memo reached
changed since it was verified. Then a deep verify would find every edge unchanged and run
nothing.

A tracked field changes only when its creator runs again, and its creator runs again only if
the creator's own filter meets the log, so the creator's filter stands in for the field's bit.

A reused interned slot changes what an id means without any input write. The walk also stops
that reuse in the current revision by raising `last_interned_at`, which is a side effect.
Neither is visible to the filter, so those edges give every bit, and a filter with every bit is
never green.

A log that is disabled, or that does not reach back to the verified revision, gives every bit.
`tests/graph_filters.rs` checks all of this differentially (see Tests below).

**64 or 128 bits.** `graph_bench::bloom::green_rates::<W>` builds a random module-shaped
graph, edits one input, and counts the memos that are truly unaffected against those the filter
proves unaffected; it panics if the filter ever proves a reader of the edit green. Each query
reaches tens to hundreds of inputs, so one 62-bit word is more than half full high in the
graph, and the bits mostly prove leaves and mid-level queries. A second word doubles the
column (80 MB more at 10M nodes) for a gain the bench prints; the default is 64 until it says
otherwise.

**The bound.** The walk the filter replaces costs one random color load per edge of the
memo's reachable subgraph. The filter check costs two loads (the column word and the log's
word) and an AND, inside the memo lookup the shallow check already does: about 20 ns a memo
against `edges × 1 to 5.5 ns`.

`benches/graph_bloom.rs` has 1000 chains of 100 memos, one edit in chain 0's head, then the
top query again. Without filters every chain is walked: 10^5 memos at about 50 ns, so about
5 ms. With filters the cost is 1000 filter checks plus one re-execution: 1000 × 20 ns + 1 µs ≈
**21 µs**. The target is within 2x of that (≤ 45 µs) and at least 100x under the run without
filters.

**Where it stops paying in blaze's front end (measured from the source, not run).** About 414
of the 880 `#[salsa::tracked]` functions in `crates/` take two or more arguments besides the
database, so each call interns its arguments. Those interned values are garbage-collected
(`DEFAULT_REVISIONS`) and have LOW durability, so every caller records an edge to a reusable
interned value. That edge gives every bit, and every bit is absorbing up the graph: most memos
above such a call get a filter that is all ones and are walked as today. The filter is never
wrong, only absent there.

Making those edges filter-friendly has one sound route: mark functions with interned arguments
`revisions = usize::MAX`, as most of hir-ty's interned types already are. Immortal interned
values record no edge, so they don't spoil the filter.

A reuse log plus a per-node "oldest interned read" column (queue 00823) was refused by
sol-salsa on 10-30, for this reason. `interned.rs`'s `data()` asserts, in release builds too,
that a reusable value was validated in the current revision, and only the walk validates it. A
filter-green memo would hand out ids whose next field read panics. `tests/interned-bad-eq.rs`
and `tests/cycle_displaced_memo.rs` pin that panic. So interned ingredients stay `Opaque`.

Which functions could become immortal, and what that costs in memory, is a front-end
question, still open.

## 2. Walk-order numbering (`graph/order.rs`)

Nodes are numbered in the order salsa first sees their keys, which interleaves modules across
threads. Renumbering in depth-first preorder from the roots puts each memo's first dependencies
right after it, so a walk reads the color column mostly in sequence. Reverse Cuthill-McKee is
implemented too, for comparison. `verify_walk` is the walk's model: an explicit stack of edge
ranges, a seen bitset, and the color prefetch from `colors.rs`.

**The bound.** A walk of E edges loads E colors and streams E edge words. In walk order that is
(E × 4 B + N/8 color lines × 64 B) / 30 GB/s ≈ 0.16 ns an edge. The instruction count is about 10
an edge (target, color, seen bit, add, branch, prefetch), or 0.7 ns at 4 IPC and 3.5 GHz. So the
walk is compute-bound at ≈ 0.7 to 1 ns an edge. In random order it is ≈ 5.5 ns an edge (a DRAM
miss at about 20 in flight).

`benches/graph_walk.rs` targets DFS preorder with prefetch at ≤ 1.1 ns an edge on
`synthetic(7, 10M, 10)`. Numbering plus renumbering should take under 2 s at 10M nodes and 100M
edges.

The synthetic graph is kinder to "as made" than nushell: 70% of its targets lie within 64
creation slots. The real gain is expected to be larger than the bench shows.

Preorder does not make the walk's color loads sequential. About 90% of a walk's edges go to
nodes it has already numbered, spread tens to hundreds of slots away, so the count of
consecutive loads that change cache line (`line_changes`) is predicted to drop only to about
0.85x, not to half. The test asserts only that it drops. What preorder gives is working-set
locality: a module's colors stay within a few lines of L1 and L2. Measuring misses under a
small LRU cache would show that better than `line_changes` does.

**Live renumbering is in (sol-salsa-order, 10-30, written blind).** A compaction
(`Zalsa::compact_edges_if_worth_it`) first gathers every live function memo's node and edge
words through `Ingredient::memo_edges` into `graph::LiveEdges`. It builds a CSR of their input
edges and numbers it with `dfs_preorder`. Nodes the walk never loads (no memo, and no input
edge names them) follow, in their old order. `Graph::compact_renumbered` then applies the
permutation in the same `&mut` pass. The key columns and the colors move to fresh pages, the
key table's slots are rewritten in place, and every edge word is renamed as the `Relocation`
copies it, at the same packed width. Each `MemoHeader.node` is renamed through
`Relocation::node`, and the filter column moves with its creators renamed
(`Closures::renumber`). Tasks' node caches live in every thread's task state, out of `&mut`'s
reach, so `Nodes` keeps an epoch that the renumbering bumps. A cache of another epoch is
emptied on its next use, so no old number can come out of one. Persistence stores keys, never
nodes. The same `LiveEdges` is the frozen writer's `Source` (`Zalsa::freeze_graph`, which writes
in walk order). `Zalsa::set_walk_order(false)` turns renumbering off for comparisons. Tests:
`graph.rs` (`renumbered_compaction_keeps_keys_colors_and_edges`), `nodes.rs` and
`tests/walk_order.rs`. Bench: `benches/verify_order.rs`.

## 3. The graph at rest (`graph/varint.rs`, `graph/frozen.rs`)

**Format.** The image section `0x0501` holds one frozen graph, read straight from the mapped
image with no decode step:

| part | size |
|---|---|
| header | 24 bytes |
| key ids | 8 bytes a node |
| filter words | 8 bytes a node, creators renumbered |
| blocks | 8 bytes per 64 nodes: stream offset (40 bits), wide index (23 bits), wide flag (1 bit) |
| key ingredients | 4 bytes a node |
| wide offsets | 4 bytes per node of a wide block |
| narrow offsets | 2 bytes a node |
| edge stream | variable, see below |

The stream holds, per node, a LEB128 count and then StreamVByte values. Each value is
`zigzag31(node − previous node) << 1 | output`, where the first edge's previous node is the
memo's own node.

`FrozenGraph::open` checks the magic, the counts, the exact length and every block's bounds.
`edges` decodes one list and checks every target is below the node count. A mismatch is a
`FrozenError` naming the node or the field. The image checks the CRC of every group of the
section on open (`Image::whole`). Nothing is rebuilt in place of a bad section (no-fallback).

**Deliberate deviation from the brief: edges are not sorted.** Deep verify must check a memo's
edges in the order it read them, because a later read may only be valid once an earlier one is
unchanged. Sorting would change behavior. Deltas from the previous edge, with the first from the
memo itself and nodes in DFS preorder, are small for the same reason sorted deltas would be: a
query's first reads are numbered right after it.

**Bytes an edge.** The varint child's computation for the synthetic distribution:

- data: 0.9 × 1 + 0.1 × 3 = 1.20 bytes;
- control: about 0.29 bytes (a quarter byte a value, plus the unused part of each list's last
  control byte);
- count: 0.10 bytes (one a node at 10 edges a node).

That is **≈ 1.59 bytes an edge**, against a 1 to 1.5 target. The count is the part above
target. It can go if the offsets give the list's end, so its length is the next node's start
minus its own. That needs the value count, not the byte count, so for now it stays.

The whole section adds 24 bytes a node for keys and filters plus about 2.1 for offsets. At 10
edges a node that is ≈ 4.2 bytes an edge in total, against 4 for today's live words alone
(sol-salsa-pack's bit-packed compaction gives 25 bits for the live array).

**The bound for reading.** StreamVByte decodes 4 values with one 16-byte load, one shuffle
(`swizzle_dyn`: `pshufb` with SSSE3, `tbl` on aarch64) and one store, at about 7 G values/s per
core. The prefix sum is about 3 SIMD ops a value. The bound is ≈ 2.5 G edges/s, plus about 5 ns
a node to find its list. At 10 edges a node that is ≈ 9 ns a node, or 90 ms for 10M nodes.
`benches/graph_frozen.rs` targets 10% of that; `benches/graph_varint.rs` targets the codec
alone. On x86-64 the shuffle needs SSSE3 at compile time (x86-64-v2), otherwise it falls back to
a per-lane loop: correct, but far slower.

**The live overlay.** `Overlay` wraps a frozen graph with one bit a node, set when that node's
memo is republished live. A replaced node reads its edges from the live array; every other node
reads them from the map. A daemon restart is then a map plus a bitset. Wiring it into restart
needs two things outside this lane:

- function memos restored onto frozen nodes (frontend-image's memo restore);
- a frozen kind of query origin in `zalsa_local.rs` (sol-salsa).

Both are requested.

## Tests

- `bloom.rs`:
  - one bit a key;
  - classes encode and decode;
  - the log agrees with a list of writes at every window boundary;
  - the log is unknown before its start;
  - creator and filter column words;
  - the 64- and 128-bit models never prove a reader green.
- `tests/graph_filters.rs`: two databases take the same random edits, one with filters and one
  without (`Zalsa::set_closure_filters`), across 6 seeds × 120 rounds and more than 64
  revisions. The graph uses input fields at every durability, dependencies that change with the
  values read, a tracked struct read through a query keyed on it, and interned values. After
  every round:
  - every query asked returns the same value in both;
  - the same functions ran in the same order;
  - every 40 rounds, both agree with a database built fresh from the final inputs.

  The test also asserts the filters validated fewer memos overall, so the test is not vacuous.
- `order.rs` (child graph-order): preorder against a recursive reference; permutations;
  renumbering round trips and keeps the walk; no allocation once warm.
- `varint.rs` (child graph-varint): exact format at every width; SIMD equals scalar; every
  truncation is an error; random garbage never panics; zigzag wraps; no allocation in hot paths.
- `frozen.rs`:
  - round trips as numbered and renumbered (the preorder stream is no larger);
  - wide blocks and the empty graph;
  - every corrupted header, length and stream byte is an error, never a panic;
  - the overlay.

## Hooks in sol-salsa's files (landed 10-30, sol-salsa's "go")

Made by lane graph as targeted edits:

- `graph.rs`: module declarations, the closures field and the bench facade;
- `lib.rs`: `portable_simd`, `graph_bench`, and the counting allocator for lib tests;
- `colors.rs`: `Color::verified_in`;
- `Ingredient::closure_class` and its three impls;
- `runtime.rs`: the change log, advanced in `new_revision`, and fresh (knowing nothing up to
  the current revision) after `deserialize_from`;
- `input.rs` `set_field`: log the write;
- `Database::synthetic_write`: every bit at and below its durability, since it names no field
  (sol-salsa's condition: a synthetic write must make memos walk);
- `memo.rs`: `closure` and `publish_closure`;
- `insert_memo` and the deserialize path: publish the closure;
- `maybe_changed_after.rs`: the filter branch, and publishing after a successful deep verify;
- `Zalsa::set_closure_filters`;
- `Cargo.toml` entries.

With them the new files are compiled in; nothing has been built yet.

**One difference that is not a result.** A memo the filter keeps green is not walked, so its
dependencies are not marked verified in this revision. Results and the queries that run are
the same (the differential test checks both), but `DidValidateMemoizedValue` events are fewer.
An `lru` function ranks memos by when they were last verified, so it may choose different values
to evict, and a later revision may then recompute a different set of evicted values.
