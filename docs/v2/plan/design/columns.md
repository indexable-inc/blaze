---
status: plan
summary: How blaze's records compress without leaving the page cache. The lightweight column encodings read in place are built (crates/blaze-columns/src/enc); left to build are the record format's column field kind and the item rows as columns; block compressors (crates/blaze-zstd) only for cold segments and transfer.
date: 2026-11-15
---
# Columns read in place

Rule 51: a record is written once and read in place through the page cache. A general-purpose
compressor (zstd, LZ4, DEFLATE) on that path makes every read a decompress-and-copy. Records are still big: the first run on `core`
made 8.1 MB of outline records and 4.4 MB of scope records from 14.7 MB of source, and memory is
the second priority. The answer for columns of ids, offsets,
lengths, small enums and names, read a few values at a time and scanned sometimes, is the column
stores' lightweight encodings: each value is found in O(1) or close, decoded alone, from bytes
that stay where they are.

## Built

The encodings are `crates/blaze-columns/src/enc/`, whose module docs are the format: bit-packing,
blocks of 1,024 under frame of reference, a frame against a line, run-length, delta and delta of
delta, dictionaries, Elias-Fano id sets, FSST and front-coded strings, each read through a view
`open` checks once (rule 74). blaze-scope's item records use them (`items_record/cols.rs`). A
safe zstd for cold and moving bytes is `crates/blaze-zstd` (decoder and encoder).

## What other crates would change (not built)

- blaze-format-core: a `Column<T>` field kind, a `Slice<u8>` whose check is `IntView::open` or
  `StrView::open`, so a record's deep check opens its columns once and a `Verified` reaches
  the view with no second check; the 8-byte padding counted in the slice.
- blaze-surface: `ItemRow`, `BindingRow` and friends become columns of a record (struct of
  arrays) instead of rows: each field one `enc` column; the string tables a `StrView`.
- blaze-columns (the Parquet lane's files): `Values::Dict` and the dictionary page could take
  `enc::ints` codes; nothing merges until Parquet wants it (its pages are Parquet's encodings).
- blaze-parquet `levels.rs` keeps Parquet's RLE/bit-packed hybrid (a spec format, not read in
  place); blaze-kit `codec.rs` keeps LEB128 and its delta (streams, not random access). The
  bit-packing here is `enc::bits`; the varint and zigzag stay in blaze-kit.

- blaze-format-tiers (the record format's warm tier: frame of reference in SIMD-BP128's interleaved
  blocks of 128) is a second encoding of the same concept, built, not wired; the block here is
  1,024 values in horizontal order (`enc::bits::BLOCK`). One of the two crates goes.

## Where block compression still belongs

zstd (vendored, with a dictionary trained per record kind) only where nothing reads in place:
whole cold segments of the store that no build has touched in a while (decompressed once into a
segment that is then read in place) and bytes moving between machines (the shared cache, remote
execution). Never on a path a build reads: a lightweight-encoded record is already small, and
zstd on top of it gains little and costs a copy on every read.
