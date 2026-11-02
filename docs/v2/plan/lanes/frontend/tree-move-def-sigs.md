---
status: plan
summary: frontend-tree-move-def-sigs, hir-def's signatures, visibility, find_path, resolver, builtin_derive, diagnostic_item and lang_item files moved from syntax handles onto borrowed views; the one edge twin (visibility_from_ast_view) and the view method the outline copy needs.
date: 2026-10-29
---
# frontend-tree-move-def-sigs: hir-def's signatures onto views

Part of [tree-move-def.md](tree-move-def.md) (frontend.md item 16, [tree.md](tree.md)). Paths in
`crates/hir-def/src`: `signatures.rs`, `visibility.rs`, `find_path.rs`, `resolver.rs`,
`builtin_derive.rs`, `diagnostic_item.rs`, `diagnostic_item/**`, `lang_item.rs`. Every file here
has a live owner, so every change is a request (headings below).

## Inventory

| file | handle uses | what moves |
|---|---|---|
| `signatures.rs` | the outline (`FileOutline`, `SignatureSyntax`, `clone_subtree_without`, `parse_or_expand`, `to_node`), `child_tokens`, every `*Signature::lower`, `VariantFields`, `EnumVariants::lower`, `extern_block_abi`, its tests | all of it |
| `visibility.rs` | `visibility_from_ast` (pub, called from `hir`, `blaze-items`, `hir-ty-codegen`), `assoc_visibility`, `trait_visibility` | twin added, in-crate callers moved |
| `resolver.rs` | `extern_crate_decls_in_scope` (`loc.source(db)`, `rename`, `name_ref`) | in place |
| `find_path.rs` | one test (`check_found_path_` parses a path and calls `ModPath::from_src`) | the test, onto `ModPath::from_src_view` |
| `diagnostic_item/tests.rs` | `function_name` (`source(db).value.name()`) | onto `source_view` |
| `builtin_derive.rs` | `AstId<ast::Adt>` only, a kind tag | nothing |
| `diagnostic_item.rs`, `lang_item.rs` | none | nothing |

## The edge twin

`visibility.rs`:

- `pub fn visibility_from_ast_view(db: &Db, has_resolver: impl HasResolver + HasModule, ast_vis:
  InFile<Option<ast::Visibility<'_>>>) -> Visibility`, beside `visibility_from_ast`. The same
  `Visibility` for the same node. Callers outside hir-def: `crates/hir/src/lib.rs` (7),
  `crates/blaze-items/src/form.rs` (6), `crates/hir-ty-codegen/type_id.rs` (1); each passes
  `loc.source_view(db).map(|it| it.visibility())`.
- The handle `visibility_from_ast` stays until those move. Its body finds its node's view in the
  database's tree (`syntax::view::to_node(ptr, file_id.root(db))`, the tree the handle came from)
  and calls the twin, so it keeps working once `item_tree::visibility_from_ast` takes a view. The
  request that moves its last caller deletes it.

Nothing else in my files is called from outside hir-def with or for a handle: `SignatureSyntax`
and `FileOutline` keep their trees as `Arc<FlatTree>` and their nodes as `SyntaxNodePtr`s, and
`InactiveEnumVariantCode::ast_id` is a kind tag (`FileAstId<syntax::ast::Variant>`).

## In place (callers all in hir-def)

`signatures.rs`, with `use syntax::view as ast;`:

- `child_tokens(node: ast::Node<'_>, kinds)` reads `node.flat()`.
- `type Elide = for<'t> fn(ast::Node<'t>) -> Option<ast::Node<'t>>`; `left_out(item:
  ast::Node<'t>) -> Option<ast::Node<'t>>`; `bodies_under(node: ast::Node<'t>, body: impl
  FnMut(ast::Node<'t>))`.
- `SignatureSyntax::new(source, item: ast::Node<'_>, elide, frame)`; `root(&self) ->
  ast::Node<'_>` (`ast::Node::root(&self.tree)`, no count); `node<'s, N: ast::AstNode<'s>>(&'s
  self) -> InFile<N>` (the view type is the parameter, so callers infer it); `FileOutline::root`
  the same.
- `copy_without(node: ast::Node<'_>, elided: &[ast::Node<'_>]) -> Arc<FlatTree>` (new, private):
  `Arc::clone(node.shared_file())` when `node` is the file's root and nothing is elided (what
  `clone_subtree` shared), else `Arc::new(node.subtree_without(elided))`. The maps are made with
  `AstIdMap::relative_without_view(anchor, ast::Node::root(&copy), ptrs)`.
- `file_outline` walks `file.root(db)`; `item_syntax` finds an item in the outline with
  `syntax::view::to_node(ptr, outline.root())` and in the file with
  `syntax::view::to_node(ast_id_map.get_erased(..), id.file_id.root(db))`.
- Each `*Signature::lower(db, id, source: InFile<ast::X<'_>>, maps)`; `with_source_map` passes
  `id.lookup(db).source_view(db)`. `adt_shape(ast::StructKind<'_>)`, `as_name_opt(Option<ast::Name<'_>>)`.
- `lower_field_list(db, module, InFile<Option<ast::FieldList<'_>>>, Option<Option<ast::Visibility<'_>>>, maps, frame)`;
  `lower_fields<'t, Field: ast::HasAttrs<'t> + ast::HasVisibility<'t> + Copy>(..,
  fields: InFile<impl Iterator<Item = (Option<ast::Type<'t>>, Field)>>, field_name: impl
  FnMut(usize, &Field) -> Name, default_value: impl FnMut(&Field) -> Option<ast::ConstArg<'t>>,
  ..)`; the field goes to `is_cfg_enabled_for` by value.
- `EnumVariants::lower` ids each variant with `ast_id_map.ast_id_of_view::<syntax::ast::Variant>(variant)`.
- `extern_block_abi` reads `source_view`.

`visibility.rs`: `assoc_visibility` and `trait_visibility` read `loc.source_view(db)` and call the
twin. `resolver.rs`: `extern_crate_decls_in_scope` reads `loc.source_view(db)`; the names are
`AsName` of the views.

## What this needs from other parts

| from | what | heading |
|---|---|---|
| frontend-tree (`syntax-tree/src/view.rs`) | `Node::subtree_without(self, elided: &[Node<'t, L>]) -> FlatTree`, the cursors' `clone_subtree_without` | `## frontend-tree-move-def-sigs: Node::subtree_without (crates/syntax-tree src/view.rs)` in `v2/requests/frontend-tree.md` |
| parent (`src.rs`) | `HasSource::source_view` | `## frontend-tree: hir-def src.rs: onto views (1)` (in `v2/requests/db-expand.md`) |
| frontend-tree-move-expand (`name.rs`, `mod_path.rs`) | `AsName` for `view::Name`/`NameRef`; `ModPath::from_src_view` | `## frontend-tree: hir-expand name.rs + mod_path.rs: onto views (4)` (in `v2/requests/db-expand.md`) |
| def-lower (`expr_store/lower.rs`) | in place, views by value: `lower_type_ref(.., InFile<Option<ast::Type<'_>>>, ..)`, `lower_generic_params_within(.., Option<ast::GenericParamList<'_>>, Option<ast::WhereClause<'_>>, ..)`, `lower_impl`/`lower_trait`/`lower_type_alias`/`lower_function` taking `InFile<ast::X<'_>>`, `ExprCollector::lower_type_ref_opt(Option<ast::Type<'_>>, ..)`, `lower_const_arg(ast::ConstArg<'_>)` | `## frontend-tree: hir-def expr_store/lower.rs: onto views` (not filed yet) |
| def-items (`item_tree/lower.rs`) | in place: `visibility_from_ast(db, Option<ast::Visibility<'_>>, span_for_range)` | `## frontend-tree: hir-def item_tree/lower.rs: onto views (1)` (in `v2/requests/frontend-deps-items.md`) |
| def-attrs (`attrs.rs`) | in place: `AttrFlags::is_cfg_enabled_for<'t>(owner: impl ast::HasAttrs<'t>, cfg_options)`, the view by value | `## frontend-tree: hir-def attrs.rs: onto views (1)` |

The headings are the siblings' own (their notes); each request of mine names them on its `needs:`
line.

`outline_item`/`outline_root` (db-expand-attrs' section in `v2/requests/db-expand.md`, for
attrs' `Tree::Outline`) move to the views frontend-tree-move-def-attrs asked for under their own
heading, `outline_item/outline_root onto views (2)`. That heading needs only db-expand-attrs'
section, not attrs (1): attrs (1) calls the view forms and (27) needs attrs (1), so folding them
into (27) would make the two wait on each other. (27) follows both, and its `file_outline` hunk
keeps db-expand-attrs' `may_start_with_inner_attr` check, on a view.

## My requests

| heading | file | owner (window) |
|---|---|---|
| `## frontend-tree-move-def-sigs: Node::subtree_without (crates/syntax-tree src/view.rs)` | `v2/requests/frontend-tree.md` | frontend-tree (`frontend-tree-next1`) |
| `## frontend-tree: hir-def signatures.rs: onto views (27)` | `v2/requests/db-expand.md` | db-expand-sigs |
| `## frontend-tree: hir-def signatures.rs: outline_item/outline_root onto views (2)` | `v2/requests/db-expand.md` | db-expand-sigs |
| `## frontend-tree: hir-def visibility.rs: onto views (4)` | `v2/requests/db-expand.md` | db-expand-paths |
| `## frontend-tree: hir-def find_path.rs: onto views (1)` | `v2/requests/db-expand.md` | db-expand-paths |
| `## frontend-tree: hir-def resolver.rs: onto views (1)` | `v2/requests/db-expand.md` | db-expand-names-next2 |
| `## frontend-tree: hir-def diagnostic_item/tests.rs: onto views (1)` | `v2/requests/db-expand.md` | db-expand-items |

## Speed

A signature is lowered from the item's own tree (`SignatureSyntax`), made once per item edit. On
views nothing is counted to walk it: `root()` is a borrow of the memo's `Arc<FlatTree>` (the
handle version took an `Arc` count per `root()` call and one per cursor step), `node()` is one
`descendant_at_range`, and `child_tokens` reads the kind column of the item's children once. The
outline copy is still one pass over the elements outside the bodies (`FlatTree::subtree_without`),
which is the bound: every byte of the outline is written once. The one allocation the move keeps
is the elided nodes' numbers inside `subtree_without` (as the cursors did), once per copy, beside
the copy itself.

## Proof

- `signatures.rs` tests move onto views: `child_tokens_as_the_accessors` (over the view
  accessors), the outline's text and cuts, `items_anchored_at_a_fn_are_read_around_its_body`,
  `moved_source_maps_are_the_files` (lowering from the whole file is now from `source_view`).
- New in `signatures.rs`: `walking_the_outline_allocates_nothing` (`blaze-alloc-count`):
  `bodies_under` over a file's root and `child_tokens` of every item allocate nothing.
- `visibility.rs`: `the_view_twin_is_the_handle_version` resolves every function's, const's,
  type alias's and trait's visibility of a fixture with all visibilities through both, equal for
  each item.
