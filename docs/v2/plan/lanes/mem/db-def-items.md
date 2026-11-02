---
status: plan
summary: mem-db-def-items' plan to move hir-def's item-level queries (signatures, attrs, lang items, the import map, diagnostic items, upvars, macro reach, src, deps) onto blaze-mem; the frame each query run opens, the memoized results made once at exact size, the hit paths made allocation-free (attribute flags, the import-map search over sorted keys in place of the FST), the budget tests and the speed-of-light bound.
date: 2026-10-29
---
# mem-db-def-items: hir-def's item queries onto blaze-mem

Part of `mem-db-def` under `mem-db` (`docs/v2/plan/lanes/mem/db.md`, whose contract this follows).
Paths: `crates/hir-def/` except format's files (`expr_store*`, `hir*`, `item_tree*`) and the
siblings' (`nameres*`, `item_scope.rs`, `resolver.rs`, `per_ns.rs`, `visibility.rs`,
`find_path.rs`, `dyn_map.rs`). That leaves `signatures.rs`, `attrs.rs` and `attrs/`,
`lang_item.rs`, `import_map.rs`, `diagnostic_item.rs`, `upvars.rs`, `macro_reach.rs`, `src.rs`,
`deps/`, `lib.rs` and the rest of the top level. Every change is a request:
`v2/requests/mem-db-def-items.md` (sections I0 to I12) has the exact code. This note gives the
design and the reasons.

## The design

Every tracked function in the slice runs under `Tag::Nameres`. The request file's tag table
lists them for mem-db-salsa's tracked attribute.

1. **A query run's temporaries go to its frame.** The query opens
   `blaze_mem::frame::current` as its first line and passes `&'f Frame` down:
   - `file_outline` and `SignatureSyntax::new`: the body cuts, their pointers and ranges.
   - `VariantFields::lower`: the fields lowered before the arena.
   - `EnumVariants::of`: the kept and cfg'd-out variants.
   - The attribute queries that gather lists: `field_attr_flags`, `generic_params_attr_flags`,
     `collect_field_attrs` and its three users, `native_libs`, `doc_aliases`, `cfgs`,
     `target_features`, `define_opaque`, `derive_info`, `must_implement_one_of` and
     `legacy_const_generic_indices`.
   - `own_of_fields`, `crate_notable_traits` and `include_macro_invoc`.
   - The import map's `Collector`: its row table, rows, infos, the BFS worklist and visited
     set, the counting sort's order, and the key buffers.
   - `crate_diagnostic_items` and `diagnostic_items`: the maps, the duplicates, the BFS.
   - `upvars_mentioned_impl`: the binding names, mentions and closures.
   - `MacroReachable`, held in its caller's frame across one fixpoint.

   Values with a destructor (`Name`, `Symbol`, `CfgExpr`, `ImportInfo`, `ModPath`, `Own`) go in
   `Vec<T, &Frame>`. Only `Copy` data goes in `frame.alloc_slice_*`.
2. **A memoized result is made once at its exact size** (contract 4). Each `Box<[T]>` is
   `Box::from(&frame_vec[..])` for `Copy` rows or an exact `drain(..).collect()` otherwise. Each
   map is `with_capacity(len)` then `extend`, and each `ArenaMap` is sized by its last index. This
   replaces every grow-then-`shrink_to_fit` in the slice: the variant arena, the enum's
   `FxIndexMap` and diagnostics, the attribute `ArenaMap`s, `Docs`, the diagnostic-item maps and
   the upvars map. The request gives the result of each query and how it is built.
3. **Hit paths keep their signatures and allocate nothing.** They run outside query runs (IDE
   requests, a later query's reads), so they never call `frame::current`.
   - `AttrFlags::has_word` used to walk the item's syntax for every word except three. The four
     words callers actually ask for (`marker`, `automatically_derived`, `rustc_main`, `default`)
     become `AttrFlags` bits 78 to 81. Every caller in the tree now gets a bit test.
   - `parse_extra_crate_attrs` concatenated and re-parsed the crate's `--crate-attr`s on every
     attribute walk of a crate root and every `hir::Semantics` request. It becomes a memo
     (`extra_crate_attrs`), with the concatenation in a lent `scratch::with_string`.
   - `#[link]` parsing interns in place and keeps the token's `Cow`, with no `String` per key.
     `link_cfg` writes into a lent string instead of calling `format!`.
   - `docs`' trims iterate `doc.lines()` with no `Vec` per comment. `extract_docs` builds in a
     per-worker `Scratch<DocBuffers>` and copies the memo out exact.
   - The import-map search goes off the FST (next section).
4. **No fallback.** Code that runs outside a task cannot reach `frame::current`. For example,
   `upvars_mentioned_impl` loses its `pub`, and the tests that called a query's body directly now
   wrap it in `frame::task`.

## The import map: sorted keys in place of the FST

`search_dependencies` runs once per completion request. Through fst's API it allocated on every
call: the `Vec` of maps, the `OpBuilder`'s boxed streams (one per dependency), each stream's
state stack and key buffer, the union's heap, and the result set. None of these can be reused
through that API. What the FST answers for this search is which runs of one lowercased name an
automaton accepts: `Str` (exact), `Str::starts_with` (prefix) or `Subsequence` (fuzzy).

The keys are already sorted, so they are stored end to end (`keys`, `key_start`, `run_start`):
- Exact is a binary search.
- Prefix is a binary search, then a scan forward while the key starts with the query.
- Fuzzy is a scan of every key. fst visits every key for `Subsequence` too, since no prefix can
  rule a subsequence out.

This is the layout of a sorted term dictionary (Lucene's block terms, LMDB's sorted pages)
without the prefix compression. Prefix compression would save bytes, but it would make the
binary search decode blocks, and the bytes only matter for a cold read. The search then
allocates nothing. `search_dependencies_into` adds into a set the caller keeps, and
`search_dependencies` stays as the wrapper at the edge. fst moves to a dev-dependency for the
differential test, which checks the new keys and run bounds against an FST built the old way,
and the search against the FST's union, in every mode.

## Budget tests (I12)

- **Attribute lookups** (`attrs/tests.rs`): on a warm item, every lookup a later query or IDE
  request makes allocates 0. That covers flags, field flags, repr, must-use, doc aliases, lang
  item, `has_word("marker")`, diagnostic item, export name, docs, and the diagnostic-item map.
- **Lang items** (`lang_item.rs`): a warm lookup allocates 0. A miss of
  `crate_notable_traits` allocates at most 1 (its box).
- **Import map** (`import_map.rs`): after the caller's set has grown once, an exact, prefix and
  fuzzy search each allocate 0, and each matches `search_dependencies`. A build allocates at
  most 9 (its nine columns) and equals the memo.
- **Signatures** (`signatures.rs`): a warm re-request of every signature query (struct, union,
  enum, const, static, trait, impl, function, type alias, variant fields, enum variants)
  allocates 0. A miss of `EnumVariants::of` allocates at most 2 (its map's two tables).
- **`has_word` differential** (`attrs/tests.rs`): the flag answers what the source walk
  answers, for every word on every item.

The misses rest on mem-db-salsa's contract 5: a memo with no heap allocation of its own, and
interning into regions.

## Speed of light

**Hits.** A lookup is the work of one salsa memo read plus a bit test or field load. The memo
read costs a few dependent loads for the ingredient, the slot and the revision check. That is
2 to 4 cache misses cold (about 300 ns) and about 20 ns warm in L1/L2, with 0 allocations.
- `has_word` for a flagged word was a syntax walk: a few microseconds and a `SmallVec` spill
  per call for a crate root's extra attributes, plus a full re-parse of those attributes. It is
  now the memo read: 100× to 1000× fewer instructions.
- `parse_extra_crate_attrs` was a parse per call. It is now an `Arc` count.

**Import-map search, per dependency with \( R \) runs of total key length \( K \) bytes.**
- Exact: \( \lceil \log_2 R \rceil \) key comparisons, each a dependent cache miss into `keys`.
  For std (\( R \approx 5 \times 10^4 \)) that is 16 misses, about 1.5 µs. That is the bound
  for a sorted array. An Eytzinger layout would prefetch the next levels and cut this to about
  half, at the cost of a second copy of `key_start`. Per completion request the search is not
  the critical path, so the plain layout is kept.
- Prefix: that, plus the matching keys read in order (sequential, bandwidth-bound).
- Fuzzy: every key byte read once. For std \( K \approx 600 \) KB, which is about 60 µs at
  10 GB/s. That is the floor for any structure that must test every key, fst included. fst
  adds a state transition per byte through a pointer-chased graph, about 3× to 5× the cycles.
  The keys are 5 to 20 bytes, so a byte loop per key is within 10% of a SIMD `memchr`, whose
  setup costs more than such a key.
- Results: one set probe per match, into the caller's set, which no longer reallocates after
  the first request.

**Builds and lowering (misses).** Each temporary is written once into the frame: a bump of
about 2 ns, growth past the end extends in place, and the drop is free. The old path paid
\( \log_2 n \) reallocations of each `Vec` and map (\( 2n \) element copies) at 15 to 25 ns per
mimalloc call, plus a shrink realloc of each result. What is left is the result's own
allocations, each made once at size: one per non-empty box, map table or arena. These are
exactly the counts the miss tests hold.
- For an import-map build of a crate with \( I \) infos, the bound is the bytes of the
  collection (about 40 bytes an info in the frame) plus the nine columns: about 64 bytes an
  info written once. For std (\( I \approx 10^5 \)) that is about 6 MB, or about 1 ms at
  memory bandwidth.
- The unstable sort of `importables` with an index tie-break gives the same order as the old
  stable sort, without the merge buffer of \( n/2 \). It runs at \( n \log n \) comparisons of
  ASCII-lowercased names.

**Signatures.** A signature's lowering is bounded by its syntax walk. The frame removes the
per-node allocator calls (fields, variants, body cuts). The remaining cost is the
`ExpressionStore` and `GenericParams`, which are format's. `ExprCollector` keeps its
temporaries on hir-def's pooled thread-local scratch, at 0 allocations warm, so the nine
signature queries open no frame of their own (I0 and I1.6 withdrawn).

## Left to others

- **To mem-db-def-hir.** The `hir` wrapper (`query_external_importables`) calls
  `search_dependencies_each` and dedups with a per-worker seen-set, so the order is the order
  found.
- **To mem-db-def-resolve.** The `Resolver` that `upvars`' visitor walks with is
  mem-db-def-resolve's.
