---
status: plan
summary: frontend-tree-move-diag-lints, blaze-diagnostics' lints (lints.rs, lints/) moved off the syntax handles onto views, with the early-pass Walk one preorder walk over views that allocates nothing per node.
date: 2026-10-29
---
# frontend-tree-move-diag-lints

Item 16 of [frontend.md](../../design/frontend.md), design in [tree.md](tree.md), for
`crates/blaze-diagnostics/src/lints.rs`, `src/lints/` and `src/lib.rs` (which has no handle
use). Every file there has a live holder (lane `diagnostics` and its children), so every move is
a request in the holder's request file.

## The walk on views (`lints/syntax.rs`)

`Walk<'db>` stays what rustc's early lint pass walks: each file of the crate's modules and each
expansion in them, in the order the walk comes to them, minus what `cfg` removes and the items
attribute macros replace. Its trees are views of the trees the database keeps
(`HirFileId::root(db)`), borrowed for `'db`, the borrow `Cx<'_, 'db>` already holds.

- `Tree<'db>`: `file: HirFileId`, `root: ast::Node<'db>`, `call: Option<MacroCallId>`,
  `nodes: Vec<ast::Node<'db>>`, `kinds: Vec<SyntaxKind>`, `attrs: Vec<bool>`,
  `off: Vec<ast::Node<'db>>`, `replaced: Vec<ast::Node<'db>>` (with `use syntax::view as ast;`).
  A node is a 24-byte `Copy` view: no count, nothing dropped, nothing cloned to keep one.
- `Tree::of_kind(&self, wanted) -> impl Iterator<Item = ast::Node<'db>> + '_` and
  `Tree::with_attrs(..)` the same, by value. `Tree::ptr(&self, node: ast::Node<'_>) ->
  InFile<SyntaxNodePtr>`. `in_file`, `skipped` unchanged.
- `Walk::trees: Vec<Tree<'db>>`, `Walk::tree(file) -> Option<&Tree<'db>>`, `Walk::files()`.
- `Walk::on(&self, owner: ast::AnyHasAttrs<'_>) -> bool`.
- `Walk::attrs<'t>(&self, owner: ast::AnyHasAttrs<'t>) -> impl Iterator<Item = (ast::Meta<'t>,
  ast::Attr<'t>)>`: the attributes `cfg_attr` leaves, outer then inner, in order, each with the
  attribute it is written in. An iterator, not a `Vec`: `.iter().any(..)` becomes `.any(..)`,
  `.into_iter().find(..)` becomes `.find(..)`.
- `from_expansion(cx, node: InFile<ast::Node<'_>>) -> bool` (was `InFile<&SyntaxNode>`).
- `Emitted`, `Pass`, `source_fact`, `Walk::place` keep their shapes (they take
  `InFile<SyntaxNodePtr>`, an id).

`Walk::add` is one `preorder()` over the view of each tree: each step is arithmetic on the flat
tree's columns. Per node it pushes three column entries into columns sized once by `node_count`
(a scan of the flat tree's token flags, unparsed lazy parts estimated from their length), so
they never grow; it makes no handle, no count and no clone. Casts are by kind (`cast` checks the
kind first), so a node that is no attribute owner, item, macro call, `mod` or ADT costs a kind
read and three pushes.

`Walk::block_module` (added by `diagnostics-speed-lints`) gives the walk the file of an
out-of-line `mod` declared in a block, on views: `cx.vsema.to_def2`, then
`Module::definition_source_view`, the root taken through `cx.root` so `cx.vsema` knows the tree.

Since a `Tree` is read by `checks/` too (`checks/macros.rs`, `checks/ast_validation.rs`,
`checks/features/attributes.rs`, `checks/features/gates/{pre,post}.rs`, sibling
`frontend-tree-move-diag-lower`), the `Tree` change lands together with every consumer's move:
each request heading of a consumer says it needs `## frontend-tree: blaze-diagnostics
lints/syntax.rs: onto views`.

## What this programs against (lower layers)

Each owner's notes are the authority; a heading here is corrected when an owner's notes change.

- `frontend-tree-move-hir` ([tree-move-hir.md](tree-move-hir.md)): `hir::view::Semantics<'db>`,
  a second `Semantics` whose methods have the handle names and take views by value (`to_def(T)`,
  `resolve_path(ast::Path<'db>)`, `type_of_expr(ast::Expr<'db>)`, `expand_macro_call`,
  `expand_attr_macro`, `expand_derive_macro`, `scope(Node<'db>)`, `hir_file_for(Node<'db>)`,
  `ancestors_with_macros_file`, `parse_or_expand(HirFileId) -> Node<'db>`, `source`, ...). It
  finds a view's file by its tree's address, so every tree a lint asks it about must be one it
  handed out: the `Walk` takes each root from it (`parse_or_expand`), never from
  `HirFileId::root(db)` directly. `HasSourceView::source_view(db)` for the item sources.
  Asked: keep `expand_derive_macro`/`resolve_derive_macro` taking `ast::Meta<'db>` as the handle
  versions do.
- `frontend-tree-move-diag-lower` ([tree-move-diag-lower.md](tree-move-diag-lower.md), `cx.rs`):
  `Cx::syntax_view(InFile<SyntaxNodePtr>) -> InFile<ast::Node<'db>>`,
  `Cx::node_view(InFile<AstPtr<N>>) -> InFile<N::Of<'db>>`, `Cx::root(HirFileId) ->
  ast::Node<'db>`, `item_range_view`, `signature_range_view`, `def_range_view`,
  `derive_paths_view`. Proposed to it: `Cx::vsema: hir::view::Semantics<'db>` (pub field, made in
  `Cx::new`), what lints call the view `Semantics` through (`cx.vsema.to_def(it)`).
  `Cx::syntax_view` must give a node of a tree `cx.vsema` handed out (asked of it).
- `frontend-tree-move-expand` (`hir-expand`): `expand_view_cfg_attr` made public as
  `hir_expand::attrs::expand_cfg_attr_view` (its request (3) in `v2/requests/db-expand.md`).
  The iterator form `cfg_attrs_view` (below) is this lane's request (8) in the same file. The
  walk out of an expansion goes through `cx.vsema.find_parent_file`, so no `MacroCallLoc` twin
  is called here.
- `frontend-tree-move-def` ([tree-move-def.md](tree-move-def.md)): `HasSource::source_view`,
  `HasChildSourceView`, `ExpressionStoreSourceMap::node_*_view`.

### `cfg_attrs_view` (requested: `v2/requests/db-expand.md`, heading (8); applied)

`Walk::attrs` is an iterator, and the `cfg_attr` rule stays in hir-expand, so hir-expand gets
the iterator form of `expand_cfg_attr_view` for a caller that has the `CfgOptions` in hand, in
`crates/hir-expand/src/attrs.rs` (it needs nothing else):

```rust
/// [`expand_view_cfg_attr`] as an iterator, for a caller with the cfg in hand: each attribute
/// `cfg_attr` leaves, in order, with the outermost [`view::Attr`] it is in.
pub fn cfg_attrs_view<'t, 'a, I: Iterator<Item = view::Attr<'t>>>(
    attrs: I,
    cfg_options: &'a CfgOptions,
) -> CfgAttrsView<'t, 'a, I> {
    CfgAttrsView { attrs, cfg_options, stack: SmallVec::new() }
}

/// What [`cfg_attrs_view`] gives.
pub struct CfgAttrsView<'t, 'a, I> {
    attrs: I,
    cfg_options: &'a CfgOptions,
    // alloc: none until a `cfg_attr` holds more than one attribute
    stack: SmallVec<[(view::Meta<'t>, view::Attr<'t>); 1]>,
}

impl<'t, I: Iterator<Item = view::Attr<'t>>> Iterator for CfgAttrsView<'t, '_, I> {
    type Item = (view::Meta<'t>, view::Attr<'t>);

    fn next(&mut self) -> Option<Self::Item> {
        'process_attrs: loop {
            let (mut meta, top_attr) = match self.stack.pop() {
                Some(it) => it,
                None => {
                    let attr = self.attrs.next()?;
                    let Some(meta) = attr.meta() else { continue };
                    (meta, attr)
                }
            };
            while let view::Meta::UnsafeMeta(unsafe_meta) = meta {
                let Some(inner) = unsafe_meta.meta() else { continue 'process_attrs };
                meta = inner;
            }
            let view::Meta::CfgAttrMeta(meta) = meta else { return Some((meta, top_attr)) };
            let Some(cfg_predicate) = meta.cfg_predicate() else { continue };
            if self.cfg_options.check(&CfgExpr::parse_from_view(cfg_predicate)) != Some(false) {
                let prev_stack_len = self.stack.len();
                self.stack.extend(meta.metas().map(|meta| (meta, top_attr)));
                self.stack[prev_stack_len..].reverse();
            }
        }
    }
}
```

The same metas in the same order as `expand_view_cfg_attr` and `expand_cfg_attr`; a test beside
it compares `cfg_attrs_view` with `expand_cfg_attr` over the handles on nested `cfg_attr`s,
`unsafe(..)`, true and false predicates.

## Proof

- `lints/syntax.rs` tests, on views: what `cfg` removes is not walked; a module file and an
  expansion are walked where they are declared and called; the kind and attribute columns match
  the nodes.
- Budget (`blaze-alloc-count`, the crate's dev-dependency):
  `walking_a_tree_allocates_only_its_columns` walks the file tree of a quiet crate of 4 and of
  400 functions again and checks it makes at most four allocations (the three columns, sized
  once by `node_count`, and the stack) at both sizes: nothing per node; and that the walk gives
  the same nodes as the first.
- `walk_matches_the_handle_walk`: on a file with `cfg`, nested `cfg_attr`, inner `#![cfg]`,
  fields, statements and arms turned off, the views' walk gives the same nodes (kind and range)
  in the same order and the same `off` ranges as a preorder over the handles of the same file
  with the handle `expand_cfg_attr` (kept while the handles exist).

## Requests

- `v2/requests/diagnostics.md`: `## frontend-tree: blaze-diagnostics lints/syntax.rs: onto views
  (1)` (the walk) and `## frontend-tree: blaze-diagnostics lints.rs: onto views (2)`
  (`body_item`): both applied by this lane under diagnostics' grant.
- `v2/requests/db-expand.md` (8): `cfg_attrs_view`, which `Walk::attrs` calls; applied by
  db-expand-views (`hir-expand/src/attrs.rs`).
- `v2/requests/diagnostics-infer2-methods.md` (1): `lints/bodies/unstable_names.rs`, a file that
  lane wrote after the split; applied by its author.
- The children's headings: their notes, `tree-move-diag-lints-<part>.md`. The 44 the `-bodies`
  and `-early` windows left were applied by this lane, merged onto today's files and marked per
  heading. The 6 imports headings (`-uses`'s, whose window had ended) were applied by this lane
  once hir-def `use_tree_sources_view` (`v2/requests/db-expand.md` src.rs (2)) was in.
