---
status: plan
summary: blaze-intent onto borrowed syntax views (frontend.md item 16) — working copies hand out Node<'_> from their Parse, path and edit walks take views, rename and item keys go through hir::view::Semantics.
date: 2026-10-29
---
# frontend-tree-move-tools-intent: blaze-intent on views

Part of [tree.md](tree.md) (frontend.md item 16), child of `frontend-tree-move-tools`. The crate
reads syntax three ways; each moves as follows. No handle use is left in `crates/blaze-intent`:
`Parse<SourceFile>` (in `Seed`, `Working`) is the only handle-layer type named, as the kind tag
of a parse.

## Working copies (`file.rs`)

`Working::base` was a `SyntaxNode` (the tree before any splice, where touched items are named).
It is now the `Parse<SourceFile>` it was made from, cloned (two `Arc` counts, once per file):
the same tree. `Working::base()` and `Working::root()` give `view::Node<'_>` from `&self`.
Nothing keeps a view past its borrow: a touched item is still kept as its start offset
(`At::Item(TextSize)`).

Because a view of `root()` borrows the working copy, `edit::apply` is split: `splice_of` borrows
the file, finds the target, records the touches and returns the one splice `(TextRange,
String)`; `apply` then makes it (and, for an added item, names what was added on the new tree).
Same splices, same touches, same order as before.

## Walks (`path.rs`, `edit.rs`)

`use syntax::view as ast;` and `Node<'t>` by value everywhere.

- `path::members` is an iterator (`impl Iterator<Item = Node<'t>>`), not a `Vec`. `find` keeps
  its `Vec<Node<'t>>` frontier (the old code's: its matches are counted and indexed; `@n` now
  keeps the one in place instead of building a second `Vec`). `of`/`step_of` walk the container
  again to count alike siblings instead of collecting them.
- Names and impls compare `&'t str` slices: `matches` reads the name token's text (a tuple
  field's index is still formatted, as before), `impl_is` compares an impl's type and trait
  with whitespace skipped (`squeezes_to`) instead of building two `String`s per impl; `impl_of`
  builds them only for the path text `of` returns.
- `edit::items_of` is an iterator; `add_field` passes the last field (not a `Vec` of all);
  `remove_entry` finds the previous and next entry of the same kind by walking siblings instead
  of collecting the list; `added` peeks the members iterator instead of collecting it; `AddUse`'s
  "already there" check compares the use tree's text without whitespace in place.

## Front end (`lib.rs`, `rename.rs`): `hir::view::Semantics`

Written against `frontend-tree-move-hir`'s declared twin ([tree-move-hir.md](tree-move-hir.md)):
`hir::view::Semantics<'db>`, the handle methods' names, views by value, every view one of a root
it handed out. `Edits` gains `roots: Vec<view::Node<'db>>` beside `ids`: each file's root from
`sema.parse(editioned).syntax()` when it is opened (the same tree as the working copy's base).
Rename finds the renamed item and scans tokens on `roots`, and `finish` keys touched items on
them, so the views the front end is asked about are its own (`'db`), never a borrow of a
working copy, and opening another file while holding the renamed item borrows nothing.

Methods used (all declared in tree-move-hir.md; the module and the to_def/resolve families are
not written yet): `Semantics::new`, `parse`, `attach_first_edition`, `file_to_module_def`,
`to_fn_def`, `to_struct_def`, `to_enum_def`, `to_trait_def`, `to_impl_def`, `to_const_def`,
`to_static_def`, `to_module_def`, `to_macro_def(view::Macro)`, `to_enum_variant_def`, `to_def`
(for `Union`, `TypeAlias`, `RecordField`), `descend_into_macros_exact(Token<'db>) ->
SmallVec<[Token<'db>; 1]>`, `resolve_record_pat_field`, `resolve_bind_pat_to_const`,
`resolve_path`, `resolve_path_per_ns`, `resolve_method_call`, `resolve_field`,
`resolve_record_field`, with the handle versions' return types; and `hir::view` re-exported
from `hir`'s `lib.rs`.

## Proof

The crate's tests are the differential test, every expectation unchanged: the same edits on the
same inputs give the same bytes and VFS roots as the text edits, a tree reparsed in place equal
to a fresh parse node for node and error for error (`tests::assert_fresh`, whose `shape` walks
views), and the same touched items as a whole-file diff (`tests/edits.rs`' `own`/`walk` walk
views). `path.rs`' round trips (`find` then `of` gives the path back, for every kind of step,
spacing, `@n` and errors) run on views. `tests/rename.rs` runs rename and the item keys through
`hir::view::Semantics` with the same expected rewrites, touches and keys.

## Speed

A walk is arithmetic on the flat tree's columns: no count bumped or dropped per step (the
handles cloned one per node in `find`, `of`, `members`, rename's token scan and every `cast`),
and no `String` built from a `SyntaxText` to compare an impl's type or a use tree. `hot_lint`
findings for the crate went from 178 to 138; the two new lines (the base `Parse` clone, the
`roots` vector) carry `// clone:` / `// alloc:`. The allocations left are the ones a result
keeps (`find`'s frontier, the path strings, the splices) and error messages.
