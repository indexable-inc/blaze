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
inside it. Eight values are exactly `w` bytes, so a group's shape is fixed by `w` alone: the
vector unpack does one 16-byte load per part, one byte shuffle (`swizzle_dyn`: `tbl` on NEON,
`pshufb` on SSSE3) from a per-width constant table moving each value's bytes to its lane, a
per-lane shift and a mask (up to 25 bits two parts of four `u32` lanes, up to 56 four parts of
two `u64` lanes, scalar above).

FastLanes' interleaved order puts value `i` in lane `i % L` at row `i / L`, so a whole block
unpacks with only vector shifts and ORs, no per-lane load: the fastest scan known (well under a
cycle a value). Its price is the read of one value: two words of one lane are `L` words apart,
so a value that straddles a word is two loads up to 128 bytes apart, and a delta in that order
also needs the transposed layout. blaze reads records a few values at a time and scans rarely,
so `get` wins: the horizontal order is chosen. The choice is per block (the scheme id), so an
interleaved scheme can join later for a column that is mostly scanned, without a format change.

## Layout

All little-endian, unaligned, readable as a view over a record's `Slice<u8>`.

- A column of integers: kind 0 (1), `len` (`u32`), a flat directory of one 24-byte entry a
  block, the bodies end to end, 8 zero bytes. A dictionary: kind 1, `len`, the values column's
  size (`u32`), the values column, the codes column; neither is a dictionary (no recursion).
- An entry: `base` (8), `slope` (8), the body's start (`u32`), width, scheme, second width, a
  zero byte. A body is only packed values. `For` is `base` = min, `slope` = 0; `Linear` a line
  `base + slope * j`; both read `base + slope * j + packed[j]`, one formula, so `get` is index,
  entry, load, with no header parsed and no dispatch between them. `Rle`: `base` = value min,
  `slope` = runs, the two widths; body: packed starts, packed values. `Delta`: first, least
  delta; `n - 1` packed. `Delta2`: first, first delta; body: least second delta (8), `n - 2`
  packed. Fields a scheme does not use are zero and `open` refuses others (one encoding).
  Arithmetic wraps, and "least" is the signed least, so falling columns pack small too.
- A column of ids (`[u8; W]`, `W` = 32 for `ContentId`): kind 3, `W`, the distinct count
  (`u32`), the distinct ids in first-use order, then the codes as an integer column with a
  constant `get`. `open` checks every code is below the count, once.
- A string column: kind (plain 0, FSST 1, dictionary 2), count (`u32`). Plain: offsets size,
  offsets (`n + 1` ends, an integer column chosen with a constant `get`), bytes. FSST: table
  size, offsets size, the table (count, lengths, symbol bytes), offsets over the codes, codes.
  Dictionary: the strings' size, a plain or FSST column of the distinct strings, the codes.

An entry is 24 bytes for 1,024 values, 0.19 bits a value; against the earlier 4-byte
directory and in-block header it costs 1 to 1.5% on `core`'s columns and buys 40% off `get`.

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

Measured on `core` (Apple arm64, the cost example, 2026-11-15):

- `get` on a hot column: floor about 1 ns (a plain array's load, 0.5 ns, plus the entry's,
  whose address is known first). Measured 1.6 to 1.8 ns for `For`/`Linear` (2.7 to 3.6 before
  the flat directory): 1.6 to 1.8 times the floor; what is left is instruction count (bounds
  checks, the entry's five fields), not loads.
- Whole-block decode: about 0.1 ns a value. Measured 0.17 ns by the shuffle unpack (0.34 by
  eight scalar loads a group before; the scalar twin 0.47 to 0.55): 1.7 times the floor.
- Size: for `For` the entropy of the value range a block; nothing here measures against an
  entropy coder, the ratio to DEFLATE on the plain bytes is printed as a reference only.

## What would shrink (estimates until the cost example runs)

- Outline records (`FileItems`, an `ItemRow` of about 160 bytes): 64 of them are two
  `ContentId`s, which no lightweight scheme compresses. The seven item-relative ranges, parent,
  end, disambiguator and tags (about 90 bytes) become columns of 8 to 16 bits a value, many
  runs of zero: an estimated 40 to 50% smaller record, 8.1 MB to about 4.5 MB on `core`.
- Scope records (`ModuleScope`, a `BindingRow` of about 84 bytes, 64 of them ids): about 15%
  from the narrow fields alone. The id dictionary (`enc::ids`) on a synthetic set shaped like
  `core` (406 records, 102,681 bindings; 15% of defs repeat in a record, 4 `vis_key`s a record:
  a model, not a measurement) takes the two id columns from 64 to 29 bytes a row: `vis_key`
  34 times smaller, `def` 1.13 times (ids that do not repeat do not shrink). Measured on real
  records once a scope record is written as columns.
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
