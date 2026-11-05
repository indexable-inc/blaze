---
status: plan
summary: speed-hir-nameres-fixpoint — the def-map collector's resolve-and-expand fixed point as a worklist (an import is retried only when a scope slot its last lookup read is written), glob importers as linked edge lists read in place, one scratch stack for glob propagation; against rustc's resolve_imports loop, rust-analyzer's DefCollector and semi-naive Datalog; imports retried per pass and allocations per propagation, before and after.
date: 2026-10-29
---
# speed-hir-nameres-fixpoint: the collector's fixed point

Files: `crates/hir-def/src/nameres/collector.rs` (with `attr_resolution.rs`, `mod_resolution.rs`,
`proc_macro.rs`), tests in `nameres/tests/worklist.rs`. A part of
[speed-hir-nameres](speed-hir-nameres.md).

`collect_defs` seeds a def map from the crate's item trees, then loops: resolve imports until a
pass records nothing, resolve and expand macros (each round's expansions one batch of database
tasks, `expand_round`), repeat; then reseed with an unresolved attribute. This note is about the
cost of that loop; what it computes is unchanged, byte for byte.

## Benchmarks

- **rustc, `ImportResolver::resolve_imports`** (`rustc_resolve/src/imports.rs`): takes the whole
  `indeterminate_imports` list, calls `resolve_import` on each, pushes back the ones still
  undetermined, and loops while the count of undetermined namespaces drops. Every pass retries
  every undetermined import: rounds times imports. rustc's per-name `NameResolution` (its
  `single_imports` set and `shadowed_glob`) decides *whether* a name is determined
  (`Determinacy`), not *which* imports to retry.
- **rust-analyzer's `DefCollector`**, as this file was: `resolve_imports` moved every
  unresolved `ImportDirective` through a fresh `Vec` each pass and `retain_mut`'d every
  indeterminate one; `resolve_import` opened an `info_span!` with
  `%import.path.display(..)` (a formatted `String` per call when a subscriber is on);
  `glob_imports: FxHashMap<ModuleId, Vec<(ModuleId, Visibility, GlobId)>>`, and every
  `update_recursive` that changed something collected a `held` `Vec` and a cloned `Vec` of the
  importers.
- **Semi-naive evaluation** (Datalog; Soufflé's delta relations): a recursive rule is joined only
  against the facts new in the last iteration, so total work follows the facts derived, not
  iterations times facts. Here an import is a rule, the scope slots its lookup read are its body,
  a scope write is a delta fact: an import fires only when a slot it read is in the delta.

## What changed

- **Imports are columns.** One `Vec<ImportDirective>`; the fixed point holds `u32` indices. Per
  import: its list (`Unresolved`, `Indeterminate`, `Resolved`), an attempt generation, two queue
  bits, its position in the indeterminate order. Nothing is moved between vectors. A `use`
  item's `RawVisibility` is borrowed from the item tree (`&'db RawVisibility`), one per `use`
  item, not cloned per use tree.
- **What a lookup read.** `resolve_import` calls `DefMap::resolve_path_fp_recording`
  (`path_resolution.rs`, `speed-hir-nameres-paths`), which reports each read in order
  (`PathRead`). Kept as wait keys: `Slot(module, name)` of this def map (namespace mask `TYPES`
  for a name that is not the path's last, all three for the last), `Env(name)` for a legacy
  macro of this def map, the extern prelude (crate def maps only) and the `macro_use` prelude.
  Reads of finished def maps (other crates, enclosing blocks) wait on nothing. A read without a
  key (`Unscoped`: a module's children, its declarations) waits on any write. A new std prelude
  module or a replaced child module wakes every import.
- **Waking.** Every scope write goes through `push_res_and_update_glob_vis` (item declarations,
  `mod` children, macro-defined items, imports, glob propagation all reach it through `update`),
  which wakes the key's waiters whose mask meets the namespaces written. Legacy macros, extern
  prelude and `macro_use` prelude entries wake `Env`. Waiters: `FxHashMap<WaitOn, u32>` heads of
  intrusive lists of 16-byte `WaitEdge`s (`import`, `generation`, `next`, `ns`) with a free list;
  an edge of an older attempt is stale and dropped when met.
- **Same order.** A pass is phase U (unresolved, by import index, from a min-heap) then phase I
  (indeterminate, by the order they became so). A woken import at or past the phase's cursor
  runs in this pass, as the old loop would have reached it after the write; one before it runs
  in the next. An import not woken read nothing that changed since its last attempt, so the old
  loop's retry of it resolved the same and recorded nothing. So each pass records what the old
  pass recorded, in its order, and returns the same progress flag: the def map and diagnostics
  are the old loop's.
- **Glob importers** (`ImporterLists`): one `Vec` of edges, linked per source (head and tail per
  source, `by_pair` for the visibility merge). Appending moves no edge, so `update_recursive`
  walks a source's list in place while it recurses; propagation adds no edge, so the walk sees the
  list as it was. A CSR form after building is not needed: the edges live only while the def map
  is collected.
- **Propagation scratch**: one `held: Vec<(Option<Name>, PerNs)>` used as a stack of ranges:
  `update` pushes its resolutions, each level pushes the filtered ones for its importers after
  its caller's and truncates when its importers are done.
- **The rest of the loop**: `resolve_macros` keeps its `Vec`s across rounds (`round_resolved`,
  `round_calls`, `macros_spare`); `skip_attrs` is probed only when non-empty; `mod_dirs` holds a
  16-byte `Copy` `ModDir` whose directory is an interned symbol; `mod foo;` candidates are built
  in one reused `String` (`path_buf`); no span formats in an inner loop.

## Before and after

| | before | after |
|---|---|---|
| imports retried per pass | every unresolved + every indeterminate | those a write since their last attempt woke |
| `resolve_import` calls, a chain of 64 modules each importing the next | ≈ 4000 (O(n²)) | ≈ 2n = 127 (O(n)) |
| allocations per `update_recursive` that changed a name | 2 (`held`, importer list clone) | 0 (shared stack, list read in place) |
| allocations per pass | 1 `Vec` of all unresolved directives, moved | 0 (heaps and queues reused) |
| per use tree | `RawVisibility` clone | a borrow |
| per `resolve_import` with a subscriber | a formatted path `String` | nothing |
| per attempt left waiting | — | one 16-byte edge per read slot of this def map, recycled |

The chain count is checked by `a_long_import_chain_takes_the_worklist_linear_calls`.

## Where it is still coarser than exact

- `Env(name)` is keyed by name alone: a `macro_rules! foo` in any module wakes every import that
  read a legacy `foo`. Exact would key by module; legacy scope walks parents, so the key would be
  the set of modules below, not worth it for how rare such waits are.
- `Unscoped` reads (a renamed enum's declaration, the test harness's parent) wait on any write.
  Both are rare; a `PathRead::Declarations(ModuleId)` would make them exact.
- Passes stay sequential: the order of records is part of the result (shadowing, diagnostics),
  so a pass cannot be run in parallel without a merge that reproduces that order.

## Tests

`nameres/tests/worklist.rs`, with `ResolveStrategy::Rounds` (the old loop, `#[cfg(test)]` only,
selected per thread) as the reference:

- every fixture of `nameres/tests.rs` and `nameres/tests/{globs,imports,macros,mod_resolution}.rs`
  (their raw strings, read with `include_str!`, `expect!` blocks skipped): every crate's
  `DefMap::dump` and diagnostics equal, and the worklist never makes more calls;
- 300 seeded crate graphs (xorshift64): 1-4 crates with dependencies and editions, nested
  modules, items under every visibility, plain, renamed, `_`, glob, enum-glob and variant
  imports from `crate`/`self`/`super`/dependencies/plain names, `pub use super::*` cycles,
  `macro_rules!` that define items (sometimes `#[macro_export]`), `#[macro_use] extern crate`,
  unresolved paths and calls;
- a 64-module import chain: rounds ≥ n(n-1)/2 calls, worklist ≤ 3n;
- a glob cycle, a glob shadowed by an explicit import, an enum reached through a renamed import.
