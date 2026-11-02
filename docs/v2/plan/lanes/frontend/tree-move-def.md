---
status: plan
summary: frontend-tree-move-def, hir-def's build path moved from syntax handles onto borrowed views; the view twins hir-def adds for its callers outside the crate, and how the in-crate move is split.
date: 2026-10-29
---
# frontend-tree-move-def: hir-def onto views

Part of [tree.md](tree.md) (frontend.md item 16). Every use of `SyntaxNode`, `SyntaxToken`,
`SyntaxElement`, `SyntaxText` and the `syntax::ast` handle types in `crates/hir-def` moves onto
`syntax::view`, except the kind tags (`AstId<N>`, `FileAstId<N>`, `AstPtr<N>`, `InFile<AstPtr<N>>`,
`Idx<ast::UseTree>`, `dyn_map::keys::*`) and the tests that compare a view with its handle.

## Two rules: in place inside the crate, twins at its edge

- **In place inside hir-def.** A function whose callers are all in hir-def keeps its name and
  parameter order. Each `ast::X`, `&ast::X` or `InFile<&ast::X>` parameter becomes `ast::X<'t>` or
  `InFile<ast::X<'t>>` by value (views are `Copy`). A `SyntaxNode`/`&SyntaxNode` becomes
  `ast::Node<'t>`, and a `SyntaxToken` becomes `ast::Token<'t>`. `&dyn ast::HasAttrs` becomes a
  generic `impl ast::HasAttrs<'t>` (or the concrete view where there is one caller type).
  Returned handles become views with the same lifetime. `'t` is the tree's borrow: `'db` when the
  tree is the database's (`file_id.root(db)`, `AstId::to_view(db)`, `HasSource::source_view`),
  and the tree's own lifetime when the tree is kept outside the database (`BodySyntax::tree`).
  The caller in another file is moved by that file's owner, under its own request heading, with
  the line `needs: <the callee's heading>`. Nothing builds until every lane is done, so the
  headings land in any order.
- **Twins at the edge.** A hir-def function that `hir`, `hir-ty*`, `blaze-diagnostics`,
  `blaze-engine`, `blaze-lints` or another crate calls with or for a handle gets a view twin beside
  it. The twin is named `<name>_view` when the name is taken, and it means exactly what the
  handle version means. A test beside it compares the two on the crate's fixtures. The handle
  version is deleted by the request that moves its last caller.

## The twins hir-def adds (callers outside the crate program against these)

`src.rs` (this lane):

- `HasSource::source_view<'db>(&self, db: &'db Db) -> InFile<<Self::Value as Typed>::Of<'db>>
  where Self::Value: Typed`: the provided method, `ast_ptr(db)` viewed under
  `file_id.root(db)`. It is the same node as `source(db)`.
- `trait HasChildSourceView<ChildId> { type Value<'db>: ast::AstNode<'db>; fn
  child_source_view<'db>(&self, db: &'db Db, child: ChildId) -> InFile<Option<Self::Value<'db>>>;
  }`: one child's node, looked up per child (shape agreed with `frontend-tree-move-hir`), with no
  map gathered. It gives the node `child_source(db)[child]` gives, with the same cfg filtering
  and the same counting order: a walk of the list that stops at the child, `None` when there is
  none. It is implemented for:
  - `VariantId`: `LocalFieldId` to `Either<ast::TupleField<'db>, ast::RecordField<'db>>`;
  - `GenericDefId`: `LocalTypeOrConstParamId` to `Either<ast::TypeOrConstParam<'db>,
    ast::Trait<'db>>` (the first index of a trait is its `Self`, the trait itself), and
    `LocalLifetimeParamId` to `ast::LifetimeParam<'db>`.
  A `UseId` has no impl: finding one use tree means lowering the whole `use` again, so it goes
  through `use_tree_to_view`.
- `use_tree_to_view<'db>(db: &'db Db, use_ast_id: AstId<syntax::ast::Use>, index:
  Idx<syntax::ast::UseTree>) -> ast::UseTree<'db>`, beside `use_tree_to_ast`.
- `use_tree_sources_view<'db>(db: &'db Db, id: UseId) -> InFile<ArenaMap<Idx<syntax::ast::UseTree>,
  ast::UseTree<'db>>>`: `UseId`'s `child_source` with view values, for callers without
  `la_arena` (blaze-diagnostics `used.rs`). Filed by frontend-tree-move-diag-lints-uses as
  `## frontend-tree: hir-def src.rs: use_tree_sources_view, the UseId child source as views (2)`
  in `v2/requests/db-expand.md`; its test is in `src/tests.rs`.

`expr_store/source_map.rs` (this lane), on `ExpressionStoreSourceMap`. Each one takes the view by
value and looks up `AstPtr::of(node)`:

- `node_expr_view(InFile<ast::Expr<'_>>)`, `node_pat_view(InFile<ast::Pat<'_>>)`,
  `node_type_view(InFile<ast::Type<'_>>)`, `node_label_view(InFile<ast::Label<'_>>)`,
  `node_macro_file_view(InFile<ast::MacroCall<'_>>)`, `expansion_view(InFile<ast::MacroCall<'_>>)`,
  `macro_expansion_expr_view(InFile<ast::MacroExpr<'_>>)`,
  `implicit_format_args_view(InFile<ast::FormatArgsExpr<'_>>)`,
  `asm_template_args_view(InFile<ast::AsmExpr<'_>>)`.
  The `*_syntax` lookups (`expr_syntax`, `pat_syntax`, `type_syntax`, `label_syntax`,
  `field_syntax`, ...) give pointers, which are kind tags, so they have no twin: view the
  pointer with `ptr.value.to_view(ptr.file_id.root(db))`.

`expr_store/body.rs` (this lane):

- `BodySyntax::tree(&self, db: &Db) -> Arc<FlatTree>` (`std::sync::Arc`): the anchor as a tree
  of its own, one count of the tree kept on this thread or a new parse. View it with
  `syntax::view::Node::root(&tree)`. `BodySyntax::item` stays a `SyntaxNodePtr`
  (`syntax::view::to_node(item, root)`). `BodySyntax::root` (the handle) is
  `SyntaxNode::new_flat_root(self.tree(db))` until `blaze-lints/src/check.rs` moves, then
  deleted. The thread's tree cache keeps `Arc<FlatTree>`s, not handles.

`lib.rs` (this lane):

- `GenericDefId::file_id_and_params_of_view<'db>(self, db: &'db Db) -> (HirFileId,
  Option<ast::GenericParamList<'db>>)` beside `file_id_and_params_of`.

`visibility.rs` (part sigs, declared here because `hir-ty-codegen/type_id.rs` needs it now):

- `visibility_from_ast_view(db: &Db, has_resolver: impl HasResolver + HasModule, ast_vis:
  InFile<Option<ast::Visibility<'_>>>) -> Visibility` beside `visibility_from_ast`, the same
  visibility for the same node. It lowers through `item_tree::visibility_from_ast` moved in place
  onto `Option<ast::Visibility<'t>>` (part items).

Asked for by `frontend-tree-move-hir` (names agreed; each written by the part owning the file):

- `ExprCollector::lower_path_view(&mut self, ast::Path<'_>, ..)` beside `lower_path`
  (`expr_store/lower.rs`, part lower), with the rest of the parameters as `lower_path`'s.
- `ModuleSourceView<'db>` (`SourceFile(ast::SourceFile<'db>)`, `Module(ast::Module<'db>)`,
  `BlockExpr(ast::BlockExpr<'db>)`, the variants of `ModuleSource`, with `node(self) ->
  ast::Node<'db>`), `ModuleOrigin::definition_source_view(&self, db: &'db Db) ->
  InFile<ModuleSourceView<'db>>`, `ModuleData::
  definition_source_view(&self, db: &'db Db) -> InFile<ModuleSourceView<'db>>`,
  `ModuleData::declaration_source_view(&self, db: &'db Db) -> Option<InFile<ast::Module<'db>>>`
  (`nameres.rs`, part nameres).
- `parse_extra_crate_attrs_view(db: &Db, krate: Crate) -> Option<ast::SourceFile<'_>>`
  (`attrs.rs`, part attrs).
- `AttrId::find_attr_range_view` is in `hir-expand/src/attrs.rs`: `frontend-tree-move-expand`'s.

This lane's request headings, for the other parts' `needs:` lines:

| file | heading | requests file (applier) |
|---|---|---|
| `src.rs` | `## frontend-tree: hir-def src.rs: onto views (1)` | `v2/requests/db-expand.md` (`db-expand-items`) |
| `expr_store/source_map.rs` | `## frontend-tree: hir-def expr_store/source_map.rs: onto views (1)` | `v2/requests/db.md`, applied in the tree by this lane (the file is free) |
| `expr_store/body.rs` | none: edited in the tree by this lane (the file is free) | |
| `lib.rs` | `## frontend-tree: hir-def lib.rs: onto views (1)` | `v2/requests/db-expand.md` (`db-expand-items`) |

The other parts declare their edge twins in their own notes (below). Each part lists them there
first, then tells `frontend-tree-move-hir`, `-diag-lower`, `-diag-lints` and `-codegen` by tmux.

## The split

| part | paths in `crates/hir-def/src` | notes |
|---|---|---|
| this lane | `src.rs`, `dyn_map.rs`, `lib.rs`, `expr_store.rs`, `expr_store/{source_map,body,scope,pretty}.rs`, `expr_store/persist/**`, `hir.rs`, `hir/**`, `item_scope.rs`, `deps/**`, and any file no part names | this file |
| `frontend-tree-move-def-lower` | `expr_store/lower.rs`, `expr_store/lower/**`, `expr_store/expander.rs`, `expr_store/tests/**` | [tree-move-def-lower.md](tree-move-def-lower.md) |
| `frontend-tree-move-def-items` | `item_tree.rs`, `item_tree/**` | [tree-move-def-items.md](tree-move-def-items.md) |
| `frontend-tree-move-def-attrs` | `attrs.rs`, `attrs/**` | [tree-move-def-attrs.md](tree-move-def-attrs.md) |
| `frontend-tree-move-def-sigs` | `signatures.rs`, `visibility.rs`, `find_path.rs`, `resolver.rs`, `builtin_derive.rs`, `diagnostic_item/**`, `lang_item*` | [tree-move-def-sigs.md](tree-move-def-sigs.md) |
| `frontend-tree-move-def-nameres` | `nameres.rs`, `nameres/**`, `macro_expansion_tests/**`, `test_db.rs` | [tree-move-def-nameres.md](tree-move-def-nameres.md) |

The `parse_from_ast` callers move to `CfgExpr::parse_from_view`: `attrs.rs` (three, part attrs)
and `expr_store/lower.rs` (one, part lower).

This lane's in-place moves that part lower calls: `impl From<view::LiteralKind<'_>> for
Literal` (`hir.rs`) and `format_args::parse(s: view::String<'_>, ..)` (`hir/format_args.rs`).

## The body tree cache on views

`body.rs` keeps, per thread, the last 32 anchors' trees so a body's tree is not parsed again
right after its `BodySyntax` is made. On handles it kept `SyntaxNode` roots. On views it keeps
`Arc<FlatTree>`. `kept(anchor: ast::Node<'_>)` hashes the anchor in place
(`subtree_hash(tree, at)` of `anchor.flat()`), finds the kept tree by hash, and otherwise makes
`Arc::new(anchor.subtree())` once (a file's root shares the file's tree instead). An anchor is identified as before: the file tree's address
(`Arc::as_ptr(anchor.shared_file())`) and its number. Nothing is counted or cloned in a walk.
The one count per lowering is `tree()`'s `Arc::clone`.

## Proof

- `src.rs` tests: for every item of the crate's fixtures, `source_view(db)` has the kind and
  range of `source(db)`. For every child id of every variant and generic owner,
  `child_source_view(db, id)` has the kind and range of `child_source(db)[id]`, and `None`
  exactly where the map has no entry. `use_tree_to_view` matches `use_tree_to_ast` for every
  index.
- `source_map.rs` tests: for every body of the fixtures, each `node_*_view` gives what `node_*`
  gives, for every node of the matching kind under the body.
- `body.rs` tests: `BodySyntax::tree` viewed and `BodySyntax::root` give the same elements, and
  `tree()` twice on one thread is one parse (`Arc::ptr_eq`).
