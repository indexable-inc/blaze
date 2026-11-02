---
status: plan
summary: speed-hir-nameres — name resolution (hir-def nameres, item scopes, path resolution, the resolver, find_path, the import map) brought to the v2 speed bar; index of its four parts and the def map's own changes.
date: 2026-10-29
---
# speed-hir-nameres: name resolution at the speed bar

Name resolution is `crate_def_map` and `block_def_map` (the collector's resolve-and-expand fixed
point), the per-module `ItemScope`s they hand to the database, `resolve_path_fp_with_macro`
(every path a body or signature names), the `Resolver` each body walk builds, and at the edge
`find_path` and the import map. The work is split by whole files:

| part | files | notes |
|---|---|---|
| the fixed point | `nameres/collector.rs`, `attr_resolution.rs`, `mod_resolution.rs`, `proc_macro.rs` | [speed-hir-nameres-fixpoint.md](speed-hir-nameres-fixpoint.md) |
| scopes and their bytes | `item_scope.rs`, `per_ns.rs`, `visibility.rs`, `nameres/frozen.rs` | [speed-hir-nameres-scope.md](speed-hir-nameres-scope.md) |
| path resolution, the resolver | `nameres/path_resolution.rs`, `resolver.rs`, `nameres/assoc.rs` | [speed-hir-nameres-paths.md](speed-hir-nameres-paths.md) |
| the edge | `find_path.rs`, `import_map.rs` | [speed-hir-nameres-edge.md](speed-hir-nameres-edge.md) |
| the def map, bodies on demand, invalidation | `nameres.rs`, `nameres/demand.rs`, `nameres/tests/backdate.rs` | this file |

## Before and after, by part

Each part's notes give the detail; the headline numbers:

| measure | before | after | part |
|---|---|---|---|
| bytes per scope entry | ~73 (`IndexMap` bucket and table, with slack) | 40 (8 names or fewer), ~45-47 above | scope |
| hashes and tables probed per `ItemScope::get` | 3 and 3 | 1 and 1 probe run for all namespaces | scope |
| allocations per decoded scope on a frozen hit | about 3 per non-empty map, 2 per list | 1 per column, plus 1 for the name index above 8 names | scope |
| allocations per path lookup | one `ModPath` per def map the path continues in, plus a `tracing` check per segment | 0 | paths |
| probes per later path segment | 1 | 1 | paths |
| bytes per `Resolver` scope | about 32 | at most 24 (asserted), 16 where the niche fits | paths |
| bytes per import-map item (one info) | about 104 | about 64 | edge |
| `ModPath`s built per `find_path` call | one per segment per candidate | one, for the chosen path | edge |
| imports retried per pass | every unresolved and indeterminate import | only those a write to a slot their last lookup read woke | fixpoint |
| `resolve_import` calls, a chain of 64 modules each importing the next | about 4000 (O(n²)) | 127 (O(n)) | fixpoint |
| allocations per glob propagation step (`update_recursive`) | 2 (importer list clone, `held`) | 0 | fixpoint |
| bytes of a collected module row's scope slot | ~64 | at most 16 | def map |
| `CfgOptions` clones per cfg'd-out item | 1 (a hash set) | 0 | def map |
| symbol interns per word of a scanned dependency body | 1 | 0 | demand |

## Benchmarks

- **rustc's resolver** (`rustc_resolve`): `NameBinding`s interned in an arena, `resolutions:
  FxIndexMap<BindingKey, &RefCell<NameResolution>>` per module, and
  `ImportResolver::resolve_imports`, which loops over every indeterminate import until a pass
  makes no progress. Its cost is rounds times imports, the same shape as the collector this
  started from; the fixpoint part replaces it with a worklist keyed by the slot each import waits
  on (semi-naive evaluation, as Soufflé's delta relations do).
- **rust-analyzer's `DefCollector`** as it was here: the same round loop, `FxIndexMap<Name, _>`
  per namespace per module, glob importers cloned per propagation step.
- **Invalidation: rustc's red-green graph and Unison.** rustc re-runs its resolver for the whole
  crate on any edit (resolution is not incremental: `resolver_for_lowering` is one query) and
  only cuts off after it. Unison keys definitions by content hash, so a name's meaning never
  changes under an edit to another definition. Here a body edit runs no def-map query
  (db-expand-split's `demand_tests.rs`, and `tests/incremental.rs`), an item edit that changes no
  name, visibility or import leaves `crate_def_map` equal so no dependent runs
  (`tests/backdate.rs`, below), and a query that reads one name of a module depends on that
  name only (`ModuleScope::name`): finer than rustc, which invalidates at crate grain, and on par
  with Unison for edits that keep names.

## The def map's own changes (`nameres.rs`)

- **No thread-local on the collection path.** `EXPAND_BUILTIN_DERIVES_ON_THIS_THREAD` was read
  inside `crate_def_map`: an untracked input, so on the plane a def map depended on which thread
  collected it, and setting it did not rerun anything. It is now a salsa singleton input
  (`BuiltinDerives`, set by `set_expand_builtin_derives(&mut db, bool)`). The process-wide
  `EXPAND_BUILTIN_DERIVES` stays an atomic: written once at startup before any database exists,
  read with one relaxed load per collection, so no core writes the line a reader loads. The
  `cfg(test)` thread-local stays, justified: `TestDB` runs batches on the calling thread.
- **Module rows**: `ScopeSlot::Building` held a `Box<ItemScope>` and an inline
  `FxIndexMap<Name, ModuleId>`, so every collected `ModuleData` (every module of every def map
  the database keeps, where the slot is only a `salsa::Id`) carried about 64 bytes for it. Both
  are now one box: the slot is at most 16 bytes (`const` assertion), about 48 bytes less per
  module row.

- **Diagnostics for cfg'd-out code** (`nameres/diagnostics.rs`): `UnconfiguredCode` carried a
  clone of the crate's `CfgOptions` (an `FxHashSet<CfgAtom>`), so collecting `std` allocated a
  hash set per disabled item. The options are always the crate's, so the field is gone and the
  one reader (`hir`'s `InactiveCode`) takes `Crate::cfg_options` when it emits.

## Bodies on demand (`nameres/demand.rs`)

Before codegen, every body of every dependency is scanned for words that may declare a root
(`may_declare_roots`).

- **Before**: each identifier of each scanned body was interned (`Symbol::intern`: a hash, a
  probe of the global symbol table, and for a word not seen before an insert under the table's
  shard lock), then compared. `owner_in_module` (per frozen block) built the module's whole list
  of body owners, then searched it.
- **After**: words are compared in place as `&str` against the root words, the crate's sorted
  macro list (binary search) and the local macro names (`FxHashSet<SmolStr>`, looked up by
  `&str`); scanning a body interns nothing and writes no shared memory. Macro definitions are
  still interned, once per macro. The owner search is a visitor that stops at the match, with no
  list built; `crate_body_owners` and `body_blocks` push into one `Vec` instead of one per
  module; `walk_bodies` allocates nothing per step; `settle` tests membership in a set instead of
  a linear `contains` per word per pass.

## Invalidation test (`nameres/tests/backdate.rs`)

Two crates, `main` glob-importing `lib`. Each edit of `lib` that changes no name, visibility or
import (a function's signature, fields of a struct and of an enum variant, the types of a const,
a static and an alias, trait and impl items, a body that declares an item) must leave both
crates' frozen bytes equal (`frozen::encode`) and, once `lib`'s def map is up to date, run
`main`'s `crate_local_def_map` zero times (`TestDB::log_executed`). Controls (a rename, `pub` to
`pub(crate)`, a new `pub use`) must change `lib`'s bytes and run `main`'s once.


## Reverse index for `name_of`/`names_of` (speed-hir-nameres-scope, for edge)

In a scope of more than 8 names, `ItemScope::by_def` (`OnceLock<Box<[u32]>>`, 4 bytes an entry)
is binary-searched instead of scanning the column. Like `sorted`, it is derived from the columns,
made once by the first read after a def changed, and cleared only through `&mut` where a def is
added or replaced. That is the write-once pattern `concurrency.md` allows, not a lock held on
reads. The answers and their order are unchanged; the model test checks `names_of`'s whole list.
