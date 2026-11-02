---
status: plan
summary: The signatures part of a dependency image (frontend.md item 5) is one blaze_format buffer with a sorted column of portable item keys and one record per signature query, so serving a dependency's signature is a binary search and one record decoded in place, with no parse; the bar is rustc's rmeta lazy tables.
date: 2026-10-29
---
# The signatures part of a dependency image (frontend-deps-sigs)

Item 5 of [frontend.md](../../design/frontend.md): a dependency crate's front end is read from
an image, not lowered again. `frontend-deps` keeps the image (def map, lang items, and a
signatures part, keyed by every input). This lane writes and serves the signatures part:
`crates/hir-def/src/deps/signatures.rs`, `deps/signatures/{key,table,codes,tests}.rs`, and the
relocation in `crates/hir-def/src/hir/persist.rs`.

## What is served

The ten signature queries of `crates/hir-def/src/signatures.rs` (`StructSignature::of`,
`UnionSignature::of`, `EnumSignature::of`, `ConstSignature::of`, `StaticSignature::of`,
`ImplSignature::of`, `TraitSignature::of`, `FunctionSignature::of`, `TypeAliasSignature::of`,
`VariantFields::of`) and `EnumVariants::of`. Each `of` first asks
`crate::deps::signatures::served_<kind>(db, id)`: `None` when the item's crate has no image
(the query lowers as today), the image's value otherwise. An image without the item, or a
record that does not read, panics naming the crate, the item and why. There is no fallback to
lowering: a stale or corrupt image is a bug in the key, not a slow path.

Items in blocks are not in the part (a block belongs to a body, which is not served); their
`served_*` is `None` and they lower from their body as today.

## Layout

The part is one buffer of schema `signatures::SCHEMA`:

- one record per (query, item), each its own blaze_format buffer, placed at `ALIGN`;
- `keys: Slice<ContentId>`, sorted: the portable key of each record (`key.rs`: the query, the
  item's module by its index in the crate's def map, its container and its ast id, hashed in
  `KEY_DOMAIN`). The key names no salsa id, so it is the same in every database;
- `places: Slice<Place { at, len }>`, plain, so it is checked in O(1) when the part is opened.

A record (`RECORD_SCHEMA`) is `RecordRoot { words, table, store, what }`: the signature's words
(`hir::persist::Words`), its expression store's bytes, the query it answers, and a relocation
table. Inside `hir::persist::relocating` every salsa id a record writes (files, macro calls, ast
ids, items, modules, blocks, hygiene contexts) becomes a dense index into that table; the table
is written portably with frozen's `Enc` and read with `Dec`, which makes each id in the reading
database. Ids a store keeps outside its words (block scopes, an `Expr::Block`'s id, hygiene
contexts in `IdPair`) are patched to and from table indexes in the store's own bytes
(`table::relocate_store`, `unrelocate_store`). Outside `relocating` the bytes are today's,
so a store written for the local cache is unchanged.

A record is read inside its `of` query, so the blocks it names are made by that query (blocks
are tracked structs). Served blocks are new ids, so tests compare a served value with the
lowered one by the bytes each writes, not by `==`.

## Modes

The image's `Mode` (from `deps.rs`):

- `Serve`: decode and return.
- `Check`: also lower from syntax and record the item through the same writer; a byte
  difference goes to `frozen::record_difference` (part `signatures`). The image's value is still
  returned, so a check run behaves as a serving run.
- `Verify`: re-record what was read and compare the bytes with the record: proves the decoder
  and the writer are inverse on every record served.

## The bar: state of the art and speed of light

State of the art is rustc's crate metadata: `rustc_metadata`'s `LazyTable`s give each
`DefIndex` an O(1) position, and `fn_sig`, `generics_of`, `predicates_of` decode one item on
demand. The benchmark to beat is that: a dependency item's signature queries under
`-Z self-profile` (`metadata_decode_entry_fn_sig`, `..._generics_of`,
`..._explicit_predicates_of`) for the items `cargo check` of a crate like `ripgrep` asks of its
dependencies, against `served_*` for the same items in blaze. Not measured yet (this lane runs
nothing).

Speed of light for one served signature, the work that cannot be skipped:

- the key: one hash of a key under 64 bytes (`SmallVec<[u8; 64]>`, one blake3 block) plus the
  lookups it names (memoized `module_indexes`, an interned loc): about 100 ns;
- the find: open checks are O(1) (a header and two slice bounds), the binary search is
  `log2(n)` 32-byte compares; for a crate of 16k records that is 14 probes, about 50 ns with
  the column cold in L2;
- the decode: a record is a few hundred bytes; reading it at even 1 GB/s is under 1 µs, and the
  work is one `Arc` and one store, the same allocations lowering ends with.

So a served signature should cost about 1 µs and allocate only what the value owns, against
tens of µs for lowering from a parse (item tree, ast id map, the signature's syntax). The
allocation tests hold the find to 0 allocations and a memo hit to 0.

Known costs above that bound, to remove next:

- frozen's `Dec` reads a file's path as a `String` per file in a table;
- a store naming blocks or contexts is copied twice when unrelocated (once to patch, once into
  `Aligned`, which has no `DerefMut`);
- a store read in place decodes with `decode_in(image)`, borrowing its columns from the image's
  `Arc<Aligned>` instead of copying them (speed-hir-store's `Col` owner; pending its
  `decode_in`);
- decoding a nested block asks `block_def_map`, which parses the block's body.

## Which test proves what

All in `crates/hir-def/src/deps/signatures/tests.rs`, on a fixture with every kind (generics,
where clauses, const generics with block expressions, `macro_rules!` items, builtin derives,
`#[path]`, `include!`, an extern block, `const _`):

- `every_served_signature_is_the_lowered_one`: each record read back writes the bytes the
  lowered value writes;
- `every_signature_is_served_through_its_query`: the `of` queries return the image's value;
- `signatures_with_blocks_read_back`: blocks in signatures in `Serve`, `Verify` and `Check`;
- `the_part_is_the_same_bytes_in_every_database`: determinism, and keys independent of ids;
- `serving_a_signature_parses_nothing`: no `parse`, `ast_id_map`, `item_tree`,
  `signature_syntax`, `with_source_map` runs on the serve path;
- `finding_a_record_allocates_nothing`, `a_memo_hit_of_a_served_signature_allocates_nothing`;
- `a_relocated_store_reads_back_as_it_was`, `a_store_outside_a_relocation_is_written_as_before`;
- `checking_finds_a_difference_and_serves_the_image`;
- `a_where_predicate_or_bound_past_its_store_is_refused`, `a_record_id_past_its_store_is_refused`
  (in `codes.rs`): a record's ids are checked against its store, which is read from a buffer of
  its own, before the value is returned;
- `an_image_without_the_item_is_corrupt`, `a_signatures_part_that_does_not_open_is_corrupt`,
  `a_record_of_another_query_is_refused`: the panics, never a quiet lowering.

## Open

Hooks into db's paths (`v2/requests/db.md`, "frontend-deps-sigs: serve signatures from a
dependency image") and the shared test lock (`v2/requests/frontend-deps.md`). Until db lands
them `deps/signatures` does not compile: it uses frozen's id codes, the two `from_parts` and
`EnumVariants::lower`, and nothing calls `served_*`. It has not been built or run yet (this lane
runs nothing); the first build after the hooks land is its first check.
