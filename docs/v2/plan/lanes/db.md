---
status: plan
summary: blaze v2: our own query database.
date: 2026-10-29
---
# blaze v2: db

Part of the [v2 plan](../../plan.md). Section numbers refer to the plan as first written.

## 7. The query database

- **Our own query database.** It starts from `vendor/salsa`, already in the workspace, and is
  remade into blaze's: red-green queries keyed by input hashes (section 2), not by revision;
  fingerprints on every edge; results in the zero-copy format; many views at once;
  admission-aware execution. It is no longer a vendored copy kept close to upstream.
  (2026-10-31: done as `blaze_query::db`, a new engine rather than a remade fork; `vendor/salsa*`
  is deleted. See [query-unify](db/query-unify.md).)
- **Why did this rerun**: the agent API answers, for any query that ran, the chain of facts
  back to the input that changed. Over-invalidation is debugged with it, and the oracle reports
  through it.
- **The LRU caps go away as a correctness problem**: today `parse` (128), `macro_expand` (512),
  `ast_id_map` (1024) and others evict values, and salsa can only backdate by comparing values,
  so every evicted result counts as changed and its dependents rerun (`INCREMENTAL.md`: every
  macro of a file expanded again on each edit). v2 keeps a fingerprint with every dependency
  edge, as rustc does; values can be evicted to disk and cutoff still works.
- **Signature and body queries split**, so dependents start when signatures are known.
- **Parallel macro expansion**: each round of the resolve-and-expand fixpoint expands every
  resolvable invocation in parallel; macros inside bodies expand with their function's task.
- **Garbage collection by reachability** from live views and kept roots; cold results move to
  disk, nothing is deleted while reachable.
- **The daemon image**: the whole database written as one zero-copy image (section 5); a start
  is an mmap and pages on demand.

## Issues this lane takes in

- **#350**: backdating across a durability drop was unsound in blaze-next and broke 49 of 71
  splices. Our own query database (section 7) verifies inputs soundly: no backdating unless the
  fingerprint and every input are equal. (`v2-db`)
- **#806, #734**: memory goes to compiled code kept after it's in the store (23%), the MIR of
  every compiled body (15%), and parses of every dependency body kept for `declared_in` (10%).
  v2 moves results to disk once stored, and parses dependency bodies only on demand (below).
  (`v2-db`, `v2-format`)
- **#1293 demand-driven front end** and **#1294 outline parse**: dependency crates are parsed
  as item outlines (4-byte packed tokens), and their bodies are parsed, lowered and inferred
  only when codegen reaches them. `blaze check` still checks the workspace's crates fully.
  (`v2-format`, `v2-db`)
- **#1295 front-end images** and **#1299 settle rebuild**: after a cold build, the daemon writes
  its image and swaps to the mapped warm state, dropping the cold arenas (anim's worker sat at
  6 GB). (`v2-db`)
- **#276 speculation for agents**: what-if views, best-of-N candidates sharing every unchanged
  result, patching before checks finish with rollback. Views (section 2) make it nearly free.
  **#1302** lazy borrow checking is allowed only on this live-patch path, never for a build's
  output (cargo's rule: a borrowck error means no binary). (`v2-db`, `v2-link`)

## One database, shared by every thread

The database is `Sync`: every worker reads it through `&Database`, and nothing clones a handle
per thread. salsa's `Storage` is `!Sync`, so v1 gives each thread its own handle (a clone that
shares the memo tables and counts clones, so a write waits for every clone to drop). v2 has no
handles. All parallel work, macro expansion rounds included, runs as plane tasks on the one
worker pool (`docs/v2/plan/lanes/core.md`), never on threads spawned by a query.

## Done when

- blaze-query is Sync, keyed by input hash, with fingerprints on every edge and sound verification
- hir-def, hir-expand and hir-ty run on it; no salsa per-thread handles; expansion rounds are plane tasks
- the image, settle swap, GC and the why-did-this-rerun explainer work
- types and the trait solver are u32-indexed tables
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
