---
status: plan
summary: frontend-tree-move-def-items, hir-def's item tree (item_tree.rs, item_tree/**) lowered from borrowed views instead of syntax handles; the in-crate signatures it changes and the twins it programs against.
date: 2026-10-29
---
# frontend-tree-move-def-items: the item tree on views

Part of [tree-move-def.md](tree-move-def.md) (frontend.md item 16). The item tree is lowered for
every file on every edit (whole, or per changed item through `items.rs`), so it is the hottest
walk hir-def has. On views it walks the file's tree once through borrowed `Copy` views
(`use syntax::view as ast;`), reads `&'t str` text and interns each name and path once, as the
handle code did, with no counted cursor, no clone and no `dyn`. Kind tags stay handle types under
`use syntax::ast as tag;` (`FileAstId<tag::Item>`, `Idx<tag::UseTree>`, the turbofish of
`AstIdMap::ast_id_of_view::<tag::Struct>`); `item_tree.rs`, which holds only tags but for
`lower_whole` and `block_item_tree_query`, keeps `ast` for the tags and names views
`view::X`.

Requests go to `v2/requests/frontend-deps-items.md` (owner `frontend-deps-items`), one heading
per file: `## frontend-tree: hir-def item_tree/<file>: onto views (<n>)`.

## Signatures changed in place (callers in hir-def, moved by their part)

| function | was | on views | caller (part) |
|---|---|---|---|
| `item_tree::lower_use_tree` | `(db, tree: ast::UseTree, span_for_range) -> Option<Arena<ast::UseTree>>` | `lower_use_tree<'t>(db: &Db, tree: ast::UseTree<'t>, span_for_range: &mut (impl FnMut(TextRange) -> SyntaxContext + ?Sized)) -> Option<Arena<ast::UseTree<'t>>>`: the arena of the use's trees as views, in the order `Use::expand` numbers them. Its indices are `Idx<ast::UseTree<'t>>`; the source map's tags are `Idx<syntax::ast::UseTree>`, converted with `Idx::from_raw(idx.into_raw())` | `src.rs` `use_tree_source_map` (parent) |
| `item_tree::visibility_from_ast` | `(db, node: Option<ast::Visibility>, span_for_range) -> RawVisibility` | `visibility_from_ast<'t>(db: &Db, node: Option<ast::Visibility<'t>>, span_for_range: &mut (impl FnMut(TextRange) -> SyntaxContext + ?Sized)) -> RawVisibility`, moved in place (same name, the same `RawVisibility` for the same node); request `frontend-tree: hir-def item_tree/lower.rs: onto views (1)`, hunk 46 | `visibility.rs` (sigs' `visibility_from_ast_view` lowers through it), `signatures.rs` (sigs) |
| `AttrsOrCfg::lower` | `(db, owner: &impl ast::HasAttrs, cfg_options, span_map)` | `lower<'t, 'a, S>(db: &Db, owner: impl ast::HasAttrs<'t>, cfg_options: &impl Fn() -> &'a CfgOptions, span_map: S) -> AttrsOrCfg`: callers pass the view by value (an `ast::AnyHasAttrs<'t>` works too) | `nameres/assoc.rs` (nameres) |

Crate-private to `item_tree`: `lower_attrs_into` takes `ast::AnyHasAttrs<'t>` (one body for
every owner kind; dedyn-def's `AttrsIncludingInner` shape on handles, with the concrete view in
place of the iterator, whose view type is opaque per owner); `Ctx`'s
`lower_top_attrs`, `lower_module_items{,_marked}`, `lower_item`, `lower_top`,
`lower_macro_stmts`, `lower_block` and every `lower_<item>` take views by value. No edge twin:
nothing outside hir-def calls into `item_tree` with a handle.

## What it programs against

- hir-expand ([tree-move-expand.md](tree-move-expand.md)): `collect_item_tree_attrs_view`
  (request 3, `attrs.rs`), `ExpandTo::of_call_site_view` (request 2, `lib.rs`), `impl AsName for
  view::Name<'_>`/`view::NameRef<'_>` and `ModPath::from_src_view` (request 4, `name.rs`,
  `mod_path.rs`); `HirFileId::root`, `AstId::to_view` (in the tree).
- hir-def `attrs.rs` (part attrs): `parse_extra_crate_attrs_view(db, krate) ->
  Option<ast::SourceFile<'_>>`.
- span: `AstIdMap::ast_id_of_view::<N>(view)` (in the tree).
- syntax-bridge: `call_input(node: view::Node<'_>, map, span, mode)` for an attribute's token
  tree, the same `FlatConverter` pass `syntax_node_to_token_tree` takes on a flat node.
- base-db: `EditionedFileId::parse(db)` returns `&Parse`; `.view()` is the file's
  `ast::SourceFile<'db>`.

`&dyn ast::HasAttrs` (one body per item, no copy per node type) becomes the concrete
`ast::AnyHasAttrs<'t>`, a node and nothing else: still one body, and no `dyn` at all.

## The keys of `items.rs`

An item's key hashes its syntax but for its bodies (`hash_item`). On views it hashes the same
bytes in the same order: `(NODE, kind as u16)`, each token's `(TOKEN, kind as u16, text)`, the
subtree hashes of `FlatTree::hash_subtree`/`subtree_hash` at `node.flat()`, `is_lazy` with the
same meaning (a lazy node not parsed yet; never parses). Where the handle code compared a child's
number in its flat tree with the body's, the view code compares the views (`Node: Eq` is file,
part and number), which is the same test.

## Proof

- The crate's item-tree tests run on views unchanged in meaning: `assert_assembled_is_whole` (an
  edited file's assembled tree equals `lower_whole`'s, to the byte, on real files and random
  edits), the lowering counts (`items_lowered_by`, `items_hashed_by`) and the pretty-printed
  fixtures.
- Allocation budgets (already in `item_tree/tests.rs` and `items/tests.rs`, `blaze-alloc-count`):
  a warm builder lowers a file in one allocation (`a_warm_builder_lowers_a_file_in_one_allocation`:
  the walk itself allocates nothing), reading and expanding a tree allocate nothing, and a file
  whose items kept their keys allocates nothing.

## Speed

The walk is one preorder over the item list's nodes (no descent into bodies), each step
arithmetic on the flat tree's arrays: speed of light is the item list's node count times a few
loads, plus one `Name`/`ModPath` intern per name and path, which the handle code paid too. The
handle code paid on top a counted cursor per node visited and a clone per `dyn ast::HasAttrs`
owner; views remove both, so the lowering is bound by interning and the columns' pushes. The same
as rustc's AST lowering, which walks borrowed `&ast::Item`s from an arena.
