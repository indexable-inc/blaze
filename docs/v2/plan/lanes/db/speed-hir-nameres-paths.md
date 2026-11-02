---
status: plan
summary: Path resolution and the resolver's scope stack made cheap per lookup (speed-hir-nameres-paths): no allocation, one item-scope probe per segment, a stack of small Copy scopes; measured against rustc's resolve_path and Rib stack.
date: 2026-10-29
---
# Path resolution and the resolver, per lookup

Notes of `speed-hir-nameres-paths`, a part of [speed-hir-nameres](speed-hir-nameres.md), as of
2026-10-29. The code is `crates/hir-def/src/nameres/path_resolution.rs`,
`crates/hir-def/src/resolver.rs` and `crates/hir-def/src/nameres/assoc.rs`; the tests are
`crates/hir-def/src/nameres/tests/paths.rs`.

## What it is measured against

- **rustc's `resolve_path`** (`rustc_resolve/src/ident.rs`, `resolve_path_with_ribs`): one
  `resolve_ident_in_module` per segment, each a single probe of the module's resolutions keyed
  by `(Ident, Namespace)`, with the segments borrowed from the path and nothing built per call.
  Segments after the first are only looked up in the type namespace.
- **rustc's `Rib` stack** (`rustc_resolve/src/late.rs`): one `Vec<Rib>` per namespace, pushed
  and popped as the walk enters and leaves a scope. Each rib owns a hash map of its bindings.
- **rust-analyzer's `Resolver`, as it was here**: a `Vec<Scope>` whose macro scope held a whole
  `MacroDefId` and whose expression scope held its owner and its `ExprScopes`; each block scope
  pushed asked salsa for the block's local def map (`only_local_def_map`, always the crate's),
  and `resolver_for_scope` collected the scope chain into a `Vec` before pushing it.

## Before and after

| | before | after |
|---|---|---|
| allocations per lookup | one `ModPath` interned per def map the path continues in (a module of another crate, `super` out of a block), plus a `tracing::debug!` level check per segment | 0 (`resolved_path_allocates_nothing`) |
| probes for the first of several segments | textual `macro_rules!` scope (a walk up the blocks), the item scope, builtins, then the extern, `macro_use` and std preludes until every namespace was found | the item scope, stopping at the first source with a type; the macro sources are skipped |
| probes per later segment | 1 | 1 (`one_probe_per_segment`) |
| enum variant reached through an import | a scan of every type of the enum's module | one probe under the name the path used, and the scan only when that is not the declaration (a rename) |
| bytes per `Scope` | about 32 (the expression and macro scopes) | at most 24, asserted in `resolver.rs`; 16 where the niche fits |
| scope chain on a new resolver | collected into a `Vec`, then pushed | pushed into the one `Vec` and the range reversed, after one reservation |

Only the types namespace of a first segment is read when more segments follow, so the shorter
lookup is exact: `paths_match_reference_on_fixtures` and `paths_match_reference_on_random_crates`
compare the whole result (every namespace, the segment index, whether it is final, the prefix
info) with a reference resolver that looks everywhere, as the code did before. They run over
fixed fixtures and 48 seeded random crates (re-exports, globs, renamed imports, enums, nested
blocks with items, a std prelude, another crate, `macro_rules!`, the test harness module, the
2015 edition), from every module and every block module, in both resolve modes and both
builtin shadow modes.

## Other changes

- `resolve_path_fp_recording` reports, in order, every slot a lookup read (`PathRead`), so the
  import fixpoint can retry a lookup when exactly one of them is written; the probe test counts
  with it.
- `Resolver::traits_in_scope_into` fills a set the caller keeps; `hir-ty`'s
  `get_traits_in_scope` still collects a new set per method call in a block (asked of its
  owner in `v2/requests/speed-hir.md`).
- `macro_def_site` turns a `macro_rules!` or proc macro away by the kind of its definition
  node before walking the crate's macros.
- The test harness check reads the module's own name and one entry of its parent's children,
  not every child as a string.
- `assoc.rs` interns no second `ModPath` for a macro call and no longer clones the crate's cfg
  options into a diagnostic.

## Still open

- `assoc.rs`'s lookups of an item by name are linear over the trait's or impl's items; rustc
  keeps them in a map sorted by name and searches it (`AssocItems::filter_by_name_unhygienic`).
- `DefMap::with_ancestor_maps` in `nameres.rs` (not this part's) takes a `&mut dyn FnMut`.
