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

## Cold and moving bytes, measured (2026-11-16)

`examples/squeeze.rs` in blaze-squeeze (release, Apple arm64, best of 5) on real record sets:
`core`'s scope, decls and impl-bucket records (blaze-scope's pipeline `--dump`), every
`library/` file outlined to `FileItems`, the ladder's drive envelopes, two leaf tables, 4 KiB
pieces of linked AArch64 code, and a build session's profile Parquet. Bytes, and in brackets
the time to have one record from the middle in hand. The dictionary column is the odd half of
each set (trained on the even half) beside plain zstd 3 on that same half; the dictionary's
own bytes are added. Solid blocks add an index of 8 bytes a record.

| kind | raw | ours | zstd 3 a record | odd half: zstd 3 → + 16 KiB dict | solid 64 KiB |
|---|---|---|---|---|---|
| scope (406) | 2,833,776 [13 ns] | 2,085,082 [2.9 us a row] | 1,485,118 [286 ns] | 720,642 → 260,426 [308 ns] | 654,895 [7.6 us] |
| decls (406) | 1,329,408 [10 ns] | - | 625,315 [264 ns] | 277,679 → 261,938 [310 ns] | 548,542 [27 us] |
| impl bucket (4,003) | 1,333,584 [3 ns] | - | 1,196,567 [48 ns] | 621,010 → 476,090 [49 ns] | 824,524 [7.1 us] |
| `FileItems` (3,130) | 44,759,792 [103 ns] | 28,849,347 (rest estimated) [3.7 us a row] | 20,428,333 [4.5 us] | 11,042,885 → 10,916,539 [1.6 us] | 19,481,215 [208 us] |
| ladder envelopes (180) | 87,696 [3 ns] | - | 61,236 [36 ns] | 29,708 → 28,579 [34 ns] | 17,868 [4.5 us] |
| leaf tables (2) | 1,823,766 [18 us] | - | 1,052,567 [493 us] | - | 1,052,599 |
| code, 4 KiB (756) | 3,095,152 [38 ns] | - | 1,874,807 [3.7 us] | 936,652 → 885,736 [3.5 us] | 1,606,270 [36 us] |
| profile Parquet (174) | 67,856,798 [40 us] | Parquet's own | 22,965,306 [972 us] | 10,604,785 → 10,610,393 | 22,956,980 [1.0 ms] |

Others measured: zstd 1 is within 1 to 3% of zstd 3 everywhere and zstd 19 gains 2 to 10% at
8 to 16 MB/s; LZ4 is 15 to 25% larger than zstd 3 at a similar read cost; 112 KiB dictionaries
lose to 16 KiB ones once their bytes are counted; ours then zstd 3: scope 1,475,113,
`FileItems` columns 16,943,435. New-layer code records call through fixups kept apart from
their bytes (`blaze-emit` `Code { bytes, fixups }`), holding no `bl` displacements, so the branch
filter was measured on the linked binary only: 3.8% smaller under zstd 3, at 4,491 MB/s. Not
measured: the old `~/.cache/blaze` code records, held in blaze-cas `.pack` files this example
does not parse.

What the numbers chose:

- (a) A trained dictionary beats plain zstd by more than 1.5 times on scope records only
  (2.77 times, its bytes counted; impl buckets 1.30, the rest 1.01 to 1.08). Built in
  blaze-squeeze: `Dictionary::train` (and `examples/train.rs` as the tool), the dictionary a
  record named by the BLAKE3 id of its bytes, a squeezed record (`BSQ1`, the id it needs, one
  zstd frame with its size and checksum) refused by name without it, its size checked against
  the caller's limit before the output grows. Tests are `blaze_sim::check` properties with
  planted-bug controls.
- (b) Cascades inside the encodings (column-level runs, a sorted dictionary with delta values,
  deltas then the chooser) against the single-level chooser: 3.8% (scope) and 0.3%
  (`FileItems`) fewer bytes within 1.3 times the get cost, 6.2% and 12.5% at any cost. Under
  15%: not added. Strings already cascade (dictionary then FSST, chosen on both sets).
- (c) Solid 64 KiB blocks win for transfer of small records (2.27 times zstd a record on scope
  records, 3.4 on ladder envelopes) and gain nothing on large ones. Design only: a transfer pack
  sorts records by kind into about 64 KiB zstd blocks, the manifest naming each record's block
  and offset; a reader asks for the block with a bound on its decompressed size and checks each
  record's digest after (docs/v2/plan/design/remote-read.md, "Not built"). Cold segments in
  the store keep per-record frames, so one read stays under 1 us.

Part of the profile: `spans.function` named a macro file by a Debug dump of its ids, up to
64 KB a value. Named by its calls and source file instead, a session's profile is 150,658,004
→ 67,856,798 bytes encoded without a codec, 30,722,873 → 28,390,672 under LZ4 and 24,670,107
→ 23,756,638 under zstd 3.

### A zstd decoder in safe Rust (a plan)

Scope: frames (header, window, content size, checksum), raw, RLE and compressed blocks, the
literals section (raw, RLE, Huffman with 1 or 4 streams), the sequences section (predefined,
RLE and FSE tables; repeat offsets), FSE and Huffman table decoding, and dictionaries (their
entropy tables and content). No encoder: CI and the remote workers write with C zstd.

Why: every fetched byte goes through the decoder before its digest is checked, and today that is
C (vendor/zstd-sys) or its c2rust translation on wasm (vendor/libzstd-rs-sys, 12,738 lines of
decompress and common code, 953 `unsafe` across the crate). A safe decoder makes the limit and
the in-range reads blaze's own promise, takes the allocator as a value, returns typed errors,
and runs on wasm without the translation.

Size: ruzstd 0.8.2's decoding side is about 7,100 lines without comments (decoding 4,189,
fse 834 and huff0 824 with their encoders, bit_io 595, dictionary 360, blocks 341); blaze's is
expected at 4,000 to 5,000. Speed is an expectation: within 1.3 times of C with four
interleaved Huffman streams, a branchless bit reader over 64-bit loads, and bounds checks
hoisted by checking a block's lengths once; ruzstd is reported 2 to 3 times slower than C.

Corpus: zstd's golden frames and dictionaries (`tests/golden-*`), frames made by its
`decodecorpus` generator, every record set above compressed by C zstd at levels 1 to 19 with
and without dictionaries, and a `blaze_sim` property against the C decoder on drawn and mutated
frames, with planted bugs.
