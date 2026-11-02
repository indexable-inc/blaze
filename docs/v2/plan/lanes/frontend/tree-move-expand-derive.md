---
status: plan
summary: The built-in derives read their own item in place (views of the parse of the derive's token tree) with no output change (request 11); CoercePointee then writes its impls as tokens that keep the item's spans, as rustc's do (request 12). The handle and editor forms stay test-only as the references the tests compare against.
date: 2026-10-29
---

# frontend-tree-move-expand-derive: the built-in derives onto views

Lane `frontend-tree-move-expand-derive`, a child of `frontend-tree-move-expand`
([tree-move-expand.md](tree-move-expand.md)). The files are
`crates/hir-expand/src/builtin/derive_macro.rs` and `builtin/derive_macro/`. Their owner is
lane db (db-expand, now its child `db-expand-derive`). Every edit is a request in
`v2/requests/db-expand.md`:

- (10) is withdrawn. db-expand-views refused it because its `CoercePointee` build changed the
  output for a 2015 item, inside what was meant to be a move with no output change.
- (11) is the view move of everything but `CoercePointee`, with no output change. db-expand-derive
  applied it on 2026-10-29.
- (12) makes `CoercePointee` a direct token build, as a correctness change of its own.
  db-expand-derive applied it on 2026-10-29. It then brought the build to rustc's
  `coerce_pointee.rs` at the pin: whole `where` predicates with their binder (the binder loss
  below is fixed), no empty predicates, a simple-path pointee, and a walk of every type inside.
  The `>` line under (12) in the request says what changed. This page describes (12) as filed.

## What it was

The derive parses its input token tree into a flat tree. `token_tree_to_syntax_node` is the
only parse, and it is unchanged. The rest went through handles:

- `to_adt_syntax` made a `SyntaxNode` per call (`parsed.syntax_node()`) and cast it to
  `ast::Adt`.
- `parse_adt_from_syntax` walked handles, made each tree with `syntax_node_to_token_tree`, and
  cloned the field list's node.
- `from_derive`, `coerce_shared_target` and `attr_words_have` read handles too.
  `coerce_shared_target` made a second `SyntaxNode` for its reparse check.
- `coerce_pointee_expand` copied the item into a `SyntaxEditor`. It rebuilt each new `where`
  predicate from text with `make` (a reparse per constructor), rebuilt the whole `where` clause
  from text, finished the edit into a new green tree, and converted that tree back to tokens.

## Request (11): the move, with no output change

- `parse_item` (the parse) and `adt_of` (a view `ast::Adt<'_>` of the parse's root) replace
  `to_adt_syntax` for every derive but `CoercePointee`. They raise the same three errors.
- `parse_adt_from_syntax`, `push_fields` and `name_to_token` take views and are generic over
  `&impl SpanMapper`. Every tree comes from `syntax_bridge::call_input`, which runs the same
  `FlatConverter` that `syntax_node_to_token_tree` ran on the flat node, so the output is the
  same tree. For a `SpanMap`, `span_for(range)` is `span_at(range.start())`. No node is cloned.
- `from_derive` and `coerce_shared_expand` read the view. `coerce_shared_target_tokens` uses
  `call_input_modified` with the parentheses' token ids removed, and its reparse check casts
  `parse.root()`.
- `CoercePointee` is unchanged. Its editor code, with the handle helpers it needs (its own
  `to_adt_syntax`, `push_fields`, `parse_adt_from_handle` and `name_to_token`, plus
  `attr_words_have`), moves verbatim into `derive_macro/editor.rs`, a module of its own.
- `reference.rs` gets a test-only `handle` module holding `from_derive` and
  `coerce_shared_target_tokens`. Its tests compare each view form with its handle form.

## Request (12): `CoercePointee` as tokens, rustc's spans

rustc (`rustc_builtin_macros/src/deriving/coerce_pointee.rs`; the pinned nightly is
`16d1ffac1`) builds the impls from `generics.clone()` and clones of the `where` predicates.
The self type's arguments are `ty_ident(p.span(), p.ident)`. Only `__S` (which also replaces
each pointee path) and the `core::marker::Unsize` path are made at the derive's span. Every
token taken from the item therefore keeps its own span, and with it its edition and hygiene.

The editor did not do this. Every token was at the derive's span, and `make` reparsed every
predicate, the item's own included, as text in `Edition::CURRENT`. Blaze parses macro output
with each token's own edition (`|ctx| ctx.edition(db)`), and the derive's span is def-site in
`Edition::CURRENT`. So a 2015 item naming `async`, `dyn` or `gen` in a predicate got impls that
read them as keywords.

The token build (`coerce_pointee_derive`, `write_pointee_predicates`, `pointee_self_type`,
`pointee_paths`, `write_copied`) has no mutable tree, no green rewrite and no reparse:

- The checks run in the editor's order, with its errors.
- `info` is read with the item's span map, as every other derive reads it.
- The added predicates come in the editor's order and spacing:
  - a written `:` and `+` are `Alone`;
  - a type's last punct is `Joint` before its `:`;
  - a punct before `__S` is `Alone`.
- A copied token is at its own span in `span_map`. `call_input_in` converts the node with the
  `AtRange` mapper, so each token's range locates both the pointee's paths and the token's span.
- Names are copied as written (`copied_name`), where the editor raw-escaped a current-edition
  keyword.
- `Unsize<__S>` bounds the pointee's own name at its span. In the self type, a lifetime is split
  the way `syntax_bridge` splits its token.

`editor.rs` becomes test-only and the reference: its `coerce_pointee_expand` and
`to_adt_syntax` are deleted.

Not fixed, and the same in both forms: take a `where` predicate whose type does not name the
pointee but whose bounds do. Blaze writes `ty: the bounds that name it`, which drops a `for<'x>`
binder (fixture `for<'x> Vec<&'x U>: Tr<T>`). rustc copies the whole predicate. The fix is
routed to db-expand-derive after (12): copy the whole predicate, binder included, with a test
on that fixture.

## What the tests prove

From (11):

- `derives_match_nested_quotes` checks every derive over the view against the nested-quote
  reference.
- `parse_adt_matches_the_handle_form` compares the view parse with the handle parse field by
  field (name, shape, parameters, lifetimes, `where`, associated types, `packed`), and their
  errors, on `ITEMS` and `POINTEE_ITEMS`.
- `from_matches_nested_quotes` compares `From` with the handle form and the nested quotes,
  including every refusal.
- `coerce_shared_target_matches_the_handle_form` covers one and two attributes, a bare one, a
  `=` one, an empty one, junk tokens, and a path with generics.
- `the_item_read_in_place_allocates_no_more`: the view parse allocates no more than the handle
  parse did. The `Clone` budget (`a_derive_allocates_nothing_per_field`) runs over the view.

From (12):

- `coerce_pointee_matches_the_editor` runs every item of `POINTEE_ITEMS` and `ITEMS` through
  both forms. It compares the errors directly, then the trees with every span set to the
  derive's: `Debug`, `Display` and encoding. It covers each refusal in order and every
  `substitute_type_in_bound` arm in a bound and in a `where` predicate.
- `coerce_pointee_keeps_the_items_spans` checks that each token is either at the derive's span
  or at an item token whose text contains it, and that `__S` is always at the derive's span.
- `coerce_pointee_keeps_a_2015_items_names` parses a 2015 item with `async`, `dyn` and `gen` in
  its predicates. It checks that all 22 copies in the impls are unraw, at their own text in the
  item, in the 2015 context.
- The harness case `harness/v2/frontend/cases/infer/118-coerce-pointee-2015-predicates.rs`
  builds and runs the same item under the pinned rustc and under blaze.

No build, test or harness run was done (the lane's brief). db-expand runs the tests when it
applies the requests. The harness case runs in the release train.
