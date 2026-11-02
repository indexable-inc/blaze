---
status: plan
summary: frontend-tree-move-def-nameres, hir-def's name resolution, macro expansion tests and test database moved from syntax handles onto borrowed views; the ModuleSource view twins hir calls, and what each file needs from below.
date: 2026-10-29
---
# frontend-tree-move-def-nameres: name resolution onto views

Part of [tree-move-def.md](tree-move-def.md) (frontend.md item 16). Paths in `crates/hir-def/src`:
`nameres.rs`, `nameres/**`, `macro_expansion_tests/**`, `test_db.rs`. Every one has a live owner
(db-expand's children), so every edit is a request in `v2/requests/db-expand.md` under
`## frontend-tree: hir-def <file>: onto views (<n>)`, except the new test file below.

## The twins this part adds (callers outside hir-def program against these)

`nameres.rs`, beside the handle versions, which stay until `hir`, `blaze-engine`, `blaze-doc` and
`blaze-diagnostics` stop calling them:

- `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] pub enum ModuleSourceView<'db> {
  SourceFile(view::SourceFile<'db>), Module(view::Module<'db>), BlockExpr(view::BlockExpr<'db>) }`
  with `fn node(self) -> view::Node<'db>`: `ModuleSource` and `ModuleSource::node`, borrowed.
- `ModuleOrigin::definition_source_view<'db>(&self, db: &'db Db) -> InFile<ModuleSourceView<'db>>`:
  the file's root from `EditionedFileId::parse(db).view()`, an inline module or a block from
  `AstId::to_view(db)`. Same file id, variant and node as `definition_source`.
- `ModuleData::definition_source_view<'db>(&self, db: &'db Db) -> InFile<ModuleSourceView<'db>>`
  and `ModuleData::declaration_source_view<'db>(&self, db: &'db Db) ->
  Option<InFile<view::Module<'db>>>`.

`hir` re-exports `ModuleSourceView` ([tree-move-hir.md](tree-move-hir.md)).

## In place inside the crate

Nothing of this part is called with a handle from another hir-def file, so no in-crate
signature changes leave it. Inside:

- `nameres/assoc.rs`: `AssocItemCollector::{collect, collect_item, record_item}` take
  `Option<ast::AssocItemList<'db>>` / `ast::AssocItem<'db>` by value; the trait or impl is
  `AstId::to_view(db)`, a macro's items `ast::MacroItems::cast(parse.root())`. Ast ids by
  `AstIdMap::ast_id_of_view::<tag::X>`.
- `nameres/demand.rs`: `body_syntax(db, owner) -> Option<ast::Node<'db>>` reads
  `HasSource::source_view(db)` (the body, or a variant's discriminant), not `BodySyntax`: a body a
  macro writes has no `BodySyntax`, and this reads those too, as before. `any_word` and `words`
  take `ast::Node<'_>`; `owning_item` walks `ancestors()` of `AstId::to_view(db)` and goes up
  through `HirFileId::call_node_view`. `macro_text` reads `AstId::to_view(db)`.
- `nameres.rs`: `SourcePosition::new` (the legacy-macro lookup) reads `MacroCallId::call_node_view`.
- `nameres/uses.rs`: `macro_call` takes `ast::MacroCall<'db>`; a store's call is
  `ptr.value.to_view(ptr.file_id.root(db))`, its expression `macro_expansion_expr_view`.
- `test_db.rs`: `definition_source_view`, `source_view`, and the ancestors at an offset made in
  place (`token_at_offset` of `parse(db).root()`, `parent_ancestors` merged by length, what
  `algo::ancestors_at_offset` does), looked up by `node_expr_view`.
- `macro_expansion_tests/`: printed from views (`Node`'s `Display` is its text, `{:#?}` the same
  tree dump). The indent of a call is read off the whitespace before its first token, as
  `IndentLevel::from_node` reads it.

## Needed from below (agreed by tmux, requested by their owners)

- hir-expand ([tree-move-expand.md](tree-move-expand.md)): `HirFileId::call_node_view(self, db)
  -> Option<InFile<view::Node<'_>>>`, `MacroCallId::call_node_view`, `MacroCallLoc::to_node_view(db)
  -> InFile<view::Node<'db>>`, `ExpansionInfo::map_range_down{,_exact}_view` (request 2,
  `lib.rs`); `ModPath::from_src_view(db, view::Path<'_>, span_for_range)`, `AsName` for
  `view::Name<'_>` (request 4, `name.rs`, `mod_path.rs`).
- hir-def items ([tree-move-def-items.md](tree-move-def-items.md)): `AttrsOrCfg::lower(db, owner:
  impl ast::HasAttrs<'t>, ..)`, called with the `view::AssocItem` by value.
- hir-def (frontend-tree-move-def): `HasSource::source_view`, `use_tree_to_view`,
  `ExpressionStoreSourceMap::{node_expr_view, macro_expansion_expr_view}`.

## Speed

Name resolution runs on every edit. On views a walk is index arithmetic on the flat tree's
columns: no count bump per step, no `clone()` of a node to cast it, no `SyntaxText` walk to read
a token. `demand.rs`'s `any_word` over a body allocates nothing (a `blaze-alloc-count` budget in
`demand.rs`' tests: zero allocations over a whole body); `words` allocates only its result. The
assoc collector allocates its result and nothing per item it reads. The bound is the tree's
size: one pass over the body's element numbers, as rustc's `rustc_ast` visitor reads borrowed
arena nodes.

## Proof

- `nameres/tests/views.rs` (new): over fixtures with a crate root, `mod foo;` files, inline
  modules, a module a macro writes and blocks with items (their block def maps), for every module,
  `definition_source_view` has the file id and variant of `definition_source`, and its `node()`
  the same elements (kind, range, text) as the handle's, element by element in preorder;
  `declaration_source_view` likewise against `declaration_source`.
- The crate's existing tests of these files (name resolution, demand, uses, expansion) run on the
  views and must print what they printed.
- `demand.rs`: `any_word` of a body view allocates nothing.
