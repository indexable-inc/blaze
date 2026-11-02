---
status: plan
summary: mem-db-def-resolve's plan for hir-def's name-resolution reads (path resolution, ItemScope, Resolver, visibility, find_path, DynMap) under blaze-mem; the hit paths at zero allocations, the per-worker scratch they use instead of per-call buffers, the exact-size scope the memo keeps, the DefMapRead view that puts the def map being collected in the collector's frame, the budget tests and the speed-of-light bound.
date: 2026-10-29
---
# mem-db-def-resolve: name-resolution reads at zero allocations

Part of `mem-db-def` under `mem-db` (`docs/v2/plan/lanes/mem/db.md`, whose contract this follows:
one frame per query run, memo values owned and exact-size, hit paths allocate nothing). The exact
code changes are in `v2/requests/mem-db-def-resolve.md` (R1 to R9, tests T1 to T4).

## Scope

The slice covers these files in `crates/hir-def/src/`: `nameres/path_resolution.rs`,
`item_scope.rs`, `resolver.rs`, `per_ns.rs`, `visibility.rs`, `find_path.rs` and `dyn_map.rs`.
They are read from inside query runs (the collector, inference, lowering) and from outside them
(the IDE layer in `hir`, diagnostics, printing). So nothing here uses `frame::current`. A
temporary is either a per-worker buffer (blaze-mem's `scratch` shelf, or a `thread_local!` holding
a `Scratch<T>` or a lifetime-free struct), or a buffer the caller lends, or a visitor in place of
a returned collection.

## What already costs nothing

- Path resolution walks subslices of the `ModPath`, takes `PathQuery` by value, and returns a
  `Copy` `PerNs`. Each segment is one `ItemScope::get`: a linear scan below `LINEAR` (8) names,
  otherwise one probe of the open-addressed index (4-byte slots holding a 2-bit namespace, a
  6-bit hash tag and a 24-bit position), or a memo hit of a collected module's `name`.
- Visibility checks walk `containing_module`, and `PerNs` holds no buffer.
- The resolver's value and type lookups read memos and `Copy` scopes. Its push and pop truncate
  one stack, which the speed-hir-sol-nameres request makes inline.

## The changes, and why each is the long-term shape

1. **Tables of a settled scope made at settle (R1, R2).** The by-name order (`entries`) and the
   by-def order (`name_of`) were built lazily on the first read after a change. For the memo's
   scope that first read is a hit path, so `settle` (and `seal` for frozen scopes) now builds both
   eagerly when the scope has more than `LINEAR` names. They are sorted in two blaze-mem `u32`
   scratch buffers and copied out once at exact length. A smaller scope keeps no table:
   `entries` sorts its at most 8 positions in an inline `SmallVec`, and `name_of` scans the
   column. `settle` also rebuilds each column and map at exact size (with capacity equal to
   length, then `extend(drain)`), rather than shrinking a doubled buffer, as the memo contract
   requires.
2. **Glob marks in the collector's frame (R3).** `PerNsGlobImports<A = Global>` holds hashbrown
   sets in `A`. The collector passes `&'f Frame`; the facts engine's carried state keeps `Global`.
   `of_scope_in` sizes the sets from the scope before filling them.
3. **Visitors in place of fresh sets (R4, R5).**
   - `for_each_trait_in_scope` and `has_trait_in_scope` walk the traits through a `ControlFlow`
     visitor.
   - `traits_in_scope_into` takes any `Extend<TraitId>`, so inference fills a frame set.
   - `names_in_scope_into` fills a map the caller keeps; hir keeps one per thread, taken and put
     back.
   - The owned `traits_in_scope` and `names_in_scope` remain as wrappers for callers that keep
     the result.
4. **`find_path` with a per-thread scratch (R8).** Module ids go into the scratch as `salsa::Id`,
   so it has no lifetime and lives in a `thread_local!`. A call takes it and puts it back, which
   makes a reentrant call get an empty scratch rather than a borrow error.
5. **`DynMap` as typed fields (R9).** One `FxHashMap` field per key replaces the anymap of
   `Box<dyn Any>`. That removes the `TypeId` hash and downcast per access, the box per kind, and
   the `dyn`.
6. **No span on `is_visible_from` (R7)**, the hottest check in `find_path` and method
   resolution.

## The def map being collected, in the collector's frame (R10)

Path resolution reads the def map while it is being collected (`resolve_path_fp` from the
collector), and that same def map then becomes the memo. So its `ModulesMap`, every scope
column, the children maps and the `macro_use_prelude` grew on the global heap. mem-db-def asked
for a view that lets them live in the collector's frame. R10 is that view:

- **`DefMapRead`.** It has a dozen methods: the header, a module's scope, a name in a module,
  legacy macros, parent, child, and the `macro_use` prelude. Path resolution, the textual macro
  scope and `is_visible_from_def_map` are written once against it, as provided methods of a
  `ResolvePath` extension trait. A collected `DefMap` implements it through its memos (the hot
  side, compiled exactly as today), and `BuildingDefMap<'f>` implements it from the frame.
  `ExternPreludeRead` covers the extern prelude the same way.
- **`ItemScope<S: ScopeStore = Settled>`.** A settled scope keeps exact `ThinVec`s and its two
  tables as boxes. A building one keeps frame `Vec`s, and its tables are remade on the first read
  after a change, into the previous table's buffer. All reads are one generic body over slices.
  The writes exist only on `ItemScope<Building<'f>>`.
- **`BuildingDefMap::settle`.** It replaces `shrink_to_fit` and `hand_over_scopes`. Each list,
  map, index and table is copied out once at its length, and every scope becomes its
  `ModuleScope`. `DefMap` keeps only collected ids, so the `Building` arms of `ModulesSlot` and
  `ScopeSlot` go.

mem-db-def's ruling (`db-def.md`, "Ruling: the scopes a collection builds live in its frame")
takes this full fix rather than a reservation. Its tmux wording, `ItemScope<A: Allocator>` with
`Vec<T, A>` lists and `DefMap<A>`, maps onto R10's names (request R10.6). An allocator parameter
alone cannot keep the memo's 8-byte `ThinVec`s, which a `Vec<T, Global>` would make about 350
bytes larger per module. Nor can it select the settled scope's exact tables, or a collected
module's `ModuleScope` id in place of an inline scope. `ScopeStore` is the allocator plus those
choices. mem-db-def-collect's Part H is written against these names, and it moves the cold path
onto the facts driver, since `collector.rs` is now the `#[cfg(test)]` reference.

Growth in the frame makes no global allocation: the frame grows in place in memory it reserved
up front. A cold `crate_local_def_map` therefore makes one global allocation per non-empty table
of its result, plus salsa's own, and none per name. T5 asserts that the count is identical for
widths of 9 to 513 names.

## Budget tests

T1 to T4 assert 0 allocations with `blaze_alloc_count`, built outside the measure on a warm
database:

- **T1** resolves one path of each shape on a warm def map: plain, `self`, `super`, `crate`,
  glob-imported, prelude, extern prelude with and without `::`, enum variant, an associated-item
  segment, and from a block both inward and out.
- **T2** runs a resolver at the innermost scope of a body: eight type-namespace and six
  value-namespace lookups, the traits through the visitor, `has_trait_in_scope`, the traits into
  a kept set, and the names into a kept map.
- **T3** reads a settled scope (small and wide) with no read before the measure.
- **T4** calls `find_path` again once the thread's scratch has grown.
- **T5** checks that a cold `crate_local_def_map` makes the same number of global allocations for
  9 to 513 names in one module.

## Speed of light

| Operation | Floor | After this plan |
|---|---|---|
| Path of n segments, warm map | n hashes and n index probes, 0 allocations | the same (T1) |
| Resolver lookup at depth d | d scope reads and one probe per scope that can hold the name | the same (T2) |
| Traits in scope | one read of each scope's memoized slice | the same, through a visitor (T2) |
| Read of a settled scope | 0 allocations from the first read | the same (T3) |
| Settled scope of n names (n > 8) | its columns, plus 4n bytes per table | the same: two tables of 4n bytes, built in reused scratch of 2 × 4n bytes per worker |
| Cold collection | one global allocation per non-empty table of the memo, plus salsa's | the same, with every table growing in the frame (R10, T5) |
| `find_path`, warm | 0 allocations, plus a `ModPath` intern only when the path is new | the same (T4) |
