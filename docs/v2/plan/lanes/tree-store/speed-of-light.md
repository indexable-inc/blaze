---
status: plan
summary: lane tree-store's speed of light: what reading a stored library tree and keeping parses in memory cost at best, and where the design stands against it until the first compile measures it.
date: 2026-10-30
---
# tree-store: speed of light

A flat tree is about 6.5 bytes an element (`crates/syntax-tree/src/flat.rs`), and Rust has
about 0.45 elements a byte of source (a token every 4 to 5 bytes, whitespace included, a little
over half the elements tokens): about 3 KB of tree a KB of source parsed whole. A library file
is an outline: bodies are one lazy node until walked, and bodies are most of library code, so
an outline is about 1 KB a KB. The pinned nightly's `library/` is 56 MB of Rust in 3,072 files:
about 56 MB of outlines, 170 MB with every body walked.

| step | bound | how it is reached |
|---|---|---|
| read a stored outline | the page faults of the bytes touched, plus one pass over the lazy nodes (4 bytes each) and a slot each | columns borrowed from the mapping (`Block::holding`); checks are O(1) a column but the lazy nodes' order, O(lazies) |
| allocations a read | a constant: the tree's handles, its lazy list and slots, its kept hashes | `a_stored_parse_reads_in_place` holds a read under 8 allocations and 64 bytes a lazy node |
| lexing a stored file | none | `a_restart_reads_library_trees_back_without_parsing` counts whole parses |
| writing the section | a copy of each live outline's columns, once per image | `write_section`, records copied on unchanged |
| memory kept | the Parse tag's budget | `Db::admit_parses` once per revision |

Where it stands short until measured: a read still allocates a slot per lazy node (about 36
bytes each), where a slot table in the mapping would need none; that is the one owned column.
