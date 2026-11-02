---
status: plan
summary: frontend-tree-move-codegen, the syntax-handle callers in hir-ty-mir, hir-ty-codegen and blaze-engine moved onto syntax::view; the APIs they take from hir-expand, hir-def and hir, and what each test proves.
date: 2026-10-29
---
# frontend-tree-move-codegen: codegen's crates onto views

[tree.md](tree.md), [frontend.md](../../design/frontend.md) item 16. Crates:
`crates/hir-ty-mir`, `crates/hir-ty-codegen`, `crates/blaze-engine` (lane codegen; every edit is a
request in `v2/requests/codegen.md`).

## No twins of my own

Every caller of `hir_ty_mir::mir::location`'s syntax functions (`written_range`, `concat_parts`,
`reported_range`, `written_call`) is in these three crates (`crates/hir-expand/src/builtin/fn_macro.rs`
has private copies of its own), so they change signature in place, each request with its callers:

- `written_range(node: view::Node<'_>) -> TextRange`
- `concat_parts(call: view::Node<'_>) -> Option<Vec<ConcatPart>>` (the `Vec` of arguments goes:
  the arguments are split while the call's token tree is walked once)
- `reported_range(db, owner, expr, file_id, node: view::Node<'_>) -> Option<TextRange>`
- `written_call<'db>(db: &'db Db, file_id) -> Option<InFile<view::Node<'db>>>`

## What I take from below

- **Already in the tree**: `HirFileId::root(db)`, `AstId::to_view(db)`, `AstPtr::to_view(root)`,
  `syntax::view::to_node(ptr, root)`, `EditionedFileId::parse(db).root()`. `HasSource::source(db)`
  is replaced by `HasSource::ast_ptr(db)` (already in `hir-def/src/src.rs`) and
  `ptr.to_view(file_id.root(db))`: the same node (`source` is `ptr.to_node(parse_or_expand)`).
  A node's `original_file_range_rooted` is `InFile<TextRange>::original_node_file_range_rooted` of
  its range, which is what the handle method calls.
- **frontend-tree-move-expand**: `MacroCallLoc::to_node_view<'db>(&self, db: &'db Db) ->
  InFile<syntax::view::Node<'db>>`, the twin of `MacroCallLoc::to_node` (same node per kind: the
  `MacroCall` of a fn-like call, the `Attr` of a derive or an attribute derive, else the item);
  `InFile<view::Token<'_>>::original_file_range(self, db) -> FileRange` (the doctest collector).
- **frontend-tree-move-def**: `BodySyntax::tree(&self, db) -> Arc<FlatTree>`
  (`hir-def/src/expr_store/body.rs`, exists as of 2026-10-29), viewed by
  `syntax::view::Node::root(&tree)` in `places.rs` (request 2 items 21, 22), and a view twin of `visibility_from_ast` (called by
  `hir-ty-codegen/type_id.rs`).
- **frontend-tree-move-hir**: `Semantics` twins for `blaze-engine` (`lib.rs`, `doctest*.rs`,
  `test_harness.rs`, `check.rs`): `ancestors_with_macros`, `hir_file_for`, `scope`, `to_def` of a
  `Fn`, `resolve_path`, `resolve_macro_call`, `resolve_attr_macro_call`, `resolve_method_call`,
  `expand_macro_call`, `parse_or_expand`, `parse_guess_edition`, `source`,
  `module_definition_node`; and `hir::HasSource::source` / `declaration_source` /
  `definition_source` of `hir::{Function, Adt, Impl, Module, Macro, Static}`.

## Moves that need nothing from below

`blaze-engine/src/outline.rs` reads only the file's parse and its `AstIdMap`: it moves whole onto
`EditionedFileId::parse(db).root()` and `syntax::view::to_node`. `functions_of_modules` keys a
function by `(module path, name)`: the path is interned to a `u32` per tree pair (a map of
`(parent, &'t str) -> u32` shared by the two trees an edit compares), the name is the `&'t str`
of its token, so no `String` or `Vec` is made per node. The persisted hashes (`copied`,
`written_print`, `lasting_static_name`) hash `&str` where they hashed `String`, which `Hash`
makes the same bytes.

## The rest of blaze-engine

Requests (6)-(9) in `v2/requests/codegen.md` move every other reader of syntax in the crate onto
`hir::view::Semantics` and `Node<'db>`: `as_rustc`, `consteval`, `mirdump`, `check`,
`borrowck`, the doctest collector (`doctest.rs`, `doctest/{program,extract}.rs`), the test
harness, and what stays in `lib.rs` (tests' attributes, a function's written place and hash).
`lib.rs`'s naming of what rustc built (closures, statics in bodies, items of blocks, `impl`
places), its count of closures and statics, its late imports and every `self.sema()` move to
`crates/rustc-symbols`, already on views, by dedup-names' request (`## dedup: one rustc-symbol
resolver ...` in the same file); (9) goes after it and leaves that code to it.

- A view's `==` is identity (same tree by `Arc` address, same index), which is what the handle
  `==` was, and the view `Semantics` hands out the database's own trees (`HirFileId::root`), so
  the comparisons along `ancestors_with_macros`, against an expansion's root and between a
  macro's definition and the scopes around a call mean what they did.
- An item's place is its source view's range mapped up by
  `InFile<TextRange>::original_node_file_range_rooted`, which is what the handle
  `original_file_range_rooted` calls.
- No `String` per node where the handle version made one only to compare: `name_in_path` gives
  the name's `&str` (`Counts::holds` tests every node of a definition), `is_main` checks
  `fn main` as it walks the tokens, `expected` matches its five tokens as they come, a
  path's name and the text before a token tree are compared as `&str`. Hashes of a function's
  text and of a test's attributes take `&str`, which hashes as the `String` did.

**Stays on handles:** `lib.rs` `callers_of_generic`, which takes a generic function's callers
from `ide_db::defs::Definition::usages`, the IDE's reference search: it gives handle nodes and
takes the handle `Semantics`. What replaces it is a build-path index of callers, not a syntax
move.

## hir-ty-codegen's v2 harness

`v2/roots/harness.rs` (codegen-api-roots, new): request (10) reads a test function from
`HasSource::ast_ptr` under `HirFileId::root` and its attributes as views, `cfg_attr` through
`CfgExpr::parse_from_view`.

## Tests

- `outline.rs`'s tests (every one kept, on views): what an edit changes of a file's outline,
  what rustc copies, functions added alone; the same answers as before on the same texts.
- `an_outline_is_hashed_with_no_allocation` (request (11); `blaze-alloc-count` is already a
  dev-dependency and the crate's test allocator counts): `hash_outside_bodies` and
  `skeleton_of` of a parsed file allocate nothing, and the skeleton is the outline's hash.
- `lib.rs`: the `new_within` tests on the view `Semantics`. `a_count_counts_the_nodes_of_its_kind`
  moves to `crates/rustc-symbols/src/tests.rs` with the dedup request.
- `doctest/extract.rs` `type_of_an_impl_as_rustdoc_names_it`, `test_harness.rs`
  `the_harness_names_each_test_by_the_file_it_is_told`: unchanged answers on views.
