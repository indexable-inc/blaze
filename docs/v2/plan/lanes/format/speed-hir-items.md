---
status: plan
summary: The item tree as columns (9 bytes an item plus a payload record of 1 to 16), lowering that allocates per file from a reused per-worker builder, and how it compares with Zig's ZIR declarations, Carbon's SemIR name scopes and rustc's arena HIR.
date: 2026-10-29
---
# speed-hir-items: the item tree against the state of the art

Code: `crates/hir-def/src/item_tree.rs` and `item_tree/` (`lower.rs`, `attrs.rs`, `persist.rs`,
`pretty.rs`, `tests.rs`). The columns are format-hir-tree's (brief `format-hir-tree.md`, under
the lead's addendum `format-hir-columns.md`). On top of them this lane:
- stores `ModPath` by value (16 bytes, `Copy`) instead of `Interned<ModPath>`;
- drops the item tree's own lazy-body guard, now that `syntax`'s `HasAttrs` does that check for
  every caller;
- owns the budgets, the checks and this comparison.

## Hot paths

- `file_item_tree` runs once per file and once per macro expansion (the def-map collector,
  `expand_round` in `nameres/collector.rs`).
- Lowering walks only the outline: `syntax`'s `HasAttrs::inner_attributes_node` enters a lazy
  `fn` body only when its text may start with `#!`, so lowering parses no body.
- The collector reads each item through `index`, `top_level_items`, `items`, `attrs` and the
  visibility table.
- On a cache hit, the tree is read from the store's bytes (`ItemTreeRef`).

## Before and after (64-bit)

| | before (rust-analyzer layout, as of 2026-10-29 17:50) | now |
|---|---|---|
| bytes per item, hot | 32 (`(FileAstId, SmallModItem)`) or 48 (`(FileAstId, BigModItem)`) | 9 (`ITEM_BYTES`: `u8` kind, `u32` data word, `u32` ast id) plus its payload record, 1 to 16 (`Impl` 1, `Function` 8, `Struct`/`Mod` 16): a `fn` is 17 |
| attributes | `(ModItemId, AttrsOrCfg)` 24 B a row, plus a `Box<[Attr]>` per item | `AttrOwner` 16 B per item that has any, and the item's attributes as one range of a flat `attrs` array (32 B an `Attr`) |
| lists (module, extern block, `use` group, constructor visibilities) | a `Box`/`ThinVec` each | `IdRange` (8 B) into one array per element type (CSR) |
| names, paths | `Name` and `Interned<ModPath>` in each row | `NameId`/`PathId` (`u32`), deduplicated per tree; a path is a 16 B `ModPath` by value, which saves the second intern probe and the pointer chase |
| allocations per file | growing `ThinVec`s for small, big, attrs and vis items, a `shrink_to_fit` realloc of each, an `FxIndexSet`, a `Vec` then `Box` for the top level | one exact-size `Box<[T]>` per non-empty column (at most 30), and no growth once the worker's builder is warm |
| allocations per item | a `Box<[Attr]>` per attributed item, a `Box<[ModItemId]>` per inline module or extern block, a `Box<[UseTree]>` per `{..}` group, an `Arena<ast::UseTree>` per `use` that was thrown away, an `FxHashSet` plus `ThinVec` per tuple struct with several `pub(in ..)` fields | none from the tree. What is left belongs to `hir-expand`: a `Box<AttrInput>` per attribute with input, and interning a `Name` or path segments, which allocates only the first time a symbol is seen |
| an edit inside a body | the tree is unchanged | unchanged byte for byte (`an_edit_inside_a_body_changes_no_byte_of_the_tree`) |
| an edit to one item | other items' rows could shift | only that item's name and ast id change (`an_edit_of_one_item_changes_only_its_rows`), so the def map can cut off on names that are unchanged |

How lowering allocates (`lower.rs`):
- `TreeBuilder` lives in a thread-local `SCRATCH`, taken for each tree and returned empty with
  its capacity kept.
- Builders that grew past `SCRATCH_ITEMS` (8,192 items, about 100 KiB) are dropped, which
  bounds what a worker holds.
- Pending lists (`pending_items`, `pending_use_trees`, `ctor_scratch`) are stacks inside the
  builder. A list is lowered onto the end of its stack and drained into its CSR array when it
  is done.
- Lowering has no `String` and no `format!`.

## The benchmarks

- **Zig's ZIR declarations.** An instruction is a `u8` tag plus 8 data bytes in a
  `MultiArrayList` (9 bytes). A declaration is one instruction plus one run of `extra` words
  (its source hash, name, flags and value bodies), and AstGen allocates nothing per
  declaration. The item tree matches this:
  - an item is 9 hot bytes plus one payload record, and nothing is allocated per item;
  - the payload (8 B for most kinds) is smaller than a ZIR declaration's `extra` run, because
    no 128-bit source hash is carried per item (salsa's per-file cutoff and the ast id do that
    job);
  - one difference is that ZIR keeps every kind's payload in one shared `extra: []u32`, where
    the item tree has one typed column per kind. This costs nothing per read, gives
    `size_of`-asserted records, and lets the persisted form be the columns.
- **Carbon's SemIR name scopes.** A scope is a name-to-instruction map plus flags, and an
  instruction is 16 bytes (kind, type, two args). The item tree holds no map: `ItemScope` is
  built by the collector (sibling speed-hir-nameres). Its items (9 to 25 B) are at or under
  Carbon's 16 B instruction plus the scope entry.
- **rustc's item collection over `DroplessArena` HIR.** A `hir::Item` is about 88 bytes plus
  its `ItemKind` payload, `Generics` and `Ident` spans, each bump-allocated per item with
  `Option<&'hir _>` links. The item tree is roughly 4 to 9 times smaller per item, and it has
  no per-item allocation, not even a bump.

## Where it doesn't match yet, and what it would take

- **`attrs(id)` is a binary search** over `attr_owners`: log2 of the number of attributed
  items, about 10 probes of 16 B records in a 1,000-item file. A per-item `u32` column would
  make it O(1) at 4 B more per item (13 B instead of 9). The collector reads attributes once
  per item, and most of that time goes into the attributes themselves, so 9 B stays. Revisit
  if a profile shows the search.
- **A cache hit copies.** `ItemTreeRef::decode` copies each record column with one `memcpy`
  (`column()` in `persist.rs`), and `top_level()` is read in place. The names, paths,
  attributes and `cfg`s, however, are re-interned from words:
  - a hash probe per `Name`;
  - a `Vec<Name>` per path in `Code for ModPath` (`hir/persist.rs`, sibling speed-hir-store).

  Reading these in place needs the symbol table persisted as ids that stay stable across
  processes (lane format and `intern`). Process-local `Symbol` pointers can't be mapped.
- **`Box<AttrInput>` per attribute with input**, and `CfgExpr`'s nested boxes, are
  `hir-expand`'s and `cfg`'s types. Columns for them (a token-tree range into a per-tree
  token array) would remove the last per-attribute allocation.
- **The first tree a worker lowers grows its columns by doubling**, about log2(items)
  reallocations per column, and later trees reuse the capacity. Sizing them up front from
  the flat tree's item count would mean walking the items twice, which costs more than the
  doubling it saves.
