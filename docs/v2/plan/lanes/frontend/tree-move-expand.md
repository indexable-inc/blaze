---
status: plan
summary: frontend-tree-move-expand, the view twins of hir-expand's handle APIs (the bottom layer every other frontend-tree-move child programs against), the move of hir-expand's and base-db's own bodies onto views, and which handle functions go once their callers move.
date: 2026-10-29
---
# frontend-tree-move-expand: hir-expand's and base-db's twins

Part of [tree.md](tree.md) (frontend.md item 16). This crate is the bottom layer. Every
`frontend-tree-move-*` child above it (def, hir, codegen, diag-lower, diag-lints, tools) moves its
callers onto the twins declared here, by these names and signatures. A twin sits beside its handle
function and adds to it. It has the handle function's meaning exactly: the same nodes, ranges and
order, and `None` in the same cases. It returns views borrowed from the database (`'db` from
`db: &'db Db`). Requests for `hir-expand` go to `v2/requests/db-expand.md` and for `base-db` to
`v2/requests/db.md`, under headings `## frontend-tree: hir-expand <file>: onto views (<n>)`.

Callers write `use syntax::view as ast;`. Below, `view::X<'db>` is the view type and `ast::X` the
handle type, used as a kind tag only (`AstId<ast::Fn>`, `AstPtr<ast::Fn>`).

## Already in the tree

- `HirFileId::root(self, db: &Db) -> view::Node<'_>` (twin of `parse_or_expand`).
- `AstId<N>::to_view<'db>(&self, db: &'db Db) -> N::Of<'db>` (twin of `AstId::to_node`;
  `N: AstNode + Typed`).
- `EditionedFileId::parse(db).root()` / `.view()` (base-db's `parse` returns the `Parse`, a kind
  tag; no twin needed).
- `AstIdMap::from_view`, `ast_id_of_view::<N>(view)`, `ast_id_for_block_view` (span).
- `syntax_bridge::call_input(node: view::Node, ..)` and `call_input_modified(.., NodeId sets ..)`
  (frontend-tokens). The build path through `cfg_process.rs` and `fixup.rs` already runs on views.
  The handle forms (`attr_macro_input_to_token_tree_handle`, `fixup_syntax_handle`,
  `macro_input_callback_handle`) serve `MacroCallId::expand_speculative` only.

## The twins (request 1 to 5 add them)

### `files.rs` (request 1)

| Handle API | Twin |
|---|---|
| `InFileWrapper<F, T>::file_syntax(&self, db) -> SyntaxNode` | `InFileWrapper<F, T>::root<'db>(&self, db: &'db Db) -> view::Node<'db>` (any `F` of `EditionedFileId`, `MacroCallId`, `HirFileId`) |
| `InFileWrapper<F, AstPtr<N>>::to_node(&self, db) -> N` | `InFileWrapper<F, AstPtr<N>>::to_view<'db>(&self, db: &'db Db) -> N::Of<'db>` (`N: AstNode + Typed`) |
| `AstId<N>::to_in_file_node(&self, db) -> InFile<N>` | `AstId<N>::to_in_file_view<'db>(&self, db: &'db Db) -> InFile<N::Of<'db>>` |
| `InFileWrapper<F, N: AstNode>::syntax(&self) -> InFileWrapper<F, &SyntaxNode>` (and `syntax_ref`) | `InFileWrapper<F, N: view::AstNode<'t>>::syntax_view(&self) -> InFileWrapper<F, view::Node<'t>>` |
| `InFileWrapper<F, N: AstNode>::node_file_range(&self)` | `InFileWrapper<F, N: view::AstNode<'t>>::node_file_range_view(&self) -> FileRangeWrapper<F>` |
| `InFile<SN>::ancestors_with_macros(self, db)` | `InFile<view::Node<'db>>::ancestors_with_macros_view(self, db: &'db Db) -> impl Iterator<Item = InFile<view::Node<'db>>> + 'db` |
| `InFile<SN>::parent_ancestors_with_macros(self, db)` | `InFile<view::Node<'db>>::parent_ancestors_with_macros_view(self, db: &'db Db) -> impl Iterator<Item = InFile<view::Node<'db>>> + 'db` |
| `InFile<SN>::original_file_range_rooted(self, db) -> FileRange` | `InFile<view::Node<'_>>::original_file_range_rooted_view(self, db) -> FileRange` |
| `InFile<SN>::original_file_range_with_macro_call_input(self, db)` | `InFile<view::Node<'_>>::original_file_range_with_macro_call_input_view(self, db) -> FileRange` |
| `InFile<SN>::original_syntax_node_rooted(self, db)` | `InFile<view::Node<'db>>::original_syntax_node_rooted_view(self, db: &'db Db) -> Option<InRealFile<view::Node<'db>>>` |
| `InFile<&SyntaxNode>::original_file_range_opt(self, db)` | `InFile<view::Node<'_>>::original_file_range_opt_view(self, db) -> Option<(FileRange, SyntaxContext)>` |
| `InFile<N: AstNode>::original_ast_node_rooted(self, db)` | `InFile<N: view::AstNode<'db>>::original_ast_node_rooted_view(self, db: &'db Db) -> Option<InRealFile<N>>` |
| `InFile<SyntaxToken>::original_file_range(self, db) -> FileRange` | `InFile<view::Token<'_>>::original_file_range(self, db) -> FileRange` (same name: the handle impl is on a concrete type, no overlap) |
| `InFile<SyntaxToken>::original_file_range_opt(self, db)` | `InFile<view::Token<'_>>::original_file_range_opt(self, db) -> Option<FileRange>` |

`InFile<SN>::kind` and `text_range` get no twin: read `it.value.kind()` and `it.value.text_range()`.
On `InFile<view::Node>`, every method is named `_view`, because a generic
`impl<SN: Borrow<SyntaxNode>> InFile<SN>` can overlap with `InFile<view::Node<'t>>` in
coherence. For the same reason the generic `N: view::AstNode<'t>` methods are `_view`.

### `lib.rs` (request 2)

| Handle API | Twin |
|---|---|
| `MacroCallId::call_node(self, db) -> InFile<SyntaxNode>` | `MacroCallId::call_node_view(self, db: &Db) -> InFile<view::Node<'_>>` |
| `HirFileId::call_node(self, db) -> Option<InFile<SyntaxNode>>` | `HirFileId::call_node_view(self, db: &Db) -> Option<InFile<view::Node<'_>>>` |
| `MacroCallLoc::to_node(&self, db) -> InFile<SyntaxNode>` | `MacroCallLoc::to_node_view<'db>(&self, db: &'db Db) -> InFile<view::Node<'db>>` |
| `MacroCallLoc::to_node_item(&self, db) -> InFile<ast::Item>` | `MacroCallLoc::to_node_item_view<'db>(&self, db: &'db Db) -> InFile<view::Item<'db>>` |
| `ExpansionInfo::expanded(&self) -> InMacroFile<SyntaxNode>` | `ExpansionInfo<'db>::expanded_view(&self) -> InMacroFile<view::Node<'db>>` |
| `ExpansionInfo::arg(&self) -> InFile<Option<&SyntaxNode>>` | `ExpansionInfo<'db>::arg_view(&self) -> InFile<Option<view::Node<'db>>>` |
| `ExpansionInfo::map_range_down(&self, span)` | `ExpansionInfo<'db>::map_range_down_view(&self, span: Span) -> Option<InMacroFile<impl Iterator<Item = (view::Token<'db>, SyntaxContext)> + '_>>` |
| `ExpansionInfo::map_range_down_exact(&self, span)` | `ExpansionInfo<'db>::map_range_down_exact_view(&self, span: Span) -> Option<InMacroFile<impl Iterator<Item = (view::Token<'db>, SyntaxContext)> + '_>>` |
| `ExpandTo::of_call_site(call: &ast::MacroCall)` | `ExpandTo::of_call_site_view(call: view::MacroCall<'_>) -> Result<ExpandTo, SyntaxKind>` |
| `HirFileId::parse_or_expand` | `HirFileId::root` (in the tree) |
| `HirFileId::original_call_node(self, db) -> Option<InRealFile<SyntaxNode>>` | `HirFileId::original_call_node_view(self, db: &Db) -> Option<InRealFile<view::Node<'_>>>` (request 9; for `blaze-lints/src/check.rs`) |

`ExpansionInfo` keeps its handle fields while the handle methods have callers, and gains
`expanded_root: view::Node<'db>` and `arg_root: Option<view::Node<'db>>`. Its range methods
(`span_for_offset`, `map_node_range_up`, `map_range_up_once`) read the views.

### `attrs.rs` (request 3)

| Handle API | Twin |
|---|---|
| `impl AstPathExt for ast::Path` / `Option<ast::Path>` (`is1`, `as_one_segment`, `as_up_to_two_segment`) | `impl AstPathExt for view::Path<'_>` / `Option<view::Path<'_>>`, same methods |
| `impl AstKeyValueMetaExt for ast::KeyValueMeta` (`value_string`) | `impl AstKeyValueMetaExt for view::KeyValueMeta<'_>` |
| `expand_cfg_attr(attrs: impl Iterator<Item = ast::Attr>, cfg, cb: FnMut(ast::Meta, ast::Attr))` | `expand_cfg_attr_view<'t, 'a, B>(attrs: impl Iterator<Item = view::Attr<'t>>, cfg: impl FnMut() -> &'a CfgOptions, cb: impl FnMut(view::Meta<'t>, view::Attr<'t>) -> ControlFlow<B>) -> Option<B>` (today's crate-private `expand_view_cfg_attr`, renamed and made public) |
| `expand_cfg_attr_with_doc_comments(..)` | `expand_cfg_attr_with_doc_comments_view<'t, 'a, D, B>(attrs: impl Iterator<Item = Either<view::Attr<'t>, D>>, cfg, cb: impl FnMut(Either<(view::Meta<'t>, view::Attr<'t>), D>) -> ControlFlow<B>) -> Option<B>` |
| `collect_item_tree_attrs(attrs: ast::AttrsIncludingInner, cfg, on_attr)` | `collect_item_tree_attrs_view<'t, 'a, B>(owner: view::AnyHasAttrs<'t>, cfg: impl Fn() -> &'a CfgOptions, on_attr: impl FnMut(view::Meta<'t>, view::Attr<'t>) -> ControlFlow<B>) -> Option<Either<B, CfgExpr>>` (one concrete type: no `dyn` and no copy per node type; callers pass `view::AnyHasAttrs::new(node)`) |
| `AttrId::find_attr_range<N>(self, db, krate, owner: AstId<N>) -> (ast::Attr, ast::Meta)` | `AttrId::find_attr_range_view<'db, N>(self, db: &'db Db, krate, owner: AstId<N>) -> (view::Attr<'db>, view::Meta<'db>)` (`N: AstNode + Typed`, `N::Of<'db>: view::HasAttrs<'db>`) |
| `AttrId::find_attr_range_with_source(self, db, krate, owner: &impl ast::HasAttrs)` | `AttrId::find_attr_range_with_source_view<'t>(self, db, krate, owner: view::AnyHasAttrs<'t>) -> (view::Attr<'t>, view::Meta<'t>)` |
| `AttrId::find_derive_range` | Its signature has no handle. Its body moves to views (request 6). |

### `name.rs`, `mod_path.rs` (request 4)

| Handle API | Twin |
|---|---|
| `impl AsName for ast::NameRef` / `ast::Name` / `ast::NameOrNameRef` / `ast::FieldKind` | `impl AsName for view::NameRef<'_>` / `view::Name<'_>` / `view::NameOrNameRef<'_>` / `view::FieldKind<'_>` (same `as_name`) |
| `ModPath::from_src(db, path: ast::Path, span_for_range)` | `ModPath::from_src_view(db, path: view::Path<'_>, span_for_range) -> Option<ModPath>` |
| `ModPath::from_tokens(..)` | none: nothing calls it, so request 4 deletes it |

### `eager.rs` (request 5)

| Handle API | Twin |
|---|---|
| `expand_eager_macro_input(db, krate, macro_call: &ast::MacroCall, ast_id, def, call_site, depth, limit, resolver, eager_calls)` | `expand_eager_macro_input_view(db, krate, macro_call: view::MacroCall<'_>, ast_id, def, call_site, depth, limit, resolver, eager_calls)`, the same otherwise |

The nested expansion moves to views too (request 5). `SyntaxEditor`'s replacement of each nested
call becomes one `FlatTreeBuilder` pass. The builder copies the input and puts each nested
expansion where its call was. A call that failed stays as written. Then `call_input` runs over the
built tree's root, with the same `InOrder` span map and the same offsets. The resulting tree has the
same tokens and text as the edited one, so it gives the same token tree.

### No twin

- `MacroCallId::expand_speculative` (and its handle helpers in `cfg_process.rs`, `fixup.rs`): the
  only caller is `crates/hir/src/semantics.rs`'s speculative expansion, which is IDE-only. These
  stay until `ide*`/`hir-ide` are deleted.
- `prettify_macro_expansion` (`SyntaxNode -> SyntaxNode`): the only caller is
  `crates/hir/src/lib.rs`, which displays an expansion. It stays with that caller.
- `InFile<SN>::file_range`: no build-path caller; it goes with the handles.

### From `syntax` (in the tree: `crates/syntax/src/view.rs`)

- `impl<L: Typed, R: Typed> Typed for Either<L, R> { type Of<'t> = Either<L::Of<'t>, R::Of<'t>>; }`:
  source maps hold `AstPtr<Either<ast::X, ast::Y>>` (21 build-path uses), and `to_view` needs it.
- `impl Typed for ast::Macro { type Of<'t> = view::Macro<'t>; }`: `AstId<ast::Macro>` (macro
  definitions) is hand-written, not generated, so it has none.

## Bodies moved (request 6 on), and base-db

- `lib.rs`: `macro_arg`'s attribute arm, `proc_macro_span`, `original_call_range`,
  `ast_id_map` (`AstIdMap::from_view(self.root(db))`; `from_source` already builds through
  `from_view`), `ExpansionInfo::new`. `parse_with_map` loses its last caller and goes.
- `declarative.rs` (`decl_macro_expander`), `span_map.rs` (`real_span_map`), `builtin/fn_macro.rs`
  (`written_call`, `call_span`, `written_range`, `write_assert_condition`, `include_expand`),
  `attrs.rs` (`find_derive_range`), `budget_tests.rs`.
- `builtin/derive_macro.rs` and `derive_macro/`: this lane does them too (no room for a child
  window). The `CoercePointee` path edits the item with `SyntaxEditor`/`make`, and becomes a
  direct token-tree build, checked against the edit by a differential test.
- base-db: `reparse.rs` `assert_whole` compares `root().file()` (no `Arc` clone); `tests.rs` reads
  `parse.root()`. base-db has no public handle API: `EditionedFileId::parse` returns the `Parse`, a
  kind tag.

## Proof

`crates/hir-expand/src/view_tests.rs` (new, declared by request 6) puts files in a `Db` and
compares each twin with its handle function element by element, on real files and on macro files.
For a macro file it builds a `MacroCallLoc` of a built-in fn-like macro by hand. The compared
results:

- `root`/`file_syntax` and `to_view`/`to_node` over every node: kind and range.
- `to_in_file_view`, `syntax_view`, `node_file_range_view`.
- The `ancestors_with_macros*` sequences, from every node of the expansion: file and range of each
  ancestor.
- `original_*_view`: the same `FileRange`s and `None`s, from every node.
- For tokens, `original_file_range{,_opt}`, from every token.
- `MacroCallLoc::to_node_view`, `to_node_item_view`, `call_node_view`, `original_call_node_view`.
- `ExpansionInfo`'s `expanded_view`, `arg_view` and `map_range_down{,_exact}_view`, from every
  span of the argument.
- `of_call_site_view` for every call in a fixture that puts calls in every position.
- `AsName` and `from_src_view` on every path and name.
- `expand_cfg_attr*_view`, `collect_item_tree_attrs_view` and `find_attr_range*_view` on the
  `cfg_attr` nests of `attrs.rs`' `ITEM`.
- `expand_eager_macro_input_view` against `expand_eager_macro_input`: the same token tree, span
  map and `eager_calls`.

An allocation budget (`blaze-alloc-count`) holds the hot twins (`to_view`, `root`, the
`ancestors_with_macros_view` walk and `expand_cfg_attr_view` with no `cfg_attr`) to zero
allocations, warm.

## Speed

A twin makes no handle. A step is arithmetic on the flat tree's arrays (tree.md). `to_view` costs
the `AstIdMap` lookup and one `descendant_at_range`, where `to_node` cost those plus a counted
cursor and its clones. Walking up out of a macro file reads `to_node_item_view`, a view, with no
count. The eager input is built in one preorder pass, where `SyntaxEditor` made a mutable copy of
the tree and then a green tree. rustc's own walks are borrowed references into an arena, and the
views match them (tree.md, "State of the art").

## Deleted

Request 9 deletes the handle functions no crate calls: `InFileWrapper::node_file_range`,
`InMacroFile<SyntaxToken>::upmap_once`, `ExpansionInfo::map_range_down_exact` and
`HirFileId::as_builtin_derive_attr_node`. Request 4 deletes `ModPath::from_tokens`, and request 6
deletes `HirFileId::parse_with_map`. `view_tests.rs` compares the remaining twins
(`node_file_range_view`, `map_range_down_exact_view`) with the deleted handles' bodies written out.

Every other handle function stays while it has callers. Those callers are in the crates of the
children above (hir-def, hir, blaze-diagnostics, blaze-engine), in `ide*`/`hir-ide` (which go at
integration: `original_file_range_with_macro_call_input` and `original_call_node` are left only
for `ide-db/src/search.rs`), or in hir-expand's IDE-only `expand_speculative`. Once those callers
move, `frontend-tree` deletes the layer.

`builtin/derive_macro.rs` (the derive input read as `ast::Adt`, and `CoercePointee`'s
`SyntaxEditor` edit) is moved by the child `frontend-tree-move-expand-derive`, with notes
[tree-move-expand-derive.md](tree-move-expand-derive.md).
