---
status: plan
summary: frontend-tree-move-tools, the tool crates (blaze-lints, blaze-doc, blaze-fmt, blaze-intent, blaze-wasm, syntax-bridge, mbe, cfg, const-ast, blaze-cargo's state.rs) moved from syntax handles onto borrowed views; the twins they need from hir and hir-def, and what each test proves.
date: 2026-10-29
---
# frontend-tree-move-tools: the tool crates onto views

Part of [tree.md](tree.md) (frontend.md item 16). Children: `-fmt` (blaze-fmt, notes
[tree-move-tools-fmt.md](tree-move-tools-fmt.md)), `-intent` (blaze-intent,
[tree-move-tools-intent.md](tree-move-tools-intent.md)), `-doc` (blaze-doc,
[tree-move-tools-doc.md](tree-move-tools-doc.md)). The rest is this part's.

## Twins added here

None in the tool crates themselves: `syntax-bridge` already has the view entry points
(`call_input`, `call_input_modified` keyed by `NodeId`), and `cfg` has
`CfgExpr::parse_from_view`. The handle functions they twin are deleted once their last caller
outside these crates moves (list below).

## What the tools use of frontend-tree-move-hir

`hir::view::Semantics` and `hir::view::HasSourceView`, as declared in
[tree-move-hir.md](tree-move-hir.md): the handle methods' names over views. A view passed to the
view `Semantics` must be of a tree it handed out, so callers holding views of the database's
trees from elsewhere (blaze-lints' `Cx`) find the node again in `parse_or_expand(file)` by range
and kind first, as the handle path did.

| Used | By |
|---|---|
| `view::Semantics::{new, hir_file_for, parse_or_expand, ancestors_with_macros, source, expand_macro_call}`, `HasSourceView::source_view` | blaze-wasm `names.rs` |
| `view::Semantics::{resolve_path, resolve_type, resolve_macro_call, scope}` | blaze-lints `check.rs` (`Cx::path`, `resolve_type`, `macro_def`, `resolve_name`) |
| `hir::Module::definition_source_range` (exists) | blaze-wasm `names.rs` `scope_path`: no `ModuleSource` twin needed |

Requested of lane db (`v2/requests/db.md`, `## frontend-tree: hir src/semantics.rs: SemanticsScope::speculative_resolve_mod_path`; -hir told): `SemanticsScope::speculative_resolve_mod_path(&self,
path: &ModPath) -> Option<PathResolution<'db>>`, the tail of `speculative_resolve` (its
`resolve_hir_path` of `Path::BarePath(path)`), which `speculative_resolve` can call. blaze-lints'
`Cx::resolve_name` builds the one-segment `ModPath` of a captured name with
`speculative_resolve`'s rule (`one_segment_path` in `check.rs`), so `make::path_from_text`
goes.

From frontend-tree-move-def (declared in [tree-move-def.md](tree-move-def.md)):
`BodySyntax::tree(&self, db) -> Arc<FlatTree>`, for check.rs `lint_body` in place of
`BodySyntax::root` and `view_of`. That removes `BodySyntax::root`'s last caller.

## Handle functions of these crates and their callers

- `cfg::CfgExpr::parse_from_ast`: deleted (with the handle side of `cfg/src/tests.rs`'s `cfgs`)
  once its callers move: hir-expand `attrs.rs`, `cfg_process.rs` (-expand), hir-def `attrs.rs`,
  `expr_store/lower.rs` (-def), blaze-engine `test_harness.rs` (-codegen), blaze-diagnostics
  (-diag-lints, -diag-lower); blaze-doc `lint.rs` is moved (-doc). Request:
  `v2/requests/frontend.md`, `## frontend-tree: cfg src/cfg_expr.rs, src/tests.rs: delete
  CfgExpr::parse_from_ast (1)`.
- `syntax_bridge::syntax_node_to_token_tree`, `syntax_node_to_token_tree_modified` and the cursor
  `Converter`: callers left in hir-expand (`builtin/derive_macro.rs`, `builtin/fn_macro.rs`,
  `eager.rs`, `budget_tests.rs`, -expand) and hir-def (`item_tree/attrs.rs`, -def), and
  syntax-bridge's own tests, which compare the view path with them (they stay until deletion).
- `syntax_bridge::token_tree_to_syntax_node` returns `(Parse<SyntaxNode>, SpanMap)`: callers read
  it with `Parse::root()`. `Parse<SyntaxNode>` is a kind tag; deleting `SyntaxNode` needs another
  tag for an untyped parse (frontend-tree's choice).
- `syntax_bridge::prettify_macro_expansion` (deprecated, edits a tree with `SyntaxFactory`): left
  out as tree.md leaves out editing; callers are hir-expand's IDE-facing
  `prettify_macro_expansion_.rs` and mbe's tests.
- `syntax_bridge::reference` (tests and `feature = "reference"` only): the handle reference the
  differential tests compare with; stays until the handles go.

## Proof

- `cfg/src/tests.rs` `cfgs`: every cfg test reads its predicate from the view (`call_input` for
  the token tree, `parse_from_view`) and asserts the handle's `parse_from_ast` gives the same
  `CfgExpr`; the expectations (DNF, why-inactive, hints) are checked on both the token-tree and
  the view reading.
- `mbe/src/benchmark.rs`: the macro_rules fixtures' token trees are taken through `call_input`;
  the benchmark's hash expectations are unchanged, so the view input is the cursor's.
- `const-ast/src/tests.rs`: unchanged, it compares the const views with the handles.
- `blaze-cargo`'s `without_tests`: request in `v2/requests/cargo.md`; same items, ranges and
  tokens, so the same hash, checked by blaze-cargo's existing state tests.
