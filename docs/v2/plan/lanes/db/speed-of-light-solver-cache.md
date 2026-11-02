---
status: plan
summary: speed of light for the trait solver's cache lookup (hir-ty next_solver/interner/tls_cache.rs, the shared and per-query tables), with the arithmetic, what the code does to reach 90% of it, and what outside the file still stands in the way.
date: 2026-10-29
---
# Speed of light: the trait solver's cache lookup

File: `crates/hir-ty/src/next_solver/interner/tls_cache.rs`. The component is the step that
takes an interner to its search-graph `GlobalCache`: `shared` (goals whose reads were captured,
one cache per database, crate and region mode) and `of_query` (goals only good for the query
that solved them, one cache per interner). What the solver does inside the cache once it has it
is the search graph's, not this file's. Nothing here was run: costs are counted from the code
and the machine, not measured. `proof`'s budgets measure them once the tree builds.

## Machine and unit costs

Apple M5 Max (`sysctl`: 6 performance cores, 16 MB L2 per performance cluster, 128-byte cache
lines). Assumed per performance core: about 4.5 GHz (0.22 ns a cycle), L1 hit about 3 cycles,
L2 hit about 16 cycles, integer multiply 3 cycles, sustained IPC about 3.5.

| step | cost on the critical path |
|---|---|
| load, L1 / L2 | 3 / 16 cycles |
| spread a hint to a slot index (`window`: multiply, shift) | 4 cycles |
| FxHash, per word (add, multiply) | 4 cycles |
| a dyn call returning two words read behind two dependent loads (`nonce_and_revision`) | about 15 cycles from L1, 35 from L2 |
| nested `blaze_reclaim::enter` and its drop: a thread-local address and a counter's increment and decrement | about 15 instructions, off the critical path (nothing loaded depends on it) |
| outermost `blaze_reclaim::enter` (no executor task around it): adds a `SeqCst` fence | about 30 cycles |

Memory bandwidth and core count are never the bound. A lookup touches three or four lines: the
hint, the slot pointer and the generation's header, which is one line by a const assertion.
The table arrays are 256 KB (shared: 2^14 slots of 16 bytes) and 8 KB (per query: 2^9 slots).
The lines in use stay in L1 or L2. A hit writes nothing but the use mark, and only once per
eviction sweep, so the lines stay shared between cores and six cores look up in parallel with
no transfer between them: the bound per core is the bound of the machine times six.

## The bound

A cache that any thread can reach, keyed by what the interner holds, costs one index computed
from the key and two dependent loads: the slot's pointer, then the header it points to, to
confirm the key. The hint load runs beside the pointer load, since the branch on it is predicted.

- From L1: 4 + 3 + 3 = **10 cycles, about 2.2 ns**.
- From L2: 4 + 16 + 16 = **36 cycles, about 8 ns**.

The roughly 30 instructions around it, including the reclaim counter, take about 9 cycles at
IPC 3.5 and overlap the chain.

Going below needs the interner to hold its cache directly. `DbInterner` is `Copy`, so it can't
own a reference count, and a raw pointer would outlive the generation it names once the slot
is replaced. The two loads are what a `Copy` interner costs.

## What the code does, and how close it gets

**`of_query`.** The hint of a `Solved` is its number, `(n << 1) | 1`, with no hash, since the
number is unique already. The lookup carries no state: a `Solved` is made with its interner and
used only through it, and the interner's `&'db` borrow of the database keeps the revision from
advancing (salsa's `new_revision` takes `&mut Zalsa`). So a lookup never asks the database
anything. Debug builds still read the database's nonce and revision and assert that they never
change for a `Solved`.

- Critical path: hint 1 + index 4 + loads 3 + 3 + compares 1 = **12 cycles against 10, 83 to
  90%** (from L2: 38 against 36, 95%).
- Before this change it called `nonce_and_revision` and hashed the key: 15 + 6 + 4 + 6 + 2 =
  33 cycles, 30%.

**`shared`.** Today the critical path is the dyn call for the nonce and revision (15), then
the FxHash of the scope (four words: the nonce, `Option<Crate>` as a discriminant and an id,
and `regions`, about 18), then the index (4), the loads (6) and the compares (2): **45 cycles
against 10, 22%.**

What stands in the way is that the interner doesn't hold its scope's hint, nonce and revision,
so each lookup computes them again. The removal is to compute them once when the interner is
made:
- a 24-byte `SharedKey { hint, nonce, revision }` in `DbInterner`, since `krate` and `regions`
  are fields already;
- `with_regions` computes the hint again (one FxHash, about 18 cycles, at a point that already
  copies the interner).

The lookup is then 4 + 6 + 1 = **11 cycles, 90%**. The cost moves to making an interner: about
35 cycles, next to the `lang_items` and def-map reads it already does (tens of nanoseconds).
It also grows `DbInterner` from 48 to 72 bytes, which costs one or two cycles on a copy the
optimizer doesn't elide. `interner.rs` belongs to another lane, so this is filed as a request
(`v2/requests/solver-sync-glue.md`). `proof` should compare the build with and without it,
because the copy cost is spread over every interner pass, not one call.

**The guard.** `of_query` and `shared` hand a reference to a closure, so they need one
`blaze_reclaim` guard for the closure's length. Inside an executor task the guard is nested and
costs the thread-local counter, off the critical path. Outside a task it is the outermost guard
and pays a fence (about 30 cycles). Solver work runs in the daemon's tasks, so that case is
rare: tests and one-off callers.

**Making a `Solved`.** A thread-local block of 4096 numbers, so the shared counter is touched
once per 4096 interners. That's one thread-local read and write each, the least a number unique
across threads can cost without a shared write.

**A miss.** Allocate a generation, one compare-and-swap, and when the slot held another
generation, lock the limbo to retire it. That's under a microsecond, and a solve (microseconds
at least) always follows a miss, so the miss path is at most about 1% of what it leads to. The
eviction sweep scans the 16-slot window at most twice, which is 32 header reads from L2, about
0.1 µs, on a miss only.

## Against the state of the art

- **rustc** keeps its global caches in a `Lock<FxHashMap>`. Under the parallel front end, an
  uncontended lock and unlock is two atomic read-modify-writes (about 40 cycles) on a line every
  thread writes, plus the hash, so it gets slower as threads are added.
- **Sharded `RwLock` maps** (dashmap) cost two read-modify-writes per read.
- **Hazard-pointer maps** (folly) cost a store and a fence per read.
- **Epoch maps** (crossbeam) cost a thread-local pin with a fence on the first pin.

This table reads with plain loads and a nested thread-local counter, so a hit writes no shared
line. That matches the best of these and stays flat as cores are added.

## Memory (docs/v2/plan/design/memory.md)

A hit and `Solved::new` make no allocation (tested: `a_hit_allocates_nothing`). The warm
loop still calls the general-purpose allocator on a miss, and misses are in the warm loop:
an edit advances the revision, so each shared scope in use is replaced once. Every query that
solves goals whose reads weren't captured makes a new `Solved`, and so a new generation.
Per miss:

| what | allocations | lifetime |
|---|---|---|
| the generation (`Box<Generation>`, its `GlobalCache` inline, about 2.5 KB) | 1 | a revision (shared) or a query (per query), then a grace |
| `Limbo::retire` on a replace: a `Box<dyn>` around it, and the limbo's `Vec` when it grows | 1, plus amortized growth | until the grace passes |
| the `GlobalCache`'s chunks as it fills (`Chunked`/`Interned`: `OnceLock<Box<[T]>>`, doubling from 64) | about one per doubling per column | the generation's |

The design is to recycle, never free. A generation is a fixed-size object that comes and goes
across workers, which is the memory doc's pool. Once a grace passes, the limbo returns a
retired generation to a `blaze_mem::Pool` tagged `Infer`, with its `GlobalCache` cleared but
its chunks kept (grown once to their high-water mark, like scratch buffers). `replace` then
takes a generation from the pool instead of `Box::new`. In the warm state a miss makes no
allocation, and a budget test holds it to zero: a replace after a warm-up allocates nothing.

The two slot arrays are setup (once per process), which the memory doc allows. They move to a
`Region` tagged `Infer` so the cache's 264 KB shows in that budget.

What stands in the way, filed in `v2/requests/solver-sync-glue.md`:
- `blaze_mem::Pool` isn't written yet, and `blaze_reclaim::Limbo` boxes and pushes per retire
  (lane mem, request 8).
- `GlobalCache` has no `clear` that keeps its chunks, and its chunks come from `Box`, not a
  tagged region (vendor patch 0003, solver-sync, request 9).
- hir-ty doesn't depend on blaze-mem (lane db, request 10).

## Left outside this file

- The `SharedKey` precompute in `DbInterner`, above (`interner.rs`, solver-sync).
