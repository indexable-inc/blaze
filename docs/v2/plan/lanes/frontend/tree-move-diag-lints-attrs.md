---
status: plan
summary: frontend-tree-move-diag-lints-attrs, blaze-diagnostics' attribute lints (lints/syntax/attrs*) and unexpected_cfgs (lints/syntax/check_cfg*) moved off the syntax handles onto views, with the per-attribute reads allocation-free.
date: 2026-10-29
---
# frontend-tree-move-diag-lints-attrs

Child of [tree-move-diag-lints.md](tree-move-diag-lints.md) (item 16 of
[frontend.md](../../design/frontend.md), design in [tree.md](tree.md)). Paths:
`crates/blaze-diagnostics/src/lints/syntax/attrs.rs`, `attrs/`, `check_cfg.rs`, `check_cfg/`.
Every edit is a request in `v2/requests/diagnostics.md` (lane `diagnostics` holds the crate),
headed `## frontend-tree: blaze-diagnostics lints/syntax/<file>: onto views (1)`. Each needs
the parent's `## frontend-tree: blaze-diagnostics lints/syntax.rs: onto views (1)` (the `Walk`
and `Tree` on views) and lands with it.

Files with handle use: `attrs.rs`, `attrs/{target,parse,check_attr,doc,diagnostic,features}.rs`,
`attrs/check_attr/meta.rs`, `attrs/doc/{comments,meta}.rs`, `attrs/diagnostic/{meta,directive}.rs`
(`directive.rs` only for the lifetimes of `meta`'s types), `check_cfg/unexpected.rs`.
`check_cfg.rs` (one lifetime), `check_cfg/{meta,well_known}.rs`, `attrs/table*`,
`attrs/features/table*`, `attrs/diagnostic/format.rs` have none.

## What these files offer others (signatures after the move)

`use syntax::view as ast;` throughout. `'db` is the database borrow of `Cx<'_, 'db>`.

- `attrs::Attr<'t>`: `Copy`. `file`, `kind: AttrKind<'t>`, `inner`.
  `AttrKind<'t>::Meta { meta: ast::Meta<'t>, written: ast::Attr<'t>, unsafety:
  Option<ast::UnsafeMeta<'t>>, from_cfg_attr }`, `AttrKind::Doc(ast::DocComment<'t>)`.
  - `meta(&self) -> Option<ast::Meta<'t>>` (by value; was `Option<&ast::Meta>`).
  - `segments(&self) -> impl Iterator<Item = &'t str>` (new): the path's segments as written,
    `"doc"` for a doc comment; nothing allocated.
  - `path(&self) -> Path` (the holder's `SmallVec<[Symbol; 2]>`, interned) stays, built from
    `segments()`; `name() -> String` too; `is(name)` asks `segments()` and allocates nothing.
  - `args(&self) -> Args<'t>`; `range`, `inner_range`, `path_range`, `in_file`, `is_unsafe`,
    `is_doc_comment` unchanged.
- `attrs::Args<'t>`: `Copy`. `Word`, `List(ast::TokenTree<'t>)`, `NameValue(Option<ast::Expr<'t>>)`.
- `attrs::Owner<'db>`: `node: InFile<ast::Node<'db>>`, `target`, `attrs: Vec<Attr<'db>>`,
  `buffered_at`. `at()`, `in_expansion()` unchanged.
- `attrs::owners<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>) -> Vec<Owner<'db>>`, `parsed`,
  `lint_at`, `checked` (`&Owner<'_>`), `lints<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>)`.
- `attrs::Target::of(cx: &Cx<'_, '_>, node: InFile<ast::Node<'_>>, crate_root: HirFileId)`
  (was `InFile<&SyntaxNode>`). Callers outside: `checks/attrs/parsers/stability.rs` (807, 1224,
  1240), told to `frontend-tree-move-diag-lower`.
- `attrs::parse::{Krate::of, crate_level, applied, and_list}`: only elided lifetimes added
  (`&[Owner<'_>]`, `&Owner<'_>`, `&Attr<'_>`); `checks/attrs/accept.rs` compiles unchanged.
- `attrs::features::{Named<'a>, named}`: `Named<'a> { attr: &'a Attr<'a>, name: &'a str, range }`
  (`name` was a `String`), `named<'a>(owner: &'a Owner<'a>)`. `checks/features.rs:163`
  (`feature.name.as_str()`) becomes `feature.name`; the `==` compares there read the same.
- `check_cfg::lints<'db>(cx: &Cx<'_, 'db>, cmd, walk: &Walk<'db>)` and
  `check_cfg::unexpected::lints<'db>(cx: &Cx<'_, 'db>, check, walk: &Walk<'db>)`: the lifetimes
  tied, as the view `Semantics` takes `'db` views. Inside, `Lints<'a, 'c, 'db>` holds
  `cx: &'a Cx<'c, 'db>` (`Cx` is invariant in its first lifetime, through `OnceCell<Paths>`).

## For `frontend-tree-move-diag-lower` (`checks/attrs.rs`, `checks/attrs/*`, `checks/features*`)

What changes at its call sites when these requests land (told by tmux, 2026-10-29):

- `Owner` and `Attr` take a lifetime: write `&[Owner<'_>]`, `&Owner<'_>`, `&Attr<'_>`
  (`elided_lifetimes_in_paths` warns). `Owner.node` is `InFile<ast::Node<'db>>`, `Copy`.
- `Attr::meta()` gives `Option<ast::Meta<'t>>` by value, a view (`syntax::view`), so
  `meta.path()?.as_single_name_ref()` and `.text()` read the same; handle-only calls on it
  (`SyntaxNodePtr::new(meta.syntax())` and the like) become `ast::ptr(meta.syntax())`.
  `AttrKind::Meta { meta, written, unsafety, .. }` binds views by value.
- `Attr::args()` gives `Args<'t>` (`List(ast::TokenTree<'t>)`, `NameValue(Option<ast::Expr<'t>>)`).
- `Target::of(cx, InFile<ast::Node<'_>>, crate_root)` (`checks/attrs/parsers/stability.rs`).
- `owners<'db>(cx, walk) -> Vec<Owner<'db>>` (`checks/features/library.rs`).
- `Named.name: &'a str` (`checks/features.rs:163`).
- `parse::{Krate::of, crate_level, applied, and_list}`, `Attr::{path, name, is}`: unchanged
  but for lifetimes.

## What these files program against (lower layers)

- Parent (`lints/syntax.rs`): `Tree<'db>`'s `nodes`/`off`/`replaced`/`root` as `ast::Node<'db>`,
  `of_kind` by value, `ptr(node)` by value, `Walk::on(ast::AnyHasAttrs<'_>)`,
  `from_expansion(cx, InFile<ast::Node<'_>>)`.
- `frontend-tree-move-diag-lower` (`cx.rs`): `Cx::vsema: hir::view::Semantics<'db>` (field,
  the parent's proposal), `cx::{item_range_view, def_range_view, signature_range_view,
  derive_paths_view}`.
- `frontend-tree-move-hir`: `hir::view::Semantics::{expand_attr_macro(ast::Item<'db>) ->
  Option<ExpandResult<InFile<Node<'db>>>>, resolve_path(ast::Path<'db>) ->
  Option<PathResolution>, hir_file_to_module_defs}` and
  `Module::declaration_source_view(db) -> Option<InFile<ast::Module<'db>>>`.
- `frontend-tree-move-expand` (asked by tmux): `hir_expand::attrs::
  expand_cfg_attr_with_doc_comments_view` (the twin of `expand_cfg_attr_with_doc_comments` over
  `view::Attr`/`view::Meta`), `MacroCallLoc::to_node_view(db) -> InFile<view::Node<'db>>`,
  `AttrId::find_attr_range_view(db, krate, AstId<N>) -> (view::Attr<'db>, view::Meta<'db>)`.
- `cfg::CfgExpr::parse_from_view(view::CfgPredicate<'_>)` (in tree).

## Speed

What the handle version did per node or attribute and the view version does not:

- `owners`: an `Owner` pushed and an `FxHashMap` entry made for every attribute owner of the
  crate (every expression), then the empty ones dropped. Now only owners with an attribute, and
  only `mod` items are indexed (a module file's inner attributes go to its `mod` item).
- `parse::lints`: the holder's speed pass already made a path an interned
  `SmallVec<[Symbol; 2]>` and `is` allocation-free; the requests were rebased onto it (its
  `Item`/`Items` aliases, streamed `items`, `with_string`, interned `check_target` kept). On
  views a path is read with `Attr::segments`, the written text, and `Attr::is` compares it.
- `parse::items`: the children `Vec` gathered to strip the delimiters is gone, and no outer
  `Vec` is made for an empty list. `string` gives `Cow<str>`: borrowed unless the literal has an
  escape.
- `check_attr`: one-word paths kept as `&str`, not tokens; string values as `Cow`.
- `check_attr/meta`, `doc/meta`, `diagnostic/meta`: the list streams with a three-element
  lookahead, so the holder's gathered `SmallVec<[_; 16]>` of elements goes; segments are `&str`
  in a `SmallVec` (`doc/meta`'s were interned `Symbol`s); tokens `Copy`, not counted.
- `unexpected`: the read list's buffer and the radix sort's scratch kept across trees
  (`blaze_radix::sort_copy_by_key_in`); `CfgExpr::parse_from_view`; a macro path's text sliced
  from the source (one `String`, not two).
- Every `clone()` of a node or token is gone: a view is 24 bytes, `Copy`.

## Proof

- The UI tests in `src/tests/lints/syntax/attrs*.rs` and `check_cfg*.rs` (rustc's `.stderr`
  byte for byte) are the differential: every lint, span and order unchanged by the move.
- `attrs/doc/meta.rs` tests (moved onto views: `Parse::root()` and `view::TokenTree`): rustc's
  meta-item lists, spans and literal reads, the same expectations.
- `attrs/parse.rs` tests (unchanged): `and_list`, target groups, the builtin table.
- New in `attrs/parse.rs`: `items_are_the_lists_items` (the items of `#[x(a, b = 1, c(d),)]`,
  `#[x()]`, `#[x]`: element kinds and ranges, empty items left out) and
  `reading_an_attribute_allocates_nothing` (`blaze-alloc-count`, the crate's dev-dependency):
  `Attr::segments`, `Attr::is` and `string` of an unescaped literal allocate nothing.
- New in `check_cfg/unexpected.rs`: `reading_a_trees_cfgs_again_allocates_nothing`. For 4 and
  400 functions with `#[cfg_attr(..)]` and `#[cfg(..)]`, a second `Lints::tree` over the same
  tree (buffers warm) allocates nothing: the read of every `cfg` and `cfg_attr`, the sort and
  `CfgExpr::parse_from_view` of an atom are allocation-free per attribute, and the lint emits
  nothing for a declared name.
- New in `attrs/check_attr/meta.rs` and `attrs/diagnostic/meta.rs`: their lists read up to the
  closing delimiter, the rest refused.
