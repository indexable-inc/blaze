---
status: plan
summary: blaze-diagnostics' AST validation (checks/ast_validation*) and attribute checks (checks/attrs*) moved off syntax handles onto borrowed views; the visitor's view API, what changed beyond renames, and what each test proves.
date: 2026-10-29
---
# frontend-tree-move-diag-lower-checks-syntax: AST validation and attributes on views

Child of [tree-move-diag-lower.md](tree-move-diag-lower.md). Paths in
`crates/blaze-diagnostics/src/`: `checks/ast_validation.rs` and `checks/ast_validation/**`,
`checks/attrs.rs` and `checks/attrs/**`. Split: `checks/ast_validation.rs` and
`checks/attrs/parsers/stability.rs` here; `ast_validation/{fns,generics,items,types}.rs` in `-astv`;
the rest of `checks/attrs*` in `-attrs`.

## The visitor's API (`checks/ast_validation.rs`)

What `-astv` programs against. Nodes are `view::Node<'db>` of the walk's trees (`Tree<'db>`, the lints
lane's `lints/syntax.rs` request), passed by value.

- `check<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>)`; `V<'v, 'a, 'db>` (the walk's lifetime is the
  database's, so the fourth lifetime goes).
- `emit`/`emit_after`/`emit_parse`/`kept`/`attr`/`has_attr` take `Node<'_>`; `each_attr<'t>` gives
  `(view::Meta<'t>, view::Attr<'t>)` by value, through `hir_expand::attrs::expand_cfg_attr_view`.
- `ctxt`/`ctxt_in -> Ctxt<'db>` (`Copy`, views of the trait, impl or `extern` block), `constness`
  gives `InFile<view::Token<'db>>`.
- `ancestors -> Ancestors<'_, 'db>`: steps out of an expansion through
  `cx.vsema.find_parent_file`, so the call's tree is one `vsema` handed out.
- `text_of<'t> -> &'t str`: a slice of the node's text where the handle made a `String`.

## Beyond renames

- **A derive's call.** `MacroCallLoc::to_node` gives a derive's (and an attribute-derive's) `ATTR`;
  `to_node_view` gives its `META`. `Ancestors` goes from either to the `ATTR` that holds it and on to
  the parent of the item, as before. `stability.rs`'s `Defs::parent` walks up through the meta and
  the `ATTR`, neither of which `Target::of` makes a definition, to the same item.
- **Trees `vsema` must know.** `stability.rs` asks `vsema.to_def` of definitions found outside the
  walk: a module file's `mod` declaration (`Module::declaration_source_view`), a macro call's node,
  a derive's item. Each tree is registered with `vsema.parse_or_expand(file)` when it is reached,
  as `break_outside_of_loop::up` does; `HirFileId::root` and `AstId::to_view` view the same
  memoized tree, so the addresses match.
- **`crate_range`** (`stability.rs`): the handle version called `kind()` on `ast::Comment`, which
  has none. Doc comments are `INNER_DOC_COMMENT`/`OUTER_DOC_COMMENT` tokens and `is_trivia` is
  `WHITESPACE | COMMENT`, so "a token or a doc comment" is "not trivia".
- `Missing::check` spans `def_span(def.node.value)` where it re-found the same node from its pointer.

## Proof

- `stability.rs` `the_crate_root_is_spanned_by_its_doc_comments_attributes_and_items`: leading plain
  comments and white space are left out, doc comments and inner attributes are in, a trailing doc
  comment ends the range, a file of comments only is spanned empty at its start.
- The UI tests in `src/tests/` (rustc's `.stderr` byte for byte) are the differential for every
  diagnostic's text and span.

## Speed of light

The visitor reads one `kind` per node of the walk's columns and casts only the kinds it checks:
with views a cast is a kind compare and a copy of 24 bytes, where a handle cast cloned a counted
cursor (a `Cell` bump and its drop). Nothing is allocated unless an error fires (the walk's `Vec`s
are the lints lane's); the per-crate cost is the walk's node count times one load and a `match`,
which is the bound. The stability pass (only for `staged_api` crates) hashes each definition's
`SyntaxNodePtr` once per lookup map, as before; it now clones no node per definition or per step up.
