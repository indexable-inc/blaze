---
status: plan
summary: speed pass on blaze-ids and blaze-format (content IDs, interner, zero-copy records, column tiers); the techniques each matches, and where it falls short of the best and why.
date: 2026-10-29
---
# speed-ids-format: content IDs, the interner, records and column tiers

Crates: `blaze-ids` and `blaze-format`. The other format crates have their own notes:
[speed-syntax.md](speed-syntax.md) (`parser`, `syntax`, `syntax-tree`),
[speed-span.md](speed-span.md) (`intern`, and through it `span`, `const-ast`, `tt`,
`syntax-bridge`, `mbe`), and the `speed-hir-*` notes (hir-def's item tree, expression store and
HIR).

## Content IDs (`blaze-ids`)

**Benchmark.** BLAKE3's own multi-input path: `hash_many` runs one input per SIMD lane (4 on
NEON and SSE4.1, 8 on AVX2, 16 on AVX-512), which is how BLAKE3 hashes chunks and parents.

**Now.**
- A domain's context key (BLAKE3's `derive_key` first stage) is derived once per process:
  `Domain` statics hold it in a `OnceLock` (one acquire load), and a `&'static str` domain finds
  it in a fixed 1024-slot open-addressed table probed by FNV-1a, slots never rewritten, a
  pointer compare before the text compare. No hash of a domain name on the hot path.
- `ContentId::of_many` groups inputs of at most one chunk (1 KiB, nearly every ID blaze
  makes) by how many blocks come before their last block. Each group's leading blocks go
  through `blake3::platform::Platform::hash_many` together.
- Each input's last block goes through `compress_lanes` with `CHUNK_END | ROOT`, eight at a
  time. That block has its own length, which `hash_many` can't take since it gives every lane
  full blocks, and for an input of at most 64 bytes it is the whole input.
  - `compress_lanes` is BLAKE3's compression transposed: word `w` of every lane is one
    `[u32; 8]`, and each lane has its own block length.
  - Its add, xor and rotate loops are what LLVM vectorizes (one AVX2 register, two NEON
    ones). It is safe code, with no intrinsics of our own to prove.
- Longer inputs go through the hasher, which already uses SIMD inside one input. So a batch of
  short inputs costs about one compression per block per SIMD lane, against one per block for
  each input on its own.
- `blake3::platform` is `#[doc(hidden)]` and unstable. `of_many_is_the_hasher_at_every_length_and_group_size`
  checks it and `compress_lanes` against `blake3::Hasher` at every length 0..=1100 in groups
  of 1 to 33, which leaves lanes empty and fills them. A blake3 bump that changes it fails
  there (or fails to compile), never in an ID. Pinning blake3 to
  `=1.8.7` is asked for in `v2/requests/format.md`.

## The interner (`Sharded`)

**Benchmarks.** SwissTable (tag bytes, then the slot); papaya (lock-free reads and CAS inserts);
boxcar (append-only chunks that never move).

**Now.**
- Shards: `available_parallelism() * 4`, rounded up to a power of two, at least 4, each
  `#[repr(align(128))]`. Shards are picked by ID bits 0..32. Each claims indices 64 at a time,
  so writers to two shards share no counter and no line.
- A table place is one `AtomicU64`: the index plus one, and the ID's bits 64..96 as a tag. A
  probe starts at the tag and compares the tag in the table. It reads the value's ID only on a
  tag match: 1 in 2^(32 - log2 table size) for a foreign ID, so 2^-19 in a 8192-place table.
  Growing rehashes from the tags alone and reads no value. At half load a find is about 1.5
  acquire loads of one line, plus one value read on a hit.
- Reads take no lock, write nothing and allocate nothing. A `find` in a shard with no table
  returns at once, without making the table. Values live in chunks of doubling size that
  never move (boxcar's layout). `&T` lasts as long as the interner.
- Against the benchmarks: reads match SwissTable's line count (one table line plus the value)
  and papaya's freedom from locks, with no epoch guard, since values are never freed. Inserts
  still lock their shard to look again and insert, where papaya uses a CAS. With 4 shards a
  core, writers rarely meet, and the critical section is a probe, a store and a bump. CAS
  inserts would need "being written" places that readers wait on. They are worth it only if a
  profile shows shard locks.

## Records (`blaze-format`)

**Benchmarks.** rkyv and FlatBuffers zero-copy reads: one bounds and alignment check per
buffer, then plain loads.

**Now.**
- A buffer is opened with one check of its header, schema hash and root. Each `Rel`/`Slice`
  is checked once, when followed, and the reference is then used directly.
- `Record::PLAIN`: a record where every bit pattern is valid (integers, `ContentId`, arrays and
  records of them, `SpanRef`, `RelSpan`) is checked by its bounds and alignment alone. A
  `Slice` of plain records is O(1) to follow, whatever its length, as in rkyv. Records with
  `bool`s, enums or references still check each element, as rkyv's `CheckBytes` does.
- Schema hashes are BLAKE3 evaluated in `const`, so they cost nothing at run time.
- `open_verified`/`read_verified` hash the buffer with BLAKE3 (SIMD across chunks) only where
  the bytes came from outside the process.

## Column tiers (`blaze-format::tiers`)

**Benchmarks.** Lemire and Boytsov's SIMD-BP128 and FastPFor ("Decoding billions of integers
per second through vectorization", 2015); zstd with prepared dictionaries; Arrow and Parquet's
codec choice per column.

**Now.**
- **Hot**: each column is stored at the narrowest of u8/u16/u32 that holds its maximum, which
  one OR fold finds. It is written straight into the buffer and read in place.
- **Warm**: frame of reference (min subtracted) plus SIMD-BP128 layout. Blocks of 128 values
  are packed in 4 interleaved lanes of u32 words, so one 128-bit load unpacks four values. The
  unpacker is monomorphised for each bit width 1..=32 (`unpack_block::<BITS>`), with no
  branches inside, and the compiler vectorises it on every target. Whole blocks decode
  straight into the caller's slice. A 32-bit column has min 0, so a read cannot overflow.
- **Cold**: one pass computes the column's shape (runs, steps, distinct count, gaps). From it,
  each codec's exact size (plain, runs, deltas, dictionary) is computed without encoding, the
  smallest is picked, and only that one is encoded. A test checks each size against the
  encoding. Distinct values are counted with the workspace's one radix sort (`blaze-radix`,
  through `blaze_ids::radix`: LSD, skipping digits every value shares, stopping early on
  sorted input, parallel on large columns). Then zstd at level 9 runs with a per-kind dictionary prepared once
  (`CDict`/`DDict`) and a thread-local `CCtx`/`DCtx`, writing into the output vector after
  the header with no extra copy.
- **Whole buffers** (`tiers::BufferColumns`, for the store's tiers): the schema's records find
  every word table reachable from the root (`Record::columns`). A word table is the items of
  a slice of records whose size is a whole number of `u32` words, at least word-aligned. Each
  word of a row is cut out as a column of its own and packed on its own: SIMD-BP128 warm,
  smallest codec then zstd cold. So a slice of `{ start, len }` pairs packs as two columns, and
  a slice of records holding `Slice`s packs their relative offsets, which fall steadily row by
  row, as deltas. The rest of the buffer is kept byte for byte, so both forms give back the
  exact buffer. This is BtrBlocks' and Parquet's approach: a struct split into its fields,
  each encoded by its own statistics, a general compressor after. Here it is driven by the
  record schema, not by guessing from the bytes. A table of under 16 rows stays one column,
  since a column's frame costs more than splitting so few rows saves.
- Where it falls short of the best: warm blocks have no exceptions, which FastPFor's patched
  frames add for columns with rare outliers. Those columns go cold instead, where the codec
  choice handles them. Adding PFor exceptions is worth it only if a profile of warm reads shows
  outlier-heavy columns.

## Unsafe (docs/v2/plan/design/unsafe.md)

- **`blaze-ids`:** `#![forbid(unsafe_code)]`. Its SIMD is blake3's, plus `compress_lanes`,
  which is safe code.
- **`blaze-format`:** every unsafe block is in `raw.rs`, behind safe functions that check
  before they cast:
  - `view` checks size and alignment, then `Record::check`;
  - `view_slice` does the same for each item, or for the bounds alone for a `PLAIN` record;
  - `in_place` reads a `Rel` or `Slice`, which any bits are, after size and alignment;
  - `bytes_of` and `blocks_bytes` give a value's bytes.
  
  What the safe version would cost: a copy of every record read out of a mapped buffer. The
  rest of the crate holds only the `Record` contract (`unsafe trait`) and its impls, each with
  a `SAFETY:` comment. `raw`'s tests are the safe reference: views equal a byte-at-a-time
  decode, and views of the wrong size or alignment are refused.
- **`syntax-tree`** (`cursor.rs`, the tree's ownership), **`syntax`** (`raw.rs`) and
  **`parser`** (`lexed_str/scan.rs` SIMD, `syntax_kind.rs`): each block has its `SAFETY:`
  comment, and each `unsafe fn` a `# Safety` section.
- **format's `hir-def` files:** no unsafe code (`hir/columns.rs` forbids it).
