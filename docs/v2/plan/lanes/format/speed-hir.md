---
status: plan
summary: speed-hir's index for lane format: hir-def's expression store and item tree as columns, measured against Zig's ZIR/AIR, Carbon's SemIR and rustc's arena HIR; headline before/after per part, speed-of-light verdicts and what is left.
date: 2026-10-29
---
# speed-hir (lane format): the HIR layout

`speed-hir` split `crates/hir-def/`, `crates/hir-expand/` and `crates/base-db/` by whole files.
This is the index of the HIR layout parts; name resolution, attributes, signatures, expansion and
inputs are lane db's: [../db/speed-hir.md](../db/speed-hir.md). Each part's notes give the
detail, the benchmark and the before/after; the speed-of-light arithmetic is in
[speed-of-light.md](speed-of-light.md).

| part | paths | notes | speed of light |
|---|---|---|---|
| expression store: exprs, pats, statements, type refs, paths, source maps, scopes, encoding | hir-def `hir.rs`, `hir/`, `expr_store.rs`, `expr_store/` | [speed-hir-store.md](speed-hir-store.md), [speed-hir-store-lower.md](speed-hir-store-lower.md), [speed-hir-store-readers.md](speed-hir-store-readers.md) | "The HIR expression store" |
| item tree | hir-def `item_tree.rs`, `item_tree/` | [speed-hir-items.md](speed-hir-items.md) | "The item tree" |

## Benchmarks

Zig's ZIR/AIR (a 1-byte tag and an 8-byte data word in separate arrays, payloads in `extra`),
Carbon's SemIR (12-byte instructions, value blocks as ranges) and rustc's `DroplessArena` HIR
(`hir::Expr` 64 bytes, a bump per crate). data.md's budget: a HIR expression 12 bytes, `extra`
counted apart.

## Headline numbers (64-bit)

| measure | before | after | benchmark |
|---|---|---|---|
| bytes per expression, hot | an `Expr` enum in an `Arena` (tens of bytes, boxed payloads) | 5 (`expr_kind` 1 + `expr_data` 4), `const` asserted | ZIR 9, SemIR 12, rustc 64 |
| bytes per pattern / type ref | enum per node | 5 / 5 | as above |
| bytes per statement | 32 (`Statement` enum) | 5 plus a `let`'s 1 to 4 `extra` words, about 9 typical | ZIR's trailing optionals |
| allocations per body lowered | 8 to 20 growing vectors plus a box per path, generic args and bound | 1 slab plus the non-empty side tables (3 or 4), scratch pooled per thread | rustc: 0 (bump), but its HIR is never freed or persisted |
| allocations for `<T as Trait<U>>::Assoc` | 6 | 0 (a 20-byte row and 2 args in path columns) | |
| back maps (HIR id to syntax) | `Option<InFile<AstPtr>>`, 24 bytes an id | 10 bytes an id plus 16 a file run | |
| `patterns_for_binding` | 32-byte `SmallVec` per binding | CSR, 4 bytes a binding plus 4 a pattern | |
| source map on a dependency's body | always built | never (`Body::of` lowers without it; `relative` lowers again with it on demand) | rustc: spans in every node |
| bytes per item, item tree | 32 or 48 | 9 plus a 1 to 16 byte payload record (a `fn` is 17) | ZIR declaration: 9 plus `extra` |
| allocations per item, item tree | a box per attributed item, inline module, `{..}` group | none from the tree; one exact box per non-empty column per file | rustc: a bump per item |
| `ModPath` in the store and tree | `Interned<ModPath>`, a second intern per path | 16 bytes by value, `Copy` | |

## Speed-of-light verdicts

- Store reads (`store.expr(e)`, `pat`, `type_ref`) and `finish`: at the bound (the node's bytes;
  one copy per column).
- Lowering a body: the store side is at the bound (\(5.7n\) ns + 1 µs per body); the syntax walk
  in front of it is estimated, to be measured at the first build.
- Store decode copies the columns once after verifying the hash; views over the verified buffer
  would reach the 0.8 cycles a byte bound.
- Item tree lowering: about 38% of the bound (29 µs against 11 µs for the example file); encode
  about 5× its bound; `attrs(id)` a binary search, O(1) with the rank index. The exact edits are
  in the item tree section of [speed-of-light.md](speed-of-light.md), filed to their owners.

## Invalidation

A body is lowered from its own subtree with relative span and ast-id maps, so an edit in one body
lowers only that body and its store backdates when its bytes are equal
(`expr_store/tests/invalidation.rs`); signatures are lowered from the item's outline alone
(`signatures.rs`, lane db). A body edit touches no signature query, which is finer than rustc's
red-green graph (its HIR is rebuilt per owner, its spans hashed relative to it).

## Left, and what it would take

- `Option<ExprId>` is 8 bytes: `la_arena::Idx` has no niche (`MatchArm` and `RefType` stay 16).
  Requested in `v2/requests/speed.md` "## speed-hir": vendor `la-arena` with a niche in `RawIdx`.
- `upvars_mentioned` decodes every expression to learn a body has no closure: requested
  `ExpressionStore::has_closures` (a byte scan of `expr_kind`) in `v2/requests/mem-format.md`
  and the caller in `v2/requests/db-expand.md`. Keeping a path's resolved local at lowering
  (rustc's `Res::Local`), so upvars needs no resolver, is open (lane db's index).
- Bindings and labels keep their arenas (16 and 8 bytes, flat records).
- The store and item tree columns move onto `blaze-mem` reserved regions and per-task frames:
  lane mem (`mem-format`), in progress.
