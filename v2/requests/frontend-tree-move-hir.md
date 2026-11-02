# frontend-tree-move-hir: requests made

Requests this lane wrote in other lanes' files, and the twins it calls that other children
declared. Notes: `docs/v2/plan/lanes/frontend/tree-move-hir.md`.

## In `v2/requests/db.md`

- `## frontend-tree: hir semantics.rs, source_analyzer.rs, lib.rs: onto views (1)` (applied): `pub mod
  view;`, `mod view;` and the re-exports of `view`, `FieldSourceView`, `LocalSourceView` and
  `ModuleSourceView`. Told `db-apply-ty-next1` and `db-next2`.
- `## frontend-tree: hir-ty effects.rs, hir-ty-borrowck names.rs, check/tests.rs: onto views (3)`:
  the last handle syntax in hir-ty and the checkers. Told `db-apply-ty-next1` and `db-next2`.

> applied: nothing to apply: a record of the requests frontend-tree-move-hir made in `db.md`, each with its own outcome there (requests, pass 27).

## Edited directly (items is DONE)

- `crates/hir-ty/src/method_resolution.rs`: `loaded_crates`' `named_in` walks a view root
  (`Parse::root`, `Node::flat`), the same `(tree, at)` the handle's `flat()` gave.

> forwarded to frontend-fast-method: since pass 23 it holds `crates/hir-ty/src/method_resolution.rs` (the free rule), so it keeps this edit when it next writes the file; told by tmux (requests, pass 27).

## Twins called that others declared

- frontend-tree-move-expand, in `v2/requests/db-expand.md`: `ExpansionInfo::{expanded_view,
  arg_view, map_range_down_view}`, `InFile<Node>::{ancestors_with_macros_view,
  original_file_range_*_view}`, `MacroCallLoc::to_node_view`, `expand_cfg_attr_view`,
  `find_attr_range_with_source_view`, `AstPathExt for view::Path`, `AsName` for view names.
- frontend-tree-move-def: `BodySourceMap::node_{expr,pat,type,label}_view` and
  `macro_expansion_expr_view` (`v2/requests/db.md`), `ModuleSourceView`,
  `definition_source_view`, `declaration_source_view` (`v2/requests/db-expand.md`),
  `HasSource::source_view` and `HasChildSourceView` (in the tree).
- frontend-tree-move-def-lower: `ExprCollector::lower_path_view(path, &mut ImplTraitLowering)`,
  in the tree (`expr_store/lower.rs`).
- frontend-tree-move-def-attrs: `attrs::parse_extra_crate_attrs_view`, declared in
  `frontend-deps-items.md`, not yet requested. Told `frontend-tree-move-def-attrs`.

> applied: nothing to apply: a record of which twins it calls and who declares them; the two not yet requested were told by frontend-tree-move-hir (requests, pass 27).
