---
status: plan
summary: frontend-tree-move-tools-doc, crates/blaze-doc moved off syntax handles onto borrowed views, and the lower layers' view twins it programs against.
date: 2026-10-29
---
# blaze-doc on views

[tree.md](tree.md)'s move for `crates/blaze-doc`. Every file writes `use syntax::view as ast;`;
nodes are `ast::X<'db>` by value with `'db` from `db: &'db RootDatabase`; text is `&'db str`
(`Node::text` is one slice of the source, so `syntax().text().to_string()` becomes `text()`, and a
`String` is made only where the old code kept one).

## Twins used

Already in the tree: `HirFileId::root(db)` (hir-expand), `hir_def::src::HasSource::ast_ptr` with
`AstPtr::to_view(root)` (a `UseId`'s `use` item), `InFile<TextRange>::original_node_file_range_rooted`
(what `InFile<&SyntaxNode>::original_file_range_rooted` is, over the node's range),
`CfgExpr::parse_from_view` (cfg).

From the layers below, as their notes declare them (tree-move-expand.md, tree-move-def.md,
tree-move-hir.md):

- frontend-tree-move-expand: `HirFileId::call_node_view(self, db: &Db) -> Option<InFile<syntax::view::Node<'_>>>`
  (`lint.rs` `level`, up from a macro's output to its call).
- frontend-tree-move-def: `hir_def::src::use_tree_to_view(db, use_ast_id, index) -> ast::UseTree<'db>`
  (`places.rs` `through_public_reexport`), `hir_def::src::HasSource::source_view(db)`
  (`places.rs` `UseAttrs::of`).
- frontend-tree-move-hir: `hir::view::HasSourceView::source_view(db)` (`collect.rs`, `sig.rs`,
  `places.rs`; `Macro` gives `Either<ast::Macro<'db>, ast::Fn<'db>>`, `Field`
  `hir::FieldSourceView::{Named, Pos}`), `Module::declaration_source_view` (`collect.rs`,
  `lint.rs`), `Module::definition_source_view` with `hir::ModuleSourceView::{SourceFile, Module,
  BlockExpr}` (`collect.rs`, `places.rs`), `hir::view::Semantics::file_to_module_def` (`lint.rs`).

## Proof

blaze-doc's tests (`src/tests.rs`: whole crates documented, their pages compared with what rustdoc
shows) keep their expectations: the same nodes are read in the same order, so the same docs,
notes, spans, `cfg` words and macro patterns come out. `lint.rs`' `lint_names` reads its token
trees through views. Nothing is allocated that the handle version did not: `cfg::parse_list`'s and
`attrs::key_values`' buffers hold `Copy` views where they held counted handles; `names_the_lint`
filters the node's text slice instead of `SyntaxText::to_string`.
