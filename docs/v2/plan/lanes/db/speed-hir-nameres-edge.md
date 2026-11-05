---
status: plan
summary: speed-hir-nameres-edge — find_path and the import map at the v2 speed bar; candidates as handles into one segment arena, the import map as columns with a CSR of infos, search compares bytes in place; benchmarks, before and after.
date: 2026-10-29
---
# speed-hir-nameres-edge: `find_path` and the import map

`find_path` (`crates/hir-def/src/find_path.rs`) finds the shortest visible path to an item from a
module; the import map (`crates/hir-def/src/import_map.rs`, the salsa query `ImportMap::of`) lists
every publicly importable item of a crate with the names and modules it is reachable by, plus an
FST over the lowercased names for search. Neither is on a build's path: a diagnostic run calls
`find_path` once per type or item it prints (`blaze-diagnostics`' `print/paths.rs`, `lower/*`),
and `search_dependencies` once per unresolved name it suggests an import for. Over a large
workspace that is thousands of calls per run, each one reading the import maps of every
dependency, so both must cost little per call and the import map little per entry.

## Benchmarks

| what | how it works | cost |
|---|---|---|
| rustc `TyCtxt::def_path_str` with `visible_parent_map` | one BFS over each crate's public module graph, cached per session; a path is a walk up parent links | one map entry per def (a `DefId` pair, 16 bytes); a lookup is a hash probe per segment; no candidates compared |
| rustc `find_similarly_named_*` (resolve diagnostics) | collects the candidate names in scope, then `edit_distance` per candidate | a `Vec<TypoSuggestion>` per failed name; a `Symbol` per candidate, no `String` |
| rust-analyzer's `find_path` (this file before) | recursive search over the import map's containers and the local module tree, every candidate a `ModPath` | see "before" below |
| the `fst` crate's automaton search | `Str`, `starts_with`, `Subsequence` automata over the shared-prefix FST; a union stream over many maps | the bytes of the FST (a few bytes a name, prefixes shared); no allocation per match |

rustc answers "a path to this def" with one parent per def, computed once; that is cheaper per call
than any search but answers a narrower question (rustc's path, not the shortest path visible
from a given module, preferring the prelude, `std` over `core` by config, and so on). `find_path`
keeps rust-analyzer's choice rules exactly (every expect-test keeps its text), so it cannot be a
parent-link walk; it matches rustc on what it allocates per call instead, and the import map is
the cached per-crate BFS rustc's `visible_parent_map` is.

## `find_path`

Before, per call:

- each candidate path was a `ModPath`. A `ModPath` is now an interned segment slice
  (`hir-expand`), so each `push_segment` of a candidate hashed the whole path and probed (and on a
  miss inserted into) the intern table under its shard lock: one intern per segment per
  candidate, almost all of them for candidates that lose;
- each `calculate_best_path` search allocated a fresh `FxHashSet` of visited `(item, module)`
  pairs;
- each `find_local_import_locations` collected a fresh worklist `Vec`.

A typical call for an item of a dependency reached through a re-export compared 5 to 20
candidates: some 10 to 40 interns and 3 to 10 heap allocations.

After:

- a candidate is a `Choice`: `Copy`, at most 28 bytes (kind, the index of its last segment,
  the segment count, the text length, stability, the prelude bit), compared by those integers.
  Its segments are a chain in `FindPathScratch::segments` (16 bytes a `Segment`: a `Name` and the
  index of the segment before it), so extending a candidate by one name pushes one segment and
  shares the prefix; nothing is copied.
- the chosen candidate becomes a `ModPath` once (`Choice::into_path`: the chain read back into a
  `SmallVec<[Name; 8]>` on the stack, then one intern).
- the visited set and one worklist per recursion depth live in the scratch; each local search
  removes the pairs it added, so the set is empty between top-level searches and is never
  re-allocated.
- `find_path_with(&mut FindPathScratch, ..)` lets a caller printing many paths keep one scratch:
  after the first calls have grown it, a call allocates nothing and interns once. `find_path`
  (what callers use today) makes a fresh scratch: the segment `Vec`, the set and one worklist per
  depth grow once each (about 3 to 6 allocations), still with no allocation or intern per
  candidate.

Per module the local search reaches, `ItemScope::name_of` was a scan of one namespace column
(40-byte packed entries). At this lane's request `speed-hir-nameres-scope` added `by_def`: the
entries sorted by `(namespace, def, position)`, built on a scope's first query, 4 bytes an
entry, so `name_of`/`names_of` are a binary search (a scan only in scopes of at most 8 names).
A local search is now `O(modules · log names)` instead of `O(names in the crate)`, with the
same first name in column order as before.

## The import map

Before: an `FxIndexMap<ItemInNs, (SmallVec<[ImportInfo; 1]>, IsTraitAssocItem)>` (an entry: its
hash 8, the item (16 to 24 bytes), a 32-byte `SmallVec`, the flag, padded to about 72 bytes plus an
8-byte index slot; a second info spilled the `SmallVec` to the heap, one allocation per such
item) and `importables: Vec<(ItemInNs, u32)>` (24 bytes per info). Building the FST lowercased
each key into a fresh `String`. Search looked each match up again in the map by item (a hash per
match). About 104 bytes for an item with one info, one allocation per multi-info item and per FST
key.

After: columns over a row per item.

| column | bytes |
|---|---|
| `items: FxIndexSet<ItemInNs>` (the row of an item in one probe) | about 32 a row (hash, item, index slot) |
| `info_start: Box<[u32]>`, a CSR of infos per row | 4 a row |
| `infos: Box<[ImportInfo]>` (`const` asserted 24: a `Name` symbol, the module id, 3 flag bytes) | 24 an info |
| `trait_assoc_items: Box<[u64]>`, a bitset | 1 bit a row |
| `importables: Box<[u32]>`, infos sorted by lowercased name | 4 an info |

About 64 bytes for an item with one info, 28 for each further one; no allocation per row or
info. The infos are grouped by row with one counting sort (stable within a row), the
importables sorted once, and the FST keys written into one reused buffer. Search maps a match
straight to its info (`importables` holds info indices) and its row by a binary search of
`info_start`; no hash per match. `SearchMode::check` compares bytes in place: case-insensitive
exact and prefix by `eq_ignore_ascii_case` on byte slices, fuzzy by a byte scan for ASCII query
chars and `match_indices` only for non-ASCII ones; no `String`, no
`to_lowercase` per candidate. The prefix check on bytes also no longer panics when the query's
length falls inside a char of the candidate (the old `&candidate[..query.len()]` did).

The walk over public modules still uses an `FxHashSet<ModuleId>` for visited modules: a crate's
import map reaches modules of other crates through re-exports, so there is no dense index over
them; it runs once per crate per revision.

## Tests

- `find_path::tests::differential_against_reference`: the old `find_path` (every candidate a
  `ModPath`, fresh visited sets) kept in the test as the model; over 16 seeded fixtures of three
  crates (random module trees, `pub`/`pub(crate)`/private modules and imports, renames, globs,
  enums, traits, `#[macro_export]` macros, a `std` with a 2018 prelude), every declared item in
  both namespaces and every variant, from every module of `main`, every prefix kind, both
  `ignore_local_imports`, three configs: the same answer, with one scratch reused across all
  calls; and each path resolves back to the item.
- `find_path::tests::shortest_against_brute_force`: a BFS enumerating paths by `find_path`'s
  rules gives the fewest segments; `find_path`'s path has exactly that length, over 32 seeds.
- `import_map::tests::differential_against_reference`: the old map built the old way; the same
  rows in the same order, the same infos and trait flags per row, the same importables, the same
  FST bytes, and `search_dependencies` the same results for exact, prefix and fuzzy queries,
  either case, every associated-item mode.
- `import_map::tests::search_mode_against_reference_check`: 4000 random query/candidate pairs
  (ASCII of both cases, `_`, digits, `あ`, `é`, `É`): the new `check` equals the old wherever the
  old did not panic; `prefix_query_ending_inside_a_char` pins the case it panicked on.
- `find_path::tests::choice_layout`: `Segment` is 16 bytes, `Choice` at most 28.
