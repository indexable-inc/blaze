---
status: plan
summary: frontend-tree-move-diag-lints-uses, blaze-diagnostics' name resolution lints (lints/names/imports*) and stability checks (lints/names/stability*) moved off the syntax handles onto borrowed views; what they program against and what each test proves.
date: 2026-10-29
---
# frontend-tree-move-diag-lints-uses

Item 16 of [frontend.md](../../design/frontend.md), design in [tree.md](tree.md), for
`crates/blaze-diagnostics/src/lints/names/imports.rs`, `imports/`, `stability.rs` and `stability/`.
Child of [tree-move-diag-lints.md](tree-move-diag-lints.md). Every file has a holder, so every
move is a request in the holder's request file, under `## frontend-tree: blaze-diagnostics <file>:
onto views (<n>)`: `imports*` in `v2/requests/diagnostics-lints-names-hot-imports.md` (lane
`diagnostics-lints-names-hot-imports`, done; its successor applies), `stability*` in
`v2/requests/diagnostics-lints-names-hot-stability.md` (whoever holds the stability lints;
`walk.rs` was `diagnostics-lints-names-stability-walk`'s port).

## What the files hold after the move

The files take `use syntax::view as ast;` and keep their bodies' names. Everything that held a
handle holds a 24-byte `Copy` view borrowed for `'db` (the `Cx<'_, 'db>` borrow): nothing is
counted, cloned or dropped to keep one.

- `imports/used.rs`: `Uses<'f, 'db>` (`'f` the frame its tables live in), `UseItem<'db>` (`node: InFile<ast::Use<'db>>`), `Tree<'db>`
  (`node: ast::UseTree<'db>`), `ExternCrate<'db>` (`node: InFile<ast::ExternCrate<'db>>`). The
  structures live inside `imports::lints`, within the borrow. `Uses::of` takes `&Walk<'db>`.
  Every path of the crate is resolved by `cx.vsema.resolve_path(path)` /
  `resolve_path_per_ns(path)` per path, by value: no handle per path. A `use` item's trees
  are numbered by `hir_def::src::use_tree_sources_view(db, UseId)`, the `UseId` `child_source`
  with view values keyed by `Idx<ast::UseTree>` (blaze-diagnostics has no `la_arena` to convert
  `use_trees_view`'s indices).
- `imports/{globs,unused,macros,qualifications}.rs` read `Uses<'_, '_>` as before; the item and
  binding nodes they report at are views (`binding_node`, `def_node`, `def_item` give
  `InFile<ast::Node<'db>>` from `HasSourceView::source_view`).
- `imports/qualifications.rs`: the one-segment probe of each name is a `ModPath` built as
  `speculative_resolve` builds one from `make::ext::ident_path(text)` (`self`, `super`, `crate`
  as their kinds, any other name one segment), resolved by
  `SemanticsScope::speculative_resolve_mod_path`: no tree is made, so the `alone` cache of made
  paths goes.
- Every node handed to the view `Semantics` (`cx.vsema.to_def(T)`, `scope`, `resolve_*`) is of a
  tree it recorded: the walk's trees, `Cx::syntax_view`, `cx.vsema.source(def)`,
  `module_definition_node`, `find_parent_file`. `HasSourceView::source_view` stays only where a
  node is read for ranges or attributes.
- `stability/def.rs`: `Def::source(cx) -> Option<InFile<ast::Node<'db>>>` by `cx.vsema.source`;
  `parent` walks out of a macro or file module by `cx.vsema.find_parent_file`; `def_parent`,
  `attr_parent`, `is_def_parent(ast::Node<'_>)` on views; `syntax_of` goes (a view's `syntax()`
  is `Copy`).
- `stability/attr.rs`: `each`, `deprecation`, `stability`, `stability_written`,
  `allows_internal_unstable`, `unstable_feature_bound`, `lists_feature` take
  `ast::Node<'t>`; `each` gives `(ast::Meta<'t>, ast::Attr<'t>)` through
  `hir_expand::attrs::expand_cfg_attr_view`.
- `stability/target.rs`: `Attribute::new(target, InFile<ast::Node<'_>>, ast::Meta<'_>,
  ast::Attr<'_>)`.
- `stability/lookup.rs`: `Index::deprecation_at`/`stability_at` walk `InFile<ast::Node<'db>>`;
  the memo keys stay `InFile<SyntaxNodePtr>` (ids).
- `stability/walk.rs`: reads the parent's `Tree<'db>::nodes` by index by value (`t.nodes[j]`),
  `replaced` as `&[ast::Node<'db>]`; every `&SyntaxNode` parameter is an `ast::Node<'db>`. The
  `CheckTraitImplStable` port (`ineffective_unstable`, `header_nodes`, `header_path`,
  `const_arg`) takes the impl, paths and nodes as views and resolves with `cx.vsema`.

None of these is called from outside `lints/names/imports*` and `lints/names/stability*`
(`imports::lints` and `stability::lints` keep their signatures), so no other mover is affected.

## What this programs against

Each owner's notes are the authority; a hunk is corrected when they change.

- Parent ([tree-move-diag-lints.md](tree-move-diag-lints.md)): `Walk<'db>`, `Tree<'db>`
  (`nodes`, `replaced`, `root` as `ast::Node<'db>`; `of_kind` by value; `ptr(ast::Node<'_>)`),
  `Walk::attrs(ast::AnyHasAttrs<'t>) -> impl Iterator<Item = (ast::Meta<'t>, ast::Attr<'t>)>`.
  Every request here needs `## frontend-tree: blaze-diagnostics lints/syntax.rs: onto views (1)`.
- `frontend-tree-move-diag-lower` (`cx.rs`): `cx.vsema: hir::view::Semantics<'db>` (the
  `Semantics` the `Walk` takes its roots from), `Cx::syntax_view(InFile<SyntaxNodePtr>) ->
  InFile<ast::Node<'db>>`, `cx::item_range_view(ast::Node<'_>)`,
  `cx::derive_paths_view(ast::Attr<'_>)`.
- `frontend-tree-move-hir` ([tree-move-hir.md](tree-move-hir.md)): `hir::view::Semantics`'s
  `scope`, `resolve_path`, `resolve_path_per_ns`, `resolve_method_call`, `resolve_macro_call`,
  `resolve_macro_call_arm`, `resolve_attr_macro_call`, `resolve_derive_macro(ast::Meta<'db>)`,
  `expand_macro_call`, `expand_attr_macro`, `expand_derive_macro(ast::Meta<'db>)`,
  `resolve_field`, `resolve_record_field`, `resolve_record_pat_field`,
  `resolve_tuple_struct_pat_fields`, `to_def`, `hir_file_for`; `HasSourceView`,
  `Module::{definition,declaration}_source_view`, `ModuleSourceView`, `FieldSourceView`.
  Called by the declared names only (`to_def(T)`, no `to_def2`). Asked of it (21:55):
  `SemanticsScope::speculative_resolve_mod_path(&ModPath)`.
- `frontend-tree-move-expand` ([tree-move-expand.md](tree-move-expand.md)):
  `MacroCallLoc::to_node_view(db)` (ranges only, `imports.rs`), `AstId::to_view(db)`,
  `expand_cfg_attr_view`, `AstKeyValueMetaExt for view::KeyValueMeta<'_>`,
  `ModPath::from_src_view`, `AsName for view::{Name, NameRef}`.
- `hir-def` `src.rs` (`frontend-tree-move-def`'s twins, held by db-expand): `use_tree_sources_view`,
  requested in `v2/requests/db-expand.md` under `## frontend-tree: hir-def src.rs:
  use_tree_sources_view, the UseId child source as views (2)`.

## Speed

Nothing in a walk allocates, counts or clones: a step is arithmetic on the flat tree's columns
(tree.md). Text compares (`.text() == "macro_use"`) read `&'db str`. `no_implicit_prelude` reads
the attributes as one chained iterator instead of a `Vec`. `macros.rs`' `inner` keeps its
`Vec<Element>` (a rule's elements are indexed by position); it holds 24-byte `Copy` views now.

## Proof

The moved tests in `imports/macros.rs` parse with `SourceFile::parse` and read the `Parse`'s view
(`parse.view()`), checking the same rules and spans as before. The UI tests of
`src/tests/lints/names/` (rustc's `.stderr` compared byte for byte) are the differential: every
diagnostic's span, text and order is unchanged by the move.
