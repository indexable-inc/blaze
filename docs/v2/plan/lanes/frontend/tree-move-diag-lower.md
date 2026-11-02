---
status: plan
summary: blaze-diagnostics outside lints/ (lower/, checks/, cx, print, emit, tests) moved off syntax handles onto borrowed views; the Cx view API, the lower-layer twins it reads, and what each test proves.
date: 2026-10-29
---
# frontend-tree-move-diag-lower: blaze-diagnostics on views

Child of frontend-tree ([tree.md](tree.md), [frontend.md](../../design/frontend.md) item 16).
Paths: `crates/blaze-diagnostics/src/{lower,checks,print,emit,tests}/`, `checks.rs`, `cx.rs`,
`check.rs`, `api.rs`, `model.rs`, `print.rs`, `emit.rs`, `tests.rs`. Sibling
`frontend-tree-move-diag-lints` has `lints/`, `lints.rs`, `lib.rs`.

## Inventory (2026-10-29 21:30)

About 3,000 handle uses in about 120 files: `lower/` about 1,700 (85 files), `checks/` about
1,100 (40 files), `cx.rs` 35, `print/` 21, tests a handful. They are of four kinds:

1. Typed handles passed and walked: `&ast::X`, `.syntax().clone()`, `ast::X::cast(node.clone())`,
   `node == &other`. Mechanical: `ast::X<'db>` by value, `syntax()` is `Copy`, no clones.
2. Nodes found from ids: `Cx::node(InFile<AstPtr<N>>)`, `Cx::syntax(InFile<SyntaxNodePtr>)`
   (75 calls), `id.to_node(db)` on `AstId`s (14). They become `to_view`.
3. `hir::Semantics` calls that take or give handles: `source` and `HasSource::source` (166),
   `resolve_path` (34), `type_of_expr` (22), `to_def` (21), `scope(&SyntaxNode)` (16),
   `parse_or_expand` (13), and 17 more with a few calls each. These wait on
   `frontend-tree-move-hir`'s twins (asked by tmux, 21:35, with the list and counts).
4. Text: `node.to_string()`/`text().to_string()` used to slice or compare. A view's `text()` is
   `&'db str`; no string is made where the handle version made one only to read it.

## Cx's view API (declared; written in `cx.rs`)

The diagnostic's location stays an id (`InFile<AstPtr<syntax::ast::X>>`, `InFile<SyntaxNodePtr>`)
and is viewed when read. `Cx<'a, 'db>` hands out views borrowing the database:

Twins beside the handle versions, because `lints/` (sibling `frontend-tree-move-diag-lints`)
still calls those; the handle versions are deleted once both lanes' callers are moved.

- `Cx::vsema: hir::view::Semantics<'db>`, beside `sema`. Every view handed to it must come from
  a tree it gave out (it finds a view's file by the tree's address), so every root below comes
  from it. It is the one view `Semantics` of a `Cx`: `lints/syntax.rs`'s `Walk` takes its roots
  from it too (`cx.vsema.parse`, `cx.root`), and `node_view`/`syntax_view` are
  `vsema.to_view`/`to_view_syntax`.
- `Cx::root(&self, file: HirFileId) -> view::Node<'db>`: `self.vsema.parse_or_expand(file)`.
- `Cx::node_view<N: syntax::AstNode + Typed>(&self, ptr: InFile<AstPtr<N>>) -> InFile<N::Of<'db>>`:
  `ptr.value.to_view(self.root(ptr.file_id))`.
- `Cx::syntax_view(&self, ptr: InFile<SyntaxNodePtr>) -> InFile<view::Node<'db>>`:
  `view::to_node(ptr.value, self.root(ptr.file_id))`.
- `break_outside_of_loop::up<'a, 'db>(cx, InFile<view::Node<'db>>) -> Up<'a, 'db>` and `edges`:
  the walk up through expansions, allocating nothing; stepping out of an expansion it takes
  `MacroCallLoc::to_node_view` and asks `vsema.parse_or_expand` for the call's file, so the call's
  tree is one `vsema` knows. The handle `ancestors` stays for `lints/` until it moves.
- `item_range_view(Node<'_>)`, `signature_range_view(view::Fn<'_>)`, `def_range_view(Node<'_>)`,
  `derive_paths_view(view::Attr<'_>)`: the same ranges and paths, from views by value.
  `def_range_view` slices `node.text()` instead of `node.to_string()`.
- `Cx::lint_level` keeps taking `InFile<SyntaxNodePtr>` (an id).

### The twins `lints/` moves onto (all in the tree, `'db` from `Cx<'_, 'db>`)

| Handle (deleted once `lints/` is off it) | View twin |
| --- | --- |
| `Cx::syntax(InFile<SyntaxNodePtr>)` | `Cx::syntax_view(InFile<SyntaxNodePtr>) -> InFile<view::Node<'db>>` |
| `Cx::node(InFile<AstPtr<N>>)` | `Cx::node_view(InFile<AstPtr<N>>) -> InFile<<N as Typed>::Of<'db>>` |
| `cx::derive_paths(&ast::Attr)` | `cx::derive_paths_view(view::Attr<'_>) -> Vec<(TextRange, String)>` |
| (token walk) | `cx::derive_path_tokens_view(view::Attr<'t>, FnMut(TextRange, &[view::Token<'t>]))` |
| `cx::item_range(&SyntaxNode)` | `cx::item_range_view(view::Node<'_>) -> TextRange` |
| `cx::def_range(&SyntaxNode)` | `cx::def_range_view(view::Node<'_>) -> TextRange` |
| `cx::signature_range(&ast::Fn)` | `cx::signature_range_view(view::Fn<'_>) -> TextRange` |
| `cx::closure_decl_range(&SyntaxNode)` | `cx::closure_decl_range_view(view::Node<'_>) -> TextRange` |
| `break_outside_of_loop::ancestors` | `break_outside_of_loop::up(cx, InFile<view::Node<'db>>)`, `Item = InFile<view::Node<'db>>` |
| (a file's tree) | `Cx::root(HirFileId) -> view::Node<'db>` (from `cx.vsema`) |

Files import the free ones under the old names (`cx::{item_range_view as item_range, ..}`).
`Found::new(Diag, Phase)` and `Found::lint(Diag, Phase, InFile<SyntaxNodePtr>)` keep taking ids:
a view's id is `view::ptr(node)`.

## Lower-layer twins read

- `HirFileId::root(db) -> view::Node<'db>` and `AstId::<N>::to_view(db)` (hir-expand, in tree).
- `AstPtr::<N>::to_view(root)` / `try_to_view` and `Typed`/`AstNode` for `Either<L, R>`
  (syntax, in tree).
- `MacroCallLoc::to_node_view(db) -> InFile<view::Node<'db>>` (hir-expand, in tree).
- hir-def's twins per [tree-move-def.md](tree-move-def.md) and its `tree-move-def-<part>.md`
  (`HasSource::source_view`, `HasChildSourceView::child_source_view`, `use_tree_to_view`,
  `ExpressionStoreSourceMap::node_*_view`, `file_id_and_params_of_view`). Nothing in these paths
  calls the handle forms; `BuiltinDeriveImplLoc::source` gives an `InFile<TextRange>`, not a node.
- `hir::view::Semantics` (`crates/hir/src/semantics/view.rs`, in tree). `to_def2(InFile<&T>)`
  has no twin; callers take `to_def(T)`, the same lookup with the file found from the tree.

## How the edits are handed over

The diagnostics owner granted direct edits (`## frontend-tree-move-diag-lower: grant` in
`v2/requests/diagnostics.md`): one function at a time, re-read before each edit, the hot-lint
`// alloc:`/`// clone:`/`// dyn:` lines kept where their line still allocates or clones, the
file's agent told by tmux. `lower/missing_match_arms.rs` and `lower/non_exhaustive_let.rs` go as
requests to `diagnostics-lints-mir-speed`. Requests (1) to (4) were staged whole first, at
`v2/staged/frontend-tree-move-diag-lower/<repo path>` with a `<repo path>.base`, and applied by
`git merge-file` onto the files as they were by then, conflicts resolved by hand.

## Proof

- `cx.rs` tests: `signature_range`, `def_range`, `item_range` and `derive_paths` on views give
  the same ranges and paths as before, on the same sources (the existing tests, moved).
- The UI tests in `src/tests/` (rustc's `.stderr` compared byte for byte) are the differential:
  every diagnostic's span and text is unchanged by the move.
