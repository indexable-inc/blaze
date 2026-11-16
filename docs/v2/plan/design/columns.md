---
status: plan
summary: How blaze's records compress without leaving the page cache. Lightweight column encodings read in place with random access (frame of reference with bit-packing in blocks of 1,024, a frame against a line, delta, dictionaries, run-length, FSST for strings), chosen per block by size within a bound on what one `get` costs; block compressors only for cold segments and transfer.
date: 2026-11-15
---
# Columns read in place

Rule 51: a record is written once and read in place through the page cache. A general-purpose
compressor (zstd, LZ4, DEFLATE) on that path makes every read a decompress-and-copy, which is
what another lane is taking off the hot path. Records are still big: the first run on `core`
made 8.1 MB of outline records and 4.4 MB of scope records from 14.7 MB of source, and memory is
the third priority. The answer for columns of ids, offsets,
lengths, small enums and names, read a few values at a time and scanned sometimes, is the column
stores' lightweight encodings: each value is found in O(1) or close, decoded alone, from bytes
that stay where they are. The code is `crates/blaze-columns/src/enc/`.

## The schemes

| scheme | from | `get(i)` |
|---|---|---|
| frame of reference + bit-packing (`For`) | Goldstein, Ramakrishnan, Shaft, ICDE 1998; blocks of 1,024 as FastLanes (Afroozeh, Boncz, VLDB 2023) | one load |
| frame against a line (`Linear`) | LeCo (Liu et al., SIGMOD 2024), simplest form: one line a block through its first and last value | one load |
| run-length (`Rle`) with a run index | classic; BtrBlocks (Kuschewski et al., SIGMOD 2023) cascades it | binary search over run starts |
| delta, delta of delta (`Delta`, `Delta2`) | classic; a checkpoint (the first value) every block | a sum over at most one block |
| dictionary, first-use order, codes cascaded into the above | Abadi, Madden, Ferreira, SIGMOD 2006; BtrBlocks | codes `get` + values `get` |
| FSST, a 255-symbol table a column | Boncz, Neumann, Leis, VLDB 2020 | decode one string's codes |
| dictionary of whole strings, its strings plain or FSST | BtrBlocks | one code, then one string |

Patched frame of reference (PFOR: Zukowski, Héman, Nes, Boncz, ICDE 2006) is not a scheme yet:
`block::pfor_estimate` prices it per block (the best width plus a 10-bit position and the high
bits of each exception) and the cost example prints whether it pays on real columns. It is added
when the sample says it saves more than 5%.

## The bit-packing layout

Value `i` of width `w` is at bit `i * w`, least significant bit first (the horizontal order).
`get(i)` is one unaligned 8-byte load at byte `i * w / 8`, a shift by `i * w % 8` and a mask
(above 56 bits a ninth byte too); every column ends with 8 zero bytes so the last load stays
inside it. Eight values are exactly `w` bytes, so the vector unpack (`core::simd`, eight `u64`
lanes) loads each lane at an offset and shifts it by an amount fixed by `w` alone, then masks
all eight at once.

FastLanes' interleaved order puts value `i` in lane `i % L` at row `i / L`, so a whole block
unpacks with only vector shifts and ORs, no per-lane load: the fastest scan known (well under a
cycle a value). Its price is the read of one value: two words of one lane are `L` words apart,
so a value that straddles a word is two loads up to 128 bytes apart, and a delta in that order
also needs the transposed layout. blaze reads records a few values at a time and scans rarely,
so `get` wins: the horizontal order is chosen. The choice is per block (the scheme byte), so an
interleaved scheme can join later for a column that is mostly scanned, without a format change.

## Layout

All little-endian, unaligned, readable as a view over a record's `Slice<u8>`.

- A column of integers: kind 0 (1), `len` (`u32`), a directory of a `u32` start a block, the
  blocks end to end, 8 zero bytes. A dictionary: kind 1, `len`, the values column's size
  (`u32`), the values column, the codes column; neither is a dictionary (no recursion).
- A block: its scheme (1), then `For`: width (1), min (8), packed; `Linear`: width, base (8),
  slope (8), packed `x - base - slope * j`; `Rle`: start width, value width, runs (`u16`),
  value min (8), packed starts, packed values; `Delta`: width, first (8), least delta (8),
  `n - 1` packed; `Delta2`: width, first, first delta, least second delta, `n - 2` packed.
  Arithmetic wraps, and "least" is the signed least, so falling columns pack small too.
- A string column: kind (plain 0, FSST 1, dictionary 2), count (`u32`). Plain: offsets size,
  offsets (`n + 1` ends, an integer column chosen with a constant `get`), bytes. FSST: table
  size, offsets size, the table (count, lengths, symbol bytes), offsets over the codes, codes.
  Dictionary: the strings' size, a plain or FSST column of the distinct strings, the codes.

A block header is 10 to 26 bytes for 1,024 values: at most 0.2 bits a value; the directory 4
bytes a block.

## Choosing (rule 25: equal input, equal bytes)

Each block takes the smallest scheme whose `get` cost (constant, a binary search, a block) is
within the policy's bound; a tie goes to the cheaper `get`. Records take `GetCost::Log`;
string offsets take `Constant`. An exact plan of 1,024 values under five schemes is arithmetic
over 8 KiB, so integer blocks are planned exactly, not sampled. Sampling is where a trial is
dear: FSST's table is built from every k-th string up to 16 KiB, the sample is encoded, and the
column is estimated at the same ratio; a dictionary is built (blaze-kit's `Dict`, one hash a
row) and sized exactly. Only the winner is written.

## Reading (rule 74)

`IntView::open` and `StrView::open` check every header, directory entry, width, run count, size
and the padding once, O(blocks); the view is the proof, and its reads check nothing again and
cannot leave the column's bytes. Values that index something inside the column (offsets,
codes) are clamped where they index, so corrupt bytes give a wrong answer, never a read outside:
their meaning is held by the record's digest, checked once at fetch. The tests flip bytes of
real columns and read everything `open` accepts.

## The floor and the ratio (rule 67)

- `get` on a hot column: the value's cache line, the block header's and the directory's (one
  line serves 16 blocks): a few nanoseconds. The cost example measures it against a plain
  array's `get`; the ratio is reported there, not here, until it has run.
- Whole-block decode: memory bandwidth, about 0.1 ns a value; the horizontal order's vector
  unpack issues eight scalar loads a group, so expect several times that floor, and the
  scalar twin about twice the vector.
- Size: for `For` the entropy of the value range a block; nothing here measures against an
  entropy coder, the ratio to DEFLATE on the plain bytes is printed as a reference only.

## What would shrink (estimates until the cost example runs)

- Outline records (`FileItems`, an `ItemRow` of about 160 bytes): 64 of them are two
  `ContentId`s, which no lightweight scheme compresses. The seven item-relative ranges, parent,
  end, disambiguator and tags (about 90 bytes) become columns of 8 to 16 bits a value, many
  runs of zero: an estimated 40 to 50% smaller record, 8.1 MB to about 4.5 MB on `core`.
- Scope records (`ModuleScope`, a `BindingRow` of about 84 bytes, 64 of them ids): about 15%
  from the narrow fields alone. The large win needs a dictionary of 32-byte ids (the same
  `def` and `vis_key` repeat within a record): the next piece, a dictionary over fixed-width
  byte values beside the `u64` one.
- Names (the string tables of both): names repeat; a dictionary of whole strings, its strings
  in FSST, is expected near 3 to 5 times smaller than plain offsets and bytes.

## What other crates would change (not done here)

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

## Where block compression still belongs

zstd (vendored, with a dictionary trained per record kind) only where nothing reads in place:
whole cold segments of the store that no build has touched in a while (decompressed once into a
segment that is then read in place) and bytes moving between machines (the shared cache, remote
execution). Never on a path a build reads: a lightweight-encoded record is already small, and
zstd on top of it gains little and costs a copy on every read.
