---
status: plan
summary: mem-db-def-collect's plan to move def-map collection (hir-def's nameres.rs and nameres/ except path resolution) onto blaze-mem; the frame each collecting query opens, which temporaries move, how each memoized result is made once at exact size, the def map being built kept in the frame and copied out once, the budget tests, salsa cycles and the speed-of-light bound.
date: 2026-10-29
---
# mem-db-def-collect: def-map collection onto blaze-mem

Part of `mem-db-def` under `mem-db` (`docs/v2/plan/lanes/mem/db.md`, whose contract this follows).
Paths: `crates/hir-def/src/nameres.rs` and, in `nameres/`, `collector.rs`, `facts.rs`, `facts/`,
`demand.rs`, `mod_resolution.rs`, `proc_macro.rs`, `attr_resolution.rs`, `assoc.rs`,
`diagnostics.rs`, `frozen.rs`, `uses.rs`, plus the new `tests/budget.rs`. Path resolution
(`path_resolution.rs`, `item_scope.rs`, `resolver.rs` and the rest) is mem-db-def-resolve's.
Every change is a request: `v2/requests/mem-db-def-collect.md`, which has the exact changes
(Parts A to H; H is the building def map on mem-db-def-resolve's R10). This note gives the design
and the reasons.

## The design

A collecting query (`crate_local_def_map`, `block_def_map`, every facts query, `TraitItems`,
`ImplItems`, `used_imports`, the demand queries) wraps its body in
`blaze_mem::frame::task(Tag::Nameres, ..)`. Salsa's `execute` opens no frame, and
`frame::current` would stop outside a task and, inside a nested query, hand out the outer query's
frame. It passes `&'f Frame` to whatever does the work: `DefCollector<'db, 'f>`,
`CollectCx<'db, 'f>`, `AssocItemCollector<'db, 'f>`, `frozen::Dec<'a, &'f Frame>`. Three kinds of
data are then told apart.

1. **Temporaries go to the frame.** These are the import worklist (`Imports`: directives,
   generations, the two binary heaps, the wait table and its edges, woken), the glob edges, the
   macro directive lists and per-round vectors, `mod_dirs`, `skip_attrs`, `prev_active_attrs`,
   the deferred derives, the name-record lists, the staged diagnostics and derive helpers, the
   import graph and its SCC solver, and the decode tables of a frozen map. They are `FrameVec`,
   `FrameMap` (hashbrown with `&'f Frame` as its allocator) and `BinaryHeap<_, &'f Frame>`, so
   dropping them is free and the next task reuses the region.
2. **Results are made once, at size.** Each owned box is made from a frame vector in one
   allocation (`Box::from(&v[..])` for `Copy` rows, `collect` from a `TrustedLen` drain otherwise),
   and maps are made with `with_capacity(len)` then `extend`. This replaces every
   `shrink_to_fit` and every `into_boxed_slice` of a vector with slack. The
   `DefMap.diagnostics` and `derive_helpers_in_scope` fields become boxed slices. A result
   that is a filter of a scope (`ModuleScope::traits`, `legacy_exports`, proc-macro helper names)
   counts first and then fills.
3. **The map being built is a frame value too.** Path resolution reads the modules, each
   module's `ItemScope` and children, `macro_use_prelude` and the extern prelude while the crate
   is collected. mem-db-def's ruling puts them in the frame, and mem-db-def-resolve's R10 gives
   path resolution a read trait (`DefMapRead`, `ExternPreludeRead`) that a collected `DefMap` and
   the collector's `BuildingDefMap<'f>` both implement. The collector writes `BuildingDefMap<'f>`
   (modules, scopes as `ItemScope<Building<'f>>`, children, both preludes, diagnostics, derive
   helpers) and `BuildingLocal<'f>`. `settle` copies each table out once, at its length, into the
   owned `DefMap` and `LocalDefMap`. The frozen read and the facts driver build their maps through
   the same `DefMap::from_settled`/`from_table`. Requests: Part H.

Values carried from one facts round to the next (`SccState.state: Arc<UnitState>`, `MacroStep`,
`SideTables`) are memo values, so they are owned. The per-round queues inside `UnitState` (heaps,
deferred lists, wait table, woken) are rebuilt each round from the carried state. They move into a
frame `ImportQueues<'f>`, so the carried part holds only what the next round reads.

Code outside query runs keeps its signatures and uses `blaze_mem::scratch`: `demand::walk_bodies`
and its kin, which drivers call, and the `scratch::with_string` buffer of module-file resolution.

## Salsa cycles

No nameres query is in a salsa cycle. There is no `cycle_fn`, `cycle_initial` or `cycle_result` in
the slice. The collector's fixpoint is a loop inside one run, and the facts rounds are distinct
keys, each reading only the round before. What a round keeps for the next is owned (above).

## The building `DefMap` (no residual left)

The building tables are frame values (Part H, on R10). The order a reader sees is kept: modules
and children in insertion order, a replaced name keeping its place as `FxIndexMap::insert` does,
scope columns, derive helper lists and diagnostics in push order, and the extern prelude in
first-insert order. `macro_use_prelude` is a hash map before and after, and its readers sort.
`settle` consumes the building map and returns only owned values, so nothing in the frame
outlives the run, and the borrow checker enforces it (`'f`). What stays on the global heap during
a miss is the result alone, plus the crate data's tables. Those are shared with the block maps,
so they are owned from the start, reserved to their bound before the first insert. The one
exception is a `#[proc_macro]` function missing from the build system's list, which grows its
table past the hint, and the tests count that by capacity.

A first-ever collection also pays salsa's interning of the ids it makes (the interned hash
tables' growth, which mem-db-salsa's M5 keeps on the global allocator). That cost is salsa's.
T5 (mem-db-def-resolve) holds the first-ever run to a count independent of scope width, and T6
holds a collection to its exact count.

## Budget tests

`nameres/tests/budget.rs`, written in full in Part F of the request file:

- **(a)** Collect a crate cold, then edit whitespace in a function body twice. The first
  re-collection warms what revalidation keeps. The second allocates nothing
  (`crate_local_def_map` revalidates, its item-tree input backdates). Reading a collected
  module's names and traits also allocates nothing.
- **(b)** A miss of `module_impls` (and of `ModuleScope::traits`) allocates salsa's run cost plus
  one allocation per non-empty box of the result. The count is computed from the result. Salsa's
  run cost is measured on the same query for a module whose result owns nothing, after a warm-up
  run takes the ingredient's one-time setup. When mem-db-salsa pools memos, that baseline goes to
  0 and the tests hold a miss to exactly its result's boxes, with no edit.
- **T6** (Part H8) A miss of `crate_local_def_map` that collects a crate again, with several
  modules, a `use`, a glob, a macro, a trait and an impl, allocates exactly its result's boxes
  plus salsa's cost of the run. The boxes are counted from the result by
  `DefMap::owned_allocations` and its kin, one per non-empty buffer. Salsa's cost is measured on a
  twin crate with the same files, modules and macro and none of the imports, so the twin makes
  the same salsa reads and ids. The edit changes a visibility, so every salsa id is found rather
  than made. The first-ever run's extra cost is salsa's interning, which T5 covers.

## Speed of light

What has to happen for collection: each import directive, macro directive and module row written
once, read by the fixpoint, and the result's rows copied once into exact boxes. Today every
temporary is a global allocation that also grows by doubling: each `Vec` field of `Imports`,
`DefCollector`, `ImportGraph` and `Solver` takes about \( \log_2 n \) reallocations, each copying
the vector (\( 2n \) element writes in total), at 15 to 25 ns per mimalloc call, then a shrink
realloc on the result. On the frame an allocation is a bump of about 2 ns, growth past the end is
an in-place extend, and dropping is free. So the bound for the temporaries is their bytes written
once, about \( 16 \) bytes per import (`WaitEdge`) plus 40 to 80 bytes per directive. That is
memory bandwidth: for a 10 000-import crate, about 1 MB, or tens of microseconds.

The building map is the same kind of temporary. Each module's scope is written once per binding
(40 bytes an entry) and read by the fixpoint. In the frame, a growing column is a bump: about 2
ns per allocation, with growth extended in place at the frame's top and otherwise copied once
within the frame. On the global heap it took about \( \log_2 n \) reallocations, at 15 to 25 ns
each plus a copy. The item tree's declared-item counts reserve each column before the module's
items are collected, which removes most of the frame copies too.

The result costs one global allocation per non-empty list, box, set and table (two per index
map), each at its length, written once by the copy in `settle`. That is \( b \) bytes of memcpy
for a result of \( b \) bytes plus about 20 ns per allocation. For a 1 000-module crate with ten
non-empty tables a module, that is about 10 000 allocations (about 0.2 ms) and a few MB copied
(under 1 ms at 10 GB/s). It is the floor, since the memo must own each table. T6 holds the count
to exactly that. On a revalidation and a hit the bound is 0 allocations, which budget test (a)
holds.

State of the art: rustc's resolver also builds its module tree and import resolutions in arenas
(`rustc_resolve`'s `ResolverArenas`, a `TypedArena` per kind), and it keeps them for the whole
session. Here the arena is the query's frame, reset per run, and only the memo is copied out at
its exact size. That gives the arena's build speed without keeping a collection's scratch alive
between edits.
