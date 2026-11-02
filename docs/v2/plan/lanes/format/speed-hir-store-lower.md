---
status: plan
summary: speed-hir-store-lower — body and signature lowering (`ExprCollector`) moved onto the pass-2 HIR store: child lists on the builder's list stacks, forward source maps pushed, owned paths moved into side tables; ids and order unchanged.
date: 2026-10-29
---
# speed-hir-store-lower: lowering onto pass 2

Files: `crates/hir-def/src/expr_store/{lower.rs,lower/,expander.rs}`. Contract:
`~/restart-2026-10-28/briefs/v2/speed-hir-store-pass2.md`. Every lowering decision, the order
every node is allocated in and so every `ExprId`, `PatId`, `BindingId`, `LabelId` and
`TypeRefId` are exactly as before; only where the children live while a node is built and how
the forward maps are written changed.

## State of the art

Data-oriented compiler front ends (Zig's AstGen into ZIR, Carbon's semantics IR, the Rust
`rustc_ast_lowering` arena) build a node's children in a scratch stack shared by the whole
lowering and copy the finished list once into the node's extra data: no allocation per list,
one copy per element, and the scratch is reused across bodies. Side data (paths, literals) is
built once and moved, never cloned. Source maps are append-only and sorted once at the end
(Zig's `extra` + source token tables). That is the bar.

## Before (format-hir, pass 1)

- Every list (call and method args, tuple and array elements, match arms, record fields and
  field pats, block statements, closure params and their types, or- and slice-pattern parts,
  tuple and dyn/impl-trait type parts) was collected into an owned `ExprList`/`PatList`
  (`SmallVec`) or a `ThinVec<TypeBound>`, or a statement buffer taken and returned per block:
  one heap allocation for each list longer than its inline size, then copied into the store.
- Each forward source-map entry (`expr_map`, `pat_map`, `label_map`, `expansions`,
  `types_map`, `lifetime_map`) was a hash-map `insert`: one hash, one probe, and the rehash
  growth of a map sized from nothing; 14 such sites in `lower.rs`.
- A `Path` built for a desugaring (`for` loops, `?`, try blocks, `format_args!`, ranges, record
  literals and patterns, path expressions and patterns) was put in an `Expr::Path(&path)` view
  and cloned into the side table: an interned-`ModPath` refcount bump at least, a deep
  `GenericArgs` clone for a qualified path. `Literal`, `OffsetOf`, `InlineAsm` and method
  generic args the same.

## After

- One builder list stack per kind (exprs, pats, types, opt types, statements, arms, record
  fields, field pats, bounds). A node takes a `ListMark`, pushes its children, and is
  allocated through `ExprCollector::with_lists(mark, |this, lists| ..)`, which lends the
  stacks out so the node's view can borrow `lists.*_since(mark)` while the store copies it, and
  then truncates back to the mark. Nested nodes push above their parent's mark, so one stack
  serves any depth: zero allocations per list once the stacks have grown (they persist for the
  collector's life). `ImplTraitLowerFn` gets the bounds' `ListMark` instead of a `ThinVec`;
  argument-position `impl Trait` and type-alias bounds drain them out without a clone.
- Every forward map write is a `push((key, value))` into a `Vec`; the store sorts once in
  `finish` (later push wins, as a later insert did). No hashing while lowering.
- Owned values move into their side table: `alloc_path_expr(Path)`, `alloc_path_pat(Path)`,
  `alloc_literal_expr`, `alloc_offset_of_expr`, `alloc_asm_expr`, `alloc_method_call` (name and
  generic args by value), `alloc_record_lit`, `alloc_record_pat`, `alloc_tuple_struct_pat`,
  `alloc_path_type`, `alloc_fn_type`. A path is built once and never cloned for the store
  (`format_args!` clones its `args` path once per argument because each argument is its own
  expression; that is an interned refcount bump).
- Per-node cost is now one column push per node plus one copy per list element, which is the
  benchmark above.

## Order preserved, where it was not obvious

- A tuple expression or pattern with a leading comma allocated its `Missing` child after the
  others; it still does, and the stack slice is rotated so it comes first, as before.
- Slice patterns and array expressions-as-patterns push prefix, then the rest pattern, then the
  suffix, in source order, and split by a prefix length.
- A coroutine closure's params are lowered on the stack, handed to
  `lower_coroutine_body_with_moved_arguments` through the collector's reusable `closure_params`
  buffer, and the rewritten formals written back over the stack slice before the closure is
  allocated.

## Left

- `lower/path.rs` still interns a `ModPath` per lowered path (`Interned::new`); removing it
  depends on `expr_store/path.rs` (speed-hir-store-readers) and the `speed-hir-expand` request.
- `expander.rs` allocates nothing per node (it clones a parse's green-tree `Arc` per expansion);
  nothing to change.
- The builder API lowering calls is requested from speed-hir-store-core
  (`v2/requests/speed-hir.md`, "for speed-hir-store-core (from speed-hir-store-lower)").
