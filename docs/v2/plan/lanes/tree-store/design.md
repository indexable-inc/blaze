---
status: plan
summary: lane tree-store's design: parse memos kept under blaze-mem's Parse budget instead of a fixed LRU, and library files' trees stored in the daemon's image and read in place from its mapping, never reparsed silently.
date: 2026-10-30
---
# tree-store: design

## Parses in memory

`EditionedFileId::parse` (`crates/base-db/src/editioned_file_id.rs`) is a plain method over the
tracked `parse_` (`lru = 0`, `mem_tag = Parse`). A tree's block is charged to the blaze-mem tag
of the task that made it (`crates/syntax-tree/src/flat/budget.rs`): `Parse` for the query, and
a lazy body's part to its tree's tag. Nothing is evicted by count. `Db::admit_parses`, called
once per revision by the driver, sets the memo's capacity from the tag's live bytes against its
budget: over budget, the count of live parses scaled to 7/8 of the budget; under half of it,
doubled until unlimited.

## Trees in the image

One mapping layer: the daemon's blaze-image, section `SYNTAX_TREES` (0x0600), schema
`base_db::trees::SCHEMA` (the layout's description, `syntax::image::TREE_VERSION` and
`syntax_tree::IMAGE_LAYOUT`). The image checks the section's CRCs; the section is read from its
mapping, never decoded.

- Section (`crates/base-db/src/trees.rs`): a 32-byte head (magic, versions, entry count, index
  offset), the records, and an index of 48-byte rows sorted by text `ContentId`, edition and
  kind (outline or full parse), binary searched.
- Record (`crates/syntax/src/image.rs`): the parse's head and errors, then the flat tree's
  record (`crates/syntax-tree/src/flat/image.rs`): its columns as they are in memory, 8-aligned,
  and each parsed lazy body's part, nested. A tree read from it borrows every column from the
  mapped bytes (`Block::holding`), owning only its kept hashes' slots and the lazy nodes' list
  and part slots. Every offset, length, alignment and the lazy nodes' order are checked before
  a column is borrowed, and each part must be its lazy node over the text.

Policy, no fallback:

- A section of another parser version or layout is refused whole by `Trees::open`; the daemon
  reports it and starts without stored trees.
- An entry that is not its text's parse stops the parse query with an error naming the file
  and the image, never a parse in its place. The failure is recorded (`Trees::failed`) so the
  next image carries none of the section's entries on.
- A file with no entry is parsed as usual and stored with the next image; an entry no file read
  this time is copied on while a file still has its text.

`TREE_VERSION` names the parses the parser makes: `crates/syntax/test_data/tree_version` holds a
digest per test file and its test fails when a parse changes under the same version.
