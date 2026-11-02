---
status: plan
summary: mem-db-solver's plan to move the trait solver (hir-ty's next_solver, rustc_type_ir, rustc_next_trait_solver) onto blaze-mem; why its per-goal temporaries recycle per worker instead of living in the run's frame, the inventory of what each goal allocates, interned storage on tagged regions, the budget tests and the speed-of-light bound.
date: 2026-10-29
---
# mem-db-solver: the trait solver onto blaze-mem

This is part of `mem-db`. It programs against the contract in `docs/v2/plan/lanes/mem/db.md`.
- **Paths:** `crates/hir-ty/src/next_solver/` (except `interner/tls_cache.rs`, which is mem-db's),
  `vendor/rustc_type_ir/` and `vendor/rustc_next_trait_solver/`.
- **Changes:** they all belong to lane db and its children, so each is a request in
  `v2/requests/mem-db-solver.md`. That file has the file, line, before and after for each.
  This note gives the design and the bound.

## Design: recycle, don't bump

The contract puts a run's temporaries in the run's frame, threaded as `&'f Frame`. The solver
can't use the frame well:

- **Too many short-lived goals.** One `infer` run solves thousands of canonical goals, and each
  one builds a fresh `InferCtxt` through `SolverDelegate::build_with_canonical`, with a search
  stack, nested-goal lists and candidate lists.
  - A frame held as `&Frame` can't roll back: `reset_to` and `scope` need `&mut`. The run's frame
    would grow by the **sum** of all goals' temporaries.
  - Recycled storage is bounded by the **deepest** stack of goals alive at once.
- **Lifetimes.** A frame type parameter would add a lifetime to `EvalCtxt` (160 mentions in
  vendored code that is rebased daily) and to `InferCtxt` (124 mentions).
  - `'db` can't carry it either, because `InferenceResult<'db>` outlives the run.
- **Upstream already recycles.** `SolverDelegate::obtain_canonicalizer_state` and
  `release_canonicalizer_state` are upstream's hook for keeping canonicalizer tables between
  uses. The design here extends that pattern to the rest of the solver.

So the solver's temporaries come from two places:

1. **Pools inside the search graph** (`SolverScratch`, carried by `SearchGraphStorage` through a
   new associated type `search_graph::Delegate::Scratch`).
   - Every `EvalCtxt` already borrows the graph mutably, so a take or give is a `Vec` pop or
     push. There is no thread-local access and no new parameter.
   - Between root goals, the storage goes back to the delegate through two new provided
     `SolverDelegate` items, `obtain_search_storage` and `release_search_storage`. Their defaults
     make a fresh one and drop it, so behavior upstream is unchanged.
2. **Per-worker shelves in hir-ty** (`next_solver/recycle.rs`) for the `InferCtxt` tables, the
   canonicalizer state, the fulfillment columns and the search storage.
   - A shelf keeps its N largest buffers and is emptied with capacity kept.
   - The `'db` lifetime is erased to `'static` only on **empty** values, behind one unsafe
     helper with a stated invariant. This is the same reasoning as the interner's
     `table::Erase`: `'db` only marks interned handles, and an emptied container holds none.
   - A context's tables bigger than 4 MiB aren't kept, so a huge body doesn't pin its high water
     on every worker.
   - Shelved bytes are counted under `Tag::Infer`.

`Interner` and `InferCtxtLike` don't change. The vendored changes are rustc_type_ir patch 0006
and rustc_next_trait_solver patch 0004.

## What each goal makes now, and where it goes

The full table, by file and line, is in the request file's inventory. In short:

- **Global-cache hit, goal with inference variables: 3 allocations.**
  - The canonical original values: the canonicalizer's `variables` buffer is given away by
    `mem::take`, so the next canonicalization regrows it.
  - The response's `opt_values`.
  - The canonical instantiator's `FxHashMap` cache, when the response holds a structured type
    with bound variables.
  - **After:** pooled buffers and a `DelayedMap`, so 0.
- **Goal computed (cache miss): roughly 10–25 allocations, plus 1 per probe and per candidate
  list.**
  - A fresh `InferCtxt`: five union-finds with four columns each, region-constraint columns,
    opaque storage and a canonicalizer state.
  - The eval context's nested goals: one list per goal and one clone per probe.
  - Candidate lists, the merge lists, the dedupe set.
  - The search stack and provisional cache, regrown for every root goal.
  - `BTreeMap` cycle heads and boxed head-usage maps under cycles.
  - **After:** all recycled or inline, so 0 once warm.
- **Fulfillment.** Each short-lived `ObligationCtxt` grows five column `Vec`s. **After:**
  recycled.
- **Results, unchanged.**
  - `GoalStalledOn`'s two lists: stored in the pending sets.
  - Nested-goal maps stored in global-cache entries.
  - External constraints, which are interned. With request 4 they are interned from a borrow,
    so a warm re-solve doesn't build them just to drop them.
  - Error lists.
- **Caches that outlive a goal.** The `GlobalCache` belongs to a `tls_cache` generation:
  - mem-db request 4 and solver-sync-glue request 8: a pool and a limbo;
  - solver-sync-glue request 9: `clear`, which keeps its chunks, and chunks on a region tagged
    `Infer`.

  This plan relies on those requests and doesn't refile them.

### Left after these requests

- **Probe clones of stalled goals.** A probe clones its parent's nested goals. A stalled goal's
  `GoalStalledOn` holds two `ThinVec`s, which clone by allocating. This costs allocations per
  probe only when the parent has stalled nested goals, which happens inside ambiguity.
  - Making them `SmallVec<[_; 2]>` in rustc_type_ir (and in hir-ty's `thin_vec!` constructors
    in `solver.rs`) would remove it.
  - It is left until request 8's warm test shows it on a real body.
- **Salsa's edge list.** A goal solved inside a salsa query replays its reads into the active
  query's edge list, which is salsa's buffer (mem-db-salsa). The solver budget tests run outside
  a query.

## Interned type storage

- **Done:** every interned kind is in hir-ty's own append-only tables (`interner.rs`
  `mod table`), with `u32` handles and lock-free reads: types, regions, consts, predicates,
  patterns, canonical inputs, valtrees, allocations, external constraints, clause lists and
  every `interned_slice!` list.
- **Left** (request 9):
  - The columns are 23 segments from the system allocator, untagged, with segment arithmetic on
    every read.
  - Lists must not cross a segment.
  - The index's grown slot tables are leaked, untagged allocations.
- **After:**
  - One lazily reserved `blaze_mem::Region` per column, tagged `Tag::Intern`: one base load per
    read, contiguous runs, and kernel-zeroed pages instead of `alloc_zeroed`.
  - The index's tables bump-allocated in one region per index, tagged `Intern`.
- **Address space:** under 1 TiB virtual in total, nothing committed until written. The largest
  column is `TYS` kinds, about 32 GiB virtual.
- **Why not `blaze_mem::Column`:** a `Rows` writes four columns at one row it claims once, so
  the claim can't be a column's own `fetch_add`.

## Budget tests

All use `blaze_alloc_count::assert_at_most`. Each request lists its own tests; these are the
three the contract asks for (request 8). They are database tests in hir-ty with `test-fixture`
as a dev-dependency, run outside any salsa query, so they measure the solver alone:

| test | measured | budget | today |
|---|---|---|---|
| `a_goal_from_the_global_cache_allocates_nothing` | 100 re-solves of `Wrap<?0>: Tr` on one context | 0 | ≥ 3 per solve |
| `a_cached_canonical_response_allocates_nothing` | build a context, make `?0`, solve from the cached response, drop | 0 | ≥ 3 per solve, plus the context's tables |
| `resolving_an_unchanged_bodys_goals_warm_allocates_nothing` | a third solve of a body's goals with a fresh interner (every goal computed) | 0 | 10–25 per goal |

Each unit test also has budgets:
- cycle heads (request 1);
- the scratch pools (request 3);
- warm contexts and forks (request 5);
- fulfillment (request 6);
- canonical instantiation (request 7);
- interning hits and misses (request 9).

## Speed of light

**The bound.** A warm goal's temporaries, as the algorithm needs them, cost:
- no allocator calls;
- the stores that empty the reused buffers: about 30 length and counter resets per `InferCtxt`,
  and one pop and one push per pooled list;
- the bytes the goal actually writes:
  - a stack entry, about 150 B;
  - 8 B per canonical variable;
  - about 40 B per nested goal and about 64 B per candidate;
  - per inference variable, 4 B of parent, 1 B of rank, its value and an undo row inside a
    snapshot.

None of that work can be removed: each byte is read again by the goal.

These are estimates (no benchmark was run, under this lane's rules):
- **Hit path.** The bound is about 50–100 ns:
  - canonicalize a few variables;
  - one lock-free global-cache probe (`tls_cache` and the `GlobalCache` get);
  - instantiate the response and apply it.

  Today there are 3 allocate/free pairs at about 20–40 ns each, which is 60–120 ns, or 60–200%
  over the bound. After the changes, the extra work is about 4 `Vec` pops and pushes on the
  graph's pools (about 1 ns each) plus one thread-local shelf access per root goal for the search
  storage (about 5 ns). That is roughly 5–10% of the bound, which meets the 10% bar.
- **Miss path.** A computed goal costs at least about 1–2 µs: candidate assembly, at least one
  probe, and canonicalizing the response. Today there are about 10–25 allocate/free pairs (about
  0.3–1 µs), or 20–60% over. After, the cost is one shelf take and give per `InferCtxt` (about
  10 ns, under 1%) plus the resets.
- **Interning.** A hit's bound is the index probe: three dependent loads and a compare.
  - On a hit, the region columns remove the segment arithmetic, a `leading_zeros` and a dependent
    segment-pointer load from each read.
  - On a miss, the cost changes from an occasional `alloc` of a growing segment to a commit
    system call per quarter of growth.

**State of the art.**
- **rustc** runs the same upstream solver code. It builds the same fresh `InferCtxt` per
  canonical goal and allocates the same lists per goal through its global allocator. Its
  interners are a `DroplessArena` (bump chunks, untagged) and sharded hashbrown sets that rehash
  by copying.
- **rust-analyzer**, which this tree's front end came from, uses the same solver with the same
  allocations.
- After these requests, blaze's solver makes no allocator calls warm, where rustc makes some on
  every goal. Its interned columns never move and are counted under one tag.

**Measuring it.** The budget tests pin the allocation counts exactly. Time against the bound is
lane speed's to measure with `blaze-bench` once the requests land: the same goals hit and missed
in a loop, with cycles per goal compared to the estimates above.
