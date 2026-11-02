---
status: plan
summary: speed-hir's index for lane db: name resolution, hir-def's attributes, signatures and interned locs, macro expansion (hir-expand) and base-db's inputs brought to the v2 speed bar; headline before/after per part and what is left.
date: 2026-10-29
---
# speed-hir (lane db): name resolution, expansion, inputs

`speed-hir` split `crates/hir-def/`, `crates/hir-expand/` and `crates/base-db/` by whole files.
The HIR layout parts (expression store, item tree) are lane format's:
[../format/speed-hir.md](../format/speed-hir.md). Each part's notes give the detail, the
benchmark it is measured against and the before/after.

| part | paths | notes |
|---|---|---|
| name resolution (fixed point, scopes, paths, find_path) | hir-def `nameres*`, `item_scope.rs`, `per_ns.rs`, `visibility.rs`, `resolver.rs`, `find_path.rs`, `import_map.rs` | [speed-hir-nameres.md](speed-hir-nameres.md) and its four parts |
| attributes | hir-def `attrs.rs`, `attrs/` | [speed-hir-defs.md](speed-hir-defs.md) |
| signatures | hir-def `signatures.rs` | [speed-hir-defs-sigs.md](speed-hir-defs-sigs.md) |
| interned locs, lang items, upvars | hir-def `lib.rs`, `lang_item.rs`, `upvars.rs`, small files | [speed-hir-defs-locs.md](speed-hir-defs-locs.md) |
| macro expansion | `crates/hir-expand/` but `builtin/` | [speed-hir-expand.md](speed-hir-expand.md) |
| builtin macros | hir-expand `builtin.rs`, `builtin/` | [speed-hir-expand-builtin.md](speed-hir-expand-builtin.md) |
| inputs | `crates/base-db/` | [speed-hir-basedb.md](speed-hir-basedb.md) |

## Headline numbers

| measure | before | after | part |
|---|---|---|---|
| imports retried per pass of the fixed point | every unresolved import | only those whose awaited slot was written | nameres |
| `resolve_import` calls, a chain of 64 modules each importing the next | about 4000 (O(n²)) | 127 (O(n)) | nameres |
| hashes and probes per `ItemScope::get` | 3 and 3 | 1 probe run for all namespaces | nameres |
| allocations per path lookup | one `ModPath` per def map crossed | 0 | nameres |
| name compares per attribute read | up to 38 string `eq`s, a `SmolStr` per segment | 1 hash, 1.18 probes on average, 0 allocations | attrs |
| memos per item `blaze-items` hashes | 5 | 1 | attrs |
| allocations for `EnumVariants` of 10 variants | about 11 | 2 | signatures |
| Fx rounds to intern a loc (`Macro2Loc`, `ItemLoc`) | 8, 4 | 3, 3 | locs |
| `ModPath` | heap `SmallVec` per 2+ segment path, then a second intern | 16 bytes, `Copy`, segments interned once; stored by value everywhere | expand |
| hygiene interns per transcribed `macro_rules` token | up to 3 lookups and 3 interns | once per distinct context per expansion | expand |
| span lookups per token, syntax to token tree | 1 binary search | 2 compares (in-order cursor) | expand |
| trees built per derive of a ten-field struct | 100+ | 1 | builtin |
| locks per `file_text`/`source_root` read | 1 `DashMap` shard `RwLock` (2 shared RMWs) | 0 (two acquire loads) | basedb |
| per unchanged file when a view is set | 2 allocations, 3 locks | 0 and 0 | basedb |
| `Arc` RMWs per `EditionedFileId::parse` | 4 | 0 (`returns(ref)`) | basedb |

Invalidation: a body edit runs no def-map or signature query (`demand_tests.rs`), and an item
edit that changes no name, visibility or import leaves both crates' frozen def-map bytes equal and
runs no dependent def map (`nameres/tests/backdate.rs`). rustc re-runs its resolver for the whole
crate on any edit, so this is finer than its red-green graph, and on par with Unison for edits
that keep names.

## Fitting the parts together (speed-hir)

- `Interned<ModPath>` removal across five children and `hir-expand`: `AstIdWithPath::path`
  (`hir-def/src/lib.rs`) and `hir_expand::attrs::Attr::path` are `ModPath`; the collector,
  `assoc.rs` and `attr_resolution.rs` read them by copy. `crates/hir/src/semantics.rs` no
  longer uses it either, so no `Interned<ModPath>` is left in the workspace;
  `intern::impl_internable!(ModPath)` in `hir-expand/src/mod_path.rs` is requested deleted.

## Round 2: speed of light, hot_lint, allocation budgets

Five children (`speed-hir-sol-basedb`, `-expand`, `-defs`, `-nameres`, `-items`) each wrote a
section of [speed-of-light.md](speed-of-light.md) (and the item tree's in
[../format/speed-of-light.md](../format/speed-of-light.md)), fixed or justified the `hot_lint`
findings in free files and filed exact edits for owned ones, and wrote `blaze-alloc-count` budget
tests. Where each stands against its bound:

| part | at the bound | furthest off, and what removes it |
|---|---|---|
| name resolution | `ItemScope::get`, scope push and lookup, `find_path` | an edit that changes no name, ≥ 40× (forward invalidation in the query engine); `resolve_path` 3.3× and `Resolver` 3.5× (`scope_name`, `SmallVec` scopes, db-expand-names) |
| attributes, locs, lang items, upvars | loc intern hit, `crate_lang_items` without the feature | `AttrFlags` after a body edit ≈ 50× (read attributes off `file_outline`, db-expand item 12); `upvars` ≈ 11× to 14× (`has_closures`, then locals kept at lowering) |
| signatures | lowering relative to the item | a body edit ≈ 30× until the attribute change above lands, then ≈ 7× |
| expansion | `Name`/`ModPath`/span lookups, `macro_rules`, `parse_macro_expansion` | a builtin derive ≈ 4× and `macro_arg` ≈ 2.5× (a reused output builder, one-block `TopSubtree`, format) |
| inputs | `parse` hit | `set_view_root` of a changed file in a large root up to 80× (a 16-way tree for the root's content id); the crate-graph set 50× (compare before hashing) |

Every memo hit is 2× its 5.4 ns bound, all of it salsa's hit path (speed-salsa's section).

Budgets: `crates/hir-def/src/alloc_budgets.rs` (flag reads, gated values, loc intern hits,
lang-item lookups, upvar reads: 0 each), path resolution (`nameres/tests/paths.rs`), hir-expand
(`budget_tests.rs`, `macro_expansion_tests/budgets.rs`), base-db (`tests.rs`); the item tree
accessors' budgets are filed as edit D in `v2/requests/frontend-deps.md` ("speed-hir-sol-items"). hir-def's harness (the
`#[global_allocator]`, `mod alloc_budgets`, and the port of `paths.rs` off its own allocator,
which must land together) is filed in `v2/requests/db-expand.md`, "speed-hir-sol-defs" item 1
and "speed-hir-sol-nameres".

Requests left with owners (2026-10-29), all in `v2/requests/db-expand.md` "## speed-hir: four
edits" unless said: `import_map.rs`'s counting sort onto `blaze_ids::radix::Csr`; base-db's
`Table` onto `intern::swiss::Index`; `upvars` reading `ExpressionStore::has_closures`
(the store side in `v2/requests/mem-format.md`); deleting `impl_internable!(ModPath)`, which
nothing uses since `Interned<ModPath>` is gone everywhere.

## Left, and what it would take

- `upvars` of a body with a closure builds a `Resolver` to find the locals its paths name (≈ 2.4
  µs against ≈ 170 ns). Keeping, for each single-segment path expression, the local it resolves
  to at lowering (rustc's `Res::Local`) lets upvars walk only the closure's own range with no
  resolver; it changes lowering and the store's encoding (format's, now mem-format's files).

- `parse_macro_expansion` and `EditionedFileId::parse` need `Hash` for `syntax::Parse<T>` and
  `mbe::ValueResult` under db-salsa's `lru` bound (requested from format by db-expand).
- hir-expand: one parse per `#[derive]` attribute instead of one per derive needs the derive's
  index in `BuiltinDeriveExpander::expander()`'s public signature; spans are 20 bytes against
  rustc's 8 (format's `Span`).
- base-db: `all_crates` still clones an `Arc` per call (callers in other lanes); `ContentId::of_many`
  hashes inputs one by one (format's choice not to use `blake3::platform`'s many-input path);
  `vfs::FileSet` has no remove, so adding or removing a file rebuilds its root's file set; views
  share no structure beyond salsa's inputs.
