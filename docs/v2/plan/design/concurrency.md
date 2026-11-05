---
status: plan
summary: blaze v2: how threads share data: reads never lock or write shared memory, values are immutable once published, task boundaries reclaim memory, no build lock.
date: 2026-10-29
---
# Concurrency: no lock on the hot path

v2 runs on every core of one big machine, so a cache line written by many cores costs more than
the work around it. Each rule says what is shared and who writes it.

## Reads

- **A read writes no shared memory.** That covers memo lookups, store reads and interner
  lookups. It also rules out `RwLock`: its read side is an atomic write to a shared counter, so
  readers on 100 cores contend on one cache line. A lookup is plain loads with acquire
  ordering.
- **Tables are append-only.** Keys are hashes, in open-addressed slots published with one
  release store. A full table grows by adding a level (as `blaze-ids` does) and is never
  rehashed in place, so readers never wait for a resize.
- **Values are immutable once published.** A result is written once (a `OnceLock` or a
  published pointer) and never changed in place. Eviction swaps in a new entry; it doesn't
  mutate the old one.
- **Bookkeeping on a read stays on the worker.** Read flags, LRU ages, hit counters and
  statistics go to the worker's own buffer and are merged when its task ends or at GC. A
  per-row flag that must be shared is one relaxed `fetch_or` on that row's word, never a lock.

## Writes

- **Writers shard by hash.** The shard count is a power of two of at least 4 times the core
  count, chosen at startup, not a fixed 64. A shard lock is held only to claim a slot, never
  while computing.
- **Dependencies are recorded per task.** A query's edges go into its task's own buffer and are
  published with its result: no shared write per edge.
- **Store writes go to per-worker segments** of the pack. Commit flips the index once; there is
  no global writer lock.

## Memory

- **Task boundaries reclaim memory.** Every query runs as a plane task, and a worker between
  tasks holds no references into shared tables. So freeing evicted values and old table levels
  waits for every worker to pass a task boundary (quiescent-state reclamation), which costs
  readers nothing.
- **Allocation is per task.** A task allocates from its own bump arena, freed when it ends, on
  the worker's NUMA node in huge pages ([hardware.md](hardware.md)). A hit allocates nothing.
- **No false sharing.** Per-worker state is padded to 128 bytes, the size of the pair of lines
  arm64 and x86 prefetch together.

## Waiting

- **No thread waits on another's work.** Single flight parks the waiting *task* in the plane,
  and the worker runs something else ([core.md](../lanes/core.md)).
- **No guard lives across a query call.** A lock held while calling into the database can
  deadlock or invert priorities. `clippy.toml`'s `disallowed-types` bans `std::sync::Mutex` and
  `RwLock` in `blaze-query`, `blaze-cas`, `blaze-ids`, `intern` and `blaze-graph`. Where a lock
  is truly needed, it is a named type whose comment says why it can't be held across a call.

## Processes

- **No build lock.** cargo makes a second build wait ("Blocking waiting for file lock"). v2
  takes any number of clients, views and commands at once against one daemon. Each output is
  committed by an atomic rename or an index flip. The only file lock is "one daemon per store",
  taken once at start.

## Proof

- `proof`'s budgets include scaling: warm no-op and hot-memo read throughput on 1, 16, 64 and
  every core must scale near-linearly. A run under `perf c2c` must show no shared cache line
  written on the read path.
