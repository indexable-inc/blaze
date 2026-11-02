---
status: plan
summary: frontend.md item 14 — a file is reparsed from its previous parse and its item tree is made from its last one, kept beside salsa, re-lowering only the items an edit touches, the tree byte-equal to the whole-file lowering.
date: 2026-10-29
---
# Reparse and re-lower only what an edit touches (frontend-deps-reparse)

Item 14 of [frontend.md](../../design/frontend.md): reparse only the items an edit touches, and
re-lower only those items into the item tree.

## State of the art

- **rustc** has no incremental parse: it parses every file of the crate again and makes
  incrementality from queries over HIR owners (`hir_owner`, `hir_owner_nodes`), each item's
  HIR hashed (`HashStable`) so a query reading one owner is a firewall for the others.
- **Roslyn** keeps a red-green tree: green nodes are position-free and shared, an edit rebuilds
  the green spine above the change and reuses every untouched green subtree; red nodes add
  positions on demand. Its incremental parser reuses old nodes the lexer agrees on.
- **rust-analyzer** reparses a single token or a `{}` block in place (`reparsing.rs`), else the
  whole file, and lowers an item tree per file: any edit outside a body re-lowers the whole file,
  and its `AstIdMap` firewall stops the edit from reaching name resolution only when the ids
  hold.

blaze takes the Roslyn shape for parsing (the flat tree's position-free `hash_subtree` is its
green identity) and the rustc shape for lowering (one query per item, keyed by what it reads).

## Parsing from the previous parse

Superseded on 2026-10-29: db's D8 (`base-db/src/reparse.rs`, over `Parse::reparse_onto`)
keeps each file's last parse instead. `SourceFile::parse_file` and `parsing/kept.rs` had no
caller, so they were deleted with their tests. `Parse::reparse_to` and `reparse_onto` stay.
What follows is the design as it was.

`SourceFile::parse_file(key, text, edition, outline)` (crates/syntax) kept the last parse of each
file in a small direct-mapped table (`parsing/kept.rs`, 128 slots by file id) and, when one was
there of the same mode, made the new parse with `Parse::reparse_to`: the edit is the
difference between the kept tree's text and the new text (`edit_between`: common prefix and
suffix, compared 64 bytes at a time, backed off to char boundaries), and
`incremental_reparse_onto` reparses the smallest token, block, body or item holding it, the
tree sharing the new text. Several edits since the kept parse compose by construction: the
difference is taken from the text the kept tree holds, never from a log of edits, so nothing
stale is trusted, and a kept parse from another database is as good as its text.

When no reparse applies (no kept parse, the edit spans items, the relexed token changes kind),
the file is parsed whole. That is not a fallback: `reparse_to` gives the tree a full parse of the
new text gives (`reparse_to_random_edits` holds them equal), so the two paths are one result
reached at two costs. The outline choice is kept: an outline is only reparsed as an outline.

base-db is db's: the request in `v2/requests/db.md` (`## frontend-deps-reparse: parse a file
from its previous parse`) gives the exact edit to `EditionedFileId::parse`, one call to
`parse_file` keyed by the file's span id.

## The item tree per item

`file_item_tree_query` is one query per file, with no query per item: per-item salsa queries
cost more than lowering the item (a tracked struct matched and a memo verified, about 250
cycles an item an edit, against about 170 for lowering it), so they could at best tie a
whole-file lowering. A source file's tree is made from its last one instead
(`item_tree/items.rs`), as `syntax::parsing::kept` makes a parse from the last parse:

- `KEPT` is 128 direct-mapped slots, each one database's file in one crate (the database's
  `Nonce`, the file, the crate): its last tree (`Arc<ItemTree>`, the same `Arc` salsa's memo
  holds, so keeping it copies nothing), the crate's `cfg` set it was lowered under, and per
  top-level item a key and its `Marks`. A slot is taken out under its lock while its tree is
  worked from; one another file took, or another thread holds, is a whole lowering, never a
  wrong one.
- A key (`item_keys`) is what lowering reads of the item: a BLAKE3 fingerprint of its syntax
  (the flat tree's position-free subtree hash, with the bodies of `fn`s, `const`s and `static`s
  and the items of `impl`s and `trait`s left out but for the inner attributes at their start,
  and a subtree of 4 KiB of text or more fed as its kept `subtree_hash`, so a big
  `macro_rules!` is read once, not on every edit), and, as they are, the ast ids of the items
  it is made of (`[start, kind, id]` from its start) and the span anchors lowering looks up in
  it (`[start, id]`, the anchors of an `impl`'s or `trait`'s items left out), in two arrays all
  the items share (`Keys`). All from the item's start, so an item keeps its key wherever an
  edit moves it.
- Only the items an edit touched are hashed (`Unchanged`). The slot keeps the text its keys
  were made of; the edit's text is compared with it from each end (`common_ends`, 4 KiB
  `memcmp`s). An item in the common suffix at its old place moved by the edit, or in the common
  prefix with two whole items after it there, keeps its old fingerprint: a file's parse is a
  function of its text, the lexer starts a token at an item's start the same way whatever came
  before, and the parser reads a top-level item from the item list's state, its own tokens and
  at most three tokens past its position, which the two items hold. Its ids and anchors are
  copied from its old key unless the bulk compare of the maps (`Unchanged::touched`, below)
  marked it, as an edit elsewhere can renumber them.
- `Marks` are where an item's rows begin in each of the tree's 28 columns (`kinds` standing
  for `data` and `ast_ids` too), then where the last item's end. The whole-file lowering
  records them at each top-level item (`Ctx::lower_module_items_marked`), and assembly at each
  item it places.
- A file's first lowering is `lower_whole` and makes no keys: a file never edited (every
  dependency) never pays for them. Its next lowering (its first edit, or a new `cfg` set) is
  `lower_whole` again, keying the items.
- After that, an edit keys the new parse's items and compares them with the kept keys from
  each end. The items of the common prefix and suffix keep their rows; those between are
  lowered alone in the file's own context (`Ctx::lower_item`, the file's ast id map and span
  map, which give the ids and spans the whole lowering gives). The top level's attributes
  (`top_attrs`: the file's inner attributes and, for a crate root, the command line's) are
  lowered again every time. If every key and the top level are the same, the kept `Arc` itself
  is the result, which salsa compares by address and backdates.
- `assemble(top, runs, marks)` builds the tree from the top level and runs of rows (a run of
  kept items, or one item lowered alone) in one pass into the tree's one block (`block.rs`: a
  header of column ends, then every column), allocated once at every column's summed length,
  moving each id by where its run's rows go less where they were: items and attribute
  owners by the item base, payloads by their kind's column, `children`, `use_trees`,
  `use_tree_lists`, `ctor_vis`, names, paths, visibilities, attributes and `cfg`s by theirs,
  sentinels (`NameId::NONE`, `AliasId::NONE`/`UNDERSCORE`, `PathId::NONE`, the builtin
  visibilities, `IdRange::NONE` of `mod m;`, an empty constructor list, a non-prefixed `use`
  tree's list, `AttrOwner::ENABLED`) left as they are.

The query reads the crate's `cfg` set on every path, so salsa reruns it when the set changes even
if nothing lowered reads it, and the kept tree is not reused under another set.

For a tree made of kept rows to be the whole-file tree to the byte, names, paths and visibilities
are deduplicated within each top-level item only, in every lowering (`Scoped` in `lower.rs`; the
whole-file lowering seals the scope after each top-level item), so no name is rehashed on
assembly. A tree's tables can hold a name once per item that uses it; the persist schema is
bumped to 4 as the tables' contents changed.

Macro files and blocks (`block_item_tree_query`) keep the one-pass lowering (`lower_whole`): an
expansion is made anew and a block's tree is small.

Known costs, all bounded (the bounds and the arithmetic are in
[speed-of-light.md](speed-of-light.md), section frontend-deps-reparse):

- An edit reads ast ids and anchors from the maps, one hash probe and one binary search an
  item, only for the items `Unchanged` doesn't keep and the kept ones the bulk compare marks.
  The slot keeps the last `AstIdMap::entries` and `RealSpanMap::anchors`. `Unchanged::touched`
  walks each array once, old and new together, over the text the edit left unchanged (the
  suffix's moved back by the edit's length). It pairs entries by place and kind, a pair counts
  only if its id is equal too, and every other entry marks the new item holding its start.
  That is about 1 µs for 500 items, against 13 µs for reading every item. A renumbered id
  (a second `fn f` before another) marks the one item holding it. A run out of step with the
  other side marks more items, never a wrong reuse. Only the touched items and the two before
  them are hashed.
- A slot holds 32 bytes of key, about 25 of ids and anchors, about 30 of the maps' entries and
  anchors and 112 of marks per item of its file, and a count of the text the keys were made
  of, only for a file edited at least once.
- An edit inside a `fn` of an inline module re-lowers the module: it moves the module's later
  items, whose starts are in its key.
- Attributes are cloned on assembly, which allocates for those holding paths or inputs.

Nothing in `file_item_tree_query` parses a file other than the one asked for.

## What each test proves

crates/syntax:

- `reparse_to_random_edits`: one to three composed random edits on every syntax source, full and
  outline, reparsed from the old tree, give the full parse's tree, errors and text, the tree
  sharing the new text.
- `edit_between_cuts_the_common_ends`: the edit found leaves no common first or last char and
  rebuilds the new text, across multi-byte chars and long runs.
- `edit_between`'s allocation test: finding the edit between two texts allocates nothing
  (`blaze-alloc-count`). `a_kept_parse_makes_the_parse_of_the_new_text` and
  `taking_and_keeping_allocate_nothing` were deleted with `parsing/kept.rs` (2026-10-29).

crates/hir-def/src/item_tree:

- every fixture of `tests.rs` (`check`, `owned_tree`): the tree `items::file_item_tree` makes,
  three times so the first lowering, the keyed one and the one from the kept tree are each
  made, equals the whole-file lowering, bytes included, and is the tree `file_item_tree` gives.
- `real_files_assemble_to_their_whole_trees`: the same for every source of the item tree, name
  resolution and the syntax tree, one database across them.
- `random_edits_assemble_to_a_fresh_whole_tree`: after each of six random edits per file
  (inserts of items, attributes, `cfg`s, visibilities, macros, braces, or text copied from the
  file), the tree the edited database makes from kept rows and re-lowered items equals, to the
  byte, the tree a fresh database lowers the text to whole.
- `an_edit_inside_bodies_lowers_no_item`: edits inside `fn`, `impl` method, `trait` method and
  `const` bodies of a keyed file lower no item (a count of the items lowered, whole or alone),
  the tree still whole; `an_edit_inside_a_body_gives_the_kept_tree`: the result is the kept
  `Arc` itself.
- `an_edit_of_one_item_lowers_it_alone`: renaming a `fn`, changing a field's visibility, making
  an `impl` a trait impl, adding `pub`, adding an item, changing an inline module's item, adding
  an inner attribute to a `fn` body each lower exactly one item; `removing_an_item_lowers_none`.
- `a_big_item_keeps_its_tree_across_an_edit_elsewhere`: a `macro_rules!` of more than 4 KiB is
  not lowered again for an edit of another item, and is for an edit of its own.
- `an_edit_hashes_only_the_items_it_touched`: an edit inside one of 40 items' bodies hashes 3
  items (it and the two before it), an item appended hashes 3, the first item removed hashes 1,
  each tree still whole; `common_ends_are_the_longest_that_do_not_overlap` holds the compare to
  a byte-by-byte one. The counting tests see their own slots: under `cfg(test)` the slots are
  per thread.
- `an_edit_reads_ids_only_for_the_items_it_touched`: in a 40-item file, a body edit of the
  same length, one that is longer, and an item added at the start read the maps' ids and
  anchors (counted by `READ`) for only the items they hash, the ones `Unchanged` doesn't keep.
  Each tree is still whole.
- `a_renumbered_id_reads_the_item_holding_it`: a second `fn f` added before another renumbers
  the later one's id in the unchanged suffix. Exactly one kept item more than those hashed is
  read, and the tree is still whole.
- `attributes_are_found_as_a_search_of_their_owners_finds_them`: the rank index `attrs` reads
  gives what a binary search of the owners gives, for every item of a tree of more than two
  blocks of 64.
- `trees_asked_again_allocate_nothing`: a memo hit of `file_item_tree`, and a file whose items
  all kept their keys, allocate nothing.
- `assembly_allocates_once_a_tree`: assembling a file of 4 items and one of 64 allocates once
  each, the tree's block, and gives the whole-file tree.
- `reading_a_tree_allocates_nothing`, `expanding_use_trees_whose_paths_are_interned_allocates_nothing`,
  `a_warm_builder_lowers_a_file_in_one_allocation` (the tree's block),
  `decoding_a_tree_allocates_its_block_and_its_paths` (the block, and each decoded path's
  segments) and `a_builder_after_an_item_of_many_names_lowers_as_a_fresh_one`
  (speed-hir-sol-items).
- `block.rs`'s tests: columns read back what was written, an empty tree allocates nothing, a
  tree is one allocation, and a fill that panics, leaves a column short, writes past a column's
  end or fails drops what it wrote.

The budgets count only once hir-def's test binary installs `blaze_alloc_count::Counting`
(requested from db).
