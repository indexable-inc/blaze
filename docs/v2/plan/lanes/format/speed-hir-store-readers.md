---
status: plan
summary: speed pass of the expression store's readers in hir-def (expr scopes as columns, a body's anchor maps shared by its anchor, the invalidation tests).
date: 2026-10-29
---
# speed-hir-store-readers

Files: `crates/hir-def/src/expr_store/{pretty.rs,scope.rs,body.rs,path.rs,tests.rs,tests/}`.
Contract: `~/restart-2026-10-28/briefs/v2/speed-hir-store-pass2.md`.

## Expression scopes (`scope.rs`)

`ExprScopes` is built for every body inference reads (`body_expr_scopes`), for every signature
and variant store, and read on every local name resolution (`resolve_name_in_scope`, the
resolver's scope chain).

**State of the art.** rustc keeps no scope tree: its resolver pushes `Rib`s on a stack while it
walks, and what later passes need is in the resolution tables. A persisted scope tree has to be
read by queries later (the IDE, the resolver of each expression), so the benchmark is the flat
parent-pointer tree with CSR entry lists of data-oriented compilers (Zig's AstGen scope data,
Carbon's semantics IR): fixed-width columns indexed by a `u32`, no allocation per node.

**Before.** `Arena<ScopeData>`: per scope an `Option<ScopeId>` parent (8 bytes, `la_arena::Idx`
has no niche), an `IdxRange` (8) and a payload enum whose `MacroDef` variant boxed its
`MacroDefId` (a heap allocation per macro scope, and the enum aligned to 8): about 32 bytes a
scope. Entries were already one flat arena with a range per scope. `scope_by_expr` an
`ArenaMap<ExprId, ScopeId>` (`Vec<Option<Idx>>`, 8 bytes an expression). A build grew three
`Vec`s from empty and then `shrink_to_fit` reallocated each.

**After.**
- Per scope four columns, 17 bytes: `parent: [u32]` (`u32::MAX` for none), `kind: [ScopeKind]`
  (`u8`), `data: [u32]` (a label's id, or an index into the cold `blocks` or `macro_defs` side
  table), `entries: [[u32; 2]]`. `const` asserts: `ScopeKind` 1, `ScopeEntry` 16 (64-bit),
  `(BlockId, Option<LabelId>)` 16, `ScopeId` 4. No `Box` anywhere.
- `scope_by_expr` a dense `[ScopeId]` by expression, 4 bytes each, a sentinel for "none";
  `ScopeByExpr::iter()` yields `(ExprId, &ScopeId)` as `ArenaMap::iter` did, so callers compile.
- Each query is one or two indexed loads (`entries`, `block`, `label`, `macro_def`, `scope_for`);
  `scope_chain` one load per step.
- Building writes into a per-thread scratch (`Cell<Option<Building>>`, let go of above 1 Mi
  elements) and copies each column into one exact-size allocation: after the first body a
  thread builds, a build allocates nothing but its results, one allocation per non-empty column.

**Kept exactly.** The walk is the same code, generic over a `ScopeSink` (the columns, or the old
arena layout as the test's model). An entry range begins where the entries were when its scope
was made and ends after its last entry, as `IdxRange::new_inclusive(start..=entry)` did. That
rule lets a scope's range cover the entries of a scope made inside it before its next entry:
`let (a, const { let w = 2; w }, b) = ..;` gives the `let`'s scope the entries `a, w, b`, so a
`w` after the `let` resolves to the `const` block's local. rustc resolves it to whatever `w` is
outside (or errors). That is a bug inherited from rust-analyzer, kept here because this pass keeps
results; the fix is reported to speed-hir-store (the columns make it a sort of the entries by
scope at `finish`, a true CSR).

Fixed by speed-hir-store on 2026-10-29. `Building` keeps each entry's scope beside it, and
`finish` sorts the entries stably by scope, skipping the sort when they are already grouped.
The model is now a list of entries per scope. `a_name_after_a_const_pattern_block_resolves_outside_it`
pins rustc's answer.

## A body's syntax (`body.rs`)

The hot path on a cache hit, `Body::of`, clones one `Arc`; `Body::relative` runs only when a
body's own text changes. What an edit of a file reruns for every body in the file is
`body_syntax` (it reads the file's parse), then backdates.

**State of the art.** rustc's incremental compilation makes spans relative to their owner
(`-Zincremental-relative-spans`, default since 1.62) so an edit above an item doesn't dirty its
HIR; the per-owner work an edit still costs is hashing the owner's nodes. blaze's relative
bodies match that; the per-body cost of finding its relative maps should be O(the body), not
O(the anchor it is in).

**Before.** Each `body_syntax` walked its whole anchor (`AstIdMap::relative_to`, a walk of every
node of the anchor), scanned the file's lazy nodes up to the anchor (`holds_lazy`), and
allocated its own `AstIdMap` and `RealSpanMap` into its memo. For an `impl` of M methods and n
nodes, an edit anywhere in its file cost O(M x n) and stored M copies of the same two maps.

**After.** The relative `AstIdMap`, `RealSpanMap` and outline flag are made once per anchor and
shared by every body of it (`Arc`, kept per thread by the anchor's tree and number and the file,
as `trees` keeps the anchors' trees): O(n + M) per edit and one copy of the maps. The
`ast_id_map` query is read once per body, on a hit or a miss alike, so what the query reads
doesn't depend on the cache. The file text is an `Arc<str>` clone (a count, no copy).
`check_relative` and `anchor_offset` are unchanged (`anchor_offset` has to keep its tracked read
of where the item is: the anchor's range in `BodySyntax` is left out of its equality).

`holds_lazy` is a binary search of `FlatTree::lazy_nodes()` (sorted, preorder) for the first
lazy node at or after the anchor: O(log bodies) per anchor, where it scanned every lazy node
before the anchor.

## Pretty printing (`pretty.rs`)

`print_type_ref` reads `store.type_ref(id)` (the `TypeRef<'_>` view of the type columns), ids by
value. The text is the same.

## Tests

- `scope.rs`: every existing scope test also checks the columns against the model (the old
  arena layout built by the same walk): scope count, chain, entries, block, label, macro, every
  name resolved in every scope, `scope_by_expr` and `scope_for`, for the body and the signature,
  and the same scopes built again on the reused scratch. A new fixture reaches every scope kind.
- `tests/invalidation.rs`: (a) a whitespace edit and a comment edit inside a body keep its store
  `==` and its bytes equal while its source map moves; (b) an edit of one body runs
  `Body::relative`, `Body::of` and `body_expr_scopes` once (for it), leaves every other body the
  same allocation, and every signature's store equal, so a query reading it doesn't run;
  (c) a body lowered again after two bigger bodies used the thread's scratch is `==` and the
  same bytes.
