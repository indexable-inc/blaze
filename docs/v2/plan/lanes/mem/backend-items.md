---
status: plan
summary: mem-backend-items - blaze-items's tables onto blaze-mem: a crate's key index and store list in reserved regions tagged Tag::Other, every index's building rows in task frames (scopes' tagged Tag::Nameres), preimages in scratch, frames kept out of the recursive hash chain, what is not moved and why, and the tests that hold it.
date: 2026-10-29
---

# mem-backend-items: the item tables onto blaze-mem

Part of [backend.md](backend.md), under lane mem's [memory.md](../../design/memory.md). Every
change is in `crates/blaze-items`. Keys, the four item hashes, the bound hash, fact values and
index values are the same bytes as before; what any query returns is the same, in the same
order. Speed of light: [speed-of-light-items.md](speed-of-light-items.md).

## What lives how long

| what | lifetime | was | is |
|---|---|---|---|
| a crate's key index (`crate_items`, `crate_inner_items`) | the memo | a `HashMap<ItemKey, Resolved>` grown by doubling, rehashed as it grew | `KeyIndex`: rows in a `RegionVec` reserved for `MAX_KEYS` (2^24) and committed as the crate is walked, sorted by the key's bytes 8..16, and a directory of `2^bits + 1` starts allocated once (`key.rs`) |
| a crate's list of stores to look inside | the memo | a `Vec` by doubling | a `RegionVec`, shrunk to its length |
| one store's keys (`inner_items`) | the memo | a `HashMap` | an exact `Box<[Row]>`, sorted and distinct, built in the query's frame |
| the rows an index is sorted from (`trait_impls*`, `inherent_impls*`, `traits_*`, `scope_names`, `crate_hashes`, `FactSet`) | one query | `Vec`s by doubling, `collect`s | `Vec<_, &Frame>` in the query's frame; the columns made at their final length (`table::column`) |
| a key's preimage (`hashed`, `Batch`), a crate's cfg atoms | one call | a fresh `Vec<u8>` a key, a `Vec<(Vec<u8>)>` of atoms | the thread's scratch bytes, `u32`s and `u64`s (`scratch::with_*`, `lend_*`); atoms are packed `start << 32 | end` offsets into one buffer |
| a cycle's colors, keys, order, ranks; Tarjan's stacks (`hash.rs`) | one component | heap `Vec`s | a frame each, opened only where nothing below recurses (see below) |
| a bound hash's MIR walk (`bound.rs`), a fact's candidates, constant blocks, derive impls, anonymous consts (`facts.rs`) | one query | heap `Vec`s, `format!`, `to_string` | the query's frame; names and errors written into `scratch::with_string` |

A region that cannot be reserved or committed, or an index past `MAX_KEYS`, stops with
`blaze-items: <which index>: <MemError>` (`key::stop`); nothing falls back to the heap.

## Tags

- `Tag::Other` for the key index, the store list and every frame except scopes': a key, a hash,
  an impl or trait index and a fact are item identity and incrementality, used by every
  subsystem (codegen keys, test selection, the store's diff), and owned by none of the tags
  there are.
- `Tag::Nameres` for `scope_names`'s frame: what a name is in a scope is name resolution's
  answer, read by its facts.

## Where a frame may open

`frame::task` keeps `DEPTH` (8) nested frames warm per thread; a deeper one maps and unmaps a
4 GiB region. The hash queries recurse as deep as the graph of refs (`interface_of` ->
`component_hash` -> `fill` -> `interface_of` of a ref), so a frame held across a fill would nest
once per level. The rule the code keeps:

- `component` (Tarjan) runs in a frame that closes before any fill: it reads forms only, and
  forms never ask for a hash.
- `component_hash` gathers what the members are spelled from (owns, refs, fills) on the heap,
  then opens a frame for the coloring rounds, which call key queries only.
- A component of one item with no ref to itself (most of them) is streamed straight into one
  `Hasher` (`hash::alone`): no list at all. It is the bytes `Batch::hash_one` gave, since a
  BLAKE3 hasher fed in pieces equals one fed the whole.
- `bound_of`, the fact writers, the index queries and `inner_items` open one frame each: none of
  them is asked for by a hash, so the frames nest only as deep as those queries do (a block's
  store inside a block's store for `inner_items`).
- Form spelling (`form.rs`) nests as types do and opens no frame.

## What is not moved, and why

- `form.rs`'s buffers: a form takes its digest, spans, refs and slot map from a thread-local
  pool of 64 (`SPARE`), given back when the form is finished. `scratch` keeps 4 a kind, and
  spelling nests deeper than that, so moving it would allocate more, not less. A deeper typed
  shelf would let it move (request to lane mem).
- Exact-size memo values (`HashColumns`, `FactSet`, `Runs`, `Keyed`, `ScopeNames`, `inner_items`):
  a `Box` each, allocated once at its length. A region each would be one mapping per memo, and
  there are millions of memos against Linux's 65 530 mappings per process. Values in revision
  arenas are salsa's to give (request).
- `ScopeEntry::Assoc` and `ScopeEntry::Module`'s legacy macros: `scope` returns an owned entry,
  so a lookup boxes them. An entry that borrows the memo is an interface change (request).
- `FxHashMap` / `FxHashSet` / `HashMap` (Tarjan's index, `bound`'s seen set, a module's names):
  std's maps take no allocator. Each is one table at its final size per call.
- `ContentId::of_many`'s result, `module_item_keys`, `crate_auto_traits`, `crates_by_prefix`,
  `changed_since`, `Unhashable::describe`, `key::path`: the memo's or the caller's own value.
- Nothing in the crate comes and goes with an ID: items are salsa's, so there is no `Slab` or
  `Pool` here.

Each of these is marked `// alloc:` where it is, and `tools/v2/hot_lint.py` finds nothing else
in the crate.

## Invariants the code relies on

- `KeyIndex::new` sorts by `(order, key)`; `bucket` is monotone in `order`, so a key's run is
  `starts[b]..starts[b + 1]` for its bucket `b`. A key given twice must name one thing (the
  assertion and its message are the old ones).
- The order of a crate's rows is no part of any answer: `crate_item_ids` feeds only
  `crate_hashes`, which sorts by key.
- `Batch::finish` gives the scratch buffers back after hashing, and every parent key is computed
  before a preimage is written, so nested key queries take other buffers off the shelf.

## Tests beside the code

`src/tests/mem.rs`:

- `a_key_index_answers_as_a_map_of_its_rows`: 0 to 50 000 keys (2 MB of rows, past the region's
  first commit steps) with repeats, against a `HashMap`, present and absent keys, two prefixes.
- `a_key_index_of_two_crates_and_their_inner_keys_finds_each`; the rows' order is not part of an
  index; `a_key_index_refuses_a_key_naming_two_things`.
- `hash_columns_answer_as_a_map_of_their_rows`, with unhashable rows.
- `a_warm_lookup_allocates_nothing` (key index, fact set, bitset rank),
  `an_append_past_the_high_water_mark_allocates_nothing` (20 000 rows, then at most one
  allocation to index them), `a_warm_resolve_and_crate_hashes_allocate_nothing` (a fixture's
  `resolve` and `crate_hashes` lookups after one cold pass).

The existing tests (`hashes.rs`'s keys round trip and stable hashes, `table.rs`, `facts.rs`,
`bound.rs`) hold the bytes. Nothing here was built or run (the brief forbids it): the first
`blaze test -p blaze-items` is the check.

## Follow-ups

- Size `MAX_KEYS` per crate from its def map: three regions a crate reserve about 2 GiB of
  address space (no memory), which a constrained `ulimit -v` would refuse for a thousand crates.
- `form.rs`'s pool onto deeper typed scratch shelves, once lane mem has them.
- `ScopeEntry` borrowing the memo, and salsa memo values in revision arenas (requests).
