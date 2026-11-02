---
status: plan
summary: blaze v2's trait solver cache and fulfillment queue as db-ty-solver wrote them, the benchmark each is held to (rustc's new solver), where blaze already beats it and what beating it everywhere takes.
date: 2026-10-29
---
# blaze v2: the trait solver's cache and fulfillment (db-ty-solver)

Design notes for [the db lane](../db.md), as written on 2026-10-29. The code is
`crates/hir-ty/src/next_solver/interner/tls_cache.rs` and `crates/hir-ty/src/next_solver/fulfill.rs`.
The search graph and its `GlobalCache` are in `vendor/rustc_type_ir/src/search_graph/`. Requests
are in `v2/requests/db-ty-solver.md`.

## The benchmark: rustc's next trait solver

- **Cache.** `tcx.new_solver_evaluation_cache` is one `GlobalCache` for the session, behind one
  `Lock`. It is a `HashMap` from the canonical input to an entry holding the result and the
  nested goals. Each goal is solved inside an anonymous dep-graph task, and a hit replays that
  task's dep node into the query that reads it, so every query reuses every goal.
- **Fulfillment.** `FulfillmentCtxt` keeps `Vec<(Obligation, Option<GoalStalledOn>)>`. Each round
  `drain_pending` takes the vector and partitions it into two new ones. A goal whose stalled
  inference variables did not change is skipped (`stalled_on`).

## Where blaze stands

| part | blaze | rustc | |
|---|---|---|---|
| sharing across threads | lock-free tables: a lookup is plain and acquire loads, and a writer locks one shard or stripe only to insert | one `Lock` around the map | beats |
| sharing across queries | `shared`: goals whose reads were captured (`salsa::capture_reads`), one cache per database, crate and region mode, a hit replaying the reads into the reader; `of_query` for the rest | every query, through dep-node replay | equal |
| goal and result storage | each goal a `u32` id, each distinct result interned once, a 24-byte hot row, nested goals off to the side | a fat `HashMap` entry per goal, results stored per goal | beats |
| hashing | FxHash once per insert, 32-bit hash kept for probing and growing | FxHash on every lookup and on every rehash | beats |
| fulfillment rounds | obligations as columns, double-buffered: a round swaps two sets, so no round allocates once they have grown | two new `Vec`s per round | beats |
| order of processing | rustc's, exactly | | equal |
| stalled goals | rustc's `stalled_on` | | equal |

## Costs

- A cache lookup (the bound and its arithmetic are in
  [speed-of-light-solver-cache.md](speed-of-light-solver-cache.md)):
  - no call into the database: an `of_query` cache is found by its `Solved` number, and the
    interner's borrow of the database keeps the revision from moving;
  - an index from the key and two dependent loads (the slot's pointer, then the header that
    confirms the key), with no lock;
  - the search graph's probe: one FxHash and about 1.5 slot reads at half load.
  - A hit allocates nothing and writes no line another task reads.
- A slot is at most 256 bytes (`const` assert). There are 512 slots.
- A pending obligation is one row of five columns (cause, param env, predicate, depth,
  stalled-on). `Pending` itself is six `Vec`s (`const` assert).

## What beating rustc everywhere takes

Sharing across queries (request 4):

- `vendor/salsa` records a solved goal's reads as a `u32` handle to a per-revision CSR edge list
  (`capture_reads`) and replays it into the query that hits the cache (`replay_reads`).
- `DbInterner`'s `with_cached_task` and `Tracked` carry that handle.
- The search graph's `GlobalCache` becomes append-only and published, read through `&self`.
- `tls_cache.rs` keys the shared cache by crate, `regions` flag and database state.

Then blaze solves each goal once per revision across every thread with lock-free reads, where
rustc solves it once per session behind a lock. Until then, the gap is that each query solves
again the goals another query solved.

## Not done, and why

- **SIMD key probes.** The first probe is the hit nearly always, so comparing 16 keys at once
  saves nothing measurable.
- **Allocation inside the search graph.** `GoalStalledOn`'s `sub_roots` and `Interned`'s growth
  use the global allocator. `stalled_vars` reuses the canonical var values, `sub_roots`
  allocates once per stalled ambiguous goal, and growth is amortized; neither happens on a cache
  hit. Moving them to the task's bump arena waits on per-task arenas existing in the plane
  (`docs/v2/plan/lanes/core.md`).
