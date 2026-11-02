---
status: plan
summary: frontend-tree-move-diag-lints-names, the dead-code pass (lints/names/dead*) and lints/names.rs, effective.rs moved off the syntax handles onto views; what they call below and what each test proves.
date: 2026-10-29
---
# frontend-tree-move-diag-lints-names

Item 16 of [frontend.md](../../design/frontend.md), design in [tree.md](tree.md), parent
[tree-move-diag-lints.md](tree-move-diag-lints.md). Paths: `crates/blaze-diagnostics/src/lints/
names.rs`, `names/effective.rs`, `names/dead.rs`, `names/dead/`. Holder: `diagnostics-lints-names`
(requests in `v2/requests/diagnostics-lints-names.md`).

## Inventory

- `names.rs`: no handle use. It re-exports `items::def_span` (sibling `-items` moves it; its one
  caller outside `lints/` is `checks/attrs/parsers/stability.rs`, `-diag-lower`'s).
- `effective.rs`, `dead.rs`, `dead/{reach,res,tests}.rs`: no handle use (`effective.rs`'s one
  `clone()` copies a `Vec<hir::Module>`).
- `dead/defs.rs` (the crate walk numbering every definition), `dead/level.rs` (the level of
  `dead_code` at a node), `dead/seed.rs`, `dead/report.rs`, `dead/mark.rs` (the signature walk),
  `dead/mark/body.rs` (bodies, over the store's columns; syntax read for `offset_of!`, the
  self-assignment's expansion test and a closure's level).

## Offered to others

Nothing of these files is called outside them with a handle: `dead::lints(cx, effective)` and
`Effective` take and give ids. Inside `dead/`, `Levels::at(cx, InFile<ast::Node<'db>>)`,
`defs::{is_anon, anon_key}` take views; `defs::cast` goes (a view's `N::cast` tests the kind and
costs nothing to call with a `Copy` node).

## What this programs against

- `Cx::vsema` (`hir::view::Semantics<'db>`), `Cx::syntax_view`, `cx::{item_range_view,
  def_range_view}` (`-diag-lower`); `crate::checks::crate_attrs` on views (`-diag-lower`).
- `hir::view::Semantics`: `parse_or_expand`, `to_def`, `is_attr_macro_call`, `expand_attr_macro`,
  `expand_macro_call`, `lint_attrs`, `file_to_module_def`, `resolve_path`, `resolve_method_call`,
  `resolve_field`, `resolve_record_field`, `resolve_record_pat_field`, `resolve_offset_of_field`,
  `resolve_bind_pat_to_const`, `type_of_expr`, `type_of_pat`, `expr_adjustments`;
  `Module::{definition_source_view, declaration_source_view}`, `hir::ModuleSourceView` (`-hir`).
- `hir_expand::attrs::expand_cfg_attr_view`, `MacroCallId::call_node_view` (`-expand`).
- `from_expansion(cx, InFile<ast::Node<'_>>)` (parent, `lints/syntax.rs`).

Every node handed to `cx.vsema` is of a tree it handed out (`parse_or_expand`, its expansions,
`Cx::syntax_view`): the view `Semantics` finds a view's file by its tree. The module sources hir
gives from `HirFileId::root` are found again under `cx.vsema.parse_or_expand(file)` by their
pointer, once per module, as the handle code did. `level.rs` takes a macro call's node and a
module's declaration as hir-expand and hir give them: it only walks up from them and hands
`lint_attrs` the file itself.

`defs.rs` asks `cx.vsema.to_def(view)` where the handle code called `to_def2(InFile<&T>)` (no
`to_def2` twin is declared): one probe of the tree's address per definition, once per crate.

## Speed

The walks (`defs`' crate walk, `mark`'s signature walk, `level`'s walk up) step by arithmetic on
the flat tree's columns: no count, clone or allocation per node. `seed::string_value` lends the
`#![crate_type]` value from the node's text instead of building two `String`s.

## Proof

- `dead/tests.rs` and `src/tests/lints/names/dead*` (the UI tests, rustc's `.stderr` byte for byte):
  the same items dead, the same spans and the same order, on views as on handles.
- `names.rs`' budgets (`dead_code_allowed_everywhere_costs_nothing`,
  `item_lints_capped_cost_nothing`) are unchanged: nothing walked when nothing may fire.
