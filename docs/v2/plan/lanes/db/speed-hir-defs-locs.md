---
status: plan
summary: speed-hir-defs-locs's pass over hir-def's interned locs, lang items, upvars and the small files (lib.rs, lang_item.rs, upvars.rs, builtin_derive.rs, builtin_type.rs, src.rs, dyn_map.rs, unstable_features.rs) against rustc's DefPathTable and LanguageItems; bytes and hash rounds per loc before and after, what changed, and what would shrink further.
date: 2026-10-29
---
# speed-hir-defs-locs: interned locs, lang items and upvars

Child of `speed-hir-defs`. Nothing here was built or run (the brief forbids it); every claim
below is from reading the code, and every change has tests to run when the lane's tree is built.

## Interned locs (`lib.rs`)

### What rustc does

rustc names a definition by a dense `DefIndex` (a `u32`) into `Definitions`' `DefPathTable`,
which stores one `DefKey` (parent index plus a disambiguated path element) and one
`DefPathHash` (a 128-bit stable hash) per definition. The `DefPathHash` is computed once, when the
definition is created, from the parent's hash and the key; nothing is hashed to look a definition
up afterwards, since the index is the handle.

### What blaze does

A loc (`ItemLoc`, `AssocItemLoc`, `EnumVariantLoc`, the macro locs, `BuiltinDeriveImplLoc`) is
interned by salsa: `intern_id` hashes the key once with `FxBuildHasher` (the generated `new`
wraps it in a `StructKey` whose derived `Hash` forwards to the loc), probes the shard's table
under the shard's lock and moves the loc in by value. `Lookup` returns `&'db Loc`, a reference
into salsa's table, which is as cheap as a copy and needs no change. So the cost per intern is one
hash of the loc plus one probe; the work was making that hash cheaper.

Sizes, from the types (each now asserted with a `const` next to the loc, and the comment says
what the bytes are):

- `salsa::Id` is 8 bytes (index and generation, hashed as one `u64`).
- `hir_expand::HirFileId` is a 12-byte enum (a tag word and an id); `FileAstId` is a `u32`;
  `AstId` is 16 bytes.
- `ItemContainerId` and `AdtId` are 12 bytes each (tag word and id); `Name` is one pointer.

| loc | bytes | Fx rounds before | after |
|---|---|---|---|
| `ItemLoc<N>` | 24 | 4 | 3 |
| `AssocItemLoc<N>` | 28 | 5 | 3 |
| `EnumVariantLoc` | 32 | 5 | 4 |
| `Macro2Loc` | 28 | 8 | 3 |
| `MacroRulesLoc` | 28 | 8 | 3 |
| `ProcMacroLoc` | 32 | 7 | 4 |
| `BuiltinDeriveImplLoc` | 24 | 5 | 3 |

A round is one `(h + w) * K` of rustc-hash 2.1; the derived `Hash` spent one per enum
discriminant and one per small field. The hand-written impls hash each id as before and pack the
file-local id, the discriminants and the small fields (expander, flags, edition, kind, trait,
index) injectively into one `u64` (`hash_ast_id`, `MacroExpander::hash_word`,
`ItemContainerId::hash_id`, `AdtId::hash_id`). Every impl destructures all fields, so a new field
does not compile until it is hashed. `Eq` is unchanged (derived or field-wise), so the hash only
has to agree with it, and the tests in `loc_hash_tests` check that `a == b` exactly when the
hashes agree over every combination of a few ids, files, containers and expanders.

Hashes per intern: one, before and after (salsa's), now 3 or 4 rounds instead of 4 to 8.

### What would shrink further, and what it takes

- `ItemContainerId` in `AssocItemLoc` (the FIXME): an erased `salsa::Id` with the kind in the
  loc's spare bits would save 4 bytes per associated item, but every `.container` match in hir-def,
  hir-ty and hir changes. Worth doing as its own lane with the callers.
- `HirFileId`'s tag word: 4 bytes of every `AstId` are a tag. A `HirFileId` packed into one
  `salsa::Id` (the kind in the generation's high bit, as `span::HirFileId` does) would make
  `AstId` 12 bytes and every loc 4 bytes smaller. That is hir-expand's type, owned by the db lane.
- `EnumVariantLoc.name`: rustc keeps the variant's index in `AdtDef`, not in the id. Swapping the
  name for an index would make ids change when variants are reordered, a worse incremental result,
  so it stays.
- `ProcMacroLoc`'s 2 bytes of padding would need `CustomProcMacroExpander` narrowed in hir-expand.
- Salsa's own cost per value (the LRU link, revision, memo table and durability next to the loc)
  and the shard lock taken on an intern hit are salsa's: concurrency-audit item 4 in
  `v2/requests/db.md` and `speed-salsa.md`.

## Lang items (`lang_item.rs`)

rustc's `LanguageItems` is a dense array indexed by `LangItem`, filled by one pass over the
crate's attributes. blaze's `LangItems` already has that shape: a struct with one
`Option<TypedId>` field per lang item, read statically by the generated accessors, and
`AttrFlags::lang_item` already tests the `LANG_ITEM` bit before reading the attribute. So the
index needed no change and `AttrFlags`' API is untouched.

The Symbol to `LangItemEnum` mapping was a chain of `==` on interned symbols, up to one compare
per lang item (173 of them) for each `#[lang]` attribute, plus a `tracing` span per item. It is now
one `FxHashMap<Symbol, LangItemEnum>` built once from a const table (`OnceLock`): one pointer hash
and probe. Symbol equality is pointer equality, so the map answers exactly as the chain did;
where a symbol appears twice the first entry wins, as in the chain. The span per item is gone.
Tests: `symbol_table_matches_compares` checks the map against the old chain (kept under
`#[cfg(test)]`) for every table symbol and some that are not lang items, and
`assign_keeps_to_the_kind` checks that a lang item lands in its own field and only when the
target has the right kind.

## Upvars (`upvars.rs`)

Before: each closure got a fresh `FxHashSet` of mentioned bindings, merged into the enclosing
closure's set on exit, and every path in a closure body went to the resolver.

Now:

- A body with no closure returns `None` before building a resolver or walking anything.
- One `Vec<BindingId>` per body holds the upvars of every closure being walked, outermost first.
  On a closure's exit its tail is sorted and deduplicated in place, copied into the closure's
  `Upvars`, then compacted to what the enclosing closure also captures (bindings not owned by it)
  or truncated. No set or vector per closure.
- A path goes to the resolver only if its single segment (or `self`) is the name of some binding
  in the body (one `FxHashSet<Symbol>` per body). The resolver returns `ValueNs::LocalBinding`
  only when a scope entry's name equals the path's first segment, and scope entries take their
  names from the store's bindings, so a path that misses the set cannot resolve to a local.
- The per-closure `Box<[BindingId]>` stays: callers take
  `Option<&FxHashMap<ExprId, Upvars>>` (hir-ty `analysis.rs`, `coerce.rs`).
- Applied speed-hir-store-ty's column-view edits (`store.expr(e)` returns a view; ids by value),
  so this file depends on format-hir-core's `ExpressionStore::expr` landing.

Tests: `nested_three_deep_with_items` (three nested closures, items and unit structs that must not
count, inner upvars passed outward) and `shadowed_by_a_closure_local`, next to the existing ones.

## The other files

- `builtin_derive.rs`: `trait_method` looked the loc up twice; now once. `is_const` re-reads the
  syntax on every call; it is reached only from hir-ty's `impl_is_const`, so it is noted, not
  changed. Storing constness in the loc's 3 bytes of padding (set by the collector) would remove
  the syntax read, an edit in `nameres/collector.rs` for the parent.
- `src.rs`: `HasChildSource<LocalTypeOrConstParamId> for GenericDefId` is on the build path
  through `attrs.rs`'s generic-parameter attribute flags. For a trait it resolved the trait's
  source twice; now once, and both maps are allocated at their final size. The other
  `HasSource`/`HasChildSource` impls are IDE paths.
- `dyn_map.rs`: used only by `hir`'s semantics and `child_by_source`, off the build path. Its two
  `transmute`s now carry `// SAFETY:` comments (`KeyMap` is `repr(transparent)` over `DynMap`).
- `builtin_type.rs`: `BuiltinType::by_name` runs only when resolution falls through to builtin
  types; cold, no change.
- `unstable_features.rs`: one compare chain per `#![feature]` line per crate; cold, no change.
