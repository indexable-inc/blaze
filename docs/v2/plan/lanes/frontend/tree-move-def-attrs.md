---
status: plan
summary: frontend-tree-move-def-attrs, hir-def's attribute readers (attrs.rs, attrs/{docs,stability,tests}.rs) moved from syntax handles onto borrowed views; the one edge twin and the in-crate signatures callers in other files move to.
date: 2026-10-29
---
# frontend-tree-move-def-attrs: hir-def's attributes onto views

Part of [tree-move-def.md](tree-move-def.md). Paths: `crates/hir-def/src/attrs.rs`,
`crates/hir-def/src/attrs/**`. Owner of the files while this runs: `db-expand-attrs`; every
change is a request under `## frontend-tree: hir-def attrs…: onto views (n)` in
`v2/requests/db-expand.md`.

Attributes are read for every item, field and generic parameter, so the walk is the hot path:
it borrows views of the database's trees (`'db` from `db: &'db Db`), reads `&'db str` text, compares
names through the existing perfect-hash `name_set!` tables, and interns a `Symbol` only where the
handle version interned one. `cfg_attr` is expanded by hir-expand's view expansion. Nothing is
cloned (views are `Copy`), no `SmolStr` or `String` is built to compare a name, and `DocAtom`
lends its text (`&'t str` keys, `Cow<'t, str>` values, borrowed unless unescaping changes
them) where the handles built a `SmolStr` per atom.

## The edge twin (callers outside hir-def program against this)

- `attrs::parse_extra_crate_attrs_view(db: &Db, krate: Crate) -> Option<ast::SourceFile<'_>>`,
  beside `parse_extra_crate_attrs`: the view of the same memoized parse
  (`extra_crate_attrs(db, krate).as_ref().map(Parse::view)`). The handle version stays for
  `crates/hir/src/semantics.rs:291` (frontend-tree-move-hir) and
  `crates/blaze-diagnostics/src/checks.rs:86` (frontend-tree-move-diag-lints) until they move;
  the request that moves the last caller deletes it. hir-def's own callers
  (`item_tree.rs:244`, `item_tree/items.rs:274,297`, part items) move to the twin.

## In-crate signature changes (callers in other parts' files move to these)

- `AttrFlags::is_cfg_enabled_for<'t>(owner: impl ast::HasAttrs<'t>, cfg_options: &CfgOptions)
  -> Result<(), CfgExpr>` (`ast` = `syntax::view`), the view by value. It is one
  `ast::AnyHasAttrs::new(owner)` and calls
  `AttrFlags::is_cfg_enabled_in(owner: ast::AnyHasAttrs<'_>, cfg_options) -> Result<(), CfgExpr>`,
  which replaces the handle `is_cfg_enabled_in(attrs: ast::AttrsIncludingInner, ..)`: the cfg
  expansion is one instance for every kind of owner, as before, with no named iterator type
  (views have none) and no dispatch. Callers: `src.rs:162,179` (`fd`, parent part),
  `signatures.rs:1836,1950` (`field`, `variant`, part sigs) drop the `&`;
  `expr_store/expander.rs:90` (part lower: `Expander::is_cfg_enabled`) takes and passes
  `ast::AnyHasAttrs<'_>` (callers build it with `ast::AnyHasAttrs::new(node)`). Told by tmux,
  2026-10-29.
- The owner's `Tree` choice (`Tree::Outline` for the memoized readers, `Tree::Live` for
  `symbol_attr` and stability) stays. `item_node` returns `N::Of<'db>` and `file_root` returns
  `ast::SourceFile<'_>`, so `signatures.rs`'s `outline_item`/`outline_root` must hand out views:
  `outline_item<'db, N: AstIdNode + Typed>(db: &'db Db, id: AstId<N>) -> Option<N::Of<'db>>`,
  `outline_root(db: &Db, file: EditionedFileId) -> ast::SourceFile<'_>` (asked of
  db-expand-attrs and sigs by tmux, 2026-10-29).
- Everything else attrs.rs changes is private to `attrs` and moves with it: `attrs_source`
  returns `(InFile<ast::AnyHasAttrs<'db>>, Option<InFile<ast::Module<'db>>>,
  Option<ast::SourceFile<'db>>, Crate)`; `collect_attrs_in` takes that source by value;
  `attr_value` takes `InFile<ast::Expr<'db>>`; `for_each_field` gives
  `InFile<ast::AnyHasAttrs<'db>>`; `is_one(path: ast::Path<'_>, ..)` and
  `path_is(Option<ast::Path<'_>>, ..)` take views; `docs::extract_docs` takes
  `InFile<ast::AnyHasAttrs<'db>>`, `Option<InFile<ast::Module<'db>>>` and
  `Option<ast::Node<'db>>`; `docs::inner_docs_node(owner: ast::AnyHasAttrs<'t>) ->
  Option<ast::Node<'t>>`; `docs::expand_attr_value` takes `InFile<ast::Expr<'_>>`.
- `symbol_attr`, `crate_supports_no_std` and every `AttrFlags` query keep their signatures
  (ids in, values out).

## What attrs needs from below

- `cfg`: `CfgExpr::parse_from_view` (in the tree): the three `parse_from_ast` calls of
  `attrs.rs` (`link_cfg`, `extract_cfgs`, `is_cfg_enabled_for`) move to it.
- `src.rs` (parent part): `HasChildSourceView::child_source_view` for `GenericDefId`'s
  lifetime and type-or-const parameters (`generic_params_attr_flags`); items are viewed with
  `AstIdLoc::ast_id().to_view(db)` (hir-expand's `AstId::to_view`, in the tree).
- hir-expand (frontend-tree-move-expand; names agreed with its notes `tree-move-expand.md`):
  1. `attrs::expand_cfg_attr_view`, today's `expand_view_cfg_attr` renamed and made `pub`;
  2. `attrs::expand_cfg_attr_with_doc_comments_view<'t, 'a, DocComment, BreakValue>(attrs: impl
     Iterator<Item = Either<view::Attr<'t>, DocComment>>, cfg_options: impl FnMut() -> &'a
     CfgOptions, callback: impl FnMut(Either<(view::Meta<'t>, view::Attr<'t>), DocComment>) ->
     ControlFlow<BreakValue>) -> Option<BreakValue>` (docs.rs);
  3. `HirFileId::call_node_view(self, db: &Db) -> Option<InFile<view::Node<'_>>>`
     (`block_disabled_sanitizers`);
  4. `ModPath::from_src_view(db: &Db, path: view::Path<'_>, span_for_range: &mut (impl
     FnMut(TextRange) -> SyntaxContext + ?Sized)) -> Option<ModPath>` (docs.rs's doc macros);
  5. `InFile<view::Node<'db>>::parent_ancestors_with_macros_view(self, db: &'db Db) -> impl
     Iterator<Item = InFile<view::Node<'db>>> + 'db` (stability.rs's `block_owner`).
- span (in the tree): `AstIdMap::ast_id_of_view::<N>(N::Of<'_>)`.

## Speed

Per item, the attribute walk is bounded by its input: one visit of each attribute's tokens
(kind and text columns of the flat tree), one perfect-hash lookup of each lent name, and one
`Symbol::intern` per value a query keeps. On views nothing else is done: every node and token is
a `Copy` index into the memo's `Arc<FlatTree>`, so no count is taken per step or per `clone()`
(the handle walk took one per cursor and per clone it marked "a cursor, no allocation"), no
`SmolStr` is built per path segment or doc atom, and a string value is a buffer of its own only
when unescaping changes it. `link_cfg` keeps its one scratch parse per `#[link(cfg(..))]`, as
before. `attrs/docs.rs` makes the one exact `String` copy per item with docs it made, plus the
`String` of a `#[doc = macro!()]` value, as before; the source map and text are built in the
worker's buffers. What remains above the bound is the name-table compare (one hash and one
`memcmp` per name) and the cfg check, both what the bound counts. The proof of the allocation
side is `match_attr_flags_allocates_nothing`, beside the existing
`warm_attribute_lookups_allocate_nothing`.

## Proof

- `attrs/tests.rs`: the differential tests that compare the word index with the string match it
  replaced (`old_match_attr_flags`, `old_parse_repr_tt`, `old_parse_link_tt`, the extractors) keep
  their old side on handles and feed the new side the view of the same parse, element for
  element (`view_of`): they compare a view with its handle, so they stay until the handles go.
  Every other test reads views.
- `attrs/tests.rs` `extra_crate_attrs_view_is_the_handles`: for crates with and without
  `--crate-attr`s, `parse_extra_crate_attrs_view`'s attributes have the kinds, ranges and texts
  of `parse_extra_crate_attrs`'s, one for one.
- `attrs/tests.rs` `is_cfg_enabled_for_views_as_handles`: on a corpus of `cfg`/`cfg_attr` nests
  over fields, variants and items, the view's answer (and the `CfgExpr` it stops at) is the
  handle walk's (`parse_from_ast` over handles, kept in the test as the reference).
- An allocation budget (`blaze-alloc-count`, already a dev-dependency of hir-def):
  `match_attr_flags_allocates_nothing` walks every attribute of the corpus through
  `match_attr_flags` on views and counts zero allocations (an attribute whose string value has
  an escape is left out: undoing it is the one buffer the reader may make).

## Requests (2026-10-29, `v2/requests/db-expand.md`, owner db-expand-attrs)

- `## frontend-tree: hir-def attrs.rs: onto views (1)`;
- `## frontend-tree: hir-def attrs/docs.rs: onto views (2)`;
- `## frontend-tree: hir-def attrs/stability.rs: onto views (3)`;
- `## frontend-tree: hir-def attrs/tests.rs: onto views (4)`.

They land together: (2)-(4) call the private helpers (1) changes.

## State of the art

rustc's attribute parsing (`rustc_attr_parsing`) reads `&'hir Attribute`s, borrowed, with
interned `Symbol` names compared by index. This matches it: names are compared through a
compile-time perfect hash of the lent `&str` (one hash, one compare), the walk is a scan of the
flat tree's columns, and `Symbol`s are interned once per kept value.
