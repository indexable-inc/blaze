---
status: plan
summary: frontend-tree-move-diag-lints-items, blaze-diagnostics' item lints (lints/names/items.rs, items/) moved off the syntax handles onto views; the item walk's view API (Item<'db>, Container<'db>, enabled, attr, attrs_named, fields, def_span) every item lint reads.
date: 2026-10-29
---
# frontend-tree-move-diag-lints-items

Item 16 of [frontend.md](../../design/frontend.md), design in [tree.md](tree.md), for
`crates/blaze-diagnostics/src/lints/names/items.rs` and `items/`. Child of
[tree-move-diag-lints](tree-move-diag-lints.md). Every file is held by `diagnostics-lints-names`:
each move is a request in `v2/requests/diagnostics-lints-names.md`.

## The item walk on views (`items/walk.rs`)

The walk is what it was: the items rustc's late passes visit, module by module, each with its own
nodes. What changes is what an item holds: views of the database's trees, borrowed for `'db`
(the `Cx<'_, 'db>` borrow), instead of counted handles. Nothing is cloned to keep one, and a walk
step is a kind read and arithmetic on the flat tree's columns.

```rust
use syntax::view as ast;

#[derive(Clone, Copy, Debug)]
pub(super) enum Container<'db> {
    Module,
    Trait(InFile<ast::Trait<'db>>),
    Impl(InFile<ast::Impl<'db>>),
    Extern(InFile<ast::ExternBlock<'db>>),
}

#[derive(Debug)]
pub(super) struct Item<'db> {
    pub(super) node: InFile<ast::Item<'db>>,
    pub(super) container: Container<'db>,
    pub(super) module: hir::Module,
    pub(super) in_body: Option<usize>,
    pub(super) end: usize,
    column: Rc<[InFile<ast::Node<'db>>]>,   // the walk's one column of every item's own nodes
    own: (u32, u32),                        // this item's range in it
}

impl<'db> Item<'db> {
    pub(super) fn nodes(&self) -> &[InFile<ast::Node<'db>>];      // its own nodes, preorder
    pub(super) fn ptr(&self) -> InFile<SyntaxNodePtr>;              // an id, unchanged
    pub(super) fn syntax(&self) -> InFile<ast::Node<'db>>;
    pub(super) fn own<N: ast::AstNode<'db>>(&self) -> impl Iterator<Item = InFile<N>> + '_;
    pub(super) fn held<'i>(&self, items: &'i [Item<'db>], index: usize) -> &'i [Item<'db>];
}

pub(super) fn fields<'db>(item: &Item<'db>) -> SmallVec<[InFile<ast::Node<'db>>; 8]>; // sorted, as before
pub(super) fn items<'db>(cx: &Cx<'_, 'db>) -> Vec<Item<'db>>;
pub(super) fn enabled<'t>(cx: &Cx<'_, '_>, owner: impl ast::HasAttrs<'t>) -> bool;
pub(super) fn attrs_named<'t>(cx: &Cx<'_, '_>, owner: impl ast::HasAttrs<'t>, name: &str)
    -> Vec<(ast::Meta<'t>, TextRange)>;
pub(super) fn attr<'t>(cx: &Cx<'_, '_>, owner: impl ast::HasAttrs<'t>, name: &str)
    -> Option<(ast::Meta<'t>, TextRange)>;
```

- Callers pass owners by value (`enabled(cx, it)`, `attr(cx, f, "no_mangle")`), never `&it`.
- `Container` is `Copy`: `match item.container { Container::Impl(imp) => imp.value.self_ty() }`.
  Two containers compare as views do (the file tree's address and the element number).
- `attr` stops at the first match (a `Break` out of the `cfg_attr` expansion): no `Vec`.
- `items.rs`' `Walk` is `Walk<'db> { items: Vec<Item<'db>> }`, made by `Walk::of(cx: &Cx<'_, 'db>)`;
  `Cx::items(&self) -> &ItemWalk<'db>` (cx.rs, request below).
- `def_span(node: ast::Node<'_>) -> TextRange` (`items.rs`, `pub(crate)`, re-exported as
  `crate::lints::def_span`): the node by value. Its callers outside these paths are
  `checks/attrs/parsers/stability.rs` and `lower/const_pattern_refused.rs`
  (`frontend-tree-move-diag-lower`'s, told 2026-10-29); they land with the `items.rs` heading.
- `externs::abi_of(Option<ast::Abi<'_>>, ExternAbi)`: the ABI's view by value (`c_void.rs` calls
  it).
- One `'db` per function: the view `Semantics<'db>` is invariant and takes `'db` views only, so a
  function taking a `Cx` with an `Item`, a `Walk`, an item slice or a view that reaches
  `cx.vsema` names `'db` for both (`cx: &Cx<'_, 'db>, item: &Item<'db>`). The attribute helpers
  (`enabled`, `attr`, `any_named`) never reach it and take any `'t`.

- `walk::named(cx, owner, name, f)` is `pub(super)`: each attribute named `name`, as
  `attrs_named` gives them, until `f` breaks.

Every root the walk and the lints read comes from the view `Semantics` (`cx.vsema`, roots by
`cx.root(file)`) or a source it hands out: `hir::view::Semantics` finds a view's file again from
its tree's address and panics on a tree it never handed out, so a node reached any other way is
not given to it.

## What this programs against

- `frontend-tree-move-diag-lower`'s `cx.rs` heading: `Cx::vsema: hir::view::Semantics<'db>`,
  `Cx::root(file)`, `Cx::node_view`/`Cx::syntax_view`, `cx::{item_range,def_range,signature_range}_view`.
  Mine: `Cx::items() -> &ItemWalk<'db>`.
- `hir::view::Semantics` (tree-move-hir.md): `parse_or_expand`, `to_def2(InFile<T>)` (the twin of
  `to_def2(InFile<&T>)`), `is_attr_macro_call(InFile<ast::Item<'db>>)`,
  `expand_attr_macro(ast::Item<'db>)`, `resolve_path`, `resolve_type`,
  `resolve_bind_pat_to_const`, `ancestors_with_macros_file`, `source`.
- `hir::Module::definition_source_view(db) -> InFile<hir::ModuleSourceView<'db>>`.
- `hir_expand::attrs::expand_cfg_attr_view` (tree-move-expand.md), `CfgExpr::parse_from_view`.

## Proof

- The item lints' UI tests (`src/tests/`, rustc's `.stderr` byte for byte) are the differential:
  the walk gives the same items, in the same order, with the same own nodes, so every lint gives
  the same diagnostics in the same order.
- `walk.rs`' test `the_view_walk_is_the_handle_walk` is not possible without the handle walk
  beside it, and the handle walk is gone: what it would check (same items, order, containers,
  own nodes by kind and range) is what the UI tests over every lint fixture check.
- `lifetimes.rs`' unit tests, now on a `Parse`'s root view: `deletion_spans_take_a_comma` (the
  parameter spans and the comma each deletion takes, from the views' ranges) and
  `uses_skip_labels_declarations_and_nested_items` (the scope's preorder with tokens over views
  finds the same lifetime tokens, in order, skipping labels, declarations and nested items).
