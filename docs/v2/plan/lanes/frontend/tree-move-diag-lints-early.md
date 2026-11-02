---
status: plan
summary: frontend-tree-move-diag-lints-early, blaze-diagnostics' early lint pass (lints/syntax/early.rs and early/) moved off the syntax handles onto views; the view functions it offers others and what each test proves.
date: 2026-10-29
---
# frontend-tree-move-diag-lints-early

Item 16 of [frontend.md](../../design/frontend.md), design in [tree.md](tree.md), for
`crates/blaze-diagnostics/src/lints/syntax/early.rs` and `early/` (the lexer, labels, macros,
asm, matchers, builtin, validation, pretty). Child of
[tree-move-diag-lints.md](tree-move-diag-lints.md), whose `Tree<'db>`/`Walk<'db>` on views
(section "The walk on views") these files read. Every edit is a request to lane `diagnostics`
(`v2/requests/diagnostics.md`), one heading per file, `## frontend-tree: blaze-diagnostics
lints/syntax/<file>: onto views (1)`: 16 headings, landing as one group with the parent's
`lints/syntax.rs` heading. `early/lexer/idents.rs` and `early/lexer/lex.rs` hold no handle and
do not change.

After the move no file here holds a `SyntaxNode`, `SyntaxToken`, `SyntaxElement` or
`SyntaxText`, or calls `clone()` on a node. What is left of the handle API is ids and kind tags:
`InFile<SyntaxNodePtr>` (where a lint is placed, and `Expansions`' key) and, in tests,
`Parse<SourceFile>`.

## What the files offer others, on views

- `early::validation::pretty` (read by `checks/ast_validation/{items,generics}.rs`,
  frontend-tree-move-diag-lower): `where_predicate(out: &mut String, pred: ast::WherePred<'_>,
  edition)`, `ty(out, ast::Type<'_>, edition)`, `bounds(out, ast::TypeBoundList<'_>, edition)`,
  `generic_arg(out, ast::GenericArg<'_>, edition)`, `expr(out, ast::Expr<'_>, edition)`, and
  `push_ident(out: &mut String, text: &str, edition)` in place of `ident(text, edition) ->
  String`. Each appends to the caller's one buffer the `&'t str`s the tree lends: nothing is
  allocated per node or per identifier.
- `early::Expansions<'w, 'db>`: `of(cx, walk: &'w Walk<'db>)`, `of_call(file, call: ast::Node<'_>)
  -> Option<&'w Tree<'db>>`. Keyed by `InFile<SyntaxNodePtr>` (an id), so a lookup makes no
  handle: the call's view gives its pointer by arithmetic (`view::ptr`).
- `early::lints<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>)` and `macros::lints` likewise: the
  walk's trees are of `cx`'s `'db`. `asm` hands a tree's `MacroCall` view to
  `cx.vsema.expand_macro_call`, which takes views of its own `'db`, and the lexer's gallery
  borrows identifiers' text (`Cow<'db, str>`) where it copied it.

## What changes beyond the types

Each is the same output, read without allocating:

- `lexer`: whether a file is ASCII is `tree.root.text().is_ascii()` (the tree's one `&str`) in
  place of a walk over `SyntaxText` chunks; the gallery of proc-macro identifiers keeps the
  first entry of each symbol through `hash_map::Entry::Vacant` and borrows its text.
- `builtin`: `allow_internal_unsafe` reads `Walk::attrs` (the walk's `cfg_attr` iterator) in
  place of calling `expand_cfg_attr` again; `special_module_name` compares
  `Meta::simple_name()`'s `&str`; `crate_items` hands each crate-root item to a closure in
  place of gathering them into a `Vec`; the `...` suggestion formats the end pattern's `&str`.
- `validation`: the `patterns_in_fns_without_body` suggestion is built with
  `pretty::push_ident` into one `String` sized for it.
- `macros/asm`: `lint_node_of` walks diag-lower's `up` (on views) and stops at the first node
  with a lint id, in place of gathering `ancestors` into a `Vec`; the first `ASM_EXPR` of a
  tree is found from the walk's kind column (`Tree::of_kind`).
- `macros/matchers/tokens`: `Tokens<'t>` borrows the definition's text (`node.text()`) in place
  of `node.to_string()`; `text` and `name` return `&'t str`.

## What this programs against

- `Tree<'db>`, `Walk<'db>` (the parent's `lints/syntax.rs` heading): nodes by value,
  `Tree::of_kind`, `Walk::attrs`.
- hir-expand ([tree-move-expand.md](tree-move-expand.md)): `MacroCallLoc::to_node_view(db) ->
  InFile<view::Node<'db>>`, `AstId::to_view(db)`, `AttrId::find_attr_range_view`, and `Typed`
  for `ast::Macro` (for `def.to_view(db)` in `macros.rs`). `to_node_view` is the one name
  [tree.md](tree.md) fixes across layers; expand's `lib.rs` request still calls it `to_view`.
- hir ([tree-move-hir.md](tree-move-hir.md)): `hir::view::Semantics` through `Cx::vsema`:
  `parse`, `expand_macro_call(ast::MacroCall<'db>)` (both in the tree). It finds a view's file
  by its tree, so `asm` asks it only about calls in the walk's trees (whose roots the parent's
  `Walk` takes from `cx.vsema`) and in what it expanded itself.
- `Cx` and `lower` ([tree-move-diag-lower.md](tree-move-diag-lower.md)): `Cx::vsema`,
  `Cx::syntax_view`, `cx::item_range_view` (the `cx.rs` heading), and
  `lower/break_outside_of_loop.rs`'s `up` on views (`up<'db>(cx, node: InFile<view::Node<'db>>)`,
  `Item = InFile<view::Node<'db>>`). Once this and the bodies lane's headings land,
  `break_outside_of_loop::ancestors` has no callers.

## Proof

- `early.rs`: `the_walk_allocates_nothing_per_node` (new): on a crate with nothing to lint, what
  the whole pass allocates, less what `unused_labels` does (hir-def's `every_body` list, which
  grows with the crate and is not this walk), is the same for 4 and for 400 functions. It holds
  the property views give: no allocation per node, per cast or per text read.
- `lexer.rs`: `an_ascii_crate_is_not_lexed` (kept): an ASCII 2021 crate is not lexed, and the
  lexer's allocations do not grow with the crate (at most 4).
- `pretty.rs`: `a_predicate_is_printed_with_rustcs_spacing` (the existing cases, on views: the
  same text as before the move); `an_identifier_is_printed_raw_where_a_keyword` (new,
  `push_ident` across editions); `printing_allocates_only_the_buffers_growth` (new: a predicate
  with a binder, bounds, a generic argument and a const expression printed into a buffer with
  room allocates nothing).
- `macros/matchers/{tokens,quoted,check,follow,expr_2024}.rs`: the existing unit tests, now
  reading the definition from `Parse::root()`: the same tokens, gluing, matchers, misuses,
  `pat` follow sets and `expr` ranges as before the move, so `Tokens` built from the lent text
  is the tokens built from the copy.
- `lexer/{idents,lex}.rs`: their tests are unchanged.
- The UI tests in `src/tests/` (rustc's `.stderr`, byte for byte) are the differential for every
  lint here: the same diagnostics, at the same spans, in the same order.
