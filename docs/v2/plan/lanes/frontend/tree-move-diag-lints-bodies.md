---
status: plan
summary: frontend-tree-move-diag-lints-bodies, blaze-diagnostics' body lints (lints/bodies*), lint levels (lints/levels*) and lint registry moved off syntax handles onto views; the view functions they offer and what each test proves.
date: 2026-10-29
---
# frontend-tree-move-diag-lints-bodies

Item 16 of [frontend.md](../../design/frontend.md), design in [tree.md](tree.md), parent
[tree-move-diag-lints.md](tree-move-diag-lints.md). Paths: `crates/blaze-diagnostics/src/lints/`
`bodies.rs`, `bodies/`, `levels.rs`, `levels/`, `registry.rs`, `registry/`, `edit_distance.rs`.
Every file has a live holder (lane `diagnostics`), so every move is a request in
`v2/requests/diagnostics.md` under `## frontend-tree: blaze-diagnostics <file>: onto views (<n>)`.

## What the files program against

- `cx.vsema: hir::view::Semantics<'db>` (the parent's proposal to `-diag-lower`, a pub field of
  `Cx` made in `Cx::new`): `type_of_expr(ast::Expr<'db>)`, `type_of_pat(ast::Pat<'db>)`,
  `expand_macro_call(ast::MacroCall<'db>)`, `resolve_*`, `parse_or_expand(HirFileId)`,
  `find_parent_file(HirFileId)`, by value, the handle versions' results.
- `Cx::syntax_view(InFile<SyntaxNodePtr>) -> InFile<ast::Node<'db>>` and
  `Cx::node_view(InFile<AstPtr<N>>) -> InFile<N::Of<'db>>` (`-diag-lower`), each a node of a
  tree `cx.vsema` handed out.
- `hir_def::src::HasSource::source_view(&self, db) -> InFile<<Self::Value as Typed>::Of<'db>>`
  (`-def`) for an ADT's or impl's item node.
- `MacroCallLoc::to_node_view(&self, db: &'db Db) -> InFile<ast::Node<'db>>` (the name
  frontend-tree fixed, beside `to_node_item_view`) and `AstId::to_view(db)` (`-expand`).
- `hir_expand::attrs::expand_cfg_attr_view` (`-expand`), over `impl Iterator<Item = ast::Attr<'t>>`,
  calling back with `(ast::Meta<'t>, ast::Attr<'t>)`.
- `cx.vsema.ancestors_with_macros_file(InFile<Node<'db>>)` (`-hir`), for `patterns.rs`'s
  `match_level`; `lower/break_outside_of_loop.rs`'s `up(cx, InFile<Node<'db>>) -> Up<'db>`
  (`-diag-lower`, asked, not filed yet) for `util.rs`, `pointers.rs`, `statements.rs`.
- `syntax::view::ptr(Node<'_>) -> SyntaxNodePtr` for `SyntaxNodePtr::new(&node)`.

Kind tags stay: `InFile<SyntaxNodePtr>` (`BodyCx::expr_node`, `pat_node`, `binding_node`,
`mir_node`, `owner_node`, `lint`'s `at`), `AstPtr<syntax::ast::X>` of source maps.

## View functions these files offer (declared)

- `bodies/types/util.rs`: `util::syntax<'db>(b: &BodyCx<'_, 'db>, expr: ExprId) ->
  Option<InFile<ast::Node<'db>>>` (was `InFile<SyntaxNode>`): `b.cx.syntax_view(b.expr_node(expr)?)`.
  Callers in `bodies/calls/` and `bodies/types/` move with it: the heading
  `## frontend-tree: blaze-diagnostics lints/bodies/types/util.rs: onto views (2)`.
- `BodyCx`'s methods keep their id signatures; `expr_span` and `mir_span` read views inside.

## The lint levels on views

- `levels/attrs.rs`: what a lint attribute is read into keeps its tokens as views:
  `Segment<'t> { token: Option<ast::Token<'t>>, range }` (`Copy`, `name() -> &'t str`), so
  `Read<'t>`, `Reads<'t>`, `Item<'t>`, `ItemKind<'t>`, `Path<'t>`, `Items<'t>` take the tree's
  lifetime. A list's elements are `Copy` `ast::Element<'t>`s in the same inline
  `SmallVec<[_; 16]>`. `owner`, `owns`, `read`, `not_allowed`, `registered_tools` take views by
  value; `parent`, `place` take `&hir::view::Semantics<'db>` and an `InFile<ast::Node<'db>>`, and
  step out of an expansion with `find_parent_file` / `MacroCallLoc::to_node_view`; `key` is
  `node.map(ast::ptr)`, the same `SyntaxNodePtr` as before, so the node table's keys and the
  `at` of every lint still meet.
- `levels/builder.rs`: `Levels<'db>` and `Expectation<'db>` hold `InFile<ast::Node<'db>>` (the
  chain from a node out to the nearest seen, an expectation's node). They live in the
  `Cx<'_, 'db>` that made them (`decide`, `LevelMap<'db>`), so the views never outlive their
  trees. No clone is left in the walk.
- `levels.rs`: `may_fire` casts each `ATTR` of `Tree::of_kind` by value; the crate root's tools
  come from `Cx::root`.

## Proof

- The UI tests in `src/tests/` (rustc's `.stderr`, byte for byte) are the differential for every
  moved lint: same diagnostics, spans and order.
- `levels/`: the existing `a_level_seen_and_a_lint_that_may_fire_are_reads` (0 allocations for a
  level at a seen node and for `may_fire`), and the new
  `building_the_level_map_allocates_per_table_not_per_attribute`: with lints on, a level map
  built for 4 and for 400 functions (each with two lint attributes, in attributed modules) and
  asked once at each function allocates at most 80 more times at 400 than at 4. The tables
  double; one allocation per node or attribute would be 396 more.
- `levels/attrs.rs`'s `attribute_names_are_read_as_matched` reads every meta of a parse's views
  both ways.
