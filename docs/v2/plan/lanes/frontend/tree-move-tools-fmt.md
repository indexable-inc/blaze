---
status: plan
summary: frontend-tree-move-tools-fmt, blaze-fmt's lowering of rust-analyzer's tree into rustfmt's ast moved from syntax handles onto syntax::view; no twins needed, and what each test proves.
date: 2026-10-29
---
# frontend-tree-move-tools-fmt: blaze-fmt onto views

[tree.md](tree.md), [frontend.md](../../design/frontend.md) item 16. Crate: `crates/blaze-fmt`
(the coordinator's, edited directly). The handle uses were all in `src/parse/`: the lowering
(`lower/{mod,expr,item,pat,ty}.rs`), the fragment parser (`stream.rs`) and the file parser
(`parser.rs`). `crate::ast` is rustfmt's own AST and is untouched.

## No twins, no requests

`SyntaxTreeBuilder::finish` already gives a `Parse<SyntaxNode>` without making a handle, and
`Parse::root()` is defined for every `Parse<T>`, so `parse_ra` returns the `Parse` and each
caller views it with `parse.root()`. The `Parse` is kept, not an `Arc<FlatTree>` taken out of
it: its drop hands the tree's last count to the dropper thread. `SyntaxNode` remains only as
`Parse`'s kind tag.

## How the code reads now

- `use syntax::view::{self as ra, ..}`: every lowering method takes its view by value
  (`ra::Expr<'_>`, `ra::Node<'_>`, `ra::Token<'_>`); `Option<&ra::RetType>` became
  `Option<ra::RetType<'_>>`; `item_generics`/`adt_generics`/`opt_bounds` take
  `impl ra::HasGenericParams<'t>`/`ra::HasTypeBounds<'t>` by value. Nothing stores a view, so
  `Lower` itself has no tree lifetime; each method borrows the tree for its call.
- `child_token` is the view's own `Node::child_token` (the helper is deleted); `normal_attr`'s
  search for `[` is `child_token(T!['['])`. `first_token`, `last_token`,
  `first_token_after_attrs`, `next_real_token`, `prev_real_token` and `range_op` stay as helpers
  over views because each adds a rule (skip trivia, skip attributes, one of three kinds).
- Allocations the move removed: `path` walks its qualifier chain from the innermost qualifier
  out through `parent` (a qualifier is a child of the path it qualifies) where it built and
  reversed a `Vec`; `mod_spans` finds its first and last tokens in one pass where it collected
  a `Vec` of tokens; `Parser::parse_item` takes the only item with two `next()` calls where it
  collected a `Vec`. No allocation was added: no `to_string()` compares a token, no `.clone()`
  of a view is left.

## Proof

- `tests/format.rs` (existing): snippets formatted against what the pinned nightly's rustfmt
  writes. The output depends on every span the lowering gives, so the same output before and
  after the move is the differential test.
- `src/parse/lower/tests.rs`, `every_source_file_lowers_as_recorded`: lexes, parses and lowers
  every `crates/blaze-fmt/src/**/*.rs` (96 files) through the views. Each must lower with nothing
  `Unsupported`. The `Debug` of each lowered `ast::Crate` (spans, symbols, token streams) is
  hashed with FNV-1a and compared, with its item count, to `src/parse/lower/lowered.txt`
  (expect-test). The hashes can't be worked out by hand, so the file isn't written yet.
  Blessing it with the code from before the move makes this a differential test; the recipe is
  below. Blessing it with the moved code makes it a regression guard from then on.
- `qualified_paths_run_from_the_innermost_qualifier`: the new qualifier-chain walk gives
  `a`, `a::b::c`, `::d::e` (`{{root}}` first), `<T as Tr<u8>>::x::y` (`Tr x y`, self at 1) and
  `<T>::z` (`z`, self at 0), worked out from rustc's `parse_qpath`.

### Blessing `lowered.txt` from the pre-move lowering

In a scratch checkout of the commit before this move, copy `src/parse/lower/tests.rs` and the
`mod tests;` line, with three edits for the handle signatures: in `lowered`, `let root =
parse_ra(..)?; let file = syntax::ast::SourceFile::cast(root).expect(..);` and
`lower.krate(&file)`; in `paths`, `.descendants()` on the `SyntaxNode` `parse_ra` returns,
`filter_map(syntax::ast::PathExpr::cast)` and `lower.path(&p)`. Run with `UPDATE_EXPECT=1`, then
copy `lowered.txt` here. The moved code must then pass unchanged.

## Speed

The lowering does per node what it did before, minus a counted cursor per step: each view
step is arithmetic on the flat tree's arrays, where each handle step touched a `Cell` count
and cloned. Three `Vec` gatherings per path, per module and per item fragment are gone.
rustfmt's own `Box`/`ThinVec` AST is unchanged (it is the output format).
