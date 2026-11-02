---
status: plan
summary: speed-span-ast — span's ast id maps, real and expansion span maps, and const-ast brought to the v2 speed bar; benchmarks, before, changes, after.
date: 2026-10-29
---
# speed-span-ast: ast id maps, span maps, const-ast

Paths: `crates/span/` (`ast_id.rs`, `map.rs`, `hygiene.rs`, `lib.rs`) and `crates/const-ast/`.
Ids are exactly as `format-span` left them (`HASH 16 | INDEX 10 | BODY 1 | KIND 5`, outline and
full maps equal); nothing here changes an id, a pointer or a span, only how they are kept and
found. No public signature changed.

## Ast id maps (`span/src/ast_id.rs`)

Built for every file (outline at once, each fn body when first asked about), looked up for
every item, macro call and block that hir-def and hir-expand name, compared and fingerprinted
by salsa (backdating, `lru` fingerprints).

**Benchmarks.** SwissTable (hashbrown, abseil): 1-byte tags matched 16 at a time with SIMD at
up to 7/8 load, for keys that are costly to compare. For `u32 -> u32` tables, the fastest known
layout is open addressing with the key inline in the slot and linear probing at moderate load:
a full-key compare is as cheap as a tag compare, and the probe stays in one cache line. A tag
pays only when the key is wide or compared through a pointer. rust-analyzer's `AstIdMap` (the
"before" of v1): an `Arena<SyntaxNodePtr>` with ids as dense arena indices (id to node is one
indexed load), and a hashbrown table from node to index (a hash of the pointer, a probe, a
compare in the arena).

**Before (format-span's v2 map).**
- Outside bodies: `Arena<(SyntaxNodePtr, ErasedFileAstId)>` (16 bytes per entry) and two
  `hashbrown::HashTable<Idx>` (about 5.7 to 11.4 bytes per entry each). Node to id: FxHash of
  the pointer (3 rounds: kind, start, end), the control group, the bucket, then the arena
  entry, so 3 cache lines. Id to node: FxHash of the id and the same 3 lines.
- Per body: a 128-byte `Body` (range, slots, text hash, an `Arc<FlatTree>` cloned per body,
  `OnceLock<BodyEntries>` of 88 bytes) plus 4 bytes in `by_slot`. A walked body was 3
  allocations (a boxed slice and two `FxHashMap`s), about 45 to 80 bytes per id, with hashing
  on each lookup. The body holding a node was a binary search over the 128-byte `Body`
  structs, and an id's body a binary search through `by_slot` into `bodies`, one indirection
  per step.
- Eq/Hash: element by element, 4 hasher writes per entry and 4 per body.
- Building: the next-index map was keyed by `(ErasedFileAstIdKind, u16)` (two hasher rounds),
  and an 8 KB `Vec` of slot bits was allocated per file.

**Changes.**
- Entries are `[u32; 4]` (start, end, kind, id): 16 bytes, `const`-asserted, in the walk's order
  with the root first.
- Two `Index`es, node by start offset and id by raw id: open addressing with `[key, number + 1]`
  slots (8 bytes, 8 per line), a power of two under 3/4 full, a Fibonacci hash (one multiply,
  one shift), linear probing. Entries that share a start are probed in insertion order and
  checked against the entry's full key.
- Bodies are columns: `starts` (searched to find the body holding a node), `rows` (end, first
  slot, slots, text hash), `by_slot` (`[first slot, body]` pairs, sorted and searched in place),
  `ats`, and `OnceLock<Box<[u32]>>` per body. The map holds one `Arc<FlatTree>`, not one per
  body.
- A walked body is one `Box<[u32]>`: its entries sorted by start, then `[id, number]` sorted by
  id. That is 24 bytes per id, one allocation per body that has ids and none for a body without.
  Both lookups are binary searches.
- Eq is 3 `memcmp`s and Hash is 3 `write`s, over the entry words, the body starts and the body
  rows. That is everything Eq compares and no address, as db-salsa's fingerprints need.
- Building: the next-index map is keyed by `kind << 16 | hash` in one `u32` (one multiply), with
  the same entry-API logic. The slot bitmap is an array on the stack.

**After.**

| | before | after |
|---|---|---|
| bytes per id outside bodies | 27 to 39 | 37 to 59 |
| node to id, outside bodies | FxHash of 3 fields, 3 lines | 1 multiply, 2 lines (slot, entry) |
| id to node, outside bodies | FxHash, 3 lines | 1 multiply, 2 lines |
| a body until walked | 132 bytes and an `Arc` increment | 60 bytes |
| a walked body | 3 allocations, 45 to 80 bytes per id | 1 allocation (none if empty), 24 bytes per id |
| lookup in a body | binary search over 128-byte structs, then a hash probe | binary search of a `u32` column, then of the body's slice |
| Eq / Hash | per element, 4 writes per entry | 3 `memcmp`s / 3 `write`s |
| locks or shared writes on a lookup | none (a filled `OnceLock` is one acquire load) | same |

Outside bodies the map uses more memory than with SwissTable (3/4 load and an inline key
against 7/8 load and a 1-byte tag). That is the trade for one cache line less per lookup, and
it only applies to items outside bodies (a few hundred per file). rust-analyzer's id-to-node is
still one line faster: its ids are dense arena indices. Ours are content-derived and stable
across edits and between outline and full parses, so they need a table. Matching it would take
ids that carry their entry's number, which breaks id stability: not worth it.

What stays as it was: a fn body's entries are filled under `OnceLock`, one per body. A shared
column for all bodies would need a shared write (an atomic reservation) on every fill. Building
the map still makes a cursor node per item (a root and an index, no tree copy) so that it reads
names through `ast::*` accessors exactly as the ids were made. Reading names from the flat
arrays directly would save that, but would have to reproduce `Impl::self_ty`/`trait_` and
`MacroCall::path` exactly. Do it after the coordinator's profile, behind the differential test
below.

## Span maps (`span/src/map.rs`)

**Benchmarks.** rustc's `Span`: 8 bytes (`lo: u32, len: u16, ctxt_or_parent: u16`), with an
interner only for wide spans, absolute positions found through `SourceMap`'s sorted file
starts. For offset lookups over dense sorted keys: a branch-free binary search of a narrow
sorted column (Eytzinger order pays only once the column is out of L2).

**`RealSpanMap`** (an anchor per item, one lookup per token of every macro input).
- Before: `(ErasedFileAstId, TextSize)` pairs plus a `TextSize` starts column, 12 bytes per
  anchor. The lookup was already a binary search of the starts and one load. Eq/Hash went pair
  by pair. `within` filtered every anchor.
- After: `anchors: [[u32; 2]]` (raw id, length) and `starts: [u32]`, still 12 bytes per anchor.
  The lookup is `partition_point` over the `u32` column, which std implements branch-free
  (conditional moves, since Rust 1.82), plus one load. Eq is one `memcmp`, Hash one `write`.
  `within` is two binary searches and a slice. Eytzinger order isn't used: `within`,
  `write_spans` and `Display` read the anchors in text order, and a file's few thousand
  anchors (under 16 KB) stay in L1 while its tokens are mapped. `Debug` prints what it printed
  before.
- Cursor (asked for by speed-hir-expand): `span_for_range_from(range, hint: &mut u32)` gives
  exactly `span_for_range`'s span. It searches forward from the anchor `hint` names in steps of
  1, 2, 4, ... then binary-searches the last step, or binary-searches what is before `hint`, and
  stores the anchor it found. Tokens mapped in order cost two compares each while they stay in
  one anchor or move to the next, and `O(log d)` for a jump of `d` anchors. The hint lives with
  the caller (a `Cell<u32>` in hir-expand's mapper), so `SpanMapper::span_for` keeps `&self`.

**`SpanMap`** (one per expansion; `span_at` for every token mapped back).
- Before: `Vec<(TextSize, Span)>`, 24 bytes per span. The binary search strode 24 bytes, so
  every step was a new line. `merge` did retain, extend and an `O(n log n)` sort.
- After: `ends: Vec<TextSize>` and `spans: Vec<Span>`. The search runs over the 4-byte ends (16
  per line, so the last 4 steps share a line), then reads one span. `merge` is two binary
  searches, an in-place shift of the tail and a splice per column: `O(n)` with no sort, giving
  the same order, because the three runs (kept, inserted, shifted) are already sorted. The
  fragment lists no longer build a temporary `Vec`. `Debug` prints the same `spans: [(end,
  span), ..]` as before.
- `span_at_from(offset, hint: &mut u32)`: the same cursor over `ends`, giving exactly
  `span_at`'s span. `with_capacity(n)` lets `token_tree_to_syntax_node` size both columns once.
- Down-mapping (`ranges_with_span_exact`, `ranges_with_span`) scanned every span. Now
  `by_anchor` holds the piece numbers sorted by (file, ast id, start, end, number), 4 bytes per
  piece. It is built under a `OnceLock` on the first down-mapping (`O(n log n)` once, then one
  acquire load), and `push` and `merge` drop it, so it can't go stale. It is not part of Eq,
  Hash or Debug. An exact lookup is a binary search plus the run of equal keys, which is
  already in text order. A containing lookup bounds the pieces with the same anchor that start
  at or before the span with two binary searches, keeps those that end at or after it, and
  sorts them back into text order: `O(log n + c log c)` and one allocation. Both return what
  the scan returned, in the same order.
- The `SpanMapDropper` and `AstIdMapDropper` threads are gone. They cost an mpsc send on every
  drop (a shared write) to defer a handful of frees, and both maps are now a few boxed
  columns. Both maps free in place.
- Not matched: rustc's 8-byte span. A `Span` here is 20 bytes (range 8, anchor 8, context 4),
  because it is relative to an anchor (file and ast id) rather than absolute. `tt` already keeps
  its spans compressed. To match, `SpanMap` would keep per expansion a table of distinct anchors
  and store `(anchor index: u16, start: u24, len: u16, ctx index: u24)`-style packed words,
  decoding to `Span` on lookup. That changes nothing public (`span_at` still returns `Span`), so
  it can be done here once a profile shows expansion maps matter. Today they are small and
  short-lived.

**`SyntaxContext`** (`hygiene.rs`): a context is a `u32` (salsa interned id, roots encoded in
the top values), and each lookup is one salsa interned read, which is db-salsa's (lock-free).
Unchanged. `marks()` collects into a `Vec` to reverse it; its one caller
(`hir-expand/src/hygiene.rs:136`) runs only for a macros 1.0 call inside a macros 2.0
definition.

## const-ast

`Tree` borrows its four arrays, and every view is `Copy`: two words, a `&Tree` and an element
number. Children are stepped over by their subtree ends, tokens read straight from the arrays,
and nothing is copied per element. Nothing to change. The host that writes the arrays
(`hir-ide/src/const_macro.rs`) pushes each element once per call, into the evaluator's memory.

## Unsafe ([unsafe.md](../../design/unsafe.md))

- None of the layouts above needed `unsafe`. Columns are `[u32; N]` arrays viewed with
  `as_flattened` and `as_chunks`, and searched with the safe `partition_point`.
- `const-ast`: no unsafe; `#![forbid(unsafe_code)]`.
- `span`: `#![deny(unsafe_code)]`, allowed only in `hygiene.rs`, the one place a
  `SyntaxContext` becomes a salsa id, through `salsa::Id::from_index`, which has no safe
  version.
  - Before, `SyntaxContext::id` passed any non-root value to `from_index`. Values come from
    persisted bytes (`hir-def/src/hir/persist.rs`) and proc-macro wire formats, so a bad value
    broke `from_index`'s `index < MAX_U32`, and `u32::MAX` made a `NonZeroU32` of 0. Now `id`
    checks that bound before the call, with one compare, so bad bytes panic. What remains of
    `from_u32`'s `# Safety` is a correctness contract, not a memory-safety one (salsa's table
    checks page, slot and type), and it says so. `from_u32` stays `unsafe fn` because ten
    callers in other crates wrap it in `unsafe`, which marks where they trust decoded bytes.
  - `EditionedFileId::edition` read the edition with `transmute`; it is now a `const` match
    that panics on a value no constructor makes. A `const` assert catches a new edition.
  - Every remaining block has a `// SAFETY:` comment with one unsafe op. Tests check that a
    context past the roots and `u32::MAX` panic.

## Tests (beside the code; none run, nothing is compiled yet)

- `ast_id`:
  - `an_index_finds_what_a_linear_search_does`: seeded keys, most shared by many entries, at
    sizes 0 to 5000. Every key is probed in insertion order, with first and last hits, absent
    keys, and a table under 3/4 full.
  - `lookups_agree_with_maps_of_the_stored_ids`: over the parser's test data and `syntax/src`,
    for full and outline maps. For every node of the file, node to id equals an `FxHashMap` of
    the stored entries (the old layout's `by_ptr`); id to node equals one built the other way
    (`by_id`), for every id and two neighbors no node may have.
  - `a_body_keeps_its_ids_sorted_both_ways`: the slice invariants of every body.
  - `equality_and_hash_follow_the_ids`: full equals outline with equal hashes; an item more
    makes another map.
  - `packed_index_keys_number_as_pairs_did`: 60,000 ids from the one-word key equal those from
    the old `(kind, hash)` key, past 1024 per key, so the overflow path runs.
  - The existing tests (outline equals full, lazy walking, 4 threads, 5000 bodies,
    `relative_to`) all still apply unchanged.
- `map`:
  - `anchors_agree_with_a_search_of_the_pairs`: every offset's anchor, `within` and `start_of`
    against a search of the pairs, for 1 to 1000 anchors with repeated starts.
  - `real_maps_are_equal_when_their_lengths_and_ids_are`: Eq exactly when (id, length) lists
    are equal, and equal maps hash the same.
  - `expansion_maps_agree_with_the_pairs`: the old `Vec<(TextSize, Span)>` layout kept as a
    model. Its `span_at`, `spans_for_range`, `ranges_with_span(_exact)`, `iter` and `merge` are
    compared, through 8 merges each, with `span_at_from` (in order, from random hints, in random
    order) and down-mapping of absent spans; the down-mapping index is rebuilt after each merge.
  - `a_search_from_any_hint_is_a_binary_search`: the galloping search against `partition_point`
    for every key, every hint (including past the end) and arrays of 0 to 39 keys.
  - `a_cursor_gives_the_spans_a_search_does`: `span_for_range_from` against `span_for_range`,
    walking in order, from random hints and in random order with a `u32::MAX` start.
  - `expansion_maps_are_equal_when_their_pairs_are`: the same Eq/Hash check for expansion maps.
