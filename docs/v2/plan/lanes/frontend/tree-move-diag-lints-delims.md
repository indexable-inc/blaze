---
status: plan
summary: frontend-tree-move-diag-lints-delims, `unused_parens`, `unused_braces` and `unused_import_braces` (lints/syntax/delims.rs, delims/precedence.rs) moved off the syntax handles onto views, with macro expansions through the view Semantics (`cx.vsema`).
date: 2026-10-29
---
# frontend-tree-move-diag-lints-delims

Item 16 of [frontend.md](../../design/frontend.md), design in [tree.md](tree.md), for
`crates/blaze-diagnostics/src/lints/syntax/delims.rs` and `delims/precedence.rs`. Child of
[tree-move-diag-lints.md](tree-move-diag-lints.md). Both files are held by lane `diagnostics`:
the move is one request, `## frontend-tree: blaze-diagnostics lints/syntax/delims.rs,
delims/precedence.rs: onto views (1)` in `v2/requests/diagnostics.md`.

## What the pass reads, on views

`use syntax::view as ast;`. Every node, expression, pattern and type the pass reads is a
24-byte `Copy` view borrowed for `'db` (the walk's `Tree<'db>`): every `&ast::X` parameter
takes `ast::X<'db>` by value, every `.clone()` of a handle is gone, `SyntaxNodePtr::new` is
`ast::ptr`. The memo of the last macro call (`Check::last`) is two views and a file, copied,
not cloned.

### Expansions through the view `Semantics`

The handle pass asked `cx.sema.expand_macro_call(&call)` for each macro call in a value
position. The view pass asks `cx.vsema.expand_macro_call(call)` (`hir::view::Semantics<'db>`,
the `Cx::vsema` field, `tree-move-hir.md`): the same `to_def` of the call and the same root.
The walk takes every root it has from `cx.vsema` (the parent's `lints/syntax.rs` heading), so
every view the pass hands it is of a tree it handed out.

### From lower layers

- `Cx::vsema` (`frontend-tree-move-diag-lower`) and its `expand_macro_call(ast::MacroCall<'db>)
  -> Option<InFile<Node<'db>>>` (`frontend-tree-move-hir`).
- `MacroCallLoc::to_node_view(&self, db: &'db Db) -> InFile<ast::Node<'db>>`
  (`frontend-tree-move-expand`, the name frontend-tree fixed): `nested` reads the outer call's place, `stand_in` its parent.
- `crate::cx::item_range_view(ast::Node<'_>) -> TextRange` (`frontend-tree-move-diag-lower`):
  the span of `unused_import_braces`.
- From the parent: `Tree<'db>` (`nodes: Vec<ast::Node<'db>>`), `Walk<'db>`, `Tree::skipped`,
  `from_expansion(cx, InFile<ast::Node<'_>>)`.

The handle `Cx::sema` is no longer read by the pass.

## What the files offer others

Nothing outside `delims`: `expanded`, `expanded_ty`, `Ctx` are `pub(super)` and read only by
`delims/precedence.rs`. `expanded*` and `precedence::*` take expressions, types and blocks by
value.

## Proof

- The UI tests in `src/tests/` (rustc's `.stderr` for `unused_parens`, `unused_braces`,
  `unused_import_braces`, compared byte for byte) are the differential: same diagnostics, spans,
  suggestions and order.
- `precedence.rs` tests, on views: the classifications (`requires_semi_to_be_stmt`,
  `is_block`, `is_plain_block`, `is_lazy`, `is_assign`, `path_return_type`) give what they gave
  on handles, on the same sources.
- `delims.rs` tests:
  - `the_view_expansions_are_the_handles`: for every `MACRO_CALL` of every tree of the walk
    of a fixture with expression, statement, pattern, type, nested and `cfg`'d-off macro calls,
    `cx.vsema.expand_macro_call` gives the same file, root kind and range as the handle
    `Semantics::expand_macro_call`, or `None` for both (kept while the handles exist); it also
    proves every tree the pass asks about is one `cx.vsema` handed out.
  - `delims_on_allocate_nothing_at_any_size`: with the three lints on (`-W unused_parens -W
    unused_braces -W unused_import_braces`), the pass over the walk of `quiet_crate(4)` and of
    `quiet_crate(400)` allocates nothing (after one warm run for the lint levels): nothing per
    node, so nothing that grows with the crate.
  - The parent's `delims_allowed_everywhere_cost_nothing` (`lints/syntax.rs`) stays true: the
    pass still returns before reading a node when no lint of it may fire.

## Speed

One pass over the walk's nodes, one `match` on each kind, a tag compare for a node that is not
delimited: the bound is a read of the walk's node column (24 bytes a node) and its kind,
memory-bound. The handle version's per-node cost on top of that was a count bump and drop for
each `cast(node.clone())` and accessor; views remove them. Expansion lookups (macro calls in
value positions only) cost what they did: `to_def` of the call and a cached root. rustc's `BuiltinCombinedEarlyLintPass` is the reference shape (one walk, static
dispatch); this matches it without its per-node `Vec`s.
