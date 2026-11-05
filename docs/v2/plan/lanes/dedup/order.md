---
status: plan
summary: dedup-order (2026-10-29): every hand-written counting sort (about 20, in 13 crates) onto one stable counting sort into compressed sparse rows, blaze_radix::Csr, beside the one radix sort; the test queue's timing wheel onto blaze_core::wheel::Wheel; with each one's speed of light.
date: 2026-10-29
---
# dedup-order: sorts and timers

Part of [dedup](../dedup.md). Two primitives: grouping and sorting by integer keys
(`blaze-radix`), and timers (`blaze_core::wheel`).

## Counting sorts: `blaze_radix::Csr`

`crates/blaze-radix` already held the one LSD radix sort. The copies left were all one other
sort: group `(row, item)` pairs by a row below a known bound (a node, a binding, a shard, a
bucket) into compressed sparse rows. Each copy counted into `starts[row + 1]`, summed, placed
the items with the starts (or a clone of them) as cursors, and shifted the starts back. There
were about 20 of them: import and SCC graphs, binding patterns, CSR adjacency in the linker,
the borrow checker's relations, dead-code owners, the plane's Kahn order, cargo's dependents,
`.gnu.hash` buckets, symbol shards, BLAKE3 batch groups, store slot buckets, a model checker's
reverse edges.

The shared one is `blaze_radix::csr` (`crates/blaze-radix/src/csr.rs`), re-exported as
`blaze_radix::Csr` and so `blaze_ids::radix::Csr`:

- `Csr::build(rows, pairs)` and `csr.rebuild(rows, pairs, &mut cursors)` walk a cloneable
  iterator of `(u32, T)` twice, once to count and once to place. The pairs are never stored,
  so a caller can group straight from nested loops, filters or `enumerate`. Items are written
  into uninitialised room, so they are not zeroed first. A `rebuild` with room allocates
  nothing; the test `a_warm_rebuild_allocates_nothing` checks this.
- `Csr::graph(nodes, edges)` gives each source's targets ascending, through two stable passes
  (by target, then by source). That is the `(source, target)` sort that rustc's `VecGraph`, the
  leak check and the import graph need, with no comparisons. `dedup_rows()` makes each sorted
  row a set.
- Reads: `row` (panics past the rows), `get` (`None` there), `range`, `starts`, `items`,
  `into_parts` / `from_parts` for callers that keep bare or boxed columns.

Soundness: placing into uninitialised room is safe only if every place is written exactly
once. A second walk that yields rows other than the first walk's would break that, so each
write checks the row's cursor against its end. Cursors only move up inside their own row, so
the writes are distinct; with `placed == total` checked as well, the `set_len` is sound. A
mismatch panics (`a_second_walk_with_other_rows_panics`) and never leaves an item unplaced.
The checks are two predictable branches per pair. Rows at or past `rows`, and more than
`u32::MAX` pairs, are bugs and panic.

Kept apart, on purpose:

- `blaze-link/src/symbols.rs` partitions into 64 shards with the radix sort itself (one 8-bit
  pass, in place, parallel past `radix::PARALLEL`), since it needs no row starts beyond 64
  `partition_point`s.
- `blaze-store`'s `sorted` keeps its threads for the per-bucket radix sorts; only its bucket
  pass moves.
- `blaze-link/src/write.rs` `Column::sized` is a layout prefix sum, not a sort.

### Speed of light

Per pair: the first walk reads the pair and increments one 4-byte counter; the second reads it
again and writes one item. The bound is the memory traffic, 2 × the pair size + the item size
(+ the item's line read for ownership): about 20-24 bytes for `(u32, u32)`. At a core's ~20
GB/s that is ~1 ns a pair while the counters fit in L1/L2 (rows ≤ ~64k). The instruction
count is about 4 per pair per walk, under 1 ns at 3-4 IPC, so memory bounds it. The prefix sum
and the cursor copy cost 8 bytes per row.

The copies were at the same bound or worse: each zeroed its output first (a third write
stream), and some cloned the starts or kept a pair per edge between passes (`leak_check`,
the import graph). The shared sort removes the zeroing and keeps the import graph's first pass
as a row list (4 bytes an edge, not 8). It is within 10% of the bound for every caller: rows
are at most tens of thousands of modules, bindings, sections or SCC nodes, so the counters stay
in L2.

The limit is a scatter over more rows than L2 holds (over ~256k rows with over ~1M pairs). There
each placed item misses its own line, and memory-level parallelism (~10-20 misses in flight)
bounds it at ~3-5 ns a pair. An MSD pre-partition by the row's high bits (mold's and ips2ra's
approach) would win there. No caller is in that range today; the first one that is should
gain that path here, not keep a copy.

## Timing wheels: `blaze_core::wheel::Wheel`

`blaze_core::wheel::Wheel` is the one. `blaze-usercode`'s test queue had a second copy
(`test/queue/wheel.rs`) with the same layout (11 levels of 64 slots, one bitmap per level):

| | `blaze_core::wheel` | queue copy |
|---|---|---|
| storage | `blaze_mem::Slab` tagged `Io`, generational `Handle` | `Vec` columns, `u64` handles with an `UNSET` sentinel |
| order within a deadline | insertion order | none |
| next wake | the soonest deadline itself | the next expiry (a cascade or a firing) |
| due by now | fires at the next advance | pushed to now + 1 |

The core wheel does everything the queue needs. Its next wake being the soonest deadline
means the queue's driver timer wakes once per deadline, not once per cascade. The only
addition is `Wheel::slots()` (the slab's size), for the queue's test that the wheel stops
growing in steady state. The copy itself is kept in `crates/blaze-core/src/wheel/
differential_tests.rs` as the spec of a proptest differential (both fire the same timers at
each advance; the core one in deadline then insertion order; its soonest deadline is at or past
the copy's next expiry). Its three tests moved there too. `blaze-usercode` already depends on
`blaze-core`, which depends on neither `blaze-usercode` nor anything that does, so there is no
cycle.

Not timer queues, and so not moved: `BinaryHeap<Reverse<_>>` in nameres, the plane's Kahn
order and `blaze-protocol`'s ready set are priority queues over indices. Three timer scans stay
on purpose, each with a comment requested: the splicer's pending binaries (a handful), the
agent API's intent deadlines (one per waiting client), and the MIR interpreter's blocked
threads, whose tie order (deadline, how long blocked, index) is the schedule's.

### Speed of light

Insert: one `lzcnt`, a shift, a slab entry and a slot head, so two cache lines, ~5 ns warm.
Cancel: an unlink through the handle (the entry and its two neighbours). Advance: one
`trailing_zeros` per occupied slot visited, and each timer moves down at most ten times. A
steady stream of timers allocates nothing (`a_warm_arm_and_fire_allocates_nothing`,
`a_wheel_in_its_steady_state_allocates_nothing`). This is the hierarchical wheel's O(1) bound
(Varghese and Lauck; tokio's and the kernel's layout). The queue holds at most two timers per
core and one per binary, so the wheel costs nothing next to the driver's syscall per wake.
